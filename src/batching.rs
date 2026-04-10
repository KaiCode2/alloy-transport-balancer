//! Transparent JSON-RPC request batching transport.
//!
//! [`BatchingTransport`] wraps any inner `tower::Service<RequestPacket>` and
//! automatically accumulates concurrent individual requests into JSON-RPC batch
//! calls. A background task flushes accumulated requests when either
//! `max_batch_size` or `flush_interval` is reached.
//!
//! Includes a 3-stage retry strategy for missing batch responses:
//! 1. Send the full batch, collect unmatched request IDs
//! 2. Retry missing requests as a smaller batch
//! 3. Send each remaining request individually (guaranteed 1:1)

use alloy_json_rpc::{Id, RequestPacket, ResponsePacket, SerializedRequest};
use alloy_transport::{TransportError, TransportErrorKind, TransportFut};
use std::{
    collections::HashMap,
    sync::{Arc, Mutex, Weak},
    task::{Context, Poll},
    time::Duration,
};
use tokio::sync::{Notify, Semaphore, oneshot};
use tower::Service;
use tracing::{debug, trace, warn};

/// Maximum concurrent individual requests when falling back from a rejected batch.
/// Prevents DNS exhaustion and connection pool saturation.
const INDIVIDUAL_FALLBACK_CONCURRENCY: usize = 10;

/// Configuration for the [`BatchingTransport`] request accumulation and retry behavior.
///
/// Controls how individual JSON-RPC requests are grouped into batches,
/// the flush timing, and how missing responses are retried.
///
/// # Examples
///
/// ```
/// use alloy_transport_balancer::BatchingConfig;
/// use std::time::Duration;
///
/// // Smaller batches with longer flush window
/// let config = BatchingConfig {
///     max_batch_size: 50,
///     flush_interval: Duration::from_millis(5),
///     ..Default::default()
/// };
/// ```
#[derive(Clone)]
pub struct BatchingConfig {
    /// Maximum number of requests to accumulate before flushing.
    pub max_batch_size: usize,
    /// Maximum time to wait for more requests before flushing.
    pub flush_interval: Duration,
    /// Maximum retry attempts when an RPC batch response is missing entries.
    /// Attempt 0 = initial batch, attempts 1..N-1 = retry as batch,
    /// attempt N = retry each remaining request individually.
    pub max_missing_retries: usize,
    /// Called when a batch is fully rejected (matched==0).
    /// Allows the caller to escalate domain-level throttling.
    pub on_batch_rejection: Option<Arc<dyn Fn() + Send + Sync>>,
}

impl std::fmt::Debug for BatchingConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("BatchingConfig")
            .field("max_batch_size", &self.max_batch_size)
            .field("flush_interval", &self.flush_interval)
            .field("max_missing_retries", &self.max_missing_retries)
            .field("on_batch_rejection", &self.on_batch_rejection.is_some())
            .finish()
    }
}

impl Default for BatchingConfig {
    fn default() -> Self {
        Self {
            max_batch_size: 150,
            flush_interval: Duration::from_millis(1),
            max_missing_retries: 2,
            on_batch_rejection: None,
        }
    }
}

/// A pending request waiting for its batched response.
struct PendingRequest {
    request: SerializedRequest,
    tx: oneshot::Sender<Result<ResponsePacket, TransportError>>,
}

/// Shared state between all transport clones and the background flush task.
struct BatchingShared {
    pending: Mutex<Vec<PendingRequest>>,
    notify: Notify,
}

/// A transport that transparently batches individual JSON-RPC requests.
///
/// Wraps any inner transport (e.g. `LoadBalancedTransport` or `Http<Client>`)
/// and accumulates `RequestPacket::Single` calls into `RequestPacket::Batch`
/// requests, dramatically reducing HTTP round-trip overhead.
///
/// Requests are accumulated until either:
/// - `max_batch_size` is reached (immediate flush), or
/// - `flush_interval` elapses since the first accumulated request
///
/// Already-batched requests (`RequestPacket::Batch`) are decomposed into
/// individual pending items and re-batched together with other concurrent
/// requests for maximum efficiency.
///
/// The inner transport is captured by a background flush task at construction
/// time. The struct itself is cheap to clone (just an `Arc`).
#[derive(Clone)]
pub struct BatchingTransport {
    shared: Arc<BatchingShared>,
    config: BatchingConfig,
}

impl BatchingTransport {
    /// Create a new batching transport wrapping an inner transport.
    ///
    /// Spawns a background flush task on the current tokio runtime.
    /// The flush task exits automatically when all `BatchingTransport`
    /// clones are dropped.
    ///
    /// # Errors
    ///
    /// Individual requests within a batch receive errors independently via
    /// their response channels. If the inner transport fails entirely, all
    /// pending requests in that batch receive the same error. If responses
    /// are missing after all retry rounds, affected requests receive a
    /// `"missing response in batch"` error.
    pub fn new<T>(inner: T, config: BatchingConfig) -> Self
    where
        T: Service<RequestPacket, Response = ResponsePacket, Error = TransportError>
            + Clone
            + Send
            + Sync
            + 'static,
        T::Future: Send,
    {
        let shared = Arc::new(BatchingShared {
            pending: Mutex::new(Vec::with_capacity(config.max_batch_size)),
            notify: Notify::new(),
        });

        let weak = Arc::downgrade(&shared);
        let flush_config = config.clone();
        tokio::spawn(flush_loop(inner, weak, flush_config));

        Self { shared, config }
    }
}

impl Service<RequestPacket> for BatchingTransport {
    type Response = ResponsePacket;
    type Error = TransportError;
    type Future = TransportFut<'static>;

    fn poll_ready(&mut self, _cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        Poll::Ready(Ok(()))
    }

    fn call(&mut self, req: RequestPacket) -> Self::Future {
        let shared = Arc::clone(&self.shared);
        let max_batch = self.config.max_batch_size;

        Box::pin(async move {
            match req {
                RequestPacket::Single(request) => {
                    let (tx, rx) = oneshot::channel();
                    {
                        let mut pending = shared.pending.lock().unwrap_or_else(|e| e.into_inner());
                        pending.push(PendingRequest { request, tx });
                        trace!("request queued for batching");
                        if pending.len() >= max_batch {
                            shared.notify.notify_one();
                        }
                    }
                    shared.notify.notify_one();

                    rx.await
                        .unwrap_or_else(|_| Err(TransportErrorKind::backend_gone()))
                }
                RequestPacket::Batch(requests) => {
                    // Decompose batch into individual pending items so they
                    // get merged with other concurrent requests.
                    let mut receivers = Vec::with_capacity(requests.len());
                    {
                        let mut pending = shared.pending.lock().unwrap_or_else(|e| e.into_inner());
                        for request in requests {
                            let (tx, rx) = oneshot::channel();
                            pending.push(PendingRequest { request, tx });
                            receivers.push(rx);
                        }
                        if pending.len() >= max_batch {
                            shared.notify.notify_one();
                        }
                    }
                    shared.notify.notify_one();

                    let mut responses = Vec::with_capacity(receivers.len());
                    for rx in receivers {
                        match rx.await {
                            Ok(Ok(ResponsePacket::Single(resp))) => responses.push(resp),
                            Ok(Ok(ResponsePacket::Batch(batch))) => responses.extend(batch),
                            Ok(Err(e)) => return Err(e),
                            Err(_) => return Err(TransportErrorKind::backend_gone()),
                        }
                    }
                    Ok(ResponsePacket::Batch(responses))
                }
            }
        })
    }
}

impl std::fmt::Debug for BatchingTransport {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let pending_count = self.shared.pending.lock().map(|p| p.len()).unwrap_or(0);
        f.debug_struct("BatchingTransport")
            .field("pending", &pending_count)
            .field("config", &self.config)
            .finish()
    }
}

/// Background flush loop that drains accumulated requests and sends them
/// as batches through the inner transport.
async fn flush_loop<T>(inner: T, weak: Weak<BatchingShared>, config: BatchingConfig)
where
    T: Service<RequestPacket, Response = ResponsePacket, Error = TransportError>
        + Clone
        + Send
        + 'static,
    T::Future: Send,
{
    loop {
        // Wait for notification or timeout
        let Some(shared) = weak.upgrade() else {
            trace!("all BatchingTransport clones dropped, flush task exiting");
            return;
        };

        let _ = tokio::time::timeout(config.flush_interval, shared.notify.notified()).await;

        // Drain all pending requests
        let batch: Vec<PendingRequest> = {
            let mut pending = shared.pending.lock().unwrap_or_else(|e| e.into_inner());
            if pending.is_empty() {
                continue;
            }
            std::mem::take(&mut *pending)
        };

        // Drop the Arc ref so we don't keep the shared state alive
        // unnecessarily during HTTP I/O.
        drop(shared);

        // Process in chunks of max_batch_size
        let mut remaining = batch;
        while !remaining.is_empty() {
            let chunk_size = remaining.len().min(config.max_batch_size);
            let chunk: Vec<PendingRequest> = remaining.drain(..chunk_size).collect();

            if chunk.len() == 1 {
                // Single request — send as Single to avoid batch overhead
                let PendingRequest { request, tx } = chunk.into_iter().next().unwrap();
                let mut transport = inner.clone();
                tokio::spawn(async move {
                    let result = transport.call(RequestPacket::Single(request)).await;
                    let _ = tx.send(result);
                });
            } else {
                // Multiple requests — send as Batch with retry for missing responses
                let transport = inner.clone();
                let max_retries = config.max_missing_retries;
                let on_rejection = config.on_batch_rejection.clone();
                tokio::spawn(async move {
                    send_batch_with_retry(transport, chunk, max_retries, on_rejection).await;
                });
            }
        }
    }
}

/// Send a batch of requests, retrying any that get dropped by the RPC.
///
/// Some RPC providers silently omit responses for certain requests in a batch.
/// This function retries missing requests: first as a smaller batch, then
/// individually as a last resort.
async fn send_batch_with_retry<T>(
    inner: T,
    pending: Vec<PendingRequest>,
    max_retries: usize,
    on_batch_rejection: Option<Arc<dyn Fn() + Send + Sync>>,
) where
    T: Service<RequestPacket, Response = ResponsePacket, Error = TransportError>
        + Clone
        + Send
        + 'static,
    T::Future: Send,
{
    let original_batch_size = pending.len();

    // Build map: Id → (request, sender). We keep the request for potential retries.
    let mut outstanding: HashMap<
        Id,
        (
            SerializedRequest,
            oneshot::Sender<Result<ResponsePacket, TransportError>>,
        ),
    > = HashMap::with_capacity(pending.len());

    for PendingRequest { request, tx } in pending {
        let id = request.id().clone();
        // Defensive: detect duplicate IDs (should not happen with shared IdAllocator,
        // but prevents silent sender drops from HashMap overwrite).
        if let Some(prev) = outstanding.insert(id.clone(), (request, tx)) {
            warn!(
                ?id,
                "duplicate request ID detected in batch, failing earlier request"
            );
            let _ = prev.1.send(Err(TransportErrorKind::custom_str(
                "duplicate request ID in batch",
            )));
        }
    }

    for attempt in 0..=max_retries {
        if outstanding.is_empty() {
            return;
        }

        let batch_size = outstanding.len();

        // On the final retry attempt, send each request individually with
        // concurrency limiting to avoid overwhelming the RPC.
        if attempt == max_retries {
            debug!(
                remaining = batch_size,
                original_batch_size, "retrying remaining missing responses individually"
            );
            let semaphore = Arc::new(Semaphore::new(INDIVIDUAL_FALLBACK_CONCURRENCY));
            for (_id, (request, tx)) in outstanding {
                let mut transport = inner.clone();
                let permit = semaphore.clone();
                tokio::spawn(async move {
                    let _permit = permit.acquire().await;
                    let result = transport.call(RequestPacket::Single(request)).await;
                    let _ = tx.send(result);
                });
            }
            return;
        }

        // Build the batch request from outstanding entries.
        let requests: Vec<SerializedRequest> =
            outstanding.values().map(|(req, _)| req.clone()).collect();

        if attempt > 0 {
            warn!(
                batch_size,
                original_batch_size, attempt, "retrying missing batch responses"
            );
        } else {
            debug!(batch_size, "flushing batched RPC requests");
        }

        let mut transport = inner.clone();
        match transport.call(RequestPacket::Batch(requests)).await {
            Ok(resp) => {
                let responses = match resp {
                    ResponsePacket::Batch(v) => v,
                    ResponsePacket::Single(s) => vec![s],
                };
                let received = responses.len();
                let matched_before = outstanding.len();
                for response in responses {
                    if let Some((_, tx)) = outstanding.remove(&response.id) {
                        let _ = tx.send(Ok(ResponsePacket::Single(response)));
                    }
                }
                if outstanding.is_empty() {
                    return;
                }
                let matched = matched_before - outstanding.len();
                warn!(
                    batch_size,
                    received,
                    matched,
                    missing = outstanding.len(),
                    attempt,
                    "RPC returned fewer responses than requests in batch"
                );
                // Batch-level rejection: the RPC returned a single non-matching
                // response (e.g. an error with id:null) instead of a proper batch
                // response array. Retrying as a batch will hit the same rejection,
                // so skip straight to individual fallback.
                if matched == 0 {
                    debug!(
                        remaining = outstanding.len(),
                        original_batch_size,
                        "batch fully rejected by RPC, falling back to individual requests"
                    );
                    // Escalate domain throttle so adaptive prefetch backs off.
                    if let Some(ref cb) = on_batch_rejection {
                        cb();
                    }
                    let semaphore = Arc::new(Semaphore::new(INDIVIDUAL_FALLBACK_CONCURRENCY));
                    for (_id, (request, tx)) in outstanding {
                        let mut transport = inner.clone();
                        let permit = semaphore.clone();
                        tokio::spawn(async move {
                            let _permit = permit.acquire().await;
                            let result = transport.call(RequestPacket::Single(request)).await;
                            let _ = tx.send(result);
                        });
                    }
                    return;
                }
                // Partial drop: some matched, some didn't. Retry the missing subset
                // as a smaller batch (next loop iteration).
            }
            Err(e) => {
                // Transport-level failure — all remaining callers get the error.
                let msg = format!("batch request failed: {e}");
                for (_, (_, tx)) in outstanding {
                    let _ = tx.send(Err(TransportErrorKind::custom_str(&msg)));
                }
                return;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy_json_rpc::{RequestPacket, Response, ResponsePacket, SerializedRequest};
    use serde_json::value::RawValue;
    use std::sync::atomic::{AtomicUsize, Ordering};

    /// A mock transport that records what it receives.
    #[derive(Clone)]
    struct MockTransport {
        /// Count of `call()` invocations.
        call_count: Arc<AtomicUsize>,
        /// Total individual requests received (sum of batch sizes).
        request_count: Arc<AtomicUsize>,
        /// Records batch sizes for each call.
        batch_sizes: Arc<Mutex<Vec<usize>>>,
    }

    impl MockTransport {
        fn new() -> Self {
            Self {
                call_count: Arc::new(AtomicUsize::new(0)),
                request_count: Arc::new(AtomicUsize::new(0)),
                batch_sizes: Arc::new(Mutex::new(Vec::new())),
            }
        }
    }

    impl Service<RequestPacket> for MockTransport {
        type Response = ResponsePacket;
        type Error = TransportError;
        type Future = TransportFut<'static>;

        fn poll_ready(&mut self, _cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
            Poll::Ready(Ok(()))
        }

        fn call(&mut self, req: RequestPacket) -> Self::Future {
            self.call_count.fetch_add(1, Ordering::Relaxed);
            let batch_size = req.len();
            self.request_count.fetch_add(batch_size, Ordering::Relaxed);
            self.batch_sizes.lock().unwrap().push(batch_size);

            Box::pin(async move {
                // Build matching responses for each request
                let responses: Vec<_> = req
                    .requests()
                    .iter()
                    .map(|r| {
                        let value = RawValue::from_string("\"0x0\"".to_string()).unwrap();
                        Response {
                            id: r.id().clone(),
                            payload: alloy_json_rpc::ResponsePayload::Success(value),
                        }
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

    /// A mock transport that always returns an error.
    #[derive(Clone)]
    struct ErrorTransport;

    impl Service<RequestPacket> for ErrorTransport {
        type Response = ResponsePacket;
        type Error = TransportError;
        type Future = TransportFut<'static>;

        fn poll_ready(&mut self, _cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
            Poll::Ready(Ok(()))
        }

        fn call(&mut self, _req: RequestPacket) -> Self::Future {
            Box::pin(async move { Err(TransportErrorKind::custom_str("mock error")) })
        }
    }

    /// A mock transport that drops every Nth response from a batch.
    #[derive(Clone)]
    struct DroppingTransport {
        /// Drop every Nth response (1-indexed). E.g. drop_every=3 drops the 3rd, 6th, etc.
        drop_every: usize,
        call_count: Arc<AtomicUsize>,
    }

    impl DroppingTransport {
        fn new(drop_every: usize) -> Self {
            Self {
                drop_every,
                call_count: Arc::new(AtomicUsize::new(0)),
            }
        }
    }

    impl Service<RequestPacket> for DroppingTransport {
        type Response = ResponsePacket;
        type Error = TransportError;
        type Future = TransportFut<'static>;

        fn poll_ready(&mut self, _cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
            Poll::Ready(Ok(()))
        }

        fn call(&mut self, req: RequestPacket) -> Self::Future {
            let drop_every = self.drop_every;
            self.call_count.fetch_add(1, Ordering::Relaxed);

            Box::pin(async move {
                let responses: Vec<_> = req
                    .requests()
                    .iter()
                    .enumerate()
                    .filter(|(i, _)| (i + 1) % drop_every != 0)
                    .map(|(_, r)| {
                        let value = RawValue::from_string("\"0x0\"".to_string()).unwrap();
                        Response {
                            id: r.id().clone(),
                            payload: alloy_json_rpc::ResponsePayload::Success(value),
                        }
                    })
                    .collect();

                if responses.len() == 1 {
                    Ok(ResponsePacket::Single(
                        responses.into_iter().next().unwrap(),
                    ))
                } else if responses.is_empty() {
                    Ok(ResponsePacket::Batch(vec![]))
                } else {
                    Ok(ResponsePacket::Batch(responses))
                }
            })
        }
    }

    /// A mock transport that drops responses in batch mode but succeeds for individual requests.
    #[derive(Clone)]
    struct BatchOnlyDropTransport {
        call_count: Arc<AtomicUsize>,
    }

    impl BatchOnlyDropTransport {
        fn new() -> Self {
            Self {
                call_count: Arc::new(AtomicUsize::new(0)),
            }
        }
    }

    impl Service<RequestPacket> for BatchOnlyDropTransport {
        type Response = ResponsePacket;
        type Error = TransportError;
        type Future = TransportFut<'static>;

        fn poll_ready(&mut self, _cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
            Poll::Ready(Ok(()))
        }

        fn call(&mut self, req: RequestPacket) -> Self::Future {
            self.call_count.fetch_add(1, Ordering::Relaxed);

            Box::pin(async move {
                let requests: Vec<_> = req.requests().to_vec();
                if requests.len() == 1 {
                    // Individual request — always succeed
                    let r = &requests[0];
                    let value = RawValue::from_string("\"0x0\"".to_string()).unwrap();
                    let resp = Response {
                        id: r.id().clone(),
                        payload: alloy_json_rpc::ResponsePayload::Success(value),
                    };
                    Ok(ResponsePacket::Single(resp))
                } else {
                    // Batch request — return a single non-matching response
                    // (simulates RPC returning an error with id:null for entire batch)
                    let value = RawValue::from_string("\"batch error\"".to_string()).unwrap();
                    let resp = Response {
                        id: Id::None,
                        payload: alloy_json_rpc::ResponsePayload::Success(value),
                    };
                    Ok(ResponsePacket::Single(resp))
                }
            })
        }
    }

    use std::sync::atomic::AtomicU64;

    static TEST_ID_COUNTER: AtomicU64 = AtomicU64::new(1);

    fn make_request(method: &str) -> SerializedRequest {
        let id = Id::Number(TEST_ID_COUNTER.fetch_add(1, Ordering::Relaxed));
        let req = alloy_json_rpc::Request::new(method.to_string(), id, ());
        req.serialize().unwrap()
    }

    #[tokio::test]
    async fn single_request_passthrough() {
        let mock = MockTransport::new();
        let call_count = mock.call_count.clone();
        let batch_sizes = mock.batch_sizes.clone();

        let mut transport = BatchingTransport::new(mock, BatchingConfig::default());

        let req = make_request("eth_blockNumber");
        let result = transport.call(RequestPacket::Single(req)).await;

        assert!(result.is_ok());
        // Wait a tick for the spawned task to complete
        tokio::time::sleep(Duration::from_millis(10)).await;
        assert_eq!(call_count.load(Ordering::Relaxed), 1);
        // Single request should be sent as Single (batch size = 1)
        assert_eq!(batch_sizes.lock().unwrap()[0], 1);
    }

    #[tokio::test]
    async fn concurrent_requests_batch() {
        let mock = MockTransport::new();
        let call_count = mock.call_count.clone();
        let request_count = mock.request_count.clone();

        let transport = BatchingTransport::new(
            mock,
            BatchingConfig {
                max_batch_size: 200,
                flush_interval: Duration::from_millis(5),
                ..Default::default()
            },
        );

        let n = 50;
        let mut handles = Vec::with_capacity(n);
        for i in 0..n {
            let mut t = transport.clone();
            handles.push(tokio::spawn(async move {
                let req = make_request(&format!("eth_getStorageAt_{}", i));
                t.call(RequestPacket::Single(req)).await
            }));
        }

        for handle in handles {
            let result = handle.await.unwrap();
            assert!(result.is_ok());
        }

        // All 50 requests should have been sent, potentially in fewer calls
        assert_eq!(request_count.load(Ordering::Relaxed), n);
        // Should have been batched into fewer HTTP calls than N
        let calls = call_count.load(Ordering::Relaxed);
        assert!(
            calls < n,
            "expected fewer HTTP calls than requests, got {} calls for {} requests",
            calls,
            n
        );
    }

    #[tokio::test]
    async fn transport_error_propagates() {
        let mut transport = BatchingTransport::new(
            ErrorTransport,
            BatchingConfig {
                max_batch_size: 200,
                flush_interval: Duration::from_millis(1),
                ..Default::default()
            },
        );

        let req = make_request("eth_blockNumber");
        let result = transport.call(RequestPacket::Single(req)).await;

        assert!(result.is_err());
    }

    #[tokio::test]
    async fn batch_passthrough() {
        let mock = MockTransport::new();
        let request_count = mock.request_count.clone();

        let mut transport = BatchingTransport::new(mock, BatchingConfig::default());

        let requests = vec![make_request("eth_getBalance"), make_request("eth_getCode")];
        let result = transport.call(RequestPacket::Batch(requests)).await;

        assert!(result.is_ok());
        let resp = result.unwrap();
        match resp {
            ResponsePacket::Batch(v) => assert_eq!(v.len(), 2),
            ResponsePacket::Single(_) => panic!("expected batch response"),
        }
        // Wait for spawned task
        tokio::time::sleep(Duration::from_millis(10)).await;
        assert_eq!(request_count.load(Ordering::Relaxed), 2);
    }

    #[tokio::test]
    async fn max_batch_size_splitting() {
        let mock = MockTransport::new();
        let batch_sizes = mock.batch_sizes.clone();

        let transport = BatchingTransport::new(
            mock,
            BatchingConfig {
                max_batch_size: 10,
                flush_interval: Duration::from_millis(50),
                ..Default::default()
            },
        );

        // Send 25 requests — should be split into batches of 10, 10, 5
        let n = 25;
        let mut handles = Vec::with_capacity(n);
        for i in 0..n {
            let mut t = transport.clone();
            handles.push(tokio::spawn(async move {
                let req = make_request(&format!("eth_getStorageAt_{}", i));
                t.call(RequestPacket::Single(req)).await
            }));
        }

        for handle in handles {
            let result = handle.await.unwrap();
            assert!(result.is_ok());
        }

        // Wait for all spawned tasks
        tokio::time::sleep(Duration::from_millis(100)).await;

        // All requests should have been processed
        let sizes = batch_sizes.lock().unwrap();
        let total: usize = sizes.iter().sum();
        assert_eq!(total, n);
        // No batch should exceed max_batch_size
        for &size in sizes.iter() {
            assert!(size <= 10, "batch size {} exceeds max of 10", size);
        }
    }

    #[tokio::test]
    async fn shutdown_on_drop() {
        let mock = MockTransport::new();
        let transport = BatchingTransport::new(mock, BatchingConfig::default());

        // Drop all clones — flush task should exit
        drop(transport);

        // Give the flush task time to notice and exit
        tokio::time::sleep(Duration::from_millis(20)).await;
        // If we get here without hanging, the test passes
    }

    #[tokio::test]
    async fn retry_on_missing_responses() {
        // DroppingTransport drops every 3rd response. With retry, all should succeed.
        let dropping = DroppingTransport::new(3);
        let call_count = dropping.call_count.clone();

        let transport = BatchingTransport::new(
            dropping,
            BatchingConfig {
                max_batch_size: 200,
                flush_interval: Duration::from_millis(5),
                max_missing_retries: 2,
                on_batch_rejection: None,
            },
        );

        let n = 12;
        let mut handles = Vec::with_capacity(n);
        for i in 0..n {
            let mut t = transport.clone();
            handles.push(tokio::spawn(async move {
                let req = make_request(&format!("eth_getStorageAt_{}", i));
                t.call(RequestPacket::Single(req)).await
            }));
        }

        for handle in handles {
            let result = handle.await.unwrap();
            assert!(
                result.is_ok(),
                "expected all requests to succeed after retry"
            );
        }

        // Should have made multiple calls due to retries
        tokio::time::sleep(Duration::from_millis(50)).await;
        let calls = call_count.load(Ordering::Relaxed);
        assert!(
            calls >= 2,
            "expected at least 2 calls (initial + retry), got {}",
            calls
        );
    }

    #[tokio::test]
    async fn individual_fallback_on_batch_rejection() {
        // BatchOnlyDropTransport returns a single non-matching response for batches
        // but succeeds individually. The fast-path should skip batch retries entirely.
        let dropping = BatchOnlyDropTransport::new();
        let call_count = dropping.call_count.clone();

        let transport = BatchingTransport::new(
            dropping,
            BatchingConfig {
                max_batch_size: 200,
                flush_interval: Duration::from_millis(5),
                max_missing_retries: 2,
                on_batch_rejection: None,
            },
        );

        let n = 6;
        let mut handles = Vec::with_capacity(n);
        for i in 0..n {
            let mut t = transport.clone();
            handles.push(tokio::spawn(async move {
                let req = make_request(&format!("eth_getStorageAt_{}", i));
                t.call(RequestPacket::Single(req)).await
            }));
        }

        for handle in handles {
            let result = handle.await.unwrap();
            assert!(
                result.is_ok(),
                "expected all requests to succeed via individual fallback"
            );
        }

        // Fast-path: should have made 1 batch attempt (rejected) + N individual calls.
        // NOT 2 batch attempts, because the batch was fully rejected (matched=0).
        tokio::time::sleep(Duration::from_millis(50)).await;
        let calls = call_count.load(Ordering::Relaxed);
        assert!(
            calls >= n + 1,
            "expected at least {} calls (1 rejected batch + {} individual), got {}",
            n + 1,
            n,
            calls
        );
    }

    #[tokio::test]
    async fn send_batch_with_retry_duplicate_id_detection() {
        let mock = MockTransport::new();

        // Create two requests with the same ID
        let id = Id::Number(TEST_ID_COUNTER.fetch_add(1, Ordering::Relaxed));
        let req1 = alloy_json_rpc::Request::new("eth_getBalance".to_string(), id.clone(), ());
        let req2 = alloy_json_rpc::Request::new("eth_getCode".to_string(), id, ());

        let (tx1, rx1) = oneshot::channel();
        let (tx2, rx2) = oneshot::channel();

        let pending = vec![
            PendingRequest {
                request: req1.serialize().unwrap(),
                tx: tx1,
            },
            PendingRequest {
                request: req2.serialize().unwrap(),
                tx: tx2,
            },
        ];

        send_batch_with_retry(mock, pending, 2, None).await;

        // First request (overwritten by duplicate) should get an error
        let result1 = rx1.await.unwrap();
        assert!(
            result1.is_err(),
            "expected duplicate ID error for first request"
        );
        let err_msg = format!("{}", result1.unwrap_err());
        assert!(
            err_msg.contains("duplicate request ID"),
            "expected duplicate ID error message, got: {}",
            err_msg
        );

        // Second request (kept in map) should succeed
        let result2 = rx2.await.unwrap();
        assert!(result2.is_ok(), "expected second request to succeed");
    }
}
