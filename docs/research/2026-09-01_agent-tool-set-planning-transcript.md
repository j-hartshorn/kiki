# Recovered agent-tool-set planning transcript

Raw Pi session: `/Users/joe/.pi/agent/sessions/--Users-joe-kiki--/2026-09-01T09-09-57-011Z_01a05c3b-7a93-7007-bbd9-cc1137bcd529.jsonl`
Session ID: `01a05c3b-7a93-7007-bbd9-cc1137bcd529`
Compaction event: line 335, ID `9622103a`

This archival extraction preserves all pre-compaction user-visible user and assistant text plus every subagent result. Ordinary tool calls and tool output are omitted. Entries outside the compaction event’s active ancestry are marked as superseded branches; they remain here because they include useful research and reasoning.

## Chronological extraction

### User · source line 4 · `a6eb9366` · active branch

Now I want to work on tool sets that will allow kiki to become an agent. First use an explore agent for maki, pi, and codex in `.research` to understand their default tool sets and how they're implemented, then we'll plan our default tool set from that, and think about how we uniquely need to implement it because of our code arena first agent. We also want to check for the arena/Monty implementation as it standas and make sure we are utilising async functions, and the prompt helps the agent understand to await the functions and gather the results. Multiple tool calls should run concurrently when passed to rust and when handled in rust too.

### Assistant · source line 18 · `ed3a6f4b` · active branch

**Codex is missing from `.research`**

Only `.research/maki` and `.research/pi` exist.

**A.** Clone `openai/codex` into `.research/codex`  
_(recommended)_ Keeps all three references together.

**B.** Use another local path  
Send me its location.

Which should I use?

### User · source line 19 · `11d13a92` · superseded branch

A yes, but please only clone the current state, not all of the history (its a big repo!)

### Subagent result 1 · source line 26 · `b1fe7524` · superseded branch

Agent: `explore`  
Model: `openai/gpt-5.6-luna`  
Task: Research `.research/maki/source` only. Determine Maki's default agent tool set and exactly how each tool is exposed and implemented. Follow the whole path: default selection/configuration, tool declarations/schemas, model-facing names/descriptions, dispatch/execution, result/error representation, prompt instructions about tools, async behaviour, and whether multiple tool calls execute concurrently. Distinguish default tools from optional or mode-specific tools. Read source and relevant tests/docs; do not edit anything. Return a concise but thorough evidence report with repository-relative `path:line` references for every material claim and note uncertainties.

Subagent was aborted

### Subagent result 2 · source line 27 · `c1bd02f5` · superseded branch

Agent: `explore`  
Model: `openai/gpt-5.6-luna`  
Task: Research `.research/pi/source` only. Determine Pi's default agent tool set and exactly how each tool is exposed and implemented. Follow the whole path: default selection/configuration, tool declarations/schemas, model-facing names/descriptions, dispatch/execution, result/error representation, prompt instructions about tools, async behaviour, and whether multiple tool calls execute concurrently. Distinguish default tools from optional/extensions/mode-specific tools. Read source and relevant tests/docs; do not edit anything. Return a concise but thorough evidence report with repository-relative `path:line` references for every material claim and note uncertainties.

Subagent was aborted

### Subagent result 3 · source line 28 · `6c57429d` · superseded branch

Agent: `explore`  
Model: `openai/gpt-5.6-luna`  
Task: Research `.research/codex/source` only. Determine OpenAI Codex's default coding-agent tool set and exactly how each tool is exposed and implemented. Follow the whole path: default selection/configuration, tool declarations/schemas, model-facing names/descriptions, dispatch/execution, result/error representation, prompt instructions about tools, async behaviour, and whether multiple tool calls execute concurrently. Distinguish default tools from optional/config-, platform-, or mode-specific tools. This is a large repo: focus on the current primary CLI/core agent implementation and relevant tests/docs. Do not edit anything. Return a concise but thorough evidence report with repository-relative `path:line` references for every material claim and note uncertainties.

Subagent was aborted

### User · source line 30 · `027c3b97` · active branch

A yes, but please only clone the current state, not all of the history (its a big repo!)

Please use gemini for the explore agents

### Subagent result 4 · source line 37 · `4647795f` · active branch

Agent: `explore`  
Model: `google/gemini-3.7-flash`  
Task: Research Maki under .research/maki/source. Read-only. Determine its default agent tool set and how each tool is defined, registered/exposed to the model, dispatched, and implemented. Trace the full tool-call lifecycle, including prompts/tool descriptions, async semantics, multiple tool calls in one model turn, concurrency or sequential execution, result collection, error handling, cancellation, and any Rust/Python/JS boundary involved. Fan out through callers/callees/tests/docs. Return concise, neutral findings with exact repository-relative file paths and 1-based line numbers for every material claim. Flag ambiguities or behaviour inferred rather than tested. Do not recommend changes to kiki.

## Files Retrieved
1. `maki-config/src/lib.rs` (lines 58–85) — Defines `DEFAULT_BUILTINS` plugin list, `EDIT_SUB_TOOLS`, and `FILE_WRITE_TOOLS`.
2. `maki-lua/src/loader.rs` (lines 27–120) — Embeds bundled Lua plugin directory entries and initialises the Lua host.
3. `maki-agent/src/tools/mod.rs` (lines 45–240, 375–420) — Shared tool infrastructure (`ToolFilter`, `ToolContext`, `Deadline`, builtin detection).
4. `maki-agent/src/tools/registry.rs` (lines 21–67, 166–275, 420–470) — `ToolRegistry`, `Tool` and `ToolInvocation` traits, `ToolAudience`, and model schema generation.
5. `maki-agent/src/tools/schema.rs` (lines 22–95, 150–258, 410–540) — `ParamSchema`, type coercion, input repair via `jsonrepair`, and validation.
6. `maki-agent/src/agent/tool_dispatch.rs` (lines 30–250, 290–460) — Dispatch loop, doom-loop detection, permission enforcement, MCP execution, and parallel turn processing.
7. `maki-agent/src/task_set.rs` (lines 10–55) — `TaskSet` panic-catching concurrent executor for parallel tool tasks.
8. `maki-agent/src/tools/interpreter_bridge.rs` (lines 1–55) — `dispatch`, `flatten`, and `build_tool_input` bridging interpreter/subagent tool calls back into dispatch.
9. `maki-lua/src/api/tool.rs` (lines 150–260, 390–520, 640–740, 1240–1390) — `maki.api.register_tool` Lua binding, `LuaTool`, and `LuaToolInvocation`.
10. `maki-lua/src/runtime.rs` (lines 45–80, 120–230, 2370–2510, 2650–2740) — Dedicated Lua runtime thread, request dispatch loop, watchdog timer, and `run_tool_call`.
11. `maki-lua/src/api/interpreter.rs` (lines 1–210) — `maki.interpreter.run` MLua binding connecting to the monty Python sandbox.
12. `maki-interpreter/src/runner.rs` (lines 1–190) — Monty Python interpreter execution loop with memory/time bounds and callback resolution.
13. `maki-agent/src/tools/file_tracker.rs` (lines 10–70) — `FileReadTracker` tracking mtimes to prevent stale file writes.
14. `maki-agent/src/permissions.rs` (lines 485–535) — `PermissionManager::enforce` handling interactive approval and rule matching.
15. `plugins/bash/init.lua` (lines 260–435) — `bash` tool implementation with AST permission parsing and streaming stdout.
16. `plugins/batch/init.lua` (lines 40–80, 520–600) — `batch` tool executing multiple tool calls concurrently via `maki.async.gather`.
17. `plugins/code_execution/init.lua` (lines 30–55, 220–370) — `code_execution` Python sandbox tool with dynamic tool injection and `describe` callback.
18. `plugins/edit/init.lua` (lines 240–450) — `edit` tool (plus conditional `multiedit`, `edit_lines`, `insert_lines`).
19. `plugins/glob/init.lua` (lines 15–90) — `glob` file search tool using `maki.fs.glob`.
20. `plugins/grep/init.lua` (lines 195–260) — `grep` regex content search tool.
21. `plugins/index/init.lua` (lines 150–210) — `index` code skeleton extraction tool via tree-sitter.
22. `plugins/list/init.lua` (lines 15–55) — `list` directory contents tool.
23. `plugins/memory/init.lua` (lines 170–225) — `memory` project notes tool.
24. `plugins/question/init.lua` (lines 35–105) — `question` interactive user questionnaire tool.
25. `plugins/read/init.lua` (lines 160–230) — `read` file slice inspection tool.
26. `plugins/sessions/init.lua` (lines 560–575) — Session manager plugin (registers `/sessions` and `/rename` commands, not a model tool).
27. `plugins/skill/init.lua` (lines 135–190) — `skill` prompt/workflow loading tool.
28. `plugins/task/init.lua` (lines 50–95, 150–280) — `task` subagent launcher with structured output support.
29. `plugins/todo_write/init.lua` (lines 105–160) — `todo_write` multi-step task list tracking tool.
30. `plugins/view_image/init.lua` (lines 115–150) — `view_image` multimodal image loader and downscaler.
31. `plugins/webfetch/init.lua` (lines 80–130) — `webfetch` HTTP scraping and HTML-to-text tool.
32. `plugins/websearch/init.lua` (lines 20–100) — `websearch` web search via Exa AI API.
33. `plugins/write/init.lua` (lines 25–90) — `write` whole-file writing tool.

---

## Key Code

### 1. Tool Trait and Invocations (`maki-agent/src/tools/registry.rs`, lines 166–212)
```rust
pub trait ToolInvocation: Send + Sync {
    fn start_header(&self) -> HeaderFuture;
    fn start_annotation(&self) -> Option<String> {
        None
    }
    fn start_output(&self, _ctx: &ToolContext) -> Option<ToolOutput> {
        None
    }
    fn mutable_path(&self) -> Option<&Path> {
        None
    }
    fn permission_scopes(&self) -> BoxFuture<'_, Option<PermissionScopes>> {
        Box::pin(std::future::ready(None))
    }
    fn start<'a>(&'a self, _ctx: &'a ToolContext) -> BoxFuture<'a, ()> {
        Box::pin(std::future::ready(()))
    }
    fn execute<'a>(self: Box<Self>, ctx: &'a ToolContext) -> ExecFuture<'a>;
}

pub trait Tool: Send + Sync + 'static {
    fn name(&self) -> &str;
    fn description(&self, ctx: &DescriptionContext) -> Cow<'_, str>;
    fn schema(&self) -> Value;
    fn examples(&self) -> Option<Value> {
        None
    }
    fn audience(&self) -> ToolAudience {
        ToolAudience::default()
    }
    fn tool_kind(&self) -> Option<&str> {
        None
    }
    fn parse(&self, input: &Value) -> Result<Box<dyn ToolInvocation>, ParseError>;
}
```

### 2. Parallel Processing of Tool Calls (`maki-agent/src/agent/tool_dispatch.rs`, lines 417–447)
```rust
    let mut set = TaskSet::new();
    let mut spawned_ids: Vec<String> = Vec::new();
    for (id, name, input) in runnable {
        spawned_ids.push(id.clone());
        let event_tx_clone = ctx.event_tx.clone();
        let tool_ctx = ToolContext {
            tool_use_id: Some(id.clone()),
            ..ctx.clone()
        };
        let mcp_owned = mcp.cloned();
        set.spawn(async move {
            let done = run(
                &tool_ctx.registry,
                mcp_owned.as_ref(),
                id,
                &name,
                &input,
                &tool_ctx,
                Emit::Notify,
            )
            .await;
            event_tx_clone.try_send(AgentEvent::ToolDone(Box::new(done.clone())));
            done
        });
    }

    let results: Vec<ToolDoneEvent> = set
        .join_all()
        .await
        .into_iter()
        .zip(spawned_ids)
        .map(|(r, id)| match r {
            Ok(out) => out,
            Err(e) => {
                error!(error = %e, "tool task panicked");
                ToolDoneEvent::error(id, format!("internal error: tool panicked: {e}"))
            }
        })
        .collect();
```

### 3. Dispatching Lua Tool Calls Across Channel (`maki-lua/src/api/tool.rs`, lines 413–444)
```rust
            let (reply_tx, reply_rx) = flume::bounded::<ToolCallReply>(1);
            let live = ctx.tool_use_id.clone().map(|id| LiveCtx {
                event_tx: ctx.event_tx.clone(),
                tool_use_id: id,
            });
            let lua_ctx = LuaCtx::handler(ctx);

            if tx
                .send_async(Request::CallTool {
                    plugin: Arc::clone(&plugin),
                    tool: Arc::clone(&tool),
                    input,
                    ctx: Box::new(lua_ctx),
                    deadline: match deadline {
                        Deadline::At(t) => Some(t),
                        Deadline::None => None,
                    },
                    reply: reply_tx,
                    live,
                })
                .await
                .is_err()
            {
                return Err("lua thread disconnected".to_string()).into();
            }
```

---

## Architecture

### 1. Default Agent Tool Set
In `maki-config/src/lib.rs` (lines 58–77), `DEFAULT_BUILTINS` lists 19 bundled plugins embedded at compile time via `include_dir!` in `maki-lua/src/loader.rs` (lines 27–118):
1. **`bash`** (`plugins/bash/init.lua:261`): Executes shell commands with AST-based tree-sitter permission analysis, live stdout streaming, and automatic `rtk` rewriting.
2. **`batch`** (`plugins/batch/init.lua:582`): Executes up to 25 independent tool calls concurrently via `maki.async.gather`.
3. **`code_execution`** (`plugins/code_execution/init.lua:357`): Runs sandboxed Python code via the `monty` interpreter with other tools exposed as async functions.
4. **`edit`** (`plugins/edit/init.lua:248`): Performs exact find-and-replace text edits on files. Depending on plugin configuration, it also registers:
   - `multiedit` (`plugins/edit/init.lua:299`): Multiple sequential replacements in a single file.
   - `edit_lines` (`plugins/edit/init.lua:385`): Replace a line number range.
   - `insert_lines` (`plugins/edit/init.lua:439`): Insert lines at a specified line index.
5. **`glob`** (`plugins/glob/init.lua:17`): Finds files matching glob patterns, sorted by mtime.
6. **`grep`** (`plugins/grep/init.lua:196`): Regular expression search across file contents with context lines.
7. **`index`** (`plugins/index/init.lua:154`): Uses tree-sitter parsers to extract structural skeletons and symbol outlines from code files.
8. **`list`** (`plugins/list/init.lua:16`): Lists directory entries (directories first, filtering out instruction files).
9. **`memory`** (`plugins/memory/init.lua:170`): Reads, writes, searches, and lists project memory notes stored under `projects/<id>/memories`.
10. **`question`** (`plugins/question/init.lua:36`): Presents interactive questions and option pickers to the user.
11. **`read`** (`plugins/read/init.lua:162`): Reads slices of text files with line numbers, enforcing `offset` and `limit`.
12. **`sessions`** (`plugins/sessions/init.lua:565`): Bundled plugin that exposes `/sessions` and `/rename` TUI commands; registers no agent tool.
13. **`skill`** (`plugins/skill/init.lua:136`): Discovers and loads prompt instructions and workflows from project/global skill directories.
14. **`task`** (`plugins/task/init.lua:254`): Launches autonomous `research` (read-only) or `general` (read-write) subagents with optional structured JSON output.
15. **`todo_write`** (`plugins/todo_write/init.lua:108`): Maintains an in-session todo list displayed in the TUI status area.
16. **`view_image`** (`plugins/view_image/init.lua:117`): Reads, downscales (to 1568px/3MB), base64-encodes, and injects images as multimodal vision blocks.
17. **`webfetch`** (`plugins/webfetch/init.lua:80`): Fetches web pages via HTTP/HTTPS and converts them to markdown, plain text, or HTML.
18. **`websearch`** (`plugins/websearch/init.lua:23`): Searches the web using the Exa AI JSON-RPC/SSE endpoint.
19. **`write`** (`plugins/write/init.lua:27`): Writes full file content to disk, checking `FileReadTracker` first.

*Note on dynamic/runtime tools:*
- **`tool_search`** (`maki-agent/src/mcp/config.rs:130`, `maki-agent/src/agent/tool_dispatch.rs:178`): Synthetic tool injected when an MCP server has deferred tools exceeding the deferral threshold.
- **`structured_output`** (`plugins/task/init.lua:153`): Temporary local tool injected into subagents when an `output_schema` is supplied to `task`.

---

### 2. Definition and Registration

```
┌─────────────────────────────────┐
│     Lua Plugin (init.lua)       │
│  maki.api.register_tool({ ... })│
└────────────────┬────────────────┘
                 │ Spec table (name, schema, handler, header, restore, etc.)
                 ▼
┌─────────────────────────────────┐
│  maki-lua/src/api/tool.rs       │
│  register_tool_from_lua()       │ ── Converts JSON schema to static ParamSchema
└────────────────┬────────────────┘
                 │ PendingTool
                 ▼
┌─────────────────────────────────┐
│  maki-lua/src/runtime.rs        │
│  LuaTool (implements Tool)      │
└────────────────┬────────────────┘
                 │ Arc<dyn Tool>
                 ▼
┌─────────────────────────────────┐
│  maki-agent ToolRegistry        │
│  ArcSwap<Vec<RegisteredTool>>   │ ── Lock-free reads, RCU atomic swap on write
└─────────────────────────────────┘
```

1. **Lua Specification** (`plugins/<name>/init.lua`): Plugins call `maki.api.register_tool(spec)` where `spec` defines `name`, `description`, `schema`, `handler`, `audiences`, `permission_scopes`, `mutable_path`, `timeout`, `header`, `restore`, `start`, `describe`, and `examples`.
2. **Schema Parsing & Leaking** (`maki-agent/src/tools/schema.rs:152`): The JSON schema table from Lua is parsed into a Rust `ParamSchema` enum (`Primitive`, `Enum`, `Array`, `Object`, `Any`). String references are leaked to produce `'static` lifetimes for lock-free access.
3. **Registry Storage** (`maki-agent/src/tools/registry.rs:248`): Tools are wrapped in `LuaTool` and registered into `ToolRegistry`. The registry stores `ArcSwap<Vec<RegisteredTool>>`, allowing lock-free reads while updates atomically swap the vector pointer via Read-Copy-Update (`rcu`).

---

### 3. Exposing Tools to the Model
When preparing an LLM request (`maki-agent/src/tools/registry.rs:420` and `maki-agent/src/headless.rs:131`):
1. **Filtering** (`ToolFilter` in `maki-agent/src/tools/mod.rs:45`): Tools are filtered against `allowed_tools` and `disabled_tools` in `AgentConfig`. Non-vision models automatically exclude `view_image` via `capability_exclusions` (`maki-agent/src/tools/mod.rs:88`).
2. **Audience Partitioning** (`ToolAudience` in `maki-agent/src/tools/registry.rs:21`): Tools specify bitflags (`MAIN`, `RESEARCH_SUB`, `GENERAL_SUB`, `INTERPRETER`, `WORKFLOW`). Only matching tools enter the payload.
3. **Dynamic Descriptions** (`maki-agent/src/tools/registry.rs:434`): If a tool defines a `describe(dctx)` function (e.g. `code_execution` dynamically listing visible tool signatures), it is queried across the Lua thread. Prompt template variables are applied via `Vars::apply`.
4. **Schema Output**: `to_json_schema` generates JSON Schema 2020-12 / OpenAI-compatible definitions. If the model supports `input_examples`, they are added as `input_examples`; otherwise they are appended to the description text (`maki-agent/src/tools/registry.rs:444`).
5. **Prompt Slots** (`maki-agent/src/prompts/`, `maki-agent/src/prompt.rs:34`): Plugins inject prompt hints into slots such as `tool_usage`, `efficient_tools`, and `after_instructions` via `maki.api.register_prompt_hint`.

---

### 4. Dispatch and Execution Lifecycle

```
Model Turn StreamResponse
       │
       ▼
streaming.rs / tool_dispatch.rs
  1. Strip 'functions.' prefix (canonical_tool_name)
  2. Record call in RecentCalls
  3. Doom-loop check (≥3 identical calls -> immediate error)
       │
       ▼
Parallel Fan-Out (maki-agent/src/task_set.rs)
  smol::spawn(TaskSet) for each runnable tool call
       │
       ├───────────────────────────────────────────────────────┐
       ▼                                                       ▼
Native / Lua Tool                                         MCP Tool / Local Override
  1. Schema validation (maki-agent/src/tools/schema.rs)    1. execute_mcp_tool
     (repair JSON, coerce types, resolve aliases)          2. Enforce MCP permissions
  2. Plan mode check (mutable_path == plan_path)           3. Call McpSession
  3. Emit ToolStartEvent (header + start preview)
  4. Enforce PermissionManager
  5. Send Request::CallTool over Flume channel to Lua
       │
       ▼
Lua Dedicated Thread (maki-lua/src/runtime.rs)
  1. Acquire LocalExecutor gate slot
  2. Spawn Luau coroutine for handler(input, ctx)
  3. Arm watchdog interrupt timer (KILL_GRACE = 500ms)
  4. Stream live buffers / progress
  5. Reply with ToolCallReply via flume sender
       │
       ▼
Result Aggregation (maki-agent/src/agent/tool_dispatch.rs)
  1. TaskSet::join_all() catches panics via catch_unwind
  2. Collect ToolDoneEvents (output, diff, image, annotations)
  3. Append combined tool_results message to History
  4. Emit AgentEvent::ToolResultsSubmitted
```

---

### 5. Async Semantics and Concurrency
- **Rust Harness**: Uses `smol` async runtime. Tools in a multi-call model turn are spawned concurrently using `TaskSet` (`maki-agent/src/task_set.rs:12`).
- **Lua Thread Isolation**: Lua/Luau execution is not multi-threaded across OS threads. A single dedicated background OS thread runs the Lua engine (`maki-lua/src/runtime.rs`). Inside this thread, a single-threaded `async_executor::LocalExecutor` schedules Luau coroutines.
- **Batching & Gathering**:
  - `batch` plugin: runs tool calls concurrently on the Lua event loop via `maki.async.gather` (`plugins/batch/init.lua:520`).
  - `code_execution` plugin: Python scripts use `gather(*calls)` or parallel awaits in Monty, which dispatches batches of `PendingCall` objects across the bridge into Lua concurrent futures (`maki-lua/src/api/interpreter.rs:142`).
  - `task` subagents: limited by a global semaphore (`maki.async.semaphore(max_concurrent)`, default 8 in `plugins/task/init.lua:94`).

---

### 6. Error Handling and Panic Safety
- **Malformed JSON Inputs**: `maki-agent/src/tools/schema.rs` runs `jsonrepair` on unparseable inputs and coerces mismatched types (e.g. numeric strings to integers, stringified JSON arrays/objects to real arrays/objects). Validation failures return clean, formatted error strings back to the model without raw serde traces.
- **Doom-Loop Detection** (`maki-agent/src/agent/tool_dispatch.rs:30`): If the model calls a tool with identical arguments 3 times in succession (`DOOM_LOOP_THRESHOLD = 3`), the call is aborted immediately with a prompt instruction to break out of the loop.
- **Panic Boundary**: `TaskSet` wraps every spawned task in `AssertUnwindSafe(future).catch_unwind()`. Panics are caught, captured with a backtrace, and converted into `ToolDoneEvent::error` (`maki-agent/src/task_set.rs:25`). The agent loop never panics.
- **Stale Write Guard**: `FileReadTracker` (`maki-agent/src/tools/file_tracker.rs:35`) records file modification times when read; any write or edit tool checking `check_before_edit` fails if the file changed on disk after the agent read it.
- **Plan Mode Write Restrictions**: In Plan mode, any tool specifying `mutable_path` attempting to modify a file other than the plan file is rejected with `PLAN_WRITE_RESTRICTED` (`maki-agent/src/agent/tool_dispatch.rs:128`).

---

### 7. Timeouts and Cancellation
- **Two-Layer Deadline Enforcement** (`maki-lua/src/runtime.rs:2370`):
  1. *CPU Watchdog*: A background watchdog thread checks task execution. If a Lua thread exceeds its deadline without yielding, Luau's debug interrupt hook kills the coroutine within `KILL_GRACE = 500ms` (`maki-lua/src/runtime.rs:66`).
  2. *Async / IO Timer*: `smol::Timer` and `futures_lite::future::race` enforce wall-clock timeouts on async waits.
- **Graceful Cancellation & Partial Returns**: When the user presses `Esc` or a timeout fires, `CancelToken` triggers. Tools like `bash`, `batch`, and `code_execution` register `maki.async.on_cancel` hooks to flush and return whatever partial output was streamed so far rather than dropping the output (`plugins/bash/init.lua:426`, `plugins/batch/init.lua:511`, `plugins/code_execution/init.lua:280`).

---

### 8. Language and Sandbox Boundaries (Rust / Lua / Python / JS)
- **Rust**: Core agent loop, async scheduling (`smol`), tool registry, schema compilation & validation, permissions, MCP client, file tracking, tree-sitter highlighter, and process spawning.
- **Luau (Lua)**: All 19 default plugins and tool handlers run in the embedded Luau engine via MLua.
- **Python (Monty Sandbox)**: The `code_execution` tool uses `monty` (a minimal Rust-implemented Python interpreter). It executes Python code with memory limits (default 50MB) and execution budgets (default 30s) (`plugins/code_execution/init.lua:51`). Tool function invocations inside Python are forwarded through Rust Flume channels (`maki-lua/src/api/interpreter.rs:142`) to invoke Lua tool functions, and output is streamed back.
- **JavaScript**: **No JavaScript runtime exists in Maki**. Tree-sitter includes a JavaScript/TypeScript grammar for code analysis and highlighting, but there is no V8, QuickJS, or Node runtime.

---

## Start Here
To trace the tool subsystem from source, open these files in order:
1. `maki-config/src/lib.rs` (line 58) — View the complete `DEFAULT_BUILTINS` list.
2. `maki-agent/src/tools/registry.rs` (line 166) — Understand the `Tool` and `ToolInvocation` trait contracts.
3. `maki-agent/src/agent/tool_dispatch.rs` (line 383) — Follow `process_tool_calls` and `run` to see how the agent executes tools in parallel.
4. `maki-lua/src/api/tool.rs` (line 1243) — Examine how Lua tool definitions are mapped to Rust `ParamSchema` and `LuaTool`.
5. `plugins/batch/init.lua` (line 520) and `plugins/code_execution/init.lua` (line 220) — See the primary orchestration tools in Lua.

---

## Gaps
1. **Session-Level Custom Tool Overrides (`local_tools`)**: `ToolContext` carries a `local_tools` map (`maki-agent/src/tools/mod.rs:159`) that shadows registry tools. In production, this is used primarily by `task` for the temporary `structured_output` tool; other host-level overrides were observed only in test scaffolding.
2. **MCP Live Server Discovery**: MCP dynamic loading and deferred tool search (`maki-agent/src/mcp/`) were analysed through code inspection and unit tests rather than against live external MCP servers.

### Subagent result 5 · source line 38 · `96edcbd3` · active branch

Agent: `explore`  
Model: `google/gemini-3.7-flash`  
Task: Research Pi under .research/pi/source. Read-only. Determine its default coding-agent tool set and how each tool is defined, registered/exposed to the model, dispatched, and implemented. Trace the full tool-call lifecycle, including system prompt/tool descriptions, async semantics, multiple tool calls in one model turn, concurrency or sequential execution, result collection, error handling, cancellation, extension/custom-tool seams, and relevant tests/docs. Fan out through callers/callees. Return concise, neutral findings with exact repository-relative file paths and 1-based line numbers for every material claim. Flag ambiguities or behaviour inferred rather than tested. Do not recommend changes to kiki.

## Files Retrieved
1. `packages/coding-agent/src/core/tools/index.ts` (lines 1–225) — Built-in tool registry, exports, `ToolName` union, factory helpers (`createToolDefinition`, `createCodingTools`, `createReadOnlyTools`, `createAllToolDefinitions`).
2. `packages/coding-agent/src/core/tools/read.ts` (lines 20–260) — `read` tool schema, `ReadOperations` interface, vision/image auto-resizing, line/byte truncation, system prompt contributions.
3. `packages/coding-agent/src/core/tools/bash.ts` (lines 35–240, 348–535) — `bash` and shell tool creation, `BashOperations`, process spawning, process-tree killing, `PI_*` session environment variable injection, streaming output via `OutputAccumulator`.
4. `packages/coding-agent/src/core/tools/powershell.ts` (lines 1–58) — `powershell` tool definition wrapping shell operations with UTF-8 encoding preamble.
5. `packages/coding-agent/src/core/tools/edit.ts` (lines 35–160, 274–370) — `edit` tool schema, legacy argument normalization, file mutation queue serialization, diff/patch generation, and line-ending preservation.
6. `packages/coding-agent/src/core/tools/write.ts` (lines 14–45, 180–210) — `write` tool schema, `WriteOperations`, directory auto-creation, file mutation queue integration.
7. `packages/coding-agent/src/core/tools/grep.ts` (lines 20–60, 115–280) — `grep` tool schema, `GrepOperations`, single-file in-memory search vs directory `rg` (ripgrep) subprocess, line and byte truncation.
8. `packages/coding-agent/src/core/tools/find.ts` (lines 20–55, 105–240) — `find` tool schema, `FindOperations`, `fd` subprocess invocation, path relativization and POSIX normalization.
9. `packages/coding-agent/src/core/tools/ls.ts` (lines 12–45, 72–135) — `ls` tool schema, `LsOperations`, directory reading, alphabetical sorting, directory trailing slash formatting.
10. `packages/coding-agent/src/core/tools/file-mutation-queue.ts` (lines 1–58) — Per-file serialization queue for concurrent file write/edit operations.
11. `packages/coding-agent/src/core/tools/output-accumulator.ts` (lines 20–120) — Bounded streaming output accumulator with rolling decoded tail and spillover to temporary log files.
12. `packages/coding-agent/src/core/tools/truncate.ts` (lines 1–80) — Shared line (default 2000) and byte (default 50KB) head/tail truncation logic.
13. `packages/coding-agent/src/core/tools/tool-definition-wrapper.ts` (lines 1–39) — Bidirectional adapter between `ToolDefinition` (extension/TUI layer) and `AgentTool` (core execution layer).
14. `packages/coding-agent/src/core/system-prompt.ts` (lines 10–165) — `buildSystemPrompt()`, formatting tool snippets into `Available tools:` and guidelines into `Guidelines:`.
15. `packages/coding-agent/src/core/agent-session.ts` (lines 485–575, 920–965, 2638–2780) — `AgentSession` tool registry management, tool allow/exclude filtering, before/after tool hooks installation, turn refresh synchronization.
16. `packages/coding-agent/src/core/extensions/types.ts` (lines 450–496, 915–1040, 1160–1260) — `ToolDefinition` interface, `ToolCallEvent`, `ToolResultEvent`, `ToolCallEventResult`, `ExtensionAPI`.
17. `packages/coding-agent/src/core/extensions/wrapper.ts` (lines 1–36) — Extension context binding and dynamic tool tracking (`addedToolNames`).
18. `packages/agent/src/agent-loop.ts` (lines 135–270, 280–340, 365–540, 544–730) — Core agent loop, model streaming, sequential vs parallel tool dispatch, argument validation, cancellation, result collection.
19. `packages/agent/src/agent.ts` (lines 50–120, 200–310) — `Agent` class wrapping mutable state and driving `agentLoop`/`agentLoopContinue`.
20. `packages/agent/src/types.ts` (lines 40–140) — Core agent types: `AgentTool`, `ToolExecutionMode`, `BeforeToolCallContext`, `AfterToolCallContext`, `AgentToolResult`.
21. `packages/ai/src/utils/validation.ts` (lines 295–350) — TypeBox schema validation and argument coercion for tool calls.

---

## Key Code

### Tool Definition Interface
`packages/coding-agent/src/core/extensions/types.ts` (lines 451–496):
```ts
export interface ToolDefinition<TParams extends TSchema = TSchema, TDetails = unknown, TState = any> {
	/** Tool name (used in LLM tool calls) */
	name: string;
	/** Human-readable label for UI */
	label: string;
	/** Description for LLM */
	description: string;
	/** Optional one-line snippet for the Available tools section in the default system prompt. Custom tools are omitted from that section when this is not provided. */
	promptSnippet?: string;
	/** Optional guideline bullets appended to the default system prompt Guidelines section when this tool is active. */
	promptGuidelines?: string[];
	/** Parameter schema (TypeBox) */
	parameters: TParams;
	/** Optional provider-side constrained sampling request for this tool. Set false to explicitly disable it, equivalent to leaving it undefined. */
	constrainedSampling?: false | ConstrainedSamplingConfig;
	/** Controls whether ToolExecutionComponent renders the standard colored shell or the tool renders its own framing. */
	renderShell?: "default" | "self";

	/** Optional compatibility shim to prepare raw tool call arguments before schema validation. Must return an object conforming to TParams. */
	prepareArguments?: (args: unknown) => Static<TParams>;

	/**
	 * Per-tool execution mode override.
	 * - "sequential": this tool must execute one at a time with other tool calls.
	 * - "parallel": this tool can execute concurrently with other tool calls.
	 *
	 * If omitted, the default execution mode applies.
	 */
	executionMode?: ToolExecutionMode;

	/** Execute the tool. */
	execute(
		toolCallId: string,
		params: Static<TParams>,
		signal: AbortSignal | undefined,
		onUpdate: AgentToolUpdateCallback<TDetails> | undefined,
		ctx: ExtensionContext,
	): Promise<AgentToolResult<TDetails>>;

	/** Custom rendering for tool call display */
	renderCall?: (args: Static<TParams>, theme: Theme, context: ToolRenderContext<TState, Static<TParams>>) => Component;

	/** Custom rendering for tool result display */
	renderResult?: (
		result: AgentToolResult<TDetails>,
		options: ToolRenderResultOptions,
		theme: Theme,
		context: ToolRenderContext<TState, Static<TParams>>,
	) => Component;
}
```

### Tool Execution Dispatch in Agent Loop
`packages/agent/src/agent-loop.ts` (lines 397–408, 462–516):
```ts
async function executeToolCalls(
	currentContext: AgentContext,
	assistantMessage: AssistantMessage,
	config: AgentLoopConfig,
	signal: AbortSignal | undefined,
	emit: AgentEventSink,
): Promise<ExecutedToolCallBatch> {
	const toolCalls = assistantMessage.content.filter((c) => c.type === "toolCall");
	const hasSequentialToolCall = toolCalls.some(
		(tc) => currentContext.tools?.find((t) => t.name === tc.name)?.executionMode === "sequential",
	);
	if (config.toolExecution === "sequential" || hasSequentialToolCall) {
		return executeToolCallsSequential(currentContext, assistantMessage, toolCalls, config, signal, emit);
	}
	return executeToolCallsParallel(currentContext, assistantMessage, toolCalls, config, signal, emit);
}

async function executeToolCallsParallel(
	currentContext: AgentContext,
	assistantMessage: AssistantMessage,
	toolCalls: AgentToolCall[],
	config: AgentLoopConfig,
	signal: AbortSignal | undefined,
	emit: AgentEventSink,
): Promise<ExecutedToolCallBatch> {
	const finalizedCalls: FinalizedToolCallEntry[] = [];

	for (const toolCall of toolCalls) {
		await emit({
			type: "tool_execution_start",
			toolCallId: toolCall.id,
			toolName: toolCall.name,
			args: toolCall.arguments,
		});

		const preparation = await prepareToolCall(currentContext, assistantMessage, toolCall, config, signal);
		if (preparation.kind === "immediate") {
			const finalized = {
				toolCall,
				result: preparation.result,
				isError: preparation.isError,
			} satisfies FinalizedToolCallOutcome;
			await emitToolExecutionEnd(finalized, emit);
			finalizedCalls.push(finalized);
			if (signal?.aborted) {
				break;
			}
			continue;
		}

		finalizedCalls.push(async () => {
			const executed = await executePreparedToolCall(preparation, signal, emit);
			const finalized = await finalizeExecutedToolCall(
				currentContext,
				assistantMessage,
				preparation,
				executed,
				config,
				signal,
			);
			await emitToolExecutionEnd(finalized, emit);
			return finalized;
		});
		if (signal?.aborted) {
			break;
		}
	}

	const orderedFinalizedCalls = await Promise.all(
		finalizedCalls.map((entry) => (typeof entry === "function" ? entry() : Promise.resolve(entry))),
	);
	const messages: ToolResultMessage[] = [];
	for (const finalized of orderedFinalizedCalls) {
		const toolResultMessage = createToolResultMessage(finalized);
		await emitToolResultMessage(toolResultMessage, emit);
		messages.push(toolResultMessage);
	}

	return {
		messages,
		terminate: shouldTerminateToolBatch(orderedFinalizedCalls),
	};
}
```

### Per-File Concurrency Protection
`packages/coding-agent/src/core/tools/file-mutation-queue.ts` (lines 29–57):
```ts
export async function withFileMutationQueue<T>(filePath: string, fn: () => Promise<T>): Promise<T> {
	const registration = registrationQueue.then(async () => {
		const key = await getMutationQueueKey(filePath);
		const currentQueue = fileMutationQueues.get(key) ?? Promise.resolve();

		let releaseNext!: () => void;
		const nextQueue = new Promise<void>((resolveQueue) => {
			releaseNext = resolveQueue;
		});
		const chainedQueue = currentQueue.then(() => nextQueue);
		fileMutationQueues.set(key, chainedQueue);

		return { key, currentQueue, chainedQueue, releaseNext };
	});
	registrationQueue = registration.then(
		() => undefined,
		() => undefined,
	);

	const { key, currentQueue, chainedQueue, releaseNext } = await registration;
	await currentQueue;
	try {
		return await fn();
	} finally {
		releaseNext();
		if (fileMutationQueues.get(key) === chainedQueue) {
			fileMutationQueues.delete(key);
		}
	}
}
```

---

## Architecture

```
                               ┌──────────────────────────────────────────────┐
                               │                 AgentSession                 │
                               │  - Instantiates built-in ToolDefinitions     │
                               │  - Merges extension/custom tools             │
                               │  - Filters allowed/excluded tools            │
                               │  - Connects ExtensionRunner hooks            │
                               └──────────────────────┬───────────────────────┘
                                                      │
                       ┌──────────────────────────────┴──────────────────────────────┐
                       │                                                            │
                       ▼                                                            ▼
    ┌──────────────────────────────────────┐                     ┌──────────────────────────────────────┐
    │          buildSystemPrompt()         │                     │             Agent (Core)             │
    │  - Extracts promptSnippets           │                     │  - Holds active tools array          │
    │  - Renders "Available tools:"        │                     │  - beforeToolCall hook               │
    │  - Renders tool "Guidelines:"        │                     │  - afterToolCall hook                │
    │  - Formats skills & context files    │                     │  - convertToLlm / transformContext   │
    └──────────────────────────────────────┘                     └──────────────────┬───────────────────┘
                                                                                    │
                                                                                    ▼
                                                                 ┌──────────────────────────────────────┐
                                                                 │              agentLoop()             │
                                                                 │  - streamAssistantResponse()         │
                                                                 │  - JSON arg parsing & validation     │
                                                                 │  - Length-truncation failure guard   │
                                                                 │  - executeToolCalls()                │
                                                                 │    ├─ Sequential vs Parallel         │
                                                                 │    ├─ onUpdate progress streaming    │
                                                                 │    └─ Result collection & ordering   │
                                                                 └──────────────────┬───────────────────┘
                                                                                    │
                       ┌────────────────────────────────────────────────────────────┴───────────────────────────┐
                       ▼                                                                                        ▼
       ┌───────────────────────────────┐                                                        ┌───────────────────────────────┐
       │   Built-in Coding Tool Set    │                                                        │   Pluggable Operation Hooks   │
       │  - read (text, images, limit) │                                                        │  - ReadOperations (SSH/remote)│
       │  - bash / powershell (spawn)  │                                                        │  - BashOperations             │
       │  - edit (diff/patch, LF/CRLF) │                                                        │  - EditOperations             │
       │  - write (mkdir -p, file lock)│                                                        │  - WriteOperations            │
       │  - grep (rg / single-file)    │                                                        │  - GrepOperations             │
       │  - find (fd / glob)           │                                                        │  - FindOperations             │
       │  - ls (readdir + stats)       │                                                        │  - LsOperations               │
       └───────────────────────────────┘                                                        └───────────────────────────────┘
```

### 1. Default Coding-Agent Tool Set
- **Default Coding Tools** (`packages/coding-agent/src/core/tools/index.ts:195–202`): `read`, `bash`, `edit`, `write`. Initialized by default when no tool override is specified (`packages/coding-agent/src/core/sdk.ts:257–264`).
- **Read-Only Suite** (`packages/coding-agent/src/core/tools/index.ts:204–211`): `read`, `grep`, `find`, `ls`.
- **All Built-in Tool Definitions** (`packages/coding-agent/src/core/tools/index.ts:114–123`, `182–193`): `read`, `bash`, `powershell`, `edit`, `write`, `grep`, `find`, `ls`.

### 2. Tool Definition & Pluggable Operations
Each tool is created via a factory returning a `ToolDefinition<TParams, TDetails>` and exposes a swappable `*Operations` interface to delegate operations (e.g. for remote execution over SSH):
- **`read`** (`packages/coding-agent/src/core/tools/read.ts`):
  - Parameters: `path` (string), optional `offset` (number, 1-based), optional `limit` (number).
  - Features: Reads text or images. Detects image MIME types (png, jpg, jpeg, gif, webp, bmp); if the model supports vision (`model.input.includes("image")`), returns image content block; auto-resizes images exceeding 2000×2000 when `autoResizeImages` is active. Truncates text output using `truncateHead` (`packages/coding-agent/src/core/tools/truncate.ts:68–120`) to 2000 lines or 50KB.
  - Pluggable Operations: `ReadOperations` (`readFile`, `access`, `detectImageMimeType`).
- **`bash`** (`packages/coding-agent/src/core/tools/bash.ts`):
  - Parameters: `command` (string), optional `timeout` (number in seconds).
  - Features: Spawns local shell in a detached process group (Unix) to allow complete subprocess tree termination via `killProcessTree` (`packages/coding-agent/src/utils/shell.ts`). Injects session environment variables (`PI_SESSION_ID`, `PI_SESSION_FILE`, `PI_PROVIDER`, `PI_MODEL`, `PI_REASONING_LEVEL`) if `exposeSessionEnvironment: true`. Streams stdout/stderr through `OutputAccumulator` (`packages/coding-agent/src/core/tools/output-accumulator.ts`), throttling updates every 100ms. Truncates to last 2000 lines / 50KB (`truncateTail`), saving full output to a temp file (`pi-bash-*.log`) if truncated.
  - Pluggable Operations: `BashOperations` (`exec`).
- **`powershell`** (`packages/coding-agent/src/core/tools/powershell.ts`):
  - Specialised shell tool wrapping `createShellToolDefinition`. Prepends `try { [Console]::OutputEncoding=[System.Text.Encoding]::UTF8 } catch {}\n` to ensure UTF-8 output decoding.
  - Pluggable Operations: `PowerShellOperations` (alias of `BashOperations`).
- **`edit`** (`packages/coding-agent/src/core/tools/edit.ts`):
  - Parameters: `path` (string), `edits` (array of `{ oldText: string, newText: string }`). Normalizes legacy top-level `{ oldText, newText }` via `prepareArguments`.
  - Features: Wraps execution in `withFileMutationQueue(absolutePath, ...)` (`packages/coding-agent/src/core/tools/file-mutation-queue.ts:29–57`) to serialize concurrent edits targeting the same canonical file path. Strips UTF-8 BOM, normalizes line endings to LF (`packages/coding-agent/src/core/tools/edit-diff.ts:11–21`), verifies exact unique occurrences of each `oldText`, computes unified diff/patch with `diff` library, restores original line endings (CRLF vs LF) and BOM, and writes output.
  - Pluggable Operations: `EditOperations` (`readFile`, `writeFile`, `access`).
- **`write`** (`packages/coding-agent/src/core/tools/write.ts`):
  - Parameters: `path` (string), `content` (string).
  - Features: Wrapped in `withFileMutationQueue`. Automatically creates missing parent directories via `mkdir(dirname(path), { recursive: true })`.
  - Pluggable Operations: `WriteOperations` (`writeFile`, `mkdir`).
- **`grep`** (`packages/coding-agent/src/core/tools/grep.ts`):
  - Parameters: `pattern` (string), optional `path`, `glob`, `ignoreCase`, `literal`, `context`, `limit` (default 100).
  - Features: Single file uses in-memory regex/literal search; directory search invokes `rg` (ripgrep) subprocess. Truncates long lines to 500 characters (`GREP_MAX_LINE_LENGTH`) and total output to 50KB or match limit.
  - Pluggable Operations: `GrepOperations` (`isDirectory`, `readFile`).
- **`find`** (`packages/coding-agent/src/core/tools/find.ts`):
  - Parameters: `pattern` (string), optional `path`, `limit` (default 1000).
  - Features: Spawns `fd` subprocess (respects `.gitignore`). Relativizes output paths against search directory and normalizes to POSIX separators (`/`).
  - Pluggable Operations: `FindOperations` (`exists`, `glob`).
- **`ls`** (`packages/coding-agent/src/core/tools/ls.ts`):
  - Parameters: optional `path`, optional `limit` (default 500).
  - Features: Node.js `fs.readdir` + `fs.stat`. Appends `/` to directory entries, sorts alphabetically including dotfiles.
  - Pluggable Operations: `LsOperations` (`exists`, `stat`, `readdir`).

### 3. Registration and Exposure to the Model
- **Registry Initialization**: `AgentSession._buildRuntime()` (`packages/coding-agent/src/core/agent-session.ts:2725–2780`) instantiates built-in definitions (`createAllToolDefinitions`), merges extension tools (`_resourceLoader.getExtensions()`) and custom tools (`customTools`), filters tools by `allowedToolNames` and `excludedToolNames`, wraps them with `wrapRegisteredTools()`, and sets `agent.state.tools` via `setActiveToolsByName()`.
- **System Prompt Assembly**: `buildSystemPrompt()` (`packages/coding-agent/src/core/system-prompt.ts:31–165`) builds `Available tools:` from the active tools' `promptSnippet`s (tools lacking a snippet are omitted from the visible list). Injects tool-specific guidelines (e.g. falling back to bash commands when `grep`/`find`/`ls` are missing; inspecting `PI_*` environment variables).
- **LLM Schema Generation**: In `agent-loop.ts:streamAssistantResponse()` (`packages/agent/src/agent-loop.ts:290–305`), `Context.tools` is passed to the provider stream function (`pi-ai`), where provider-specific serializers convert TypeBox parameter schemas into Anthropic, OpenAI, or Google function-calling tool specifications.

### 4. Full Tool-Call Lifecycle
1. **Model Streaming**: LLM stream emits `toolcall_start`, `toolcall_delta`, and `toolcall_end` events (`packages/agent/src/agent-loop.ts:316–336`). Tool arguments are assembled from streamed JSON chunks.
2. **Length-Truncation Check**: If the assistant message terminates with `stopReason === "length"` (`packages/agent/src/agent-loop.ts:207–211`), `failToolCallsFromTruncatedMessage` immediately fails all tool calls without execution (`packages/agent/src/agent-loop.ts:365–392`), preventing execution of truncated or corrupted arguments.
3. **Execution Mode Determination**: `executeToolCalls()` (`packages/agent/src/agent-loop.ts:397–408`) checks whether `config.toolExecution === "sequential"` or any tool in the batch specifies `executionMode === "sequential"`. If so, sequential dispatch is forced; otherwise parallel dispatch is used.
4. **Tool Preparation**:
   - `tool.prepareArguments()` transforms raw inputs (e.g. `edit` tool normalizes legacy arguments).
   - `validateToolArguments(tool, toolCall)` (`packages/ai/src/utils/validation.ts:308–350`) validates and coerces arguments against TypeBox schema.
   - `beforeToolCall` hook fires (`packages/agent/src/agent-loop.ts:561–591`). In `AgentSession` (`packages/coding-agent/src/core/agent-session.ts:489–510`), this fires the extension runner's `tool_call` event. Extensions can mutate arguments in place or return `{ block: true, reason: string, terminate?: boolean }`.
   - If blocked, an error `AgentToolResult` is synthesized immediately without invoking `tool.execute`.
5. **Execution & Async Semantics**:
   - Emits `tool_execution_start`.
   - Sequential mode executes tools one by one (`executeToolCallsSequential`, `packages/agent/src/agent-loop.ts:415–460`).
   - Parallel mode launches all allowed tool promises concurrently via `Promise.all` (`executeToolCallsParallel`, `packages/agent/src/agent-loop.ts:462–516`).
   - During execution, tools invoke `onUpdate(partialResult)` to emit `tool_execution_update` events.
   - If `tool.execute()` throws an exception, `executePreparedToolCall()` catches it and converts it into `{ result: createErrorToolResult(err.message), isError: true }` (`packages/agent/src/agent-loop.ts:634–643`).
6. **AfterToolCall Hook & Result Normalization**:
   - `finalizeExecutedToolCall()` (`packages/agent/src/agent-loop.ts:648–687`) triggers `afterToolCall`. In `AgentSession` (`packages/coding-agent/src/core/agent-session.ts:512–548`), this emits the extension runner's `tool_result` event, allowing extensions to override `content`, `details`, `isError`, `usage`, or set `terminate: true`.
   - `normalizeToolResultImages()` auto-resizes image attachments present in tool results (`packages/coding-agent/src/core/agent-session.ts:534–536`).
7. **Result Emission & Ordering**:
   - Emits `tool_execution_end` (in parallel mode, this fires in completion order as each tool finishes; `packages/agent/src/agent-loop.ts:503`).
   - Generates `ToolResultMessage` (`role: "toolResult"`).
   - In parallel mode, `ToolResultMessage`s are emitted (`message_start`, `message_end`) and pushed into `currentContext.messages` in original assistant tool-call source order (`packages/agent/src/agent-loop.ts:509–514`).
8. **Loop Continuation / Termination**:
   - Checks `shouldTerminateToolBatch` (`packages/agent/src/agent-loop.ts:524–526`): if all tool calls in the batch requested termination (`terminate === true`), execution halts.
   - Emits `turn_end`.
   - Fires `prepareNextTurnWithContext()` (`packages/coding-agent/src/core/agent-session.ts:550–569`), refreshing system prompts, active tools, and models before the next turn.

### 5. Extension & Custom Tool Seams
- **Extension Tool Registration**: Extensions call `pi.registerTool(definition)` (`packages/coding-agent/src/core/extensions/types.ts:451–496`, `1347–1365`). Tools registered during startup or mid-session are immediately available in `pi.getAllTools()`, `pi.getActiveTools()`, and callable by the LLM.
- **Dynamic Tool Enabling**: When a tool execution modifies active tools via `runner.setActiveTools()`, `wrapRegisteredTool()` (`packages/coding-agent/src/core/extensions/wrapper.ts:18–35`) attaches `addedToolNames` to the result, signaling to the agent loop that new tools were activated.
- **Custom UI Rendering**: `ToolDefinition` supports custom TUI component renderers (`renderCall`, `renderResult`, `renderShell: "default" | "self"`).
- **SDK Overrides**: Callers of `createAgentSession()` can provide `customTools: ToolDefinition[]` or `baseToolsOverride: Record<string, AgentTool>` (`packages/coding-agent/src/core/sdk.ts:180–240`).

---

## Start Here
Open `packages/coding-agent/src/core/tools/index.ts` (lines 1–225) to see the built-in tool definitions, followed by `packages/agent/src/agent-loop.ts` (lines 135–730) to trace how tool calls are parsed, validated, dispatched, and finalized.

---

## Gaps
1. **`fd` / `rg` Binary Resolution**: In `find.ts` (line 11) and `grep.ts` (line 11), `ensureTool("fd")` and `ensureTool("rg")` manage local binary availability; the exact caching and binary distribution mechanics inside `tools-manager.ts` were not traced in depth.
2. **Provider-Specific Tool Payload Formats**: While the TypeBox schema conversion entry points in `packages/ai/src/api/*` were verified, provider-specific nuances (e.g. OpenAI strict schema flags vs Anthropic JSON schema conversions) were not evaluated beyond tool dispatch.

### Subagent result 6 · source line 39 · `d39564ba` · active branch

Agent: `explore`  
Model: `google/gemini-3.7-flash`  
Task: Research current OpenAI Codex under .research/codex/source. Read-only. Focus on the Rust CLI/core implementation. Determine its default coding-agent tool set and how each tool is defined, assembled/registered/exposed to the model, dispatched, sandboxed, and implemented. Trace the full tool-call lifecycle, including system/developer prompt guidance, async semantics, multiple tool calls in one model turn, concurrency versus sequential execution, result collection/order, error handling, cancellation, and tests/docs. Fan out through callers/callees but distinguish default tools from optional/experimental tools. Return concise, neutral findings with exact repository-relative file paths and 1-based line numbers for every material claim. Flag ambiguities or behaviour inferred rather than tested. Do not recommend changes to kiki.

## Scope and Interpretation
This investigation examines the Rust CLI/core implementation of OpenAI Codex in `.research/codex/source`. It analyses how Codex defines its toolset, exposes tools to models, orchestrates execution (sandboxing, approvals, concurrency, async streaming), and handles errors/cancellation.

---

## Files Retrieved
1. `/Users/joe/kiki/.research/codex/source/codex-rs/core/src/tools/spec_plan.rs` (lines 120–497, 997–1331) — Assembles turn-level tool plans, enforces exposure policy, resolves collisions, and configures Code Mode / Tool Search.
2. `/Users/joe/kiki/.research/codex/source/codex-rs/core/src/tools/router.rs` (lines 1–250) — Owns runtime tool lookup, wire-payload parsing (`ToolCall`), namespace resolution, and parallel tool dispatch query.
3. `/Users/joe/kiki/.research/codex/source/codex-rs/core/src/tools/parallel.rs` (lines 45–220) — Implements `ToolCallRuntime` with an `Arc<RwLock<()>>` execution gate governing parallel vs sequential tool calls, timing telemetry, and cancellation interception.
4. `/Users/joe/kiki/.research/codex/source/codex-rs/core/src/tools/orchestrator.rs` (lines 40–460) — Drives the approval, sandbox selection, execution attempt, and sandbox-denial escalation/retry loop.
5. `/Users/joe/kiki/.research/codex/source/codex-rs/core/src/tools/sandboxing.rs` (lines 140–390) — Defines `Approvable`, `Sandboxable`, `ToolRuntime`, and `SandboxAttempt` interfaces, plus default approval rules.
6. `/Users/joe/kiki/.research/codex/source/codex-rs/core/src/stream_events_utils.rs` (lines 280–350) — Converts model streaming `OutputItemDone` events into immediate asynchronous `handle_tool_call` tasks.
7. `/Users/joe/kiki/.research/codex/source/codex-rs/core/src/session/turn.rs` (lines 2150–2820) — Drives the model streaming loop, accumulates tool futures into `FuturesOrdered`, and drains completed results in order.
8. `/Users/joe/kiki/.research/codex/source/codex-rs/core/src/context/world_state/tools.rs` (lines 1–110) — Renders deferred tool namespace diffs into `<tools>` system/developer prompt XML tags.
9. `/Users/joe/kiki/.research/codex/source/codex-rs/tools/src/tool_executor.rs` (lines 10–137) — Defines the core `ToolExecutor` trait, `ToolExposure` enum, and bitflag surfaces (`Direct`, `Deferred`, `CodeMode`).
10. `/Users/joe/kiki/.research/codex/source/codex-rs/tools/src/function_call_error.rs` (lines 1–10) — Defines `FunctionCallError` (`RespondToModel` vs `Fatal`).
11. `/Users/joe/kiki/.research/codex/source/codex-rs/features/src/lib.rs` (lines 870–1620) — Built-in feature definitions and their default enabled/disabled states.
12. `/Users/joe/kiki/.research/codex/source/codex-rs/sandboxing/src/lib.rs` and `manager.rs` (lines 1–150) — Platform-level sandboxing (macOS Seatbelt, Linux Landlock/Seccomp/bwrap, Windows RestrictedToken/AppContainer).

---

## Key Code

### 1. Concurrency vs Sequential Lock Gate
From `/Users/joe/kiki/.research/codex/source/codex-rs/core/src/tools/parallel.rs` (lines 154–173):
```rust
let _guard = if supports_parallel {
    Either::Left(lock.read().await)
} else {
    Either::Right(lock.write().await)
};
// Admission through the parallel-execution gate marks the end
// of dispatch waiting and the start of handler execution.
if let Some(execution_started_at) = execution_started_at {
    let _ = execution_started_at.set(Instant::now());
}

router
    .dispatch_tool_call_with_terminal_outcome(
        session,
        step_context,
        invocation_cancellation_token,
        tracker,
        dispatch_call,
        source,
        dispatch_terminal_outcome_reached,
    )
    .instrument(dispatch_span.clone())
    .await
```

### 2. Stream-Time Dispatch and Ordered Drain
From `/Users/joe/kiki/.research/codex/source/codex-rs/core/src/stream_events_utils.rs` (lines 318–328):
```rust
record_completed_response_item(ctx.sess.as_ref(), ctx.turn_context.as_ref(), &item)
    .await;

let cancellation_token = ctx.cancellation_token.child_token();
let tool_future: InFlightFuture<'static> = Box::pin(
    ctx.tool_runtime
        .clone()
        .handle_tool_call(call, cancellation_token),
);

output.needs_follow_up = true;
output.tool_future = Some(tool_future);
```

From `/Users/joe/kiki/.research/codex/source/codex-rs/core/src/session/turn.rs` (lines 2158–2174):
```rust
async fn drain_in_flight(
    in_flight: &mut FuturesOrdered<InFlightFuture<'static>>,
    sess: Arc<Session>,
    turn_context: Arc<TurnContext>,
) -> CodexResult<()> {
    while let Some(res) = in_flight.next().await {
        match res {
            Ok(envelope) => {
                mark_thread_memory_mode_polluted_if_external_context(
                    sess.as_ref(),
                    turn_context.as_ref(),
                    &envelope.item,
                )
                .await;
                sess.record_annotated_conversation_items(&turn_context, vec![envelope])
                    .await;
            }
            Err(err) => {
                error_or_panic(format!("in-flight tool future failed during drain: {err}"));
            }
        }
    }
    Ok(())
}
```

### 3. Tool Error Contract
From `/Users/joe/kiki/.research/codex/source/codex-rs/tools/src/function_call_error.rs` (lines 3–9):
```rust
#[derive(Debug, Error, PartialEq)]
pub enum FunctionCallError {
    #[error("{0}")]
    RespondToModel(String),
    #[error("Fatal error: {0}")]
    Fatal(String),
}
```

---

## Architecture

### 1. Default vs Optional / Experimental Tool Inventory

#### Default Coding-Agent Tools
- **`exec_command`** (`ExecCommandHandler` in `codex-rs/core/src/tools/handlers/unified_exec/exec_command.rs:120`): Runs commands in a PTY/process session. Enabled by default (`Feature::ShellTool` is stable and default-enabled at `features/src/lib.rs:886`; `Feature::UnifiedExec` is stable and default-enabled at `features/src/lib.rs:910`). `supports_parallel_tool_calls = true`.
- **`write_stdin`** (`WriteStdinHandler` in `codex-rs/core/src/tools/handlers/unified_exec/write_stdin.rs:40`): Writes characters/interrupts (`\x03`) to an active PTY session. Registered alongside `exec_command` under unified exec (`spec_plan.rs:1092`). `supports_parallel_tool_calls = true`.
- **`apply_patch`** (`ApplyPatchHandler` in `codex-rs/core/src/tools/handlers/apply_patch.rs:75`): Applies freeform unified diffs/file edits using Lark grammar (`apply_patch.lark`). Registered whenever `model_info.apply_patch_tool_type.is_some()` (`spec_plan.rs:1202`). `supports_parallel_tool_calls = false` (runs sequentially under the write lock).
- **`view_image`** (`ViewImageHandler` in `codex-rs/core/src/tools/handlers/view_image.rs:75`): Reads and downsamples images from disk into data URLs. Enabled by default (`Feature::ViewImage` default-enabled at `features/src/lib.rs:892`). `supports_parallel_tool_calls = true`.
- **`request_user_input`** (`RequestUserInputHandler` in `codex-rs/core/src/tools/handlers/request_user_input.rs:60`): Prompts user with 1–3 structured multiple-choice questions. Defaults to enabled (`config/mod.rs:2631` via `tools.experimental_request_user_input`). Exposure is `DirectModelOnly`. `supports_parallel_tool_calls = false`.
- **Collaboration / Multi-Agent V1** (`multi_agents/` in `codex-rs/core/src/tools/handlers/multi_agents/`): `collab.spawn_agent`, `collab.send_input`, `collab.resume_agent`, `collab.wait_agent`, `collab.close_agent`. Active by default (`Feature::Collab` is stable and default-enabled at `features/src/lib.rs:1210`). `supports_parallel_tool_calls = false`.

#### Optional, Config-Gated, or Experimental Tools
- **`update_plan`** (`PlanHandler` in `codex-rs/core/src/tools/handlers/plan.rs`): Gated by `[tools.update_plan] enabled = true` (default `false`, `config/mod.rs:2639`).
- **`tool_search`** (`ToolSearchHandler` in `codex-rs/core/src/tools/handlers/tool_search.rs`): Automatically added when deferred tools are present and the provider/model supports namespaces + search (`spec_plan.rs:372`).
- **`code_mode` & `wait`** (`CodeModeExecuteHandler` & `CodeModeWaitHandler` in `codex-rs/core/src/tools/code_mode/`): Activated when `ToolMode` is `CodeMode` or `CodeModeOnly` (`Feature::CodeMode` / `Feature::CodeModeOnly`, default `false`).
- **MCP Resource Tools** (`list_mcp_resources`, `list_mcp_resource_templates`, `read_mcp_resource` in `codex-rs/core/src/tools/handlers/mcp_resource/`): Added only when active MCP servers are connected (`spec_plan.rs:1125`).
- **MCP Dynamic Tools** (`McpHandler` in `codex-rs/core/src/tools/handlers/mcp.rs`): Dynamically mapped from external MCP server tool lists.
- **`request_permissions`** (`RequestPermissionsHandler` in `codex-rs/core/src/tools/handlers/request_permissions.rs`): Gated by `Feature::RequestPermissionsTool` (default `false`, `features/src/lib.rs:1132`).
- **`get_context_remaining` & `new_context_window`** (`codex-rs/core/src/tools/handlers/get_context_remaining.rs` & `new_context_window.rs`): Gated by `Feature::TokenBudget` (default `false`, `features/src/lib.rs:1532`).
- **`current_time`** (`CurrentTimeHandler` in `codex-rs/core/src/tools/handlers/current_time.rs`): Gated by `Feature::CurrentTimeReminder` or model capability `clock` (`spec_plan.rs:1175`).
- **`sleep`** (`SleepHandler` in `codex-rs/core/src/tools/handlers/sleep.rs`): Gated by `Feature::SleepTool` and sleep mode settings (`spec_plan.rs:1181`).
- **Multi-Agent V2** (`multi_agents_v2/`): Gated by `Feature::MultiAgentV2` (default `false`, `features/src/lib.rs:1216`).

---

### 2. Tool Assembly, Registration, and Exposure
1. **Per-Turn Assembly (`spec_plan.rs:125` `build_tool_router`)**:
   - `CoreToolPlanContext` is created from the current turn snapshot, models, features, environments, and MCP bindings.
   - Handlers are added to a `ToolRegistry` with designated `ToolExposure` (`Direct`, `Deferred`, `DirectModelOnly`, `CodeModeOnly`, `Hidden`).
   - MCP tools are evaluated against server configuration `omit_tools_from` (`spec_plan.rs:184`). If `tool_search` is enabled, deferred MCP tools have their direct exposure removed and become discoverable via search only.
2. **Schema Deduplication & Collision Enforcement (`spec_plan.rs:356` `finalize_tool_router`)**:
   - Multiple namespaces are merged (`merge_into_namespaces` at `spec_plan.rs:900`).
   - If `error_on_tool_collisions` is enabled, naming collisions between built-in tools, MCP servers, and dynamic tools fail the turn immediately (`spec_plan.rs:411`).
3. **Model Visibility Projection (`spec_plan.rs:525` `build_model_visible_specs`)**:
   - Iterates through registry entries with direct exposure.
   - Serialises schemas into `codex_tools::ToolSpec` (`Function`, `Freeform`, `Namespace`, or `ToolSearch`).
   - Passes final tool definitions to the OpenAI Responses API payload (`codex-rs/tools/src/responses_api.rs:157`).

---

### 3. Prompt Guidance & Context Injection
- **World State Sections (`codex-rs/core/src/session/world_state.rs:35`)**:
  - `ModelInstructionsState`: Injects system base instructions (pruned of plan instructions if `update_plan_enabled` is false, `world_state.rs:47`).
  - `ToolsState` (`context/world_state/tools.rs:38`): Injects deferred tool namespaces into the developer prompt inside `<tools>` and `</tools>` XML tags.
  - `AgentsMdState` (`context/world_state/agents_md.rs`): Injects repo `AGENTS.md` into the prompt.
  - `PermissionsState` / `CompactPermissionsState` (`context/world_state/permissions.rs`): Injects allowed and denied filesystem/network rules.

---

### 4. Dispatch, Execution, Concurrency, and Sandboxing
- **Stream Event Trigger (`stream_events_utils.rs:289` `handle_output_item_done`)**:
  - When the SSE stream emits `ResponseEvent::OutputItemDone(ResponseItem::FunctionCall | ResponseItem::CustomToolCall)`, the item is logged and persisted to conversation history.
  - An async task is spawned immediately via `ctx.tool_runtime.handle_tool_call(...)`.
  - The returned future is appended to an in-memory `FuturesOrdered<InFlightFuture<'static>>` (`turn.rs:2437`).
- **Parallel vs Sequential Concurrency (`parallel.rs:155`)**:
  - `ToolCallRuntime` checks `router.tool_supports_parallel(&call)`.
  - Tools returning `true` (`exec_command`, `write_stdin`, `view_image`, `tool_search`, read-only `mcp`) acquire `parallel_execution.read().await` — running **concurrently**.
  - Tools returning `false` (`apply_patch`, `request_user_input`, `plan`, `collab.*`) acquire `parallel_execution.write().await` — running **sequentially**, blocking other tools until finished.
- **Sandboxing & Escalation Loop (`orchestrator.rs:127` `ToolOrchestrator::run`)**:
  1. **Approval check**: Queries `ApprovalStore` for cached approvals or invokes `session.request_approval(...)` based on `AskForApproval` policy (`Never`, `OnRequest`, `UnlessTrusted`, `Granular`).
  2. **Sandbox Selection**: `SandboxManager` selects initial sandbox backend:
     - macOS: Apple Seatbelt (`codex-rs/sandboxing/src/seatbelt.rs`).
     - Linux: Landlock + Seccomp + Namespaces (`codex-rs/sandboxing/src/landlock.rs` & `codex-rs/bwrap`).
     - Windows: Restricted Token (`WindowsRestrictedToken`, `codex-rs/sandboxing/src/windows.rs`).
     - Network: MITM Proxy (`codex-rs/network-proxy`).
  3. **First Attempt**: `tool.run(attempt, tool_ctx)` executes inside the sandbox.
  4. **Denial Escalation / Retry**: If the sandbox denies access (`SandboxErr::Denied`), and unsandboxed execution is allowed by policy, Codex requests user approval to escalate and retries without sandbox or with an elevated policy.
- **Ordered Result Collection (`turn.rs:2158` `drain_in_flight`)**:
  - Once the model completes its sampling turn (`ResponseEvent::Completed`), `drain_in_flight` awaits the `FuturesOrdered` queue.
  - Tool outputs are recorded into conversation history in the exact order emitted by the model, even if background execution finished out of order.

---

### 5. Error Handling and Cancellation
- **Tool Error Handling (`parallel.rs:78`, `stream_events_utils.rs:340`)**:
  - If a tool returns `FunctionCallError::RespondToModel(msg)`, it is mapped to a `FunctionCallOutputPayload` with `success: Some(false)` and `body: Text(msg)`. The error message is passed into conversation history for the next model turn.
  - If a tool returns `FunctionCallError::Fatal(msg)`, it propagates as `CodexErr::Fatal` and terminates the turn.
- **Cancellation (`parallel.rs:177`, `oneshot.rs:59`)**:
  - Each invocation listens to `tokio_util::sync::CancellationToken`.
  - When cancelled during sampling or tool execution, running tasks are aborted (`dispatch_handle.abort()`).
  - Running processes in `unified_exec` are sent SIGTERM / SIGKILL via `process.terminate_confirmed().await`.
  - `notify_tool_aborted` emits an aborted event, and an aborted payload (e.g. `"Wall time: Xs\naborted by user"`) is recorded.

---

## Start Here
To trace the tool execution system from start to finish, open:
1. `/Users/joe/kiki/.research/codex/source/codex-rs/core/src/tools/spec_plan.rs` — See how the tool router is constructed, filtered, and exposed to the model.
2. `/Users/joe/kiki/.research/codex/source/codex-rs/core/src/stream_events_utils.rs` (around line 289) — Follow the entry point where model streaming output items are converted into tool calls.
3. `/Users/joe/kiki/.research/codex/source/codex-rs/core/src/tools/parallel.rs` (around line 90) — See the concurrent/sequential lock gate and background task spawning.
4. `/Users/joe/kiki/.research/codex/source/codex-rs/core/src/tools/orchestrator.rs` (around line 127) — See the approval and sandbox execution loop.

---

## Gaps
1. **Remote Executor Shell Resolution**: In `core/src/tools/handlers/unified_exec/exec_command.rs:248`, resolving custom shells on remote execution environments contains an explicit TODO (`anp: Resolve requested shells in remote environments`). Remote environments currently fall back to their reported default shell.
2. **Dynamic / Enterprise Requirement Delivery**: The exact network payload schema for cloud-delivered managed requirement configs (`delivered_requirements_toml.rs`) was not executed against a live backend; findings are derived strictly from static code analysis.

### Subagent result 7 · source line 45 · `29ed69b8` · active branch

Agent: `explore`  
Model: `google/gemini-3.7-flash`  
Task: Inspect the current kiki repository read-only, focusing on src/arena.rs, Monty integration, src/tools.rs, the agent loop, OpenAI tool/result handling, and prompts. Trace end-to-end how Python code executed in the arena sees Rust tools, whether generated functions are async, whether Python is explicitly instructed to await them and gather independent calls, how Monty yields pending calls, how multiple calls cross into Rust, whether Rust executes them concurrently, whether results preserve call order, and how errors/cancellation work. Find relevant tests and run no commands that alter files. Compare observed behaviour only against these concrete requested properties: async functions in the arena, prompt guidance to await/gather, batch passage to Rust, concurrent Rust handling. Return neutral, evidence-backed findings with exact repository-relative paths and 1-based lines. Clearly label each property as satisfied, partial, absent, or unverified. Do not propose the future default tool set.

## Files Retrieved

1. `/Users/joe/kiki/prompts/read_only_agent.md` (lines 1–22) — Bundled system prompt defining the agent's instructions, persistent arena semantics, and `await read(path)` requirement.
2. `/Users/joe/kiki/src/prompt.rs` (lines 80–125) — System prompt resolver, override loader, and concept test suite asserting bundled prompt terms.
3. `/Users/joe/kiki/src/tools.rs` (lines 60–160) — `ReadTool` filesystem implementation with cap-std sandboxing, path validation, limits, and blocking reads.
4. `/Users/joe/kiki/src/arena.rs` (lines 265–295, 450–575, 635–690, 725–910, 1705–1760, 2515–2690) — Monty REPL integration, `drive` loop, name lookup, future yielding, single read suspension, crash/cancellation recovery, and interruption tests.
5. `/Users/joe/kiki/src/agent.rs` (lines 480–675, 685–750) — Agent step loop coordinating the model and arena, executing Python, handling errors/cancellations, and constructing feedback.
6. `/Users/joe/kiki/src/model.rs` (lines 85–130, 420–440) — Model traits, history structures, capability description string, and `CodeResult` types.
7. `/Users/joe/kiki/src/openai.rs` (lines 70–160, 740–775) — OpenAI Responses API client, single `execute_python` tool configuration (`parallel_tool_calls: false`), and replay validation.

---

## Evaluation of Concrete Requested Properties

| Property | Status | Evidence Summary |
| :--- | :--- | :--- |
| **Async functions in the arena** | **Satisfied** | `src/arena.rs:792-808, 865-877`. `read` is provided via `TurnEvent::NameLookup` and when invoked, Rust resumes Monty with `ResumeValue::Future`, creating an awaitable Python coroutine. |
| **Prompt guidance to await/gather** | **Partial** | `prompts/read_only_agent.md:5`. Explicitly instructs `await read(path)` at top level, but contains **no** mention of `gather`, `asyncio.gather`, or gathering independent calls. |
| **Batch passage to Rust** | **Absent** | `src/arena.rs:268-270, 798-812, 889-893`. Monty suspension expects exactly one pending `call_id` matching the immediate call; multiple call IDs trigger fatal `UnsupportedSuspension` errors. |
| **Concurrent Rust handling** | **Absent** | `src/arena.rs:815-839`. Rust invokes `read_tool.read(&path).await` sequentially inside the turn loop and immediately resumes Monty with `resume_futures` for that single call. |

---

## Key Code

### 1. Host Function Exposure and Async Future Suspension
From `src/arena.rs` (lines 792–842, 865–877):

```rust
                TurnEvent::FunctionCall {
                    function_name,
                    args,
                    kwargs,
                    call_id,
                    method_call,
                } if function_name == "read" => {
                    if !kwargs.is_empty()
                        || method_call
                        || !matches!(args.as_slice(), [MontyObject::String(_)])
                    {
                        match with_print!(on_print => checkout
                            .resume(
                                ResumeValue::Error(MontyException::new(
                                    ExcType::TypeError,
                                    Some(
                                        "read() expects one positional string argument".to_owned(),
                                    ),
                                )),
                                &mut on_print,
                            )
                            .await)
                        {
                            Ok(event) => event,
                            Err(PoolError::Runtime(error)) => {
                                break 'drive Err(DriverError::Runtime(error));
                            }
                            Err(error) => {
                                break 'drive Err(DriverError::Fatal(pool_failure(error)));
                            }
                        }
                    } else {
                        let [MontyObject::String(path)] = args.as_slice() else {
                            unreachable!("read argument was validated above");
                        };
                        let pending = match with_print!(on_print => checkout
                            .resume(ResumeValue::Future, &mut on_print)
                            .await)
                        {
                            Ok(TurnEvent::ResolveFutures { pending_call_ids })
                                if pending_call_ids.as_slice() == [call_id] =>
                            {
                                pending_call_ids
                            }
                            Ok(event) => {
                                break 'drive Err(DriverError::Fatal(
                                    ArenaError::unsupported_suspension(format!(
                                        "expected one pending read future, got {event:?}"
                                    )),
                                ));
                            }
                            Err(PoolError::Runtime(error)) => {
                                break 'drive Err(DriverError::Runtime(error));
                            }
                            Err(error) => {
                                break 'drive Err(DriverError::Fatal(pool_failure(error)));
                            }
                        };
                        let path = path.clone();
                        read.started(&path);
                        progress(ArenaProgress::ReadStarted { path: path.clone() });
                        let read_result = read_tool.read(&path).await;
                        let outcome = match &read_result {
                            Ok(lines) => ReadOutcome::Success {
                                line_count: lines.len(),
                            },
                            Err(error) => ReadOutcome::failure(error.to_string()),
                        };
                        read.finished(outcome.clone());
                        #[cfg(test)]
                        read_tool.wait_after_read_settled().await;
                        progress(ArenaProgress::ReadFinished { path, outcome });
                        let value = match read_result {
                            Ok(lines) => ResumeValue::Return(MontyObject::List(
                                lines.into_iter().map(MontyObject::String).collect(),
                            )),
                            Err(error) => ResumeValue::Error(read_error(error)),
                        };
                        match with_print!(on_print => checkout
                            .resume_futures(vec![(pending[0], value)], &mut on_print)
                            .await)
```

```rust
                TurnEvent::NameLookup { name } => {
                    let value = (name == "read").then(|| MontyObject::Function {
                        name: "read".to_owned(),
                        docstring: None,
                    });
                    match with_print!(on_print => checkout
                        .resume_name_lookup(value, &mut on_print)
                        .await)
```

### 2. Single Pending Call Enforcement
From `src/arena.rs` (lines 268–272, 889–893):

```rust
impl InterruptedReadState {
    fn started(&mut self, path: &str) {
        assert!(self.pending.is_none(), "only one read can be pending");
        self.pending = Some(bounded_interruption_text(path.to_owned()));
        self.read_calls.push(ReadCall::Pending);
    }
```

```rust
                TurnEvent::ResolveFutures { pending_call_ids } => {
                    break 'drive Err(DriverError::Fatal(ArenaError::unsupported_suspension(
                        format!("unexpected pending futures: {pending_call_ids:?}"),
                    )));
                }
```

### 3. Prompt Guidance
From `prompts/read_only_agent.md` (lines 5–7) and `src/model.rs` (line 430):

```markdown
`read(path)` is asynchronous and is the only host capability. Call it with `await read(path)` at the top level. It returns an accepted UTF-8 file as a list of exact lines with their line endings retained. Host access is read-only.
```

```rust
pub const READ_CAPABILITY_DESCRIPTION: &str = "read(path) returns the accepted file as complete exact lines with endings retained. Use standard Python print(\"\".join(lines[start:end]), end=\"\") to inspect a selection.";
```

### 4. OpenAI Responses API Tool Configuration
From `src/openai.rs` (lines 135, 142–155):

```rust
            "parallel_tool_calls": false,
            "reasoning": {
                "effort": "medium",
                "summary": "auto",
                "context": "all_turns"
            },
            "include": ["reasoning.encrypted_content"],
            "tools": [{
                "type": "function",
                "name": "execute_python",
                "strict": true,
                "parameters": {
                    "type": "object",
                    "properties": {
                        "code": {"type": "string"}
                    },
                    "required": ["code"],
                    "additionalProperties": false
                }
            }]
```

---

## End-to-End Trace

### 1. How Python Code in the Arena Sees Rust Tools
- The OpenAI model is given only one tool definition: `execute_python(code)` (`src/openai.rs:142-156`).
- When `execute_python` runs, Monty evaluates Python code in the persistent REPL environment (`src/arena.rs:740-745`).
- When Python references `read`, Monty yields `TurnEvent::NameLookup { name: "read" }` (`src/arena.rs:865-877`). Rust responds via `checkout.resume_name_lookup` with `MontyObject::Function { name: "read", docstring: None }`. Any lookup for another name returns `None`, producing Python `NameError` (`src/arena.rs:1758-1768`).

### 2. Async Nature of Arena Functions
- When Python calls `read(path)`, Monty yields `TurnEvent::FunctionCall` (`src/arena.rs:747-753`).
- Rust validates arguments and responds with `ResumeValue::Future` (`src/arena.rs:792-794`).
- This marks `read(...)` as an asynchronous call yielding a coroutine/future inside Python that must be evaluated with `await`.

### 3. Prompt Guidance to Await / Gather
- **Await:** `prompts/read_only_agent.md:5` states: "`read(path)` is asynchronous and is the only host capability. Call it with `await read(path)` at the top level."
- **Gather:** There is **no** instruction or mention of `gather`, `asyncio.gather`, or concurrent tool calls anywhere in `prompts/read_only_agent.md` or `src/model.rs:430`.

### 4. How Monty Yields Pending Calls & Crosses into Rust
- In response to `ResumeValue::Future`, Monty yields `TurnEvent::ResolveFutures { pending_call_ids }` (`src/arena.rs:798-812`).
- Rust enforces that `pending_call_ids.as_slice() == [call_id]` (matching the single call ID).
- If multiple calls or an unassociated `ResolveFutures` event is yielded, Rust returns fatal `ArenaError::UnsupportedSuspension` (`src/arena.rs:804-809, 889-893`).

### 5. Multi-Call Execution & Concurrency
- Multiple simultaneous calls across the host boundary are **not** supported by the arena driver.
- Rust executes the requested tool sequentially on a worker thread via `ReadTool::read` (`src/arena.rs:820`, `src/tools.rs:94-106`).
- Once finished, Rust immediately resumes Monty with `checkout.resume_futures(vec![(pending[0], value)])` (`src/arena.rs:838-842`).

### 6. Preservation of Call Order
- Because calls are evaluated strictly sequentially, each read finishes before Python resumes execution.
- Call history is recorded sequentially in `read_calls: Vec<ReadCall>` on `Execution` and `InterruptedExecution` (`src/arena.rs:84, 139, 266-296`).
- Test `src/arena.rs:2519-2536` confirms sequential traces preserve invocation order across successes and failures.

### 7. Errors and Cancellation
- **Tool / Validation Errors:** `ReadError` variants (e.g. `AbsolutePath`, `LineLimitExceeded`, `InvalidUtf8`) are converted by `read_error()` (`src/arena.rs:898-908`) into `MontyException` (`ValueError`, `OSError`, `RuntimeError`) and resumed into Monty as `ResumeValue::Error`. Python can catch or raise them (`src/arena.rs:1728-1756`).
- **Python Errors & Commit Semantics:** Normal Python exceptions commit preceding mutations (`src/arena.rs:662-677, 2324-2345`). The exception string is captured and sent back to the model in `CodeResult::execution(...)` (`src/agent.rs:613-639`).
- **Unsafe Memory Failures:** Memory exhaustion (`is_unsafe_memory_error`, `src/arena.rs:678-692`) is treated as fatal infrastructure failure, triggering worker recovery.
- **Cancellation & Timeouts:** `Arena::execute_with_progress` runs under a `tokio::select!` with `cancellation.cancelled()` and `self.execution_timeout` (`src/arena.rs:460-475`).
  - Upon cancellation/timeout, the `checkout` is dropped (killing the Monty subprocess worker), and `Arena::recover` checks out a fresh worker and restores the last clean idle checkpoint (`src/arena.rs:498-504, 547-571`).
  - Interrupted stdout, stderr, and pending/completed read call evidence are preserved (`src/arena.rs:244-263`, `src/agent.rs:687-735`).
  - If checkpoint recovery itself fails, `ArenaError::RecoveryFailed` is raised and disables subsequent agent submissions (`src/runtime.rs:104-110`).

---

## Relevant Tests

1. `src/arena.rs:1709-1726` (`read_returns_exact_line_endings`) — Verifies top-level `await read('lines.txt')` returns exact lines.
2. `src/arena.rs:1728-1744` (`malformed_read_is_a_model_visible_python_error`) — Verifies invalid arguments to `await read(...)` yield `TypeError`.
3. `src/arena.rs:1746-1756` (`read_failures_are_model_visible_python_errors`) — Verifies invalid path yields `ValueError`.
4. `src/arena.rs:1758-1768` (`unknown_host_call_is_a_model_visible_python_error`) — Verifies unknown host functions yield `NameError`.
5. `src/arena.rs:2324-2345` (`ordinary_python_errors_commit_preceding_mutation`) — Proves that Python errors retain mutated state in the arena checkpoint.
6. `src/arena.rs:2519-2536` (`read_call_trace_keeps_ordered_successes_and_failures`) — Verifies ordered tracking in `read_calls`.
7. `src/arena.rs:2538-2556` (`read_call_trace_keeps_pending_calls_when_cancellation_interrupts_a_read`) — Verifies pending read state is captured when cancelled mid-read.
8. `src/arena.rs:2760-2784` (`cancellation_recovers_the_trusted_checkpoint_before_returning`) — Verifies cancellation discards uncommitted state and restores the idle checkpoint.
9. `src/prompt.rs:90-110` (`bundled_prompt_covers_only_the_approved_read_only_agent_behaviour`) — Verifies prompt contents contain required concepts (`"asynchronous"`, `"await read(path)"`, `"top level"`).
10. `src/agent.rs:1855-1943` (`agent_cancellation_recovers_the_arena_without_consuming_later_actions`) — Verifies agent-level cancellation recovery.

---

## Architecture

```
OpenAI Responses API (gpt-5.6-sol/default)
  │   ▲
  │   │  Single Tool: execute_python(code)
  │   │  Result: function_call_output (stdout, stderr, expression / Python error)
  ▼   │
Agent Loop (src/agent.rs)
  │   ▲
  │   │  Executes Python code in Monty arena
  ▼   │
Arena Driver (src/arena.rs)
  │   ▲
  │   │  TurnEvent::NameLookup -> MontyObject::Function("read")
  │   │  TurnEvent::FunctionCall("read", [path]) -> ResumeValue::Future
  │   │  TurnEvent::ResolveFutures([call_id]) -> ResumeValue::Return(lines)
  ▼   │
Read Tool (src/tools.rs)
      Cap-std workspace-relative non-blocking file read
```

---

## Start Here

To inspect the Monty tool suspension and execution event loop:
- Open `/Users/joe/kiki/src/arena.rs` at line **725** (`async fn drive`). This contains the complete state machine handling name resolution, async future generation, read execution, future resolution, and rejection of unsupported multi-call suspensions.

---

## Gaps

- None. All requested components (`src/arena.rs`, Monty integration, `src/tools.rs`, `src/agent.rs`, `src/openai.rs`, `src/prompt.rs`, and tests) were directly inspected in the repository.

### Subagent result 8 · source line 144 · `659c0113` · active branch

Agent: `explore`  
Model: `google/gemini-3.7-flash`  
Task: Read docs/research/2026-09-01_agent-tool-sets-and-arena-execution.md and validate it against the linked Maki, Pi, Codex, Kiki, and local Monty 0.0.21 source. Read-only. Check specifically: default-tool claims, direct-call concurrency/order, Maki code_execution batching, Kiki's current single-future limitation, the distinction between OpenAI outer parallel_tool_calls and nested arena concurrency, and whether each requested topic is covered neutrally. Report only concrete factual errors, misleading wording, broken line references, or material omissions, with exact corrected evidence. If none, say none.

## Validation Summary

Validated `docs/research/2026-09-01_agent-tool-sets-and-arena-execution.md` against the source code of Maki (`e0d87e35`), Pi (`587be985`), Codex (`2b7c2797`), Kiki, and local Monty 0.0.21 (`~/.cargo/registry/src/.../monty-0.0.21` and `monty-pool-0.0.21`):

- **Default-tool claims**: Verified. Maki enables 19 bundled plugins (18 primary model tools + `sessions` commands `/sessions` and `/rename`), with `multiedit` enabled by default. Pi defines 8 built-ins, exposing `{read, bash, edit, write}` for standard coding and `{read, grep, find, ls}` for read-only. Codex constructs dynamic per-turn tool registries from environment, feature flags, and model metadata.
- **Direct-call concurrency & ordering**: Verified. Maki spawns `smol` tasks and awaits in source order; Pi executes concurrently via `Promise.all` while serialising per-file mutations and preserving source result order; Codex handles streamed calls via `FuturesOrdered` with an async read/write lock gating parallel vs serial executors.
- **Maki `code_execution` batching**: Verified. Maki defines a preamble `gather` helper, instructs the model to await all tool calls and use `gather` for concurrency, records `FunctionCall` events as pending in `runner.rs`, batches requested call IDs on `ResolveFutures`, and executes them concurrently in Lua via `join_all`.
- **Kiki's single-future limitation**: Verified. `arena.rs` returns `ResumeValue::Future` for `read`, strictly matches `TurnEvent::ResolveFutures { pending_call_ids }` on `[call_id]`, awaits the single read inline, resumes with `vec![(pending[0], value)]`, treats any other suspension as fatal, and asserts single-pending invariant in `InterruptedReadState`. Monty 0.0.21 protocol itself supports multiple pending IDs and subset resolution.
- **OpenAI outer `parallel_tool_calls` vs inner arena concurrency**: Verified. `openai.rs` configures `"parallel_tool_calls": false` solely for the outer OpenAI Responses API payload (`execute_python`), whereas nested arena tool execution is governed independently by Monty's event loop in `arena.rs`.
- **Neutrality & line references**: Verified. All file paths, line numbers, and citations match exact commit targets. The document remains strictly descriptive and fact-finding without ungrounded speculation or prescriptive bias.

## Concrete Errors, Broken References, Misleading Wording, or Material Omissions

None.

### Assistant · source line 155 · `84220125` · active branch

Research: `docs/research/2026-09-01_agent-tool-sets-and-arena-execution.md`

Key finding: Kiki’s `read` is async, but batching, `gather` guidance, and concurrent Rust execution are absent. Monty already supports them. Maki provides the closest working reference.

**How should models access Kiki’s default tools?**

**A.** Only through `execute_python` as async arena functions  
_(recommended)_ Preserves Kiki’s code-arena-first identity and makes `await gather(...)` the composition mechanism.

**B.** Both direct model tools and arena functions  
More flexible, but duplicates schemas, prompts, dispatch paths, and semantics.

**C.** Primarily direct model tools  
Closer to Pi/Codex, but weakens the arena-first design.

Which exposure model should the spec use?

### User · source line 156 · `edca05dd` · active branch

A

### Assistant · source line 165 · `46b62d5e` · active branch

**What should the first self-hosting default tool set include?**

`docs/VISION.md` lists nine tools, but also says full subagents are not required for the first complete loop.

**A.** Seven local coding tools  
`read`, `write`, `edit`, `list`, `glob`, `grep`, `run`. Defer skills and subagents.

**B.** Local coding tools plus `load_skill`  
_(recommended)_ Supports inspect, modify, verify, and progressive skill loading. Plan `subagent` separately because it requires another complete agent, arena, and model loop.

**C.** All nine Vision tools  
Include `subagent` now. Complete, but much broader and riskier.

Which scope should we use?

### User · source line 166 · `f264080b` · active branch

A, and let's make sure to pay attention to each interface so that it's minimal and useful and the agent can make best use of it

### Subagent result 9 · source line 172 · `8a275266` · active branch

Agent: `explore`  
Model: `google/gemini-3.7-flash`  
Task: Design-It-Twice candidate 1 for Kiki's default tool module. Read CONTEXT.md, docs/VISION.md, docs/research/2026-09-01_agent-tool-sets-and-arena-execution.md, src/arena.rs, src/tools.rs, and related tests. Chosen product decisions: models access tools only as async functions inside execute_python; first self-hosting set is read, write, edit, list, glob, grep, run; load_skill and subagent are out of scope. Use the exact codebase-design vocabulary module/interface/seam/port/adapter/depth/leverage/locality and Kiki domain terms Tool/Arena. Optimise for the smallest deep interface: 1–3 Rust entry points maximum and minimal model-facing Python signatures, while retaining typed transferable results, asyncio.gather batching, concurrent Rust execution, deterministic input-associated results, cancellation, progress, and mutation safety. Return: (1) concrete Rust module interface including invariants/errors/ordering/performance, (2) exact Python signatures/result shapes for all seven tools, (3) arena-driver usage example, (4) hidden implementation, (5) dependency classification/adapters, (6) trade-offs. Read-only; no edits.

# Design-It-Twice Candidate 1: The Unified Batch Tool Engine

## Scope & Interpretation
This design represents Candidate 1 for Kiki's default tool module: the **Unified Batch Tool Engine**. It prioritises maximum **depth** and minimal surface area across the host-interpreter **seam**, exposing only three concrete Rust entry points on an inbound **port** to drive all seven self-hosting tools (`read`, `write`, `edit`, `list`, `glob`, `grep`, `run`) inside the persistent Monty **Arena**. Model-authored Python interacts with tools exclusively as top-level async functions within `execute_python`.

---

## Files Retrieved
1. `/Users/joe/kiki/CONTEXT.md` (lines 1–105) — Kiki domain terms: **Agent**, **Arena**, **Session**, **Session Entry**, **Run**, **Model Step**, **Tool**, **Settlement**.
2. `/Users/joe/kiki/docs/VISION.md` (lines 80–120) — Tool definitions for self-hosting milestone: `read`, `write`, `edit`, `list`, `glob`, `grep`, `run`.
3. `/Users/joe/kiki/docs/research/2026-09-01_agent-tool-sets-and-arena-execution.md` (lines 1–145) — Concurrency, batching, and result ordering across Maki, Pi, Codex, and Kiki's current single-read driver.
4. `/Users/joe/kiki/src/arena.rs` (lines 720–930) — Current Monty driver event loop handling `TurnEvent::FunctionCall`, `TurnEvent::NameLookup`, and `TurnEvent::ResolveFutures`.
5. `/Users/joe/kiki/src/tools.rs` (lines 105–210) — Current `ReadTool` implementation with sandboxed `cap_std::fs::Dir` traversal and byte/line limit validation.

---

## Key Code
Current single-call suspension in `src/arena.rs` (lines 793–809):
```rust
let pending = match with_print!(on_print => checkout
    .resume(ResumeValue::Future, &mut on_print)
    .await)
{
    Ok(TurnEvent::ResolveFutures { pending_call_ids })
        if pending_call_ids.as_slice() == [call_id] =>
    {
        pending_call_ids
    }
    Ok(event) => {
        break 'drive Err(DriverError::Fatal(
            ArenaError::unsupported_suspension(format!(
                "expected one pending read future, got {event:?}"
            )),
        ));
    }
    // ...
};
```

---

## 1. Concrete Rust Module Interface

The `ToolSet` **module** acts as the inbound **port** for host capabilities invoked by the **Arena** driver. It encapsulates file operations, ripgrep traversal, command execution, per-file mutation queues, and sandboxing behind 3 entry points:

```rust
pub struct ToolSet {
    workspace: Arc<cap_std::fs::Dir>,
    mutation_queue: Arc<FileMutationQueue>,
    limits: ToolLimits,
}

#[derive(Debug, Clone)]
pub struct ToolCallRequest {
    pub call_id: u32,
    pub function_name: String,
    pub args: Vec<MontyObject>,
    pub kwargs: Vec<(String, MontyObject)>,
}

#[derive(Debug)]
pub struct ToolCallResult {
    pub call_id: u32,
    pub outcome: Result<MontyObject, MontyException>,
    pub summary: ToolExecutionSummary,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ToolProgress {
    Started { call_id: u32, tool_name: String, target: String },
    Finished { call_id: u32, tool_name: String, target: String, success: bool },
}

impl ToolSet {
    /// 1. Construction and sandboxed capability binding
    pub async fn new(workspace_root: impl AsRef<Path>) -> Result<Self, ToolError>;

    /// 2. Inbound port for Python symbol discovery and docstring registration
    pub fn lookup(&self, name: &str) -> Option<MontyObject>;

    /// 3. Inbound port for concurrent, cancellation-aware batch execution
    pub async fn dispatch_batch<P>(
        &self,
        calls: Vec<ToolCallRequest>,
        cancellation: &CancellationToken,
        progress: &mut P,
    ) -> Vec<ToolCallResult>
    where
        P: FnMut(ToolProgress) + Send;
}
```

### Invariants & Guarantees
- **Sandboxing Invariant**: All filesystem operations (`read`, `write`, `edit`, `list`, `glob`, `grep`) are constrained to `workspace` using `cap-std`. Absolute paths, parent path components (`..`), and symlink escapes outside the workspace are rejected immediately with `ValueError`.
- **Mutation Safety Invariant**: Read-only tools (`read`, `list`, `glob`, `grep`) execute concurrently with all operations. Write operations (`write`, `edit`) acquire an exclusive lock on the canonical relative file path via `FileMutationQueue`. Concurrent writes to distinct files run in parallel; concurrent writes or edits to the same file are serialised deterministically.
- **Subprocess Safety**: `run` executes commands rooted in the workspace with strict timeouts and process-group isolation. Dropping or cancelling a `run` invocation sends `SIGKILL` to the entire process group.
- **Deterministic Result Association**: `dispatch_batch` maps output `ToolCallResult` records 1:1 with input `call_id` values, ensuring Monty futures resolve to their exact callers regardless of execution completion order.
- **Cancellation**: When `CancellationToken` fires, pending background tasks are aborted, active child processes are terminated, and partially acquired locks are released cleanly without corrupting the workspace.
- **Performance Characteristics**: Blocking I/O runs on Tokio's blocking threadpool (`spawn_blocking`); subcommands run via asynchronous Tokio process streams. Memory output is strictly bounded (`max_output_bytes: 256KB`, `max_file_bytes: 4MB`, `max_lines: 100_000`).

---

## 2. Python Signatures and Result Shapes

All tools are exposed in the **Arena** as asynchronous top-level callables returning typed, transferable Python structures:

### `read`
```python
async def read(path: str, offset: int = 1, limit: int | None = None) -> list[str]:
    """Read lines from a UTF-8 file in the workspace.
    
    Returns a list of lines with original line endings preserved.
    """
```
- **Return Shape**: `list[str]` (e.g. `["fn main() {\n", "}\n"]`).
- **Transferable `MontyObject`**: `MontyObject::List(Vec<MontyObject::String>)`.

### `write`
```python
async def write(path: str, content: str) -> dict[str, int]:
    """Create or overwrite a file in the workspace."""
```
- **Return Shape**: `{"bytes_written": int, "lines_written": int}`.
- **Transferable `MontyObject`**: `MontyObject::Dict(...)`.

### `edit`
```python
async def edit(path: str, old_str: str, new_str: str) -> dict[str, int]:
    """Perform an exact, atomic substring replacement in a file.
    
    Raises ValueError if old_str does not occur exactly once.
    """
```
- **Return Shape**: `{"replacements": int, "lines_delta": int}`.
- **Transferable `MontyObject`**: `MontyObject::Dict(...)`.

### `list`
```python
async def list(path: str = ".", max_entries: int = 500) -> list[dict[str, Any]]:
    """List directory entries in stable, sorted order."""
```
- **Return Shape**: `[{"name": "src", "type": "dir", "size": 128}, {"name": "Cargo.toml", "type": "file", "size": 1024}]`.
- **Transferable `MontyObject`**: `MontyObject::List(Vec<MontyObject::Dict>)`.

### `glob`
```python
async def glob(pattern: str, path: str = ".") -> list[str]:
    """Find files matching a glob pattern relative to path, ignoring .git."""
```
- **Return Shape**: `list[str]` (e.g. `["src/arena.rs", "src/tools.rs"]`).
- **Transferable `MontyObject`**: `MontyObject::List(Vec<MontyObject::String>)`.

### `grep`
```python
async def grep(
    pattern: str,
    path: str = ".",
    glob: str | None = None,
    ignore_case: bool = False,
    max_matches: int = 100,
    context: int = 0,
) -> list[dict[str, Any]]:
    """Search file contents using regex or literal text via ripgrep."""
```
- **Return Shape**: `[{"path": "src/main.rs", "line": 42, "text": "fn main() {\n", "context_before": [], "context_after": []}]`.
- **Transferable `MontyObject`**: `MontyObject::List(Vec<MontyObject::Dict>)`.

### `run`
```python
async def run(command: str, cwd: str = ".", timeout: int = 30) -> dict[str, Any]:
    """Execute a shell command inside the workspace."""
```
- **Return Shape**: `{"exit_code": int, "stdout": str, "stderr": str, "timed_out": bool}`.
- **Transferable `MontyObject`**: `MontyObject::Dict(...)`.

---

## 3. Arena-Driver Usage Example

Inside `src/arena.rs`, the driver acts as the **driving adapter**. When model-authored Python uses `asyncio.gather`, Monty emits multiple `FunctionCall` events before suspending at `ResolveFutures`. The driver accumulates calls and passes the entire batch across the **seam** to `ToolSet`:

```rust
// In src/arena.rs inside drive()
let mut pending_calls: HashMap<u32, ToolCallRequest> = HashMap::new();

loop {
    event = match event {
        TurnEvent::Complete(value) => break 'drive Ok(value),
        TurnEvent::NameLookup { name } => {
            let func_obj = tool_set.lookup(&name);
            checkout.resume_name_lookup(func_obj, &mut on_print).await?
        }
        TurnEvent::FunctionCall { function_name, args, kwargs, call_id, .. } => {
            pending_calls.insert(call_id, ToolCallRequest {
                call_id,
                function_name,
                args,
                kwargs,
            });
            // Immediately register the pending future with Monty
            checkout.resume(ResumeValue::Future, &mut on_print).await?
        }
        TurnEvent::ResolveFutures { pending_call_ids } => {
            let batch: Vec<ToolCallRequest> = pending_call_ids
                .iter()
                .filter_map(|id| pending_calls.remove(id))
                .collect();

            // Concurrent execution in Rust across all pending calls
            let results = tool_set.dispatch_batch(batch, cancellation, progress).await;

            let resume_payload: Vec<(u32, ResumeValue)> = results
                .into_iter()
                .map(|r| {
                    let val = match r.outcome {
                        Ok(obj) => ResumeValue::Return(obj),
                        Err(exc) => ResumeValue::Error(exc),
                    };
                    (r.call_id, val)
                })
                .collect();

            checkout.resume_futures(resume_payload, &mut on_print).await?
        }
        TurnEvent::OsCall { .. } => {
            checkout.resume(ResumeValue::NotHandled, &mut on_print).await?
        }
    };
}
```

---

## 4. Hidden Implementation

Behind the 3 public entry points, `ToolSet` encapsulates substantial internal machinery:

```
┌────────────────────────────────────────────────────────┐
│                        ToolSet                         │
│  new()        lookup()        dispatch_batch()         │
├────────────────────────────────────────────────────────┤
│                 FileMutationQueue                      │
│      Keyed per-path RwLock / async mutex queue         │
├────────────────────────┬───────────────────────────────┤
│    Filesystem Engine   │       Ripgrep / Process       │
│  - cap-std Dir sandbox │  - ignore / grep-searcher     │
│  - read/write/edit     │  - tokio::process::Command    │
│  - atomic file swap    │  - pgid SIGKILL on cancel     │
└────────────────────────┴───────────────────────────────┘
```

1. **`FileMutationQueue`**: Per-canonical-path synchronization. Read operations acquire shared permits; write and edit operations acquire exclusive permits for that specific path. Unrelated paths proceed concurrently without contention.
2. **Atomic Edit Engine**: Verifies exact unique match for `old_str`, calculates line deltas, performs UTF-8 validation, and writes changes via a temporary file before an atomic rename within `cap_std::fs::Dir`.
3. **Ripgrep Search Adapter**: Integrates `ignore::WalkBuilder` for globbing and `grep_searcher` with `grep_regex` for fast parallel file scanning respecting `.gitignore` without spawning shell subprocesses.
4. **Process Tree Supervision**: Wraps command executions with `tokio::process::Command`, assigns process groups (`setpgid`), monitors standard I/O streams with head/tail byte buffers, and terminates orphaned child processes on cancellation.
5. **Transferable Marshaler**: Automatically maps Rust errors (`std::io::Error`, validation failures, regex errors) into appropriate Monty exceptions (`ExcType::OSError`, `ExcType::ValueError`, `ExcType::TimeoutError`).

---

## 5. Dependency Classification and Adapters

Following the `codebase-design` vocabulary:

- **Module**: `crate::tools` (`ToolSet`).
- **Interface**: The 3 Rust methods (`new`, `lookup`, `dispatch_batch`) plus the Python async function specifications, error mappings, and concurrency invariants.
- **Seam**:
  - *Inbound Seam*: Between `Arena` and `ToolSet`.
  - *Outbound Seam*: Between `ToolSet` and OS resources (filesystem, process execution, search).
- **Port**:
  - *Inbound Port*: `ToolSet` struct presenting `lookup` and `dispatch_batch`.
  - *Outbound Ports*: `cap_std::fs::Dir` (filesystem port), process executor port, and ripgrep searcher port.
- **Adapter**:
  - *Driving Adapter*: `Arena::drive` in `src/arena.rs`, which translates Monty interpreter events into `ToolSet` batch requests.
  - *Driven Adapters*: Sandboxed `cap-std` filesystem adapter, `tokio::process` command adapter, and `ignore`/`grep` ripgrep adapter.
- **Testability & Fakes**: The inbound port allows full integration testing of all 7 tools without initializing a Monty subprocess, and allows testing the `Arena` driver using an in-memory fake `ToolSet`.

---

## 6. Architecture & Data Flow

```
[ Model Step: execute_python ]
             │
             ▼
      [ Monty Arena ]
   (asyncio.gather calls)
             │
   TurnEvent::FunctionCall (accumulate IDs)
   TurnEvent::ResolveFutures(batch)
             │
             ▼  (Inbound Seam)
      [ ToolSet::dispatch_batch ]
             │
   ┌─────────┴────────────────────────┐
   │ Concurrent Task Spawning         │
   │ (Tokio join_set / FuturesUnordered)
   ├──────────────────┬───────────────┤
   │                  │               │
   ▼ (Read/Grep/Glob)  ▼ (Write/Edit)  ▼ (Run)
[Parallel Blocking] [Mutation Lock] [Async Process]
   │                  │               │
   └─────────┬────────┴───────────────┘
             │
   [ Map to MontyObject & ToolCallResult ]
             │
             ▼  (Resume Seam)
[ checkout.resume_futures(results) ]
             │
             ▼
    [ Python resumes ]
```

---

## 7. Trade-offs & Evaluation

### Depth & Leverage
- **High Depth**: Extremely high leverage. Callers in `Arena` learn only 3 methods. Python models learn standard async Python functions that compose naturally with `asyncio.gather`.
- **Low Interface Surface**: `Arena` does not know what tools exist, what parameters they require, how locks are acquired, or how processes are spawned.

### Locality
- **High Locality**: Adding a new tool, modifying path validation, altering byte limits, or changing process supervision requires editing only files inside `src/tools/`. `src/arena.rs` remains untouched.

### Trade-offs Considered
1. **Batch Entry Point vs Individual Function Dispatches**:
   - *Chosen*: A single `dispatch_batch` entry point handling `Vec<ToolCallRequest>`.
   - *Alternative Rejected*: Exposing individual async methods (`tool_set.read(...)`, `tool_set.write(...)`) on `ToolSet`. That would shift batch orchestration, concurrency spawning, and error grouping into `src/arena.rs`, making `Arena` shallow and coupling it to every tool's argument structure.
2. **Per-file Mutation Queue vs Global Workspace Lock**:
   - *Chosen*: Fine-grained per-file locking via `FileMutationQueue`.
   - *Alternative Rejected*: Coarse single-writer workspace lock. While simpler, a global lock prevents parallel writes to different files when an agent generates multiple modules simultaneously via `asyncio.gather(write("a.py", ...), write("b.py", ...))`.

---

## Start Here
To inspect and implement this design, begin at `/Users/joe/kiki/src/tools.rs`. The new `ToolSet` structure will replace the single-purpose `ReadTool`, introducing `dispatch_batch` and housing the 7 tool implementations. Next, open `/Users/joe/kiki/src/arena.rs` (around line 760) to connect `TurnEvent::ResolveFutures` to `ToolSet::dispatch_batch`.

---

## Gaps
1. **Subprocess Standard Input**: The candidate design models `run` as a one-shot execution. Interactive stdin streaming for long-running processes is excluded from this initial self-hosting set.
2. **File Watcher / Change Invalidation**: The tool set does not maintain an in-memory cache of file contents; each `read` hits disk via `cap-std`. If caching is needed in the future, it should be placed behind `ToolSet`'s internal seam.

### Subagent result 10 · source line 173 · `40117bdc` · active branch

Agent: `explore`  
Model: `google/gemini-3.7-flash`  
Task: Design-It-Twice candidate 2 for Kiki's default tool module. Read CONTEXT.md, docs/VISION.md, docs/research/2026-09-01_agent-tool-sets-and-arena-execution.md, src/arena.rs, src/tools.rs, and related tests. Chosen product decisions: tools are only async functions inside execute_python; scope is read, write, edit, list, glob, grep, run; no skills/subagents. Use exact codebase-design vocabulary and Kiki domain terms. Optimise for maximum future flexibility and extension while staying evidence-based: consider heterogeneous typed tool specs, registry/discovery, adapters, per-tool concurrency policy, progress, cancellation, and transferable values. Return: (1) concrete Rust module interface and invariants/errors/ordering/performance, (2) exact Python signatures/result shapes for all seven tools, (3) arena-driver usage example, (4) hidden implementation, (5) dependency classification/adapters, (6) trade-offs and any extra complexity. Read-only; no edits.

(no output)

### Subagent result 11 · source line 174 · `0a7222ec` · active branch

Agent: `explore`  
Model: `google/gemini-3.7-flash`  
Task: Design-It-Twice candidate 3 for Kiki's default tool module. Read CONTEXT.md, docs/VISION.md, docs/research/2026-09-01_agent-tool-sets-and-arena-execution.md, src/arena.rs, src/tools.rs, and related tests. Chosen product decisions: models use tools only as async Python functions inside execute_python; scope is read, write, edit, list, glob, grep, run. Use exact codebase-design vocabulary and Kiki domain terms. Optimise for the common model-authored Python caller: signatures should be obvious, useful without parsing text, composable with ordinary Python and asyncio.gather, and difficult to misuse. Work backwards to the smallest Rust seam that can serve them. Return: (1) concrete Rust module interface/invariants/errors/ordering/performance, (2) exact Python signatures/result shapes for all seven tools, (3) realistic agent Python examples, (4) hidden implementation, (5) dependency classification/adapters, (6) trade-offs. Read-only; no edits.

# Design-It-Twice Candidate 3: Default Tool Module (Caller-Optimised)

## Executive Summary

Candidate 3 designs Kiki's default tool module around the primary caller: model-authored Python running inside a persistent Monty Arena. The module consolidates all seven pinned capabilities (`read`, `write`, `edit`, `list`, `glob`, `grep`, `run`) behind a unified, deep Rust seam.

Every tool is exposed as an async Python function returning structured, immediately inspectable Python values (lists, dictionaries, integers, booleans). Model code never parses CLI output or text protocols. The Rust implementation encapsulates capabilities-based filesystem isolation (`cap_std`), ripgrep traversal, per-file mutation serialisation, and process group lifecycle control behind a minimal batch-capable inbound port.

```
┌─────────────────────────────────────────────────────────────────────────┐
│                      Model-Authored Python Arena                        │
│   read()  write()  edit()  list()  glob()  grep()  run()  [asyncio]     │
└────────────────────────────────────┬────────────────────────────────────┘
                                     │  TurnEvent::FunctionCall / ResolveFutures
┌────────────────────────────────────▼────────────────────────────────────┐
│ Inbound Port: DefaultTools / ToolDispatcher                             │
│   dispatch_batch(Vec<(CallId, ToolInvocation)>) -> Vec<(CallId, Res)>   │
├─────────────────────────────────────────────────────────────────────────┤
│ Deep Implementation:                                                    │
│   - Workspace confinement & cap_std sandboxing                          │
│   - Per-path mutation queue & concurrency gates                         │
│   - In-process ripgrep traversal & search engine                        │
│   - Async process-group execution & timeout killing                     │
│   - MontyObject / MontyException translation & limits enforcement       │
└─────────────────────────────────────────────────────────────────────────┘
```

---

## 1. Concrete Rust Module Interface

### Rust Types and Signatures

The module exposes an inbound port `DefaultTools` consumed by the `Arena` execution engine (`arena.rs`).

```rust
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;
use monty_types::{CallId, MontyException, MontyObject, ResumeValue};
use thiserror::Error;

/// Configuration limits for default tools.
#[derive(Debug, Clone, Copy)]
pub struct ToolLimits {
    pub max_read_bytes: usize,       // Default: 4 * 1024 * 1024 (4 MB)
    pub max_read_lines: usize,       // Default: 100_000
    pub max_write_bytes: usize,      // Default: 4 * 1024 * 1024 (4 MB)
    pub max_list_entries: usize,     // Default: 10_000
    pub max_grep_matches: usize,     // Default: 10_000
    pub max_run_output_bytes: usize, // Default: 1 * 1024 * 1024 (1 MB)
    pub default_run_timeout: Duration,// Default: 60 seconds
}

/// Inbound port for all pinned tool execution within an Arena.
#[derive(Clone)]
pub struct DefaultTools {
    inner: Arc<DefaultToolsInner>,
}

/// A parsed, type-validated tool invocation from Python.
#[derive(Debug, Clone)]
pub enum ToolInvocation {
    Read {
        path: String,
        offset: usize,
        limit: Option<usize>,
    },
    Write {
        path: String,
        content: String,
    },
    Edit {
        path: String,
        old: String,
        new: String,
        count: usize,
    },
    List {
        path: String,
        recursive: bool,
        depth: Option<usize>,
    },
    Glob {
        pattern: String,
        path: String,
    },
    Grep {
        pattern: String,
        path: String,
        glob: Option<String>,
        ignore_case: bool,
        limit: Option<usize>,
    },
    Run {
        command: String,
        cwd: String,
        timeout: Duration,
    },
}

/// Structured outcome of a tool execution before Monty encoding.
#[derive(Debug, Clone)]
pub enum ToolOutcome {
    Read(Vec<String>),
    Write { bytes_written: usize },
    Edit { replacements: usize },
    List(Vec<DirEntryRecord>),
    Glob(Vec<String>),
    Grep(Vec<GrepMatchRecord>),
    Run(RunResultRecord),
}

#[derive(Debug, Clone)]
pub struct DirEntryRecord {
    pub name: String,
    pub path: String,
    pub is_dir: bool,
    pub is_file: bool,
    pub is_symlink: bool,
    pub size: u64,
}

#[derive(Debug, Clone)]
pub struct GrepMatchRecord {
    pub path: String,
    pub line_number: usize,
    pub line: String,
}

#[derive(Debug, Clone)]
pub struct RunResultRecord {
    pub exit_code: i32,
    pub stdout: String,
    pub stderr: String,
    pub success: bool,
    pub duration_ms: u64,
}

impl DefaultTools {
    /// Initialises tool capabilities bound to the workspace root.
    pub async fn new(
        workspace_root: impl AsRef<Path>,
        limits: ToolLimits,
    ) -> Result<Self, ToolError>;

    /// Returns true if the function name is a pinned default tool.
    pub fn is_pinned_tool(name: &str) -> bool;

    /// Validates and parses Monty function arguments into a typed invocation.
    pub fn parse_invocation(
        name: &str,
        args: &[MontyObject],
        kwargs: &[(String, MontyObject)],
    ) -> Result<ToolInvocation, ToolError>;

    /// Executes a single invocation (used for direct serial calls).
    pub async fn execute(&self, invocation: ToolInvocation) -> Result<ToolOutcome, ToolError>;

    /// Dispatches a batch of concurrent calls, managing parallelism and file locks.
    pub async fn execute_batch(
        &self,
        batch: Vec<(CallId, ToolInvocation)>,
    ) -> Vec<(CallId, ResumeValue)>;
}
```

### Invariants

1. **Workspace Confinement:** All path arguments are interpreted relative to the workspace root directory. Absolute paths, root prefixes, parent components (`..`), and symlink escapes targeting outside the workspace root are rejected before any filesystem or process operation.
2. **UTF-8 and Binary Safety:** `read`, `write`, and `edit` enforce valid UTF-8. Non-UTF-8 files or binary content fail explicitly rather than being silently corrupted.
3. **Atomic File Mutations:** `write` and `edit` write to temporary files on the same filesystem before performing an atomic rename. Partial writes never touch target files.
4. **Strict Edit Preconditions:** `edit` requires `old` to match the exact target content. If `count` is 1 (default) and `old` matches 0 or >1 times, the edit is aborted with `ValueError`.
5. **Deterministic Command Execution:** `run` executes commands within a detached process group, with working directory confined to the workspace. On timeout or cancellation, the process group receives `SIGKILL` to prevent orphan processes.
6. **Cancellation Safety:** If a Model Step is cancelled or times out, all in-flight tool tasks are aborted without corrupting workspace files.

### Error Modes and Python Mapping

Errors map to standard Python exception types (`monty_types::ExcType`) with descriptive messages:

```rust
#[derive(Debug, Error)]
pub enum ToolError {
    #[error("workspace paths must be relative and cannot contain parent components: {0}")]
    InvalidPath(String),

    #[error("path not found: {0}")]
    NotFound(String),

    #[error("path is a directory: {0}")]
    IsDirectory(String),

    #[error("path is not a regular file: {0}")]
    NotRegularFile(String),

    #[error("file content is not valid UTF-8")]
    InvalidUtf8,

    #[error("content exceeds limit of {limit} bytes")]
    ByteLimitExceeded { limit: usize },

    #[error("file exceeds limit of {limit} lines")]
    LineLimitExceeded { limit: usize },

    #[error("edit failed: {message}")]
    EditMismatch { message: String },

    #[error("permission denied: {0}")]
    PermissionDenied(String),

    #[error("command execution timed out after {duration:?}")]
    Timeout { duration: Duration },

    #[error("invalid argument for {tool}: {message}")]
    InvalidArgument { tool: &'static str, message: String },

    #[error("filesystem operation failed: {0}")]
    Io(#[from] std::io::Error),
}

impl From<ToolError> for MontyException {
    fn from(error: ToolError) -> Self {
        let (exc_type, msg) = match &error {
            ToolError::NotFound(p) => (ExcType::FileNotFoundError, format!("No such file or directory: {p}")),
            ToolError::IsDirectory(p) => (ExcType::IsADirectoryError, format!("Is a directory: {p}")),
            ToolError::PermissionDenied(p) => (ExcType::PermissionError, format!("Permission denied: {p}")),
            ToolError::Timeout { duration } => (ExcType::TimeoutError, format!("Operation timed out after {duration:?}")),
            ToolError::InvalidPath(p) => (ExcType::ValueError, format!("Invalid path: {p}")),
            ToolError::InvalidUtf8 => (ExcType::ValueError, "File is not valid UTF-8".to_owned()),
            ToolError::ByteLimitExceeded { limit } => (ExcType::ValueError, format!("Exceeded byte limit of {limit}")),
            ToolError::LineLimitExceeded { limit } => (ExcType::ValueError, format!("Exceeded line limit of {limit}")),
            ToolError::EditMismatch { message } => (ExcType::ValueError, message.clone()),
            ToolError::InvalidArgument { tool, message } => (ExcType::TypeError, format!("{tool}(): {message}")),
            ToolError::NotRegularFile(p) => (ExcType::ValueError, format!("Not a regular file: {p}")),
            ToolError::Io(err) => (ExcType::OSError, err.to_string()),
        };
        MontyException::new(exc_type, Some(msg))
    }
}
```

### Ordering Constraints

- **Suspension Batching:** When Python executes `asyncio.gather(*calls)`, Monty yields `TurnEvent::ResolveFutures { pending_call_ids }`.
- **Concurrent Dispatch:** Read-only operations (`read`, `list`, `glob`, `grep`) and independent `run` commands dispatch in parallel onto Tokio tasks.
- **Per-File Mutation Queue:** Concurrent mutations (`write`, `edit`) targeting the *same* canonical path are serialised using an internal per-file asynchronous lock queue, while mutations to *different* paths proceed in parallel.
- **Ordered Resolution:** Results correlate with `CallId`s and resume Monty via `checkout.resume_futures()`, preserving Monty's expected completion mapping.

### Performance Characteristics

- **Offloaded Blocking I/O:** All `cap_std` filesystem operations run inside `tokio::task::spawn_blocking`.
- **In-Process Traversal:** `glob` and `grep` run using ripgrep internal crates (`ignore`, `grep-searcher`, `grep-regex`) without spawning external shell processes.
- **Bounded Buffering:** `run` streams output into ring-buffered byte accumulators capped at `max_run_output_bytes` to prevent memory blowup.

---

## 2. Exact Python Signatures and Result Shapes

All seven tools are injected into the Arena as top-level asynchronous functions.

### 1. `read`

Reads a file as a list of exact lines with original line endings preserved.

```python
async def read(
    path: str,
    offset: int = 1,
    limit: int | None = None,
) -> list[str]: ...
```

- **Arguments:**
  - `path`: Workspace-relative file path.
  - `offset`: 1-based start line index (default: `1`).
  - `limit`: Maximum line count (default: `None`, reads to file end or line limit).
- **Result:** `list[str]` (e.g. `["fn main() {\n", "    println!(\"Hello\");\n", "}\n"]`).
- **Errors:** `FileNotFoundError`, `IsADirectoryError`, `ValueError` (for non-UTF-8 or path escape).

### 2. `write`

Creates a new file or completely overwrites an existing file, creating parent directories automatically.

```python
async def write(
    path: str,
    content: str,
) -> int: ...
```

- **Arguments:**
  - `path`: Workspace-relative destination path.
  - `content`: Text content to write.
- **Result:** `int` (bytes written).
- **Errors:** `ValueError` (path escape, size exceeded), `OSError`.

### 3. `edit`

Performs an exact string replacement with strict count verification.

```python
async def edit(
    path: str,
    old: str,
    new: str,
    count: int = 1,
) -> int: ...
```

- **Arguments:**
  - `path`: Workspace-relative file path.
  - `old`: Exact substring to find.
  - `new`: Replacement substring.
  - `count`: Expected number of occurrences (default: `1`).
- **Result:** `int` (number of replacements performed).
- **Errors:** `FileNotFoundError`, `ValueError` (if `old` occurs 0 or != `count` times).

### 4. `list`

Lists directory entries in a machine-oriented structured form.

```python
async def list(
    path: str = ".",
    recursive: bool = False,
    depth: int | None = None,
) -> list[dict[str, Any]]: ...
```

- **Arguments:**
  - `path`: Directory path relative to workspace (default: `"."`).
  - `recursive`: Traverse subdirectories if `True` (default: `False`).
  - `depth`: Maximum recursion depth (default: `None`).
- **Result:** List of dictionaries:
  ```python
  [
      {
          "name": "arena.rs",
          "path": "src/arena.rs",
          "is_dir": False,
          "is_file": True,
          "is_symlink": False,
          "size": 34812,
      },
      ...
  ]
  ```
- **Errors:** `FileNotFoundError`, `ValueError`.

### 5. `glob`

Finds relative paths matching a glob pattern, respecting `.gitignore`.

```python
async def glob(
    pattern: str,
    path: str = ".",
) -> list[str]: ...
```

- **Arguments:**
  - `pattern`: Glob pattern (e.g. `"**/*.rs"`, `"src/tools/*.ts"`).
  - `path`: Search root (default: `"."`).
- **Result:** `list[str]` of sorted workspace-relative paths (e.g. `["src/arena.rs", "src/tools.rs"]`).
- **Errors:** `ValueError` (invalid glob syntax, path escape).

### 6. `grep`

Searches file contents using regex or literal patterns, respecting `.gitignore`.

```python
async def grep(
    pattern: str,
    path: str = ".",
    glob: str | None = None,
    ignore_case: bool = False,
    limit: int | None = 100,
) -> list[dict[str, Any]]: ...
```

- **Arguments:**
  - `pattern`: Regular expression or search string.
  - `path`: Search root directory or file (default: `"."`).
  - `glob`: Optional file name filter (e.g. `"*.rs"`).
  - `ignore_case`: Case-insensitive search if `True` (default: `False`).
  - `limit`: Maximum match count (default: `100`).
- **Result:** List of structured match records:
  ```python
  [
      {
          "path": "src/tools.rs",
          "line_number": 106,
          "line": "impl ReadTool {\n",
      },
      ...
  ]
  ```
- **Errors:** `ValueError` (invalid regex, path escape).

### 7. `run`

Executes a command inside the workspace environment with timeout protection.

```python
async def run(
    command: str,
    cwd: str = ".",
    timeout: float = 60.0,
) -> dict[str, Any]: ...
```

- **Arguments:**
  - `command`: Shell command string.
  - `cwd`: Working directory relative to workspace root (default: `"."`).
  - `timeout`: Timeout in seconds (default: `60.0`).
- **Result:** Structured execution result (non-throwing on non-zero exit codes):
  ```python
  {
      "exit_code": 0,
      "stdout": "test result: ok. 357 passed; 0 failed\n",
      "stderr": "",
      "success": True,
      "duration_ms": 1420,
  }
  ```
- **Errors:** `TimeoutError` (if timeout exceeded), `ValueError` (invalid `cwd`).

---

## 3. Realistic Agent Python Examples

### Example 1: Parallel Codebase Inspection with `asyncio.gather`

The agent discovers files matching a query, filters them, and reads candidate headers in parallel without blocking.

```python
import asyncio

# Step 1: Find all Rust files and search for driver implementations concurrently
files, matches = await asyncio.gather(
    glob("src/**/*.rs"),
    grep(r"async fn drive\(", glob="*.rs"),
)

print(f"Discovered {len(files)} Rust files; {len(matches)} driver functions.")

# Step 2: Read the top 20 lines of all matched files in a single concurrent batch
preview_tasks = [read(m["path"], offset=1, limit=20) for m in matches]
previews = await asyncio.gather(*preview_tasks)

for match, lines in zip(matches, previews):
    header = "".join(lines[:3]).strip()
    print(f"== {match['path']}:{match['line_number']} ==\n{header}\n")
```

### Example 2: Verified Surgical Refactoring with `edit` and `run`

The agent inspects a method signature, updates callers with strict match validation, and verifies test suites immediately.

```python
# Step 1: Find all call sites for the deprecated function
call_sites = await grep(r"ReadTool::new\(workspace\)", glob="*.rs")
print(f"Found {len(call_sites)} call sites to update.")

# Step 2: Apply atomic edits to each unique file
updated_files = set()
for match in call_sites:
    file_path = match["path"]
    # edit() raises ValueError if old string does not match exactly once
    await edit(
        file_path,
        old="ReadTool::new(workspace)",
        new="DefaultTools::new(workspace, limits).await?",
        count=1,
    )
    updated_files.add(file_path)

print(f"Successfully updated {len(updated_files)} files.")

# Step 3: Run the test suite and inspect structured result
check = await run("cargo test --lib tools", timeout=120.0)
if not check["success"]:
    print(f"Tests failed (exit code {check['exit_code']}):\n{check['stderr']}")
else:
    print(f"Verification passed in {check['duration_ms']}ms:\n{check['stdout']}")
```

### Example 3: Multi-file Project Scaffolding

The agent constructs directory trees, writes source files, and inspects directory layout using structured entries.

```python
import asyncio

# Step 1: Write module scaffold concurrently
files_to_create = {
    "src/tools/mod.rs": "pub mod read;\npub mod write;\npub use read::*;\n",
    "src/tools/read.rs": "pub struct ReadTool;\n",
    "src/tools/write.rs": "pub struct WriteTool;\n",
}

# Automatic parent directory creation in write() handles 'src/tools/'
write_tasks = [write(path, content) for path, content in files_to_create.items()]
bytes_written = await asyncio.gather(*write_tasks)
print(f"Wrote {len(bytes_written)} files ({sum(bytes_written)} bytes total).")

# Step 2: Verify structured directory entries without text parsing
entries = await list("src/tools", recursive=False)
for entry in entries:
    print(f"Entry: {entry['name']} (file={entry['is_file']}, size={entry['size']}B)")
```

---

## 4. Hidden Implementation

Behind the `DefaultTools` seam, the module encapsulates all implementation complexity:

1. **Ambient Capability Sandboxing:** Filesystem access is strictly mediated through `cap_std::fs::Dir`. Paths are canonicalised relative to the ambient directory handle; attempts to traverse outside via symlinks or `..` are caught at the kernel/VFS layer.
2. **Per-File Mutation Serialisation (`FileMutationQueue`):** An internal sharded key-lock table (`Arc<DashMap<PathBuf, Arc<tokio::sync::Mutex<()>>>>`) ensures that concurrent `write` and `edit` operations targeting the same normalised file path are queued sequentially, while mutations to independent files execute concurrently under `asyncio.gather`.
3. **In-Process Ripgrep Integration:** `glob` and `grep` do not spawn external CLI binaries. They instantiate `ignore::WalkBuilder` with `.gitignore` filtering, and execute parallel chunked regex search via `grep_searcher::Searcher` and `grep_regex::RegexMatcher`.
4. **Subprocess Lifecycle & Group Isolation:** `run` spawns child processes inside a dedicated process group (`libc::setpgid(0, 0)`). Standard output and error streams are asynchronously drained into bounded memory ring-buffers. On timeout or cancellation, `SIGKILL` is sent to the process group (`kill(-pid, SIGKILL)`).
5. **Zero-Copy Monty Protocol Bridge:** The module unpacks Monty arguments, maps Rust `Result` types directly into `ResumeValue::Return(MontyObject)` or `ResumeValue::Error(MontyException)`, and handles `TurnEvent::ResolveFutures` batch resolution.
6. **Live Runtime Event Emission:** Starts and completions emit `ArenaProgress::ToolStarted` and `ArenaProgress::ToolFinished` runtime events for display projection without stalling the execution pipeline.

---

## 5. Dependency Classification and Adapters

Following the codebase design dependency taxonomy:

| Dependency | Category | Driven Adapter / Implementation | Verification & Testing Strategy |
|---|---|---|---|
| **Path validation & glob matching** | In-process | Pure Rust (`std::path`, `globset`) | Unit tests with diverse path inputs (relative, absolute, parent escapes, edge cases). |
| **String replacement (`edit`)** | In-process | Pure Rust exact substring matcher | Unit tests verifying single, multi, zero, and overlapping replacements. |
| **Monty Object Conversion** | In-process | `monty_types` encoders/decoders | Unit tests asserting mapping between Rust structs and `MontyObject`/`MontyException`. |
| **Filesystem (`read`/`write`/`edit`/`list`)** | Local-substitutable | `CapStdFilesystem` backed by `cap_std::fs::Dir` | Behavioral integration tests against isolated `tempfile::TempDir` instances. |
| **Search (`glob`/`grep`)** | Local-substitutable | In-process ripgrep engine (`ignore`, `grep_searcher`) | Behavioral integration tests over fixture trees verifying `.gitignore` rules. |
| **Command Execution (`run`)** | Local-substitutable / OS Environment | `TokioProcessRunner` backed by `tokio::process::Command` | Integration tests executing portable commands (`echo`, `exit`, `sleep`) with timeout checks. |

### Seam Discipline

- **Primary Seam (Inbound Port):** `DefaultTools` at the boundary between `arena.rs` and tool execution.
- **Internal Ports:** The module defines two internal driven traits for testing:
  - `WorkspaceFs`: abstracting filesystem reads, writes, and walks (defaulting to `CapStdFs`).
  - `ProcessExecutor`: abstracting command execution (defaulting to `TokioProcessExecutor`).
- **Test Surface:** All primary behavioural tests target the `DefaultTools` interface directly using real temporary directories and process execution.

---

## 6. Trade-offs and Evaluation

### High Leverage Areas

- **Frictionless Python Composability:** Exposing tools as ordinary async functions returning standard Python dictionaries and lists allows models to use list comprehensions, `for` loops, and `asyncio.gather` naturally without parsing CLI strings or harness-specific batching syntax.
- **Precondition Safety on Mutations:** Requiring exact count matching in `edit` eliminates ambiguous replacements and silent file corruption, which are common failure modes in agent harnesses.
- **Non-throwing `run` Results:** Returning structured exit codes and output strings rather than raising Python exceptions on non-zero exit codes allows models to inspect build and compiler failures cleanly without bloated `try...except` blocks.
- **Zero Shell Spawning for Inspection:** Running `grep` and `glob` in-process via ripgrep crates provides sub-millisecond execution times and avoids escaping bugs inherent in CLI shell generation.

### Deliberate Trade-offs

- **Dictionary Returns over Custom Python Classes:** Returning primitive `MontyObject::Dict` dictionaries rather than custom Python classes (`DirEntry`, `GrepMatch`) requires dictionary subscript access (`m["path"]`), but avoids complex Python bootstrap class injection into the sandboxed Arena.
- **Strict Workspace Confinement:** Operations outside the workspace root are forbidden. Agents cannot inspect global user configuration files or system paths unless explicitly mounted into the workspace.
- **In-Memory Size Limits:** Files over 4 MB or command outputs over 1 MB are capped. Agents must use pagination (`offset`/`limit`) or file filters for large targets.

### Subagent result 12 · source line 175 · `c64a6c5a` · active branch

Agent: `explore`  
Model: `google/gemini-3.7-flash`  
Task: Design-It-Twice candidate 4 for Kiki's default tool module. Read CONTEXT.md, docs/VISION.md, docs/research/2026-09-01_agent-tool-sets-and-arena-execution.md, src/arena.rs, src/tools.rs, and related tests. Chosen product decisions: arena-only async Python tools; scope read/write/edit/list/glob/grep/run. Use exact codebase-design vocabulary and Kiki domain terms. Design around ports and adapters only where evidence supports them. Classify filesystem, process execution, ripgrep traversal/search, Monty protocol, progress, and cancellation dependencies. Focus on seam placement that keeps Monty types out of tool policy and tool-specific details out of the Arena driver, with useful fakes and behavioural tests. Return: (1) concrete interfaces and types/invariants/errors/ordering/performance, (2) exact Python signatures/result shapes for seven tools, (3) adapter map and usage example, (4) hidden implementation, (5) testing strategy, (6) trade-offs. Read-only; no edits.

# Design-It-Twice Candidate 4: Decoupled Port-and-Adapter Tool Module with Monty-Neutral Policy

## Files Retrieved
1. `/Users/joe/kiki/CONTEXT.md` (lines 1–110) — Kiki domain terms, definitions, and architectural boundaries.
2. `/Users/joe/kiki/docs/VISION.md` (lines 1–210) — Persistent Python workspace model, tool exposure policies, and core coding toolset (`read`, `write`, `edit`, `list`, `glob`, `grep`, `run`).
3. `/Users/joe/kiki/docs/research/2026-09-01_agent-tool-sets-and-arena-execution.md` (lines 1–160) — Analysis of Maki, Pi, and Codex tool architectures, Monty 0.0.21 future batching, and Kiki's existing single-read driver.
4. `/Users/joe/kiki/src/tools.rs` (lines 1–250) — Existing `ReadTool` implementation with cap-std directory handle, path validation, limits, and test gate.
5. `/Users/joe/kiki/src/arena.rs` (lines 1–200, 725–950) — Monty driver loop, `TurnEvent` dispatch, single-call suspension assertion, output observation buffering, and execution lifecycle.

---

## 1. Concrete Interfaces, Types, Invariants, Errors, Ordering, and Performance

### Inbound Ports (Exposed to the Arena Driver)

The Arena driver requires a small, generic interface to discover tools, execute tool calls, and handle `asyncio.gather` concurrency batches without importing tool-specific logic or embedding tool policies inside the driver loop.

```rust
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;
use tokio_util::sync::CancellationToken;

/// Unique identifier for an in-flight tool invocation within an Arena execution.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct ToolCallId(pub u64);

/// Description of a pinned tool exposed to Monty name lookup.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolSpec {
    pub name: &'static str,
    pub docstring: &'static str,
}

/// Generic untyped tool argument container decoded from Monty.
#[derive(Debug, Clone, PartialEq)]
pub enum ToolValue {
    None,
    Bool(bool),
    Int(i64),
    Float(f64),
    String(String),
    List(Vec<ToolValue>),
    Dict(Vec<(String, ToolValue)>),
}

/// Protocol-neutral invocation payload received from the driving adapter.
#[derive(Debug, Clone, PartialEq)]
pub struct ToolInvocation {
    pub call_id: ToolCallId,
    pub tool_name: String,
    pub positional_args: Vec<ToolValue>,
    pub keyword_args: HashMap<String, ToolValue>,
}

/// Progress notifications emitted by executing tools.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ToolProgressEvent {
    Started { call_id: ToolCallId, tool_name: String, detail: String },
    Delta { call_id: ToolCallId, text: String },
    Finished { call_id: ToolCallId, tool_name: String, summary: String, success: bool },
}

/// Settled outcome of a single tool invocation.
#[derive(Debug, Clone, PartialEq)]
pub enum ToolExecutionResult {
    Success(ToolValue),
    Failure(ToolError),
}

/// Inbound port implemented by the Tool Module and consumed by the Arena Driver.
pub trait ToolHost: Send + Sync {
    /// Returns specifications of all tools available for name lookup in the Arena.
    fn registered_tools(&self) -> &[ToolSpec];

    /// Checks if a name corresponds to a registered tool.
    fn find_tool(&self) -> Option<&ToolSpec> {
        None
    }

    /// Dispatches a batch of tool invocations concurrently with cancellation support.
    /// Returns results mapped to their corresponding ToolCallId.
    fn dispatch_batch<'a>(
        &'a self,
        invocations: Vec<ToolInvocation>,
        cancellation: CancellationToken,
        progress_sink: Arc<dyn ProgressSink>,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Vec<(ToolCallId, ToolExecutionResult)>> + Send + 'a>>;
}
```

### Outbound Ports (Driven Capabilities Required by Tool Policy)

To decouple application policy from concrete OS mechanisms and enable useful fakes:

```rust
/// Outbound Port 1: Filesystem operations scoped to the workspace directory.
pub trait WorkspaceFs: Send + Sync {
    fn read_to_string(&self, path: &std::path::Path) -> Result<String, FsError>;
    fn read_lines(&self, path: &std::path::Path, offset: Option<usize>, limit: Option<usize>) -> Result<Vec<String>, FsError>;
    fn write_atomic(&self, path: &std::path::Path, contents: &str) -> Result<WriteMetadata, FsError>;
    fn list_directory(&self, path: &std::path::Path, recursive: bool, max_depth: usize) -> Result<Vec<DirEntryMetadata>, FsError>;
    fn file_metadata(&self, path: &std::path::Path) -> Result<FileMetadata, FsError>;
}

/// Outbound Port 2: Process execution with process-tree isolation and streaming.
pub trait ProcessRunner: Send + Sync {
    fn run_command<'a>(
        &'a self,
        command: ProcessCommand,
        cancellation: CancellationToken,
        progress_sink: Arc<dyn ProgressSink>,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<ProcessExecutionOutput, ProcessError>> + Send + 'a>>;
}

/// Outbound Port 3: Traversal and content search (ripgrep abstraction).
pub trait WorkspaceSearch: Send + Sync {
    fn glob_paths(&self, pattern: &str, base_path: &std::path::Path, include_hidden: bool) -> Result<Vec<PathBuf>, SearchError>;
    fn grep_content(&self, query: GrepQuery) -> Result<Vec<GrepMatch>, SearchError>;
}

/// Outbound Port 4: Structured progress sink.
pub trait ProgressSink: Send + Sync {
    fn emit(&self, event: ToolProgressEvent);
}
```

### Types and Invariants

```rust
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WriteMetadata {
    pub bytes_written: usize,
    pub lines_written: usize,
    pub created_new_file: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DirEntryMetadata {
    pub relative_path: String,
    pub entry_type: EntryKind,
    pub size_bytes: u64,
    pub is_symlink: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EntryKind {
    File,
    Directory,
    Other,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileMetadata {
    pub size_bytes: u64,
    pub is_file: bool,
    pub is_dir: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProcessCommand {
    pub program: String,
    pub args: Vec<String>,
    pub cwd: PathBuf,
    pub timeout: Duration,
    pub extra_env: HashMap<String, String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProcessExecutionOutput {
    pub exit_code: Option<i32>,
    pub stdout: String,
    pub stderr: String,
    pub duration: Duration,
    pub truncated: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GrepQuery {
    pub regex_pattern: String,
    pub search_path: PathBuf,
    pub file_glob: Option<String>,
    pub case_insensitive: bool,
    pub max_matches: usize,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GrepMatch {
    pub path: String,
    pub line_number: usize,
    pub line_text: String,
    pub match_range: (usize, usize),
}
```

#### Core Invariants
1. **Workspace Boundary Confinement**: Every path is checked before resolution. Paths must be relative, cannot contain parent directory components (`..`), and cannot escape the workspace root via symlinks (guaranteed via `cap-std::fs::Dir`).
2. **Atomic Mutations**: File modifications (`write` and `edit`) must write to a unique temporary file within the target directory and atomically replace the destination to prevent partial file corruption on interruption or crash.
3. **Exact-Match Edits**: `edit` requires `old_str` to occur exactly `count` times (default 1) in the file. If `old_str` occurs 0 times or more than `count` times, the operation fails without modifying the file.
4. **Bounded Buffers**: 
   - `read`: maximum 4MB or 100,000 lines.
   - `list`: maximum 10,000 entries.
   - `glob`: maximum 10,000 paths.
   - `grep`: maximum 1,000 matches or 50KB total match text.
   - `run`: maximum 256KB total output captured with streaming head/tail retention.
5. **Process Group Lifetime**: Subprocesses spawned by `run` run in an isolated process group (on Unix via `setpgid`) and are guaranteed to receive `SIGTERM`/`SIGKILL` on cancellation, timeout, or drop.
6. **Same-Path Mutation Serialization**: When a concurrent batch contains multiple mutating operations (`write` or `edit`) for the exact same target path, they are executed in deterministic invocation order under a per-path asynchronous lock. Independent paths run concurrently.

### Error Hierarchy and Python Exception Mapping

```rust
#[derive(Debug, thiserror::Error, Clone, PartialEq)]
pub enum ToolError {
    #[error("invalid argument: {0}")]
    InvalidArgument(String),
    #[error("file not found: {0}")]
    NotFound(String),
    #[error("path is a directory: {0}")]
    IsDirectory(String),
    #[error("path is not a regular file: {0}")]
    NotRegularFile(String),
    #[error("path escapes workspace boundary: {0}")]
    PathEscape(String),
    #[error("file is not valid UTF-8")]
    InvalidUtf8,
    #[error("size limit exceeded: {0}")]
    LimitExceeded(String),
    #[error("edit match failed: {0}")]
    EditMismatch(String),
    #[error("command execution failed: {0}")]
    CommandFailed(String),
    #[error("operation timed out after {0:?}")]
    Timeout(Duration),
    #[error("operation cancelled")]
    Cancelled,
    #[error("I/O error: {0}")]
    Io(String),
}
```

The driving adapter translates `ToolError` into standard Python exceptions in Monty:
- `ToolError::InvalidArgument`, `ToolError::PathEscape`, `ToolError::NotRegularFile`, `ToolError::LimitExceeded`, `ToolError::EditMismatch` $\to$ `ValueError`.
- `ToolError::NotFound` $\to$ `FileNotFoundError`.
- `ToolError::IsDirectory` $\to$ `IsADirectoryError`.
- `ToolError::InvalidUtf8` $\to$ `UnicodeDecodeError`.
- `ToolError::Timeout` $\to$ `TimeoutError`.
- `ToolError::Cancelled` $\to$ `asyncio.CancelledError`.
- `ToolError::CommandFailed`, `ToolError::Io` $\to$ `OSError`.

### Ordering and Concurrency Semantics
- **Batch Resumption**: Monty 0.0.21 emits `TurnEvent::ResolveFutures { pending_call_ids }`. The Tool Module executes all pending calls concurrently using `tokio::task::JoinSet`, collects their settled outcomes, and returns the results vector indexed by `pending_call_ids` in the exact order requested by Monty.
- **Progress Delta Ordering**: Tool progress deltas (`ToolProgressEvent::Delta`) are timestamped and drained into the Agent's display projection without stalling tool execution.

### Performance Characteristics
- Filesystem I/O runs inside `tokio::task::spawn_blocking` pools to prevent starvation of the Tokio async reactor.
- Traversal and regex searches are executed using the native multithreaded Rust `ignore` and `grep-searcher` crates directly inside the process, bypassing child process fork-exec overhead.
- Output capture uses streaming scalar line counters and ring-buffers to maintain $O(1)$ memory usage during large process output streams.

---

## 2. Exact Python Signatures and Result Shapes for Seven Tools

All seven tools are injected as pinned asynchronous functions in the Monty Arena.

### 1. `read`
```python
async def read(
    path: str,
    offset: int | None = None,
    limit: int | None = None,
) -> list[str]:
    """Read lines from a workspace file.
    
    Args:
        path: Relative path to the file.
        offset: Optional 1-based starting line number.
        limit: Optional maximum number of lines to return.
        
    Returns:
        List of line strings preserving exact newline terminators (\n or \r\n).
    """
```
**Return Shape:** `["line 1\n", "line 2\n", "line 3\n"]`

### 2. `write`
```python
async def write(
    path: str,
    contents: str,
) -> dict[str, Any]:
    """Create or overwrite a file atomically.
    
    Args:
        path: Relative path to the destination file.
        contents: Complete UTF-8 text to write.
        
    Returns:
        Dictionary with write metadata.
    """
```
**Return Shape:**
```python
{
    "path": "src/main.rs",
    "bytes_written": 1420,
    "lines_written": 48,
    "created_new_file": False,
}
```

### 3. `edit`
```python
async def edit(
    path: str,
    old_str: str,
    new_str: str,
    count: int = 1,
) -> dict[str, Any]:
    """Apply an exact atomic string replacement in a file.
    
    Args:
        path: Relative path to the file.
        old_str: Exact string to find and replace.
        new_str: Replacement string.
        count: Expected number of exact occurrences (default 1).
        
    Returns:
        Dictionary with edit metadata.
    """
```
**Return Shape:**
```python
{
    "path": "src/tools.rs",
    "replacements": 1,
    "lines_changed": 12,
}
```

### 4. `list`
```python
async def list(
    path: str = ".",
    recursive: bool = False,
    depth: int = 1,
) -> list[dict[str, Any]]:
    """List directory entries in sorted, machine-readable format.
    
    Args:
        path: Relative path to the directory (default current directory).
        recursive: Whether to traverse subdirectories recursively.
        depth: Maximum traversal depth when recursive=True (default 1).
        
    Returns:
        List of entry dictionaries sorted alphabetically by path.
    """
```
**Return Shape:**
```python
[
    {
        "path": "Cargo.toml",
        "type": "file",
        "size": 2480,
        "is_symlink": False,
    },
    {
        "path": "src",
        "type": "directory",
        "size": 0,
        "is_symlink": False,
    },
]
```

### 5. `glob`
```python
async def glob(
    pattern: str,
    path: str = ".",
    include_hidden: bool = False,
) -> list[str]:
    """Find relative paths matching a glob pattern, respecting .gitignore.
    
    Args:
        pattern: Glob pattern (e.g. "**/*.rs" or "docs/*.md").
        path: Base relative directory to search from (default ".").
        include_hidden: Whether to include dotfiles (default False).
        
    Returns:
        Sorted list of matching relative path strings.
    """
```
**Return Shape:** `["src/agent.rs", "src/arena.rs", "src/tools.rs"]`

### 6. `grep`
```python
async def grep(
    pattern: str,
    path: str = ".",
    glob: str | None = None,
    ignore_case: bool = False,
    max_matches: int = 100,
) -> list[dict[str, Any]]:
    """Search file contents for a regular expression pattern using ripgrep.
    
    Args:
        pattern: Regular expression pattern string.
        path: Base relative directory or file to search (default ".").
        glob: Optional file glob filter (e.g. "*.rs").
        ignore_case: Whether to perform case-insensitive matching.
        max_matches: Maximum number of matches to return (default 100).
        
    Returns:
        List of match records with line numbers and line text.
    """
```
**Return Shape:**
```python
[
    {
        "path": "src/arena.rs",
        "line_number": 725,
        "line": "async fn drive(\n",
        "match": "drive",
    },
]
```

### 7. `run`
```python
async def run(
    command: str | list[str],
    cwd: str = ".",
    timeout: float = 30.0,
    env: dict[str, str] | None = None,
) -> dict[str, Any]:
    """Execute a shell command or argument list in the workspace.
    
    Args:
        command: Shell string (executed via sh/cmd) or list of argument strings.
        cwd: Working directory relative to workspace root (default ".").
        timeout: Maximum duration in seconds before SIGTERM/SIGKILL (default 30.0).
        env: Optional environment variables to set/override.
        
    Returns:
        Dictionary containing exit code, stdout, stderr, duration, and truncation status.
    """
```
**Return Shape:**
```python
{
    "exit_code": 0,
    "stdout": "test result: ok. 357 passed; 0 failed\n",
    "stderr": "",
    "duration_ms": 420,
    "truncated": False,
}
```

---

## 3. Adapter Map and Usage Example

### Adapter Map

| Port | Role | Production Adapter | Test Fake Adapter | Rationale & Seam Evidence |
|---|---|---|---|---|
| `ToolHost` | **Inbound Port** | `DefaultToolRegistry` | `FakeToolRegistry` | Allows Arena driver to execute tools without importing any tool policy or file/process crates. |
| `MontyBridge` | **Driving Adapter** | `MontyToolAdapter` | `DirectCallDriver` | Translates Monty `TurnEvent` / `MontyObject` / `MontyException` to/from protocol-neutral `ToolInvocation` and `ToolValue`. Keeps Monty out of tool policy. |
| `WorkspaceFs` | **Outbound Port** | `CapStdFsAdapter` | `MemoryFsAdapter` | Confines filesystem access via `cap-std::fs::Dir` in production; enables lightning-fast, zero-I/O in-memory tests via `BTreeMap<PathBuf, Vec<u8>>`. |
| `ProcessRunner` | **Outbound Port** | `TokioProcessAdapter` | `FakeProcessRunner` | Manages child process lifecycle, process groups, and timeouts; fake enables deterministic simulation of long-running, failing, or hung processes. |
| `WorkspaceSearch` | **Outbound Port** | `RipgrepEngineAdapter` | `MemorySearchEngine` | Embeds `ignore` and `grep-searcher` in-process; fake searches the `MemoryFsAdapter` in tests without disk or gitignore files. |
| `ProgressSink` | **Outbound Port** | `ArenaProgressSink` | `RecordingProgressSink` | Converts `ToolProgressEvent` to `ArenaProgress` in production; records events into a `Vec` for assertions in tests. |

### Usage Example

#### 1. Wiring the Module at the Composition Root (`main.rs` / `runtime.rs`)

```rust
use std::sync::Arc;
use cap_std::ambient_authority;
use cap_std::fs::Dir;

pub async fn build_default_tool_host(workspace_root: &std::path::Path) -> Arc<dyn ToolHost> {
    let root_path = workspace_root.to_path_buf();
    let dir = tokio::task::spawn_blocking(move || {
        Dir::open_ambient_dir(root_path, ambient_authority()).expect("open workspace")
    }).await.unwrap();

    let fs = Arc::new(CapStdFsAdapter::new(Arc::new(dir)));
    let runner = Arc::new(TokioProcessAdapter::new(workspace_root.to_path_buf()));
    let search = Arc::new(RipgrepEngineAdapter::new(workspace_root.to_path_buf()));

    Arc::new(DefaultToolRegistry::new(fs, runner, search))
}
```

#### 2. Monty Driver Executing Concurrent Tool Calls (`arena.rs`)

```rust
// Inside Arena driver `drive()` loop handling TurnEvent::ResolveFutures:
TurnEvent::ResolveFutures { pending_call_ids } => {
    // 1. Collect pending invocations staged during FunctionCall events
    let invocations = pending_calls.drain_matching(&pending_call_ids);
    
    // 2. Dispatch batch through the inbound port concurrently
    let results = tool_host.dispatch_batch(
        invocations,
        cancellation.clone(),
        progress_sink.clone(),
    ).await;
    
    // 3. Convert results back to Monty ResumeValue objects
    let resume_payload = results.into_iter().map(|(id, res)| {
        let resume_val = match res {
            ToolExecutionResult::Success(val) => ResumeValue::Return(val.into_monty_object()),
            ToolExecutionResult::Failure(err) => ResumeValue::Error(err.into_monty_exception()),
        };
        (id.0, resume_val)
    }).collect();
    
    // 4. Resume Monty with ordered results
    checkout.resume_futures(resume_payload, &mut on_print).await?
}
```

#### 3. Model-Authored Python in Arena Utilizing Concurrency

```python
# Model Step in Kiki Arena:
import asyncio

async def inspect_codebase():
    # Concurrent reading and searching via Python-native asyncio.gather
    files_to_read = ["src/arena.rs", "src/tools.rs", "Cargo.toml"]
    reads_future = asyncio.gather(*(read(p) for p in files_to_read))
    grep_future = grep(pattern="fn drive", glob="*.rs")
    
    results, matches = await asyncio.gather(reads_future, grep_future)
    
    print(f"Read {len(results)} files; found {len(matches)} matching lines.")
    return {"matches": matches}

state = await inspect_codebase()
```

---

## 4. Hidden Implementation

The Tool Module hides substantial operational complexity behind its narrow interfaces:

1. **Protocol Conversion and Argument Extraction**:
   - Extraction of Python positional and keyword arguments, type assertions, and defaults (e.g. `path: str = "."`, `count: int = 1`) without leaking `MontyObject` matching into business logic.
2. **Path Sanitisation and Confinement**:
   - `validate_and_resolve_path`: normalises separators, strips relative prefixes, rejects null bytes, Windows drive prefixes, and verifies containment inside `cap_std::fs::Dir`.
3. **Atomic File Modification Engine**:
   - `write_file_atomic`: writes to `.kiki_tmp_<uuid>` in the destination directory, forces `fsync`, and performs `rename` over the target file.
   - `edit_file_atomic`: reads the original file, scans for exact `old_str` substrings, calculates 1-based replacement line numbers, validates occurrence count, writes to temp file, and swaps atomically.
4. **Ripgrep In-Process Search Machinery**:
   - Initialises `ignore::WalkBuilder` with standard gitignore, git exclude, and hidden file traversal policies.
   - Streams matches through `grep_searcher::Searcher` and `grep_regex::RegexMatcher` with binary detection and bounded memory buffers.
5. **Process Group Supervisor and Ring-Buffer Stream Pumping**:
   - On Unix: configures `std::os::unix::process::CommandExt::process_group(0)`.
   - Attaches asynchronous `tokio::io::BufReader` to child `stdout` and `stderr` pipes.
   - Merges output chunks into an `ObservationAccumulator` that preserves the first 192KB and the last 64KB while discarding the middle if total output exceeds 256KB.
   - In flight cancellation kills the process group via `libc::killpg(pid, libc::SIGKILL)` to prevent orphaned zombie processes.
6. **Per-Path Concurrency Gate**:
   - A striped async read-write lock table (`DashMap<PathBuf, Arc<RwLock<()>>>`) allows concurrent reads across all paths and concurrent writes across distinct paths, while safely serialising concurrent writes/edits targeting identical files in the same batch.

---

## 5. Testing Strategy

Following the principle that the interface is the primary test surface:

```
┌────────────────────────────────────────────────────────┐
│                   Behavioural Tests                    │
│   (Fast, pure in-memory tests against Inbound Port)    │
├──────────────────────────┬─────────────────────────────┤
│                          │                             │
│   Adapter Integration    │    Monty Bridge Protocol    │
│   (Real OS Fs, Subproc,  │    (Real Monty Sandbox &    │
│    Ripgrep on tempdirs)  │     asyncio.gather tests)   │
└──────────────────────────┴─────────────────────────────┘
```

### 1. Behavioural Tests (Against `ToolHost` using Test Fakes)
- **Zero Disk/Process Overhead**: Run in <10ms in-memory.
- **Path Traversal Security**: Verify rejection of `../`, `/etc/passwd`, `C:\`, `foo/../../bar`.
- **Edit Invariants**:
  - `old_str` missing $\to$ `ToolError::EditMismatch`.
  - `old_str` present 3 times with `count: 1` $\to$ `ToolError::EditMismatch`.
  - Exact replacement preserves line ending styles (LF vs CRLF).
- **Concurrency & Batching**: Feed multiple invocations into `dispatch_batch` and verify returned IDs match input IDs.
- **Output Truncation**: Verify limits on `read` lines, `list` depth, `glob` results, and `grep` max match capping.

### 2. Driven Adapter Integration Tests
- **`CapStdFsAdapter`**: Test symlink escape prevention, read-only filesystem handling, atomic replacement under file contention.
- **`TokioProcessAdapter`**:
  - Test timeout termination: spawn `sleep 10` with `timeout: 0.1s` and assert elapsed duration and `ToolError::Timeout`.
  - Test cancellation: trigger cancellation token and assert immediate process tree termination.
  - Test standard input closed, stdout/stderr captured separately.
- **`RipgrepEngineAdapter`**: Verify `.gitignore` exclusion, hidden file skipping, and binary file bypass.

### 3. Driving Adapter (Monty Bridge) Contract Tests
- Execute actual Python scripts inside the Monty pool worker:
  - `await read("file.txt")` returns Python `list[str]`.
  - `await asyncio.gather(read("a"), read("b"), grep("pat"))` resolves correctly with 3 concurrent items.
  - Calling `read(123)` raises Python `TypeError`.
  - Reading non-existent file raises Python `FileNotFoundError`.

### 4. Deterministic Concurrency and Cancellation Test Gates
- Extend the `ReadTestGate` pattern into a generic `ToolTestGate`:
  - Intercept tool execution before start, during execution, and after settlement.
  - Assert that cancelling an Arena model step mid-tool cleanly terminates background tasks without poisoning the Arena or leaving orphaned files.

---

## 6. Trade-offs

### 1. Pure Rust Ripgrep Crates vs Spawning `rg` CLI via `ProcessRunner`
- **Choice**: Embed Rust crates (`ignore`, `grep-regex`, `grep-searcher`) directly in the `RipgrepEngineAdapter`.
- **Advantages**: Eliminates external binary dependency on `rg`; avoids OS fork-exec latency on every glob/grep; allows fine-grained, cancellation-safe in-memory stream truncation without reading unbounded process pipes.
- **Cost**: Adds ripgrep dependencies to Kiki's Cargo dependency tree.

### 2. Inbound Dispatcher vs Individual Tool Structs in Arena
- **Choice**: Arena holds a single `Arc<dyn ToolHost>` rather than seven separate tool structs (`ReadTool`, `WriteTool`, etc.).
- **Advantages**: Complete depth and locality. The Arena driver never changes when adding, deprecating, or modifying tools; Monty event handling remains entirely tool-agnostic.
- **Cost**: Requires a small dynamic dispatch or invocation enum step at the host seam.

### 3. Protocol-Neutral `ToolValue` vs Exposing `MontyObject` Directly
- **Choice**: Decouple tool policy from Monty using `ToolValue` and `ToolError`.
- **Advantages**: Enables testing all tool policy, invariants, and edge cases with simple, fast unit tests without instantiating or linking the Monty interpreter or runtime worker.
- **Cost**: Adds a lightweight translation mapping when entering/exiting Monty.

### 4. Per-Path Concurrency Locking vs Turn-Level Global Serialisation
- **Choice**: Manage concurrency with per-path locks during batch execution.
- **Advantages**: Preserves maximum parallelism for independent reads and writes across different files in `asyncio.gather`, while guaranteeing deterministic consistency for repeated edits to the same file.
- **Cost**: Requires maintaining an internal path lock map inside the tool execution engine.

---

## Start Here
To implement this design, begin with **`src/tools.rs`**:
1. Define the outbound ports (`WorkspaceFs`, `ProcessRunner`, `WorkspaceSearch`, `ProgressSink`) and the core `ToolHost` inbound port.
2. Implement the seven tool policies in pure Rust against these ports.
3. Add the `CapStdFsAdapter`, `TokioProcessAdapter`, and `RipgrepEngineAdapter`.
4. Update `src/arena.rs` to replace the hardcoded `read` handler in `drive()` with the generic `ToolHost` batch dispatcher.

## Gaps
1. **Monty Module Imports**: Currently, tools are resolved via `TurnEvent::NameLookup` into global async functions. If Monty later adds custom module import support (e.g. `import kiki.tools`), the driving adapter can bind these same tools to module namespaces without changing tool policy.
2. **Subagent & MCP Tools**: Subagent spawning and MCP external adapters will plug into this identical `ToolHost` inbound port as additional tool definitions when those milestones are reached.

### User · source line 176 · `dd473d43` · active branch

It's also worth grabbing https://github.com/huggingface/smolagents as another research reference, and checking how that works as another reference point.

### Subagent result 13 · source line 185 · `17472c53` · active branch

Agent: `explore`  
Model: `google/gemini-3.7-flash`  
Task: Research the current shallow checkout of Hugging Face smolagents at .research/smolagents/source, read-only. We are planning Kiki's arena-only default tools read/write/edit/list/glob/grep/run. Determine smolagents' default tool sets for CodeAgent/ToolCallingAgent and any coding or sandbox executor variants; distinguish framework defaults, automatically added tools, and user-supplied tools. Trace how tools are declared, schemas generated, registered/exposed in prompts or model tool payloads, called from generated Python, executed, and returned. Focus especially on code-first agent semantics, async tool functions, whether Python must await tools, multiple tool calls, concurrency/gather support, result ordering, executor boundaries/sandboxing, state persistence, errors/cancellation, and typed versus text results. Follow callers/callees/tests/docs. Return neutral findings with exact repository-relative paths and 1-based line numbers for each material claim. End with concrete similarities/differences from Kiki's chosen arena-only approach, without recommendations.

## Files Retrieved
1. `.research/smolagents/source/src/smolagents/agents.py` (lines 268–620, 1215–1500, 1505–1820) — Core agent classes `MultiStepAgent`, `ToolCallingAgent`, and `CodeAgent`; tool setup, execution loop, step stream, and parallel tool orchestration.
2. `.research/smolagents/source/src/smolagents/tools.py` (lines 106–350, 1061–1180, 1335–1370) — `Tool` base class, `@tool` decorator, docstring/type-hint validation, code prompt generation (`to_code_prompt`), tool calling prompt formatting (`to_tool_calling_prompt`), and remote tool source code generation (`get_tools_definition_code`).
3. `.research/smolagents/source/src/smolagents/default_tools.py` (lines 39–120, 678–695) — Standard default tool implementations (`PythonInterpreterTool`, `FinalAnswerTool`, `DuckDuckGoSearchTool`, `VisitWebpageTool`, etc.) and `TOOL_MAPPING`.
4. `.research/smolagents/source/src/smolagents/local_python_executor.py` (lines 42–240, 825–950, 1417–1768) — AST interpreter `LocalPythonExecutor`, `BASE_PYTHON_TOOLS` dictionary, `evaluate_python_code`, node dispatch, function call evaluation, and `FinalAnswerException` handling.
5. `.research/smolagents/source/src/smolagents/remote_executors.py` (lines 53–250, 335–420, 551–640, 726–780, 859–920) — `RemotePythonExecutor` base class and sandbox implementations (`E2BExecutor`, `DockerExecutor`, `ModalExecutor`, `BlaxelExecutor`); tool injection, serialization, and package installation.
6. `.research/smolagents/source/src/smolagents/models.py` (lines 288–330, 535–560) — Schema conversion `get_tool_json_schema` for API model tool calling payloads (`completion_kwargs["tools"]`).
7. `.research/smolagents/source/src/smolagents/_function_type_hints_utils.py` (lines 86–250) — Type-hint and Google-style docstring parsing into JSON schema for dynamic `@tool` creation.
8. `.research/smolagents/source/src/smolagents/serialization.py` (lines 32–215) — Safe JSON/base64 serializer `SafeSerializer` for passing agent variables and complex objects across executor boundaries.
9. `.research/smolagents/source/src/smolagents/prompts/code_agent.yaml` (lines 1–110) & `structured_code_agent.yaml` (lines 1–110) — System prompt templates rendering tool signatures as Python function definitions for `CodeAgent`.
10. `.research/smolagents/source/src/smolagents/prompts/toolcalling_agent.yaml` (lines 1–110) — System prompt template for JSON-based tool calling.
11. `.research/smolagents/source/src/smolagents/mcp_client.py` (lines 28–100) — MCP client wrapper adapting asynchronous MCP server transports into synchronous `Tool` instances.
12. `.research/smolagents/source/examples/async_agent/main.py` (lines 1–45) & `docs/source/en/examples/async_agent.md` (lines 1–55) — Official pattern for async integration (wrapping synchronous `agent.run` with `anyio.to_thread.run_sync`).

---

## Key Code

### 1. Automatic Tool Injection & Base Tools Setup (`agents.py`)
```python
# .research/smolagents/source/src/smolagents/agents.py:394-404
def _setup_tools(self, tools, add_base_tools):
    assert all(isinstance(tool, BaseTool) for tool in tools), (
        "All elements must be instance of BaseTool (or a subclass)"
    )
    self.tools = {tool.name: tool for tool in tools}
    if add_base_tools:
        self.tools.update(
            {
                name: cls()
                for name, cls in TOOL_MAPPING.items()
                if name != "python_interpreter" or self.__class__.__name__ == "ToolCallingAgent"
            }
        )
    self.tools.setdefault("final_answer", FinalAnswerTool())
```

### 2. Code Prompt Generation for CodeAgent (`tools.py`)
```python
# .research/smolagents/source/src/smolagents/tools.py:254-287
def to_code_prompt(self) -> str:
    args_signature = ", ".join(f"{arg_name}: {arg_schema['type']}" for arg_name, arg_schema in self.inputs.items())

    # Use dict type for tools with output schema to indicate structured return
    has_schema = hasattr(self, "output_schema") and self.output_schema is not None
    output_type = "dict" if has_schema else self.output_type
    tool_signature = f"({args_signature}) -> {output_type}"
    tool_doc = self.description
    ...
    if self.inputs:
        args_descriptions = "\n".join(
            f"{arg_name}: {arg_schema['description']}" for arg_name, arg_schema in self.inputs.items()
        )
        args_doc = f"Args:\n{textwrap.indent(args_descriptions, '    ')}"
        tool_doc += f"\n\n{args_doc}"
    ...
    tool_doc = f'"""{tool_doc}\n"""'
    return f"def {self.name}{tool_signature}:\n{textwrap.indent(tool_doc, '    ')}"
```

### 3. Local AST Interpreter Function Call Evaluation (`local_python_executor.py`)
```python
# .research/smolagents/source/src/smolagents/local_python_executor.py:840-853, 915-920
elif isinstance(call.func, ast.Name):
    func_name = call.func.id
    if func_name in state:
        func = state[func_name]
    elif func_name in static_tools:
        func = static_tools[func_name]
    elif func_name in custom_tools:
        func = custom_tools[func_name]
    elif func_name in ERRORS:
        func = ERRORS[func_name]
    else:
        raise InterpreterError(
            f"Forbidden function evaluation: '{call.func.id}' is not among the explicitly allowed tools or defined/imported in the preceding code"
        )
...
    return func(*args, **kwargs)
```

### 4. Final Answer Control Flow via BaseException (`local_python_executor.py`)
```python
# .research/smolagents/source/src/smolagents/local_python_executor.py:1573-1581, 1646-1651
class FinalAnswerException(BaseException):
    """Exception raised when final_answer is called.

    Inherits from BaseException instead of Exception to prevent being caught
    by generic `except Exception` clauses in agent-generated code.
    """
    def __init__(self, value):
        self.value = value
...
if "final_answer" in static_tools:
    previous_final_answer = static_tools["final_answer"]

    def final_answer(*args, **kwargs):  # Allow arbitrary arguments to be passed
        raise FinalAnswerException(previous_final_answer(*args, **kwargs))

    static_tools["final_answer"] = final_answer
```

### 5. Multi-Tool Parallelism in ToolCallingAgent (`agents.py`)
```python
# .research/smolagents/source/src/smolagents/agents.py:1432-1448
else:
    # If multiple tool calls, process them in parallel
    with ThreadPoolExecutor(self.max_tool_threads) as executor:
        futures = []
        for tool_call in parallel_calls.values():
            ctx = copy_context()
            futures.append(executor.submit(ctx.run, process_single_tool_call, tool_call))
        for future in as_completed(futures):
            tool_output = future.result()
            outputs[tool_output.id] = tool_output
            yield tool_output

memory_step.tool_calls = [parallel_calls[k] for k in sorted(parallel_calls.keys())]
memory_step.observations = memory_step.observations or ""
for tool_output in [outputs[k] for k in sorted(outputs.keys())]:
    memory_step.observations += tool_output.observation + "\n"
```

---

## Architecture

```
User -> agent.run(task, additional_args)
          │
          ▼
    MultiStepAgent (ReAct loop)
          │
    ┌─────┴────────────────────────────────────────────────┐
    ▼                                                      ▼
CodeAgent (Code-first)                               ToolCallingAgent (JSON Tool calls)
    │                                                      │
    ├─ Model Generates Python snippet                      ├─ Model Generates ChatMessageToolCalls
    │  (Jinja2 prompt exposes tools as                     │  (Model payload receives JSON schemas
    │   Python function signatures)                        │   via get_tool_json_schema)
    │                                                      │
    ▼                                                      ▼
PythonExecutor.send_tools & send_variables           ToolCallingAgent.process_tool_calls
    │                                                      │
    ├─ Local: LocalPythonExecutor (AST interpreter)        ├─ Single call: Direct execution
    │         - Evaluates AST tree                         └─ Multiple calls: ThreadPoolExecutor
    │         - Dispatches ast.Call to static_tools           - Concurrent threads (no async/await)
    │         - Captures print() into _print_outputs          - Results sorted deterministically
    │         - final_answer() raises BaseException              by tool_call.id
    │                                                      │
    └─ Remote: E2B / Docker / Modal / Blaxel               ▼
       - SafeSerializer serializes variables/state   Observation string appended to memory
       - Tool source code injected to kernel
       - Remote kernel executes multi-line code
          │
          ▼
    Result: execution logs + last evaluated expression / final answer
```

---

## Findings

### 1. Tool Categories and Default Tool Sets
Smolagents divides tools across three distinct tiers:

1. **Framework Core Defaults (Always Injected):**
   - `FinalAnswerTool` (`.research/smolagents/source/src/smolagents/default_tools.py:83-93`): Automatically registered by `MultiStepAgent._setup_tools` via `self.tools.setdefault("final_answer", FinalAnswerTool())` (`agents.py:403`).
   - `BASE_PYTHON_TOOLS` (`.research/smolagents/source/src/smolagents/local_python_executor.py:73-125`): In `LocalPythonExecutor`, a hardcoded dictionary of ~45 safe Python built-in and `math` functions is always injected into the interpreter namespace (`print`, `isinstance`, `range`, `len`, `sum`, `min`, `max`, `round`, `ceil`, `floor`, `sqrt`, `pow`, `abs`, `enumerate`, `zip`, `reversed`, `sorted`, `map`, `filter`, `getattr`, `hasattr`, `setattr`, `issubclass`, `type`, `complex`, etc.).
   - `BASE_BUILTIN_MODULES` (`.research/smolagents/source/src/smolagents/utils.py:48-60`): Standard library modules permitted by default without explicit authorization (`collections`, `datetime`, `itertools`, `math`, `queue`, `random`, `re`, `stat`, `statistics`, `time`, `unicodedata`).

2. **Optional Base Tools (`add_base_tools=True`):**
   - Configured via `TOOL_MAPPING` (`.research/smolagents/source/src/smolagents/default_tools.py:678-685`):
     - `duckduckgo_search`: `DuckDuckGoSearchTool`
     - `visit_webpage`: `VisitWebpageTool`
     - `python_interpreter`: `PythonInterpreterTool` (added only when the agent is a `ToolCallingAgent`; excluded for `CodeAgent` because `CodeAgent` executes Python natively, `agents.py:397-402`).
   - Other specialized tools exist in `default_tools.py` (`UserInputTool`, `GoogleSearchTool`, `ApiWebSearchTool`, `WikipediaSearchTool`, `SpeechToTextTool`), but require explicit instantiation and passing by the user (`default_tools.py:96-120, 687-698`).

3. **User-Supplied Tools & Managed Agents:**
   - Supplied via `tools: list[Tool]` to agent initialization (`agents.py:295`).
   - Managed agents passed via `managed_agents: list[MultiStepAgent]` are dynamically wrapped as tools with inputs `task` (string) and `additional_args` (object) (`agents.py:380-392`).

### 2. Tool Declaration, Schema Generation, and Exposure
- **Subclassing `Tool` (`tools.py:106-220`):** Requires class attributes `name`, `description`, `inputs` (dictionary defining `type`, `description`, and optional `nullable`), and `output_type`. Class attributes are strictly validated at class definition time via `__init_subclass__` (`tools.py:130-131, 133-219`).
- **Function Decorator `@tool` (`tools.py:1061-1180`):** Parses function signatures and Google-style docstrings into JSON schema using `_function_type_hints_utils.get_json_schema` (`_function_type_hints_utils.py:86-250`), generates a dynamic subclass `SimpleTool(Tool)`, and extracts AST source code for remote serialization (`tools.py:1078-1155`).
- **Prompt vs Model Payload Exposure:**
  - **`CodeAgent`:** Tools are converted to Python function signatures with docstrings via `tool.to_code_prompt()` (`tools.py:254-287`) and injected into the system prompt via Jinja2 templates (`prompts/code_agent.yaml:81-85`, `prompts/structured_code_agent.yaml:77-81`). Model API `tools` parameter is **not** passed in standard code mode.
  - **`ToolCallingAgent`:** Converted into standard OpenAPI/JSON schema format via `get_tool_json_schema` (`models.py:288-320`) and passed in model generation requests as `completion_kwargs["tools"]` (`models.py:539-540`). Also formatted as text descriptions in system prompt fallback via `tool.to_tool_calling_prompt()` (`tools.py:289-290`, `prompts/toolcalling_agent.yaml:93-96`).

### 3. Code-First Agent Semantics vs Tool Calling
- **Code-First (`CodeAgent`):**
  - The model outputs executable Python code blocks (`<code>...</code>` or ````python ... ````) (`prompts/code_agent.yaml:7-10`, `agents.py:1546-1555`).
  - The entire multi-line block executes as a single unit in the interpreter. The agent can use variable assignments, control flow (`for`, `while`, `if`), nested loops, list comprehensions, math, and multiple tool calls within one block.
  - Observations consist of captured `print()` logs (`Execution logs:\n...`) plus the evaluated return value of the last statement in the script (`truncate_content(str(code_output.output))`) (`agents.py:1770-1777`).
  - Calling `final_answer(value)` raises `FinalAnswerException` (`local_python_executor.py:1573-1581`), halting code execution immediately and setting `is_final_answer=True` (`local_python_executor.py:1646-1651`).
- **Tool Calling (`ToolCallingAgent`):**
  - Generates JSON action payloads parsed into `ChatMessage.tool_calls` (`agents.py:1318-1335`).
  - Dispatches tool invocations individually via `execute_tool_call` (`agents.py:1465-1502`).

### 4. Async Tool Functions and Await Semantics
- **No Async/Await in Generated Python:** Tool functions in smolagents are strictly synchronous callables (`tools.py:237-251`).
- **AST Interpreter Rejection:** `LocalPythonExecutor` implements explicit visitor methods for supported AST nodes (`local_python_executor.py:1431-1571`). It does **not** implement `ast.Await`, `ast.AsyncFor`, `ast.AsyncWith`, or `ast.AsyncFunctionDef`. Any attempt by the LLM to write `await my_tool(...)` triggers the fallback in `evaluate_ast`: `InterpreterError: Await is not supported` (`local_python_executor.py:1579-1580`).
- **MCP Async Bridging:** When loading tools from async MCP servers (`ToolCollection.from_mcp` / `MCPClient`), smolagents delegates to `mcpadapt`, which runs an `asyncio` event loop in a background thread and exposes synchronous wrapper methods (`tools.py:962-963`, `mcp_client.py:28-100`).
- **Synchronous Agent Run:** `agent.run()` is synchronous and blocking. Integrating with async ASGI applications (e.g. Starlette/FastAPI) requires offloading `agent.run` to a worker thread via `anyio.to_thread.run_sync` (`examples/async_agent/main.py:24-26`, `docs/source/en/examples/async_agent.md:14-18`).

### 5. Multiple Tool Calls, Concurrency, and Ordering
- **In `ToolCallingAgent`:**
  - If the model emits multiple tool calls in one step, they are executed concurrently using Python's `concurrent.futures.ThreadPoolExecutor(self.max_tool_threads)` (`agents.py:1434-1442`).
  - Thread safety context is preserved using `contextvars.copy_context().run` (`agents.py:1438`).
  - Result ordering is deterministic: results are sorted by tool call ID (`[outputs[k] for k in sorted(outputs.keys())]`, `agents.py:1445-1448`) and concatenated into `memory_step.observations`.
  - Calling `final_answer` alongside other tool calls in the same turn raises `AgentExecutionError` ("If you want to return an answer, please do not perform any other tool calls than the final answer tool call!", `agents.py:1341-1345`).
- **In `CodeAgent`:**
  - Generated code executes sequentially within a single thread inside the interpreter loop (`local_python_executor.py:1633-1638`).
  - There is no language-level `gather` or concurrent tool dispatch primitive exposed to the LLM code.

### 6. Executor Boundaries and Sandboxing
- **`LocalPythonExecutor` (In-Process AST Interpreter):**
  - Evaluates Python AST in the host process (`local_python_executor.py:1688-1768`).
  - **Not a security sandbox:** The docstring explicitly notes: *"This executor evaluates Python code with restricted access to imports and built-in functions. It is not a security sandbox: for isolated execution of untrusted code, use a remote executor."* (`local_python_executor.py:1693-1695`).
  - Safety checks include import whitelisting against `BASE_BUILTIN_MODULES` + `additional_authorized_imports` (`local_python_executor.py:1713`), dunder blocking (`nodunder_getattr`, `ALLOWED_DUNDER_METHODS` = `__init__`, `__str__`, `__repr__`, `local_python_executor.py:68-71, 917-920`), blacklisting dangerous builtins/modules (`local_python_executor.py:127-154`), and execution limits (`MAX_OPERATIONS = 10_000_000`, `MAX_EXECUTION_TIME_SECONDS = 30` via `ThreadPoolExecutor` timeout wrapper, `local_python_executor.py:63, 65, 303-345, 1673-1674`).
- **Remote Sandbox Executors (`remote_executors.py:53-978`):**
  - Variants: `E2BExecutor` (E2B cloud MicroVMs, lines 335–548), `DockerExecutor` (local Docker container running Jupyter Kernel Gateway, lines 551–723), `ModalExecutor` (Modal cloud containers, lines 726–856), `BlaxelExecutor` (Blaxel workspaces, lines 859–978).
  - **Boundary Transfer:** Tools are serialized to Python source code via `get_tools_definition_code(tools)` (`tools.py:1335-1358`) and instantiated in the remote kernel (`remote_executors.py:107-119`). Variables in `state` are serialized via `SafeSerializer.dumps` (supporting JSON primitives, dicts with complex keys, sets, tuples, bytes, datetimes, decimals, PIL images, numpy arrays; or optional pickle fallback) (`serialization.py:32-212`, `remote_executors.py:121-137`).
  - **Constraint:** Managed agents cannot run inside remote executors (`if self.managed_agents: raise Exception("Managed agents are not yet supported with remote code execution.")`, `agents.py:1588`).

### 7. State Persistence
- **In `CodeAgent`:**
  - `self.state` dictionary persists across all steps of a run (`local_python_executor.py:1708, 1756-1763`). Variables assigned in Step 1 remain defined and accessible in Step 2 (`prompts/code_agent.yaml:104`: *"The state persists between code executions: so if in one step you've created variables or imported modules, these will all persist."*).
  - In remote executors, the remote Jupyter kernel namespace persists across steps throughout the session (`remote_executors.py:130-137`).
- **In `ToolCallingAgent`:**
  - Tool calls are stateless by default. However, special rich outputs (`AgentImage`, `AgentAudio`) are automatically stored in `self.state["image.png"]` / `self.state["audio.mp3"]` (`agents.py:1406-1416`), and `_substitute_state_variables` resolves string arguments matching state keys back to stored objects (`agents.py:1451-1463`).

### 8. Errors and Cancellation
- **Error Types & Recovery:**
  - `AgentGenerationError`: Model generation/API failures; terminates the run immediately (`agents.py:596-598, 1324, 1686`).
  - `AgentParsingError`: Code block or JSON parsing errors; caught as `AgentError` in `_run_stream`, logged to `action_step.error`, and returned to the LLM in the next turn's observation so it can correct its formatting (`agents.py:599-601, 1328, 1700`).
  - `AgentExecutionError` / `AgentToolExecutionError`: Raised on runtime exception in tool or Python execution; captured into `action_step.error` and presented as an observation traceback to allow the LLM to self-heal code errors (`agents.py:600-601, 1487, 1768`).
  - `AgentMaxStepsError`: Raised when step limit is reached without final answer (`agents.py:530, 608-610, 638-662`).
- **Cancellation:**
  - `MultiStepAgent` provides `self.interrupt_switch = False` (`agents.py:464`). If set to `True` during streaming, the loop aborts on the next step with `AgentError("Agent interrupted.")` (`agents.py:545-546`).
  - Execution timeout is enforced via `timeout_seconds` in `LocalPythonExecutor` (`local_python_executor.py:303-345, 1673-1674`).

### 9. Typed vs Text Results
- **Tools Return Python Objects:** Tool `forward()` methods return native Python types (`dict`, `list`, `int`, `PIL.Image.Image`, custom classes) (`tools.py:237-251`).
- **CodeAgent Handling:** Python code retains full type fidelity during execution (e.g. `res = fetch_data(); item = res["data"][0]`). When formatted for ReAct memory observation, results are converted to text via `truncate_content(str(code_output.output))` and `print()` logs (`agents.py:1770-1777`). However, `final_answer(typed_obj)` returns the original raw typed object to the caller (`agents.py:503-535`).
- **ToolCallingAgent Handling:** Returns from tool calls are converted to strings via `str(tool_call_result).strip()` unless they are `AgentImage` or `AgentAudio`, which are saved to `self.state` (`agents.py:1406-1418`).

---

## Start Here
To inspect the core tool calling and code execution engine:
1. Open `.research/smolagents/source/src/smolagents/agents.py` at line 394 (`_setup_tools`), line 1282 (`ToolCallingAgent._step_stream`), and line 1640 (`CodeAgent._step_stream`) to examine how tool execution diverges between code-first and tool-calling paradigms.
2. Open `.research/smolagents/source/src/smolagents/local_python_executor.py` at line 825 (`evaluate_call`) and line 1583 (`evaluate_python_code`) to examine the synchronous AST execution loop and tool injection.

---

## Gaps
1. **Managed Agent Serialization in Remote Sandboxes:** As documented in `agents.py:1588`, managed agents cannot be invoked from inside remote executors (`E2BExecutor`, `DockerExecutor`, `ModalExecutor`, `BlaxelExecutor`); investigating remote multi-agent RPC mechanisms is not supported in the codebase.
2. **Dynamic Top-Level Async in Remote Kernels:** While Jupyter kernels support top-level `await`, smolagents generates synchronous tool stubs (`tools.py:1346-1358`), so async kernel interactions within remote executors are not utilized by smolagents' abstractions.

---

## Similarities and Differences from Kiki's Arena-Only Tool Approach

| Feature / Dimension | Smolagents (`smolagents`) | Kiki Planned Arena-Only Approach |
| :--- | :--- | :--- |
| **Primary Tool Interface** | Polyglot: Python AST/code blobs (`CodeAgent`) or JSON tool calling (`ToolCallingAgent`). | Fixed arena tools: `read`, `write`, `edit`, `list`, `glob`, `grep`, `run`. |
| **Tool Execution Paradigm** | Multi-line Python execution in custom AST interpreter or remote Jupyter kernel; agent writes logic/loops in Python. | CLI / discrete tool-calling operations executed against the sandboxed workspace. |
| **Default Tool Set** | Dynamic: `FinalAnswerTool` always injected; optional `duckduckgo_search`, `visit_webpage`, `python_interpreter` via `add_base_tools=True`; ~45 Python built-ins in AST interpreter. | Fixed arena-only default tools (`read`, `write`, `edit`, `list`, `glob`, `grep`, `run`) operating on workspace filesystem and processes. |
| **Execution Environment / Boundary** | Dual: In-process AST interpreter (`LocalPythonExecutor`, not a sandbox) or remote container/VM executors (`Docker`, `E2B`, `Modal`, `Blaxel`). | Isolated arena sandbox execution boundary for all filesystem and command invocations. |
| **Async / Await Tool Invocation** | Synchronous only: AST interpreter raises on `ast.Await`; MCP async servers bridged to sync; `agent.run` is synchronous blocking. | Async-first harnesses where tool executions and process spawning are managed asynchronously. |
| **Multi-Tool Concurrency** | Thread-based in `ToolCallingAgent` (`ThreadPoolExecutor` sorted by ID); strictly sequential line-by-line in `CodeAgent`. | Deterministic tool execution (sequential or orchestrated async dispatch per agent protocol). |
| **State Persistence** | Python variable namespace (`self.state` dict in AST, or Jupyter kernel namespace in remote containers) persists across steps. | Workspace filesystem state and shell process state persist across turns. |
| **Result Representation** | Native Python typed objects in code, stringified/truncated logs in ReAct observations, typed final answer object. | Structured text/file content and command output buffers formatted for model context. |
| **Completion Flow** | Special tool `final_answer(...)` raising `FinalAnswerException` to exit execution loop. | Agent emits final textual response / completion message after tool operations finish. |

### Subagent result 14 · source line 206 · `5bf075b0` · active branch

Agent: `explore`  
Model: `google/gemini-3.7-flash`  
Task: Validate only the smolagents additions in docs/research/2026-09-01_agent-tool-sets-and-arena-execution.md against .research/smolagents/source at commit 30bb1161095dbae2271e6bc3cc4c219cc3897a57. Read-only. Check default/base tool claims, CodeAgent prompt exposure, persistent state, synchronous call/AST await claims, ToolCallingAgent thread-pool ordering, typed return handling, sandbox caveat, and all local line references. Report only concrete errors or material omissions; say none if accurate.

(no output)

### Assistant · source line 207 · `9732fcc0` · active branch

Smolagents is now shallow-cloned at `.research/smolagents/source` and added to the research doc. Its useful pattern is concrete Python signatures, docstrings, and structured return schemas. Its `CodeAgent` remains synchronous, with no `await` or `gather`.

**Where should Kiki’s tool-module seam sit?**

The seam must hide argument validation, workspace confinement, mutation ordering, process control, concurrent batching, cancellation, and call-ID correlation from the Arena driver.

**A.** Monty-coupled `DefaultTools`  
`lookup()` and `dispatch_batch()` accept Monty types directly. Smallest interface, but leaks interpreter details into tool policy.

**B.** Kiki-owned batch interface with a thin Monty adapter  
_(recommended)_ `definitions()` exposes Python-facing metadata; `dispatch_batch()` accepts Kiki `ToolCall` values and returns correlated `ToolResult` values. Preserves the existing rule that only `arena.rs` knows Monty while avoiding a speculative plugin registry.

**C.** Extensible trait-based registry  
Each tool supplies metadata, parsing, execution, concurrency policy, and adapters. Similar to Codex/Maki, but too much interface for seven fixed tools.

Which seam should we use?

### User · source line 208 · `0d1b277a` · active branch

B, does this also make sense when we get to having user plugins as python modules?

### Assistant · source line 211 · `95a4627b` · active branch

Yes. B gives us a stable Kiki-owned invocation protocol:

- Rust built-ins implement it now.
- Trusted Python plugin tools can later use a Python adapter behind the same seam.
- MCP tools can use another adapter.
- Pure Python modules running entirely inside the Arena bypass it until they need a host effect.
- `arena.rs` remains unaware of which adapter owns a tool.

We should not build the plugin registry now. The future router can sit behind the existing interface without changing Monty handling.

**What values should `ToolCall` and `ToolResult` carry?**

**A.** A small JSON-like transferable value set  
_(recommended)_ `None`, booleans, integers, floats, strings, lists, and string-keyed dictionaries. Handlers parse these into private typed arguments.

**B.** Full Monty objects  
More expressive, but recouples tools and future plugins to Monty.

**C.** Only per-tool Rust structs  
Strongly typed, but makes future dynamic plugin and MCP tools awkward.

Which value model should we use?

### User · source line 212 · `06ed4f4a` · active branch

A

### Assistant · source line 215 · `23e3b072` · active branch

**How should the agent learn each Python tool interface?**

**A.** Generate concise Python signatures and return shapes from `ToolDefinition` metadata  
_(recommended)_ Keeps prompt documentation aligned with runtime names and parameters. Markdown still owns behavioural guidance such as always `await` and use `asyncio.gather()` for independent calls.

**B.** Handwrite every interface in the Markdown prompt  
Simpler initially, but runtime validation and prompt documentation can drift.

**C.** Require Arena introspection  
Keeps the base prompt smaller, but forces extra model work before ordinary calls.

Which documentation model should we use?

### User · source line 216 · `e2923e55` · active branch

A, we can add the tool definitions in their own part of the system prompt as they are generated. I'm thinking to maximally keep the agent focussed on the code, they ought to be python function skeletons (and perhaps dataclass type definitions beforehand) with function bodies as `...` and a docstring with the tool description. For example, something like 
```
async def list(dir: str) -> list[Path]:
    """list directories

    ...docs go here...
    """
    ...
```
what do you think about that? any issues you forsee?

### Assistant · source line 223 · `eb619f4a` · active branch

Strong direction. Python skeletons are compact, familiar, and make `async` visually unavoidable. Smolagents provides evidence that models use signature-and-docstring tool references well.

Guardrails:

- Generated annotations must exactly match runtime values.
- Mark the block as reference-only: functions already exist and must not be redefined.
- Keep shared rules such as `await`, `gather`, paths, errors, and concurrency outside individual docstrings.
- `list` conflicts with Python’s builtin and currently would not reach Monty name lookup. We should choose another name separately.

Actual dataclasses would complicate our chosen JSON-like value model. Returning dictionaries while documenting them as dataclasses would be misleading.

**How should structured result shapes appear?**

**A.** Reference-only `TypedDict` definitions, returning ordinary dictionaries  
_(recommended)_ Gives fields precise names and types while retaining simple JSON-like transfer and dictionary access.

**B.** Real dataclass instances injected into the Arena  
Nicer attribute access, but adds bootstrap classes, conversion rules, checkpointed type identity, and plugin compatibility concerns.

**C.** Inline `dict` annotations with keys described only in docstrings  
Smallest prompt, but weaker and easier for the agent to misuse.

Which result-shape model should we use?

### User · source line 224 · `76db663a` · active branch

Let's discuss B, it is fine for us to bootstrap the Monty environment, I don't think that's an issue?

### Assistant · source line 240 · `0d76e4a3` · active branch

Agreed. Bootstrapping is not the problem. Monty 0.0.21 supports basic dataclasses, persistence, attribute access, and transfer.

The clean design is public Python wrappers over private host functions:

```python
@dataclass
class DirectoryEntry:
    path: str
    kind: str

async def list_dir(path: str = ".") -> list[DirectoryEntry]:
    """List one directory in stable order."""
    rows = await _kiki_list_dir(path)
    return [DirectoryEntry(**row) for row in rows]
```

This preserves the JSON-like Rust seam while giving Arena code real typed objects.

Main constraints:

- Use basic dataclasses only. Monty rejects features such as `frozen=True`, `InitVar`, and `__post_init__`.
- Bootstrap definitions and generated prompt skeletons must come from one source.
- Durable checkpoints will eventually need a bootstrap schema/version because changing fields changes restored Arena semantics.
- Use dataclasses only for genuine records:
  - `DirectoryEntry`, `GrepMatch`, and `CommandResult`: yes.
  - `read` and `glob`: plain `list[str]`.
  - `write` and `edit`: probably simple results rather than ceremonial one-field classes.
- Name types precisely. `Path` would misleadingly resemble `pathlib.Path`; `DirectoryEntry` is clearer.
- Public wrappers add a small conversion cost, but it is negligible beside filesystem and process work.

**Should we adopt real bootstrap dataclasses only for record-shaped results?**

**A.** Yes  
_(recommended)_ Typed attribute access where useful, plain primitives elsewhere.

**B.** Keep all structured results as dictionaries  
Less bootstrap code, but weaker Arena ergonomics.

Which should we use?

### User · source line 241 · `6f4352d5` · active branch

A

### Assistant · source line 244 · `98739240` · active branch

**What should replace the conflicting `list` name?**

Python’s builtin `list()` resolves before Monty host lookup, so a global tool named `list` is unsafe and confusing.

**A.** `list_dir(path: str = ".")`  
_(recommended)_ Precise, searchable, and preserves the ordinary `list()` builtin.

**B.** `ls(path: str = ".")`  
Concise and familiar, but less descriptive in Python.

**C.** Put every tool under a namespace, such as `tools.list()`  
Avoids global collisions, but introduces a module-like object and changes every tool call.

Which name should we use?

### User · source line 245 · `0ebbefee` · active branch

A

### Assistant · source line 246 · `6eca3230` · active branch

**What should `read` return?**

Kiki’s Arena changes the trade-off. File content stays in Python rather than entering model context automatically, so pagination arguments are less valuable than in Pi or Codex.

**A.** `async def read(path: str) -> str`  
_(recommended)_ Returns the complete accepted UTF-8 file exactly. The agent can retain it, search it directly, or call `.splitlines(keepends=True)` when line access is useful.

**B.** `async def read(path: str) -> list[str]`  
Matches Kiki’s current behaviour, but makes ordinary text searching and parsing require `"".join(lines)`.

**C.** `async def read(path: str, offset: int = 1, limit: int | None = None) -> str`  
Supports host-side pagination, but adds interface the Arena can usually replace with normal Python slicing.

All options retain workspace confinement and host-side file-size limits. Which interface should we use?

### User · source line 247 · `45248254` · superseded branch

A seems good. 

However I do want to have a look quickly at how Mackie does line reads and edits because I think it uses mini hashes and I want to understand why that is and if that would be useful in this context

### Subagent result 15 · source line 251 · `a0be7d4d` · superseded branch

Agent: `explore`  
Model: `google/gemini-3.7-flash`  
Task: Quickly research Maki's line-oriented reads and edits under .research/maki/source, focusing on any mini-hash, line hash, anchor, or content identity mechanism. Trace from read output generation through prompt-visible format into edit parsing/application and conflict/staleness checks. Explain exactly what is hashed, hash size/algorithm, how the model supplies hashes, whether hashes identify lines/regions/files, how collisions/duplicates/line movement are handled, and what tests establish. Distinguish this from FileReadTracker mtime stale-write protection and fuzzy replacement. Then assess factually how the mechanism's problem differs when Kiki's read(path) returns exact text into persistent Python and edits are called from Python. Return concise findings with exact Maki file paths and 1-based line numbers, plus concrete costs/benefits in Kiki's context. Read-only; no edits.

### Scope & Interpretation
Investigated the complete read, edit, staleness, and fuzzy-matching subsystems in Maki under `/Users/joe/kiki/.research/maki/source`. 
**Key Finding:** Maki does **not** implement a mini-hash, line-hash, or cryptographic content-identity token mechanism. Instead, it combines:
1. Formatted 1-based decimal line numbers in `read` (`plugins/read/init.lua`).
2. Filesystem modification-time (`mtime`) tracking via Rust `FileReadTracker` (`maki-agent/src/tools/file_tracker.rs`).
3. An 8-stage fuzzy string replacement engine (`plugins/lib/maki/fuzzy_replace.lua`), which includes a `block_anchor` fuzzy middle-line matching algorithm and automatic indentation drift rebasing.
4. Optional integer line-range tools (`edit_lines` and `insert_lines` in `plugins/edit/init.lua`).

---

## Files Retrieved
1. `/Users/joe/kiki/.research/maki/source/maki-agent/src/tools/file_tracker.rs` (lines 10–73) — In-memory `mtime` registry checking for stale reads before file modifications.
2. `/Users/joe/kiki/.research/maki/source/maki-lua/src/api/util/ctx.rs` (lines 322–343) — Lua tool context bridging `record_read` and `check_before_edit` to the native agent tracker.
3. `/Users/joe/kiki/.research/maki/source/plugins/read/init.lua` (lines 82–159) — `read` tool handler formatting line numbers, tracking read files, and generating truncation notices.
4. `/Users/joe/kiki/.research/maki/source/plugins/read/read_helpers.lua` (lines 3–28) — UTF-8 safe byte truncation and newline splitting.
5. `/Users/joe/kiki/.research/maki/source/plugins/edit/init.lua` (lines 164–270, 403–490) — Edit lifecycle (`apply_edit`), staleness checks, tool registration for `edit`, `multiedit`, `edit_lines`, and `insert_lines`.
6. `/Users/joe/kiki/.research/maki/source/plugins/lib/maki/fuzzy_replace.lua` (lines 49–118, 401–447, 549–628) — Multi-tier string matcher, `block_anchor` Levenshtein middle-matching, duplicate detection, and indentation drift correction.
7. `/Users/joe/kiki/.research/maki/source/plugins/edit/tests/spec.lua` (lines 13–185) — Test cases validating exact matching, indentation drift re-indentation, `block_anchor`, ambiguity, and duplicate match rejections.
8. `/Users/joe/kiki/.research/maki/source/plugins/code_execution/init.lua` (lines 113–160) — Monty Python interpreter integration calling async host tools.

---

## Key Code

### 1. Stale-Write Protection via Filesystem `mtime` (`file_tracker.rs:37-70`)
```rust
    pub fn record_read(&self, path: &Path) {
        let normalized = normalize_path(path);
        match get_mtime(&normalized) {
            Some(mtime) => {
                self.0.lock().unwrap().insert(normalized, mtime);
            }
            None => warn!(
                path = %path.display(),
                "record_read: could not get mtime, file will not be tracked"
            ),
        }
    }

    pub fn check_before_edit(&self, path: &Path) -> Result<(), String> {
        let normalized = normalize_path(path);
        let mut guard = self.0.lock().unwrap();
        let Some(&recorded) = guard.get(&normalized) else {
            return Ok(());
        };
        let Some(current) = get_mtime(&normalized) else {
            guard.remove(&normalized);
            return Ok(());
        };
        if recorded != current {
            return Err(format!(
                "{STALE_READ_MSG}: {} - re-read using read tool before editing",
                path.display(),
            ));
        }
        Ok(())
    }
```

### 2. Prompt-Visible Read Output Generation (`plugins/read/init.lua:100-120`)
```lua
  ctx:record_read(path)

  local parts = {}
  local nr_fmt = ToolView.line_nr_fmt(start + #lines - 1) .. ": %s"
  for i, line in ipairs(lines) do
    parts[#parts + 1] = string.format(nr_fmt, start + i - 1, line)
  end
  local llm_output = table.concat(parts, "\n")

  local trunc_start = start + #lines
  if trunc_start <= total_lines then
    llm_output = llm_output
      .. string.format(
        "\n\n...\n\nTruncated lines: %d-%d. Use offset=%d to read further.",
        trunc_start,
        total_lines,
        trunc_start
      )
  end
```

### 3. Edit Execution Flow (`plugins/edit/init.lua:164-190`)
```lua
local function apply_edit(path, ctx, transform)
  path = maki.fs.abspath(path)

  local ok, err = ctx:check_before_edit(path)
  if not ok then
    return nil, err
  end

  local before, read_err = maki.fs.read(path)
  if read_err then
    return nil, "read error: " .. tostring(read_err)
  end

  local after, transform_err = transform(before)
  if transform_err then
    return nil, transform_err
  end

  local _, write_err = maki.fs.write(path, after)
  if write_err then
    return nil, "write error: " .. tostring(write_err)
  end

  ctx:record_read(path)

  return {
    path = path,
    before = before,
    after = after,
  }
end
```

### 4. The `block_anchor` Matching Strategy (`plugins/lib/maki/fuzzy_replace.lua:401-447`)
```lua
local function block_anchor(content, find)
  local content_lines = split_lines(content)
  local search_lines = split_lines(find)
  if #search_lines > 0 and search_lines[#search_lines] == "" then
    search_lines[#search_lines] = nil
  end
  if #search_lines < CONTEXT_AWARE_LINE_MIN then
    return {}
  end

  local first_trimmed = trim(search_lines[1])
  local last_trimmed = trim(search_lines[#search_lines])

  local candidates = {}
  for i = 1, #content_lines do
    if trim(content_lines[i]) == first_trimmed then
      local tail_start = i + 2
      if tail_start <= #content_lines then
        for j = tail_start, #content_lines do
          if trim(content_lines[j]) == last_trimmed then
            candidates[#candidates + 1] = { i, j }
            break
          end
        end
      end
    end
  end

  if #candidates == 0 then
    return {}
  end

  if #candidates == 1 then
    local c = candidates[1]
    local count = c[2] - c[1] + 1
    local block = table_slice(content_lines, c[1], count)
    local sim = middle_similarity(block, search_lines)
    if sim >= SINGLE_CANDIDATE_THRESHOLD then
      return { table.concat(block, "\n") }
    end
    return {}
  end

  local best_block, best_sim = nil, -1.0
  for _, c in ipairs(candidates) do
    local count = c[2] - c[1] + 1
    local block = table_slice(content_lines, c[1], count)
    local sim = middle_similarity(block, search_lines)
    if sim > best_sim then
      best_block, best_sim = block, sim
    end
  end

  if best_sim >= MULTI_CANDIDATE_THRESHOLD then
    return { table.concat(best_block, "\n") }
  end
  return {}
end
```

### 5. Multi-Stage Match Cascade & Ambiguity Guard (`plugins/lib/maki/fuzzy_replace.lua:549-623`)
```lua
local REPLACERS = { exact, line_trimmed, block_anchor, whitespace_normalized, indentation_flexible }
local LATE_REPLACERS = { trimmed_boundary, context_aware }

-- ... in M.replace():
  local function try_match(candidates, find, replacement)
    for _, matched in ipairs(candidates) do
      local at = occurrences(content, matched)
      if #at > 0 then
        any_found = true
        if replace_all or #at == 1 then
          local text = replacement
          if all_start_a_line(content, at) then
            text = reindent(matched, find, replacement)
          end
          return splice(content, at, #matched, text)
        end
      end
    end
    return nil
  end
```

---

## Architecture & Data Flow

```text
[ read(path, offset, limit) ]
           │
           ├─► maki.fs.read(path)
           ├─► ctx:record_read(path) ──────────────► FileReadTracker.record_read() [stores mtime]
           ├─► truncate_bytes(500B UTF-8 safe)
           └─► Format prompt output: "<line_nr>: <line_text>" + Truncation notices
                     │
                     ▼
          [ Model Generates Edit ]
                     │
                     ▼
[ edit(path, old_string, new_string) ]
           │
           ├─► ctx:check_before_edit(path) ────────► FileReadTracker.check_before_edit()
           │                                            └─► If current mtime != recorded mtime: REJECT (stale read)
           ├─► maki.fs.read(path) [reads fresh file]
           ├─► fuzzy_replace.replace(content, old, new)
           │     ├─► 1. exact
           │     ├─► 2. line_trimmed
           │     ├─► 3. block_anchor (Levenshtein middle distance between 1st & last lines)
           │     ├─► 4. whitespace_normalized
           │     ├─► 5. indentation_flexible
           │     ├─► 6. escape_normalized (unescapes \n, \", etc.)
           │     ├─► 7. trimmed_boundary
           │     ├─► 8. context_aware (>=50% matching intermediate lines)
           │     ├─► Uniqueness Check: if #matches > 1 and not replace_all: REJECT (MULTIPLE_MATCHES)
           │     └─► reindent(): rebases new_string indentation to match file frame
           ├─► maki.fs.write(path, after)
           └─► ctx:record_read(path) [updates recorded mtime]
```

### Specific Technical Breakdown

1. **What is Hashed / Hash Size / Algorithm:**
   - **File and line content are not hashed.** There are no line hashes, mini-hashes, or line checksums in Maki's read/edit tools.
   - Line numbers in prompt output are plain decimal integers.
   - File staleness tracking records standard OS `SystemTime` modification timestamps (`mtime`) in a `Mutex<HashMap<PathBuf, SystemTime>>`.
   - *(Note: SipHash / 64-bit DefaultHasher is only used in Maki for tool dispatch deduplication in `tool_dispatch.rs` and UI streaming cache invalidation in `streaming_content.rs`).*

2. **How the Model Supplies Edits / Line & Region Identification:**
   - For `edit` / `multiedit`: The model supplies raw string snippets (`old_string` and `new_string`). The system prompt explicitly instructs: *"When copying text from read output, do NOT include the line number prefix (e.g. `42: `) - only the content after it."*
   - For `edit_lines` / `insert_lines`: The model supplies 1-based integer line indices (`start`, `end`, `line`).
   - Regions are identified by content search (finding `old_string` uniquely in the target file).

3. **Collisions, Duplicates, and Line Movement:**
   - **Duplicates / Ambiguity:** In `fuzzy_replace.lua:602-613`, every candidate match finds all occurrences. If `replace_all == false` and more than one instance of `matched` is found, the replacement fails with `M.MULTIPLE_MATCHES` (`"old_string matches multiple locations; add surrounding context to make it unique"`).
   - **Line Movement:** `edit` search is location-agnostic. If other edits or external additions shift lines up or down, `edit` still locates `old_string` accurately anywhere in the file as long as the content itself did not change.

4. **Distinction Between Subsystems:**
   - **`FileReadTracker`:** External/concurrency safety barrier. Compares canonicalized disk `mtime` at `edit` time against `read` time. Prevents overwriting external edits or concurrent tool edits without a fresh `read`.
   - **`fuzzy_replace`:** LLM tolerance layer. Forgives tokenization anomalies, escaped characters, trailing spaces, and indentation discrepancies without altering the exact string replacement contract.
   - **`block_anchor`:** A specific heuristic matcher within `fuzzy_replace` for blocks $\ge 3$ lines. It anchors on the trimmed first and last lines, then uses Levenshtein edit distance on interior lines to pick the best block.

5. **What Tests Establish:**
   - `plugins/edit/tests/spec.lua`: Verifies exact matching, boundary trimming, tab-to-space indentation conversion, non-leaking block indentation rebasing, `block_anchor` with modified interior lines, `block_anchor` candidate disambiguation, Unicode whitespace collapsing, escape normalization, and duplicate match rejection.
   - `maki-agent/src/tools/file_tracker.rs:75-144`: Verifies that untracked files pass, modified `mtime` triggers rejection, re-reading updates `mtime` and clears errors, deleted files are handled, and symlinks resolve to canonical paths.
   - `plugins/read/tests/spec.lua`: Verifies UTF-8 boundary-safe truncation and multi-platform newline splitting.

---

## Comparison: Maki Model vs Kiki (Exact Text in Persistent Python)

In Maki, the LLM acts as an error-prone transcriber: it reads lines formatted with line number prefixes, must strip prefixes mentally, serialize `old_string` into JSON tool call payloads, and handle indentation whitespace. This transcription vulnerability is why Maki needs `fuzzy_replace`, `block_anchor`, and indentation rebasing.

When Kiki provides `read(path)` returning exact text directly into a persistent Python execution environment:

### Core Differences
1. **Zero Transcription Hallucination:** Python holds exact file content in memory (`content = read("foo.py")`). Python code can use exact slicing, regex, AST manipulation, or `.replace()` without passing string literals through the LLM tokenizer.
2. **Deterministic Region Editing:** Rather than relying on fuzzy string matching or brittle integer line numbers, Python scripts can slice by exact character offsets, functions, or AST nodes.
3. **In-Process Pre-Commit Verification:** Python can validate syntax (`ast.parse(new_content)`) or run tests against in-memory modifications before calling `write(path, new_content)`.

### Concrete Costs & Benefits in Kiki

| Concern | Maki Approach (Prompt-visible text + LLM JSON tool calls) | Kiki Context (Exact text in persistent Python) |
| :--- | :--- | :--- |
| **Token Cost** | High: Full file text must enter LLM context, and large `old_string`/`new_string` blocks must be generated in JSON. | **Low:** LLM writes concise Python transformation code; intermediate file contents stay in Python memory. |
| **Fuzzy Matching Complexity** | High: Requires 8 fallback replacers, Levenshtein distance calculations, and indentation drift solvers. | **Unnecessary for programmatic edits:** Exact string/AST manipulation in Python avoids all whitespace drift. *(Only needed if the LLM writes hardcoded string literals in Python scripts).* |
| **Stale-Write Protection** | Tracked via `FileReadTracker` (`mtime`). | **Still Required:** An `mtime` or content-hash check in Kiki's `write()` or `edit()` is still necessary to prevent overwriting disk changes made externally between the Python `read()` and `write()`. |
| **Line Number Drift** | Mitigated by global string search in `edit`; `edit_lines` remains brittle. | **Eliminated:** Python can split, mutate line arrays, and rejoin without line-number transcription errors. |

---

## Start Here
Open `/Users/joe/kiki/.research/maki/source/plugins/lib/maki/fuzzy_replace.lua` (lines 400–628) to inspect the 8-stage matching cascade and `block_anchor` implementation, followed by `/Users/joe/kiki/.research/maki/source/maki-agent/src/tools/file_tracker.rs` to see the `mtime` staleness implementation.

## Gaps
- None. Confirmed exhaustively across the entire Maki codebase (`.research/maki/source`) that no mini-hash, line-hash, or checksum mechanism exists in Maki's read/edit subsystems.

### Assistant · source line 260 · `c64e6554` · superseded branch

I checked both the pinned Maki checkout and current remote `main`. Neither uses mini-hashes.

Maki uses:

- Decimal line numbers in read output: `.research/maki/source/plugins/read/init.lua:109`
- `old_string` / `new_string` edits
- A fuzzy replacement cascade for whitespace, indentation, and approximate block anchors: `.research/maki/source/plugins/lib/maki/fuzzy_replace.lua:401`
- An `mtime` tracker to reject edits after an externally changed file: `.research/maki/source/maki-agent/src/tools/file_tracker.rs:36`

The fuzzy machinery exists because the model copies text from rendered, numbered tool output back into another tool call. Kiki keeps exact text in Python, so that transcription problem largely disappears.

Mini-hashes would add little here:

- They would pollute or wrap otherwise exact file content.
- Duplicate lines still need contextual disambiguation.
- They introduce a second addressing language alongside normal Python strings and slicing.
- Exact replacement already acts as a local optimistic-concurrency check.
- File-level revision protection may still be useful for whole-file `write`, but that is separate from line hashes.

**Which `edit` interface should Kiki use?**

**A.** `async def edit(path: str, old: str, new: str) -> None`  
_(recommended)_ Require `old` to occur exactly once in the current file, then replace atomically. External changes elsewhere are preserved; changes to the target region make the edit fail.

**B.** Line ranges plus mini-hash preconditions  
Useful for rendered line-oriented tools, but duplicates Python’s existing string and list operations.

**C.** Remove `edit` and require `read` → transform → `write`  
Smallest tool set, but every targeted change becomes a whole-file overwrite.

Which edit model should we use?

### User · source line 261 · `6f2ceaa8` · active branch

A seems good for read

### Assistant · source line 264 · `b69d9924` · active branch

**What overwrite guarantee should `write` provide?**

The minimal signature can remain:

```python
async def write(path: str, content: str) -> None:
    ...
```

It should create parent directories and commit atomically.

**A.** Existing files must have been read and remained unchanged  
_(recommended)_ No extra parameter. Prevents Kiki from replacing unseen or externally modified content. New files write directly.

**B.** Always replace existing files  
Simplest implementation, but permits stale or accidental whole-file overwrites.

**C.** Create new files only  
Safest narrow meaning, but forces awkward edits when complete replacement is appropriate.

Which guarantee should `write` use?

### User · source line 265 · `00319e14` · active branch

A, but we would need to track that _during_ a code execution run: reads and writes could be pipes together or orchestrated some other way. Whole file writes should still definitely require a read that's not stale in this or a previous run, but not sure that a mid run edit should invalidate edits after that. Am I confusinf write and edit?

### Assistant · source line 268 · `c5a8bd00` · active branch

You’re separating them correctly. The distinction is:

- **`write`** replaces the complete file. It needs a previously observed version because it could erase unseen changes.
- **`edit`** reads the current file internally and applies an exact replacement. It preserves unrelated current content, so its `old` text can act as the precondition.

The observation tracker should persist across Arena executions and Runs for the Agent:

1. Successful `read` records the file’s content fingerprint.
2. Successful `edit` or `write` advances that fingerprint to the new content.
3. Failed mutations do not change it.
4. External changes, including changes made by `run`, cause the next whole-file `write` to reject the stale fingerprint.

A mid-execution edit therefore **does not invalidate** later work. It establishes the new observed version.

Dependent calls must use sequential awaits:

```python
content = await read("src/main.rs")
await write("src/main.rs", transform(content))
```

A read and write placed in the same `asyncio.gather()` are concurrent; the read must not implicitly authorise the write. The prompt should reserve `gather` for independent operations.

**Should `edit` require a prior `read` too?**

**A.** No, its exact unique `old` match is sufficient  
_(recommended)_ It preserves unrelated current content and fails without mutation when the precondition no longer matches.

**B.** Yes, both `edit` and `write` require an observed current version  
Safer but blocks useful edits based on `grep` results or already-known exact text.

Which rule should `edit` use?

### User · source line 269 · `beae3d85` · active branch

A

### Assistant · source line 272 · `f526cb1d` · active branch

**What should one `edit` call mean?**

**A.** One exact unique replacement

```python
async def edit(path: str, old: str, new: str) -> None:
    ...
```

_(recommended)_ Minimal common case. Zero or multiple matches fail without changing the file. For several related transformations, the agent can `read`, transform in Python, then `write` the complete observed file atomically.

**B.** An atomic list of replacement records

```python
async def edit(path: str, edits: list[Replacement]) -> None:
    ...
```

Supports several sequential replacements with all-or-nothing behaviour, but makes every simple edit verbose.

**C.** Accept either one replacement or a list  
Convenient, but creates an irregular interface and more validation branches.

Which `edit` interface should we use?

### User · source line 273 · `c29c86a4` · active branch

A

### Assistant · source line 276 · `405790b6` · active branch

**What should `list_dir` return?**

It should remain non-recursive. `glob` handles traversal.

**A.** Minimal `DirectoryEntry` records

```python
@dataclass
class DirectoryEntry:
    path: str
    kind: str
    size_bytes: int | None

async def list_dir(path: str = ".") -> list[DirectoryEntry]:
    ...
```

_(recommended)_ Workspace-relative paths pass directly to other tools. `kind` distinguishes `file`, `directory`, `symlink`, and `other`; size applies only to regular files. Results are stably sorted.

**B.** Records containing only `path` and `is_dir`  
Smaller, but loses symlink/other distinctions and file-size guidance.

**C.** Plain path strings, with `/` marking directories  
Smallest result, but makes machine-oriented filtering less explicit.

Which result should `list_dir` use?

### User · source line 277 · `21d0e65c` · active branch

A, are there any other attributes that would be useful?

### Assistant · source line 280 · `1a3147a9` · active branch

I’d stop at those three.

Possible additions, but none earns its place initially:

- `name`: derivable from `path`.
- `extension`: derivable in Python.
- `hidden`: derivable from the basename.
- modification time: unstable and encourages freshness heuristics.
- permissions or executable status: platform-specific.
- symlink target: potentially exposes host paths and needs a clearer symlink policy.
- observed file version: internal safety state, not agent-facing data.

`path`, `kind`, and `size_bytes` answer the useful questions: what can I call another tool with, what is it, and is it sensible to read?

**What traversal policy should `glob` expose?**

**A.** Fixed project-aware defaults

```python
async def glob(pattern: str, path: str = ".") -> list[str]:
    ...
```

_(recommended)_ Respect ignore files, exclude hidden paths, apply the pattern beneath `path`, and return stable workspace-relative paths. `list_dir` or `run` handles exceptional inspection.

**B.** Add `hidden` and `ignored` boolean options  
More complete, but widens a common interface for uncommon cases.

**C.** Search the whole workspace with only `pattern`  
Smallest signature, but makes scoped monorepo searches unnecessarily expensive.

Which traversal policy should `glob` use?

### User · source line 281 · `034dc2ad` · active branch

A, Note that I imagine we make sure to keep access to within and below the current directory that kiki is running in and not arbitrary paths on the host

### Assistant · source line 284 · `07a5a14e` · active branch

Yes. That becomes a shared invariant:

- All tool paths are workspace-relative.
- Absolute paths and `..` are rejected.
- Symlinks cannot escape the launch directory.
- Returned paths are workspace-relative.
- The same root capability is shared by every filesystem tool.

`run` is the difficult case. Validating its `cwd` is insufficient because a command could still read `/etc`, `$HOME`, or `../`. It needs a real process sandbox.

**What filesystem policy should `run` use?**

**A.** Workspace read/write, required system and toolchain files read-only, private temporary directory, everything else denied  
_(recommended)_ Build tools remain usable without exposing arbitrary user files.

**B.** Fully isolated container containing only the workspace and bundled tools  
Stronger isolation, but substantially heavier and complicates local toolchains.

**C.** Validate only `cwd` and trust the command  
Simplest, but does not satisfy your containment requirement.

Which policy should `run` use?

### User · source line 285 · `d1f18367` · active branch

C, I think that's fine for now

### Assistant · source line 288 · `45c0b237` · active branch

Understood. Filesystem tools remain capability-confined, but `run` is an explicit escape hatch with normal host permissions. Its generated docstring and the spec must state that plainly.

**What should `grep` expose?**

**A.** Regex, search path, and optional file glob

```python
@dataclass
class GrepMatch:
    path: str
    line: int
    text: str

async def grep(
    pattern: str,
    path: str = ".",
    file_glob: str | None = None,
) -> list[GrepMatch]:
    ...
```

_(recommended)_ Regex flags such as `(?i)` stay in the pattern. Context can be obtained with `read`. Results are complete up to a fixed safety cap; exceeding it raises an error asking for a narrower query.

**B.** Add `literal`, `ignore_case`, `context`, and `limit` options  
More familiar to direct-tool agents, but much of it duplicates Python and regex capabilities.

**C.** Search only one file per call  
Smallest host implementation, but forces excessive `glob` plus `gather` orchestration.

Which `grep` interface should we use?

### User · source line 289 · `5111be15` · active branch

A

### User · source line 293 · `584f1d67` · active branch

Also worth noting that we don't need to be explicit in the prompt for `run` that it's an excape hatch, and instead add directives to keep things local. That is best effort for the moment.

### Assistant · source line 296 · `af2cccb7` · active branch

Agreed. The generated `run` docstring should describe project-local use, while shared prompt guidance says commands must keep their reads and effects within the project. The implementation limitation belongs in the spec and user documentation, not as an advertised capability.

**What should the one-shot `run` interface be?**

**A.** Shell command with structured result

```python
@dataclass
class CommandResult:
    exit_code: int | None
    stdout: str
    stderr: str
    timed_out: bool
    output_truncated: bool

async def run(
    command: str,
    cwd: str = ".",
    timeout_seconds: int = 120,
) -> CommandResult:
    ...
```

_(recommended)_ Supports pipes, redirects, `&&`, Git, and project scripts. Non-zero exits return normally for inspection; launch failures raise exceptions. Timeout returns captured output with `timed_out=True`.

**B.** Argument vector only  
Avoids shell parsing, but makes normal coding workflows awkward.

**C.** Separate `run` and `run_shell` functions  
Clear distinction, but unnecessary surface for the initial agent.

Which `run` interface should we use?

### User · source line 297 · `f3ecfedb` · active branch

A

### Assistant · source line 302 · `6d342160` · active branch

**How should Rust schedule one gathered batch?**

**A.** Concurrent by default, with same-file mutations serialised in call order  
_(recommended)_ All calls start promptly. `write` and `edit` targeting the same normalised path queue deterministically; mutations to different files and all independent reads, searches, and commands run concurrently. Results resume Monty in its requested call-ID order.

**B.** Global mutation gate  
Any `write`, `edit`, or `run` excludes every other call in the batch. Safer, but undermines useful parallelism.

**C.** Fully concurrent  
Smallest scheduler, but same-file mutations can race or corrupt intent.

In every option, the prompt says `asyncio.gather()` is only for independent operations. Which scheduling policy should we use?

### User · source line 303 · `5cdb4e25` · active branch

A

### Assistant · source line 308 · `a93d1703` · active branch

**Should concurrent batches have a fixed execution ceiling?**

Arena code can generate hundreds or thousands of calls, so unconstrained spawning could exhaust file descriptors, memory, processes, or Tokio’s blocking pool.

**A.** Queue calls behind one fixed limit of 16 active tools  
_(recommended)_ Still genuinely concurrent, simple to explain, and applies uniformly. Same-file mutation ordering remains an additional constraint.

**B.** Separate limits for filesystem, search, and command tools  
More operational control, but adds policy and tuning before we have evidence.

**C.** Start every call immediately  
Simplest scheduler, but unsafe for model-generated comprehensions.

Which concurrency policy should we use?

### User · source line 309 · `1c5a9dce` · active branch

A

### Assistant · source line 312 · `ec8db098` · active branch

**How should collection tools report safety limits?**

This applies mainly to `glob`, `grep`, and `list_dir`.

**A.** Raise an explicit error instead of returning partial data  
_(recommended)_ Preserves the simple `list[...]` return types and prevents the agent mistaking truncated results for complete results. The error tells it to narrow the path or pattern.

**B.** Return result dataclasses containing `items` and `truncated`  
Retains partial data, but makes ordinary iteration and composition more verbose.

**C.** Silently truncate at the fixed cap  
Smallest interface, but can produce incorrect conclusions.

Which limit behaviour should we use?

### User · source line 313 · `ed8e19b4` · active branch

A

### Assistant · source line 318 · `09715375` · active branch

**Which shell should `run(command)` use?**

**A.** A fixed non-login system shell with Kiki’s inherited environment  
_(recommended)_ On Unix, `/bin/sh -c`. Predictable, supports pipes and redirects, and avoids executing user profile scripts.

**B.** The user’s configured shell as a login shell  
Better matches an interactive terminal, but profiles can alter commands, print output, access secrets, or fail unpredictably.

**C.** The user’s configured shell without login mode  
A compromise, but behaviour still varies with `$SHELL`.

Which shell policy should we use?

### User · source line 319 · `cf9d6f59` · active branch

A

### Assistant · source line 324 · `355ec8d6` · active branch

**How should the Arena execution timeout treat awaited tools?**

Kiki currently applies a 10-second wall-clock limit to the whole execution. That would cancel the proposed `run(..., timeout_seconds=120)` after ten seconds.

**A.** Keep a 10-second active-Python budget, excluding time awaiting Rust tools  
_(recommended)_ Tool calls use their own limits; `run` gets its requested timeout. User cancellation still aborts the whole execution and restores the checkpoint.

**B.** Replace it with one larger whole-execution wall timeout  
Simpler, but either cuts off valid commands or permits Python loops to run much longer.

**C.** Keep the current 10-second wall timeout  
No implementation work, but makes `run` unsuitable for most builds and tests.

Which timeout model should we use?

### User · source line 325 · `626a7634` · superseded branch

B

### User · source line 327 · `508c33c1` · active branch

Is it simple to do A? That sounds like a lot of complexity

## Extraction counts

- User messages: 29
- Assistant replies: 24
- Subagent results: 15
