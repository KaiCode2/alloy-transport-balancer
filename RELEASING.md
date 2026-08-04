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
cargo test --locked --all-features
cargo clippy --locked --all-targets --all-features --no-deps -- -D warnings
RUSTDOCFLAGS="-D warnings" cargo doc --locked --all-features --no-deps
cargo check --locked --no-default-features
cargo check --locked --no-default-features --features balancer
cargo check --locked --no-default-features --features batching
cargo check --locked --no-default-features --features balancer,batching
cargo build --locked --examples --all-features
cargo +1.90.0 check --locked --all-features
cargo bench --locked --no-run --all-features
bash scripts/check-authoring-hygiene.sh
cargo package --locked
cargo audit
```

The audit currently allows only the documented unmaintained-crate warnings in
`SECURITY.md`; it must report no vulnerability advisories. Recheck their active
reverse-dependency paths before publication.

Confirm that `CHANGELOG.md`, `Cargo.toml`, and the package archive all name the
same version. Inspect the archive with `cargo package --list --locked` before
publishing. Confirm every third-party `uses:` entry remains pinned to the full
commit recorded in `SECURITY.md`, not a mutable tag or branch.

## Publish

```bash
cargo publish --locked
git tag -s v0.3.0-alpha.2 -m "Release alloy-transport-balancer v0.3.0-alpha.2"
git push origin v0.3.0-alpha.2
```

Wait for 0.3.0-alpha.2 to appear in the crates.io index before packaging or
publishing downstream crates.
