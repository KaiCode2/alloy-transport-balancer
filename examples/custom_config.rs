//! Builder pattern with custom balancer, throttle, and HTTP client configuration.
//!
//! Demonstrates tuning retry behavior, backoff curves, and connection pool
//! settings for a high-throughput workload.
//!
//! ```sh
//! cargo run --example custom_config
//! ```

use alloy_transport_balancer::{
    BalancerConfig, HttpClientConfig, LoadBalancedTransport, ThrottleConfig, Weight,
};
use reqwest::Url;
use std::time::Duration;

fn main() {
    let transport = LoadBalancedTransport::builder(vec![
        (
            Url::parse("https://eth.llamarpc.com").unwrap(),
            Weight(150), // preferred provider: 60% of traffic
        ),
        (
            Url::parse("https://rpc.ankr.com/eth").unwrap(),
            Weight(100), // fallback: 40% of traffic
        ),
    ])
    // More aggressive retry: 5 rounds with faster initial backoff.
    .config(BalancerConfig {
        max_retry_rounds: 5,
        initial_backoff: Duration::from_millis(50),
        max_backoff: Duration::from_secs(2),
        ..Default::default()
    })
    // Slower recovery: require 20 consecutive successes to drop a backoff level.
    .throttle_config(ThrottleConfig {
        recovery_threshold: 20,
        ..Default::default()
    })
    // Tighter timeouts for latency-sensitive workloads.
    .http_client_config(HttpClientConfig {
        connect_timeout: Duration::from_secs(5),
        request_timeout: Duration::from_secs(10),
        pool_max_idle_per_host: 16,
        ..Default::default()
    })
    .build();

    println!("Transport created with custom configuration:");
    println!("  {:?}", transport);
}
