# Research: Session-tree merge impact on compaction

## Scope

This document compares the compaction planning baseline `25e29b4` with current `main` at `45583c0`, after the interactive session-tree work was merged. It traces the merged changes through Agent, Session persistence and restoration, Runtime operation sequencing, subagent visibility, transcript projection, and the TUI session tree. It then maps those changes to the compaction specification and its three phase plans.

The document records current mechanisms and planning impact. It does not revise the compaction plans, choose the final tree-row vocabulary, or design unrelated compaction behaviour.

## Summary

- `work/compaction` now points at `45583c0`. The update was a 54-commit fast-forward from `25e29b4`; `cargo check --all-targets` passes.
- Phase 1 remains necessary: `Agent` still owns the live mutable root `ModelHistory`. The merge added arbitrary branch restoration, so Session-owned context installation must now cover startup and successful live navigation, not only linear resume and Run commits ([Agent ownership](../../src/agent.rs#L537-L604), [navigation installation](../../src/runtime.rs#L1092-L1195)).
- A Compaction Entry is now part of the interactive session tree. It must be understood by the persisted codec, selected-branch restoration, off-branch accounting, session catalogue, Runtime record mapping, transcript, tree rendering, tree search, conversation mode, and `Ctrl-X` copy. An unknown off-branch entry currently makes the restored session non-executable ([off-branch sweep](../../src/session.rs#L2175-L2192)).
- The merged Runtime and TUI are still Run-shaped. Manual compaction, automatic preflight, and post-final compaction can be active without the existing `RunId` lifecycle, so the foreground-operation distinction in the plans now affects Runtime busy events, TUI settled checks, Escape routing, tree availability, and submission rejection ([active Run](../../src/runtime.rs#L354-L366), [transcript Run state](../../src/transcript.rs#L64-L69), [TUI input state](../../src/tui.rs#L457-L613)).
- Session-tree navigation establishes useful constraints: branch changes occur only while foreground and child work are settled; selected ancestry controls context, checkpoint, and subagent visibility; all terminal child records remain retained. Compaction can preserve those rules without a branch table or checkpoint mutation ([navigation guard](../../src/runtime.rs#L1054-L1079), [selected origins](../../src/subagents.rs#L2495-L2538)).
- The SQLite schema, provider-neutral model contract, settings implementation, and tool-effect representation did not change between the two commits. Existing compaction work in those areas is not displaced by the merge.
- The implementation plans remain directionally valid, but Phase 1 Tasks 1–2, Phase 2 Tasks 5–8, and Phase 3 Tasks 2 and 4 have material new integration surface. Their baseline references and several named tests are stale.

## Findings

### Branch update and changed surface

The branch was fast-forwarded from `25e29b4` to merge commit `45583c0`. The changes relevant to compaction are concentrated in:

- `src/agent.rs`: persisted Run provenance, restoration support, and branch-scoped child invocation;
- `src/session.rs` and `src/session/codec.rs`: arbitrary-point projection, active-leaf selection, complete-tree catalogue, off-branch accounting, and retained tasks;
- `src/runtime.rs`: tree catalogue/navigation, restoration compensation, early-cancellation correction, and selected-origin updates;
- `src/subagents.rs`: complete terminal-task retention and selected-branch access fencing;
- `src/transcript.rs`, `src/tui.rs`, and new `src/tui/tree.rs`: replaceable display projections and the interactive tree.

The persisted schema itself is unchanged. It still stores an unconstrained entry `kind`, parent, Run provenance, and checkpoint reference ([schema](../../src/session/schema.sql#L22-L54)). Entries are immutable, and triggers guard Run/Model Step settlement ([immutability](../../src/session/schema.sql#L105-L117), [settlement guards](../../src/session/schema.sql#L128-L179)).

### Model Context still has two authorities

`Agent` still stores `model_context: ModelHistory`, constructs it on fresh or restored startup, reads it for every provider call, and mutates it after durable writes ([definition](../../src/agent.rs#L537-L565), [provider read](../../src/agent.rs#L634-L708)). The Session module independently reconstructs a `ModelHistory` from selected durable ancestry in `restored_history` ([projection](../../src/session.rs#L2754-L2835)).

The merge adds a second live installation path beyond startup:

1. Session projects an arbitrary requested entry and normalises a selected request to its parent ([target projection](../../src/session.rs#L1870-L1893)).
2. Runtime validates the projected history through the active model.
3. Runtime restores the target Arena checkpoint, retaining the previous checkpoint for compensation.
4. Runtime durably selects the effective Active Leaf.
5. Runtime replaces selected subagent origins.
6. Runtime installs the projected history into `Agent` with `replace_idle_state` ([navigation](../../src/runtime.rs#L1092-L1195)).

Startup follows the same validation boundary: Runtime validates history and restores Arena before constructing a restored Agent ([startup restore](../../src/runtime.rs#L701-L784)). Session cannot perform either provider validation or Arena restoration itself.

#### Phase 1 impact

The ownership goal in the compaction spec remains current, but the Phase 1 installation seam is broader than the baseline plan records:

- Session-owned context needs an uninstalled or prior snapshot while Runtime validates a startup/navigation target.
- Successful navigation needs to install the Session snapshot after target Arena restoration and Active Leaf persistence, replacing the current `Agent::replace_idle_state` call.
- Failed history validation, target Arena restore, or Active Leaf persistence must leave the previous live Session snapshot installed, matching the existing compensation order.
- The existing `agent::tests::restored_state_replaces_arena_and_context_atomically` exercises the Agent-owned seam which Phase 1 removes; current Runtime navigation tests are the cross-layer replacement seam.

The current Phase 1 plan names fresh and restored Runtime paths but does not name arbitrary-point navigation or `select_active_entry` ([Phase 1 plan](../2026-09-20_compaction/phase-1_plan.md)).

### Selected ancestry is now the operative history boundary

The merge turns “current history” into a selected branch projection. `restore_projection_at` builds one ancestry from the effective Active Leaf, validates it, derives readable records, reconstructs Model Context, selects visible task origins, and moves all other attempt accounting into `footer_adjustment` ([projection](../../src/session.rs#L2056-L2377)). Selecting a stored user request projects its parent and returns the request for editor prefill; every other valid entry projects through itself ([target normalisation](../../src/session.rs#L1870-L1893)).

This matches the compaction spec's branch-local design, but makes these consequences observable:

- Tail selection, previous-compaction lookup, cumulative files, and usage baselines operate only on selected ancestry.
- Selecting an Applied Compaction Entry in `/tree` projects through that entry, so its summary and retained tail become the immediate Model Context.
- Selecting a Failed or Cancelled Compaction Entry also projects through the entry as the durable Active Leaf, while Model Context ignores its payload and remains equal to its parent projection.
- Submitting after any selected Compaction Entry creates a new child branch from that exact entry because request admission always uses `session.active_entry_id` ([request commit](../../src/session.rs#L1129-L1164)).
- A Compaction Entry adds a new descendant revision. The tree orders sibling subtrees by latest descendant revision, so compacting an older branch can move that subtree ahead of a sibling ([tree ordering](../../src/tui/tree.rs#L263-L279)).

The compaction spec already says sibling branches project their own ancestry, but does not spell out direct selection of Applied, Failed, and Cancelled Compaction Entries ([spec](../2026-09-20_compaction/spec.md#preparation-and-durable-outcomes)).

### A new entry kind crosses every tree projection

The session-tree merge made entry handling exhaustive in more layers than existed at the planning baseline.

| Layer | Current exhaustive seam | Compaction consequence |
| --- | --- | --- |
| Stored payload | [`DecodedEntryPayload` and `DecodedEntryKind`](../../src/session/codec.rs#L80-L103) | All three compaction outcomes need one typed codec vocabulary. |
| Decode | [`decode_entry`](../../src/session/codec.rs#L427-L470) | Unknown `kind = "compaction"` is invalid. |
| Attempts | [`decoded_attempts`](../../src/session/codec.rs#L515-L531) | Summary attempts must be available on selected and abandoned branches. |
| Catalogue | [`SessionCatalogueKind`](../../src/session.rs#L239-L255), [`session_catalogue_entry`](../../src/session.rs#L2009-L2046) | `/tree` loads every entry, so one unknown compaction entry fails catalogue loading as a unit. |
| Selected records | [`readable_record`](../../src/session.rs#L2603-L2738) | Compaction is readable but does not settle a Model Step. |
| Model Context | [`restored_history`](../../src/session.rs#L2754-L2835) | Applied replaces the older prefix; Failed/Cancelled contribute no context. |
| Off-branch accounting | [`footer_adjustment` sweep](../../src/session.rs#L2175-L2192) | The current wildcard marks the whole restore non-executable for an off-branch compaction. |
| Runtime record | [`Record`](../../src/runtime.rs#L68-L108), [`restored_stored_record`](../../src/runtime.rs#L620-L667) | Live and restored cards need one typed representation. |
| Transcript | [`append_record_attempts`](../../src/transcript.rs#L3410-L3420), record projection in `record_blocks` | Summary attempts count in totals but not as the latest normal context measurement. |
| Tree | [`append_kind_row`](../../src/tui/tree.rs#L1031-L1055), [`row_text`](../../src/tui/tree.rs#L1151-L1164), [`copy_text`](../../src/tui/tree.rs#L1197-L1231) | Compaction needs semantic rows, searchable text, and copy content. |

The generic SQL schema can store this entry without adding a table or column. The code's typed projections cannot accept it without coordinated changes.

#### Phase 2 impact

Phase 2 Task 5 currently covers outcome codec, restoration, context projection, and accounting. The merged tree makes catalogue projection and direct branch selection part of that persistence task. Phase 2 Task 6 currently covers transcript cards only; the tree adds row rendering, search text, copy, semantic colour, conversation-mode eligibility, and latest-descendant ordering tests.

The merged tree UX plan explicitly reserves a distinct semantic colour for future compaction rows ([tree UX acceptance](../2026-09-15_session-trees/tree-ux_plan.md#acceptance-criteria)).

### Tree inspection differs from transcript inspection

The compaction spec defines collapsed transcript cards expanded by global `Ctrl-O`. In ordinary TUI state, `Ctrl-O` toggles transcript arena/card expansion ([key handling](../../src/tui.rs#L489-L509), [transcript expansion](../../src/transcript.rs#L2741-L2764)).

When the session tree is open, all keys are routed to `TreeNavigator` first ([input precedence](../../src/tui.rs#L457-L461)). The tree has single-line rows and uses `Ctrl-X` to copy the selected catalogue representation ([tree hints](../../src/tui/tree.rs#L36-L45), [copy dispatch](../../src/tui/tree.rs#L758-L770)). Existing User, Kiki, and Python catalogue variants retain rich copy data, while Cancelled and Failed variants copy only their label ([catalogue kinds](../../src/session.rs#L239-L255), [copy formatting](../../src/tui/tree.rs#L1197-L1231)). `Ctrl-O` therefore does not expand a compaction row inside the tree, and a compaction catalogue variant must retain whichever fields its copy representation exposes.

Current tree behaviour leaves three compaction details unspecified by the plans:

- the exact single-line row vocabulary and which stored fields enter search text;
- whether all outcomes or only Applied outcomes appear in `Ctrl-U` conversation mode, whose current rule excludes only executions ([conversation projection](../../src/tui/tree.rs#L102-L130));
- the exact `Ctrl-X` copy payload. Copying the full Applied summary would also copy its persisted cumulative file paths to the local clipboard.

These are tree behaviours, not changes to transcript `Ctrl-O` expansion.

### Foreground control is still Run-shaped

Runtime currently has one `Option<ActiveRun<M>>`. `ActiveRun` may temporarily have no `RunId` during request admission, but its eventual lifecycle is still a Run. Later submission text is discarded; Runtime counts those attempts and defers their busy-rejection events until the request commit reveals the Run ID ([state](../../src/runtime.rs#L335-L366), [submission handling](../../src/runtime.rs#L998-L1021), [commit handling](../../src/runtime.rs#L1620-L1651)). `SubmissionRejectedBusy` requires an `active_run_id` ([event](../../src/runtime.rs#L242-L266)).

Transcript has only `Idle`, `Submitting`, and `Active(RunId)` ([state](../../src/transcript.rs#L64-L69)). TUI settled checks, Escape cancellation, Enter submission, and double-Escape tree opening are based on that Run state plus navigation/correction/subagent flags ([input](../../src/tui.rs#L511-L613)). Runtime tree catalogue and navigation guards only know `self.active` and child operation count ([guards](../../src/runtime.rs#L1054-L1079)).

A compaction can occupy the foreground in four shapes which are not all Runs:

- idle or queued manual compaction;
- automatic preflight before request/Run admission;
- automatic inter-step compaction inside an open Run;
- post-final compaction after the terminal record but before `RunSettled`.

The merged state machines make the Phase 2 “Run/compaction foreground-operation distinction” observable outside Runtime. Without an explicit operation state, an idle manual/preflight compaction can appear idle to TUI submission and tree controls, and existing busy rejection cannot identify it without inventing a Run ID.

#### Cancellation and correction interaction

The merge adds early-cancellation correction. The first cancellation request fixes `correction_requested`; after a Cancelled terminal is appended, Runtime may restore the request's parent before emitting `RunSettled` ([cancellation latch](../../src/runtime.rs#L1578-L1595), [settlement and correction](../../src/runtime.rs#L1721-L1812)).

Manual `/compact` during an active Run has a different sequence in the compaction spec: retain one compaction request, cancel without correction, settle the Run, and compact the resulting leaf. The plans do not yet state the outcome when correction intent was already latched before `/compact` arrives. That interaction did not exist at baseline.

#### Phase 2 impact

Phase 2 Tasks 7–8 now cross these additional seams:

- Runtime foreground operation identity and busy rejection without a Run ID;
- Transcript/App foreground status so Enter, Escape, double Escape, and `/tree` see compaction as active;
- independent compaction cancellation state rather than the correction latch;
- shutdown and storage-disable cleanup of a pending manual compaction;
- `/compact` rejection while navigation or correction restoration is pending.

### Automatic boundaries now have concrete insertion points

The merge does not change the three approved threshold boundaries, but it changes their controller context.

#### Pre-request

Runtime currently starts the worker immediately and the worker's first durable action is `commit_request` ([start](../../src/runtime.rs#L1355-L1575), [admission](../../src/agent.rs#L634-L648)). Automatic preflight therefore has to retain the raw request outside an admitted Run. Cancellation, shutdown, or storage disablement can end this operation without a `RunId` or `RunSettled`; subsequent Submit/Compact commands need a non-Run busy result.

#### Inter-step

After execution persistence, Agent publishes `ExecutionCommitted`, clears the pending step, and loops to allocate the next Model Step ([execution settlement](../../src/agent.rs#L850-L901)). This remains the in-Run threshold seam. It must leave Runtime's root caller and selected subagent origins active because the Run has not settled.

#### Post-final

Current Runtime appends the terminal record, settles root subagent provenance, performs optional correction, emits `RunSettled`, and clears `active` in one worker-finished branch ([settlement](../../src/runtime.rs#L1721-L1812)). Post-final compaction requires a continuation between terminal publication and `RunSettled`. A queued manual compaction, shutdown, and the existing correction branch now share that continuation point.

#### Overflow

Current Agent closes the pending Model Step and Run immediately on provider failure ([failure path](../../src/agent.rs#L705-L756)). Overflow recovery instead leaves the same Model Step pending while an Overflow Compaction Entry is appended and the normal call is retried once.

The persistence schema permits an ordinary entry under an open Run. Interrupted-operation recovery is already kind-agnostic: it appends one Failed terminal beneath the current Active Leaf, settles every pending Model Step onto it, and closes the Run ([recovery](../../src/session.rs#L1418-L1504)). `valid_run_membership` likewise accepts non-terminal entry kinds as long as they carry valid Run provenance; it enumerates only the allowed terminal-entry kinds ([Run validation](../../src/session.rs#L2565-L2601)). The new implementation surface is compaction codec/catalogue/context projection, plus a regression proving the legal sequence “pending normal step → Applied Overflow Compaction Entry → process interruption” retains the compaction, closes once, and never repeats provider work.

#### Phase 3 impact

Phase 3 Task 2 already names preflight and post-final operation composition, but the merged code makes non-Run busy events, TUI foreground state, correction/manual precedence, and tree guards explicit acceptance surface. Task 4's restart tests need the Compaction Entry to remain catalogue-readable while created beneath an open Run with a pending Model Step.

### Checkpoint and subagent continuity fit the compaction design

Every non-execution entry currently reuses its parent's checkpoint; execution entries alone may introduce a new checkpoint ([checkpoint validation](../../src/session.rs#L2492-L2563)). A Compaction Entry can therefore preserve the exact Arena state without a schema change or Runtime restore.

The merge now retains all terminal tasks in the coordinator while selected origin Model Steps control root access ([startup retention](../../src/runtime.rs#L718-L734), [access check](../../src/subagents.rs#L3653-L3671)). Selecting another branch replaces the origin set only after all child operations settle ([replacement](../../src/subagents.rs#L2495-L2512)).

Compaction neither creates a Model Step nor changes selected ancestry before its parent. As a result:

- tasks originating before compaction on the selected branch remain visible and accessible;
- tasks on sibling branches remain retained but hidden;
- compaction is not a task origin and does not enter `persisted_model_steps`;
- manual compaction can allow background subagents to continue, as specified, but cannot reuse the stricter navigation guard which requires `operation_count() == 0`.

These are preserved behaviours rather than new compaction features.

### Accounting gained an off-branch dimension

Session restoration now keeps usage totals session-wide across branches. Attempts from unselected root entries and hidden tasks are accumulated into `footer_adjustment` ([root entries](../../src/session.rs#L2175-L2192), [hidden tasks](../../src/session.rs#L2300-L2348)). Transcript adds that adjustment to total attempts while choosing its latest foreground attempt separately for context/cache display ([footer](../../src/transcript.rs#L2913-L2931)).

Compaction summary attempts therefore have two distinct projection roles:

- selected or unselected summary attempts contribute once to total token/cost accounting;
- no summary attempt becomes the latest normal context-usage measurement.

Phase 2 already states the second rule, but the merged off-branch sweep adds a failure mode and corresponding branch-navigation tests.

## Plan impact by phase

| Plan area | Status after merge | Concrete impact |
| --- | --- | --- |
| Spec: canonical ownership | Expanded | Covers startup, live Run commits, and arbitrary successful navigation installation after Runtime validation. |
| Phase 1 Task 1 | Expanded | Session projection state needs a validated installation seam as well as commit-time publication. |
| Phase 1 Task 2 | Expanded | Runtime startup and `restore_selected_entry` stop installing history into Agent; navigation compensation preserves the prior Session snapshot. |
| Phase 1 Task 3 | Unchanged in intent | Agent-owned root history still exists and must be removed; child operation-local history remains isolated. |
| Phase 2 Tasks 1–4 | Mostly unchanged | Settings, summary provider operation, file evidence, and deterministic preparation were not changed by the merge; preparation now reads selected ancestry from the merged projection. |
| Phase 2 Task 5 | Materially expanded | Adds catalogue decoding, off-branch attempts, direct Compaction Entry selection/branching, no-Model-Step readable records, and branch restore tests. |
| Phase 2 Task 6 | Materially expanded | Adds `src/tui/tree.rs`: semantic row, latest-descendant ordering, search, conversation mode, and `Ctrl-X` copy, alongside transcript `Ctrl-O`. |
| Phase 2 Task 7 | Materially expanded | Foreground operation and busy protocol span Runtime, Transcript, App, tree availability, correction, shutdown, and storage disablement. |
| Phase 2 Task 8 | Expanded | `/compact` completion and dispatch must honour disabled state plus pending navigation/correction, not only ordinary slash parsing. |
| Phase 3 Task 2 | Materially expanded | Preflight needs a no-Run result/busy path; post-final needs a settlement continuation coordinated with manual work and current Runtime ordering. |
| Phase 3 Task 4 | Expanded | Overflow's pending-step sequence must survive catalogue, restoration, interruption closure, tree navigation, and branch accounting. |

All three plan files still identify `25e29b4` as their source baseline ([Phase 1](../2026-09-20_compaction/phase-1_plan.md#source), [Phase 2](../2026-09-20_compaction/phase-2_plan.md#source), [Phase 3](../2026-09-20_compaction/phase-3_plan.md#source)). Their source line references now point at pre-merge locations.

## Unchanged areas and separately exposed assumptions

The diff from `25e29b4` to `45583c0` does not modify `src/model.rs`, `src/config/settings.rs`, `src/tools.rs`, `src/session/schema.sql`, or `src/main.rs`. The following facts therefore remain as they were when the compaction plans were written:

- provider-neutral summarisation and overflow classification are still absent from the `Model` contract;
- `[compaction]` settings still need a resolved value carried from file-layer resolution into Runtime;
- successful tool effects retain normalised `PathBuf` values, while the spec currently says “exact tool-argument paths” ([effect](../../src/tools.rs#L853-L900), [normalisation](../../src/tools.rs#L2295-L2315));
- the session format remains version 1 even though a future compaction payload introduces a new entry-kind vocabulary ([format constant](../../src/session.rs#L30), [schema version](../../src/session/schema.sql#L1)).

These are not regressions introduced by the session-tree merge. Revalidating the plans against current code makes them visible alongside the merge-specific changes.

## Conventions & notable decisions

- Session tree selection changes only the Active Leaf; no entry is rewritten or deleted. The next request creates a child from the selected point ([selection write](../../src/session.rs#L1083-L1125)).
- Runtime validates model history and restores Arena before publishing a new live projection. SQLite and Arena are deliberately not wrapped in one transaction; compensation restores the prior checkpoint when a later selection write fails ([navigation](../../src/runtime.rs#L1092-L1195)).
- Tree catalogue loading decodes every durable root entry, including abandoned branches. Compatibility with only the active branch is insufficient ([catalogue](../../src/session.rs#L1965-L2046)).
- Tree search text and clipboard content are separate from width-dependent rendering. New entry kinds require all three representations rather than deriving complete copy text from a truncated row ([tree row/search/copy](../../src/tui/tree.rs#L1031-L1231)).
- Child histories remain outside root Session Entries. Root Model Context only receives child information which Python explicitly retrieves, so compaction preparation must continue to ignore task execution history ([selected task projection](../../src/session.rs#L2300-L2348)).

## Key references

- [`src/agent.rs:537`](../../src/agent.rs#L537) — current Agent-owned root Model Context.
- [`src/session.rs:1870`](../../src/session.rs#L1870) — arbitrary-entry target and user-request parent projection.
- [`src/session.rs:2056`](../../src/session.rs#L2056) — complete selected-branch restoration projection.
- [`src/session.rs:2175`](../../src/session.rs#L2175) — off-branch attempt accounting and unknown-kind failure.
- [`src/session.rs:2603`](../../src/session.rs#L2603) — readable selected-entry projection.
- [`src/session.rs:2754`](../../src/session.rs#L2754) — restored Model Context reconstruction.
- [`src/runtime.rs:998`](../../src/runtime.rs#L998) — current request admission and Run-shaped busy protocol.
- [`src/runtime.rs:1092`](../../src/runtime.rs#L1092) — branch navigation validation, compensation, persistence, and installation order.
- [`src/runtime.rs:1721`](../../src/runtime.rs#L1721) — terminal publication, correction, and Run settlement ordering.
- [`src/subagents.rs:2495`](../../src/subagents.rs#L2495) — selected-origin replacement and settled-operation fence.
- [`src/tui/tree.rs:239`](../../src/tui/tree.rs#L239) — tree catalogue projection and latest-descendant ordering.
- [`src/tui/tree.rs:1031`](../../src/tui/tree.rs#L1031) — exhaustive semantic tree-row rendering.
- [Model context compaction spec](../2026-09-20_compaction/spec.md) — intended compaction semantics.
- [Compaction Phase 1 plan](../2026-09-20_compaction/phase-1_plan.md) — Session Model ownership migration.
- [Compaction Phase 2 plan](../2026-09-20_compaction/phase-2_plan.md) — manual compaction and continuity.
- [Compaction Phase 3 plan](../2026-09-20_compaction/phase-3_plan.md) — automatic thresholds and overflow recovery.
- [Session-tree Phase 2 plan](../2026-09-15_session-trees/phase-2_plan.md) — merged branch navigation and correction behaviour.
- [Session-tree UX plan](../2026-09-15_session-trees/tree-ux_plan.md) — merged tree ordering, modes, semantic rows, and copy behaviour.
