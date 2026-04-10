//! Multi-chain setup with shared domain throttle state.
//!
//! Different chains use separate `LoadBalancedTransport` instances, but
//! endpoints on the same domain (e.g., Alchemy) automatically share
//! throttle state and HTTP clients through the process-wide registry.
//!
//! ```sh
//! cargo run --example multi_chain
//! ```

use alloy_transport_balancer::{
    BatchingConfig, BatchingTransport, LoadBalancedTransport, Weight, throttle_snapshot,
};
use reqwest::Url;

fn main() {
    // --- Ethereum mainnet ---
    let eth_transport = LoadBalancedTransport::new(vec![
        (
            Url::parse("https://eth-mainnet.g.alchemy.com/v2/demo").unwrap(),
            Weight(100),
        ),
        (
            Url::parse("https://lb.drpc.org/ogrpc?network=ethereum").unwrap(),
            Weight(50),
        ),
    ]);
    let _eth = BatchingTransport::new(eth_transport, BatchingConfig::default());

    // --- Arbitrum ---
    let arb_transport = LoadBalancedTransport::new(vec![
        (
            Url::parse("https://arb-mainnet.g.alchemy.com/v2/demo").unwrap(),
            Weight(100),
        ),
        (
            Url::parse("https://lb.drpc.org/ogrpc?network=arbitrum").unwrap(),
            Weight(50),
        ),
    ]);
    let _arb = BatchingTransport::new(arb_transport, BatchingConfig::default());

    // --- Base ---
    let base_transport = LoadBalancedTransport::new(vec![(
        Url::parse("https://base-mainnet.g.alchemy.com/v2/demo").unwrap(),
        Weight(100),
    )]);
    let _base = BatchingTransport::new(base_transport, BatchingConfig::default());

    // All three chains share throttle state for alchemy.com and drpc.org.
    // A 429 from Alchemy on Ethereum causes Arbitrum and Base to back off too.
    println!("Created transports for 3 chains with shared domain throttling.");
    println!("Alchemy endpoints across all chains share one throttle state.");
    println!();

    // Inspect the shared throttle state
    for snap in throttle_snapshot() {
        println!(
            "  Domain: {} | Level: {} | Delay: {}ms",
            snap.domain, snap.backoff_level, snap.delay_ms
        );
    }
}
