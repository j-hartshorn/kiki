pub mod agent;
pub mod arena;
pub mod model;
mod openai;
pub mod prompt;
pub mod runtime;
pub mod tools;
pub mod tui;

use std::env;
use std::error::Error;
use std::io;
use std::path::PathBuf;

use tokio::runtime::Builder;
use tokio::sync::mpsc;

use crate::agent::Agent;
use crate::arena::Arena;
use crate::model::ScriptedModel;
use crate::runtime::{RuntimeCommand, RuntimeController};
use crate::tools::ReadTool;

fn main() -> Result<(), Box<dyn Error>> {
    let worker_path = env::var_os("MONTY_BIN")
        .map(PathBuf::from)
        .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "MONTY_BIN is required"))?;
    if !worker_path.is_absolute() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "MONTY_BIN must be an absolute path",
        )
        .into());
    }

    let workspace = env::current_dir()?;
    let runtime = Builder::new_multi_thread().enable_all().build()?;
    let read_tool = runtime.block_on(ReadTool::new(workspace))?;
    let arena = runtime.block_on(Arena::new(worker_path, read_tool))?;
    let controller = RuntimeController::new(Agent::new(arena, ScriptedModel::default()));
    let (command_sender, command_receiver) = mpsc::channel(8);
    let (event_sender, event_receiver) = mpsc::unbounded_channel();
    let controller = runtime.spawn(controller.run(command_receiver, event_sender));

    let tui_result = tui::run(command_sender.clone(), event_receiver);
    let _ = command_sender.blocking_send(RuntimeCommand::Shutdown);
    let controller_result = runtime.block_on(controller);

    tui_result?;
    controller_result?;
    Ok(())
}
