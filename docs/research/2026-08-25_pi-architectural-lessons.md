# Research: Pi architectural lessons for Kiki

## Scope

This document examines Pi at commit `587be985a093cc74ff24a6e7a8f0f4d84fa1e7b2` as a reference implementation for Kiki. It covers the agent loop, provider and tool boundaries, event ordering, cancellation, session projection, resources, skills, extensions, and the TUI/runtime seam.

The broader Pi architecture is documented in [the existing Pi harness research](../../.research/pi/pi-agentic-harness.md). This document focuses on lessons relevant to Kiki's smaller persistent-Python design. It does not propose an implementation plan or adopt Pi's feature set wholesale.

## Summary

- Pi's strongest reusable idea is a three-part boundary: a provider-neutral turn loop, a session runtime that owns policy and durable state, and front ends that project typed events. `.research/pi/source/packages/agent/src/agent.ts:167-226` `.research/pi/source/packages/coding-agent/src/core/sdk.ts:173-342` `.research/pi/source/packages/coding-agent/src/modes/interactive/interactive-mode.ts:3158-3340`
- Event order is part of correctness, not merely telemetry. Final assistant messages precede tool execution, tool-result messages remain source ordered even when work completes concurrently, and persistence occurs at final message boundaries. `.research/pi/source/packages/agent/src/agent-loop.ts:175-255` `.research/pi/source/packages/agent/src/agent-loop.ts:489-558` `.research/pi/source/packages/coding-agent/src/core/agent-session.ts:623-757`
- Provider details stay outside the loop. The loop receives an explicit stream function and canonical messages; providers own authentication, request preparation, protocol adaptation, and normalised stream events. `.research/pi/source/packages/agent/src/types.ts:153-216` `.research/pi/source/packages/ai/src/models.ts:636-695` `.research/pi/source/packages/ai/src/types.ts:521-551`
- Pi keeps its durable session log distinct from the context projected to the model. Its parent-linked entries preserve alternate futures while an active leaf selects the branch projected into model context. Kiki should use the same tree shape from its first persisted format and make the active leaf durable. `.research/pi/source/packages/coding-agent/src/core/session-manager.ts:383-470` `.research/pi/source/packages/coding-agent/src/core/session-manager.ts:845-969`
- Pi's TUI subscribes to runtime events and builds presentation state without owning agent behaviour. Live tool components are keyed by tool-call ID, and rendering is requested after visible changes. `.research/pi/source/packages/coding-agent/src/modes/interactive/interactive-mode.ts:3158-3360` `.research/pi/source/packages/tui/src/tui.ts:331-349`
- Progressive skill disclosure is small and effective: metadata enters the initial prompt while full instructions remain in files until invoked. `.research/pi/source/packages/coding-agent/src/core/skills.ts:299-354` `.research/pi/source/packages/coding-agent/src/core/agent-session.ts:1182-1186`
- Pi's operational safeguards matter more to Kiki's first reliable loop than its advanced features: incomplete tool calls are never executed, tool failures become model-visible results, cancellation is propagated, high-volume built-in file reads and shell output are bounded, and same-file mutations are serialised. `.research/pi/source/packages/agent/src/agent-loop.ts:192-214` `.research/pi/source/packages/agent/src/agent-loop.ts:600-708` `.research/pi/source/packages/coding-agent/src/core/tools/read.ts:284-312` `.research/pi/source/packages/coding-agent/src/core/tools/output-accumulator.ts:24-107` `.research/pi/source/packages/coding-agent/src/core/tools/file-mutation-queue.ts:20-55`
- Pi's broad extension API, branch summaries and compaction, compatibility façades, and provider catalogue machinery solve mature-product problems. They are useful seams to preserve, not features Kiki needs before self-hosting. `.research/pi/source/packages/coding-agent/src/core/extensions/types.ts:1232-1516` `.research/pi/source/packages/coding-agent/src/core/session-manager.ts:845-1048` `.research/pi/source/packages/coding-agent/src/core/model-registry.ts:28-86`

## Findings

### The turn loop is narrow and injected

Pi's `Agent` owns live loop state such as messages, tools, queues, and cancellation. The lower-level loop receives a model, canonical context, an explicit stream function, message conversion, context transforms, tool hooks, and queue readers. Provider discovery, credentials, persistence, compaction, and UI are not responsibilities of that loop. Provider adapters share one canonical assistant-stream vocabulary. `.research/pi/source/packages/agent/src/agent.ts:167-226` `.research/pi/source/packages/agent/src/types.ts:153-216` `.research/pi/source/packages/agent/src/agent-loop.ts:281-331` `.research/pi/source/packages/ai/src/types.ts:521-551`

`createAgentSession()` is the composition root that resolves resources, models, tools, extensions, and session state before constructing the core `Agent`. `AgentSession` then adds coding-product policy around the loop. `.research/pi/source/packages/coding-agent/src/core/sdk.ts:173-342` `.research/pi/source/packages/coding-agent/src/core/agent-session.ts:384-535`

This boundary maps cleanly to Kiki even though its model action is Python code rather than direct tool calls. Kiki's kernel can own the `code`/`message` cycle and consume an injected provider stream; project resources, persistence, retry policy, and presentation can remain outside it.

### Complete messages are commit boundaries

Pi streams partial assistant content for display, but it does not execute tool calls until the assistant message is final. An `error` or `aborted` response closes the turn without tool execution. A response stopped by the output limit fails every included tool call because apparently valid arguments may be truncated. `.research/pi/source/packages/agent/src/agent-loop.ts:175-214` `.research/pi/source/packages/agent/src/agent-loop.ts:374-409`

The stateful `Agent` reduces each event into internal state before awaiting subscribers. A `message_end` appends the message to history; streaming deltas do not. `.research/pi/source/packages/agent/src/agent.ts:544-590`

For Kiki, the equivalent invariant is that streamed Python is display-only. The host should execute only a complete, valid final `code` action. Provider interruption, malformed framing, or output truncation must never leave a prefix eligible for execution.

### Concurrent effects retain deterministic history

Pi's default tool batch can execute concurrently. Preflight happens serially, completion events occur as calls finish, and final tool-result messages are emitted in the assistant's original call order. Tools can force the whole batch to run sequentially. `.research/pi/source/packages/agent/src/agent-loop.ts:411-558`

The built-in `edit` and `write` tools add a path-specific mutation queue. Calls affecting one canonical path are serialised while unrelated paths remain concurrent. The lock is released only when the mutation operation settles. `.research/pi/source/packages/coding-agent/src/core/tools/file-mutation-queue.ts:20-55` `.research/pi/source/packages/coding-agent/src/core/tools/edit.ts:331-340` `.research/pi/source/packages/coding-agent/src/core/tools/write.ts:201-215`

Kiki already intends `asyncio.gather()` to preserve input order. Pi shows the extra distinction required at the host boundary: live completion events may arrive in completion order, but the values resumed into Python and any model-facing summary need deterministic request order. Same-file mutation queues become relevant once external calls can truly overlap. Arbitrary process execution remains harder to make race-safe and is a reason to begin conservatively.

### Tool failures are data unless the runtime itself has failed

Tool lookup, argument preparation, validation, policy blocking, cancellation, execution, and post-processing all converge on a standard result shape. Execution exceptions become error results rather than tearing down the whole agent loop. `.research/pi/source/packages/agent/src/agent-loop.ts:600-708` `.research/pi/source/packages/agent/src/agent-loop.ts:767-796`

This distinction fits Kiki's arena model. A normal tool failure should resume Python as an explicit exception or typed failure the authored code can handle. Failure of the arena, event channel, or runtime invariant is a different class and may end the run. Keeping those classes separate avoids turning ordinary command and filesystem errors into session failures.

Pi bounds output in selected built-in tools rather than at the tool boundary, so extension tools can still return unbounded content. Kiki's host boundary is the stronger place to enforce a transferable-result limit consistently. `.research/pi/source/packages/coding-agent/src/core/tools/truncate.ts:1-95` `.research/pi/source/packages/coding-agent/src/core/tools/tool-definition-wrapper.ts:4-30`

### Model history, durable history, and display history are different projections

Pi's session entries include ordinary messages, model changes, compactions, branch summaries, extension state, and custom context messages. Plain custom entries are durable but do not enter model context. `buildContextEntries()` selects one active path and applies the latest compaction before messages are projected. `.research/pi/source/packages/coding-agent/src/core/session-manager.ts:30-126` `.research/pi/source/packages/coding-agent/src/core/session-manager.ts:383-470`

The interactive mode independently projects session and runtime events into components for assistant streaming, tool progress, errors, queued input, and custom records. `.research/pi/source/packages/coding-agent/src/modes/interactive/interactive-mode.ts:3158-3360`

Kiki needs an additional state product that Pi does not have: the live Monty arena. Arena state, durable run records, compact model context, and rich display history should have explicit projections rather than sharing one transcript type.

The parent-linked session tree is itself useful to Kiki, not surrounding machinery to postpone behind a temporary linear format. From the first persisted format, rewinding to a restorable entry and continuing should create a new branch while preserving the previous descendants. The durable active leaf determines the model-history projection and arena state to restore. External filesystem and process effects remain outside that restoration.

### Events have different semantic weight

Pi distinguishes ordered core lifecycle events, chained transforms, cancellable decisions, best-effort observations, and an untyped inter-extension event bus. These mechanisms have different awaiting and failure behaviour. `.research/pi/source/packages/agent/src/agent.ts:544-590` `.research/pi/source/packages/coding-agent/src/core/extensions/runner.ts:801-1038` `.research/pi/source/packages/coding-agent/src/core/event-bus.ts:3-25`

The distinction matters because persistence and policy hooks are correctness barriers, while TUI repaint and logging are projections. A single generic event bus would make it unclear whether a handler can transform data, block work, delay commitment, or fail independently.

Kiki's initial runtime event vocabulary can stay small, but event types should distinguish commands, ordered state transitions, and observational notifications. The TUI should consume committed state transitions and live progress without becoming an awaited dependency of the agent loop.

Pi also distinguishes low-level `agent_end` from coding-session `agent_settled`, because retries or recovery may continue after one loop run. Kiki does not need the second state until it adds automatic recovery, but should avoid overloading one completion event later. `.research/pi/source/packages/coding-agent/src/core/agent-session.ts:1085-1097`

### Cancellation has both semantic and mechanical parts

Pi passes one abort signal through provider streaming and tool execution. A partial aborted assistant message is retained as the terminal outcome rather than silently deleted. Already-started concurrent tools are awaited, so cancellation remains cooperative and each tool must actually honour the signal. `.research/pi/source/packages/agent/src/agent.ts:318-321` `.research/pi/source/packages/agent/src/agent-loop.ts:192-198` `.research/pi/source/packages/agent/src/agent-loop.ts:540-558`

The coding `bash` tool performs process-tree termination, illustrating that setting an abort flag alone is insufficient for external processes. `.research/pi/source/packages/coding-agent/src/core/tools/bash.ts:113-146`

Kiki therefore needs both a user-visible cancellation state transition and backend-specific cancellation. The [Maki TUI research](2026-08-25_maki-tui-architecture.md#cancellation-is-both-cooperative-and-generational) shows why a generation/run-ID guard remains useful: cooperative teardown cannot guarantee that no late event arrives.

### Resources are loaded through one boundary

Pi's `ResourceLoader` centralises extensions, skills, prompt templates, themes, context files, and system-prompt inputs behind one mutable loading boundary that can reload or extend its resources. `.research/pi/source/packages/coding-agent/src/core/resource-loader.ts:38-50` `.research/pi/source/packages/coding-agent/src/core/resource-loader.ts:388-543`

Skills demonstrate progressive disclosure. Discovery validates metadata and records the skill path and base directory; the prompt contains name, description, and location rather than every skill body. `.research/pi/source/packages/coding-agent/src/core/skills.ts:299-354` `.research/pi/source/packages/coding-agent/src/core/skills.ts:375-507`

Pi's project-context discovery is less suitable for Kiki as-is. It walks from the working directory to filesystem root and eagerly loads the first recognised context file from every ancestor. `.research/pi/source/packages/coding-agent/src/core/resource-loader.ts:71-156`

Kiki can use one resource-loading boundary that publishes an immutable snapshot for the current working scope. When the agent first works below that scope, the boundary can discover the nested `AGENTS.md` and publish a new versioned snapshot. The loop, TUI, tools, and skill loader consume that version rather than rescan independently.

### Pi's extension system is intentionally broad trusted code

Pi extensions can register tools, commands, renderers, providers, shortcuts, flags, and lifecycle hooks. Registration is transactional during loading, and stale contexts are invalidated when a session runtime is replaced. `.research/pi/source/packages/coding-agent/src/core/extensions/loader.ts:254-291` `.research/pi/source/packages/coding-agent/src/core/extensions/types.ts:1232-1516` `.research/pi/source/packages/coding-agent/src/core/extensions/runner.ts:543-698`

Extensions execute with the user's host permissions. Project trust gates loading but is not a sandbox. `.research/pi/source/packages/coding-agent/docs/extensions.md:110-117` `.research/pi/source/packages/coding-agent/src/core/resource-loader.ts:380-443`

Kiki's model-authored Monty code and trusted Python plugin split is a smaller and clearer boundary. Pi's stale-handle invalidation is worth retaining when arenas or sessions are replaced; its full UI/provider/command extension surface is not required for the first milestone.

## Scope discipline for Kiki

### Adopt with the first persisted format

- Use parent-linked append-only entries and persist the active leaf. Continuing from an earlier entry creates a branch rather than truncating its descendants.
- Project only the selected root-to-leaf path into model context and restore the arena from the applicable checkpoint on that path.
- Keep rewind explicitly limited to agent and arena state. Filesystem, process, network, and other external effects are recorded but not reversed.

### Defer the surrounding machinery

- Defer compaction, branch summaries, merging, and a rich tree-navigation UI. The tree data model does not require those features.
- Decide explicitly whether accepted user input or completed turns are the durability boundary when persistence arrives.
- Add retries, steering, and follow-up queues only after one-run settlement is reliable.
- Add cross-provider history normalisation when model switching becomes real.
- Add per-resource concurrency controls when parallel effects are enabled.
- Add extension reload and stale-handle machinery after the trusted Python tool boundary is stable.
- Do not copy Pi's newer V4 mutation-log machinery merely because it exists; it is not yet Pi's live coding-agent runtime. `.research/pi/source/packages/agent/src/harness/agent-harness.ts:326-502`

### Avoid copying

- Process-global provider stream fallbacks. `.research/pi/source/packages/agent/src/stream-fn.ts:3-17` `.research/pi/source/packages/agent/src/agent.ts:216-222`
- Mutable dispatch inputs after boundary validation. Pi's pre-tool hook can mutate validated arguments without another schema check. `.research/pi/source/packages/agent/src/agent-loop.ts:615-648` `.research/pi/source/packages/agent/test/agent-loop.test.ts:444-518`
- A single event mechanism for transforms, policy decisions, ordered state transitions, and best-effort notifications. `.research/pi/source/packages/coding-agent/src/core/extensions/runner.ts:801-1038`
- Deriving the inspectable display timeline from compacted model context. `.research/pi/source/packages/coding-agent/src/core/session-manager.ts:411-470` `.research/pi/source/packages/coding-agent/src/modes/interactive/interactive-mode.ts:3776-3844`
- A non-durable branch cursor if Kiki later supports rewind or branching. `.research/pi/source/packages/coding-agent/src/core/session-manager.ts:958-969` `.research/pi/source/packages/coding-agent/src/core/session-manager.ts:1360-1373`
- A broad plugin API before the trust model and core lifecycle are proven. `.research/pi/source/packages/coding-agent/src/core/extensions/types.ts:1232-1516`

## Key references

- [Pi harness architecture](../../.research/pi/pi-agentic-harness.md) — broader package, loop, extension, session, and provider study.
- `.research/pi/source/packages/agent/src/agent-loop.ts` — turn ordering and tool execution.
- `.research/pi/source/packages/agent/src/agent.ts` — live state, cancellation, and event reduction.
- `.research/pi/source/packages/agent/src/types.ts` — provider-neutral loop, tool, and event contracts.
- `.research/pi/source/packages/coding-agent/src/core/sdk.ts` — session composition root.
- `.research/pi/source/packages/coding-agent/src/core/agent-session.ts` — policy, persistence, retry, and settlement.
- `.research/pi/source/packages/coding-agent/src/core/session-manager.ts` — durable entries and model-context projection.
- `.research/pi/source/packages/coding-agent/src/core/resource-loader.ts` — resource discovery boundary.
- `.research/pi/source/packages/coding-agent/src/core/skills.ts` — skill metadata and progressive disclosure.
- `.research/pi/source/packages/coding-agent/src/core/extensions` — trusted extension lifecycle and hook semantics.
- `.research/pi/source/packages/coding-agent/src/modes/interactive/interactive-mode.ts` — runtime-event to TUI projection.
- `.research/pi/source/packages/ai/src/models.ts` — provider registration, authentication, and stream dispatch.
