# Spec: Session-owned subagents

## Context

Kiki runs one foreground agent with a persistent Python arena. Its async tools support concurrent external operations, but it cannot delegate work to another model-driven agent with independent working state and model context.

Subagents let an agent delegate focused work without copying the parent's model history. Each child receives an independent copy of the parent's pre-execution arena checkpoint and a task prompt. Background children continue across Python executions, parent replies, and subsequent user requests, allowing the parent to keep working or ask for clarification.

Delegation remains inspectable without embedding child activity inside Python-run cards. Separate subagent event cards provide retained history and expandable prompts and terminal results. A compact sticky list above the prompt shows running and completed children, including model, model-step count, runtime, and estimated cost. Session accounting includes all descendant work without double-counting.

## Goals

- Delegate to complete agents with fresh model context, independent copies of the parent's pre-execution arena, and an optional model override.
- Support background start, explicit status refresh and cancellation, and waiting for results without blocking unrelated foreground work.
- Return the child's final message and explicit `result_values` dictionary without merging its arena or automatically inserting its activity into parent model context.
- Allow background work across parent replies and subsequent requests, with bounded resource use, inherited permission restrictions, and predictable cancellation.
- Show separate, expandable subagent history cards and a compact sticky list with identity, prompt preview, model, step count, status, runtime, and cost.
- Include all root and descendant usage in session accounting exactly once, preserving uncertainty where provider usage is unavailable.

## Non-goals

- Changing, upgrading, or forking Monty; cloning state created during the currently executing Python chunk.
- Variable discovery, a variable registry, shared mutable arenas, or automatic merging of child state into the parent.
- Filesystem isolation, automatic worktrees, or rollback of external effects. Agents share the workspace.
- Modal navigation, interactive child conversations, steering running children, or resuming completed children.
- Persisting background tasks across Kiki restarts.
- Automatic delegation retries, automatic model selection, or exact billing guarantees. Existing provider-attempt retry behaviour is unchanged.
- A general job scheduler, queued admission, new provider router, or a new slash-command/settings interface.

## Key discoveries

References describe the accepted model/thinking prerequisite merged to `main` by `1d20e16` and inspected again on 2026-09-05 after this branch was rebased. Its [model-selector spec](../2026-09-05_interactive-model-thinking-settings/spec.md) and landed implementation are the implementation baseline; extend rather than duplicate that work.

- Checkpoints are cloneable, include whole-file freshness fingerprints, and update after completed arena executions. A host call during an execution still has the earlier committed checkpoint available (`src/arena.rs:53`, `src/arena.rs:540`, `src/arena.rs:597`).
- Restoring a suspended checkpoint preserves its continuation, not an independent idle environment; Kiki rejects that state as an ordinary arena restore (`src/arena.rs:664`). Monty's typed worker interface supplies no supported suspended-to-idle fork ([Monty checkout](https://github.com/pydantic/monty/blob/70fe3f5781381eb33579e45046f8cb3845953373/crates/monty-pool/src/checkout.rs#L336)).
- Monty does not honour custom handle `__await__` methods ([await handling](https://github.com/pydantic/monty/blob/70fe3f5781381eb33579e45046f8cb3845953373/crates/monty/src/bytecode/vm/async_exec.rs#L41)). A local feasibility spike verified ordinary async instance methods, refreshable cached data, and handle dump/restore: 22 assertions passed, including three expected limitations. This is evidence about interpreter mechanics, not an implemented child lifecycle; maintained integration tests are required below.
- Kiki already generates Python wrappers and record construction from neutral host values. Incoming values are limited to scalars, lists, and string-keyed dictionaries (`src/tools.rs:346`, `src/tools.rs:566`, `src/arena.rs:1172`).
- The five-minute arena deadline currently includes every host wait. Each arena owns a one-worker pool, and Kiki dispatches gathered calls at `ResolveFutures` before resuming with all batch results (`src/arena.rs:488`, `src/arena.rs:540`, `src/arena.rs:1008`).
- Tool exposure is static and `ToolHost` has no per-agent permission policy. Copied wrappers cannot determine authority (`src/tools.rs:521`, `src/tools.rs:922`, `src/tools.rs:1639`).
- The runtime owns one foreground run and stops polling workers while idle. Its records and events have no child identity (`src/runtime.rs:19`, `src/runtime.rs:83`, `src/runtime.rs:336`).
- The prerequisite provides qualified models, per-step immutable selections, layered TOML, mixed-model cost summation, and footer formatting (`src/config/models.rs:40`, `src/config/settings.rs:46`, `src/openai.rs:710`, `src/pricing.rs:143`). However, successful `ModelStep` results do not carry selection provenance, and remembered per-model thinking currently lives in TUI state (`src/model.rs:26`, `src/tui.rs:164`, `src/tui.rs:625`).
- Settled history is cached separately from the live foreground tail; footer context/cache-hit currently use the latest root attempt (`src/transcript.rs:2265`, `src/transcript.rs:2390`, `src/transcript.rs:3203`).

## Proposed approach

### Invocation and observations

All model-facing operations remain ordinary Python calls through the arena:

| Operation | Contract |
| --- | --- |
| `await start_subagent(prompt, model=None, read_only=False)` | Admit and start a child, returning a task handle without waiting for completion. |
| `await run_subagent(prompt, model=None, read_only=False)` | The same start followed by waiting; return the child's result. |
| `await get_subagent(id)` | Return a handle to an existing authorised descendant. Never start work or enumerate tasks. |
| `await task.refresh()` | Read authoritative task state, update the handle's cached observation, and return that observation. |
| `await task.wait()` | Wait for this child's terminal outcome, update the cached observation, and return its result or raise. Repeated calls do not rerun work. |
| `await task.cancel()` | Cancel unfinished work throughout the identified subtree, await cleanup, and return the identified task's final observation. Already-completed results remain unchanged. |

The Python handle is `SubagentTask`, with `id: str` and `status: SubagentStatus` plus the async methods above. `SubagentStatus` is a generated record with this exact surface:

| Attribute | Python type | Meaning |
| --- | --- | --- |
| `id` | `str` | Six-character task ID. |
| `state` | `str` | `starting`, `running`, `stopping`, `completed`, `failed`, `cancelled`, or `timed_out`. |
| `model` | `str` | Fixed qualified model ID. |
| `thinking` | `str` | Fixed catalogue-supported thinking value. |
| `step_count` | `int` | Number of model steps started. |
| `elapsed_ms` | `int` | Monotonic elapsed milliseconds from admission, frozen at settlement. |
| `observed_at_ms` | `int` | Host UTC Unix timestamp in milliseconds when this observation was read. |
| `estimated_cost_usd` | `float | None` | Convenient numeric projection of the host's estimate; `None` when unavailable. |
| `cost_status` | `str` | `complete`, `lower_bound`, or `unavailable`, using the existing accounting meanings. |
| `error` | `str | None` | Terminal failure diagnostic, or requested stop reason while stopping; otherwise `None`. Bounded to 4 KiB UTF-8. |

Generate the record definition and prompt reference from one description. Keep the existing precise monetary representation internally and format UI costs from it, not from the Python float. Start/get provide an initial observation; refresh, wait, and cancel replace the cache, including before a wait raises for a terminal child outcome. These are ordinary caller-owned Python values: mutating them never changes host state or authority. An observation does not copy the prompt, full execution records, exported values, or streamed text.

Cached data means **last observed**, not current. Refresh is authoritative at its observation point only. No automatic Python mutation, awaitable property, locally maintained `is_stale` flag, polling loop, or additional wait/result alias is introduced. Call fresh methods rather than attempting to reuse one coroutine object. Independent starts and waits compose with `asyncio.gather()`; a gather still waits for all its inputs.

Validate arguments and admission policy before starting provider or arena work. Require a non-empty string prompt within the existing 64 KiB UTF-8 foreground-prompt limit, an omitted or supported exact qualified model ID, and a boolean `read_only`. IDs use exactly `[a-z0-9]{6}`, are unique within the session, and are not reused after completion. They are references, not secrets or permission grants.

Malformed types raise `TypeError`; invalid values or unknown IDs raise `ValueError`; denied access raises `PermissionError`; exhausted admission limits raise `RuntimeError`. Child failure and cancellation raise `RuntimeError` containing the ID and outcome; child timeout raises `TimeoutError`. Operational child outcomes remain distinguishable in observations. A rejected start creates no child and consumes no admission; a failure after admission retains its ID, consumes that start, and settles as a failed task.

### Arena inheritance and results

Every child started during one Python execution inherits an independent copy of that execution's starting Arena Checkpoint. Same-execution mutations and newly created bindings are excluded. The prompt itself is evaluated at invocation time, so a loop can provide distinct instructions or indices into an already-persisted collection. A spawn-only execution is not mandatory.

Preserve Python data, helpers, classes, and aliases supported by the checkpoint format. Retain its whole-file content fingerprints, subject to normal current-file freshness validation. Create an independent child arena through the existing subprocess mechanism; do not parse checkpoint bytes, continue a copied parent stack, or replay parent code. Child reads and mutations do not alter the parent's arena or authorisation state.

After restoring, reset the child's `result_values` binding to a fresh empty dictionary before its first model step. This also applies to grandchildren. The child may fill or replace that binding. With no exports, the result values are `{}`; deleting it, replacing it with a non-dictionary, or providing unsupported values makes completion fail explicitly.

Successful completion returns a dictionary with `id`, `message` (the child's final reply), and `values` (its validated `result_values`). Collect the known binding without global discovery and without allowing host effects during collection. Accept Kiki's existing neutral scalar/list/string-keyed-dictionary values; functions, task handles, arbitrary class instances, cycles, and unsupported numeric representations are not exported. Existing arena and worker transport limits still apply. Failure while collecting, converting, or transporting the export from the child must settle it as failed, not publish partial success or disable the foreground runtime. A later failure delivering an already-published result into the parent arena affects that execution only and never rewrites the child's terminal outcome.

Capture message and values before admitting success. Terminal outcomes are immutable host-owned results; each wait returns an independent value copy, so parent mutations cannot alter later waits. Cancellation accepted before success admission wins; cancellation after success leaves that result unchanged. Retain the generated message as failure evidence if result collection fails, but never label it a successful result.

Retain the unadorned resolved base instructions as the canonical prompt resource, rather than treating the already-assembled root prompt as the base. Assemble root and child prompts separately: the child gets that base, a shipped child prompt resource, and exactly one generated effective tool reference. Preserve the root's existing assembled instructions and override provenance. Never append a restricted reference to a root prompt that already contains an unrestricted generated reference. Explain the pre-execution copy point, fresh `result_values`, explicit wait/refresh, shared files, and effective restrictions. Do not append the parent's model history, provider replay, or automatic dumps of inherited values. Returned values enter parent Python state; only deliberate printing/inspection or existing bounded execution feedback brings them into parent model context.

### Model-selector integration

The prerequisite is assumed available, not reimplemented. A child's qualified model and thinking are fixed for its lifetime:

- With no model override, inherit the exact selection used for the invoking model step, not a newer foreground selection accepted during that step.
- With an explicit model override, use that model's remembered accepted session thinking, then resolved persisted thinking, then its catalogue default. Resolve this at child admission.
- Later `/model`, `/thinking`, or Shift+Tab changes affect the foreground only. Uncommitted previews never affect children.

Carry actual step-selection provenance independently of provider usage; missing usage cannot cause fallback to a newer model. Make remembered accepted selections available to trusted spawning code without reaching into TUI state. Keep provider construction and credentials at the existing host/adapter seams; no model router or additional thinking parameter is needed.

### Ownership and permissions

A session-owned subagent module owns admission, task identity, ancestry, fixed parameters, lifecycle, results, and child records. The runtime keeps it alive independently of the foreground worker. Use an injected child runner for deterministic lifecycle tests; the production runner uses the existing Agent, Arena, and provider adapter rather than a second agent loop.

A caller may manage its descendants. The foreground agent remains the same owner across later requests; child agents cannot manage ancestors, siblings, or themselves. Every private/public operation validates authority from the calling arena's trusted context, never from an owner field supplied by Python. Copied or mutated handles do not confer authority. `get_subagent` recovers access when arena cancellation discarded a handle variable while the host-owned child survived.

Permissions only narrow. `read_only=True` denies `write`, `edit`, and unrestricted `run`; ordinary Python mutation and read tools remain available. For a read-only caller, `read_only=False` means inherit and succeeds with read-only permissions; it is not a request to widen authority and does not itself raise. Effective read-only mode is the caller's restriction OR the requested flag. Enforce actual operation denials beneath wrappers, including aliases and direct private host calls. Disabling nested delegation denies spawning from any child while leaving root delegation available. No generic configurable tool registry is required.

Normal child completion leaves descendants running, and waiting for a completed child does not implicitly join its descendants. Failure, explicit cancellation, or timeout cancels that child's unfinished descendants, not its parent or siblings. Cancelling a completed ancestor still cancels unfinished descendants while preserving the ancestor's terminal result.

Keep terminal results and child records until the session exits. Release settled workers, provider execution state, and unnecessary checkpoints promptly. Removing terminal entries from the sticky list is presentation only. Checkpoint restoration neither restarts nor rewinds tasks, and late child events remain valid even after the originating foreground run has ended.

### Cancellation and settlement

Foreground completion, failure, and cancellation do not automatically cancel background tasks. A first Escape while the foreground is running requests foreground cancellation only. Another Escape when cancellation has already been requested, or when the foreground is idle, cancels all background work. Rapid `Esc Esc` must work while foreground recovery is still settling; foreground cancellation is recorded immediately for this decision.

An open selector retains the prerequisite's Escape-to-close behaviour before runtime cancellation shortcuts apply. Background cancellation also remains available when foreground submission is disabled. Existing Ctrl+C draft-clearing behaviour remains; actual shutdown, input closure, or controller shutdown cancels and joins all owned work before returning.

Dropping a Python wait, cancelling the foreground execution, or losing a handle does not cancel the independently admitted child. Background cancel-all fences admissions from affected descendants while they are stopping, so a racing nested start cannot escape cancellation. A later foreground request may admit new work after that cancellation action.

Lifecycle observations distinguish starting, running, stopping, completed, failed, cancelled, and timed out. Stopping retains the requested terminal reason while cleanup runs. Settlement releases the concurrency slot, freezes runtime/metrics/outcome, and admits the terminal record once. A model's final reply alone is not success until export validation and the success/cancellation admission decision complete. Normal completion need not wait for surviving descendants; cancellation/failure settlement includes cleanup of the affected unfinished subtree.

Completed filesystem effects remain. Preserve existing command-group termination and late-authorisation safeguards. Child-local provider errors or failed arena recovery fail that child; they do not set the foreground runtime's disabled state.

### Limits and configuration

Extend the prerequisite's user/project TOML settings, with field-wise project overrides and existing strict diagnostics. Missing values use defaults. These settings are read at startup; there is no new environment-variable family, picker, live limit mutation, or automatic persistence.

| `[subagents]` key | Default | Meaning |
| --- | ---:| --- |
| `allow_nested` | `true` | Permit children to spawn descendants. |
| `max_concurrent` | `8` | Session-wide admitted, unsettled children, including those starting or stopping. |
| `max_starts_per_run` | `64` | Admissions charged to the originating root request, including later descendant starts. |
| `max_depth` | `4` | Maximum child depth; the root is depth 0. |
| `timeout_seconds` | `900` | Overall child lifetime allowance, including startup, model work, Python, and waits. |

Numeric settings must be positive integers representable by the corresponding counter/deadline; reject invalid types, zero, negatives, overflow, and unknown keys with bounded file/field remediation. Existing `/model` and `/thinking` saves must preserve this table.

Reject excess starts immediately rather than queueing. Reserve an admission before launching resources, and release its concurrent slot only after settlement. Refresh/wait/cancel operations do not require a child slot. Each new foreground request has a new start budget, but old descendants retain their original budget and all requests share the concurrency cap. IDs and origin/ancestry records survive as long as needed by retained tasks.

A child's effective absolute deadline is the earlier of its own admission time plus the configured allowance and any inherited ancestor deadline. This remains true if that ancestor completes normally. Nesting cannot extend a subtree deadline. Expiry requests cancellation, marks the timed-out outcome, and awaits cleanup before resource release.

Retain the existing 64 MiB arena memory limit. Each Python execution has five minutes of chargeable time. Pause its clock only while suspended solely on child completion/cleanup waits; startup/control work and any other outstanding host work remain chargeable. A mixed gather with a shell/read operation remains charged until those ordinary operations settle. Resume with the remaining budget, never a fresh five minutes. Cancellation and child deadlines continue to operate while the arena clock is paused. This timing belongs to Kiki's host driver, not Monty's cumulative interpreter duration limit.

### Records and accounting

Retain full child execution records under child identity without inserting them into root model context or interleaving them as ordinary root Python/thinking blocks. Publish separate root-display Spawned and terminal records, with immediate parent identity for nested work. Lifecycle, usage, and terminal evidence must use reliable publication; lossy progress may update previews but cannot be the only source of a counter, cost, or result.

Attribute every model step and attempt to its owning agent. Step count increments on model-step start; retries do not add model steps. Reuse existing disjoint attempt accounting so a retry or a cumulative terminal summary cannot count an already-recorded attempt twice. Observations and display cards are projections, not additional accounting entries.

Each agent's cost covers only its own attempts. Session cost and token totals include the foreground plus all descendants, including known usage from failure/cancellation. Reuse the prerequisite's per-attempt mixed-model pricing and complete/lower-bound/unavailable semantics. Missing usage is not zero. Update costs when usage is reported; do not invent continuous spend. Foreground context occupancy and cache hit remain derived only from the latest foreground request. Sticky removal or later requests never subtract earlier usage.

### History and sticky presentation

A Spawned history card contains the ID, fixed qualified model, applicable parent ID, and prompt. A terminal card contains outcome, step count, final runtime/cost, and a bounded final-message or diagnostic preview. Python tool rows use concise Spawned, Refreshed, Waiting, and cancellation summaries referencing the same ID without embedding child activity or repeating full prompts.

History prompts and terminal messages show their first three wrapped display lines by default, with an omission indicator when necessary. Omitted terminal messages append `[result omitted · Ctrl-O to expand]`, including failure diagnostics. `Ctrl-O` expands retained prompts and terminal messages completely; they wrap and scroll with ordinary history. The sticky list never expands. Apply terminal-text sanitisation and existing rendering bounds to previews; do not render exported dictionaries automatically.

Cards are immutable event-time observations. New child records join committed history in runtime publication order, even while the foreground has a live tail; they do not consume or replace that live step. A child's Spawned record always precedes its terminal record. Preserve existing settled rasters during unrelated live updates; no nested mutable child timelines are added to Python cards.

The sticky list sits above the prompt with one blank row of padding above and below when space permits. Each entry has two rows:

```text
  • subagent:a7k2m9  openai/gpt-5.6-luna  6 steps  04:47  $0.1200
    └ Review the cancellation paths and identify…
```

Use the full qualified identity matching model selection, even where the ordinary footer abbreviates the sole provider. Use `1 step` and otherwise `N steps`. Cost formatting is the prerequisite footer's format; omit unavailable cost rather than displaying zero. Runtime is elapsed monotonic time from admission through settlement, displayed as total minutes and seconds with at least two digits each; it freezes at settlement. Update elapsed display only when a visible whole second changes, not every TUI poll.

The prompt starts after the six-cell prefix `    └ ` and uses all remaining width except a six-cell right gutter. Truncate by display cells at grapheme boundaries, using `…` only when text was omitted; that ellipsis precedes the right gutter. Normalise embedded newlines/controls for this single-line preview. There is no fixed character-count truncation.

Show up to four entries, active work first and admission order within active/terminal groups. If any entries are hidden, add one row `  +N more subagents` (`subagent` for one). Use `•` for starting/running, `…` for stopping, `✓` for completed, `!` for failed/timed-out, and `×` for cancelled. After the ID add `starting`, `stopping`, `failed`, `timed out`, or `cancelled` where applicable; running and successful completion need only their indicator. All terminal entries are greyed out and retain their final measures. Clear terminal entries only on the next accepted foreground user request, not draft edits, rejected/busy submissions, selectors, slash commands, or model-setting changes. Active entries remain, and those settling afterwards remain until the following accepted request. No expiry timer exists.

On small terminals, remove padding and reduce visible entries while preserving the prompt and at least one otherwise-available history row. If only one spare row remains, use `  Subagents: A active, T finished`; if no spare row remains, omit the list for that frame. Counts include all retained visible-list tasks, not only entries that fit. Under first-row width pressure omit step count, then cost, then runtime; front-truncate the model with `…`, then omit it if needed. Clip the remaining ID/state prefix only when it alone cannot fit. Clip overflow/count rows safely too. The six-cell prompt gutters apply whenever the line can accommodate them; narrower rows never underflow their available width. Do not let sticky content enter conversation text selection; history geometry changes invalidate or preserve selection using the existing rules.

## Key decisions & tradeoffs

- Deliver one cohesive PR with an embedded plan: session ownership, safety, control, and visible background work ship together, without intermediate feature modes or a separate modal phase.
- Copy the pre-execution arena checkpoint; reject unsupported at-call cloning and selective-variable registries.
- Use session-owned tasks and explicit handles; reject tying lifetime to Python execution or a foreground final reply.
- Use ordinary async methods and known-ID lookup; reject custom awaiting, automatic Python refresh, and speculative aliases.
- Return a dedicated dictionary with the final message; reject an export tool and implicit arena/model-context merging.
- Freeze child model/thinking and reuse the model-selector prerequisite; reject global settings changes affecting existing children.
- Enforce authority and bounded admission in Rust; reject permission-by-prompt and queued admission.
- Use two-stage Escape and persistent-until-next-request sticky outcomes; reject first-press cancel-all and timed disappearance.
- Separate immutable event history from a compact live list; defer modal navigation and nested execution displays.
- Reuse accounting/formatting conventions, adding descendant ownership exactly once rather than rewriting mixed-model pricing.

## Risks / unknowns

- Inheritance timing can surprise the model. Guidance and real arena tests must demonstrate exclusion of same-execution changes; no source-analysis workaround is planned.
- Shared files change concurrently. Freshness checks protect structured operations, not unrestricted shell effects, and cancellation is not rollback.
- Late events must not revive settled tasks, corrupt recovered arenas, or be discarded merely because their originating foreground request ended.
- Wait-clock suspension must not exempt ordinary host work, reset budgets, or deadlock cancellation behind occupied child slots.
- Active limits do not cap accumulated session records/results. Release settled execution resources; retained results remain subject to existing interpreter/transport constraints and can grow with session length.
- Child selection must come from actual step provenance despite concurrent settings updates and missing usage.
- Sticky geometry, Unicode truncation, and per-second updates must preserve prompt editing, scrolling, selection safety, and settled-history caching.
- The prerequisite now owns configured two- and three-row terminal fallbacks which retain the footer while omitting prompt/history. Sticky-list layout must preserve those accepted fallbacks rather than treating their rows as available monitor space (`src/tui.rs:1564`).

## Dependencies

- [Interactive model, thinking, and session settings](../2026-09-05_interactive-model-thinking-settings/spec.md), merged to `main` by `1d20e16`. Its catalogue, selections, TOML, pricing, formatting, selectors, and footer are the implementation baseline.
- Existing Agent/Arena/tool execution and exact Monty `0.0.21` worker. No Monty dependency/protocol change.
- [TUI responsiveness](../2026-09-03_tui-responsiveness/spec.md) and [history text selection](../2026-09-02_history-text-selection/spec.md); preserve their rendering and geometry guarantees.
- [Default agent tools](../2026-09-01_default-agent-tools/spec.md) for host effects, authorisations, and Python composition; [Vision](../VISION.md#L98) for delegation intent. Whole-checkpoint inheritance intentionally widens Vision's transferable-data-only initial direction, without widening returned values.

## Plan

### Acceptance criteria

- The Invocation contract works through real Monty Python, with both start/continue and start/wait paths, known-ID recovery, repeatable results, and explicit cached observations.
- Inheritance, empty child `result_values`, fixed step-derived model/thinking, fresh model history, shared files, and export failure behaviour match the corresponding approach sections.
- Background work survives foreground Python completion, replies, cancellation, and later accepted requests. Subtree failures/cancellation, immutable settlement, restoration, and shutdown follow Ownership and Cancellation without escaping work or reviving results.
- All admission limits, settings precedence, permission restrictions, and sole-wait clock rules are enforced, including nesting and mixed host batches at saturation.
- Reliable records account for root and descendant attempts once, retain child evidence separately, and preserve foreground-only context/cache indicators under dropped progress.
- History cards and the sticky list match Presentation, including full prompt and terminal-message expansion, exact gutters, elapsed runtime, terminal retention, tiny terminals, selection/cache safety, and rapid two-stage Escape.

### Overall validation

Run from this rebased implementation worktree. Isolate user settings without hiding the installed Rust toolchain. Use this shell for the focused commands below and the full suite:

```sh
test_home="$(mktemp -d)"
export CARGO_HOME="${CARGO_HOME:-$HOME/.cargo}"
export RUSTUP_HOME="${RUSTUP_HOME:-$HOME/.rustup}"
export HOME="$test_home"
export MONTY_BIN=/tmp/kiki-monty-0.0.21/bin/monty
unset OPENAI_API_KEY KIKI_MODEL KIKI_THINKING KIKI_SYSTEM_PROMPT
trap 'rm -rf "$test_home"' EXIT
"$MONTY_BIN" --version
cargo fmt --all -- --check
cargo check --all-targets
cargo test --all-targets -- --test-threads=1
cargo clippy --all-targets -- -D warnings
test "$(rg -l 'async_openai' src --glob '*.rs')" = "src/openai.rs"
test "$(rg -l 'monty_(pool|types)' src --glob '*.rs')" = "src/arena.rs"
test "$(rg -l '(cap_std|rustix|blake3|getrandom)' src --glob '*.rs')" = "src/tools.rs"
git diff --check
```

Expect exact worker identity `monty-runtime 0.0.21`, all credential-free tests/checks passing, and no dependency-seam leakage. Tests must inject home/workspace/settings and gates rather than observe real configuration or use sleeps to establish ordering. Use deterministic host-time control for clock tests; do not wait fifteen minutes or make provider calls in the ordinary suite.

Manual acceptance, after deterministic verification: start two children using different configured models; let the foreground reply; submit a clarification while they run; inspect changing steps/runtime/cost, expand long history prompts and terminal results, and verify the sticky list stays compact. Confirm completed entries survive idle time and clear on the next accepted request. Verify Esc once stops only the foreground and Esc again stops background work, including rapid presses and picker precedence. Check shared-file warnings, readonly denial, an ordinary child failure, Unicode gutters, narrow terminal layout, and session cost retention. Live checks are opt-in, not replacements for deterministic assertions.

### Planning context

- Extend strict TOML validation and targeted-save preservation rather than creating another loader (`src/config/settings.rs:46`, `src/config/settings.rs:472`, `src/config/settings.rs:686`). Settings diagnostics use `StartupProblemKind` from `src/config.rs:40`.
- `ModelStep` lacks selection provenance; OpenAI already captures selection once at step start. `ModelUsage` is not a reliable substitute (`src/model.rs:26`, `src/openai.rs:710`). Accepted per-model thinking is currently remembered by the TUI when it sends settings (`src/tui.rs:164`, `src/tui.rs:625`), while runtime acceptance only updates the watch and acknowledges a revision (`src/runtime.rs:439`). Move the authoritative remembered values to trusted runtime state without publishing uncommitted TUI previews.
- The resolved `SettingsContext` lives on `StartupSnapshot`, while `ReadyStartup` supplies the provider's initial model/thinking (`src/config.rs:191`, `src/config.rs:313`). Preserve that split when making persisted fallback choices available to child admission.
- Agent final messages are staged and attempt evidence is admitted through runtime settlement. Reuse that admission/accounting logic for children rather than inventing a second model loop or counting cumulative terminal attempts again (`src/agent.rs:440`, `src/agent.rs:477`, `src/runtime.rs:665`).
- Keep Monty imports in `src/arena.rs`, OpenAI imports in `src/openai.rs`, and filesystem-specific imports in `src/tools.rs`/the existing command adapter. Subagent policy uses neutral values and injected execution, not foreign runtime types.
- Existing generated tool defaults/types need boolean and task-wrapper support; private names are independently reachable and must receive the same checks (`src/tools.rs:203`, `src/tools.rs:277`, `src/tools.rs:566`, `src/arena.rs:884`).
- The controller's idle branch currently waits only for commands; session-owned children need servicing there and during foreground work (`src/runtime.rs:336`).
- Extend existing inline module tests, scripted models, read/command gates, injected TUI time, and cache construction counters. Test through the same host/arena/runtime seams used by callers, not duplicate implementations inside fixtures (`src/arena.rs:2238`, `src/arena.rs:4604`, `src/runtime.rs:1297`, `src/transcript.rs:2364`, `src/transcript.rs:4248`, `src/tui.rs:2300`).

### Task 1: Resolve subagent admission settings

**Changes** — Extend the established settings path with the Limits table, defaults, strict validation, and field-wise user/project precedence. Expose resolved policy at startup; preserve `[subagents]` during existing model/thinking saves. Do not advertise delegation yet.

**Tests** (`src/config/settings.rs`, settings; `src/config.rs`, startup integration) — First red: a project subagent field overrides only the corresponding user value while other limits inherit. Cover defaults, absent files/table, every invalid type/value/unknown key/overflow, bounded remediation, configuration-only failure, startup propagation, and comment-preserving selector saves with the new table present.

**Files** — Modify: `src/config/constants.rs`, `src/config/settings.rs`, `src/config.rs`.

**Verification** — `cargo test config::settings::tests && cargo test config::tests && cargo check --all-targets` passes.

### Task 2: Preserve delegation model selections

**Changes** — Make actual successful model-step selection available to Agent execution independently of usage. Retain accepted per-model thinking choices in trusted runtime state, seeded from resolved configuration/startup selection, while preserving the prerequisite's watch and revision semantics. Provide fixed-selection child model construction through existing host/provider seams without sharing the foreground's mutable selection receiver.

**Tests** (`src/openai.rs`, adapter; `src/model.rs` and `src/agent.rs`, provenance; `src/runtime.rs`, accepted selection) — First red: settings change during a gated code-producing step, but its delegation provenance remains the original pair despite unavailable usage. Cover immutable retries, target-model remembered/persisted/default fallback, uncommitted previews excluded, grandchildren's caller selection, and independent fixed versus foreground model receivers.

**Files** — Modify: `src/model.rs`, `src/openai.rs`, `src/agent.rs`, `src/runtime.rs`, `src/tui.rs`, `src/main.rs`.

**Verification** — `cargo test model::tests && cargo test openai::tests && cargo test agent::tests -- --test-threads=1 && cargo test runtime::tests -- --test-threads=1 && cargo check --all-targets` passes.

### Task 3: Prepare isolated child arenas and exports

**Changes** — Add host-owned child arena construction from an idle checkpoint, child-local permission narrowing, hidden output-binding initialisation, and side-effect-free collection of the known output dictionary. Keep ordinary foreground arena behaviour unchanged and retain independent worker ownership. Host policy denies write/edit/run beneath wrappers; generated effective references reflect restrictions without being the enforcement mechanism.

**Tests** (`src/arena.rs`, real-worker integration; `src/tools.rs`, host permissions) — First red: clone an arena with existing data/helpers/aliases and non-empty `result_values`; the child preserves useful state but starts with empty exports and cannot mutate the parent. Cover pre-execution exclusion, separate authorisation changes, external-file freshness rejection, public/private/aliased write/edit/run denials, inherited narrowing, successful nested neutral exports, absent/non-dictionary/unsupported/cyclic/oversized exports, no host effects during collection, and failure cleanup without damaging the source arena.

**Files** — Modify: `src/arena.rs`, `src/tools.rs`.

**Verification** — `cargo test tools::tests -- --test-threads=1 && cargo test arena::tests -- --test-threads=1 && cargo check --all-targets` passes.

### Task 4: Own subagent lifecycle in the session

**Changes** — Introduce a focused subagent module owning identity, ancestry/origin, atomic admission, frozen parameters, observations, immutable results, cancellation/deadline state, and retained records. Accept an injected child runner so coordinator tests do not need Monty or a provider. Implement authorised lookup/wait/cancel, original-run budgets, descendant deadlines, slot release after cleanup, and shutdown draining. No generic scheduler or queue.

**Tests** (`src/subagents.rs`, coordinator behaviour with gated runners) — First red: an admitted task remains retrievable and completes after its initiating caller has returned. Cover ID shape/non-reuse, concurrent admission at limits, failures before/after admission, no lost wait wake-ups, repeat waits/value-copy isolation, per-origin accounting across requests, depth/nested settings, guessed/mutated/ancestor/sibling/self IDs, completed ancestors with live descendants, normal completion versus failed-subtree cleanup, ancestor deadlines, cancel-all racing nested admission, cancellation versus result admission, and resource release exactly once.

**Files** — Create: `src/subagents.rs`. Modify: `src/main.rs`.

**Verification** — `cargo test subagents::tests -- --test-threads=1 && cargo check --all-targets` passes.

### Task 5: Connect complete child agents to runtime ownership

**Changes** — Bind the coordinator to the existing Agent/Arena/provider runner. Keep the resolved unadorned prompt base canonical and assemble root/child instructions with exactly one effective generated tool reference. Give root and child executions trusted caller context and capture each invoking execution's starting checkpoint. Keep child execution separate from foreground cancellation and settings watches. Service child lifecycle/records while the foreground is active, idle, or disabled; join owned work on shutdown/channel closure. Capture and validate message/exports before success admission. Add a separate background-cancel command while retaining foreground cancel.

**Tests** (`src/subagents.rs`, real child-runner integration; `src/runtime.rs`, lifecycle; `src/agent.rs`, result admission; `src/prompt.rs`, assembly) — First red: a gated scripted child continues after a foreground final reply and completes during a later foreground request. Cover fresh child model history, preserved root override/provenance, exactly one generated reference per prompt, readonly child guidance without an unrestricted generated reference, fixed selection, exported result admission/cancellation races, child failure/recovery failure isolation, preserved model attempts, retained child records, foreground cancellation leaving children, completed-parent descendants, background cancellation, disabled-root control, and shutdown with blocked child provider/arena work.

**Files** — Create: `prompts/subagent.md`. Modify: `src/subagents.rs`, `src/runtime.rs`, `src/agent.rs`, `src/arena.rs`, `src/openai.rs`, `src/prompt.rs`, `src/config.rs`, `src/main.rs`.

**Verification** — `cargo test subagents::tests -- --test-threads=1 && cargo test runtime::tests -- --test-threads=1 && cargo test agent::tests -- --test-threads=1 && cargo test prompt::tests && cargo check --all-targets` passes.

### Task 6: Expose the Python delegation interface

**Changes** — Extend existing bootstrap/reference generation and neutral dispatch with exactly the Invocation interface. Handles carry IDs and cached observations; effective host context controls authority. Implement run as the same start/wait lifecycle, reconnect by known ID, builtin exception mapping, and ID-only tool summaries. Preserve filesystem batch effects and gather ordering; no foreign adapter leaks into tool policy.

**Tests** (`src/arena.rs`, real Python/host integration; `src/tools.rs` and `src/prompt.rs`, reference generation) — First red: Python starts a gated child, runs subsequent statements, then refreshes and waits for its dictionary result. Cover both invocation forms, get versus start, repeated fresh method awaits and gather, cache updates on terminal errors, builtin exception catches, prompt byte limit/Unicode boundaries, strict arguments, one representative wrapper-to-host permission denial (the matrix belongs to Task 3), same-execution multiple starts sharing the earlier checkpoint, restored/lost/copied handles, direct and nested recursion through real Agent code, failed siblings, parent result-delivery failure leaving child success intact, and no automatic parent-context/value printing.

**Files** — Modify: `src/tools.rs`, `src/arena.rs`, `src/subagents.rs`, `src/prompt.rs`.

**Verification** — `cargo test tools::tests -- --test-threads=1 && cargo test arena::tests -- --test-threads=1 && cargo test subagents::tests -- --test-threads=1 && cargo test prompt::tests && cargo check --all-targets` passes.

### Task 7: Charge arena time around child waits

**Changes** — Replace the indivisible arena wall deadline with the specified chargeable-time budget, preserving all existing interruption/recovery behaviour. Pause only during sole child completion/cleanup waits, resume the remaining allowance, and keep child absolute deadlines independent. Control operations must remain available at child-capacity saturation.

**Tests** (`src/arena.rs`, driver integration; `src/subagents.rs`, deadlines) — First red: a gated child wait exceeds the parent's ordinary wall allowance without consuming its chargeable budget. Cover work before/after repeated waits, running Python, ordinary reads/commands in mixed gathers, the transition when the last ordinary call settles, failed gather siblings, run-subagent startup versus waiting, cancellation while paused, child deadline during a paused parent, saturated recursive starts, refresh/cancel at capacity, timeout classification, and checkpoint recovery. Reuse command process-group cleanup tests; use injected/controlled host time and gates, not long sleeps.

**Files** — Modify: `src/arena.rs`, `src/subagents.rs`, `src/tools.rs`.

**Verification** — `cargo test arena::tests -- --test-threads=1 && cargo test subagents::tests -- --test-threads=1 && cargo test tools::tests -- --test-threads=1 && cargo check --all-targets` passes.

### Task 8: Publish authoritative child records and accounting

**Changes** — Extend runtime events with child identity, reliable lifecycle/step/attempt evidence, and immutable root-display Spawned/terminal records. Retain child execution details separately. Attribute observations to one owning agent and preserve existing disjoint accounting when records replace live data. Aggregate descendant usage through the prerequisite estimator without using lossy progress as authority or changing foreground context/cache selection.

**Tests** (`src/runtime.rs`, publication/accounting; `src/transcript.rs`, footer aggregation) — First red: child usage arriving after the foreground final reply increases session cost while foreground context/cache fields remain unchanged. Cover nested and mixed-model attempts, retries and unavailable usage, failure/cancellation, live-to-terminal replacement, duplicate/stale observations, dropped/full/undrained progress channels, original foreground IDs ending, per-agent versus subtree totals, and removal of sticky presentation without removing accounting.

**Files** — Modify: `src/subagents.rs`, `src/runtime.rs`, `src/transcript.rs`.

**Verification** — `cargo test runtime::tests -- --test-threads=1 && cargo test transcript::tests::footer -- --test-threads=1 && cargo test subagents::tests -- --test-threads=1 && cargo check --all-targets` passes. The prerequisite's pricing tests remain unchanged and pass in Overall validation.

### Task 9: Render separate subagent history cards

**Changes** — Project Spawned and terminal records into independent history cards with the specified previews, parent/child identity, measures, and prompt and terminal-message expansion. Render concise subagent tool summaries without nested traces. Preserve publication order and the existing live foreground tail, selection text, and settled-raster cache behaviour.

**Tests** (`src/transcript.rs`, rendering/projection) — First red: a Spawned card arriving during a live foreground Python execution appears separately without replacing that live execution. Cover spawn-before-terminal order, multiple children/descendants, first-three-wrapped-line prompt and terminal-message previews with full Ctrl-O expansion, narrow Unicode/control-text rendering, no exported-data dump or child execution interleaving, no duplicate costs, copying retained history text, and construction counters showing old settled cards are not rebuilt by child progress.

**Files** — Modify: `src/transcript.rs`.

**Verification** — `cargo test transcript::tests -- --test-threads=1 && cargo check --all-targets` passes.

### Task 10: Add the compact monitor and two-stage Escape

**Changes** — Add a small TUI-owned sticky-list projection over authoritative child observations, without owning tasks or creating a second accounting source. Apply the exact two-row format, qualified IDs, elapsed display, six-cell gutters, active-first overflow, completed retention, and small-terminal fallbacks. Preserve the prerequisite's configured `height <= 3` footer-only rendering; those rows do not host the monitor. Wire foreground-then-background Escape, including rapid presses and picker precedence. Reuse the existing footer numeric formatting and injected-time seam.

**Tests** (`src/tui/subagents.rs`, layout/time projection; `src/tui.rs`, input and geometry integration) — First red: where prompt rendering is available, a two-row child entry renders immediately above it with exactly six cells of left/right prompt space. Cover width-filling ellipsis, graphemes/wide characters, model namespace, step/retry counts, cost formatting/unknown usage, whole-second redraw boundaries, elapsed freeze, four-entry limit/overflow/ordering, no sticky expansion, terminal entries across idle/slash/busy/rejected actions and accepted requests, active children surviving reset, growing prompts, tiny dimensions including unchanged configured two-/three-row footer-only views, history scroll/selection geometry, foreground-disabled control, rapid Esc Esc, and selector Escape without cancellation. Prove timer ticks with no visible active runtime cause no redraw or settled-history rebuilding.

**Files** — Create: `src/tui/subagents.rs`. Modify: `src/tui.rs`, `src/transcript.rs`.

**Verification** — `cargo test tui::subagents::tests && cargo test tui::tests -- --test-threads=1 && cargo test transcript::tests -- --test-threads=1 && cargo check --all-targets` passes.

### Task 11: Verify and document the complete delegation workflow

**Changes** — Add the cross-request integration scenario and user/tool documentation tying together the approved contracts. Keep tests focused on composition not duplicating lower-level matrices. Extend README's existing model/thinking settings section with subagent configuration rather than restating prerequisite setup. Document known-ID recovery, pre-execution handoff, shared files, frozen selection, status staleness, Escape escalation, output limits/failures, and the independent meanings of history expansion and sticky retention.

**Tests** (`src/runtime.rs`, scripted end-to-end integration; `src/prompt.rs`, guidance regression) — First red: one deterministic scenario starts mixed-model children, receives a foreground reply, changes foreground model, submits a clarification, retrieves an existing child by ID, collects values, and cancels another subtree while retaining correct history and session totals. Include one readonly descendant and a controlled failure; assert no escaped workers and no child data in root model context except deliberately inspected output. Lower-level timing/rendering boundaries remain owned by Tasks 3–10.

**Files** — Modify: `src/runtime.rs`, `src/prompt.rs`, `prompts/read_only_agent.md`, `prompts/subagent.md`, `README.md`.

**Verification** — `cargo test runtime::tests -- --test-threads=1 && cargo test prompt::tests` passes, followed by every Overall validation command and the documented manual acceptance where credentials are available. Record any unrun live check explicitly; do not describe it as passed.
