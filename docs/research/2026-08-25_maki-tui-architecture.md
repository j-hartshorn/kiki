# Research: Maki TUI architecture

## Scope

This document examines the Ratatui architecture in Maki at commit `e0d87e35a45a313b8c3e03dca21ae5aef3d35050`. It covers terminal ownership, input, event flow, agent/UI separation, display state, cancellation, and repaint behaviour. It does not cover Maki's provider, tool, storage, or Lua systems except where they cross the UI boundary.

The broader harness is documented in [the existing Maki architecture research](../../.research/maki/maki-architecture.md).

## Summary

- One thread owns the terminal, mutable UI state, and all Ratatui rendering. Agent work runs separately and communicates through channels. `.research/maki/source/maki-ui/src/lib.rs:1-4` `.research/maki/source/maki-ui/src/event_loop.rs:370-383`
- `App` receives a small message vocabulary, mutates its state, and returns actions. The event-loop supervisor performs those effects. `.research/maki/source/maki-ui/src/app/mod.rs:205-269` `.research/maki/source/maki-ui/src/app/mod.rs:474-509` `.research/maki/source/maki-ui/src/event_loop.rs:1254-1393`
- Terminal input has its own blocking thread. The UI loop can therefore wait for terminal input, agent events, shell output, and plugin events together. `.research/maki/source/maki-ui/src/input.rs:1-45` `.research/maki/source/maki-ui/src/event_loop.rs:660-683`
- The agent exposes typed lifecycle events rather than mutating UI state. `Chat` projects these events into human-facing display state. `.research/maki/source/maki-agent/src/types.rs:548-629` `.research/maki/source/maki-ui/src/chat.rs:79-184`
- Provider-facing history and human-facing transcript state are separate representations. Tool status, rich output, collapse state, and rendering metadata do not need to enter model history. `.research/maki/source/maki-ui/src/components/mod.rs:283-374` `.research/maki/source/maki-ui/src/chat.rs:404-552`
- Cooperative cancellation is paired with a monotonically increasing `run_id`. Late events from cancelled or replaced work are ignored at the UI boundary. `.research/maki/source/maki-ui/src/app/mod.rs:1077-1097` `.research/maki/source/maki-ui/src/app/mod.rs:1119-1157`
- Rendering is demand-driven. The loop draws after events or visible animation ticks, rather than continuously. `.research/maki/source/maki-ui/src/event_loop.rs:596-653` `.research/maki/source/maki-ui/src/repaint.rs:1-209`
- Transcript segment caches, viewport-only reflow, worker highlighting, rich buffers, and multi-session supervision are later optimisations layered on this core. `.research/maki/source/maki-ui/src/components/messages/mod.rs:1212-1390` `.research/maki/source/maki-ui/src/render_worker.rs:1-128`

## Findings

### UI and agent ownership are separate

`maki-ui::run` creates a terminal guard and then starts `EventLoop`. The guard remains alive for the complete UI generation and restores the terminal on drop. `.research/maki/source/maki-ui/src/lib.rs:60-66` `.research/maki/source/maki-ui/src/terminal.rs:251-270`

`EventLoop` owns the terminal and session runtimes. Each runtime holds one `App` plus handles and channels for its agent and shell work. Agent work is spawned separately, so no asynchronous task directly mutates Ratatui state. `.research/maki/source/maki-ui/src/event_loop.rs:262-290` `.research/maki/source/maki-ui/src/event_loop.rs:370-383` `.research/maki/source/maki-ui/src/agent/mod.rs:215-273`

### The update/action split contains effects

The UI message vocabulary is limited to key, paste, mouse, scroll, and agent events. `App::update` routes each message and returns zero or more `Action` values. `.research/maki/source/maki-ui/src/app/mod.rs:205-211` `.research/maki/source/maki-ui/src/app/mod.rs:474-509`

Actions describe external effects such as sending or cancelling agent work, running a shell command, switching sessions, opening an editor, or exiting. The event loop dispatches them after `App` has made the UI decision. `.research/maki/source/maki-ui/src/components/mod.rs:186-236` `.research/maki/source/maki-ui/src/event_loop.rs:1254-1393`

This is Elm-inspired rather than a pure reducer. `App` mutates in place and some answer channels are used directly, but terminal access and long-running effects remain outside normal UI state transitions. `.research/maki/source/maki-ui/src/app/mod.rs:511-524`

### Input and asynchronous events share one wake loop

Crossterm's blocking input functions run on a dedicated thread and forward events over Flume. A pause guard lets Maki stop that reader before handing the TTY to an external editor or suspended shell. `.research/maki/source/maki-ui/src/input.rs:27-70` `.research/maki/source/maki-ui/src/input.rs:72-115`

The UI loop uses `flume::Selector` to wait across terminal input and asynchronous producers. Every selected wake is handled on the UI thread, then any resulting actions are dispatched. `.research/maki/source/maki-ui/src/event_loop.rs:660-695` `.research/maki/source/maki-ui/src/event_loop.rs:722-746`

A bounded drain of 256 already-ready wakes allows busy producers to make progress without indefinitely delaying the next frame. `.research/maki/source/maki-ui/src/event_loop.rs:748-759`

### Typed events form the UI boundary

The agent protocol distinguishes text and thinking deltas, tool pending/start/output/done events, turn completion, run completion, errors, permissions, authentication, compaction, and subagent state. `.research/maki/source/maki-agent/src/types.rs:548-629`

`Chat::handle_event` translates that protocol into transcript state and returns the few outcomes requiring app-level handling, such as completion, errors, permission requests, or consumed queued input. `.research/maki/source/maki-ui/src/chat.rs:32-45` `.research/maki/source/maki-ui/src/chat.rs:79-184`

The display model stores data that provider history does not need: tool input and output, live output, annotations, snapshots, timestamps, truncation counts, collapse state, and visual status. `.research/maki/source/maki-ui/src/components/mod.rs:283-374`

### Cancellation is both cooperative and generational

On cancellation, `App` increments `run_id` before emitting the cancel action, flushes current display state, and returns to idle immediately. `.research/maki/source/maki-ui/src/app/mod.rs:1077-1097`

Every agent envelope carries the run that produced it. Envelopes from earlier generations are discarded, which prevents late network or tool events from corrupting a new run even if the old task has not stopped yet. `.research/maki/source/maki-agent/src/types.rs:858-923` `.research/maki/source/maki-ui/src/app/mod.rs:1119-1157`

### Repainting follows visible change

The event loop accumulates a `Dirty` flag, draws only when a frame is owed, and then waits for either a channel wake or the current animation cadence. `.research/maki/source/maki-ui/src/event_loop.rs:596-653`

Maki separates three repaint concerns: `Dirty` records that visible state changed, `Watch` notices shared snapshots published without a channel event, and `Cadence` describes when time itself may alter visible pixels. `.research/maki/source/maki-ui/src/repaint.rs:1-27` `.research/maki/source/maki-ui/src/repaint.rs:57-209`

The transcript adds segment caches, viewport-aware reflow, and background syntax rendering. Those mechanisms address long histories and rich tool output rather than defining the basic event architecture. `.research/maki/source/maki-ui/src/components/messages/mod.rs:1212-1390` `.research/maki/source/maki-ui/src/render_worker.rs:1-128`

## Conventions & notable decisions

- Terminal state is guarded by RAII and explicitly torn down when another process temporarily owns the TTY. `.research/maki/source/maki-ui/src/terminal.rs:251-309`
- UI mutation happens on one thread. Asynchronous producers publish messages, immutable snapshots, or buffered output. `.research/maki/source/maki-ui/src/event_loop.rs:370-383`
- The UI and model receive different projections of the same run. `.research/maki/source/maki-ui/src/components/mod.rs:283-374`
- Generation checks make late asynchronous delivery harmless rather than requiring perfect task teardown. `.research/maki/source/maki-ui/src/app/mod.rs:1119-1157`
- Rendering work is introduced in layers: direct event-driven redraw first, then cadence, shared-value watches, segment caches, and workers. `.research/maki/source/maki-ui/src/repaint.rs:1-209`

## Key references

- `.research/maki/source/maki-ui/src/lib.rs:1-66` — top-level UI ownership and terminal lifetime.
- `.research/maki/source/maki-ui/src/app/mod.rs:205-269` — message and app state vocabulary.
- `.research/maki/source/maki-ui/src/app/mod.rs:474-509` — update entry point.
- `.research/maki/source/maki-ui/src/event_loop.rs:596-759` — draw, wait, multiplex, and fairness loop.
- `.research/maki/source/maki-ui/src/event_loop.rs:1254-1393` — action dispatch and effect ownership.
- `.research/maki/source/maki-ui/src/input.rs:1-115` — dedicated terminal input thread.
- `.research/maki/source/maki-ui/src/chat.rs:79-184` — agent event to display-state projection.
- `.research/maki/source/maki-ui/src/components/mod.rs:283-374` — rich display message representation.
- `.research/maki/source/maki-ui/src/repaint.rs:1-209` — demand-driven rendering model.
