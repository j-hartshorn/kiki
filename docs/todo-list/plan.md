# Plan: TODO list

> Implementation approved by the user in autonomous mode. Worktree:
> `worktrees/todo-list`; branch: `work/todo-list`; review base: `main`.
> The first technical task proves the
> required seams; stop for source review if they cannot honor the agreed contract.
> The `planning` skill was unavailable; its review workflow is not claimed passed.

## Source and delivery contract

[TODO list spec](spec.md). Ship a pinned foreground checklist and TODO details in
the existing inline subagent display, not multiple new layouts. Each agent owns
its list; children start empty. All / Current / None filter each human-facing
list independently of context. Enabled + All defaults, between-Run availability
changes, and overflow inspection are agreed. Capture each agent's complete list
at Run start/child launch and after compaction it already supports, not every
Model Step. No automatic continuation or parent/child list merging.

## 1. Record reviewed source and establish the implementation gate

Product review is complete. The user accepted synchronous object methods,
ordinary-data snapshots, list-local integer IDs, fresh replacement IDs,
per-method atomicity/errors, 64 items/512 Unicode code points, adaptive tree-style
pinned displays, and full-list owner-selection/navigation. Replacement accepts
snapshot dictionaries but discards old IDs; reject other unexpected fields.

Present the source/plan, autonomous mode, tasks, worktree/branch, `main` review
base, validation commands, and technical stop conditions; wait for explicit
implementation approval. Keep root `main` and unrelated untracked work untouched.
Use a matching `work/todo-list` branch and `worktrees/todo-list` worktree. Carry
only the accepted TODO spec/plan into that worktree and commit this task's reviewed
documentation baseline before technical changes. No red/green runtime test is
applicable to this documentation-only task; verify saved content and consistency.

Do not reopen settled choices or add live migration/cancellation, completion
timers, async host tools solely for local mutation, or automatic continuation.

## 2. Prove the Arena state, availability, and context seams

Write focused tests/proof code around bootstrap/checking declarations, commit,
restoration, child construction, progress/terminal records, Model Context, and
provider replay before the full implementation. Demonstrate:

- Validated synchronous methods mutate one Arena-owned object; snapshots are
  independent transferable copies. No second live host-owned authority.
- Projection capture detects object mutation without requiring top-level rebinding,
  another Model Step, or changes to user bindings/checkpoint bookkeeping.
- Ordinary exceptions preserve the state actually committed under current rules;
  cancelled/discarded execution leaks no uncommitted display/context.
- Child construction starts with an empty child list while preserving unrelated
  inherited Arena state and leaving the parent/other children unchanged.
- Each Run's captured capability policy governs retained object references after
  restore, branching, and preference changes. New children capture the current
  preference rather than blindly inheriting their parent's older policy. Read-only
  children can edit local TODO state without new external-effect permissions.
- Live child projections can reach the UI with owner, branch, revision/generation
  checks; terminal projections can be retained durably without restarting children.
- Context capture works per agent, with explicit empty state and no child-private
  state automatically injected into parent context.
- Captured context survives later steps, respects newer object updates, and can
  be replaced at required triggers with correct cross-provider replay.
- Existing checkpoint/checking/session compatibility stays enforced, including
  explicit handling of sessions created before this capability existed.

Likely files: `src/tools.rs`, `src/arena.rs`, `src/prompt.rs`, `src/agent.rs`,
`src/model.rs`, `src/compaction.rs`, `src/subagents.rs`, `src/runtime.rs`,
`src/session.rs`, and session codec/schema as required by proven retention needs.
Inspect actual interfaces and choose the narrowest implementation. Do not build
a general plugin framework. If the proof conflicts with the agreed contract,
revise/review the spec before implementation rather than silently moving state.

## 3. Implement the Python object and committed projections

Use red-green tests for the agreed object methods, item identity, validated
atomic mutations, explicit clear, independent snapshots, capability guidance,
checking declarations, and enable gating. Keep display filters out of object
policy. Publish projections after committed execution, restoration, selected
branch changes, and Arena replacement without relying on changed-binding reports.

Cover invalid/oversized inputs at the exact 64-item/512-code-point boundaries,
whitespace-only text, unexpected fields, ignored snapshot IDs on replacement,
unchanged allocators after failure, stale identifiers, replacement semantics,
direct return-copy mutation, no-op updates, exceptions, cancellation, and independent
resumed/branched states through the same interface model code uses. Add adapter
tests only for distinct bootstrap/transport/checkpoint behavior.

Deliverable: correct local state and capability behavior independent of the UI.

## 4. Wire independent child lists, progress, and terminal retention

Initialize a fresh TODO list in every child (including nested children), without
sharing or implicitly inheriting the parent's list. Preserve other copied Arena
bindings and current launch/permission behavior. Collect committed child
projections and propagate them through the existing coordinator/runtime path,
without parent refresh/wait calls or new model steps. Do not expose cross-agent
mutation through the model-facing object.

Retain each child's final committed projection in branch-scoped task history,
including cancellation/failure, and restore it for inspection after the child's
Arena is released. Do not auto-complete items or resume terminal work. Keep live
projections as derived views and terminal snapshots as immutable history, not a
second live source of truth.

Test parent/sibling/nested isolation, no unintended initial parent-list copy,
read-only children, background progress, event reordering/revisions, branch
navigation, stale lifecycle updates, terminal records, restart, and selected-list
inspection after the sticky task row has naturally disappeared. Child UI updates
must not silently inject private lists into parent Model Context.

Deliverable: correct per-agent state/progress without child checklist rendering.

## 5. Implement per-agent initial and post-compaction context

Capture each agent's full committed list before its first normal Model Step
(foreground Run start or child launch). Include text, statuses, and order in all
UI modes. Empty lists are explicit. Label capture time and treat task text as
data, not harness policy. Subsequent accepted object updates supersede the older
snapshot.

Refresh after installed manual, threshold, or overflow-recovery compaction before
the next normal step, where those paths already exist for that agent. Do not add
child compaction solely for TODOs. Coalesce initial capture/first-step compaction.
Failed/no-op compaction, ordinary retries, and individual steps do not recapture.
Keep the captured state logically available without accumulating durable reminder
messages. Replacing/disabling generated projections does not rewrite historical
execution records.

Test actual requests: full state despite Current/None, no per-step recapture,
newer mutations winning, empty versus stale summary state, isolated agent/branch
context, all existing compaction triggers, retries, context-budget accounting,
and OpenAI/Anthropic/Google replay and switching. Unfinished items must never
schedule automatic continuation or extra Settlement work.

Deliverable: the context policy works without checklist rendering.

## 6. Add configuration and settings navigation

Extend existing field-wise settings resolution and comment-preserving user writes
for `[todos]`. Add TODO list, Enabled, and Display to existing settings navigation.
Test agreed defaults, partial user/project tables, validation, masked/failed saves,
return navigation, retained mode while off, immediate display changes, and the
agreed effective/pending availability rules across root and child Runs. Cover an
enabled background child retaining access after the preference becomes off, new
foreground work and newly admitted children starting disabled, the converse
transition, and restoration not overriding the captured policy. Idle/terminal
inspection follows the current preference. Merely moving picker focus must not
change availability. Menu indicators identify mixed active values; object access,
guidance, context capture, and owner-specific displays use the effective policy.
Display-mode changes remain immediate across owners. No startup wizard, child
cancellation, or live-toggle migration machinery is required.

## 7. Render foreground and inline child checklists with inspection

Use a narrow display module for list filtering/formatting and integrate with the
existing root/subagent height allocation. Apply the agreed Pi-style visual tree:
`● Todos (completed/total)`, `├─` / `└─` connectors, and `✓` / `◐` / `○` item markers.
Keep the underlying data flat. Recompute branches for filtered/overflow rows,
count the owner's full list, and keep the in-progress glyph static. All includes every status in order;
Current includes only explicitly in-progress items; None/disabled show no TODO
items/counts and preserve existing subagent lifecycle rows. Keep independent
owner labels and prevent child items appearing in the foreground list.

Implement bounded overflow plus the agreed explicit read-only viewer for root
and branch-accessible child lists. Support terminal child inspection, scrolling,
close behavior, preserved drafts, deliberate inspection in None, and disabled
inspection behavior. Cover empty/complete/multiple-active lists, many children,
nested children, Unicode, long labels, both themes, resizing, multiline prompts,
and very short terminals. Do not break existing monitor visibility, Ctrl-T,
Ctrl-O, text selection, or keyboard navigation.

## 8. Whole-feature verification and documentation

Required current commands: `just test`, `just lint`, and
`cargo fmt --all -- --check`. Use deterministic fake/scripted models for context
assertions; normal tests must not require live providers.

Add end-to-end coverage for object -> committed root/child state -> UI/context;
settings changes; failures/cancellation; compaction, resume, and branch navigation;
terminal child retention; and disabled stale references. Manually exercise all
modes, overflow/owner selection, active and terminal children, nested progress,
short terminals, and prompt draft retention.

Update README capability/settings/UI/context documentation. Review correctness
and simplification across the full diff. Do not add general inspectors, arbitrary
cross-agent writers, extra layout modes, or future task-dependency abstractions.
Implementation follows the matching branch/worktree workflow. Initial approval
has been granted; complete the approved tasks with verification and task-sized
commits. Do not merge without final user approval. Stop for any technical proof
that cannot honor the agreed source contract.

## Implementation record

- Task 1: user approved autonomous implementation; matching worktree/branch
  created from `main` at `13cb3ed`. Reviewed baseline contains only the accepted
  TODO spec and plan. Independent saved-document review found no contradictions;
  orchestrator checked the diff and exact source copy. Runtime red/green is not
  applicable to this documentation-only task. `git diff --check` passed.
- Tool availability: Pi TODO capability and user-only reviewer prompt resources
  are unavailable in this session. Task tracking uses persistent Arena state;
  reviews use explicit bounded read-only briefs, not a claimed review-skill run.

- Task 2, Monty subset: recovered after provider transport failure; no failed
  child's code effects were present. Eight test-only checked-Monty proofs now
  pass locally. Object/checkpoint/policy/clone behavior is demonstrated; complete
  root/child runtime integration and provider request assertions are not yet
  implemented or claimed verified.
- Compatibility finding: restored pre-feature checkpoints retain old bootstrap
  and checking state even when the runtime identity matches. Production must
  explicitly choose safe handling before enabling/advertising TODOs for such
  sessions; no migration or identity change has been made by this test-only proof.
- Verification: proof suite 8 passed; `just lint`, formatting, and diff checks
  passed. Default `just test` hit two existing worker deadline failures; both
  passed in isolation. `RUST_TEST_THREADS=4 just test` passed 1789 unit tests,
  5 credentials CLI tests, and 17 session CLI tests, with 8 expected ignores.
  No unrelated timeout behavior was changed. Original passing evidence is saved
  in orchestrator Arena variables; concurrency is part of its applicability.

- Task 2 subset review: independent review PASS, no must-fix findings. Strengthened
  the nonblocking legacy characterization with a trusted executable-namespace
  assertion after restore; eight proof tests and lint/fmt passed again. Final
  `RUST_TEST_THREADS=4 just test` passed after that assertion change. This remains
  a partial feasibility proof, not completed production context/UI integration.
- Paused before production bootstrap/identity changes pending a compatibility
  decision: follow existing stub-version protection (older sessions remain
  history-readable but not executable), or require an explicit tested migration
  preserving executable older sessions. No migration, identity bump, source-contract
  change, or production TODO feature has been made. Task 2 remains in progress;
  reviewed proof changes are intentionally uncommitted until its gate is resolved.

- Compatibility resolution: user selected existing checkpoint-contract policy.
  Production TODO bootstrap/stubs advance `type-check-v3` to `type-check-v4`;
  older saved sessions retain readable history but cannot resume execution.
  No migration or executable-state reconstruction is in scope.
- Task 2: bounded Monty proof accepted after independent PASS review and local
  verification. Existing execution -> child task execution -> stored task record
  paths and the model-history/provider projection paths were inspected as the
  reuse seams. No external dependency blocker was found. Feature-specific child
  publication/retention acceptance tests remain in task 4; actual generated
  context, compaction, budgeting, and provider request tests remain in task 5.
  Those are not claimed implemented or verified by the test-only fixture.

- Task 3: production `src/todos.py` + `src/todos.rs`, generated bootstrap/checking
  declarations, v4 checkpoint identity, host policy/reset/read hooks, committed
  Execution projections, and backward-readable StoredExecution fields implemented.
  The toy proof object was removed; its checked-path assertions now use production.
  Coordinator admission, settings, generated context, and UI remain later tasks.
- Task 3 interruption: implementer 7 timed out after25steps, leaving compiling code.
  Orchestrator inspected every affected module/diff and completed verification.
  Original implementer red evidence was unavailable; explicit bootstrap-absence
  fault injection reproduced missing-capability failure and was exactly restored.
- Task 3 review: fixed valid snapshot-ID/replacement-variable checking rejection
  via permissive dictionary values in those two declarations, preserving typed
  method arguments and all runtime schema/limit checks. Added checked regression:
  observed3invalid-argument-type errors red, then14focused TODO tests green.
  Independent re-review CLEAN. Wheel-scroll observation declined as unrelated
  root-main divergence; actual task-base diff has only Execution fixture fields.
- Task 3 current evidence: `RUST_TEST_THREADS=1 just test` passed1795unit +5credentials
  +17sessions CLI tests,8ignored. `just lint`, fmtcheck, diffcheck passed. Serial
  execution avoids independently observed pre-body worker-startup deadline flakes;
  no worker timing behavior changed. Root guidance/policy lifecycle remains an
  explicit task5/6 integration requirement, not a completed capability toggle.

- Task 4: admission mutex captures latest preference into immutable ChildParameters
  TodoPolicy, including new children of older-policy parents. Actual runner resets
  inherited object in place and applies captured policy before first model use;
  explicit child guidance policy matches it. Added durable policy/defaults and
  reused committed Execution snapshots through existing revision/event/history paths.
  Host hooks: coordinator.set_todos_enabled/todos_enabled (async), to be wired in6.
- Task 4 verification: authoritative child red showed parent-list inheritance and
  wrong guidance, then focused5green. Orchestrator reran5childTODO,30admission,
  25codec tests, lint/fmt/diffcheck, all passed. Related branch1/read-only16/object6
  regressions passed in implementer evidence. Independent task review CLEAN.
- Task 5 execution decomposition: one task/commit remains. A bounded implementer
  prepares typed generated-context transport and provider serialization tests;
  orchestrator then owns run/compaction capture and budget/cache integration.
  This avoids another oversized delegation and does not change source behavior.

- Task 5: typed generated TodoContext is stored only in the projected ModelHistory
  cache. Root Run start captures full list+guidance; children capture own data only
  because their admitted base guide is immutable. Later steps retain that capture;
  applied compaction refreshes after retained history; failed/no-op/retries do not.
  Root static guide is omitted so disabled Runs have no generated advertisement.
  No new durable per-step records or parent/child context merges were introduced.
- Task 5 budgeting: cache stores capture anchor; generated text is counted when
  estimated usage predates it, and not double-counted after an eligible measured
  call. Older opaque baselines can conservatively overestimate until new usage.
- Task 5 TDD: transport missing-behavior red7fail ->8green; capture timing red4fail
  ->green; budget red equal[50009,50009] ->counted. Added failed-summary, initial
  coalescing, and measured-baseline non-double-count tests. 35TODO-related and
  65Agent tests passed; older context fixtures now explicitly require the new frame.
- Task 5 verification recovery: direct long calls lost results to Arena timeout.
  User authorized detached one-shot verification. New-session Python Popen + tee,
  source snapshot, PID, and exit-code files under ignored target survive tool calls;
  nohup alone was reaped. No harness timeout change is in this branch. Aborted
  intermediate suite143 exposed test-gate assertions, which were corrected without
  changing gates/timeouts. A review worker hit memory; obsolete Arena source/output
  buffers were pruned and the narrow review succeeded.
- Task 5 review: production context review CLEAN. Runtime/provider fixture review
  preserved privacy/replay/cache/cancellation assertions; accepted its enabled-child
  coverage finding and retained actual enabled reference with expected static guide.
  No defaults were disabled or generated entries filtered from observed histories.
- Task 5 final evidence: target/todo-verification/full-context-final.1c8fjz/ has exit0,
  output.log and matching source.diff. RUST_TEST_THREADS=2 just test passed1816unit
  +5credentialsCLI+17sessionsCLI,8ignored; justlint/fmtcheck/diffcheck passed. Source
  snapshot matched before this documentation-only record was appended.

- Task 6: defaults/field-wise TOML resolution, exact all/current/none parsing,
  per-field project masks, comment-preserving user saves implemented (7cfg tests,
  144precedence combinations; 50existing settings tests passed).
- Runtime initializes coordinator preference from resolved config; new root and
  manual-compaction operations synchronously capture policy and invalidate a
  changed-policy checkout for restoration on the worker execution path. No Arena
  I/O blocks the control loop. ApplyTodoEnabled stores latest preference then
  emits revisioned acknowledgement, never alters active Agent policy.
- RunStarted now carries actual immutable policy via WorkerMessage, so a newer
  preference acknowledgement cannot mislabel a delayed root start. Startup Off
  versus On+None and live blocked-code/retained-alias tests established behavior
  red->green; delayed request-persistence test covers both initial policies.
- Settings menu adds TODO submenu with Enabled/Display and parent-return enum.
  Confirm applies/saves current choice even on mask/error; focus has no toggle
  effect. Display changes are local only. Active-owner notices use captured root
  policy and monitor operational child policy; fully-off Display is unavailable.
- Menu worker i failed memory while preparing extra tests; no report assumed.
  Parent inspected partial files, cleared obsolete Arena buffers in a separate
  execution, and completed tests/implementation. Dynamic menus needed searchable
  rows, not tree caller-prefiltered Direct rows; added Searchable+contains_value
  with existing replace_items preserving filter/focus, leaving tree semantics.
- Parent found valid RunStarted may return no visible transcript change because
  begin_submission already set starting status. Confirmed oldguard red using
  real submitting state; active_run_id query accepts current policy and rejects
  settled/obsolete facts. Live menu root/child rows now refresh without losing
  search/focus. File masks/save errors/ackordering/drafts/three modes covered.
- Task 6 review j identified stale Display chooser confirmation after lastenabled
  owner settled. Added root/child xEnter/CtrlS regression (red modeNone vsAll),
  rechecked availability before save/mutation, retained oldmode; rereview k CLEAN.
- Task 6 final evidence: target/todo-verification/full-settings-final.irRpsG exit0
  and source.diff matched before this documentation-only entry. RUST_TEST_THREADS=2
  just test passed1835unit+5credentialsCLI+17sessionsCLI,8ignored; justlint/fmtcheck/
  diffcheck passed. Actual checklist/overflow viewer is task7, not claimed here.
