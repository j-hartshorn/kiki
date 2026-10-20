# Plan: Settings menu and themes — Phase 2: Theme picker and settings menu

> **Approved** — spec review READY, simplification review CLEAN.

## Source
- Spec: `docs/2026-10-14_settings-and-themes/spec.md`, Phase 2.
- Carries the two seams deferred from `docs/2026-10-14_settings-and-themes/phase-1_plan.md`: `SettingsContext::save_theme` (spec Seams §3) and theme-keyed settled transcript rasters (spec Seams §4).

## Acceptance criteria
- `/settings` ("Open settings") and `/theme` ("Choose theme") appear in slash completion in catalogue order and execute only without arguments.
- `/theme` opens a picker with `automatic (currently dark)` or `automatic (currently light)` (using the startup-detected appearance), `dark` and `light`, focused on the active theme setting.
- Moving focus in `/theme` (including via filter changes) immediately re-renders the whole screen, including settled transcript cards, in the focused item's resolved theme without re-querying the terminal.
- Pressing `Enter` or `Ctrl-S` in `/theme` keeps the selected theme for the session and writes `theme = "<setting>"` to `~/.kiki/config.toml` while preserving comments and other keys. A project `theme` setting that overrides the user file is reported (`User default saved; currently overridden by project settings.`), and a save failure reports the existing save error notice while keeping the selected theme in the session.
- Pressing `Esc` in `/theme` restores the theme setting and resolved theme that were active when the theme picker opened, without writing user settings.
- `/settings` opens a single-row picker showing `Theme  <setting>` for the active theme setting. Pressing `Enter` on that row opens the `/theme` picker; closing the `/theme` picker with `Enter`, `Ctrl-S` or `Esc` returns to `/settings` (updating the row when a new theme was applied); pressing `Esc` in `/settings` closes the picker.

## Overall validation
- `just test` — all tests pass.
- `just lint` — clean.
- `cargo fmt --all -- --check` — no diff.
- Manual (user):
  - Run `just run`, send a prompt so settled user/assistant/Arena cards are on screen, then run `/theme` and arrow through `automatic`, `dark` and `light`: the whole screen (including settled cards) previews live on each move, `Esc` restores the opening theme, and `Enter` saves to `~/.kiki/config.toml`.
  - Run `/settings`: shows `Theme  <setting>`; `Enter` opens the theme picker; `Enter` or `Esc` in the theme picker returns to `/settings` with the updated or restored setting; `Esc` closes `/settings`.

## Planning context
- Settled transcript blocks cache `Arc<SettledRaster>` on `SettledBlock::raster` (`src/transcript.rs:2228-2263`). `layout_with_cache` already takes `theme: &'static Theme` (`src/transcript.rs:3122-3128`) and currently checks only `raster.buffer.area.width == width` (`src/transcript.rs:3155-3165`). Recording `theme: &'static Theme` on `SettledRaster` in `rasterise` (`src/transcript.rs:3900-3919`) and checking `std::ptr::eq(raster.theme, theme)` matches spec Seams §1 and §4 without adding fields to `Theme`.
- `SettingsContext::save_model` (`src/config/settings.rs:281-290,325-330`) writes a single root key through `update_user_document` and `replace_string` (`src/config/settings.rs:1260-1284`) and reports masking from `model_origin`. `resolve` currently resolves `theme` without recording whether `project` supplied it (`src/config/settings.rs:534-537,636`); recording whether a project `theme` masks user saves lets `save_theme` return `SaveOutcome::SavedButMasked { by: SettingsOrigin::Project }`.
- Every existing entry in `slash::CATALOGUE` (`src/tui/slash.rs:20-47`) accepts an optional argument via `slash_argument` (`src/tui/slash.rs:68-77,155-163`). `/settings` and `/theme` are the first catalogue commands that take no argument (spec Seams §5), so `parse` and `recognised_token_range` must reject non-empty arguments for them while still allowing trailing whitespace.
- `App` stores `theme: &'static Theme` and `appearance: Appearance` (`src/tui.rs:200-201,364-372,400`), passes `app.theme` to every renderer (`src/tui.rs:2838-3204`), and routes picker keys through `handle_picker_key` (`src/tui.rs:1041-1102`) and user-default saves through `save_user_default` (`src/tui.rs:1267-1279`). Tracking the active `ThemeSetting` on `App` (plus whether `/theme` was opened from `/settings`) keeps `/settings` and `/theme` consistent across live preview, `Esc` restore, and masked or failed saves.
- Conventions: test-first; tests live in `#[cfg(test)] mod tests` in the same file; commits signed off (`git commit -s`); work in a `worktrees/<slug>` worktree on `work/<slug>`.

## Tasks

### Task 1: Theme-keyed settled transcript rasters
**Changes** — Record `theme: &'static Theme` on `SettledRaster` in `src/transcript.rs` and reuse a cached raster in `layout_with_cache` only when both `raster.buffer.area.width == width` and `std::ptr::eq(raster.theme, theme)` (spec Seams §1, §4).
**Tests** (`src/transcript.rs` unit) — Red first: extend `settled_cache_appends_and_invalidates_only_affected_rasters` (`src/transcript.rs:7675-7760`) so laying out with `&LIGHT` after `&DARK` at the same width rebuilds every settled block's raster, markdown and highlighting with `LIGHT` colours (e.g. `LIGHT.user_message_bg` on the user card), and a second `&LIGHT` layout at the same width reuses those cached rasters without additional rasterisations.
**Files** — Modify: `src/transcript.rs`.
**Verification** — `just test`, `just lint` and `cargo fmt --all -- --check` all pass.

### Task 2: `save_theme` and the `/theme` and `/settings` pickers
**Changes** — Implement `SettingsContext::save_theme(ThemeSetting) -> Result<SaveOutcome, SaveError>` (spec Seams §3), reporting `SaveOutcome::SavedButMasked { by: SettingsOrigin::Project }` when project settings define `theme`. Add `Settings` and `Theme` to `slash::Action` and `slash::CATALOGUE` (`/settings` "Open settings", `/theme` "Choose theme"), rejecting non-empty arguments in `slash::parse`. Add `PickerValue::Theme(ThemeSetting)` and `PickerValue::OpenThemePicker` in `src/tui.rs` (spec Seams §5):
- `/theme` opens a picker with `automatic (currently <dark|light>)`, `dark`, `light`, focused on the active `ThemeSetting`.
- Focus changes in the theme picker immediately update `app.theme` to the focused `ThemeSetting` resolved with `app.appearance`.
- `Enter` and `Ctrl-S` on `PickerValue::Theme(setting)` both apply `setting`, save via `save_theme`, and either close the picker or return to `/settings` if opened from there.
- `Esc` in the theme picker restores the `ThemeSetting` and `app.theme` active when the theme picker opened, and either closes the picker or returns to `/settings` if opened from there.
- `/settings` opens a single-row picker (`Theme  <setting>`) with `PickerValue::OpenThemePicker`; `Enter` (or `Ctrl-S`) opens the theme picker with return-to-settings set, and `Esc` closes `/settings`.
- Update `README.md` (`README.md:187,193`) so the `theme` documentation reflects interactive `/theme` and `/settings` selection instead of requiring a restart.
**Tests** (`src/config/settings.rs`, `src/tui/slash.rs`, `src/tui.rs` unit) — Red first:
- `src/config/settings.rs`: `save_theme` writes `theme = "dark" | "light" | "automatic"` at the document root, preserves comments and unrelated tables (`src/config/settings.rs:2627-2658`), and returns `SavedButMasked { by: SettingsOrigin::Project }` when `.kiki/config.toml` sets `theme` (`src/config/settings.rs:2734-2759`).
- `src/tui/slash.rs`: catalogue order includes `/settings` and `/theme` (`src/tui/slash.rs:268-286,320-346`); `/settings` and `/theme` parse with optional trailing whitespace and reject arguments (`/theme dark`, `/settings theme`).
- `src/tui.rs`:
  - update `slash_completion_lists_catalogue_in_order_and_clamps_keyboard_selection` (`src/tui.rs:5329-5372`) for the seven-command catalogue;
  - `/theme` lists `automatic (currently dark)` (or `light` when `appearance` is `Light`), `dark`, `light`, focused on the current setting;
  - moving focus in `/theme` (via arrow keys and filter typing) live-previews the resolved theme across the screen (including a settled user card's background), and `Esc` restores the opening theme without writing `.kiki/config.toml`;
  - `Enter` and `Ctrl-S` in `/theme` both apply the theme and persist `theme = "..."` to `~/.kiki/config.toml`, reporting project masking (`overridden by project settings`) or save errors while keeping the applied theme for the session;
  - `/settings` shows `Theme  <setting>`, `Enter` opens `/theme`, `Esc` in `/theme` restores the opening theme and returns to `/settings`, `Enter` in `/theme` saves and returns to `/settings` with the updated `Theme  <setting>` row, and `Esc` in `/settings` closes the picker.
**Files** — Modify: `src/config/settings.rs`, `src/tui/slash.rs`, `src/tui.rs`, `README.md`.
**Verification** — `just test`, `just lint` and `cargo fmt --all -- --check` all pass.
