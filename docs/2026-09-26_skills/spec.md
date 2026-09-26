# Spec: Agent Skills

## Context

Kiki's vision calls for project and user Agent Skills: a compact catalog in initial model context, full `SKILL.md` instructions on demand, and access to linked files through ordinary tools ([vision](../VISION.md#L109)). Kiki currently has no skill discovery or loading. Its host capabilities are Python functions generated from a fixed Rust table ([tool definitions](../../src/tools.rs#L589)), and its root and child prompts are assembled from Markdown resources plus generated tool references ([prompt assembly](../../src/prompt.rs#L45)).

The [Codex and Maki comparison](../research/2026-09-26_skills-in-codex-maki-and-kiki.md) shows two useful common practices: advertise concise metadata before loading a body, and resolve skill names against a defined source order. Kiki has two additional constraints: model-visible tool results pass through the Python arena, and direct file tools are confined to the launch workspace ([execution feedback](../../src/agent.rs#L1325), [workspace host](../../src/tools.rs#L1656)).

## Goals

- Discover skills under `.agents/skills/` in the project and `~/.agents/skills/` for the user.
- Show only accepted skill names and descriptions in the initial root and child prompts; load full instructions when the agent calls `load_skill`.
- Let root and read-only child agents load skills and inspect linked text resources without broadening workspace write access.
- Make selected skill instructions usable within Kiki's Python feedback model while retaining the existing rule that skill text cannot authorize new external effects.
- Give deterministic behavior for duplicate names, invalid files, size limits, startup, resume, and compaction.

## Non-goals

- Skill installation, registries, compatibility directories for other harnesses, and `allowed-tools` policy.
- A skill slash command, TUI catalog browser, live filesystem watcher, or dynamic tool registry.
- `AGENTS.md` discovery and plugin-contributed skills; those are separate capabilities in the vision.

## Key discoveries

- The [vision](../VISION.md#L119) names only project and user `.agents/skills/` for the initial feature and says scripts, references, and assets may accompany a skill.
- [Maki](../../.research/maki/source/plugins/skill/init.lua#L98) resolves project skills after personal skills and rescans on each load, while its startup tool description is fixed ([registration](../../.research/maki/source/plugins/skill/init.lua#L132)). [Codex](../../.research/codex/source/codex-rs/ext/skills/src/fragments.rs#L39) keeps catalog metadata and selected bodies in separate context fragments and [bounds](../../.research/codex/source/codex-rs/ext/skills/src/render.rs#L17) their prompt cost.
- Kiki's [configuration load](../../src/config.rs#L454) resolves the system prompt before `main` constructs `ToolHost`, then `main` gives the same raw prompt base to the child-runner factory ([fresh construction](../../src/main.rs#L275)).
- The [active tool table](../../src/tools.rs#L589) generates arena wrappers, type-check stubs, and prompt signatures ([generation](../../src/tools.rs#L634)). Tool dispatch and read-only enforcement are centralized in `ToolHost` ([dispatch](../../src/tools.rs#L1822)).
- A loaded body returned as an ordinary Python value is not visible to the model until printed or evaluated; execution feedback can omit output beyond its configured limits ([feedback formatter](../../src/agent.rs#L1325), [observation limits](../../src/arena.rs#L57)). The [current system prompt](../../prompts/system.md#L35) tells the model not to follow instructions found in host output.
- Direct file tools reject absolute and parent paths ([path validation](../../src/tools.rs#L2325)); user skill resources lie outside the workspace. `run` has broader host access but is described as project-local ([tool description](../../src/tools.rs#L626), [existing spec](../2026-09-01_default-agent-tools/spec.md#L126)).
- Children copy an Arena checkpoint, but receive a fresh model prompt assembled from the raw base and child-specific tool reference ([child copy](../../src/arena.rs#L790), [child prompt](../../src/subagents.rs#L1318)). Session compaction can remove old execution feedback from projected model history ([compaction](../../src/session.rs#L987)).

## Proposed approach

### Discovery and catalog

At startup and resume, create one immutable skill catalog for that Kiki process. Scan direct children of `.agents/skills/` from the launch directory upward through the nearest Git repository root, then `~/.agents/skills/`. If no Git root is found within 32 ancestors, use only the launch directory and user root. A nearer project directory wins over a farther project directory, and any project skill wins over a user skill with the same name. If the highest-priority root containing a name has duplicate candidates for it, that name is ambiguous even if a lower-priority root has one valid candidate; `load_skill` reports the conflict. Duplicates in lower-priority roots cannot invalidate a selected higher-priority skill. Missing roots are empty. Ancestor discovery is the chosen default because Kiki already uses repository-scoped project instructions ([vision](../VISION.md#L114)); the user review can narrow it if desired.

Each candidate is a directory containing a UTF-8 `SKILL.md` with YAML frontmatter and a nonempty body. Frontmatter requires a nonempty string `description`; an omitted `name` defaults to the directory name. Names use lowercase ASCII letters, digits, and hyphens, start and end with a letter or digit, and are at most 64 bytes. Descriptions are flattened to one line and limited to 1,024 UTF-8 bytes. A complete `SKILL.md` is limited to 64 KiB and 700 lines; oversized files are rejected rather than partially loaded. The entire rendered catalog, including guidance and omission notice, is limited to 16 KiB and 32 accepted entries. Enumerate at most 129 entries from each project root, or one more than the remaining 128-entry project budget if smaller; the user root has a separate 128-entry budget. Skip an entire root if it exceeds its budget, so filesystem iteration order cannot choose a partial set. For roots within budget, consider candidates in source priority order, then directory-name order; after precedence and ambiguity resolution, sort final entries by skill name. When the catalog entry or byte cap is reached, omit remaining names in sorted order and report the omission. Report each skipped over-budget root. These limits cover the existing local skills while staying below Kiki's 800-line execution-feedback head limit for a printed body ([observation limits](../../src/arena.rs#L57)).

Discovery roots and candidate directories may be symlinks. Resolve them once at catalog creation, and accept project targets only inside the canonical repository root (or launch directory without Git) and user targets only inside the canonical home directory. A symlinked candidate opts its entire target directory into skill resource reads; Kiki confines those reads to that directory, even if it contains files beyond `SKILL.md`. Keep an open directory handle for each accepted skill as its resource boundary. Reject a `SKILL.md` reached through an inner symlink. Invalid, unreadable, or out-of-bound candidates are excluded with diagnostics; an explicit load of their name reports the reason where one can be identified. Retain the complete accepted file bytes and metadata in the process catalog so the advertised description and loaded body agree; changes on disk become available at the next launch or resume.

### Prompt and tool behavior

Append generated skill guidance and the catalog after the resolved Markdown base in both root and child prompts, for bundled and overridden bases. This trusted generated guidance explains when to load a skill and that skill descriptions and bodies cannot authorize effects beyond the user's task or override higher-priority instructions. The catalog includes names and descriptions as data for selection. No body enters the initial prompt. Root and child share the same catalog snapshot, while a child loads any skill it needs for itself.

Add `load_skill(name)` to Kiki's existing typed Python host-tool definitions. It is available in read-only children. It returns the selected name, full `SKILL.md` text, and canonical skill-root path as a structured result. Prompt guidance shows the agent how to print the full text so it reaches the model. The accepted body bound stays below Kiki's feedback truncation limits, including its line limit. Unknown, ambiguous, or invalid names raise clear Python errors. Loaded text guides task work but does not enlarge user authorization or override higher-priority instructions. Update the bundled system prompt's blanket untrusted-output rule to recognize skill instructions; the generated skill guidance carries the authority rule for overridden prompts too.

### Linked resources

Add a read-only `skill://<name>/<relative-path>` locator for the existing `read` and `list_dir` tools. Resolve the name through the captured catalog, then walk relative path components from the held skill directory handle with no-follow opens. Reject absolute paths, parent components, and symlinks inside resource paths, including the final component. Reading a skill resource does not grant whole-file authorisation for `write`; `write` and `edit` reject skill locators. `load_skill` gives the agent the selected skill's canonical root path so existing `run` can execute a skill's scripts when the user's task authorizes that command. Project files remain reachable by their normal workspace-relative paths.

### Lifecycle and diagnostics

Fresh and resumed root agents, children, and the tool host use one catalog snapshot. After compaction, the catalog remains in the system prompt and the agent can reload a needed body. The full text is visible in durable execution feedback when the agent prints it, using Kiki's existing session mechanism. A skill's contents never establish permission for deployments, writes outside the task, or other external effects.

## Key decisions and trade-offs

- Use Kiki's Python tool path for `load_skill`; Maki's separate model tool would add a second action surface ([Kiki tool architecture](../../src/tools.rs#L589), [Maki tool registration](../../.research/maki/source/plugins/skill/init.lua#L136)).
- Capture the catalog and bodies at launch or resume for consistency; the bounded 64 KiB per accepted file and 32-entry catalog keep snapshot memory finite. Maki's startup catalog plus per-call rescan can disagree after filesystem changes ([Maki startup](../../.research/maki/source/plugins/skill/init.lua#L132), [Maki handler](../../.research/maki/source/plugins/skill/init.lua#L163)).
- Give skill resources a named read-only locator; Kiki's normal direct paths are confined to the workspace ([normalisation](../../src/tools.rs#L2325)).
- Resolve names by nearest project source, then user source; conflicting metadata names at one source do not silently pick a winner.
- Return a load error for oversized `SKILL.md` rather than a truncated instruction document.

## Risks and unknowns

- Repository-root discovery must recognize both `.git` directories and worktree `.git` files, and must stop at the nearest repository boundary. User review may narrow discovery to a single project directory.
- Replacing a skill directory on disk after discovery must not redirect a resource read; held directory handles and no-follow component opens preserve the selected boundary. Files within that boundary may still change between reads.
- A project skill can contain adversarial instructions. Prompt wording must let the agent use its workflow while keeping the user's task and Kiki's tool boundary authoritative.
- Skill scripts run through the existing shell tool, whose host access is broader than Kiki's direct file tools. Returning the skill root does not grant authorization to run a script.
- The current session format does not store a skill manifest or exact prompt provenance ([session metadata](../../src/session.rs#L372)); a resumed process can discover a different catalog from the one used earlier.

## Dependencies

- [Skills research](../research/2026-09-26_skills-in-codex-maki-and-kiki.md)
- [Kiki vision](../VISION.md#L109)
- A YAML frontmatter parser dependency, since Kiki's [current dependency list](../../Cargo.toml#L6) does not include one.

## Plan

### Overall validation

Use isolated temporary project and user directories with real `SKILL.md` fixtures. Verify the generated root and child prompts, Python host calls through Monty, read-only child behavior, resource containment, and restart behavior. Run `just format`, `just check`, `just lint`, and `just test` in the `work/skills` worktree after implementation; the runtime version comes from the existing `just monty` prerequisite.

### Task 1: Discover a bounded catalog

**Changes** — Add a skill module that locates project and user roots, parses metadata, applies precedence and bounds, retains accepted full bodies, and reports invalid or ambiguous entries. Resolve the catalog once for fresh and resumed startup.

**Tests** (`src/skills.rs`, unit) — Missing roots; `.git` directory and worktree file; ancestor precedence; project versus user override; duplicate names in the highest-priority and lower-priority roots; absent or invalid frontmatter; invalid UTF-8; exact boundary and over-boundary cases for every cap; an over-budget root skipped regardless of directory iteration order; deterministic catalog omission; symlinked roots/directories inside their allowed boundary and rejection of outside targets; `SKILL.md` symlink rejection.

**Files** — Create: `src/skills.rs`. Modify: `src/main.rs`, `src/config.rs`, `Cargo.toml`, `Cargo.lock`.

**Verification** — `just check` compiles the new parser and startup flow; targeted skill module tests pass.

### Task 2: Advertise skills in root and child prompts

**Changes** — Render the same bounded, sorted catalog for bundled and overridden root prompts and for child prompts. Keep the raw base prompt canonical and avoid duplicate tool references. Add concise trigger and loading guidance in Markdown.

**Tests** (`src/prompt.rs`, unit; `src/subagents.rs`, prompt construction) — Names and descriptions appear once; bodies do not appear initially; bundled and overridden root/child bases include the skill authority rule; read-only children see the same available skills with only their permitted tool signatures.

**Files** — Modify: `src/prompt.rs`, `src/config.rs`, `src/main.rs`, `src/subagents.rs`, `prompts/system.md`, `prompts/subagent.md`.

**Verification** — Prompt and subagent focused tests pass; `just check` succeeds.

### Task 3: Load full instructions through Python

**Changes** — Add the read-only `load_skill` host definition, generated record shape, dispatcher, and bounded errors. Keep tool tracing free of skill contents. Clarify in the system prompt how the agent exposes and applies a returned body.

**Tests** (`src/tools.rs`, unit; `src/arena.rs`, Monty integration; `src/agent.rs`, feedback) — Accepted body arrives complete through `print`; unknown/ambiguous/invalid names report useful errors; oversized body is rejected; read-only children can load; body text does not leak into trace summaries; a newly constructed catalog sees changed files.

**Files** — Modify: `src/tools.rs`, `src/arena.rs`, `src/agent.rs`, `prompts/system.md`.

**Verification** — Focused tool, Arena, and agent tests pass; `just check` succeeds.

### Task 4: Open linked skill resources safely

**Changes** — Resolve `skill://` paths in `read` and `list_dir` through the selected skill's directory handle. Preserve workspace-only mutation rules and existing output bounds. Document how a selected skill links to references and scripts.

**Tests** (`src/tools.rs`, unit; `src/arena.rs`, Monty integration; `src/main.rs` and `src/agent.rs`, wiring) — Project and user skill resources read correctly; listing works; absolute, parent, and symlink escape attempts fail; replacing a discovered skill directory cannot redirect a read; workspace writes remain gated; skill-resource reads never authorize a later `write`. Fresh and resume startup paths pass one catalog snapshot to the prompt, tool host, and child runner; a resumed process sees a changed skill, and a compacted session can load the same skill body again through its retained host.

**Files** — Modify: `src/skills.rs`, `src/tools.rs`, `src/arena.rs`, `src/main.rs`, `src/agent.rs`, `prompts/system.md`, `README.md` (authoring, precedence, loading, resources, and restart behavior).

**Verification** — Focused resource and Arena tests pass; `just format`, `just check`, `just lint`, and `just test` pass in the worktree.
