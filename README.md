# Kiki

Kiki runs a Python agent with either the OpenAI Responses API or the Anthropic Messages API. `KIKI_MODEL` selects `openai/gpt-6-astra` (a frontier model for computer/browser use, software engineering, scientific analysis, and document artifacts), `openai/gpt-6.1-sol`, `openai/gpt-6-sol`, `openai/gpt-6-luna`, `openai/gpt-5.6-sol` (the default flagship model), `openai/gpt-5.6-terra` (the intelligence/cost balance), `openai/gpt-5.6-luna` (for cost-sensitive, high-volume work), `anthropic/claude-opus-5-5`, `anthropic/claude-sonnet-5-5`, or `anthropic/claude-fable-5-1`. OpenAI uses the `default` service tier; Anthropic uses `standard`. The agent works in one persistent Python arena: values survive model steps and later requests, while its async, workspace-rooted functions are `read`, `write`, `edit`, `list_dir`, `glob`, `grep`, and `run`. Independent calls can use `asyncio.gather()` and run with at most 16 active host calls. By default, Monty's `ty` checker rejects model code with type errors before execution and returns diagnostics to the model; clean checks add no extra feedback. [Known Monty type-check limitations](docs/issues/monty-repl-type-check.md) are recorded separately.

## Setup

Install the `kiki` command into `~/.cargo/bin` (make sure that directory is on your `PATH`):

```sh
just install   # or: cargo install --path . --locked
```

The Monty worker is embedded: `kiki` runs itself as `kiki subprocess` for its Python arena, so no separate `monty-runtime` install is needed. `kiki --version` prints `kiki <version> (monty-runtime 0.0.22)`. Setting `MONTY_BIN` to the absolute path of another `kiki` or a `monty-runtime` 0.0.22 executable overrides the embedded worker.

Store an API key for each provider you use:

```sh
kiki login openai      # prompts for the key without echoing it; Ctrl-C cancels
kiki login anthropic
kiki logout anthropic  # removes a stored key
```

Keys are stored in `~/.kiki/auth.toml`. Kiki always writes that file with owner-only mode `0600`; if it has to create `~/.kiki` it makes it `0700`, but an existing `~/.kiki` keeps its mode. Kiki refuses to read or write stored keys unless `~/.kiki` and the auth file belong to you, `~/.kiki` is not writable by other users (`chmod go-w ~/.kiki`), and other users cannot access the auth file (`chmod 600 ~/.kiki/auth.toml`). The checks apply to the opened file that Kiki then reads. Surrounding whitespace in a stored key is ignored; if the selected provider's key comes from the environment, the unusable file is shown as a startup warning instead. Concurrent `kiki login`/`logout` runs are serialised with `~/.kiki/auth.lock`. Piping a key into `kiki login <provider>` also works. `OPENAI_API_KEY` and `ANTHROPIC_API_KEY` still work and take precedence over stored keys.

A non-empty key is needed for the selected model's provider: OpenAI or Anthropic. The keys are independently optional; with the default `openai/gpt-5.6-sol` model, only an OpenAI key is needed. On resume, the saved model requires its provider's key unless an explicit `KIKI_MODEL` overrides it. Optionally set `KIKI_SYSTEM_PROMPT` to the path of a UTF-8 Markdown system-prompt override, `KIKI_MODEL` to any supported qualified model ID above, and `KIKI_THINKING` to a supported thinking level. Kiki falls back to `openai/gpt-5.6-sol` with `medium` thinking only when no higher-precedence layer selects those values. Astra appears first in the model picker when no favourites are configured and an OpenAI key is present, but does not change the Sol default. All ten models support `low`, `medium`, `high`, `xhigh`, and `max`; Astra defaults to `low`, Opus to `medium`, Sonnet and Fable to `high`, and the remaining OpenAI models to `medium`. Higher Astra efforts may have greater latency and may require provider account eligibility (Daybreak Blue / Trusted Access or suitable rate limits); Kiki does not grant eligibility or locally block those efforts.

Set `KIKI_TYPE_CHECK=off` at startup to disable checking for a new session or opt out when resuming one. Unset or `on` leaves checking enabled for new sessions. An opt-out persists across restarts and tree navigation even after the variable is removed or set to `on`; start a new session to re-enable checking. Other values are rejected.

Optional environment variables:

```sh
export KIKI_SYSTEM_PROMPT=/absolute/path/to/system.md
export KIKI_MODEL=anthropic/claude-opus-5-5
export KIKI_THINKING=high
```

Launch Kiki from the workspace the agent may inspect and modify:

```sh
kiki
```

When developing, `just run` builds and launches the checkout, loading the ignored root `.env` file; plain `cargo run` also works, but does not load `.env`.

## Agent Skills

Put a skill in `<project>/.agents/skills/<name>/SKILL.md` or `~/.agents/skills/<name>/SKILL.md`. The UTF-8 file needs YAML frontmatter with a nonempty `description` and a nonempty Markdown body. Kiki flattens the description to one catalog line. Its `name` defaults to the directory name; names use lowercase letters, digits, and hyphens. For a skill that only the user may invoke, add `disable-model-invocation: true` to the frontmatter. For example:

```md
---
name: review
description: Review a change and report concrete findings
---
Follow this review workflow...
```

Kiki looks for project skills from the launch directory up to the nearest Git root within 32 ancestors, then user skills. Without a Git root in that bound, it checks only the launch directory and the user skill root. A nearer project directory wins over a farther one, and project skills win over user skills with the same name. On fresh launch or resume, Kiki asks before enabling project skills from an untrusted scope. **Skip** applies to this process; **Trust** saves a positive decision for later launches. User skills need no project trust. The initial model prompt shows accepted model-invocable names and descriptions, not their bodies. Set `KIKI_SKILLS_CATALOG_PROMPT` to a UTF-8 Markdown file to override the catalog guidance included with those descriptions. The agent can activate one with `await load_skill("review")`; that call returns a short acknowledgement and inserts the captured instructions into its next model context. Use `/skill:review [request]` to activate a skill yourself, including one marked `disable-model-invocation: true`. A bare `/skill:review` starts a turn in which the agent can ask for task details.

Skills may include references and other files. The agent can read UTF-8 text with `await read("skill://review/references/checklist.md")` and list a skill directory with `await list_dir("skill://review/")`. These paths stay inside the selected skill directory and cannot traverse parent paths or inner symlinks. `write` and `edit` do not accept `skill://` paths. Reading a skill resource does not authorize a workspace write. Kiki captures each activated `SKILL.md` body from the process catalog; changing that file takes a new launch or resume and an explicit reactivation. Linked resources are read live, so their contents can change during a process. Skill instructions do not grant permission for actions outside your request.

## Sessions

Kiki keeps each session in local SQLite storage under `~/.kiki/sessions`. Its five CLI forms are:

```sh
kiki
kiki sessions
kiki --continue
kiki --resume <32-lowercase-hex-id>
kiki --resume <32-lowercase-hex-id> --here
```

`kiki sessions` is a read-only, current-directory-scoped listing sorted by saved activity, newest first, then by ascending ID. Each line contains the full ID, a UTC timestamp, and the session name, falling back to its first request or `(empty session)`. It shows only sessions whose saved workspace resolves to the current workspace, so equivalent symlink spellings match. It needs neither credentials nor Monty and does not modify sessions. Normal resume has the same resolved-directory guard and resumes the saved active leaf. Only one Kiki process can write a session; another resume fails while it is owned.

`kiki --continue` resumes the first session `kiki sessions` would show: the most recently active session saved for the current workspace, with ties broken by ascending ID. It skips unreadable entries with the same warnings. When no session matches, it fails without creating a session. If the selected session cannot be opened, for example because another process owns it, Kiki reports that error rather than falling back to an older session. It does not accept `--here`; relocation needs an explicit ID. With the variables in `.env`, `just continue` runs `cargo run -- --continue` after the same Monty check as `just run`.

Use `--here` only to deliberately relocate a session: it binds tools to the current directory. Only after successful executable restoration does Kiki save that directory and print:

```text
Warning: session relocated; retained paths, data, and external effects were not rewritten.
```

It does not rewrite retained paths, Arena data, or external effects. On normal exit or a handled runtime failure after a session opens, Kiki restores the terminal and completes session cleanup before printing `Session: <id>`. A failure before opening the session and forced termination cannot guarantee that output.

On resume, unfinished foreground work and children are settled from committed state; unfinished work is never replayed. Terminal child results are retained, but no child execution resumes. Required malformed or incompatible executable state is not reconstructed; if its history remains readable, Kiki shows it in the disabled TUI. Checkpoints saved before Monty type checking was enabled, or with earlier type-check stubs, cannot be resumed because loading them would bypass checks or retain outdated stubs. Start a new session for executable work. Unsupported session-format versions are rejected without migration. Kiki never undoes external effects already completed by commands or other tools.

Each session persists its `/name`, selected model, and accepted thinking values per model. Saved session preferences override ordinary user and project settings on resume, while explicit `KIKI_MODEL` and `KIKI_THINKING` still win. After a successful executable restore, Kiki saves the effective choice, including an environment override, for a later resume. `Ctrl-S` in a picker saves a user default separately from the active session's accepted setting.

Deferred features exclude in-client session switching, separate-session forks or clones, export, sharing, branch summaries, labels, merging, bookmarks, custom themes, horizontal panning, configurable keys, automatic Monty installation, checkpoint migration, unsent-draft retention, automatic history pruning, filesystem sandboxing, and external-effect reversal.

## Context compaction

`/compact [instructions]` manually summarises a removable prefix of the selected Model Context, even below the automatic threshold or when automatic compaction is disabled. The optional trimmed remainder is additional focus for the summary. Kiki keeps the command draft unchanged and temporarily read-only until Runtime acknowledges it: Accepted clears it, while Busy or Disabled leaves it byte-for-byte intact and editable whenever the ordinary runtime state permits.

Compaction settings use field-wise user/project precedence in the same files as the model settings. Both token counts must be positive integers; omitted fields use these defaults:

```toml
[compaction]
enabled = true
reserve_tokens = 16384
keep_recent_tokens = 20000
```

`reserve_tokens` provides summary-output headroom. `keep_recent_tokens` selects an approximate recent tail at whole Model Context entry boundaries, so an indivisible entry can exceed the target and completed code is never split from its execution feedback. There is no environment, session, picker, or `Ctrl-S` override. Setting `enabled = false` disables threshold checks and overflow recovery, but not manual `/compact`.

When enabled, Kiki estimates the committed selected context immediately before **each normal root Model Step**, after the new request or preceding code execution has been saved. The first check includes the new request; there is no pre-admission or post-final compaction. A strict estimate above the selected model's context window minus `reserve_tokens` triggers a summary when there is a removable prefix. The subtraction stops at zero if reserve exceeds the window. A structural no-op skips the summary and sends the scheduled step; a provider-started failed threshold summary is stored and the step proceeds with unchanged context. The estimate uses model-matched usage when eligible, otherwise an approximate byte-based count of what the selected provider will see. It is **not** an exact provider token count and does not guarantee fit. The selected model and thinking level are fixed for a Run, including its later steps, summaries and any retry; changes made mid-Run apply to the next Run. Both OpenAI and Anthropic support threshold and manual compaction.

For a recognised OpenAI context-token overflow, Kiki tries to compact and retry **once in the same pending Model Step**. If it cannot remove a valid prefix, the summary fails, or the retry fails, the Run ends with a diagnostic; it does not loop. `Esc` cancels the containing Run: cancellation before a summary attempt writes no Compaction Entry, while cancellation after an Applied outcome retains that entry and suppresses the retry. The initial failed call's bounded diagnostic, partial generation when available, and billable attempts remain separate from the retry's result and usage. Anthropic still supports threshold and manual compaction, but its positive token-overflow diagnostic has not been verified from a current response, so Anthropic failures are **not** retried as context overflow. HTTP 413 `request_too_large` is a request-byte limit, not a token-overflow signal.

An Applied compaction replaces the removable prefix in projected Model Context with a summary and retains the exact ordinary tail. Repeated compaction appends a new outcome whose summary supersedes the previous summary in that projection and moves the retained boundary forward; it never revisits entries before the previous boundary. If no valid prefix can be removed, the operation is a no-op. Existing Session Entries, including older Compaction Entries, and pre-compaction display history remain immutable and visible throughout. The complete selected ancestry is not deleted or rewritten.

Compaction reuses the prepared leaf's exact persistent Arena checkpoint, so Python values and authorisations continue without reconstructing or probing live Arena state. The summary prompt asks the model for an `Arena State` section based on conversation and execution evidence, with uncertain availability and types marked as such; Kiki neither inspects the checkpoint nor validates that the model returned this section, so it is not a complete live inventory. Kiki also derives sorted, deduplicated cumulative `<read-files>` and `<modified-files>` sections from successful root-agent `read`, `write`, and `edit` results observed on the selected ancestry through the summarised range. Modified paths dominate reads, and empty sections are omitted. These normalised workspace-relative paths are continuity metadata, not a complete filesystem audit: unobserved cancellation races and old entries cannot be inferred or backfilled, and child-agent paths remain private unless a child result is explicitly retrieved. Kiki appends the lists to the stored summary after generation, making them part of Model Context on subsequent requests and copying them with an Applied outcome; they are not separately added to ordinary tool cards, execution feedback, tree execution rows/search, or execution copy.

Compaction is foreground work without a synthetic Run or Model Step. Automatic summaries belong to the containing Run, and overflow retry reuses its pending Model Step. While generation is active, status reads `compacting context`; Submit, duplicate `/compact`, and tree navigation are busy, and `Esc` cancels the foreground token. If manual compaction is requested during a Run, one command is retained, the Run settles first, then compaction prepares against the resulting Active Leaf with a fresh token. An already-pending correction finishes before compaction and determines which leaf is used. `Esc` before retained generation starts cancels only that follow-up; there is no general queue. Background subagents continue independently.

Provider-started Applied, Failed, and Cancelled outcomes are immutable durable entries when the process survives settlement and storage remains writable. Only Applied changes projected Model Context; Failed and Cancelled retain the parent's context. Transcript cards are collapsed by default; `Ctrl-O` reveals the exact Applied Markdown or Failed diagnostic, shows the Cancelled state, and appends available summary-call accounting. Full and conversation `/tree` views show exact one-line rows `Compaction · {reason} · applied · {measurement} tokens`, `Compaction · {reason} · failed`, or `Compaction · {reason} · cancelled`, using a distinct semantic colour. Search indexes only that complete row. `Ctrl-X` copies only the stored Applied summary, bounded Failed diagnostic, or exact Cancelled reason label. Selecting any outcome restores through it, and the next request branches from that entry; sibling branches retain their own compaction projection.

Sessions use format version 2. Version-1 sessions are rejected before SQLite is opened or mutated; there is no migration or compatibility default. Overflow recovery evidence is an optional addition to version-2 terminals; older version-2 entries remain readable.

## Session tree navigation

While fully idle, `/tree` opens the searchable retained session tree; `/tree <filter>` applies an initial filter, and `Ctrl-R` opens the same picker without clearing an editor draft. With a whitespace-only prompt, two `Esc` presses less than 500 milliseconds apart also open the unfiltered tree. Navigation is unavailable while foreground or background work is active and never queues, cancels or otherwise affects that work.

The stable tree retains every durable branch in parent-before-descendant preorder. At each fork, sibling subtrees are ordered by their latest descendant revision, newest first. Linear continuations stay flat below `Session start`; indentation and joints increase only at genuine retained forks, at unlimited depth, with no structural `…` or horizontal panning. Rows use `Session start`, `User: …`, `Python…`, `Kiki: …`, `Cancelled`, `Failed`, and the Compaction forms described above; Python rows include recorded tool-count variants where applicable.

Rows use semantic colours: User and active connectors use the accent, Kiki and successful tool marks use success, Python and tool summaries use muted or dim text, Cancelled uses warning, Failed uses error, and Compaction uses its own semantic colour. Alternative branches are subdued without losing their semantic prefixes or terminal colours. `•` marks active ancestors, `●` marks the exact durable current point, and `›` with a subtle background marks the focused row without replacing its semantic foreground colours.

Tree search splits the query on whitespace and requires every case-insensitive token to match the complete, untruncated canonical row text. Matching rows retain non-selectable ancestor context. The first `Esc` with a non-empty query clears it; the next closes the tree.

`Left` folds the selected expanded visible segment, or moves to its enclosing segment start. `Right` unfolds the selected folded segment, or follows the first visible child through flat continuations to the next fork or branch end. Expanded and folded joints use `⊟` and `⊞`; a folded root uses `⊞`. Folds are transient and reset on query edits, when `Esc` clears a query, and when `Ctrl-U` changes view mode.

`Ctrl-U` toggles a transient conversation projection. It retains `Session start`, User, Kiki, Cancelled, Failed, and Compaction rows, omits Python rows, and reconnects remaining rows to their nearest visible ancestor. The sole exception is the exact current Python execution, which remains visible with `●`. Search, conversation mode, and folds are view state only: they never alter the durable branches.

`Ctrl-X` copies the complete terminal-safe stored content of the focused durable entry to the local clipboard without tree connectors, markers, or preview truncation; the tree remains open and reports `Copied` or `Copy failed`. User and Kiki entries copy their complete stored text. A Python entry includes stored commentary, complete source, concise tool names and summaries, and every available durable execution observation: bounded stdout, stderr, and result, or a Python error. A Compaction entry copies only its outcome payload as described above. Omission markers remain in bounded observations and Python errors, so the copy never claims unavailable raw tool arguments or output.

The scrollable tree replaces the prompt while open, uses the full terminal width, and hides the normal prompt header. Its dock has full-width rules above the local header, between that header and the content, and below the content in the existing footer/accounting rule. The responsive one-line local header shows, as space permits, the title, active notice, live query, selected/visible count, conversation state, and key hints. Only content rows, not the header or rules, are capped at half the terminal height.

Selecting a user request restores its parent and fills only an empty editor with that request; an existing or pending draft wins. Other entries restore through the selected point. A successful replacement temporarily shows `Session restored.` until the next request is durably accepted. No-op, rejected or failed navigation creates no restoration card.

`Esc` at most two seconds after submission requests cancellation and correction. Once cancellation settles, correction restores the request's parent only when background work is already settled; otherwise it retains the cancelled branch and shows `Session was not restored because background work is active.` It neither cancels background work nor queues restoration for later. A corrected request is a sibling of the cancelled request. Rewinding model context or the Arena never reverses filesystem, command, network or other external effects.

## Model and thinking settings

Settings use this precedence: built-in defaults < user settings < project settings < saved session settings when resuming < `KIKI_MODEL` / `KIKI_THINKING`. Model IDs are qualified; thinking values are stored per model.

User defaults live at `~/.kiki/config.toml`:

```toml
model = "anthropic/claude-opus-5-5"
favourites = ["anthropic/claude-opus-5-5", "openai/gpt-6-astra", "openai/gpt-6-sol"]
cross_provider_reasoning = "inline"

[anthropic]
cache_ttl = "5m"

[thinking]
"openai/gpt-6-astra" = "low"
"openai/gpt-6.1-sol" = "medium"
"openai/gpt-6-sol" = "high"
"openai/gpt-6-luna" = "medium"
"openai/gpt-5.6-sol" = "low"
"openai/gpt-5.6-terra" = "high"
"openai/gpt-5.6-luna" = "medium"
"anthropic/claude-opus-5-5" = "medium"
"anthropic/claude-sonnet-5-5" = "high"
"anthropic/claude-fable-5-1" = "high"
```

Project settings use the same TOML shape at `.kiki/config.toml` in the exact current working directory; parent directories are not read:

```toml
model = "anthropic/claude-opus-5-5"

[thinking]
"anthropic/claude-opus-5-5" = "medium"
"openai/gpt-6-astra" = "low"
"openai/gpt-5.6-sol" = "low"
"openai/gpt-5.6-terra" = "high"
"openai/gpt-5.6-luna" = "medium"
```

`cross_provider_reasoning` is `"inline"` (default) or `"drop"`: when switching providers between runs, Kiki reconstructs foreign history from its stored semantic entries, including reasoning summaries as assistant text with `inline`, or omitting those summaries with `drop`. Same-provider history keeps its provider replay. `[anthropic] cache_ttl` is `"5m"` (default) or `"1h"`; Ordinary Anthropic requests place two cache breakpoints, on the system block and the latest message; summaries have none. Both settings use user/project precedence, are read at startup, and have no environment or session override.

`favourites` is an optional, non-empty, ordered list of unique supported model IDs. User and project lists use the same precedence as other file settings; a project list replaces the user list. Available favourites lead the model picker in their written order, followed by the full available catalogue under `OpenAI` and `Anthropic` headings; a favourite also appears in its provider group. Models without their provider's key are hidden from every picker group. A typed `/model <id>` for a keyless provider instead reports an error and retains the current model. When favourites are set, only favourites may be used by subagents, and the first is the default for every new subagent; a missing key for a requested or default subagent model causes a tool error. The foreground model remains independent of favourites. Without the setting, the picker uses provider groups in catalogue order and subagents retain the invoking model by default and may use any supported model whose provider has a key.

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

Children have fresh model context and a model/thinking selection fixed at admission. Without an override, they use the first configured favourite; if no favourites are configured, they use the invoking run's selection. An explicit model must be a favourite when favourites are configured and uses its accepted per-model thinking setting. Later foreground `/model` or `/thinking` changes do not alter admitted children.

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

`store: false` disables retained OpenAI Response objects, but does not prevent user requests or workspace content printed by Python from being sent to the selected provider. Ordinary Anthropic step requests use the configured cache TTL, not OpenAI's `store` setting.

Session storage is local and unencrypted. It includes prompts, outputs, provider replay, Arena contents, and potentially sensitive project data. On Unix, Kiki restricts session directories to `0700` and session files to `0600`; this is not a portable permission guarantee. Configured API credentials are not saved as session configuration or shown in Kiki's display, but credentials included in a prompt, output, or Arena value are ordinary session content. Local persistence does not protect against disk loss.

Returned token usage has a local direct-API estimate, not a provider bill. OpenAI `default`-tier rates are snapshot `2026-09-30` for `gpt-6.1-sol`, version `2026-09-04` for `gpt-6-astra`, snapshot `2026-09-23` for `gpt-6-sol` and `gpt-6-luna`, and version `2026-08-27` for `gpt-5.6-sol`, `gpt-5.6-terra`, and `gpt-5.6-luna`. OpenAI pricing uses short-context rates through 272,000 input tokens and long-context rates above that boundary. Per million tokens, GPT-6.1 Sol costs $2 input, $10 output, $0.10 cache read, or $2.50 cache write in short context; long context doubles input and cache rates and multiplies output by 1.5, per [OpenAI pricing](https://developers.openai.com/api/docs/pricing). Anthropic `standard` rates for Opus and Fable use the `2026-09-23` identity from [Anthropic pricing](https://docs.anthropic.com/en/docs/about-claude/pricing); Sonnet uses snapshot `2026-09-30` from the [current pricing page](https://platform.claude.com/docs/en/about-claude/pricing). None has a long-context price band. Per million tokens, Opus 5.5 costs $4 input, $20 output, $0.20 cache read, $5 five-minute cache write, or $8 one-hour cache write; Fable 5.1 costs $10 input, $50 output, $0.25 cache read, $12.50 five-minute cache write, or $20 one-hour cache write; Sonnet 5.5 costs $2 input, $10 output, $0.20 cache read, $2.50 five-minute cache write, or $4 one-hour cache write. Opaque or retried attempts can be billed but unreported, so a displayed cost may be a lower bound or unavailable. Batch and Flex (50% of OpenAI Standard), Fast (200%), and separately billed computer/browser action surcharges are not selected, controlled, or estimated by Kiki; token estimates are not a complete provider bill.

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
- Model and thinking changes from `/model`, `/thinking`, `Shift-Tab`, or the picker are saved as soon as they apply. A change made during a run applies from the next run; the active run keeps the selection it started with.
- When a run's model or thinking differs from the previous run on the branch, a muted `Model selected: <model> · <thinking>` card sits above its request. There is no card for the first run, or when either run was saved without its selection. When the current selection, including a `Shift-Tab` preview, differs from the last run's, the same card sits above the prompt, reading `[pending] Model selected: …` until the change applies, and settles into history when the next run starts.
- `/model` and `/thinking` open their selectors; exact arguments select immediately. `/name <name>` gives this session a name, shown in the footer and terminal title. `/compact [instructions]` manually compacts the selected context, optionally focusing its summary. `/tree` opens the session-tree picker; `Ctrl-R` and settled double-`Esc` open it without clearing a draft.
- `Ctrl-O` toggles retained arena cards and subagent history prompts and terminal results between automatic collapse and expanded display; it does not expand the sticky subagent list.
- `Up` / `Down` edit the configured prompt. In read-only configuration-only and runtime-disabled views, they retain history scrolling.
- `PageUp` / `PageDown` scroll history ten lines; manual scrolling anchors its numeric document top. New history follows only at the newest page; reaching it or `Ctrl-End` resumes following. `Ctrl-Home` jumps to the oldest history. The mouse wheel scrolls history three lines per event.
- Drag the primary mouse button over display history to select its rendered text in either direction. Resting the drag at the viewport's top or bottom edge scrolls it one row at a time; release copies a non-empty selection to the local native desktop clipboard.
- Copied rendered logical text preserves hard line breaks, removes terminal soft wraps, card decoration, and hidden content, and separates touched cards with one blank line. It is rendered text, not source Markdown.
- After a successful copy, the selection remains highlighted, flashes off and on twice, and the header shows `Copied`. A local clipboard failure is nonfatal: the selection remains highlighted and the header shows `Copy failed`.
- A completed selection survives ordinary history scrolling, but a new primary-button press, display-history content change, `Ctrl-O` expansion toggle, prompt-driven viewport change, or terminal resize clears it. Clipboard copy is local-desktop only; SSH, tmux, and OSC 52 forwarding are not supported.

Missing or invalid startup configuration opens a read-only configuration-only view, where prompt editing and submission are disabled. A selected-provider authentication failure or failed arena checkpoint recovery makes the runtime prompt read-only until Kiki restarts. `Ctrl-T` and `Ctrl-O` affect applicable retained transcript content only, not submission availability.

## Transcript presentation

Assistant responses and retained thinking stream and settle as GitHub-Flavoured Markdown (GFM), including tables, task lists, links, blockquotes, strikethrough, and fenced code. Raw HTML is shown literally and images use textual fallbacks. Fenced Markdown code is not Tree-sitter highlighted.

User requests appear in neutral cards without a `you:` label. RunId and ModelStepId remain authoritative but are not displayed; live display identifies current activity instead. Retained thinking is shown by default in a subtly grey, italic, borderless card with two-cell horizontal and one-row top-and-bottom padding; `Ctrl-T` hides every thinking block behind a `thinking hidden · Ctrl-T to show` placeholder. Thinking has no lifecycle colour. Every arena uses a full-width rounded outer frame with its existing lifecycle background: blue while in progress, green when complete, and red on error or cancellation. Ordinary settled frames have no heading; live and error frames keep concise activity, status, or diagnostics exactly once inside the frame. Each available arena section is a separately closed, muted-border box in order: `code`, `tool calls`, then `stdout`, non-empty `stderr`, `result` or `error`, interruption details, `failure`, `warning`, and current `stdout preview`. Sections with no underlying content are omitted. After an execution settles, `tool calls` retains one path-free row for every valid host call: a bounded count, fixed mutation statuses `created`, `replaced`, `applied`, `failed`, or `pending`. It is authoritative completion evidence, not lossy live progress or model context; it stores no paths, arguments, error text, or user text. Reported fields across a step's attempts aggregate into one muted, right-aligned, unboxed accounting line inside the outer frame after a blank separator; multiple attempts may be identified compactly. A final assistant response appears below its arena cards as a neutral page-background, borderless block with one-row top-and-bottom padding and no side padding or label. Execution content is ordered stdout, non-empty stderr, result or Python error, interruption details, then any cancellation warning.

Automatic collapse applies only when source exceeds 32 logical lines or combined execution-content lines exceeds 16 logical lines. It keeps the latest 32 source lines and latest 16 combined execution-content lines. Each affected code or execution section owns its skipped-line count in its top border as `<label> · N earlier line(s) omitted`; a non-empty execution section wholly consumed by the shared output budget remains as a closed titled box. Section labels and borders, live activity, failure or cancellation diagnostics, display-gap, and accounting do not consume those budgets. `tool calls` has its own automatic preview: it shows the latest 16 rows, preceded by `[N earlier tool calls omitted]` when needed. The tool-call trace is deliberately unbounded per execution; this display collapse is its only bound. `Ctrl-O` expands every retained source, tool-call, and execution row together or restores automatic collapse; it cannot recover source or execution content outside their existing retention bounds. Tree-sitter highlights exact partial and settled Python source. If highlighting fails, Kiki shows the exact source as plain text.

The configured prompt is a grapheme-aware editor with a visible terminal cursor. It is bottom anchored between full-width top and always-present bottom rules, wraps at display-cell boundaries while preserving explicit newlines, grows upward to roughly one-third of terminal height with a three-row prompt-box minimum where dimensions permit, then follows its tail or keeps a moved caret visible. The one-row footer keeps the current cwd and optional session name on the left, abbreviating home as `~`. On the right it shows reported `↑` input, `↓` output, `R` cached input, `W` cache-write input, `CH` latest cache-hit percentage, local estimated cost, latest settled-request context occupancy, and `model • thinking level`. Counts use compact lowercase `k` and `m` suffixes. Context is `<percent>/<capacity>`, yellow from 80.0% through 89.9% and red from 90.0%; OpenAI uses `272k` through the long-context pricing threshold and `1.05m` above it; Anthropic uses `1m`. Missing values are omitted. On narrow terminals Kiki front-truncates cwd, then removes metrics in display order while preserving model and thinking last. Complete and lower-bound costs deliberately use the same ordinary, unmarked `$…`; unknown or unpriceable cost is omitted, never invented as zero.

## Manual interactive transcript demonstration

Start Kiki with live credentials:

```sh
ANTHROPIC_API_KEY=... \
KIKI_MODEL=anthropic/claude-opus-5-5 \
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
cargo build
env -u OPENAI_API_KEY -u ANTHROPIC_API_KEY MONTY_BIN="${CARGO_TARGET_DIR:-$PWD/target}/debug/kiki" cargo test --all-targets -- --test-threads=1
cargo clippy --all-targets -- -D warnings
test "$(rg -l 'async_openai' src --glob '*.rs')" = "src/openai.rs"
test "$(rg -l 'monty_(pool|types)' src --glob '*.rs')" = "src/arena.rs"
test "$(rg -l '(cap_std|blake3)' src --glob '*.rs')" = "src/tools.rs"
```

## Opt-in live smoke test

The ignored live scenarios require a key for the selected provider and a worker; the `just` recipes point `MONTY_BIN` at the freshly built `target/debug/kiki`. `just` loads the ignored root `.env` if used; the Anthropic recipe explicitly selects Opus 5.5:

```sh
OPENAI_API_KEY=... just live-test
ANTHROPIC_API_KEY=... just live-test-anthropic
```

Each runs three requests on one controller. The complete scenario has a five-minute watchdog; every provider attempt has a two-minute deadline; if the first request starts a ninth model step, the test cancels it and fails. The cancellation request must start an attempt within 30 seconds and settle within another 30 seconds. The follow-up request must finish within two minutes. Cleanup always requests shutdown, waits up to ten seconds, then aborts and awaits the controller if necessary.
