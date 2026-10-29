# Spec: TODO list

> Behavior reviewed with the user: tree-style pinned checklist; All / Current /
> None; per-agent Run-start/post-compaction context; enabled + All defaults;
> launch-pinned availability; independent child lists and inline detail; Python
> object methods and ID rules; read-only overflow inspection; 64-item/512-character
> limits. Technical feasibility is not yet demonstrated. Implementation still
> approval was granted for autonomous implementation. Pre-feature executable
> checkpoints follow the agreed history-only compatibility policy below.

## Goal

Give each agent an optional TODO capability through codemode, and give the
user a clear, optional view of each agent's own list. This is progress tracking, not a
planning mode, approval gate, or restriction on other tools.

## Selected design

Use a full-width pinned checklist above the prompt, alongside the existing
bottom-area activity displays. It does not scroll away with session text or
steal prompt focus. Use existing theme roles: completed items are muted,
in-progress items accented, and pending items neutral. Status is also conveyed
by markers, never colour alone.

```text
● Todos (2/4)
├─ ✓ Define list behavior
├─ ✓ Add settings and codemode object
├─ ◐ Test persistence and branch restoration
└─ ○ Verify terminal layouts

Working…
───────────────────────────────────────────────
›
```

Use the supplied Pi-style tree treatment consistently in main, child, and
full-list displays: a `● Todos (completed/total)` header, `├─` for non-final visible
rows, and `└─` for the final visible row. Item markers are `✓` completed, `◐`
in progress, and `○` pending. The in-progress marker is static, not another
animation. Use existing theme roles and retain meaningful non-colour markers.

This is a visual tree over a flat ordered list, not nested subtasks or phases.
Recompute connectors after filtering. Header progress counts cover the owner's
full list, not only the Current-filtered subset. Overflow is an explicitly
labelled final row when room permits; it must not imply omitted work is complete.
Parenthesized descriptions can remain ordinary task text, not a new status-label
field. None suppresses the entire TODO header, counts, branches, and item rows.

### Display modes

These replace the earlier Always / During changed runs / Never proposal.
There is no separate run-based visibility setting or automatic end-of-run hide.

| Mode | Dedicated TODO display |
| --- | --- |
| All | All items, preserving list order, regardless of status. |
| Current | Only items explicitly marked `in_progress`, preserving their order. |
| None | No automatic TODO display, including counts or hidden-list badges. |

Current does not infer a current task or fall back to the next pending item.
If no items match the selected mode, the panel occupies no space. An empty list
therefore has no panel in any mode. An all-completed list stays visible in All
but not Current. Multiple in-progress items are allowed and all are eligible
for Current. Switching modes changes presentation only, never list contents.
Run completion, failure, or cancellation does not itself clear the list or hide
otherwise eligible committed items.

None suppresses the dedicated display, not ordinary execution source/results
or the agent's ability to discuss its work.

### Feature availability

Enabled is independent of display mode:

- Enabled + All/Current: TODO object usable; filtered checklist shown.
- Enabled + None: TODO object usable; no dedicated checklist.
- Disabled: TODO capability not advertised; supported object methods unavailable;
  no dedicated checklist or generated TODO context.

Disabling retains the stored list and the chosen display mode. Re-enabling
restores access and the applicable display. Merely removing tool documentation
is not sufficient: retained callable references must also reject supported
TODO operations while disabled. This controls the supplied feature, not the
agent's ability to create unrelated Python lists or write its own task notes.

### Settings menu

```text
Settings
  Theme                  automatic
  TODO list              on · all              ›

TODO list
  Enabled                On                    ›
  Display                All                   ›

Display
  All
  Current
  None
```

Display remains remembered but is dimmed/unavailable while the feature is off.

## Feature behavior

### Defaults and persistence

Agreed defaults: enabled, display All. Proposed file shape:

```toml
[todos]
enabled = true
display = "all" # "all", "current", or "none"
```

Use existing field-wise project-over-user settings precedence. No environment
variable or session-specific display preference in v1. Menu confirmation applies
and saves the user default, following existing save-error and project-masking
notices; do not silently edit project configuration. Retain current-process
choices when a user-default save fails or is masked, as the theme picker does.
Do not toggle feature availability merely by moving picker focus.

Display changes apply immediately. Feature availability is fixed for each Run:
read the latest accepted enabled preference at foreground Run start or child
admission, then retain it until that Run finishes, including background children.
New children use the latest preference at their own admission even if their
parent's active Run captured a different value. Do not cancel, reset, pause, or
live-migrate an active agent merely because this preference changes.

The full toggle is primarily a persistent session-start preference, defaults to
on, and is expected to remain on in normal use. Do not add a special startup
wizard, extra environment controls, or an elaborate live-toggle mechanism.

The menu shows the preference for new work and identifies active agents still
using an earlier value (for example, Off with an active child retaining On until
finish). Guidance, supported object methods, and generated context use each
agent's effective Run value, not the latest preference or an old flag restored
from its Arena checkpoint. Retained references reject operations when their
owning Run is disabled; an already-running enabled child is not retroactively
disabled by a preference change.

Apply effective availability per owner to the human-facing TODO displays too:
an enabled active child may retain its TODO detail while new foreground work is
disabled; None hides automatic TODO details immediately for every owner. Idle
foreground and retained terminal-child inspection use the current preference,
not an expired Run's captured setting. An availability change never replaces an
Arena or deletes its other bindings.

### List and codemode interface (agreed)

Each agent has one ordered list on its selected session branch. Child ownership
and human-facing child inspection do not grant cross-list mutation privileges.
Each item has nonblank text and one status: `pending`, `in_progress`, or
`completed`. No priorities, dependencies, due dates, phases, or automatic links
between parent items and child items in v1.

Expose a trusted-bootstrap Python `TodoList` object bound as `todos` in each
Arena. Its methods are synchronous; local list operations do not require async
host tools or external effects. Concise guidance and checking declarations make
this model-facing capability usable through codemode. It need not be a Rust
ToolName solely to expose local operations.

Agreed method set:

- `todos.add(text, status="pending")`: append an item and return its integer ID.
- `todos.update(item_id, text=..., status=...)`: change the specified fields of
  an existing item; preserve its ID and position. Omitted fields are unchanged.
- `todos.remove(item_id)`: remove an item without renumbering other items.
- `todos.replace(items)`: atomically replace the list, assign fresh IDs, and
  return the accepted snapshot. `[]` explicitly clears the list.
- `todos.snapshot()`: return an independent copy of the complete list.

Replacement input is an ordered list of ordinary dictionaries requiring `text`
and `status`. It also accepts dictionaries returned by `snapshot()`: any supplied
`id` is ignored and fresh IDs are allocated. Reject other unexpected fields.
Replacement never preserves IDs or matches by text. Snapshots are ordinary
transferable dictionaries with `id`, `text`, and `status` fields:

```python
[
    {"id": 1, "text": "Inspect existing code", "status": "in_progress"},
    {"id": 2, "text": "Implement the change", "status": "pending"},
]
```

Use item methods for routine progress changes and replacement for a new or
substantially revised plan. Mutating a returned snapshot does not mutate live
state. No separate clear/start/complete methods are needed.

IDs are integers local to one agent/list history, not global task IDs. Independent
agents and branches may have the same numeric IDs. Existing-item updates preserve
IDs; removals, replacement, and clear never reset the allocator or reuse a removed
ID along the selected history. The allocator is checkpointed/restored with the
list, so branch restoration restores both together. Replacement allocates fresh
IDs rather than carrying any old handles forward.

Unknown IDs raise `KeyError`; invalid text/status/replacement input raises
`ValueError`; supported methods raise `RuntimeError` when unavailable under the
owning Run's policy. Each invalid operation leaves the list and ID allocator
unchanged. Atomicity is per method call, not a transaction over an entire Python
execution containing several calls.

Agreed validation bounds are 64 items per agent, counting every status, and 512
Unicode code points per item's text (`len(text)`, not UTF-8 byte length). Text
must be a string containing at least one non-whitespace character; status must
be one of the three supported values. These are internal safety defaults, not
new configuration settings. Reject oversized input with `ValueError`, never
silently truncate. Validate all replacement entries before mutation/allocation;
failed add/update/replace operations do not consume IDs or alter accepted state.

Prefer these methods over direct nested-field mutation so validation/atomicity
stay local to the object. Regenerate its projection after committed execution,
not only top-level rebinding. An ordinary exception later in the execution does
not imply rollback: display follows the state actually committed under existing
Arena rules. Cancelled/discarded execution publishes no uncommitted list changes.
No prose parsing, completion inference from tool success, or automatic resets.

Guidance encourages substantial multi-step use, not checklists for trivial
requests. The list remains readable after compaction, and the Model Context
policy below restores orientation without relying on an old update being retained.

### Model Context (agreed policy)

When enabled, provide a complete snapshot of TODO item text, statuses, and order:

1. At the start of each foreground Run or child launch, before that agent's
   first normal Model Step.
2. After successful compaction is installed, before the next normal Model Step.

This covers manual, threshold, and overflow-recovery compaction. Failed or
structurally no-op compaction is not a refresh trigger. If run-start compaction
occurs before the first normal step, coalesce the triggers into one snapshot of
the selected committed state after compaction. A request retry uses the same
snapshot unless successful compaction requires a refresh.

Read canonical state on the selected branch, not text extracted from an earlier
tool call, a display cache, or the compaction summary. Snapshot the full list,
including completed items: All / Current / None are strictly UI filters. Enabled
+ None still receives the snapshot; disabled receives neither TODO guidance nor
a generated TODO snapshot. An enabled empty list is represented explicitly as
no current TODO items, so an old list in history or a summary cannot resurrect
cleared work.

The snapshot is a generated Model Context projection, not a new user request or
a new model step. Capture once per trigger and keep that captured snapshot
available to subsequent steps; do not refresh it from live state on every step
or append another durable reminder record each step. Replace obsolete generated
TODO-context projections rather than accumulating independent reminder copies.
This does not rewrite ordinary tool history or pretend the model did not see an
earlier state. The storage/replay representation must preserve these semantics
across all supported providers and follow existing replay rules.

Label the snapshot as state at capture time. Accepted TODO object mutations later
in the sequence supersede it; a fixed run-start snapshot must never be presented
as overriding newer updates. The object exposes its current state through `snapshot()`,
and the agent can read/print it through ordinary codemode when needed. No
additional per-step reminder machinery is required. A fresh post-compaction
snapshot takes precedence over any older TODO state described by the summary.

Count the generated snapshot in context estimates and request budgets. TODO
item text is task data, not trusted harness instructions; serialize it as data
rather than interpolating it into policy text. The snapshot provides orientation,
not enforcement: unfinished items do not prevent a final response, trigger
automatic continuation, or require additional model work at Settlement.

### Durable state and Display Projection

Follow the existing vision: the list is canonical Arena state. The host reads a
transferable Display Projection after committed execution, restoration, branch
selection, or Arena replacement. A TUI cache is never a second source of truth.
Resume and branch navigation display the corresponding list without replaying
mutations. Old or incompatible checkpoints must follow existing compatibility
rules, never bypass checking or silently fabricate executable state.

Keep the implementation narrow. This feature does not require delivering the
general plugin framework or a generic Arena inspector.

### Checkpoint compatibility (agreed)

Follow Kiki's existing checkpoint-contract version policy. Adding the TODO
bootstrap and checking declarations creates a new executable checkpoint contract
(`type-check-v4`, replacing `type-check-v3`). Pre-feature checkpoints remain
history-readable but cannot resume execution after this upgrade, even with TODOs
disabled. Do not silently install new bootstrap/checker state into an old dump,
construct replacement executable state from history, or migrate user bindings.
Report the incompatibility through the existing disabled/read-only recovery path.
Fresh sessions and checkpoints made under the new contract use the feature's
normal availability policy. Existing legacy/foreign identity rejection stays in
place. User explicitly selected this compatibility policy after the Monty proof.

### Independent agent lists and inline child display

Illustrative Current-mode display:

```text
● c1 · Inspect persistence · running
  ● Todos (1/3)
  └─ ◐ Trace checkpoint restoration
● c2 · Review terminal layout · running
  ● Todos (0/2)
  └─ ◐ Check short-terminal behavior
```

All also includes completed and pending child items within the available space;
None removes TODO details/counts but preserves the existing task rows.

Each agent owns its own TODO list, including nested subagents. A child starts
with an empty TODO list even though its other Arena bindings are copied from
the invoking checkpoint. Initialize the child's TODO object independently
without modifying the parent's object or discarding unrelated inherited state.
No automatic copying, shared mutation, result merge, or parent-status changes.
A parent may describe delegated work in the child prompt; that does not create
shared TODO state.

Show child TODO details nested under the corresponding entry in the existing
inline/sticky subagent display, not in the foreground checklist and not in a
new layout. Apply the same All / Current / None policy to each list. In None,
keep the existing subagent lifecycle/status row but omit TODO items and TODO
counts. A running child's lifecycle state does not imply that any TODO item is
`in_progress`.

Publish child projections after committed child execution so the human can see
progress without waiting for the parent model to call refresh/wait. Scope every
update by agent/task identity, selected branch, and current lifecycle generation;
stale updates must not affect another child or a newly selected branch. These
UI updates do not automatically inject child-private TODO lists into the parent
Model Context or merge them into the parent's list.

Keep the last committed child projection when a child finishes, fails, or is
cancelled; do not auto-complete its items. Live projections are derived from the
child Arena, not a second authority. Once the child Arena is released, retain
its final projection as immutable task-history data so it remains inspectable
through existing branch-scoped child history. Do not resume terminal children
merely to inspect their TODOs. Child inline rows retain the existing subagent
monitor's visibility lifecycle; retained-list inspection must not depend on a
row still being pinned.

An agent's generated TODO context contains only its own list. Capture initial
context at child launch before its first normal Model Step, and refresh after
any successful compaction that agent already supports. This feature does not
add a new child-compaction mechanism. Apply the same independence to descendants.
Read-only children may update their own Arena-local TODOs without gaining any
new filesystem/process permissions.

### Long lists and terminal constraints (agreed)

All means no status filtering, not unlimited terminal height. Keep the foreground
checklist above the prompt, with child details under existing subagent entries.
Preserve list order and apply All / Current / None consistently. Use an adaptive
shared height budget; TODO detail shrinks before essential prompt controls and
subagent lifecycle information. Do not let TODOs consume the whole session view.
Height limits are internal defaults, not additional user settings.

When eligible items do not fit, use an explicit overflow count and `/todos` hint
where space permits. Never silently turn All into an active-only preview.

`/todos` opens a read-only full-list viewer. With only the main list available,
open it directly. When child lists also exist on the selected branch, first show
an owner selector containing the main agent and branch-accessible active/retained
children, with owner labels and progress/lifecycle information. Selecting an
owner opens that owner's complete list. Terminal child lists remain accessible
even after their sticky rows disappear.

The viewer always shows the complete selected list regardless of All / Current /
None. It is deliberate inspection, not another automatic display mode. Support
keyboard scrolling; Esc goes back from a child/main view to the owner selector,
or closes when there was no selector; Esc in the selector closes it. Opening,
back navigation, and closing do not start model work or discard existing prompt
drafts. The viewer does not edit TODOs or add arbitrary cross-agent access.

None still suppresses every automatic TODO detail/count. Apply effective
availability per owner: disabled-owner inspection reports the feature is off and
does not reveal retained list contents. Existing enabled child Runs keep the
agreed policy while new disabled work does not gain access through inspection.

Define/test exact height allocation with the existing sticky monitor rather than
introducing a configurable layout system. Preserve current lifecycle-row
selection, Ctrl-T (thinking), Ctrl-O (retained-history expansion), text selection,
and prompt keyboard behavior.

## Acceptance criteria

- All / Current / None behave exactly as specified, including zero or multiple
  active items and an all-completed list; there is no run-based auto-hide.
- Disabled differs from None: tool availability is removed, not just the UI.
- Display and enable changes preserve stored contents and unrelated Arena state.
- Validation is atomic; clearing is explicit; returned snapshots cannot mutate
  live object state. The 64-item/512-code-point bounds apply to every status;
  oversized/invalid replacements cause no truncation, partial state, or consumed IDs.
- Tree markers/connectors and completed/total counts remain correct after
  filtering, overflow, owner selection, resize, and terminal child restoration;
  no nested-task data model or extra progress-label field is introduced.
- Display always follows the selected committed Arena state through ordinary
  errors, cancellation, resume, branching, and context compaction.
- Every child starts with its own empty list; parent/child/sibling/descendant
  mutations and context remain isolated. Live child details appear under the
  correct inline entry after committed changes, without parent polling.
- Terminal child lists remain branch-scoped and inspectable without child replay,
  with no auto-completion, stale-event leaks, or automatic parent-context insertion.
- Before an enabled agent's first normal step and after successful compaction,
  the model receives the complete selected list, even in Current or None; an
  empty list is explicit and stale generated snapshots are replaced.
- Captured context remains available between steps without live per-step refresh;
  later accepted updates supersede it. No duplicate per-step reminder records,
  TODO-triggered automatic continuation, or retry-dependent state drift.
- Manual, threshold, and overflow-recovery compaction respect the same policy;
  failed/no-op compaction does not trigger it. Snapshot costs enter budget checks
  and provider replay remains correct for OpenAI, Anthropic, and Google.
- Availability is captured per Run/child admission. Active children keep their
  captured value after a preference change; new foreground Runs and newly admitted
  children use the latest value. Menus show mixed effective values honestly,
  display changes remain immediate, and restored Arena flags cannot override Run
  policy. No child cancellation, restart, or silent state reset on preference change.
- Settings honor existing precedence, persistence, masking, and error conventions.
- Long lists, Unicode, narrow/short terminals, multiline prompts, themes, and
  active subagents remain usable without focus theft or shortcut regressions.

## Technical feasibility gate

Product/interface review is complete, including tree styling, object operations,
identity/replacement rules, validation limits, display/viewer behavior, per-agent
ownership, lifecycle, and context policy. No remaining product choices are being
assigned to the technical proof.

The first technical work must demonstrate support in the existing bootstrap,
checking, checkpoint, progress, session, and provider-context interfaces: fresh
child lists, committed projections, terminal retention, effective method gating,
and captured per-agent context/replay. Do not introduce a general plugin
framework or a second live host authority merely to avoid that proof.

Obtain the implementation skill's initial approval before code work. If the
proof exposes a conflict with the agreed contract, stop and review a source
revision rather than silently changing behavior. Explicitly establish existing-
checkpoint handling before claiming compatibility. The task outline and tests
are in [the implementation plan](plan.md).

## Evidence and integration pointers

- [TODO Arena state and projection direction](../VISION.md#L159), with
  [bootstrap example](../VISION.md#L66).
- [No built-in plan mode](../adr/0001-do-not-add-plan-mode.md).
- `src/tools.rs`: typed capabilities, generated codemode bootstrap, references,
  and checking stubs; `src/prompt.rs`: capability guidance assembly.
- `src/arena.rs`: bootstrap, execution commit, checkpoints, and restoration.
- `src/agent.rs`, `src/model.rs`, `src/compaction.rs`: normal-step preparation,
  Model Context entries, context estimation, and installed compaction state.
- `src/subagents.rs`: child Arena construction, progress, task observations,
  terminal records, and branch-scoped retained task data.
- `src/runtime.rs` / `src/session.rs`: Run lifecycle and selected session state.
- `src/config/settings.rs`: precedence, validation, and user-default writes.
- `src/tui.rs`, `src/tui/picker.rs`, `src/tui/subagents.rs`: settings navigation
  and pinned-display height allocation; `src/tui/slash.rs`: viewer command.

The local Maki TODO plugin demonstrates a pinned checklist; Codex's plan cells
illustrate compact counts and status styling. Pi has multiple TODO examples;
the user's enabled extension has not been identified. These are inspiration,
not requirements to copy their tool shapes or lifecycle rules.
