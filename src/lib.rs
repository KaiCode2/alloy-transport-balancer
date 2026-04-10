#![cfg_attr(docsrs, feature(doc_cfg))]
#![doc = include_str!("../README.md")]

#[cfg(feature = "batching")]
mod batching;
#[cfg(feature = "balancer")]
mod client_pool;
#[cfg(feature = "balancer")]
mod throttle;

#[cfg(feature = "batching")]
pub use batching::{BatchingConfig, BatchingTransport};
#[cfg(feature = "balancer")]
pub use client_pool::{HttpClientConfig, set_default_http_client_config};
#[cfg(feature = "balancer")]
pub use throttle::{
    DomainThrottleSnapshot, DomainThrottleState, ThrottleConfig, domain_throttle,
    extract_rate_limit_domain, log_throttle_summary, max_domain_backoff, pre_request_delay,
    record_batch_rejection, record_rate_limit, record_success, set_default_throttle_config,
    throttle_snapshot, weighted_domain_backoff,
};

#[cfg(feature = "balancer")]
use alloy_json_rpc::{RequestPacket, ResponsePacket};
#[cfg(feature = "balancer")]
use alloy_transport::{RpcError, TransportError, TransportErrorKind, TransportFut};
#[cfg(feature = "balancer")]
use alloy_transport_http::Http;
#[cfg(feature = "balancer")]
use client_pool::shared_client_for_domain;
#[cfg(feature = "balancer")]
use reqwest::Url;
#[cfg(feature = "balancer")]
use std::{
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
    task::{Context, Poll},
    time::Duration,
};
#[cfg(feature = "balancer")]
use throttle::record_skip;
#[cfg(feature = "balancer")]
use tokio::time::sleep;
#[cfg(feature = "balancer")]
use tower::Service;
#[cfg(feature = "balancer")]
use tracing::{debug, warn};

/// Relative weight for an endpoint in the load balancer.
///
/// Higher values route proportionally more traffic to an endpoint.
/// The default weight of 100 provides headroom for relative adjustments
/// (e.g., `Weight(150)` for a preferred provider, `Weight(50)` for a backup).
///
/// # Examples
///
/// ```
/// use alloy_transport_balancer::Weight;
///
/// // Default weight of 100 for equal distribution
/// let w = Weight::default();
/// assert_eq!(w.0, 100);
///
/// // Convert from u32
/// let w: Weight = 150.into();
/// assert_eq!(w, Weight(150));
/// ```
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[cfg(feature = "balancer")]
pub struct Weight(pub u32);

#[cfg(feature = "balancer")]
impl Default for Weight {
    fn default() -> Self {
        Self(100)
    }
}

#[cfg(feature = "balancer")]
impl From<u32> for Weight {
    fn from(w: u32) -> Self {
        Self(w)
    }
}

/// Configuration for the load balancer's retry and failover behavior.
///
/// # Examples
///
/// ```
/// use alloy_transport_balancer::BalancerConfig;
/// use std::time::Duration;
///
/// let config = BalancerConfig {
///     max_retry_rounds: 5,
///     initial_backoff: Duration::from_millis(200),
///     ..Default::default()
/// };
/// ```
#[derive(Clone, Debug)]
#[cfg(feature = "balancer")]
pub struct BalancerConfig {
    /// Maximum number of full-rotation retry rounds after the initial attempt.
    /// Total rotation attempts = 1 (initial) + max_retry_rounds. Default: 3.
    pub max_retry_rounds: u32,
    /// Initial backoff delay between retry rounds. Default: 100ms.
    pub initial_backoff: Duration,
    /// Maximum backoff delay between retry rounds. Default: 4s.
    pub max_backoff: Duration,
    /// Delay inserted between failovers on 429 responses. Default: 75ms.
    pub rate_limit_failover_delay: Duration,
    /// Domain throttle delay above which an endpoint is skipped. Default: 500ms.
    pub heavy_throttle_threshold: Duration,
}

#[cfg(feature = "balancer")]
impl Default for BalancerConfig {
    fn default() -> Self {
        Self {
            max_retry_rounds: 3,
            initial_backoff: Duration::from_millis(100),
            max_backoff: Duration::from_secs(4),
            rate_limit_failover_delay: Duration::from_millis(75),
            heavy_throttle_threshold: Duration::from_millis(500),
        }
    }
}

/// Builder for [`LoadBalancedTransport`].
///
/// Allows configuring the balancer, throttle, and HTTP client behavior
/// before constructing the transport.
///
/// # Example
///
/// ```
/// use alloy_transport_balancer::{LoadBalancedTransport, Weight, BalancerConfig};
/// use reqwest::Url;
///
/// let transport = LoadBalancedTransport::builder(vec![
///     (Url::parse("http://localhost:8545").unwrap(), Weight::default()),
///     (Url::parse("http://localhost:8546").unwrap(), Weight::default()),
/// ])
/// .config(BalancerConfig {
///     max_retry_rounds: 5,
///     ..Default::default()
/// })
/// .build();
/// ```
#[cfg(feature = "balancer")]
pub struct LoadBalancedTransportBuilder {
    endpoints: Vec<(Url, Weight)>,
    config: BalancerConfig,
    throttle_config: Option<ThrottleConfig>,
    http_client_config: Option<HttpClientConfig>,
}

#[cfg(feature = "balancer")]
impl LoadBalancedTransportBuilder {
    /// Set the balancer retry/failover configuration.
    pub const fn config(mut self, config: BalancerConfig) -> Self {
        self.config = config;
        self
    }

    /// Set the domain throttle configuration.
    ///
    /// This sets the process-wide default for newly created domains.
    /// Already-registered domains keep their existing config.
    pub fn throttle_config(mut self, config: ThrottleConfig) -> Self {
        self.throttle_config = Some(config);
        self
    }

    /// Set the HTTP client pool configuration.
    ///
    /// This sets the process-wide default for newly created clients.
    /// Already-registered domains keep their existing client.
    pub const fn http_client_config(mut self, config: HttpClientConfig) -> Self {
        self.http_client_config = Some(config);
        self
    }

    /// Build the [`LoadBalancedTransport`].
    ///
    /// # Panics
    ///
    /// Panics if endpoints is empty or any weight is 0.
    pub fn build(self) -> LoadBalancedTransport {
        // Apply global configs if provided.
        if let Some(tc) = self.throttle_config {
            set_default_throttle_config(tc);
        }
        if let Some(hc) = self.http_client_config {
            set_default_http_client_config(hc);
        }

        assert!(!self.endpoints.is_empty(), "at least one endpoint required");

        let mut transports = Vec::with_capacity(self.endpoints.len());
        let mut throttles = Vec::with_capacity(self.endpoints.len());
        let mut cumulative = Vec::with_capacity(self.endpoints.len());
        let mut total = 0u32;

        for (url, weight) in self.endpoints {
            assert!(weight.0 > 0, "endpoint weight must be > 0");
            total += weight.0;
            cumulative.push(total);

            let domain = {
                let host = url.host_str().unwrap_or("unknown");
                extract_rate_limit_domain(host).to_owned()
            };

            let client = shared_client_for_domain(&domain);
            throttles.push(domain_throttle(&domain));
            transports.push(Http::with_client(client, url));
        }

        debug!(
            endpoints = transports.len(),
            "LoadBalancedTransport created with shared HTTP clients and cross-thread throttling"
        );

        LoadBalancedTransport {
            endpoints: Arc::new(transports),
            endpoint_throttles: Arc::new(throttles),
            cumulative_weights: Arc::new(cumulative),
            total_weight: total,
            counter: Arc::new(AtomicU64::new(0)),
            config: Arc::new(self.config),
        }
    }
}

/// A load-balanced RPC transport that distributes requests across multiple
/// HTTP endpoints using weighted round-robin with automatic failover.
///
/// Implements `tower::Service<RequestPacket>` and is `Clone + Send + Sync`,
/// so it satisfies alloy's `Transport` trait and can be passed to
/// `RpcClient::new()` / `ProviderBuilder::connect_client()`.
///
/// **Cross-thread throttling**: endpoints on the same domain share a global
/// [`DomainThrottleState`] so that a 429 from any chain thread causes all
/// threads targeting that domain to back off.
///
/// **HTTP client sharing**: endpoints on the same domain share a single
/// `reqwest::Client` configured for HTTP/2 multiplexing and connection pooling.
///
/// When all endpoints fail with retryable errors (429, 502, 503, 504),
/// the transport retries the full rotation with exponential backoff.
#[derive(Clone)]
#[cfg(feature = "balancer")]
pub struct LoadBalancedTransport {
    /// Inner HTTP transports, one per configured endpoint.
    endpoints: Arc<Vec<Http<reqwest::Client>>>,
    /// Per-endpoint domain throttle state. Endpoints on the same domain
    /// share the same `Arc<DomainThrottleState>`.
    endpoint_throttles: Arc<Vec<Arc<DomainThrottleState>>>,
    /// Precomputed cumulative weight boundaries for O(log n) weighted selection.
    cumulative_weights: Arc<Vec<u32>>,
    /// Sum of all endpoint weights.
    total_weight: u32,
    /// Monotonically increasing counter for round-robin distribution.
    counter: Arc<AtomicU64>,
    /// Balancer retry/failover configuration.
    config: Arc<BalancerConfig>,
}

#[cfg(feature = "balancer")]
impl LoadBalancedTransport {
    /// Create a new load-balanced transport from a list of `(url, weight)` pairs
    /// with default configuration.
    ///
    /// Endpoints on the same domain automatically share:
    /// - A single `reqwest::Client` (HTTP/2 multiplexing + connection pool)
    /// - A single [`DomainThrottleState`] (cross-thread rate-limit coordination)
    ///
    /// For custom configuration, use [`LoadBalancedTransport::builder`].
    ///
    /// # Panics
    ///
    /// Panics if `endpoints` is empty or any weight is 0.
    ///
    /// # Errors
    ///
    /// The transport retries automatically on 429, 502, 503, 504, connection
    /// failures, and timeouts. Non-retryable errors (400, 401, JSON-RPC errors)
    /// are returned immediately. After `max_retry_rounds` full rotations through
    /// all endpoints, the last retryable error is returned to the caller.
    pub fn new(endpoints: Vec<(Url, Weight)>) -> Self {
        Self::builder(endpoints).build()
    }

    /// Create a builder for a load-balanced transport.
    ///
    /// # Example
    ///
    /// ```
    /// use alloy_transport_balancer::{LoadBalancedTransport, Weight, BalancerConfig};
    /// use reqwest::Url;
    ///
    /// let transport = LoadBalancedTransport::builder(vec![
    ///     (Url::parse("http://localhost:8545").unwrap(), Weight::default()),
    /// ])
    /// .config(BalancerConfig { max_retry_rounds: 5, ..Default::default() })
    /// .build();
    /// ```
    pub fn builder(endpoints: Vec<(Url, Weight)>) -> LoadBalancedTransportBuilder {
        LoadBalancedTransportBuilder {
            endpoints,
            config: BalancerConfig::default(),
            throttle_config: None,
            http_client_config: None,
        }
    }

    /// Select an endpoint index using weighted round-robin.
    ///
    /// Uses an atomic counter mapped into cumulative weight buckets via
    /// binary search for lock-free O(log n) selection.
    fn select_endpoint(&self) -> usize {
        let tick = self.counter.fetch_add(1, Ordering::Relaxed);
        let bucket = (tick % self.total_weight as u64) as u32;
        self.cumulative_weights.partition_point(|&w| w <= bucket)
    }

    /// Returns `true` if the error warrants failover to the next provider.
    ///
    /// Retryable: 429 (rate limit), 502/503/504 (gateway errors),
    /// connection failures, timeouts, and backend-gone.
    fn is_retryable(err: &TransportError) -> bool {
        if let RpcError::Transport(kind) = err {
            match kind {
                TransportErrorKind::HttpError(http_err) => {
                    matches!(http_err.status, 429 | 502 | 503 | 504)
                }
                TransportErrorKind::BackendGone => true,
                TransportErrorKind::Custom(e) => {
                    let msg = e.to_string();
                    msg.contains("timed out")
                        || msg.contains("connection")
                        || msg.contains("429 Too Many Requests")
                }
                _ => false,
            }
        } else {
            false
        }
    }

    /// Returns `true` if the error is specifically a 429 rate-limit response.
    fn is_rate_limit(err: &TransportError) -> bool {
        if let RpcError::Transport(kind) = err {
            match kind {
                TransportErrorKind::HttpError(http_err) => http_err.status == 429,
                TransportErrorKind::Custom(e) => e.to_string().contains("429 Too Many Requests"),
                _ => false,
            }
        } else {
            false
        }
    }
}

#[cfg(feature = "balancer")]
impl Service<RequestPacket> for LoadBalancedTransport {
    type Response = ResponsePacket;
    type Error = TransportError;
    type Future = TransportFut<'static>;

    fn poll_ready(&mut self, _cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        Poll::Ready(Ok(()))
    }

    fn call(&mut self, req: RequestPacket) -> Self::Future {
        let this = self.clone();
        Box::pin(async move {
            let num_endpoints = this.endpoints.len();
            let config = &this.config;
            let mut backoff = config.initial_backoff;

            for round in 0..=config.max_retry_rounds {
                if round > 0 {
                    warn!(
                        round,
                        backoff_ms = backoff.as_millis() as u64,
                        "all endpoints failed with retryable errors, backing off before retry"
                    );
                    sleep(backoff).await;
                    backoff = (backoff * 2).min(config.max_backoff);
                }

                let start_idx = this.select_endpoint();
                let mut last_err: Option<TransportError> = None;

                for attempt in 0..num_endpoints {
                    let idx = (start_idx + attempt) % num_endpoints;

                    // Cross-thread domain throttle: skip heavily-throttled
                    // endpoints if non-throttled alternatives remain.
                    let throttle = &this.endpoint_throttles[idx];
                    let delay = pre_request_delay(throttle);
                    if delay >= config.heavy_throttle_threshold && attempt < num_endpoints - 1 {
                        record_skip(throttle);
                        continue;
                    }

                    // Apply domain-level backoff delay before sending.
                    if !delay.is_zero() {
                        sleep(delay).await;
                    }

                    let mut transport = this.endpoints[idx].clone();

                    match transport.call(req.clone()).await {
                        Ok(resp) => {
                            record_success(throttle);
                            return Ok(resp);
                        }
                        Err(err) => {
                            let retryable = Self::is_retryable(&err);
                            if !retryable {
                                return Err(err);
                            }

                            let rate_limited = Self::is_rate_limit(&err);

                            // Update cross-thread throttle state on 429.
                            if rate_limited {
                                record_rate_limit(throttle);
                            }

                            warn!(
                                url = transport.url(),
                                endpoint_attempt = attempt + 1,
                                round,
                                rate_limited,
                                "RPC request failed, failing over: {err}"
                            );

                            last_err = Some(err);

                            if rate_limited && attempt < num_endpoints - 1 {
                                sleep(config.rate_limit_failover_delay).await;
                            }
                        }
                    }
                }

                // All endpoints failed this round. On the final round, return the error.
                if round == config.max_retry_rounds {
                    return Err(last_err.expect("at least one endpoint was tried"));
                }
            }

            unreachable!("retry loop always returns")
        })
    }
}

#[cfg(feature = "balancer")]
impl std::fmt::Debug for LoadBalancedTransport {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LoadBalancedTransport")
            .field("endpoints", &self.endpoints.len())
            .field("total_weight", &self.total_weight)
            .finish()
    }
}

#[cfg(all(test, feature = "balancer"))]
mod tests {
    use super::*;

    fn test_transport(weights: &[u32]) -> LoadBalancedTransport {
        let endpoints: Vec<(Url, Weight)> = weights
            .iter()
            .enumerate()
            .map(|(i, &w)| {
                (
                    Url::parse(&format!("http://provider-{i}.example.com")).unwrap(),
                    Weight(w),
                )
            })
            .collect();
        LoadBalancedTransport::new(endpoints)
    }

    #[test]
    fn weighted_distribution() {
        let transport = test_transport(&[10, 5, 2]);
        let total = 10 + 5 + 2;
        let iterations = total as usize * 1000;
        let mut counts = [0usize; 3];

        for _ in 0..iterations {
            let idx = transport.select_endpoint();
            counts[idx] += 1;
        }

        // With perfect round-robin, distribution should be exact over full cycles.
        // 17000 requests / 17 total_weight = 1000 full cycles.
        assert_eq!(counts[0], 10_000, "provider 0 (weight=10)");
        assert_eq!(counts[1], 5_000, "provider 1 (weight=5)");
        assert_eq!(counts[2], 2_000, "provider 2 (weight=2)");
    }

    #[test]
    fn single_endpoint() {
        let transport = test_transport(&[5]);
        for _ in 0..100 {
            assert_eq!(transport.select_endpoint(), 0);
        }
    }

    #[test]
    fn equal_weights() {
        let transport = test_transport(&[1, 1, 1]);
        let mut counts = [0usize; 3];
        for _ in 0..300 {
            counts[transport.select_endpoint()] += 1;
        }
        assert_eq!(counts, [100, 100, 100]);
    }

    #[test]
    #[should_panic(expected = "at least one endpoint required")]
    fn empty_endpoints_panics() {
        LoadBalancedTransport::new(vec![]);
    }

    #[test]
    #[should_panic(expected = "endpoint weight must be > 0")]
    fn zero_weight_panics() {
        test_transport(&[10, 0, 5]);
    }

    #[test]
    fn weight_default_is_100() {
        assert_eq!(Weight::default(), Weight(100));
    }

    #[test]
    fn weight_from_u32() {
        assert_eq!(Weight::from(42), Weight(42));
    }

    #[test]
    fn balancer_config_defaults_are_sane() {
        let config = BalancerConfig::default();
        assert!(config.max_retry_rounds >= 1);
        assert!(config.initial_backoff <= config.max_backoff);
        assert!(config.rate_limit_failover_delay < config.initial_backoff);
    }

    #[test]
    fn builder_with_defaults() {
        let transport = LoadBalancedTransport::builder(vec![(
            Url::parse("http://example.com").unwrap(),
            Weight::default(),
        )])
        .build();
        assert_eq!(transport.total_weight, 100);
    }

    #[test]
    fn builder_with_custom_config() {
        let config = BalancerConfig {
            max_retry_rounds: 5,
            ..Default::default()
        };
        let transport = LoadBalancedTransport::builder(vec![(
            Url::parse("http://example.com").unwrap(),
            Weight::default(),
        )])
        .config(config)
        .build();
        assert_eq!(transport.config.max_retry_rounds, 5);
    }

    #[test]
    fn retryable_429() {
        let err = TransportErrorKind::http_error(429, "rate limited".into());
        assert!(LoadBalancedTransport::is_retryable(&err));
    }

    #[test]
    fn retryable_502() {
        let err = TransportErrorKind::http_error(502, "bad gateway".into());
        assert!(LoadBalancedTransport::is_retryable(&err));
    }

    #[test]
    fn retryable_503() {
        let err = TransportErrorKind::http_error(503, "unavailable".into());
        assert!(LoadBalancedTransport::is_retryable(&err));
    }

    #[test]
    fn retryable_504() {
        let err = TransportErrorKind::http_error(504, "gateway timeout".into());
        assert!(LoadBalancedTransport::is_retryable(&err));
    }

    #[test]
    fn not_retryable_400() {
        let err = TransportErrorKind::http_error(400, "bad request".into());
        assert!(!LoadBalancedTransport::is_retryable(&err));
    }

    #[test]
    fn retryable_backend_gone() {
        let err = TransportErrorKind::backend_gone();
        assert!(LoadBalancedTransport::is_retryable(&err));
    }

    #[test]
    fn retryable_timeout() {
        let err = TransportErrorKind::custom_str("request timed out");
        assert!(LoadBalancedTransport::is_retryable(&err));
    }

    #[test]
    fn retryable_connection_error() {
        let err = TransportErrorKind::custom_str("connection refused");
        assert!(LoadBalancedTransport::is_retryable(&err));
    }

    #[test]
    fn not_retryable_json_rpc_error() {
        use alloy_json_rpc::ErrorPayload;
        let payload: ErrorPayload =
            serde_json::from_str(r#"{"code":-32600,"message":"invalid request"}"#).unwrap();
        let err: TransportError = RpcError::ErrorResp(payload);
        assert!(!LoadBalancedTransport::is_retryable(&err));
    }

    #[test]
    fn rate_limit_detection_429() {
        let err = TransportErrorKind::http_error(429, "rate limited".into());
        assert!(LoadBalancedTransport::is_rate_limit(&err));
    }

    #[test]
    fn rate_limit_detection_502_is_not_rate_limit() {
        let err = TransportErrorKind::http_error(502, "bad gateway".into());
        assert!(!LoadBalancedTransport::is_rate_limit(&err));
    }

    #[test]
    fn rate_limit_detection_custom_429() {
        let err = TransportErrorKind::custom_str("429 Too Many Requests");
        assert!(LoadBalancedTransport::is_rate_limit(&err));
    }

    #[test]
    fn rate_limit_detection_custom_timeout_is_not_rate_limit() {
        let err = TransportErrorKind::custom_str("request timed out");
        assert!(!LoadBalancedTransport::is_rate_limit(&err));
    }

    #[test]
    fn shared_domain_endpoints_share_throttle() {
        let endpoints = vec![
            (
                Url::parse("https://arb-mainnet.g.alchemy.com/v2/key1").unwrap(),
                Weight(100),
            ),
            (
                Url::parse("https://base-mainnet.g.alchemy.com/v2/key1").unwrap(),
                Weight(100),
            ),
            (Url::parse("https://lb.drpc.org/ogrpc").unwrap(), Weight(50)),
        ];
        let transport = LoadBalancedTransport::new(endpoints);

        // First two endpoints are on alchemy.com — should share throttle state.
        assert!(Arc::ptr_eq(
            &transport.endpoint_throttles[0],
            &transport.endpoint_throttles[1]
        ));

        // Third endpoint (drpc.org) should be different.
        assert!(!Arc::ptr_eq(
            &transport.endpoint_throttles[0],
            &transport.endpoint_throttles[2]
        ));
    }
}
