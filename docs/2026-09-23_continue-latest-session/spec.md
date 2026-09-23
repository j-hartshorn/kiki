# Spec: Continue the latest session

## Context

Kiki can list sessions for the current workspace and resume one by ID. Continuing the latest one currently requires copying its ID from `kiki sessions`.

## Goals

- Add `kiki --continue` and a matching `just continue` recipe to resume the most recently active session saved for the current workspace without an ID.
- Use the same ordering and workspace matching as `kiki sessions` (newest saved activity first, then ascending ID; resolved paths match).
- Preserve the existing by-ID resume, ownership, restoration and error behaviour.
- Fail clearly without creating a session when there is no matching session.

## Non-goals

- Choosing a session across workspaces or implicit relocation.
- Falling back to an older session if the selected session cannot be opened.
- Adding `--continue --here`, changing the session store, or adding an in-TUI switcher.

## Key discoveries

- CLI parsing and dispatch are in `src/main.rs:52-95` and `src/main.rs:402-409`; `--resume` enters the common restoration path at `src/main.rs:284`.
- `kiki sessions` filters resolved workspaces, sorts by `updated_at_ms` descending and ID ascending, and warns about unreadable entries at `src/main.rs:140-182`.
- `Session::list` reads existing databases without creating a session, skips unreadable candidates with warnings, and returns an empty listing for an absent sessions directory at `src/session.rs:643-683`.
- Process-level CLI tests already construct production-schema fixtures and check invalid arguments, listing and resume errors at `tests/sessions_cli.rs:24-116` and `tests/sessions_cli.rs:190-542`.
- `justfile:1-7` loads `.env` and ensures the pinned Monty worker; `justfile:18-19` runs the CLI via `cargo run --`.
- The worktree workflow is defined in `worktrees/README.md:1-19`.

## Proposed approach

Accept only bare `--continue`. Select the first row that would be displayed by `kiki sessions`, using its workspace-scoped listing and deterministic order, then pass its ID to the existing normal resume path. Match the listing's warning/skip behaviour for unreadable entries and invalid timestamps. If no candidate remains, report a clear error before initialising tracing or runtime configuration. If the chosen session later fails to open (including ownership or workspace changes), surface the normal resume error rather than trying another one. Add `just continue` alongside `just run` using the same Monty prerequisite and dotenv configuration. Document both commands alongside existing session CLI forms.

## Key decisions & tradeoffs

- Current-workspace scope (chosen by the user) matches the existing list and avoids surprise relocation; a global most-recent selector was rejected.
- Reuse read-only listing and normal resume rather than storing a separate latest-session pointer or writing a new restore path.
- Do not support `--here` for `--continue`; relocation still requires an explicit session ID.

## Risks / unknowns

- The session may change between listing and opening; existing resume guards should remain authoritative. No new lock or fallback is needed.

## Dependencies

- Existing session listing and resume in `src/main.rs` and `src/session.rs`; no new dependencies.

## Plan

### Acceptance criteria

- `kiki --continue` picks exactly the first session shown by `kiki sessions` for the current workspace, including symlink-equivalent roots and equal-timestamp ID ties.
- No matching session yields a clear failure, no new session, and no runtime/tracing startup. Corrupt/unreadable candidates are skipped with warnings; a selected but owned session fails instead of opening an older one.
- `--resume <id>` and `--resume <id> --here` continue unchanged; `--continue` with extra arguments fails with usage before HOME lookup.
- `just continue` runs `cargo run -- --continue` with the existing Monty prerequisite and `.env` loading.

### Overall validation

`MONTY_BIN=/tmp/kiki-monty/bin/monty cargo test --test sessions_cli`; `just --dry-run continue` (prints the Monty prerequisite and `cargo run -- --continue` without launching the TUI); `cargo check --all-targets`; `cargo clippy --all-targets -- -D warnings`; `cargo fmt --all --check`; `MONTY_BIN=/tmp/kiki-monty/bin/monty cargo test --all-targets`.

### Tasks

#### Task 1: Continue the latest workspace session

**Changes** — Add the CLI form and usage text, share the existing listing's workspace filtering and ordering with selection, and route the chosen ID through normal resume. Keep unreadable-row warnings, empty-selection error and failed-open behaviour explicit. Add a `just continue` recipe with the same Monty and dotenv behaviour as `just run`; document both commands, workspace scope and empty-session behaviour.

**Tests** (`tests/sessions_cli.rs`, binary integration) — First red: bare `--continue` with two workspaces selects the newest matching session rather than the globally newest one. Cover equal-timestamp ID ties, no matching session/no new session or tracing, a skipped unreadable candidate (including invalid timestamps where necessary to match displayed listing), and selected-session ownership failure without fallback. Extend the existing invalid-argument test for `--continue` with extra arguments. Existing listing tests cover symlink-equivalent roots and corrupt entries; existing `--resume` tests protect by-ID behaviour.

**Files** — Modify: `src/main.rs`, `tests/sessions_cli.rs`, `justfile`, `README.md`.

**Verification** — `MONTY_BIN=/tmp/kiki-monty/bin/monty cargo test --test sessions_cli`, `just --dry-run continue`, `cargo check --all-targets`, `cargo clippy --all-targets -- -D warnings`, `cargo fmt --all --check`, and `MONTY_BIN=/tmp/kiki-monty/bin/monty cargo test --all-targets` pass. The dry run prints the Monty prerequisite and `cargo run -- --continue`; check README examples against the tested CLI.

After plan approval, create `work/continue-latest-session` and `worktrees/continue-latest-session` from `main`. Implement and commit in that worktree with signed-off commits. Do not merge into `main` until the result is accepted.
