# Security Policy

Only the latest published pre-1.0 line receives security fixes. Report a
vulnerability privately through this repository's GitHub Security tab. If that
channel is unavailable, open a minimal public issue requesting private contact
without disclosing the vulnerability.

Include the crate version, Rust version, enabled features, impact, and a minimal
reproduction where possible.

## Dependency and CI policy

Release validation audits the checked-in lockfile with `cargo-audit 0.22.2`.
The workflow never regenerates the lockfile before auditing, so the reviewed,
reproducible dependency graph is the graph that must pass.

The current lockfile reports two unmaintained-crate warnings, not vulnerability
advisories. `derivative 2.2.0` is an unreachable lockfile entry under the
all-feature/all-target graph. `paste 1.0.15` is an active compile-time macro in
the pinned Alloy graph and is not called by this crate at runtime. A new
vulnerability advisory remains release-blocking; any change to either warning's
reachability requires renewed review.

Every third-party workflow action is pinned to an immutable full commit. The
adjacent comments retain the reviewed human-readable upstream reference.

| Action | Reviewed ref | Pinned commit |
| --- | --- | --- |
| [`actions/checkout`](https://github.com/actions/checkout/releases/tag/v4.4.0) | `v4.4.0` | `11d5960a326750d5838078e36cf38b85af677262` |
| [`dtolnay/rust-toolchain`](https://github.com/dtolnay/rust-toolchain/commit/4cda84d5c5c54efe2404f9d843567869ab1699d4) | `stable` | `4cda84d5c5c54efe2404f9d843567869ab1699d4` |
| [`Swatinem/rust-cache`](https://github.com/Swatinem/rust-cache/releases/tag/v2.9.1) | `v2.9.1` | `c19371144df3bb44fab255c43d04cbc2ab54d1c4` |

The stable, pinned nightly, and Rust 1.90 jobs use the same reviewed toolchain
action commit and pass their requested toolchain explicitly. Updating any
action requires verifying the new upstream ref and full commit before changing
the pin.
