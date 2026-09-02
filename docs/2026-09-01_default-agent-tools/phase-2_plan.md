# Plan: Default agent tools — Phase 2: Safe source mutation

## Source

- Spec: `docs/2026-09-01_default-agent-tools/spec.md`, Phase 2

## Acceptance criteria

- Arena exposes exactly `read`, `write`, `edit`, `list_dir`, `glob`, and `grep`; `run` remains unavailable. Generated wrappers and prompt references remain one typed source of truth. OpenAI exposes only `execute_python`.
- `write(path, content)` creates missing parents and new UTF-8 files, or replaces an authorised regular file. Content is bounded by `FILE_MAX_BYTES`; new files use `0666` filtered by umask; replacements preserve permission bits.
- `edit(path, old, new)` performs exactly one replacement in an existing bounded UTF-8 regular file. Empty, identical, absent, multiple, and overlapping matches fail without mutation.
- Mutations publish complete same-directory temporary files atomically. New-file publication never clobbers a target appearing concurrently. External freshness races raise `RuntimeError`.
- Shared descriptor-relative `rustix` traversal closes the existing symlink-swap window for every direct filesystem tool on macOS and Linux. No direct tool follows terminal or intermediate symlinks.
- Whole-file authorisation maps normalised paths to 32-byte BLAKE3 content digests. Successful reads and writes authorise their resulting content; edits advance matching authorisation or remove stale authorisation.
- Freshness transitions use the batch-start snapshot and pending-call order. Same-batch reads never authorise mutations on that path. Dependent read-transform-write operations require sequential awaits.
- Same-path writes and edits execute in pending-call order. Different paths and independent inspections remain concurrent under the existing 16-call ceiling. Ordinary tool failures still let every accepted sibling settle.
- Normal completion and ordinary Python errors commit Monty and authorisation state together. Cancellation, timeout, worker loss, unsafe recovery, or abandoned blocking work restores the prior authorisation snapshot without rolling back completed filesystem effects.
- Evidence remains ordered and path-free: `write · created`, `write · replaced`, `edit · applied`, `failed`, or `pending`. Existing reliable-record and last-16 display behaviour remains unchanged.
- No command execution, rollback promise, crash durability, external configuration, direct model mutation tools, or broader plugin system enters Phase 2.

## Overall validation

```sh
cargo fmt --all -- --check
cargo check --all-targets
env -u OPENAI_API_KEY MONTY_BIN=/tmp/kiki-monty-0.0.21/bin/monty cargo test --all-targets -- --test-threads=1
cargo clippy --all-targets -- -D warnings
test "$(rg -l 'async_openai' src --glob '*.rs')" = "src/openai.rs"
test "$(rg -l 'monty_(pool|types)' src --glob '*.rs')" = "src/arena.rs"
test "$(rg -l '(cap_std|rustix|blake3)' src --glob '*.rs')" = "src/tools.rs"
! rg -n 'read-only Python arena|only_the_four_phase_one|generated_phase_one_reference' src README.md
git diff --check
```

Expected: formatting, compilation, all credential-free tests, Clippy, dependency-locality, stale-contract, and whitespace checks pass. The live OpenAI test remains the only ignored test.

## Planning context

- The generated interface currently supports four `ToolName` variants and only string or record-list result shapes; successful mutation wrappers need a `None` result shape (`src/tools.rs:40-75`, `src/tools.rs:222-250`, `src/tools.rs:346-371`).
- `ToolHost` accepts one call at a time, while `arena.rs` owns the 16-call `buffer_unordered` scheduler (`src/tools.rs:896-917`, `src/arena.rs:899-950`). Phase 2 moves this policy behind one host-owned batch interface.
- `Checkpoint` currently stores only runtime identity and Monty bytes. Normal completion and ordinary Python errors replace it; cancellation, timeout, and infrastructure recovery retain it (`src/arena.rs:48-51`, `src/arena.rs:454-518`).
- Current direct-file traversal performs a separate metadata check before a `cap-std` open with custom no-follow flags (`src/tools.rs:1842-1909`). Phase 2 replaces that split with descriptor-relative `rustix` operations shared by inspection and mutation.
- Tool evidence currently represents only pending, failed, or counted results. Runtime and transcript rendering exhaustively match those states (`src/arena.rs:113-117`, `src/runtime.rs:1028-1040`, `src/transcript.rs:347-356`).
- Prompt resolution already preserves bundled or overridden base text and appends one generated reference (`src/prompt.rs:55-88`). Existing prompt and provider tests explicitly assume the four Phase 1 signatures (`src/prompt.rs:109-140`, `src/openai.rs:4000-4040`).
- `rustix` is already transitive but not directly declared; BLAKE3 is absent. Phase 2 adds both as direct dependencies (`Cargo.toml:6-30`, `Cargo.lock:2463`).
- Tests live inline with their owning modules. The clean Phase 2 baseline is 421 passed, 0 failed, and 1 ignored.

## Tasks

### Task 1: Move concurrent dispatch behind the tool-host seam

**Changes** — Replace Arena-owned per-call scheduling with one Monty-neutral `ToolHost` batch operation. Preserve the 16-call ceiling, completion-order progress, call-ID correlation, ordinary-error sibling settlement, cancellation by dropping active work, and one ordered Monty resume. Do not add mutation behaviour yet.

**Tests** (`src/tools.rs`, scheduler unit; `src/arena.rs`, Monty integration) — First move the existing gated concurrency cases to the host interface: reversed settlement, exactly 16 active calls, later admission, ordinary failure settling queued siblings, cancellation dropping active work, and unique correlated results. Preserve Arena tests for admission-order traces and exactly one `resume_futures`.

**Files** — Modify: `src/tools.rs`, `src/arena.rs`.

**Verification** — `env -u OPENAI_API_KEY MONTY_BIN=/tmp/kiki-monty-0.0.21/bin/monty cargo test --all-targets -- --test-threads=1` — all existing tests pass with the scheduler owned by `ToolHost`.

### Task 2: Harden descriptor-relative filesystem access

**Changes** — Add direct `rustix` filesystem support and replace split metadata/open checks with shared descriptor-relative no-follow file and directory opens. Keep paths rooted beneath the existing workspace capability and reuse the hardened walker for `read`, `list_dir`, `glob`, `grep`, and later mutations.

**Tests** (`src/tools.rs`, filesystem integration) — Start with direct primitive tests that refuse final and intermediate symlinks without relying on a prior metadata check. Cover inside/outside targets, component swaps, renamed-but-open parent directories, directories, regular files, FIFOs, missing paths, and the existing Python error categories. Preserve all Phase 1 inspection behaviour.

**Files** — Modify: `Cargo.toml`, `Cargo.lock`, `src/tools.rs`.

**Verification** — `cargo test tools::tests -- --test-threads=1` — hardened confinement and all inspection tests pass.

### Task 3: Implement capability-confined whole-file writes

**Changes** — Add BLAKE3 content fingerprints, the canonical opaque authorisation types, and the host's private write operation. Create missing parents one component at a time, create unpredictable same-directory temporary files exclusively using system entropy, and clean up unpublished files. Create a missing target with `0666` filtered by umask and no-clobber publication; replace an existing regular file only when its current digest matches the supplied authorisation, preserving permission bits and performing the latest practical recheck before atomic rename. Use a safe hard-link fallback only when a supported platform lacks no-replace rename. Return internal created/replaced and resulting-digest outcomes; do not expose Python mutation functions yet.

**Tests** (`src/tools.rs`, filesystem integration) — Start with unauthorised replacement, stale replacement, no-clobber, and atomic-visibility failures. Cover nested parent creation, intermediate symlinks, target appearance and final-recheck races, same-length external changes, old-or-new complete reader observations, exact 4 MiB boundaries, directories, symlinks, `other` entries, binary existing files without authorisation, permission preservation, umask-filtered new modes, temporary-name collisions, conditional fallback publication, cleanup after failure, unchanged targets after failure, and the accepted possibility that created parents remain.

**Files** — Modify: `Cargo.toml`, `Cargo.lock`, `src/tools.rs`.

**Verification** — `cargo test tools::tests -- --test-threads=1` — write mechanics, BLAKE3 fingerprints, publication, and confinement tests pass.

### Task 4: Implement exact atomic edits

**Changes** — Add the host's private edit operation over complete bounded UTF-8 files using the shared publisher and canonical authorisation types. Count overlapping occurrences at UTF-8 character boundaries, require exactly one non-empty replacement target, enforce pre-edit and resulting file limits, perform a final freshness recheck, publish atomically, and return internal pre/post digests plus an applied outcome.

**Tests** (`src/tools.rs`, filesystem integration) — Begin with zero, multiple, and overlapping-match failures. Cover empty and identical replacements, multibyte boundaries, exact byte limits before and after replacement, invalid UTF-8, path-kind errors, target-change races as `RuntimeError`, unchanged files after every failure, atomic visibility, and preserved permissions.

**Files** — Modify: `src/tools.rs`.

**Verification** — `cargo test tools::tests -- --test-threads=1` — exact-edit behaviour and failure atomicity pass.

### Task 5: Checkpoint whole-file authorisation

**Changes** — Add the opaque normalised-path-to-BLAKE3 map to the Arena checkpoint and execution attempt. Successful reads return internal digest effects. Apply read-only effects in pending-call order, commit the resulting snapshot beside successful Monty dumps and ordinary Python errors, and discard it on cancellation, timeout, worker loss, unsafe resource recovery, or failed recovery. Blocking closures receive no reference to canonical state.

**Tests** (`src/tools.rs`, policy unit; `src/arena.rs`, checkpoint integration) — Start with pure transition tests for ordered reads and failed calls. Prove explicit checkpoint restore, ordinary Python-error commit, reversed read completion, cancellation, timeout, worker loss, unsafe memory recovery, and late blocking completion all retain or discard authorisation with the same boundary as Monty state.

**Files** — Modify: `src/tools.rs`, `src/arena.rs`.

**Verification** — `env -u OPENAI_API_KEY MONTY_BIN=/tmp/kiki-monty-0.0.21/bin/monty cargo test --all-targets -- --test-threads=1` — checkpoint, recovery, and existing concurrency tests pass.

### Task 6: Activate safe source mutation

**Changes** — Add the `None` result shape and generated `write(...)->None` and `edit(...)->None` wrappers in canonical order after `read`. Route them through host-owned batch dispatch, serialise normalised same-path mutations without consuming slots needed by independent calls, and apply mutation transitions from the batch-start snapshot in pending order. Ignore same-batch reads for mutated paths. Add fixed path-free created/replaced/applied summaries, update prompt/provider assertions and README, and keep `run` absent.

**Tests** (`src/tools.rs`, scheduler/generated contract; `src/arena.rs`, Monty integration; `src/runtime.rs`, reliable evidence; `src/transcript.rs`, presentation; `src/prompt.rs`, generated reference; `src/openai.rs`, provider request) — Begin with sequential `read` → `write` and exact `edit` scenarios. Cover every authorisation transition, same-batch read/write denial in either completion order, ordered same-path write/edit batches and lexical aliases, concurrent different-path mutations, failed mutation siblings, cancellation/timeout after completed or late publication, ordinary Python-error commit, checkpoint recovery, exact six-tool exposure, Python `None`, safe statuses, reliable path-free evidence, one managed prompt section, and outer-only `execute_python`.

**Files** — Modify: `src/tools.rs`, `src/arena.rs`, `src/runtime.rs`, `src/transcript.rs`, `src/prompt.rs`, `src/openai.rs`, `README.md`.

**Verification** — Run **Overall validation** — Phase 2 passes completely with only the live OpenAI test ignored.
