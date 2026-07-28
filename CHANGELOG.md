# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

## [0.3.0-alpha.1] - 2026-07-28

### Added

- Added stable, non-secret endpoint identities and provider provenance for
  source-aware follow-up reads.
- Added a single `flashblocks` endpoint marker plus pinned endpoint leases for
  chain-specific preconfirmation subscribers.
- Added balanced, preferred, and required endpoint routing policies. Preferred
  routing tries the announcing provider first while retaining ordinary retry
  and failover; required routing stays pinned.

### Changed

- Provider selection now validates duplicate identities and rejects a
  Flashblocks-capable endpoint without an identity.
- Transport diagnostics identify providers by configured ID without exposing
  credential-bearing endpoint URLs.

## [0.2.0] - 2026-07-14

### Added

- Added `EndpointConfig` with per-provider serialized request-body ceilings and
  independent in-flight concurrency limits for heterogeneous RPC pools.
- Added opt-in gzip response negotiation through `HttpClientConfig::gzip`.

### Changed

- Expanded supported Alloy transport versions to `>=1.0.38, <1.7`.
- HTTP 413 responses now participate in endpoint failover, allowing an
  oversized request to retry on a larger-capability provider.
- Heavily throttled endpoints are skipped only when another eligible,
  non-throttled endpoint remains.

### Fixed

- Prevented a panic when request-size limits leave a heavily throttled endpoint
  as the only provider eligible to receive a request.
- Reject a zero `BatchingConfig::max_batch_size` at construction instead of
  allowing the flush worker to spin forever, and support endpoint weight totals
  larger than `u32::MAX` without overflow.
- Align the Criterion throughput baseline with the documented six-connection
  chart benchmark.

## [0.1.2] - 2026-04-10

### Fixed

- Corrected dependency bounds so Alloy 1.0 and 1.1 applications resolve a
  compatible HTTP transport and reqwest release.

## [0.1.1] - 2026-04-10

### Fixed

- Pin `alloy-transport-http` to `>=1.0.38, <1.2` and `reqwest` to `0.12` for compatibility with the alloy 1.0-1.1 ecosystem (v0.1.0 resolved to alloy 1.8 + reqwest 0.13 which is incompatible with most existing alloy projects)
- Lower MSRV from 1.91 to 1.85 (edition 2024 minimum)
- License changed to MIT only

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

[Unreleased]: https://github.com/KaiCode2/alloy-transport-balancer/compare/v0.3.0-alpha.1...HEAD
[0.3.0-alpha.1]: https://github.com/KaiCode2/alloy-transport-balancer/compare/v0.2.0...v0.3.0-alpha.1
[0.2.0]: https://github.com/KaiCode2/alloy-transport-balancer/compare/v0.1.2...v0.2.0
[0.1.2]: https://github.com/KaiCode2/alloy-transport-balancer/compare/v0.1.1...v0.1.2
[0.1.1]: https://github.com/KaiCode2/alloy-transport-balancer/compare/v0.1.0...v0.1.1
[0.1.0]: https://github.com/KaiCode2/alloy-transport-balancer/releases/tag/v0.1.0
