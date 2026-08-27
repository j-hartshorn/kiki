export MONTY_BIN := "/tmp/kiki-monty-0.0.21/bin/monty"

check:
    cargo check --all-targets

lint:
    cargo clippy --all-targets -- -D warnings

format:
    cargo fmt --all

run:
    cargo run
