#![allow(missing_docs)]

use alloy_json_rpc::{Id, RequestPacket, Response, ResponsePacket, SerializedRequest};
use alloy_transport::{TransportError, TransportFut};
use alloy_transport_balancer::{BatchingConfig, BatchingTransport};
use criterion::{BenchmarkId, Criterion, Throughput, criterion_group, criterion_main};
use serde_json::value::RawValue;
use std::{
    sync::{
        Arc,
        atomic::{AtomicU64, AtomicUsize, Ordering},
    },
    task::{Context, Poll},
    time::Duration,
};
use tokio::runtime::Runtime;
use tower::Service;

/// A mock RPC transport that simulates network latency without actual HTTP.
#[derive(Clone)]
struct MockRpcTransport {
    latency: Duration,
    call_count: Arc<AtomicUsize>,
}

impl MockRpcTransport {
    fn new(latency: Duration) -> Self {
        Self {
            latency,
            call_count: Arc::new(AtomicUsize::new(0)),
        }
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

        Box::pin(async move {
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

static ID_COUNTER: AtomicU64 = AtomicU64::new(1);

fn make_request(method: &str) -> SerializedRequest {
    let id = Id::Number(ID_COUNTER.fetch_add(1, Ordering::Relaxed));
    alloy_json_rpc::Request::new(method.to_string(), id, ())
        .serialize()
        .unwrap()
}

/// Fire `n` concurrent requests through the transport and wait for all to complete.
async fn fire_concurrent<T>(transport: T, n: usize)
where
    T: Service<RequestPacket, Response = ResponsePacket, Error = TransportError>
        + Clone
        + Send
        + 'static,
    T::Future: Send,
{
    let mut handles = Vec::with_capacity(n);
    for i in 0..n {
        let mut t = transport.clone();
        handles.push(tokio::spawn(async move {
            let req = make_request(&format!("eth_call_{i}"));
            let _ = t.call(RequestPacket::Single(req)).await.unwrap();
        }));
    }
    for h in handles {
        h.await.unwrap();
    }
}

fn throughput_benchmark(c: &mut Criterion) {
    let rt = Runtime::new().unwrap();
    let latency = Duration::from_millis(2);

    for &concurrency in &[10u64, 50, 100, 500] {
        let mut group = c.benchmark_group(format!("throughput/concurrent_{concurrency}"));
        group.throughput(Throughput::Elements(concurrency));
        group.sample_size(20);
        group.measurement_time(Duration::from_secs(5));

        // Vanilla: single mock transport, no batching
        group.bench_with_input(
            BenchmarkId::new("vanilla", concurrency),
            &concurrency,
            |b, &n| {
                let transport = MockRpcTransport::new(latency);
                b.to_async(&rt)
                    .iter(|| fire_concurrent(transport.clone(), n as usize));
            },
        );

        // Batching only: wraps single mock transport
        group.bench_with_input(
            BenchmarkId::new("batching", concurrency),
            &concurrency,
            |b, &n| {
                b.to_async(&rt).iter(|| {
                    let transport = MockRpcTransport::new(latency);
                    let batching = BatchingTransport::new(
                        transport,
                        BatchingConfig {
                            max_batch_size: 150,
                            flush_interval: Duration::from_millis(1),
                            ..Default::default()
                        },
                    );
                    fire_concurrent(batching, n as usize)
                });
            },
        );

        group.finish();
    }
}

fn latency_benchmark(c: &mut Criterion) {
    let rt = Runtime::new().unwrap();
    let latency = Duration::from_millis(2);

    for &concurrency in &[10u64, 50, 100, 500] {
        let mut group = c.benchmark_group(format!("latency/concurrent_{concurrency}"));
        group.sample_size(20);
        group.measurement_time(Duration::from_secs(5));

        // Vanilla: measure mean per-request latency
        group.bench_with_input(
            BenchmarkId::new("vanilla", concurrency),
            &concurrency,
            |b, &n| {
                let transport = MockRpcTransport::new(latency);
                b.to_async(&rt).iter(|| {
                    let t = transport.clone();
                    async move {
                        let start = std::time::Instant::now();
                        fire_concurrent(t, n as usize).await;
                        // Return wall-clock time for all requests (criterion measures this)
                        start.elapsed()
                    }
                });
            },
        );

        // Batching: measure mean per-request latency
        group.bench_with_input(
            BenchmarkId::new("batching", concurrency),
            &concurrency,
            |b, &n| {
                b.to_async(&rt).iter(|| async move {
                    let transport = MockRpcTransport::new(latency);
                    let batching = BatchingTransport::new(
                        transport,
                        BatchingConfig {
                            max_batch_size: 150,
                            flush_interval: Duration::from_millis(1),
                            ..Default::default()
                        },
                    );
                    let start = std::time::Instant::now();
                    fire_concurrent(batching, n as usize).await;
                    start.elapsed()
                });
            },
        );

        group.finish();
    }
}

fn batch_efficiency_benchmark(c: &mut Criterion) {
    let rt = Runtime::new().unwrap();
    let latency = Duration::from_millis(2);

    let mut group = c.benchmark_group("batch_efficiency");
    group.sample_size(20);

    for &n in &[50u64, 100, 500, 1000] {
        // Vanilla: every request = 1 HTTP call
        group.bench_with_input(BenchmarkId::new("vanilla_calls", n), &n, |b, &n| {
            let mock = MockRpcTransport::new(latency);
            let call_count = mock.call_count.clone();
            b.to_async(&rt).iter(|| {
                call_count.store(0, Ordering::Relaxed);
                fire_concurrent(mock.clone(), n as usize)
            });
        });

        // Batching: N requests compressed into fewer HTTP calls
        group.bench_with_input(BenchmarkId::new("batching_calls", n), &n, |b, &n| {
            b.to_async(&rt).iter(|| {
                let mock = MockRpcTransport::new(latency);
                let batching = BatchingTransport::new(
                    mock,
                    BatchingConfig {
                        max_batch_size: 150,
                        flush_interval: Duration::from_millis(1),
                        ..Default::default()
                    },
                );
                fire_concurrent(batching, n as usize)
            });
        });
    }

    group.finish();
}

criterion_group!(
    benches,
    throughput_benchmark,
    latency_benchmark,
    batch_efficiency_benchmark
);
criterion_main!(benches);
