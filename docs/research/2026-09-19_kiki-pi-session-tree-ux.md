# Research: Kiki and Pi session-tree UX

## Scope

This document compares Kiki's session-tree interface at commit `c9ce372` with the installed Pi `0.85.1` session-tree interface. It covers user-visible layout, styling, tree projection, search, controls, navigation semantics, and adjacent session features.

The installed Pi distribution under `/opt/homebrew/lib/node_modules/@earendil-works/pi-coding-agent` is the source of truth for current Pi behaviour. Pi documentation and its tree screenshot are used where they describe product intent, but not where they differ from the installed implementation. This document records existing behaviour and deliberate scope differences; it does not recommend changes.

## Summary

- Both interfaces replace the editor with a full-width tree selector and retain the transcript above it. Kiki caps the whole selector at half the terminal height; Pi caps only the list at half the terminal height and adds title, help, search, borders, status, and spacing around it ([Kiki layout](../../src/tui.rs#L2148-L2186), [Pi selector](../../../../../../../opt/homebrew/lib/node_modules/@earendil-works/pi-coding-agent/dist/modes/interactive/components/tree-selector.js#L1186-L1208)).
- Kiki uses a compact bordered picker with a title and filter row. Pi uses three rules, three blank rows, a title, binding-aware help, a search prompt, a position/filter status row, and the tree list ([Kiki picker](../../src/tui/picker.rs#L205-L264), [Pi list status](../../../../../../../opt/homebrew/lib/node_modules/@earendil-works/pi-coding-agent/dist/modes/interactive/components/tree-selector.js#L509-L610), [Pi help](../../../../../../../opt/homebrew/lib/node_modules/@earendil-works/pi-coding-agent/dist/modes/interactive/components/tree-selector.js#L1020-L1116), [Pi assembly](../../../../../../../opt/homebrew/lib/node_modules/@earendil-works/pi-coding-agent/dist/modes/interactive/components/tree-selector.js#L1186-L1208)).
- Both flatten single-child chains and introduce indentation at forks. Kiki preserves chronological sibling order and caps nested connector columns with an ellipsis; Pi moves the active subtree first at every fork and horizontally pans the tree body when indentation would hide the selected row's content ([Kiki projection](../../src/tui/tree.rs#L98-L129), [Kiki connectors](../../src/tui/tree.rs#L210-L299), [Pi ordering](../../../../../../../opt/homebrew/lib/node_modules/@earendil-works/pi-coding-agent/dist/modes/interactive/components/tree-selector.js#L120-L230), [Pi viewport](../../../../../../../opt/homebrew/lib/node_modules/@earendil-works/pi-coding-agent/dist/modes/interactive/components/tree-selector.js#L5-L37)).
- Kiki's tree styling is fixed: light text, cyan active ancestry, grey alternatives, a `●` current-leaf marker, and reverse-video selection. Pi separates selection, active ancestry, and semantic entry type through a cursor, active-path bullets, a selected background, and role/status theme tokens ([Kiki styling](../../src/tui/tree.rs#L176-L259), [Kiki palette](../../src/transcript.rs#L366-L382), [Pi styling](../../../../../../../opt/homebrew/lib/node_modules/@earendil-works/pi-coding-agent/dist/modes/interactive/components/tree-selector.js#L530-L699)).
- Kiki matches one complete case-insensitive substring and retains non-focusable ancestor rows. Pi tokenises the query on whitespace, requires every token to match, removes non-matching intermediates, and makes every remaining row selectable ([Kiki filtering](../../src/tui/tree.rs#L153-L198), [Pi filtering](../../../../../../../opt/homebrew/lib/node_modules/@earendil-works/pi-coding-agent/dist/modes/interactive/components/tree-selector.js#L233-L318)).
- Pi includes filter modes, folding, labels, label timestamps, copying, branch summaries, configurable keybindings, and themes. Kiki deliberately limits Phase 2 to search, navigation, retained branching, correction, draft preservation, and branch-scoped subagent restoration ([Pi controls](../../../../../../../opt/homebrew/lib/node_modules/@earendil-works/pi-coding-agent/docs/keybindings.md#tree-navigation), [Kiki scope](../2026-09-15_session-trees/phase-2_plan.md)).
- Kiki restores model context, the executable Python Arena checkpoint, and branch-scoped subagent state together. Pi rebuilds conversational context and can append an LLM-generated summary of the branch being left ([Kiki specification](../2026-09-15_session-trees/spec.md), [Pi navigation](../../../../../../../opt/homebrew/lib/node_modules/@earendil-works/pi-coding-agent/dist/core/agent-session.js#L2471-L2629)).

## Findings

### Layout and screen ownership

| Aspect | Kiki | Pi 0.85.1 |
|---|---|---|
| Editor relationship | Replaces the prompt dock while preserving the hidden draft ([source](../../src/tui.rs#L2148-L2186)) | Replaces `editorContainer` and restores editor focus on close ([source](../../../../../../../opt/homebrew/lib/node_modules/@earendil-works/pi-coding-agent/dist/modes/interactive/interactive-mode.js#L3738-L3763)) |
| Width | Full terminal width, with a Ratatui border and two-column inner-width deduction ([source](../../src/tui.rs#L2148-L2184)) | Full component width, with full-width rules and no enclosing box ([source](../../../../../../../opt/homebrew/lib/node_modules/@earendil-works/pi-coding-agent/dist/modes/interactive/components/tree-selector.js#L1198-L1208)) |
| Height | Whole picker is content-sensitive and capped at `terminal height / 2` and the available area ([source](../../src/tui.rs#L2150-L2158)) | Tree list is `max(5, floor(terminal height / 2))`; fixed and wrapping chrome is added outside that list ([source](../../../../../../../opt/homebrew/lib/node_modules/@earendil-works/pi-coding-agent/dist/modes/interactive/components/tree-selector.js#L1186-L1208)) |
| Transcript | Retained above the dock ([source](../../src/tui.rs#L2164-L2170)) | Retained above the editor container in fullscreen layout; regular mode keeps the selector in the main component stream ([source](../../../../../../../opt/homebrew/lib/node_modules/@earendil-works/pi-coding-agent/dist/modes/interactive/chat-viewport.js#L3-L27)) |
| Additional live UI | Kiki retains the subagent monitor immediately above the tree and the footer rule below it ([source](../../src/tui.rs#L2159-L2184)) | Pi retains editor-adjacent widgets and footer through its outer interactive layout; Pi core has no native subagent monitor ([usage](../../../../../../../opt/homebrew/lib/node_modules/@earendil-works/pi-coding-agent/docs/usage.md#design-principles)) |
| Tiny terminals | The half-height cap can leave only the filter row after borders when the terminal is very short ([picker geometry](../../src/tui/picker.rs#L205-L264)) | The list has a five-row minimum while help may wrap; the fullscreen layout clips the resulting component to its allocation ([selector](../../../../../../../opt/homebrew/lib/node_modules/@earendil-works/pi-coding-agent/dist/modes/interactive/components/tree-selector.js#L1186-L1208), [layout](../../../../../../../opt/homebrew/lib/node_modules/@earendil-works/pi-coding-agent/node_modules/@earendil-works/pi-tui/dist/layout.js#L275-L306)) |

Kiki's global one-row header remains in prompt mode while the tree is open. It continues to advertise `Enter submit`, editor arrows, and `Esc cancel`; the picker itself adds only `Session tree` and `Filter:` ([header](../../src/tui.rs#L2085-L2115), [picker](../../src/tui/picker.rs#L205-L264)). Pi's selector instead renders tree-specific, current-keybinding-aware help for movement, paging, branch folding, copying, labels, and filters ([source](../../../../../../../opt/homebrew/lib/node_modules/@earendil-works/pi-coding-agent/dist/modes/interactive/components/tree-selector.js#L1033-L1116)).

Pi also renders `(selected/total)` and active filter labels below the visible rows ([source](../../../../../../../opt/homebrew/lib/node_modules/@earendil-works/pi-coding-agent/dist/modes/interactive/components/tree-selector.js#L509-L610)). Kiki maintains scroll and page size internally but renders neither a count nor a scrollbar ([source](../../src/tui/picker.rs#L217-L264)).

### Tree projection and row density

Kiki's catalogue is a domain projection rather than a raw persistence-entry list. Its row vocabulary is limited to session start, user request, Python execution with optional commentary and aggregate tool outcomes, final Kiki response, cancellation, and failure ([source](../../src/tui/tree.rs#L302-L355)). This makes one Kiki execution step one tree row even when that step used multiple tools.

Pi renders a broader set of persisted entry types. User, assistant, tool-result, shell, custom, compaction, branch-summary, model-change, thinking-level, label, and session-info entries each have display rules ([source](../../../../../../../opt/homebrew/lib/node_modules/@earendil-works/pi-coding-agent/dist/modes/interactive/components/tree-selector.js#L612-L699)). Its default filter hides bookkeeping entries and tool-call-only assistant entries, while tool results remain visible until the user selects the `no-tools` mode ([source](../../../../../../../opt/homebrew/lib/node_modules/@earendil-works/pi-coding-agent/dist/modes/interactive/components/tree-selector.js#L233-L292)).

Both implementations keep single-child chains visually flat and increase indentation around forks ([Kiki source](../../src/tui/tree.rs#L262-L299), [Pi source](../../../../../../../opt/homebrew/lib/node_modules/@earendil-works/pi-coding-agent/dist/modes/interactive/components/tree-selector.js#L192-L228)). They differ in two structural choices:

1. Kiki sorts siblings by durable revision and entry ID, producing stable chronological preorder ([source](../../src/tui/tree.rs#L98-L129)). Pi moves the child subtree containing the current leaf before its siblings at every fork ([source](../../../../../../../opt/homebrew/lib/node_modules/@earendil-works/pi-coding-agent/dist/modes/interactive/components/tree-selector.js#L124-L230)).
2. Kiki retains the durable fork depth while filtering and shows ancestors as context ([source](../../src/tui/tree.rs#L158-L198)). Pi removes non-matching entries and recalculates a new visible tree by attaching each remaining row to its nearest visible ancestor ([source](../../../../../../../opt/homebrew/lib/node_modules/@earendil-works/pi-coding-agent/dist/modes/interactive/components/tree-selector.js#L304-L430)).

Kiki bounds connector indentation to six ancestor columns and inserts `…` beyond that point ([source](../../src/tui/tree.rs#L17-L28, ../../src/tui/tree.rs#L210-L248)). Pi does not cap tree depth; it keeps a two-column selection gutter fixed and pans all visible row bodies horizontally when the selected row's content anchor would otherwise be obscured ([source](../../../../../../../opt/homebrew/lib/node_modules/@earendil-works/pi-coding-agent/dist/modes/interactive/components/tree-selector.js#L5-L37)).

### Visual hierarchy and theming

#### Kiki

Kiki's tree derives from a fixed, dark-oriented palette:

- ordinary text: RGB `212,212,212`
- muted alternatives: RGB `128,128,128`
- active ancestry: terminal cyan
- current leaf: `● ` plus ordinary text
- focused row: reverse-video modifier
- terminal background: reset/default

The palette and tree style are defined in [`src/transcript.rs`](../../src/transcript.rs#L366-L382) and [`src/tui/tree.rs`](../../src/tui/tree.rs#L200-L259). The entire row receives one base style, so role names, connectors, tool counts, success, and failure do not have independent colours. The selection modifier is applied later by the generic picker ([source](../../src/tui/picker.rs#L249-L260)).

Kiki has no theme setting or theme resource format. Its user and project configuration covers [model and thinking](../../README.md#model-and-thinking-settings) and [subagents](../../README.md#subagents).

#### Pi

Pi styles independent row segments:

- `› ` selection cursor: `accent`
- active-path `• `: `accent`
- selected cells: `selectedBg`
- connectors and metadata: `dim`
- user prefix: `accent`
- assistant prefix: `success`
- tool result: `muted`
- errors: `error`
- labels and branch summaries: `warning`
- compactions: `borderAccent`

The row composition is implemented in [`tree-selector.js`](../../../../../../../opt/homebrew/lib/node_modules/@earendil-works/pi-coding-agent/dist/modes/interactive/components/tree-selector.js#L530-L699). Selection preserves semantic foreground colours, adds bold content, and places a background behind the rendered gutter and body.

Pi ships dark and light palettes. The dark palette uses text `#d4d4d4`, accent `#8abeb7`, muted `#808080`, dim `#666666`, and selected background `#3a3a4a`; the light palette uses text `#1f2328`, accent `#5a8080`, muted `#6c6c6c`, dim `#767676`, and selected background `#d0d0e0` ([dark theme](../../../../../../../opt/homebrew/lib/node_modules/@earendil-works/pi-coding-agent/dist/modes/interactive/theme/dark.json), [light theme](../../../../../../../opt/homebrew/lib/node_modules/@earendil-works/pi-coding-agent/dist/modes/interactive/theme/light.json)). It detects terminal appearance for the initial built-in theme, accepts custom JSON themes, applies theme changes immediately, and hot-reloads the active custom theme ([themes documentation](../../../../../../../opt/homebrew/lib/node_modules/@earendil-works/pi-coding-agent/docs/themes.md)).

The complete Pi theme contract contains core, backgrounds, content, Markdown, diff, syntax, thinking-level, and shell-mode tokens. The tree uses a subset of that wider contract rather than defining its own palette ([theme tokens](../../../../../../../opt/homebrew/lib/node_modules/@earendil-works/pi-coding-agent/docs/themes.md#color-tokens)).

### Selection, current position, and active ancestry

Kiki represents three states as follows:

- current durable point: `● ` on one row
- selected row: reverse video
- active ancestry excluding the current point: cyan text

Alternative branches are grey ([source](../../src/tui/tree.rs#L200-L259)).

Pi uses an accent `› ` for selection, an accent `• ` on every visible active-path row, and a selected background. The current leaf has no distinct glyph after selection moves elsewhere ([source](../../../../../../../opt/homebrew/lib/node_modules/@earendil-works/pi-coding-agent/dist/modes/interactive/components/tree-selector.js#L530-L610)).

Kiki initially focuses the active value through its generic picker ([source](../../src/tui/tree.rs#L138-L145), [picker source](../../src/tui/picker.rs#L266-L271)). Pi initially focuses the current leaf, but its active-first subtree ordering also moves the current branch towards the beginning of every fork ([source](../../../../../../../opt/homebrew/lib/node_modules/@earendil-works/pi-coding-agent/dist/modes/interactive/components/tree-selector.js#L59-L70, ../../../../../../../opt/homebrew/lib/node_modules/@earendil-works/pi-coding-agent/dist/modes/interactive/components/tree-selector.js#L120-L230)).

### Search, filtering, and scrolling

| Behaviour | Kiki | Pi 0.85.1 |
|---|---|---|
| Text matching | One lowercased substring over complete normalised row text ([source](../../src/tui/picker.rs#L132-L138, ../../src/tui/picker.rs#L436-L438)) | Lowercased whitespace tokens, all of which must occur in the searchable node text ([source](../../../../../../../opt/homebrew/lib/node_modules/@earendil-works/pi-coding-agent/dist/modes/interactive/components/tree-selector.js#L233-L292)) |
| Search source | Complete Kiki row text before display truncation ([source](../../src/tui/tree.rs#L75-L95, ../../src/tui/tree.rs#L176-L198)) | Labels, roles, message text, commands, summaries, and metadata; extracted message text is capped at 200 characters ([source](../../../../../../../opt/homebrew/lib/node_modules/@earendil-works/pi-coding-agent/dist/modes/interactive/components/tree-selector.js#L432-L487, ../../../../../../../opt/homebrew/lib/node_modules/@earendil-works/pi-coding-agent/dist/modes/interactive/components/tree-selector.js#L720-L734)) |
| Ancestors | Retained as visible, non-focusable context rows ([source](../../src/tui/tree.rs#L158-L198), [picker source](../../src/tui/picker.rs#L372-L410)) | Non-matches are removed; matching descendants reconnect to the nearest visible matching ancestor or become visible roots ([source](../../../../../../../opt/homebrew/lib/node_modules/@earendil-works/pi-coding-agent/dist/modes/interactive/components/tree-selector.js#L304-L430)) |
| Kind filters | None | `default`, `no-tools`, `user-only`, `labeled-only`, and `all` ([source](../../../../../../../opt/homebrew/lib/node_modules/@earendil-works/pi-coding-agent/dist/modes/interactive/components/tree-selector.js#L254-L283)) |
| Escape with query | Closes the picker ([source](../../src/tui/picker.rs#L149-L188)) | Clears query and folding first; a later Escape closes ([source](../../../../../../../opt/homebrew/lib/node_modules/@earendil-works/pi-coding-agent/dist/modes/interactive/components/tree-selector.js#L879-L887)) |
| Movement | Up/down clamp; page up/down use visible direct-match count; Home/End jump to boundaries ([source](../../src/tui/picker.rs#L176-L185, ../../src/tui/picker.rs#L324-L358)) | Up/down wrap; left/right or page keys move one list page; branch keys fold or jump by segment ([source](../../../../../../../opt/homebrew/lib/node_modules/@earendil-works/pi-coding-agent/dist/modes/interactive/components/tree-selector.js#L834-L1009)) |
| Position feedback | None | `(selected/total)` plus filter and label-time flags ([source](../../../../../../../opt/homebrew/lib/node_modules/@earendil-works/pi-coding-agent/dist/modes/interactive/components/tree-selector.js#L509-L610)) |

Neither implementation provides tree-row mouse selection or wheel scrolling. Kiki routes non-loading mouse events to transcript scrolling/selection even while the tree picker is open ([source](../../src/tui.rs#L358-L380)); Pi's tree component has no mouse handler and fullscreen mode can route unhandled wheel input to the transcript ([tree source](../../../../../../../opt/homebrew/lib/node_modules/@earendil-works/pi-coding-agent/dist/modes/interactive/components/tree-selector.js#L1167-L1244)).

### Opening and navigation semantics

Both products open the tree from a double Escape with an empty, idle editor. Both use a strict interval shorter than 500 ms and silently consume the first qualifying Escape ([Kiki source](../../src/tui.rs#L584-L605), [Pi source](../../../../../../../opt/homebrew/lib/node_modules/@earendil-works/pi-coding-agent/dist/modes/interactive/interactive-mode.js#L2253-L2284)). Kiki also exposes `/tree <initial filter>` and `Ctrl-R` ([source](../../src/tui.rs#L490-L492, ../../src/tui.rs#L813-L840)); Pi exposes a configurable `app.session.tree` action and can configure double Escape as tree, fork, or none ([keybindings](../../../../../../../opt/homebrew/lib/node_modules/@earendil-works/pi-coding-agent/docs/keybindings.md#sessions), [settings](../../../../../../../opt/homebrew/lib/node_modules/@earendil-works/pi-coding-agent/docs/settings.md#ui--display)).

Both treat a selected user request as an edit point: the durable position moves to its parent and the old request is offered to the editor. Kiki preserves any non-empty draft, including whitespace-only text; Pi preserves drafts containing non-whitespace text but treats whitespace-only text as empty and replaces it with the selected request ([Kiki source](../../src/tui.rs#L1534-L1563), [Pi source](../../../../../../../opt/homebrew/lib/node_modules/@earendil-works/pi-coding-agent/dist/modes/interactive/interactive-mode.js#L4435-L4441), [Pi session source](../../../../../../../opt/homebrew/lib/node_modules/@earendil-works/pi-coding-agent/dist/core/agent-session.js#L2574-L2589)). Selecting a non-user point continues from that point without request prefill.

The restored state differs materially:

- Kiki restores model history, the full Python Arena checkpoint, whole-file authorisations, selected task origins, transcript, and subagent monitor. It displays a temporary `Session restored.` card and prevents submission while navigation is pending ([specification](../2026-09-15_session-trees/spec.md), [TUI source](../../src/tui.rs#L1503-L1563)).
- Pi changes the session leaf, rebuilds conversational model context, and optionally appends an LLM-generated summary of the branch being left. Its summary prompt offers no summary, default summary, or custom instructions, and cancellation returns to the same tree selection ([source](../../../../../../../opt/homebrew/lib/node_modules/@earendil-works/pi-coding-agent/dist/modes/interactive/interactive-mode.js#L4357-L4474), [branch summaries](../../../../../../../opt/homebrew/lib/node_modules/@earendil-works/pi-coding-agent/docs/compaction.md#branch-summarization)).

Kiki rejects tree catalogue requests and navigation while foreground or background operations remain active and reports the rejection as a status notice ([runtime](../../src/runtime.rs#L1045-L1095), [TUI](../../src/tui.rs#L1436-L1474)). Pi's tree flow can stop an active response when the user commits to navigation, although double Escape itself first aborts streaming rather than opening the tree ([source](../../../../../../../opt/homebrew/lib/node_modules/@earendil-works/pi-coding-agent/dist/modes/interactive/interactive-mode.js#L2253-L2284, ../../../../../../../opt/homebrew/lib/node_modules/@earendil-works/pi-coding-agent/dist/modes/interactive/interactive-mode.js#L4403-L4407)).

Kiki additionally treats an Escape within two seconds of request submission as correction intent: it cancels first, then restores the request's parent if all foreground and background work settles ([source](../../src/tui.rs#L500-L518), [README](../../README.md#session-tree-navigation)). Pi aborts active work and retains the aborted path for later tree navigation; it has no matching automatic correction gesture in its documented tree workflow ([usage](../../../../../../../opt/homebrew/lib/node_modules/@earendil-works/pi-coding-agent/docs/usage.md#message-queue)).

### Adjacent user-facing functionality

| Capability | Kiki | Pi 0.85.1 |
|---|---|---|
| Copy selected tree entry | Not present | `app.message.copy`, `Ctrl+X` by default ([source](../../../../../../../opt/homebrew/lib/node_modules/@earendil-works/pi-coding-agent/dist/modes/interactive/components/tree-selector.js#L870-L878), [caller](../../../../../../../opt/homebrew/lib/node_modules/@earendil-works/pi-coding-agent/dist/modes/interactive/interactive-mode.js#L4460-L4472)) |
| Fold branch segments | Not present | Fold/unfold or jump between segment boundaries ([source](../../../../../../../opt/homebrew/lib/node_modules/@earendil-works/pi-coding-agent/dist/modes/interactive/components/tree-selector.js#L842-L870, ../../../../../../../opt/homebrew/lib/node_modules/@earendil-works/pi-coding-agent/dist/modes/interactive/components/tree-selector.js#L963-L1009)) |
| Labels/bookmarks | Session name only | Per-entry labels, optional label timestamps, and labeled-only filtering ([sessions documentation](../../../../../../../opt/homebrew/lib/node_modules/@earendil-works/pi-coding-agent/docs/sessions.md#tree-controls)) |
| Branch summaries | Deliberately absent | Optional generated summary when leaving a branch ([documentation](../../../../../../../opt/homebrew/lib/node_modules/@earendil-works/pi-coding-agent/docs/compaction.md#branch-summarization)) |
| Session switching | CLI lifecycle only | In-TUI `/resume`, `/new`, `/fork`, and `/clone` ([documentation](../../../../../../../opt/homebrew/lib/node_modules/@earendil-works/pi-coding-agent/docs/sessions.md#session-commands)) |
| Keybinding configuration | Fixed controls | Namespaced, user-configurable action bindings ([documentation](../../../../../../../opt/homebrew/lib/node_modules/@earendil-works/pi-coding-agent/docs/keybindings.md)) |
| Theme configuration | Fixed palette | Built-in dark/light, custom themes, immediate selection, and hot reload ([documentation](../../../../../../../opt/homebrew/lib/node_modules/@earendil-works/pi-coding-agent/docs/themes.md)) |
| Subagent state | Native branch-scoped retained tasks and monitor | No native subagent orchestration in core ([Kiki specification](../2026-09-15_session-trees/spec.md), [Pi design principles](../../../../../../../opt/homebrew/lib/node_modules/@earendil-works/pi-coding-agent/docs/usage.md#design-principles)) |
| Executable state | Restores Python Arena checkpoints and authorisations | Restores conversational branch context only ([Kiki specification](../2026-09-15_session-trees/spec.md), [Pi session format](../../../../../../../opt/homebrew/lib/node_modules/@earendil-works/pi-coding-agent/docs/session-format.md#context-building)) |

## Conventions & notable decisions

- Kiki's full-width tree is the only picker that replaces the prompt dock. Model and thinking pickers remain centred and width-capped through the generic `popup_area` path ([tree render](../../src/tui.rs#L2148-L2186), [generic picker](../../src/tui/picker.rs#L440-L449)).
- Kiki keeps complete search text separately from width-dependent rendered rows, so terminal resizing changes truncation without changing match semantics ([source](../../src/tui/tree.rs#L75-L95, ../../src/tui/tree.rs#L153-L198)).
- Kiki treats filtered ancestors as context rather than selectable results. The generic picker explicitly distinguishes direct and context roles ([source](../../src/tui/picker.rs#L372-L410)).
- Pi treats the selected row, current leaf, and active path as separate concepts. Its selected background and active-path marker can coexist on the same row ([source](../../../../../../../opt/homebrew/lib/node_modules/@earendil-works/pi-coding-agent/dist/modes/interactive/components/tree-selector.js#L530-L610)).
- Pi's documentation screenshot shows only the list presentation, not the installed selector's complete title/help/search/border chrome ([screenshot](../../../../../../../opt/homebrew/lib/node_modules/@earendil-works/pi-coding-agent/docs/images/tree-view.png), [installed component](../../../../../../../opt/homebrew/lib/node_modules/@earendil-works/pi-coding-agent/dist/modes/interactive/components/tree-selector.js#L1167-L1208)).
- Features omitted from Kiki Phase 2 include branch names, labels, summaries, folding, bookmarks, merging, pruning, export, cloning, keyboard configurability, in-client session switching, compaction, and migration infrastructure ([plan](../2026-09-15_session-trees/phase-2_plan.md)).

## Key references

### Kiki

- [`src/tui.rs:444`](../../src/tui.rs#L444) — input precedence, tree entry points, settled double Escape, catalogue loading, and navigation outcomes.
- [`src/tui.rs:2085`](../../src/tui.rs#L2085) — global header and docked tree geometry.
- [`src/tui/picker.rs:149`](../../src/tui/picker.rs#L149) — picker key handling, focus movement, filter display, selection, and scrolling.
- [`src/tui/tree.rs:64`](../../src/tui/tree.rs#L64) — catalogue projection, stable ordering, filtering, connectors, styling, and row vocabulary.
- [`src/transcript.rs:366`](../../src/transcript.rs#L366) — fixed colour palette used by the tree.
- [Persisted session-tree specification](../2026-09-15_session-trees/spec.md) — state-restoration and safety semantics.
- [Phase 2 plan](../2026-09-15_session-trees/phase-2_plan.md) — approved interaction scope and explicit exclusions.

### Pi 0.85.1

- `/opt/homebrew/lib/node_modules/@earendil-works/pi-coding-agent/dist/modes/interactive/components/tree-selector.js:1` — complete current tree selector.
- `/opt/homebrew/lib/node_modules/@earendil-works/pi-coding-agent/dist/modes/interactive/interactive-mode.js:2253` — double Escape and tree selector lifecycle.
- `/opt/homebrew/lib/node_modules/@earendil-works/pi-coding-agent/dist/modes/interactive/interactive-mode.js:4357` — navigation, copying, labels, and branch-summary flow.
- `/opt/homebrew/lib/node_modules/@earendil-works/pi-coding-agent/dist/modes/interactive/theme/dark.json` — built-in dark palette.
- `/opt/homebrew/lib/node_modules/@earendil-works/pi-coding-agent/dist/modes/interactive/theme/light.json` — built-in light palette.
- [Pi sessions](../../../../../../../opt/homebrew/lib/node_modules/@earendil-works/pi-coding-agent/docs/sessions.md) — documented session-tree workflow.
- [Pi keybindings](../../../../../../../opt/homebrew/lib/node_modules/@earendil-works/pi-coding-agent/docs/keybindings.md) — tree and session actions.
- [Pi themes](../../../../../../../opt/homebrew/lib/node_modules/@earendil-works/pi-coding-agent/docs/themes.md) — theme format, tokens, selection, and hot reload.
