# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [0.1.0] - 2026-04-10

### Added

- Weighted round-robin load balancing across multiple RPC endpoints (`LoadBalancedTransport`)
- Builder pattern with `BalancerConfig`, `ThrottleConfig`, and `HttpClientConfig`
- Cross-thread domain throttle with 6-level exponential backoff and time-based decay
- Asymmetric recovery: immediate escalation on 429, gradual recovery after consecutive successes
- HTTP/2 client pooling with one `reqwest::Client` per rate-limit domain
- Transparent JSON-RPC request batching with 3-stage retry for missing responses
- Individual request fallback when RPC fully rejects a batch
- Duplicate request ID detection in batch responses
- Concurrency-limited individual fallback (`Semaphore`) to prevent connection exhaustion
- Observability: `throttle_snapshot()`, `log_throttle_summary()`, `weighted_domain_backoff()`
- `Weight` newtype for type-safe endpoint weighting (default: 100)
- `balancer` and `batching` feature flags for selective compilation
