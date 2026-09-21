# Plan: Persisted session trees: Improve tree navigation UX

## Source

- [Persisted session trees spec](spec.md), especially **Navigation and correction**.
- [Completed Phase 2 plan](phase-2_plan.md), which provides the implementation baseline.
- [Kiki and Pi session-tree UX research](../research/2026-09-19_kiki-pi-session-tree-ux.md).
- User-approved follow-up decisions from the Pi comparison.

This is one post-Phase 2 PR. It supersedes Phase 2's deferral of transient display folding only; durable labels, bookmarks, branch summaries, compaction and other branch-management features remain deferred.

## Acceptance criteria

- The tree dock uses three full-width rules: above its header, between header and content, and below content. Only visible tree content is capped at half the terminal height; chrome may expand above that.
- The normal prompt header is absent while the tree is open. One responsive tree header line contains, as space permits: title; active notice; non-empty live search query; selected/visible count; conversation-mode state; then key hints when no notice is active.
- At every fork, sibling branches are ordered by their latest descendant revision descending. Parents still render before descendants, so each thread reads forwards.
- Linear continuations remain flat. All genuine fork levels render without the six-level cap or structural `…`; ordinary text preview truncation remains Unicode-safe. There is no horizontal panning.
- Left/Right provide Pi-style segment folding and navigation. Fold state is transient; `⊟` and `⊞` identify expanded and folded segments.
- Search edits and conversation-mode changes clear folds.
- Search tokenises on whitespace and requires every case-insensitive token to match the complete untruncated row text, in any order.
- Escape with a non-empty search clears it; Escape with an empty search closes the tree.
- Ctrl-U toggles conversation-only mode. It shows Session start, User, Kiki, Cancelled and Failed, omits Python rows, and reconnects remaining nodes to their nearest visible ancestors. If the durable current point is a Python execution, that one Python row remains visible with `●`.
- Tree rows use a small internal dark semantic palette derived from Pi: accent, success, warning, error, muted, dim and selected background. No theme configuration is added.
- Active ancestors retain `•`, the current point retains `●`, and active connectors are accented. Alternative branches are subdued without losing semantic type colours.
- Selection uses `›` and Pi's subtle selected background behind rendered cells rather than reverse video.
- Ctrl-X copies the complete terminal-safe underlying entry content without connectors, markers or display truncation. Python copies all durable execution evidence: commentary, source, tool names and summaries, stdout, stderr, result or Python error. The tree stays open and reports copy success or failure.
- Loading, no-match, tiny-terminal, idle-only navigation, draft preservation, correction, subagent restoration and read-only failure behaviour remain intact.
- Compaction receives a distinct semantic colour when implemented later; this change adds no unused compaction token.

### Non-goals

- Custom themes, theme selection, theme files or hot reload.
- Horizontal panning.
- Labels, timestamps, bookmarks, branch summaries or compaction.
- Mouse tree navigation or configurable keybindings.
- Persisted fold or view state.
- Browsing or navigating the tree while foreground or background work is active.
- Storage-schema, persisted wire-codec, runtime-navigation or restoration changes. The in-memory catalogue projection may expose execution data already decoded from the existing payload.

## Overall validation

Use exact Monty `0.0.22` for the complete suite:

```sh
just monty
export MONTY_BIN=/tmp/kiki-monty/bin/monty
```

Each behavioural task starts with its named failing test, becomes one signed-off atomic commit, and passes its focused verification before review. Run the complete committed branch verification after all tasks:

```sh
cargo fmt --all -- --check
git diff --check main...HEAD
cargo check --all-targets
cargo clippy --all-targets -- -D warnings
just test
```

Expected result: formatting and diff checks produce no output; check and clippy complete without warnings; all unit, integration and actual-binary CLI tests pass, with only explicitly ignored tests skipped.

## Planning context

- `App` currently combines a generic `Picker<PickerValue>` with a parallel `TreePicker`, so tree loading, input, catalogue, width and rendering policy leak into the caller (`src/tui.rs:152`, `src/tui.rs:157`, `src/tui.rs:691`, `src/tui.rs:777`, `src/tui.rs:1436`, `src/tui.rs:2148`). Replace them for tree use with one concrete navigator over `SessionEntryId`; remove `PickerValue::Tree` and `TreePicker`. `App` keeps runtime and clipboard effects only.
- The generic picker already owns focus, scrolling, paging and contextual rows (`src/tui/picker.rs:49`, `src/tui/picker.rs:96`, `src/tui/picker.rs:205`). Retain that earned machinery behind the tree seam. Add only caller-provided focused variants for dynamic rows when semantic selection first needs them, focused-value access when copy first needs it, then position access and frameless rows when the local header/dock first needs them. Tree tokenisation, query editing, Escape, Ctrl-S, Ctrl-U, Ctrl-X and Left/Right remain tree policy. `PickerSavePolicy` can disappear once tree Ctrl-S is intercepted (`src/tui/picker.rs:40`, `src/tui/picker.rs:153`).
- Ordinary model and thinking pickers remain centred, bordered, capped at 72 columns, substring-filtered and reverse-video selected (`src/tui.rs:843`, `src/tui.rs:864`, `src/tui/picker.rs:440`).
- The catalogue already returns immutable parent ID, durable revision, full request/final text, execution commentary, aggregate tool outcomes and the active entry (`src/session.rs:237`). Existing payloads also contain source, complete decoded tool names/summaries, bounded stdout/stderr and result/error evidence, and catalogue loading already decodes each full payload (`src/session/codec.rs:56`, `src/session/codec.rs:127`, `src/session/codec.rs:158`, `src/session.rs:1930`). Represent one catalogue execution canonically as source, commentary and the existing `arena::Execution`; derive both rows and copy text from it.
- Catalogue installation, view refresh, resize and copy are distinct work. Installation computes immutable topology, subtree recency, active ancestry and canonical search text once. View refresh applies mode, token search, context and folds. Resize rebuilds only width-dependent rows. Ctrl-X formats only the selected node on demand; complete output is not retained in every projected row.
- The tree currently sorts children by their own ascending revision, traverses iteratively, stores six fixed ancestor connector slots and emits a structural ellipsis beyond them (`src/tui/tree.rs:17`, `src/tui/tree.rs:98`, `src/tui/tree.rs:210`, `src/tui/tree.rs:262`). Compute recency bottom-up and carry connector ancestry during iterative projection; do not introduce recursion, a replacement fixed cap or an unbounded ancestry vector on every durable node.
- Search text is already separate from width-dependent display rows, and filtered projections distinguish selectable direct matches from non-focusable context (`src/tui/tree.rs:75`, `src/tui/tree.rs:153`, `src/tui/picker.rs:394`). Preserve that separation.
- The current tree gives each row one style and the picker applies reverse video (`src/tui/tree.rs:176`, `src/tui/tree.rs:250`, `src/tui/picker.rs:249`). The tree navigator supplies normal and focused styled variants for dynamic rows, keeping semantic foreground and selected-background policy out of the generic picker.
- Kiki has no theme module; fixed presentation colours are private to their renderers (`src/transcript.rs:366`, `src/tui.rs:2438`, `src/tui/subagents.rs:740`). Keep the approved Pi-derived tree palette private to `src/tui/tree.rs` rather than relocating unrelated transcript constants into a shallow module.
- Clipboard effects already pass through `ClipboardPort`, `pending_copy_text` and `finish_pending_copy`, including copy success/failure notices (`src/clipboard.rs:4`, `src/tui.rs:1194`). The navigator returns content; `App` performs the effect.
- Transcript already owns concise tool labels and bounded observation rendering (`src/transcript.rs:420`, `src/transcript.rs:468`, `src/transcript.rs:3881`, `src/transcript.rs:3912`). Expose those two formatters narrowly for tree copy instead of creating a second representation.
- The current dock caps the complete bordered picker at half-height and deducts border width from rows (`src/tui.rs:2148`, `src/tui/picker.rs:205`). The replacement caps content rows only, renders full-width rows, and uses the existing footer/accounting rule as its bottom rule.
- Pi provides the behavioural reference for active markers, subtle selection, fold markers and segment navigation, while Kiki keeps latest-descendant ordering, ancestor-preserving search, exact current-point marking and executable-state safety ([research](../research/2026-09-19_kiki-pi-session-tree-ux.md)).

## Proposed approach

Deepen `src/tui/tree.rs` around one concrete tree navigator over `SessionEntryId`. It privately owns loading/ready state, the generic picker, durable topology, query, mode, folds, focus and rows; later presentation tasks add status and local-header formatting at first use. `App` opens it, installs a matching catalogue, passes input and rendering space, and reacts only to close, navigate or copy outcomes. Notices enter the navigator only as render input; tree state never depends on them.

The navigator separates work into four paths:

1. **Catalogue installation, once** — build parent/children links; compute latest-descendant ordering and active ancestry iteratively; retain canonical row search text and canonical execution data.
2. **View refresh** — apply full/conversation eligibility, the exact-current Python exception, token-AND search/context, transient folds, visible relationships and fork levels.
3. **Resize/render** — generate only width-dependent normal/focused styled rows and the responsive local header. Connector ancestry is carried by the final iterative traversal rather than copied onto every durable node.
4. **Copy on demand** — format only the selected node's complete durable content and return it to `App`.

Search edits and mode changes clear folds before refresh. The generic picker remains a small focus/viewport module, not a configurable tree renderer.

## Tasks

### Task 1: Own tree navigation and token search behind one seam

**Changes** — Replace `App`'s split tree state with one concrete tree navigator over `SessionEntryId`, internally using `Picker<SessionEntryId>`. Remove `PickerValue::Tree`, `TreePicker` and the entry-to-generic-value mapping callback. The navigator owns correlated loading/ready state, query, row width, focus and tree input outcomes. `App` translates only close, navigate and later copy outcomes.

Keep token-AND matching tree-specific: split the canonical query on whitespace and require every case-insensitive token in complete canonical row text. Tree Escape clears a non-empty query; the next closes. The navigator intercepts Ctrl-S so the generic `PickerSavePolicy` can be removed. Loading remains Esc-only. Preserve `/tree <filter>`, Ctrl-R, double-Escape, initial active focus, stale catalogue rejection, width refresh and idle/read-only errors. Ordinary pickers retain substring matching, one-Escape close and save behaviour.

Do not add selection/header picker APIs before their later tasks need them. Replace tree-seam tests rather than retaining tests of the removed parallel state.

**Tests** (`src/tui/tree.rs`, navigator interface; `src/tui/picker.rs`, save-policy regression; `src/tui.rs`, composition) — First red: `tui::tree::tests::navigator_tokenises_search_and_clears_it_before_closing`. Cover out-of-order token matching, case/whitespace normalisation, second-Escape close, loading precedence and initial filter. Picker tests prove unchanged static substring/Escape/save behaviour after removing `PickerSavePolicy`. TUI tests cover stale catalogue correlation and one navigate command through the new seam.

**Files** — Modify: `src/tui.rs`, `src/tui/picker.rs`, `src/tui/tree.rs`.

**Verification** —

```sh
cargo test --bin kiki tui::picker::tests -- --test-threads=1
cargo test --bin kiki tui::tree::tests -- --test-threads=1
cargo test --bin kiki tui::tests::tree_picker_loading_ -- --test-threads=1
cargo test --bin kiki tui::tests::tree_navigation_ -- --test-threads=1
cargo check --all-targets
```

The deep navigator replaces the parallel tree state without changing runtime navigation or ordinary pickers.

### Task 2: Order recent branches first and render every fork level

**Changes** — During catalogue installation, compute each node's latest descendant revision with one iterative bottom-up pass. At every fork, sort child subtrees by that revision descending, then by durable descending revision/entry ID. Keep parent-before-descendant preorder; active state never changes order.

Remove the six-fork cap and structural ellipsis. During iterative projection, carry only the current connector stack and emit complete visible prefixes; do not store an unbounded ancestry vector on every node. Linear continuations remain flat and ordinary preview truncation remains Unicode-safe. Add no horizontal viewport state.

**Tests** (`src/tui/tree.rs`, navigator topology/golden) — First red: `tui::tree::tests::orders_sibling_branches_by_latest_descendant_revision`. Cover an older branch continued most recently, an older active branch that remains below a recent sibling, nested parent-before-child order, complete depth-7/depth-8 connectors with no structural `…`, the existing 4,096-entry flat chain, and one large iterative mixed tree without a quadratic golden.

**Files** — Modify: `src/tui/tree.rs`.

**Verification** —

```sh
cargo test --bin kiki tui::tree::tests -- --test-threads=1
cargo fmt --all -- --check
cargo clippy --all-targets -- -D warnings
```

Ordering, full-depth connectors and stack-safety cases pass.

### Task 3: Toggle a conversation-only projection

**Changes** — Add a transient full/conversation mode, defaulting to full on each open. Ctrl-U toggles mode. Conversation mode retains Session start, User, Kiki, Cancelled and Failed, omits Python rows, and reconnects retained nodes to their nearest retained visible ancestors. Retain the exact current Python row as the sole execution exception.

Ordering remains based on the complete durable subtree. Search only eligible rows and retains eligible context ancestry. If Ctrl-U removes a focused Python row during search, focus its nearest retained ancestor that directly matches the query; if none does, allow the picker's ordinary first matching-row fallback. Preserve context rows as non-selectable. Keep mode private. Add only narrow generic picker `focused_value` and `focus_value` primitives for this fallback; tree policy stays in `tree.rs`. Task 7 derives selected/visible position through the picker only when header formatting needs it.

**Tests** (`src/tui/tree.rs`, navigator interface/golden; `src/tui/picker.rs`, focused-value contract; `src/tui.rs`, key composition) — First red: `tui::tree::tests::conversation_mode_reparents_history_and_keeps_current_python`. Cover default/round-trip mode, request-execution-final chains, terminal outcomes, forks from omitted executions, reconstructed connectors, exact-current exception, nearest-ancestor focus, search and recency ordering. Picker tests cover focused-value access and focusing an eligible value. One TUI test proves Ctrl-U reaches the navigator rather than prompt controls.

**Files** — Modify: `src/tui.rs`, `src/tui/picker.rs`, `src/tui/tree.rs`.

**Verification** —

```sh
cargo test --bin kiki tui::tree::tests -- --test-threads=1
cargo test --bin kiki tui::tests::tree_picker_ -- --test-threads=1
cargo check --all-targets
```

Both projections and their search/focus/order composition pass.

### Task 4: Fold and traverse visible branch segments

**Changes** — Keep one transient set of folded `SessionEntryId` heads. Derive foldable segments, enclosing starts and first-child traversal from the current pre-fold visible parent/child projection; add no persistent segment graph or generic folding engine.

Left folds the selected expanded segment, otherwise jumps to its enclosing segment start. Right unfolds the selected folded segment, otherwise follows the first visible child through flat continuations to the next fork or branch end. At joints replace `─` with `⊟`/`⊞`, producing `├⊟ ` or `└⊞ `. A connectorless root follows Pi and shows `⊞ ` only while folded. Folding preserves durable identity and active state; an active folded head retains its marker. Search edits, Escape clearing search and Ctrl-U mode changes clear the folded-ID set before refresh.

**Tests** (`src/tui/tree.rs`, navigator interface/golden; `src/tui.rs`, key composition) — First red: `tui::tree::tests::left_and_right_fold_and_traverse_branch_segments`. Cover root and segment foldability, flat non-foldable continuations, joint/root markers, enclosing jumps, first-child traversal, boundaries, focus retention, active hidden descendants, query edits and Ctrl-U fold reset. One TUI test proves Left/Right route to the tree while open.

**Files** — Modify: `src/tui.rs`, `src/tui/tree.rs`.

**Verification** —

```sh
cargo test --bin kiki tui::tree::tests -- --test-threads=1
cargo test --bin kiki tui::tests::tree_picker_ -- --test-threads=1
cargo fmt --all -- --check
cargo clippy --all-targets -- -D warnings
```

Fold projection, markers and segment navigation pass without changing durable state.

### Task 5: Render semantic dark tree history with subtle selection

**Changes** — Define a private fixed tree palette in `src/tui/tree.rs`, seeded from Pi dark: text `#d4d4d4`, accent `#8abeb7`, success `#b5bd68`, error `#cc6666`, warning `#ffff00`, muted `#808080`, dim `#666666` and selected background `#3a3a4a`. Do not move transcript constants or add theme configuration/unused compaction colour.

Build background-transparent semantic spans. Ordinary connectors are dim; active connectors and `•` ancestors use accent; `●` marks the current point. User uses accent, Python/tool summaries use muted/dim with semantic success/failure marks, Kiki uses success, Cancelled warning and Failed error. Alternative rows keep the semantic prefix but use muted connector/content and no active marker/accent.

For each dynamic tree item, supply normal and focused row variants. Every row reserves a two-column selection gutter: `› ` when focused and two spaces otherwise, so connectors never shift. The focused variant adds selected background only under rendered gutter/body cells while retaining semantic foregrounds. The generic picker chooses the caller-supplied focused variant; static model/thinking rows remain reverse-video. Preserve complete search text and grapheme-safe style-preserving truncation.

**Tests** (`src/tui/tree.rs`, navigator render/golden; `src/tui/picker.rs`, focused-variant contract) — First red: `tui::tree::tests::semantic_rows_preserve_colours_under_subtle_selection`. Assert every row kind, tool marks, active/current/alternative treatment, connector colour, cursor/background and absence of reverse video. Picker regression proves static rows remain reversed. Replace old whole-row colour assertions; retain CJK, emoji and combining-grapheme truncation coverage.

**Files** — Modify: `src/tui/picker.rs`, `src/tui/tree.rs`.

**Verification** —

```sh
cargo test --bin kiki tui::picker::tests -- --test-threads=1
cargo test --bin kiki tui::tree::tests -- --test-threads=1
cargo check --all-targets
cargo clippy --all-targets -- -D warnings
```

Semantic tree styling and unchanged static-picker styling pass.

### Task 6: Copy the complete selected history entry

**Changes** — Replace the lossy catalogue execution projection with one canonical representation containing source, commentary and the existing decoded `arena::Execution`. Use `codec::decoded_readable_execution` during catalogue construction and delete the redundant `decoded_execution_catalogue` helper. Derive row tool counts from `Execution.tool_calls`; no schema or persisted payload changes.

Reuse the picker's focused-value accessor added by Task 3. Ctrl-X formats only the selected node and returns a copy outcome without closing or moving focus. User/Kiki copy complete stored text without row prefixes. Python emits non-empty sections in this exact order, separated by one blank line:

1. `Commentary:` with complete commentary, when present.
2. `Python:` with complete source, always.
3. `Tools:` with one existing concise transcript tool label per line, when present.
4. `Stdout:` and `Stderr:` with bounded stored observation text, when present.
5. `Result:` or `Error:`, when present.

Every section uses `<Heading>:\n<content>` and sections are joined by exactly `\n\n`.

Observation text includes the existing omitted-lines/bytes marker and does not claim to recover bytes omitted by execution limits. Session start, Cancelled and Failed copy canonical text. Normalise terminal controls while preserving line breaks.

Expose transcript tool-label and observation formatting narrowly for reuse. `App` sends returned text through existing `pending_copy_text`/`ClipboardPort`, retaining `Copied`/`Copy failed` notices and flash timing.

**Tests** (`src/session.rs`, catalogue projection; `src/tui/tree.rs`, navigator copy; `src/tui.rs`, clipboard composition) — First red: `session::tests::catalogue_retains_complete_execution_for_tree_copy`. Session coverage proves one decoded `Execution`; existing codec tests continue to own round-trip detail. Tree table tests cover exact section order, absent sections, mixed tools, omissions, Python error, multiline User/Kiki and terminal controls. TUI adds only clipboard success/failure while the tree remains open.

**Files** — Modify: `src/session.rs`, `src/session/codec.rs`, `src/transcript.rs`, `src/tui.rs`, `src/tui/tree.rs`, `src/runtime.rs` (test-only).

**Verification** —

```sh
cargo test --bin kiki session::tests::catalogue_ -- --test-threads=1
cargo test --bin kiki session::codec::tests -- --test-threads=1
cargo test --bin kiki tui::tree::tests -- --test-threads=1
cargo test --bin kiki tui::tests::successful_copy_ -- --test-threads=1
cargo test --bin kiki tui::tests::failed_copy_ -- --test-threads=1
cargo check --all-targets
```

Complete durable execution evidence copies through the existing clipboard port without duplicate representations.

### Task 7: Separate the tree with rules and a responsive local header

**Changes** — Let the navigator own local-header formatting and tree content rendering; `App` allocates geometry, passes any active notice and renders surrounding transcript/monitor/footer. Add narrow position access and frameless dynamic-row rendering to the generic picker at this first use, leaving centred static pickers unchanged.

Replace the tree box with: top rule, one local header, middle rule, content rows, then the existing footer/accounting rule as bottom rule. Hide the prompt header while open and return that row to transcript/monitor space. Cap content at half terminal height, then cap the complete dock by available space. Render rows full-width.

Separate header segments with ` · ` and render position as `N/M`; no-match is `0/0`, loading omits position. Canonical segment order is `Session tree`, active notice, non-empty `Search: <query>`, position, `[conversation]`, then hints. Omit the search segment when the query is empty. A notice removes all hints but leaves the remaining core segments in that order. Without a notice, append whole hints in priority: `Enter restore`, `Esc clear/close`, `↑/↓ move`, `←/→ branch`, `Ctrl-X copy`, `Ctrl-U conversation`, `PgUp/PgDn page`. Add only complete segments that fit. Under pressure remove hints first, omit position next, then front-truncate and finally omit the query. Retain title, active notice and mode until final Unicode-safe clipping.

Define the fixed Pi-derived border colours privately here at their first use for the top/middle rules; retain the existing thinking/accounting bottom rule.

**Tests** (`src/tui/tree.rs`, table-driven header/content render; `src/tui/picker.rs`, frameless contract; `src/tui.rs`, composition) — First red: `tui::tree::tests::header_prioritises_live_state_on_one_line`. Table-test wide/medium/narrow/tiny widths, delimiters, count, notices, hint priority, query tail, loading/no-match and conversation state at the navigator seam. Picker tests only framed versus frameless rows. Replace obsolete boxed-picker tests. TUI keeps one normal dock layout and one tiny layout covering rules, content-only half-height cap, transcript, monitor, footer and hidden/restored draft.

**Files** — Modify: `src/tui.rs`, `src/tui/picker.rs`, `src/tui/tree.rs`.

**Verification** —

```sh
cargo test --bin kiki tui::picker::tests -- --test-threads=1
cargo test --bin kiki tui::tree::tests -- --test-threads=1
cargo test --bin kiki tui::tests::session_tree_picker_ -- --test-threads=1
cargo test --bin kiki tui::tests::only_session_tree_ -- --test-threads=1
cargo fmt --all -- --check
cargo clippy --all-targets -- -D warnings
```

The navigator owns header/content policy; App composition and ordinary picker layouts remain correct.

### Task 8: Document improved tree navigation

**Changes** — Update `README.md` with latest-descendant ordering, complete fork indentation, semantic active/current/selection presentation, token search and Escape clearing, Left/Right folds, Ctrl-U conversation mode, Ctrl-X complete entry copy, rule-separated layout and the content-only half-height cap. Keep transient view state distinct from durable branches and preserve idle-only/external-effect safety.

Retain compaction, branch summaries, labels, custom themes, horizontal panning and configurable keys as deferred. Note only that future compaction rows need a distinct semantic colour; do not add unavailable controls or rewrite the completed Phase 2 plan.

**Tests** — Documentation-only. Verify every documented key/string against committed tests and remove obsolete claims that the whole picker is half-height capped.

**Files** — Modify: `README.md`.

**Verification** —

```sh
git diff --check
rg -n "Session tree|conversation|Ctrl-U|Ctrl-X|Left|Right|half" README.md
```

README matches the implemented navigator.
