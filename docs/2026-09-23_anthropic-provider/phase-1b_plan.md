# Plan: Anthropic provider, Phase 1b: model change cards

## Source
- Spec: `docs/2026-09-23_anthropic-provider/spec.md`, Phase 1b (Proposed approach › Phase 1b: model change cards)
- Research: `docs/research/2026-09-23_pi-model-change-entries.md`

## Acceptance criteria
- Each new run's `user_request` entry stores the run's selection. `Record::UserRequest` and the restored user-request record carry it as `Option<ModelSelection>`. A payload written before this change decodes with `None`, and the session format stays at version 2.
- `PersistedCode.selection` no longer exists. Stored execution and terminal payloads that still contain a `selection` field decode.
- A history card `Model selected: <model id> · <thinking>` sits directly above a run's request card when that run's selection and the previous run's selection on the branch are both known and differ. Otherwise there is no card: not for the first run, not across an unknown selection, not for an unchanged selection.
- The pending card has the same text and is attached to the prompt area (Task 5), prefixed `[pending] ` until the change applies. It shows when the branch's last run selection is known and differs from the footer's current selection, including during a Shift-Tab preview. It disappears when the selection is switched back. After the next run's request record arrives, it is replaced by that run's history card.
- Cards use the footer's muted grey text on the page background (no card background) with the standard card padding. They behave the same live, after restore and after `/tree` navigation.
- Cards are never session entries, never appear in `/tree`, and never reach model context.

## Overall validation
- `cargo fmt --all -- --check`: no diff.
- `env -u OPENAI_API_KEY MONTY_BIN=/tmp/kiki-monty/bin/monty cargo test --all-targets -- --test-threads=1`: all pass.
- `cargo clippy --all-targets -- -D warnings`: clean.
- `rg -n "selection" src/session/codec.rs | rg -i "code\.selection|StoredCode"` and `sed -n '/pub(crate) struct PersistedCode/,/^}/p' src/session.rs | rg selection`: no matches.
- Manual: `just run`, complete one request, then `/model openai/gpt-5.6-terra`. A `Model selected: openai/gpt-5.6-terra · …` card appears just above the prompt (briefly `[pending] ` until the runtime accepts). Scroll history up: the card stays put. Switch back with `/model openai/gpt-5.6-sol` and it disappears. Switch to terra again and submit: the card sits between the first run and the second request. Quit, `just continue`: the card is still there. Shift-Tab after that run shows `[pending] Model selected: … · <level>` above the prompt for two seconds, then the prefix goes.

## Planning context
- Request persistence:
  - `SessionWriter::commit_request` (`src/session.rs:1155`) calls the private `commit_request` (`src/session.rs:1439`), which encodes the payload via `codec::encode_request` (`src/session/codec.rs:461`).
  - `Agent::run_persisted` calls it (`src/agent.rs:~706`) and already has the run's selection since Phase 1.
- Codec:
  - `StoredPayload` is `deny_unknown_fields` (`src/session/codec.rs:70-88`). `UserRequest { request }` needs `#[serde(default)] selection: Option<StoredSelection>`.
  - Decode sites: `decode_request` (`:468`), `decode_entry` (`:483`) and `decoded_request` (`:539`).
  - `encode_selection` and `decode_selection` exist (`:1032-1045`).
  - `StoredCode.selection` (`:176`) has no `deny_unknown_fields` on `StoredCode` (`:170`), so removing the field keeps old payloads decodable.
  - Existing codec tests set it at `:1886` and `:2028`.
- Restore: `RestoredRecordKind::UserRequest { request }` is built at `src/session.rs:3491` and mapped to `Record::UserRequest { request }` at `src/runtime.rs:689`.
- Live: `start_run` captures `self.selection` into the worker task (`src/runtime.rs:1566`). The worker forwards `PersistedAgentEvent::RequestCommitted` as `WorkerMessage::RequestCommitted` (`:1576-1581`), and the controller appends `Record::UserRequest { request }` when handling it (`:1845-1870`).
- Other `Record::UserRequest` constructors are tests only: synthetic root records in `src/tui/subagents.rs:981-1850` monitor tests, plus `src/transcript.rs` and `src/openai.rs`. Child runs use `run_with_progress` (`src/subagents.rs:1416`) and produce no request records.
- Request text readers stay text-only:
  - `decode_request` feeds the sessions-list title (`src/session.rs:2184-2202`).
  - `decoded_request` feeds the catalogue, model context and restore (`src/session.rs:2434-2443,2753-2757,3680-3686`; `src/session/codec.rs:538-543`).
- The legacy request tagging is `{"UserRequest":{"request":"…"}}` (`tests/sessions_cli.rs:69`).
- Branch replacement:
  - `/tree` navigation and correction rebuild the transcript (`src/tui.rs:1568-1579,1698-1712`).
  - Early correction can restore the request's parent (`src/runtime.rs:2050-2107`).
- Copy and text selection:
  - Copy extracts text from rendered blocks (`src/transcript.rs:2212-2249`).
  - Transcript content changes clear a completed text selection (`src/tui.rs:1587-1608`) via `invalidate_layout_selection` (`src/tui.rs:1339`). `apply_selection` and Shift-Tab (`src/tui.rs:983-1011`) currently don't.
- Transcript:
  - `append_record` (`src/transcript.rs:3172`) pushes the `DisplayedRecord` into `self.records` and appends `record_blocks(&displayed)` to `settled_blocks`.
  - Restore and navigation replay records through `apply_initial_state`, then `append_record` (`:2364-2374`). There is no second production rebuild path; the loop at `:2980-2989` is the test-only `projection`.
  - `record_blocks` maps `UserRequest` to `TranscriptBlock::User` (`:3563-3570`).
  - `layout_with_footer` receives `FooterContext { selection, .. }` (`:2861`; built from `settings_selection` at `src/tui.rs:1111-1118`).
  - Live and pending-subagent sources are appended after settled blocks as fresh blocks (`:2940-2950`).
  - Restore and navigation rebuild through `restored_replacement` / `apply_initial_state` (`:2348-2365`).
- Styling:
  - `muted_page_style()` is MUTED grey on the page background (`src/transcript.rs:424`), which the footer uses (`FooterField::muted`, `:1647`).
  - Standard card padding is `RenderBlock::card(text, style, width, 1, 1)`, like `SessionRestored` (`:3447-3456`).
- Conventions:
  - Tests live in module `#[cfg(test)] mod tests`.
  - Transcript tests assert on block kinds and rendered text.
  - Commits are signed off.

## Tasks

### Task 1: Remove the unused per-step persisted selection
**Changes** — Delete `PersistedCode.selection` (`src/session.rs:223`) and `StoredCode.selection` (`src/session/codec.rs:176`), with their encode and decode lines. The agent no longer sets it (`src/agent.rs:~1264`). `PendingCode.selection` stays, because it feeds the arena invocation context.

**Tests**
- Red first, in `src/session/codec.rs`: newly encoded `execution` and `terminal` payloads have no `selection` key in their `code` object. This currently fails, because `selection: null` or a value is written.
- Legacy `execution` and `terminal` payloads whose `code` contains `"selection": {...}` still decode.
- Update the codec and session tests that construct `PersistedCode` or `StoredCode`.

**Files** — Modify: `src/session.rs`, `src/session/codec.rs`, `src/agent.rs`, plus any test constructors the compiler flags (e.g. `src/runtime.rs`)

**Verification**
- The overall test and clippy commands: all pass and clean.
- The two `selection` searches from Overall validation: no matches.

### Task 2: Save each run's selection on its request entry
**Changes**
- `SessionWriter::commit_request(request, selection: ModelSelection)`. `encode_request` writes `selection`. `StoredPayload::UserRequest` gains `#[serde(default)] selection: Option<StoredSelection>`.
- Keep the text accessors (`decode_request`, `decoded_request`) unchanged, so the title, catalogue and model context never see the selection. Add a separate selection accessor that the restore path uses.
- `RestoredRecordKind::UserRequest` and `Record::UserRequest` gain `selection: Option<ModelSelection>`.
- `Agent::run_persisted` passes its run selection.
- The runtime fills the live `Record::UserRequest` from the selection `start_run` already captured in the worker task. Add it to the existing `WorkerMessage::RequestCommitted`; it needs no new run state and no `PersistedAgentEvent` change.
- Restore maps the decoded selection through.
- Child histories (`src/tui/subagents.rs`) and test constructors use `None`, unless a test needs a selection.
- Model history building ignores the field.

**Tests**
- Red first, in `src/session/codec.rs`: a raw payload `{"UserRequest":{"request":"hi","selection":{...}}}` decodes with that selection. This currently fails on `deny_unknown_fields`.
- A legacy payload `{"UserRequest":{"request":"hi"}}` decodes with `None`, and an encoded request with a selection round-trips.
- `src/session.rs`: after committing requests with two different selections and reopening, the restored user-request records carry them in order, and `model_history()` contains only the request text.
- `src/runtime.rs`: a submitted run's `RecordAppended` user request carries the selection captured at submit, even when `ApplySettings` lands during the run. Reuse the Phase 1 gated-model pattern (`settings_accepted_during_a_run_apply_to_the_next_run`).

**Files** — Modify: `src/session.rs`, `src/session/codec.rs`, `src/agent.rs`, `src/runtime.rs`, `src/tui/subagents.rs`, `src/transcript.rs` (constructors only), `src/openai.rs` (test patterns only)

**Verification**
- The overall test and clippy commands: all pass and clean.

### Task 3: Show history model change cards between runs
**Changes**
- New `TranscriptBlock::ModelChange { selection }` and matching normalised block. It renders `Model selected: {model id} · {thinking}` with `muted_page_style()` for both text and card style, and `RenderBlock::card(.., width, 1, 1)`.
- `record_blocks` takes the previous run's selection on the branch, and emits the card before the `User` block when both are known and differ.
- `append_record` derives the previous selection by searching `self.records` backwards for the nearest `UserRequest`. If that request's selection is `None`, the search stops there rather than skipping it, so there's no card. There is no stored card state. The test-only `projection` applies the same rule.
- Check any exhaustive matches over block kinds, such as block text for copy and test labels, and give the card its plain text.
- `README.md`: one line in the transcript/controls section describing the card.

**Tests** (red first, `src/transcript.rs`)
- Two requests with different known selections: `ModelChange` sits directly before the second `User` block with the exact text. The rendered cell style is the MUTED foreground on the page background.
- There's no card for the first request, the same selection, or a `None` on either side.
- A compaction record between the runs doesn't hide the change.
- `restored_replacement` with the same timeline produces the same blocks as the live appends.
- Copying a rendered region that covers the card yields `Model selected: … · …` (`src/transcript.rs:2212-2249`). The pending card renders the same block, so this one copy test covers both.

**Files** — Modify: `src/transcript.rs`, `README.md`

**Verification**
- The overall test and clippy commands: all pass and clean.

### Task 4: Show the pending model change card
**Changes**
- In `layout_with_cache`, when a `FooterContext` is present, compare its `selection` with the last `UserRequest` selection in `self.records`.
- When that selection is known and differs, append one fresh `ModelChange` block after the live and pending-subagent sources, so it is the last block.
- `layout` and `layout_fresh` (no footer context) show no pending card.
- `src/tui.rs`: when `apply_selection` or Shift-Tab adds, changes or removes the pending card, reuse `invalidate_layout_selection` to clear a completed history text selection, as transcript content changes already do. An unchanged pending state leaves the selection alone.

**Tests** (red first)
- `src/transcript.rs`:
  - After one request with selection A, `layout_with_footer` with selection B ends with the `ModelChange` card for B.
  - With A there is no card.
  - Before any request, and when the last request's selection is `None`, there is no card.
  - While a live step is active, the card comes after the live blocks.
  - After a second request with B is appended, the pending card is gone and the history card for B sits above the second request.
- `src/tui.rs`:
  - After a run, Shift-Tab (which changes `settings_selection` for the preview) makes the rendered history end with the pending card, and clears a completed text selection.
  - `/tree` navigation between branches whose last runs used different selections: the history and pending cards follow the selected branch, and the footer selection is unchanged. Correction goes through the same `replace_restored_state` path (`src/tui.rs:1650-1712`), so this test covers it.

**Files** — Modify: `src/transcript.rs`, `src/tui.rs`

**Verification**
- The overall test and clippy commands: all pass and clean.
- Manual check from Overall validation.

### Task 5: Attach the pending card to the prompt area
Added after Tasks 1-4 landed, at the user's request: the pending card sat at the end of history and scrolled with it.

**Changes**
- `src/transcript.rs`: `layout_with_cache` no longer appends the pending card. Keep `pending_model_change` as the single rule. Expose a small way for the TUI to build the card with the same style and padding as the history card, with an optional `[pending] ` prefix.
- `src/tui.rs` `render_at`: when `pending_model_change(settings_selection)` is `Some`, reserve the card's rows below the subagent monitor and above the throbber and prompt, shrinking the history viewport, and skip it when there isn't room. The prefix shows while `preview` is set or `pending_settings` holds the current selection.
- Remove `set_settings_selection`: the card no longer changes history layout, and a history viewport size change already clears a text selection (`src/tui.rs:~2503`).
- `README.md`: describe the card as sitting above the prompt, with the `[pending]` prefix.

**Tests** (red first)
- `src/transcript.rs`: `layout_with_footer` never contains the pending card; the Task 4 transcript tests move to the TUI or become this assertion.
- `src/tui.rs`:
  - After a run, `/model` to another selection: the card renders directly above the prompt rule (above the throbber while a run is active, below the subagent monitor when shown), prefixed `[pending] ` until `SettingsAccepted`, then without the prefix.
  - Shift-Tab: `[pending] ` during the preview, gone after the preview is accepted.
  - Scrolling history up leaves the card in place; the history viewport is shorter by the card's height while it shows.
  - Switching back removes it; the next run's request record settles it into history as the history card above that request.
  - `/tree` navigation re-compares against the selected branch (adapt the Task 4 test).

**Files** — Modify: `src/transcript.rs`, `src/tui.rs`, `README.md`

**Verification**
- The overall test and clippy commands: all pass and clean.
- Manual check from Overall validation.

