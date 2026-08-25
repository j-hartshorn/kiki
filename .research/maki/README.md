# Maki harness research

Research target: [`tontinton/maki`](https://github.com/tontinton/maki) at commit [`e0d87e35`](https://github.com/tontinton/maki/tree/e0d87e35a45a313b8c3e03dca21ae5aef3d35050).

## Documents

- [Architecture deep dive](maki-architecture.md) covers the core loop, tools, Lua plugins, events, conversation history, providers, context control, code execution, and TUI integration.
- [`source/`](source/) is a local checkout pinned to the researched commit.

## Headline findings

1. **The core loop is reusable, not UI-owned.** `Agent::run` receives input and mutable history, talks to one provider interface, dispatches tools, and emits typed events. TUI, SDK, and ACP hosts sit outside it.
2. **The events are two deliberately different layers.** Typed `AgentEvent` envelopes are the lossless host protocol. Lua autocmds are a smaller, higher-level plugin protocol. A `run_id` generation prevents stale async events mutating replacement runs.
3. **Plugins own broad behaviour through narrow host APIs.** A sandboxed Luau runtime can register tools, commands, keymaps, prompt contributions, composable slots, autocmds, UI, and subagents. Registrations are lifecycle-owned and staged before commit.
4. **Provider extensibility reuses transports.** Built-ins implement one `Provider` trait. TOML providers select a supported wire protocol; executable provider scripts inherit a built-in transport while supplying auth, endpoints, catalogues, and login/refresh behaviour.
5. **The tool path preserves context and policy.** Parallel calls return one ordered tool-result message. Permissions, cancellation, audiences, plan mode, repeated-call detection, and panic isolation sit around execution rather than in each tool.
6. **Human-visible and model-visible outputs differ.** Rich live tool UI can remain locally inspectable while a smaller textual representation enters model history.
7. **There is no general conversation tree.** Root history is linear. Rewind truncates it, SDK fork copies it without a parent link, and subagent transcripts are attachments keyed by parent tool-use ID.

## Best ideas to carry into another harness

### Keep the agent kernel headless

Put provider streaming, history mutation, tool dispatch, and event production in a library with no terminal dependency. Hosts become replaceable adapters rather than alternative implementations of agent semantics.

### Use a generational event envelope

Stamp every async event with the run generation and optional child-agent identity. On cancellation or replacement, increment the generation and ignore late events. This is simpler than proving every producer has stopped before state can move on.

### Separate host events from plugin hooks

The host protocol needs detailed streaming and accounting events. Plugin authors usually need stable semantic hooks such as turn start/end, tool start/end, and session changes. Keeping them separate avoids freezing every internal event as public plugin API.

### Make registrations resource-owned

Track everything a plugin registers. Stage registrations while loading, publish only after success, and tear them down by owner. Maki takes the simple route on reload: replace the whole Lua host rather than patching live state.

### Extend providers by composition

Treat transport, model metadata, and credentials as separate concerns:

- a narrow provider streaming interface;
- a model value carrying capabilities, limits, tier, and pricing;
- built-in protocol adapters;
- configuration for compatible endpoints;
- scripts for external auth and catalogue lifecycles.

This gives custom providers substantial flexibility without loading arbitrary provider code into the agent process.

### Control context at every publication boundary

Do not wait for global compaction. Maki narrows file reads, MCP schemas, tool output, subagent history, interpreter output, and UI state independently. The reusable principle is to decide explicitly what becomes model context whenever data crosses a boundary.

## Important caveats

- The “conversation tree” is a linear transcript plus attached child transcripts, not a DAG. A harness needing navigable branches needs a different persistence model.
- Lua and the Monty interpreter are in-process. Their controls are guardrails, not kernel isolation.
- Tool resolution has separate paths for registry, session-local, and MCP tools, so not every path receives identical validation and permission handling.
- Provider scripts are subprocesses, but Lua plugins are a different extension system. Combining both into one universal plugin API would lose the useful distinction between runtime behaviour and model transport/auth setup.
