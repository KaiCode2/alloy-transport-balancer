# Releasing

`alloy-transport-balancer` 0.3.0-alpha.2 is the first crate published in the
Flashblocks alpha release train. Downstream crates must use this versioned
dependency before their packages can be verified against crates.io:

1. `alloy-transport-balancer 0.3.0-alpha.2`;
2. `evm-fork-cache 0.4.0-alpha.2`;
3. `evm-amm-state 0.3.0-alpha.2`.

No release step is automatic. Prepare and review clean commits first, and do
not publish, tag, or push without explicit authorization.

## Preflight

```bash
cargo fmt --all -- --check
cargo test --all-features
cargo clippy --all-targets --all-features -- -D warnings
RUSTDOCFLAGS="-D warnings" cargo doc --all-features --no-deps
cargo +1.90.0 check --locked --all-features
cargo package --locked
cargo audit
```

Confirm that `CHANGELOG.md`, `Cargo.toml`, and the package archive all name the
same version. Inspect the archive with `cargo package --list` before publishing.

## Publish

```bash
cargo publish --locked
git tag -s v0.3.0-alpha.2 -m "Release alloy-transport-balancer v0.3.0-alpha.2"
git push origin v0.3.0-alpha.2
```

Wait for 0.3.0-alpha.2 to appear in the crates.io index before packaging or
publishing downstream crates.
