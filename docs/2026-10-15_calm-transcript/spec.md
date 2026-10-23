# Spec: Calm transcript

## Context

Kiki's transcript is visually busy. A code step draws up to three levels of box drawing: an outer rounded card, a titled rounded frame per section (`code`, `tool calls`, `stdout`, ...), and a `╭─ $ cmd │ … ╰─` preview inside `tool calls`. Transcript blocks are stacked with no gap, so cards touch. Tool call rows carry no state colour, and the transcript ignores the theme's `success`, `error`, `warning` and `accent` keys.

Subagents appear in up to four places for one spawn: a row in the code step, a grey Spawned card, a terminal card, and the docked subagent monitor. The monitor and the cards word the same subagent differently, and every terminal card is green, including failures.

Pi's interactive TUI renders the same ideas more quietly. Kiki and pi already agree on the order of a code step: input, tool calls, output. This spec adopts pi's visual language while keeping the parts of Kiki that pi lacks, such as the live subagent monitor.

## Goals

- A code step reads as one tinted block with no borders, its state carried by the background.
- Tool calls read as one line each, with a coloured status glyph.
- One blank row separates every transcript block.
- Collapsed views say what was cut and how to see it.
- One subagent has one live presence (the monitor) and one history entry (its outcome), with the same glyphs and wording in both.
- Failure is never green.

## Non-goals

- Persisting tool call arguments or durations; see Phase 3.
- A `Took 3.2s` line for code steps; it needs step timing that display history does not hold today.
- A focusable or scrollable subagent view.
- Renaming the Arena domain term. "Code step" is the name for the presentation unit only.
- Changing reasoning, user message or assistant message styling beyond the shared gap.
- New theme keys.

## Key discoveries

- Pi draws no borders in its transcript. `Box` paints a full-width background with padding (`.research/pi/source/packages/tui/src/components/box.ts:106-162`). Every tool row gets a spacer row above it and one padded box whose background is `toolPendingBg`, `toolSuccessBg` or `toolErrorBg` (`.research/pi/source/packages/coding-agent/src/modes/interactive/components/tool-execution.ts:90-91, 270-300`).
- Pi's codemode renderer shows highlighted code (first 10 lines collapsed), a spacer, one line per nested call (`…`/`✓`/`✗`/`⊘` glyph, `toolTitle` name, muted args, dim duration; last 8 collapsed), a spacer, output (5 wrapped lines collapsed), then `Elapsed`/`Took` (`.research/pi/source/packages/coding-agent/src/extensions/codemode/renderer.ts:18-160`). Each cut has a `… N more, ctrl+o to expand` hint.
- Pi has no built-in subagent tool. Its example extension (`.research/pi/source/packages/coding-agent/examples/extensions/subagent/index.ts:765-1032`) renders child tool calls as `→` lines inside the tool box and has no display outside the transcript, so it is not a model for Kiki's background subagents.
- Kiki's code step layout lives in one place: `ArenaRenderBlock::new` (`src/transcript.rs:1377-1485`), with `ArenaSection` and `draw_rounded_frame` (`src/transcript.rs:1173-1323, 1543-1606`) used only by it. Per-kind padding for every other block is in `build_render_block` (`src/transcript.rs:3722-3900`). Blocks are stacked back to back in `TranscriptLayout::render_with_selection` (`src/transcript.rs:2381-2420`).
- The arena's heading and status rows (`running code`, latest tool label) repeat the `Status:` header line (`src/tui.rs:2975-2981`) and the tool call list (`src/transcript.rs:4178-4183`).
- Display tool calls hold only the tool and a summary, not arguments or timing (`src/arena.rs:280-296`). Run previews (command plus stdout tail) exist only for the live step and the step it settles into; restored sessions have none (`src/transcript.rs:3522-3529`).
- The run preview stdout is the subprocess output tail, not Python's stdout, so it is not strictly a duplicate of the `stdout` section, though agents often print it.
- Subagent cards: `spawned_subagent_block`/`finished_subagent_block` (`src/transcript.rs:701-737`), text in `subagent_spawned_text`/`subagent_finished_text` (`src/transcript.rs:898-973`). The finished card always uses `tool_success_bg` (`src/transcript.rs:3767`). The monitor is `src/tui/subagents.rs`, rows built in `render_node` and helpers (`src/tui/subagents.rs:685-927`). Step and elapsed formatting is duplicated between the two (`src/transcript.rs:744-761`, `src/tui/subagents.rs:861-876`).

## Key decisions & tradeoffs

- **No borders in the transcript.** Background tint plus one row of vertical padding replaces every frame. Rejected: keeping only the outer frame (still two visual systems; pi shows tint alone is enough).
- **Uniform one-row gap between blocks**, owned by layout rather than per-block padding. Without it, frameless tinted blocks of the same colour merge. Assistant blocks lose their own vertical padding and the accounting block loses its leading blank line, so spacing stays at one row. Rejected: a spacer on arena blocks only (cards elsewhere still touch).
- **Section labels only where meaning would be lost.** Code is recognisable by highlighting and stdout is the default output, so neither gets a label. `stderr`, `result`, `error`, `type check`, `failure` and `warning` get a dim one-word label line. `error`, `type check` and `failure` text uses `theme.error`; `warning` and `stderr` use `theme.warning`. Rejected: pi's no-labels-at-all (Kiki distinguishes more output kinds than pi does).
- **Status glyphs from the summary.** `…` (`warning`) pending; `✓` (`success`) for counts, mutations and exit 0; `✗` (`error`) for failed, non-zero exit and signal. Tool name in `accent`, summary muted. A run call with a preview shows `$ <command>` after the name.
- **Run stdout tail only while the call is pending.** Once a call settles, its preview tail disappears and the call collapses to one line. Rejected: keeping the tail after settlement (it is what makes steps tall, and restored sessions never had it).
- **No heading or status rows in a live step.** The header line already shows activity. A failed or cancelled step keeps its diagnostic as the first line, in `theme.error`. `execution: started|not started` and `display gap` stay as dim lines because they carry information found nowhere else.
- **Collapsed limits:** code shows the first 10 lines when settled and the last 10 while decoding; tool calls show the last 8; output shows the last 8 lines across all sections. Each cut shows a dim `… N more lines · Ctrl-O to expand` (or `earlier calls`) at the cut. Rejected: pi's 5 output lines (Kiki output is what the model sees, so it deserves more room).
- **Subagents live in the monitor and finish in history.** Remove the Spawned card; the spawn call line in the code step already records it. Keep one outcome entry, tinted by outcome. Monitor rows use the same glyphs and one shared summary formatter. Rejected: rendering subagents live inside the code step (Kiki subagents outlive the step that started them).

## Proposed approach

### Code step

```
                                                    ← gap (page background)
 files = tools.glob("src/*.rs")                     ← tool_success_bg, 1 row padding above
 print(tools.run("cargo test"))

 ✓ glob · 12 entries
 ✓ run $ cargo test · exit 0

 test result: ok. 42 passed

 ↑1200 ↓300 Σ1500 · $0.0123                         ← muted
                                                    ← 1 row padding below
```

Live, with a pending run:

```
 files = tools.glob("src/*.rs")                     ← tool_pending_bg
 print(tools.run("cargo test"))

 ✓ glob · 12 entries
 … run $ cargo test
   running 42 tests                                 ← dim, indented tail
   test a ... ok
```

Failed:

```
 failed: interrupted by user                        ← tool_error_bg, error colour
 execution: started                                 ← dim

 x = tools.read("a.txt")

 ✓ read · 12 bytes

 stderr                                             ← dim label
 Traceback ...                                      ← warning colour
```

`ArenaSection`, `RoundedFrame` and `draw_rounded_frame` are deleted. `ArenaRenderBlock` keeps its item list (`Text`, `Spacer`) with spacers between groups and one row of background padding at top and bottom, and keeps its one column of horizontal padding.

### Transcript gap

`TranscriptLayout` inserts one page-background row between consecutive blocks. Selection and scroll maths use the same block offsets, so they follow automatically once offsets include the gap.

### Subagents

```
 ✓ start_subagent · subagent:c1                     ← inside the code step

 ✓ subagent:c1 · gpt-5.6-terra · 3 steps · 00:47 · $0.02   ← success bg; error bg on failure, timeout
 review the diff and…                               ← dim prompt, one line; full with Ctrl-O
 Found three issues in the diff: …                  ← ≤3 rows, then dim `… Ctrl-O to expand`
```

Cancelled uses `tool_pending_bg` with `⊘`. Failed, timed-out and cancelled headers add the state word (`✗ subagent:c1 failed · …`). The prompt moves from the removed Spawned card into this entry. The monitor shows one row per subagent, with the prompt last when it fits:

```
  … subagent:c1  gpt-5.6-terra  3 steps  00:47  $0.02  review the diff and…
  ✓ subagent:c2  gpt-5.6-sol  1 step  00:12  summarise tests
```

Monitor glyphs move to `…` running, `✓` completed, `✗` failed or timed out, `⊘` cancelled, matching tool calls. Tree structure and the four-entry limit stay.

## Risks / unknowns

- Many TUI and transcript tests assert exact rows; the gap and frame removal will change a lot of expectations. Mitigation: update expectations in the same commit as each behaviour change, never in bulk afterwards.
- Text selection regions for framed section titles go away; selection of code and output text must still work.
- Without frames, a long output directly following the tool calls relies on the spacer row alone. If that proves ambiguous in practice, the dim label can be extended to stdout.

## Dependencies

- None. Phase 3 depends on a session codec change.

## Phase outline

1. **Calm code step.** Uniform block gap; frameless code step with glyph call lines, labels only for non-stdout output, pending-only run tails, collapse hints.
2. **Subagents.** Remove the Spawned card; outcome-tinted finish entry; one-row monitor with shared glyphs and formatter.
3. **Richer call lines.** Call subjects and durations; see [Call lines](../2026-10-23_call-lines/spec.md).
