# Pre-spec: Subagents on unchanged Monty

## Status and purpose

Research handoff, not an approved specification or implementation plan. Capture the agreed behaviour, establish what unchanged Monty supports, and identify the decisions and experiments needed before `/skill:planning` produces a spec.

Baseline: Kiki `4dc120d`, Monty pool/types/runtime `0.0.21`. Research performed on 2026-09-05 by a Sol subagent; its executable spike was inspected and rerun by the calling agent. No product code or dependencies changed.

## Agreed intent and constraints

An agent should delegate a prompt to another complete agent with independent arena state and fresh model context, then inspect its progress and receive its final message plus explicitly returned data.

- The caller supplies a prompt and optionally a model; omission inherits the immediate caller's model.
- Prefer copying the whole arena environment, including useful Python helpers, over discovering and selectively copying variables. The precise copy point is still a decision.
- The child starts with fresh model context, not the parent's model history. Its prompt can name useful inherited variables.
- Results contain a final message and a dict-like collection of explicitly returned values. No automatic assignment into parent variables or insertion of all child activity into parent model context.
- Background invocation is awaited to start work and receive a handle; the parent then continues. A wait-for-result invocation is also required. Both must compose with normal Python control flow and `asyncio.gather()`.
- Explicit status refresh is acceptable. Cached observations must not pretend to be current; expose when they were observed.
- Children may launch children. A user setting can disable nested delegation. Generous finite limits are wanted; values and accounting scope remain undecided.
- Per-invocation read-only mode excludes `write`, `edit`, and unrestricted `run`. Descendants cannot regain authority denied to an ancestor. Local Python mutation remains allowed.
- Support cancellation and distinguish success, failure, cancellation, and timeout. Avoid automatic retries of potentially effectful delegation.
- Arenas are independent; files are shared. Tool guidance must say that arena isolation does not isolate or roll back filesystem effects.
- Retain enough identity, status, activity, and accounting for later child inspection. Keep this minimal; the modal UI is a separate future capability.
- Work within unchanged Monty. Do not require an upgrade, fork, worker-protocol extension, or in-process interpreter.
- Defer variable discovery and a variable registry. A registry adds little over naming useful bindings in the delegation prompt.
- Prefer an interface that is intuitive to agents over clever syntax. Product decisions can be discussed in groups of two or three.

## What is now established

### Whole-arena copying works at an idle checkpoint

Kiki already holds an opaque Arena Checkpoint and replaces it after an execution completes, including ordinary Python errors. During a suspended host call, that checkpoint still represents the state before the current Python execution. See [`src/arena.rs:53`](../../src/arena.rs#L53), [`src/arena.rs:540`](../../src/arena.rs#L540), [`src/arena.rs:597`](../../src/arena.rs#L597), and [`src/arena.rs:745`](../../src/arena.rs#L745).

The spike demonstrated that an idle checkpoint restored into another worker preserves a list, class, instance, function, and identity aliases. Parent and child mutations were independent. It also demonstrated that a binding created in the currently suspended execution is absent from the earlier checkpoint. How a persisted value was originally populated is irrelevant to checkpoint cloning.

A suspended dump instead restores the parent's suspended continuation. A fresh child code execution is rejected while that continuation awaits an answer. The typed pool offers no known operation to extract an independent idle environment at that point. See [Monty `checkout.rs:336`](https://github.com/pydantic/monty/blob/70fe3f5781381eb33579e45046f8cb3845953373/crates/monty-pool/src/checkout.rs#L336) and [worker `worker.rs:580`](https://github.com/pydantic/monty/blob/70fe3f5781381eb33579e45046f8cb3845953373/crates/monty-proto/src/worker.rs#L580).

The interpreter's lower-level `into_repl` is not exposed through the pool protocol and drops the suspended executor rather than committing its compiler metadata. It is not a supported solution here. See [Monty `repl.rs:889`](https://github.com/pydantic/monty/blob/70fe3f5781381eb33579e45046f8cb3845953373/crates/monty/src/repl.rs#L889) and [`repl.rs:1000`](https://github.com/pydantic/monty/blob/70fe3f5781381eb33579e45046f8cb3845953373/crates/monty/src/repl.rs#L1000). Do not manufacture an idle clone by injecting exceptions, executing the remaining parent code, parsing opaque checkpoint bytes, or rewriting Python source.

**Consequence:** the supported starting point is the checkpoint from before the invoking Python execution. This is a research recommendation, not yet the user's acceptance of that semantic restriction.

### A method-based task handle works; awaiting the handle does not

The spike demonstrated Python async instance methods, a dataclass constructed in Python from a host-returned dictionary, cached-field refresh, and persistence of that handle and its methods through dump/restore. Kiki already constructs Python records from neutral host values: [`src/tools.rs:331`](../../src/tools.rs#L331), [`src/tools.rs:566`](../../src/tools.rs#L566), and [`src/arena.rs:1172`](../../src/arena.rs#L1172).

Fresh calls to a handle's async wait method can be awaited repeatedly and gathered. Reusing one already-awaited coroutine fails. A custom `__await__` method does not make a user-defined handle awaitable: pinned Monty accepts only its coroutine, gather-future, and external-future representations. See [Monty `async_exec.rs:41`](https://github.com/pydantic/monty/blob/70fe3f5781381eb33579e45046f8cb3845953373/crates/monty/src/bytecode/vm/async_exec.rs#L41) and [`asyncio.rs:73`](https://github.com/pydantic/monty/blob/70fe3f5781381eb33579e45046f8cb3845953373/crates/monty/src/modules/asyncio.rs#L73).

**Consequence:** investigate ordinary methods such as `await task.wait()` and `await task.refresh()`, not `await task`. A field storing one coroutine would also be a poor repeated-refresh interface. The method names are illustrative, not approved signatures.

### Status can be explicitly observed, not continuously fresh

Host results become Python values; changing Rust-owned task state does not mutate a previously returned Python dictionary. A handle can retain its latest observation and replace it after an awaited refresh. The spike proved this Python mechanism, not a complete child lifecycle.

A host-provided observation timestamp can say when status was read, but cannot determine whether cached state has changed since then without contacting the host. Even refresh is authoritative only at its observation point. No task revision is needed for this basic contract.

**Consequence:** use “last observed” terminology. Do not promise a locally maintained `is_stale` flag or continuously updating age without a defined clock/update mechanism. A terminal observation can remain definitive only if terminal outcomes are immutable and the handle still identifies the same task.

### Existing gather semantics do not require a scheduler rewrite

Kiki admits calls at `FunctionCall`, dispatches the batch at `ResolveFutures`, and resumes Monty after every batch result settles: [`src/arena.rs:884`](../../src/arena.rs#L884), [`src/arena.rs:1008`](../../src/arena.rs#L1008). The spike confirmed that mixed gathered calls are announced before their resolution batch; its fake host did not test real background scheduling.

A child-start operation can start host-owned work and return promptly when dispatched. If it is gathered with a long operation, the child may run while the sibling is pending, but the enclosing gather still waits for both results. Directly awaiting the start operation avoids that wait for unrelated siblings.

**Consequence:** start with existing dispatch timing. Earlier admission-time scheduling or incremental future resolution needs a demonstrated requirement, not inclusion merely because Monty supports partial resolution.

## Recommended direction to investigate

These recommendations narrow the research, not the user's remaining choices.

1. Clone the pre-execution checkpoint. Tell the agent to finish preparing inherited data in an earlier Python execution. A dedicated spawn-only execution is not mandatory: several child starts and subsequent parent work may share the invoking execution, but all automatic inheritance comes from its starting checkpoint.
2. Pass the task prompt at invocation time. A loop can compute distinct task instructions or refer to different indices of an already-persisted collection even though its children inherit the same checkpoint. Do not imply that a newly created loop-local binding is inherited.
3. Use a Python handle with explicit wait and refresh methods over host-owned work. Avoid aliases and awaitable properties unless they solve an observed usability problem.
4. Represent cached status as an observation with a host-generated time, not a freshness guarantee.
5. Enforce permissions and task ownership in Rust, irrespective of copied wrappers, functions, IDs, or Python-mutated handle fields.

If pre-execution inheritance proves too awkward, compare it with an explicitly supplied input mapping before considering anything more elaborate. That would be a new user decision and interface parameter, not an approved feature. Do not add a variable registry as a workaround.

## Research agenda and outcome consequences

### R1. Handoff semantics and agent usability

**Understand:** whether clear prompt guidance makes pre-execution inheritance predictable enough without enforcing spawn-only code blocks.

**Exercise:** demonstrate prior-execution inheritance, exclusion of same-execution mutations/new bindings, and distinct per-call prompts over one inherited checkpoint. Review one representative model-authored handoff for misunderstandings. Loop/helper syntax does not change the copy point; concurrent lifecycle checks belong to R6.

**Outcomes:** if the rule is clear, specify one checkpoint per invoking execution and concise guidance. If it repeatedly causes wrong assumptions, present explicit input passing or an explicit handoff operation to the user. Do not silently claim invocation-time inheritance or add source analysis to infer dependencies. A scripted test proves mechanics; claims about agent intuitiveness need representative model-authored examples or must remain unproven.

### R2. Smallest intuitive invocation and observation interface

**Understand:** use one start operation plus explicit wait as the baseline. Decide whether that also satisfies the requested wait-for-result invocation, or whether a separate convenience operation earns its additional public surface.

**Exercise:** with a real gated child, show that start returns before completion, refresh reads current host state, and wait returns the final result. Distinguish the handle, status observation, and final result in the tool guidance. The spike already established method/coroutine/gather syntax; do not repeat it merely under different names.

**Outcomes:** choose one canonical completion method, not both `wait` and `result` aliases. Prefer `wait` over a surprising async reinterpretation of standard task `result()` behaviour. Do not advertise a standard `asyncio.Task` contract that Monty cannot provide. Awaiting the handle itself is ruled out by the spike. An awaitable status attribute remains unproven and has no demonstrated benefit over an ordinary method.

**Freshness decisions:** observation timestamp format and which cached fields refresh/wait updates. Terminal immutability belongs to result settlement in R3/R4. A dynamic age display can be computed by the host/UI when rendering; it is not a self-updating Python field.

### R3. Returning data alongside the final message

**Understand:** the child must nominate Python values without reproducing them through model-generated prose. Compare an explicit host-side export operation with a known result binding read after the child finishes. No global enumeration is required to read an agreed binding.

**Exercise:** message-only completion; nested dict/list/scalar data; large results remaining outside parent model context; unsupported values; missing exports; a child exporting then continuing; cancellation or failure after export; repeated waits. Determine how the final message and values become one terminal outcome.

**Outcomes:** specify accepted value types, size limits, validation errors, and when a result becomes immutable. Pending/partial exports must not masquerade as success. Keep returned data namespaced in the result. Do not transfer child functions, closures, or whole child checkpoints back into the parent.

### R4. Task ownership, cancellation, and checkpoint recovery

**Understand:** whether children belong to the invoking execution, parent run, or longer-lived session. Background work must survive the start operation returning; what happens on parent final reply, Python error, cancellation, timeout, or failed recovery is still undecided. Kiki currently owns one root run and propagates cancellation through its worker: [`src/runtime.rs:182`](../../src/runtime.rs#L182), [`src/runtime.rs:418`](../../src/runtime.rs#L418), [`src/runtime.rs:524`](../../src/runtime.rs#L524).

**Exercise:** cancel one child and its descendants without cancelling siblings; cancel the root; abandon a wait; complete a child while cancellation races; restore a parent checkpoint from before a child start; restore a checkpoint containing an existing handle; deliver late progress/completion after recovery; shut down with children running.

**Outcomes:** choose one ownership rule and define joining or cancelling unfinished children at its end. Restored Python data must not resurrect work or duplicate side effects. Specify what remains inspectable and how missing/expired handles fail. Session-independent background services should require a separate decision, not emerge accidentally.

### R5. Permissions and copied handles

**Understand:** full checkpoints also copy tool wrappers, cached external-function references, and any existing task handles. These are Python data, not authority. `ToolHost` currently has no per-agent permission policy and dispatches every active tool: [`src/tools.rs:521`](../../src/tools.rs#L521), [`src/tools.rs:922`](../../src/tools.rs#L922), [`src/tools.rs:1636`](../../src/tools.rs#L1636).

**Exercise:** denied calls through public wrappers, aliases, and private host names; read-only child tries to delegate a write-enabled grandchild; recursion disabled but a copied spawn wrapper remains; child tries to inspect, wait for, or cancel a task owned by its parent or sibling; copied scalar task IDs collide across owners; forged/mutated handle fields; allowed child controls its own descendants.

**Outcomes:** attach capability checks and owner identity to the calling arena, not its arguments. Distinguish access to the child's own descendants from inherited handles to someone else's tasks. The simplest cloning baseline retains the checkpoint's whole-file content fingerprints; these are freshness evidence, not permission grants. Verify that external file changes still invalidate them. Forcing an additional read needs a separate justification, and no policy may rely solely on omitting a function from the prompt.

### R6. Bounds, waits, and recursion

**Understand:** Kiki currently allows five minutes per entire Python execution, 64 MiB per arena, and 16 active host calls per batch; each arena has a one-worker pool. Waiting for a full child consumes the parent's execution deadline today. See [`src/config/constants.rs:6`](../../src/config/constants.rs#L6) and [`src/arena.rs:488`](../../src/arena.rs#L488).

**Exercise:** full child concurrency while a child requests a grandchild; root cancellation at capacity; a parent waiting longer than its ordinary arena deadline; limits reached by both sequential and concurrent spawning; multiple simultaneous arena copies; refreshes while execution slots are occupied.

**Outcomes:** select finite depth, concurrency, total-start, and time limits with explicit scope. Start with clear rejection at admission limits rather than prototyping a queue. Ensure wait/refresh/control work cannot deadlock behind children holding all execution slots. Decide whether child-wait time counts towards, extends, or is separately bounded from the ordinary arena deadline; do not simply remove that deadline. Numerical defaults need a justified proposal, not arbitrary precision. Exact billed-cost ceilings cannot be promised from incomplete provider usage.

### R7. Minimal inspection and accounting

**Understand:** what must be retained now for a later modal to inspect active or completed children. Current events have run/step identity but no child identity; live progress may be dropped, while authoritative records use a separate path: [`src/runtime.rs:50`](../../src/runtime.rs#L50), [`src/runtime.rs:81`](../../src/runtime.rs#L81), [`src/runtime.rs:133`](../../src/runtime.rs#L133).

**Exercise:** distinguish children with the same prompt; associate nested activity with its parent invocation; drop progress without losing terminal state; inspect a child created before opening its future view; retain failures without copying all child activity into parent model context.

**Outcomes:** choose minimal stable ownership/identity and retained activity, with a bounded retention rule. Do not build the modal, a generic event framework, or session persistence as part of this research. Observation metadata for Python and retained activity for a human view are different needs.

Mixed-model accounting also needs a decision: [`src/pricing.rs:143`](../../src/pricing.rs#L143) returns unavailable for mixed pricing identities. Preserve per-child usage and avoid double-counting parent/tree totals; do not sum token counts and price them as though all models were identical.

### R8. Model and prompt construction

**Understand:** reuse the existing agent loop without loading the parent's model history. `Agent::new` creates a fresh history, while production currently constructs one model from startup settings: [`src/agent.rs:421`](../../src/agent.rs#L421), [`src/main.rs:30`](../../src/main.rs#L30).

**Exercise:** omitted model inherits the immediate caller, including grandchildren; valid override selects a different supported model; invalid override fails before child work starts; child prompt includes applicable base instructions, tool restrictions, the delegation task, and the pre-execution inheritance warning without parent history or automatic value dumps.

**Outcomes:** identify the smallest Kiki-only model-construction seam and where child-specific prompt guidance belongs. Model inheritance is already agreed; do not reopen it as an undecided default.

## Executed feasibility checks

The isolated Rust spike used only `monty-pool = "=0.0.21"`, `monty-types = "=0.0.21"`, Tokio, and the existing subprocess worker. Its private host calls returned deterministic fixtures, not actual agents.

| Area | Executed finding | Not established by this spike |
| --- | --- | --- |
| Idle clone | Data, classes, functions, aliases, and mutation isolation passed | Every possible Python object or pending coroutine can form a useful child environment |
| Copy timing | Same-execution new binding absent from earlier checkpoint | Automatic detection of accidentally omitted inputs |
| Suspended restore | Same continuation restored; fresh feed rejected | A supported at-call idle clone |
| Handle | Python wrapper, async methods, cache update, dump/restore passed | Host task lifetime, ownership enforcement, or cancellation |
| Awaiting | Fresh method coroutines and gather passed; double-await of one coroutine and `await handle` rejected | Equivalence to a standard Python `asyncio.Task` |
| Scheduling | Both mixed gather calls announced before resolution | Real concurrent child scheduling, latency, or deadlock freedom |

Result on 2026-09-05: **22 assertions passed, 0 failed, 3 expected limitations verified**. Expected limitations were refusal to feed a restored suspension, reuse of an awaited coroutine, and unsupported custom `__await__`.

Local scratch artefacts, deliberately not product code: `/tmp/kiki-subagent-feasibility-20260905/{Cargo.toml,Cargo.lock,src/main.rs,REPORT.md,commands.sh,run.log}`. Reproduce while those temporary files remain with:

```sh
sh /tmp/kiki-subagent-feasibility-20260905/commands.sh
```

That runs the exact worker's version check, `cargo fmt -- --check`, and `cargo run --locked`. Expected worker identity: `monty-runtime 0.0.21`; expected final line: `SUMMARY assertions_passed=22 assertions_failed=0 expected_product_limitations=3`. These local artefacts are temporary evidence, not a retained regression suite. If unavailable, recreate the isolated scenarios described above against the pinned source before relying on the executable claims.

Dependency links above pin the published crates' recorded source revision, `70fe3f5781381eb33579e45046f8cb3845953373`. Earlier research may cite a different Monty source revision; use the installed exact-version source for this work.

## Handoff to the next agent

Read this brief, the cited implementation, and [the earlier Monty feasibility research](../research/2026-08-26_monty-feasibility.md). [Vision's subagent section](../VISION.md#L98) supplies intent, not a binding implementation: its transferable-data inheritance is narrower than the whole-checkpoint approach now being considered. Any eventual spec should explicitly record that difference.

Return:

1. An evidence table for R1–R8: established facts, remaining engineering checks, and decisions only the user can make.
2. At most two viable interface sketches, with their checkpoint timing, result, failure, and freshness semantics. No speculative aliases or unsupported await behaviour.
3. Recommended defaults for lifecycle and finite limits, with failure/deadlock reasoning and any measurements still needed.
4. Exact commands and results for additional experiments, clearly distinguishing scripted mechanics from observed model usability.
5. A short list of unresolved product decisions grouped into two or three questions at a time.

Before writing the final spec, resolve the copy-time contract, invocation/observation shape, result publication, ownership/cleanup, permission scope, waiting deadlines/limits, and minimal retained inspection/accounting. Prototype only behaviour whose feasibility or architectural cost remains uncertain; other cases can become concrete implementation acceptance tests. Research completion is not approval to implement.
