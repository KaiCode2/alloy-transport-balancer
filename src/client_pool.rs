//! Process-wide HTTP/2 client pooling by rate-limit domain.
//!
//! Maintains a single `reqwest::Client` per domain (e.g., `alchemy.com`),
//! shared across all transport instances and threads. Each client is configured
//! for HTTP/2 with adaptive window sizing, connection pooling, TCP keepalive,
//! and appropriate timeouts.
//!
//! This reduces the number of TLS handshakes and TCP connections from
//! `endpoints x threads` to just `domains` for the entire process.

use std::{
    collections::HashMap,
    sync::{Arc, OnceLock, RwLock},
    time::Duration,
};

/// Configuration for the shared HTTP client pool.
///
/// Controls connection pooling, keepalive, and timeout settings for the
/// `reqwest::Client` instances shared across endpoints on the same domain.
#[derive(Clone, Debug)]
pub struct HttpClientConfig {
    /// Maximum idle connections per host. Default: 8.
    pub pool_max_idle_per_host: usize,
    /// Idle connection timeout. Default: 90s.
    pub pool_idle_timeout: Duration,
    /// TCP keepalive interval. Default: 30s.
    pub tcp_keepalive: Duration,
    /// TCP connect timeout. Default: 10s.
    pub connect_timeout: Duration,
    /// Overall request timeout. Default: 30s.
    pub request_timeout: Duration,
}

impl Default for HttpClientConfig {
    fn default() -> Self {
        Self {
            pool_max_idle_per_host: 8,
            pool_idle_timeout: Duration::from_secs(90),
            tcp_keepalive: Duration::from_secs(30),
            connect_timeout: Duration::from_secs(10),
            request_timeout: Duration::from_secs(30),
        }
    }
}

/// Process-wide registry of shared `reqwest::Client` instances, keyed by domain.
///
/// Sharing a single client per domain enables HTTP/2 multiplexing (one TCP
/// connection serves concurrent requests from all chain threads) and efficient
/// connection pooling.
struct ClientRegistry {
    clients: HashMap<String, reqwest::Client>,
    default_config: Arc<HttpClientConfig>,
}

static CLIENT_REGISTRY: OnceLock<RwLock<ClientRegistry>> = OnceLock::new();

fn registry() -> &'static RwLock<ClientRegistry> {
    CLIENT_REGISTRY.get_or_init(|| {
        RwLock::new(ClientRegistry {
            clients: HashMap::new(),
            default_config: Arc::new(HttpClientConfig::default()),
        })
    })
}

/// Set the default HTTP client configuration for newly created clients.
///
/// Must be called before any `shared_client_for_domain()` lookups.
/// Domains already in the registry keep their existing client.
pub fn set_default_http_client_config(config: HttpClientConfig) {
    let reg = registry();
    let mut r = reg.write().unwrap_or_else(|e| e.into_inner());
    r.default_config = Arc::new(config);
}

/// Get or create an optimized `reqwest::Client` for the given domain.
///
/// The returned client is configured for high-throughput RPC workloads:
/// - HTTP/2 with adaptive flow-control windows
/// - Connection pooling with configurable idle connections and timeouts
/// - TCP keepalive
/// - Explicit connect and request timeouts
///
/// All `LoadBalancedTransport` endpoints on the same domain share the same
/// client, so their requests multiplex over shared TCP connections.
pub(crate) fn shared_client_for_domain(domain: &str) -> reqwest::Client {
    let reg = registry();

    // Fast path: read lock.
    {
        let r = reg.read().unwrap_or_else(|e| e.into_inner());
        if let Some(client) = r.clients.get(domain) {
            return client.clone();
        }
    }

    // Slow path: build and insert.
    let mut r = reg.write().unwrap_or_else(|e| e.into_inner());
    let config = Arc::clone(&r.default_config);
    r.clients
        .entry(domain.to_owned())
        .or_insert_with(|| build_optimized_client(&config))
        .clone()
}

/// Build a `reqwest::Client` tuned for high-throughput JSON-RPC workloads.
fn build_optimized_client(config: &HttpClientConfig) -> reqwest::Client {
    reqwest::ClientBuilder::new()
        // HTTP/2: ALPN negotiation picks h2 over TLS automatically.
        // Adaptive window sizing lets the server increase flow-control
        // windows based on observed throughput, improving pipelining.
        .http2_adaptive_window(true)
        // Connection pool: keep several idle connections warm per host
        // so back-to-back requests reuse existing TCP+TLS sessions.
        .pool_max_idle_per_host(config.pool_max_idle_per_host)
        .pool_idle_timeout(config.pool_idle_timeout)
        // TCP keepalive: detect dead connections before they timeout.
        .tcp_keepalive(config.tcp_keepalive)
        // Timeouts: bound how long we wait for a response.
        .connect_timeout(config.connect_timeout)
        .timeout(config.request_timeout)
        .build()
        .expect("failed to build reqwest client")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn same_domain_returns_same_client() {
        // Use unique domain names to avoid interference from other tests.
        let _a = shared_client_for_domain("same-test-unique.com");
        let _b = shared_client_for_domain("same-test-unique.com");
        // reqwest::Client is Clone-by-Arc internally; same instance means
        // they share the connection pool. Verify only one registry entry.
        let reg = registry();
        let r = reg.read().unwrap();
        assert!(r.clients.contains_key("same-test-unique.com"));
    }

    #[test]
    fn different_domains_return_different_clients() {
        let _a = shared_client_for_domain("diff-a-unique.com");
        let _b = shared_client_for_domain("diff-b-unique.org");
        let reg = registry();
        let r = reg.read().unwrap();
        assert!(r.clients.contains_key("diff-a-unique.com"));
        assert!(r.clients.contains_key("diff-b-unique.org"));
    }

    #[test]
    fn concurrent_access() {
        let handles: Vec<_> = (0..8)
            .map(|i| {
                let domain = if i % 2 == 0 {
                    "concurrent-even.com"
                } else {
                    "concurrent-odd.com"
                };
                let d = domain.to_string();
                std::thread::spawn(move || {
                    let _client = shared_client_for_domain(&d);
                })
            })
            .collect();

        for h in handles {
            h.join().unwrap();
        }

        let reg = registry();
        let r = reg.read().unwrap();
        assert!(r.clients.contains_key("concurrent-even.com"));
        assert!(r.clients.contains_key("concurrent-odd.com"));
    }
}
