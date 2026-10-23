# Spec: Call lines with subjects and durations

Phase 3 of [Calm transcript](../2026-10-15_calm-transcript/spec.md).

## Context

After phases 1 and 2, a code step lists one line per tool call, but a settled call only says what kind of call it was and how it ended: `✓ read · 1832 bytes`, `✓ run · exit 0`. The command of a `run` call is visible only while it is pending. A loop such as `for p in paths: read(p)` leaves a column of identical lines, and nothing says which call was slow.

## Goals

- Every call line says what the call acted on: a path, a pattern, a command, a skill or a subagent.
- Every settled call line says how long it took.
- A call line reads the same while pending and after it settles, apart from the glyph, result and duration.
- Sessions saved before this change still restore and render.

## Non-goals

- A `Took` or `Elapsed` total for a code step or an agent turn.
- Storing raw tool arguments. File contents, edit text and subagent prompts never enter the call record.
- Changing the `Status:` header line or `tool_call_label`.
- Per-call cost (Kiki tools have none).
- Showing call errors under the call line.

## Key discoveries

- Display calls hold only `{tool, summary}` (`src/arena.rs:280-296`), admitted in `ToolTraceState::admit` and settled in `ToolTraceState::settle` (`src/arena.rs:315-340`). The arena admits a call with its converted arguments in hand (`src/arena.rs:2013-2028`) and settles it when the host result arrives (`src/arena.rs:2276-2287`).
- The live `$ command` comes from `run_command_argument` (`src/arena.rs:2342-2354`) via `RunPreview.command` (`src/arena.rs:210`), rendered by `tool_call_line` (`src/transcript.rs:663-691`).
- Host functions take positional arguments only (`src/arena.rs:2004-2007`): `read(path)`, `write(path, content)`, `edit(path, old, new)`, `list_dir(path?)`, `glob(pattern, path?)`, `grep(pattern, path?, file_glob?)`, `run(command, cwd?)`, `load_skill(name)` (`src/tools.rs:453-605`). Subagent tools take an id, apart from `start_subagent`, whose id is only known from its result.
- The arena already has an injectable clock, `ArenaClock` (`src/arena.rs:628-645`).
- Stored calls are `StoredToolCall {tool, summary}` without `deny_unknown_fields` (`src/session/codec.rs:263-267`). Other stored types already use `#[serde(default, skip_serializing_if = "Option::is_none")]` for optional fields (`src/session/codec.rs:306-319`). `FORMAT_VERSION` is checked for exact equality (`src/session.rs:36, 2381`), so a bump would make every older session unreadable.
- Read sizes render as raw counts with `count_label` (`src/tools.rs:136-139`).

## Key decisions & tradeoffs

- **One subject per tool, not raw arguments.** A short, readable string chosen per tool. Rejected: pi's truncated JSON (`{"path":"src/main.rs"}`), which is noisy and would store file contents for `write` and `edit`.
- **Aligned tool name column.** Names are padded to the widest name among the step's shown calls, so subjects line up. Rejected: inline names (ragged in long lists) and a full table with aligned results (shortens subjects early and needs a narrow-width fallback).
- **Duration on every settled call, always dim.** Consistent rows beat hiding fast calls. Rejected: slow-only durations, and durations only when expanded.
- **No step total.** Per-call durations carry the useful timing; a `Took` line would add a row to every step for little gain.
- **Optional stored fields, no format bump.** New sessions add `subject` and `duration_ms`; old sessions decode without them and render as they do today. Older binaries ignore the unknown fields.

## Proposed approach

### Call line

```
 ✓ read            src/transcript.rs · 84 KB  4ms
 ✓ read            src/tui.rs · 12 KB  3ms
 ✓ grep            "fn tool_call" src/ · 12 matches  31ms
 ✓ edit            src/transcript.rs · applied  2ms
 ✓ start_subagent  subagent:c1  1ms
 ✗ run             just test 2>&1 | grep -E 'test result|F… · exit 101  44.9s
```

Pending, with the run stdout tail unchanged:

```
 ✓ glob  "src/*.rs" · 12 entries  2ms
 … run   cargo test
   running 42 tests
   test a ... ok
```

Each row is `glyph`, space, tool name in `accent` padded to the column width plus two spaces, subject in text colour, ` · result` muted, two spaces, duration dim. A pending call shows no result or duration. A call without a subject, from an older session, renders as today: `✓ read · 84 KB`, with no padding.

Collapsed, a row never wraps: the subject is truncated from the end with `…` so result and duration stay visible. If even an empty subject does not fit, the row is cut from the end like any other line. Expanded (Ctrl-O), the row wraps and shows the full subject.

### Subjects

| Tool | Subject |
|---|---|
| `read`, `write`, `edit` | path |
| `list_dir` | path, or `.` when omitted |
| `glob` | `"pattern"`, then ` path` when given |
| `grep` | `"pattern"`, then ` path` and ` file_glob` when given |
| `run` | command; ` (in cwd)` when `cwd` is given |
| `load_skill` | skill name |
| `get_subagent`, `refresh_subagent`, `wait_subagent`, `cancel_subagent` | `subagent:<id>` |
| `start_subagent` | none while pending; `subagent:<id>` once settled |

The subject is built when the call is admitted (and for `start_subagent`, when it settles), normalised with `normalise_terminal_text`, newlines collapsed to spaces, and bounded to 1 KiB. Arguments with unexpected shapes give no subject. Subagent summaries stop repeating the id: a subagent call's result is dropped, since the subject already says `subagent:<id>`.

### Results and durations

- Read sizes become `512 B`, `1.2 KB`, `84 KB`, `3.4 MB` (base 1000, one decimal below 10). The same formatter is used wherever a read byte count is shown.
- Durations are measured with `ArenaClock` from admission to settlement and shown as `4ms` below one second, `44.9s` below one minute, then `2m05s`. Calls that never settle (cancelled or interrupted steps) have no duration.

### Data

- `arena::ToolCall` gains `subject: Option<String>` and `duration: Option<Duration>`. `ToolTraceState::admit` takes the subject; `settle` takes the duration (and the `start_subagent` subject).
- `RunPreview.command` is deleted; the pending row reads the subject instead.
- `StoredToolCall` gains `subject: Option<String>` and `duration_ms: Option<u64>`, both `#[serde(default, skip_serializing_if = "Option::is_none")]`. Decoding re-applies the 1 KiB bound.

## Risks / unknowns

- Long paths dominate the row; front truncation would keep file names visible but is inconsistent with commands. Start with end truncation and revisit if paths read badly.
- Wide padding from `start_subagent`: the 14-character name widens every row in steps that spawn. Accepted, since padding is per step.
- Many transcript tests assert exact call rows; expectations change in the same commit as the behaviour.

## Dependencies

- Phases 1 and 2 of Calm transcript (merged).

## Phase outline

1. **Record subjects and durations.** Display `ToolCall` and the session codec carry both; old sessions still decode.
2. **Render call lines.** Aligned name column, subject, humanised sizes, dim durations, truncation and expansion; drop `RunPreview.command`.
