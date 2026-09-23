# Kiki

Kiki runs a production OpenAI Responses API agent against the standard OpenAI endpoint. `KIKI_MODEL` selects `openai/gpt-6-astra` (a frontier model for computer/browser use, software engineering, scientific analysis, and document artifacts), `openai/gpt-6-sol`, `openai/gpt-6-luna`, `openai/gpt-5.6-sol` (the default flagship model), `openai/gpt-5.6-terra` (the intelligence/cost balance), or `openai/gpt-5.6-luna` (for cost-sensitive, high-volume work); each uses the `default` service tier. The agent works in one persistent Python arena: values survive model steps and later requests, while its async, workspace-rooted functions are `read`, `write`, `edit`, `list_dir`, `glob`, `grep`, and `run`. Independent calls can use `asyncio.gather()` and run with at most 16 active host calls.

## Setup

Install the exact Monty worker:

```sh
cargo install monty-runtime --version 0.0.22 --locked --root /tmp/kiki-monty
```

Set a non-empty OpenAI key and an absolute path to that exact worker. Optionally set `KIKI_SYSTEM_PROMPT` to the path of a UTF-8 Markdown system-prompt override, `KIKI_MODEL` to one of `openai/gpt-6-astra`, `openai/gpt-6-sol`, `openai/gpt-6-luna`, `openai/gpt-5.6-sol`, `openai/gpt-5.6-terra`, or `openai/gpt-5.6-luna`, and `KIKI_THINKING` to a supported thinking level. Kiki falls back to `openai/gpt-5.6-sol` with `medium` thinking only when no higher-precedence layer selects those values. Astra appears first in the model picker when no favourites are configured but does not change the Sol default. All six OpenAI models support `low`, `medium`, `high`, `xhigh`, and `max`; Astra defaults to `low`, while Sol and Luna default to `medium`. Higher Astra efforts may have greater latency and may require provider account eligibility (Daybreak Blue / Trusted Access or suitable rate limits); Kiki does not grant eligibility or locally block those efforts.

Export the variables:

```sh
export OPENAI_API_KEY=...
export MONTY_BIN=/tmp/kiki-monty/bin/monty
export KIKI_SYSTEM_PROMPT=/absolute/path/to/system.md
export KIKI_MODEL=openai/gpt-5.6-terra
export KIKI_THINKING=high
```

Launch Kiki from the workspace the agent may inspect and modify:

```sh
cargo run
```

Alternatively, put the variables in the ignored root `.env` file and launch with `just run`. Plain `cargo run` does not load `.env`.

## Sessions

Kiki keeps each session in local SQLite storage under `~/.kiki/sessions`. Its four CLI forms are:

```sh
kiki
kiki sessions
kiki --resume <32-lowercase-hex-id>
kiki --resume <32-lowercase-hex-id> --here
```

`kiki sessions` is a read-only, current-directory-scoped listing sorted by saved activity, newest first, then by ascending ID. Each line contains the full ID, a UTC timestamp, and the session name, falling back to its first request or `(empty session)`. It shows only sessions whose saved workspace resolves to the current workspace, so equivalent symlink spellings match. It needs neither credentials nor Monty and does not modify sessions. Normal resume has the same resolved-directory guard and resumes the saved active leaf. Only one Kiki process can write a session; another resume fails while it is owned.

Use `--here` only to deliberately relocate a session: it binds tools to the current directory. Only after successful executable restoration does Kiki save that directory and print:

```text
Warning: session relocated; retained paths, data, and external effects were not rewritten.
```

It does not rewrite retained paths, Arena data, or external effects. On normal exit or a handled runtime failure after a session opens, Kiki restores the terminal and completes session cleanup before printing `Session: <id>`. A failure before opening the session and forced termination cannot guarantee that output.

On resume, unfinished foreground work and children are settled from committed state; unfinished work is never replayed. Terminal child results are retained, but no child execution resumes. Required malformed or incompatible executable state is not reconstructed; if its history remains readable, Kiki shows it in the disabled TUI. Unsupported session-format versions are rejected without migration. Kiki never undoes external effects already completed by commands or other tools.

Each session persists its `/name`, selected model, and accepted thinking values per model. Saved session preferences override ordinary user and project settings on resume, while explicit `KIKI_MODEL` and `KIKI_THINKING` still win. After a successful executable restore, Kiki saves the effective choice, including an environment override, for a later resume. `Ctrl-S` in a picker saves a user default separately from the active session's accepted setting.

Deferred features exclude in-client session switching, separate-session forks or clones, export, sharing, compaction, branch summaries, labels, merging, bookmarks, custom themes, horizontal panning, configurable keys, automatic Monty installation, checkpoint migration, unsent-draft retention, automatic history pruning, filesystem sandboxing, and external-effect reversal. When compaction is implemented, its rows need a distinct semantic colour.

## Session tree navigation

While fully idle, `/tree` opens the searchable retained session tree; `/tree <filter>` applies an initial filter, and `Ctrl-R` opens the same picker without clearing an editor draft. With a whitespace-only prompt, two `Esc` presses less than 500 milliseconds apart also open the unfiltered tree. Navigation is unavailable while foreground or background work is active and never queues, cancels or otherwise affects that work.

The stable tree retains every durable branch in parent-before-descendant preorder. At each fork, sibling subtrees are ordered by their latest descendant revision, newest first. Linear continuations stay flat below `Session start`; indentation and joints increase only at genuine retained forks, at unlimited depth, with no structural `…` or horizontal panning. Rows use `Session start`, `User: …`, `Python…`, `Kiki: …`, `Cancelled`, and `Failed`; Python rows include recorded tool-count variants where applicable.

Rows use semantic colours: User and active connectors use the accent, Kiki and successful tool marks use success, Python and tool summaries use muted or dim text, Cancelled uses warning, and Failed uses error. Alternative branches are subdued without losing their semantic prefixes or terminal colours. `•` marks active ancestors, `●` marks the exact durable current point, and `›` with a subtle background marks the focused row without replacing its semantic foreground colours.

Tree search splits the query on whitespace and requires every case-insensitive token to match the complete, untruncated canonical row text. Matching rows retain non-selectable ancestor context. The first `Esc` with a non-empty query clears it; the next closes the tree.

`Left` folds the selected expanded visible segment, or moves to its enclosing segment start. `Right` unfolds the selected folded segment, or follows the first visible child through flat continuations to the next fork or branch end. Expanded and folded joints use `⊟` and `⊞`; a folded root uses `⊞`. Folds are transient and reset on query edits, when `Esc` clears a query, and when `Ctrl-U` changes view mode.

`Ctrl-U` toggles a transient conversation projection. It retains `Session start`, User, Kiki, Cancelled, and Failed rows, omits Python rows, and reconnects remaining rows to their nearest visible ancestor. The sole exception is the exact current Python execution, which remains visible with `●`. Search, conversation mode, and folds are view state only: they never alter the durable branches.

`Ctrl-X` copies the complete terminal-safe stored content of the focused durable entry to the local clipboard without tree connectors, markers, or preview truncation; the tree remains open and reports `Copied` or `Copy failed`. User and Kiki entries copy their complete stored text. A Python entry includes stored commentary, complete source, concise tool names and summaries, and every available durable execution observation: bounded stdout, stderr, and result, or a Python error. Omission markers remain in bounded observations and Python errors, so the copy never claims unavailable raw tool arguments or output.

The scrollable tree replaces the prompt while open, uses the full terminal width, and hides the normal prompt header. Its dock has full-width rules above the local header, between that header and the content, and below the content in the existing footer/accounting rule. The responsive one-line local header shows, as space permits, the title, active notice, live query, selected/visible count, conversation state, and key hints. Only content rows, not the header or rules, are capped at half the terminal height.

Selecting a user request restores its parent and fills only an empty editor with that request; an existing or pending draft wins. Other entries restore through the selected point. A successful replacement temporarily shows `Session restored.` until the next request is durably accepted. No-op, rejected or failed navigation creates no restoration card.

`Esc` at most two seconds after submission requests cancellation and correction. Once cancellation settles, correction restores the request's parent only when background work is already settled; otherwise it retains the cancelled branch and shows `Session was not restored because background work is active.` It neither cancels background work nor queues restoration for later. A corrected request is a sibling of the cancelled request. Rewinding model context or the Arena never reverses filesystem, command, network or other external effects.

## Model and thinking settings

Settings use this precedence: built-in defaults < user settings < project settings < saved session settings when resuming < `KIKI_MODEL` / `KIKI_THINKING`. Model IDs are qualified; thinking values are stored per model.

User defaults live at `~/.kiki/config.toml`:

```toml
model = "openai/gpt-5.6-terra"
favourites = ["openai/gpt-6-astra", "openai/gpt-6-sol", "openai/gpt-6-luna"]

[thinking]
"openai/gpt-6-astra" = "low"
"openai/gpt-6-sol" = "high"
"openai/gpt-6-luna" = "medium"
"openai/gpt-5.6-sol" = "low"
"openai/gpt-5.6-terra" = "high"
"openai/gpt-5.6-luna" = "medium"
```

Project settings use the same TOML shape at `.kiki/config.toml` in the exact current working directory; parent directories are not read:

```toml
model = "openai/gpt-5.6-terra"

[thinking]
"openai/gpt-6-astra" = "low"
"openai/gpt-5.6-sol" = "low"
"openai/gpt-5.6-terra" = "high"
"openai/gpt-5.6-luna" = "medium"
```

`favourites` is an optional, non-empty, ordered list of unique supported model IDs. User and project lists use the same precedence as other file settings; a project list replaces the user list. Favourites lead the model picker in their written order, followed by all other models. When set, only favourites may be used by subagents, and the first is the default for every new subagent. The foreground model remains independent of favourites. Without the setting, the picker uses catalogue order and subagents retain the invoking model by default and may use any supported model.

`Ctrl-S` in either model or thinking picker writes the user default separately from the active session setting. If project settings or environment variables mask it, Kiki reports that override.

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

Children have fresh model context and a model/thinking selection fixed at admission. Without an override, they use the first configured favourite; if no favourites are configured, they use the invoking model step's selection. An explicit model must be a favourite when favourites are configured and uses its accepted per-model thinking setting. Later foreground `/model` or `/thinking` changes do not alter admitted children.

Use `read_only=True` to narrow a child: `write`, `edit`, and `run` are denied by the host. A read-only child cannot widen a descendant. All agents still share the same filesystem. Completed file effects are visible to every agent and cancellation never rolls them back.

Child work continues after the foreground execution, final reply, or later request. Waiting does not join a completed child's descendants. Cancelling a task cancels its unfinished subtree while preserving completed results. Child failure cancels that child's unfinished descendants only, never its parent or siblings. There are no automatic retries, merges, result printing, or root model-context insertion: inspect a waited result deliberately.

Kiki records immutable Spawned and terminal subagent history cards separately from ordinary Python activity. Tree navigation retains terminal child data in memory but scopes root access, history cards and the sticky list to the selected branch; returning to a branch makes its tasks and latest results visible again without restarting them. `Ctrl-O` expands retained history prompts and terminal results; it never expands the compact sticky list. The sticky list shows active and completed children, retains terminal entries until the next accepted foreground request, and does not affect model context. Session token and cost accounting remains session-wide across foreground and every descendant attempt, including known failure/cancellation usage; unavailable provider usage remains unavailable rather than zero.

## Commands

`run(command, cwd=".")` starts one non-login `/bin/sh -c` command with null standard input and Kiki's inherited environment; interactive input is unavailable. `cwd` is workspace-relative, rejects absolute and parent paths, and is opened component by component without following symlinks before the validated directory descriptor is applied immediately before the command starts. There is no command-specific timeout: the five-minute whole-Arena deadline is the only limit.

A normal or non-zero exit returns `CommandResult(exit_code, stdout, stderr)`; signal termination returns `exit_code=None`. stdout and stderr are captured independently, limited to 8 MiB each by their original byte counts, then lossily decoded as UTF-8. An overflowing stream kills the command's owned process group, drains both pipes, returns no partial result, and raises `RuntimeError` after any completed effects remain. Kiki also kills that group after shell exit; background services are unsupported.

`run` has ordinary host filesystem, environment, and network authority. It is not a sandbox or approval boundary: a command can access host resources available to the Kiki process.

## Diagnostics

Set `KIKI_TRACE_FILE` to a trace-file path to opt into append-only JSONL diagnostic telemetry:

```sh
export KIKI_TRACE_FILE=/absolute/path/to/kiki-trace.jsonl
```

When unset, Kiki creates no trace file or telemetry writer. A trace records fixed process and coordinator-lock-span events, control-command events, root-run and child-lifecycle events, child-runner setup, Arena-construction and execution milestones, and provider-attempt events with timestamps and model selections. It does not record prompts, model messages, Python source or output, tool values, result values, credentials, provider responses, or arbitrary dynamic data.

A trace is diagnostic only, not authoritative or resumable history: it cannot restore a session, Arena, conversation, or child result. Use exactly one Kiki process per trace path. Multiple processes appending to the same file can interleave their event streams. Kiki flushes each event; a runtime trace-write failure is reported after normal terminal restoration and controller shutdown.

## Privacy and costs

`store: false` disables retained Response objects but does not prevent user requests or workspace content printed by Python from being sent to OpenAI.

Session storage is local and unencrypted. It includes prompts, outputs, provider replay, Arena contents, and potentially sensitive project data. On Unix, Kiki restricts session directories to `0700` and session files to `0600`; this is not a portable permission guarantee. Configured API credentials are not saved as session configuration or shown in Kiki's display, but credentials included in a prompt, output, or Arena value are ordinary session content. Local persistence does not protect against disk loss.

Returned token usage has a local direct-API Standard/`default`-tier token estimate: version `2026-09-04` for `gpt-6-astra`, snapshot `2026-09-23` for `gpt-6-sol` and `gpt-6-luna`, and version `2026-08-27` for `gpt-5.6-sol`, `gpt-5.6-terra`, and `gpt-5.6-luna`. The estimate selects that model's short-context rates through 272,000 input tokens and its long-context rates above that boundary; it is not billing. Opaque SDK retries can be billed but unreported, so a displayed cost may be a lower bound or unavailable. Batch and Flex (50% of Standard), Fast (200%), and separately billed computer/browser action surcharges are not selected, controlled, or estimated by Kiki; token estimates are not a complete provider bill.

## Controls and availability

- The configured prompt has a visible terminal cursor and a 64 KiB UTF-8 input limit. `Left` / `Right`, `Backspace`, and `Delete` operate on extended grapheme clusters; `Ctrl-B` / `Ctrl-F` move by grapheme and `Ctrl-D` deletes the next one. `Up` / `Down` move through visual wrapped rows.
- The `Command` shortcuts below work where the terminal reports `SUPER` or `META`. `Home` / `End` move within the current logical line; `Command-Left` / `Command-Right` do the same, with `Ctrl-A` / `Ctrl-E` as portable fallbacks. `Command-Up` / `Command-Down` move to the start / end of the whole prompt.
- `Option-Left` / `Option-Right`, `Alt-B` / `Alt-F`, and `Ctrl-Left` / `Ctrl-Right` move by Unicode word. `Option-Backspace` / `Ctrl-W` delete the preceding word and `Alt-D` deletes the next one. `Command-Backspace` / `Ctrl-U` delete to the logical line start; `Command-Delete` / `Ctrl-K` delete to its end.
- Typing `/` shows automatic command suggestions. With suggestions visible, `Up` / `Down` select, `Tab` or the first `Enter` completes, and a second `Enter` executes the command. Otherwise `Enter` submits while idle.
- Bracketed paste inserts a whole draft edit without submitting. It remains available while a Run is active; CRLF and lone CR become newlines, tabs become four spaces, and other controls become visible `\xNN` text. At the 64 KiB limit, Kiki retains the longest complete UTF-8 prefix that fits.
- `Shift-Enter` or `Ctrl-J` inserts a prompt newline. `Ctrl-J` remains available when a terminal cannot report modified Enter; plain `Enter` retains submission behaviour.
- `Esc` first requests cancellation of the active foreground run. At most two seconds after submission, it also requests correction; a later `Esc` is ordinary cancellation. Press it again while cancellation is pending, or press it while background subagents are active, to request background cancellation. When all work is settled and the prompt is whitespace-only, two presses less than 500 milliseconds apart open the session tree instead.
- `Ctrl-C` clears a non-empty editable prompt. With an empty or read-only prompt, it shuts down.
- `Ctrl-T` toggles all retained thinking. Thinking is shown by default; hidden blocks show a placeholder.
- `Shift-Tab` cycles the current model's thinking level and applies the final level after two quiet seconds.
- `Ctrl-P` opens the model picker without consuming the current prompt, including while a run is active.
- `/model` and `/thinking` open their selectors; exact arguments select immediately. `/name <name>` gives this session a name, shown in the footer and terminal title. `/tree` opens the session-tree picker; `Ctrl-R` and settled double-`Esc` open it without clearing a draft.
- `Ctrl-O` toggles retained arena cards and subagent history prompts and terminal results between automatic collapse and expanded display; it does not expand the sticky subagent list.
- `Up` / `Down` edit the configured prompt. In read-only configuration-only and runtime-disabled views, they retain history scrolling.
- `PageUp` / `PageDown` scroll history ten lines; manual scrolling anchors its numeric document top. New history follows only at the newest page; reaching it or `Ctrl-End` resumes following. `Ctrl-Home` jumps to the oldest history. The mouse wheel scrolls history three lines per event.
- Drag the primary mouse button over display history to select its rendered text in either direction. Resting the drag at the viewport's top or bottom edge scrolls it one row at a time; release copies a non-empty selection to the local native desktop clipboard.
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
MONTY_BIN=/tmp/kiki-monty/bin/monty \
cargo run
```

Submit this prompt:

```text
Use the Python arena in separate steps. First read `README.md` into persistent Python state named `readme` and print its character count. In a later Python step, use that retained `readme` value to count its `## ` headings and print the count. Then return a GFM response with a heading, task list, link to `README.md`, and fenced code block.
```

Check streamed GFM and thinking, partial and settled highlighted Python, live stdout, blue-to-green arena state transition for the successful prompt, red only on cancellation or error, unlabelled user cards, activity without internal identifiers, a closed rounded lifecycle frame with closed labelled source and execution sections, unboxed accounting, and the neutral borderless assistant block. Verify `Shift-Enter` and `Ctrl-J` add prompt newlines; verify the bottom-anchored wrapping prompt, `Ctrl-T`, `Ctrl-O`, scrolling, and the footer embedded in its bottom rule, including cwd front-truncation and model/thinking preservation.

## Deterministic verification

The ordinary suite is credential-free and uses local fixtures:

```sh
cargo fmt --all -- --check
cargo check --all-targets
env -u OPENAI_API_KEY MONTY_BIN=/tmp/kiki-monty/bin/monty cargo test --all-targets -- --test-threads=1
cargo clippy --all-targets -- -D warnings
test "$(rg -l 'async_openai' src --glob '*.rs')" = "src/openai.rs"
test "$(rg -l 'monty_(pool|types)' src --glob '*.rs')" = "src/arena.rs"
test "$(rg -l '(cap_std|rustix|blake3|getrandom)' src --glob '*.rs')" = "src/tools.rs"
```

## Opt-in live smoke test

The ignored live scenario is selected explicitly and requires the setup above:

```sh
OPENAI_API_KEY=... MONTY_BIN=/tmp/kiki-monty/bin/monty just live-test
```

It runs three requests on one controller. The complete scenario has a five-minute watchdog; every provider attempt has a two-minute deadline; if the first request starts a ninth model step, the test cancels it and fails. The cancellation request must start an attempt within 30 seconds and settle within another 30 seconds. The follow-up request must finish within two minutes. Cleanup always requests shutdown, waits up to ten seconds, then aborts and awaits the controller if necessary.
