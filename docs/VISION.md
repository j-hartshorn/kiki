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

The arena lives for the running session initially. When persistence is added, session history should be an append-only tree from its first durable format. Each entry has a parent, and a durable active leaf selects the branch projected into model context and arena state. Rewinding to an earlier restorable point and continuing creates a new branch without deleting the previous future.

Each arena checkpoint records the exact Monty runtime identity required to restore it. When an older session is opened after an upgrade, Kiki must select a compatible worker or explicitly migrate the checkpoint through one. If neither is available, Kiki preserves the session records but marks that arena state non-restorable rather than attempting an incompatible load.

Rewind restores agent and arena state only. It does not roll back filesystem changes or other external effects performed along either branch.

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

A **tool** and a **plugin** are different concepts. A tool is a typed async function exposed to model-authored code in an arena. A plugin is trusted host code loaded by Kiki which can extend selected harness behaviour. A plugin may provide tools, but a tool may instead be built into Rust or backed by MCP or another adapter.

The model uses tools through normal Python control flow, filtering, loops, and `asyncio.gather()` rather than harness-specific batching syntax. Tool arguments and results cross the runtime boundary as transferable values. Arbitrary Python functions and closures are not transferable results initially. Strings remain available when an agent needs to represent source code.

Trusted user-authored Python plugins run outside the model-authored arena and may use normal Python packages. Loading one grants it the user's host permissions; it is not sandboxed model code. Rust controls the interfaces through which Kiki and model-authored code invoke the plugin, but cannot contain arbitrary effects performed by the plugin itself.

The initial plugin interface should stay narrower than Pi's extension surface. Plugins may register tools and user commands, contribute resources, observe typed lifecycle events, participate in explicit transform or policy seams, append typed session records, and produce structured display data. They should not receive mutable session or arena objects, replace the session runtime, or construct Ratatui components directly.

Plugins may also contribute deterministic sandbox bootstrap source and initial bindings to an arena. This supports domain objects whose source of truth belongs in persistent arena state while still projecting outside it. A TODO plugin, for example, can define a `TodoList`, initialise a `todos` binding, and export a transferable task-list projection which Rust renders. The object follows arena checkpoints and session branches without requiring a second plugin-owned copy of its state. Ordinary importable plugin-defined modules require a future Monty module-registration facility and are not part of the initial interface.

Plugin records are durable session entries with two independent optional projections: one into future model context and one into human-facing display. A record with neither projection is durable plugin state; a display-only record is still durable but omitted from model context.

Every durable post-turn record producer receives the committed final message and runs in deterministic registration order. Rust serially appends its returned records to the originating branch. The final answer may be displayed immediately, but no later run begins or takes a model-context snapshot until every producer has finished and the run has settled. A failed producer appends an inspectable plugin error record without changing or removing the message the model actually produced. Display anchoring can place a summary beneath that message independently of the record's parent link in the session tree.

If a plugin needs another model call, it should request auxiliary model work through a narrow Rust-owned interface so provider credentials, cancellation, usage, and recording remain host concerns.

## Tools

Model-facing capabilities have an explicit exposure policy. **Pinned** capabilities receive concise descriptions in the initial context, **discoverable** capabilities expose only enough metadata to find and load them on demand, and **hidden** capabilities are not advertised to the model. Plugins can suggest an exposure level, but user configuration has final control.

Core tools are pinned. A discovery API lets the agent progressively search the wider registry and inspect tool signatures and docstrings only when needed.

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

## Prompt resources

All substantial static instructions sent to models should be Markdown resources rather than prose embedded in Rust source. This includes the base system prompt, the `code`/`message` protocol, arena guidance, future compaction instructions, subagent instructions, and prompts for auxiliary model work. Rust still assembles dynamic facts such as tool schemas, applicable project instructions, session context, and current resource metadata.

Each prompt has a stable logical identity and a shipped default. Users can replace a prompt with their own Markdown file without rebuilding Kiki. The resource loader resolves overrides in one place, and sessions record the identity and provenance of the prompts they used so behaviour remains inspectable. Plugins contribute model instructions through the same prompt-resource mechanism rather than appending opaque strings from arbitrary hooks.

## Interface

Kiki follows an MVC-like split. The session model is canonical and independent of any front end: it owns the session tree, active leaf, committed records, model-context projection, and references to restorable arena state. The runtime acts as controller, accepting commands and coordinating providers, arenas, tools, plugins, and persistence. The Ratatui interface is a view over session snapshots and typed runtime events; it does not own agent behaviour.

One thread owns terminal access, ephemeral UI state, and rendering. Focus, scrolling, expansion, and other presentation choices stay in the TUI. Opening a view produces a session snapshot and an ordered event subscription from the same revision, so no state transition can be lost between rebuilding the view and receiving later events.

Events describe ordered facts and live progress; they are not the canonical state or a generic mutation mechanism. Commands request work, the runtime updates or appends to the session model, and events report the resulting transition. Plugin observations receive immutable events. Hooks which transform data or make policy decisions use separate typed interfaces with explicit ordering, validation, and failure semantics.

The initial event vocabulary should cover:

- session entries and active-leaf changes;
- run start, final answer, failure, cancellation, and settlement;
- streamed model output;
- arena execution, output, errors, and changed values;
- tool invocation, progress, and completion;
- plugin records and other committed display projections.

Each event carries the stable identifiers needed to associate it with its session, branch, run, model step, execution, or tool invocation. Generation checks prevent late events from obsolete runs changing current state.

Model history and display history are different products. The model receives the compact representation needed for its next decision. The session and interface may retain richer code, tool output, status, timing, plugin records, and inspection data without feeding those details back into context.

The final conversation remains prominent. Code executions and tool calls appear as a collapsed timeline that the user can expand. Cancellation must make the interface responsive immediately and prevent late events from an obsolete run changing current state.

Rendering should be driven by visible changes rather than a permanent frame loop. Transcript caches, background rendering, rich interactive tool output, and other optimisations should be added only when the simpler interface demonstrates a need for them.

A later arena inspector should work like a debugger: variable names and types listed in a keyboard- and mouse-navigable panel, with values available for deeper inspection. It is a projection of arena state, not a requirement for the first working agent.

Other structured state follows the same projection pattern. A plugin-provided TODO object remains in the arena, while its registered transferable projection becomes a generic task-list view in the TUI. The projection is regenerated from the selected arena state after committed execution and after restoration, branch selection, or arena replacement; a host-side cache never becomes a second source of truth. Concise pinned guidance can make an existing TODO binding the obvious interface for the model without making TODO management part of the core agent protocol or allowing the plugin to construct UI components. Names such as `TodoList` and `todos` remain illustrative.

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

- persisted and resumable tree-shaped sessions;
- branch navigation and arena restoration without pretending external effects were undone;
- parallel subagent workflows and richer result inspection;
- a debugger-like arena variable inspector;
- richer structured projections and interaction with recognised arena objects;
- tool search and richer progressive discovery;
- MCP-to-Python tool translation;
- memory and web capabilities;
- broader provider support;
- safe ways to share useful Python tools and skills.

These are directions, not commitments to build them before the core loop works.
