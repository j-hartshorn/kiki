# Plan: Default agent tools — Phase 3: One-shot command verification

## Source

- Spec: `docs/2026-09-01_default-agent-tools/spec.md`, Phase 3

## Acceptance criteria

- Arena exposes exactly `read`, `write`, `edit`, `list_dir`, `glob`, `grep`, and `run`, in that order. OpenAI continues to expose only `execute_python`.
- `run(command, cwd=".")` invokes one non-login `/bin/sh -c` command with Kiki's inherited environment, null standard input, and no command-specific timeout.
- `cwd` follows the existing lexical and no-follow directory rules. The command starts in the exact validated directory even if its pathname is replaced before spawn.
- Successful and non-zero commands return complete `CommandResult` records. A signal-terminated command returns `exit_code=None`.
- stdout and stderr are captured independently, decoded lossily as UTF-8, and limited to 8 MiB each using original byte counts. Overflow immediately kills the owned process group, drains its pipes through cleanup, returns no partial result, and raises `RuntimeError` with the safe diagnostic required by the spec.
- Each command owns a new Unix process group. After shell exit, output overflow, cancellation, or Arena timeout, Kiki immediately sends `SIGKILL` to the group.
- Dropping an active command future requests supervisor cleanup. Cancellation takes precedence over command completion and output overflow; Arena state recovers from its checkpoint while completed command effects remain.
- Commands use the existing 16-call scheduler, remain independent of direct filesystem mutations, preserve call-ID correlation, and do not stop ordinary siblings from settling after a tool error.
- Generated wrappers and prompt references add `CommandResult` and `run` from the existing typed source of truth. Prompt guidance requires project-local, one-shot commands and focused model-visible output.
- Retained evidence stays ordered and path-free. Numeric exits render `run · exit N`, signal termination renders `run · signal`, and failures retain no command, path, output, or error text.
- macOS and Linux are supported. A descendant that deliberately creates a separate session remains outside the best-effort process-group guarantee.

## Overall validation

```sh
cargo fmt --all -- --check
cargo check --all-targets
env -u OPENAI_API_KEY MONTY_BIN=/tmp/kiki-monty-0.0.21/bin/monty cargo test --all-targets -- --test-threads=1
cargo clippy --all-targets -- -D warnings
test "$(rg -l 'async_openai' src --glob '*.rs')" = "src/openai.rs"
test "$(rg -l 'monty_(pool|types)' src --glob '*.rs')" = "src/arena.rs"
test "$(rg -l '(cap_std|rustix|blake3|getrandom)' src --glob '*.rs')" = "src/tools.rs"
! rg -n 'six_phase_two|generated_phase_two_reference|run_missing' src README.md
! rg -n -F 'assert!(!reference.contains("run("))' src
git diff --check
```

Expected result: formatting, compilation, the credential-free serial test suite, Clippy, dependency-locality checks, stale-contract checks, and whitespace checks all pass. The live OpenAI test remains ignored without explicit credentials.

## Planning context

- `ToolHost::dispatch_batch` already owns the 16-call scheduler, completion-order settlement, same-path mutation queues, and working authorisation state; `run` can use this seam without a second scheduler (`src/tools.rs:754-837`, `src/tools.rs:1507-1548`).
- Arena already drops the active checkout future on cancellation or its whole-execution deadline, then restores the trusted checkpoint. The command future therefore needs drop-triggered cleanup rather than a new Arena cancellation interface (`src/arena.rs:449-538`).
- `ToolHost` retains a workspace directory capability, and the existing component-wise directory opener yields a no-follow validated directory descriptor. Passing that descriptor to child setup avoids retaining or re-resolving an ambient `cwd` path (`src/tools.rs:842-850`, `src/tools.rs:2690-2849`).
- The typed definition model generates dataclasses, wrappers, result conversion, and prompt skeletons, but its result vocabulary currently has no singular record shape for `CommandResult` (`src/tools.rs:240-371`, `src/tools.rs:453-611`).
- Neutral values already support the dictionary, integer, `None`, and string values needed for `CommandResult`; the existing Monty adapter needs no command-specific value type (`src/tools.rs:33-42`, `src/arena.rs:1022-1089`).
- Safe summaries are settled in completion order while rows remain in pending-call order. Summary classification must use the admitted tool identity rather than interpreting every dictionary result as a command result (`src/arena.rs:114-185`, `src/arena.rs:953-971`, `src/arena.rs:1093-1107`).
- Tokio process and asynchronous I/O support plus Unix `libc` are already dependencies, so Phase 3 needs no new crate (`Cargo.toml:19-35`).
- The only current subprocess path validates the Monty worker version with leader-only `kill_on_drop`; Kiki has no reusable process-group or bounded-pipe implementation (`src/arena.rs:609-621`).
- Keep the process implementation in a private `src/tools/command.rs` module. `src/tools.rs` remains the host adapter and the only source file importing `cap_std`, `rustix`, `blake3`, or `getrandom`.

## Tasks

### Task 1: Own bounded Unix command processes

**Changes** — Add the 8 MiB command-stream policy and a private command module behind one async command-execution interface. Invoke `/bin/sh -c` with inherited environment, null stdin, independent piped stdout and stderr, and an owned validated directory descriptor applied with child-side `fchdir`. Establish a new process group before exec. Let a private supervisor own the child, pipes, and group, and immediately send `SIGKILL` to the group after normal shell exit, output overflow, or drop-triggered cancellation. Drain both streams concurrently through cleanup, count original bytes, decode retained complete output lossily, and return a safe typed overflow or launch failure without retaining command text, paths, or output in diagnostics. Do not add graceful-shutdown timing or escalation state, a command timeout, environment policy, sandbox, persistent session, or Windows fallback.

**Tests** (`src/tools/command.rs`, private policy/unit and Unix process integration) — Start with exact-boundary output tests that fail without the new module. Cover independent stdout and stderr, invalid UTF-8 replacement, exactly 8 MiB accepted, one byte over rejected, omitted-byte accounting, overflow-triggered group cleanup, pipe drainage after cleanup, and no partial success. Exercise zero, non-zero, and signal status; inherited environment; immediate stdin EOF; child-side descriptor `cwd` after its original pathname is renamed and replaced by a symlink; shell exit with a pipe-holding descendant; group-wide cleanup; drop-triggered cleanup; immediate guard cleanup if the supervisor is dropped; and launch failure. Use one private lifecycle gate rather than a process simulator or wall-clock readiness guesses, and use POSIX shell syntax shared by macOS and Linux.

**Files** — Modify: `src/config/constants.rs`, `src/tools.rs`; Create: `src/tools/command.rs`

**Verification** — `cargo fmt --all -- --check && cargo check --all-targets && cargo test 'tools::command::tests' --all-targets -- --test-threads=1` — the private runner's policy and real-process tests pass without invoking Monty.

### Task 2: Route validated command host calls

**Changes** — Add the dormant `Run` tool identity and route neutral host calls through the private command module without adding it to the active generated definitions yet. Parse exactly `command` plus optional `cwd`, normalise `cwd`, open every component through the existing no-follow workspace capability, and hand the resulting owned directory descriptor to the command supervisor. Map lexical and path-kind failures to the specified built-in error kinds, launch failures to `OSError`, and output overflow to `RuntimeError`; return zero, non-zero, and signal statuses as neutral `CommandResult` dictionaries. Keep `run` outside mutation paths and authorisation transitions so it uses ordinary scheduler slots without claiming freshness for command effects.

**Tests** (`src/tools.rs`, host adapter and scheduler unit) — Start with a direct `Run` host call that cannot yet compile. Cover missing, extra, and non-string arguments; default, root, nested, and lexically aliased `cwd`; absolute and parent denial; missing, non-directory, final-symlink, and intermediate-symlink `cwd`; exact result dictionaries; launch and overflow error mapping; unchanged authorisations after successful and failed commands; concurrent admission of independent commands; and settlement of a successful sibling after an ordinary command error. Reuse the existing batch gate rather than adding another scheduler.

**Files** — Modify: `src/tools.rs`

**Verification** — `cargo fmt --all -- --check && cargo test 'tools::tests' --all-targets -- --test-threads=1` — all host, path, scheduler, and existing filesystem tests pass while `run` remains unavailable in Monty.

### Task 3: Activate the generated Arena command contract

**Changes** — Add a singular-record result shape and the `CommandResult` definition, then activate `run` seventh in the canonical definition set. Generate its runtime wrapper, return conversion, prompt skeleton, description, and project-local command guidance from the existing metadata path. Extend Arena summaries with numeric exit and fixed signal states, using the admitted tool identity to classify the result safely. Render those states through runtime progress and transcripts without retaining commands, paths, output, or errors. Update bundled and overridden prompt assertions, keep OpenAI limited to `execute_python`, update README command semantics and authority caveats, and remove Phase 2 assertions that require `run` to be absent.

**Tests** (`src/tools.rs`, generated contract; `src/arena.rs`, real Monty integration; `src/runtime.rs`, `src/transcript.rs`, evidence; `src/prompt.rs`, prompt integration; `src/openai.rs`, provider request integration) — Start with generated-source and exact-active-set tests requiring `CommandResult` and `run`. Execute the wrapper in Monty and cover successful fields, non-zero status as a normal result, signal status as `None`, null-stdin EOF, separate stdout and stderr, malformed calls and every `cwd` exception class, ordinary overflow after a completed side effect, and Python-state commitment after overflow. Drive gathered commands through the existing batch gate to prove concurrency, settlement-order-independent call-ID association, ordered results, and sibling settlement after one error. Prove pending, numeric-exit, signal, and failed evidence is updated in place and contains none of the command, `cwd`, output, or error text. Prove bundled and overridden prompts contain each of the seven signatures once with project-local guidance, while the provider request still exposes only `execute_python`.

**Files** — Modify: `README.md`, `src/arena.rs`, `src/openai.rs`, `src/prompt.rs`, `src/runtime.rs`, `src/tools.rs`, `src/transcript.rs`

**Verification** — `cargo fmt --all -- --check && cargo check --all-targets && env -u OPENAI_API_KEY MONTY_BIN=/tmp/kiki-monty-0.0.21/bin/monty cargo test --all-targets -- --test-threads=1` — the seven-tool generated contract, normal command behaviour, evidence, prompts, provider boundary, and all earlier phases pass.

### Task 4: Preserve command cleanup through Arena recovery

**Changes** — Expose the single private lifecycle gate needed to verify command cleanup at the existing Arena attempt seam. Preserve the current biased cancellation and whole-Arena timeout ownership: dropping an in-flight host batch requests immediate group cleanup, queued calls receive no later admission, and checkpoint recovery does not claim rollback of completed command effects. Do not add a process simulator, thread a second cancellation token through `ToolHost`, or add a command-specific timeout.

**Tests** (`src/tools/command.rs`, supervisor integration; `src/arena.rs`, real Monty cancellation/recovery integration) — Start with cancellation of a running `await run(...)` and prove the owned process group reaches cleanup. Use the same lifecycle gate to cover a short test-only Arena deadline, restoration of prior Python and authorisation state, preservation of a command side effect completed before interruption, frozen path-free evidence, and cancellation after output overflow but before error publication, proving cancellation wins over `RuntimeError`. Rely on the existing scheduler tests for queued-admission behaviour and rerun existing agent/runtime cancellation and recovery tests unchanged; add no sleep-based readiness guesses.

**Files** — Modify: `src/arena.rs`, `src/tools.rs`, `src/tools/command.rs`

**Verification** — `cargo fmt --all -- --check && env -u OPENAI_API_KEY MONTY_BIN=/tmp/kiki-monty-0.0.21/bin/monty cargo test 'arena::tests' --all-targets -- --test-threads=1` — command cancellation, timeout, cleanup, evidence, and checkpoint recovery tests pass; then run the Overall validation suite.
