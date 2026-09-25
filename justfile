set dotenv-load

monty_root := "/tmp/kiki-monty"
export MONTY_BIN := monty_root + "/bin/monty"

monty:
    test -x "$MONTY_BIN" && test "$("$MONTY_BIN" --version)" = "monty-runtime 0.0.22" || cargo install monty-runtime --version 0.0.22 --locked --root "{{ monty_root }}" --force

check:
    cargo check --all-targets

lint:
    cargo clippy --all-targets -- -D warnings

format:
    cargo fmt --all

run *args: monty
    cargo run -- {{args}}

continue: monty
    cargo run -- --continue

test: monty
    cargo test --all-targets

live-test: monty
    KIKI_MODEL=openai/gpt-5.6-sol cargo test openai::tests::live_sol -- --exact --ignored --test-threads=1

live-test-anthropic: monty
    KIKI_MODEL=anthropic/claude-opus-5-5 cargo test anthropic::tests::live_opus -- --exact --ignored --test-threads=1
