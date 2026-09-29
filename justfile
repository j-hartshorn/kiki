set dotenv-load

# Kiki embeds its Monty worker (`kiki subprocess`). Tests that construct an
# arena directly need a worker path, so point them at the freshly built binary.
export MONTY_BIN := justfile_directory() / "target/debug/kiki"

# Install `kiki` into ~/.cargo/bin. Store API keys with `kiki login <provider>`.
install:
    cargo install --path . --locked

worker:
    cargo build

check:
    cargo check --all-targets

lint:
    cargo clippy --all-targets -- -D warnings

format:
    cargo fmt --all

run *args: worker
    cargo run -- {{args}}

continue: worker
    cargo run -- --continue

test: worker
    cargo test --all-targets

live-test: worker
    KIKI_MODEL=openai/gpt-5.6-sol cargo test openai::tests::live_sol -- --exact --ignored --test-threads=1

live-test-anthropic: worker
    KIKI_MODEL=anthropic/claude-opus-5-5 cargo test anthropic::tests::live_opus -- --exact --ignored --test-threads=1
