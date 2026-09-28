# Review: `work/skills`

**Scope:** 12 feature/refactor commits on top of `main` (`d5212a0`). That's about 3.0k production lines, 4.7k test lines and 104 new tests. The spec and plan docs weren't read; this review is based on the code alone.

**Checks** (worktree, `MONTY_BIN` set):

- `cargo test --all-targets`: 1519 passed, 3 ignored, plus 14 passed
- `cargo clippy --all-targets -- -D warnings`: clean
- `cargo fmt --check`: clean

**Overall:** The core shape is good. There's one immutable catalog snapshot per process, each skill holds a directory handle, and each activation is captured, persisted and replayed along the session history. The security edges are handled carefully. The weak points are:

- the rule for which skills are active is written out in four places;
- making the catalog `Option` has doubled several APIs;
- a few policy edges look unintended.

---

## Should fix before merge

### 1. The model can read direct-only skills through `skill://`

`tools.rs:2226-2246`: `skill_directory` uses `catalog.lookup`, not `lookup_model_invocable`, and even has `unreachable!("direct-only skills are permitted as resources")`. So `read("skill://<name>/SKILL.md")` returns the full body of a `disable-model-invocation: true` skill without activating it. The catalog hides these names and there's a test that they don't leak, so this undercuts the flag.

Suggested fix: gate resources on "model-invocable, or activated in the current history".

### 2. Compaction counts project skill text twice in the normal case

`compaction.rs:337-366`: `uncertain_projection_before_baseline` is true whenever any project activation exists anywhere before the usage baseline. It then adds the full current skill text on top of a baseline that normally already includes it.

That covers every trusted, same-scope project skill, not just `--here` or Skip, so auto-compaction fires early by up to 64k tokens (256 KiB ÷ 4). No test covers this case; the only one covers the other-scope case.

### 3. The session's skill policy lets everything through by default

`session.rs:612/875/2262` store `Mutex<Option<SkillProjectionPolicy>>`, set after open. `None` is treated as allow-all at 2835, 4031 and 4125.

Production calls `configure_skill_projection` before restore today, but any future caller that forgets gets untrusted project instructions projected. Default to user-only, or pass the policy explicitly.

### 4. The persisted field names are misleading and about to become schema

`SkillActivation { source, body }`, and the same names in `StoredSkillActivation`:

- `source` is actually the skill's root directory.
- `body` is the whole `SKILL.md`, frontmatter included.

`Skill` calls these `canonical_root` and `source_text`. Rename them now; later it's a migration.

---

## Worth deciding

### 5. Skills dropped by the catalog budget can't be used directly either

`skills.rs:596-610` moves visible skills that don't fit into `rejected`. `dispatch_skill` uses `lookup`, so the 33rd skill alphabetically fails even for `/skill:`, and it's missing from completion. The budget arguably only needs to block `load_skill`.

### 6. Fallback is inconsistent

- If the nearer skill is invalid, the lookup quietly falls back to a farther one with the same name. For example, a typo in the project `review` hands the name to `~/.agents/skills/review`, and the only sign is a startup diagnostic.
- If the nearer name is ambiguous, there's no fallback.

Pick one rule. Recommendation: both block.

### 7. Direct activation doesn't show in the live transcript

Only the request text shows. A bare `/skill:review` shows a made-up sentence as if the user typed it. That sentence is hard-coded at `tui.rs:824`, not in `prompts/`. The tree shows `Skill: review`; the transcript doesn't.

### 8. The trust store can't be read by hand

Keys look like `unix:2f55…` (hex). Users can't see or remove trusted repos without decoding them, and there's no revoke path.

Each git worktree is also its own scope, so this repo's worktree-per-change workflow will get a prompt every time. Option: store UTF-8 paths as plain text and hex-encode only the non-UTF-8 ones.

---

## Smaller items

- **`pub mod skills` hides dead code.** Checked with a scratch crate: `pub` items inside a `pub mod` at the root of a binary crate don't get `dead_code` warnings. These are test-only:
  - `visible_names`, `direct_only_names`
  - `Skill::discovery_path()`
  - `ProjectScope::git_root_found` (a pub field)
  - most of the `MAX_*` constants, which don't need to be pub
- **`pub mod tests`** at `skills.rs:919`: nothing imports it.
- **Comments:** the doc comment at `skills.rs:90-91` refers to "Task 3" from the plan, and `compaction.rs:334-336` is a three-line comment.
- **`run_resume` cleanup:** the new `?` early returns (`main.rs:457-481`) skip `close_open_session`, unlike the paths around them.
- **Resume cost:** `probe_active_project_skill_in_scope` (`session.rs:2574`) loads every entry's full payload on each resume, and duplicates the row-loading code from restore.
- **Dead branch:** the `SkillActivation` branch in `context_bytes` (`compaction.rs:589`) can't be reached, because skill facts are never `Context`.
- **Dependency:** `yaml-rust` 0.4.5 is unmaintained (the RustSec advisory is probably RUSTSEC-2024-0320). `yaml-rust2` is the maintained fork, and the event pre-pass guard (anchors, depth) would carry over.
- **Trust locking:** a process `Mutex`, an fcntl lock file and an atomic rename, for a file written at most once per prompt. It's correct but heavy. Mutex poisoning maps to `InvalidStore` (`settings.rs:423-425`), which is misleading.
- **Non-unix branches:** the `cfg(not(unix))` branches in `skills.rs` can never compile. `session.rs` (already on main) and now `settings.rs` and `tui.rs` use unix APIs unconditionally, and `libc` is a unix-only dependency.
- **Prompt overrides differ:** `KIKI_SKILLS_CATALOG_PROMPT` strips one trailing newline; `KIKI_SYSTEM_PROMPT` doesn't.
- **`split_frontmatter`:** its first loop always breaks on the first pass, so it's really `split_inclusive('\n').next()`. `identified_name` also parses the YAML a second time.
- **Redundant branch:** the `if/else` in `run_persisted_with_skill` (`agent.rs:726-732`) picks between `commit_request_with_skill(.., Some(a))` and `commit_request`, which is just `commit_request_with_skill(.., None)`. Pass the `Option` straight through.
- **Skill wrapper:** the body goes verbatim into `<skill name=…>…</skill>`, so a body containing `</skill>` closes the wrapper early. Low risk given the trust gate.

---

## Abstractions

### New and good

- **`ProjectScope`**: pure and cheap, reads no `SKILL.md`. It lets the trust decision come before discovery, which is the right split.
- **`SkillCatalog`**: one immutable `Arc` snapshot. It makes "captured body, live resources" easy to reason about.
- **`Skill` holding an `Arc<Dir>`**: resource reads stay on the accepted directory even after a rename or swap, and that's tested.
- **`SkillActivation`**: validated on construction and on decode, so catalog input and database input pass one gate. The origin/trust-scope invariant is enforced there.
- **`ResourcePath`**: small and right.

### New or changed, questionable

- **The catalog also owns prompt text.** `CATALOG_HEADER` and `CATALOG_OMISSION` live in `skills.rs`, while all other prompt text is in `prompts/`. The budget depends on the rendered bytes, so keep the budgeting there but move the strings.
- **`SkillProjectionPolicy`** lives in `model.rs` and is stored as mutable state on `SessionState`. It isn't a model concern, and it creates ordering coupling on top of item 3.
- **`SkillLoadLedger`** builds a whole `ModelHistory` just to reuse the cap check. That's a symptom of the missing type below.
- **`Arena.last_skill_activations` plus `take_skill_activations()`** is a side-channel output. Returning `(Execution, Vec<SkillActivation>)` would be more direct. The likely motive is keeping skill bodies out of `Execution`'s `Debug` output and codec.
- **`ToolHost` lists `LoadSkill` but can't run it.** `call` returns a runtime error for it (`tools.rs:2005`), and the real dispatch is special-cased in the arena's `drive`. This is justified: an immediate, catchable Python error needs per-execution state.
- **`RuntimeCommand::SubmitSkill`** is a second variant that gets re-matched with `_ => unreachable!()` (`runtime.rs:1123`). `Submit { request, activation: Option<_> }` would do.
- **`PromptError` grew variants that each resolver can never produce**, so `config.rs:466` and `:766` need `unreachable!` arms. One `read_override(var) -> Result<String, OverrideError>` helper would remove both, and the copied read/UTF-8 code at `prompt.rs:182`.
- **The trust store sits in `config/settings.rs`**: about 160 lines unrelated to `config.toml`. It deserves its own module.

### Missed

1. **An `ActiveSkills` type for the "latest activation per name wins" rule.** The rule is written out four times:
   - `ModelHistory::activate_skill` (`model.rs:454`)
   - `restored_history`, twice (`session.rs:3974-4130`)
   - `probe_active_project_skill_in_scope` (`session.rs:2574`)
   - `compaction::active_skills_in_range` (`compaction.rs:388`)

   Plus the ledger. One ordered map with a cap-checked `activate()` and a `from_ancestry(iter, policy)` constructor would stop these drifting apart. This is the highest-value refactor.
2. **An empty catalog instead of `Option<Arc<SkillCatalog>>`.** The `Option` appears in `ToolHost`, `App`, `ArenaChildSource`, `production_model` and the subagents match. `rendered_catalog()` is already `None` when nothing is visible, so an empty catalog behaves the same. That removes the `*_with_catalog` function pairs. It also removes `production_system_prompt`, which gives exactly the same output as `root_prompt_with_catalog` (verified), plus the "no model-invocable skills" branch.
3. **A helper to open a directory safely inside its boundary.** `scan_root` and `read_candidate` repeat the same symlink check → canonicalise → boundary check → is-dir → open sequence, with the same diagnostics.
4. **One function to make a path safe for model text.** `prompt.rs:82` and `tools.rs:1850` filter control and bidi characters with *different* sets: tools also filters U+2028/2029. That's a latent bug.
5. **One `SkillLookupError` → `ToolError` mapping.** There are two near-identical matches in `tools.rs`.
6. **One helper for the `skill_activation` INSERT**, which is duplicated in `session.rs:1652` and `1759`.
7. **`_with_x` names plus test-only shims.** `run_persisted`, `settle_execution`, `execute_model_code_with_progress_and_context` and `restore_projection_at` became `#[cfg(test)]` wrappers so old tests compile. Production now has two names per operation. Extend the original signatures instead.
8. **A shared test fixture.** `SKILL.md`-writing boilerplate is repeated across about 9 modules. `skills.rs` already has a `Fixture`, but it's private despite `pub mod tests`.

---

## Tests

- **Strong edge coverage:** exact byte and line boundaries, CR line endings, symlink escapes, held handles after a swap, alias bombs, lock contention, and rollback via a trigger.
- **Long multi-scenario tests:** for example `skill_ancestry_selects_latest_version_before_trust_and_keeps_independent_user`, which is about 180 lines covering roughly 10 behaviours. When it fails, it won't say which behaviour broke.
- **Tests tied to prompt wording:** strings like `"cannot authorize effects beyond the user's task"` are asserted in `prompt.rs`, `main.rs` and `subagents.rs`, so editing the prompt breaks 4+ tests. Assert on structure instead.
- **Overclaiming names:** the `main.rs` tests called "fresh_and_resumed_*" just flip a bool on `startup_skill_catalog`; they never run `run_fresh` or `run_resume`.
- **Missing case:** no test for the trusted, same-scope compaction case from item 2.

---

## Suggested order

1. Decide the intent for #1 (direct-only resources).
2. #4 (field names), #3 (policy default), #2 (compaction).
3. The `ActiveSkills` type and the empty catalog; together these shrink most of the rest.
4. The smaller items.
