use std::future::Future;

use tokio_util::sync::CancellationToken;

use crate::{
    anthropic::AnthropicModel,
    config::{
        ModelSelection,
        models::Provider,
        settings::{CacheTtl, CrossProviderReasoning},
    },
    model::{
        Model, ModelError, ModelFailure, ModelHistory, ModelProgress, ModelStep, SummaryRequest,
        SummaryResult,
    },
    openai::OpenAiModel,
    subagents::ChildModelFactory,
};

#[derive(Clone)]
pub(crate) struct ProviderModel {
    openai: Option<OpenAiModel>,
    anthropic: Option<AnthropicModel>,
}

impl ProviderModel {
    pub(crate) fn new(
        prompt: &str,
        openai_api_key: Option<&str>,
        anthropic_api_key: Option<&str>,
        anthropic_cache_ttl: CacheTtl,
        cross_provider_reasoning: CrossProviderReasoning,
    ) -> Self {
        Self {
            openai: openai_api_key
                .map(|api_key| OpenAiModel::new(prompt, api_key, cross_provider_reasoning)),
            anthropic: anthropic_api_key.map(|api_key| {
                AnthropicModel::new(
                    prompt,
                    api_key,
                    anthropic_cache_ttl,
                    cross_provider_reasoning,
                )
            }),
        }
    }

    pub(crate) fn providers(&self) -> Vec<Provider> {
        [
            (Provider::OpenAi, self.openai.is_some()),
            (Provider::Anthropic, self.anthropic.is_some()),
        ]
        .into_iter()
        .filter_map(|(provider, present)| present.then_some(provider))
        .collect()
    }
}

fn missing_adapter(provider: Provider) -> ModelError {
    ModelError::InvalidSelection(match provider {
        Provider::OpenAi => "OPENAI_API_KEY is not set",
        Provider::Anthropic => "ANTHROPIC_API_KEY is not set",
        Provider::Google => "GEMINI_API_KEY is not set",
    })
}

impl Model for ProviderModel {
    fn validate_history(
        &self,
        history: &ModelHistory,
        selection: ModelSelection,
    ) -> Result<(), ModelError> {
        match selection.model().provider() {
            Provider::OpenAi => self
                .openai
                .as_ref()
                .ok_or(missing_adapter(Provider::OpenAi))?
                .validate_history(history, selection),
            Provider::Anthropic => self
                .anthropic
                .as_ref()
                .ok_or(missing_adapter(Provider::Anthropic))?
                .validate_history(history, selection),
            Provider::Google => Err(missing_adapter(Provider::Google)),
        }
    }

    #[allow(clippy::manual_async_fn)]
    fn summarise<'a>(
        &'a mut self,
        request: SummaryRequest,
        cancellation: &'a CancellationToken,
        attempt_started: &'a mut (dyn FnMut() + Send),
    ) -> impl Future<Output = Result<SummaryResult, ModelFailure>> + Send + 'a {
        async move {
            match request.selection.model().provider() {
                Provider::OpenAi => {
                    self.openai
                        .as_mut()
                        .ok_or(missing_adapter(Provider::OpenAi))?
                        .summarise(request, cancellation, attempt_started)
                        .await
                }
                Provider::Anthropic => {
                    self.anthropic
                        .as_mut()
                        .ok_or(missing_adapter(Provider::Anthropic))?
                        .summarise(request, cancellation, attempt_started)
                        .await
                }
                Provider::Google => Err(missing_adapter(Provider::Google).into()),
            }
        }
    }

    #[allow(clippy::manual_async_fn)]
    fn step<'a>(
        &'a mut self,
        selection: ModelSelection,
        history: &'a ModelHistory,
        cancellation: &'a CancellationToken,
        progress: &'a mut (dyn FnMut(ModelProgress) + Send),
    ) -> impl Future<Output = Result<ModelStep, ModelFailure>> + Send + 'a {
        async move {
            match selection.model().provider() {
                Provider::OpenAi => {
                    self.openai
                        .as_mut()
                        .ok_or(missing_adapter(Provider::OpenAi))?
                        .step(selection, history, cancellation, progress)
                        .await
                }
                Provider::Anthropic => {
                    self.anthropic
                        .as_mut()
                        .ok_or(missing_adapter(Provider::Anthropic))?
                        .step(selection, history, cancellation, progress)
                        .await
                }
                Provider::Google => Err(missing_adapter(Provider::Google).into()),
            }
        }
    }
}

impl ChildModelFactory for ProviderModel {
    type Model = Self;

    fn create(&self, prompt: String) -> Self {
        Self {
            openai: self
                .openai
                .as_ref()
                .map(|model| model.with_prompt(prompt.clone())),
            anthropic: self
                .anthropic
                .as_ref()
                .map(|model| model.with_prompt(prompt)),
        }
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;
    use tokio_util::sync::CancellationToken;

    use super::ProviderModel;
    use crate::{
        anthropic::AnthropicModel,
        config::{
            ModelSelection,
            models::{OPUS_5_5, Provider, SOL},
            settings::{CacheTtl, CrossProviderReasoning},
        },
        model::{
            Model, ModelAction, ModelError, ModelFailure, ModelHistory, ModelStep, SummaryRequest,
        },
        openai::OpenAiModel,
        prompt::{child_prompt, root_prompt},
        skills::SkillCatalog,
        sse_fixture::{Fixture, FixtureReply},
        subagents::ChildModelFactory,
    };

    const OPENAI_KEY: &str = "openai-fixture-key";
    const ANTHROPIC_KEY: &str = "anthropic-fixture-key";

    fn openai() -> ModelSelection {
        ModelSelection::new(&SOL, SOL.default_thinking).unwrap()
    }

    fn anthropic() -> ModelSelection {
        ModelSelection::new(&OPUS_5_5, OPUS_5_5.default_thinking).unwrap()
    }

    fn history() -> ModelHistory {
        let mut history = ModelHistory::new();
        history.append_user_request("inspect");
        history
    }

    fn summary_request(selection: ModelSelection) -> SummaryRequest {
        SummaryRequest {
            instructions: "summary instructions".to_owned(),
            input: "summary input".to_owned(),
            selection,
            output_limit: 321,
        }
    }

    fn openai_message(text: &str) -> FixtureReply {
        FixtureReply::OwnedSse(format!(
            "data: {}\n\ndata: [DONE]\n\n",
            json!({
                "type": "response.completed",
                "sequence_number": 1,
                "response": {
                    "id": "resp_1",
                    "status": "completed",
                    "service_tier": "default",
                    "output": [{
                        "type": "message",
                        "id": "msg_1",
                        "role": "assistant",
                        "content": [{"type": "output_text", "text": text}],
                        "status": "completed"
                    }],
                    "usage": {
                        "input_tokens": 1,
                        "input_tokens_details": {"cached_tokens": 0, "cache_write_tokens": 0},
                        "output_tokens": 1,
                        "output_tokens_details": {"reasoning_tokens": 0},
                        "total_tokens": 2
                    }
                }
            })
        ))
    }

    fn anthropic_message(text: &str) -> FixtureReply {
        let events = [
            json!({
                "type": "message_start",
                "message": {
                    "id": "msg_1",
                    "type": "message",
                    "role": "assistant",
                    "content": [],
                    "model": "claude-opus-5-5",
                    "stop_reason": null,
                    "stop_sequence": null,
                    "usage": {"input_tokens": 1, "output_tokens": 1}
                }
            }),
            json!({"type": "content_block_start", "index": 0, "content_block": {"type": "text", "text": ""}}),
            json!({"type": "content_block_delta", "index": 0, "delta": {"type": "text_delta", "text": text}}),
            json!({"type": "content_block_stop", "index": 0}),
            json!({
                "type": "message_delta",
                "delta": {"stop_reason": "end_turn", "stop_sequence": null},
                "usage": {"output_tokens": 1}
            }),
            json!({"type": "message_stop"}),
        ];
        FixtureReply::OwnedSse(
            events
                .iter()
                .map(|event| {
                    format!(
                        "event: {}\ndata: {event}\n\n",
                        event["type"].as_str().unwrap()
                    )
                })
                .collect(),
        )
    }

    fn router(
        prompt: &str,
        openai: Option<&Fixture>,
        anthropic: Option<&Fixture>,
    ) -> ProviderModel {
        ProviderModel {
            openai: openai
                .map(|fixture| OpenAiModel::with_endpoint(prompt, OPENAI_KEY, &fixture.endpoint)),
            anthropic: anthropic.map(|fixture| {
                AnthropicModel::with_endpoint(
                    prompt,
                    ANTHROPIC_KEY,
                    &fixture.endpoint,
                    CacheTtl::FiveMinutes,
                    CrossProviderReasoning::Inline,
                )
            }),
        }
    }

    async fn step(
        model: &mut ProviderModel,
        selection: ModelSelection,
    ) -> Result<ModelStep, ModelFailure> {
        let cancellation = CancellationToken::new();
        model
            .step(selection, &history(), &cancellation, &mut |_| {})
            .await
    }

    fn header<'a>(head: &'a str, name: &str) -> Option<&'a str> {
        head.lines().find_map(|line| {
            let (header, value) = line.split_once(':')?;
            header.eq_ignore_ascii_case(name).then(|| value.trim())
        })
    }

    #[tokio::test]
    async fn each_selection_dispatches_to_its_provider_adapter() {
        let openai_fixture = Fixture::start_sequence(vec![
            openai_message("from OpenAI"),
            openai_message("OpenAI summary"),
        ])
        .await;
        let anthropic_fixture = Fixture::start_sequence(vec![
            anthropic_message("from Anthropic"),
            anthropic_message("Anthropic summary"),
        ])
        .await;
        let mut model = router(
            "root prompt",
            Some(&openai_fixture),
            Some(&anthropic_fixture),
        );
        let cancellation = CancellationToken::new();

        let anthropic_step = step(&mut model, anthropic()).await.unwrap();
        assert_eq!(
            anthropic_step.action,
            ModelAction::Message {
                text: "from Anthropic".to_owned()
            }
        );
        assert_eq!(anthropic_step.replay.provider(), "anthropic");
        assert_eq!(
            (
                openai_fixture.request_count(),
                anthropic_fixture.request_count()
            ),
            (0, 1)
        );

        let openai_step = step(&mut model, openai()).await.unwrap();
        assert_eq!(
            openai_step.action,
            ModelAction::Message {
                text: "from OpenAI".to_owned()
            }
        );
        assert_eq!(openai_step.replay.provider(), "openai");
        assert_eq!(
            (
                openai_fixture.request_count(),
                anthropic_fixture.request_count()
            ),
            (1, 1)
        );

        let mut anthropic_history = history();
        anthropic_history.append_final_message(
            "from Anthropic".to_owned(),
            String::new(),
            anthropic_step.replay,
        );
        assert_eq!(
            model.validate_history(&anthropic_history, anthropic()),
            Ok(())
        );
        assert_eq!(model.validate_history(&anthropic_history, openai()), Ok(()));

        let summary = model
            .summarise(summary_request(openai()), &cancellation, &mut || {})
            .await
            .unwrap();
        assert_eq!(summary.text, "OpenAI summary");
        let summary = model
            .summarise(summary_request(anthropic()), &cancellation, &mut || {})
            .await
            .unwrap();
        assert_eq!(summary.text, "Anthropic summary");
        assert_eq!(
            (
                openai_fixture.request_count(),
                anthropic_fixture.request_count()
            ),
            (2, 2)
        );
    }

    #[tokio::test]
    async fn a_missing_adapter_is_an_invalid_selection_naming_its_key_variable() {
        let fixture = Fixture::start(openai_message("unused")).await;
        let cancellation = CancellationToken::new();

        for (mut model, selection, variable) in [
            (
                router("root prompt", Some(&fixture), None),
                anthropic(),
                "ANTHROPIC_API_KEY",
            ),
            (
                router("root prompt", None, Some(&fixture)),
                openai(),
                "OPENAI_API_KEY",
            ),
        ] {
            let mut progress_events = 0;
            let mut attempts_started = 0;
            let step = model
                .step(selection, &history(), &cancellation, &mut |_| {
                    progress_events += 1
                })
                .await
                .unwrap_err();
            let summary = model
                .summarise(summary_request(selection), &cancellation, &mut || {
                    attempts_started += 1
                })
                .await
                .unwrap_err();
            let validation = model.validate_history(&history(), selection).unwrap_err();

            for error in [step.error(), summary.error(), &validation] {
                let ModelError::InvalidSelection(message) = error else {
                    panic!("expected an invalid selection, got {error:?}");
                };
                assert!(message.contains(variable), "{message}");
            }
            assert!(step.attempts().is_empty());
            assert!(summary.attempts().is_empty());
            assert_eq!((progress_events, attempts_started), (0, 0));
        }
        assert_eq!(fixture.request_count(), 0);
    }

    #[test]
    fn the_provider_set_matches_the_present_keys() {
        for (openai_key, anthropic_key, providers) in [
            (
                Some("openai-key"),
                Some("anthropic-key"),
                vec![Provider::OpenAi, Provider::Anthropic],
            ),
            (Some("openai-key"), None, vec![Provider::OpenAi]),
            (None, Some("anthropic-key"), vec![Provider::Anthropic]),
            (None, None, Vec::new()),
        ] {
            assert_eq!(
                ProviderModel::new(
                    "root prompt",
                    openai_key,
                    anthropic_key,
                    CacheTtl::FiveMinutes,
                    CrossProviderReasoning::Inline
                )
                .providers(),
                providers
            );
        }
    }

    #[tokio::test]
    async fn child_requests_use_the_supplied_prompt_and_the_parent_keeps_its_root_prompt() {
        let workspace = tempfile::tempdir().unwrap();
        let reference = crate::tools::ToolHost::new(workspace.path())
            .await
            .unwrap()
            .for_child(true)
            .prompt_reference(true);
        let empty = SkillCatalog::default();
        let root = root_prompt("canonical base", &empty);
        let child_instructions = child_prompt("canonical base", &empty, reference);
        assert!(root.contains("async def write("));
        assert!(!child_instructions.contains("async def write("));
        let openai_fixture = Fixture::start(openai_message("done")).await;
        let anthropic_fixture = Fixture::start(anthropic_message("done")).await;
        let mut parent = ProviderModel {
            anthropic: Some(AnthropicModel::with_endpoint(
                &root,
                ANTHROPIC_KEY,
                &anthropic_fixture.endpoint,
                CacheTtl::OneHour,
                CrossProviderReasoning::Inline,
            )),
            ..router(&root, Some(&openai_fixture), None)
        };

        let mut child = parent.create(child_instructions.clone());
        for selection in [openai(), anthropic()] {
            step(&mut child, selection).await.unwrap();
            step(&mut parent, selection).await.unwrap();
        }

        let openai_requests = openai_fixture.requests.lock().unwrap().clone();
        let anthropic_requests = anthropic_fixture.requests.lock().unwrap().clone();
        assert_eq!(
            openai_requests
                .iter()
                .map(|request| request.body["instructions"].clone())
                .collect::<Vec<_>>(),
            [json!(child_instructions), json!(root)]
        );
        assert_eq!(
            anthropic_requests
                .iter()
                .map(|request| request.body["system"].clone())
                .collect::<Vec<_>>(),
            [
                json!([{
                    "type": "text",
                    "text": child_instructions,
                    "cache_control": {"type": "ephemeral", "ttl": "1h"}
                }]),
                json!([{
                    "type": "text",
                    "text": root,
                    "cache_control": {"type": "ephemeral", "ttl": "1h"}
                }])
            ]
        );
        for request in &openai_requests {
            assert_eq!(
                header(&request.head, "authorization"),
                Some(format!("Bearer {OPENAI_KEY}").as_str())
            );
        }
        for request in &anthropic_requests {
            assert_eq!(header(&request.head, "x-api-key"), Some(ANTHROPIC_KEY));
        }
    }
}
