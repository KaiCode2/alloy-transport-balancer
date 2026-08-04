#![cfg_attr(docsrs, feature(doc_cfg))]
#![doc = include_str!("../README.md")]

#[cfg(feature = "batching")]
mod batching;
#[cfg(feature = "balancer")]
mod client_pool;
#[cfg(feature = "balancer")]
mod request_rate;
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
use request_rate::{RequestRateFallback, RequestRateLimiter, RequestRateReservation};
#[cfg(feature = "balancer")]
use reqwest::Url;
use std::fmt;
#[cfg(feature = "balancer")]
use std::{
    collections::HashMap,
    sync::{
        Arc, RwLock,
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

/// Stable, non-secret identity for one configured RPC provider endpoint.
///
/// Endpoint identities let callers route follow-up reads back to the provider
/// that announced a block or event. They should be short operator-defined
/// labels such as `quicknode` or `alchemy`, never URLs or credentials.
#[derive(Clone, PartialEq, Eq, PartialOrd, Ord, Hash, serde::Serialize, serde::Deserialize)]
pub struct EndpointId(String);

impl EndpointId {
    /// Construct a non-empty endpoint identity.
    ///
    /// # Panics
    ///
    /// Panics when `value` is empty or contains leading/trailing whitespace.
    pub fn new(value: impl Into<String>) -> Self {
        let value = value.into();
        assert!(!value.is_empty(), "endpoint id must not be empty");
        assert_eq!(
            value.trim(),
            value,
            "endpoint id must not contain outer whitespace"
        );
        Self(value)
    }

    /// Borrow the configured identity.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Debug for EndpointId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_tuple("EndpointId").field(&self.as_str()).finish()
    }
}

impl fmt::Display for EndpointId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl From<&str> for EndpointId {
    fn from(value: &str) -> Self {
        Self::new(value)
    }
}

impl From<String> for EndpointId {
    fn from(value: String) -> Self {
        Self::new(value)
    }
}

/// Scoped endpoint ordering for a cloned transport handle.
///
/// Routing policy is deliberately local to the handle. An event consumer can
/// prefer or pin its announcing provider without changing the ordering used by
/// unrelated requests sharing the same underlying connection pools.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
#[cfg(feature = "balancer")]
pub enum RoutePolicy {
    /// Use normal weighted selection and cross-provider failover.
    #[default]
    Balanced,
    /// Try the named endpoint first, then permit normal failover.
    Prefer(EndpointId),
    /// Send every attempt to the named endpoint and never fail over.
    Require(EndpointId),
}

/// Error returned when a requested endpoint route cannot be constructed.
#[derive(Clone, Debug, PartialEq, Eq)]
#[cfg(feature = "balancer")]
pub enum EndpointRouteError {
    /// No configured endpoint has the requested identity.
    UnknownEndpoint(EndpointId),
    /// The endpoint exists but was not marked as Flashblocks-capable.
    FlashblocksUnsupported(EndpointId),
    /// The endpoint is declared Flashblocks-capable but has not passed its
    /// current runtime liveness/capability preflight.
    FlashblocksUnqualified(EndpointId),
}

#[cfg(feature = "balancer")]
impl fmt::Display for EndpointRouteError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::UnknownEndpoint(id) => write!(f, "unknown RPC endpoint id {id}"),
            Self::FlashblocksUnsupported(id) => {
                write!(f, "RPC endpoint {id} is not configured for Flashblocks")
            }
            Self::FlashblocksUnqualified(id) => {
                write!(f, "RPC endpoint {id} has not passed Flashblocks preflight")
            }
        }
    }
}

#[cfg(feature = "balancer")]
impl std::error::Error for EndpointRouteError {}

/// Latest runtime Flashblocks qualification for one declared endpoint.
#[derive(Clone, Debug, PartialEq, Eq)]
#[cfg(feature = "balancer")]
pub enum FlashblocksQualificationState {
    /// No preflight result has been recorded for the current process.
    Unprobed,
    /// Chain identity and provider-advertised capabilities were validated.
    Qualified {
        /// Chain id observed through the same provider lease.
        chain_id: u64,
        /// Opaque capability names returned by the provider, when available.
        capabilities: Vec<String>,
    },
    /// The most recent preflight failed. A later successful probe can recover
    /// the endpoint without rebuilding the balancer.
    Ineligible {
        /// Stable operator-facing failure reason; never an endpoint URL.
        reason: String,
    },
}

/// Result supplied by the component that performs the live WebSocket and
/// pending-state preflight.
#[derive(Clone, Debug, PartialEq, Eq)]
#[cfg(feature = "balancer")]
pub enum FlashblocksProbeOutcome {
    /// All required checks passed.
    Qualified {
        /// Validated chain id.
        chain_id: u64,
        /// Opaque provider-advertised capabilities, if the RPC supports them.
        capabilities: Vec<String>,
    },
    /// At least one required check failed.
    Ineligible {
        /// Stable operator-facing failure reason; never an endpoint URL.
        reason: String,
    },
}

/// Observable proactive request-rate state for one configured endpoint.
#[derive(Clone, Debug, PartialEq, Eq)]
#[cfg(feature = "balancer")]
pub struct EndpointRequestRateSnapshot {
    /// Stable operator identity, when configured.
    pub endpoint_id: Option<EndpointId>,
    /// Configured JSON-RPC call ceiling for a rolling one-second window.
    pub max_requests_per_second: u32,
    /// Calls admitted during the current rolling one-second window.
    pub used_in_rolling_window: u32,
    /// Calls still available during the current rolling one-second window.
    pub remaining_in_rolling_window: u32,
    /// Total JSON-RPC calls admitted since construction.
    pub total_admitted_rpc_calls: u64,
    /// Total packets redirected after this endpoint lacked immediate capacity.
    pub total_capacity_rollovers: u64,
    /// Total times callers waited for this endpoint's capacity.
    pub total_capacity_waits: u64,
}

/// Observable cumulative probe metrics and latest qualification state.
#[derive(Clone, Debug, PartialEq, Eq)]
#[cfg(feature = "balancer")]
pub struct FlashblocksQualificationSnapshot {
    /// Total recorded probe attempts.
    pub attempts: u64,
    /// Successful probe attempts.
    pub successes: u64,
    /// Failed probe attempts.
    pub failures: u64,
    /// Latest result.
    pub state: FlashblocksQualificationState,
}

#[derive(Clone, Debug)]
#[cfg(feature = "balancer")]
struct FlashblocksQualification {
    attempts: u64,
    successes: u64,
    failures: u64,
    state: FlashblocksQualificationState,
}

#[cfg(feature = "balancer")]
impl Default for FlashblocksQualification {
    fn default() -> Self {
        Self {
            attempts: 0,
            successes: 0,
            failures: 0,
            state: FlashblocksQualificationState::Unprobed,
        }
    }
}

#[cfg(feature = "balancer")]
impl FlashblocksQualification {
    fn snapshot(&self) -> FlashblocksQualificationSnapshot {
        FlashblocksQualificationSnapshot {
            attempts: self.attempts,
            successes: self.successes,
            failures: self.failures,
            state: self.state.clone(),
        }
    }
}

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
/// Request-size, in-flight, and request-rate limits let a heterogeneous provider
/// pool keep a low-throughput fallback available without sending it work it
/// cannot accept.
#[derive(Clone, Debug)]
#[cfg(feature = "balancer")]
pub struct EndpointConfig {
    /// Stable provider identity used for source-aware follow-up routing.
    pub id: Option<EndpointId>,
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
    /// Maximum JSON-RPC calls admitted to this endpoint in any rolling second.
    /// A batch consumes one unit per member. `None` leaves the rate unrestricted.
    pub max_requests_per_second: Option<u32>,
    /// Whether this provider pair is expected to expose a complete Flashblocks
    /// feed and matching preconfirmed state.
    pub flashblocks: bool,
}

#[cfg(feature = "balancer")]
impl EndpointConfig {
    /// Construct an endpoint with unrestricted request size, concurrency, and rate.
    pub const fn new(url: Url, weight: Weight) -> Self {
        Self {
            id: None,
            url,
            weight,
            max_request_bytes: None,
            max_in_flight: None,
            max_requests_per_second: None,
            flashblocks: false,
        }
    }

    /// Attach a stable provider identity for source-aware routing.
    pub fn with_id(mut self, id: impl Into<EndpointId>) -> Self {
        self.id = Some(id.into());
        self
    }

    /// Mark whether the provider pair supports Flashblocks.
    pub const fn with_flashblocks(mut self, supported: bool) -> Self {
        self.flashblocks = supported;
        self
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

    /// Bound JSON-RPC calls admitted to this endpoint in any rolling second.
    ///
    /// A single request consumes one unit and a batch consumes one unit per
    /// member. When the budget is exhausted, balanced and preferred routes try
    /// another eligible endpoint before waiting for capacity.
    pub const fn with_max_requests_per_second(mut self, limit: u32) -> Self {
        self.max_requests_per_second = Some(limit);
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
        let mut request_rate_limits = Vec::with_capacity(self.endpoints.len());
        let mut endpoint_ids = Vec::with_capacity(self.endpoints.len());
        let mut endpoint_flashblocks = Vec::with_capacity(self.endpoints.len());
        let mut endpoint_index_by_id = HashMap::new();
        let mut flashblocks_qualifications = HashMap::new();
        let mut throttles_by_domain = HashMap::new();

        for endpoint in self.endpoints {
            let EndpointConfig {
                id,
                url,
                weight,
                max_request_bytes,
                max_in_flight,
                max_requests_per_second,
                flashblocks,
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
            assert!(
                max_requests_per_second.is_none_or(|limit| limit > 0),
                "endpoint max_requests_per_second must be > 0 when set"
            );
            assert!(
                !flashblocks || id.is_some(),
                "Flashblocks endpoints must have an endpoint id"
            );
            if let Some(id) = &id {
                assert!(
                    endpoint_index_by_id
                        .insert(id.clone(), transports.len())
                        .is_none(),
                    "duplicate endpoint id {id}"
                );
                if flashblocks {
                    flashblocks_qualifications
                        .insert(id.clone(), FlashblocksQualification::default());
                }
            }
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
            request_rate_limits.push(
                max_requests_per_second.map(|limit| Arc::new(RequestRateLimiter::new(limit))),
            );
            endpoint_ids.push(id);
            endpoint_flashblocks.push(flashblocks);
        }

        debug!(
            endpoints = transports.len(),
            "LoadBalancedTransport created with shared HTTP clients, proactive request-rate admission, and cross-thread throttling"
        );

        LoadBalancedTransport {
            endpoints: Arc::new(transports),
            endpoint_throttles: Arc::new(throttles),
            endpoint_request_caps: Arc::new(request_caps),
            endpoint_in_flight: Arc::new(in_flight_limits),
            endpoint_request_rate_limits: Arc::new(request_rate_limits),
            endpoint_ids: Arc::new(endpoint_ids),
            endpoint_flashblocks: Arc::new(endpoint_flashblocks),
            endpoint_index_by_id: Arc::new(endpoint_index_by_id),
            flashblocks_qualifications: Arc::new(RwLock::new(flashblocks_qualifications)),
            cumulative_weights: Arc::new(cumulative),
            total_weight: total,
            counter: Arc::new(AtomicU64::new(0)),
            config: Arc::new(self.config),
            route_policy: RoutePolicy::Balanced,
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
    /// Optional rolling one-second JSON-RPC admission limits.
    endpoint_request_rate_limits: Arc<Vec<Option<Arc<RequestRateLimiter>>>>,
    /// Stable operator identities, when configured.
    endpoint_ids: Arc<Vec<Option<EndpointId>>>,
    /// Provider-level Flashblocks declarations.
    endpoint_flashblocks: Arc<Vec<bool>>,
    /// Endpoint lookup by stable identity.
    endpoint_index_by_id: Arc<HashMap<EndpointId, usize>>,
    /// Runtime qualification is shared across every scoped transport clone.
    flashblocks_qualifications: Arc<RwLock<HashMap<EndpointId, FlashblocksQualification>>>,
    /// Precomputed cumulative weight boundaries for O(log n) weighted selection.
    cumulative_weights: Arc<Vec<u64>>,
    /// Sum of all endpoint weights.
    total_weight: u64,
    /// Monotonically increasing counter for round-robin distribution.
    counter: Arc<AtomicU64>,
    /// Balancer retry/failover configuration.
    config: Arc<BalancerConfig>,
    /// Ordering policy scoped to this transport clone.
    route_policy: RoutePolicy,
}

/// A pinned Flashblocks-capable transport and its provider identity.
///
/// The lease shares connection pools, request-rate capacity, and throttle state
/// with the parent balancer, but its transport never fails over to another
/// endpoint.
#[derive(Clone)]
#[cfg(feature = "balancer")]
pub struct EndpointLease {
    endpoint_id: EndpointId,
    transport: LoadBalancedTransport,
}

#[cfg(feature = "balancer")]
impl EndpointLease {
    /// Provider pinned by this lease.
    pub const fn endpoint_id(&self) -> &EndpointId {
        &self.endpoint_id
    }

    /// Clone the pinned Alloy transport.
    pub fn transport(&self) -> LoadBalancedTransport {
        self.transport.clone()
    }
}

#[cfg(feature = "balancer")]
impl fmt::Debug for EndpointLease {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("EndpointLease")
            .field("endpoint_id", &self.endpoint_id)
            .finish_non_exhaustive()
    }
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

    /// Create a builder with per-endpoint request, concurrency, and rate capabilities.
    pub fn builder_with_endpoints(endpoints: Vec<EndpointConfig>) -> LoadBalancedTransportBuilder {
        LoadBalancedTransportBuilder {
            endpoints,
            config: BalancerConfig::default(),
            throttle_config: None,
            http_client_config: None,
        }
    }

    /// Return the routing policy scoped to this transport handle.
    pub const fn route_policy(&self) -> &RoutePolicy {
        &self.route_policy
    }

    /// Whether the named endpoint is configured for Flashblocks.
    pub fn endpoint_supports_flashblocks(&self, id: &EndpointId) -> bool {
        self.endpoint_index_by_id
            .get(id)
            .is_some_and(|&index| self.endpoint_flashblocks[index])
    }

    /// Snapshot every configured proactive request-rate limit.
    ///
    /// Endpoint URLs are deliberately excluded; use stable endpoint IDs for
    /// operator-facing metrics and diagnostics.
    pub fn endpoint_request_rate_snapshot(&self) -> Vec<EndpointRequestRateSnapshot> {
        self.endpoint_request_rate_limits
            .iter()
            .enumerate()
            .filter_map(|(index, limiter)| {
                limiter.as_ref().map(|limiter| {
                    let snapshot = limiter.snapshot();
                    EndpointRequestRateSnapshot {
                        endpoint_id: self.endpoint_ids[index].clone(),
                        max_requests_per_second: snapshot.limit,
                        used_in_rolling_window: snapshot.used,
                        remaining_in_rolling_window: snapshot.remaining,
                        total_admitted_rpc_calls: snapshot.total_admitted_rpc_calls,
                        total_capacity_rollovers: snapshot.total_capacity_rollovers,
                        total_capacity_waits: snapshot.total_capacity_waits,
                    }
                })
            })
            .collect()
    }

    /// Clone this transport with source-first routing for canonical follow-up
    /// reads. Retryable failures may still use another configured endpoint.
    pub fn prefer_endpoint(&self, id: impl Into<EndpointId>) -> Result<Self, EndpointRouteError> {
        self.with_route_policy(RoutePolicy::Prefer(id.into()))
    }

    /// Create a pinned lease for Flashblocks and preconfirmed-state reads.
    ///
    /// The endpoint must exist and be explicitly marked with
    /// [`EndpointConfig::with_flashblocks`]. The returned transport retries the
    /// same endpoint but never crosses into another provider's pending cache.
    pub fn flashblocks_lease(
        &self,
        id: impl Into<EndpointId>,
    ) -> Result<EndpointLease, EndpointRouteError> {
        let id = id.into();
        if !self.endpoint_index_by_id.contains_key(&id) {
            return Err(EndpointRouteError::UnknownEndpoint(id));
        }
        if !self.endpoint_supports_flashblocks(&id) {
            return Err(EndpointRouteError::FlashblocksUnsupported(id));
        }
        let transport = self.with_route_policy(RoutePolicy::Require(id.clone()))?;
        Ok(EndpointLease {
            endpoint_id: id,
            transport,
        })
    }

    /// Record one live Flashblocks capability/liveness probe result.
    ///
    /// Qualification is process-local and shared by every scoped clone. A
    /// later success replaces an ineligible result, allowing transient provider
    /// failures to recover without rebuilding transports.
    pub fn record_flashblocks_probe(
        &self,
        id: impl Into<EndpointId>,
        outcome: FlashblocksProbeOutcome,
    ) -> Result<(), EndpointRouteError> {
        let id = id.into();
        if !self.endpoint_index_by_id.contains_key(&id) {
            return Err(EndpointRouteError::UnknownEndpoint(id));
        }
        if !self.endpoint_supports_flashblocks(&id) {
            return Err(EndpointRouteError::FlashblocksUnsupported(id));
        }
        let mut qualifications = self
            .flashblocks_qualifications
            .write()
            .expect("Flashblocks qualification lock poisoned");
        let qualification = qualifications
            .get_mut(&id)
            .expect("declared Flashblocks endpoint missing qualification state");
        qualification.attempts = qualification.attempts.saturating_add(1);
        qualification.state = match outcome {
            FlashblocksProbeOutcome::Qualified {
                chain_id,
                mut capabilities,
            } => {
                qualification.successes = qualification.successes.saturating_add(1);
                capabilities.sort_unstable();
                capabilities.dedup();
                FlashblocksQualificationState::Qualified {
                    chain_id,
                    capabilities,
                }
            }
            FlashblocksProbeOutcome::Ineligible { reason } => {
                qualification.failures = qualification.failures.saturating_add(1);
                FlashblocksQualificationState::Ineligible { reason }
            }
        };
        Ok(())
    }

    /// Return the latest process-local qualification for a declared endpoint.
    pub fn flashblocks_qualification(
        &self,
        id: &EndpointId,
    ) -> Result<FlashblocksQualificationSnapshot, EndpointRouteError> {
        if !self.endpoint_index_by_id.contains_key(id) {
            return Err(EndpointRouteError::UnknownEndpoint(id.clone()));
        }
        if !self.endpoint_supports_flashblocks(id) {
            return Err(EndpointRouteError::FlashblocksUnsupported(id.clone()));
        }
        Ok(self
            .flashblocks_qualifications
            .read()
            .expect("Flashblocks qualification lock poisoned")
            .get(id)
            .expect("declared Flashblocks endpoint missing qualification state")
            .snapshot())
    }

    /// Create a provider-pinned lease only after the endpoint's latest runtime
    /// preflight has qualified it.
    pub fn qualified_flashblocks_lease(
        &self,
        id: impl Into<EndpointId>,
    ) -> Result<EndpointLease, EndpointRouteError> {
        let id = id.into();
        let qualification = self.flashblocks_qualification(&id)?;
        if !matches!(
            qualification.state,
            FlashblocksQualificationState::Qualified { .. }
        ) {
            return Err(EndpointRouteError::FlashblocksUnqualified(id));
        }
        self.flashblocks_lease(id)
    }

    fn with_route_policy(&self, route_policy: RoutePolicy) -> Result<Self, EndpointRouteError> {
        let id = match &route_policy {
            RoutePolicy::Balanced => None,
            RoutePolicy::Prefer(id) | RoutePolicy::Require(id) => Some(id),
        };
        if let Some(id) = id
            && !self.endpoint_index_by_id.contains_key(id)
        {
            return Err(EndpointRouteError::UnknownEndpoint(id.clone()));
        }
        let mut routed = self.clone();
        routed.route_policy = route_policy;
        Ok(routed)
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

    fn log_request_rate_denial(
        &self,
        index: usize,
        request_units: u32,
        reservation: RequestRateReservation,
    ) {
        let endpoint_id = self.endpoint_ids[index]
            .as_ref()
            .map_or("<anonymous>", EndpointId::as_str);
        match reservation {
            RequestRateReservation::Admitted => {}
            RequestRateReservation::RetryAfter(retry_after) => debug!(
                endpoint_id,
                request_units,
                retry_after_ms = retry_after.as_millis() as u64,
                "RPC endpoint request-rate capacity unavailable"
            ),
            RequestRateReservation::PacketTooLarge => debug!(
                endpoint_id,
                request_units, "JSON-RPC packet exceeds endpoint request-rate capacity"
            ),
        }
    }

    fn candidate_indices(&self, request_bytes: usize) -> Vec<usize> {
        let mut candidates = Vec::with_capacity(self.endpoints.len());
        match &self.route_policy {
            RoutePolicy::Balanced => {
                let start = self.select_endpoint();
                for attempt in 0..self.endpoints.len() {
                    let index = (start + attempt) % self.endpoints.len();
                    if self.endpoint_accepts_request(index, request_bytes) {
                        candidates.push(index);
                    }
                }
            }
            RoutePolicy::Prefer(id) => {
                let preferred = self.endpoint_index_by_id[id];
                if self.endpoint_accepts_request(preferred, request_bytes) {
                    candidates.push(preferred);
                }
                let start = self.select_endpoint();
                for attempt in 0..self.endpoints.len() {
                    let index = (start + attempt) % self.endpoints.len();
                    if index != preferred && self.endpoint_accepts_request(index, request_bytes) {
                        candidates.push(index);
                    }
                }
            }
            RoutePolicy::Require(id) => {
                let required = self.endpoint_index_by_id[id];
                if self.endpoint_accepts_request(required, request_bytes) {
                    candidates.push(required);
                }
            }
        }
        candidates
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
            let request_bytes = serde_json::to_vec(&req)
                .map(|serialized| serialized.len())
                .unwrap_or(usize::MAX);
            let request_units = u32::try_from(req.len().max(1)).unwrap_or(u32::MAX);
            let mut candidates = this.candidate_indices(request_bytes);
            if candidates.is_empty() {
                return Err(TransportErrorKind::custom_str(
                    "JSON-RPC request exceeds every endpoint allowed by the routing policy",
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
                    candidates = this.candidate_indices(request_bytes);
                }

                let mut last_err: Option<TransportError> = None;

                loop {
                    let mut rate_fallback = RequestRateFallback::default();

                    for (attempt, &idx) in candidates.iter().enumerate() {
                        if let Some(rate_limit) = &this.endpoint_request_rate_limits[idx] {
                            let reservation = rate_limit.check(request_units);
                            if rate_fallback.rejects(rate_limit, reservation) {
                                this.log_request_rate_denial(idx, request_units, reservation);
                                continue;
                            }
                        }

                        // Cross-thread domain throttle: skip heavily-throttled
                        // endpoints if non-throttled alternatives remain.
                        let throttle = &this.endpoint_throttles[idx];
                        let delay = pre_request_delay(throttle);
                        let has_unthrottled_eligible_alternative =
                            candidates[(attempt + 1)..].iter().any(|&next_idx| {
                                pre_request_delay(&this.endpoint_throttles[next_idx])
                                    < config.heavy_throttle_threshold
                            });
                        let is_preferred_source =
                            attempt == 0 && matches!(&this.route_policy, RoutePolicy::Prefer(_));
                        if !is_preferred_source
                            && delay >= config.heavy_throttle_threshold
                            && has_unthrottled_eligible_alternative
                        {
                            record_skip(throttle);
                            continue;
                        }

                        // Apply domain-level backoff delay before sending.
                        if !delay.is_zero() {
                            sleep(delay).await;
                        }

                        let permit = match &this.endpoint_in_flight[idx] {
                            Some(limit) => Some(
                                Arc::clone(limit)
                                    .acquire_owned()
                                    .await
                                    .map_err(|_| TransportErrorKind::backend_gone())?,
                            ),
                            None => None,
                        };
                        if let Some(rate_limit) = &this.endpoint_request_rate_limits[idx] {
                            let reservation = rate_limit.try_reserve(request_units);
                            if rate_fallback.rejects(rate_limit, reservation) {
                                this.log_request_rate_denial(idx, request_units, reservation);
                                drop(permit);
                                continue;
                            }
                        }
                        let mut transport = this.endpoints[idx].clone();
                        rate_fallback.record_rollovers();

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
                                    endpoint_id = this.endpoint_ids[idx]
                                        .as_ref()
                                        .map_or("<anonymous>", EndpointId::as_str),
                                    endpoint_attempt = attempt + 1,
                                    round,
                                    rate_limited,
                                    "RPC request failed, failing over: {err}"
                                );

                                last_err = Some(err);

                                if rate_limited && attempt + 1 < candidates.len() {
                                    sleep(config.rate_limit_failover_delay).await;
                                }
                            }
                        }
                    }

                    if last_err.is_none() {
                        if let Some((retry_after, rate_limit)) = rate_fallback.earliest_capacity() {
                            rate_limit.record_wait();
                            debug!(
                                request_units,
                                retry_after_ms = retry_after.as_millis() as u64,
                                "all eligible RPC endpoints lack request-rate capacity; waiting"
                            );
                            sleep(retry_after).await;
                            continue;
                        }
                        if rate_fallback.packets_too_large() == candidates.len() {
                            return Err(TransportErrorKind::custom_str(
                                "JSON-RPC packet exceeds every endpoint request-rate capacity allowed by the routing policy",
                            ));
                        }
                    }
                    break;
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
            .field("route_policy", &self.route_policy)
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
        sync::mpsc,
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

    fn fixed_http_server(bodies: Vec<&'static str>) -> (Url, thread::JoinHandle<()>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let handle = thread::spawn(move || {
            for body in bodies {
                let (mut stream, _) = listener.accept().unwrap();
                let mut request = [0_u8; 8 * 1024];
                let _ = stream.read(&mut request).unwrap();
                let response = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                );
                stream.write_all(response.as_bytes()).unwrap();
            }
        });
        (Url::parse(&format!("http://{address}")).unwrap(), handle)
    }

    fn delayed_http_server(
        delay: Duration,
        body: &'static str,
    ) -> (Url, mpsc::Receiver<()>, thread::JoinHandle<()>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let (accepted_tx, accepted_rx) = mpsc::channel();
        let handle = thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut request = [0_u8; 8 * 1024];
            let _ = stream.read(&mut request).unwrap();
            accepted_tx.send(()).unwrap();
            thread::sleep(delay);
            let response = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            );
            stream.write_all(response.as_bytes()).unwrap();
        });
        (
            Url::parse(&format!("http://{address}")).unwrap(),
            accepted_rx,
            handle,
        )
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

    fn identified_transport() -> LoadBalancedTransport {
        LoadBalancedTransport::builder_with_endpoints(vec![
            EndpointConfig::new(Url::parse("http://a.example.com").unwrap(), Weight(1))
                .with_id("a"),
            EndpointConfig::new(Url::parse("http://b.example.com").unwrap(), Weight(1))
                .with_id("b")
                .with_flashblocks(true),
            EndpointConfig::new(Url::parse("http://c.example.com").unwrap(), Weight(1))
                .with_id("c"),
        ])
        .build()
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
    fn preferred_route_orders_source_first_without_changing_parent() {
        let transport = identified_transport();
        transport.counter.store(2, Ordering::Relaxed);

        let preferred = transport.prefer_endpoint("b").unwrap();
        let candidates = preferred.candidate_indices(1);

        assert_eq!(candidates.first(), Some(&1));
        assert_eq!(candidates.len(), 3);
        assert!(matches!(transport.route_policy(), RoutePolicy::Balanced));
        assert!(matches!(
            preferred.route_policy(),
            RoutePolicy::Prefer(id) if id.as_str() == "b"
        ));
    }

    #[test]
    fn flashblocks_lease_is_pinned_to_one_declared_endpoint() {
        let transport = identified_transport();
        let lease = transport.flashblocks_lease("b").unwrap();
        let pinned = lease.transport();

        assert_eq!(lease.endpoint_id().as_str(), "b");
        assert_eq!(pinned.candidate_indices(1), vec![1]);
        assert!(matches!(
            pinned.route_policy(),
            RoutePolicy::Require(id) if id.as_str() == "b"
        ));
    }

    #[test]
    fn flashblocks_lease_rejects_undeclared_endpoint_support() {
        let transport = identified_transport();

        assert_eq!(
            transport.flashblocks_lease("a").unwrap_err(),
            EndpointRouteError::FlashblocksUnsupported(EndpointId::from("a"))
        );
        assert_eq!(
            transport.flashblocks_lease("missing").unwrap_err(),
            EndpointRouteError::UnknownEndpoint(EndpointId::from("missing"))
        );
    }

    #[test]
    fn flashblocks_qualification_is_shared_observable_and_recoverable() {
        let transport = identified_transport();
        let id = EndpointId::from("b");

        assert_eq!(
            transport
                .qualified_flashblocks_lease(id.clone())
                .unwrap_err(),
            EndpointRouteError::FlashblocksUnqualified(id.clone())
        );
        assert_eq!(
            transport.flashblocks_qualification(&id).unwrap(),
            FlashblocksQualificationSnapshot {
                attempts: 0,
                successes: 0,
                failures: 0,
                state: FlashblocksQualificationState::Unprobed,
            }
        );

        let clone = transport.clone();
        clone
            .record_flashblocks_probe(
                id.clone(),
                FlashblocksProbeOutcome::Ineligible {
                    reason: "pendingLogs subscription did not become live".to_owned(),
                },
            )
            .unwrap();
        assert_eq!(
            transport
                .qualified_flashblocks_lease(id.clone())
                .unwrap_err(),
            EndpointRouteError::FlashblocksUnqualified(id.clone())
        );

        transport
            .record_flashblocks_probe(
                id.clone(),
                FlashblocksProbeOutcome::Qualified {
                    chain_id: 8_453,
                    capabilities: vec!["flashblocks".to_owned()],
                },
            )
            .unwrap();
        let snapshot = clone.flashblocks_qualification(&id).unwrap();
        assert_eq!(snapshot.attempts, 2);
        assert_eq!(snapshot.successes, 1);
        assert_eq!(snapshot.failures, 1);
        assert!(matches!(
            snapshot.state,
            FlashblocksQualificationState::Qualified {
                chain_id: 8_453,
                ..
            }
        ));
        assert_eq!(
            transport
                .qualified_flashblocks_lease(id)
                .unwrap()
                .endpoint_id()
                .as_str(),
            "b"
        );
    }

    #[test]
    #[should_panic(expected = "Flashblocks endpoints must have an endpoint id")]
    fn flashblocks_endpoint_requires_identity() {
        LoadBalancedTransport::builder_with_endpoints(vec![
            EndpointConfig::new(Url::parse("http://a.example.com").unwrap(), Weight(1))
                .with_flashblocks(true),
        ])
        .build();
    }

    #[test]
    #[should_panic(expected = "duplicate endpoint id a")]
    fn duplicate_endpoint_identity_is_rejected() {
        LoadBalancedTransport::builder_with_endpoints(vec![
            EndpointConfig::new(Url::parse("http://a.example.com").unwrap(), Weight(1))
                .with_id("a"),
            EndpointConfig::new(Url::parse("http://b.example.com").unwrap(), Weight(1))
                .with_id("a"),
        ])
        .build();
    }

    #[test]
    #[should_panic(expected = "endpoint max_requests_per_second must be > 0 when set")]
    fn zero_endpoint_request_rate_limit_is_rejected() {
        LoadBalancedTransport::builder_with_endpoints(vec![
            EndpointConfig::new(Url::parse("http://a.example.com").unwrap(), Weight(1))
                .with_max_requests_per_second(0),
        ])
        .build();
    }

    #[test]
    fn large_endpoint_request_rate_limit_does_not_preallocate_the_window() {
        let transport = LoadBalancedTransport::builder_with_endpoints(vec![
            EndpointConfig::new(Url::parse("http://a.example.com").unwrap(), Weight(1))
                .with_id("primary")
                .with_max_requests_per_second(u32::MAX),
        ])
        .build();

        assert_eq!(
            transport.endpoint_request_rate_snapshot()[0].max_requests_per_second,
            u32::MAX
        );
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

    #[tokio::test]
    async fn preferred_endpoint_at_request_rate_limit_rolls_to_fallback() {
        let (primary_url, primary_server) =
            one_shot_http_server("200 OK", r#"{"jsonrpc":"2.0","id":3,"result":"primary"}"#);
        let (fallback_url, fallback_server) =
            one_shot_http_server("200 OK", r#"{"jsonrpc":"2.0","id":4,"result":"fallback"}"#);
        let transport = LoadBalancedTransport::builder_with_endpoints(vec![
            EndpointConfig::new(primary_url, Weight(1))
                .with_id("primary")
                .with_max_requests_per_second(1),
            EndpointConfig::new(fallback_url, Weight(1)).with_id("fallback"),
        ])
        .config(BalancerConfig {
            max_retry_rounds: 0,
            ..Default::default()
        })
        .build();
        let mut preferred = transport.prefer_endpoint("primary").unwrap();
        let first = alloy_json_rpc::Request::new("eth_blockNumber".to_owned(), Id::Number(3), ());
        let second = alloy_json_rpc::Request::new("eth_blockNumber".to_owned(), Id::Number(4), ());

        let first_response = preferred
            .call(RequestPacket::Single(first.serialize().unwrap()))
            .await
            .unwrap();
        let second_response = preferred
            .call(RequestPacket::Single(second.serialize().unwrap()))
            .await
            .unwrap();

        assert!(matches!(first_response, ResponsePacket::Single(_)));
        assert!(matches!(second_response, ResponsePacket::Single(_)));
        primary_server.join().unwrap();
        fallback_server.join().unwrap();
    }

    #[tokio::test]
    async fn endpoint_request_rate_snapshot_reports_admitted_capacity() {
        let (url, server) =
            one_shot_http_server("200 OK", r#"{"jsonrpc":"2.0","id":5,"result":"0x1"}"#);
        let mut transport = LoadBalancedTransport::builder_with_endpoints(vec![
            EndpointConfig::new(url, Weight(1))
                .with_id("quicknode")
                .with_max_requests_per_second(2),
        ])
        .build();
        let request = alloy_json_rpc::Request::new("eth_blockNumber".to_owned(), Id::Number(5), ());

        transport
            .call(RequestPacket::Single(request.serialize().unwrap()))
            .await
            .unwrap();

        assert_eq!(
            transport.endpoint_request_rate_snapshot(),
            vec![EndpointRequestRateSnapshot {
                endpoint_id: Some(EndpointId::from("quicknode")),
                max_requests_per_second: 2,
                used_in_rolling_window: 1,
                remaining_in_rolling_window: 1,
                total_admitted_rpc_calls: 1,
                total_capacity_rollovers: 0,
                total_capacity_waits: 0,
            }]
        );
        server.join().unwrap();
    }

    #[tokio::test]
    async fn sole_rate_limited_endpoint_waits_for_capacity_without_rollover() {
        let (url, server) = fixed_http_server(vec![
            r#"{"jsonrpc":"2.0","id":6,"result":"0x1"}"#,
            r#"{"jsonrpc":"2.0","id":7,"result":"0x2"}"#,
        ]);
        let mut transport = LoadBalancedTransport::builder_with_endpoints(vec![
            EndpointConfig::new(url, Weight(1))
                .with_id("quicknode")
                .with_max_requests_per_second(1),
        ])
        .build();
        let first = alloy_json_rpc::Request::new("eth_blockNumber".to_owned(), Id::Number(6), ());
        let second = alloy_json_rpc::Request::new("eth_blockNumber".to_owned(), Id::Number(7), ());
        let first_started_at = tokio::time::Instant::now();

        transport
            .call(RequestPacket::Single(first.serialize().unwrap()))
            .await
            .unwrap();
        transport
            .call(RequestPacket::Single(second.serialize().unwrap()))
            .await
            .unwrap();

        assert!(first_started_at.elapsed() >= Duration::from_secs(1));
        let snapshot = transport.endpoint_request_rate_snapshot().pop().unwrap();
        assert_eq!(snapshot.total_admitted_rpc_calls, 2);
        assert_eq!(snapshot.total_capacity_rollovers, 0);
        assert_eq!(snapshot.total_capacity_waits, 1);
        server.join().unwrap();
    }

    #[tokio::test]
    async fn batch_members_each_consume_request_rate_capacity() {
        let primary_url = Url::parse("http://127.0.0.1:1").unwrap();
        let (fallback_url, fallback_server) = one_shot_http_server(
            "200 OK",
            r#"[{"jsonrpc":"2.0","id":8,"result":"0x1"},{"jsonrpc":"2.0","id":9,"result":"0x2"}]"#,
        );
        let mut transport = LoadBalancedTransport::builder_with_endpoints(vec![
            EndpointConfig::new(primary_url, Weight(1))
                .with_id("quicknode")
                .with_max_requests_per_second(1),
            EndpointConfig::new(fallback_url, Weight(1)).with_id("fallback"),
        ])
        .config(BalancerConfig {
            max_retry_rounds: 0,
            ..Default::default()
        })
        .build()
        .prefer_endpoint("quicknode")
        .unwrap();
        let first = alloy_json_rpc::Request::new("eth_blockNumber".to_owned(), Id::Number(8), ());
        let second = alloy_json_rpc::Request::new("eth_chainId".to_owned(), Id::Number(9), ());

        let response = transport
            .call(RequestPacket::Batch(vec![
                first.serialize().unwrap(),
                second.serialize().unwrap(),
            ]))
            .await
            .unwrap();

        assert!(matches!(response, ResponsePacket::Batch(_)));
        let snapshot = transport.endpoint_request_rate_snapshot().pop().unwrap();
        assert_eq!(snapshot.total_admitted_rpc_calls, 0);
        assert_eq!(snapshot.total_capacity_rollovers, 1);
        fallback_server.join().unwrap();
    }

    #[tokio::test]
    async fn packet_larger_than_every_rate_capacity_fails_without_network_io() {
        let mut transport = LoadBalancedTransport::builder_with_endpoints(vec![
            EndpointConfig::new(Url::parse("http://127.0.0.1:1").unwrap(), Weight(1))
                .with_id("quicknode")
                .with_max_requests_per_second(1),
        ])
        .build();
        let first = alloy_json_rpc::Request::new("eth_blockNumber".to_owned(), Id::Number(20), ());
        let second = alloy_json_rpc::Request::new("eth_chainId".to_owned(), Id::Number(21), ());

        let error = transport
            .call(RequestPacket::Batch(vec![
                first.serialize().unwrap(),
                second.serialize().unwrap(),
            ]))
            .await
            .unwrap_err();

        assert!(
            error
                .to_string()
                .contains("exceeds every endpoint request-rate capacity")
        );
        let snapshot = transport.endpoint_request_rate_snapshot().pop().unwrap();
        assert_eq!(snapshot.total_admitted_rpc_calls, 0);
        assert_eq!(snapshot.total_capacity_rollovers, 0);
        assert_eq!(snapshot.total_capacity_waits, 0);
    }

    #[tokio::test]
    async fn concurrent_clones_cannot_overshoot_request_rate_limit() {
        const PRIMARY_CAPACITY: usize = 5;
        const TOTAL_REQUESTS: usize = 20;
        let response = r#"{"jsonrpc":"2.0","id":10,"result":"0x1"}"#;
        let (primary_url, primary_server) = fixed_http_server(vec![response; PRIMARY_CAPACITY]);
        let (fallback_url, fallback_server) =
            fixed_http_server(vec![response; TOTAL_REQUESTS - PRIMARY_CAPACITY]);
        let transport = LoadBalancedTransport::builder_with_endpoints(vec![
            EndpointConfig::new(primary_url, Weight(1))
                .with_id("quicknode")
                .with_max_requests_per_second(PRIMARY_CAPACITY as u32),
            EndpointConfig::new(fallback_url, Weight(1)).with_id("fallback"),
        ])
        .config(BalancerConfig {
            max_retry_rounds: 0,
            ..Default::default()
        })
        .build()
        .prefer_endpoint("quicknode")
        .unwrap();

        let tasks = (0..TOTAL_REQUESTS)
            .map(|request_id| {
                let mut transport = transport.clone();
                tokio::spawn(async move {
                    let request = alloy_json_rpc::Request::new(
                        "eth_blockNumber".to_owned(),
                        Id::Number(request_id as u64 + 10),
                        (),
                    );
                    transport
                        .call(RequestPacket::Single(request.serialize().unwrap()))
                        .await
                        .unwrap();
                })
            })
            .collect::<Vec<_>>();
        for task in tasks {
            task.await.unwrap();
        }

        let snapshot = transport.endpoint_request_rate_snapshot().pop().unwrap();
        assert_eq!(snapshot.used_in_rolling_window, PRIMARY_CAPACITY as u32);
        assert_eq!(snapshot.total_admitted_rpc_calls, PRIMARY_CAPACITY as u64);
        assert_eq!(
            snapshot.total_capacity_rollovers,
            (TOTAL_REQUESTS - PRIMARY_CAPACITY) as u64
        );
        primary_server.join().unwrap();
        fallback_server.join().unwrap();
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn full_rate_budget_rolls_over_before_waiting_for_in_flight_permit() {
        let (primary_url, primary_accepted, primary_server) = delayed_http_server(
            Duration::from_millis(500),
            r#"{"jsonrpc":"2.0","id":30,"result":"primary"}"#,
        );
        let (fallback_url, fallback_server) =
            one_shot_http_server("200 OK", r#"{"jsonrpc":"2.0","id":31,"result":"fallback"}"#);
        let transport = LoadBalancedTransport::builder_with_endpoints(vec![
            EndpointConfig::new(primary_url, Weight(1))
                .with_id("quicknode")
                .with_max_in_flight(1)
                .with_max_requests_per_second(1),
            EndpointConfig::new(fallback_url, Weight(1)).with_id("fallback"),
        ])
        .config(BalancerConfig {
            max_retry_rounds: 0,
            ..Default::default()
        })
        .build()
        .prefer_endpoint("quicknode")
        .unwrap();
        let mut first_transport = transport.clone();
        let first = tokio::spawn(async move {
            let request =
                alloy_json_rpc::Request::new("eth_blockNumber".to_owned(), Id::Number(30), ());
            first_transport
                .call(RequestPacket::Single(request.serialize().unwrap()))
                .await
                .unwrap();
        });
        primary_accepted
            .recv_timeout(Duration::from_secs(2))
            .unwrap();
        let mut second_transport = transport.clone();
        let second = alloy_json_rpc::Request::new("eth_chainId".to_owned(), Id::Number(31), ());

        tokio::time::timeout(
            Duration::from_millis(250),
            second_transport.call(RequestPacket::Single(second.serialize().unwrap())),
        )
        .await
        .expect("fallback should not wait for the primary in-flight permit")
        .unwrap();

        first.await.unwrap();
        let snapshot = transport.endpoint_request_rate_snapshot().pop().unwrap();
        assert_eq!(snapshot.total_admitted_rpc_calls, 1);
        assert_eq!(snapshot.total_capacity_rollovers, 1);
        primary_server.join().unwrap();
        fallback_server.join().unwrap();
    }

    #[tokio::test]
    async fn pinned_endpoint_lease_shares_rate_budget_and_never_fails_over() {
        let (primary_url, primary_server) = fixed_http_server(vec![
            r#"{"jsonrpc":"2.0","id":32,"result":"0x1"}"#,
            r#"{"jsonrpc":"2.0","id":33,"result":"0x2"}"#,
        ]);
        let fallback_url = Url::parse("http://127.0.0.1:1").unwrap();
        let transport = LoadBalancedTransport::builder_with_endpoints(vec![
            EndpointConfig::new(primary_url, Weight(1))
                .with_id("quicknode")
                .with_flashblocks(true)
                .with_max_requests_per_second(1),
            EndpointConfig::new(fallback_url, Weight(1)).with_id("fallback"),
        ])
        .build();
        let mut initial = transport.clone();
        let first = alloy_json_rpc::Request::new("eth_blockNumber".to_owned(), Id::Number(32), ());
        let first_started_at = tokio::time::Instant::now();
        initial
            .call(RequestPacket::Single(first.serialize().unwrap()))
            .await
            .unwrap();
        let mut pinned = transport
            .flashblocks_lease("quicknode")
            .unwrap()
            .transport();
        let second = alloy_json_rpc::Request::new("eth_chainId".to_owned(), Id::Number(33), ());

        pinned
            .call(RequestPacket::Single(second.serialize().unwrap()))
            .await
            .unwrap();

        assert!(first_started_at.elapsed() >= Duration::from_secs(1));
        let snapshot = transport.endpoint_request_rate_snapshot().pop().unwrap();
        assert_eq!(snapshot.total_admitted_rpc_calls, 2);
        assert_eq!(snapshot.total_capacity_rollovers, 0);
        assert_eq!(snapshot.total_capacity_waits, 1);
        primary_server.join().unwrap();
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
