# Spec: Agent Skills

## Context

Kiki's vision calls for project and user Agent Skills: a compact catalog in initial model context, full `SKILL.md` instructions on demand, and access to linked files through ordinary tools ([vision](../VISION.md#L109)). Kiki currently has no skill discovery or loading. Its host capabilities are Python functions generated from a fixed Rust table ([tool definitions](../../src/tools.rs#L589)), and its root and child prompts are assembled from Markdown resources plus generated tool references ([prompt assembly](../../src/prompt.rs#L45)).

The [Codex and Maki comparison](../research/2026-09-26_skills-in-codex-maki-and-kiki.md) shows two useful common practices: advertise concise metadata before loading a body, and resolve skill names against a defined source order. Kiki has two additional constraints: model-visible tool results pass through the Python arena, and direct file tools are confined to the launch workspace ([execution feedback](../../src/agent.rs#L1325), [workspace host](../../src/tools.rs#L1656)).

## Goals

- Discover skills under `.agents/skills/` in the project and `~/.agents/skills/` for the user.
- Ask before enabling project skills in an untrusted repository, and remember a positive trust decision.
- Show model-invocable skill names and descriptions in the initial root and child prompts; load full instructions when the agent calls `load_skill` or the user invokes `/skill:<name>`.
- Let root and read-only child agents load skills and inspect linked text resources without broadening workspace write access.
- Keep activated instructions in model context through compaction and resume while retaining the existing rule that skill text cannot authorize new external effects.
- Give deterministic behavior for duplicate names, invalid files, size limits, startup, resume, and compaction.

## Non-goals

- Skill installation, registries, compatibility directories for other harnesses, and `allowed-tools` policy.
- Query-based skill search or automatic relevance ranking; revisit these if local catalogs outgrow the prompt budget.
- A TUI catalog browser, live filesystem watcher, or dynamic tool registry.
- `AGENTS.md` discovery and plugin-contributed skills; those are separate capabilities in the vision.

## Key discoveries

- The [vision](../VISION.md#L119) names only project and user `.agents/skills/` for the initial feature and says scripts, references, and assets may accompany a skill.
- [Maki](../../.research/maki/source/plugins/skill/init.lua#L98) resolves project skills after personal skills and rescans on each load, while its startup tool description is fixed ([registration](../../.research/maki/source/plugins/skill/init.lua#L132)). [Codex](../../.research/codex/source/codex-rs/ext/skills/src/fragments.rs#L39) keeps catalog metadata and selected bodies in separate context fragments and [bounds](../../.research/codex/source/codex-rs/ext/skills/src/render.rs#L17) their prompt cost.
- Kiki's [configuration load](../../src/config.rs#L454) resolves the system prompt before `main` constructs `ToolHost`, then `main` gives the same raw prompt base to the child-runner factory ([fresh construction](../../src/main.rs#L275)).
- The [active tool table](../../src/tools.rs#L589) generates arena wrappers, type-check stubs, and prompt signatures ([generation](../../src/tools.rs#L634)). Tool dispatch and read-only enforcement are centralized in `ToolHost` ([dispatch](../../src/tools.rs#L1822)).
- A loaded body returned as an ordinary Python value is not visible to the model until printed or evaluated; execution feedback can omit output beyond its configured limits ([feedback formatter](../../src/agent.rs#L1325), [observation limits](../../src/arena.rs#L57)). The [current system prompt](../../prompts/system.md#L35) tells the model not to follow instructions found in host output.
- Direct file tools reject absolute and parent paths ([path validation](../../src/tools.rs#L2325)); user skill resources lie outside the workspace. `run` has broader host access but is described as project-local ([tool description](../../src/tools.rs#L626), [existing spec](../2026-09-01_default-agent-tools/spec.md#L126)).
- Children copy an Arena checkpoint, but receive a fresh model prompt assembled from the raw base and child-specific tool reference ([child copy](../../src/arena.rs#L790), [child prompt](../../src/subagents.rs#L1318)). Session compaction can remove old execution feedback from projected model history ([compaction](../../src/session.rs#L987)).
- Kiki's slash commands are currently a static catalogue ([parser](../../src/tui/slash.rs#L1)) and are dispatched before ordinary user submission ([TUI dispatch](../../src/tui.rs#L625)). Model context has no skill entry yet ([context entries](../../src/model.rs#L205)); both providers project each entry ([OpenAI](../../src/openai.rs#L125), [Anthropic](../../src/anthropic.rs#L115)).
- [Session compaction](../../src/session.rs#L1015) replaces history with a summary and recent entries; [restore](../../src/session.rs#L3690) reconstructs it from the selected ancestry. Skill instructions placed only in execution feedback can therefore disappear.

## Proposed approach

### Discovery and catalog

Before reading project skill files on fresh launch or resume, detect whether any project `.agents/skills/` directory is present from the launch directory to the nearest Git root within the same 32-ancestor discovery bound. On resume, an active project skill from that same scope also triggers the check, even if its directory is gone. If the canonical repository root has no saved positive trust decision, show a startup popup naming that root. The default choice skips project skills for this process; choosing Trust includes them and atomically saves the canonical root in a user-owned trust store. A saved positive decision keeps that scope trusted on later launches without another popup. A skipped or cancelled decision is not remembered, so Kiki asks again on the next launch. With no Git root inside the bound, check only the launch directory and use its canonical path as the trust scope. User skills need no repository trust. Project settings cannot grant it. A failed trust save leaves project skills disabled and shows the error. Resume with `--here` uses the effective new workspace for this check; activations from a different prior trust scope stay suppressed. No popup appears when neither a project skill directory nor an in-scope active project skill is present.

At startup and resume, create one immutable skill catalog for that Kiki process from the trusted roots. Scan direct children of `.agents/skills/` from the launch directory upward through the nearest Git repository root, then `~/.agents/skills/`. If no Git root is found within 32 ancestors, use only the launch directory and user root. A nearer project directory wins over a farther project directory, and any project skill wins over a user skill with the same name. If the highest-priority root containing a name has duplicate candidates for it, that name is ambiguous even if a lower-priority root has one valid candidate; activation reports the conflict. Duplicates in lower-priority roots cannot invalidate a selected higher-priority skill. Missing roots are empty. Ancestor discovery follows Kiki's repository-scoped project-instruction model ([vision](../VISION.md#L114)).

Each candidate is a directory containing a UTF-8 `SKILL.md` with YAML frontmatter and a nonempty body. Frontmatter requires a nonempty string `description`; an omitted `name` defaults to the directory name. `disable-model-invocation` is an optional boolean, defaulting to `false`; a true value makes the skill direct-invocation only. Names use lowercase ASCII letters, digits, and hyphens, start and end with a letter or digit, and are at most 64 bytes. Descriptions are flattened to one line and limited to 1,024 UTF-8 bytes. A complete `SKILL.md` is limited to 64 KiB and 700 lines; oversized files are rejected rather than partially loaded. Accept at most 32 model-invocable and 32 direct-only skills; direct-only skills do not consume prompt slots. The entire rendered catalog, including guidance and omission notice, is limited to 16 KiB. Enumerate at most 129 entries from each project root, or one more than the remaining 128-entry project budget if smaller; the user root has a separate 128-entry budget. Skip an entire root if it exceeds its budget, so filesystem iteration order cannot choose a partial set. For roots within budget, consider candidates in source priority order, then directory-name order; after precedence and ambiguity resolution, sort final entries by skill name. When a category's entry or prompt byte bound is reached, omit remaining names in sorted order and report the omission without exposing direct-only names to the model. Omitted skills cannot be activated in that process. Report each skipped over-budget root.

Discovery roots and candidate directories may be symlinks. Resolve them once at catalog creation, and accept project targets only inside the canonical repository root (or launch directory without Git) and user targets only inside the canonical home directory. A symlinked candidate opts its entire target directory into skill resource reads; Kiki confines those reads to that directory, even if it contains files beyond `SKILL.md`. Keep an open directory handle for each accepted skill as its resource boundary. Reject a `SKILL.md` reached through an inner symlink. Invalid, unreadable, or out-of-bound candidates are excluded with diagnostics; an explicit load of their name reports the reason where one can be identified. Retain the complete accepted `SKILL.md` bytes and metadata in the process catalog so the advertised description and loaded body agree; changed skill definitions or candidates become available at the next launch or resume. Linked resource contents are read live from the accepted directory and may change between reads.

### Prompt and tool behavior

Append generated skill guidance and the catalog after the resolved Markdown base in both root and child prompts, for bundled and overridden bases. The stable guidance lives in an overridable Markdown prompt resource and explains when to load a skill and that skill descriptions and bodies cannot authorize effects beyond the user's task or override higher-priority instructions. The catalog includes only accepted model-invocable names and descriptions as selection data. Direct-only names, descriptions, and even their count stay out of model-facing catalog text. If there are no model-invocable skills, omit the generated catalog section, its behavioral guidance, and the `load_skill` signature from the model tool reference; the fixed host function may still exist but has no permitted names. No body enters the initial prompt. Root and child share the same catalog snapshot, while a child loads any skill it needs for itself.

Add `load_skill(name)` to Kiki's existing typed Python host-tool definitions. It is available in read-only children and accepts only model-invocable catalog names. A successful call returns a compact acknowledgement with the selected name, resource locator, and canonical skill-root path; Kiki inserts the captured full `SKILL.md` as a separate, named model-context entry after the settled tool execution. The agent does not need to print the body, and tool feedback and trace summaries do not repeat it. Unknown, ambiguous, invalid, or direct-only names raise clear Python errors. A separate Markdown prompt resource supplies the entry's wrapper, identifying the skill and stating that it cannot enlarge user authorization or override higher-priority instructions. The entry is distinct from host output, so the bundled prompt's existing host-output rule remains accurate, including when no skill catalog is shown.

Intercept `/skill:<name> [request]` in the TUI before ordinary submission. The first token is an exact accepted name from the same snapshot, including direct-only skills; text after whitespace, including newlines, is the optional request. Unknown or rejected names show a notice and do not submit. Completion lists available command tokens and descriptions for the user, including direct-only skills. The command activates the skill before running the request. A bare `/skill:<name>` starts a turn asking the assistant to use that skill, allowing it to request task details. The command text is not used as an instruction to make the model call `load_skill`.

### Linked resources

Add a read-only `skill://<name>/<relative-path>` locator for the existing `read` and `list_dir` tools. Resolve the name through the captured catalog, then walk relative path components from the held skill directory handle with no-follow opens. Reject absolute paths, parent components, and symlinks inside resource paths, including the final component. Reading a skill resource does not grant whole-file authorisation for `write`; `write` and `edit` reject skill locators. `load_skill` gives the agent the selected skill's canonical root path so existing `run` can execute a skill's scripts when the user's task authorizes that command. Project files remain reachable by their normal workspace-relative paths.

### Lifecycle and diagnostics

Fresh and resumed root agents, children, and the tool host use one catalog snapshot. A successful root activation persists its name, captured full `SKILL.md` text, origin, and trust scope in session ancestry. Each branch's latest activation for a name determines that name's selected version, regardless of current trust; both providers project at most one permitted entry per name. Repeating an activation with unchanged source, text, origin, and trust scope is a no-op; if any of those fields differs, explicit activation replaces that name's projected body on the branch. Branching before an activation does not inherit it. Compaction retains active entries outside the summarised history, includes their cost in context accounting, and does not duplicate their bodies. Resume restores the exact captured text of each permitted selected version from the selected branch, even if the current catalog has changed. Trusting a repository does not replace saved text with the current file; another explicit activation can select the changed snapshot and record a new version. A child uses the same activation and deduplication rules within its in-memory run; it does not inherit a parent's activated instructions automatically.

Limit the combined active instruction text to 256 KiB per model context. If another activation would exceed that bound, report a clear error and leave the active set unchanged. On resume and branch navigation, select the latest activation for each name before applying the current trust rule. A project-origin active entry reaches the model only when its recorded trust scope matches the effective workspace scope and that scope is trusted for this process. Otherwise, omit that name from the active context without reviving an older user or project activation; the older record remains in session ancestry. Skipping trust prevents project skill resource reads, while leaving independently active user skills available. A skill's contents never establish permission for deployments, writes outside the task, or other external effects.

## Key decisions and trade-offs

- Advertise the small local catalog's names and descriptions up front, following the [Agent Skills progressive-disclosure pattern](https://agentskills.io/specification#progressive-disclosure). Defer search until catalog size makes the prompt budget a practical limit; adding search then requires a searchable index separate from the rendered prompt cap.
- Use Kiki's Python tool path for `load_skill`; Maki's separate model tool would add a second action surface ([Kiki tool architecture](../../src/tools.rs#L589), [Maki tool registration](../../.research/maki/source/plugins/skill/init.lua#L136)).
- Keep `disable-model-invocation: true` skills out of model-facing discovery and `load_skill`; the explicit `/skill:<name>` command can still activate them. The TUI can show direct-only names to the user.
- Deliver activated instructions in a dedicated durable model-context entry. Returning them only as Python output would require printing, is subject to feedback truncation, and can be discarded by compaction.
- Resolve the active version by branch ancestry before applying repository trust. This preserves the version actually active in the conversation and prevents a skipped project skill from reviving an older same-name user skill.
- Remember only positive repository trust in user-owned settings. A skipped project is offered again next launch; the project cannot approve itself.
- Capture the catalog and bodies at launch or resume for consistency; the bounded 64 KiB per accepted file and 64-entry combined cap keep snapshot memory finite. Maki's startup catalog plus per-call rescan can disagree after filesystem changes ([Maki startup](../../.research/maki/source/plugins/skill/init.lua#L132), [Maki handler](../../.research/maki/source/plugins/skill/init.lua#L163)).
- Give skill resources a named read-only locator; Kiki's normal direct paths are confined to the workspace ([normalisation](../../src/tools.rs#L2325)).
- Resolve names by nearest project source, then user source; conflicting metadata names at one source do not silently pick a winner.
- Return a load error for oversized `SKILL.md` rather than a truncated instruction document.

## Risks and unknowns

- Repository-root discovery must recognize both `.git` directories and worktree `.git` files, and must stop at the nearest repository boundary.
- Replacing a skill directory on disk after discovery must not redirect a resource read; held directory handles and no-follow component opens preserve the selected boundary. Files within that boundary may still change between reads.
- A project skill can contain adversarial instructions. Prompt wording must let the agent use its workflow while keeping the user's task and Kiki's tool boundary authoritative.
- Skill scripts run through the existing shell tool, whose host access is broader than Kiki's direct file tools. Returning the skill root does not grant authorization to run a script.
- The current session format does not store a skill manifest or exact prompt provenance ([session metadata](../../src/session.rs#L372)); a resumed process can discover a different catalog from the one used earlier.
- A resumed process may have an older pinned `SKILL.md` body while linked resource files have changed on disk. Instructions remain reproducible on the selected branch; resource reads use the current accepted directory snapshot and may differ from the earlier files.

## Dependencies

- [Skills research](../research/2026-09-26_skills-in-codex-maki-and-kiki.md)
- [Kiki vision](../VISION.md#L109)
- A YAML frontmatter parser dependency, since Kiki's [current dependency list](../../Cargo.toml#L6) does not include one.

## Plan

### Overall validation

Use isolated temporary project and user directories with real `SKILL.md` fixtures. Verify trust on fresh and resumed startup, catalog and direct-only filtering, root and child prompts, Python and slash activation, provider projections, compaction and branching, resource containment, and restart behavior. Run `just format`, `just check`, `just lint`, and `just test` in the `work/skills` worktree after implementation; the runtime version comes from the existing `just monty` prerequisite.

### Task 1: Discover a bounded catalog

**Changes** — Add a skill module that locates project and user roots, parses metadata including `disable-model-invocation`, applies precedence and bounds, retains accepted full bodies, and reports invalid or ambiguous entries. Keep model-visible and direct-only entries separate within one snapshot.

**Tests** (`src/skills.rs`, unit) — Missing roots; `.git` directory and worktree file; ancestor precedence; project versus user override; duplicate names in the highest-priority and lower-priority roots; absent or invalid frontmatter and boolean opt-out; invalid UTF-8; exact boundary and over-boundary cases for each cap; direct-only skills do not consume prompt slots or reveal names in omission text; an over-budget root skipped regardless of directory iteration order; deterministic omission; symlinked roots/directories inside their allowed boundary and rejection of outside targets; `SKILL.md` symlink rejection.

**Files** — Create: `src/skills.rs`. Modify: `src/main.rs`, `Cargo.toml`, `Cargo.lock`.

**Verification** — Targeted skill module tests pass; `just check` compiles the new module.

### Task 2: Gate project skills at startup

**Changes** — Before constructing a fresh or resumed model and tool host, show a TUI popup for a project skill directory whose canonical trust scope lacks a saved positive decision. Save Trust only in user-owned settings; Skip applies for one process. Build one catalog from the resulting roots and retain it for the later prompt and tool wiring tasks. Configuration-only startup and session listing do not prompt.

**Tests** (`src/skills.rs` and `src/config/settings.rs`, unit; `src/tui.rs` and `src/main.rs`, startup) — No popup without project skills or for a trusted root; Trust survives the next launch; Skip and cancel re-prompt next launch; default is Skip; failed save cannot enable project skills; project configuration cannot self-approve; no-Git scope and worktree Git file resolve correctly; a skipped catalog excludes project skills while retaining user skills. Task 3 tests the active-entry resume gate and `--here` filtering; Tasks 4, 5, and 7 test the host, prompt, child, and resource wiring.

**Files** — Modify: `src/skills.rs`, `src/config/settings.rs`, `src/main.rs`, `src/tui.rs`.

**Verification** — Targeted trust and startup tests pass; `just check` succeeds.

### Task 3: Persist and project activated instructions

**Changes** — Add a named skill activation to model history and selected session ancestry, project it for both providers, deduplicate it by name on each branch, and keep its captured text through compaction and restore. Count protected text toward context usage and enforce the combined active-text bound. On restore, compaction, branch navigation, and pre-popup probing, resolve the latest activation per name before applying trust. Detect a latest in-scope project entry for the resume trust popup even if its directory was removed. Suppress an untrusted or out-of-scope latest project entry without exposing an older same-name activation.

**Tests** (`src/model.rs`, `src/openai.rs`, `src/anthropic.rs`, `src/session.rs`, `src/session/codec.rs`, `src/compaction.rs`, unit and session integration) — Full text appears once in each provider's next request; duplicate activation adds no second copy or session row and a changed source, text, origin, or trust scope replaces the old projection, including identical source/text with changed origin; an earlier branch excludes a later activation; compaction preserves active bodies outside summaries and charges their cost, including when usage was recorded under another `--here` scope; resume restores exact bytes despite changed or missing source files until explicit reactivation; saved positive trust plus a removed or changed project `SKILL.md` resumes with pinned text and no popup; a missing project skill directory triggers the popup only for a latest in-scope entry, while a later same-name activation from another scope or user source supersedes it; pre-popup detection remains independent of the unfiltered text cap and does not prevent damaged sessions from opening in their existing read-only view; skipped trust or a different `--here` scope omits the latest project-origin entry without restoring an older same-name user skill, including after compaction and branch navigation; an explicit same-name user activation after Skip becomes the new latest entry and independently active user skills remain available; boundary and over-boundary active-text totals return the documented result.

**Files** — Create: `prompts/skill-activation.md`. Modify: `src/model.rs`, `src/openai.rs`, `src/anthropic.rs`, `src/session.rs`, `src/session/codec.rs`, `src/compaction.rs`, `src/prompt.rs`, `src/main.rs`, `src/skills.rs`, `src/tui/tree.rs`.

**Verification** — Focused provider, session, and compaction tests pass; `just check` succeeds.

### Task 4: Activate model-selected skills through Python

**Changes** — Add the read-only `load_skill` host definition and dispatcher, and pass the process catalog to the host on fresh and resumed startup. Track selected skills in an execution-scoped ledger seeded from the branch's active set so multiple loads share the text limit. On successful, settled tool execution, persist the captured activations atomically before the next model step and return only a compact acknowledgement to Python. Keep bodies out of Python results, execution feedback, and traces. Only model-invocable names can enter through this path; record the new tool name in the session codec.

**Tests** (`src/tools.rs`, unit; `src/arena.rs` and `src/agent.rs`, integration) — Accepted body reaches the next model step without `print`; unknown, ambiguous, invalid, oversized, direct-only, and skipped-project names report useful errors; multiple loads in one Python cell honor the combined active-text limit and persist together or not at all; Python errors and cancelled execution do not activate even after a successful host call; read-only children can load model-invocable skills; trace summaries omit body text; unsafe root paths cannot inject acknowledgement text; a new process sees changed files for newly activated names.

**Files** — Modify: `src/skills.rs`, `src/tools.rs`, `src/arena.rs`, `src/agent.rs`, `src/session.rs`, `src/session/codec.rs`, `src/main.rs`.

**Verification** — Focused tool, Arena, and agent tests pass; `just check` succeeds.

### Task 5: Advertise the visible catalog

**Changes** — Render the same bounded, sorted model-invocable catalog for bundled and overridden root and child prompts. Omit the generated section and guidance entirely when it has no entries. Keep the raw base prompt canonical and avoid duplicate tool references.

**Tests** (`src/prompt.rs` and `src/tools.rs`, unit; `src/subagents.rs`, prompt construction) — Names and descriptions appear once; direct-only names and bodies do not appear; no visible skills produces no generated skill section, guidance, or model-facing `load_skill` signature; bundled and overridden root and child bases include the authority rule when applicable; read-only children see the same visible catalog with only their permitted tool signatures.

**Files** — Create: `prompts/skills-catalog.md`. Modify: `src/prompt.rs`, `src/tools.rs`, `src/config.rs`, `src/main.rs`, `src/subagents.rs`.

**Verification** — Focused prompt and subagent tests pass; `just check` succeeds.

### Task 6: Activate user-selected skills with `/skill:<name>`

**Changes** — Parse and complete dynamic `/skill:<name>` commands, including direct-only skills. Pass the exact selection through runtime handling so activation is persisted before the associated user request is run. Show clear notices for unknown or unavailable names. A bare command starts a turn using the skill.

**Tests** (`src/tui/slash.rs`, unit; `src/tui.rs` and `src/runtime.rs`, integration) — Completion includes accepted direct-only names but untrusted project names stay absent; command parsing respects exact names and optional request; bare and request forms activate before the model step; unknown or rejected names submit nothing; repeating an unchanged candidate is a no-op while an explicit changed candidate records its new captured text; navigation to a branch before invocation removes it.

**Files** — Modify: `src/tui/slash.rs`, `src/tui.rs`, `src/runtime.rs`, `src/session.rs`, `src/skills.rs`.

**Verification** — Focused slash, TUI, and runtime tests pass; `just check` succeeds.

### Task 7: Open linked skill resources safely

**Changes** — Resolve `skill://` paths in `read` and `list_dir` through the selected skill's directory handle. Preserve workspace-only mutation rules and existing output bounds. Document authoring, trust, loading, direct invocation, and linked resources.

**Tests** (`src/tools.rs`, unit; `src/arena.rs`, Monty integration; `src/main.rs` and `src/agent.rs`, wiring) — Project and user skill resources read correctly; listing works; absolute, parent, and symlink escape attempts fail; replacing a discovered skill directory cannot redirect a read; workspace writes remain gated; skill-resource reads never authorize a later `write`; live resource edits appear within one process while changed `SKILL.md` bodies need a new catalog; skipped trust denies project resources.

**Files** — Modify: `src/skills.rs`, `src/tools.rs`, `src/arena.rs`, `src/main.rs`, `src/agent.rs`, `README.md`.

**Verification** — Focused resource and Arena tests pass; `just format`, `just check`, `just lint`, and `just test` pass in the worktree.
