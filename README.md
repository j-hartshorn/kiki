# Kiki

Kiki runs a production OpenAI Responses API agent against the standard OpenAI endpoint. `KIKI_MODEL` selects `openai/gpt-5.6-sol` (the default flagship model), `openai/gpt-5.6-terra` (the intelligence/cost balance), or `openai/gpt-5.6-luna` (for cost-sensitive, high-volume work); each uses the `default` service tier. The agent works in one persistent Python arena: values survive model steps and later requests, while its async, workspace-rooted functions are `read`, `write`, `edit`, `list_dir`, `glob`, `grep`, and `run`. Independent calls can use `asyncio.gather()` and run with at most 16 active host calls.

## Setup

Install the exact Monty worker:

```sh
cargo install monty-runtime --version 0.0.21 --locked --root /tmp/kiki-monty-0.0.21
```

Set a non-empty OpenAI key and an absolute path to that exact worker. Optionally set `KIKI_SYSTEM_PROMPT` to the path of a UTF-8 Markdown system-prompt override, `KIKI_MODEL` to one of `openai/gpt-5.6-sol`, `openai/gpt-5.6-terra`, or `openai/gpt-5.6-luna`, and `KIKI_THINKING` to a supported thinking level. Kiki falls back to `openai/gpt-5.6-sol` with `medium` thinking only when no higher-precedence layer selects those values. Export the variables:

```sh
export OPENAI_API_KEY=...
export MONTY_BIN=/tmp/kiki-monty-0.0.21/bin/monty
export KIKI_SYSTEM_PROMPT=/absolute/path/to/read_only_agent.md
export KIKI_MODEL=openai/gpt-5.6-terra
export KIKI_THINKING=high
```

Launch Kiki from the workspace the agent may inspect and modify:

```sh
cargo run
```

Alternatively, put the variables in the ignored root `.env` file and launch with `just run`. Plain `cargo run` does not load `.env`.

## Model and thinking settings

Settings use this precedence: built-in defaults < user settings < project settings < `KIKI_MODEL` / `KIKI_THINKING`. Model IDs are qualified; thinking values are stored per model.

User defaults live at `~/.kiki/config.toml`:

```toml
model = "openai/gpt-5.6-terra"

[thinking]
"openai/gpt-5.6-sol" = "low"
"openai/gpt-5.6-terra" = "high"
"openai/gpt-5.6-luna" = "medium"
```

Project settings use the same TOML shape at `.kiki/config.toml` in the exact current working directory; parent directories are not read:

```toml
model = "openai/gpt-5.6-terra"

[thinking]
"openai/gpt-5.6-sol" = "low"
"openai/gpt-5.6-terra" = "high"
"openai/gpt-5.6-luna" = "medium"
```

`Ctrl-S` in either model or thinking picker writes only the user default. If project settings or environment variables mask it, Kiki reports that override.

## Subagents

Subagent settings use the same user/project precedence as model settings. Omitted fields use these defaults:

```toml
[subagents]
allow_nested = true
max_concurrent = 8
max_starts_per_run = 64
max_depth = 4
timeout_seconds = 900
```

`max_concurrent` covers every admitted unsettled child in the session. `max_starts_per_run` covers a root request and all of its descendants. The root has depth 0, so `max_depth = 4` permits four child levels. Starts beyond a limit fail immediately; they are never queued.

### Delegation workflow

Use `start_subagent(...)` to receive a `SubagentTask` immediately, or `run_subagent(...)` when the current Python execution should wait. Keep `task.id`: `get_subagent(id)` reconnects an authorised task later, including after a foreground reply or a lost Arena variable. `refresh()`, `wait()`, and `cancel()` replace the handle's cached status. A status is only the last observation, not live state.

A child receives an independent copy of the invoking Python execution's **pre-execution** Arena checkpoint. Values committed before that execution are available; assignments made earlier in the same execution are not. Every child starts with a fresh `result_values` dictionary. A successful `wait()` returns a dictionary with `id`, `message`, and `values` keys. Export only supported neutral scalars, lists, and string-keyed dictionaries within the existing Arena and transport limits. Missing, non-dictionary, cyclic, or unsupported `result_values` make that child fail; they do not disable the foreground agent. `wait()` raises `RuntimeError` for a failed or cancelled child and `TimeoutError` for a timed-out child.

Children have fresh model context and a model/thinking selection fixed at admission. Without an override, that selection is the invoking model step's selection. An explicit model uses its accepted per-model thinking setting. Later foreground `/model` or `/thinking` changes do not alter admitted children.

Use `read_only=True` to narrow a child: `write`, `edit`, and `run` are denied by the host. A read-only child cannot widen a descendant. All agents still share the same filesystem. Completed file effects are visible to every agent and cancellation never rolls them back.

Child work continues after the foreground execution, final reply, or later request. Waiting does not join a completed child's descendants. Cancelling a task cancels its unfinished subtree while preserving completed results. Child failure cancels that child's unfinished descendants only, never its parent or siblings. There are no automatic retries, merges, result printing, or root model-context insertion: inspect a waited result deliberately.

Kiki records immutable Spawned and terminal subagent history cards separately from ordinary Python activity. `Ctrl-O` expands retained history prompts; it never expands the compact sticky list. The sticky list shows active and completed children, retains terminal entries until the next accepted foreground request, and does not affect model context. Session token and cost accounting includes foreground and every descendant attempt once, including known failure/cancellation usage; unavailable provider usage remains unavailable rather than zero.

## Commands

`run(command, cwd=".")` starts one non-login `/bin/sh -c` command with null standard input and Kiki's inherited environment; interactive input is unavailable. `cwd` is workspace-relative, rejects absolute and parent paths, and is opened component by component without following symlinks before the validated directory descriptor is applied immediately before the command starts. There is no command-specific timeout: the five-minute whole-Arena deadline is the only limit.

A normal or non-zero exit returns `CommandResult(exit_code, stdout, stderr)`; signal termination returns `exit_code=None`. stdout and stderr are captured independently, limited to 8 MiB each by their original byte counts, then lossily decoded as UTF-8. An overflowing stream kills the command's owned process group, drains both pipes, returns no partial result, and raises `RuntimeError` after any completed effects remain. Kiki also kills that group after shell exit; background services are unsupported.

`run` has ordinary host filesystem, environment, and network authority. It is not a sandbox or approval boundary: a command can access host resources available to the Kiki process.

## Privacy and costs

`store: false` disables retained Response objects but does not prevent user requests or workspace content printed by Python from being sent to OpenAI.

Credentials and opaque provider replay data stay hidden from Kiki records and display. Returned token usage has a versioned `2026-08-27` local direct-API Standard estimate for the reported `gpt-5.6-sol`, `gpt-5.6-terra`, or `gpt-5.6-luna` model. The estimate selects that model's short-context rates through 272,000 input tokens and its long-context rates above that boundary; it is not billing. Opaque SDK retries can be billed but unreported, so a displayed cost may be a lower bound or unavailable.

## Controls and availability

- The configured prompt has a visible terminal cursor and a 64 KiB UTF-8 input limit. `Left` / `Right`, `Backspace`, and `Delete` operate on extended grapheme clusters; `Ctrl-B` / `Ctrl-F` move by grapheme and `Ctrl-D` deletes the next one. `Up` / `Down` move through visual wrapped rows.
- The `Command` shortcuts below work where the terminal reports `SUPER` or `META`. `Home` / `End` move within the current logical line; `Command-Left` / `Command-Right` do the same, with `Ctrl-A` / `Ctrl-E` as portable fallbacks. `Command-Up` / `Command-Down` move to the start / end of the whole prompt.
- `Option-Left` / `Option-Right`, `Alt-B` / `Alt-F`, and `Ctrl-Left` / `Ctrl-Right` move by Unicode word. `Option-Backspace` / `Ctrl-W` delete the preceding word and `Alt-D` deletes the next one. `Command-Backspace` / `Ctrl-U` delete to the logical line start; `Command-Delete` / `Ctrl-K` delete to its end.
- `Enter` submits while idle.
- Bracketed paste inserts a whole draft edit without submitting. It remains available while a Run is active; CRLF and lone CR become newlines, tabs become four spaces, and other controls become visible `\xNN` text. At the 64 KiB limit, Kiki retains the longest complete UTF-8 prefix that fits.
- The `Shift-Enter newline` header hint appears only when enhanced modified-key reporting is available; then `Shift-Enter` inserts a newline. Without that capability, a modified Enter does nothing and plain `Enter` retains submission behaviour.
- `Esc` first cancels the active foreground run. Press it again while cancellation is pending, or press it while foreground work is idle, to cancel background subagent work.
- `Ctrl-C` clears a non-empty editable prompt. With an empty or read-only prompt, it shuts down.
- `Ctrl-T` toggles all retained thinking. Thinking is shown by default; hidden blocks show a placeholder.
- `Shift-Tab` cycles the current model's thinking level and applies the final level after two quiet seconds.
- `/model` and `/thinking` open their selectors; exact arguments select immediately. `/name <name>` gives this session a name, shown in the footer and terminal title.
- `Ctrl-O` toggles retained arena cards and subagent history prompts between automatic collapse and expanded display; it does not expand the sticky subagent list.
- `Up` / `Down` edit the configured prompt. In read-only configuration-only and runtime-disabled views, they retain history scrolling.
- `PageUp` / `PageDown` scroll history ten lines; `Ctrl-Home` / `Ctrl-End` jump to the oldest / newest history. The mouse wheel scrolls history three lines per event.
- Drag the primary mouse button over display history to select its rendered text in either direction. Resting the drag at the viewport's top or bottom edge scrolls history one row at a time; release copies a non-empty selection to the local native desktop clipboard.
- Copied rendered logical text preserves hard line breaks, removes terminal soft wraps, card decoration, and hidden content, and separates touched cards with one blank line. It is rendered text, not source Markdown.
- After a successful copy, the selection remains highlighted, flashes off and on twice, and the header shows `Copied`. A local clipboard failure is nonfatal: the selection remains highlighted and the header shows `Copy failed`.
- A completed selection survives ordinary history scrolling, but a new primary-button press, display-history content change, `Ctrl-O` expansion toggle, prompt-driven viewport change, or terminal resize clears it. Clipboard copy is local-desktop only; SSH, tmux, and OSC 52 forwarding are not supported.

Missing or invalid startup configuration opens a read-only configuration-only view, where prompt editing and submission are disabled. An OpenAI authentication failure or failed arena checkpoint recovery makes the runtime prompt read-only until Kiki restarts. `Ctrl-T` and `Ctrl-O` affect applicable retained transcript content only, not submission availability.

## Transcript presentation

Assistant responses and retained thinking stream and settle as GitHub-Flavoured Markdown (GFM), including tables, task lists, links, blockquotes, strikethrough, and fenced code. Raw HTML is shown literally and images use textual fallbacks. Fenced Markdown code is not Tree-sitter highlighted.

User requests appear in neutral cards without a `you:` label. RunId and ModelStepId remain authoritative but are not displayed; live display identifies current activity instead. Retained thinking is shown by default in a subtly grey, italic, borderless card with two-cell horizontal and one-row top-and-bottom padding; `Ctrl-T` hides every thinking block behind a `thinking hidden · Ctrl-T to show` placeholder. Thinking has no lifecycle colour. Every arena uses a full-width rounded outer frame with its existing lifecycle background: blue while in progress, green when complete, and red on error or cancellation. Ordinary settled frames have no heading; live and error frames keep concise activity, status, or diagnostics exactly once inside the frame. Each available arena section is a separately closed, muted-border box in order: `code`, `tool calls`, then `stdout`, non-empty `stderr`, `result` or `error`, interruption details, `failure`, `warning`, and current `stdout preview`. Sections with no underlying content are omitted. After an execution settles, `tool calls` retains one path-free row for every valid host call: a bounded count, fixed mutation statuses `created`, `replaced`, `applied`, `failed`, or `pending`. It is authoritative completion evidence, not lossy live progress or model context; it stores no paths, arguments, error text, or user text. Reported fields across a step's attempts aggregate into one muted, right-aligned, unboxed accounting line inside the outer frame after a blank separator; multiple attempts may be identified compactly. A final assistant response appears below its arena cards as a neutral page-background, borderless block with one-row top-and-bottom padding and no side padding or label. Execution content is ordered stdout, non-empty stderr, result or Python error, interruption details, then any cancellation warning.

Automatic collapse applies only when source exceeds 32 logical lines or combined execution-content lines exceeds 16 logical lines. It keeps the latest 32 source lines and latest 16 combined execution-content lines. Each affected code or execution section owns its skipped-line count in its top border as `<label> · N earlier line(s) omitted`; a non-empty execution section wholly consumed by the shared output budget remains as a closed titled box. Section labels and borders, live activity, failure or cancellation diagnostics, display-gap, and accounting do not consume those budgets. `tool calls` has its own automatic preview: it shows the latest 16 rows, preceded by `[N earlier tool calls omitted]` when needed. The tool-call trace is deliberately unbounded per execution; this display collapse is its only bound. `Ctrl-O` expands every retained source, tool-call, and execution row together or restores automatic collapse; it cannot recover source or execution content outside their existing retention bounds. Tree-sitter highlights exact partial and settled Python source. If highlighting fails, Kiki shows the exact source as plain text.

The configured prompt is a grapheme-aware editor with a visible terminal cursor. It is bottom anchored between full-width top and always-present bottom rules, wraps at display-cell boundaries while preserving explicit newlines, grows upward to roughly one-third of terminal height with a three-row prompt-box minimum where dimensions permit, then follows its tail or keeps a moved caret visible. The one-row footer keeps the current cwd and optional session name on the left, abbreviating home as `~`. On the right it shows reported `↑` input, `↓` output, `R` cached input, `W` cache-write input, `CH` latest cache-hit percentage, local estimated cost, latest settled-request context occupancy, and `model • thinking level`. Counts use compact lowercase `k` and `m` suffixes. Context is `<percent>/<capacity>`, yellow from 80.0% through 89.9% and red from 90.0%; it uses `272k` through the long-context pricing threshold and `1.05m` above it. Missing values are omitted. On narrow terminals Kiki front-truncates cwd, then removes metrics in display order while preserving model and thinking last. Complete and lower-bound costs deliberately use the same ordinary, unmarked `$…`; unknown or unpriceable cost is omitted, never invented as zero.

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

Check streamed GFM and thinking, partial and settled highlighted Python, live stdout, blue-to-green arena state transition for the successful prompt, red only on cancellation or error, unlabelled user cards, activity without internal identifiers, a closed rounded lifecycle frame with closed labelled source and execution sections, unboxed accounting, and the neutral borderless assistant block. On a terminal with enhanced modified-key reporting, verify `Shift-Enter` adds a prompt newline; verify the bottom-anchored wrapping prompt, `Ctrl-T`, `Ctrl-O`, scrolling, and the footer embedded in its bottom rule, including cwd front-truncation and model/thinking preservation.

## Deterministic verification

The ordinary suite is credential-free and uses local fixtures:

```sh
cargo fmt --all -- --check
cargo check --all-targets
env -u OPENAI_API_KEY MONTY_BIN=/tmp/kiki-monty-0.0.21/bin/monty cargo test --all-targets -- --test-threads=1
cargo clippy --all-targets -- -D warnings
test "$(rg -l 'async_openai' src --glob '*.rs')" = "src/openai.rs"
test "$(rg -l 'monty_(pool|types)' src --glob '*.rs')" = "src/arena.rs"
test "$(rg -l '(cap_std|rustix|blake3|getrandom)' src --glob '*.rs')" = "src/tools.rs"
```

## Opt-in live smoke test

The ignored live scenario is selected explicitly and requires the setup above:

```sh
OPENAI_API_KEY=... MONTY_BIN=/tmp/kiki-monty-0.0.21/bin/monty just live-test
```

It runs three requests on one controller. The complete scenario has a five-minute watchdog; every provider attempt has a two-minute deadline; if the first request starts a ninth model step, the test cancels it and fails. The cancellation request must start an attempt within 30 seconds and settle within another 30 seconds. The follow-up request must finish within two minutes. Cleanup always requests shutdown, waits up to ten seconds, then aborts and awaits the controller if necessary.
