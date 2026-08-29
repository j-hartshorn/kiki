# Kiki

Kiki runs a production OpenAI Responses API agent against the standard OpenAI endpoint with the fixed `gpt-5.6-sol/default` model. The agent works in one persistent, read-only Python arena: values survive model steps and later requests, while its only host capability is an async, workspace-rooted `read(path)`.

## Setup

Install the exact Monty worker:

```sh
cargo install monty-runtime --version 0.0.21 --locked --root /tmp/kiki-monty-0.0.21
```

Set a non-empty OpenAI key and an absolute path to that exact worker. Optionally set `KIKI_SYSTEM_PROMPT` to the path of a UTF-8 Markdown system-prompt override.

```sh
export OPENAI_API_KEY=...
export MONTY_BIN=/tmp/kiki-monty-0.0.21/bin/monty
export KIKI_SYSTEM_PROMPT=/absolute/path/to/read_only_agent.md
```

Launch Kiki from the workspace the agent may read:

```sh
cargo run
```

## Privacy and costs

`store: false` disables retained Response objects but does not prevent user requests or workspace content printed by Python from being sent to OpenAI.

Credentials and opaque provider replay data stay hidden from Kiki records and display. Returned token usage has a versioned local estimate for `gpt-5.6-sol/default`; it is an estimate, not billing. SDK retries can be opaque and potentially billed, so the displayed total may be a lower bound or unavailable rather than a complete cost.

## Controls and availability

- Type to edit the single-line prompt. Input is limited to 16 KiB.
- `Enter` submits while idle.
- `Esc` cancels the active run.
- `Ctrl-C` shuts down.
- `Up` / `Down` scroll one line.
- `PageUp` / `PageDown` scroll ten lines.
- `Home` / `End` jump to the oldest / newest history.

Missing or invalid startup configuration opens a read-only configuration-only view, where prompt editing and submission are disabled. An OpenAI authentication failure or failed arena checkpoint recovery disables runtime submission until Kiki restarts.

## Deterministic verification

The ordinary suite is credential-free and uses local fixtures:

```sh
cargo fmt --all -- --check
cargo check --all-targets
env -u OPENAI_API_KEY MONTY_BIN=/tmp/kiki-monty-0.0.21/bin/monty cargo test --all-targets -- --test-threads=1
cargo clippy --all-targets -- -D warnings
test "$(rg -l 'async_openai' src --glob '*.rs')" = "src/openai.rs"
test "$(rg -l 'monty_(pool|types)' src --glob '*.rs')" = "src/arena.rs"
test "$(rg -l 'cap_std' src --glob '*.rs')" = "src/tools.rs"
```

## Opt-in live smoke test

The ignored live scenario is selected explicitly and requires the setup above:

```sh
OPENAI_API_KEY=... MONTY_BIN=/tmp/kiki-monty-0.0.21/bin/monty just live-test
```

It runs three requests on one controller. The complete scenario has a five-minute watchdog; every provider attempt has a two-minute deadline; if the first request starts a ninth model step, the test cancels it and fails. The cancellation request must start an attempt within 30 seconds and settle within another 30 seconds. The follow-up request must finish within two minutes. Cleanup always requests shutdown, waits up to ten seconds, then aborts and awaits the controller if necessary.
