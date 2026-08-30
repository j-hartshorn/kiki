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

Credentials and opaque provider replay data stay hidden from Kiki records and display. Returned token usage has a versioned local estimate for `gpt-5.6-sol/default`; it is not billing. Opaque SDK retries can be billed but unreported, so a displayed cost may be a lower bound or unavailable.

## Controls and availability

- Type to edit the single-line prompt. Input is limited to 16 KiB.
- `Enter` submits while idle.
- `Esc` cancels the active run.
- `Ctrl-C` shuts down.
- `Ctrl-T` toggles all retained reasoning. Reasoning is shown by default; hidden blocks show a placeholder.
- `Ctrl-O` toggles all arena cards between automatic collapse and expanded display.
- `Up` / `Down` scroll one line.
- `PageUp` / `PageDown` scroll ten lines.
- `Home` / `End` jump to the oldest / newest history.

Missing or invalid startup configuration opens a read-only configuration-only view, where prompt editing and submission are disabled. An OpenAI authentication failure or failed arena checkpoint recovery disables runtime submission until Kiki restarts. `Ctrl-T` and `Ctrl-O` affect applicable retained transcript content only, not submission availability.

## Transcript presentation

Assistant responses and retained reasoning stream and settle as GitHub-Flavoured Markdown (GFM), including tables, task lists, links, blockquotes, strikethrough, and fenced code. Raw HTML is shown literally and images use textual fallbacks. Fenced Markdown code is not Tree-sitter highlighted.

Retained reasoning is shown by default in a subtly grey, padded italic card; `Ctrl-T` hides every reasoning block behind a `reasoning hidden · Ctrl-T to show` placeholder. Reasoning has no lifecycle colour. Arena cards are borderless and full width: blue while in progress, green when complete, and red on error or cancellation. A final assistant response appears below its arena cards. Each arena card includes its title and status, source, execution content, display-gap and usage/cost metadata. Execution content is ordered stdout, non-empty stderr, result or Python error, interruption details, then any cancellation warning.

Automatic collapse applies only when source exceeds 32 logical lines or combined execution content exceeds 16 logical lines. It keeps the source head and execution-output tail, marking omitted lines; titles, status, display-gap, usage, and cost metadata do not consume those budgets. `Ctrl-O` expands retained card content or restores automatic collapse; it cannot recover content outside retention bounds. Tree-sitter highlights exact partial and settled Python source. If highlighting fails, Kiki shows the exact source as plain text.

The session footer appears in one row immediately beneath the prompt, in this order when reported: `↑` reported input, `↓` reported output, `R` cached input, `W` cache-write input, `r` reasoning output, `Σ` provider-reported total, `$` local estimated cost. `r` is an informational subset of output, not an additional total; `Σ` is not recomputed locally. Missing values are omitted. The row truncates with an ellipsis on narrow terminals and is absent in the configuration-only view. Complete and lower-bound costs deliberately use the same ordinary, unmarked `$…`; unknown or unpriceable cost is omitted, never invented as zero.

## Manual interactive transcript demonstration

Start Kiki with live credentials:

```sh
OPENAI_API_KEY=... \
MONTY_BIN=/tmp/kiki-monty-0.0.21/bin/monty \
cargo run
```

Submit this prompt:

```text
Use the Python arena in separate steps. First read `README.md` into persistent Python state named `readme` and print its character count. In a later Python step, use that retained `readme` value to count its `## ` headings and print the count. Then return a GFM response with a heading, task list, link to `README.md`, and fenced code block.
```

Check streamed GFM and reasoning, partial and settled highlighted Python, live stdout, blue-to-green arena state transition for the successful prompt, red only on cancellation or error, `Ctrl-T` and `Ctrl-O`, scrolling, and the session footer.

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
