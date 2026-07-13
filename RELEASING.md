# Releasing

`alloy-transport-balancer` is the first crate published in the reactive AMM
runtime release train. Downstream crates must use a versioned dependency before
their packages can be verified against crates.io.

## Preflight

```bash
cargo fmt --all -- --check
cargo test --all-features
cargo clippy --all-targets --all-features -- -D warnings
RUSTDOCFLAGS="-D warnings" cargo doc --all-features --no-deps
cargo +1.88 check --locked --all-features
cargo package --locked
cargo audit
```

Confirm that `CHANGELOG.md`, `Cargo.toml`, and the package archive all name the
same version. Inspect the archive with `cargo package --list` before publishing.

## Publish

```bash
cargo publish --locked
git tag -s v0.2.0 -m "Release alloy-transport-balancer v0.2.0"
git push origin v0.2.0
```

Do not publish downstream crates until the new version is visible through the
crates.io index.
