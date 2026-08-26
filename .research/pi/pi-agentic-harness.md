# Research: Pi agentic harness architecture

## Scope

This document studies Pi as a reference for building another agentic harness. It covers:

- the core agent loop;
- tool definition and execution;
- extension/plugin discovery, loading, APIs, and event dispatch;
- conversation persistence as a tree;
- model-provider abstraction, authentication, and provider composition;
- the boundaries between the provider-neutral kernel, coding-agent session runtime, and front ends;
- design properties worth retaining, plus implementation costs and migration seams visible in the repository.

The evidence is from `earendil-works/pi` at commit `587be985a093cc74ff24a6e7a8f0f4d84fa1e7b2`, cloned under [`source/`](source/). The live coding-agent uses the V3 `SessionManager`; the newer V4 generic harness session layer is called out separately because it is currently an execution scaffold with most runtime operations unimplemented.

This is an architectural study, not an API tutorial. It does not catalogue every TUI component, provider-specific request field, CLI command, or built-in tool implementation.

## Summary

- Pi has a useful three-layer shape: a provider-neutral `Agent` and loop, a coding-agent `AgentSession` that owns persistence and policy, and front ends that subscribe to the session event stream. The core receives provider streaming as an injected function rather than importing model-provider logic ([`source/packages/agent/src/agent.ts:167`](source/packages/agent/src/agent.ts#L167), [`source/packages/coding-agent/src/core/sdk.ts:304`](source/packages/coding-agent/src/core/sdk.ts#L304)).
- The loop is small in concept: request a streamed assistant message, execute any tool calls, append ordered tool results, and repeat while tools or queued messages require another turn ([`source/packages/agent/src/agent-loop.ts:153`](source/packages/agent/src/agent-loop.ts#L153)). Steering is checked before the first request and after turns; follow-ups enter only when the agent would otherwise stop. An all-terminating tool batch removes tool execution as a reason for another turn, but queued messages may still continue the run; `shouldStopAfterTurn` ends it before either queue is polled ([`source/packages/agent/src/agent-loop.ts:150`](source/packages/agent/src/agent-loop.ts#L150), [`source/packages/agent/src/agent-loop.ts:216`](source/packages/agent/src/agent-loop.ts#L216), [`source/packages/agent/src/agent-loop.ts:247`](source/packages/agent/src/agent-loop.ts#L247)).
- Events are not just telemetry. Awaited, ordered transform and decision hooks are the main composition mechanism for context, prompts, provider payloads, tool policy, tool results, compaction, and tree navigation ([`source/packages/coding-agent/src/core/extensions/runner.ts:801`](source/packages/coding-agent/src/core/extensions/runner.ts#L801), [`source/packages/coding-agent/src/core/extensions/runner.ts:984`](source/packages/coding-agent/src/core/extensions/runner.ts#L984)).
- Extensions are plain TypeScript factories loaded through `jiti`. Registration is transactional during loading, and a failed extension does not prevent later extensions from loading ([`source/packages/coding-agent/src/core/extensions/loader.ts:254`](source/packages/coding-agent/src/core/extensions/loader.ts#L254), [`source/packages/coding-agent/src/core/extensions/loader.ts:545`](source/packages/coding-agent/src/core/extensions/loader.ts#L545)). Packages bundle extensions alongside skills, prompts, and themes through a small manifest or conventional directories ([`source/packages/coding-agent/src/core/pi-manifest.ts:4`](source/packages/coding-agent/src/core/pi-manifest.ts#L4)).
- Sessions are append-oriented JSONL trees. Every entry points to a parent, while the active leaf determines the root-to-leaf branch projected into model context. Branching moves the leaf and preserves old descendants ([`source/packages/coding-agent/src/core/session-manager.ts:1298`](source/packages/coding-agent/src/core/session-manager.ts#L1298), [`source/packages/coding-agent/src/core/session-manager.ts:1360`](source/packages/coding-agent/src/core/session-manager.ts#L1360)).
- Provider support is separated from the loop. A `Provider` owns auth, model discovery/filtering, and stream adapters; a `Models` collection resolves auth and dispatches by `model.provider`; coding-agent adds file configuration, cached catalogues, and extension overlays through `ModelRuntime` ([`source/packages/ai/src/models.ts:97`](source/packages/ai/src/models.ts#L97), [`source/packages/coding-agent/src/core/model-runtime.ts:172`](source/packages/coding-agent/src/core/model-runtime.ts#L172)).
- Pi’s best reusable quality is not any single feature. It is the separation of execution, durable session state, presentation, and provider implementations while preserving strict ordering at their boundaries. The main cost is a very broad extension compatibility surface, especially around UI and session replacement ([`source/packages/coding-agent/src/core/extensions/types.ts:1232`](source/packages/coding-agent/src/core/extensions/types.ts#L1232)).

## 1. Architectural map

### 1.1 Packages and responsibilities

| Layer | Main responsibility | Primary evidence |
|---|---|---|
| `packages/ai` | Canonical model/message/tool types, provider contracts, auth, and protocol adapters | [`source/packages/ai/src/types.ts:427`](source/packages/ai/src/types.ts#L427), [`source/packages/ai/src/models.ts:97`](source/packages/ai/src/models.ts#L97) |
| `packages/agent` | Stateful provider-neutral agent, turn loop, queues, tool protocol, core lifecycle events | [`source/packages/agent/src/agent.ts:167`](source/packages/agent/src/agent.ts#L167), [`source/packages/agent/src/agent-loop.ts:95`](source/packages/agent/src/agent-loop.ts#L95) |
| `packages/coding-agent` core | Session persistence, extensions, resources, retries, compaction, provider composition, coding tools | [`source/packages/coding-agent/src/core/sdk.ts:173`](source/packages/coding-agent/src/core/sdk.ts#L173), [`source/packages/coding-agent/src/core/agent-session.ts:384`](source/packages/coding-agent/src/core/agent-session.ts#L384) |
| `packages/coding-agent` modes | TUI, print/JSON, and RPC adapters over `AgentSession` | [`source/packages/coding-agent/src/modes/print-mode.ts:74`](source/packages/coding-agent/src/modes/print-mode.ts#L74), [`source/packages/coding-agent/src/modes/rpc/rpc-mode.ts:317`](source/packages/coding-agent/src/modes/rpc/rpc-mode.ts#L317) |

`createAgentSession()` is the composition root for one session. It resolves restored session context, resources, model state, tools, settings, and extension runtime, then creates the provider-neutral `Agent` and wraps it in `AgentSession` ([`source/packages/coding-agent/src/core/sdk.ts:173`](source/packages/coding-agent/src/core/sdk.ts#L173), [`source/packages/coding-agent/src/core/sdk.ts:304`](source/packages/coding-agent/src/core/sdk.ts#L304)).

`AgentSessionRuntime` sits one level higher when the active session or working directory can change. It tears down the old session-scoped runtime and recreates services for the replacement session rather than trying to mutate all old objects in place ([`source/packages/coding-agent/src/core/agent-session-runtime.ts:167`](source/packages/coding-agent/src/core/agent-session-runtime.ts#L167), [`source/packages/coding-agent/src/core/agent-session-runtime.ts:196`](source/packages/coding-agent/src/core/agent-session-runtime.ts#L196)).

### 1.2 Why the split matters

The core loop knows about a selected model but does not know how model catalogues, credentials, persisted sessions, extensions, or UI work. It receives `streamFn`, `convertToLlm`, context transforms, tool hooks, and queue readers through configuration ([`source/packages/agent/src/types.ts:153`](source/packages/agent/src/types.ts#L153), [`source/packages/agent/src/agent.ts:396`](source/packages/agent/src/agent.ts#L396)).

Coding-agent can therefore add policy around the loop without forking it:

- extension context transformation is attached through `transformContext`;
- provider dispatch comes from `ModelRuntime.streamSimple()`;
- tool gates and result transforms become `beforeToolCall` and `afterToolCall`;
- retries and overflow recovery call `agent.continue()` after changing live context;
- persistence listens to final message events ([`source/packages/coding-agent/src/core/sdk.ts:314`](source/packages/coding-agent/src/core/sdk.ts#L314), [`source/packages/coding-agent/src/core/agent-session.ts:486`](source/packages/coding-agent/src/core/agent-session.ts#L486), [`source/packages/coding-agent/src/core/agent-session.ts:623`](source/packages/coding-agent/src/core/agent-session.ts#L623)).

## 2. The core loop

### 2.1 Entry points and state ownership

`Agent` is a stateful wrapper around two low-level functions:

- `agentLoop()` adds one or more prompt messages and starts a run;
- `agentLoopContinue()` starts from existing context, chiefly for retry and recovery paths ([`source/packages/agent/src/agent-loop.ts:31`](source/packages/agent/src/agent-loop.ts#L31), [`source/packages/agent/src/agent-loop.ts:64`](source/packages/agent/src/agent-loop.ts#L64)).

`Agent` owns the current transcript, active tools, selected model, thinking level, pending tool-call IDs, abort controller, and steering/follow-up queues. Before a low-level run it snapshots system prompt, messages, and tools, preventing arbitrary top-level array replacement from changing the already-started call ([`source/packages/agent/src/agent.ts:194`](source/packages/agent/src/agent.ts#L194), [`source/packages/agent/src/agent.ts:388`](source/packages/agent/src/agent.ts#L388)). A `prepareNextTurn` hook permits deliberate changes between turns ([`source/packages/agent/src/agent-loop.ts:226`](source/packages/agent/src/agent-loop.ts#L226)).

### 2.2 Run and turn algorithm

A fresh run emits:

1. `agent_start`;
2. `turn_start`;
3. `message_start` and `message_end` for each supplied prompt;
4. streamed assistant message events;
5. tool lifecycle and tool-result message events when tools are called;
6. `turn_end`;
7. another turn if tool calls or queued messages require one, unless an explicit stop path applies;
8. `agent_end` ([`source/packages/agent/src/agent-loop.ts:95`](source/packages/agent/src/agent-loop.ts#L95), [`source/packages/agent/src/agent-loop.ts:153`](source/packages/agent/src/agent-loop.ts#L153)).

There are two explicit stop controls. A tool or tool-result hook may return `terminate: true`; only when every finalised result in that batch terminates does tool execution cease to require another model call. Steering or follow-up messages can still continue the run. Independently, `shouldStopAfterTurn` runs before either message queue is polled and can end the run after any turn ([`source/packages/agent/src/agent-loop.ts:216`](source/packages/agent/src/agent-loop.ts#L216), [`source/packages/agent/src/agent-loop.ts:247`](source/packages/agent/src/agent-loop.ts#L247), [`source/packages/agent/src/agent-loop.ts:582`](source/packages/agent/src/agent-loop.ts#L582)).

Before each model call the loop:

1. optionally transforms application-level `AgentMessage[]`;
2. converts those messages to provider-compatible `Message[]`;
3. builds `{ systemPrompt, messages, tools }`;
4. resolves a current API key, allowing expiring credentials;
5. invokes the injected stream function;
6. maps canonical provider stream events into message lifecycle events ([`source/packages/agent/src/agent-loop.ts:281`](source/packages/agent/src/agent-loop.ts#L281)).

The canonical assistant stream has start, text/thinking/tool-call block updates, and terminal done/error events. The same stream contract is implemented by protocol-specific adapters in `packages/ai` ([`source/packages/ai/src/types.ts:521`](source/packages/ai/src/types.ts#L521), [`source/packages/ai/src/api/anthropic-messages.ts:501`](source/packages/ai/src/api/anthropic-messages.ts#L501), [`source/packages/ai/src/api/openai-completions.ts:361`](source/packages/ai/src/api/openai-completions.ts#L361)).

### 2.3 Steering and follow-up queues

The loop has two intentionally different interruption semantics:

- **steering** is polled before the first provider request and after completed turns, then inserted before the next request;
- **follow-up** is polled only when no tool calls or steering remain and the agent would otherwise stop ([`source/packages/agent/src/agent-loop.ts:163`](source/packages/agent/src/agent-loop.ts#L163), [`source/packages/agent/src/agent-loop.ts:247`](source/packages/agent/src/agent-loop.ts#L247)).

Each queue drains one message at a time by default and can be configured to drain all queued messages together ([`source/packages/agent/src/types.ts:45`](source/packages/agent/src/types.ts#L45), [`source/packages/agent/src/agent.ts:231`](source/packages/agent/src/agent.ts#L231)).

This preserves a valid assistant/tool-result sequence. A steering message does not interrupt a tool batch halfway through. Coding-agent similarly defers extension-injected messages so they cannot appear between an assistant tool call and its required tool result ([`source/packages/coding-agent/src/core/agent-session.ts:1483`](source/packages/coding-agent/src/core/agent-session.ts#L1483), [`source/packages/coding-agent/src/core/agent-session.ts:1509`](source/packages/coding-agent/src/core/agent-session.ts#L1509)).

### 2.4 Failure and settlement semantics

An assistant message ending with `error` or `aborted` closes the turn and run without tool execution ([`source/packages/agent/src/agent-loop.ts:192`](source/packages/agent/src/agent-loop.ts#L192)). An exception escaping the low-level loop is converted into a synthetic assistant error/aborted message followed by `turn_end` and `agent_end`, provided recovery event dispatch itself succeeds ([`source/packages/agent/src/agent.ts:486`](source/packages/agent/src/agent.ts#L486)).

Core `Agent` listeners are awaited in subscription order after internal state is reduced, but their errors are not isolated. A throwing listener prevents later listeners from seeing that event and can also interrupt synthetic failure dispatch, so core subscriptions are control-flow participants rather than best-effort observers ([`source/packages/agent/src/agent.ts:529`](source/packages/agent/src/agent.ts#L529), [`source/packages/agent/src/agent.ts:584`](source/packages/agent/src/agent.ts#L584)). `waitForIdle()` resolves only after `agent_end` listeners have settled ([`source/packages/agent/src/agent.ts:240`](source/packages/agent/src/agent.ts#L240), [`source/packages/agent/src/agent.ts:529`](source/packages/agent/src/agent.ts#L529)).

Coding-agent adds a second settlement level. `agent_end` ends one low-level run, while `agent_settled` means no automatic retry, overflow compaction/retry, or queued continuation remains ([`source/packages/coding-agent/src/core/agent-session.ts:609`](source/packages/coding-agent/src/core/agent-session.ts#L609), [`source/packages/coding-agent/src/core/agent-session.ts:1085`](source/packages/coding-agent/src/core/agent-session.ts#L1085)).

## 3. Tools

### 3.1 Core contract

A core `AgentTool` contains:

- name, label, description, and TypeBox parameter schema;
- optional pre-validation argument preparation;
- `execute(toolCallId, params, signal, onUpdate)`;
- optional `executionMode: "sequential"` ([`source/packages/agent/src/types.ts:385`](source/packages/agent/src/types.ts#L385)).

Coding-agent has a richer `ToolDefinition` that adds extension context, prompt metadata, and TUI renderers. A thin wrapper adapts it to `AgentTool`, keeping UI details out of the loop contract ([`source/packages/coding-agent/src/core/tools/tool-definition-wrapper.ts:4`](source/packages/coding-agent/src/core/tools/tool-definition-wrapper.ts#L4)).

The built-in registry includes `read`, `bash`, `powershell`, `edit`, `write`, `grep`, `find`, and `ls`; the normal default active set is `read`, `bash`, `edit`, and `write` ([`source/packages/coding-agent/src/core/tools/index.ts:93`](source/packages/coding-agent/src/core/tools/index.ts#L93), [`source/packages/coding-agent/src/core/tools/index.ts:182`](source/packages/coding-agent/src/core/tools/index.ts#L182)).

### 3.2 Execution pipeline

For each model-emitted tool call Pi:

1. emits `tool_execution_start`, including for invalid or blocked calls;
2. finds the named tool;
3. optionally reshapes old argument forms with `prepareArguments`;
4. validates arguments against the current schema;
5. runs the `beforeToolCall` decision hook;
6. either produces an immediate error result for lookup, preparation, validation, blocking, or abort failures, or executes the prepared tool with cancellation and progress callbacks;
7. only for an executed tool, runs the `afterToolCall` transform hook;
8. emits `tool_execution_end`;
9. emits a standard `toolResult` transcript message ([`source/packages/agent/src/agent-loop.ts:433`](source/packages/agent/src/agent-loop.ts#L433), [`source/packages/agent/src/agent-loop.ts:600`](source/packages/agent/src/agent-loop.ts#L600), [`source/packages/agent/src/agent-loop.ts:767`](source/packages/agent/src/agent-loop.ts#L767)).

The coding-agent `tool_result` extension event is attached through `afterToolCall`, so it likewise runs only for executed tools, not immediate failures ([`source/packages/coding-agent/src/core/agent-session.ts:509`](source/packages/coding-agent/src/core/agent-session.ts#L509)). Thrown tool errors become textual results with `isError: true`; they do not tear down the whole loop ([`source/packages/agent/src/agent-loop.ts:670`](source/packages/agent/src/agent-loop.ts#L670)). If the assistant response hit its output limit, Pi fails all included tool calls without executing them because apparently valid JSON arguments may be truncated ([`source/packages/agent/src/agent-loop.ts:202`](source/packages/agent/src/agent-loop.ts#L202), [`source/packages/agent/src/agent-loop.ts:374`](source/packages/agent/src/agent-loop.ts#L374)).

### 3.3 Concurrency and deterministic history

Tool batches execute in parallel by default. Preflight remains serial, execution can overlap, and `tool_execution_end` events occur in completion order. Final persisted `toolResult` messages are emitted in the assistant’s original call order ([`source/packages/agent/src/agent-loop.ts:411`](source/packages/agent/src/agent-loop.ts#L411), [`source/packages/agent/src/agent-loop.ts:489`](source/packages/agent/src/agent-loop.ts#L489)). A global sequential option or any called tool marked sequential serialises the whole batch ([`source/packages/agent/src/agent-loop.ts:411`](source/packages/agent/src/agent-loop.ts#L411)).

Built-in `edit` and `write` add a finer-grained safeguard: mutations to the same canonical path are queued, while different paths may still be changed concurrently ([`source/packages/coding-agent/src/core/tools/file-mutation-queue.ts:28`](source/packages/coding-agent/src/core/tools/file-mutation-queue.ts#L28), [`source/packages/coding-agent/src/core/tools/edit.ts:332`](source/packages/coding-agent/src/core/tools/edit.ts#L332)).

Selected built-in coding tools bound output at 2,000 lines or 50 KiB by default, with full output saved separately where appropriate ([`source/packages/coding-agent/src/core/tools/truncate.ts:2`](source/packages/coding-agent/src/core/tools/truncate.ts#L2), [`source/packages/coding-agent/src/core/tools/read.ts:284`](source/packages/coding-agent/src/core/tools/read.ts#L284), [`source/packages/coding-agent/src/core/tools/bash.ts:364`](source/packages/coding-agent/src/core/tools/bash.ts#L364)). This is not centrally enforced: extension and SDK tools can return arbitrarily large results because the wrapper and core insert their content unchanged ([`source/packages/coding-agent/src/core/tools/tool-definition-wrapper.ts:4`](source/packages/coding-agent/src/core/tools/tool-definition-wrapper.ts#L4), [`source/packages/agent/src/agent-loop.ts:777`](source/packages/agent/src/agent-loop.ts#L777)).

## 4. Extension and plugin structure

### 4.1 Discovery and package model

Extensions can come from user/project auto-discovery, configured local paths, npm/git packages, temporary CLI paths, or inline SDK factories. Standard directory discovery accepts direct `.ts`/`.js` files and subdirectories with a `pi.extensions` manifest or `index.ts`/`index.js` ([`source/packages/coding-agent/src/core/package-manager.ts:557`](source/packages/coding-agent/src/core/package-manager.ts#L557), [`source/packages/coding-agent/src/core/resource-loader.ts:405`](source/packages/coding-agent/src/core/resource-loader.ts#L405)).

A Pi package may declare four resource types in `package.json`: extensions, skills, prompts, and themes. Without the manifest, convention directories are scanned ([`source/packages/coding-agent/src/core/pi-manifest.ts:4`](source/packages/coding-agent/src/core/pi-manifest.ts#L4), [`source/packages/coding-agent/src/core/package-manager.ts:2153`](source/packages/coding-agent/src/core/package-manager.ts#L2153)). Packages can be user, project, or temporary scoped; project resources require project trust ([`source/packages/coding-agent/src/core/package-manager.ts:2398`](source/packages/coding-agent/src/core/package-manager.ts#L2398)).

Before deciding trust, Pi loads only user/global and CLI extensions so those extensions can participate in `project_trust`; project-local executable resources are loaded only after trust is resolved ([`source/packages/coding-agent/src/core/resource-loader.ts:380`](source/packages/coding-agent/src/core/resource-loader.ts#L380)). This is trust gating, not sandboxing: loaded extensions execute with the user’s full process permissions ([`source/packages/coding-agent/docs/extensions.md:96`](source/packages/coding-agent/docs/extensions.md#L96)).

### 4.2 Loading and registration

Extensions export a default factory receiving `ExtensionAPI`. Factories may be async, and Pi waits for them before startup continues. Modules are loaded sequentially through `jiti`, with aliases for Pi’s own packages and TypeBox ([`source/packages/coding-agent/src/core/extensions/loader.ts:490`](source/packages/coding-agent/src/core/extensions/loader.ts#L490), [`source/packages/coding-agent/src/core/extensions/loader.ts:545`](source/packages/coding-agent/src/core/extensions/loader.ts#L545)).

Registration during factory execution is transactional: a factory’s pending tools, handlers, commands, providers, and renderers commit only if that factory finishes. A failed factory is recorded and loading continues with later paths ([`source/packages/coding-agent/src/core/extensions/loader.ts:254`](source/packages/coding-agent/src/core/extensions/loader.ts#L254), [`source/packages/coding-agent/src/core/extensions/loader.ts:576`](source/packages/coding-agent/src/core/extensions/loader.ts#L576)).

`ExtensionAPI` exposes:

- typed event subscription;
- tools, commands, shortcuts, CLI flags, and renderers;
- custom messages and non-context session entries;
- active-tool, model, and thinking-level control;
- provider registration;
- session naming and labels;
- process execution and a shared event bus ([`source/packages/coding-agent/src/core/extensions/types.ts:1232`](source/packages/coding-agent/src/core/extensions/types.ts#L1232), [`source/packages/coding-agent/src/core/extensions/types.ts:1458`](source/packages/coding-agent/src/core/extensions/types.ts#L1458)).

Actions that require a bound session are rejected during factory execution. Provider registrations are the exception: they are queued and flushed when a model runtime is bound, allowing providers to exist during startup model selection ([`source/packages/coding-agent/src/core/extensions/loader.ts:179`](source/packages/coding-agent/src/core/extensions/loader.ts#L179), [`source/packages/coding-agent/src/core/agent-session-services.ts:154`](source/packages/coding-agent/src/core/agent-session-services.ts#L154)).

### 4.3 Extension lifecycle

`ExtensionContext` is constructed per handler invocation with lazy guarded access to current UI, model, session manager, abort signal, and trust state ([`source/packages/coding-agent/src/core/extensions/runner.ts:673`](source/packages/coding-agent/src/core/extensions/runner.ts#L673)). Reload and session replacement invalidate old extension APIs and contexts, preventing stale objects from silently changing a replacement runtime ([`source/packages/coding-agent/src/core/extensions/runner.ts:543`](source/packages/coding-agent/src/core/extensions/runner.ts#L543), [`source/packages/coding-agent/src/core/agent-session-runtime.ts:167`](source/packages/coding-agent/src/core/agent-session-runtime.ts#L167)).

That explicit invalidation is necessary because session replacement tears down the old extension instance, creates a new runtime, and emits `session_start` to the new instance before a replacement callback runs ([`source/packages/coding-agent/src/core/agent-session-runtime.ts:167`](source/packages/coding-agent/src/core/agent-session-runtime.ts#L167), [`source/packages/coding-agent/docs/extensions.md:1217`](source/packages/coding-agent/docs/extensions.md#L1217)).

## 5. Event structure

### 5.1 Core events

The core `AgentEvent` discriminated union has four lifecycle groups ([`source/packages/agent/src/types.ts:421`](source/packages/agent/src/types.ts#L421)):

| Group | Events | Purpose |
|---|---|---|
| Run | `agent_start`, `agent_end` | Bound one low-level loop run |
| Turn | `turn_start`, `turn_end` | Bound one assistant response and its tool results |
| Message | `message_start`, `message_update`, `message_end` | Stream user, assistant, and tool-result transcript items |
| Tool | `tool_execution_start`, `tool_execution_update`, `tool_execution_end` | Stream execution lifecycle separately from transcript messages |

The separation between tool execution events and tool-result message events lets a UI show live work while retaining a deterministic transcript protocol ([`source/packages/agent/src/agent-loop.ts:489`](source/packages/agent/src/agent-loop.ts#L489), [`source/packages/agent/src/agent-loop.ts:789`](source/packages/agent/src/agent-loop.ts#L789)).

### 5.2 Extension events

Coding-agent projects core events into a broader `ExtensionEvent` union and adds domain seams ([`source/packages/coding-agent/src/core/extensions/types.ts:1068`](source/packages/coding-agent/src/core/extensions/types.ts#L1068)):

| Domain | Main events | Semantics |
|---|---|---|
| Trust/resources | `project_trust`, `resources_discover` | Decide project trust; contribute resource paths |
| Session | `session_start`, `session_shutdown`, `session_before_switch`, `session_before_fork`, compaction and tree events | Observe or cancel lifecycle transitions; replace summaries |
| Input/prompt | `input`, `before_agent_start` | Transform/handle raw input; inject messages or transform system prompt |
| Agent | core run, turn, message, and tool-execution events plus `agent_settled` | Observe execution and transform final messages |
| Provider | `context`, `before_provider_headers`, `before_provider_request`, `after_provider_response` | Transform model context and request layers; observe response metadata |
| Model | `model_select`, `thinking_level_select` | Observe selected model/reasoning changes |
| Tool policy | `tool_call`, `tool_result`, `user_bash` | Mutate/block model tool calls, transform results, replace user shell backends |

The input path demonstrates deliberate hook placement: extension commands are checked first, then `input`, then skills/templates are expanded, then `before_agent_start` runs ([`source/packages/coding-agent/src/core/agent-session.ts:1139`](source/packages/coding-agent/src/core/agent-session.ts#L1139), [`source/packages/coding-agent/src/core/agent-session.ts:1256`](source/packages/coding-agent/src/core/agent-session.ts#L1256)).

### 5.3 Dispatch semantics

Pi has several event semantics rather than one generic emitter:

- **Ordered observation:** ordinary extension handlers run serially by extension and registration order; most errors are reported and later handlers continue ([`source/packages/coding-agent/src/core/extensions/runner.ts:801`](source/packages/coding-agent/src/core/extensions/runner.ts#L801)).
- **Cancellable decisions:** session-before events short-circuit when a handler returns `{ cancel: true }`. Other truthy decision payloads are neither merged nor chained: the last such handler result wins ([`source/packages/coding-agent/src/core/extensions/runner.ts:801`](source/packages/coding-agent/src/core/extensions/runner.ts#L801)).
- **Chained transforms:** context, provider payload, final message, tool result, input, and system prompt handlers receive the previous handler’s output ([`source/packages/coding-agent/src/core/extensions/runner.ts:835`](source/packages/coding-agent/src/core/extensions/runner.ts#L835), [`source/packages/coding-agent/src/core/extensions/runner.ts:984`](source/packages/coding-agent/src/core/extensions/runner.ts#L984), [`source/packages/coding-agent/src/core/extensions/runner.ts:1016`](source/packages/coding-agent/src/core/extensions/runner.ts#L1016)).
- **Fail-closed policy:** unlike most handlers, `tool_call` errors are not swallowed by the extension runner. The core’s guarded preflight converts the error into an error tool result, so a broken safety gate does not permit the tool ([`source/packages/coding-agent/src/core/extensions/runner.ts:932`](source/packages/coding-agent/src/core/extensions/runner.ts#L932), [`source/packages/agent/src/agent-loop.ts:649`](source/packages/agent/src/agent-loop.ts#L649)).

Core event handling in `AgentSession` runs extensions before external session listeners and persists `message_end` after extension replacement. This means extensions can normalise a final message before it becomes durable history ([`source/packages/coding-agent/src/core/agent-session.ts:623`](source/packages/coding-agent/src/core/agent-session.ts#L623), [`source/packages/coding-agent/src/core/agent-session.ts:748`](source/packages/coding-agent/src/core/agent-session.ts#L748)).

The shared `pi.events` bus is separate. It is an untyped Node `EventEmitter` wrapper intended for inter-extension notifications; listener failures are logged, and emissions are not awaited ([`source/packages/coding-agent/src/core/event-bus.ts:3`](source/packages/coding-agent/src/core/event-bus.ts#L3), [`source/packages/coding-agent/src/core/event-bus.ts:13`](source/packages/coding-agent/src/core/event-bus.ts#L13)). It does not have the control-flow guarantees of lifecycle hooks.

## 6. Conversation tree

### 6.1 Live V3 format

The first JSONL line is a session header. Later entries have an ID, parent ID, timestamp, and a typed payload. Entry types include messages, model/thinking changes, compactions, branch summaries, extension state, extension context messages, labels, and session metadata ([`source/packages/coding-agent/src/core/session-manager.ts:30`](source/packages/coding-agent/src/core/session-manager.ts#L30), [`source/packages/coding-agent/src/core/session-manager.ts:46`](source/packages/coding-agent/src/core/session-manager.ts#L46)).

`SessionManager` holds:

- the append-ordered entry list;
- an ID index;
- the current active `leafId`;
- a persistence path when the session is not in-memory ([`source/packages/coding-agent/src/core/session-manager.ts:855`](source/packages/coding-agent/src/core/session-manager.ts#L855)).

Appending creates a child of the current leaf and advances the leaf. Existing session files persist the entry immediately, but a new persistent session deliberately writes nothing until its first assistant message; that first write flushes the header and all accumulated entries together. A crash before then loses the initial prompt ([`source/packages/coding-agent/src/core/session-manager.ts:1015`](source/packages/coding-agent/src/core/session-manager.ts#L1015), [`source/packages/coding-agent/src/core/session-manager.ts:1044`](source/packages/coding-agent/src/core/session-manager.ts#L1044)). `branch(entryId)` only changes the in-memory leaf, so the next append creates another child without deleting the old descendants ([`source/packages/coding-agent/src/core/session-manager.ts:1360`](source/packages/coding-agent/src/core/session-manager.ts#L1360)).

A limitation follows from this implementation: opening an existing file sets the active leaf to the final physically stored entry. Merely navigating to an old node writes nothing, so that selection is not durable unless another entry is appended on the selected branch ([`source/packages/coding-agent/src/core/session-manager.ts:958`](source/packages/coding-agent/src/core/session-manager.ts#L958), [`source/packages/coding-agent/src/core/session-manager.ts:1360`](source/packages/coding-agent/src/core/session-manager.ts#L1360)).

### 6.2 Context is a projection, not the log

To build model context, Pi walks parent links from the selected leaf to the root and reverses the path. Model and thinking state are derived from that full path ([`source/packages/coding-agent/src/core/session-manager.ts:334`](source/packages/coding-agent/src/core/session-manager.ts#L334), [`source/packages/coding-agent/src/core/session-manager.ts:362`](source/packages/coding-agent/src/core/session-manager.ts#L362)).

The newest compaction on the path changes the projection. The context contains:

1. a compaction-summary message;
2. entries beginning at `firstKeptEntryId` up to the compaction checkpoint;
3. all entries after that checkpoint ([`source/packages/coding-agent/src/core/session-manager.ts:418`](source/packages/coding-agent/src/core/session-manager.ts#L418), [`source/packages/coding-agent/src/core/session-manager.ts:461`](source/packages/coding-agent/src/core/session-manager.ts#L461)).

Ordinary extension `custom` entries are durable but absent from model context. `custom_message`, `branch_summary`, and `compaction` entries project to application-level messages ([`source/packages/coding-agent/src/core/session-manager.ts:383`](source/packages/coding-agent/src/core/session-manager.ts#L383)). This provides two explicit persistence channels for plugins: state/display data and model-visible context.

### 6.3 Navigation, summaries, and forks

In-place `/tree` navigation changes the leaf. Selecting a user/custom message positions the leaf at that entry’s parent and returns the text for editing; selecting an assistant or structural entry positions at the entry itself ([`source/packages/coding-agent/src/core/agent-session.ts:3193`](source/packages/coding-agent/src/core/agent-session.ts#L3193), [`source/packages/coding-agent/src/core/agent-session.ts:3205`](source/packages/coding-agent/src/core/agent-session.ts#L3205)).

Optional branch summarisation finds the deepest common ancestor, summarises the path being left, appends the summary at the new target, and then reconstructs live model context ([`source/packages/coding-agent/src/core/compaction/branch-summarization.ts:108`](source/packages/coding-agent/src/core/compaction/branch-summarization.ts#L108), [`source/packages/coding-agent/src/core/agent-session.ts:3094`](source/packages/coding-agent/src/core/agent-session.ts#L3094)).

File-level forks have distinct semantics:

- interactive runtime `fork`/`clone` copies only the selected root-to-leaf path, re-chains parent IDs, and keeps applicable labels;
- CLI `--fork` copies all non-header entries, preserving the entire source tree ([`source/packages/coding-agent/src/core/session-manager.ts:1413`](source/packages/coding-agent/src/core/session-manager.ts#L1413), [`source/packages/coding-agent/src/core/session-manager.ts:1580`](source/packages/coding-agent/src/core/session-manager.ts#L1580)).

### 6.4 V4 generic harness direction

A separate generic session layer under `packages/agent/src/harness/session` uses a V4 mutation log with durable lane pointers, monotonic sequence numbers, entries, records, lanes, and facts. Its compaction checkpoints embed a retained tail rather than referring back through `firstKeptEntryId` ([`source/packages/agent/src/harness/session/types.ts:14`](source/packages/agent/src/harness/session/types.ts#L14), [`source/packages/agent/src/harness/session/jsonl/codec.ts:70`](source/packages/agent/src/harness/session/jsonl/codec.ts#L70), [`source/packages/agent/src/harness/session/context.ts:45`](source/packages/agent/src/harness/session/context.ts#L45)).

This is not yet the live coding-agent conversation runtime or a usable generic replacement. Beyond `prompt()`, `compact()`, and `navigateTree()`, hooks, restore/resume, abort, queues, action driving, watches, and lane operations also reject as unimplemented; creation rejects existing records ([`source/packages/agent/src/harness/agent-harness.ts:326`](source/packages/agent/src/harness/agent-harness.ts#L326), [`source/packages/agent/src/harness/agent-harness.ts:347`](source/packages/agent/src/harness/agent-harness.ts#L347), [`source/packages/agent/src/harness/agent-harness.ts:365`](source/packages/agent/src/harness/agent-harness.ts#L365), [`source/packages/agent/src/harness/agent-harness.ts:502`](source/packages/agent/src/harness/agent-harness.ts#L502)). The current session-format documentation mentions both retained-tail and older `firstKeptEntryId` forms, while live V3 context reconstruction still uses the latter ([`source/packages/coding-agent/docs/session-format.md:237`](source/packages/coding-agent/docs/session-format.md#L237), [`source/packages/coding-agent/src/core/session-manager.ts:418`](source/packages/coding-agent/src/core/session-manager.ts#L418)).

## 7. Multiple model providers

### 7.1 Canonical boundary

A `Model` carries provider-qualified metadata: provider ID, model ID, API/protocol ID, endpoint, input modalities, reasoning support, pricing, context/output limits, headers, and compatibility flags ([`source/packages/ai/src/types.ts:821`](source/packages/ai/src/types.ts#L821)). `Api` permits arbitrary strings, allowing extensions to introduce protocols beyond the built-in union ([`source/packages/ai/src/types.ts:29`](source/packages/ai/src/types.ts#L29)).

A `Provider` owns:

- identity and static endpoint metadata;
- provider-specific authentication;
- synchronous current model discovery;
- optional dynamic refresh and credential-dependent filtering;
- canonical raw/simple streaming methods ([`source/packages/ai/src/models.ts:97`](source/packages/ai/src/models.ts#L97)).

A `Models` collection owns provider registration, auth application, availability, refresh, and stream dispatch by `model.provider` ([`source/packages/ai/src/models.ts:156`](source/packages/ai/src/models.ts#L156), [`source/packages/ai/src/models.ts:254`](source/packages/ai/src/models.ts#L254)). Dynamic refresh restores stored catalogue state before credential resolution and network access, which makes cached models available offline and avoids letting stale refresh generations publish over newer ones ([`source/packages/ai/src/models.ts:386`](source/packages/ai/src/models.ts#L386)).

### 7.2 Protocol adapters and message portability

Built-in provider factories bind generated catalogues and auth to lazy protocol adapters ([`source/packages/ai/src/providers/all.ts:89`](source/packages/ai/src/providers/all.ts#L89), [`source/packages/ai/src/api/lazy.ts:46`](source/packages/ai/src/api/lazy.ts#L46)). The adapters translate provider requests and responses to the same `Context`, `AssistantMessage`, and `AssistantMessageEvent` contracts, so the agent loop consumes one stream shape ([`source/packages/ai/src/types.ts:427`](source/packages/ai/src/types.ts#L427), [`source/packages/ai/src/types.ts:521`](source/packages/ai/src/types.ts#L521)).

Before serialisation, `transformMessages()` handles cross-provider portability. It removes unsupported images, preserves signed thinking only for the same provider/API/model, turns incompatible thinking into text, normalises tool IDs, skips failed assistant turns, and synthesises missing tool results ([`source/packages/ai/src/api/transform-messages.ts:35`](source/packages/ai/src/api/transform-messages.ts#L35), [`source/packages/ai/src/api/transform-messages.ts:95`](source/packages/ai/src/api/transform-messages.ts#L95), [`source/packages/ai/src/api/transform-messages.ts:163`](source/packages/ai/src/api/transform-messages.ts#L163)). This is what makes mid-session model switching more than a catalogue/UI feature.

### 7.3 Coding-agent composition

`ModelRuntime.create()` combines:

1. built-in providers and generated catalogues;
2. persisted remote catalogue overlays;
3. `models.json` provider/model configuration;
4. extension-registered native providers or legacy provider configs;
5. credential and model stores ([`source/packages/coding-agent/src/core/model-runtime.ts:172`](source/packages/coding-agent/src/core/model-runtime.ts#L172), [`source/packages/coding-agent/src/core/provider-composer.ts:420`](source/packages/coding-agent/src/core/provider-composer.ts#L420)).

The composition order inside one provider is base/native-extension models, `models.json` changes and upserts, extension config models, optional legacy OAuth projection, then per-model overrides ([`source/packages/coding-agent/src/core/provider-composer.ts:168`](source/packages/coding-agent/src/core/provider-composer.ts#L168), [`source/packages/coding-agent/src/core/provider-composer.ts:420`](source/packages/coding-agent/src/core/provider-composer.ts#L420)).

Extensions can register either a complete native `Provider` or a legacy config with models, OAuth, dynamic refresh, and an optional custom stream function. Registration can happen during factory loading or immediately at runtime; unregistering recomposes the original provider layers ([`source/packages/coding-agent/src/core/extensions/types.ts:1458`](source/packages/coding-agent/src/core/extensions/types.ts#L1458), [`source/packages/coding-agent/src/core/model-runtime.ts:733`](source/packages/coding-agent/src/core/model-runtime.ts#L733)).

### 7.4 Authentication

Provider auth separates credential storage, login/refresh behaviour, ambient resolution, and request-time auth derivation ([`source/packages/ai/src/auth/types.ts:170`](source/packages/ai/src/auth/types.ts#L170)). Generic request resolution prefers an explicit request key, then a stored credential, then provider ambient sources such as environment or cloud credentials; expired OAuth refresh occurs through credential-store locking ([`source/packages/ai/src/auth/resolve.ts:50`](source/packages/ai/src/auth/resolve.ts#L50), [`source/packages/ai/src/auth/resolve.ts:127`](source/packages/ai/src/auth/resolve.ts#L127)).

Coding-agent persists credentials in a locked `auth.json` created with mode `0600`. CLI `--api-key` becomes a non-persistent runtime override ([`source/packages/coding-agent/src/core/auth-storage.ts:63`](source/packages/coding-agent/src/core/auth-storage.ts#L63), [`source/packages/coding-agent/src/main.ts:804`](source/packages/coding-agent/src/main.ts#L804)).

At request time, `ModelRuntime` resolves auth, applies endpoint/header overrides, runs the header transform, and delegates to the effective provider’s `streamSimple()` ([`source/packages/coding-agent/src/core/model-runtime.ts:472`](source/packages/coding-agent/src/core/model-runtime.ts#L472), [`source/packages/coding-agent/src/core/model-runtime.ts:573`](source/packages/coding-agent/src/core/model-runtime.ts#L573), [`source/packages/coding-agent/src/core/model-runtime.ts:636`](source/packages/coding-agent/src/core/model-runtime.ts#L636)). Provider-payload and response hooks are passed as `onPayload` and `onResponse` callbacks rather than centrally invoked; custom stream implementations must call them for `before_provider_request` and `after_provider_response` to fire ([`source/packages/coding-agent/src/core/sdk.ts:325`](source/packages/coding-agent/src/core/sdk.ts#L325), [`source/packages/coding-agent/src/core/extensions/types.ts:1494`](source/packages/coding-agent/src/core/extensions/types.ts#L1494)).

## 8. What Pi does well

### 8.1 It keeps the loop boring

The core algorithm is recognisable and testable because provider specifics, storage, compaction, project resources, and UI sit outside it. The injected boundaries are explicit: stream function, message conversion, context transform, tool hooks, queue readers, and next-turn refresh ([`source/packages/agent/src/types.ts:153`](source/packages/agent/src/types.ts#L153)).

The useful lesson is the boundary, not Pi’s exact class hierarchy: the loop should operate on a canonical message/tool protocol and ask collaborators for everything provider- or product-specific.

### 8.2 It treats ordering as a correctness property

Several details preserve replayable history:

- awaited core events update state before subscribers run;
- steering waits for tool results;
- parallel tool execution still produces source-ordered transcript results;
- extension final-message transforms run before persistence;
- cross-provider conversion repairs or omits invalid message sequences ([`source/packages/agent/src/agent.ts:529`](source/packages/agent/src/agent.ts#L529), [`source/packages/agent/src/agent-loop.ts:489`](source/packages/agent/src/agent-loop.ts#L489), [`source/packages/coding-agent/src/core/agent-session.ts:623`](source/packages/coding-agent/src/core/agent-session.ts#L623), [`source/packages/ai/src/api/transform-messages.ts:163`](source/packages/ai/src/api/transform-messages.ts#L163)).

This is more important than the visual tree. A harness can only branch, compact, replay, or switch providers reliably if transcript ordering is an explicit invariant.

### 8.3 Events are placed at decision seams

The strongest hooks sit where the host still has context and can act:

- raw input before expansion;
- assembled system prompt before the run;
- canonical messages before each provider request;
- provider headers and payload after serialisation;
- tool arguments before execution;
- tool output before transcript insertion;
- session transition before destructive/replacement work ([`source/packages/coding-agent/src/core/extensions/types.ts:521`](source/packages/coding-agent/src/core/extensions/types.ts#L521), [`source/packages/coding-agent/src/core/extensions/types.ts:688`](source/packages/coding-agent/src/core/extensions/types.ts#L688), [`source/packages/coding-agent/src/core/extensions/types.ts:831`](source/packages/coding-agent/src/core/extensions/types.ts#L831)).

The events also have different control semantics rather than pretending all hooks are notifications. Transform, decision, and observation behaviour is visible in dedicated runner methods ([`source/packages/coding-agent/src/core/extensions/runner.ts:801`](source/packages/coding-agent/src/core/extensions/runner.ts#L801)).

### 8.4 The session log and model context are separate concepts

Pi preserves full branch history while projecting only one compacted path into model context ([`source/packages/coding-agent/src/core/session-manager.ts:411`](source/packages/coding-agent/src/core/session-manager.ts#L411)). Extension state can be durable without entering context, while extension messages can explicitly enter it ([`source/packages/coding-agent/src/core/session-manager.ts:383`](source/packages/coding-agent/src/core/session-manager.ts#L383)).

That separation avoids making the provider transcript the only source of truth. It supports audit history, UI-only records, bookmarks, model changes, summaries, and future projections without polluting every model call.

### 8.5 Provider support is an adapter system, not branching in the loop

The loop consumes one stream protocol. Provider factories own auth/catalogue/adapter behaviour, and coding-agent composes user and extension overrides above them ([`source/packages/ai/src/models.ts:97`](source/packages/ai/src/models.ts#L97), [`source/packages/coding-agent/src/core/provider-composer.ts:420`](source/packages/coding-agent/src/core/provider-composer.ts#L420)). Cross-provider transcript transformation handles the hard part of model switching: provider-specific thinking signatures, tool IDs, media support, and incomplete calls ([`source/packages/ai/src/api/transform-messages.ts:35`](source/packages/ai/src/api/transform-messages.ts#L35)).

### 8.6 Operational constraints are first-class

Pi includes cancellation, built-in tool output limits, same-file mutation queues, failed-tool normalisation, overflow recovery, credential locking, cached provider catalogues, and extension trust gating in the relevant layers ([`source/packages/coding-agent/src/core/tools/file-mutation-queue.ts:28`](source/packages/coding-agent/src/core/tools/file-mutation-queue.ts#L28), [`source/packages/coding-agent/src/core/auth-storage.ts:327`](source/packages/coding-agent/src/core/auth-storage.ts#L327), [`source/packages/ai/src/models.ts:386`](source/packages/ai/src/models.ts#L386)). These details are less visible than the plugin API but account for much of the harness’s practical robustness.

## 9. Costs and cautions

### 9.1 The extension API is broad

The API spans core lifecycle, tools, shell execution, providers, models, session mutation, terminal UI components, editor replacement, themes, shortcuts, flags, and shared events ([`source/packages/coding-agent/src/core/extensions/types.ts:1232`](source/packages/coding-agent/src/core/extensions/types.ts#L1232)). Each capability becomes a compatibility promise across TUI, RPC, JSON, and print modes. RPC already has to make several TUI-specific operations no-ops ([`source/packages/coding-agent/src/modes/rpc/rpc-mode.ts:136`](source/packages/coding-agent/src/modes/rpc/rpc-mode.ts#L136), [`source/packages/coding-agent/src/modes/rpc/rpc-mode.ts:228`](source/packages/coding-agent/src/modes/rpc/rpc-mode.ts#L228)).

A smaller harness can preserve Pi’s event/tool/command pattern without exposing raw presentation components or full session replacement to plugins initially.

### 9.2 Delivery guarantees are not uniform

Core `Agent` subscribers are awaited, but `AgentSession`’s external listener loop does not await listener promises ([`source/packages/agent/src/agent.ts:240`](source/packages/agent/src/agent.ts#L240), [`source/packages/coding-agent/src/core/agent-session.ts:569`](source/packages/coding-agent/src/core/agent-session.ts#L569)). Some extension notifications are fire-and-forget, and the shared event bus is synchronous/unawaited ([`source/packages/coding-agent/src/core/agent-session.ts:1768`](source/packages/coding-agent/src/core/agent-session.ts#L1768), [`source/packages/coding-agent/src/core/event-bus.ts:13`](source/packages/coding-agent/src/core/event-bus.ts#L13)).

Consumers must know which events are ordering barriers, which are transforms, and which are best-effort notifications. This distinction belongs in event types and naming in a new harness.

### 9.3 Tool argument mutation bypasses revalidation

`tool_call` runs after schema validation and may mutate `event.input`; the mutated object is executed without another schema check ([`source/packages/coding-agent/src/core/extensions/types.ts:921`](source/packages/coding-agent/src/core/extensions/types.ts#L921), [`source/packages/agent/src/agent-loop.ts:619`](source/packages/agent/src/agent-loop.ts#L619)). This enables patching but places type safety on extensions. A transform-return-and-revalidate design would provide a narrower contract.

### 9.4 The live tree has a non-durable cursor

V3 branch navigation only changes in-memory `leafId`; reopening picks the final physical entry ([`source/packages/coding-agent/src/core/session-manager.ts:958`](source/packages/coding-agent/src/core/session-manager.ts#L958), [`source/packages/coding-agent/src/core/session-manager.ts:1360`](source/packages/coding-agent/src/core/session-manager.ts#L1360)). The V4 lane mutation format appears to address durable pointers, but the generic harness using it is unfinished ([`source/packages/agent/src/harness/session/state.ts:112`](source/packages/agent/src/harness/session/state.ts#L112), [`source/packages/agent/src/harness/agent-harness.ts:347`](source/packages/agent/src/harness/agent-harness.ts#L347)).

### 9.5 Compatibility layers are visible

The core accepts a process-global fallback stream function for older consumers, and coding-agent retains a synchronous `ModelRegistry` compatibility façade over async `ModelRuntime` ([`source/packages/agent/src/agent.ts:216`](source/packages/agent/src/agent.ts#L216), [`source/packages/coding-agent/src/core/sdk.ts:34`](source/packages/coding-agent/src/core/sdk.ts#L34), [`source/packages/coding-agent/src/core/model-registry.ts:28`](source/packages/coding-agent/src/core/model-registry.ts#L28)). These are migration mechanisms, not ideal starting points for a new harness.

### 9.6 Plugins are trusted code

Project trust controls whether executable project resources load, but extensions and package code run with full system access. Local built-in tools also accept absolute/home paths and are not a sandbox ([`source/packages/coding-agent/docs/extensions.md:96`](source/packages/coding-agent/docs/extensions.md#L96), [`source/packages/coding-agent/src/core/tools/path-utils.ts:45`](source/packages/coding-agent/src/core/tools/path-utils.ts#L45)). Sandboxing is a separate backend/policy concern.

## 10. A minimal architecture extracted from Pi

This section translates the observed boundaries into a smaller starting shape for another harness. It deliberately omits Pi’s compatibility layers and broad UI API.

### 10.1 Kernel

Keep only:

- canonical user, assistant, tool-call, and tool-result messages;
- a canonical assistant stream event protocol;
- an injected provider stream function;
- a turn loop;
- validated cancellable tools with progress;
- ordered lifecycle events;
- steering and follow-up queues.

Pi evidence: [`source/packages/agent/src/types.ts:303`](source/packages/agent/src/types.ts#L303), [`source/packages/agent/src/agent-loop.ts:153`](source/packages/agent/src/agent-loop.ts#L153).

### 10.2 Session runtime

Make one layer own:

- durable append-only records;
- projection from history to provider context;
- active branch/cursor;
- retries and compaction;
- extension dispatch;
- model/tool selection for the next turn;
- the distinction between low-level run completion and full settlement.

Pi evidence: [`source/packages/coding-agent/src/core/agent-session.ts:384`](source/packages/coding-agent/src/core/agent-session.ts#L384), [`source/packages/coding-agent/src/core/session-manager.ts:461`](source/packages/coding-agent/src/core/session-manager.ts#L461).

A linear append-only log is enough initially unless branch navigation is a product requirement. If branching is required, persist the active cursor/lane rather than relying on the final physical entry.

### 10.3 Hook system

Define separate typed contracts for:

- **observations**, with isolated failures;
- **transforms**, with chained input/output and post-transform validation where relevant;
- **decisions**, with explicit cancellation/blocking and fail-open/fail-closed policy.

Start with prompt, context, tool-call, tool-result, and session lifecycle hooks. Keep an untyped cross-plugin bus separate and explicitly best-effort, if one exists at all.

Pi evidence: [`source/packages/coding-agent/src/core/extensions/runner.ts:801`](source/packages/coding-agent/src/core/extensions/runner.ts#L801), [`source/packages/coding-agent/src/core/extensions/runner.ts:835`](source/packages/coding-agent/src/core/extensions/runner.ts#L835), [`source/packages/coding-agent/src/core/extensions/runner.ts:932`](source/packages/coding-agent/src/core/extensions/runner.ts#L932).

### 10.4 Plugin surface

The smallest useful plugin API is:

- register hooks;
- register tools;
- register user commands;
- append structured plugin records;
- read a scoped invocation context.

Add provider registration when custom providers are a requirement. Add mode-neutral UI operations such as select/confirm/notify before exposing raw editor or TUI component control.

Pi evidence: [`source/packages/coding-agent/src/core/extensions/types.ts:1232`](source/packages/coding-agent/src/core/extensions/types.ts#L1232), [`source/packages/coding-agent/src/core/extensions/loader.ts:254`](source/packages/coding-agent/src/core/extensions/loader.ts#L254).

### 10.5 Provider runtime

Keep provider concerns outside the kernel. One runtime should own:

- provider registration;
- model catalogues and availability;
- credentials and refresh;
- protocol adapters;
- request preparation;
- cross-provider transcript normalisation.

Pass one explicit `stream(model, context, options)` function into the kernel. Avoid a global fallback registry in a new implementation.

Pi evidence: [`source/packages/ai/src/models.ts:97`](source/packages/ai/src/models.ts#L97), [`source/packages/coding-agent/src/core/model-runtime.ts:573`](source/packages/coding-agent/src/core/model-runtime.ts#L573), [`source/packages/ai/src/api/transform-messages.ts:35`](source/packages/ai/src/api/transform-messages.ts#L35).

### 10.6 Front ends

TUI, print, web, and RPC adapters should subscribe to the same session event stream and send commands to the session runtime. They should not implement retry, persistence, or tool ordering themselves.

Pi evidence: [`source/packages/coding-agent/src/modes/print-mode.ts:74`](source/packages/coding-agent/src/modes/print-mode.ts#L74), [`source/packages/coding-agent/src/modes/rpc/rpc-mode.ts:317`](source/packages/coding-agent/src/modes/rpc/rpc-mode.ts#L317).

## Conventions and notable decisions

- Application-specific `AgentMessage` variants are added to the core type through TypeScript declaration merging, allowing the loop to carry messages it does not understand while `convertToLlm` controls provider projection ([`source/packages/agent/src/types.ts:303`](source/packages/agent/src/types.ts#L303), [`source/packages/coding-agent/src/core/messages.ts:27`](source/packages/coding-agent/src/core/messages.ts#L27)).
- Among extensions, the first tool registration with a name wins; extension/SDK tools are then overlaid on built-ins, allowing a custom tool to replace a built-in ([`source/packages/coding-agent/src/core/extensions/runner.ts:451`](source/packages/coding-agent/src/core/extensions/runner.ts#L451), [`source/packages/coding-agent/src/core/agent-session.ts:2626`](source/packages/coding-agent/src/core/agent-session.ts#L2626)).
- Branch-correct extension state is a plugin convention, not automatic host behaviour. The todo example explicitly replays durable entries on both `session_start` and `session_tree`, rather than relying on a mutable object surviving reload, replacement, or navigation ([`source/packages/coding-agent/examples/extensions/todo.ts:110`](source/packages/coding-agent/examples/extensions/todo.ts#L110), [`source/packages/coding-agent/src/core/agent-session.ts:3217`](source/packages/coding-agent/src/core/agent-session.ts#L3217)).

## Key references

- [`source/packages/agent/src/agent-loop.ts`](source/packages/agent/src/agent-loop.ts) — complete provider-neutral turn and tool loop.
- [`source/packages/agent/src/agent.ts`](source/packages/agent/src/agent.ts) — stateful wrapper, queues, cancellation, event ordering, and settlement.
- [`source/packages/agent/src/types.ts`](source/packages/agent/src/types.ts) — core extension points, message types, tool contract, and event union.
- [`source/packages/coding-agent/src/core/agent-session.ts`](source/packages/coding-agent/src/core/agent-session.ts) — session orchestration, extension projection, persistence, retry, compaction, and tree navigation.
- [`source/packages/coding-agent/src/core/sdk.ts`](source/packages/coding-agent/src/core/sdk.ts) — composition root connecting resources, model runtime, extensions, tools, and core agent.
- [`source/packages/coding-agent/src/core/extensions/types.ts`](source/packages/coding-agent/src/core/extensions/types.ts) — public extension capability and event surface.
- [`source/packages/coding-agent/src/core/extensions/loader.ts`](source/packages/coding-agent/src/core/extensions/loader.ts) — transactional loading and API construction.
- [`source/packages/coding-agent/src/core/extensions/runner.ts`](source/packages/coding-agent/src/core/extensions/runner.ts) — event ordering, transforms, cancellation, and error isolation.
- [`source/packages/coding-agent/src/core/session-manager.ts`](source/packages/coding-agent/src/core/session-manager.ts) — live V3 JSONL tree and context projection.
- [`source/packages/agent/src/harness/session`](source/packages/agent/src/harness/session) — newer V4 mutation-log session direction.
- [`source/packages/ai/src/models.ts`](source/packages/ai/src/models.ts) — provider and model-runtime contracts.
- [`source/packages/ai/src/api/transform-messages.ts`](source/packages/ai/src/api/transform-messages.ts) — cross-provider transcript normalisation.
- [`source/packages/coding-agent/src/core/model-runtime.ts`](source/packages/coding-agent/src/core/model-runtime.ts) — coding-agent provider/auth/catalogue composition.
- [`source/packages/coding-agent/src/core/provider-composer.ts`](source/packages/coding-agent/src/core/provider-composer.ts) — precedence of built-in, file, and extension provider layers.
- [`source/packages/coding-agent/docs/extensions.md`](source/packages/coding-agent/docs/extensions.md) — public extension lifecycle and API semantics.
- [`source/packages/coding-agent/docs/sdk.md`](source/packages/coding-agent/docs/sdk.md) — embedding and runtime composition.
- [`source/packages/coding-agent/docs/session-format.md`](source/packages/coding-agent/docs/session-format.md) — documented session records and context projection.
- [`source/packages/coding-agent/docs/custom-provider.md`](source/packages/coding-agent/docs/custom-provider.md) — public provider extension model.
- [`source/packages/coding-agent/examples/extensions/README.md`](source/packages/coding-agent/examples/extensions/README.md) — index of concrete extension patterns.
