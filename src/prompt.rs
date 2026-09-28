use std::env;
use std::ffi::OsString;
use std::fs;
use std::io;
use std::path::Path;
use std::path::PathBuf;
use std::str::Utf8Error;

use crate::skills::{SkillCatalog, model_visible_path};
use crate::tools::prompt_reference;

const BUNDLED_PROMPT: &str = include_str!("../prompts/system.md");
const BUNDLED_CHILD_PROMPT: &str = include_str!("../prompts/subagent.md");
const COMPACTION_SYSTEM_PROMPT: &str = include_str!("../prompts/compaction-system.md");
const COMPACTION_INITIAL_PROMPT: &str = include_str!("../prompts/compaction-initial.md");
const COMPACTION_UPDATE_PROMPT: &str = include_str!("../prompts/compaction-update.md");
const COMPACTION_SPLIT_RUN_PROMPT: &str = include_str!("../prompts/compaction-split-run.md");
const SKILL_ACTIVATION_PROMPT: &str = include_str!("../prompts/skill-activation.md");
const SKILLS_CATALOG_GUIDANCE: &str = include_str!("../prompts/skills-catalog.md");
const SKILLS_CATALOG_HEADER: &str = include_str!("../prompts/skills-catalog-header.md");
const SKILLS_CATALOG_OMISSION: &str = include_str!("../prompts/skills-catalog-omission.md");
const BUNDLED_PROMPT_NAME: &str = "system";
const SYSTEM_PROMPT_ENV: &str = "KIKI_SYSTEM_PROMPT";
const SKILLS_CATALOG_PROMPT_ENV: &str = "KIKI_SKILLS_CATALOG_PROMPT";

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PromptSource {
    Bundled { name: &'static str },
    Override { path: PathBuf },
}

pub struct ResolvedPrompt {
    base: String,
    source: PromptSource,
}

impl ResolvedPrompt {
    pub(crate) fn base(&self) -> &str {
        &self.base
    }

    pub fn source(&self) -> &PromptSource {
        &self.source
    }
}

pub(crate) fn root_prompt(base: &str, catalog: &SkillCatalog) -> String {
    assemble(base, catalog, prompt_reference(catalog))
}

pub(crate) fn child_prompt(base: &str, catalog: &SkillCatalog, tool_reference: String) -> String {
    assemble(
        &format!("{base}\n\n{BUNDLED_CHILD_PROMPT}"),
        catalog,
        tool_reference,
    )
}

pub(crate) fn skills_catalog_guidance() -> &'static str {
    strip_file_final_lf(SKILLS_CATALOG_GUIDANCE)
}

pub(crate) fn skills_catalog_header() -> &'static str {
    strip_file_final_lf(SKILLS_CATALOG_HEADER)
}

pub(crate) fn skills_catalog_omission() -> &'static str {
    strip_file_final_lf(SKILLS_CATALOG_OMISSION)
}

pub(crate) fn skill_activation_text(
    name: &str,
    canonical_root: &Path,
    source_text: &str,
) -> String {
    let root = model_visible_path(canonical_root)
        .map_or_else(String::new, |path| format!("Skill root path: {path}\n"));
    format!(
        "{}\n\n{root}Skill resource locator: skill://{name}/\n\n<skill name=\"{name}\">\n{source_text}\n</skill>",
        strip_file_final_lf(SKILL_ACTIVATION_PROMPT),
    )
}

pub(crate) fn compaction_system_prompt() -> &'static str {
    strip_file_final_lf(COMPACTION_SYSTEM_PROMPT)
}

pub(crate) fn compaction_initial_prompt() -> &'static str {
    strip_file_final_lf(COMPACTION_INITIAL_PROMPT)
}

pub(crate) fn compaction_update_prompt() -> &'static str {
    strip_file_final_lf(COMPACTION_UPDATE_PROMPT)
}

pub(crate) fn compaction_split_run_prompt() -> &'static str {
    strip_file_final_lf(COMPACTION_SPLIT_RUN_PROMPT)
}

fn strip_file_final_lf(prompt: &str) -> &str {
    prompt
        .strip_suffix('\n')
        .expect("bundled prompt resources end in a line feed")
}

fn assemble(base: &str, catalog: &SkillCatalog, tool_reference: String) -> String {
    match catalog.rendered_catalog() {
        Some(catalog) => format!("{base}\n\n{catalog}\n{tool_reference}"),
        None => format!("{base}\n\n{tool_reference}"),
    }
}

#[derive(Debug, thiserror::Error)]
pub enum PromptError {
    #[error(
        "prompt override `{path}` could not be read: {source}",
        path = path.display()
    )]
    UnreadableOverride {
        path: PathBuf,
        #[source]
        source: io::Error,
    },
    #[error(
        "prompt override `{path}` is not valid UTF-8: {source}",
        path = path.display()
    )]
    InvalidUtf8Override {
        path: PathBuf,
        #[source]
        source: Utf8Error,
    },
}

pub fn resolve() -> Result<ResolvedPrompt, PromptError> {
    resolve_from_override(env::var_os(SYSTEM_PROMPT_ENV))
}

pub(crate) fn resolve_skills_catalog_guidance() -> Result<String, PromptError> {
    resolve_skills_catalog_guidance_from_override(env::var_os(SKILLS_CATALOG_PROMPT_ENV))
}

fn resolve_skills_catalog_guidance_from_override(
    override_path: Option<OsString>,
) -> Result<String, PromptError> {
    let Some(override_path) = override_path else {
        return Ok(skills_catalog_guidance().to_owned());
    };
    read_override(Path::new(&override_path))
}

fn resolve_from_override(override_path: Option<OsString>) -> Result<ResolvedPrompt, PromptError> {
    let Some(override_path) = override_path else {
        return Ok(ResolvedPrompt {
            base: BUNDLED_PROMPT.to_owned(),
            source: PromptSource::Bundled {
                name: BUNDLED_PROMPT_NAME,
            },
        });
    };

    let path = PathBuf::from(override_path);
    Ok(ResolvedPrompt {
        base: read_override(&path)?,
        source: PromptSource::Override { path },
    })
}

fn read_override(path: &Path) -> Result<String, PromptError> {
    let bytes = fs::read(path).map_err(|source| PromptError::UnreadableOverride {
        path: path.to_owned(),
        source,
    })?;
    let text = std::str::from_utf8(&bytes).map_err(|source| PromptError::InvalidUtf8Override {
        path: path.to_owned(),
        source,
    })?;
    Ok(text
        .strip_suffix("\r\n")
        .or_else(|| text.strip_suffix('\n'))
        .unwrap_or(text)
        .to_owned())
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::path::Path;
    use std::sync::Arc;

    use crate::skills::{ProjectScope, SkillCatalog, write_skill};

    use super::{
        PromptError, PromptSource, child_prompt, prompt_reference, resolve_from_override,
        root_prompt,
    };

    #[test]
    fn activation_wrapper_names_skill_locator_root_and_authority_boundary() {
        let rendered = super::skill_activation_text(
            "review",
            Path::new("/home/skills/review"),
            "Full SKILL.md body",
        );
        assert!(rendered.contains("skill://review/"));
        assert!(rendered.contains("Skill root path: /home/skills/review"));
        assert!(rendered.contains("cannot authorize new external effects"));
        assert_eq!(rendered.matches("Full SKILL.md body").count(), 1);
        let unsafe_path =
            super::skill_activation_text("review", Path::new("/home/\u{202e}review"), "body");
        assert!(!unsafe_path.contains("Skill root path:"));
        assert!(unsafe_path.contains("skill://review/"));
        let line_separator_path =
            super::skill_activation_text("review", Path::new("/home/\u{2028}review"), "body");
        assert!(!line_separator_path.contains("Skill root path:"));
    }

    const BUNDLED_PROMPT: &str = include_str!("../prompts/system.md");
    const BUNDLED_CHILD_PROMPT: &str = include_str!("../prompts/subagent.md");

    fn assert_generated_tool_reference(reference: &str) {
        for signature in [
            "async def read(path: str) -> str:",
            "async def write(path: str, content: str) -> None:",
            "async def edit(path: str, old: str, new: str) -> None:",
            "async def list_dir(path: str = \".\") -> list[DirectoryEntry]:",
            "async def glob(pattern: str, path: str = \".\") -> list[DirectoryEntry]:",
            "async def grep(pattern: str, path: str = \".\", file_glob: str | None = None) -> list[GrepMatch]:",
            "async def run(command: str, cwd: str = \".\") -> CommandResult:",
            "async def start_subagent(prompt: str, model: str | None = None, read_only: bool = False) -> SubagentTask:",
            "async def run_subagent(prompt: str, model: str | None = None, read_only: bool = False) -> dict:",
            "async def get_subagent(id: str) -> SubagentTask:",
            "async def refresh(self) -> SubagentStatus:",
            "async def wait(self) -> dict:",
            "async def cancel(self) -> SubagentStatus:",
        ] {
            assert_eq!(reference.matches(signature).count(), 1);
        }
        assert_eq!(reference.matches("class DirectoryEntry:").count(), 1);
        assert_eq!(reference.matches("class GrepMatch:").count(), 1);
        assert_eq!(reference.matches("class CommandResult:").count(), 1);
        assert_eq!(reference.matches("class SubagentStatus:").count(), 1);
        assert_eq!(reference.matches("class SubagentTask:").count(), 1);
        for guidance in [
            "Every host call must be awaited.",
            "independent calls expected to succeed",
            "Keep dependent calls sequential.",
            "Avoid unbounded fan-out.",
            "A gather failure can follow completed host calls",
            "Run project-local, one-shot commands.",
            "Keep model-visible output focused.",
            "five minutes of chargeable time",
            "Time pauses only while waiting exclusively for child completion or cleanup; ordinary and control calls stay charged.",
            "a, b = await asyncio.gather(read(\"a.rs\"), read(\"b.rs\"))",
        ] {
            assert!(reference.contains(guidance), "missing `{guidance}`");
        }
    }

    #[test]
    fn compaction_resources_contain_the_required_arena_section() {
        const ARENA_SECTION: &str = "## Arena State\n- `<name>` (`<latest known type>`) — <purpose and useful contents>\n- Include useful persistent variables, functions, classes, and datasets.\n- Mark uncertain availability or type as unverified.\n- Do not claim this is a complete live-variable inventory.\n- State \"(none identified)\" if no useful Arena state is evidenced.";

        let resources = [
            (
                super::COMPACTION_SYSTEM_PROMPT,
                super::compaction_system_prompt(),
            ),
            (
                super::COMPACTION_INITIAL_PROMPT,
                super::compaction_initial_prompt(),
            ),
            (
                super::COMPACTION_UPDATE_PROMPT,
                super::compaction_update_prompt(),
            ),
            (
                super::COMPACTION_SPLIT_RUN_PROMPT,
                super::compaction_split_run_prompt(),
            ),
        ];

        for (resource, prompt) in resources {
            assert!(resource.ends_with('\n'));
            assert_eq!(prompt, resource.strip_suffix('\n').unwrap());
            assert!(!prompt.ends_with('\n'));
            assert!(prompt.len() > 200);
            assert!(!include_str!("prompt.rs").contains(prompt));
        }
        assert!(super::compaction_system_prompt().contains("context summarization assistant"));
        assert!(super::compaction_initial_prompt().contains("## Critical Context"));
        assert!(
            super::compaction_update_prompt().contains("Update the existing structured summary")
        );
        assert!(super::compaction_split_run_prompt().contains("## Original Request"));
        assert_eq!(
            super::strip_file_final_lf("\n  prompt  \n\n"),
            "\n  prompt  \n"
        );
        assert!(!super::compaction_system_prompt().contains(ARENA_SECTION));
        for prompt in [
            super::compaction_initial_prompt(),
            super::compaction_update_prompt(),
            super::compaction_split_run_prompt(),
        ] {
            assert_eq!(prompt.matches(ARENA_SECTION).count(), 1);
        }
    }

    #[test]
    fn bundled_prompt_explains_monty_arena_constraints() {
        for guidance in [
            "### Monty/Arena Python primer",
            "not CPython",
            "f-strings",
            "`format()`, `str.format()` and string `%` formatting are missing",
            "Importable modules (complete list): `asyncio`, `base64`, `binascii`, `collections`, `dataclasses`, `datetime`, `functools`, `itertools`, `json`, `math`, `os`, `pathlib`, `re`, `sys`, `typing`, `unicodedata`",
            "`functools` only `reduce`/`partial`",
            "`json` has `loads`/`dumps`/`JSONDecodeError`",
            "No ambient file, network or process access",
            "they cannot suspend inside synchronous callbacks",
            "Generator expressions materialise as lists",
            "`enumerate`, `zip`, `map`, `filter` and `reversed` return eager lists",
            "`next(x for x in xs)` fails",
            "No `yield`, inheritance, method decorators, `match`, `del`, `async with`, `async for` or wildcard imports",
            "Built-in methods generally work only as direct calls",
            "annotate globals you may reassign",
            "never repeat side-effecting calls merely to satisfy a diagnostic",
        ] {
            assert!(BUNDLED_PROMPT.contains(guidance), "missing `{guidance}`");
        }
    }

    #[test]
    fn root_and_child_prompts_use_the_same_direct_first_delegation_gate() {
        for prompt in [BUNDLED_PROMPT, BUNDLED_CHILD_PROMPT] {
            for guidance in [
                "Default to doing the work yourself",
                "Before starting a child, identify that benefit",
                "Sequential work, generic exploration, routine review, and difficulty alone do not justify delegation",
                "Apply this rule recursively",
            ] {
                assert!(prompt.contains(guidance), "missing `{guidance}`");
            }
        }
    }

    #[test]
    fn bundled_prompt_requests_commentary_before_code_execution() {
        let prompt = BUNDLED_PROMPT.to_lowercase();
        for guidance in [
            "one or two sentences",
            "intent",
            "execute_python",
            "commentary",
            "run continues",
        ] {
            assert!(prompt.contains(guidance), "missing `{guidance}`");
        }
    }

    #[test]
    fn resolves_the_named_bundled_prompt_with_one_generated_tool_reference() {
        let resolved = resolve_from_override(None).unwrap();
        let empty = SkillCatalog::default();
        let text = root_prompt(resolved.base(), &empty);

        assert_eq!(
            text,
            format!("{BUNDLED_PROMPT}\n\n{}", prompt_reference(&empty))
        );
        assert_generated_tool_reference(&text);
        assert_eq!(resolved.base(), BUNDLED_PROMPT);
        assert_eq!(resolved.source(), &PromptSource::Bundled { name: "system" });
    }

    #[test]
    fn an_override_takes_exact_precedence_and_reports_only_its_path() {
        let directory = tempfile::tempdir().unwrap();
        fs::create_dir(directory.path().join("nested")).unwrap();
        const OVERRIDE: &str = "private override text";
        fs::write(directory.path().join("override.md"), OVERRIDE).unwrap();
        let override_path = directory
            .path()
            .join("nested")
            .join("..")
            .join("override.md");

        let resolved = resolve_from_override(Some(override_path.clone().into_os_string())).unwrap();
        let empty = SkillCatalog::default();
        let text = root_prompt(resolved.base(), &empty);

        assert_eq!(text, format!("{OVERRIDE}\n\n{}", prompt_reference(&empty)));
        assert_generated_tool_reference(&text);
        assert_eq!(resolved.base(), OVERRIDE);
        assert_eq!(
            resolved.source(),
            &PromptSource::Override {
                path: override_path
            }
        );
        assert!(!format!("{:?}", resolved.source()).contains("private override text"));
    }

    #[test]
    fn an_override_drops_one_file_final_line_ending() {
        let directory = tempfile::tempdir().unwrap();
        let override_path = directory.path().join("override.md");
        for contents in ["override text\n", "override text\r\n"] {
            fs::write(&override_path, contents).unwrap();

            let resolved =
                resolve_from_override(Some(override_path.clone().into_os_string())).unwrap();

            assert_eq!(resolved.base(), "override text");
        }
    }

    #[test]
    fn child_prompts_keep_the_canonical_base_and_one_effective_reference() {
        let base = "canonical base";
        let empty = SkillCatalog::default();
        let full = child_prompt(base, &empty, prompt_reference(&empty));

        assert_eq!(
            full,
            format!(
                "{base}\n\n{BUNDLED_CHILD_PROMPT}\n\n{}",
                prompt_reference(&empty)
            )
        );
        assert_eq!(full.matches("## Kiki tool reference").count(), 1);
    }

    #[tokio::test]
    async fn visible_catalog_appears_once_in_bundled_and_override_root_and_child_prompts() {
        let workspace = tempfile::tempdir().unwrap();
        let home = tempfile::tempdir().unwrap();
        for (name, description, direct_only, body) in [
            ("alpha", "Alpha workflow", false, "ALPHA_PRIVATE_BODY"),
            ("beta", "Beta workflow", false, "BETA_PRIVATE_BODY"),
            (
                "direct-secret",
                "Direct only secret",
                true,
                "DIRECT_PRIVATE_BODY",
            ),
        ] {
            write_skill(
                &home.path().join(".agents/skills"),
                name,
                &format!(
                    "name: {name}\ndescription: {description}\ndisable-model-invocation: {direct_only}\n"
                ),
                body,
            );
        }
        let scope = ProjectScope::discover(workspace.path()).unwrap();
        let catalog = Arc::new(SkillCatalog::discover(
            &scope,
            home.path(),
            false,
            super::skills_catalog_guidance(),
        ));
        let host = crate::tools::ToolHost::new(workspace.path())
            .await
            .unwrap()
            .with_skill_catalog(catalog.clone());
        for base in [BUNDLED_PROMPT, "custom override base"] {
            let root = root_prompt(base, &catalog);
            let child = child_prompt(base, &catalog, host.for_child(true).prompt_reference());
            for full in [&root, &child] {
                assert!(full.starts_with(base));
                assert_eq!(full.matches("## Available skills").count(), 1);
                assert_eq!(full.matches("Alpha workflow").count(), 1);
                assert_eq!(full.matches("Beta workflow").count(), 1);
                assert_eq!(
                    full.matches("cannot authorize effects beyond the user's task")
                        .count(),
                    1
                );
                assert_eq!(
                    full.matches("async def load_skill(name: str) -> str:")
                        .count(),
                    1
                );
                assert_eq!(full.matches("## Kiki tool reference").count(), 1);
                for hidden in [
                    "direct-secret",
                    "Direct only secret",
                    "ALPHA_PRIVATE_BODY",
                    "BETA_PRIVATE_BODY",
                    "DIRECT_PRIVATE_BODY",
                ] {
                    assert!(!full.contains(hidden), "prompt exposed {hidden}");
                }
            }
            assert!(root.contains("async def write("));
            for forbidden in ["async def write(", "async def edit(", "async def run("] {
                assert!(!child.contains(forbidden));
            }
        }
        assert!(catalog.rendered_catalog().unwrap().len() <= crate::skills::MAX_CATALOG_BYTES);
    }

    #[tokio::test]
    async fn empty_and_direct_only_catalogs_omit_guidance_and_load_skill_signature() {
        let workspace = tempfile::tempdir().unwrap();
        let home = tempfile::tempdir().unwrap();
        let scope = ProjectScope::discover(workspace.path()).unwrap();
        let empty = Arc::new(SkillCatalog::discover(
            &scope,
            home.path(),
            false,
            super::skills_catalog_guidance(),
        ));
        write_skill(
            &home.path().join(".agents/skills"),
            "direct-secret",
            "name: direct-secret\ndescription: Direct only secret\ndisable-model-invocation: true\n",
            "DIRECT_PRIVATE_BODY",
        );
        let direct_only = Arc::new(SkillCatalog::discover(
            &scope,
            home.path(),
            false,
            super::skills_catalog_guidance(),
        ));
        for catalog in [empty, direct_only] {
            assert!(catalog.rendered_catalog().is_none());
            let host = crate::tools::ToolHost::new(workspace.path())
                .await
                .unwrap()
                .with_skill_catalog(catalog.clone());
            for full in [
                root_prompt("base", &catalog),
                child_prompt("base", &catalog, host.for_child(true).prompt_reference()),
            ] {
                for absent in [
                    "## Available skills",
                    "cannot authorize effects beyond the user's task",
                    "async def load_skill(",
                    "Direct only secret",
                    "direct-secret",
                ] {
                    assert!(!full.contains(absent), "prompt exposed {absent}");
                }
                assert_eq!(full.matches("## Kiki tool reference").count(), 1);
            }
        }
    }

    async fn root_and_child_prompts(workspace: &Path, catalog: Arc<SkillCatalog>) -> [String; 2] {
        let host = crate::tools::ToolHost::new(workspace)
            .await
            .unwrap()
            .with_skill_catalog(catalog.clone());
        [
            root_prompt("base", &catalog),
            child_prompt("base", &catalog, host.for_child(true).prompt_reference()),
        ]
    }

    #[test]
    fn catalog_guidance_override_drops_one_file_final_line_feed() {
        let directory = tempfile::tempdir().unwrap();
        let override_path = directory.path().join("catalog-guidance.md");
        fs::write(&override_path, "CUSTOM_CATALOG_GUIDANCE  \n\n").unwrap();
        let guidance = super::resolve_skills_catalog_guidance_from_override(Some(
            override_path.into_os_string(),
        ))
        .unwrap();
        assert_eq!(guidance, "CUSTOM_CATALOG_GUIDANCE  \n");
    }

    #[tokio::test]
    async fn catalog_guidance_renders_once_only_when_a_skill_is_visible() {
        let workspace = tempfile::tempdir().unwrap();
        let home = tempfile::tempdir().unwrap();
        let scope = ProjectScope::discover(workspace.path()).unwrap();
        let guidance = "CUSTOM_CATALOG_GUIDANCE  \n";
        let empty = Arc::new(SkillCatalog::discover(&scope, home.path(), false, guidance));
        assert!(empty.rendered_catalog().is_none());
        for prompt in root_and_child_prompts(workspace.path(), empty).await {
            assert!(!prompt.contains("CUSTOM_CATALOG_GUIDANCE"));
        }

        write_skill(
            &home.path().join(".agents/skills"),
            "review",
            "name: review\ndescription: Review\n",
            "PRIVATE_BODY",
        );
        let visible = Arc::new(SkillCatalog::discover(&scope, home.path(), false, guidance));
        assert!(
            visible
                .rendered_catalog()
                .unwrap()
                .contains("CUSTOM_CATALOG_GUIDANCE  \n\n")
        );
        for prompt in root_and_child_prompts(workspace.path(), visible).await {
            assert_eq!(prompt.matches("CUSTOM_CATALOG_GUIDANCE").count(), 1);
            assert!(!prompt.contains("PRIVATE_BODY"));
        }
    }

    #[test]
    fn catalog_guidance_counts_toward_the_catalog_byte_budget() {
        let workspace = tempfile::tempdir().unwrap();
        let home = tempfile::tempdir().unwrap();
        write_skill(
            &home.path().join(".agents/skills"),
            "review",
            "name: review\ndescription: Review\n",
            "PRIVATE_BODY",
        );
        let scope = ProjectScope::discover(workspace.path()).unwrap();
        let consumes_catalog_budget = "X".repeat(crate::skills::MAX_CATALOG_BYTES - 32);
        let omitted = SkillCatalog::discover(&scope, home.path(), false, &consumes_catalog_budget);
        assert!(omitted.rendered_catalog().is_none());
        assert!(matches!(
            omitted.lookup_model_invocable("review"),
            Err(crate::skills::SkillLookupError::Omitted { .. })
        ));
    }

    #[test]
    fn unreadable_catalog_guidance_override_is_a_typed_error() {
        let directory = tempfile::tempdir().unwrap();
        let missing = directory.path().join("missing.md");
        assert!(matches!(
            super::resolve_skills_catalog_guidance_from_override(Some(missing.into_os_string())),
            Err(PromptError::UnreadableOverride { .. })
        ));
    }

    #[test]
    fn invalid_utf8_catalog_guidance_override_is_a_typed_error() {
        let directory = tempfile::tempdir().unwrap();
        let invalid = directory.path().join("invalid.md");
        fs::write(&invalid, b"private\xff").unwrap();
        assert!(matches!(
            super::resolve_skills_catalog_guidance_from_override(Some(invalid.into_os_string())),
            Err(PromptError::InvalidUtf8Override { .. })
        ));
    }

    #[tokio::test]
    async fn read_only_child_prompt_omits_unavailable_tool_wrappers() {
        let workspace = tempfile::tempdir().unwrap();
        let reference = crate::tools::ToolHost::new(workspace.path())
            .await
            .unwrap()
            .for_child(true)
            .prompt_reference();
        let prompt = child_prompt("canonical base", &SkillCatalog::default(), reference);

        assert_eq!(prompt.matches("## Kiki tool reference").count(), 1);
        for signature in ["async def write(", "async def edit(", "async def run("] {
            assert!(!prompt.contains(signature), "unexpected `{signature}`");
        }
    }

    #[test]
    fn an_unreadable_override_is_a_typed_error_without_fallback() {
        let directory = tempfile::tempdir().unwrap();
        let override_path = directory.path().join("missing.md");

        let Err(error) = resolve_from_override(Some(override_path.clone().into_os_string())) else {
            panic!("unreadable override should fail");
        };

        assert!(matches!(
            &error,
            PromptError::UnreadableOverride { path, source }
                if path == &override_path && source.kind() == std::io::ErrorKind::NotFound
        ));
    }

    #[test]
    fn an_invalid_utf8_override_is_a_typed_error_without_retaining_its_content() {
        let directory = tempfile::tempdir().unwrap();
        let override_path = directory.path().join("invalid.md");
        fs::write(&override_path, b"private prompt\xff").unwrap();

        let Err(error) = resolve_from_override(Some(override_path.clone().into_os_string())) else {
            panic!("invalid UTF-8 override should fail");
        };

        assert!(matches!(
            &error,
            PromptError::InvalidUtf8Override { path, source }
                if path == &override_path && source.valid_up_to() == 14
        ));
        assert!(!format!("{error:?}").contains("private prompt"));
        assert!(!error.to_string().contains("private prompt"));
    }
}
