check:
    cargo fmt --check
    cargo clippy --all-targets --locked -- -D warnings
    cargo test --locked

build:
    nix build --no-link --print-out-paths .
