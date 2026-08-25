# Research: Maki agentic harness

## Scope

This document examines Maki at commit [`e0d87e35`](https://github.com/tontinton/maki/tree/e0d87e35a45a313b8c3e03dca21ae5aef3d35050), focusing on:

- the agent loop;
- native, Lua, MCP, and code-execution tools;
- Lua plugins and other extension seams;
- agent and plugin events;
- conversation history, rewind, subagents, and persistence;
- context management and compaction;
- the Rust TUI and its integration with the agent;
- the security boundaries and limitations of the code-execution sandbox.

The source was inspected directly from the repository. This is an architectural account, not a performance benchmark or a complete API reference. Maki changes quickly, so the pinned revision matters.

## Summary

- Maki separates the reusable async agent from its TUI. The agent owns the model/tool loop and emits typed events; a per-session UI supervisor owns queuing, cancellation, persistence, and display. [`maki-agent/src/agent/run.rs:199-380`](https://github.com/tontinton/maki/blob/e0d87e35a45a313b8c3e03dca21ae5aef3d35050/maki-agent/src/agent/run.rs#L199-L380) [`maki-ui/src/agent/agent_loop.rs:106-286`](https://github.com/tontinton/maki/blob/e0d87e35a45a313b8c3e03dca21ae5aef3d35050/maki-ui/src/agent/agent_loop.rs#L106-L286)
- Tool calls from one assistant turn execute concurrently, but their results are collected into one ordered history message before the next model turn. Tool panics are isolated, repeated calls are detected, and permission checks sit in the shared dispatcher. [`maki-agent/src/agent/tool_dispatch.rs:383-468`](https://github.com/tontinton/maki/blob/e0d87e35a45a313b8c3e03dca21ae5aef3d35050/maki-agent/src/agent/tool_dispatch.rs#L383-L468) [`maki-agent/src/task_set.rs:1-50`](https://github.com/tontinton/maki/blob/e0d87e35a45a313b8c3e03dca21ae5aef3d35050/maki-agent/src/task_set.rs#L1-L50)
- `code_execution` is not a CPython or direct OS-execution environment. It is a fresh, in-process Monty interpreter that exposes selected Maki tools as async Python functions. Python can filter and combine large intermediate results before returning stdout and any final expression value to model history. Direct Monty OS calls are rejected, but delegated tools can provide permission-controlled filesystem, network, or shell effects. Time, recursion, and memory are bounded, although the memory limit is explicitly a guardrail rather than a hard security boundary. [`plugins/code_execution/init.lua:113-176`](https://github.com/tontinton/maki/blob/e0d87e35a45a313b8c3e03dca21ae5aef3d35050/plugins/code_execution/init.lua#L113-L176) [`plugins/code_execution/init.lua:283-332`](https://github.com/tontinton/maki/blob/e0d87e35a45a313b8c3e03dca21ae5aef3d35050/plugins/code_execution/init.lua#L283-L332) [`maki-interpreter/src/runner.rs:116-278`](https://github.com/tontinton/maki/blob/e0d87e35a45a313b8c3e03dca21ae5aef3d35050/maki-interpreter/src/runner.rs#L116-L278) [`maki-interpreter/src/alloc.rs:1-27`](https://github.com/tontinton/maki/blob/e0d87e35a45a313b8c3e03dca21ae5aef3d35050/maki-interpreter/src/alloc.rs#L1-L27)
- Lua is a first-class extension layer. Plugins can add tools, commands, keymaps, prompt content, autocmds, composable function slots, rich tool views, windows, and subagents. Registration is staged and tool replacement is atomic after a successful plugin load. [`maki-lua/src/api/mod.rs:36-95`](https://github.com/tontinton/maki/blob/e0d87e35a45a313b8c3e03dca21ae5aef3d35050/maki-lua/src/api/mod.rs#L36-L95) [`maki-lua/src/runtime.rs:1734-1870`](https://github.com/tontinton/maki/blob/e0d87e35a45a313b8c3e03dca21ae5aef3d35050/maki-lua/src/runtime.rs#L1734-L1870)
- Events form two related protocols: typed `AgentEvent` envelopes carry model/tool lifecycle state into hosts, while Lua autocmds expose higher-level turn/tool/session hooks to plugins. A monotonically increasing `run_id` makes late async output safe to ignore after cancellation or respawn. [`maki-agent/src/types.rs:553-613`](https://github.com/tontinton/maki/blob/e0d87e35a45a313b8c3e03dca21ae5aef3d35050/maki-agent/src/types.rs#L553-L613) [`maki-agent/src/types.rs:858-923`](https://github.com/tontinton/maki/blob/e0d87e35a45a313b8c3e03dca21ae5aef3d35050/maki-agent/src/types.rs#L858-L923) [`maki-lua/src/api/autocmd.rs:66-153`](https://github.com/tontinton/maki/blob/e0d87e35a45a313b8c3e03dca21ae5aef3d35050/maki-lua/src/api/autocmd.rs#L66-L153)
- Maki does not persist a general conversation tree. A session is one linear message vector with maps of tool output and subagent transcripts. Rewind destructively truncates that vector; SDK “fork” copies it into a new, unlinked session. Subagent conversations are attachments keyed by parent tool-use ID, not recursive branch nodes. [`maki-storage/src/sessions.rs:195-208`](https://github.com/tontinton/maki/blob/e0d87e35a45a313b8c3e03dca21ae5aef3d35050/maki-storage/src/sessions.rs#L195-L208) [`maki-ui/src/app/session.rs:315-338`](https://github.com/tontinton/maki/blob/e0d87e35a45a313b8c3e03dca21ae5aef3d35050/maki-ui/src/app/session.rs#L315-L338) [`src/sdk_mode.rs:663-694`](https://github.com/tontinton/maki/blob/e0d87e35a45a313b8c3e03dca21ae5aef3d35050/src/sdk_mode.rs#L663-L694)
- Context efficiency is treated as data flow, not just prompt wording: compact file indexing, deferred MCP schemas, model-generated history compaction, separate UI/model tool output, output truncation, and in-sandbox transformations all prevent unnecessary data from entering model context. [`maki-agent/src/mcp/mod.rs:346-388`](https://github.com/tontinton/maki/blob/e0d87e35a45a313b8c3e03dca21ae5aef3d35050/maki-agent/src/mcp/mod.rs#L346-L388) [`maki-agent/src/agent/compaction.rs:28-115`](https://github.com/tontinton/maki/blob/e0d87e35a45a313b8c3e03dca21ae5aef3d35050/maki-agent/src/agent/compaction.rs#L28-L115) [`maki-agent/src/types.rs:300-364`](https://github.com/tontinton/maki/blob/e0d87e35a45a313b8c3e03dca21ae5aef3d35050/maki-agent/src/types.rs#L300-L364)
- Model providers share one narrow streaming interface and normalised message/event vocabulary. Built-ins, configuration-defined protocol adapters, script-backed providers, and models.dev catalogue providers all resolve behind the same factory, while `Model` carries capability and pricing metadata independently from transport. [`maki-providers/src/provider.rs:254-337`](https://github.com/tontinton/maki/blob/e0d87e35a45a313b8c3e03dca21ae5aef3d35050/maki-providers/src/provider.rs#L254-L337) [`maki-providers/src/model.rs:234-303`](https://github.com/tontinton/maki/blob/e0d87e35a45a313b8c3e03dca21ae5aef3d35050/maki-providers/src/model.rs#L234-L303)
- The TUI is Elm-inspired and demand-rendered rather than a fixed 60 FPS loop. One thread owns mutable UI state; most external effects are returned as actions; channel traffic enters through one selector while worker results and live buffers are polled during ticks; transcript layout is cached; expensive rendering runs on workers; cadence drops when nothing moves. [`maki-ui/src/app/mod.rs:205-269`](https://github.com/tontinton/maki/blob/e0d87e35a45a313b8c3e03dca21ae5aef3d35050/maki-ui/src/app/mod.rs#L205-L269) [`maki-ui/src/event_loop.rs:596-760`](https://github.com/tontinton/maki/blob/e0d87e35a45a313b8c3e03dca21ae5aef3d35050/maki-ui/src/event_loop.rs#L596-L760) [`maki-ui/src/components/messages/mod.rs:698-716`](https://github.com/tontinton/maki/blob/e0d87e35a45a313b8c3e03dca21ae5aef3d35050/maki-ui/src/components/messages/mod.rs#L698-L716) [`maki-ui/src/repaint.rs:1-209`](https://github.com/tontinton/maki/blob/e0d87e35a45a313b8c3e03dca21ae5aef3d35050/maki-ui/src/repaint.rs#L1-L209)

## Findings

## 1. System shape

The workspace has deliberately separate crates for agent behaviour, providers, storage, Lua, the Python interpreter, ACP, and the TUI. `maki-agent::Agent` can be used by the TUI, headless SDK mode, or ACP without depending on Ratatui. The TUI wraps it in a longer-lived `AgentLoop` that owns the input queue and mutable `History`. [`maki-agent/src/lib.rs:1-39`](https://github.com/tontinton/maki/blob/e0d87e35a45a313b8c3e03dca21ae5aef3d35050/maki-agent/src/lib.rs#L1-L39) [`maki-ui/src/agent/mod.rs:215-282`](https://github.com/tontinton/maki/blob/e0d87e35a45a313b8c3e03dca21ae5aef3d35050/maki-ui/src/agent/mod.rs#L215-L282) [`maki-agent/src/headless.rs:311-489`](https://github.com/tontinton/maki/blob/e0d87e35a45a313b8c3e03dca21ae5aef3d35050/maki-agent/src/headless.rs#L311-L489)

```text
terminal / SDK / ACP input
        |
        v
session supervisor and serial queue
        |
        v
Agent::run(input, mutable History)
        |
        +--> provider stream --> typed AgentEvents --> host/UI
        |
        +--> parallel tool dispatch --> one tool-result history message
        |
        +--> next turn / compaction / queued interrupt / completion
        |
        v
shared immutable HistorySnapshot --> checkpointed JSONL storage
```

This split is one of Maki's strongest architectural properties: agent semantics, presentation, and persistence communicate through explicit types and channels rather than sharing the TUI's internal state. The same agent can therefore support interactive, headless, and protocol-driven hosts. [`maki-agent/src/types.rs:553-629`](https://github.com/tontinton/maki/blob/e0d87e35a45a313b8c3e03dca21ae5aef3d35050/maki-agent/src/types.rs#L553-L629) [`maki-acp/src/server.rs:268-332`](https://github.com/tontinton/maki/blob/e0d87e35a45a313b8c3e03dca21ae5aef3d35050/maki-acp/src/server.rs#L268-L332)

## 2. The core loop

### 2.1 Queue and run lifecycle

The TUI does not call the agent directly from a key handler. Submitting a prompt increments a session's `run_id`, marks it streaming, and returns an action. The event loop converts the action into a `QueueItem`, and the per-session `AgentLoop` serially drains that queue. A bounded one-slot notification channel avoids one wake-up per queued item, while the queue itself is a mutex-protected `VecDeque`. [`maki-ui/src/app/queue.rs:228-240`](https://github.com/tontinton/maki/blob/e0d87e35a45a313b8c3e03dca21ae5aef3d35050/maki-ui/src/app/queue.rs#L228-L240) [`maki-ui/src/event_loop.rs:1277-1291`](https://github.com/tontinton/maki/blob/e0d87e35a45a313b8c3e03dca21ae5aef3d35050/maki-ui/src/event_loop.rs#L1277-L1291) [`maki-ui/src/agent/shared_queue.rs:94-120`](https://github.com/tontinton/maki/blob/e0d87e35a45a313b8c3e03dca21ae5aef3d35050/maki-ui/src/agent/shared_queue.rs#L94-L120)

`AgentLoop` refreshes instructions, prompt slots, and tool definitions for each queue item, creates run-scoped cancellation state, and constructs an `Agent` around the session's long-lived mutable history. This makes configuration/plugin changes visible between runs without discarding the conversation. [`maki-ui/src/agent/agent_loop.rs:192-286`](https://github.com/tontinton/maki/blob/e0d87e35a45a313b8c3e03dca21ae5aef3d35050/maki-ui/src/agent/agent_loop.rs#L192-L286)

### 2.2 `Agent::run`

A run records a rollback boundary, appends preamble and mailbox observations, appends the user's message/images, then repeatedly calls `turn()` until a non-tool answer, a configured turn limit, cancellation, or error. Cancellation is represented as a normal `DoneReason::Cancelled`; partially streamed assistant text is preserved with a cancellation marker and dangling tool calls are sanitised. [`maki-agent/src/agent/run.rs:199-250`](https://github.com/tontinton/maki/blob/e0d87e35a45a313b8c3e03dca21ae5aef3d35050/maki-agent/src/agent/run.rs#L199-L250) [`maki-agent/src/agent/history.rs:199-226`](https://github.com/tontinton/maki/blob/e0d87e35a45a313b8c3e03dca21ae5aef3d35050/maki-agent/src/agent/history.rs#L199-L226)

A turn:

1. Rebuilds the effective tool list, adding the current MCP session's loaded definitions.
2. Sends system prompt, complete history, tools, and options to the provider through retry-aware streaming.
3. Emits accounting and lifecycle events.
4. Appends an ordinary assistant answer or dispatches all tool calls.
5. Checks automatic compaction and queued interrupts.
6. Continues after tool use or finishes after a normal answer.

[`maki-agent/src/agent/run.rs:252-380`](https://github.com/tontinton/maki/blob/e0d87e35a45a313b8c3e03dca21ae5aef3d35050/maki-agent/src/agent/run.rs#L252-L380) [`maki-agent/src/agent/streaming.rs:87-153`](https://github.com/tontinton/maki/blob/e0d87e35a45a313b8c3e03dca21ae5aef3d35050/maki-agent/src/agent/streaming.rs#L87-L153)

If a provider returns an empty assistant message immediately after tool results, Maki stores an explicit empty marker and can inject a bounded nudge to continue. If output ends because of the provider's token limit, it can transparently request another turn up to a configured limit. These are narrow recovery behaviours in the main state machine rather than general exception retries. [`maki-agent/src/agent/run.rs:349-380`](https://github.com/tontinton/maki/blob/e0d87e35a45a313b8c3e03dca21ae5aef3d35050/maki-agent/src/agent/run.rs#L349-L380) [`maki-agent/src/agent/run.rs:419-444`](https://github.com/tontinton/maki/blob/e0d87e35a45a313b8c3e03dca21ae5aef3d35050/maki-agent/src/agent/run.rs#L419-L444)

### 2.3 Interrupts while running

Queued user input is polled between turns. It becomes a user message wrapped in `<user-interrupt>` while retaining a separate display value, so the model sees explicit timing semantics without the UI showing protocol markup. A queued compaction command runs in place and the same agent continues. [`maki-agent/src/agent/run.rs:537-561`](https://github.com/tontinton/maki/blob/e0d87e35a45a313b8c3e03dca21ae5aef3d35050/maki-agent/src/agent/run.rs#L537-L561)

This is intentionally not mid-token pre-emption. The safe interrupt boundary is after a complete model/tool turn, where history invariants are known.

## 3. Tools

### 3.1 Registry and schema

Native and Lua-backed tools implement a common `Tool` interface. A tool parses raw input once into a `ToolInvocation`; that parsed value then supplies its header, writable path, permission scopes, preview/start behaviour, and execution. The registry publishes immutable `ArcSwap<Vec<RegisteredTool>>` snapshots, so model-request reads avoid a hot-path mutex. [`maki-agent/src/tools/registry.rs:190-230`](https://github.com/tontinton/maki/blob/e0d87e35a45a313b8c3e03dca21ae5aef3d35050/maki-agent/src/tools/registry.rs#L190-L230) [`maki-agent/src/tools/registry.rs:250-314`](https://github.com/tontinton/maki/blob/e0d87e35a45a313b8c3e03dca21ae5aef3d35050/maki-agent/src/tools/registry.rs#L250-L314)

One internal `ParamSchema` generates closed model-facing JSON Schema and validates runtime input. Validation also repairs common model mistakes by coercing primitives and parsing stringified JSON containers. This keeps advertisement and execution close while accepting predictable LLM formatting errors. [`maki-agent/src/tools/schema.rs:95-157`](https://github.com/tontinton/maki/blob/e0d87e35a45a313b8c3e03dca21ae5aef3d35050/maki-agent/src/tools/schema.rs#L95-L157) [`maki-agent/src/tools/schema.rs:420-502`](https://github.com/tontinton/maki/blob/e0d87e35a45a313b8c3e03dca21ae5aef3d35050/maki-agent/src/tools/schema.rs#L420-L502) [`maki-agent/src/tools/schema.rs:617-667`](https://github.com/tontinton/maki/blob/e0d87e35a45a313b8c3e03dca21ae5aef3d35050/maki-agent/src/tools/schema.rs#L617-L667)

Definitions are rebuilt for each request and filtered by audience and current tool filters. Examples are adapted to provider capabilities. Audiences are also used by subagents and the interpreter to expose narrower tool sets without creating separate registries. [`maki-agent/src/tools/registry.rs:437-472`](https://github.com/tontinton/maki/blob/e0d87e35a45a313b8c3e03dca21ae5aef3d35050/maki-agent/src/tools/registry.rs#L437-L472)

### 3.2 Dispatch and concurrency

The practical dispatch order is:

1. session-local Lua handlers;
2. registered native/Lua tools;
3. synthetic MCP `tool_search`;
4. MCP tools from the session-local MCP index.

[`maki-agent/src/agent/tool_dispatch.rs:72-205`](https://github.com/tontinton/maki/blob/e0d87e35a45a313b8c3e03dca21ae5aef3d35050/maki-agent/src/agent/tool_dispatch.rs#L72-L205)

For registered tools, dispatch validates input, blocks writes prohibited by plan mode, publishes a start/preview event, checks permissions, executes, and publishes a structured completion event. `start` intentionally runs before permission approval so the UI can show exactly what is awaiting consent without performing the effect. [`maki-agent/src/agent/tool_dispatch.rs:105-186`](https://github.com/tontinton/maki/blob/e0d87e35a45a313b8c3e03dca21ae5aef3d35050/maki-agent/src/agent/tool_dispatch.rs#L105-L186)

All tool calls from one assistant message are added to a `TaskSet` and run concurrently. Panics become tool errors rather than process failures. Results are gathered and submitted to history as one user-role message containing matching tool-result blocks, preserving the provider's expected conversation shape. A three-identical-call threshold catches simple doom loops. [`maki-agent/src/agent/tool_dispatch.rs:383-468`](https://github.com/tontinton/maki/blob/e0d87e35a45a313b8c3e03dca21ae5aef3d35050/maki-agent/src/agent/tool_dispatch.rs#L383-L468) [`maki-agent/src/task_set.rs:1-50`](https://github.com/tontinton/maki/blob/e0d87e35a45a313b8c3e03dca21ae5aef3d35050/maki-agent/src/task_set.rs#L1-L50) [`maki-agent/src/types.rs:503-525`](https://github.com/tontinton/maki/blob/e0d87e35a45a313b8c3e03dca21ae5aef3d35050/maki-agent/src/types.rs#L503-L525)

### 3.3 MCP tools and deferred schemas

MCP tools are not actually stored in the same registry despite the registry module's “one lookup path” description. Each `McpSession` owns a separate tool index and loaded set; the dispatcher branches to it after a registry miss. MCP input schemas are sanitised before model exposure, but invocation arguments are forwarded to the MCP server rather than validated through Maki's `ParamSchema`. [`maki-agent/src/mcp/mod.rs:195-220`](https://github.com/tontinton/maki/blob/e0d87e35a45a313b8c3e03dca21ae5aef3d35050/maki-agent/src/mcp/mod.rs#L195-L220) [`maki-agent/src/mcp/mod.rs:966-1006`](https://github.com/tontinton/maki/blob/e0d87e35a45a313b8c3e03dca21ae5aef3d35050/maki-agent/src/mcp/mod.rs#L966-L1006) [`maki-agent/src/agent/tool_dispatch.rs:324-379`](https://github.com/tontinton/maki/blob/e0d87e35a45a313b8c3e03dca21ae5aef3d35050/maki-agent/src/agent/tool_dispatch.rs#L324-L379)

When enough MCP tools are marked deferrable, Maki sends only loaded and `always_load` definitions plus a compact `tool_search` definition. Searching ranks tool names above descriptions and loads at most five full definitions for the next turn. Loaded state belongs to the MCP session, so a subagent's discovery does not enlarge its parent's subsequent requests. [`maki-agent/src/mcp/mod.rs:301-306`](https://github.com/tontinton/maki/blob/e0d87e35a45a313b8c3e03dca21ae5aef3d35050/maki-agent/src/mcp/mod.rs#L301-L306) [`maki-agent/src/mcp/mod.rs:346-471`](https://github.com/tontinton/maki/blob/e0d87e35a45a313b8c3e03dca21ae5aef3d35050/maki-agent/src/mcp/mod.rs#L346-L471)

This turns tool-schema cost into an on-demand discovery step. It saves recurring context at the cost of an extra turn when an unloaded tool must first be found.

## 4. Code execution

### 4.1 Division of responsibility

`code_execution` is a built-in Lua plugin over a Rust interpreter bridge:

- Lua chooses which tools Python may see, builds Python signatures and descriptions, provides orchestration helpers, renders live output, and truncates model output.
- Rust runs Monty, enforces resource settings, rejects OS calls, converts values, and bridges async Python calls back through Maki tool dispatch.

[`plugins/code_execution/init.lua:1-4`](https://github.com/tontinton/maki/blob/e0d87e35a45a313b8c3e03dca21ae5aef3d35050/plugins/code_execution/init.lua#L1-L4) [`maki-lua/src/api/interpreter.rs:1-198`](https://github.com/tontinton/maki/blob/e0d87e35a45a313b8c3e03dca21ae5aef3d35050/maki-lua/src/api/interpreter.rs#L1-L198) [`maki-interpreter/src/runner.rs:1-4`](https://github.com/tontinton/maki/blob/e0d87e35a45a313b8c3e03dca21ae5aef3d35050/maki-interpreter/src/runner.rs#L1-L4)

A new sandbox is created for every call. The model writes async Python, for example to glob files, read them concurrently, filter them locally, and print only matching filenames. Tool functions return strings. This makes Python a context-side data plane rather than a source of unrestricted machine access. [`plugins/code_execution/init.lua:113-152`](https://github.com/tontinton/maki/blob/e0d87e35a45a313b8c3e03dca21ae5aef3d35050/plugins/code_execution/init.lua#L113-L152)

### 4.2 Tool exposure and policy preservation

The same `interpreter_tools` predicate enforces enabled state, current audience, and interpreter/workflow audience for both description and callable bindings. Consequently, a read-only subagent cannot reach write tools merely by calling them through Python. There is one filter mismatch: the description additionally applies the current `only`/`except` tool filter, while the binding table uses `get_tools({ config = config })`, which applies disabled tools but not the full request filter. A tool hidden by an allow-list can therefore remain callable if the model knows its name. [`plugins/code_execution/init.lua:155-237`](https://github.com/tontinton/maki/blob/e0d87e35a45a313b8c3e03dca21ae5aef3d35050/plugins/code_execution/init.lua#L155-L237) [`plugins/code_execution/init.lua:283-301`](https://github.com/tontinton/maki/blob/e0d87e35a45a313b8c3e03dca21ae5aef3d35050/plugins/code_execution/init.lua#L283-L301) [`maki-lua/src/api/tool.rs:933-953`](https://github.com/tontinton/maki/blob/e0d87e35a45a313b8c3e03dca21ae5aef3d35050/maki-lua/src/api/tool.rs#L933-L953)

Each Python tool call returns through `maki.agent.call_tool`, which uses a cloned `ToolContext` and the ordinary dispatcher. Mode, permission manager, cancellation, file tracker, MCP state, and audience therefore survive the hop through the interpreter. [`maki-lua/src/api/agent.rs:298-345`](https://github.com/tontinton/maki/blob/e0d87e35a45a313b8c3e03dca21ae5aef3d35050/maki-lua/src/api/agent.rs#L298-L345) [`maki-lua/src/api/util/ctx.rs:43-75`](https://github.com/tontinton/maki/blob/e0d87e35a45a313b8c3e03dca21ae5aef3d35050/maki-lua/src/api/util/ctx.rs#L43-L75)

An important limitation is that `maki.api.get_tools()` enumerates the shared `ToolRegistry`, while MCP definitions live in `McpSession`. The built-in plugin therefore does not expose dynamically discovered MCP tools as Python functions at this revision. [`plugins/code_execution/init.lua:286-301`](https://github.com/tontinton/maki/blob/e0d87e35a45a313b8c3e03dca21ae5aef3d35050/plugins/code_execution/init.lua#L286-L301) [`maki-lua/src/api/tool.rs:933-953`](https://github.com/tontinton/maki/blob/e0d87e35a45a313b8c3e03dca21ae5aef3d35050/maki-lua/src/api/tool.rs#L933-L953)

### 4.3 Async batching

Monty's external async calls are host-driven. The Rust bridge streams stdout and batches pending external calls over a FIFO channel; Lua dispatches each batch concurrently with `join_all`. The custom Python `gather` catches each tool call's `RuntimeError`, preserving successful siblings instead of losing all paid-for results when one tool fails. [`maki-lua/src/api/interpreter.rs:38-61`](https://github.com/tontinton/maki/blob/e0d87e35a45a313b8c3e03dca21ae5aef3d35050/maki-lua/src/api/interpreter.rs#L38-L61) [`maki-lua/src/api/interpreter.rs:115-198`](https://github.com/tontinton/maki/blob/e0d87e35a45a313b8c3e03dca21ae5aef3d35050/maki-lua/src/api/interpreter.rs#L115-L198) [`plugins/code_execution/init.lua:25-50`](https://github.com/tontinton/maki/blob/e0d87e35a45a313b8c3e03dca21ae5aef3d35050/plugins/code_execution/init.lua#L25-L50)

Interpreter waiting time for host tool calls does not consume the script's execution budget. That separates model-controlled CPU time from host I/O time and avoids timing out a script merely because a delegated tool is slow. The configured defaults are 30 seconds and 50 MiB, with lower bounds of 5 seconds and 10 MiB. [`plugins/code_execution/init.lua:56-63`](https://github.com/tontinton/maki/blob/e0d87e35a45a313b8c3e03dca21ae5aef3d35050/plugins/code_execution/init.lua#L56-L63) [`maki-interpreter/src/runner.rs:116-235`](https://github.com/tontinton/maki/blob/e0d87e35a45a313b8c3e03dca21ae5aef3d35050/maki-interpreter/src/runner.rs#L116-L235)

### 4.4 Sandbox boundary

The runner rejects every direct Monty `OsCall`, configures duration, recursion, and memory limits, and exposes only explicit host functions. It does not start CPython, resolve `sys.path`, or load site packages. It can still perform filesystem, network, and shell effects indirectly when an exposed Maki tool provides them; the default `bash` plugin has all tool audiences and is therefore callable from Python, subject to the ordinary dispatcher and permissions. [`maki-interpreter/src/runner.rs:116-278`](https://github.com/tontinton/maki/blob/e0d87e35a45a313b8c3e03dca21ae5aef3d35050/maki-interpreter/src/runner.rs#L116-L278) [`Cargo.lock:3120-3122`](https://github.com/tontinton/maki/blob/e0d87e35a45a313b8c3e03dca21ae5aef3d35050/Cargo.lock#L3120-L3122) [`maki-config/src/lib.rs:54-57`](https://github.com/tontinton/maki/blob/e0d87e35a45a313b8c3e03dca21ae5aef3d35050/maki-config/src/lib.rs#L54-L57) [`maki-lua/src/api/tool.rs:1153-1155`](https://github.com/tontinton/maki/blob/e0d87e35a45a313b8c3e03dca21ae5aef3d35050/maki-lua/src/api/tool.rs#L1153-L1155)

It is still an in-process interpreter, not an OS sandbox. Maki's global accounting allocator explicitly documents that overlapping runs share accounting, allocations may be freed on another thread, and the limit is a runaway-code guardrail rather than a security boundary. This design avoids deadlocking nested interpreter runs but does not provide kernel-enforced isolation against hostile code. [`maki-interpreter/src/alloc.rs:1-27`](https://github.com/tontinton/maki/blob/e0d87e35a45a313b8c3e03dca21ae5aef3d35050/maki-interpreter/src/alloc.rs#L1-L27) [`maki-interpreter/src/runner.rs:389-406`](https://github.com/tontinton/maki/blob/e0d87e35a45a313b8c3e03dca21ae5aef3d35050/maki-interpreter/src/runner.rs#L389-L406)

### 4.5 Context containment

The tool keeps rich, streaming output in a Lua-backed UI buffer. On success it returns stdout plus any final expression value through line/byte truncation, with defaults of 2,000 lines and 50 KiB. Ordinary interpreter errors bypass that truncation and return the error plus an optional gather hint; cancellation and timeout return a separately cut partial result. Nested tool images are replaced by a textual note when converted to Python strings. UI snapshots, render state, annotations, and clicks are retained separately from `ToolOutput::as_text()`. [`plugins/code_execution/init.lua:11-14`](https://github.com/tontinton/maki/blob/e0d87e35a45a313b8c3e03dca21ae5aef3d35050/plugins/code_execution/init.lua#L11-L14) [`plugins/code_execution/init.lua:258-335`](https://github.com/tontinton/maki/blob/e0d87e35a45a313b8c3e03dca21ae5aef3d35050/plugins/code_execution/init.lua#L258-L335) [`maki-agent/src/tools/interpreter_bridge.rs:7-37`](https://github.com/tontinton/maki/blob/e0d87e35a45a313b8c3e03dca21ae5aef3d35050/maki-agent/src/tools/interpreter_bridge.rs#L7-L37) [`maki-agent/src/types.rs:300-364`](https://github.com/tontinton/maki/blob/e0d87e35a45a313b8c3e03dca21ae5aef3d35050/maki-agent/src/types.rs#L300-L364)

This separation is central to Maki's context strategy: the human can retain visibility into execution while the next model request receives only the part intended for reasoning.

## 5. Extensibility and plugins

### 5.1 Plugin host

Maki creates one sandboxed Luau VM on its own OS thread. It applies a 512 MiB VM limit, removes ambient `require`, `io`, and `package`, enables Lua sandbox mode, and exposes a host-built `maki` API. Work crosses the VM boundary through request channels. [`maki-lua/src/runtime.rs:100-103`](https://github.com/tontinton/maki/blob/e0d87e35a45a313b8c3e03dca21ae5aef3d35050/maki-lua/src/runtime.rs#L100-L103) [`maki-lua/src/runtime.rs:1369-1454`](https://github.com/tontinton/maki/blob/e0d87e35a45a313b8c3e03dca21ae5aef3d35050/maki-lua/src/runtime.rs#L1369-L1454)

User extension discovery is intentionally small. Maki executes the first applicable global `init.lua` and then `<cwd>/.maki/init.lua`; modules below each `lua/` directory are loaded only by explicit `require`. Built-in plugins are embedded in the binary and loaded after configuration. There is no external package manager or arbitrary plugin-directory scan. [`maki-lua/src/loader.rs:198-307`](https://github.com/tontinton/maki/blob/e0d87e35a45a313b8c3e03dca21ae5aef3d35050/maki-lua/src/loader.rs#L198-L307) [`site/docs/content/plugins/_index.md:17-31`](https://github.com/tontinton/maki/blob/e0d87e35a45a313b8c3e03dca21ae5aef3d35050/site/docs/content/plugins/_index.md#L17-L31)

The custom module loader checks generated modules, embedded modules, then local modules; uses one environment and cache per plugin load; and rejects lexical traversal outside the plugin's `lua/` root. Embedded helper names cannot be shadowed by local files. [`maki-lua/src/runtime.rs:553-652`](https://github.com/tontinton/maki/blob/e0d87e35a45a313b8c3e03dca21ae5aef3d35050/maki-lua/src/runtime.rs#L553-L652) [`maki-lua/src/runtime.rs:1676-1715`](https://github.com/tontinton/maki/blob/e0d87e35a45a313b8c3e03dca21ae5aef3d35050/maki-lua/src/runtime.rs#L1676-L1715)

### 5.2 Registration and lifecycle

A plugin's tool registrations and permission rules are staged while its source executes. Only a successful load commits them, and replacement of all tools owned by that plugin is atomic. The runtime tracks plugin-owned commands, jobs, async tasks, prompt hints, options, autocmds, slots, keymaps, UI hints, and registry keys so they can be torn down centrally. [`maki-lua/src/runtime.rs:1734-1899`](https://github.com/tontinton/maki/blob/e0d87e35a45a313b8c3e03dca21ae5aef3d35050/maki-lua/src/runtime.rs#L1734-L1899) [`maki-agent/src/tools/registry.rs:361-397`](https://github.com/tontinton/maki/blob/e0d87e35a45a313b8c3e03dca21ae5aef3d35050/maki-agent/src/tools/registry.rs#L361-L397)

`/reload` reconstructs the whole host rather than hot-swapping one module. It clears Lua tools from the registry, starts a fresh host/configuration/builtin set, and lets the old VM join in the background. This loses in-memory Lua state but reduces stale-registration cases. [`src/cmd/tui.rs:369-401`](https://github.com/tontinton/maki/blob/e0d87e35a45a313b8c3e03dca21ae5aef3d35050/src/cmd/tui.rs#L369-L401) [`maki-lua/src/loader.rs:182-192`](https://github.com/tontinton/maki/blob/e0d87e35a45a313b8c3e03dca21ae5aef3d35050/maki-lua/src/loader.rs#L182-L192)

### 5.3 Extension seams

The public Lua surface includes:

| Seam | Mechanism |
|---|---|
| Agent tools | JSON schema, handler, dynamic description, examples, audiences, timeouts, permission scopes, mutable paths, start/restore/header callbacks, rich UI state, diffs and images. [`maki-lua/src/api/tool.rs:1243-1343`](https://github.com/tontinton/maki/blob/e0d87e35a45a313b8c3e03dca21ae5aef3d35050/maki-lua/src/api/tool.rs#L1243-L1343) |
| Commands and keymaps | Immutable snapshots read by the TUI; invocation routes back to the Lua thread. [`maki-lua/src/api/tool.rs:1362-1405`](https://github.com/tontinton/maki/blob/e0d87e35a45a313b8c3e03dca21ae5aef3d35050/maki-lua/src/api/tool.rs#L1362-L1405) [`maki-lua/src/api/keymap.rs:247-321`](https://github.com/tontinton/maki/blob/e0d87e35a45a313b8c3e03dca21ae5aef3d35050/maki-lua/src/api/keymap.rs#L247-L321) |
| Prompt contributions | Aggregate hints and singleton identity/tone content are resolved before prompt assembly. [`maki-lua/src/runtime.rs:1552-1643`](https://github.com/tontinton/maki/blob/e0d87e35a45a313b8c3e03dca21ae5aef3d35050/maki-lua/src/runtime.rs#L1552-L1643) [`maki-agent/src/prompt.rs:35-82`](https://github.com/tontinton/maki/blob/e0d87e35a45a313b8c3e03dca21ae5aef3d35050/maki-agent/src/prompt.rs#L35-L82) |
| Function slots | A plugin declares a default function; other plugins wrap it with one-shot `prev` functions, including before the owner loads. [`maki-lua/src/api/slot.rs:117-257`](https://github.com/tontinton/maki/blob/e0d87e35a45a313b8c3e03dca21ae5aef3d35050/maki-lua/src/api/slot.rs#L117-L257) |
| Autocmds | Named event hooks with patterns, one-shot listeners, custom data, bounded recursive dispatch, and isolated callback errors. [`maki-lua/src/api/autocmd.rs:66-230`](https://github.com/tontinton/maki/blob/e0d87e35a45a313b8c3e03dca21ae5aef3d35050/maki-lua/src/api/autocmd.rs#L66-L230) |
| UI | Styled mutable buffers, click/change callbacks, asynchronous windows, splits, status hints, and restorable tool displays. [`maki-lua/src/api/ui/buf.rs:14-145`](https://github.com/tontinton/maki/blob/e0d87e35a45a313b8c3e03dca21ae5aef3d35050/maki-lua/src/api/ui/buf.rs#L14-L145) [`maki-lua/src/api/ui/win.rs:17-328`](https://github.com/tontinton/maki/blob/e0d87e35a45a313b8c3e03dca21ae5aef3d35050/maki-lua/src/api/ui/win.rs#L17-L328) |
| Subagents | Model resolution, system prompt construction, narrowed tool audiences, local tools, child cancellation, and separate history. [`maki-lua/src/api/agent.rs:387-580`](https://github.com/tontinton/maki/blob/e0d87e35a45a313b8c3e03dca21ae5aef3d35050/maki-lua/src/api/agent.rs#L387-L580) |

The built-in plugins demonstrate that the extension boundary is not restricted to leaf commands. `task` implements subagent policy in Lua, `code_execution` implements interpreter policy, `question` owns an interactive form and restoration state, and `memory` contributes tools, permission rules, prompt content, and commands. [`plugins/task/init.lua:126-264`](https://github.com/tontinton/maki/blob/e0d87e35a45a313b8c3e03dca21ae5aef3d35050/plugins/task/init.lua#L126-L264) [`plugins/question/init.lua:36-111`](https://github.com/tontinton/maki/blob/e0d87e35a45a313b8c3e03dca21ae5aef3d35050/plugins/question/init.lua#L36-L111) [`plugins/memory/init.lua:175-249`](https://github.com/tontinton/maki/blob/e0d87e35a45a313b8c3e03dca21ae5aef3d35050/plugins/memory/init.lua#L175-L249)

### 5.4 Plugin runtime controls

Lua work is capped at 64 concurrent coroutines. The runtime combines a watchdog for non-yielding code, cancellation-aware deadlines for yielded work, and a short cleanup grace. This bounds plugin mistakes while retaining asynchronous UI and tool APIs inside one VM. [`maki-lua/src/runtime.rs:676-764`](https://github.com/tontinton/maki/blob/e0d87e35a45a313b8c3e03dca21ae5aef3d35050/maki-lua/src/runtime.rs#L676-L764) [`maki-lua/src/runtime.rs:1038-1101`](https://github.com/tontinton/maki/blob/e0d87e35a45a313b8c3e03dca21ae5aef3d35050/maki-lua/src/runtime.rs#L1038-L1101)

Plugin capabilities are separate from agent tool permissions. `plugin.toml` controls filesystem read/write, network, process execution, and environment access. Missing or invalid manifests deny all gated APIs; once a valid manifest exists, unspecified fields default to allowed. Built-ins are trusted. This is an API capability boundary inside the process, not process isolation for hostile native code. [`maki-lua/src/plugin_permissions.rs:10-159`](https://github.com/tontinton/maki/blob/e0d87e35a45a313b8c3e03dca21ae5aef3d35050/maki-lua/src/plugin_permissions.rs#L10-L159) [`maki-lua/src/loader.rs:294-304`](https://github.com/tontinton/maki/blob/e0d87e35a45a313b8c3e03dca21ae5aef3d35050/maki-lua/src/loader.rs#L294-L304)

Dynamic model providers use a different extension seam: executable scripts in the config `providers/` directory implement metadata, model listing, credential resolution, and login/refresh commands. They are discovered by `maki-providers`, not registered through Lua. [`maki-providers/src/providers/dynamic.rs:394-599`](https://github.com/tontinton/maki/blob/e0d87e35a45a313b8c3e03dca21ae5aef3d35050/maki-providers/src/providers/dynamic.rs#L394-L599)

## 6. Model providers

### 6.1 Unified transport boundary

Every backend implements the same `Provider` trait: stream a normalised message history and tool schema into `ProviderEvent`s plus a final `StreamResponse`, list models, and optionally expose usage, auth refresh, key rotation, or model adjustment. The core agent therefore depends on one streaming contract rather than provider SDK types. [`maki-providers/src/provider.rs:254-288`](https://github.com/tontinton/maki/blob/e0d87e35a45a313b8c3e03dca21ae5aef3d35050/maki-providers/src/provider.rs#L254-L288) [`maki-providers/src/types.rs:335-350`](https://github.com/tontinton/maki/blob/e0d87e35a45a313b8c3e03dca21ae5aef3d35050/maki-providers/src/types.rs#L335-L350)

`ProviderKind` holds the built-in transports and their fallback capabilities. `provider_for_slug` resolves a slug in precedence order across built-ins, executable dynamic providers, `providers.toml` custom providers, and models.dev catalogue providers. `from_model` then lets the selected provider refine model metadata before use. [`maki-providers/src/provider.rs:39-252`](https://github.com/tontinton/maki/blob/e0d87e35a45a313b8c3e03dca21ae5aef3d35050/maki-providers/src/provider.rs#L39-L252) [`maki-providers/src/provider.rs:290-337`](https://github.com/tontinton/maki/blob/e0d87e35a45a313b8c3e03dca21ae5aef3d35050/maki-providers/src/provider.rs#L290-L337)

Provider adapters do more than change URLs. They own wire-format translation into Maki's common `Message`, `ContentBlock`, stop-reason, usage, and streaming event types. Model family and per-model overrides tell the rest of the harness whether tool examples, vision, thinking, output limits, and context limits are available without leaking transport-specific response types. [`maki-providers/src/model.rs:234-303`](https://github.com/tontinton/maki/blob/e0d87e35a45a313b8c3e03dca21ae5aef3d35050/maki-providers/src/model.rs#L234-L303) [`maki-providers/src/types.rs:335-380`](https://github.com/tontinton/maki/blob/e0d87e35a45a313b8c3e03dca21ae5aef3d35050/maki-providers/src/types.rs#L335-L380)

### 6.2 Models, tiers, and discovery

Models use `provider/model_id` identities and are assigned to weak, medium, strong, or compaction tiers. Lookup combines compiled manifests, runtime provider discovery, user tier overrides, and protocol fallbacks; prefix matching allows dated model snapshots to inherit metadata without updating every exact identifier. [`maki-providers/src/model.rs:1-4`](https://github.com/tontinton/maki/blob/e0d87e35a45a313b8c3e03dca21ae5aef3d35050/maki-providers/src/model.rs#L1-L4) [`maki-providers/src/model.rs:417-518`](https://github.com/tontinton/maki/blob/e0d87e35a45a313b8c3e03dca21ae5aef3d35050/maki-providers/src/model.rs#L417-L518)

Offline discovery returns static and cached candidates immediately. Live discovery fans out provider `list_models()` calls, stores richer metadata in the registry, and falls back to compiled manifests on provider errors. Startup model selection checks an explicit model, the saved model, configured default, then available strong/medium providers in a fixed priority order. [`maki-providers/src/provider.rs:408-520`](https://github.com/tontinton/maki/blob/e0d87e35a45a313b8c3e03dca21ae5aef3d35050/maki-providers/src/provider.rs#L408-L520) [`src/setup.rs:23-93`](https://github.com/tontinton/maki/blob/e0d87e35a45a313b8c3e03dca21ae5aef3d35050/src/setup.rs#L23-L93)

### 6.3 Provider extension seams

`providers.toml` can define a custom slug by selecting one of four supported protocols: OpenAI chat completions, OpenAI responses, Anthropic, or Google. Configuration supplies endpoint, credentials, model metadata, discovery policy, tiers, capabilities, and pricing; construction reuses the corresponding built-in wire adapter. [`maki-providers/src/providers/custom.rs:20-100`](https://github.com/tontinton/maki/blob/e0d87e35a45a313b8c3e03dca21ae5aef3d35050/maki-providers/src/providers/custom.rs#L20-L100) [`maki-providers/src/providers/custom.rs:102-174`](https://github.com/tontinton/maki/blob/e0d87e35a45a313b8c3e03dca21ae5aef3d35050/maki-providers/src/providers/custom.rs#L102-L174)

Executable provider scripts are the more capable seam. A discovered script reports metadata and optional models, resolves headers/base URL, and may implement interactive login/logout plus token refresh. The script chooses a built-in base transport, so it extends authentication and catalogue behaviour without reimplementing streaming. Descriptions are cached by file stamp, malformed model entries are isolated, slug collisions have explicit precedence, and parallel authentication failures share a refresh gate. [`maki-providers/src/providers/dynamic.rs:394-521`](https://github.com/tontinton/maki/blob/e0d87e35a45a313b8c3e03dca21ae5aef3d35050/maki-providers/src/providers/dynamic.rs#L394-L521) [`maki-providers/src/providers/dynamic.rs:533-650`](https://github.com/tontinton/maki/blob/e0d87e35a45a313b8c3e03dca21ae5aef3d35050/maki-providers/src/providers/dynamic.rs#L533-L650) [`maki-providers/src/providers/dynamic.rs:672-783`](https://github.com/tontinton/maki/blob/e0d87e35a45a313b8c3e03dca21ae5aef3d35050/maki-providers/src/providers/dynamic.rs#L672-L783)

This layer is separate from Lua plugins. Lua extends harness behaviour inside a running process; provider scripts extend model/auth resolution at agent construction time and communicate through process stdout and exit status.

## 7. Events

### 7.1 Agent event protocol

`AgentEvent` is the cross-host protocol. Its variants cover:

- streamed text and thinking;
- pending/start/output/done tool lifecycle;
- turn completion, accounting, and submitted tool results;
- queue consumption/drain;
- run completion, retries, errors, authentication, and permissions;
- compaction and nudges;
- subagent transcripts;
- static and live rich-tool buffers;
- prompt-processing progress.

[`maki-agent/src/types.rs:553-613`](https://github.com/tontinton/maki/blob/e0d87e35a45a313b8c3e03dca21ae5aef3d35050/maki-agent/src/types.rs#L553-L613)

Every event is wrapped in an `Envelope` containing optional `SubagentInfo` and the producing run's `run_id`. `EventSender` stamps the `run_id` and initially leaves subagent identity empty. A Lua child-agent relay attaches `SubagentInfo` to forwarded events and deliberately suppresses child `Done`, `Error`, `ToolOutput`, `ToolPending`, and nested `SubagentHistory` events. Provider deltas are translated at the streaming boundary, while higher-level tool lifecycle events come from dispatch. [`maki-agent/src/types.rs:858-923`](https://github.com/tontinton/maki/blob/e0d87e35a45a313b8c3e03dca21ae5aef3d35050/maki-agent/src/types.rs#L858-L923) [`maki-lua/src/api/agent.rs:82-110`](https://github.com/tontinton/maki/blob/e0d87e35a45a313b8c3e03dca21ae5aef3d35050/maki-lua/src/api/agent.rs#L82-L110) [`maki-agent/src/agent/streaming.rs:28-63`](https://github.com/tontinton/maki/blob/e0d87e35a45a313b8c3e03dca21ae5aef3d35050/maki-agent/src/agent/streaming.rs#L28-L63)

Cancellation and agent respawn increment `run_id` before replacing work. The TUI retains the existing output channel but discards envelopes from older generations. This turns late delivery from a teardown race into an ordinary equality check. [`maki-ui/src/app/mod.rs:1077-1097`](https://github.com/tontinton/maki/blob/e0d87e35a45a313b8c3e03dca21ae5aef3d35050/maki-ui/src/app/mod.rs#L1077-L1097) [`maki-ui/src/app/mod.rs:1119-1156`](https://github.com/tontinton/maki/blob/e0d87e35a45a313b8c3e03dca21ae5aef3d35050/maki-ui/src/app/mod.rs#L1119-L1156) [`maki-ui/src/agent/mod.rs:135-173`](https://github.com/tontinton/maki/blob/e0d87e35a45a313b8c3e03dca21ae5aef3d35050/maki-ui/src/agent/mod.rs#L135-L173)

### 7.2 Lua autocmds

Lua autocmds are a separate, higher-level extension protocol. Host events include `TurnStart`, `TurnEnd`, `TurnError`, `ToolStart`, `ToolDone`, `SessionReset`, `SessionFocusChanged`, and `SessionStatusChanged`; plugins may also fire custom event names. Matching listeners are snapshotted, `once` handlers are removed before invocation, callback errors are isolated, and recursive event/slot dispatch is bounded to eight levels. [`maki-lua/src/api/autocmd.rs:66-153`](https://github.com/tontinton/maki/blob/e0d87e35a45a313b8c3e03dca21ae5aef3d35050/maki-lua/src/api/autocmd.rs#L66-L153) [`maki-lua/src/api/util/dispatch.rs:7-78`](https://github.com/tontinton/maki/blob/e0d87e35a45a313b8c3e03dca21ae5aef3d35050/maki-lua/src/api/util/dispatch.rs#L7-L78)

Autocmd callbacks are synchronous Lua calls, so the event itself is suited to short reactions. Longer work can be scheduled through the plugin async APIs. UI consequences return later through `UiAction`, avoiding re-entrant mutation of Rust `App` state. [`maki-lua/src/api/autocmd.rs:91-117`](https://github.com/tontinton/maki/blob/e0d87e35a45a313b8c3e03dca21ae5aef3d35050/maki-lua/src/api/autocmd.rs#L91-L117) [`maki-lua/src/runtime.rs:2827-2830`](https://github.com/tontinton/maki/blob/e0d87e35a45a313b8c3e03dca21ae5aef3d35050/maki-lua/src/runtime.rs#L2827-L2830)

## 8. Conversation history and “tree”

### 8.1 Linear root history

The persisted `Session` has one `Arc<Vec<Message>>`. Tool outputs and subagent messages are maps keyed by tool-use ID, and stored subagent metadata is a flat vector. There is no parent-session field, branch node ID, or recursive conversation structure. [`maki-storage/src/sessions.rs:195-208`](https://github.com/tontinton/maki/blob/e0d87e35a45a313b8c3e03dca21ae5aef3d35050/maki-storage/src/sessions.rs#L195-L208)

Live `History` publishes `HistorySnapshot { epoch, Arc<Vec<Message>> }` through `ArcSwap`. Appends use copy-on-write without changing the epoch; replacement and truncation mint a new epoch. Consumers can checkpoint a frozen snapshot without locking the agent's mutation path or reconstructing history from UI events. [`maki-agent/src/agent/history.rs:11-45`](https://github.com/tontinton/maki/blob/e0d87e35a45a313b8c3e03dca21ae5aef3d35050/maki-agent/src/agent/history.rs#L11-L45) [`maki-agent/src/agent/history.rs:82-112`](https://github.com/tontinton/maki/blob/e0d87e35a45a313b8c3e03dca21ae5aef3d35050/maki-agent/src/agent/history.rs#L82-L112)

The UI checkpoints that snapshot and UI metadata at most once per frame. Storage appends message deltas while the cursor and epoch match; divergence or shrinkage triggers a canonical JSONL rewrite. A shrinking rewrite archives the previous log first. [`maki-ui/src/app/session.rs:64-113`](https://github.com/tontinton/maki/blob/e0d87e35a45a313b8c3e03dca21ae5aef3d35050/maki-ui/src/app/session.rs#L64-L113) [`maki-ui/src/storage_writer.rs:156-175`](https://github.com/tontinton/maki/blob/e0d87e35a45a313b8c3e03dca21ae5aef3d35050/maki-ui/src/storage_writer.rs#L156-L175) [`maki-storage/src/sessions.rs:658-838`](https://github.com/tontinton/maki/blob/e0d87e35a45a313b8c3e03dca21ae5aef3d35050/maki-storage/src/sessions.rs#L658-L838)

### 8.2 Rewind

The rewind picker lists canonical user messages. Selecting one truncates history immediately before that prompt, prunes tool outputs and subagent histories no longer reachable from retained tool calls, restores the selected prompt into the input box, and respawns the agent against the shortened history. [`maki-ui/src/components/rewind_picker.rs:29-73`](https://github.com/tontinton/maki/blob/e0d87e35a45a313b8c3e03dca21ae5aef3d35050/maki-ui/src/components/rewind_picker.rs#L29-L73) [`maki-ui/src/app/session.rs:315-340`](https://github.com/tontinton/maki/blob/e0d87e35a45a313b8c3e03dca21ae5aef3d35050/maki-ui/src/app/session.rs#L315-L340) [`maki-storage/src/sessions.rs:1599-1616`](https://github.com/tontinton/maki/blob/e0d87e35a45a313b8c3e03dca21ae5aef3d35050/maki-storage/src/sessions.rs#L1599-L1616)

This is destructive replacement of the current session, not creation of a sibling branch. The archived JSONL protects data operationally but is not a navigable conversation node.

### 8.3 Fork and subagents

SDK `--fork-session` loads a source session's messages but deliberately drops its `SessionRef`, causing a fresh session ID unless explicitly overridden. No parent link is persisted. ACP `session/load`, by contrast, resumes the original session reference. [`src/sdk_mode.rs:663-694`](https://github.com/tontinton/maki/blob/e0d87e35a45a313b8c3e03dca21ae5aef3d35050/src/sdk_mode.rs#L663-L694) [`maki-agent/src/headless.rs:335-340`](https://github.com/tontinton/maki/blob/e0d87e35a45a313b8c3e03dca21ae5aef3d35050/maki-agent/src/headless.rs#L335-L340) [`maki-acp/src/server.rs:268-300`](https://github.com/tontinton/maki/blob/e0d87e35a45a313b8c3e03dca21ae5aef3d35050/maki-acp/src/server.rs#L268-L300)

A Lua-created subagent gets its own history, provider/model, tool set, cancellation child, and MCP session. Its envelopes carry `SubagentInfo { parent_tool_use_id, name, prompt, model }`; the TUI uses the parent tool-use ID to create a separate chat pane. On close, the child transcript is persisted in the parent session's subagent-message map under that same ID. [`maki-lua/src/api/agent.rs:387-546`](https://github.com/tontinton/maki/blob/e0d87e35a45a313b8c3e03dca21ae5aef3d35050/maki-lua/src/api/agent.rs#L387-L546) [`maki-lua/src/api/agent.rs:697-715`](https://github.com/tontinton/maki/blob/e0d87e35a45a313b8c3e03dca21ae5aef3d35050/maki-lua/src/api/agent.rs#L697-L715) [`maki-ui/src/app/mod.rs:1335-1360`](https://github.com/tontinton/maki/blob/e0d87e35a45a313b8c3e03dca21ae5aef3d35050/maki-ui/src/app/mod.rs#L1335-L1360)

The resulting shape is a linear root transcript with inspectable child-transcript attachments. It provides useful task visibility without implementing a general conversation DAG.

## 9. Context management

### 9.1 Compaction

Automatic compaction triggers when measured or estimated input reaches the model context window minus a configured reserve; the default reserve is 20 percent. Provider-reported usage replaces estimates after each model response, while newly appended tool results are estimated at four bytes per token. The estimator explicitly excludes the system prompt and tool schemas. [`maki-agent/src/agent/compaction.rs:147-151`](https://github.com/tontinton/maki/blob/e0d87e35a45a313b8c3e03dca21ae5aef3d35050/maki-agent/src/agent/compaction.rs#L147-L151) [`maki-config/src/lib.rs:31-32`](https://github.com/tontinton/maki/blob/e0d87e35a45a313b8c3e03dca21ae5aef3d35050/maki-config/src/lib.rs#L31-L32) [`maki-agent/src/agent/run.rs:568-593`](https://github.com/tontinton/maki/blob/e0d87e35a45a313b8c3e03dca21ae5aef3d35050/maki-agent/src/agent/run.rs#L568-L593)

Before asking a model to summarise, compaction:

- removes orphaned tool results;
- replaces images with `[image]`;
- removes thinking blocks;
- replaces all but the newest three tool results with placeholders;
- adds a focused compaction instruction;
- retries context overflow by dropping the oldest coherent rounds.

[`maki-agent/src/agent/compaction.rs:28-80`](https://github.com/tontinton/maki/blob/e0d87e35a45a313b8c3e03dca21ae5aef3d35050/maki-agent/src/agent/compaction.rs#L28-L80) [`maki-agent/src/agent/compaction.rs:154-223`](https://github.com/tontinton/maki/blob/e0d87e35a45a313b8c3e03dca21ae5aef3d35050/maki-agent/src/agent/compaction.rs#L154-L223)

A successful summary replaces the entire live history with a synthetic user question, “What did we do so far?”, and the assistant summary. Automatic compaction then appends a continuation instruction. An empty summary leaves original history untouched. [`maki-agent/src/agent/compaction.rs:96-115`](https://github.com/tontinton/maki/blob/e0d87e35a45a313b8c3e03dca21ae5aef3d35050/maki-agent/src/agent/compaction.rs#L96-L115) [`maki-agent/src/agent/run.rs:514-535`](https://github.com/tontinton/maki/blob/e0d87e35a45a313b8c3e03dca21ae5aef3d35050/maki-agent/src/agent/run.rs#L514-L535)

This is intentionally lossy. Old image pixels, thinking, and detailed tool results survive only if the summary captures their consequences.

### 9.2 Context-control mechanisms outside compaction

Maki applies several narrower mechanisms before falling back to global summarisation:

- the `index` plugin uses tree-sitter to return structural file skeletons and line ranges before full reads; [`plugins/index/init.lua:1-120`](https://github.com/tontinton/maki/blob/e0d87e35a45a313b8c3e03dca21ae5aef3d35050/plugins/index/init.lua#L1-L120)
- deferred MCP tools keep most schemas out of repeated requests; [`maki-agent/src/mcp/mod.rs:346-388`](https://github.com/tontinton/maki/blob/e0d87e35a45a313b8c3e03dca21ae5aef3d35050/maki-agent/src/mcp/mod.rs#L346-L388)
- `code_execution` lets tool output be filtered and joined before the model sees it; [`plugins/code_execution/init.lua:113-152`](https://github.com/tontinton/maki/blob/e0d87e35a45a313b8c3e03dca21ae5aef3d35050/plugins/code_execution/init.lua#L113-L152)
- rich UI rendering and model-facing tool text are different fields; [`maki-agent/src/types.rs:300-364`](https://github.com/tontinton/maki/blob/e0d87e35a45a313b8c3e03dca21ae5aef3d35050/maki-agent/src/types.rs#L300-L364)
- subagents have their own histories and MCP loaded sets; [`maki-lua/src/api/agent.rs:387-546`](https://github.com/tontinton/maki/blob/e0d87e35a45a313b8c3e03dca21ae5aef3d35050/maki-lua/src/api/agent.rs#L387-L546)
- output limits are applied at the boundary where text returns to model history rather than necessarily hiding it from the human. [`plugins/code_execution/init.lua:258-335`](https://github.com/tontinton/maki/blob/e0d87e35a45a313b8c3e03dca21ae5aef3d35050/plugins/code_execution/init.lua#L258-L335)

The common pattern is selective publication: retain operational detail in local state, storage, or UI, and explicitly construct the smaller representation sent to the model.

## 10. Rust TUI

### 10.1 Elm-inspired update/effects split

`App` owns session UI state. External input is normalised into `Msg::Key`, `Paste`, `Mouse`, coalesced `Scroll`, or `Agent(Envelope)`. `App::update(&mut self, Msg) -> Vec<Action>` mutates state and returns most requested effects for the event loop to perform. Some effects, notably Lua turn/tool autocmds, are fired directly from app methods. `App::view` composes the focused transcript, input, overlays, splits, status, and hit-test zones. [`maki-ui/src/app/mod.rs:205-269`](https://github.com/tontinton/maki/blob/e0d87e35a45a313b8c3e03dca21ae5aef3d35050/maki-ui/src/app/mod.rs#L205-L269) [`maki-ui/src/app/mod.rs:474-511`](https://github.com/tontinton/maki/blob/e0d87e35a45a313b8c3e03dca21ae5aef3d35050/maki-ui/src/app/mod.rs#L474-L511) [`maki-ui/src/app/mod.rs:1177-1192`](https://github.com/tontinton/maki/blob/e0d87e35a45a313b8c3e03dca21ae5aef3d35050/maki-ui/src/app/mod.rs#L1177-L1192) [`maki-ui/src/event_loop.rs:1254-1385`](https://github.com/tontinton/maki/blob/e0d87e35a45a313b8c3e03dca21ae5aef3d35050/maki-ui/src/event_loop.rs#L1254-L1385) [`maki-ui/src/app/view.rs:33-46`](https://github.com/tontinton/maki/blob/e0d87e35a45a313b8c3e03dca21ae5aef3d35050/maki-ui/src/app/view.rs#L33-L46)

It is not strict Elm: `update` mutates in place, a few effects occur directly, and `view` also updates render-local caches and geometry. The useful boundary is single-threaded UI ownership plus an action-oriented effect path, not reducer purity. [`maki-ui/src/components/messages/mod.rs:728-797`](https://github.com/tontinton/maki/blob/e0d87e35a45a313b8c3e03dca21ae5aef3d35050/maki-ui/src/components/messages/mod.rs#L728-L797)

### 10.2 Multiplexed event loop

A `flume::Selector` waits over terminal input, Lua UI actions, every session's agent envelopes, shell output, warnings, and timing cadence. Crossterm runs on a dedicated native input thread so blocking terminal reads do not block the multiplexer. Key repeats are filtered, and scroll/drag bursts are coalesced. [`maki-ui/src/event_loop.rs:385-393`](https://github.com/tontinton/maki/blob/e0d87e35a45a313b8c3e03dca21ae5aef3d35050/maki-ui/src/event_loop.rs#L385-L393) [`maki-ui/src/event_loop.rs:663-721`](https://github.com/tontinton/maki/blob/e0d87e35a45a313b8c3e03dca21ae5aef3d35050/maki-ui/src/event_loop.rs#L663-L721) [`maki-ui/src/input.rs:30-115`](https://github.com/tontinton/maki/blob/e0d87e35a45a313b8c3e03dca21ae5aef3d35050/maki-ui/src/input.rs#L30-L115)

The loop drains at most 256 immediately available wakes before drawing, preventing a busy producer from indefinitely postponing a frame. Events for background sessions are still applied, but only the focused `App` runs its complete tick and view. [`maki-ui/src/event_loop.rs:710-760`](https://github.com/tontinton/maki/blob/e0d87e35a45a313b8c3e03dca21ae5aef3d35050/maki-ui/src/event_loop.rs#L710-L760)

### 10.3 Demand-driven rendering

Maki does not continuously redraw at 60 FPS. Its repaint model distinguishes:

- `Dirty`: a frame is owed;
- `Watch<T>`: an immutable published snapshot changed identity;
- `Cadence`: when to wake next and whether elapsed time itself changes pixels.

[`maki-ui/src/repaint.rs:1-27`](https://github.com/tontinton/maki/blob/e0d87e35a45a313b8c3e03dca21ae5aef3d35050/maki-ui/src/repaint.rs#L1-L27) [`maki-ui/src/repaint.rs:57-209`](https://github.com/tontinton/maki/blob/e0d87e35a45a313b8c3e03dca21ae5aef3d35050/maki-ui/src/repaint.rs#L57-L209)

Smooth typewriter/fade work requests 16 ms cadence, spinners use 80 ms, pending worker results can be polled without automatically repainting, and fully idle state falls back to a 100 ms bound unless a channel wakes sooner. A waiting tool therefore animates much less often than moving text. [`maki-ui/src/repaint.rs:37-40`](https://github.com/tontinton/maki/blob/e0d87e35a45a313b8c3e03dca21ae5aef3d35050/maki-ui/src/repaint.rs#L37-L40) [`maki-ui/src/event_loop.rs:596-653`](https://github.com/tontinton/maki/blob/e0d87e35a45a313b8c3e03dca21ae5aef3d35050/maki-ui/src/event_loop.rs#L596-L653) [`maki-ui/src/components/messages/mod.rs:706-719`](https://github.com/tontinton/maki/blob/e0d87e35a45a313b8c3e03dca21ae5aef3d35050/maki-ui/src/components/messages/mod.rs#L706-L719)

Transcript rendering uses segment caches, rebuilds only changed tool segments, reflows around the visible anchor rather than the entire history, and renders only rows intersecting the viewport. Syntax-heavy code/tool rendering is sent to worker threads, and monotonically increasing job IDs make stale results harmless. [`maki-ui/src/components/messages/mod.rs:1212-1264`](https://github.com/tontinton/maki/blob/e0d87e35a45a313b8c3e03dca21ae5aef3d35050/maki-ui/src/components/messages/mod.rs#L1212-L1264) [`maki-ui/src/components/messages/mod.rs:1340-1390`](https://github.com/tontinton/maki/blob/e0d87e35a45a313b8c3e03dca21ae5aef3d35050/maki-ui/src/components/messages/mod.rs#L1340-L1390) [`maki-ui/src/render_worker.rs:18-128`](https://github.com/tontinton/maki/blob/e0d87e35a45a313b8c3e03dca21ae5aef3d35050/maki-ui/src/render_worker.rs#L18-L128)

### 10.4 Subagent and plugin UI

Each subagent is displayed through the same `Chat` abstraction as the parent and is lazily keyed by parent tool-use ID. The task picker previews a selected child chat before changing active navigation. Lua windows are Rust-owned render objects that can float, split, or occupy a bottom panel while Rust preserves minimum chat dimensions. [`maki-ui/src/app/mod.rs:1335-1362`](https://github.com/tontinton/maki/blob/e0d87e35a45a313b8c3e03dca21ae5aef3d35050/maki-ui/src/app/mod.rs#L1335-L1362) [`maki-ui/src/app/mod.rs:575-760`](https://github.com/tontinton/maki/blob/e0d87e35a45a313b8c3e03dca21ae5aef3d35050/maki-ui/src/app/mod.rs#L575-L760) [`maki-ui/src/components/lua_float.rs:181-365`](https://github.com/tontinton/maki/blob/e0d87e35a45a313b8c3e03dca21ae5aef3d35050/maki-ui/src/components/lua_float.rs#L181-L365)

Lua owns behaviour and mutable rich buffers; Rust owns terminal access, layout, repaint, input routing, and persisted snapshots. Completed tool buffers retain a bounded warm click cache, and older views can be reconstructed through plugin `restore` hooks. [`maki-ui/src/components/messages/mod.rs:938-957`](https://github.com/tontinton/maki/blob/e0d87e35a45a313b8c3e03dca21ae5aef3d35050/maki-ui/src/components/messages/mod.rs#L938-L957) [`maki-lua/src/runtime.rs:2035-2174`](https://github.com/tontinton/maki/blob/e0d87e35a45a313b8c3e03dca21ae5aef3d35050/maki-lua/src/runtime.rs#L2035-L2174)

## Conventions & notable decisions

### Transferable design properties

These are properties visible in Maki's implementation, not prescriptions for another harness.

1. **The model loop is a library boundary.** Provider streaming, history mutation, tool execution, and event production live outside the host UI. This permits multiple hosts without duplicating agent semantics. [`maki-agent/src/agent/run.rs:199-380`](https://github.com/tontinton/maki/blob/e0d87e35a45a313b8c3e03dca21ae5aef3d35050/maki-agent/src/agent/run.rs#L199-L380) [`maki-agent/src/headless.rs:311-489`](https://github.com/tontinton/maki/blob/e0d87e35a45a313b8c3e03dca21ae5aef3d35050/maki-agent/src/headless.rs#L311-L489)
2. **Generational event filtering replaces fragile async teardown.** Old producers may finish naturally; their `run_id` makes their output inert after cancel/respawn. [`maki-ui/src/app/mod.rs:1119-1156`](https://github.com/tontinton/maki/blob/e0d87e35a45a313b8c3e03dca21ae5aef3d35050/maki-ui/src/app/mod.rs#L1119-L1156)
3. **Human-visible state and model-visible state are different products.** Tool UI, streamed buffers, clicks, and restoration can remain rich while `as_text()` and interpreter truncation define a smaller context representation. [`maki-agent/src/types.rs:300-364`](https://github.com/tontinton/maki/blob/e0d87e35a45a313b8c3e03dca21ae5aef3d35050/maki-agent/src/types.rs#L300-L364)
4. **Context control happens at each boundary.** File structure, MCP definitions, tool outputs, subagent histories, and code-execution intermediates each have their own publication policy instead of relying only on whole-history compaction. [`plugins/index/init.lua:1-120`](https://github.com/tontinton/maki/blob/e0d87e35a45a313b8c3e03dca21ae5aef3d35050/plugins/index/init.lua#L1-L120) [`maki-agent/src/mcp/mod.rs:346-388`](https://github.com/tontinton/maki/blob/e0d87e35a45a313b8c3e03dca21ae5aef3d35050/maki-agent/src/mcp/mod.rs#L346-L388)
5. **Plugin extension points are host-owned seams.** Tools, events, prompt slots, function slots, keymaps, and UI objects are registered through explicit APIs with lifecycle ownership rather than arbitrary mutation of Rust internals. [`maki-lua/src/api/mod.rs:36-95`](https://github.com/tontinton/maki/blob/e0d87e35a45a313b8c3e03dca21ae5aef3d35050/maki-lua/src/api/mod.rs#L36-L95) [`maki-lua/src/runtime.rs:1482-1570`](https://github.com/tontinton/maki/blob/e0d87e35a45a313b8c3e03dca21ae5aef3d35050/maki-lua/src/runtime.rs#L1482-L1570)
6. **Code execution has no ambient machine access.** Python receives explicit async tool capabilities and returns through the same dispatcher, so filesystem, network, and shell effects remain delegated, visible, and permission-controlled rather than direct interpreter operations. [`plugins/code_execution/init.lua:155-301`](https://github.com/tontinton/maki/blob/e0d87e35a45a313b8c3e03dca21ae5aef3d35050/plugins/code_execution/init.lua#L155-L301) [`maki-lua/src/api/agent.rs:298-345`](https://github.com/tontinton/maki/blob/e0d87e35a45a313b8c3e03dca21ae5aef3d35050/maki-lua/src/api/agent.rs#L298-L345)
7. **The TUI pays for motion rather than time.** Dirty state, cadence, immutable watches, viewport caches, and stale-job IDs keep one-threaded rendering responsive without a permanent frame loop. [`maki-ui/src/repaint.rs:1-209`](https://github.com/tontinton/maki/blob/e0d87e35a45a313b8c3e03dca21ae5aef3d35050/maki-ui/src/repaint.rs#L1-L209)

### Caveats relevant to reuse

- The conversation model is simpler than the UI language can imply: rewind and fork do not create a persisted branch graph. [`maki-storage/src/sessions.rs:195-208`](https://github.com/tontinton/maki/blob/e0d87e35a45a313b8c3e03dca21ae5aef3d35050/maki-storage/src/sessions.rs#L195-L208)
- The tool system has parallel resolution paths for registry, local, and MCP tools. Session-local handlers bypass the generic registry parse/permission path, and MCP schemas are not runtime-validated by Maki. [`maki-agent/src/agent/tool_dispatch.rs:72-93`](https://github.com/tontinton/maki/blob/e0d87e35a45a313b8c3e03dca21ae5aef3d35050/maki-agent/src/agent/tool_dispatch.rs#L72-L93) [`maki-agent/src/agent/tool_dispatch.rs:260-379`](https://github.com/tontinton/maki/blob/e0d87e35a45a313b8c3e03dca21ae5aef3d35050/maki-agent/src/agent/tool_dispatch.rs#L260-L379)
- A Lua tool participates in agent permissions only if it declares permission scopes. Plugin capability permissions and agent action permissions are distinct systems. [`maki-lua/src/api/tool.rs:220-245`](https://github.com/tontinton/maki/blob/e0d87e35a45a313b8c3e03dca21ae5aef3d35050/maki-lua/src/api/tool.rs#L220-L245) [`maki-lua/src/plugin_permissions.rs:10-35`](https://github.com/tontinton/maki/blob/e0d87e35a45a313b8c3e03dca21ae5aef3d35050/maki-lua/src/plugin_permissions.rs#L10-L35)
- A valid but empty `plugin.toml` grants all listed plugin capabilities because fields default to true; no manifest denies all. [`maki-lua/src/plugin_permissions.rs:65-159`](https://github.com/tontinton/maki/blob/e0d87e35a45a313b8c3e03dca21ae5aef3d35050/maki-lua/src/plugin_permissions.rs#L65-L159)
- Monty and Luau execute in-process. Their resource/capability controls reduce accidental and model-generated harm, but neither is equivalent to an OS process/container boundary for hostile code. [`maki-interpreter/src/alloc.rs:1-27`](https://github.com/tontinton/maki/blob/e0d87e35a45a313b8c3e03dca21ae5aef3d35050/maki-interpreter/src/alloc.rs#L1-L27) [`maki-lua/src/runtime.rs:1369-1419`](https://github.com/tontinton/maki/blob/e0d87e35a45a313b8c3e03dca21ae5aef3d35050/maki-lua/src/runtime.rs#L1369-L1419)
- Some TUI channels are unbounded. The 256-wake budget protects frame fairness but does not apply producer backpressure. [`maki-ui/src/event_loop.rs:350-359`](https://github.com/tontinton/maki/blob/e0d87e35a45a313b8c3e03dca21ae5aef3d35050/maki-ui/src/event_loop.rs#L350-L359) [`maki-ui/src/event_loop.rs:426-431`](https://github.com/tontinton/maki/blob/e0d87e35a45a313b8c3e03dca21ae5aef3d35050/maki-ui/src/event_loop.rs#L426-L431) [`maki-ui/src/event_loop.rs:748-760`](https://github.com/tontinton/maki/blob/e0d87e35a45a313b8c3e03dca21ae5aef3d35050/maki-ui/src/event_loop.rs#L748-L760)
- Context-size estimates after local tool-result growth omit system/tool-schema cost and use a four-bytes-per-token approximation until the next provider measurement. [`maki-agent/src/agent/run.rs:568-593`](https://github.com/tontinton/maki/blob/e0d87e35a45a313b8c3e03dca21ae5aef3d35050/maki-agent/src/agent/run.rs#L568-L593)

## Key references

- [`maki-agent/src/agent/run.rs:199-561`](https://github.com/tontinton/maki/blob/e0d87e35a45a313b8c3e03dca21ae5aef3d35050/maki-agent/src/agent/run.rs#L199-L561) — core model/tool/compaction/interrupt loop.
- [`maki-agent/src/agent/tool_dispatch.rs:72-468`](https://github.com/tontinton/maki/blob/e0d87e35a45a313b8c3e03dca21ae5aef3d35050/maki-agent/src/agent/tool_dispatch.rs#L72-L468) — all practical tool dispatch paths and parallel result collection.
- [`maki-agent/src/types.rs:300-629`](https://github.com/tontinton/maki/blob/e0d87e35a45a313b8c3e03dca21ae5aef3d35050/maki-agent/src/types.rs#L300-L629) — separation of model output, UI output, and event vocabulary.
- [`plugins/code_execution/init.lua`](https://github.com/tontinton/maki/blob/e0d87e35a45a313b8c3e03dca21ae5aef3d35050/plugins/code_execution/init.lua) — interpreter policy, Python interface, tool filtering, rich output, and truncation.
- [`maki-interpreter/src/runner.rs`](https://github.com/tontinton/maki/blob/e0d87e35a45a313b8c3e03dca21ae5aef3d35050/maki-interpreter/src/runner.rs) — Monty execution and resource/host-call bridge.
- [`maki-interpreter/src/alloc.rs:1-27`](https://github.com/tontinton/maki/blob/e0d87e35a45a313b8c3e03dca21ae5aef3d35050/maki-interpreter/src/alloc.rs#L1-L27) — explicit memory-boundary limitations.
- [`maki-lua/src/runtime.rs`](https://github.com/tontinton/maki/blob/e0d87e35a45a313b8c3e03dca21ae5aef3d35050/maki-lua/src/runtime.rs) — Lua VM, loading, lifecycle, async work, restore, and host request loop.
- [`maki-lua/src/api/tool.rs:1243-1490`](https://github.com/tontinton/maki/blob/e0d87e35a45a313b8c3e03dca21ae5aef3d35050/maki-lua/src/api/tool.rs#L1243-L1490) — Lua tool registration and rich result contract.
- [`maki-lua/src/api/autocmd.rs:66-230`](https://github.com/tontinton/maki/blob/e0d87e35a45a313b8c3e03dca21ae5aef3d35050/maki-lua/src/api/autocmd.rs#L66-L230) — plugin event structure.
- [`maki-storage/src/sessions.rs:195-208`](https://github.com/tontinton/maki/blob/e0d87e35a45a313b8c3e03dca21ae5aef3d35050/maki-storage/src/sessions.rs#L195-L208) — persisted conversation shape.
- [`maki-agent/src/agent/compaction.rs`](https://github.com/tontinton/maki/blob/e0d87e35a45a313b8c3e03dca21ae5aef3d35050/maki-agent/src/agent/compaction.rs) — context compaction mechanics.
- [`maki-ui/src/event_loop.rs`](https://github.com/tontinton/maki/blob/e0d87e35a45a313b8c3e03dca21ae5aef3d35050/maki-ui/src/event_loop.rs) — multiplexing, fairness, persistence cadence, and action effects.
- [`maki-ui/src/repaint.rs`](https://github.com/tontinton/maki/blob/e0d87e35a45a313b8c3e03dca21ae5aef3d35050/maki-ui/src/repaint.rs) — demand-driven rendering model.
- [`maki-ui/src/components/messages/mod.rs`](https://github.com/tontinton/maki/blob/e0d87e35a45a313b8c3e03dca21ae5aef3d35050/maki-ui/src/components/messages/mod.rs) — transcript caching, live buffers, and viewport rendering.
