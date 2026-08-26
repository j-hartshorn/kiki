# Kiki

Kiki is a programmable coding harness in which agents work through persistent, sandboxed Python state while a trusted host controls models, external effects, sessions, and presentation.

## Agent state

**Agent**:
A model-driven participant with its own arena and model loop.

**Arena**:
An agent's persistent, sandboxed Python workspace. Values survive model steps and user requests until the arena or session ends.
_Avoid_: Workspace, Python runtime

**Session**:
The tree-shaped history of an agent's durable records and restorable arena states.
_Avoid_: Conversation, transcript

**Session Entry**:
One immutable record in a session, linked to exactly one parent except at the root.
_Avoid_: Message

**Active Leaf**:
The durable session entry whose root-to-leaf path defines the currently selected history and restorable agent state.
_Avoid_: Cursor, latest message

**Arena Checkpoint**:
An opaque restorable arena state paired with the exact Monty runtime identity required to load it.
_Avoid_: Snapshot

**Model Context**:
The selected, possibly compacted projection of session state sent to a model for its next response.
_Avoid_: Session, transcript

**Display History**:
The inspectable human-facing projection of a session, including details which do not belong in model context.
_Avoid_: Model context, transcript

**Run**:
The work initiated by one user request through the agent's final message, failure, or cancellation. Required post-processing may continue until settlement.
_Avoid_: Turn

**Model Step**:
One complete model response and, for a `code` response, its resulting arena execution. A run may contain several model steps before a final `message` response.
_Avoid_: Turn

## Extensibility

**Tool**:
A typed async capability exposed to model-authored arena code. A tool may perform an external effect through a Rust, plugin, MCP, or other adapter.
_Avoid_: Plugin, extension

**Plugin**:
Trusted host code which extends selected Kiki behaviour through defined seams. A plugin may provide tools but is not itself model-callable.
_Avoid_: Extension, tool

**Plugin Record**:
A durable typed session entry produced by a plugin with independent optional projections into model context and human-facing display.
_Avoid_: Custom message

**Arena Projection**:
A transferable representation of recognised arena state which the host can inspect or present without moving the source object out of its arena.
_Avoid_: Renderer, widget

**Prompt Resource**:
A named Markdown document containing static model instructions, with a shipped default that a user may replace.
_Avoid_: Hard-coded prompt, prompt string

**Pinned Capability**:
A model-facing capability whose concise description is always included in initial model context.
_Avoid_: Core tool

**Discoverable Capability**:
A model-facing capability whose metadata and full instructions are loaded only when sought or selected.
_Avoid_: Optional tool

**Hidden Capability**:
A host or user-facing capability which is not advertised to the model.

## Presentation and lifecycle

**Session Model**:
The front-end-independent source of truth for the session tree, selected branch, records, context projection, and restorable agent state.
_Avoid_: TUI state

**Runtime Event**:
An ordered fact or progress notification emitted by the runtime after or during work. Events inform projections and observers but are not the canonical session state.
_Avoid_: Hook, command

**Display Projection**:
A human-facing view derived from session or arena state without changing that state.
_Avoid_: Renderer state

**Settlement**:
The point at which a completed run has no retry, recovery, or durable post-turn producer left to perform. A later run cannot begin before settlement.
_Avoid_: Agent end
