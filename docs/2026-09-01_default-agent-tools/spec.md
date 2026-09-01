# Spec: Default agent tools

## Context

Kiki’s model currently acts through one outer `execute_python` tool and retains Python state in a checkpointed Monty Arena. The only host capability is async `read`, documented as returning lines (`prompts/read_only_agent.md:5`). The Arena driver accepts exactly one pending future and resolves that read inline (`src/arena.rs:798`, `src/arena.rs:820`), so Monty’s existing multi-future support is unused.

Kiki’s Vision instead treats Python as the orchestration language: host functions are awaited, independent work uses `asyncio.gather()`, and Rust resolves concurrent calls in input order (`docs/VISION.md:60`, `docs/VISION.md:94`). The first self-hosting loop therefore needs a coherent inspection, mutation, and verification tool set without introducing direct model tools or harness-specific batching.

Research found Maki to be the closest async Arena reference, Pi to provide useful exact-edit and per-file mutation-ordering patterns, Codex to demonstrate concurrent execution with ordered result commitment, and smolagents to demonstrate concise generated Python tool references (`docs/research/2026-09-01_agent-tool-sets-and-arena-execution.md:16-18`). The complete decision history and recovered pre-compaction research are retained in `docs/research/2026-09-01_agent-tool-set-planning-context.md` and `docs/research/2026-09-01_agent-tool-set-planning-transcript.md`.

## Goals

- Give Arena code the seven default async tools needed to inspect, modify, and verify Kiki: `read`, `write`, `edit`, `list_dir`, `glob`, `grep`, and `run`.
- Let `asyncio.gather()` pass independent calls to Rust as one correlated batch, execute up to 16 calls concurrently, and resume Monty with deterministic call-ID association.
- Make filesystem operations predictable and difficult to misuse through workspace confinement, no-follow symlinks, complete-result bounds, exact edits, guarded whole-file writes, and atomic publication.
- Generate public Arena wrappers, basic result dataclasses, and reference-only prompt skeletons from one Monty-neutral Rust definition model.
- Preserve Kiki’s checkpoint, cancellation, model-feedback, and reliable path-free tool-call evidence semantics as host effects become concurrent and mutating.
- Keep the module narrow enough that future Rust, trusted Python, or MCP adapters can use its invocation seam without building that plugin system now.

## Non-goals

- Directly expose the seven capabilities as model tools, or add a separate batch or code-execution tool.
- Add `load_skill`, subagents, MCP integration, trusted Python plugins, or a general dynamic tool registry.
- Provide process sandboxing, command approvals, network isolation, environment filtering, interactive stdin, persistent command sessions, or background commands.
- Support Windows command execution or process-tree ownership in this slice.
- Add durable Arena checkpoints, bootstrap migrations, or external configuration through files, environment variables, or CLI options.
- Support binary file editing, image inspection, lossy file reads, fuzzy edits, line-number edits, or host-side read pagination.
- Promise rollback of completed host effects, crash-durable filesystem transactions, or race-proof replacement against arbitrary external writers.

## Key discoveries

- Kiki already exposes `read` as an async external future, but its driver accepts only one pending call ID and resolves it inline (`src/arena.rs:798-820`). The missing batch capability is in Kiki, not Monty.
- Monty 0.0.21 supports several pending futures and partial or complete `resume_futures` batches (`docs/research/2026-09-01_agent-tool-sets-and-arena-execution.md:18`, `docs/research/2026-09-01_agent-tool-sets-and-arena-execution.md:123`). Its `asyncio.gather()` preserves argument order but lacks `return_exceptions`; one failure propagates and cancels sibling Python tasks (`docs/research/2026-09-01_agent-tool-set-planning-context.md:53`).
- Kiki commits Python state after ordinary Python exceptions, but cancellation and infrastructure recovery restore the pre-execution checkpoint; neither mechanism rolls back completed host effects (`docs/research/2026-08-26_monty-feasibility.md:115-121`).
- The existing `ReadTool` already uses a workspace directory capability, rejects absolute and parent paths plus non-regular files, runs blocking I/O outside Tokio, and enforces 4 MiB plus line-count limits (`src/tools.rs:13-14`, `src/tools.rs:135-207`).
- Maki demonstrates the target Arena pattern: generated async host functions, explicit await/gather guidance, batched Monty resolution, concurrent host dispatch, and ordered correlation (`docs/research/2026-09-01_agent-tool-sets-and-arena-execution.md:16`, `docs/research/2026-09-01_agent-tool-sets-and-arena-execution.md:94`).
- Pi, Maki, and Codex all execute independent work concurrently while preserving model call order; Pi additionally serialises same-canonical-file mutations without blocking different files (`docs/research/2026-09-01_agent-tool-sets-and-arena-execution.md:84-88`).
- Basic Monty dataclasses are suitable for persistent record-shaped results, but advanced features including `frozen=True`, `InitVar`, and `__post_init__` are unsupported (`docs/research/2026-09-01_agent-tool-set-planning-context.md:55`).
- OpenAI’s `parallel_tool_calls: false` governs only the single outer `execute_python` call, not nested Python-to-Rust tool concurrency (`src/openai.rs:135`, `docs/research/2026-09-01_agent-tool-sets-and-arena-execution.md:106`).

## Proposed approach

### Tool host and Arena boundary

Replace the read-specific host path with one Kiki-owned tool host. It exposes tool definitions and accepts correlated batches of Monty-neutral `ToolCall` values, returning `ToolResult` values keyed by call ID. Arguments and results use only `None`, booleans, integers, floats, strings, lists, and string-keyed dictionaries.

`arena.rs` remains the only Monty-aware adapter. It converts function calls into Kiki values, passes each complete pending-future batch to the host, and converts results or ordinary Python exceptions back into Monty resume values.

Every accepted call in a pending batch runs to settlement even when another call returns a normal tool error; errors do not cancel active or queued siblings. Kiki resumes the pending futures once with the complete result set, after which Monty applies normal gather failure semantics. Only Arena timeout, user cancellation, or fatal recovery stops admitting queued calls and cancels active work.

Use ripgrep’s focused `ignore`, `globset`, `grep-regex`, and `grep-searcher` crates in process. Do not depend on an installed `rg` executable.

### Python surface

Generate runtime wrappers, basic dataclasses, and prompt skeletons from one typed Rust metadata model:

```python
@dataclass
class DirectoryEntry:
    path: str
    kind: str
    size_bytes: int | None

@dataclass
class GrepMatch:
    path: str
    line: int
    text: str

@dataclass
class CommandResult:
    exit_code: int | None
    stdout: str
    stderr: str

async def read(path: str) -> str: ...
async def write(path: str, content: str) -> None: ...
async def edit(path: str, old: str, new: str) -> None: ...
async def list_dir(path: str = ".") -> list[DirectoryEntry]: ...
async def glob(pattern: str, path: str = ".") -> list[DirectoryEntry]: ...
async def grep(
    pattern: str,
    path: str = ".",
    file_glob: str | None = None,
) -> list[GrepMatch]: ...
async def run(command: str, cwd: str = ".") -> CommandResult: ...
```

Bootstrap public wrappers over private `_kiki_*` host functions before taking the initial Arena checkpoint. One small result-shape vocabulary covers primitive, record, and record-list results and renders both each return annotation and its wrapper conversion; do not store a second return-type string. Explicit bootstrap versioning remains deferred while checkpoints are in-memory and tied to one binary lifetime.

### Filesystem behaviour

Root every direct filesystem tool operation at Kiki’s launch directory. Inputs and outputs use workspace-relative paths. Lexically normalise inputs by removing `.` and repeated separators, reject root/prefix and `..` components, do not case-fold or canonicalise through the filesystem, and use the resulting slash-separated relative spelling for mutation queue keys and returned paths. `read`, `write`, `edit`, `list_dir`, `glob`, and `grep` reject absolute paths, parent traversal, and traversal through symlinks. Resolve path components one at a time beneath an already-open workspace capability, refusing a symlink before opening the next component; checking only the final component is insufficient. Recursive search walks through that same no-follow boundary and uses ripgrep crates for ignore, glob, regex, and line-search policy rather than granting `WalkBuilder` ambient paths. `list_dir` and `glob` can report a symlink entry itself, but direct filesystem tools never follow it. Shell commands executed by `run` are the explicit exception and retain ordinary host path behaviour.

- `read` opens only a regular non-symlink file and returns one complete, exact UTF-8 string. An explicitly targeted terminal `other` entry raises `ValueError` without blocking on it; directories and symlinks follow the exception matrix below.
- `write` creates missing parents and creates or replaces one complete regular file. It rejects an existing symlink or `other` entry rather than replacing it; an existing directory raises `IsADirectoryError`.
- `edit` opens only an existing regular non-symlink file and performs one exact unique replacement. Reject empty `old` or identical `old`/`new`. Count overlapping occurrences at UTF-8 character boundaries, require exactly one, and fail otherwise. Both existing and resulting complete file contents must fit `FILE_MAX_BYTES`; a replacement cannot grow a valid file beyond the limit.
- `list_dir` requires a directory root and returns all immediate entries, including hidden and ignored entries, in stable path order.
- `glob` requires a directory search root and recursively matches every filesystem entry kind, including regular files, directories, symlinks, and `other` entries such as FIFOs. It applies standard project-local `.gitignore`, `.ignore`, and `.rgignore` rules, excludes hidden descendants, returns stable path order, and never descends through symlinks. An explicitly named hidden or parent-ignored root is nevertheless traversed; filtering then applies normally beneath it. Match `pattern` against slash-normalised paths relative to `path`, while returning workspace-relative entry paths; for example, `glob("nested/*.rs", path="src")` may return `src/nested/mod.rs`. A basename-only pattern such as `*.rs` matches that basename at any descendant depth. Its description calls it a filesystem-entry matcher rather than a file finder.
- `grep` accepts a regular file or directory search root. Directory traversal shares `glob`’s project-ignore and hidden-descendant filtering, searches only regular non-symlink files, and skips every other entry kind plus binary or invalid-UTF-8 content. An explicitly named hidden or parent-ignored file or directory is searched, with normal filtering beneath an explicit directory. Explicitly targeting a symlink is denied; explicitly targeting `other`, binary, or invalid-UTF-8 content raises `ValueError` rather than returning no matches. It returns one record per matching line ordered by path and one-based line number and omits line terminators from `text`. For a directory root, `file_glob` matches paths relative to that root and a basename pattern such as `*.rs` applies at any descendant depth. For a regular-file root, match `file_glob` against that file’s basename before opening or validating its content; a mismatch returns an empty list even when the excluded file would otherwise be binary or invalid UTF-8. Once the file passes that filter, the explicit-content error rules apply.

`DirectoryEntry.kind` is `file`, `directory`, `symlink`, or `other`; `size_bytes` is non-`None` only for regular files. Paths must be valid UTF-8. Apply hidden and ignore filtering to raw filesystem names before converting a candidate path for matching, searching, or returning. A filtered hidden or ignored non-UTF-8 name is skipped. If `list_dir`, `glob`, or directory `grep` must otherwise consider a non-UTF-8 entry name, the whole call raises `ValueError` rather than aliasing it through lossy conversion or returning an apparently complete result. Such paths remain accessible only through `run` in this slice.

Use a shared 4 MiB file-content limit for `read`, `write`, and `edit`. Remove the existing line-count limit. `list_dir`, `glob`, and `grep` share limits of 10,000 items and 4 MiB of UTF-8 payload; exceeding either raises without returning partial data. Payload accounting is the sum of UTF-8 byte lengths for returned string fields: `path` plus `kind` for each `DirectoryEntry`, and `path` plus `text` for each `GrepMatch`. Integers, `None`, and container overhead do not count.

### Freshness and atomic mutation

Represent whole-file authorisation as one map from normalised path to authorised content fingerprint; absence means the agent has not observed or supplied the complete current content. Replacing an existing file through `write` requires a stored fingerprint equal to the current content. Creating a new file does not require a prior read, but no-clobber publication prevents a newly appearing file from being overwritten. Successful `read` and `write` set the authorised fingerprint to the resulting current content.

An exact `edit` does not require whole-file authorisation. When the stored fingerprint equals the pre-edit content, successful edit replaces it with the post-edit fingerprint; otherwise successful edit removes any stale entry and does not authorise a later whole-file write. Failed mutations leave the map unchanged.

A read gathered in the same Monty future batch as a write cannot authorise that write. Read-transform-write operations require sequential awaits. At batch settlement, update freshness by fingerprints rather than completion order: a successful read authorises only when its observed fingerprint still equals the path’s final on-disk fingerprint, and an obsolete read can never replace a newer mutation transition. A read that actually observed the final version may authorise later batches.

Publish mutations through same-directory temporary files, preserve existing permission bits, perform the latest practical freshness check, and use no-clobber publication for new files. This guarantees atomic visibility, not crash durability, inode identity, complete extended metadata preservation, or race-proof replacement against arbitrary external writers. Parent directories created before a later failure may remain.

Use the Arena’s existing execution-attempt boundary for freshness and authoritative trace state. Each attempt starts with a working copy of the committed authorised-fingerprint map and owns its trace. Ordinary completion and Python exceptions commit the working map beside the Monty checkpoint. Cancellation, timeout, worker loss, unsafe resource failure, or another recovery path freezes the interrupted trace and discards the working map while restoring the prior Monty checkpoint. Blocking closures receive no canonical-state reference; if one publishes after its async caller is dropped, it can affect disk but cannot authorise later writes or amend captured evidence. The changed content then disagrees with the retained authorised fingerprint and requires another read.

### Concurrent dispatch

Accept Monty’s complete pending-call batch, record every valid member as pending, allow at most 16 active calls, and queue the rest. Independent calls run concurrently. `write` and `edit` calls targeting the same normalised path run sequentially in pending-call order; mutations to different paths remain independent. Reads, listings, searches, and commands are not globally locked against mutations; prompt guidance reserves gather for independent work.

Return results deterministically in Monty’s requested call-ID order, irrespective of completion order. Whole-file authorisation established by a read becomes available only after its current pending-future batch settles.

Apply per-call result bounds only. Monty’s Arena memory limit remains the aggregate backstop for large gathered results. A memory recovery can therefore restore Arena and freshness state after external effects have completed.

### Command execution

Run one non-login `/bin/sh -c` command with Kiki’s inherited environment, null standard input, and a validated workspace-relative `cwd`. A command reading stdin receives immediate EOF; interactive input is unavailable. Non-zero status returns a `CommandResult`; process-launch failures raise. A status without a numeric exit code returns `exit_code=None`.

Commands retain Kiki’s ordinary host filesystem, environment, and network permissions. Do not advertise an escape hatch to the model. The tool description and shared prompt guidance require project-local reads and effects, while this limitation remains explicit in user-facing documentation.

Each command starts in a new Unix process group. Kiki owns that group and terminates remaining members after shell exit, Arena timeout, or user cancellation. If the shell exits while a background descendant still holds an output pipe, terminate the group before waiting for pipe EOF. Cancellation always wins over command completion or output-overflow reporting: it produces the Arena cancellation outcome, not a later tool exception. Background services are unsupported. A descendant that deliberately creates a separate session can escape this best-effort group cleanup.

There is no separate command timeout. The five-minute whole-Arena wall deadline bounds Python and tools together.

Decode stdout and stderr lossily as UTF-8, replacing invalid sequences with `U+FFFD`, while counting original bytes. Limit each stream independently to 8 MiB. If either exceeds its bound, continue draining until the command settles and then raise `RuntimeError`. The diagnostic names overflowing streams, configured limits, omitted bytes, final status when available, possible completed effects, and filtering or redirection remediation. No result is assigned, so every successful `CommandResult` contains complete output.

Earlier Python state and completed host effects remain committed after this ordinary runtime error. In `asyncio.gather()`, sibling host calls may also have completed even though gather raises and assigns no sibling results.

### Python errors

Use ordinary built-in exceptions. Argument-shape failures raise `TypeError`; invalid patterns, invalid UTF-8 names or content, oversized files, `other` content entries, and invalid or non-unique edit replacements raise `ValueError`; stale or unauthorised whole-file writes, collection overflow, and command-output overflow raise `RuntimeError`; remaining filesystem and process-launch failures raise `OSError`.

Lexically invalid paths containing an absolute/root/prefix or `..` component raise `PermissionError` for every direct filesystem tool and for `run.cwd`. A missing `run.cwd` raises `FileNotFoundError`, a non-directory `cwd` raises `NotADirectoryError`, and a symlink `cwd` raises `PermissionError`. Path-kind errors for content tools are fixed rather than collapsed into one non-regular error:

| Path condition | `read` | `write` | `edit` | `grep` |
|---|---|---|---|---|
| missing terminal | `FileNotFoundError` | create file | `FileNotFoundError` | `FileNotFoundError` |
| terminal directory | `IsADirectoryError` | `IsADirectoryError` | `IsADirectoryError` | traverse directory |
| terminal symlink | `PermissionError` | `PermissionError` | `PermissionError` | `PermissionError` |
| terminal `other` | `ValueError` | `ValueError` | `ValueError` | `ValueError` |
| intermediate symlink | `PermissionError` | `PermissionError` | `PermissionError` | `PermissionError` |

For `list_dir` and `glob`, a non-directory search root raises `NotADirectoryError`, a symlink root raises `PermissionError`, and a missing root raises `FileNotFoundError`. Ordinary OS permission denial remains `PermissionError`.

A non-zero command exit is not a tool error.

### Prompt and bootstrap

Each tool definition carries typed parameters and defaults, one structured result shape, and a concise description. The result shape renders the return annotation and conversion, and the same metadata renders trusted bootstrap wrappers and a dedicated reference-only system-prompt section containing dataclasses plus async function skeletons with `...` bodies. Generated annotations must match runtime values, and the prompt tells the model not to redefine the functions.

Keep shared behavioural rules outside tool docstrings. Include one concrete example:

```python
a, b = await asyncio.gather(read("a.rs"), read("b.rs"))
```

The rules state that every host call must be awaited; gather is for independent calls expected to succeed; dependent mutations require sequential awaits; gather failures may follow completed host effects and assign no sibling results; commands must remain project-local; model-visible output should be focused; and the whole execution has the effective five-minute deadline.

Append generated tool documentation to both bundled and overridden base prompts so runtime capabilities and their signatures cannot be omitted by a prompt override. Remove the legacy `ReadCapability` model-history entry and `READ_CAPABILITY_DESCRIPTION`; otherwise every request would retain the stale line-list and read-only contract after the generated section. Generalise the bundled base prompt accordingly. Request-level tests must show every active signature exactly once for both bundled and overridden prompts, with no stale read-only or line-list instruction.

### Progress and retained evidence

Generalise the existing read-specific progress and trace into one row per valid host call. Retain rows in Monty pending-call order and update each in place from pending to a safe count or status summary despite out-of-order completion. Examples include `read · 4 KiB`, `write · replaced`, `grep · 12 matches`, `run · exit 1`, `edit · failed`, and `glob · pending`.

The trace remains reliable, display-only evidence and never enters model context. It stores no paths, arguments, shell commands, error text, tool output, or arbitrary user text. Preserve unbounded authoritative retention per execution and the existing last-16 collapsed display. Lossy live progress may project the same safe summaries; settled execution records remain authoritative through cancellation races.

### Limits and platform

Move runtime policy constants into `src/config/constants.rs`, including model identity, Arena timeout and memory, tool concurrency, file and collection limits, command output limits, and provider or accepted-content bounds. Leave retry delays, rate windows, channel capacities, presentation constants, labels, and diagnostic prose beside their implementations.

Keep these as compile-time values in this slice. The default Arena deadline is five minutes; the existing Arena memory default remains 64 MiB. Future external configuration can raise memory and per-tool limits without changing the Python interfaces.

This implementation supports macOS and Linux. Windows shell and Job Object support require a separate design.

## Key decisions and trade-offs

- Expose capabilities only inside `execute_python`; reject duplicate direct model-tool surfaces.
- Start with seven self-hosting tools; defer skills, subagents, MCP, and plugin registration.
- Use a Kiki-owned batch seam with a thin Monty adapter; reject both Monty-coupled handlers and a speculative extensible registry.
- Transfer only JSON-like values, then bootstrap real dataclasses for record-shaped Python results.
- Generate bootstrap wrappers and prompt skeletons from typed Rust metadata; reject duplicated or opaque Python definitions.
- Use complete bounded strings for `read`; reject host pagination, line decoration, and line-list results.
- Make `glob` a general filesystem-entry matcher returning `DirectoryEntry`; reject ambiguous path strings and file-only semantics.
- Require current whole-file authorisation for replacement `write`, while letting exact unique `edit` operate safely without a prior full read.
- Use content fingerprints rather than modification-time or mini-hash freshness heuristics.
- Never follow symlinks in direct filesystem tools; accept reduced convenience for a simpler workspace boundary while retaining `run` as an explicit high-authority exception.
- Use focused ripgrep crates in process; accept four dependencies to avoid an external executable protocol.
- Run independent calls concurrently with a 16-call ceiling and same-path mutation queues; reject unbounded fan-out and a global mutation lock.
- Raise when collection results exceed fixed bounds; reject partial collections that appear exhaustive.
- Keep only per-call result bounds; accept that a large gather can hit the Arena memory limit.
- Use ordinary Python exceptions; reject a Kiki-specific error hierarchy and result-union boilerplate.
- Use one-shot non-login `/bin/sh` with inherited environment and owned process groups; defer interactive, background, sandboxed, and Windows execution.
- Let the five-minute whole-Arena deadline be the sole command timeout; reject both a second `run` timeout and active-Python deadline accounting.
- Raise after an oversized command completes; accept loss of its result so incomplete output cannot be consumed as complete.
- Restore freshness state with a rolled-back Arena even though external effects remain; accept conservative rereads after uncertain execution.
- Retain ordered path-free tool summaries; reject argument retention and aggregate-only evidence.
- Keep policy in compile-time `src/config/constants.rs`; defer external configuration and bootstrap schema versioning.

## Behavioural verification

- Drive real Monty `asyncio.gather()` calls through the adapter and prove call-ID correlation plus input-order results despite reversed completion order.
- Prove every member of a normally failing batch, including calls queued beyond the 16-call ceiling, runs to settlement; prove Arena cancellation stops queued admission, freezes attempt evidence, and prevents orphaned attempt state from becoming canonical.
- Exercise exact item and string-field byte boundaries for file and collection tools, pre-edit and post-edit file limits, empty/identical replacement rejection, overlapping edit matches, lexical path aliases and denials including `run.cwd`, canonical output spelling, stable ordering, standard ignore files, filtered non-UTF-8 names, explicit hidden or parent-ignored file and directory roots, every `DirectoryEntry.kind`, per-line grep results, file-root `file_glob`, and explicit versus traversed binary files.
- Prove glob patterns are rooted at `path`, including direct-child, nested-path, and basename-only cases, while results remain workspace-relative.
- Exercise final and intermediate symlinks whose targets are both inside and outside the workspace. Prove direct filesystem search can report a symlink entry without descending, can report `other` entries such as a Unix FIFO with `size_bytes=None`, and follows the complete path-kind exception matrix. Use Unix non-UTF-8 names to prove collections fail without lossy aliases. Test `run` separately as outside this direct-filesystem guarantee.
- Cover every freshness transition: read, new write, authorised replacement, stale replacement, authorised edit, grep-based edit, same-batch read/write, concurrent read/edit in both pending and completion orders, external change, ordinary Python error, and checkpoint recovery after completed or late mutation.
- Execute generated bootstrap source in Monty and snapshot its prompt representation. At the provider-request boundary, prove bundled and overridden prompts advertise exactly the active phase’s runtime tools once and contain no legacy read contract.
- Cover command success, non-zero and signal status, null-stdin EOF, independent stdout/stderr, lossy decoding, exact output boundaries, overflow after side effects, shell exit with a pipe-holding background child, process-group cleanup, and cancellation precedence.
- Preserve existing checkpoint, model-feedback, progress-loss, transcript, collapse, memory-recovery, and credential-free all-target test coverage in every phase.

## Risks and unknowns

- `run` is a high-authority escape from filesystem confinement. Prompt guidance cannot stop model mistakes or prompt injection from reading credentials, changing host files, or using the network.
- Unix process groups clean up ordinary descendants but cannot contain a process that deliberately creates a new session or otherwise daemonises.
- Atomic replacement narrows but cannot remove the race between the final fingerprint check and rename when external writers do not cooperate.
- Ripgrep’s traversal machinery and Kiki’s capability boundary do not share one filesystem abstraction. A capability-owned component walker plus ripgrep matching policy is more implementation work than calling `WalkBuilder`, and nested ignore semantics need parity tests.
- `asyncio.gather()` lacks `return_exceptions` in Monty 0.0.21. One failed call can hide successful sibling return values even though their host effects occurred.
- Per-call bounds do not prevent many gathered results from exhausting Arena memory. Recovery restores Python and freshness state but not external effects.
- Blocking filesystem work may finish after its async future is dropped. Atomic publication and tracker rollback must leave later writes conservative when settlement is uncertain.
- Generated bootstrap code is trusted executable input. Its renderer and definition metadata require snapshot and execution tests so malformed or drifting definitions fail at startup.
- Five minutes is deliberately lenient for an infinite Python loop. User cancellation and the whole-execution deadline remain the only time controls.
- The tool-call trace remains unbounded per execution by prior design; generated batches can increase its retained memory cost.

## Dependencies

- Product direction: `docs/VISION.md:46-100`.
- Reference comparison: `docs/research/2026-09-01_agent-tool-sets-and-arena-execution.md`.
- Complete planning decisions: `docs/research/2026-09-01_agent-tool-set-planning-context.md`.
- Monty checkpoint and resource findings: `docs/research/2026-08-26_monty-feasibility.md`.
- Existing live-agent checkpoint, feedback, cancellation, and display contract: `docs/2026-08-27_live-openai-agent/spec.md`.
- Existing exact dependencies: `monty-pool = 0.0.21`, `monty-types = 0.0.21`, `cap-std = 4`, Tokio, futures, serde, and libc (`Cargo.toml:6-27`).
- New focused ripgrep crates: `ignore`, `globset`, `grep-regex`, and `grep-searcher`.

## Phase outline

This is one cohesive self-hosting capability but too broad for one reviewable implementation plan. Split it vertically so every phase produces an independently demonstrable widening of the Arena.

1. **Concurrent project inspection** — Introduce the Monty-neutral tool host, generated bootstrap and prompt reference, batched concurrent dispatch, generic retained evidence, and the read-only `read`, `list_dir`, `glob`, and `grep` surface.
2. **Safe source mutation** — Add `write` and `edit`, content-fingerprint authorisation, same-path mutation ordering, atomic publication, and freshness checkpoint recovery.
3. **One-shot command verification** — Add Unix `run`, bounded output, process-group ownership, cancellation integration, and project-local prompt guidance.

Each phase generates, bootstraps, advertises, and tests only the tools implemented by that phase. Later phases extend the active definition set; no phase exposes placeholder wrappers or prompt signatures for unavailable tools.
