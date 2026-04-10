//! Basic load-balanced transport with default configuration.
//!
//! Distributes requests equally across two RPC providers with automatic
//! failover and transparent JSON-RPC batching.
//!
//! ```sh
//! cargo run --example basic
//! ```

use alloy_transport_balancer::{BatchingConfig, BatchingTransport, LoadBalancedTransport, Weight};
use reqwest::Url;

fn main() {
    // Two endpoints with equal weight — traffic splits 50/50.
    let endpoints = vec![
        (
            Url::parse("https://eth.llamarpc.com").unwrap(),
            Weight::default(),
        ),
        (
            Url::parse("https://rpc.ankr.com/eth").unwrap(),
            Weight::default(),
        ),
    ];

    // Create the load-balanced transport.
    let balanced = LoadBalancedTransport::new(endpoints);

    // Wrap with batching for transparent request accumulation.
    // Concurrent requests are automatically grouped into batch RPCs.
    let _transport = BatchingTransport::new(balanced, BatchingConfig::default());

    // Pass to RpcClient or ProviderBuilder:
    //   let client = RpcClient::new(transport, false);
    //   let provider = ProviderBuilder::new().connect_client(client);

    println!("Transport created with 2 equal-weight endpoints and batching enabled.");
    println!("Weight::default() = {:?}", Weight::default());
}
