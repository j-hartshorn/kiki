pub mod agent;
pub mod arena;
mod clipboard;
pub mod config;
mod highlight;
pub mod model;
mod openai;
pub mod pricing;
pub mod prompt;
mod prompt_editor;
pub mod runtime;
mod selection;
mod session;
mod subagents;
mod telemetry;
pub mod tools;
mod transcript;
pub mod tui;

use std::error::Error;
use std::io;
use std::path::PathBuf;
use std::sync::Arc;

use tokio::runtime::Builder;
use tokio::sync::{mpsc, watch};

use crate::agent::Agent;
use crate::arena::Arena;
use crate::config::ModelSelection;
use crate::openai::{OpenAiChildModelFactory, OpenAiModel};
use crate::runtime::{
    PROGRESS_CHANNEL_CAPACITY, ProgressEnvelope, RuntimeCommand, RuntimeController, RuntimeEvent,
};
use crate::session::Session;
use crate::subagents::{
    CompleteChildRunner, SessionSubagents, SubagentCoordinator, SubagentPolicy,
};
use crate::telemetry::TraceOwner;
use crate::tools::ToolHost;

fn production_model(
    ready: &config::ReadyStartup<'_>,
    settings: watch::Receiver<ModelSelection>,
) -> OpenAiModel {
    OpenAiModel::with_settings(ready.system_prompt(), ready.api_key().expose(), settings)
}

fn run_configuration(
    startup: config::StartupSnapshot,
    trace_owner: TraceOwner,
) -> Result<(), Box<dyn Error>> {
    let tui_result = tui::run_configuration(startup);
    let trace_result = trace_owner.shutdown();
    tui_result?;
    trace_result?;
    Ok(())
}

fn main() -> Result<(), Box<dyn Error>> {
    let trace_owner = TraceOwner::from_path(std::env::var_os("KIKI_TRACE_FILE"))?;
    let trace = trace_owner.handle();
    let configuration = config::load();
    let Some(ready) = configuration.ready() else {
        return run_configuration(configuration.snapshot().clone(), trace_owner);
    };
    let startup = configuration.snapshot().clone();
    let worker_path = ready.worker_path().to_path_buf();
    let workspace_root = ready.workspace_root().to_path_buf();
    let selection = ModelSelection::new(ready.model(), ready.thinking())
        .expect("ready startup selection must be valid");
    let (settings, model_settings) = watch::channel(selection);
    let model = production_model(&ready, model_settings);

    let runtime = Builder::new_multi_thread().enable_all().build()?;
    let tool_host = match runtime.block_on(ToolHost::new(&workspace_root)) {
        Ok(tool_host) => tool_host,
        Err(error) => {
            let configuration = configuration.with_workspace_construction_failure(error);
            return run_configuration(configuration.snapshot().clone(), trace_owner);
        }
    };
    let arena = match runtime.block_on(Arena::new(worker_path, tool_host)) {
        Ok(arena) => arena,
        Err(error) => {
            let configuration = configuration.with_worker_construction_failure(error);
            return run_configuration(configuration.snapshot().clone(), trace_owner);
        }
    };
    let home = std::env::var_os("HOME")
        .map(PathBuf::from)
        .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "HOME is unset"))?;
    let session = Session::create(&home, &workspace_root, selection, arena.checkpoint())?;
    let subagents: SessionSubagents = SubagentCoordinator::new(
        SubagentPolicy::from(startup.subagents()),
        Arc::new(CompleteChildRunner::new(
            ready.prompt_base().to_owned(),
            OpenAiChildModelFactory::new(ready.api_key().expose()),
        )),
    )
    .with_trace(trace.clone());
    let controller = RuntimeController::new(
        Agent::new(arena, model),
        settings,
        startup.settings().clone(),
        subagents,
    )
    .with_trace(trace.clone());
    let (command_sender, command_receiver) = mpsc::channel(8);
    let (event_sender, event_receiver) = mpsc::unbounded_channel::<RuntimeEvent>();
    let (progress_sender, progress_receiver) =
        mpsc::channel::<ProgressEnvelope<RuntimeEvent>>(PROGRESS_CHANNEL_CAPACITY);
    let controller = runtime.spawn(controller.run(command_receiver, event_sender, progress_sender));

    let tui_result = tui::run(
        startup,
        command_sender.clone(),
        event_receiver,
        progress_receiver,
        trace,
    );
    let _ = command_sender.blocking_send(RuntimeCommand::Shutdown);
    let controller_result = runtime.block_on(controller);
    let session_result = session.close();
    let trace_result = trace_owner.shutdown();

    tui_result?;
    controller_result?;
    session_result?;
    trace_result?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::production_model;
    use tokio::sync::watch;

    use crate::config::{ModelSelection, ReadyStartup};
    use crate::openai::OpenAiModel;

    #[test]
    fn production_model_composition_is_openai() {
        let _: for<'a> fn(&ReadyStartup<'a>, watch::Receiver<ModelSelection>) -> OpenAiModel =
            production_model;
    }
}
