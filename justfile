default: build test lint format-check

build:
    cargo build --workspace

test:
    cargo test --workspace

lint:
    cargo clippy --workspace

format-check:
    cargo fmt --check

format:
    cargo fmt
