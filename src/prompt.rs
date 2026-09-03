use std::env;
use std::ffi::OsString;
use std::fs;
use std::io;
use std::path::PathBuf;
use std::str::Utf8Error;

use crate::tools::prompt_reference;

const BUNDLED_PROMPT: &str = include_str!("../prompts/read_only_agent.md");
const BUNDLED_PROMPT_NAME: &str = "read_only_agent";
const SYSTEM_PROMPT_ENV: &str = "KIKI_SYSTEM_PROMPT";

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PromptSource {
    Bundled { name: &'static str },
    Override { path: PathBuf },
}

pub struct ResolvedPrompt {
    text: String,
    source: PromptSource,
}

impl ResolvedPrompt {
    pub fn text(&self) -> &str {
        &self.text
    }

    pub fn source(&self) -> &PromptSource {
        &self.source
    }
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
            text: append_tool_reference(BUNDLED_PROMPT),
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
        text: append_tool_reference(&text),
        source: PromptSource::Override { path },
    })
}

fn append_tool_reference(base: &str) -> String {
    format!("{base}\n\n{}", prompt_reference())
}

#[cfg(test)]
mod tests {
    use std::fs;

    use super::{PromptError, PromptSource, prompt_reference, resolve_from_override};

    const BUNDLED_PROMPT: &str = include_str!("../prompts/read_only_agent.md");

    fn assert_concept(terms: &[&str]) {
        let prompt = BUNDLED_PROMPT.to_lowercase();
        for term in terms {
            assert!(
                prompt.contains(term),
                "bundled prompt must include the `{term}` concept"
            );
        }
    }

    fn assert_generated_tool_reference(reference: &str) {
        for signature in [
            "async def read(path: str) -> str:",
            "async def write(path: str, content: str) -> None:",
            "async def edit(path: str, old: str, new: str) -> None:",
            "async def list_dir(path: str = \".\") -> list[DirectoryEntry]:",
            "async def glob(pattern: str, path: str = \".\") -> list[DirectoryEntry]:",
            "async def grep(pattern: str, path: str = \".\", file_glob: str | None = None) -> list[GrepMatch]:",
        ] {
            assert_eq!(reference.matches(signature).count(), 1);
        }
        assert_eq!(reference.matches("class DirectoryEntry:").count(), 1);
        assert_eq!(reference.matches("class GrepMatch:").count(), 1);
        for guidance in [
            "Every host call must be awaited.",
            "independent calls expected to succeed",
            "Keep dependent calls sequential.",
            "Avoid unbounded fan-out.",
            "A gather failure can follow completed host calls",
            "Keep model-visible output focused.",
            "five-minute deadline",
            "a, b = await asyncio.gather(read(\"a.rs\"), read(\"b.rs\"))",
        ] {
            assert!(reference.contains(guidance), "missing `{guidance}`");
        }
        assert!(!reference.contains("run("));
    }

    #[test]
    fn bundled_prompt_keeps_shared_agent_behaviour_outside_the_tool_reference() {
        assert_concept(&[
            "persistent",
            "arena",
            "model steps",
            "user requests",
            "survive",
        ]);
        assert_concept(&["host access", "tool reference"]);
        assert_concept(&["execution feedback", "python error", "committed"]);
        for managed_guidance in ["asyncio.gather", "focused", "five-minute deadline"] {
            assert!(
                !BUNDLED_PROMPT.contains(managed_guidance),
                "bundled prompt must not duplicate managed `{managed_guidance}` guidance"
            );
        }
        assert_concept(&["execute_python", "final assistant text", "exactly one"]);
        assert_concept(&["do not repeat", "executed python", "unless the user asks"]);
        assert_concept(&["untrusted", "files", "output", "instructions"]);
        assert!(!BUNDLED_PROMPT.contains("async def"));

        for speculative_capability in [
            "resource registry",
            "write(path)",
            "edit(path)",
            "glob(",
            "grep(",
            "run(",
            "subagent",
        ] {
            assert!(
                !BUNDLED_PROMPT
                    .to_lowercase()
                    .contains(speculative_capability),
                "bundled prompt must not advertise `{speculative_capability}`"
            );
        }
    }

    #[test]
    fn resolves_the_named_bundled_prompt_with_one_generated_tool_reference() {
        let resolved = resolve_from_override(None).unwrap();

        assert_eq!(
            resolved.text(),
            format!("{BUNDLED_PROMPT}\n\n{}", prompt_reference())
        );
        assert_generated_tool_reference(
            resolved
                .text()
                .strip_prefix(format!("{BUNDLED_PROMPT}\n\n").as_str())
                .expect("bundled prompt is preserved verbatim before the managed reference"),
        );
        assert!(
            resolved
                .text()
                .contains("Do not redefine these host functions.")
        );
        assert!(!resolved.text().contains("list of exact lines"));
        assert_eq!(
            resolved.source(),
            &PromptSource::Bundled {
                name: "read_only_agent"
            }
        );
    }

    #[test]
    fn an_override_takes_exact_precedence_and_reports_only_its_path() {
        let directory = tempfile::tempdir().unwrap();
        fs::create_dir(directory.path().join("nested")).unwrap();
        const OVERRIDE: &str =
            "private override text: async def read(path: str) -> str: is mentioned independently";
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
        assert_eq!(
            resolved
                .text()
                .matches("async def read(path: str) -> str:")
                .count(),
            2
        );
        assert_generated_tool_reference(
            resolved
                .text()
                .strip_prefix(format!("{OVERRIDE}\n\n").as_str())
                .expect("override is preserved verbatim before the managed reference"),
        );
        assert_eq!(
            resolved.source(),
            &PromptSource::Override {
                path: override_path
            }
        );
        assert!(!format!("{:?}", resolved.source()).contains("private override text"));
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
