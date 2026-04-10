//! Observability: inspecting domain throttle state.
//!
//! Shows how to monitor rate-limit backoff across all registered domains,
//! useful for dashboards, adaptive concurrency control, or alerting.
//!
//! ```sh
//! cargo run --example observability
//! ```

use alloy_transport_balancer::{
    LoadBalancedTransport, Weight, domain_throttle, log_throttle_summary, max_domain_backoff,
    record_rate_limit, record_success, throttle_snapshot,
};
use reqwest::Url;

fn main() {
    // Create a transport to register some domains in the throttle registry.
    let _transport = LoadBalancedTransport::new(vec![
        (
            Url::parse("https://eth-mainnet.g.alchemy.com/v2/demo").unwrap(),
            Weight::default(),
        ),
        (
            Url::parse("https://lb.drpc.org/ogrpc").unwrap(),
            Weight::default(),
        ),
    ]);

    // Simulate some rate-limit events on alchemy.com.
    let alchemy_state = domain_throttle("alchemy.com");
    record_rate_limit(&alchemy_state); // level 0 → 1
    record_rate_limit(&alchemy_state); // level 1 → 2

    // Simulate recovery on drpc.org.
    let drpc_state = domain_throttle("drpc.org");
    for _ in 0..5 {
        record_success(&drpc_state);
    }

    // Option 1: Structured snapshots for custom metrics/dashboards.
    println!("=== Domain Throttle Snapshots ===");
    for snap in throttle_snapshot() {
        println!(
            "  {}: backoff_level={}, delay={}ms, 429s={}, successes={}, skips={}",
            snap.domain,
            snap.backoff_level,
            snap.delay_ms,
            snap.total_rate_limits,
            snap.total_successes,
            snap.total_skips,
        );
    }

    // Option 2: One-liner log output (uses tracing at INFO/DEBUG level).
    println!("\n=== Log Summary (via tracing) ===");
    log_throttle_summary();

    // Option 3: Worst-case delay for adaptive concurrency control.
    let worst = max_domain_backoff();
    println!("\nWorst-case domain backoff: {:?}", worst);
    println!("(Useful for reducing batch concurrency when providers are under pressure)");
}
