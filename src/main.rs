pub mod agent;
pub mod arena;
pub mod config;
pub mod model;
mod openai;
pub mod pricing;
pub mod prompt;
pub mod runtime;
pub mod tools;
pub mod tui;

use std::error::Error;

use tokio::runtime::Builder;
use tokio::sync::mpsc;

use crate::agent::Agent;
use crate::arena::Arena;
use crate::openai::OpenAiModel;
use crate::runtime::{
    PROGRESS_CHANNEL_CAPACITY, ProgressEnvelope, RuntimeCommand, RuntimeController, RuntimeEvent,
};
use crate::tools::ReadTool;

fn production_model(ready: &config::ReadyStartup<'_>) -> OpenAiModel {
    OpenAiModel::new(ready.system_prompt(), ready.api_key().expose())
}

fn main() -> Result<(), Box<dyn Error>> {
    let configuration = config::load();
    let Some(ready) = configuration.ready() else {
        return Ok(tui::run_configuration(configuration.snapshot().clone())?);
    };
    let worker_path = ready.worker_path().to_path_buf();
    let workspace_root = ready.workspace_root().to_path_buf();
    let model = production_model(&ready);

    let runtime = Builder::new_multi_thread().enable_all().build()?;
    let read_tool = match runtime.block_on(ReadTool::new(workspace_root)) {
        Ok(read_tool) => read_tool,
        Err(error) => {
            let configuration = configuration.with_workspace_construction_failure(error);
            return Ok(tui::run_configuration(configuration.snapshot().clone())?);
        }
    };
    let arena = match runtime.block_on(Arena::new(worker_path, read_tool)) {
        Ok(arena) => arena,
        Err(error) => {
            let configuration = configuration.with_worker_construction_failure(error);
            return Ok(tui::run_configuration(configuration.snapshot().clone())?);
        }
    };
    let startup = configuration.snapshot().clone();
    let controller = RuntimeController::new(Agent::new(arena, model));
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
    );
    let _ = command_sender.blocking_send(RuntimeCommand::Shutdown);
    let controller_result = runtime.block_on(controller);

    tui_result?;
    controller_result?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::production_model;
    use crate::config::ReadyStartup;
    use crate::openai::OpenAiModel;

    #[test]
    fn production_model_composition_is_openai() {
        let _: for<'a> fn(&ReadyStartup<'a>) -> OpenAiModel = production_model;
    }
}
