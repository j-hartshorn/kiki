# Kiki Vision

Kiki is a personal, programmable coding harness built around a persistent Python workspace. It should let an agent accumulate useful state, compose tools as ordinary code, and work on a real project without repeatedly pushing raw tool output through the model context.

Kiki is being built for one person's workflow first. If that workflow proves useful, the project should be releasable without first undoing assumptions that make it needlessly personal or closed.

The first practical milestone is self-hosting: Kiki should become capable enough to support its own continued development. Self-hosting proves the core loop without requiring the complete product vision first.

This is a direction-setting document, not an implementation specification or roadmap.

## The problem

Most coding harnesses expose isolated tool calls. Results flow back into the conversation, useful details are repeatedly copied or summarised, and the context gradually fills with stale intermediate work. Parallelism, filtering, and chaining require either more model turns or harness-specific orchestration tools.

Kiki should give the agent a persistent computational workspace instead. The agent can keep large or structured values outside the conversation, manipulate them with Python, and bring only the useful result back into context. Python is both working memory and the language for composing tools.

## Core model

An agent can respond in only two modes:

- **Code:** Python that Kiki immediately executes in the agent's Monty arena.
- **Message:** a user-facing response.

One user turn may contain repeated code executions and execution results before the agent produces a final message. Code can inspect and mutate existing variables, call tools, recover from errors, and retain useful results for later turns.

After an execution, the model receives explicit output, errors, and a compact summary of changed variable names and types. Values do not enter the model context merely because they changed. Large values stay in the arena until the agent deliberately inspects, filters, or summarises them.

The arena lives for the running session initially. The architecture must leave a route to saving and resuming sessions and restoring earlier agent state. Restoring agent state does not need to roll back filesystem changes or other external effects.

## Runtime boundary

### Rust is the trusted host

Rust owns:

- the LLM interface and provider adapters;
- the agent loop and orchestration;
- session and arena lifecycle;
- the Ratatui interface;
- tool registration, discovery, and dispatch;
- access to the filesystem, processes, network, and other external effects;
- the boundary between parent agents and subagents.

The LLM interface should be provider-neutral. OpenAI is the first provider, Google is next, and Anthropic can follow. Existing Rust implementations should be used where they provide a sound dependency without dictating Kiki's internal interface.

### Monty is the agent arena

Model-authored Python runs inside a Monty sandbox. It has no ambient access to the host filesystem, processes, network, environment, or credentials. External effects happen through functions exposed by the Rust runtime.

Each agent owns an arena. Variables survive across code executions and user turns for the life of the session.

### Python is the extension language

Tools appear inside the arena as typed async Python functions. The model uses normal Python control flow, filtering, loops, and `asyncio.gather()` rather than harness-specific batching syntax.

Rust-backed tools perform core external effects. Trusted user-authored Python modules can also expose functions as tools. These modules run outside the model-authored arena and may use normal Python packages. Loading one grants it the user's host permissions; it is trusted plugin code, not sandboxed model code. Rust controls whether the model may invoke it, but cannot contain arbitrary effects performed inside it.

Tool arguments and results cross the runtime boundary as transferable values. Arbitrary Python functions and closures are not transferable results initially. Strings remain available when an agent needs to represent source code.

## Tools

Core tools are described in the model's initial context. A discovery API lets the agent progressively search the wider registry and inspect tool signatures and docstrings only when needed.

The initial coding toolset is:

- `read`: read a bounded region of a file;
- `write`: create or replace a file;
- `edit`: apply precise, atomic changes to a file;
- `list`: list directory entries in a stable, machine-oriented form;
- `glob`: find paths matching a pattern, backed by ripgrep's traversal machinery;
- `grep`: search file contents using ripgrep;
- `run`: execute project commands such as builds, tests, and Git operations;
- `subagent`: delegate work to another complete agent;
- `load_skill`: load an Agent Skill's instructions on demand.

The exact names and signatures are implementation decisions. The important boundary is that these capabilities are async Python functions returning useful values, not text protocols the model must parse unnecessarily.

Kiki does not need separate batch or code-execution tools. Code execution is the agent's native action mode, and `asyncio.gather()` expresses parallel work. Rust schedules concurrent external calls and resumes Monty with results in input order.

MCP tools may later be translated into the same Python-function interface. The model should not need to care whether a function is backed by Rust, trusted Python, MCP, or another runtime.

## Subagents

A subagent is a complete agent with its own model loop and code arena. It starts with an inherited copy of the parent's transferable data, ready to use without reconstructing context manually. Subagents can be launched concurrently through ordinary Python and `asyncio.gather()`.

A subagent returns:

- a message explaining its findings and returned data; and
- explicitly named, transferable values whose names and types the parent can inspect.

There is no automatic state merge. Child functions, closures, and full arena snapshots are not returned initially. External effects performed through tools are not implied to be isolated or reversible.

## Project instructions and skills

Kiki should support the open `AGENTS.md` and Agent Skills conventions from the outset.

### `AGENTS.md`

Kiki loads applicable instructions from the repository root towards the working directory. Nested `AGENTS.md` files scope instructions to their subtree, with the nearest applicable instruction taking precedence. Deeper files should be discovered progressively when the agent first works in that part of the repository rather than loading every monorepo instruction upfront.

### Agent Skills

Kiki discovers skills from:

- `.agents/skills/` in the project; and
- `~/.agents/skills/` for the user.

Only each skill's name and description enter the initial context. Full `SKILL.md` instructions load on demand. Skills may contain scripts, references, and assets which the agent can access through its normal tools. Installation, registries, compatibility paths for other harnesses, and the experimental `allowed-tools` field are not initial requirements.

## Interface

The Ratatui interface is a projection of agent state and events, not the owner of agent behaviour. The agent loop must remain usable independently of the TUI.

One thread owns terminal access, mutable UI state, and rendering. Providers, agents, code execution, and tools run separately and communicate through typed events. The initial event vocabulary should cover message and code output, execution lifecycle, tool lifecycle, arena changes, errors, cancellation, and completion without exposing provider-specific wire formats.

Model history and display history are different products. The model receives the compact representation needed for its next decision. The interface may retain richer code, tool output, status, timing, and inspection data without feeding those details back into context.

The final conversation remains prominent. Code executions and tool calls appear as a collapsed timeline that the user can expand. Cancellation must make the interface responsive immediately and prevent late events from an obsolete run changing current state.

Rendering should be driven by visible changes rather than a permanent frame loop. Transcript caches, background rendering, rich interactive tool output, and other optimisations should be added only when the simpler interface demonstrates a need for them.

A later arena inspector should work like a debugger: variable names and types listed in a keyboard- and mouse-navigable panel, with values available for deeper inspection. It is a projection of arena state, not a requirement for the first working agent.

Other structured state can follow the same pattern. A TODO list can begin as a Python object with methods that mutate it; Kiki can later recognise and render that object in the TUI without making TODO management part of the core agent protocol.

## First complete loop

The first complete Kiki should let one agent inspect, modify, and verify a real project:

1. The user enters a request in the TUI.
2. Rust calls an OpenAI model through the provider-neutral LLM interface.
3. The model emits Python code or a message.
4. Rust executes code in the persistent Monty arena.
5. Python calls async Rust-backed tools to inspect files, author code, and run checks.
6. Rust returns explicit output, errors, and a compact state-change summary.
7. The cycle repeats until the model sends its final message.

This loop includes core project instructions and progressive skill loading. It does not depend on persisted sessions, a variable inspector, or the full subagent design being complete.

Kiki reaches its first practical milestone when this loop is reliable enough to inspect, modify, and verify the Kiki repository itself. Self-hosting does not imply feature completeness. It requires one dependable agent, its persistent arena, the core coding tools, a minimal inspectable TUI, and cancellation and error paths that do not require restarting the harness.

## Design principles

### Persistent state over repeated context

Keep working data in the arena. Put only decisions, summaries, and information needed for the next model decision into context.

### Python-native composition

Functions, values, `await`, and `asyncio.gather()` are the orchestration language. Avoid inventing parallel harness-specific equivalents.

### Controlled effects

Model-authored code is sandboxed. Effects cross an explicit Rust-controlled boundary. Trusted plugins remain visibly outside that security boundary.

### Progressive disclosure

Load core capabilities and essential project instructions upfront. Discover extended tools, nested instructions, skills, and large values only when needed.

### Inspectability without noise

Record what the agent did and make it easy to inspect, but do not force every intermediate detail into the main conversation.

### Personal first, releasable later

Optimise for the author's real workflow. Prefer standards and clear seams where they cost little, but do not generalise ahead of demonstrated need.

### A small core

Keep built-in concepts few. Features such as memory, web access, TODO presentation, richer skills, and specialised workflows should begin in Python when possible and become built-ins only when the host genuinely needs to understand them.

## Later directions

- persisted and resumable sessions;
- restoring earlier agent state without pretending external effects were undone;
- parallel subagent workflows and richer result inspection;
- a debugger-like arena variable inspector;
- TUI projections for TODOs and other recognised Python objects;
- tool search and richer progressive discovery;
- MCP-to-Python tool translation;
- memory and web capabilities;
- broader provider support;
- safe ways to share useful Python tools and skills.

These are directions, not commitments to build them before the core loop works.
