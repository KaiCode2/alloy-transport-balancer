//! Proactive per-endpoint JSON-RPC request-rate admission.

use std::{
    collections::VecDeque,
    sync::{
        Arc, Mutex,
        atomic::{AtomicU64, Ordering},
    },
    time::Duration,
};
use tokio::time::Instant;

const RATE_WINDOW: Duration = Duration::from_secs(1);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum RequestRateReservation {
    Admitted,
    RetryAfter(Duration),
    PacketTooLarge,
}

#[derive(Debug)]
struct RequestRateWindow {
    admitted: VecDeque<(Instant, u32)>,
    used: u32,
}

#[derive(Debug)]
pub(crate) struct RequestRateLimiter {
    limit: u32,
    window: Mutex<RequestRateWindow>,
    total_admitted_rpc_calls: AtomicU64,
    total_capacity_rollovers: AtomicU64,
    total_capacity_waits: AtomicU64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct RequestRateSnapshot {
    pub(crate) limit: u32,
    pub(crate) used: u32,
    pub(crate) remaining: u32,
    pub(crate) total_admitted_rpc_calls: u64,
    pub(crate) total_capacity_rollovers: u64,
    pub(crate) total_capacity_waits: u64,
}

#[derive(Debug, Default)]
pub(crate) struct RequestRateFallback {
    earliest_capacity: Option<(Duration, Arc<RequestRateLimiter>)>,
    denied: Vec<Arc<RequestRateLimiter>>,
    packets_too_large: usize,
}

impl RequestRateFallback {
    pub(crate) fn rejects(
        &mut self,
        limiter: &Arc<RequestRateLimiter>,
        reservation: RequestRateReservation,
    ) -> bool {
        match reservation {
            RequestRateReservation::Admitted => false,
            RequestRateReservation::RetryAfter(retry_after) => {
                self.denied.push(Arc::clone(limiter));
                if self
                    .earliest_capacity
                    .as_ref()
                    .is_none_or(|(current, _)| retry_after < *current)
                {
                    self.earliest_capacity = Some((retry_after, Arc::clone(limiter)));
                }
                true
            }
            RequestRateReservation::PacketTooLarge => {
                self.denied.push(Arc::clone(limiter));
                self.packets_too_large += 1;
                true
            }
        }
    }

    pub(crate) fn record_rollovers(&mut self) {
        for limiter in self.denied.drain(..) {
            limiter.record_rollover();
        }
    }

    pub(crate) fn earliest_capacity(&self) -> Option<(Duration, Arc<RequestRateLimiter>)> {
        self.earliest_capacity.clone()
    }

    pub(crate) const fn packets_too_large(&self) -> usize {
        self.packets_too_large
    }
}

impl RequestRateLimiter {
    pub(crate) const fn new(limit: u32) -> Self {
        Self {
            limit,
            window: Mutex::new(RequestRateWindow {
                admitted: VecDeque::new(),
                used: 0,
            }),
            total_admitted_rpc_calls: AtomicU64::new(0),
            total_capacity_rollovers: AtomicU64::new(0),
            total_capacity_waits: AtomicU64::new(0),
        }
    }

    pub(crate) fn try_reserve(&self, units: u32) -> RequestRateReservation {
        if units > self.limit {
            return RequestRateReservation::PacketTooLarge;
        }

        let now = Instant::now();
        let mut window = self
            .window
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        Self::prune(&mut window, now);

        let reservation = self.evaluate(&window, now, units);
        if reservation == RequestRateReservation::Admitted {
            window.admitted.push_back((now, units));
            window.used += units;
            self.total_admitted_rpc_calls
                .fetch_add(u64::from(units), Ordering::Relaxed);
        }
        reservation
    }

    pub(crate) fn check(&self, units: u32) -> RequestRateReservation {
        if units > self.limit {
            return RequestRateReservation::PacketTooLarge;
        }

        let now = Instant::now();
        let mut window = self
            .window
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        Self::prune(&mut window, now);
        self.evaluate(&window, now, units)
    }

    fn evaluate(
        &self,
        window: &RequestRateWindow,
        now: Instant,
        units: u32,
    ) -> RequestRateReservation {
        let available = self.limit.saturating_sub(window.used);
        if units <= available {
            return RequestRateReservation::Admitted;
        }

        let required = units - available;
        let mut expiring = 0_u32;
        let retry_after = window
            .admitted
            .iter()
            .find_map(|(admitted_at, admitted_units)| {
                expiring = expiring.saturating_add(*admitted_units);
                (expiring >= required)
                    .then(|| (*admitted_at + RATE_WINDOW).saturating_duration_since(now))
            })
            .unwrap_or(RATE_WINDOW);
        RequestRateReservation::RetryAfter(retry_after)
    }

    pub(crate) fn record_rollover(&self) {
        self.total_capacity_rollovers
            .fetch_add(1, Ordering::Relaxed);
    }

    pub(crate) fn record_wait(&self) {
        self.total_capacity_waits.fetch_add(1, Ordering::Relaxed);
    }

    pub(crate) fn snapshot(&self) -> RequestRateSnapshot {
        let now = Instant::now();
        let mut window = self
            .window
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        Self::prune(&mut window, now);
        RequestRateSnapshot {
            limit: self.limit,
            used: window.used,
            remaining: self.limit.saturating_sub(window.used),
            total_admitted_rpc_calls: self.total_admitted_rpc_calls.load(Ordering::Relaxed),
            total_capacity_rollovers: self.total_capacity_rollovers.load(Ordering::Relaxed),
            total_capacity_waits: self.total_capacity_waits.load(Ordering::Relaxed),
        }
    }

    fn prune(window: &mut RequestRateWindow, now: Instant) {
        while window
            .admitted
            .front()
            .is_some_and(|(admitted_at, _)| now.duration_since(*admitted_at) >= RATE_WINDOW)
        {
            if let Some((_, expired)) = window.admitted.pop_front() {
                window.used = window.used.saturating_sub(expired);
            }
        }
    }
}
