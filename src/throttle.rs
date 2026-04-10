//! Cross-thread domain throttle with atomic state machine.
//!
//! Provides rate-limit coordination across all transport instances targeting
//! the same RPC provider domain. When any thread receives a 429 response,
//! all threads sharing that domain see the updated backoff immediately via
//! lock-free atomic operations.
//!
//! Key design properties:
//! - **Lock-free hot path**: all reads use `Relaxed` atomics (no locks)
//! - **Asymmetric recovery**: escalation is immediate (one 429 bumps the level),
//!   recovery requires `recovery_threshold` consecutive successes per level
//! - **Time-based decay**: automatically drops backoff levels if no 429 is
//!   received for `decay_interval_ms * current_level`, preventing permanent stalls
//! - **Domain grouping**: URLs are mapped to rate-limit groups via
//!   [`extract_rate_limit_domain`], so `arb-mainnet.g.alchemy.com` and
//!   `base-mainnet.g.alchemy.com` share the same throttle state

use std::{
    collections::HashMap,
    sync::{
        Arc, OnceLock, RwLock,
        atomic::{AtomicU32, AtomicU64, Ordering},
    },
    time::{Duration, SystemTime, UNIX_EPOCH},
};

/// Configuration for domain throttle behavior.
///
/// Controls backoff escalation, recovery, time-based decay, and the
/// delay curve applied when a domain is rate-limited.
#[derive(Clone, Debug)]
pub struct ThrottleConfig {
    /// Maximum backoff level (index into `backoff_table`). Default: 5.
    pub max_backoff_level: u32,
    /// Consecutive successes required to drop one backoff level. Default: 5.
    pub recovery_threshold: u32,
    /// Time-based auto-recovery interval in milliseconds.
    ///
    /// If no 429 response has been received for this many milliseconds,
    /// one backoff level is automatically dropped. Each additional interval
    /// drops one more level. This prevents "throttle death spirals" where
    /// the system is too throttled to send enough requests to accumulate
    /// the `recovery_threshold` consecutive successes needed for manual recovery.
    ///
    /// Example with default (10,000ms): at backoff level 3, if 25 seconds
    /// pass with no 429, two levels are dropped (25,000 / 10,000 = 2),
    /// bringing the level down to 1.
    ///
    /// Default: 10,000 (10 seconds per level).
    pub decay_interval_ms: u64,
    /// Delay in milliseconds for each backoff level. Length must be
    /// `max_backoff_level + 1`. Default: `[0, 50, 150, 400, 1000, 2000]`.
    pub backoff_table: Vec<Duration>,
}

impl Default for ThrottleConfig {
    fn default() -> Self {
        Self {
            max_backoff_level: 5,
            recovery_threshold: 5,
            decay_interval_ms: 10_000,
            backoff_table: vec![
                Duration::ZERO,
                Duration::from_millis(50),
                Duration::from_millis(150),
                Duration::from_millis(400),
                Duration::from_millis(1000),
                Duration::from_millis(2000),
            ],
        }
    }
}

/// Per-domain rate-limit state shared across all chain threads.
///
/// All fields are atomics for lock-free access on the hot path.
/// A single `DomainThrottleState` is shared (via `Arc`) by every
/// `LoadBalancedTransport` endpoint that resolves to the same domain.
pub struct DomainThrottleState {
    /// Monotonic timestamp (millis since UNIX epoch) of the last 429 received.
    last_rate_limit_at: AtomicU64,
    /// Current backoff level: 0 = no backoff, 1..=max_backoff_level = escalating.
    backoff_level: AtomicU32,
    /// Consecutive successes since last rate-limit. After recovery_threshold,
    /// backoff_level is decremented by 1 and this resets to 0.
    consecutive_successes: AtomicU32,
    /// Total 429 responses received on this domain (monotonically increasing).
    total_rate_limits: AtomicU64,
    /// Total successful responses on this domain (monotonically increasing).
    total_successes: AtomicU64,
    /// Total requests where this domain was skipped due to heavy throttling.
    total_skips: AtomicU64,
    /// Configuration for this domain's throttle behavior.
    config: Arc<ThrottleConfig>,
}

/// Process-wide domain throttle registry.
///
/// Keyed by the "rate-limit group" extracted from the URL host (e.g. `alchemy.com`).
/// Writes (inserting a new domain) are rare — typically 3-5 total at startup.
/// Reads (looking up existing domains) are the hot path and only take a read lock.
struct ThrottleRegistry {
    domains: HashMap<String, Arc<DomainThrottleState>>,
    default_config: Arc<ThrottleConfig>,
}

static REGISTRY: OnceLock<RwLock<ThrottleRegistry>> = OnceLock::new();

fn registry() -> &'static RwLock<ThrottleRegistry> {
    REGISTRY.get_or_init(|| {
        RwLock::new(ThrottleRegistry {
            domains: HashMap::new(),
            default_config: Arc::new(ThrottleConfig::default()),
        })
    })
}

fn now_millis() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

/// Set the process-wide default throttle configuration.
///
/// Newly registered domains will inherit this config. Already-registered
/// domains keep their existing configuration.
///
/// # Examples
///
/// ```
/// use alloy_transport_balancer::{ThrottleConfig, set_default_throttle_config};
/// use std::time::Duration;
///
/// set_default_throttle_config(ThrottleConfig {
///     recovery_threshold: 3,
///     ..Default::default()
/// });
/// ```
pub fn set_default_throttle_config(config: ThrottleConfig) {
    let reg = registry();
    let mut r = reg.write().unwrap_or_else(|e| e.into_inner());
    r.default_config = Arc::new(config);
}

/// Get or create the throttle state for a domain.
///
/// The `domain` should be the rate-limit group key (see [`extract_rate_limit_domain`]).
///
/// # Examples
///
/// ```
/// use alloy_transport_balancer::domain_throttle;
///
/// let state = domain_throttle("alchemy.com");
/// // Second lookup returns the same Arc
/// let same = domain_throttle("alchemy.com");
/// assert!(std::sync::Arc::ptr_eq(&state, &same));
/// ```
pub fn domain_throttle(domain: &str) -> Arc<DomainThrottleState> {
    let reg = registry();

    // Fast path: read lock (non-blocking with concurrent readers).
    {
        let r = reg.read().unwrap_or_else(|e| e.into_inner());
        if let Some(state) = r.domains.get(domain) {
            return Arc::clone(state);
        }
    }

    // Slow path: write lock to insert new domain (happens once per domain).
    let mut r = reg.write().unwrap_or_else(|e| e.into_inner());
    let config = Arc::clone(&r.default_config);
    Arc::clone(r.domains.entry(domain.to_owned()).or_insert_with(|| {
        Arc::new(DomainThrottleState {
            last_rate_limit_at: AtomicU64::new(0),
            backoff_level: AtomicU32::new(0),
            consecutive_successes: AtomicU32::new(0),
            total_rate_limits: AtomicU64::new(0),
            total_successes: AtomicU64::new(0),
            total_skips: AtomicU64::new(0),
            config,
        })
    }))
}

/// Compute the delay to apply before sending a request to a throttled domain.
///
/// Includes time-based decay: if no rate-limit has been received for a while,
/// backoff levels are automatically dropped to prevent permanently stuck throttle.
/// Returns `Duration::ZERO` if no backoff is active.
///
/// # Examples
///
/// ```
/// use alloy_transport_balancer::{domain_throttle, pre_request_delay, record_rate_limit};
///
/// let state = domain_throttle("example-delay.com");
/// assert!(pre_request_delay(&state).is_zero());
///
/// record_rate_limit(&state);
/// assert!(pre_request_delay(&state) > std::time::Duration::ZERO);
/// ```
pub fn pre_request_delay(state: &DomainThrottleState) -> Duration {
    let level = state.backoff_level.load(Ordering::Relaxed);
    if level == 0 {
        return Duration::ZERO;
    }

    let config = &state.config;

    // Time-based decay: if enough time has passed since the last rate-limit,
    // automatically drop levels. This breaks throttle death spirals where
    // the system is too throttled to accumulate enough successes for recovery.
    let last_rl = state.last_rate_limit_at.load(Ordering::Relaxed);
    if last_rl > 0 {
        let elapsed_ms = now_millis().saturating_sub(last_rl);
        let decayable = (elapsed_ms / config.decay_interval_ms) as u32;
        if decayable > 0 {
            state
                .backoff_level
                .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |cur| {
                    Some(cur.saturating_sub(decayable.min(cur)))
                })
                .ok();
        }
    }

    let level = state.backoff_level.load(Ordering::Relaxed);
    let idx = level.min(config.max_backoff_level) as usize;
    config
        .backoff_table
        .get(idx)
        .copied()
        .unwrap_or(Duration::from_millis(2000))
}

/// Record a 429 rate-limit response from this domain.
///
/// Escalates backoff for all transport instances sharing this domain.
///
/// # Examples
///
/// ```
/// use alloy_transport_balancer::{domain_throttle, record_rate_limit, pre_request_delay};
///
/// let state = domain_throttle("example-rl.com");
/// record_rate_limit(&state);
/// // Backoff is now active
/// assert!(!pre_request_delay(&state).is_zero());
/// ```
pub fn record_rate_limit(state: &DomainThrottleState) {
    state
        .last_rate_limit_at
        .store(now_millis(), Ordering::Relaxed);
    state.consecutive_successes.store(0, Ordering::Relaxed);
    state.total_rate_limits.fetch_add(1, Ordering::Relaxed);
    let max = state.config.max_backoff_level;
    state
        .backoff_level
        .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |cur| {
            Some(cur.saturating_add(1).min(max))
        })
        .ok();
}

/// Record that an endpoint was skipped due to heavy throttling.
pub(crate) fn record_skip(state: &DomainThrottleState) {
    state.total_skips.fetch_add(1, Ordering::Relaxed);
}

/// Record a successful response from this domain.
///
/// After `recovery_threshold` consecutive successes, drops one backoff level.
///
/// # Examples
///
/// ```
/// use alloy_transport_balancer::{domain_throttle, record_rate_limit, record_success};
///
/// let state = domain_throttle("example-success.com");
/// record_rate_limit(&state);
/// // Multiple successes gradually reduce backoff
/// for _ in 0..10 {
///     record_success(&state);
/// }
/// ```
pub fn record_success(state: &DomainThrottleState) {
    state.total_successes.fetch_add(1, Ordering::Relaxed);
    let level = state.backoff_level.load(Ordering::Relaxed);
    if level == 0 {
        return;
    }

    let prev = state.consecutive_successes.fetch_add(1, Ordering::Relaxed);
    if prev + 1 >= state.config.recovery_threshold {
        state.consecutive_successes.store(0, Ordering::Relaxed);
        state
            .backoff_level
            .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |cur| {
                Some(cur.saturating_sub(1))
            })
            .ok();
    }
}

/// Extract the rate-limit group from a URL host.
///
/// Takes the last two segments of the hostname, which groups subdomains
/// belonging to the same provider:
/// - `arb-mainnet.g.alchemy.com` → `alchemy.com`
/// - `lb.drpc.org` → `drpc.org`
/// - `arb1.arbitrum.io` → `arbitrum.io`
/// - `mainnet.base.org` → `base.org`
///
/// # Examples
///
/// ```
/// use alloy_transport_balancer::extract_rate_limit_domain;
///
/// assert_eq!(extract_rate_limit_domain("arb-mainnet.g.alchemy.com"), "alchemy.com");
/// assert_eq!(extract_rate_limit_domain("lb.drpc.org"), "drpc.org");
/// assert_eq!(extract_rate_limit_domain("localhost"), "localhost");
/// ```
pub fn extract_rate_limit_domain(host: &str) -> &str {
    let parts: Vec<&str> = host.rsplitn(3, '.').collect();
    if parts.len() >= 2 {
        // Find the start index of the second-to-last segment.
        // rsplitn(3, '.') on "a.b.c.d" gives ["d", "c", "a.b"].
        // We want "c.d", so we compute the offset.
        let tld = parts[0];
        let sld = parts[1];
        let start = host.len() - tld.len() - 1 - sld.len();
        &host[start..]
    } else {
        host
    }
}

/// Returns the maximum pre-request delay across all registered domains.
///
/// This reflects the worst-case domain backoff. Prefer [`weighted_domain_backoff`]
/// for adaptive prefetch decisions, as `max` lets a single low-traffic sick domain
/// penalize all operations.
///
/// # Examples
///
/// ```
/// use alloy_transport_balancer::max_domain_backoff;
///
/// let delay = max_domain_backoff();
/// // Returns Duration::ZERO when no domains are throttled
/// ```
pub fn max_domain_backoff() -> Duration {
    let reg = registry();
    let r = reg.read().unwrap_or_else(|e| e.into_inner());
    r.domains
        .values()
        .map(|state| pre_request_delay(state))
        .max()
        .unwrap_or(Duration::ZERO)
}

/// Returns a traffic-weighted average of pre-request delays across all domains.
///
/// Weights each domain's delay by its `total_successes` count, so high-traffic
/// healthy domains dilute the impact of low-traffic sick domains. This prevents
/// a rarely-used fallback endpoint (e.g., weight=1 public RPC) from dragging
/// down the entire prefetch system.
///
/// Falls back to [`max_domain_backoff`] if no successes have been recorded yet
/// (e.g., at startup before any requests).
///
/// # Examples
///
/// ```
/// use alloy_transport_balancer::weighted_domain_backoff;
///
/// // Traffic-weighted average — healthy high-traffic domains dilute
/// // the impact of throttled low-traffic domains
/// let delay = weighted_domain_backoff();
/// ```
pub fn weighted_domain_backoff() -> Duration {
    let reg = registry();
    let r = reg.read().unwrap_or_else(|e| e.into_inner());
    let mut total_weight: u64 = 0;
    let mut weighted_delay: u64 = 0;
    for state in r.domains.values() {
        let successes = state.total_successes.load(Ordering::Relaxed);
        let delay_ms = pre_request_delay(state).as_millis() as u64;
        // Use successes as weight; domains with no traffic yet get weight=1
        // so they still contribute but don't dominate.
        let weight = successes.max(1);
        weighted_delay += delay_ms * weight;
        total_weight += weight;
    }
    weighted_delay
        .checked_div(total_weight)
        .map_or(Duration::ZERO, Duration::from_millis)
}

/// Snapshot of a single domain's throttle state for observability.
#[derive(Debug, Clone)]
pub struct DomainThrottleSnapshot {
    /// The rate-limit domain name (e.g., `"alchemy.com"`).
    pub domain: String,
    /// Current backoff level (0 = healthy, higher = more throttled).
    pub backoff_level: u32,
    /// Current pre-request delay in milliseconds.
    pub delay_ms: u64,
    /// Total 429 responses received on this domain since startup.
    pub total_rate_limits: u64,
    /// Total successful responses on this domain since startup.
    pub total_successes: u64,
    /// Total requests skipped due to heavy throttling on this domain.
    pub total_skips: u64,
}

/// Returns a snapshot of all registered domain throttle states.
///
/// Useful for periodic logging / diagnostics.
///
/// # Examples
///
/// ```
/// use alloy_transport_balancer::throttle_snapshot;
///
/// for snap in throttle_snapshot() {
///     println!("{}: level={}, delay={}ms", snap.domain, snap.backoff_level, snap.delay_ms);
/// }
/// ```
pub fn throttle_snapshot() -> Vec<DomainThrottleSnapshot> {
    let reg = registry();
    let r = reg.read().unwrap_or_else(|e| e.into_inner());
    r.domains
        .iter()
        .map(|(domain, state)| DomainThrottleSnapshot {
            domain: domain.clone(),
            backoff_level: state.backoff_level.load(Ordering::Relaxed),
            delay_ms: pre_request_delay(state).as_millis() as u64,
            total_rate_limits: state.total_rate_limits.load(Ordering::Relaxed),
            total_successes: state.total_successes.load(Ordering::Relaxed),
            total_skips: state.total_skips.load(Ordering::Relaxed),
        })
        .collect()
}

/// Log a summary of all domain throttle states at INFO level.
///
/// # Examples
///
/// ```
/// use alloy_transport_balancer::log_throttle_summary;
///
/// // Logs all domain states via tracing at INFO/DEBUG level
/// log_throttle_summary();
/// ```
pub fn log_throttle_summary() {
    for snap in throttle_snapshot() {
        if snap.total_rate_limits > 0 || snap.backoff_level > 0 {
            tracing::info!(
                domain = snap.domain,
                backoff_level = snap.backoff_level,
                delay_ms = snap.delay_ms,
                total_429s = snap.total_rate_limits,
                total_ok = snap.total_successes,
                total_skips = snap.total_skips,
                "domain throttle status"
            );
        } else {
            tracing::debug!(
                domain = snap.domain,
                total_ok = snap.total_successes,
                "domain throttle status (healthy)"
            );
        }
    }
}

/// Record that an entire batch was rejected by this domain.
///
/// Escalates by 1 backoff level (same as a single 429). The batch rejection
/// is already handled by the individual fallback path in the batching layer,
/// so aggressive double-escalation is unnecessary and can cause throttle
/// death spirals where the system becomes too slow to recover.
///
/// # Examples
///
/// ```
/// use alloy_transport_balancer::{domain_throttle, record_batch_rejection, pre_request_delay};
///
/// let state = domain_throttle("example-batch.com");
/// record_batch_rejection(&state);
/// assert!(!pre_request_delay(&state).is_zero());
/// ```
pub fn record_batch_rejection(state: &DomainThrottleState) {
    state
        .last_rate_limit_at
        .store(now_millis(), Ordering::Relaxed);
    state.consecutive_successes.store(0, Ordering::Relaxed);
    state.total_rate_limits.fetch_add(1, Ordering::Relaxed);
    let max = state.config.max_backoff_level;
    state
        .backoff_level
        .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |cur| {
            Some(cur.saturating_add(1).min(max))
        })
        .ok();
}

/// Reset all throttle state (for testing).
#[cfg(test)]
pub(crate) fn reset_registry() {
    let reg = registry();
    let mut r = reg.write().unwrap_or_else(|e| e.into_inner());
    r.domains.clear();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extract_domain_alchemy() {
        assert_eq!(
            extract_rate_limit_domain("arb-mainnet.g.alchemy.com"),
            "alchemy.com"
        );
        assert_eq!(
            extract_rate_limit_domain("base-mainnet.g.alchemy.com"),
            "alchemy.com"
        );
        assert_eq!(
            extract_rate_limit_domain("eth-mainnet.g.alchemy.com"),
            "alchemy.com"
        );
    }

    #[test]
    fn extract_domain_drpc() {
        assert_eq!(extract_rate_limit_domain("lb.drpc.org"), "drpc.org");
    }

    #[test]
    fn extract_domain_arbitrum() {
        assert_eq!(extract_rate_limit_domain("arb1.arbitrum.io"), "arbitrum.io");
    }

    #[test]
    fn extract_domain_base() {
        assert_eq!(extract_rate_limit_domain("mainnet.base.org"), "base.org");
    }

    #[test]
    fn extract_domain_bare_two_parts() {
        assert_eq!(extract_rate_limit_domain("example.com"), "example.com");
    }

    #[test]
    fn extract_domain_single() {
        assert_eq!(extract_rate_limit_domain("localhost"), "localhost");
    }

    #[test]
    fn backoff_escalation() {
        reset_registry();
        let state = domain_throttle("test-escalation.com");

        assert_eq!(pre_request_delay(&state), Duration::ZERO);

        record_rate_limit(&state);
        assert_eq!(pre_request_delay(&state), Duration::from_millis(50));

        record_rate_limit(&state);
        assert_eq!(pre_request_delay(&state), Duration::from_millis(150));

        record_rate_limit(&state);
        assert_eq!(pre_request_delay(&state), Duration::from_millis(400));

        record_rate_limit(&state);
        assert_eq!(pre_request_delay(&state), Duration::from_millis(1000));

        record_rate_limit(&state);
        assert_eq!(pre_request_delay(&state), Duration::from_millis(2000));
    }

    #[test]
    fn backoff_caps_at_max() {
        reset_registry();
        let state = domain_throttle("test-cap.com");

        for _ in 0..20 {
            record_rate_limit(&state);
        }
        assert_eq!(pre_request_delay(&state), Duration::from_millis(2000));
        assert_eq!(
            state.backoff_level.load(Ordering::Relaxed),
            state.config.max_backoff_level
        );
    }

    #[test]
    fn recovery_after_consecutive_successes() {
        reset_registry();
        let state = domain_throttle("test-recovery.com");

        // Escalate to level 2
        record_rate_limit(&state);
        record_rate_limit(&state);
        assert_eq!(state.backoff_level.load(Ordering::Relaxed), 2);

        // Freeze last_rate_limit_at to now so time-based decay doesn't interfere
        state
            .last_rate_limit_at
            .store(now_millis(), Ordering::Relaxed);

        // 5 successes → drop to level 1
        for _ in 0..state.config.recovery_threshold {
            record_success(&state);
        }
        assert_eq!(state.backoff_level.load(Ordering::Relaxed), 1);

        // 5 more → drop to level 0
        for _ in 0..state.config.recovery_threshold {
            record_success(&state);
        }
        assert_eq!(state.backoff_level.load(Ordering::Relaxed), 0);
        assert_eq!(pre_request_delay(&state), Duration::ZERO);
    }

    #[test]
    fn rate_limit_resets_success_counter() {
        reset_registry();
        let state = domain_throttle("test-reset.com");

        record_rate_limit(&state);
        // Pin last_rate_limit_at to now so time-based decay doesn't interfere
        state
            .last_rate_limit_at
            .store(now_millis(), Ordering::Relaxed);
        // Accumulate 4 successes (not enough to recover with threshold=5)
        for _ in 0..state.config.recovery_threshold - 1 {
            record_success(&state);
        }
        assert_eq!(state.backoff_level.load(Ordering::Relaxed), 1);

        // Another 429 resets the counter
        record_rate_limit(&state);
        assert_eq!(state.consecutive_successes.load(Ordering::Relaxed), 0);
        assert_eq!(state.backoff_level.load(Ordering::Relaxed), 2);
    }

    #[test]
    fn success_at_level_zero_is_noop() {
        reset_registry();
        let state = domain_throttle("test-noop.com");

        // Many successes at level 0 should not underflow or cause issues
        for _ in 0..100 {
            record_success(&state);
        }
        assert_eq!(state.backoff_level.load(Ordering::Relaxed), 0);
    }

    #[test]
    fn shared_state_across_lookups() {
        reset_registry();
        let a = domain_throttle("shared-test.com");
        let b = domain_throttle("shared-test.com");

        record_rate_limit(&a);
        assert_eq!(b.backoff_level.load(Ordering::Relaxed), 1);
    }

    #[test]
    fn different_domains_are_independent() {
        reset_registry();
        let a = domain_throttle("domain-a.com");
        let b = domain_throttle("domain-b.com");

        record_rate_limit(&a);
        assert_eq!(a.backoff_level.load(Ordering::Relaxed), 1);
        assert_eq!(b.backoff_level.load(Ordering::Relaxed), 0);
    }

    #[test]
    fn batch_rejection_escalates_by_one() {
        reset_registry();
        let state = domain_throttle("test-batch-rejection.com");

        assert_eq!(pre_request_delay(&state), Duration::ZERO);

        // Batch rejection should escalate by 1 level (same as single 429)
        record_batch_rejection(&state);
        assert_eq!(state.backoff_level.load(Ordering::Relaxed), 1);
        assert_eq!(pre_request_delay(&state), Duration::from_millis(50));

        // Another batch rejection: level 1 → 2
        record_batch_rejection(&state);
        assert_eq!(state.backoff_level.load(Ordering::Relaxed), 2);
        assert_eq!(pre_request_delay(&state), Duration::from_millis(150));

        // Keep escalating: 2 → 3 → 4 → 5 (cap)
        record_batch_rejection(&state);
        assert_eq!(state.backoff_level.load(Ordering::Relaxed), 3);
        record_batch_rejection(&state);
        assert_eq!(state.backoff_level.load(Ordering::Relaxed), 4);
        record_batch_rejection(&state);
        assert_eq!(
            state.backoff_level.load(Ordering::Relaxed),
            state.config.max_backoff_level
        );
        assert_eq!(pre_request_delay(&state), Duration::from_millis(2000));
    }

    #[test]
    fn time_based_decay_drops_levels() {
        reset_registry();
        let state = domain_throttle("test-time-decay.com");

        // Escalate to level 3
        record_rate_limit(&state);
        record_rate_limit(&state);
        record_rate_limit(&state);
        assert_eq!(state.backoff_level.load(Ordering::Relaxed), 3);

        // Simulate 20 seconds passing (should decay 2 levels: 20s / 10s = 2)
        state.last_rate_limit_at.store(
            now_millis().saturating_sub(state.config.decay_interval_ms * 2),
            Ordering::Relaxed,
        );
        let delay = pre_request_delay(&state);
        assert_eq!(state.backoff_level.load(Ordering::Relaxed), 1);
        assert_eq!(delay, Duration::from_millis(50));
    }

    #[test]
    fn time_based_decay_fully_recovers() {
        reset_registry();
        let state = domain_throttle("test-full-decay.com");

        // Escalate to level 4
        for _ in 0..4 {
            record_rate_limit(&state);
        }
        assert_eq!(state.backoff_level.load(Ordering::Relaxed), 4);

        // Simulate 60 seconds passing (should decay all 4 levels: 60s / 10s = 6 > 4)
        state.last_rate_limit_at.store(
            now_millis().saturating_sub(state.config.decay_interval_ms * 6),
            Ordering::Relaxed,
        );
        let delay = pre_request_delay(&state);
        assert_eq!(state.backoff_level.load(Ordering::Relaxed), 0);
        assert_eq!(delay, Duration::ZERO);
    }

    #[test]
    fn weighted_domain_backoff_dilutes_sick_domain() {
        reset_registry();

        // Healthy high-traffic domain
        let healthy = domain_throttle("healthy-weighted.com");
        for _ in 0..1000 {
            record_success(&healthy);
        }

        // Sick low-traffic domain at level 3 (400ms)
        let sick = domain_throttle("sick-weighted.com");
        record_rate_limit(&sick);
        record_rate_limit(&sick);
        record_rate_limit(&sick);
        // Give it a few successes so it has some weight, but stay below
        // recovery_threshold (5) to avoid dropping backoff levels.
        for _ in 0..4 {
            record_success(&sick);
        }

        // max_domain_backoff should be 400ms (the sick domain)
        let max_delay = max_domain_backoff();
        assert_eq!(max_delay, Duration::from_millis(400));

        // weighted_domain_backoff should be much lower (~1.6ms: 400*4 / (1000+4))
        let weighted = weighted_domain_backoff();
        assert!(
            weighted < Duration::from_millis(50),
            "weighted backoff should be << 400ms when healthy domain dominates, got {}ms",
            weighted.as_millis()
        );
    }

    #[test]
    fn custom_throttle_config() {
        reset_registry();
        set_default_throttle_config(ThrottleConfig {
            max_backoff_level: 3,
            recovery_threshold: 2,
            decay_interval_ms: 5_000,
            backoff_table: vec![
                Duration::ZERO,
                Duration::from_millis(100),
                Duration::from_millis(500),
                Duration::from_millis(1500),
            ],
        });

        let state = domain_throttle("test-custom-config.com");
        record_rate_limit(&state);
        assert_eq!(pre_request_delay(&state), Duration::from_millis(100));

        record_rate_limit(&state);
        record_rate_limit(&state);
        // Should cap at level 3
        assert_eq!(state.backoff_level.load(Ordering::Relaxed), 3);
        assert_eq!(pre_request_delay(&state), Duration::from_millis(1500));

        // Recovery with threshold=2
        state
            .last_rate_limit_at
            .store(now_millis(), Ordering::Relaxed);
        record_success(&state);
        record_success(&state);
        assert_eq!(state.backoff_level.load(Ordering::Relaxed), 2);

        // Reset to default config for other tests
        set_default_throttle_config(ThrottleConfig::default());
    }

    #[test]
    fn cross_thread_visibility() {
        reset_registry();
        let state = domain_throttle("cross-thread.com");

        let handles: Vec<_> = (0..4)
            .map(|_| {
                let s = Arc::clone(&state);
                std::thread::spawn(move || {
                    record_rate_limit(&s);
                })
            })
            .collect();

        for h in handles {
            h.join().unwrap();
        }

        // 4 rate limits → level should be 4
        assert_eq!(state.backoff_level.load(Ordering::Relaxed), 4);
    }
}
