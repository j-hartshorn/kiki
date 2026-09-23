use std::env;
use std::ffi::OsString;
use std::fs;
use std::io;
use std::path::PathBuf;
use std::str::Utf8Error;

use crate::tools::prompt_reference;

const BUNDLED_PROMPT: &str = include_str!("../prompts/system.md");
const BUNDLED_CHILD_PROMPT: &str = include_str!("../prompts/subagent.md");
const COMPACTION_SYSTEM_PROMPT: &str = include_str!("../prompts/compaction-system.md");
const COMPACTION_INITIAL_PROMPT: &str = include_str!("../prompts/compaction-initial.md");
const COMPACTION_UPDATE_PROMPT: &str = include_str!("../prompts/compaction-update.md");
const COMPACTION_SPLIT_RUN_PROMPT: &str = include_str!("../prompts/compaction-split-run.md");
const BUNDLED_PROMPT_NAME: &str = "system";
const SYSTEM_PROMPT_ENV: &str = "KIKI_SYSTEM_PROMPT";

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PromptSource {
    Bundled { name: &'static str },
    Override { path: PathBuf },
}

pub struct ResolvedPrompt {
    base: String,
    text: String,
    source: PromptSource,
}

impl ResolvedPrompt {
    pub fn text(&self) -> &str {
        &self.text
    }

    pub(crate) fn base(&self) -> &str {
        &self.base
    }

    pub fn source(&self) -> &PromptSource {
        &self.source
    }
}

pub(crate) fn root_prompt(base: &str) -> String {
    assemble(base, prompt_reference())
}

pub(crate) fn child_prompt(base: &str, tool_reference: String) -> String {
    assemble(&format!("{base}\n\n{BUNDLED_CHILD_PROMPT}"), tool_reference)
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
        .expect("bundled compaction prompts end in a line feed")
}

fn assemble(base: &str, tool_reference: String) -> String {
    format!("{base}\n\n{tool_reference}")
}

#[derive(Debug, thiserror::Error)]
pub enum PromptError {
    #[error(
        "system prompt override `{path}` could not be read: {source}",
        path = path.display()
    )]
    UnreadableOverride {
        path: PathBuf,
        #[source]
        source: io::Error,
    },
    #[error(
        "system prompt override `{path}` is not valid UTF-8: {source}",
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

fn resolve_from_override(override_path: Option<OsString>) -> Result<ResolvedPrompt, PromptError> {
    let Some(override_path) = override_path else {
        return Ok(ResolvedPrompt {
            base: BUNDLED_PROMPT.to_owned(),
            text: root_prompt(BUNDLED_PROMPT),
            source: PromptSource::Bundled {
                name: BUNDLED_PROMPT_NAME,
            },
        });
    };

    let path = PathBuf::from(override_path);
    let bytes = match fs::read(&path) {
        Ok(bytes) => bytes,
        Err(source) => return Err(PromptError::UnreadableOverride { path, source }),
    };
    let text = match std::str::from_utf8(&bytes) {
        Ok(text) => text.to_owned(),
        Err(source) => return Err(PromptError::InvalidUtf8Override { path, source }),
    };

    Ok(ResolvedPrompt {
        text: root_prompt(&text),
        base: text,
        source: PromptSource::Override { path },
    })
}

#[cfg(test)]
mod tests {
    use std::fs;

    use super::{PromptError, PromptSource, child_prompt, prompt_reference, resolve_from_override};

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
            "generator expressions materialize as lists",
            "`enumerate`, `zip`, `map`, `filter`, and `reversed` are eager",
            "Host functions must be awaited",
            "There are no third-party packages",
            "`sys.path`, or ambient filesystem/network/process access",
            "Unsupported features include `yield`, inheritance, method decorators, `match`, `del`, `async with`, `async for`, async comprehensions, and wildcard imports",
            "consider a Monty incompatibility",
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

        assert_eq!(
            resolved.text(),
            format!("{BUNDLED_PROMPT}\n\n{}", prompt_reference())
        );
        assert_generated_tool_reference(resolved.text());
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

        assert_eq!(
            resolved.text(),
            format!("{OVERRIDE}\n\n{}", prompt_reference())
        );
        assert_generated_tool_reference(resolved.text());
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
    fn child_prompts_keep_the_canonical_base_and_one_effective_reference() {
        let base = "canonical base";
        let full = child_prompt(base, prompt_reference());

        assert_eq!(
            full,
            format!("{base}\n\n{BUNDLED_CHILD_PROMPT}\n\n{}", prompt_reference())
        );
        assert_eq!(full.matches("## Kiki tool reference").count(), 1);
    }

    #[tokio::test]
    async fn read_only_child_prompt_omits_unavailable_tool_wrappers() {
        let workspace = tempfile::tempdir().unwrap();
        let reference = crate::tools::ToolHost::new(workspace.path())
            .await
            .unwrap()
            .for_child(true)
            .prompt_reference();
        let prompt = child_prompt("canonical base", reference);

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
