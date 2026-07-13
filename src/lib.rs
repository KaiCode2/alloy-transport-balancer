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
    collections::HashMap,
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
use tokio::sync::Semaphore;
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

/// Per-endpoint routing capabilities.
///
/// Request-size and in-flight limits let a heterogeneous provider pool keep a
/// low-throughput fallback available without sending it work it cannot accept.
#[derive(Clone, Debug)]
#[cfg(feature = "balancer")]
pub struct EndpointConfig {
    /// RPC URL.
    pub url: Url,
    /// Relative traffic weight.
    pub weight: Weight,
    /// Maximum serialized JSON-RPC request bytes accepted by this endpoint.
    /// `None` leaves request size unrestricted.
    pub max_request_bytes: Option<usize>,
    /// Maximum simultaneous requests routed to this endpoint. `None` leaves
    /// concurrency unrestricted.
    pub max_in_flight: Option<usize>,
}

#[cfg(feature = "balancer")]
impl EndpointConfig {
    /// Construct an endpoint with unrestricted request size and concurrency.
    pub const fn new(url: Url, weight: Weight) -> Self {
        Self {
            url,
            weight,
            max_request_bytes: None,
            max_in_flight: None,
        }
    }

    /// Set the largest serialized request this endpoint may receive.
    pub const fn with_max_request_bytes(mut self, bytes: usize) -> Self {
        self.max_request_bytes = Some(bytes);
        self
    }

    /// Bound simultaneous requests sent to this endpoint.
    pub const fn with_max_in_flight(mut self, max_in_flight: usize) -> Self {
        self.max_in_flight = Some(max_in_flight);
        self
    }
}

#[cfg(feature = "balancer")]
impl From<(Url, Weight)> for EndpointConfig {
    fn from((url, weight): (Url, Weight)) -> Self {
        Self::new(url, weight)
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
/// use alloy_transport_balancer::{
///     BalancerConfig, HttpClientConfig, LoadBalancedTransport, Weight,
/// };
/// use reqwest::Url;
/// use std::time::Duration;
///
/// let transport = LoadBalancedTransport::builder(vec![
///     (Url::parse("http://localhost:8545").unwrap(), Weight::default()),
///     (Url::parse("http://localhost:8546").unwrap(), Weight::default()),
/// ])
/// .config(BalancerConfig {
///     max_retry_rounds: 5,
///     ..Default::default()
/// })
/// .http_client_config(HttpClientConfig {
///     request_timeout: Duration::from_secs(10),
///     gzip: true,
///     ..Default::default()
/// })
/// .build();
/// ```
#[cfg(feature = "balancer")]
pub struct LoadBalancedTransportBuilder {
    endpoints: Vec<EndpointConfig>,
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
        let mut total = 0u64;

        let mut request_caps = Vec::with_capacity(self.endpoints.len());
        let mut in_flight_limits = Vec::with_capacity(self.endpoints.len());
        let mut throttles_by_domain = HashMap::new();

        for endpoint in self.endpoints {
            let EndpointConfig {
                url,
                weight,
                max_request_bytes,
                max_in_flight,
            } = endpoint;
            assert!(weight.0 > 0, "endpoint weight must be > 0");
            assert!(
                max_request_bytes.is_none_or(|limit| limit > 0),
                "endpoint max_request_bytes must be > 0 when set"
            );
            assert!(
                max_in_flight.is_none_or(|limit| limit > 0),
                "endpoint max_in_flight must be > 0 when set"
            );
            total = total
                .checked_add(u64::from(weight.0))
                .expect("total endpoint weight exceeds u64::MAX");
            cumulative.push(total);

            let domain = {
                let host = url.host_str().unwrap_or("unknown");
                extract_rate_limit_domain(host).to_owned()
            };

            let client = shared_client_for_domain(&domain);
            let throttle = throttles_by_domain
                .entry(domain.clone())
                .or_insert_with(|| domain_throttle(&domain));
            throttles.push(Arc::clone(throttle));
            transports.push(Http::with_client(client, url));
            request_caps.push(max_request_bytes);
            in_flight_limits.push(max_in_flight.map(|limit| Arc::new(Semaphore::new(limit))));
        }

        debug!(
            endpoints = transports.len(),
            "LoadBalancedTransport created with shared HTTP clients and cross-thread throttling"
        );

        LoadBalancedTransport {
            endpoints: Arc::new(transports),
            endpoint_throttles: Arc::new(throttles),
            endpoint_request_caps: Arc::new(request_caps),
            endpoint_in_flight: Arc::new(in_flight_limits),
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
/// When all endpoints fail with retryable errors (413, 429, 502, 503, 504),
/// the transport retries the full rotation with exponential backoff.
#[derive(Clone)]
#[cfg(feature = "balancer")]
pub struct LoadBalancedTransport {
    /// Inner HTTP transports, one per configured endpoint.
    endpoints: Arc<Vec<Http<reqwest::Client>>>,
    /// Per-endpoint domain throttle state. Endpoints on the same domain
    /// share the same `Arc<DomainThrottleState>`.
    endpoint_throttles: Arc<Vec<Arc<DomainThrottleState>>>,
    /// Optional maximum serialized request bytes per endpoint.
    endpoint_request_caps: Arc<Vec<Option<usize>>>,
    /// Optional per-endpoint concurrency gates.
    endpoint_in_flight: Arc<Vec<Option<Arc<Semaphore>>>>,
    /// Precomputed cumulative weight boundaries for O(log n) weighted selection.
    cumulative_weights: Arc<Vec<u64>>,
    /// Sum of all endpoint weights.
    total_weight: u64,
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
    /// The transport retries automatically on 413, 429, 502, 503, 504, connection
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
        Self::builder_with_endpoints(endpoints.into_iter().map(EndpointConfig::from).collect())
    }

    /// Create a builder with per-endpoint request and concurrency capabilities.
    pub fn builder_with_endpoints(endpoints: Vec<EndpointConfig>) -> LoadBalancedTransportBuilder {
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
        let bucket = tick % self.total_weight;
        self.cumulative_weights.partition_point(|&w| w <= bucket)
    }

    fn endpoint_accepts_request(&self, index: usize, request_bytes: usize) -> bool {
        self.endpoint_request_caps[index].is_none_or(|limit| request_bytes <= limit)
    }

    /// Returns `true` if the error warrants failover to the next provider.
    ///
    /// Retryable: 429 (rate limit), 502/503/504 (gateway errors),
    /// connection failures, timeouts, and backend-gone.
    fn is_retryable(err: &TransportError) -> bool {
        if let RpcError::Transport(kind) = err {
            match kind {
                TransportErrorKind::HttpError(http_err) => {
                    matches!(http_err.status, 413 | 429 | 502 | 503 | 504)
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
            let request_bytes = serde_json::to_vec(&req)
                .map(|serialized| serialized.len())
                .unwrap_or(usize::MAX);
            if !(0..num_endpoints).any(|idx| this.endpoint_accepts_request(idx, request_bytes)) {
                return Err(TransportErrorKind::custom_str(
                    "JSON-RPC request exceeds every configured endpoint size limit",
                ));
            }
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
                    if !this.endpoint_accepts_request(idx, request_bytes) {
                        continue;
                    }

                    // Cross-thread domain throttle: skip heavily-throttled
                    // endpoints if non-throttled alternatives remain.
                    let throttle = &this.endpoint_throttles[idx];
                    let delay = pre_request_delay(throttle);
                    let has_unthrottled_eligible_alternative =
                        ((attempt + 1)..num_endpoints).any(|next_attempt| {
                            let next_idx = (start_idx + next_attempt) % num_endpoints;
                            this.endpoint_accepts_request(next_idx, request_bytes)
                                && pre_request_delay(&this.endpoint_throttles[next_idx])
                                    < config.heavy_throttle_threshold
                        });
                    if delay >= config.heavy_throttle_threshold
                        && has_unthrottled_eligible_alternative
                    {
                        record_skip(throttle);
                        continue;
                    }

                    // Apply domain-level backoff delay before sending.
                    if !delay.is_zero() {
                        sleep(delay).await;
                    }

                    let _permit = match &this.endpoint_in_flight[idx] {
                        Some(limit) => Some(
                            Arc::clone(limit)
                                .acquire_owned()
                                .await
                                .map_err(|_| TransportErrorKind::backend_gone())?,
                        ),
                        None => None,
                    };
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
    use alloy_json_rpc::Id;
    use std::{
        io::{Read, Write},
        net::TcpListener,
        thread,
    };

    fn one_shot_http_server(
        status: &'static str,
        body: &'static str,
    ) -> (Url, thread::JoinHandle<()>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let handle = thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut request = [0_u8; 8 * 1024];
            let _ = stream.read(&mut request).unwrap();
            let response = format!(
                "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            );
            stream.write_all(response.as_bytes()).unwrap();
        });
        (Url::parse(&format!("http://{address}")).unwrap(), handle)
    }

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
    fn builder_supports_total_weights_above_u32_max() {
        let transport = test_transport(&[u32::MAX, u32::MAX]);

        assert_eq!(transport.endpoints.len(), 2);
        assert_eq!(transport.total_weight as u128, u128::from(u32::MAX) * 2);
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
    fn retryable_413_can_fail_over_to_a_larger_endpoint() {
        let err = TransportErrorKind::http_error(413, "payload too large".into());
        assert!(LoadBalancedTransport::is_retryable(&err));
    }

    #[test]
    fn endpoint_request_caps_gate_eligibility() {
        let transport = LoadBalancedTransport::builder_with_endpoints(vec![
            EndpointConfig::new(Url::parse("http://small.example.com").unwrap(), Weight(25))
                .with_max_request_bytes(1_000),
            EndpointConfig::new(Url::parse("http://large.example.com").unwrap(), Weight(100))
                .with_max_request_bytes(5_000),
        ])
        .build();

        assert!(!transport.endpoint_accepts_request(0, 2_000));
        assert!(transport.endpoint_accepts_request(1, 2_000));
    }

    #[tokio::test]
    async fn heavily_throttled_final_eligible_endpoint_is_still_attempted() {
        let mut transport = LoadBalancedTransport::builder_with_endpoints(vec![
            EndpointConfig::new(Url::parse("http://127.0.0.1:1").unwrap(), Weight(1)),
            EndpointConfig::new(Url::parse("http://127.0.0.1:2").unwrap(), Weight(1))
                .with_max_request_bytes(1),
        ])
        .config(BalancerConfig {
            max_retry_rounds: 0,
            heavy_throttle_threshold: Duration::ZERO,
            ..Default::default()
        })
        .build();
        let request = alloy_json_rpc::Request::new("eth_blockNumber".to_owned(), Id::Number(1), ());

        let result = transport
            .call(RequestPacket::Single(request.serialize().unwrap()))
            .await;

        assert!(result.is_err());
    }

    #[tokio::test]
    async fn payload_too_large_response_fails_over_to_next_endpoint() {
        let (small_url, small_server) = one_shot_http_server("413 Payload Too Large", "");
        let (large_url, large_server) =
            one_shot_http_server("200 OK", r#"{"jsonrpc":"2.0","id":2,"result":"0x1"}"#);
        let mut transport =
            LoadBalancedTransport::builder(vec![(small_url, Weight(1)), (large_url, Weight(1))])
                .config(BalancerConfig {
                    max_retry_rounds: 0,
                    ..Default::default()
                })
                .build();
        let request = alloy_json_rpc::Request::new("eth_blockNumber".to_owned(), Id::Number(2), ());

        let response = transport
            .call(RequestPacket::Single(request.serialize().unwrap()))
            .await
            .unwrap();

        assert!(matches!(response, ResponsePacket::Single(_)));
        small_server.join().unwrap();
        large_server.join().unwrap();
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
