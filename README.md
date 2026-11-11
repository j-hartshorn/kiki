# Kiki

**A terminal coding agent with a persistent Python workspace.**

Kiki reads and edits files, runs commands, and helps you work on a codebase from your terminal. Its agent works through Python, keeping variables and tool results between steps instead of repeatedly copying everything into the conversation.

Built in Rust, with support for **OpenAI, Anthropic, and Google Gemini**.

[Quick start](#quick-start) · [User guide](docs/usage.md) · [Development](#development)

## Highlights

- **Persistent working state.** The agent can retain data, filter large results, and compose tools with ordinary Python.
- **Resumable, branching sessions.** Pick up where you left off, revisit an earlier point, or explore a different approach without losing the previous branch.
- **An inspectable terminal UI.** Follow streaming responses, thinking, Python executions, and tool results; expand the detail when you need it.
- **Your choice of model.** Switch providers, models, and thinking levels from inside a session.
- **Skills and subagents.** Add reusable instructions and delegate bounded tasks to agents with their own Python state.
- **Long-running work.** Track tasks with TODO lists and compact conversation context when needed.

## Quick start

### 1. Install from source

You need Git and [Rust installed through rustup](https://rustup.rs/). The repository pins its Rust toolchain.

```sh
git clone https://github.com/j-hartshorn/kiki.git
cd kiki
cargo install --path . --locked
```

Make sure `~/.cargo/bin` is on your `PATH`. The Monty Python runtime is embedded in the binary; no separate Monty installation is needed.

### 2. Connect a provider

You only need an API key for **one** provider. For example, to use OpenAI:

```sh
kiki login openai
```

For Anthropic or Google Gemini, use `kiki login anthropic` or `kiki login google` instead. Login prompts for your key without echoing it and stores it in `~/.kiki/auth.toml` with owner-only file permissions.

You can also supply `OPENAI_API_KEY`, `ANTHROPIC_API_KEY`, or `GEMINI_API_KEY` through the environment. See the [setup reference](docs/usage.md#setup) for configuration details.

### 3. Start in your project

```sh
cd /path/to/your/project
kiki
```

Ask for a change, an explanation, or help investigating a problem. Kiki selects a default model from the available credentials; use `/model` to choose another.

## Everyday use

| Command | What it does |
| --- | --- |
| `/model` | Choose a model. |
| `/thinking` | Choose the model's thinking level. |
| `/settings` | Open settings, including theme and TODO display. |
| `/tree` | Browse the session history and return to an earlier point. |
| `/compact` | Summarize the current context to make room for more work. |
| `/todos` | View the agent's task list. |
| `/name <name>` | Give the current session a name. |

Type `/` for command suggestions. `Ctrl-P` opens the model picker, `Ctrl-T` toggles thinking, and `Ctrl-O` expands or collapses execution detail. `Esc` requests cancellation; `Ctrl-C` clears a draft, or exits when the prompt is empty.

Sessions are saved locally. From the same project directory:

```sh
kiki sessions       # List saved sessions
kiki --continue     # Resume the most recent session
```

To open a specific session, use `kiki --resume SESSION_ID` with an ID from `kiki sessions`.

See the [full controls](docs/usage.md#controls-and-availability) and [session guide](docs/usage.md#sessions) for more.

## Documentation

- [User guide](docs/usage.md) — setup, configuration, models, and detailed controls.
- [Skills](docs/usage.md#agent-skills) — reusable instructions from your project or user directory.
- [Subagents](docs/usage.md#subagents) — delegation and parallel work.
- [Session trees](docs/usage.md#session-tree-navigation) — navigating and branching history.
- [Context compaction](docs/usage.md#context-compaction) — keeping longer sessions manageable.
- [Project vision](docs/VISION.md) — the ideas behind Kiki's design.

## Safety and privacy

Kiki is under active development. Use version control and review its changes.

- **Commands run with your permissions.** The Python arena has no ambient host access, but its tools can read and write files and run shell commands. Kiki is not an OS sandbox or a command-approval boundary.
- **Rewinding does not undo side effects.** Restoring a session branch does not reverse file changes, commands, or network requests.
- **Provider requests can include project data.** Prompts and workspace content surfaced by the agent may be sent to the selected API provider.
- **Sessions are local, but unencrypted.** They can contain prompts, outputs, Python state, and sensitive project data.
- **API usage costs money.** Displayed costs are estimates, not a provider bill.

Read the [privacy and cost details](docs/usage.md#privacy-and-costs) before using Kiki with sensitive projects.

## Development

From a source checkout, the optional [`just`](https://github.com/casey/just) recipes cover common tasks:

```sh
just run       # Build and launch; loads the local .env file if present
just check     # Type-check all targets
just test      # Build the worker and run the test suite
just lint      # Run Clippy with warnings treated as errors
```

The ordinary tests use local fixtures and do not need provider credentials. Live API tests are separate and opt-in. See [verification](docs/usage.md#deterministic-verification) and [live smoke tests](docs/usage.md#opt-in-live-smoke-test) for the full commands.

Development conventions are in [AGENTS.md](AGENTS.md) and the [worktree guide](worktrees/README.md).
