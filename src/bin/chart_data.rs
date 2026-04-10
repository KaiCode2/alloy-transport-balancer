//! Benchmark runner and SVG chart generator for alloy-transport-balancer.
//!
//! Compares vanilla transport (with connection pool limiting) against
//! `BatchingTransport` at various concurrency levels.
//!
//! ## Methodology
//!
//! - **Mock transport**: Simulates RPC latency via `tokio::time::sleep` (no real HTTP)
//! - **Vanilla baseline**: Limited to 6 concurrent connections via `tokio::sync::Semaphore`,
//!   simulating a realistic HTTP connection pool. Excess requests queue behind the pool.
//! - **Batching**: `BatchingTransport` wrapping an unlimited mock — all concurrent requests
//!   are compressed into batch JSON-RPC calls, eliminating connection pool queuing.
//! - **Latency**: 50ms per HTTP round-trip (configurable in `main()`)
//! - **Iterations**: 5 per data point, averaged
//!
//! ## Output
//!
//! - Markdown table printed to stdout
//! - `assets/throughput.svg` — bar chart comparing requests/sec
//! - `assets/http_calls.svg` — bar chart comparing HTTP round-trips
//!
//! ## Usage
//!
//! ```sh
//! cargo run --bin chart_data --release --all-features
//! ```

#![allow(missing_docs, clippy::all)]

use alloy_json_rpc::{Id, RequestPacket, Response, ResponsePacket, SerializedRequest};
use alloy_transport::{TransportError, TransportFut};
use alloy_transport_balancer::{BatchingConfig, BatchingTransport};
use serde_json::value::RawValue;
use std::{
    fmt::Write as FmtWrite,
    sync::{
        Arc,
        atomic::{AtomicU64, AtomicUsize, Ordering},
    },
    task::{Context, Poll},
    time::{Duration, Instant},
};
use tokio::runtime::Runtime;
use tower::Service;

// ---------------------------------------------------------------------------
// Mock transport
// ---------------------------------------------------------------------------

/// Mock RPC transport with optional connection pool limiting.
///
/// When `max_concurrent` is set, a semaphore limits how many in-flight
/// HTTP calls can proceed simultaneously — simulating real HTTP connection
/// pool behavior where excess requests queue behind the pool.
#[derive(Clone)]
struct MockRpcTransport {
    latency: Duration,
    call_count: Arc<AtomicUsize>,
    request_count: Arc<AtomicUsize>,
    /// Simulates HTTP connection pool limit (None = unlimited).
    concurrency_limit: Option<Arc<tokio::sync::Semaphore>>,
}

impl MockRpcTransport {
    fn new(latency: Duration) -> Self {
        Self {
            latency,
            call_count: Arc::new(AtomicUsize::new(0)),
            request_count: Arc::new(AtomicUsize::new(0)),
            concurrency_limit: None,
        }
    }

    /// Create a mock with limited concurrent connections (like a real HTTP pool).
    fn with_pool_limit(latency: Duration, max_concurrent: usize) -> Self {
        Self {
            latency,
            call_count: Arc::new(AtomicUsize::new(0)),
            request_count: Arc::new(AtomicUsize::new(0)),
            concurrency_limit: Some(Arc::new(tokio::sync::Semaphore::new(max_concurrent))),
        }
    }

    fn reset(&self) {
        self.call_count.store(0, Ordering::Relaxed);
        self.request_count.store(0, Ordering::Relaxed);
    }
}

impl Service<RequestPacket> for MockRpcTransport {
    type Response = ResponsePacket;
    type Error = TransportError;
    type Future = TransportFut<'static>;

    fn poll_ready(&mut self, _cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        Poll::Ready(Ok(()))
    }

    fn call(&mut self, req: RequestPacket) -> Self::Future {
        let latency = self.latency;
        self.call_count.fetch_add(1, Ordering::Relaxed);
        let num_requests = req.requests().len();
        self.request_count
            .fetch_add(num_requests, Ordering::Relaxed);
        let semaphore = self.concurrency_limit.clone();

        Box::pin(async move {
            // If pool-limited, wait for a connection slot
            let _permit = match &semaphore {
                Some(sem) => Some(sem.acquire().await.unwrap()),
                None => None,
            };
            tokio::time::sleep(latency).await;
            let value = RawValue::from_string("\"0x1\"".to_string()).unwrap();
            let responses: Vec<Response<Box<RawValue>>> = req
                .requests()
                .iter()
                .map(|r| Response {
                    id: r.id().clone(),
                    payload: alloy_json_rpc::ResponsePayload::Success(value.clone()),
                })
                .collect();
            if responses.len() == 1 {
                Ok(ResponsePacket::Single(
                    responses.into_iter().next().unwrap(),
                ))
            } else {
                Ok(ResponsePacket::Batch(responses))
            }
        })
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

static ID_COUNTER: AtomicU64 = AtomicU64::new(1);

fn make_request() -> SerializedRequest {
    let id = Id::Number(ID_COUNTER.fetch_add(1, Ordering::Relaxed));
    alloy_json_rpc::Request::new("eth_call".to_string(), id, ())
        .serialize()
        .unwrap()
}

async fn fire_concurrent<T>(transport: T, n: usize) -> Duration
where
    T: Service<RequestPacket, Response = ResponsePacket, Error = TransportError>
        + Clone
        + Send
        + 'static,
    T::Future: Send,
{
    let start = Instant::now();
    let mut handles = Vec::with_capacity(n);
    for _ in 0..n {
        let mut t = transport.clone();
        handles.push(tokio::spawn(async move {
            let req = make_request();
            let _ = t.call(RequestPacket::Single(req)).await.unwrap();
        }));
    }
    for h in handles {
        h.await.unwrap();
    }
    start.elapsed()
}

// ---------------------------------------------------------------------------
// Benchmark runner
// ---------------------------------------------------------------------------

struct BenchResult {
    concurrency: usize,
    vanilla_ms: f64,
    batching_ms: f64,
    vanilla_rps: f64,
    batching_rps: f64,
    vanilla_calls: usize,
    batching_calls: usize,
    speedup: f64,
}

fn run_benchmark(
    rt: &Runtime,
    concurrency: usize,
    latency: Duration,
    iterations: usize,
) -> BenchResult {
    // Vanilla — limited to 6 concurrent HTTP connections (realistic pool size).
    // With N > 6 requests, excess requests must wait for a connection slot.
    let vanilla_mock = MockRpcTransport::with_pool_limit(latency, 6);
    let mut vanilla_total = Duration::ZERO;
    let mut vanilla_calls_total = 0usize;
    for _ in 0..iterations {
        vanilla_mock.reset();
        let elapsed = rt.block_on(fire_concurrent(vanilla_mock.clone(), concurrency));
        vanilla_total += elapsed;
        vanilla_calls_total += vanilla_mock.call_count.load(Ordering::Relaxed);
    }
    let vanilla_avg = vanilla_total / iterations as u32;
    let vanilla_calls_avg = vanilla_calls_total / iterations;

    // Batching — must create BatchingTransport inside the runtime context
    // because it spawns a background flush task via tokio::spawn().
    let mut batching_total = Duration::ZERO;
    let mut batching_calls_total = 0usize;
    for _ in 0..iterations {
        let mock = MockRpcTransport::new(latency);
        let call_count = mock.call_count.clone();
        let elapsed = rt.block_on(async {
            let batching = BatchingTransport::new(
                mock,
                BatchingConfig {
                    max_batch_size: 150,
                    flush_interval: Duration::from_millis(1),
                    ..Default::default()
                },
            );
            fire_concurrent(batching, concurrency).await
        });
        batching_total += elapsed;
        batching_calls_total += call_count.load(Ordering::Relaxed);
    }
    let batching_avg = batching_total / iterations as u32;
    let batching_calls_avg = batching_calls_total / iterations;

    let vanilla_ms = vanilla_avg.as_secs_f64() * 1000.0;
    let batching_ms = batching_avg.as_secs_f64() * 1000.0;

    BenchResult {
        concurrency,
        vanilla_ms,
        batching_ms,
        vanilla_rps: concurrency as f64 / vanilla_avg.as_secs_f64(),
        batching_rps: concurrency as f64 / batching_avg.as_secs_f64(),
        vanilla_calls: vanilla_calls_avg,
        batching_calls: batching_calls_avg,
        speedup: vanilla_ms / batching_ms,
    }
}

// ---------------------------------------------------------------------------
// SVG chart generation
//
// Colors are injected via write!() with explicit color variables so that
// `#rrggbb` never appears as `#identifier` inside a format string (which
// Rust 2021 reserves as a prefix literal).
// ---------------------------------------------------------------------------

/// Convenience: hex color as a plain &str to avoid the `#ident` prefix issue.
macro_rules! c {
    ($hex:literal) => {
        concat!("#", $hex)
    };
}

const CLR_BG: &str = c!("0f172a");
const CLR_TITLE: &str = c!("f1f5f9");
const CLR_SUBTITLE: &str = c!("94a3b8");
const CLR_GRID: &str = c!("1e293b");
const CLR_AXIS_TEXT: &str = c!("94a3b8");
const CLR_LABEL: &str = c!("cbd5e1");
const CLR_BAR_TEXT: &str = c!("e2e8f0");
const CLR_SPEEDUP: &str = c!("4ade80");

// Gradient stops
const CLR_VANILLA_TOP: &str = c!("94a3b8");
const CLR_VANILLA_BOT: &str = c!("64748b");
const CLR_BATCH_TOP: &str = c!("38bdf8");
const CLR_BATCH_BOT: &str = c!("0284c7");
const CLR_BATCH2_TOP: &str = c!("a78bfa");
const CLR_BATCH2_BOT: &str = c!("7c3aed");

fn generate_throughput_svg(results: &[BenchResult], path: &str) {
    let width = 800i32;
    let height = 480i32;
    let ml = 80i32; // margin_left
    let mr = 40i32;
    let mt = 80i32; // extra top margin for title + subtitle
    let mb = 70i32;
    let pw = width - ml - mr; // plot width
    let ph = height - mt - mb; // plot height

    let max_rps = results
        .iter()
        .flat_map(|r| [r.vanilla_rps, r.batching_rps])
        .fold(0.0f64, f64::max)
        * 1.15;

    let n = results.len();
    let bgw = pw as f64 / n as f64; // bar group width
    let bw = bgw * 0.35; // bar width
    let gap = bgw * 0.05;

    let mut s = String::with_capacity(8192);

    // Header
    let _ = write!(
        s,
        "<svg xmlns=\"http://www.w3.org/2000/svg\" viewBox=\"0 0 {width} {height}\" \
         font-family=\"system-ui, -apple-system, sans-serif\">\n\
         <defs>\n\
         <linearGradient id=\"vg\" x1=\"0\" y1=\"0\" x2=\"0\" y2=\"1\">\
         <stop offset=\"0%\" stop-color=\"{CLR_VANILLA_TOP}\"/>\
         <stop offset=\"100%\" stop-color=\"{CLR_VANILLA_BOT}\"/></linearGradient>\n\
         <linearGradient id=\"bg\" x1=\"0\" y1=\"0\" x2=\"0\" y2=\"1\">\
         <stop offset=\"0%\" stop-color=\"{CLR_BATCH_TOP}\"/>\
         <stop offset=\"100%\" stop-color=\"{CLR_BATCH_BOT}\"/></linearGradient>\n\
         </defs>\n\
         <rect width=\"{width}\" height=\"{height}\" fill=\"{CLR_BG}\" rx=\"12\"/>\n\
         <text x=\"{}\" y=\"30\" fill=\"{CLR_TITLE}\" font-size=\"18\" font-weight=\"600\" \
         text-anchor=\"middle\">Throughput: Vanilla vs Batching Transport</text>\n\
         <text x=\"{}\" y=\"50\" fill=\"{CLR_SUBTITLE}\" font-size=\"12\" \
         text-anchor=\"middle\">(50ms simulated RPC latency, 6-connection pool)</text>\n",
        width / 2,
        width / 2
    );

    // Y gridlines + labels
    let yt = 5;
    for i in 0..=yt {
        let rps = max_rps * i as f64 / yt as f64;
        let y = mt + ph - (ph as f64 * i as f64 / yt as f64) as i32;
        let _ = write!(
            s,
            "<line x1=\"{ml}\" y1=\"{y}\" x2=\"{}\" y2=\"{y}\" stroke=\"{CLR_GRID}\" stroke-width=\"1\"/>\n\
             <text x=\"{}\" y=\"{}\" fill=\"{CLR_AXIS_TEXT}\" font-size=\"11\" text-anchor=\"end\">{:.0}</text>\n",
            ml + pw,
            ml - 8,
            y + 4,
            rps
        );
    }

    // Y-axis label
    let ymid = mt + ph / 2;
    let _ = write!(
        s,
        "<text x=\"16\" y=\"{ymid}\" fill=\"{CLR_AXIS_TEXT}\" font-size=\"12\" \
         transform=\"rotate(-90, 16, {ymid})\" text-anchor=\"middle\">Requests / second</text>\n"
    );

    // Bars
    for (i, r) in results.iter().enumerate() {
        let gx = ml as f64 + i as f64 * bgw + bgw * 0.1;

        // Vanilla
        let vh = (r.vanilla_rps / max_rps * ph as f64).max(1.0);
        let vy = mt as f64 + ph as f64 - vh;
        let _ = write!(
            s,
            "<rect x=\"{gx:.1}\" y=\"{vy:.1}\" width=\"{bw:.1}\" height=\"{vh:.1}\" \
             fill=\"url(#vg)\" rx=\"4\"/>\n\
             <text x=\"{:.1}\" y=\"{:.1}\" fill=\"{CLR_BAR_TEXT}\" font-size=\"10\" \
             text-anchor=\"middle\" font-weight=\"500\">{:.0}</text>\n",
            gx + bw / 2.0,
            vy - 6.0,
            r.vanilla_rps
        );

        // Batching
        let bx = gx + bw + gap;
        let bh = (r.batching_rps / max_rps * ph as f64).max(1.0);
        let by = mt as f64 + ph as f64 - bh;
        let _ = write!(
            s,
            "<rect x=\"{bx:.1}\" y=\"{by:.1}\" width=\"{bw:.1}\" height=\"{bh:.1}\" \
             fill=\"url(#bg)\" rx=\"4\"/>\n\
             <text x=\"{:.1}\" y=\"{:.1}\" fill=\"{CLR_BAR_TEXT}\" font-size=\"10\" \
             text-anchor=\"middle\" font-weight=\"500\">{:.0}</text>\n",
            bx + bw / 2.0,
            by - 6.0,
            r.batching_rps
        );

        // Speedup
        let _ = write!(
            s,
            "<text x=\"{:.1}\" y=\"{:.1}\" fill=\"{CLR_SPEEDUP}\" font-size=\"11\" \
             text-anchor=\"middle\" font-weight=\"600\">{:.1}x</text>\n",
            gx + bw + gap / 2.0,
            by - 20.0,
            r.speedup
        );

        // X label
        let _ = write!(
            s,
            "<text x=\"{:.1}\" y=\"{}\" fill=\"{CLR_LABEL}\" font-size=\"12\" \
             text-anchor=\"middle\">{}</text>\n",
            gx + bw + gap / 2.0,
            mt + ph + 20,
            r.concurrency
        );
    }

    // X title
    let _ = write!(
        s,
        "<text x=\"{}\" y=\"{}\" fill=\"{CLR_SUBTITLE}\" font-size=\"12\" \
         text-anchor=\"middle\">Concurrent Requests</text>\n",
        ml + pw / 2,
        height - 12
    );

    // Legend — positioned in the upper-right, above the plot area
    let lx = width - mr - 190;
    let ly = 14;
    let _ = write!(
        s,
        "<rect x=\"{lx}\" y=\"{ly}\" width=\"14\" height=\"14\" fill=\"url(#vg)\" rx=\"3\"/>\n\
         <text x=\"{}\" y=\"{}\" fill=\"{CLR_LABEL}\" font-size=\"11\">\
         Vanilla (1 req = 1 call)</text>\n\
         <rect x=\"{lx}\" y=\"{}\" width=\"14\" height=\"14\" fill=\"url(#bg)\" rx=\"3\"/>\n\
         <text x=\"{}\" y=\"{}\" fill=\"{CLR_LABEL}\" font-size=\"11\">\
         Batching Transport</text>\n",
        lx + 20,
        ly + 12,
        ly + 20,
        lx + 20,
        ly + 32
    );

    s.push_str("</svg>\n");
    std::fs::write(path, &s).unwrap();
    eprintln!("Wrote {path}");
}

fn generate_calls_svg(results: &[BenchResult], path: &str) {
    let width = 800i32;
    let height = 430i32;
    let ml = 80i32;
    let mr = 40i32;
    let mt = 60i32; // extra top margin for legend
    let mb = 70i32;
    let pw = width - ml - mr;
    let ph = height - mt - mb;

    let max_calls = results
        .iter()
        .flat_map(|r| [r.vanilla_calls, r.batching_calls])
        .max()
        .unwrap_or(1) as f64
        * 1.15;

    let n = results.len();
    let bgw = pw as f64 / n as f64;
    let bw = bgw * 0.35;
    let gap = bgw * 0.05;

    let mut s = String::with_capacity(8192);

    let _ = write!(
        s,
        "<svg xmlns=\"http://www.w3.org/2000/svg\" viewBox=\"0 0 {width} {height}\" \
         font-family=\"system-ui, -apple-system, sans-serif\">\n\
         <defs>\n\
         <linearGradient id=\"vg2\" x1=\"0\" y1=\"0\" x2=\"0\" y2=\"1\">\
         <stop offset=\"0%\" stop-color=\"{CLR_VANILLA_TOP}\"/>\
         <stop offset=\"100%\" stop-color=\"{CLR_VANILLA_BOT}\"/></linearGradient>\n\
         <linearGradient id=\"bg2\" x1=\"0\" y1=\"0\" x2=\"0\" y2=\"1\">\
         <stop offset=\"0%\" stop-color=\"{CLR_BATCH2_TOP}\"/>\
         <stop offset=\"100%\" stop-color=\"{CLR_BATCH2_BOT}\"/></linearGradient>\n\
         </defs>\n\
         <rect width=\"{width}\" height=\"{height}\" fill=\"{CLR_BG}\" rx=\"12\"/>\n\
         <text x=\"{}\" y=\"32\" fill=\"{CLR_TITLE}\" font-size=\"18\" font-weight=\"600\" \
         text-anchor=\"middle\">HTTP Round-Trips: Vanilla vs Batching</text>\n",
        width / 2
    );

    // Y gridlines
    let yt = 5;
    for i in 0..=yt {
        let calls = max_calls * i as f64 / yt as f64;
        let y = mt + ph - (ph as f64 * i as f64 / yt as f64) as i32;
        let _ = write!(
            s,
            "<line x1=\"{ml}\" y1=\"{y}\" x2=\"{}\" y2=\"{y}\" stroke=\"{CLR_GRID}\" stroke-width=\"1\"/>\n\
             <text x=\"{}\" y=\"{}\" fill=\"{CLR_AXIS_TEXT}\" font-size=\"11\" text-anchor=\"end\">{:.0}</text>\n",
            ml + pw,
            ml - 8,
            y + 4,
            calls
        );
    }

    let ymid = mt + ph / 2;
    let _ = write!(
        s,
        "<text x=\"16\" y=\"{ymid}\" fill=\"{CLR_AXIS_TEXT}\" font-size=\"12\" \
         transform=\"rotate(-90, 16, {ymid})\" text-anchor=\"middle\">HTTP Calls</text>\n"
    );

    for (i, r) in results.iter().enumerate() {
        let gx = ml as f64 + i as f64 * bgw + bgw * 0.1;

        // Vanilla bar
        let vh = (r.vanilla_calls as f64 / max_calls * ph as f64).max(1.0);
        let vy = mt as f64 + ph as f64 - vh;
        let _ = write!(
            s,
            "<rect x=\"{gx:.1}\" y=\"{vy:.1}\" width=\"{bw:.1}\" height=\"{vh:.1}\" \
             fill=\"url(#vg2)\" rx=\"4\"/>\n\
             <text x=\"{:.1}\" y=\"{:.1}\" fill=\"{CLR_BAR_TEXT}\" font-size=\"10\" \
             text-anchor=\"middle\" font-weight=\"500\">{}</text>\n",
            gx + bw / 2.0,
            vy - 6.0,
            r.vanilla_calls
        );

        // Batching bar
        let bx = gx + bw + gap;
        let bh = (r.batching_calls as f64 / max_calls * ph as f64).max(1.0);
        let by = mt as f64 + ph as f64 - bh;
        let _ = write!(
            s,
            "<rect x=\"{bx:.1}\" y=\"{by:.1}\" width=\"{bw:.1}\" height=\"{bh:.1}\" \
             fill=\"url(#bg2)\" rx=\"4\"/>\n\
             <text x=\"{:.1}\" y=\"{:.1}\" fill=\"{CLR_BAR_TEXT}\" font-size=\"10\" \
             text-anchor=\"middle\" font-weight=\"500\">{}</text>\n",
            bx + bw / 2.0,
            by - 6.0,
            r.batching_calls
        );

        // Reduction label
        let reduction = (1.0 - r.batching_calls as f64 / r.vanilla_calls as f64) * 100.0;
        let _ = write!(
            s,
            "<text x=\"{:.1}\" y=\"{:.1}\" fill=\"{CLR_SPEEDUP}\" font-size=\"11\" \
             text-anchor=\"middle\" font-weight=\"600\">-{:.0}%</text>\n",
            gx + bw + gap / 2.0,
            by - 18.0,
            reduction
        );

        // X label
        let _ = write!(
            s,
            "<text x=\"{:.1}\" y=\"{}\" fill=\"{CLR_LABEL}\" font-size=\"12\" \
             text-anchor=\"middle\">{} requests</text>\n",
            gx + bw + gap / 2.0,
            mt + ph + 20,
            r.concurrency
        );
    }

    // X title
    let _ = write!(
        s,
        "<text x=\"{}\" y=\"{}\" fill=\"{CLR_SUBTITLE}\" font-size=\"12\" \
         text-anchor=\"middle\">Logical Requests Sent</text>\n",
        ml + pw / 2,
        height - 12
    );

    // Legend — positioned in the upper-right, above the plot area
    let lx = width - mr - 200;
    let ly = 14;
    let _ = write!(
        s,
        "<rect x=\"{lx}\" y=\"{ly}\" width=\"14\" height=\"14\" fill=\"url(#vg2)\" rx=\"3\"/>\n\
         <text x=\"{}\" y=\"{}\" fill=\"{CLR_LABEL}\" font-size=\"11\">\
         Vanilla (1 call per request)</text>\n\
         <rect x=\"{lx}\" y=\"{}\" width=\"14\" height=\"14\" fill=\"url(#bg2)\" rx=\"3\"/>\n\
         <text x=\"{}\" y=\"{}\" fill=\"{CLR_LABEL}\" font-size=\"11\">\
         Batching (auto-grouped)</text>\n",
        lx + 20,
        ly + 12,
        ly + 20,
        lx + 20,
        ly + 32
    );

    s.push_str("</svg>\n");
    std::fs::write(path, &s).unwrap();
    eprintln!("Wrote {path}");
}

// ---------------------------------------------------------------------------
// Main
// ---------------------------------------------------------------------------

fn main() {
    let rt = Runtime::new().unwrap();
    let latency = Duration::from_millis(50);
    let iterations = 5;

    println!("\n## Benchmark Results\n");
    println!(
        "Simulated RPC latency: {}ms per HTTP call",
        latency.as_millis()
    );
    println!("Iterations per data point: {iterations}\n");

    println!(
        "| Concurrent Reqs | Vanilla (ms) | Batching (ms) | Speedup \
         | Vanilla Calls | Batch Calls | Calls Saved |"
    );
    println!(
        "|----------------|-------------|--------------|---------|\
         --------------|------------|------------|"
    );

    let concurrencies = [10, 50, 100, 250, 500, 1000];
    let mut results = Vec::new();

    for &n in &concurrencies {
        let r = run_benchmark(&rt, n, latency, iterations);
        let saved = ((1.0 - r.batching_calls as f64 / r.vanilla_calls as f64) * 100.0).max(0.0);
        println!(
            "| {:>14} | {:>11.1} | {:>12.1} | {:>6.1}x | {:>12} | {:>10} | {:>9.0}% |",
            r.concurrency,
            r.vanilla_ms,
            r.batching_ms,
            r.speedup,
            r.vanilla_calls,
            r.batching_calls,
            saved
        );
        results.push(r);
    }

    // Generate SVG charts
    std::fs::create_dir_all("assets").unwrap();
    generate_throughput_svg(&results, "assets/throughput.svg");
    generate_calls_svg(&results, "assets/http_calls.svg");

    println!("\nSVG charts written to assets/throughput.svg and assets/http_calls.svg");
}
