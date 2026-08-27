# Kiki

Kiki’s first vertical slice runs a deterministic scripted agent in a persistent Monty arena. The `read` capability is rooted at the directory from which Kiki is launched.

## Run

Install the exact worker version:

```sh
cargo install monty-runtime --version 0.0.21 --locked --root /tmp/kiki-monty-0.0.21
```

Launch Kiki from the workspace the agent may read:

```sh
MONTY_BIN=/tmp/kiki-monty-0.0.21/bin/monty cargo run
```

`MONTY_BIN` is required and must be an absolute path.

## Controls

- Type to edit the single-line prompt. Input is limited to 16 KiB.
- `Enter` submits while idle.
- `Esc` cancels the active run.
- `Ctrl-C` shuts down.
- `Up` / `Down` scroll one line.
- `PageUp` / `PageDown` scroll ten lines.
- `Home` / `End` jump to the oldest / newest history.

## Verify

```sh
export MONTY_BIN=/tmp/kiki-monty-0.0.21/bin/monty

cargo fmt --all -- --check
cargo check --all-targets
cargo test --all-targets -- --test-threads=1
cargo clippy --all-targets -- -D warnings
```
