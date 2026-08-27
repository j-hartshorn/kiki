use thiserror::Error;
use tokio_util::sync::CancellationToken;

use crate::arena::{Arena, ArenaError, Execution, ExecutionOutcome, Observation, PythonError};
use crate::model::{
    ModelAction, ModelContextEntry, ModelError, READ_CAPABILITY_DESCRIPTION, ScriptedModel,
};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CompletedStep {
    pub source: String,
    pub execution: Execution,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgentRun {
    pub final_message: String,
    pub completed_steps: Vec<CompletedStep>,
}

#[derive(Debug, Error)]
pub enum AgentError {
    #[error("agent run was cancelled")]
    Cancelled,
    #[error(transparent)]
    Model(#[from] ModelError),
    #[error(transparent)]
    Arena(#[from] ArenaError),
}

pub struct Agent {
    arena: Arena,
    model: ScriptedModel,
    model_context: Vec<ModelContextEntry>,
}

impl Agent {
    pub fn new(arena: Arena, model: ScriptedModel) -> Self {
        Self {
            arena,
            model,
            model_context: vec![ModelContextEntry::ReadCapability {
                description: READ_CAPABILITY_DESCRIPTION.to_owned(),
            }],
        }
    }

    pub async fn run(
        &mut self,
        request: impl Into<String>,
        cancellation: &CancellationToken,
    ) -> Result<AgentRun, AgentError> {
        if cancellation.is_cancelled() {
            return Err(AgentError::Cancelled);
        }
        self.model.reset();
        self.model_context.push(ModelContextEntry::UserRequest {
            text: request.into(),
        });
        let mut completed_steps = Vec::new();

        loop {
            if cancellation.is_cancelled() {
                return Err(AgentError::Cancelled);
            }
            match self.model.next_action(&self.model_context)? {
                ModelAction::Code { source } => {
                    let execution = match self.arena.execute(&source, cancellation).await {
                        Ok(execution) => execution,
                        Err(ArenaError::Cancelled) => return Err(AgentError::Cancelled),
                        Err(error) => return Err(AgentError::Arena(error)),
                    };
                    let feedback = execution_feedback(&execution);
                    self.model_context.push(ModelContextEntry::Execution {
                        source: source.clone(),
                        feedback,
                    });
                    completed_steps.push(CompletedStep { source, execution });
                }
                ModelAction::Message { text } => {
                    self.model_context
                        .push(ModelContextEntry::FinalMessage { text: text.clone() });
                    return Ok(AgentRun {
                        final_message: text,
                        completed_steps,
                    });
                }
            }
        }
    }
}

fn execution_feedback(execution: &Execution) -> String {
    let mut frames = Vec::new();
    append_output(&mut frames, "stdout", &execution.stdout);
    append_output(&mut frames, "stderr", &execution.stderr);
    match &execution.outcome {
        ExecutionOutcome::Complete {
            result: Some(result),
        } => {
            let truncated = result.tail.is_some();
            frames.push(render_result(result));
            if truncated {
                frames.push(truncation_warning("result"));
            }
        }
        ExecutionOutcome::Complete { result: None } => {}
        ExecutionOutcome::PythonError(error) => {
            frames.push(render_error(error));
            if error.omitted_bytes > 0 {
                frames.push(error_truncation_warning());
            }
        }
    }
    frames.join("\n")
}

fn append_output(frames: &mut Vec<String>, stream: &str, observation: &Observation) {
    if observation.total_bytes == 0 {
        return;
    }
    let truncated = observation.tail.is_some();
    frames.push(render_output(stream, observation));
    if truncated {
        frames.push(truncation_warning("output"));
    }
}

fn render_output(stream: &str, observation: &Observation) -> String {
    render_observation("kiki_output", &format!(" stream=\"{stream}\""), observation)
}

fn render_result(observation: &Observation) -> String {
    render_observation("kiki_result", "", observation)
}

fn render_observation(tag: &str, extra_attributes: &str, observation: &Observation) -> String {
    let truncated = observation.tail.is_some();
    let mut rendered = format!(
        "<{tag}{extra_attributes} truncated=\"{truncated}\" total_lines=\"{}\" total_bytes=\"{}\">\n<kiki_output_head lines=\"{}\" bytes=\"{}\" ends_mid_line=\"{}\">\n{}",
        observation.total_lines,
        observation.total_bytes,
        observation.head_lines,
        observation.head.len(),
        observation.head_ends_mid_line,
        observation.head,
    );
    rendered.push_str("</kiki_output_head>\n");
    if let Some(tail) = &observation.tail {
        rendered.push_str(&format!(
            "<kiki_output_omitted lines=\"{}\" bytes=\"{}\" partial_lines=\"{}\" />\n<kiki_output_tail lines=\"{}\" bytes=\"{}\" starts_mid_line=\"{}\">\n{}",
            observation.omitted_lines,
            observation.omitted_bytes,
            observation.partial_lines,
            observation.tail_lines,
            tail.len(),
            observation.tail_starts_mid_line,
            tail,
        ));
        rendered.push_str("</kiki_output_tail>\n");
    }
    rendered.push_str(&format!("</{tag}>"));
    rendered
}

fn truncation_warning(kind: &str) -> String {
    format!(
        "<kiki_warning code=\"{kind}_truncated\" arena_state=\"committed\">\nExecution completed, but its model-visible {kind} was truncated. The underlying arena values were not changed. Print a smaller selection to inspect the omitted data.\n</kiki_warning>"
    )
}

fn render_error(error: &PythonError) -> String {
    format!(
        "<kiki_error omitted_bytes=\"{}\">\n{}</kiki_error>",
        error.omitted_bytes, error.text
    )
}

fn error_truncation_warning() -> String {
    "<kiki_warning code=\"error_truncated\" arena_state=\"committed\">\nExecution completed with a Python error, but its model-visible error was truncated. The underlying arena values were not changed.\n</kiki_warning>".to_owned()
}

#[cfg(test)]
mod tests {
    use std::env;
    use std::fs;
    use std::path::PathBuf;

    use tempfile::TempDir;
    use tokio_util::sync::CancellationToken;

    use super::{Agent, AgentError};
    use crate::arena::{Arena, Execution, ExecutionOutcome, Observation, PythonError};
    use crate::model::{
        ModelAction, ModelContextEntry, ModelError, READ_CAPABILITY_DESCRIPTION, ScriptedModel,
    };
    use crate::tools::ReadTool;

    fn worker_path() -> PathBuf {
        env::var_os("MONTY_BIN")
            .map(PathBuf::from)
            .expect("MONTY_BIN must point to the monty-runtime 0.0.21 worker")
    }

    async fn agent(workspace: &TempDir, actions: Vec<ModelAction>) -> Agent {
        let arena = Arena::new(
            worker_path(),
            ReadTool::new(workspace.path())
                .await
                .expect("open read tool"),
        )
        .await
        .expect("create arena");
        Agent::new(arena, ScriptedModel::new(actions))
    }

    struct ObservationMetadata {
        head_lines: usize,
        tail_lines: usize,
        total_lines: usize,
        total_bytes: usize,
        omitted_lines: usize,
        omitted_bytes: usize,
        head_ends_mid_line: bool,
        tail_starts_mid_line: bool,
    }

    fn observation(head: &str, tail: Option<&str>, metadata: ObservationMetadata) -> Observation {
        Observation {
            head: head.to_owned(),
            tail: tail.map(str::to_owned),
            head_lines: metadata.head_lines,
            tail_lines: metadata.tail_lines,
            total_lines: metadata.total_lines,
            total_bytes: metadata.total_bytes,
            omitted_lines: metadata.omitted_lines,
            omitted_bytes: metadata.omitted_bytes,
            head_ends_mid_line: metadata.head_ends_mid_line,
            tail_starts_mid_line: metadata.tail_starts_mid_line,
            partial_lines: metadata.head_ends_mid_line || metadata.tail_starts_mid_line,
        }
    }

    #[tokio::test]
    async fn agent_feeds_completed_code_back_before_a_final_message() {
        let workspace = tempfile::tempdir().unwrap();
        let mut agent = agent(
            &workspace,
            vec![
                ModelAction::Code {
                    source: "print('hello')".to_owned(),
                },
                ModelAction::Message {
                    text: "done".to_owned(),
                },
                ModelAction::Code {
                    source: "message_did_not_terminate = True".to_owned(),
                },
            ],
        )
        .await;
        let cancellation = CancellationToken::new();

        let run = agent.run("inspect", &cancellation).await.unwrap();

        assert_eq!(run.final_message, "done");
        assert_eq!(run.completed_steps.len(), 1);
        assert_eq!(run.completed_steps[0].source, "print('hello')");
        assert_eq!(
            agent.model.contexts(),
            [
                vec![
                    ModelContextEntry::ReadCapability {
                        description: READ_CAPABILITY_DESCRIPTION.to_owned(),
                    },
                    ModelContextEntry::UserRequest {
                        text: "inspect".to_owned(),
                    },
                ],
                vec![
                    ModelContextEntry::ReadCapability {
                        description: READ_CAPABILITY_DESCRIPTION.to_owned(),
                    },
                    ModelContextEntry::UserRequest {
                        text: "inspect".to_owned(),
                    },
                    ModelContextEntry::Execution {
                        source: "print('hello')".to_owned(),
                        feedback: "<kiki_output stream=\"stdout\" truncated=\"false\" total_lines=\"1\" total_bytes=\"6\">\n<kiki_output_head lines=\"1\" bytes=\"6\" ends_mid_line=\"false\">\nhello\n</kiki_output_head>\n</kiki_output>".to_owned(),
                    },
                ],
            ]
        );
    }

    #[tokio::test]
    async fn agent_skips_a_pre_cancelled_run_without_changing_persistent_state() {
        let workspace = tempfile::tempdir().unwrap();
        let mut agent = agent(
            &workspace,
            vec![
                ModelAction::Code {
                    source: "first_action = 1\nfirst_action".to_owned(),
                },
                ModelAction::Message {
                    text: "done".to_owned(),
                },
            ],
        )
        .await;
        let first_cancellation = CancellationToken::new();

        agent.run("first", &first_cancellation).await.unwrap();
        let persistent_context = agent.model_context.clone();
        let observed_contexts = agent.model.contexts().to_vec();
        let action_position = agent.model.position();
        let cancelled = CancellationToken::new();
        cancelled.cancel();

        let error = agent.run("cancelled", &cancelled).await.unwrap_err();

        assert!(matches!(error, AgentError::Cancelled));
        assert_eq!(agent.model_context, persistent_context);
        assert_eq!(agent.model.contexts(), observed_contexts.as_slice());
        assert_eq!(agent.model.position(), action_position);

        let fresh = agent.run("fresh", &CancellationToken::new()).await.unwrap();

        assert_eq!(fresh.final_message, "done");
        assert_eq!(fresh.completed_steps.len(), 1);
        assert_eq!(
            fresh.completed_steps[0].source,
            "first_action = 1\nfirst_action"
        );
        assert_eq!(result_head(&fresh.completed_steps[0].execution), "1");
    }

    #[tokio::test]
    async fn agent_returns_exhaustion_without_retrying_an_empty_script() {
        let workspace = tempfile::tempdir().unwrap();
        let mut agent = agent(&workspace, vec![]).await;
        let cancellation = CancellationToken::new();

        let error = agent.run("inspect", &cancellation).await.unwrap_err();

        assert!(matches!(error, AgentError::Model(ModelError::Exhausted)));
        assert_eq!(agent.model.contexts().len(), 1);
        assert_eq!(agent.model.position(), 0);
        assert_eq!(
            agent.model_context,
            vec![
                ModelContextEntry::ReadCapability {
                    description: READ_CAPABILITY_DESCRIPTION.to_owned(),
                },
                ModelContextEntry::UserRequest {
                    text: "inspect".to_owned(),
                },
            ]
        );
    }

    #[tokio::test]
    async fn agent_cancellation_recovers_the_arena_without_consuming_later_actions() {
        let workspace = tempfile::tempdir().unwrap();
        let mut agent = agent(
            &workspace,
            vec![
                ModelAction::Code {
                    source: "while True:\n    pass".to_owned(),
                },
                ModelAction::Code {
                    source: "later_action_ran = True".to_owned(),
                },
                ModelAction::Message {
                    text: "done".to_owned(),
                },
            ],
        )
        .await;
        let cancellation = CancellationToken::new();
        let canceller = cancellation.clone();
        let cancel = tokio::spawn(async move {
            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
            canceller.cancel();
        });

        let error = agent.run("cancel", &cancellation).await.unwrap_err();
        cancel.await.unwrap();

        assert!(matches!(error, AgentError::Cancelled));
        assert_eq!(agent.model.contexts().len(), 1);
        let recovered = agent
            .arena
            .execute("1 + 1", &CancellationToken::new())
            .await
            .expect("arena should recover before Agent returns cancellation");
        assert_eq!(result_head(&recovered), "2");
    }

    #[tokio::test]
    async fn agent_returns_python_errors_as_feedback_and_executes_the_following_action() {
        let workspace = tempfile::tempdir().unwrap();
        let mut agent = agent(
            &workspace,
            vec![
                ModelAction::Code {
                    source: "value = 1\nraise ValueError('expected')".to_owned(),
                },
                ModelAction::Code {
                    source: "value + 1".to_owned(),
                },
                ModelAction::Message {
                    text: "done".to_owned(),
                },
            ],
        )
        .await;
        let cancellation = CancellationToken::new();

        let run = agent.run("continue", &cancellation).await.unwrap();

        assert_eq!(run.completed_steps.len(), 2);
        assert!(matches!(
            run.completed_steps[0].execution.outcome,
            ExecutionOutcome::PythonError(_)
        ));
        assert_eq!(result_head(&run.completed_steps[1].execution), "2");
        let ModelContextEntry::Execution { feedback, .. } = agent.model.contexts()[1]
            .last()
            .expect("following action should receive the Python error")
        else {
            panic!("expected completed execution context");
        };
        assert!(feedback.contains("<kiki_error omitted_bytes=\"0\">"));
        assert!(feedback.contains("ValueError"));
    }

    #[test]
    fn agent_frames_stdout_stderr_and_result_in_order_without_escaping_raw_content() {
        let execution = Execution {
            stdout: observation(
                "<raw>&",
                None,
                ObservationMetadata {
                    head_lines: 1,
                    tail_lines: 0,
                    total_lines: 1,
                    total_bytes: 6,
                    omitted_lines: 0,
                    omitted_bytes: 0,
                    head_ends_mid_line: false,
                    tail_starts_mid_line: false,
                },
            ),
            stderr: observation(
                "a\nb\n",
                Some("d\n"),
                ObservationMetadata {
                    head_lines: 2,
                    tail_lines: 1,
                    total_lines: 4,
                    total_bytes: 8,
                    omitted_lines: 1,
                    omitted_bytes: 2,
                    head_ends_mid_line: false,
                    tail_starts_mid_line: false,
                },
            ),
            outcome: ExecutionOutcome::Complete {
                result: Some(observation(
                    "res<",
                    Some(">ult"),
                    ObservationMetadata {
                        head_lines: 1,
                        tail_lines: 1,
                        total_lines: 1,
                        total_bytes: 10,
                        omitted_lines: 0,
                        omitted_bytes: 2,
                        head_ends_mid_line: true,
                        tail_starts_mid_line: true,
                    },
                )),
            },
        };

        assert_eq!(
            super::execution_feedback(&execution),
            concat!(
                "<kiki_output stream=\"stdout\" truncated=\"false\" total_lines=\"1\" total_bytes=\"6\">\n",
                "<kiki_output_head lines=\"1\" bytes=\"6\" ends_mid_line=\"false\">\n",
                "<raw>&</kiki_output_head>\n",
                "</kiki_output>\n",
                "<kiki_output stream=\"stderr\" truncated=\"true\" total_lines=\"4\" total_bytes=\"8\">\n",
                "<kiki_output_head lines=\"2\" bytes=\"4\" ends_mid_line=\"false\">\n",
                "a\nb\n</kiki_output_head>\n",
                "<kiki_output_omitted lines=\"1\" bytes=\"2\" partial_lines=\"false\" />\n",
                "<kiki_output_tail lines=\"1\" bytes=\"2\" starts_mid_line=\"false\">\n",
                "d\n</kiki_output_tail>\n",
                "</kiki_output>\n",
                "<kiki_warning code=\"output_truncated\" arena_state=\"committed\">\n",
                "Execution completed, but its model-visible output was truncated. The underlying arena values were not changed. Print a smaller selection to inspect the omitted data.\n",
                "</kiki_warning>\n",
                "<kiki_result truncated=\"true\" total_lines=\"1\" total_bytes=\"10\">\n",
                "<kiki_output_head lines=\"1\" bytes=\"4\" ends_mid_line=\"true\">\n",
                "res<</kiki_output_head>\n",
                "<kiki_output_omitted lines=\"0\" bytes=\"2\" partial_lines=\"true\" />\n",
                "<kiki_output_tail lines=\"1\" bytes=\"4\" starts_mid_line=\"true\">\n",
                ">ult</kiki_output_tail>\n",
                "</kiki_result>\n",
                "<kiki_warning code=\"result_truncated\" arena_state=\"committed\">\n",
                "Execution completed, but its model-visible result was truncated. The underlying arena values were not changed. Print a smaller selection to inspect the omitted data.\n",
                "</kiki_warning>"
            )
        );
    }

    fn result_head(execution: &Execution) -> &str {
        let ExecutionOutcome::Complete {
            result: Some(result),
        } = &execution.outcome
        else {
            panic!("expected expression result, got {:?}", execution.outcome);
        };
        &result.head
    }

    #[tokio::test]
    async fn agent_default_script_reads_retained_lines_and_returns_its_final_message() {
        let workspace = tempfile::tempdir().unwrap();
        let cargo_toml = "[package]\nname = \"example\"\n";
        fs::write(workspace.path().join("Cargo.toml"), cargo_toml).unwrap();
        let arena = Arena::new(
            worker_path(),
            ReadTool::new(workspace.path())
                .await
                .expect("open read tool"),
        )
        .await
        .expect("create arena");
        let mut agent = Agent::new(arena, ScriptedModel::default());
        let cancellation = CancellationToken::new();

        let run = agent.run("inspect", &cancellation).await.unwrap();

        assert_eq!(run.final_message, "I inspected Cargo.toml.");
        assert_eq!(run.completed_steps.len(), 2);
        assert_eq!(
            run.completed_steps[0].source,
            "try:\n    request_count += 1\nexcept NameError:\n    request_count = 1\nlines = await read(\"Cargo.toml\")"
        );
        assert_eq!(
            run.completed_steps[1].source,
            "print(\"\".join(lines[0:20]), end=\"\")\nrequest_count"
        );
        assert_eq!(run.completed_steps[1].execution.stdout.head, cargo_toml);
        assert_eq!(result_head(&run.completed_steps[1].execution), "1");
        assert_eq!(
            agent.model.contexts()[0][0],
            ModelContextEntry::ReadCapability {
                description: "read(path) returns the accepted file as complete exact lines with endings retained. Use standard Python print(\"\".join(lines[start:end]), end=\"\") to inspect a selection.".to_owned(),
            }
        );
    }

    #[tokio::test]
    async fn agent_replays_the_default_script_with_persistent_arena_state_and_context() {
        let workspace = tempfile::tempdir().unwrap();
        fs::write(workspace.path().join("Cargo.toml"), "[package]\n").unwrap();
        let arena = Arena::new(
            worker_path(),
            ReadTool::new(workspace.path())
                .await
                .expect("open read tool"),
        )
        .await
        .expect("create arena");
        let mut agent = Agent::new(arena, ScriptedModel::default());
        let cancellation = CancellationToken::new();

        let first = agent.run("first", &cancellation).await.unwrap();
        let second = agent.run("second", &cancellation).await.unwrap();

        assert_eq!(result_head(&first.completed_steps[1].execution), "1");
        assert_eq!(result_head(&second.completed_steps[1].execution), "2");
        let second_request_context = &agent.model.contexts()[3];
        assert!(second_request_context.iter().any(|entry| {
            matches!(
                entry,
                ModelContextEntry::FinalMessage { text } if text == "I inspected Cargo.toml."
            )
        }));
        assert_eq!(
            second_request_context.last(),
            Some(&ModelContextEntry::UserRequest {
                text: "second".to_owned(),
            })
        );
        let ModelContextEntry::Execution { feedback, .. } = agent.model.contexts()[5]
            .last()
            .expect("second final message should receive execution feedback")
        else {
            panic!("expected completed execution context");
        };
        assert!(feedback.contains("<kiki_result truncated=\"false\" total_lines=\"1\" total_bytes=\"1\">\n<kiki_output_head lines=\"1\" bytes=\"1\" ends_mid_line=\"false\">\n2</kiki_output_head>\n</kiki_result>"));
    }

    #[test]
    fn agent_frames_python_errors_after_output_with_a_distinct_truncation_warning() {
        let execution = Execution {
            stdout: observation(
                "output\n",
                None,
                ObservationMetadata {
                    head_lines: 1,
                    tail_lines: 0,
                    total_lines: 1,
                    total_bytes: 7,
                    omitted_lines: 0,
                    omitted_bytes: 0,
                    head_ends_mid_line: false,
                    tail_starts_mid_line: false,
                },
            ),
            stderr: observation(
                "",
                None,
                ObservationMetadata {
                    head_lines: 0,
                    tail_lines: 0,
                    total_lines: 0,
                    total_bytes: 0,
                    omitted_lines: 0,
                    omitted_bytes: 0,
                    head_ends_mid_line: false,
                    tail_starts_mid_line: false,
                },
            ),
            outcome: ExecutionOutcome::PythonError(PythonError {
                text: "<error>".to_owned(),
                omitted_bytes: 3,
            }),
        };

        assert_eq!(
            super::execution_feedback(&execution),
            concat!(
                "<kiki_output stream=\"stdout\" truncated=\"false\" total_lines=\"1\" total_bytes=\"7\">\n",
                "<kiki_output_head lines=\"1\" bytes=\"7\" ends_mid_line=\"false\">\n",
                "output\n</kiki_output_head>\n",
                "</kiki_output>\n",
                "<kiki_error omitted_bytes=\"3\">\n",
                "<error></kiki_error>\n",
                "<kiki_warning code=\"error_truncated\" arena_state=\"committed\">\n",
                "Execution completed with a Python error, but its model-visible error was truncated. The underlying arena values were not changed.\n",
                "</kiki_warning>"
            )
        );
    }
}
