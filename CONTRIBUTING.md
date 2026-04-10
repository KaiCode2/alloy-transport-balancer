# Contributing

Thank you for considering contributing to `alloy-transport-balancer`!

## Development

### Prerequisites

- Rust 1.85+ (edition 2024)
- Cargo

### Running Tests

```sh
# All tests with all features
cargo test --all-features

# Feature-specific
cargo test --no-default-features --features balancer
cargo test --no-default-features --features batching
```

### Linting

```sh
cargo clippy --all-features -- -D warnings
cargo fmt --check
```

### Documentation

```sh
# Build and open docs
cargo doc --all-features --open

# Verify doc tests pass
cargo test --doc --all-features
```

## Feature Flag Testing

Before submitting a PR, verify all feature combinations compile:

```sh
cargo check --no-default-features
cargo check --no-default-features --features balancer
cargo check --no-default-features --features batching
cargo check --all-features
```

## Submitting Changes

1. Fork the repository
2. Create a feature branch
3. Ensure all tests and lints pass
4. Submit a pull request with a clear description of the change

For larger changes, please open an issue first to discuss the approach.

## License

By contributing, you agree that your contributions will be licensed under the MIT license.
