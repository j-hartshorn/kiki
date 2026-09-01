# Plan: Default agent tools — Phase 1: Concurrent project inspection

## Source

- Spec: `docs/2026-09-01_default-agent-tools/spec.md`, Phase 1

## Acceptance criteria

- Arena Python exposes only the Phase 1 async inspection functions `read`, `list_dir`, `glob`, and `grep`, with the specified `DirectoryEntry` and `GrepMatch` dataclasses; runtime wrappers and prompt references come from the same typed Rust definitions.
- `read` returns one complete, exact UTF-8 string. `list_dir`, `glob`, and `grep` implement the specified path, ordering, ignore, hidden-file, symlink, entry-kind, UTF-8, result-shape, and complete-result limit behaviour.
- Every direct filesystem inspection remains confined to the launch workspace without following a symlink. Absolute paths and parent traversal are denied with the specified built-in Python exception classes.
- A real Monty `asyncio.gather()` dispatches independent host calls with at most 16 active calls, lets every normally failing batch member settle, associates results by call ID, and resumes results in Monty’s requested order regardless of completion order.
- Arena cancellation or timeout stops queued admission, cancels active async work, freezes the interrupted attempt’s ordered path-free tool trace, and restores the trusted checkpoint. Ordinary Python and tool errors retain the existing checkpoint commit behaviour.
- Kiki appends exactly one generated tool-reference section to bundled and overridden base prompts, preserving all base text verbatim. That managed section contains every active signature once, the await/gather guidance from the spec, and no legacy line-list or single-read capability contract. User-authored mentions outside the managed section are neither rejected nor rewritten. OpenAI continues to expose only `execute_python`.
- Retained and live tool evidence uses ordered safe summaries only: tool name, pending/failed status, or bounded counts. It contains no paths, arguments, pattern text, errors, output, or arbitrary user text, and retains the existing reliable-record and last-16 display behaviour.
- Shared Phase 1 runtime policy is compile-time data in `src/config/constants.rs`. No mutation, command, plugin, MCP, subagent, direct model-tool, or placeholder later-phase surface is added.

## Overall validation

```sh
cargo fmt --all -- --check
cargo check --all-targets
env -u OPENAI_API_KEY MONTY_BIN=/tmp/kiki-monty-0.0.21/bin/monty cargo test --all-targets -- --test-threads=1
cargo clippy --all-targets -- -D warnings
test "$(rg -l 'async_openai' src --glob '*.rs')" = "src/openai.rs"
test "$(rg -l 'monty_(pool|types)' src --glob '*.rs')" = "src/arena.rs"
test "$(rg -l 'cap_std' src --glob '*.rs')" = "src/tools.rs"
! rg -n 'ReadCapability|READ_CAPABILITY_DESCRIPTION|LineLimitExceeded|MAX_LINES' src prompts README.md
```

Expected: formatting, checks, all credential-free tests, Clippy, dependency-locality checks, and stale-contract checks pass; the live OpenAI test remains the only ignored test.

## Planning context

- The current `ReadTool` owns the workspace capability, performs blocking reads off Tokio, returns line lists, and enforces 4 MiB plus 100,000-line limits (`src/tools.rs:13-14`, `src/tools.rs:107-174`, `src/tools.rs:196-225`).
- The Arena adapter currently recognises only `read`, requires exactly one pending future, executes that read inline, and resumes one result (`src/arena.rs:725-858`). Monty types are already isolated to this adapter (`README.md:85-87`).
- Arena completion and ordinary Python errors commit a dumped checkpoint, while cancellation, timeout, worker loss, and unsafe recovery restore the prior checkpoint (`src/arena.rs:452-590`, `src/arena.rs:639-686`). Preserve that outer attempt boundary.
- Read-specific progress and retained evidence currently span Arena, Agent, Runtime, and Transcript (`src/arena.rs:80-147`, `src/agent.rs:39-80`, `src/runtime.rs:92-107`, `src/transcript.rs:315-345`). Reliable terminal records are separate from lossy progress and must remain authoritative.
- The system prompt currently documents line-list `read` as the only host capability, and model history appends a second read capability description to provider requests (`prompts/read_only_agent.md:1-16`, `src/model.rs:172-208`, `src/openai.rs:72-86`). Prompt resolution already has one bundled-or-override seam (`src/prompt.rs:55-83`). Base prompts remain opaque user text: Kiki appends one managed reference section but does not inspect, reject, or rewrite user-authored mentions.
- The codebase keeps unit and integration-style tests inline in each owning Rust module. The documented deterministic gate is credential-free and single-threaded (`README.md:80-88`). The pre-change gate passes 357 tests with one ignored live test.
- Phase 1 adds only constants it consumes. Command-output policy remains deferred with `run` in Phase 3 rather than creating unused configuration now.

## Tasks

### Task 1: Centralise Phase 1 runtime policy

**Changes** — Create the constants module and move the fixed model, accepted/provider content bounds, Arena five-minute deadline and 64 MiB memory default, file and collection limits, and 16-call concurrency ceiling into it. Remove the obsolete read line limit. Keep retry timing, channel/presentation bounds, labels, worker protocol identity, and diagnostic prose beside their implementations; do not add later-phase command constants.

**Tests** (`src/config.rs`, `src/model.rs`, `src/openai.rs`, `src/pricing.rs`, `src/arena.rs`, unit/integration) — First make the existing fixed-model, pricing-identity, content-bound, and production-Arena policy assertions consume the shared constants and expect the five-minute deadline. Preserve exact boundary tests for accepted/provider content and Arena memory.

**Files** — Create: `src/config/constants.rs`. Modify: `src/config.rs`, `src/model.rs`, `src/openai.rs`, `src/pricing.rs`, `src/agent.rs`, `src/arena.rs`, `src/tools.rs`.

**Verification** — `env -u OPENAI_API_KEY MONTY_BIN=/tmp/kiki-monty-0.0.21/bin/monty cargo test --all-targets -- --test-threads=1` — shared-policy and existing boundary tests pass.

### Task 2: Generalise tool evidence without exposing call data

**Changes** — Replace read-specific progress, interruption state, and retained rows with one generic ordered tool-call representation. Admit one pending row per valid call, update that row in place on settlement, and carry only the tool name plus safe pending/failed/count summaries. Remove the separate path/error-bearing interrupted-read evidence and keep tool traces out of model feedback. Preserve reliable terminal records, lossy progress gaps, cancellation freezing, and the last-16 collapsed display.

**Tests** (`src/arena.rs`, `src/agent.rs`, `src/runtime.rs`, `src/transcript.rs`, `src/tui.rs`, unit/integration/presentation) — Write the generic trace-state assertions first, driving admission and reversed completion directly without introducing the later batch scheduler. Prove cancellation freezes settled and pending rows; trace/progress text cannot contain fixture paths or errors; reliable execution records survive dropped progress; collapse still shows the latest 16 rows and expansion shows all rows. Update existing single-read integration fixtures to the generic representation without weakening checkpoint, cancellation, tiny-terminal, or scrolling coverage; defer real multi-call races to Task 7.

**Files** — Modify: `src/arena.rs`, `src/agent.rs`, `src/runtime.rs`, `src/transcript.rs`, `src/tui.rs`.

**Verification** — `env -u OPENAI_API_KEY MONTY_BIN=/tmp/kiki-monty-0.0.21/bin/monty cargo test --all-targets -- --test-threads=1` — generic evidence and all existing recovery, reliable-record, transcript, and TUI behaviour pass.

### Task 3: Replace the read special case with the generated tool host

**Changes** — Introduce the concrete Monty-neutral host seam, JSON-like argument/result values, typed definitions, result shapes, and built-in exception categories in `tools.rs`. Route `read` through that host, return one exact string, enforce the no-follow component walk and 4 MiB bound, and remove the line bound. Generate the public `read` wrapper and its prompt skeleton from the same definition, invoke the private host function through the Arena adapter, bootstrap before the initial checkpoint, and wire the host at startup. Append generated references to bundled and overridden prompts, generalise the bundled prompt, and remove the legacy `ReadCapability` history entry and provider-request suffix.

**Tests** (`src/tools.rs`, unit; `src/arena.rs`, Monty integration; `src/prompt.rs`, `src/model.rs`, `src/openai.rs`, request integration; `src/runtime.rs`, construction fixtures) — Start with failing tests for exact-string reads, lexical aliases and denials, inside- and outside-workspace intermediate/final symlinks, every read path-kind exception, non-blocking FIFO rejection, exact byte boundaries, and more than 100,000 lines. Execute and snapshot the generated bootstrap, prove the wrapper survives checkpoint restore, reject unsupported neutral values and malformed arguments as `TypeError`, and prove bundled and override prompts preserve their base text then append one managed section whose active signature appears once, tells the model not to redefine it, and contains no stale line-list text. Include an override that independently mentions `read` to prove user text is preserved rather than policed. Preserve ambient-filesystem denial, Runtime’s Arena construction/recovery fixtures, and the single outer `execute_python` request schema.

**Files** — Modify: `src/tools.rs`, `src/arena.rs`, `src/main.rs`, `src/prompt.rs`, `prompts/read_only_agent.md`, `src/model.rs`, `src/agent.rs`, `src/runtime.rs`, `src/openai.rs`.

**Verification** — `env -u OPENAI_API_KEY MONTY_BIN=/tmp/kiki-monty-0.0.21/bin/monty cargo test --all-targets -- --test-threads=1` — the generated read surface, prompt/request contract, confinement, errors, and checkpoint persistence pass.

### Task 4: Add bounded directory listing

**Changes** — Add `DirectoryEntry` and `list_dir` to the typed definition set and host. Extend the capability-owned no-follow component walk to open directory roots and classify every immediate entry kind, normalise returned paths, and return stable complete results within the shared item/payload bounds. Run blocking filesystem work outside Tokio’s async workers. Include hidden and ignored entries; do not add recursive traversal yet.

**Tests** (`src/tools.rs`, unit; `src/arena.rs`, Monty integration; `src/prompt.rs`, generated-reference integration) — Write the direct-root matrix first: missing root raises `FileNotFoundError`, a file root raises `NotADirectoryError`, and a symlink root raises `PermissionError`. Cover stable immediate listing, hidden and ignored entries, files/directories/symlinks/FIFOs with exact `kind` and `size_bytes`, inside- and outside-workspace intermediate symlinks, non-blocking FIFO classification, unfiltered non-UTF-8 names failing the whole call, workspace-relative output spelling, and exact item/payload limits without partial results. Use a deterministic gate to prove a blocked listing does not prevent another inspection future from making progress. Execute `DirectoryEntry` attribute access in Monty and prove only implemented signatures are advertised once.

**Files** — Modify: `src/tools.rs`, `src/arena.rs`, `src/prompt.rs`.

**Verification** — `env -u OPENAI_API_KEY MONTY_BIN=/tmp/kiki-monty-0.0.21/bin/monty cargo test --all-targets -- --test-threads=1` — directory listing, wrapper conversion, bounds, confinement, and generated references pass.

### Task 5: Add bounded project-aware globbing

**Changes** — Add `glob` to the same definitions and host, plus the `ignore` and `globset` crates. Build recursive traversal on the capability-owned no-follow directory walk rather than `WalkBuilder`, and run the blocking scan outside Tokio’s async workers. Read `.rgignore`, `.ignore`, and `.gitignore` through the capability; compile their lines with `ignore::gitignore::GitignoreBuilder` and preserve ripgrep’s per-directory precedence and negation behaviour. Apply hidden and ignore decisions to raw names before UTF-8 conversion, match valid patterns relative to the requested root including basename-only patterns, and return stable complete `DirectoryEntry` results without descending through symlinks.

**Tests** (`src/tools.rs`, unit; `src/arena.rs`, Monty integration; `src/prompt.rs`, generated-reference integration) — Begin with invalid-pattern `ValueError` and the complete missing/file/symlink root matrix. Cover root-relative direct-child/nested and basename patterns; nested ignore files; `.rgignore` over `.ignore` over `.gitignore` precedence; negation; explicit hidden and parent-ignored roots; filtered hidden/ignored non-UTF-8 names versus unfiltered whole-call failure; inside- and outside-workspace symlink entries without descent; FIFO entries without blocking; workspace-relative result spelling; stable ordering; and exact item/payload overflow without partial data. Execute the generated wrapper in Monty and prove the prompt and runtime shape remain aligned.

**Files** — Modify: `Cargo.toml`, `Cargo.lock`, `src/tools.rs`, `src/arena.rs`, `src/prompt.rs`.

**Verification** — `env -u OPENAI_API_KEY MONTY_BIN=/tmp/kiki-monty-0.0.21/bin/monty cargo test --all-targets -- --test-threads=1` — glob matching, ignore semantics, bounds, confinement, and generated references pass.

### Task 6: Add bounded regular-expression search

**Changes** — Add `GrepMatch` and `grep` to the same definitions and host, using `grep-regex` and `grep-searcher` in process. Reuse the capability-owned traversal, project-ignore precedence, hidden, path normalisation, file-glob, and collection-bound policy, with the blocking scan outside Tokio’s async workers. Implement the distinct explicit-file and traversed-file rules for file-glob filtering, binary/invalid UTF-8, symlinks, and `other` entries, returning stable path and one-based line order without terminators.

**Tests** (`src/tools.rs`, unit; `src/arena.rs`, Monty integration; `src/prompt.rs`, generated-reference integration) — Begin with matching-line records plus invalid regex and invalid `file_glob` `ValueError`s. Cover missing, directory, regular-file, symlink, and `other` direct roots; root-relative and basename file globs; pre-validation file-glob exclusion; explicit binary/invalid/other errors; traversed binary/invalid/non-file skips; explicit hidden/parent-ignored roots; nested ignore files with precedence and negation; inside- and outside-workspace symlinks without traversal; non-blocking FIFOs; line endings; stable ordering; raw-name non-UTF-8 filtering/failure; and exact item/payload overflow. Execute `GrepMatch` attribute access in Monty and prove generated prompt/runtime return shapes agree.

**Files** — Modify: `Cargo.toml`, `Cargo.lock`, `src/tools.rs`, `src/arena.rs`, `src/prompt.rs`.

**Verification** — `env -u OPENAI_API_KEY MONTY_BIN=/tmp/kiki-monty-0.0.21/bin/monty cargo test --all-targets -- --test-threads=1` — grep semantics, errors, wrapper conversion, bounds, and generated references pass.

### Task 7: Dispatch complete Monty batches concurrently

**Changes** — Replace the one-future driver assumption with complete pending-batch admission and host dispatch. Run no more than 16 calls at once, queue the rest in pending order, let all accepted calls settle after ordinary tool errors, update trace rows by call ID as calls finish, and resume Monty once with results in its requested call-ID order. Keep cancellation and timeout at the Arena attempt boundary so they stop queued admission and drop active async work before checkpoint recovery. Extend the generated managed reference section with the final await/gather, independent-work, failure, focused-output, and five-minute deadline guidance plus the concrete gather example; do not duplicate tool guidance in the opaque base prompt. Update README’s active tool and path-free trace documentation without documenting later phases.

**Tests** (`src/tools.rs`, scheduler unit; `src/arena.rs`, real Monty integration; `src/agent.rs`, `src/runtime.rs`, `src/transcript.rs`, cancellation/reliable-record integration; `src/prompt.rs`, prompt integration; `src/openai.rs`, request integration) — Start with a gated reversed-completion `asyncio.gather()` test proving Python argument order and call-ID correlation. Cover exactly 16 active calls with later calls queued, normal failure settling every active and queued sibling, one complete `resume_futures`, out-of-order trace updates without row reordering, and read-only calls executing concurrently. Exercise cancellation and a short test-only Arena timeout against a 17+ call batch; each must prevent queued admission, freeze authoritative pending/settled evidence, and restore the prior checkpoint. Execute a final exact-active-set check in Monty: the four Phase 1 globals work, while `write`, `edit`, and `run` remain unbound. Prove bundled and overridden requests append exactly one managed section containing each Phase 1 signature once, say not to redefine them, include the gather guidance, omit later-phase tools, preserve arbitrary base text, and still expose only `execute_python` directly.

**Files** — Modify: `src/tools.rs`, `src/arena.rs`, `src/agent.rs`, `src/runtime.rs`, `src/transcript.rs`, `src/tui.rs`, `src/prompt.rs`, `src/openai.rs`, `README.md`.

**Verification** — `env -u OPENAI_API_KEY MONTY_BIN=/tmp/kiki-monty-0.0.21/bin/monty cargo test --all-targets -- --test-threads=1` — concurrent settlement, deterministic correlation, cancellation/timeout recovery, evidence, prompt/request, and presentation tests pass; then run **Overall validation**.
