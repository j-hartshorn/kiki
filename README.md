# Kiki

Kiki runs a production OpenAI Responses API agent against the standard OpenAI endpoint with the fixed `gpt-5.6-sol/default` model. The agent works in one persistent, read-only Python arena: values survive model steps and later requests, while its only host capability is an async, workspace-rooted `read(path)`.

## Setup

Install the exact Monty worker:

```sh
cargo install monty-runtime --version 0.0.21 --locked --root /tmp/kiki-monty-0.0.21
```

Set a non-empty OpenAI key and an absolute path to that exact worker. Optionally set `KIKI_SYSTEM_PROMPT` to the path of a UTF-8 Markdown system-prompt override. Export the variables:

```sh
export OPENAI_API_KEY=...
export MONTY_BIN=/tmp/kiki-monty-0.0.21/bin/monty
export KIKI_SYSTEM_PROMPT=/absolute/path/to/read_only_agent.md
```

Launch Kiki from the workspace the agent may read:

```sh
cargo run
```

Alternatively, put the variables in the ignored root `.env` file and launch with `just run`. Plain `cargo run` does not load `.env`.

## Privacy and costs

`store: false` disables retained Response objects but does not prevent user requests or workspace content printed by Python from being sent to OpenAI.

Credentials and opaque provider replay data stay hidden from Kiki records and display. Returned token usage has a versioned local estimate for `gpt-5.6-sol/default`; it is not billing. Opaque SDK retries can be billed but unreported, so a displayed cost may be a lower bound or unavailable.

## Controls and availability

- The configured prompt has a visible terminal cursor and a 16 KiB UTF-8 input limit. `Left` / `Right`, `Backspace`, and `Delete` operate on extended grapheme clusters; `Ctrl-B` / `Ctrl-F` move by grapheme and `Ctrl-D` deletes the next one. `Up` / `Down` move through visual wrapped rows.
- The `Command` shortcuts below work where the terminal reports `SUPER` or `META`. `Home` / `End` move within the current logical line; `Command-Left` / `Command-Right` do the same, with `Ctrl-A` / `Ctrl-E` as portable fallbacks. `Command-Up` / `Command-Down` move to the start / end of the whole prompt.
- `Option-Left` / `Option-Right`, `Alt-B` / `Alt-F`, and `Ctrl-Left` / `Ctrl-Right` move by Unicode word. `Option-Backspace` / `Ctrl-W` delete the preceding word and `Alt-D` deletes the next one. `Command-Backspace` / `Ctrl-U` delete to the logical line start; `Command-Delete` / `Ctrl-K` delete to its end.
- `Enter` submits while idle.
- The `Shift-Enter newline` header hint appears only when enhanced modified-key reporting is available; then `Shift-Enter` inserts a newline. Without that capability, a modified Enter does nothing and plain `Enter` retains submission behaviour.
- `Esc` cancels the active run.
- `Ctrl-C` clears a non-empty editable prompt. With an empty or read-only prompt, it shuts down.
- `Ctrl-T` toggles all retained reasoning. Reasoning is shown by default; hidden blocks show a placeholder.
- `Ctrl-O` toggles all arena cards between automatic collapse and expanded display.
- `Up` / `Down` edit the configured prompt. In read-only configuration-only and runtime-disabled views, they retain history scrolling.
- `PageUp` / `PageDown` scroll history ten lines; `Ctrl-Home` / `Ctrl-End` jump to the oldest / newest history. The mouse wheel scrolls history three lines per event; other mouse input is ignored.

Missing or invalid startup configuration opens a read-only configuration-only view, where prompt editing and submission are disabled. An OpenAI authentication failure or failed arena checkpoint recovery makes the runtime prompt read-only until Kiki restarts. `Ctrl-T` and `Ctrl-O` affect applicable retained transcript content only, not submission availability.

## Transcript presentation

Assistant responses and retained reasoning stream and settle as GitHub-Flavoured Markdown (GFM), including tables, task lists, links, blockquotes, strikethrough, and fenced code. Raw HTML is shown literally and images use textual fallbacks. Fenced Markdown code is not Tree-sitter highlighted.

User requests appear in neutral cards without a `you:` label. RunId and ModelStepId remain authoritative but are not displayed; live display identifies current activity instead. Retained reasoning is shown by default in a subtly grey, italic, borderless card with two-cell horizontal and one-row top-and-bottom padding; `Ctrl-T` hides every reasoning block behind a `reasoning hidden · Ctrl-T to show` placeholder. Reasoning has no lifecycle colour. Every arena uses a full-width rounded outer frame with its existing lifecycle background: blue while in progress, green when complete, and red on error or cancellation. Ordinary settled frames have no heading; live and error frames keep concise activity, status, or diagnostics exactly once inside the frame. Each available arena section is a separately closed, muted-border box in order: `code`, `tool calls`, then `stdout`, non-empty `stderr`, `result` or `error`, interrupted `read`, `failure`, `warning`, and current `stdout preview`. Sections with no underlying content are omitted. After an execution settles, `tool calls` retains one path-free row for every valid host `read`: `read · N lines`, `read · failed`, or `read · pending`. It is authoritative completion evidence, not lossy live progress or model context; it stores no paths, arguments, error text, or user text. Reported fields across a step's attempts aggregate into one muted, right-aligned, unboxed accounting line inside the outer frame after a blank separator; multiple attempts may be identified compactly. A final assistant response appears below its arena cards as a neutral page-background, borderless block with one-row top-and-bottom padding and no side padding or label. Execution content is ordered stdout, non-empty stderr, result or Python error, interruption details, then any cancellation warning.

Automatic collapse applies only when source exceeds 32 logical lines or combined execution-content lines exceeds 16 logical lines. It keeps the latest 32 source lines and latest 16 combined execution-content lines. Each affected code or execution section owns its skipped-line count in its top border as `<label> · N earlier line(s) omitted`; a non-empty execution section wholly consumed by the shared output budget remains as a closed titled box. Section labels and borders, live activity, failure or cancellation diagnostics, display-gap, and accounting do not consume those budgets. `tool calls` has its own automatic preview: it shows the latest 16 rows, preceded by `[N earlier tool calls omitted]` when needed. The tool-call trace is deliberately unbounded per execution; this display collapse is its only bound. `Ctrl-O` expands every retained source, tool-call, and execution row together or restores automatic collapse; it cannot recover source or execution content outside their existing retention bounds. Tree-sitter highlights exact partial and settled Python source. If highlighting fails, Kiki shows the exact source as plain text.

The configured prompt is a grapheme-aware editor with a visible terminal cursor. It is bottom anchored between full-width top and always-present bottom rules, wraps at display-cell boundaries while preserving explicit newlines, grows upward to roughly one-third of terminal height with a three-row prompt-box minimum where dimensions permit, then follows its tail or keeps a moved caret visible. The session footer sits right-aligned inside the prompt's bottom rule, in this order when reported: `↑` reported input, `↓` reported output, `R` cached input, `W` cache-write input, `r` reasoning output, `Σ` provider-reported total, `$` local estimated cost. It never adds or moves a row; on narrow terminals it truncates its left side with `…` to preserve the rightmost totals and cost. `r` is an informational subset of output, not an additional total; `Σ` is not recomputed locally. Missing values are omitted. The footer is absent without metrics and in the configuration-only view. Complete and lower-bound costs deliberately use the same ordinary, unmarked `$…`; unknown or unpriceable cost is omitted, never invented as zero.

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

Check streamed GFM and reasoning, partial and settled highlighted Python, live stdout, blue-to-green arena state transition for the successful prompt, red only on cancellation or error, unlabelled user cards, activity without internal identifiers, a closed rounded lifecycle frame with closed labelled source and execution sections, unboxed accounting, and the neutral borderless assistant block. On a terminal with enhanced modified-key reporting, verify `Shift-Enter` adds a prompt newline; verify the bottom-anchored wrapping prompt, `Ctrl-T`, `Ctrl-O`, scrolling, and the right-aligned footer embedded in its bottom rule, including left-side ellipsis truncation that preserves the cost.

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
