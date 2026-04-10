//! Using BatchingTransport standalone (without load balancing).
//!
//! BatchingTransport wraps any `tower::Service<RequestPacket>` — it doesn't
//! require LoadBalancedTransport. This example shows how to add batching
//! to a single-endpoint setup.
//!
//! ```sh
//! cargo run --example batching_only
//! ```

use alloy_transport_balancer::{BatchingConfig, BatchingTransport};
use alloy_transport_http::Http;
use reqwest::Url;
use std::time::Duration;

fn main() {
    // Plain single-endpoint HTTP transport (no load balancing).
    let url = Url::parse("https://eth.llamarpc.com").unwrap();
    let client = reqwest::Client::new();
    let http = Http::with_client(client, url);

    // Wrap with batching: smaller batches with a longer flush window.
    let config = BatchingConfig {
        max_batch_size: 50,
        flush_interval: Duration::from_millis(5),
        ..Default::default()
    };
    let _transport = BatchingTransport::new(http, config);

    // All concurrent `eth_getStorageAt`, `eth_getBalance`, etc. calls
    // are automatically accumulated and sent as a single batch RPC.
    // A single request is sent as-is (no batch overhead).

    println!("BatchingTransport wrapping a single HTTP endpoint.");
    println!("Batch size: 50, flush interval: 5ms");
    println!("No load balancing — just transparent request batching.");
}
