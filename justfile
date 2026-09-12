set dotenv-load

monty_root := "/tmp/kiki-monty"
export MONTY_BIN := monty_root + "/bin/monty"

monty:
    test -x "$MONTY_BIN" || cargo install monty-runtime --version 0.0.21 --locked --root "{{ monty_root }}"

check:
    cargo check --all-targets

lint:
    cargo clippy --all-targets -- -D warnings

format:
    cargo fmt --all

run: monty
    cargo run

test: monty
    cargo test --all-targets

live-test: monty
    cargo test openai::tests::live_sol -- --exact --ignored --test-threads=1
