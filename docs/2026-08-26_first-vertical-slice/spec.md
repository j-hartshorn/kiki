# Spec: First vertical slice

## Context

Kiki currently contains only a placeholder binary and its Ratatui dependency. Monty feasibility is proven independently, but no product path yet connects user input, model actions, persistent Python execution, host tools, runtime events, or presentation.

The first vertical slice is a deterministic walking skeleton:

> A user submits a request in a minimal TUI; a scripted fake model produces multiple code actions and a final message; the code executes in one persistent subprocess Monty arena, calls a bounded Rust `read` tool, and reports progress through typed runtime events.

The slice proves the product's principal seams without introducing provider networking, filesystem mutation, command execution, durable sessions, or snapshot migration. Monty checkpoints remain in memory and support cancellation recovery. Kiki requires an explicitly configured, version-matched worker through `MONTY_BIN`.

## Goals

- Provide one functional Ratatui screen with a scrollable chronological event log, single-line prompt, run status, submission, cancellation, and exit controls.
- Drive repeated model steps through a scripted fake model which returns complete typed `code` or `message` actions.
- Execute code in a persistent `monty-pool` arena, retaining Python values across model steps and user requests.
- Expose one size-limited, workspace-relative asynchronous `read` tool which returns a complete accepted UTF-8 file as exact, sliceable lines.
- Keep model-visible observations bounded without changing the underlying arena values, using explicit Kiki-owned framing and truncation metadata.
- Keep Monty-specific types inside a concrete arena module; represent checkpoints, execution results, commands, records, and events with Kiki-owned types.
- Keep the TUI responsive by running terminal presentation and the runtime controller separately, connected through typed commands and runtime events.
- Recover cancellation or execution timeout by discarding the worker and restoring the in-memory pre-execution checkpoint.
- Verify the whole path deterministically with automated tests and an interactive fake-model demonstration.

## Non-goals

- Real LLM providers, credentials, network streaming, token accounting, retries, or provider-specific action framing.
- Durable sessions, session-tree navigation, branch selection, compaction, or restart recovery.
- Monty snapshot migration, multiple runtime bundles, automatic worker installation, or worker discovery beyond `MONTY_BIN`.
- Changed-variable summaries, complete arena introspection, structured arena projections, or a debugger-style inspector.
- Filesystem mutation, search, shell execution, subprocess management, or any tool beyond bounded `read`.
- Project-instruction discovery, prompt-resource overrides, skills, plugins, subagents, MCP, or auxiliary model work.
- Rich transcript cards, collapsed details, mouse interaction, multiline editing, syntax highlighting, or persistent TUI preferences.
- Partial model-output streaming. Only complete typed fake-model actions may execute or become final messages. Streaming moves into the first real-provider slice, where provider framing and cancellation can be tested honestly.

## Key discoveries

- The implementation baseline is deliberately empty: `src/main.rs:1` is a placeholder and Ratatui is the only dependency in `Cargo.toml:7`.
- Kiki's intended seam already separates the canonical runtime/session model from the TUI projection and typed event stream (`docs/VISION.md:134-136`).
- Persistent arena state and host-call suspension work through `monty-pool`; the feasibility spike also proved cancellation by discarding the worker and restoring the previous checkpoint (`docs/research/2026-08-26_monty-feasibility.md:13`, `docs/research/2026-08-26_monty-feasibility.md:49`).
- Each model-authored execution must begin from an idle checkpoint, and the first implementation should use one concrete Monty path rather than a speculative multi-runtime abstraction (`docs/research/2026-08-26_monty-feasibility.md:165-166`).
- Monty's session duration budget is cumulative and its request timeout covers one protocol turn, so Kiki needs an execution-wide cancellation and deadline scope around the complete feed-and-resume cycle (`docs/research/2026-08-26_monty-feasibility.md:119`).
- Monty exposes neither a complete global inventory nor a semantic change set. The slice must report only output, errors, and expression results rather than claiming changed-variable information (`docs/research/2026-08-26_monty-feasibility.md:17`).
- Partial model output must never execute, although actual partial-output streaming is deferred until a real provider exists (`docs/research/2026-08-25_pi-architectural-lessons.md:32-36`).
- The product goal requires Python values to persist across model steps and user requests, while host effects remain explicit Rust-controlled tool calls (`docs/VISION.md:52-60`).

## Proposed approach

### Application shape

`main` validates `MONTY_BIN`, enters the terminal, constructs the application modules, and starts two long-lived loops:

- the **TUI loop** owns terminal input, ephemeral presentation state, and rendering;
- the **runtime controller** accepts typed commands, enforces one active run, assigns run identifiers, manages cancellation, commits in-memory records, and publishes ordered events.

The runtime controller coordinates an Agent; it does not implement model, arena, or tool behaviour itself.

### Module responsibilities

- **Agent:** drives repeated model actions until a final message and supplies each execution result to the next model step.
- **Scripted model:** returns complete Kiki-owned `code` or `message` actions from a configurable deterministic script.
- **Monty arena:** owns the pool checkout, persistent Python state, execution driver, checkpoints, restoration, Monty conversions, and worker replacement.
- **Read tool:** confines paths to the workspace and returns a complete accepted UTF-8 file as exact lines. The arena directly recognises the sole `read` host function; a generic dispatcher is deferred until a second tool exists.
- **Runtime controller:** owns only application policy spanning a run: command ordering, active-run lifecycle, cancellation, run IDs, record commitment, and event publication.
- **TUI:** projects runtime events and committed records while owning scrolling, input, focus, and terminal lifecycle.

### Deterministic model loop

The interactive fake-model script performs multiple actions:

1. read `Cargo.toml` through the Rust tool and retain its complete lines in an arena variable;
2. inspect a selected slice in a later code action with standard Python such as `print("".join(lines[100:250]), end="")`;
3. return a final message.

The `read` tool description teaches this standard slicing pattern. Kiki does not override `print()` or give lists surprising display semantics.

Repeated user requests reuse the same arena. Partial model-output streaming remains deferred; the agent receives only complete typed actions.

### Execution and recovery

Before each code action, the arena creates an in-memory checkpoint containing opaque Monty state and the current runtime identity. It drives the sole `read` host-call suspension until execution completes.

`read(path)` returns `list[str]` with each line ending retained, so `"".join(lines)` reconstructs the accepted file exactly. It rejects absolute paths, workspace escapes, non-regular files, non-UTF-8 files, files over 4 MiB, and files over 100,000 lines rather than silently changing their contents.

Normal Python exceptions preserve partial arena mutations and produce model-visible errors. Cancellation, timeout, worker failure, protocol failure, or unsafe resource failure discard the worker and restore the pre-execution checkpoint.

Model-visible stdout, stderr, and expression results use Kiki-owned XML-like presentation markers. Raw observed content is not XML-escaped and is never parsed back into trusted state. Each observation has a 256 KiB retained-content budget split into a 192 KiB head and 64 KiB tail, plus line ceilings of 800 head lines and 200 tail lines. These are maxima, not guarantees: whichever byte or line ceiling is reached first wins.

Monty print fragments are reassembled into logical lines using the equivalent of `splitlines(keepends=True)`; a final non-empty unterminated segment counts as one line. Head and tail storage stays bounded while total line and UTF-8 byte counts continue. If a byte boundary falls inside a line, Kiki cuts only at a valid UTF-8 boundary and marks that segment as partial. For a single oversized line, the head and tail contain prefixes and suffixes of that same line. The omission marker's `lines` value counts fully omitted logical lines, `bytes` counts every omitted raw UTF-8 byte, and `partial_lines` states whether either boundary line was only partly retained.

Truncation appears in place as a self-closing omission marker between explicitly labelled head and tail sections:

```xml
<kiki_output stream="stdout" truncated="true" total_lines="1734" total_bytes="287913">
<kiki_output_head lines="800" bytes="196608" ends_mid_line="false">
raw head
</kiki_output_head>
<kiki_output_omitted lines="734" bytes="25769" partial_lines="false" />
<kiki_output_tail lines="200" bytes="65536" starts_mid_line="false">
raw tail
</kiki_output_tail>
</kiki_output>
<kiki_warning code="output_truncated" arena_state="committed">
Execution completed, but its model-visible output was truncated. The underlying arena values were not changed. Print a smaller selection to inspect the omitted data.
</kiki_warning>
```

Python errors and generated metadata remain visible outside the observation budget. Error text is capped at 32 KiB without removing its truncation warning. Prompt input is capped at 16 KiB. A model-authored execution has a ten-second whole-execution deadline and a 64 MiB Monty allocator-backed memory limit. These are fixed slice defaults exposed to tests through smaller constructor values, not user configuration.

The current arena accepts only its own runtime identity. Multiple runtime bundles and snapshot migration remain deferred.

### TUI behaviour

The runtime controller owns the canonical in-memory record sequence. Its minimal committed records are user request, completed execution, final message, cancelled run, and failed run. A completed execution record includes normal Python errors because their arena mutations commit; an abandoned execution does not commit an execution record, while its terminal cancellation or failure record remains inspectable.

The TUI and runtime communicate through typed command and event channels. `RecordAppended` events carry the committed record; its position in the one ordered in-memory sequence is canonical for this slice. Run-started, execution-started, tool-started, tool-finished, and run-settled events are live progress only and never become canonical state by themselves. The lifecycle order is run started, user request committed, zero or more execution progress sequences followed by committed execution records, exactly one terminal record, then settlement.

The single screen provides a scrollable chronological log, one-line prompt, run status, Enter to submit, Escape to cancel, and Ctrl-C to exit. Every event carries a run identifier; model-step events also carry a step identifier. Run identifiers prevent obsolete progress from changing the current display.

The terminal-owning loop uses a bounded Crossterm input poll of at most 50 ms, drains ready runtime events on every wake, and redraws only after visible state changes. Runtime progress therefore becomes visible without waiting for another key press, while no permanent frame loop is introduced.

### Verification shape

Most behaviour is tested below the terminal:

- scripted agent-loop ordering and repeated model steps;
- persistence across code actions and user requests;
- bounded and confined file reads;
- complete execution feedback;
- Python-error continuation;
- cancellation, worker replacement, and checkpoint restoration;
- stale-event rejection by run identifier.

A lightweight TUI test checks command handling and rendered state without requiring an interactive terminal. One manual demonstration runs the complete fake-model path against Kiki's own `Cargo.toml`.

## Key decisions and tradeoffs

- Use a scripted fake model before OpenAI so the first end-to-end path remains deterministic and provider failures cannot obscure runtime defects.
- Let the Agent depend directly on the concrete `ScriptedModel`; introduce a model port only when the first real provider supplies a second implementation.
- Accept only complete typed model actions. Provider streaming and provider-specific action framing move together into the first real-provider slice.
- Separate the terminal loop from a front-end-independent runtime controller using typed commands and events; reject direct TUI ownership of agent behaviour.
- Keep the runtime controller narrow. Model sequencing, Monty execution, tool dispatch, and file access remain in deeper modules.
- Keep sessions and checkpoints in memory. Restart recovery and the first durable tree format are separate work.
- Record the arena runtime identity in every checkpoint now, but support only the current runtime. Multi-version bundle resolution comes later; snapshot migration remains out of scope.
- Require an explicit `MONTY_BIN` and reject automatic builds, `PATH` lookup, or implicit worker selection.
- Return only framed stdout, stderr, expression results, generated warnings, and errors. Do not infer or claim changed-variable information.
- Return complete accepted files to the arena as exact lines; enforce safety with explicit whole-file limits rather than silently paginating or truncating the arena value.
- Truncate only model-visible observations, preserving a head, tail, in-place omission marker, and warning while committing the completed arena state.
- Expose only workspace-confined `read`; defer a generic tool dispatcher, mutation, search, and process tools until another capability requires them.
- Treat normal Python errors as committed arena executions, preserving mutations made before the exception.
- Treat cancellation and infrastructure failure as abandoned executions, replacing the worker and restoring the pre-execution checkpoint.
- Use one concrete Monty arena module rather than a speculative runtime trait or registry.
- Keep the first TUI functional but plain; defer rich transcript presentation until real runtime behaviour can shape it.

## Risks and unknowns

- **Worker lifecycle:** `monty-pool` cancellation abandons the current checkout, so recovery must prove that the old worker is terminated and a fresh checkout restores the checkpoint before another request begins.
- **Cumulative limits:** Monty's execution-duration budget spans the session. Kiki's per-execution timeout must wrap the complete feed-and-resume cycle rather than relying on Monty's duration limit alone.
- **Filesystem confinement:** lexical path checks are insufficient because symlinks and path replacement can escape a workspace. The read tool needs descriptor-based or equivalently structural confinement, not merely `canonicalize()` followed by an ordinary read.
- **Bounded data:** accepted files are limited to 4 MiB and 100,000 exact lines before entering the arena. Model-visible observations retain up to 800 head lines within 192 KiB and 200 tail lines within 64 KiB, with deterministic partial-line metadata when byte limits win. Generated warnings and Python errors remain separately bounded and visible.
- **Terminal recovery:** panic, cancellation, and normal shutdown must all restore terminal state. This needs an RAII terminal guard and tests below the real terminal integration.
- **Event ordering:** cancellation may race with output already in transit. Every event needs its run identifier, and terminal outcomes must prevent later progress from altering committed state.
- **Checkpoint cost:** the slice takes one complete opaque dump before every code action. Progressive, differential, compressed, or copy-on-write checkpoint implementations remain possible behind the arena interface, but are deferred until measurement justifies them and Monty's format can support them safely.
- **Fake-model limits:** the walking skeleton proves Kiki's internal model-action seam, not whether a real provider can reliably produce or stream those actions. That remains the principal unknown for the next slice.
- **Transfer conversion:** Monty cannot transfer every Python object directly. The demonstration uses strings, integers, lists of strings, and dictionaries needed for internal metadata; unsupported expression values must degrade to an explicit representation or error rather than crossing as hidden Monty types.

## Dependencies

### Runtime dependencies

- Rust `1.98.0`, already pinned by `rust-toolchain.toml`.
- Ratatui `0.30.2`, already present.
- Crossterm `0.29` for terminal input and Ratatui's terminal backend.
- Tokio `1` for the runtime task, asynchronous channels, deadlines, and blocking-task dispatch.
- `tokio-util` `0.7` for hierarchical cancellation tokens.
- `monty-pool = 0.0.21` and `monty-types = 0.0.21`.
- A separately installed `monty-runtime 0.0.21` executable supplied through an absolute `MONTY_BIN` path. Development uses `cargo install monty-runtime --version 0.0.21 --locked --root /tmp/kiki-monty-0.0.21`, producing `/tmp/kiki-monty-0.0.21/bin/monty`. Kiki checks that `monty --version` reports `monty-runtime 0.0.21` before creating the pool; Monty's protocol handshake remains the second compatibility check.
- `cap-std` `4` so the read tool operates relative to an opened workspace directory rather than repeatedly trusting ambient paths.
- `thiserror` `2` for narrow typed errors.

### Test dependencies

- `tempfile` `3` for workspace-confinement and bounded-read tests.
- Ratatui's test backend for rendering assertions.
- The explicit Monty worker for arena integration tests. Tests which require it fail with a clear setup error rather than silently skipping core behaviour.

### Design inputs

- `docs/VISION.md`
- `CONTEXT.md`
- `docs/research/2026-08-25_maki-tui-architecture.md`
- `docs/research/2026-08-25_pi-architectural-lessons.md`
- `docs/research/2026-08-26_monty-feasibility.md`

No provider SDK, HTTP client, persistence engine, plugin runtime, parser, or process-execution dependency belongs in this slice.

## Plan

### Overall validation

The slice is complete when all of the following hold:

1. With an exact Monty `0.0.21` worker supplied through `MONTY_BIN`, `cargo run` opens the functional TUI.
2. Submitting any request starts one deterministic run:
   - the fake model emits a complete code action;
   - Monty calls the bounded Rust `read` tool for `Cargo.toml`;
   - a later code action uses the retained arena value and prints an observable result;
   - the fake model emits a final message;
   - the run settles and another request can be submitted.
3. A second request visibly demonstrates that the same arena survived the first run.
4. Escape cancels an active run without restarting Kiki. Automated coverage proves that an interrupted infinite Python execution is discarded and the pre-execution checkpoint is restored.
5. Tool paths cannot escape the opened workspace through absolute paths, `..`, or symlinks. An accepted read places the complete file in the arena as exact lines; files over 4 MiB or 100,000 lines and non-regular or non-UTF-8 files fail explicitly without creating a partial value.
6. Printing a selected line slice uses standard Python. Output beyond 1,000 lines or 256 KiB is framed using at most 800 head lines within 192 KiB, a self-closing omission marker with exact counts, and at most 200 tail lines within 64 KiB, followed by a generated warning. Byte boundaries preserve valid UTF-8 and explicitly identify partial lines, including the single-oversized-line case. The arena value and completed execution remain committed.
7. Python exceptions remain model-visible and preserve mutations made before the exception.
8. Cancellation, timeout, worker loss, protocol failure, and unsafe resource failure share the worker-replacement path and never commit partial arena state. Automated tests exercise cancellation, timeout, worker death, and fatal allocation as representative failures.
9. The TUI mirrors only committed records as canonical history, ignores live events from obsolete run identifiers, receives runtime wakes within 50 ms without key input, and restores terminal state on ordinary exit or failure.
10. No Monty, cap-std, Crossterm, or Ratatui type crosses its owning module's interface.

Full automated verification:

```sh
cargo install monty-runtime --version 0.0.21 --locked \
  --root /tmp/kiki-monty-0.0.21
export MONTY_BIN=/tmp/kiki-monty-0.0.21/bin/monty

cargo fmt --all -- --check
cargo check --all-targets
cargo test --all-targets -- --test-threads=1
cargo clippy --all-targets -- -D warnings
```

Manual verification:

```sh
MONTY_BIN=/tmp/kiki-monty-0.0.21/bin/monty cargo run
```

Submit two requests, confirm that retained arena state advances between them, inspect the chronological execution log, and exit with Ctrl-C. Cancellation recovery is authoritative in automated tests because the normal scripted run is intentionally short.

### Task 1: Add confined full-file reads

**Changes** — Add a capability-rooted asynchronous `ReadTool`. `read(path)` accepts only a relative workspace path and returns the complete accepted UTF-8 file as `Vec<String>`, retaining every line ending so joining the lines reconstructs the original text exactly. It rejects non-regular files, files over 4 MiB, and files over 100,000 lines rather than returning a partial value. Blocking descriptor operations run outside Tokio's async workers. No generic dispatcher or tool registry is introduced.

**Tests** (`src/tools.rs`, unit) — Write failing tests first for exact reconstruction with LF, CRLF, and a missing final newline; empty files; absolute and parent paths; symlinks escaping the workspace; missing and non-regular files; invalid UTF-8; byte-limit rejection; and line-count rejection.

**Files** — Create `src/tools.rs`; modify `src/main.rs`, `Cargo.toml`, and `Cargo.lock`.

**Verification** — `cargo fmt --all -- --check && cargo test tools && cargo clippy --all-targets -- -D warnings` passes.

**Commit** — `Add confined read tool`

### Task 2: Add persistent Monty execution

**Changes** — Add the concrete arena module as the only importer of Monty crates. Validate that the absolute `MONTY_BIN` executable reports `monty-runtime 0.0.21`, create one subprocess pool checkout, initialise an idle REPL, and retain a Kiki-owned checkpoint with opaque bytes and the current runtime identity.

Execute complete code actions against the persistent REPL, capture stdout and stderr, convert final expression values into Kiki-owned observations, and expose only the single asynchronous Python function `read(path)`. The arena directly validates this function's arguments, awaits one pending read, resumes Monty with the complete line list, rejects unknown host functions, and leaves ambient OS calls unhandled. A generic tool registry and multi-future `asyncio.gather()` scheduler remain deferred.

The arena returns structured observations rather than pre-rendered prompt text. Each stdout, stderr, or expression observation retains up to 800 head lines within 192 KiB and up to 200 tail lines within 64 KiB, plus exact omission and partial-line metadata. Monty `0.0.21` emits only stdout, but Kiki keeps stderr in its observation boundary for later tools and routes it if a future worker emits it. Error text is capped at 32 KiB with explicit truncation metadata. Ordinary Python errors are execution outcomes and advance the current checkpoint.

**Tests** (`src/arena.rs`, worker-backed integration within the module) — Write failing tests first for worker-version validation; values surviving several executions; `await read(...)` returning exact lines; malformed and unknown host calls; direct ambient filesystem access being denied; worker-backed stdout and expression capture; synthetic stderr routing because Monty `0.0.21` cannot emit it; line-only truncation; byte-only truncation; simultaneous line and byte truncation; a single oversized Unicode line; exact omitted-byte and fully-omitted-line counts; ordinary Python errors preserving mutations; and incompatible checkpoint identities being rejected. Tests require `MONTY_BIN` and fail with a clear setup message when absent.

**Files** — Create `src/arena.rs`; modify `src/main.rs`, `src/tools.rs`, `Cargo.toml`, and `Cargo.lock`.

**Verification** — `MONTY_BIN=/tmp/kiki-monty-0.0.21/bin/monty cargo test arena -- --test-threads=1 && cargo fmt --all -- --check && cargo clippy --all-targets -- -D warnings` passes.

**Commit** — `Add persistent Monty arena`

### Task 3: Add arena failure recovery

**Changes** — Add the ten-second whole-execution deadline, cancellation token, 64 MiB allocator-backed memory limit, and one recovery path for abandoned or unsafe executions. Cancellation, timeout, worker death, protocol failure, failed post-error checkpointing, and fatal allocation discard the checkout, create a fresh worker, validate the checkpoint runtime identity, and restore the pre-execution state before returning. A later execution cannot start until recovery finishes.

Keep normal Python exceptions on the commit path. If a nominal runtime exception has already killed the worker, the failed post-error checkpoint moves it onto the recovery path instead of committing uncertain state.

**Tests** (`src/arena.rs`, worker-backed integration) — Write failing tests first for cancelling an infinite loop; an execution deadline; externally killing the worker where the platform exposes a process identifier; fatal allocation terminating the worker; normal Python exceptions remaining committed; restoration preserving the earlier value after each representative infrastructure failure; and no execution being accepted while recovery is incomplete. Test constructors use smaller limits where appropriate.

**Files** — Modify `src/arena.rs` and `Cargo.toml`; update `Cargo.lock` if required.

**Verification** — `MONTY_BIN=/tmp/kiki-monty-0.0.21/bin/monty cargo test arena -- --test-threads=1 && cargo fmt --all -- --check && cargo clippy --all-targets -- -D warnings` passes.

**Commit** — `Add arena failure recovery`

### Task 4: Add scripted agent loop

**Changes** — Add complete typed `code` and `message` actions, compact model-context entries, and a configurable finite `ScriptedModel`. The Agent depends directly on that concrete fake model in this slice; a provider-neutral model port is introduced with the first real provider.

Drive repeated complete actions, execute code through the arena, and append tagged execution feedback to the next model step. Render structured observations with `<kiki_output>`, `<kiki_output_head>`, the self-closing `<kiki_output_omitted />`, `<kiki_output_tail>`, `<kiki_result>`, `<kiki_error>`, and `<kiki_warning>` markers. Raw observed content remains unescaped and is never parsed back into trusted state. Script exhaustion without a final message is a deterministic fake-model failure rather than a configurable model-step policy.

The default script reads `Cargo.toml` into exact arena lines, prints a selected slice using standard Python joining, increments an arena-resident request counter, and returns a final message. The read description teaches `print("".join(lines[start:end]), end="")`; Kiki does not alter Python's list or `print()` semantics.

**Tests** (`src/model.rs`, unit; `src/agent.rs`, worker-backed) — Write failing tests first for scripted action ordering and exhaustion; code actions feeding tagged execution feedback into the next fake-model context; explicit head/omission/tail and warning framing; raw unescaped output; final messages terminating the run; Python errors remaining feedback rather than fatal failures; arena state surviving into a second request; and cancellation preventing further scripted actions.

**Files** — Create `src/model.rs` and `src/agent.rs`; modify `src/main.rs`.

**Verification** — `MONTY_BIN=/tmp/kiki-monty-0.0.21/bin/monty cargo test agent -- --test-threads=1 && cargo fmt --all -- --check && cargo clippy --all-targets -- -D warnings` passes.

**Commit** — `Add scripted agent loop`

### Task 5: Add runtime command loop

**Changes** — Add typed submit, cancel, and shutdown commands; monotonic run and model-step identifiers; committed in-memory records; and ordered runtime events. The canonical records are user request, completed execution, final message, cancelled run, and failed run. `RecordAppended` carries the committed record; sequence position supplies its order. Live run, execution, and tool lifecycle events never become canonical state by themselves.

Implement the narrow runtime controller around one Agent. It accepts one request at a time, creates the run cancellation scope, maps agent progress into events, and enforces this order: run started; user request committed; zero or more execution-start/tool-start/tool-finish sequences followed by committed execution records; exactly one final-message, cancelled-run, or failed-run record; then run settled. A normal Python-error execution commits; an abandoned execution does not, while its terminal record remains inspectable. No event for a run may follow its settlement.

While a run is active, cancel and shutdown remain responsive. Concurrent submission receives an explicit busy outcome. A later request cannot start before recovery and settlement finish. No Monty, capability-filesystem, Ratatui, or Crossterm type enters this module.

**Tests** (`src/runtime.rs`, worker-backed controller tests) — Write failing tests first for the exact successful event and record order; monotonic run and model-step identifiers; normal Python-error commitment; concurrent submission; cancellation reaching an active infinite execution; cancellation recording no partial execution; no events after settlement; a new request succeeding after recovery; shutdown waiting for settlement; and committed records remaining the source of truth independently of live progress.

**Files** — Create `src/runtime.rs`; modify `src/main.rs` and `src/agent.rs`.

**Verification** — `MONTY_BIN=/tmp/kiki-monty-0.0.21/bin/monty cargo test runtime -- --test-threads=1 && cargo fmt --all -- --check && cargo clippy --all-targets -- -D warnings` passes.

**Commit** — `Add runtime command loop`

### Task 6: Add event-driven TUI

**Changes** — Replace the placeholder program with the composition root and one functional Ratatui screen. `main` requires the absolute `MONTY_BIN`, opens the current directory as the workspace capability, constructs the modules, starts the Tokio runtime controller, and runs the terminal loop on the terminal-owning thread.

The TUI maintains only prompt text up to 16 KiB, scrolling, active-run, status, and a projection of committed records plus live progress. A bounded Crossterm poll wakes at least every 50 ms; each wake drains ready runtime events and redraws only after a visible change. Enter submits, Escape cancels, and Ctrl-C sends shutdown before leaving. Run identifiers prevent stale live events from changing the active display. An RAII guard restores the terminal on normal return, loop error, and unwinding.

Add development instructions with the exact `cargo install monty-runtime --version 0.0.21 --locked --root /tmp/kiki-monty-0.0.21` command, full verification commands, and demonstration launch.

**Tests** (`src/tui.rs`, unit with Ratatui `TestBackend`; existing module tests, end-to-end) — Write failing tests first for command generation; prompt bounds; busy-state transitions; committed-record projection; live status; chronological display; scrolling; obsolete-run events; final-message and settlement rendering; stable rendering at small terminal sizes; runtime-event wake latency without keyboard input; and guard cleanup when the loop returns an error. Run the complete scripted Agent against a temporary workspace containing `Cargo.toml`.

**Files** — Create `src/tui.rs` and `README.md`; modify `src/main.rs`, `Cargo.toml`, and `Cargo.lock`.

**Verification** — With `MONTY_BIN=/tmp/kiki-monty-0.0.21/bin/monty` exported, `cargo fmt --all -- --check && cargo check --all-targets && cargo test --all-targets -- --test-threads=1 && cargo clippy --all-targets -- -D warnings && cargo run` passes and launches the demonstration.

**Commit** — `Add event-driven TUI`
