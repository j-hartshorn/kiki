# Monty feasibility for Kiki

**Date:** 2026-08-26

**Monty version:** `0.0.21`

**Source revision:** [`2a6fd3044769a9fafbc0424af23e9d0097ba84b6`](https://github.com/pydantic/monty/tree/2a6fd3044769a9fafbc0424af23e9d0097ba84b6)

## Verdict

Proceed with Monty as Kiki's arena, using `monty-pool` and an explicitly located, exactly version-matched `monty-runtime` worker.

The core thesis is feasible: a Rust host can keep Python state across snippets, suspend for typed host calls, drive concurrent calls from `asyncio.gather()`, capture output and errors, and dump or restore the arena. A cancelled execution can be abandoned safely by killing its worker and restoring the last idle dump.

There are two product gaps, neither of which requires replacing Monty:

1. Monty exposes no public inventory of all live globals or their types, and no semantic change set. Kiki cannot yet promise a complete changed-variable summary or debugger-like variable list through `monty-pool`.
2. The sandbox has a fixed module set. Plugin-provided sandbox Python can be bootstrap source and initial bindings, but cannot currently be installed as an ordinary importable module.

The first vertical slice should not hide either gap. It should use explicit arena projections where needed and either narrow the state summary to what Kiki can prove or add the small upstream/forked introspection surface first.

## Evaluation

The investigation used Monty's current Rust crates and source rather than the Python wrapper. Monty `0.0.21` requires Rust 1.95; Kiki's pinned Rust 1.98 toolchain is compatible.

A throwaway Rust spike under `/tmp/kiki-monty-spike` used:

- `monty-pool = "=0.0.21"`;
- `monty-types = "=0.0.21"`;
- a release `monty-runtime 0.0.21` worker at an explicit path;
- Tokio for the asynchronous host driver.

The spike was intentionally not retained as product code. It answered feasibility questions rather than establishing Kiki's runtime design.

## Capability matrix

| Requirement | Result | Evidence and consequence |
| --- | --- | --- |
| Persistent globals | Supported | Two feeds shared an integer and a custom `TodoList` instance. Monty's [`MontyRepl`](https://github.com/pydantic/monty/blob/2a6fd3044769a9fafbc0424af23e9d0097ba84b6/crates/monty/src/repl.rs) preserves globals, heap, functions, classes, and source metadata without replaying earlier snippets. |
| Top-level async | Supported | The spike executed top-level `await asyncio.gather(...)`. `async def` and `await` are supported, although Monty's [`asyncio` subset](https://github.com/pydantic/monty/blob/2a6fd3044769a9fafbc0424af23e9d0097ba84b6/limitations/asyncio.md) is deliberately small. |
| Rust-backed functions | Supported | An unresolved Python call suspended as `TurnEvent::FunctionCall`; Rust inspected its name and transferable arguments, then resumed it with a value, error, or pending future. |
| Concurrent host calls | Supported | Both branches of `asyncio.gather()` reached Rust before resolution. The spike resolved them in reverse completion order and Monty returned results in Python input order. Rust, not Monty, owns actual host task scheduling. |
| Standard output and error streams | Supported | [`Checkout::feed`](https://github.com/pydantic/monty/blob/2a6fd3044769a9fafbc0424af23e9d0097ba84b6/crates/monty-pool/src/checkout.rs) streams `PrintStream` fragments to an async callback. The spike captured stdout across feeds; the same interface distinguishes stderr. |
| Python errors | Supported | Syntax, typing, and runtime errors are distinct pool errors. A runtime error left the session usable. Mutations performed before an exception remained visible, matching Python REPL semantics. |
| Transferable values | Supported with boundaries | [`MontyObject`](https://github.com/pydantic/monty/blob/2a6fd3044769a9fafbc0424af23e9d0097ba84b6/crates/monty-types/src/object.rs) covers scalar values, nested collections, datetimes, exceptions, paths, and dataclasses. Arbitrary class instances fall back to output-only representations, so domain objects need explicit transferable projections. |
| Idle checkpoints | Supported | The spike dumped a session, finished its worker, restored into a fresh worker, and read the original integer and custom object state. |
| Suspended checkpoints | Supported | `Checkout::dump` also serialises a paused call stack at a host call or unresolved-futures suspension. Restore re-announces the suspension. See [Snapshots](https://github.com/pydantic/monty/blob/2a6fd3044769a9fafbc0424af23e9d0097ba84b6/docs/snapshots.md). |
| Branching | Supported | The same opaque dump may be loaded into more than one fresh session. Kiki can therefore attach one arena dump to a session entry and restore it independently on each branch. |
| Cancellation | Supported by abandonment and restore | A dropped `monty-pool` turn is not resumable. Dropping its checkout kills the worker. The spike timed out an infinite loop, discarded that worker, restored the pre-execution dump, and continued. Kiki must treat cancellation as replacement, not continuation of a partially executed arena. |
| Resource limits | Supported with caveats | A duration limit stopped an infinite loop in the spike. Monty also supports memory, recursion, and allocation limits plus parent-side request deadlines. Limit failures can leave unsafe heap state, so Kiki must discard the session after one. |
| Global inventory | Not exposed | `MontyRepl` internally stores a name map and globals, but publicly exposes only function names. `monty-pool` exposes neither. The spike could inspect types only for names already known to Rust by executing a generated Python expression. |
| Semantic changed-variable detection | Not exposed | Monty does not report which bindings or reachable mutable objects changed. Source-level assignment analysis would miss calls such as `todos.add(...)`; serialised dump differences cannot identify the responsible name. |
| Sandbox-safe plugin modules | Not currently supported as modules | Monty has no `sys.path`, package loader, or arbitrary module import. Its [standard library set is fixed](https://github.com/pydantic/monty/blob/2a6fd3044769a9fafbc0424af23e9d0097ba84b6/limitations/modules.md). Kiki can execute trusted bootstrap source inside a new arena and inject transferable bindings. |
| Ambient host isolation | Supported by design, not certified | With no mounts or host functions, sandbox code has no filesystem, environment, network, process, FFI, `eval`, `exec`, `globals`, or `locals` capability. Monty remains experimental and has not been independently audited. See its [security model](https://github.com/pydantic/monty/blob/2a6fd3044769a9fafbc0424af23e9d0097ba84b6/docs/security.md). |
| Crash isolation | Supported by `monty-pool` | Monty recommends the subprocess pool for untrusted code because the in-process `monty` crate cannot contain interpreter aborts. A dead worker is discarded and replaced without taking down Kiki. |

## Spike scenarios

Four focused tests passed:

1. **State, output, errors, projection, and restore**
   - Defined a custom `TodoList` and an integer in one feed.
   - Mutated both in a later feed.
   - Returned the TODO object's dictionary projection across the process boundary.
   - Captured output from both feeds.
   - Raised an error after assigning another global and confirmed that the assignment survived.
   - Dumped and restored the arena into a fresh worker.

2. **Asynchronous host composition**
   - Ran `await asyncio.gather(fetch("first"), fetch("second"))` at top level.
   - Marked both Rust-side calls as futures.
   - Resolved the second call before the first.
   - Observed results in `gather()` input order.

3. **Cancellation recovery**
   - Created an idle checkpoint.
   - Started `while True: pass`.
   - Cancelled the host turn after 50 ms and discarded its checkout.
   - Restored the checkpoint in a fresh worker and continued successfully.

4. **Interpreter duration limit**
   - Configured a 20 ms execution budget.
   - Confirmed that an infinite loop returned a sandbox runtime error rather than hanging the host.

Verification commands:

```sh
cargo fmt --manifest-path /tmp/kiki-monty-spike/Cargo.toml --all -- --check
MONTY_BIN=/tmp/kiki-monty-runtime/bin/monty \
  cargo test --manifest-path /tmp/kiki-monty-spike/Cargo.toml
cargo clippy --manifest-path /tmp/kiki-monty-spike/Cargo.toml \
  --all-targets -- -D warnings
```

All four tests and all checks passed on macOS arm64 with Rust 1.98.0. The locally built release worker was 26 MB. That size is an observation, not a cross-platform packaging estimate.

## Architectural consequences

### Use the subprocess pool

Kiki should use `monty-pool`, not run `MontyRepl` inside the TUI process. The language sandbox removes ambient capabilities, but only the worker process contains stack overflows, allocator aborts, interpreter defects, and hard cancellation.

The worker path must be explicit. Falling back to `PATH` lets an unrelated or malicious binary become the sandbox. Kiki's distribution must build or bundle `monty-runtime` for each target and keep its version aligned with `monty-pool` and `monty-types`.

### Checkpoint before model-authored execution

A cancelled pool turn cannot safely resume. The minimal reliable sequence is:

1. dump the idle arena;
2. start the model-authored snippet;
3. drive host-call suspensions until completion;
4. on success, dump the new idle arena and commit its execution record;
5. on cancellation, timeout, worker crash, or protocol failure, discard the checkout and restore the earlier dump in a fresh worker.

Python mutations made before a normal Python exception remain part of the arena and should be committed with the error result. Host cancellation is different: it deliberately returns to the pre-execution checkpoint. Neither path rolls back external tool effects.

### Enforce a whole-execution deadline in Rust

Monty's `max_duration` is cumulative across the session, not renewed for each snippet. `monty-pool`'s `request_timeout` applies to one protocol turn and can restart after a host suspension. Neither alone expresses Kiki's per-execution budget.

Kiki should wrap the complete feed-and-resume driver in its own deadline and cancellation token. Expiry aborts outstanding host work, drops the checkout, and restores the pre-execution dump. Interpreter duration and memory limits plus the pool request timeout remain defence-in-depth backstops. Any interpreter resource-limit failure must also discard the arena rather than continue against potentially damaged state.

### Treat snapshots as versioned runtime artefacts

Dumps are opaque and Monty-version-specific. They carry globals, heap, code, resource configuration, and consumed execution time, but not host mounts. Kiki must record the Monty runtime version beside each arena checkpoint.

An upgrade cannot assume old checkpoints will load. Kiki should select the version-matched worker recorded with an older session whenever that session is restored. A migration may use that compatible worker to load and re-checkpoint state under a newer runtime, but must never assume the conversion is safe. If the required worker is unavailable and no explicit migration exists, Kiki preserves the human and model records while marking the arena state non-restorable. Silent incompatibility is not acceptable.

### Keep tool dispatch host-owned

Monty supplies suspension and `gather()` semantics, not a general event loop. Kiki should spawn Rust futures when calls arrive, retain their Monty call IDs, emit completion-order progress, and resume Monty with deterministic input-associated results.

Monty does not cancel host calls when a `gather()` sibling fails. Every Kiki tool invocation therefore still needs the run or execution cancellation token, and late completions need generation checks before they can affect current state.

### Use explicit projection expressions for domain objects

Custom arena classes can persist and snapshot, but they do not cross the host boundary as live objects. A plugin-provided arena object should expose a pure Python projection returning `MontyObject`-compatible data. Kiki can evaluate the registered projection after successful execution and restoration.

That evaluation is itself a Monty feed and consumes execution budget. Projection failures must be visible and must not replace the arena object with a host-side copy. The projection remains derived data.

### Do not fake complete arena introspection

Known-name probing is enough to demonstrate type inspection, but not enough to implement the vision's current wording faithfully. It cannot discover all globals, and source analysis cannot detect deep mutation through aliases or methods.

The clean next move is a narrow Monty enhancement exposed through the worker protocol:

- enumerate live global names and types without converting their values;
- ideally expose binding writes from the completed snippet;
- keep deep-object mutation separate and projection-specific rather than pretending it is generally detectable.

Until then, Kiki should describe any first-slice summary precisely, for example “top-level bindings written by this snippet”, rather than “all changed variables”. A debugger-like complete global list remains blocked on the introspection API.

### Bootstrap plugin arena code rather than importing it

The current Monty sandbox cannot import plugin-defined modules. The viable initial mechanism is deterministic bootstrap source executed when an arena is created, plus transferable initial inputs and host-backed functions. This is sufficient for a TODO-like class and binding.

Normal `import plugin_name` syntax needs an upstream module-registration facility or a maintained fork. It should not be simulated with host filesystem mounts because that weakens the capability boundary and Monty has no general package loader anyway.

## Recommendation for the first vertical slice

Proceed, with these constraints:

1. Pin all Monty crates and the worker to one exact version.
2. Bundle or otherwise locate the worker explicitly.
3. Implement one concrete `monty-pool` arena path; do not add a speculative multi-runtime abstraction.
4. Take an idle dump before each model-authored execution.
5. Drive function calls and unresolved futures in Rust under one execution-wide cancellation scope.
6. On cancellation, crash, protocol error, timeout, or resource-limit failure, discard the worker and restore the pre-execution dump.
7. Keep first-slice tool values within the directly transferable `MontyObject` subset.
8. Use bootstrap source and explicit projections for custom arena objects.
9. Resolve global introspection before claiming complete changed-variable summaries.
10. Record the Monty version with every durable arena dump.

This is a **go** for Monty and for planning the first vertical slice. It is not yet a go for durable cross-version session restoration or complete arena inspection without the additional policies and API work above.
