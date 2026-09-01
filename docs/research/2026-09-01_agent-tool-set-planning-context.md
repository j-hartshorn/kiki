# Agent tool-set planning context

This document preserves the working design context for Kiki’s first self-hosting tool set. It complements:

- [`2026-09-01_agent-tool-sets-and-arena-execution.md`](2026-09-01_agent-tool-sets-and-arena-execution.md), the validated reference-codebase research;
- [`2026-09-01_agent-tool-set-planning-transcript.md`](2026-09-01_agent-tool-set-planning-transcript.md), the complete recovered pre-compaction user/assistant transcript and all subagent outputs.

The transcript was recovered from Pi session `01a05c3b-7a93-7007-bbd9-cc1137bcd529`. Its pre-compaction span ends at raw JSONL line 334; line 335 is the compaction event. The archive includes active and superseded branches because superseded branches contain useful research, particularly the Maki edit investigation.

## Goal

Give Kiki the smallest default tool set with which it can inspect, modify, and verify its own codebase while preserving its code-arena-first identity. The model should compose host effects as ordinary async Python rather than issue direct model tool calls.

The change must:

- expose useful, difficult-to-misuse Python interfaces;
- make `await` unavoidable in the reference signatures;
- encourage `asyncio.gather()` for independent work;
- pass all Monty pending calls to Rust as one correlated batch;
- execute independent host work concurrently in Rust;
- preserve deterministic result association and same-file mutation order;
- keep Monty types out of the tool module;
- avoid building a speculative plugin registry.

## Reference findings

### Maki

Maki is the closest execution-model reference. Its `code_execution` plugin exposes host tools as async functions inside Monty, tells the model to await them, recommends `asyncio.gather()` for independent work, receives Monty’s full pending-future batch, resolves host calls concurrently, and resumes the interpreter with correlated results.

Maki’s broader default set is intentionally not Kiki’s starting scope. Relevant implementation ideas are its batched resolver, async task execution, generated tool metadata, and file freshness tracking.

Maki does not use mini-hashes for line identity. Its read output uses decimal line numbers, its ordinary edit performs `old_str`/`new_str` replacement, its stale-read tracker records filesystem modification time, and its fallback matching uses whitespace normalisation plus Levenshtein similarity. Disabled-by-default line editing tools exist, but exact text editing is the primary interface. Kiki should not copy line numbering, fuzzy replacement, or mini-hash machinery.

### Pi

Pi’s standard coding tools are `read`, `bash`, `edit`, and `write`; its read-only set adds `grep`, `find`, and `ls`. It executes direct model calls concurrently with `Promise.all`, then serialises mutations to the same canonical file through a per-file queue. Its edit requires a unique exact match, and its write creates parent directories.

Kiki borrows the exact-edit and per-file queue ideas, but exposes them only through the Arena and lets Python replace direct tool-call batching.

### Codex

Codex assembles tools dynamically per turn. Its useful precedent is the separation between tool definition/execution and orchestration, plus concurrent execution with deterministic source-order result commitment. Its read/write parallel-safety gate is broader than Kiki needs initially.

### smolagents

Smolagents generates concise Python tool representations from signatures, annotations, and docstrings. Its persistent `CodeAgent` state resembles Kiki’s Arena, but its code tools are synchronous and it has no `await` or `asyncio.gather()` path. Kiki borrows the Python-reference presentation, not its execution model.

### Kiki and Monty

Kiki currently exposes only async `read`. `src/arena.rs` accepts exactly one pending future, awaits that read inline, and rejects other `ResolveFutures` states. The bundled prompt says to await `read` but gives no gather guidance.

Monty 0.0.21 already supports several pending external futures and `asyncio.gather()`. Gather preserves argument order in its returned list. It currently does not support keyword arguments such as `return_exceptions`; a failure propagates and cancels sibling Python tasks. The prompt should therefore reserve gather for independent calls expected to succeed, without adding a Kiki-specific gather abstraction.

Monty supports basic dataclasses, persistence, attribute access, and transfer. It rejects some advanced dataclass features, including `frozen=True`, `InitVar`, and `__post_init__`. Bootstrap classes must remain basic. Explicit bootstrap schema versioning is deferred until durable checkpoints or runtime-loaded plugins make a mismatch possible; generated bootstrap runs before Kiki takes the initial in-memory checkpoint, so code and checkpoint cannot currently drift across a binary upgrade.

OpenAI’s outer `parallel_tool_calls: false` applies to the single `execute_python` model tool and does not constrain nested Arena concurrency.

The verified baseline command was:

```sh
env -u OPENAI_API_KEY MONTY_BIN=/tmp/kiki-monty-0.0.21/bin/monty cargo test --all-targets -- --test-threads=1
```

Result: 357 passed, 0 failed, 1 ignored.

## Settled design

### Exposure and scope

Only `execute_python` is a direct model tool. The following host capabilities appear as top-level async Python functions inside the persistent Arena:

1. `read`
2. `write`
3. `edit`
4. `list_dir`
5. `glob`
6. `grep`
7. `run`

`load_skill`, `subagent`, and a general plugin registry are deferred. `list_dir` replaces the Vision document’s `list` name because Python’s builtin `list` resolves before Monty host name lookup and would make the tool unreliable.

### Kiki-owned Rust seam

The tool module owns a Monty-neutral batch boundary:

- `ToolDefinition` supplies the Python-facing name, signature/type metadata, and concise description;
- `ToolCall` carries a call ID, tool identity, and JSON-like arguments;
- `ToolResult` carries the same call ID and either a JSON-like value or a typed host error;
- `dispatch_batch` is the one Arena-facing execution operation.

Only `arena.rs` translates between Monty objects/events and these Kiki-owned values. Tool handlers parse their own private typed arguments. A future router may place Rust built-ins, trusted Python plugins, or MCP adapters behind the same boundary, but that router is not part of this change. Pure Python modules that need no host effect remain entirely inside the Arena.

The transferable `ToolValue` set is deliberately small: null/`None`, booleans, integers, floats, strings, lists, and string-keyed dictionaries.

### Python bootstrap and generated prompt reference

Kiki bootstraps public Python wrappers over private host functions. Rust still transfers JSON-like dictionaries; wrappers convert genuinely record-shaped results into basic dataclass instances.

Use dataclasses for record-shaped results, including both `list_dir` and `glob` entries:

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
```

Use primitives for simple results. `read` returns a string, and successful `write`/`edit` return `None`.

A small typed Rust metadata model is the single source for both runtime bootstrap and prompt reference. Each `ToolDefinition` contains typed parameters, defaults, return annotation, and concise description. A narrow result-shape enum covers primitive, record, and record-list conversion, which is all seven tools require. The generator renders trusted public wrappers over private `_kiki_*` host functions and separately renders the reference-only skeletons; do not store opaque pre-rendered Python fragments.

`ToolDefinition` metadata generates a dedicated system-prompt section containing these dataclass shapes and async function skeletons with concise docstrings and `...` bodies. The section is reference-only: the functions already exist and must not be redefined. Generated annotations must exactly match runtime values.

Shared behavioural rules remain outside individual tool docstrings as short rules plus one concrete example:

```python
a, b = await asyncio.gather(read("a.rs"), read("b.rs"))
```

The rules state:

- always await host functions;
- use `asyncio.gather()` only for independent calls expected to succeed;
- await dependent calls sequentially;
- a failed gather can follow already-completed host effects and does not assign sibling results;
- keep command reads and effects within the project;
- inspect and print only focused result selections;
- each execution has the effective five-minute whole-Arena deadline.

### Tool interfaces

#### `read`

```python
async def read(path: str) -> str:
    ...
```

Return the complete accepted UTF-8 file exactly, up to the shared `FILE_MAX_BYTES` policy constant, initially 4 MiB. Do not add line numbering, offsets, pagination, a separate line-count limit, or token-oriented formatting. Arena Python can search, split, and slice the retained string without putting the whole file into model context.

A successful read records a content fingerprint and establishes whole-file write authorisation for that exact version.

#### `write`

```python
async def write(path: str, content: str) -> None:
    ...
```

Create a new file or replace the complete contents of an existing regular file, subject to the shared `FILE_MAX_BYTES` policy constant, initially 4 MiB. Create missing parent directories. Publish the file atomically.

Replacing an existing file requires whole-file write authorisation for its current content fingerprint. Authorisation comes from a successful full `read`, from the agent having supplied the complete content in a successful `write`, or from a qualifying edit transition described below. If the current content differs, fail and require another read. A newly appearing file must not be silently overwritten as though it were still absent.

#### `edit`

```python
async def edit(path: str, old: str, new: str) -> None:
    ...
```

Apply one exact unique replacement to an existing UTF-8 regular file bounded by the shared `FILE_MAX_BYTES` policy constant, initially 4 MiB. Zero matches or more than one match fail without changing the file. Do not use fuzzy or line-number fallback. A prior full-file read is not required because the exact unique `old` text is the mutation precondition and unrelated current content is preserved.

Represent whole-file write authorisation as a map from normalised path to authorised content fingerprint; absence means the agent has not observed or supplied the complete current file. If an edit begins from the stored fingerprint, successful edit replaces it with the post-edit fingerprint. If no matching authorisation exists, the edit may succeed but removes any stale entry and does not authorise a later whole-file write. Failed mutations leave the map unchanged.

Whole-file authorisation established by a `read` is available only after that Monty future batch settles. For a path with any mutation call in the batch, ignore same-batch reads and apply successful mutation transitions to the batch-start map in pending mutation order. For a path without mutations, apply successful reads in pending-call order. This conservative rule avoids final-disk reconciliation and completion-order state while ensuring a read gathered with a mutation cannot authorise it. Dependent read-transform-write work must cross separate sequential awaits.

#### `list_dir`

```python
async def list_dir(path: str = ".") -> list[DirectoryEntry]:
    ...
```

List one directory non-recursively in stable order. Return workspace-relative paths directly usable by other tools. `kind` is one of `file`, `directory`, `symlink`, or `other`; `size_bytes` is present only for regular files.

Include every immediate entry, including hidden and ignored entries. Recursive project-aware filtering belongs to `glob` and `grep`.

Do not add redundant fields such as basename, extension, hidden status, modification time, permissions, executable status, symlink target, or internal observation version.

#### `glob`

```python
async def glob(pattern: str, path: str = ".") -> list[DirectoryEntry]:
    ...
```

Recursively match filesystem entries beneath an explicit workspace-relative root. Respect project ignore files, exclude hidden paths, and return stable `DirectoryEntry` records for matching regular files, directories, and symlink entries. Never descend through a symlink. Describe the tool as “Match filesystem entries by glob pattern” so its general filesystem semantics are explicit. Keep fixed project-aware defaults rather than adding hidden/ignored flags.

Unlike a shell or Python glob, Kiki returns entry records rather than ambiguous path strings because it has no separate `stat` tool. This supersedes the earlier file-only `list[str]` design.

Apply shared collection bounds of 10,000 items and 4 MiB of UTF-8 payload. If either complete-result bound is exceeded, raise an explicit exception asking the caller to narrow the root or pattern. Never return a silently partial list.

#### `grep`

```python
async def grep(
    pattern: str,
    path: str = ".",
    file_glob: str | None = None,
) -> list[GrepMatch]:
    ...
```

Search a file or directory with a regular expression. `file_glob` provides host-side file filtering without proliferating literal/case/context/limit switches. Inline regex flags such as `(?i)` express case behaviour. Context can be obtained with `read`.

Search UTF-8 text and return one `GrepMatch` per matching line, even when a line contains several occurrences. Order results by workspace-relative path and then one-based line number; omit the line terminator from `text`. During directory traversal, skip binary and invalid-UTF-8 files. If `path` explicitly names such a file, raise `ValueError` rather than misleadingly returning no matches. Apply `file_glob` relative to the explicit search root, with a basename pattern such as `*.rs` matching at any descendant depth.

Apply the shared collection bounds of 10,000 items and 4 MiB of UTF-8 payload. Exceeding either bound raises an explicit narrowing error rather than returning partial data.

#### `run`

```python
async def run(command: str, cwd: str = ".") -> CommandResult:
    ...
```

Execute one `/bin/sh -c` command in a validated workspace-relative working directory, using Kiki’s inherited environment and no login profile. Pipes, redirects, `&&`, Git, and project scripts are supported. Non-zero exit status returns normally for inspection. Launch failures raise. There is no per-command timeout or timeout parameter; the whole-Arena deadline is the sole time bound.

Each command starts in its own Unix process group. Kiki owns that whole group: on Arena timeout, user cancellation, or normal shell exit, it terminates any descendants that remain. One-shot `run` cannot create an unmanaged background service.

`run` is not yet an OS filesystem sandbox: a command has Kiki’s ordinary host permissions. Do not advertise it to the model as an escape hatch. Its docstring describes project-local use and shared prompt guidance directs all command reads and effects to remain inside the project. The specification and user-facing documentation must record the real best-effort limitation.

Command output is retained in the Arena and may therefore have a much larger bound than model-visible execution output. Decode stdout and stderr lossily as UTF-8, replacing invalid sequences with `U+FFFD`, while measuring limits against original bytes. Bound stdout and stderr independently at 8 MiB each. Continue draining a command after either bound is exceeded, discard excess bytes from the end, and raise `RuntimeError` after the command settles. The diagnostic names each overflowing stream, its configured byte limit and omitted byte count, the command’s final status when available, and states that the command ran and may have produced side effects; it tells the model to rerun with filtering or redirection. No `CommandResult` is assigned, and successful command results are always complete. Earlier Python state and completed host effects remain committed. If the call was part of `asyncio.gather()`, host effects from sibling calls may also have completed even though gather raises and assigns no sibling results.

### Atomic mutations

`write` and `edit` provide atomic visibility, not a durable filesystem transaction. Build replacement content in a temporary file in the target directory, preserve an existing file’s permission bits, perform the latest possible freshness check, and publish by rename. New files use no-clobber publication so a file appearing after validation is not overwritten. Readers observe old or new content, never a partial target.

Creating missing parent directories is not rolled back if a later write step fails. The implementation does not promise file or directory syncing across a crash, preservation of inode identity or every extended metadata field, or race-proof compare-and-swap against arbitrary external writers. A final external change can still occur between the freshness check and rename on portable filesystems; minimise and document that window rather than adding platform-specific transactions.

### Paths and symlinks

Every path-taking filesystem tool is rooted at Kiki’s launch directory. Inputs are workspace-relative. Reject absolute paths, parent traversal, and any traversal through symlinks. Returned paths are workspace-relative.

`list_dir` and `glob` may report a matched symlink entry as `kind="symlink"`, but no filesystem tool follows it and `glob` never descends through it. `grep` skips symlink files. This deliberately rejects even symlinks whose current target happens to remain within the workspace, avoiding canonicalisation races and inconsistent target changes.

`run` validates only `cwd` against this path policy. Its shell command remains the explicit initial security exception described above.

### Tool progress and retained evidence

Generalise the existing read-only trace into one ordered, path-free row for every valid host call. Retain rows in Monty’s pending-call order and replace pending outcomes in place even when calls finish out of order. Examples include `read · 4 KiB`, `write · replaced`, `grep · 12 matches`, `run · exit 1`, `edit · failed`, and `glob · pending`.

The trace remains reliable display-only evidence and never enters model context. It stores no paths, arguments, shell commands, error text, tool output, or arbitrary user text. Preserve the earlier unbounded-per-execution authoritative retention and last-16 collapsed display policy. Lossy live progress may project the same safe name/state summaries; completed execution records remain authoritative through cancellation races.

### Search implementation

Link ripgrep’s focused Rust crates directly rather than invoking an installed `rg` binary: `ignore` for project-aware traversal, `globset` for file patterns, `grep-regex` for the matcher, and `grep-searcher` for line-oriented searching and binary detection. This keeps structured results in process and removes a runtime executable dependency. It deliberately accepts the focused dependencies instead of recreating ignore semantics.

### Concurrency, ordering, and cancellation

When Monty asks to resolve several pending futures, pass the entire correlated batch to the tool module. Permit at most 16 active tool calls; queue the rest rather than reject the batch. Apply per-call result bounds only; do not add an aggregate batch-result accounting layer. A sufficiently large gathered result set can therefore reach Monty’s configurable Arena memory limit and fail the execution. This accepted simplification can leave completed external effects even though the Arena checkpoint is restored.

Independent calls execute concurrently. `write` and `edit` calls targeting the same normalised path execute sequentially in source/pending-call order. Mutations to different files, reads, listings, searches, and commands may run concurrently. Freshness settlement uses batch-start state and pending-call order rather than completion order; same-batch reads never grant authorisation for a mutated path. The prompt still tells the model not to gather dependent operations. Searches and `run` are not globally locked against mutations because their affected paths are not knowable cheaply.

Resume Monty with every call ID deterministically associated with its own result, ordered according to Monty’s requested pending-call sequence regardless of completion order.

Freshness state is checkpointed transactionally with the Arena execution. Normal completion and ordinary Python exceptions commit both Arena and tracker state. Arena timeout, user cancellation, worker loss, unsafe resource failure, or another recovery path restores the pre-execution tracker snapshot with the Python checkpoint. Reads from an abandoned execution therefore cannot authorise later writes. Completed external mutations remain on disk, but their content normally disagrees with the restored fingerprint and forces a fresh read.

One wall-clock deadline covers the complete Arena execution, including awaited tools. The initial compile-time default is five minutes. Expiry cancels pending tools, abandons the active Monty checkout, and restores the pre-execution checkpoint. External side effects completed before cancellation are not rolled back. The generated prompt states the effective deadline so the model can choose appropriate command timeouts.

### Platform scope

This first implementation supports macOS and Linux. `/bin/sh` and Unix process-group ownership are part of the declared `run` semantics. Do not add a partial Windows shell fallback or vary the default tool inventory by platform; Windows support requires a separate design using an appropriate shell and Job Objects.

### Limits and configuration seam

Do not add environment variables, command-line flags, or a configuration file in this slice. Gather runtime policy constants in `src/config/constants.rs` so there is one future external-configuration seam.

Move user-tunable policy such as:

- fixed model identity;
- Arena timeout and memory, including a memory value that can later be raised to support very large in-Arena data sets;
- tool concurrency;
- accepted file and command-output sizes, with Arena memory and command output controlled by separate policy constants;
- collection result counts/bytes;
- provider and model-content bounds.

Leave operational internals, retry/rate windows, channel capacities, display colours, scroll increments, labels, and fixed diagnostic prose beside their implementations.

Use one `FILE_MAX_BYTES` policy constant for `read`, `write`, and `edit`, initially preserving the existing 4 MiB value. Remove the existing 100,000-line limit because these tools now transfer one string rather than a line collection. Keep the separately chosen 8 MiB per-stream command-output bound. Apply shared collection limits of 10,000 items and 4 MiB of UTF-8 payload to `list_dir`, `glob`, and `grep`; either excess raises rather than returning partial data.

## Superseded or rejected directions

- Direct model exposure, or dual direct/Arena exposure, was rejected in favour of Arena-only composition.
- `load_skill` and `subagent` were deferred from the first tool set.
- A general trait/plugin registry was rejected for now; preserve only the narrow future adapter seam.
- `list` was replaced by `list_dir`.
- Prompt-only dictionaries/`TypedDict` shapes were superseded by real basic dataclasses for genuine records.
- Paginated or line-list `read` interfaces were rejected in favour of complete strings.
- The earlier file-only `glob(...) -> list[str]` decision was superseded by general filesystem glob semantics returning `list[DirectoryEntry]`; this makes entry kind explicit instead of copying ambiguous shell path strings.
- Mini-hashes, line-number edits, fuzzy replacement, and atomic multi-edit records were rejected.
- `edit` does not require prior read, but neither does it automatically grant whole-file write authorisation.
- Glob/grep hidden and ignored switches were rejected in favour of fixed project defaults.
- Collection truncation was rejected; bounds produce errors.
- A strict process sandbox for `run` was deferred. Prompt guidance is best effort.
- User login shells were rejected in favour of fixed non-login `/bin/sh`.
- The initially proposed `run(..., timeout_seconds=120)` and `CommandResult.timed_out` were removed. The five-minute whole-Arena deadline is the sole command time bound.
- Unbounded active tool fan-out was rejected in favour of a queue with 16 active calls, but aggregate batch-result accounting was also rejected; per-call bounds plus the Arena memory limit are the simpler initial policy.
- A global mutation gate was rejected in favour of per-normalised-path mutation ordering.
- An active-Python timeout that pauses during host calls was rejected as too complex. The existing whole-execution deadline model remains, with a five-minute default.
- External timeout configuration was deferred; the value is a compile-time constant for now.
- Centralising every uppercase constant was rejected. Only runtime policy moves to the constants module.

### Python errors

Use ordinary built-in Python exceptions rather than bootstrapping a Kiki-specific hierarchy or returning error records:

- `TypeError` for invalid argument shapes;
- `ValueError` for invalid patterns, non-UTF-8 content, oversized files, and zero/non-unique edit matches;
- `PermissionError` for absolute paths, parent traversal, symlink traversal, and denied filesystem access;
- `FileNotFoundError`, `IsADirectoryError`, and `NotADirectoryError` for their corresponding path failures;
- `RuntimeError` for stale or unauthorised whole-file writes, collection overflow, and command-output overflow;
- `OSError` for other filesystem and process-launch failures.

A non-zero command exit is still an ordinary `CommandResult`, not an exception.

## Open questions before drafting the specification

1. Define red-green tests for batches, ordering, freshness transitions, confinement, cancellation, prompt generation, and all seven public interfaces.
