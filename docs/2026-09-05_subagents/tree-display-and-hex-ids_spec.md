# Spec: Compact subagent tree and hexadecimal IDs

## Context

The live sticky subagent monitor is flat: it stores records in admission order, partitions active entries before terminal entries, and renders at most four two-row entries (`src/tui/subagents.rs:69-75`, `src/tui/subagents.rs:180-280`). Nested task records already carry their immediate parent, but the monitor discards it (`src/subagents.rs:1316-1352`, `src/tui/subagents.rs:37-57`). The flat display therefore gives no visible indication that one subagent started another.

Task IDs are currently six-character sequential base-36 strings (`src/subagents.rs:34-56`, `src/subagents.rs:2608-2626`). That width is unnecessary at the configured scale and consumes useful header space. This follow-up supersedes the ID format and flat sticky-list rules in `docs/2026-09-05_subagents/spec.md`; history cards and runtime ownership remain unchanged.

## Goals

- Show parent-child relationships in the live sticky subagent monitor without changing durable history cards.
- Prioritise the deepest branch containing active work so recursion progress is immediately visible.
- Keep the preferred display to four real task nodes while retaining each visible task's two-row header and prompt preview.
- Preserve tree context when a terminal ancestor still has active descendants.
- Replace six-character base-36 task IDs with compact, sequential lowercase hexadecimal IDs that start at `00` and expand after `ff`.
- Keep tree selection cheap and derived from current monitor state rather than adding persistent layout caches.

## Non-goals

- Nested history cards, a tree navigator, expansion controls, or changes to transcript ordering.
- Random, reused, fixed-width, or user-configurable task IDs.
- Changes to subagent admission, ancestry, lifecycle, limits, Python signatures, telemetry ownership, or authority.
- A general TUI tree widget or a runtime-owned hierarchical projection.
- A workaround for the deferred Monty gathered-host-error issue.

## Key discoveries

- `TaskRecord` already exposes `parent()` on reliable spawned, record, and finished events; status-only updates apply only to entries the monitor already knows (`src/subagents.rs:1316-1352`, `src/tui/subagents.rs:95-160`).
- The monitor currently drops parent identity, keeps a flat admission-order vector, and derives its complete layout on each redraw (`src/tui/subagents.rs:37-75`, `src/tui/subagents.rs:180-280`).
- Terminal entries are cleared on the next accepted user request even when an active descendant may outlive its parent (`src/tui/subagents.rs:162-177`, `docs/2026-09-05_subagents/spec.md:114-116`).
- The existing layout already performs a linear scan of retained entries and rebuilds lines. The configured defaults are 64 starts per run and eight active tasks, but settings may be larger. A temporary derived forest can preserve the existing linear layout cost; persistent cache invalidation state is not justified without measurement (`src/config/constants.rs:15-18`, `src/config/settings.rs:623-656`, `src/tui/subagents.rs:180-240`).
- Header and transcript formatting use task IDs as variable-width strings, but `TaskId` derives lexical ordering. Variable-width hexadecimal IDs therefore need numeric ordering once `100` follows `ff` (`src/subagents.rs:38-56`, `src/tui/subagents.rs:320-370`, `src/transcript.rs:460-480`).
- The Python task interface types IDs only as `str`; no model-facing signature changes are required (`src/tools.rs:687-746`).
- Task state and sequence allocation are in-memory for one session. Trace output serialises IDs but Kiki does not deserialize those traces into live tasks (`src/subagents.rs:1655-1783`, `src/telemetry.rs:325-405`).

## Proposed approach

### Canonical task IDs

Generate IDs from the session sequence as lowercase hexadecimal with a minimum width of two:

```text
00, 01, …, 0f, 10, …, ff, 100, …
```

Parsing is strict. A valid ID contains only lowercase ASCII hexadecimal digits, has at least two characters, fits in the sequence counter, and has no redundant leading zero after the two-character range. Uppercase and non-canonical forms such as `0A`, `000`, and `0100` raise the existing invalid-ID `ValueError`. Unknown but well-formed IDs remain distinguishable from malformed IDs.

IDs remain session-unique and are never reused. Allocation must detect sequence exhaustion rather than wrap. Ordering remains numeric across width changes, so `ff` precedes `100` anywhere `TaskId` is used as a sorted key. Serialization and every model-facing/display surface use the canonical string.

### Derived monitor tree

Copy each reliable record's immediate parent into its monitor entry. Do not add a tree to runtime events and do not retain a cached render tree. Each `layout()` call derives the small forest and its preferred visible projection from current entries; temporary lookup maps local to that calculation are acceptable.

A branch candidate is the retained ancestor path from a root to an active task. Select the candidate with the greatest depth. Ties select the earliest-admitted leaf, keeping the display stable. If no retained task is active, use the deepest retained branch with the same tie-break. A record whose parent is unavailable is treated as a root so a publication gap cannot suppress the task.

Render the selected tree first. Preserve its root-to-leaf path and show at most the first four real nodes from that root. A terminal node on the path uses the existing muted terminal style. Each real node retains the current two-row content: lifecycle/model/measures on its header and the normalised beginning of its own prompt beneath it, enclosed in literal backticks. Apply display-cell-safe truncation to the prompt content so both backtick delimiters remain visible whenever the row can fit its prefix and the delimiter pair. Tree indentation reduces the available header and prompt width.

Use this row grammar, where node depth is relative to the displayed root:

- Root header prefix: two spaces followed by the lifecycle indicator.
- Nested header prefix at depth `d`: two base spaces, `d` two-space indents, then `└─` and the lifecycle indicator.
- Prompt prefix at depth `d`: two base spaces, `d` two-space indents, then `  └ `; the root therefore retains the current `    └ ` prefix.
- A summary owned by a visible parent at depth `d` is rendered as a child row at depth `d + 1`: two base spaces, `d + 1` two-space indents, then `└─…`.
- The global omitted-root summary has no parent and uses two spaces followed by `…`.

For example:

```text
  ✓ subagent:00  openai/gpt-5.6-sol  2 steps  00:18
    └ `Investigate the recursive behaviour and…`
    └─• subagent:03  openai/gpt-5.6-sol  1 step  00:06
      └ `Check the deepest active branch`
    └─… +2 siblings
```

The real-node budget is four. A selected path of one, two, three, or at least four visible nodes therefore leaves room for four, three, two, or one root respectively. After the selected tree, fill remaining root slots using trees containing active work first, where the root itself counts as part of its tree, and admission order within active/terminal groups. Additional displayed roots remain collapsed.

Omission categories are disjoint and use explicit labels:

- `… +N siblings` belongs to an expanded parent and is placed after the selected child's rendered subtree at that child's depth. `N` counts omitted direct children only; their subtrees stay folded beneath those siblings and do not increase `N`.
- `… +N descendants` belongs to a collapsed displayed root and is rendered at depth 1. `N` counts every retained descendant in that root's subtree.
- `… +N deeper · A active` belongs to the deepest visible selected-path node and is rendered one level beneath it. `N` counts every retained descendant of that anchor which is not otherwise visible, including a selected-path node removed by height degradation and its subtree; `A` is the active subset of those `N` tasks.
- `… +N roots` is the global root-level summary. `N` counts omitted root nodes; each omitted root's whole subtree stays folded beneath it and does not increase `N`.

Summary rows do not consume real-node slots, but all rows count against available terminal height. The deeper summary is re-anchored to the deepest selected-path node that remains visible whenever height degradation shortens the path. It keeps recursion activity observable when configured depth exceeds the four-node cap, even if all visible ancestors are terminal.

### Retention and constrained layouts

On an accepted user request, clear a terminal entry only when it is not an ancestor of an active retained task. Retain the complete terminal ancestor chain needed to explain active work. Once that subtree settles, those ancestors follow the existing rule and clear on the next accepted request. Late updates for entries already cleared remain ignored.

Build the preferred projection before fitting it to available rows. Never render half of a task's two-row entry. Height degradation is deterministic:

1. Remove top and bottom padding.
2. Remove collapsed-root descendant summaries, from the last displayed root backwards.
3. Remove selected-branch sibling summaries, deepest first.
4. Remove the global omitted-root summary.
5. Remove additional collapsed root node pairs, last displayed first.
6. Shorten the selected branch one deepest node pair at a time, retaining its root and adding or updating the deeper summary whenever that row fits.
7. Remove the deeper summary only when needed to retain a complete selected-root entry.
8. If the selected root's two rows cannot fit but one row remains, use the existing aggregate `Subagents: A active, T finished`; with no spare row, omit the monitor.

Counts cover all retained monitor tasks, not only rendered nodes. Derive surviving summaries once from the final fitted node slice; only branch shortening changes the deeper summary's anchor and hidden set.

Only visible active task headers participate in whole-second redraw tracking. Tree derivation stays uncached and preserves the current linear layout complexity by building one temporary parent/child index and memoising ancestry/depth within that render. Persistent cache invalidation across records, status, clearing, width, height and time remains unjustified without measurement.

## Key decisions & tradeoffs

- Derive the tree in the existing monitor; reject mutable tree indexes and a runtime-owned display projection.
- Expand the deepest branch containing active work, then the deepest retained branch; reject latest-update selection because it would make the display jump.
- Break equal-depth ties by admission order and render the selected tree first.
- Use four real task nodes rather than four root trees; omission summaries may add rows but remain bounded by tree depth, displayed roots, and terminal height.
- Keep each visible child's prompt preview; reject one-line child nodes because prompt intent is part of the useful live view.
- Retain terminal ancestors of active work; reject promoting their children to roots because that hides ancestry.
- Derive layout on demand without a persistent cache; measure before adding invalidation state.
- Use strict, expanding lowercase hexadecimal IDs; reject random two-character IDs because their 256-value hexadecimal space collides quickly, and reject fixed-width IDs because sessions can exceed that space.
- Preserve numeric ID ordering and reject permissive case or leading-zero normalisation.

## Risks / unknowns

- Tree connectors and nested indentation reduce width sooner than the flat monitor; narrow-width tests must preserve readable ID/state prefixes and safe clipping.
- Omission summaries create variable-height entries, so fitting must preserve the prompt/history geometry guarantees and one-row fallback.
- Terminal-ancestor retention changes clearing from a per-entry predicate to an ancestry-aware predicate; stale-event suppression must remain correct after the whole retained subtree becomes clearable.
- Configured depth may exceed the default. The four-node cap and deeper summary must bound the display without assuming `max_depth = 4`.

## Dependencies

- Original subagent contract and presentation constraints: `docs/2026-09-05_subagents/spec.md`.
- Current live monitor and geometry integration: `src/tui/subagents.rs`, `src/tui.rs:1538-1675`.
- Existing task ancestry and identity ownership: `src/subagents.rs`.
- Deferred Monty issue, unchanged by this work: `docs/research/2026-09-12_monty-gathered-host-errors.md`.

## Plan

### Acceptance criteria

- Canonical task IDs are sequential lowercase hexadecimal strings: `00` through `ff`, then `100`; they expand without reuse, reject malformed/non-canonical input, and retain numeric ordering across width changes.
- The live sticky monitor renders the deepest active branch first, with first-admitted tie-breaking, terminal ancestors, tree connectors, and each real node's prompt preview.
- The preferred projection contains at most four real task nodes. Root capacity falls from four to one as the expanded visible branch grows from one to four nodes.
- Headers, backtick-delimited prompt rows and omission rows follow the specified indentation grammar. Omitted siblings, collapsed descendants, deeper nodes and roots have non-overlapping clipped counts; deeper summaries include active descendants, while missing parents degrade to roots.
- Accepted requests preserve terminal ancestors of active tasks and clear them on a later accepted request after the subtree settles.
- Durable history cards, runtime event shapes, Python subagent signatures, admission behaviour, selection boundaries, narrow-terminal fallbacks, and once-per-visible-second redraw behaviour remain unchanged.

### Overall validation

From the repository root checkout, create `work/subagent-tree-and-ids` and its matching worktree from the accepted `work/subagents` head rather than `main`, then enter it:

```sh
git worktree add -b work/subagent-tree-and-ids worktrees/subagent-tree-and-ids work/subagents
cd worktrees/subagent-tree-and-ids
```

Run implementation and validation from that feature worktree with no live provider calls:

```sh
cargo fmt --all -- --check
cargo check --all-targets
env -u OPENAI_API_KEY just test
cargo clippy --all-targets -- -D warnings
git diff --check
```

After user acceptance, return to the repository root checkout, merge the tested feature branch back into `work/subagents`, not `main`, then remove its worktree and branch. The broader `work/subagents` branch retains its existing separate acceptance and merge path to `main`:

```sh
cd /path/to/kiki
git -C worktrees/subagents merge --no-ff work/subagent-tree-and-ids
git worktree remove worktrees/subagent-tree-and-ids
git -C worktrees/subagents branch -d work/subagent-tree-and-ids
```

### Planning context

- `TaskId` owns validation, display and sorted-key behaviour; allocation currently increments `Inner.next_task` after admission (`src/subagents.rs:34-56`, `src/subagents.rs:1960-1995`, `src/subagents.rs:2608-2626`).
- Exact six-digit fixtures occur across coordinator, Arena, runtime, transcript and TUI tests. Valid-unknown fixtures must remain canonical hexadecimal rather than becoming malformed (`src/subagents.rs:3185-3210`, `src/arena.rs:5140-5180`, `src/runtime.rs:6815-7000`).
- Reliable task records already contain the parent needed by the monitor. No production runtime or coordinator interface change is required (`src/subagents.rs:1316-1352`, `src/runtime.rs:595-685`).
- Monitor layout, clearing, prompt clipping, width pressure and timer visibility are concentrated in `src/tui/subagents.rs`; outer geometry and selection behaviour are covered in `src/tui.rs:5808-6011`.

### Task 1: Use canonical hexadecimal task IDs

**Changes** — Replace six-character base-36 generation and parsing with strict minimum-two-character lowercase hexadecimal IDs, checked non-reusing sequence exhaustion, and numeric ordering across variable widths. Keep serialized/model-facing IDs as strings. Update the original subagent specification's ID contract and examples. Update exact ID fixtures without weakening malformed-versus-unknown coverage.

**Tests** (`src/subagents.rs`, identity policy; existing integration fixtures) — First make the identity test require `00`, `01`, `0f`, `10`, `ff`, and `100`, numeric sorted order across the width boundary, strict rejection of uppercase/redundant-leading-zero/non-hex/overflow forms, a canonical unknown ID, and no reuse after settlement. Seed the isolated allocator at its final representable sequence, admit that final ID, and prove the next admission returns identifier-space exhaustion without wrapping or reuse. Existing Arena, runtime, transcript, telemetry and TUI tests must continue to cover correlation and presentation using the new IDs.

**Files** — Modify: `src/subagents.rs`, `src/arena.rs`, `src/runtime.rs`, `src/transcript.rs`, `src/tui.rs`, `src/tui/subagents.rs`, `docs/2026-09-05_subagents/spec.md`; modify other test-only fixtures only where the compiler or focused test search identifies an exact old-format assumption.

**Verification** — `MONTY_BIN=/tmp/kiki-monty/bin/monty cargo test subagents::tests --all-targets -- --test-threads=1 && MONTY_BIN=/tmp/kiki-monty/bin/monty cargo test arena::tests --all-targets -- --test-threads=1` — identity boundaries, authority lookup and real-worker task operations pass with canonical hexadecimal IDs.

### Task 2: Render the bounded live subagent tree

**Changes** — Retain parent identity in monitor entries, preserve terminal ancestors of active tasks, derive the deepest preferred branch and collapsed roots during layout, and render the selected tree with a four-real-node budget, counted omission summaries and per-node prompt previews. Preserve active-tree/admission ordering, terminal styles, narrow-terminal degradation and visible-second ticking. Update the original subagent specification's sticky presentation contract.

**Tests** (`src/tui/subagents.rs`, monitor behaviour; `src/tui.rs`, rendered geometry) — First render a forest where the deepest active branch is not the earliest root and require it first with its terminal ancestor and backtick-delimited prompt previews. Assert exact row prefixes and output for equal-depth admission tie-breaking, an active root competing with an earlier terminal root, all-terminal fallback, four/three/two/one root capacity, disjoint sibling/root/descendant/deeper counts, an active task below four terminal visible ancestors, collapsed non-selected descendants, and missing-parent roots. Cover prompt truncation retaining both backtick delimiters when they fit, re-anchored deeper counts as each height transition shortens the selected path, ancestor retention and later clearing, narrow width clipping, muted terminal rows, and redraws only for visible active whole seconds. Preserve outer prompt/history placement and exclusion from text selection.

**Files** — Modify: `src/tui/subagents.rs`, `src/tui.rs`, `docs/2026-09-05_subagents/spec.md`.

**Verification** — `cargo test tui::subagents::tests --all-targets -- --test-threads=1 && cargo test tui::tests::subagent_monitor --all-targets -- --test-threads=1` — tree policy, rendering bounds, clearing, timers and outer geometry pass deterministically.
