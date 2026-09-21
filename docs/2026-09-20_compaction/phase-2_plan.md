# Plan: Model context compaction — Phase 2: Manual compaction and continuity

## Source

- [Model context compaction spec](spec.md), Phase 2: Manual compaction and continuity.
- Requires completed Phase 1 and its canonical Session Model context projection.
- Current implementation baseline: `45583c0`, including interactive session trees; re-check line references after Phase 1.
- [Session-tree merge impact analysis](../research/2026-09-21_session-tree-compaction-impact.md).
- This phase delivers `/compact [instructions]`; it does not add automatic threshold checks, `enabled`, or overflow recovery.
- Follow [the worktree instructions](../../worktrees/README.md).

## Acceptance criteria

1. User/project TOML accepts positive `reserve_tokens` and `keep_recent_tokens` with defaults 16,384 and 20,000. Unknown fields, including future `enabled`, remain strict errors in this phase. Runtime receives one immutable resolved policy without an environment, session, picker, or `Ctrl-S` override.
2. `/compact [instructions]` below any threshold prepares and generates Pi-compatible initial, repeated, and split-Run summaries with the approved `Arena State` extension and manual focus routing.
3. Applied compaction projects the canonical wrapper plus exact retained ordinary entries, survives restart/navigation, reuses the exact prepared Arena Checkpoint, and leaves immutable pre-compaction Session and Display History intact.
4. Successful observed root `read`, `write`, and `edit` operations persist normalised workspace-relative paths through restart. Cumulative modified paths dominate reads; the new effect vectors are not separately exposed through ordinary tool cards, execution feedback, tree rows/search, or execution-tree copy.
5. While the process remains alive through settlement, provider-started outcomes persist their outcome-specific fields: Applied has summary, first-retained identity, measurement, reason, and attempts; Failed has bounded diagnostic, reason, and attempts; Cancelled has reason and attempts. Only Applied changes Model Context; durably settled selected and abandoned attempts count exactly once. Process loss has no in-progress compaction journal.
6. Applied, Failed, and Cancelled Compaction Entries are valid `/tree` points and branch parents. They appear in full and conversation modes with semantic rows/search/copy; selecting Failed/Cancelled preserves the parent context, while selecting Applied restores its summary and retained tail.
7. Phase 2 creates format-version-2 sessions and rejects version 1 directly. It adds no migration or compatibility path.
8. Manual compaction during a Run cancels and settles that Run, then compacts the resulting Active Leaf with a fresh token. If correction was already pending, one retained compaction waits for correction and uses whichever leaf remains selected, including the terminal leaf when Final/Failed wins cancellation. No general queue is added.
9. Esc follows the specified pre-persistence linearisation rule; late Esc cannot rewrite a chosen outcome. No removable prefix performs no provider call/write and reports a clear notice. Invalid summary, provider failure, expected-leaf mismatch, and persistence failure follow the specified paths.
10. Transcript cards are collapsed by default and use global `Ctrl-O`. `/tree` uses one-line rows and `Ctrl-X`; tree mode does not reinterpret `Ctrl-O`. Status reads `compacting context`, every Compact dispatch receives one immediate acknowledgement, rejected drafts survive byte-for-byte, and compaction blocks Submit, duplicate Compact, and tree navigation even without a Run ID.
11. No threshold or overflow path invokes compaction in this phase. Existing normal Runs and subagents behave unchanged when `/compact` is unused.

## Overall validation

Use real temporary file-backed SQLite sessions for outcome transactions, selected/off-branch projection, exact checkpoint reuse, direct tree selection, restart, corruption, format rejection, and accounting. Use real Monty and temporary workspaces for successful, Python-error, and cancellation-time file evidence. Use scripted models for orchestration and the existing local OpenAI fixture server for summary wire requests, streaming, retries, cancellation, validation, and usage. Render tree/transcript cases with Ratatui's test backend. No live-provider tests are required.

Before tests which execute Python:

```sh
just monty
export MONTY_BIN=/tmp/kiki-monty/bin/monty
```

Every task runs:

```sh
cargo fmt --all -- --check
cargo check --all-targets
```

Whole-phase gates:

```sh
cargo fmt --all -- --check
git diff --check main...HEAD
cargo check --all-targets
cargo clippy --all-targets -- -D warnings
just test
```

Each task is one atomic signed-off commit. Affected unit tests live in the binary target, so focused commands use `cargo test --bin kiki`; the whole-phase gate also covers integration targets.

## Planning context

- Phase 1 makes Arc-backed Session state the only root Model Context owner. Runtime retains its Session handle and gives foreground workers a clone (`src/session.rs:315`, `src/runtime.rs:1355`). Compaction extends the same selected projection.
- Static model instructions are Markdown resources by architectural policy (`docs/VISION.md:128`). Existing prompt loading is centralised in `src/prompt.rs:10`; copied Pi prompts remain outside Rust.
- Model selection supplies context-window and maximum-output limits (`src/config/models.rs:27`). Summary generation captures the active `ModelSelection`; there is no second model authority.
- Normal OpenAI requests combine the system prompt, opaque replay, tools, selection, streaming, and usage (`src/openai.rs:161`, `src/openai.rs:259`, `src/openai.rs:616`). Summary calls reuse transport/retry/accounting but have a distinct no-tools request.
- `ToolResult` retains private, success-confirmed `ToolEffect` paths normalised by tool parsing. Existing source/commentary presentation can itself contain user-written lexical paths; the privacy boundary is that the new normalised effect vectors are not separately added to `ToolCall` summaries, execution feedback, ordinary cards, search, or execution copy (`src/tools.rs:183`, `src/tools.rs:853`, `src/tools.rs:2295`, `src/arena.rs:134`).
- Cancellation has priority over a ready tool result in the Arena driver (`src/arena.rs:1822`). Persist only effects the driver observes and settles; no drain or filesystem audit is added.
- Entry payloads are typed JSON and checkpoints are separate immutable rows (`src/session/codec.rs:64`, `src/session/schema.sql:16`, `src/session/schema.sql:22`). Phase 2 changes the stored payload vocabulary and therefore advances `user_version`/`FORMAT_VERSION` from 1 to 2 without compatibility code (`src/session/schema.sql:1`, `src/session.rs:30`).
- Restoration decodes every selected entry and every off-branch accounting entry; catalogue loading decodes every durable entry (`src/session.rs:1965`, `src/session.rs:2056`, `src/session.rs:2175`). A new Compaction kind must cross codec, readable records, context, accounting, catalogue, Runtime, transcript, and tree in one compiling task.
- Every non-root entry requires Run provenance, while compaction is not a Model Step (`src/session/schema.sql:22`, `src/session.rs:2565`). Reuse the prepared parent's Run and checkpoint; do not create a synthetic request-less Run or Step.
- Tree rows derive semantic rendering, complete search text, conversation topology, latest-descendant order, and copy independently (`src/tui/tree.rs:102`, `src/tui/tree.rs:263`, `src/tui/tree.rs:1031`, `src/tui/tree.rs:1151`, `src/tui/tree.rs:1197`). Compaction participates in all five.
- Runtime's active foreground state and busy event are Run-shaped, while Transcript/App settled logic keys off Run state (`src/runtime.rs:335`, `src/runtime.rs:242`, `src/transcript.rs:64`, `src/tui.rs:606`). Manual compaction needs one explicit non-Run foreground state shared across controller and interface.
- Early-Escape correction restores after terminal publication but before `RunSettled` (`src/runtime.rs:1721`). A pending manual compaction survives that sequence and starts only after correction and Run settlement complete.

## Tasks

### Task 1: Add manual policy and prompt resources

**Changes**

Add file-based `[compaction]` settings for `reserve_tokens` and `keep_recent_tokens` with approved defaults, positive checked values, and field-wise user/project precedence. Carry the immutable resolved policy on the existing startup/settings context consumed by Runtime. Do not accept `enabled` until Phase 3 and do not add environment, session, picker, or `Ctrl-S` overrides.

Copy Pi 0.85.1's system, initial, update, and split-prefix compaction prompts into Markdown resources. The immutable source is npm package `@earendil-works/pi-coding-agent@0.85.1`, tarball `https://registry.npmjs.org/@earendil-works/pi-coding-agent/-/pi-coding-agent-0.85.1.tgz`, integrity `sha512-FGRN+OHbWaefBPGaTggAdLjrIHW+s2PzLyglz/5dfLzb9of7uuXMXYC0fJIeZTw+shS32o2cuQ9jF7YSDuL/oQ==`; the constants are in `dist/core/compaction/utils.js` and `dist/core/compaction/compaction.js`. Do not use the mutable globally installed package. Add only the approved `Arena State` output section to initial, update, and split-prefix formats. Expose all resources through the existing prompt module as logical prompt text with exactly one repository file-final LF removed and no other trimming; this keeps request bytes independent of editor newline convention.

**Tests** (`src/config/settings.rs`, resolution/carrier; `src/prompt.rs`, resource loading)

First red: `config::settings::tests::compaction_defaults_and_project_fields_override_user_fields`.

- Cover defaults, partial precedence, unknown keys including `enabled`, wrong types, zero, negative, integer overflow, and the Runtime-facing resolved accessor.
- Existing settings precedence, persisted ModelSelection, and saved defaults remain unchanged.
- Assert all four resources load and the three summary output formats contain the exact Arena section once; substantial prose remains out of Rust.

**Files**

- Create: `prompts/compaction-system.md`, `prompts/compaction-initial.md`, `prompts/compaction-update.md`, `prompts/compaction-split-run.md`.
- Modify: `src/config/settings.rs`, `src/prompt.rs`.

**Verification**

```sh
cargo test --bin kiki config::settings::tests
cargo test --bin kiki prompt::tests
```

### Task 2: Add provider-neutral summary generation

**Changes**

Add summary request/result types and a provider-neutral Compaction Summary Model Context entry which owns the canonical wrapper text. Extend the model contract only with standalone summarisation; keep normal code/message action generation unchanged. Give the trait method a default which returns `UnsupportedSummarisation` with no provider attempts, so unrelated test/fake model implementations keep compiling; OpenAI and `ScriptedModel` override it. Put the captured `ModelSelection` and computed output limit in the request. Extend scripted models with deterministic summary responses and captured requests.

Implement OpenAI summary generation through the Responses API with captured model/thinking, output cap, summary system/user prompts, streaming, cancellation, existing transport retries, usage accounting, `store = false`, and no tools. Accept exactly one complete text result containing at least one non-whitespace Unicode scalar and preserve its bytes unchanged. Reject function calls, incomplete/output-limit responses, malformed content, and empty/whitespace-only output. Encode canonical wrapped summaries in normal OpenAI context without duplicating decoded assistant text or weakening retained replay validation.

**Tests** (`src/model.rs`, contract; `src/openai.rs`, local fixture-server protocol)

First red: `openai::tests::summary_request_has_no_tools_and_uses_captured_selection`.

- Assert instructions/input, active wire model/thinking, output cap, service tier, no tool schema/choice, streaming, and no storage.
- Cover text streaming, retries, usage available/unavailable, cancellation, authentication/provider failure, empty text, function calls, incomplete response, and output-limit truncation.
- Verify ScriptedModel records normal and summary requests independently.
- Verify normal requests encode the canonical wrapper exactly once as one synthetic user-role input item immediately before retained ordinary replay and still reject malformed replay without HTTP.

**Files**

- Modify: `src/model.rs`, `src/openai.rs`.

**Verification**

```sh
cargo test --bin kiki model::tests
cargo test --bin kiki openai::tests
```

### Task 3: Persist observed normalised file operations in format v2

**Changes**

Advance new session databases and the accepted format constant directly to version 2; reject version 1 as unsupported with no upgrade path.

Classify successful observed settled effects immediately into private per-execution `read_paths` and `modified_paths`, with both `write` and `edit` entering modified. Persist each category as a sorted, deduplicated vector of normalised workspace-relative paths; do not persist lexical argument spellings, event order, duplicates, fingerprints, or file content. Carry already-settled sets into Agent execution evidence for ordinary Python errors and cancellations; do not drain in-flight results after cancellation.

Round-trip the sets through foreground execution, terminal evidence, and child-task history. Do not add the new normalised effect vectors to `ToolCall`, model-facing execution feedback, ordinary transcript cards, tree rows/search, or session-tree execution copy. Existing Python source and commentary remain unchanged and may independently contain lexical paths written by the model or user.

**Tests** (`src/tools.rs`, effect extraction; `src/arena.rs`, real execution/interruption; `src/session/codec.rs` and `src/session.rs`, v2 codec/restart; `src/transcript.rs` and `src/tui/tree.rs`, privacy)

First red: `arena::tests::execution_retains_normalised_file_operations_without_exposing_paths_in_tool_calls`.

- Classify successful read versus write/edit, normalise equivalent spellings, sort/deduplicate each per-execution set, and retain a read in a batch which also mutates that path.
- Exclude failed, pending, and cancellation-won unobserved results.
- Ordinary Python errors retain committed Python state and observed operations; cancellation restores prior Python state but its evidence retains already-settled operations.
- Round-trip paths in completed, terminal, and child records under format v2.
- Version-1 databases fail preflight as unsupported; new databases report version 2.
- Use a sentinel normalised effect path which does not occur in source, commentary, or feedback; collapsed/expanded ordinary execution cards, model feedback, tree rows, search, and `Ctrl-X` execution copy never expose that sentinel. Existing lexical source text is unchanged.

**Files**

- Modify: `src/tools.rs`, `src/arena.rs`, `src/agent.rs`, `src/model.rs`, `src/openai.rs`, `src/runtime.rs`, `src/session.rs`, `src/session/codec.rs`, `src/session/schema.sql`, `src/subagents.rs`, `src/transcript.rs`, `src/tui.rs`, `src/tui/tree.rs`.

Most non-Arena changes update existing execution/evidence fixtures or exhaustive construction sites; they do not expose paths in those layers.

**Verification**

```sh
for filter in tools::tests arena::tests session:: transcript::tests tui::tree::tests; do
  cargo test --bin kiki "$filter" -- --test-threads=1 || exit
done
```

### Task 4: Prepare compaction deterministically

**Changes**

Add a pure compaction module which accepts typed selected-ancestry facts and returns either a complete manual preparation or no removable prefix. Add only narrow read accessors needed on provider-neutral Model Context entries; keep canonical wrapper construction in `src/model.rs`. Implement the spec's usage baseline, byte estimator, whole-entry retained-tail selection, previous-summary boundary, bounded repeated split-Run rule, initial/update/split prompts, 80%/50% output budgets, execution-result truncation, custom focus routing, and exact retained entry identity. Usage candidates must be ordinary durable descendants of the latest Applied Compaction Entry; retained ancestors are ineligible despite their later position in projected Model Context and enter only full byte fallback.

Derive cumulative file sets from selected immutable root execution metadata through the newly summarised range. Modified paths remove the same path from reads. Deduplicate/sort and append only non-empty Pi tags to the final stored summary; do not create a second cumulative-list authority.

Treat `<read-files>` and `<modified-files>` as reserved generated blocks. Remove well-formed prior blocks before sending previous summaries or accepting generated text; reject nested, unmatched, stray, or inline reserved tag tokens. Encode each canonical normalised path as a one-line JSON string, additionally escaping literal `<` and `>` as `\u003c` and `\u003e`. Sort before encoding.

Serialise entry fields to byte-exact labelled blocks with no whitespace normalisation, omitting only zero-length reasoning/commentary blocks and joining emitted blocks with `\n\n` with no outer newline:

- `UserRequest`: `[User]: {text}`.
- `CompletedCode`: optional `[Assistant reasoning]: {reasoning_summary}`, then `[Assistant action]: {commentary}\n\nPython:\n{source}` when non-empty commentary exists or `[Assistant action]: Python:\n{source}` otherwise, then `[Execution result]: {feedback}` for every `CodeResult` variant.
- `FinalMessage`: optional `[Assistant reasoning]: {reasoning_summary}`, then `[Assistant action]: {text}`.
- Compaction entries emit no conversation block.

Truncate only the execution-result feedback after 2,000 Unicode scalar values and append `\n\n[... N more characters truncated]`, where `N` is the omitted scalar count. A history request user input is exactly `<conversation>\n{blocks}\n</conversation>\n\n`, then optional `<previous-summary>\n{stripped previous summary}\n</previous-summary>\n\n`, then the initial or update prompt, then optional `\n\nAdditional focus: {trimmed focus}`. A split-prefix request uses the same conversation wrapper followed directly by the split prompt and optional focus. If split-prefix generation has no new history blocks and a previous summary plus manual focus, issue an empty-conversation update request with `<previous-summary>` and focus before the split request. Without focus, reuse the stripped previous summary unchanged; without a previous summary, use literal `No prior history.`. Combine split output with `\n\n---\n\n**Turn Context (split turn):**\n\n`.

Compute budgets with saturating integer multiplication, floor division by 100, a one-token minimum, and selected-model maximum cap. This module performs no model call, SQLite write, Arena action, Runtime event, or rendering.

**Tests** (`src/compaction.rs`, table-driven pure unit tests)

First red: `compaction::tests::repeated_compaction_retains_exact_tail_from_previous_boundary`.

- Baselines: newest eligible durable descendant normal attempt, retained-ancestor exclusion after Applied, total versus input+output fallback, cache/reasoning detail exclusion, untracked-retry eligibility, terminal-only exclusion, zero/missing/overflow full-projection fallback, and same-entry feedback-only addition.
- Estimator: every Model Context variant, canonical wrapper, opaque replay bytes, UTF-8, aggregate round-up, saturation, and no decoded/replay double count.
- Tail: exact target, one indivisible over-target entry, no valid cut, initial/repeated boundaries, ignored Compaction Entries, and previous-summary-only no-progress.
- Split Runs: initial split, request boundary, repeated same-Run split, prefix-only input, no overlap/gap, and literal separator. A focused prefix-only repeat with a previous summary issues an empty-conversation update then a focused split request; the same case without focus reuses the previous summary and issues only the split request.
- Protocol: exact labels/order, previous-summary wrapping, Unicode-safe truncation/count, focused repeated no-progress, and malformed reserved-tag rejection.
- Files: retained-tail delay, repeated selected ancestry, stale/duplicate/conflicting generated blocks, modified-dominates-read, child exclusion, canonical sorted tags, empty omission, equivalent lexical spellings, and JSON lines containing newlines, backslashes, quotes, or reserved-tag text.
- Budgets: saturating ratio arithmetic, floor order, minimum, and model cap.

**Files**

- Create: `src/compaction.rs`.
- Modify: `src/main.rs`, `src/model.rs`.

**Verification**

```sh
cargo test --bin kiki compaction::tests
cargo test --bin kiki model::tests
```

### Task 5: Commit, project, and present Compaction Entries

**Changes**

Add read-only manual preparation and guarded outcome commits to the Session Model. Preparation converts selected ancestry into planner input and captures expected leaf/checkpoint/Run. Outcome commits encode Applied, Failed, and Cancelled fields, reuse parent checkpoint/Run, advance Active Leaf atomically, and publish live Model Context only after durable success. A stale expected leaf appends nothing and enters the existing storage/invariant failure path.

Extend every exhaustive entry projection in one compiling change: codec, selected/off-branch attempts, readable records, latest-Applied Model Context, complete-tree catalogue, Runtime records, transcript, and tree. Applied projects canonical summary plus exact retained ordinary entries and later descendants; Failed/Cancelled leave context equal to their parent. Selecting any outcome in `/tree` restores through it and a later request becomes its child.

Bound every durable Failed diagnostic to the first 4,096 UTF-8 bytes, backing up to a scalar boundary and adding no truncation marker. Render transcript cards collapsed with reason/outcome and Applied measurement. `Ctrl-O` reveals exact Applied Markdown, Failed diagnostic, and accounting.

Use these exact single-line tree texts with the existing `format_token_count` decimal/SI rounding moved or exposed as a shared view helper:

- Applied: `Compaction · {reason} · applied · {measurement} tokens`
- Failed: `Compaction · {reason} · failed`
- Cancelled: `Compaction · {reason} · cancelled`

Reason labels are exactly `manual`, `automatic threshold`, and `context overflow`. Give `Compaction` a distinct semantic colour. Include all outcomes in full and conversation modes. Search canonicalises exactly the complete row text above; it does not index focus, summary, or diagnostic content. `Ctrl-X` passes only the Applied summary, bounded Failed diagnostic, or exact reason label for Cancelled through existing `normalise_terminal_text` before copying; control normalisation is the only difference from the stored payload. Compaction revisions participate in existing latest-descendant ordering.

Keep all summary attempts in session totals, including abandoned branches, while deriving footer context/cache fields from the latest normal attempt only. Keep Ratatui types and display strings outside Session.

**Tests** (`src/session/codec.rs` and `src/session.rs`, transactions/restoration/catalogue; `src/runtime.rs`, record mapping/navigation; `src/transcript.rs`, cards/accounting; `src/tui/tree.rs` and `src/tui.rs`, tree/expansion)

First red: `session::tests::applied_compaction_restores_summary_tail_checkpoint_and_catalogue`.

- Round-trip outcome-specific fields and reject forbidden combinations.
- Preserve exact checkpoint bytes/authorisations, parent Run, immutable ancestors, Active Leaf/revision order, and no Model Step allocation.
- Failed/Cancelled advance the branch but preserve parent context. Applied restores summary plus exact tail after same-process selection and fresh-process reopen.
- Select and branch from each outcome; sibling branches retain independent compaction projection, tasks, context usage, and global accounting.
- Off-branch summary attempts count once and never make restore non-executable or replace latest normal context measurement.
- Blocked/rolled-back/stale writes do not publish. Corrupt outcome/boundary/ancestry/checkpoint/catalogue data follows existing read-only failure behaviour.
- Transcript cards remain collapsed until `Ctrl-O`; expansion content and accounting match durable fields exactly.
- Exact full/conversation tree strings, reason labels, shared measurement formatting, semantic colour, row-only search fields, terminal-safe outcome copy, direct selection, folds, and latest-descendant ordering cover all three outcomes. Control characters are normalised exactly once; `Ctrl-O` remains transcript-only while tree input owns focus.
- Oversized provider and validation diagnostics truncate deterministically to 4,096 valid UTF-8 bytes without a marker and round-trip unchanged.

**Files**

- Modify: `src/compaction.rs`, `src/config/constants.rs`, `src/model.rs`, `src/openai.rs`, `src/runtime.rs`, `src/session.rs`, `src/session/codec.rs`, `src/transcript.rs`, `src/tui.rs`, `src/tui/tree.rs`.

`src/openai.rs` updates exhaustive context/Runtime-record observers; it adds no orchestration here.

**Verification**

```sh
for filter in compaction::tests session:: runtime::tests transcript::tests tui::tree::tests tui::tests; do
  cargo test --bin kiki "$filter" -- --test-threads=1 || exit
done
```

### Task 6: Orchestrate manual compaction as foreground work

**Changes**

Add Agent orchestration which requests manual preparation, invokes one or two standalone summaries, combines validated outputs and deterministic file sections, samples cancellation at the specified linearisation point, and commits exactly one outcome when a provider attempt started and the process remains alive through settlement. No-op preparation performs no provider/storage work; no in-progress compaction journal is added.

Add `RuntimeCommand::Compact`, a Run/compaction foreground-operation distinction, and non-Run busy rejection. Broaden the existing submission-busy event to carry `Option<RunId>`: ordinary Runs report `Some`, while compaction reports `None` rather than inventing an ID. Runtime retains its shared Session handle and moves/returns only Agent. When idle, start compaction directly. During a Run, retain one pending manual request and cancel the Run with a fresh compaction token reserved for later. If correction was already latched, finish any applicable correction and `RunSettled`, then start the retained compaction against the resulting current leaf; Replaced, SkippedBackground, and non-disabling Failed correction use the leaf which remains selected. If Final or Failed wins the Run-cancellation race, no correction event occurs, but the retained compaction still starts after `RunSettled` against that terminal leaf and preserves its focus. A disabled result clears pending work. Do not add a general queue.

While compaction is pending/active, reject Submit and duplicate Compact without retention. Tree catalogue/navigation also report busy. Esc before a retained compaction reaches `Started` clears its reserved token/focus, emits reliable `Settled(CancelledBeforeStart)` with no durable card, and leaves the already-cancelled Run plus any correction to finish normally; no compaction starts later. Shutdown clears pending work; shutdown during active generation cancels and awaits any provider-started durable Cancelled outcome. Settings/name commands retain existing semantics; preparation captures one accepted selection.

Emit one immediate `Accepted`, `Busy`, or `Disabled` acknowledgement for every `RuntimeCommand::Compact` before any later `Started`, `Settled`, or `NoOp` lifecycle. Send every compaction acknowledgement, lifecycle transition, and `RecordAppended` card through the existing unbounded reliable event channel only, preserving FIFO order; none may use bounded/lossy progress. A durable card precedes its terminal lifecycle event on that channel. `Started` means generation actually began, not merely that the request was retained. Runtime serialises commands and the TUI permits only one unacknowledged Compact dispatch, so no request ID or queued flag is needed. Keep summary streaming internal to the provider. Extend Transcript/App foreground state so status, Enter, Escape, double Escape, cancellation latches, storage disablement, and settled checks recognise accepted compaction without a Run ID. Preserve Agent return on every worker path.

**Tests** (`src/agent.rs`, scripted orchestration; `src/runtime.rs`, operation sequencing; `src/transcript.rs` and `src/tui.rs`, foreground state; `src/session.rs`, durable ordering)

First red: `runtime::tests::manual_compaction_persists_before_reporting_completion`.

- Initial, repeated, split-prefix, prefix-only, and focused compaction issue exactly prepared calls and project into the next normal request.
- Run cancellation settles once; manual compaction starts only after terminal publication and `RunSettled`, with a fresh token and resulting Active Leaf. Esc while it is retained emits `Settled(CancelledBeforeStart)`, creates no entry/attempt, does not disturb correction ordering, and prevents later `Started`, including Final/Failed race winners.
- When correction was already pending, compaction starts after Replaced, SkippedBackground, or non-disabling Failed correction and uses the actual selected leaf. If Final or Failed wins cancellation, it starts after settlement from that terminal leaf with unchanged focus. Disabled correction/settlement clears it.
- Every Compact command receives exactly one immediate acknowledgement. Submit while pending/active receives busy with no Run ID; duplicate Compact receives Busy and tree request receives its own busy outcome. Nothing is retained and no synthetic Run ID is created.
- Saturate/delay the progress channel while processing compaction: reliable acknowledgement, Started, durable card, and Settled/NoOp remain FIFO, no lifecycle event drops, and delayed progress cannot reactivate settled foreground state.
- Shutdown before queued start clears it; shutdown during generation awaits settlement and starts nothing else. Storage disablement returns Agent and clears pending state.
- Esc before first `AttemptStarted` writes no outcome/revision/accounting. Esc between split requests writes one Cancelled outcome with only started attempts. Pre-linearisation Esc chooses Cancelled; late Esc cannot rewrite Applied/Failed.
- Provider/validation failure chooses Failed. No removable prefix emits NoOp with no model call, entry, or accounting.
- Expected-leaf/persistence failure disables without successful card or projection publication.
- Background subagents continue; private child history/file effects do not enter preparation.
- Summary chunks never enter Runtime progress. `Started` sets `compacting context`; every terminal lifecycle clears non-Run foreground state exactly once.

**Files**

- Modify: `src/agent.rs`, `src/compaction.rs`, `src/openai.rs`, `src/runtime.rs`, `src/session.rs`, `src/transcript.rs`, `src/tui.rs`, `src/tui/subagents.rs`.

`src/openai.rs`, `src/tui.rs`, and `src/tui/subagents.rs` update exhaustive Runtime-event observers; slash dispatch follows in Task 7.

**Verification**

```sh
for filter in agent::tests runtime::tests session:: compaction::tests transcript::tests tui::tests; do
  cargo test --bin kiki "$filter" -- --test-threads=1 || exit
done
```

### Task 7: Expose `/compact` in the TUI

**Changes**

Add `/compact [instructions]` to the slash catalogue/parser and dispatch the complete trimmed remainder through `RuntimeCommand::Compact`. Preserve ordinary text and existing slash behaviour. Dispatch during an active Run so Runtime may retain one request; dispatch while compaction is pending/active receives Busy. If correction is pending, dispatch joins the same one pending compaction slot and does not replace correction.

Unlike ordinary slash handling, leave the exact `/compact` draft in the editor and freeze editor mutation until the next immediate Compact acknowledgement. Accepted clears it; Busy releases the freeze and leaves it unchanged and editable. Disabled releases the Compact-specific freeze and preserves the draft, after which ordinary read-only input rules apply. Only acknowledgement variants consume this single outstanding draft, so later Started/Settled events cannot affect it. Local read-only/navigation rejection occurs before dispatch and likewise leaves the draft untouched.

Block `/compact` in read-only/configuration-only state and while manual navigation restoration is pending. Connect Esc/notices to Task 6 lifecycle. Add no picker, expansion key, settings control, automatic trigger, or second Runtime/card implementation.

**Tests** (`src/tui/slash.rs`, catalogue/parser; `src/tui.rs`, dispatch/editor/correction/cancellation)

First red: `tui::slash::tests::compact_preserves_optional_focus_text`.

- Parse bare, whitespace, and focused commands; completion/help stays in sync.
- Dispatch once without turning focus into a user request; Accepted clears the frozen draft, while Busy/Disabled and local rejection retain it byte-for-byte. Busy restores ordinary editing; Disabled preserves it under ordinary read-only rules.
- Dispatch while a Run or correction is active; Runtime owns the one pending request. Reject while navigation, read-only state, or compaction work prevents it.
- Esc cancels accepted pending compaction before `Started`, or the active token while `compacting context`; existing `Ctrl-O`, `/tree`, model/thinking/name, submission, and correction behaviour remains unchanged.

**Files**

- Modify: `src/tui/slash.rs`, `src/tui.rs`.

**Verification**

```sh
cargo test --bin kiki tui::slash::tests
cargo test --bin kiki tui::tests
```

### Task 8: Document manual compaction

**Changes**

Document `/compact [instructions]`, Phase 2 settings/defaults, retained-tail/repeated-summary behaviour, persistent Arena continuity, evidence-based `Arena State`, normalised cumulative path disclosure, transcript/tree inspection, direct tree selection/branching, cancellation/correction queueing, immutable full history, and format-version-1 rejection. Remove compaction from README's deferred list while retaining automatic compaction and `enabled` as unavailable.

**Tests** (documentation review)

No first-red test applies. Check every command, setting, default, path/privacy statement, tree row/copy, status, and key against implemented tests.

**Files**

- Modify: `README.md`.

**Verification**

Run the whole-phase gates:

```sh
cargo fmt --all -- --check
git diff --check main...HEAD
cargo check --all-targets
cargo clippy --all-targets -- -D warnings
just test
```
