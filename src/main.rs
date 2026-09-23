pub mod agent;
pub mod arena;
mod clipboard;
mod compaction;
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
use std::path::{Path, PathBuf};
use std::sync::Arc;

use time::OffsetDateTime;
use time::format_description::well_known::Rfc3339;
use tokio::runtime::Builder;
use tokio::sync::{mpsc, watch};
use unicode_segmentation::UnicodeSegmentation;

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

enum Invocation {
    Fresh,
    List,
    Resume { id: String, here: bool },
}

fn usage() -> io::Error {
    io::Error::new(
        io::ErrorKind::InvalidInput,
        "Usage: kiki [sessions | --resume <32-lowerhex-id> [--here]]",
    )
}

fn valid_session_id(value: &str) -> bool {
    value.len() == 32
        && value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (byte.is_ascii_lowercase() && byte <= b'f'))
}

fn invocation() -> Result<Invocation, io::Error> {
    let mut arguments = Vec::new();
    for argument in std::env::args_os().skip(1) {
        arguments.push(argument.into_string().map_err(|_| usage())?);
    }
    match arguments.as_slice() {
        [] => Ok(Invocation::Fresh),
        [command] if command == "sessions" => Ok(Invocation::List),
        [command, id] if command == "--resume" && valid_session_id(id) => Ok(Invocation::Resume {
            id: id.clone(),
            here: false,
        }),
        [command, id, here]
            if command == "--resume" && here == "--here" && valid_session_id(id) =>
        {
            Ok(Invocation::Resume {
                id: id.clone(),
                here: true,
            })
        }
        _ => Err(usage()),
    }
}

fn home_directory() -> Result<PathBuf, io::Error> {
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "HOME is unset"))
}

fn current_workspace_root() -> Result<PathBuf, io::Error> {
    let current = std::env::current_dir().map_err(|error| {
        io::Error::other(format!("could not determine current workspace: {error}"))
    })?;
    std::fs::canonicalize(current)
        .map_err(|error| io::Error::other(format!("could not resolve current workspace: {error}")))
}

fn saved_workspace_root(cwd: &Path) -> Result<PathBuf, io::Error> {
    std::fs::canonicalize(cwd)
        .map_err(|error| io::Error::other(format!("could not resolve saved workspace: {error}")))
}

fn session_timestamp(updated_at_ms: i64) -> Result<String, io::Error> {
    OffsetDateTime::from_unix_timestamp_nanos(i128::from(updated_at_ms) * 1_000_000)
        .map_err(|error| io::Error::other(format!("invalid session timestamp: {error}")))?
        .format(&Rfc3339)
        .map_err(|error| io::Error::other(format!("could not format session timestamp: {error}")))
}

fn session_description(name: Option<&str>, request: Option<&str>) -> String {
    let text = name
        .filter(|name| !name.trim().is_empty())
        .or(request)
        .unwrap_or("(empty session)");
    let text = crate::transcript::normalise_terminal_text(text);
    let text = text.split_whitespace().collect::<Vec<_>>().join(" ");
    let mut graphemes = text.graphemes(true);
    let preview = graphemes.by_ref().take(79).collect::<String>();
    if graphemes.next().is_some() {
        format!("{preview}…")
    } else if preview.is_empty() {
        "(empty session)".to_owned()
    } else {
        preview
    }
}

fn list_sessions(home: &Path) -> Result<(), Box<dyn Error>> {
    let current_root = current_workspace_root()?;
    let (mut rows, warnings) = Session::list(home)?.into_parts();
    for warning in warnings {
        eprintln!("Warning: could not read session {}.", warning.id());
    }
    rows.retain(|row| match saved_workspace_root(row.cwd()) {
        Ok(saved_root) if saved_root == current_root => true,
        Ok(_) => false,
        Err(_) => {
            eprintln!("Warning: could not resolve session {} workspace.", row.id());
            false
        }
    });
    rows.sort_by(|left, right| {
        right
            .updated_at_ms()
            .cmp(&left.updated_at_ms())
            .then_with(|| left.id().cmp(right.id()))
    });
    for row in rows {
        let timestamp = match session_timestamp(row.updated_at_ms()) {
            Ok(timestamp) => timestamp,
            Err(_) => {
                eprintln!("Warning: could not read session {}.", row.id());
                continue;
            }
        };
        println!(
            "{} {} {}",
            row.id(),
            timestamp,
            session_description(row.name(), row.first_request())
        );
    }
    Ok(())
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

fn close_open_session(session: Session, trace_owner: TraceOwner) -> Result<(), Box<dyn Error>> {
    let id = session.id().to_owned();
    let session_result = session.close();
    let trace_result = trace_owner.shutdown();
    println!("Session: {id}");
    session_result?;
    trace_result?;
    Ok(())
}

fn discard_open_session(session: Session, trace_owner: TraceOwner) {
    let _ = session.close();
    let _ = trace_owner.shutdown();
}

fn run_open_configuration(
    session: Session,
    startup: config::StartupSnapshot,
    trace_owner: TraceOwner,
) -> Result<(), Box<dyn Error>> {
    let tui_result = tui::run_configuration(startup);
    let close_result = close_open_session(session, trace_owner);
    tui_result?;
    close_result
}

fn run_fresh(home: PathBuf) -> Result<(), Box<dyn Error>> {
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
    let prompt_base = ready.prompt_base().to_owned();
    let api_key = ready.api_key().expose().to_owned();

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
    let session = Session::create(&home, &workspace_root, selection, arena.checkpoint())?;
    let subagents: SessionSubagents = SubagentCoordinator::new(
        SubagentPolicy::from(startup.subagents()),
        Arc::new(CompleteChildRunner::new(
            prompt_base,
            OpenAiChildModelFactory::new(api_key),
        )),
    )
    .with_trace(trace.clone());
    let controller = RuntimeController::with_session(
        Agent::new(arena, model),
        settings,
        startup.settings().clone(),
        subagents,
        session.writer(),
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
    let close_result = close_open_session(session, trace_owner);
    tui_result?;
    controller_result?;
    close_result
}

fn run_resume(home: PathBuf, id: String, here: bool) -> Result<(), Box<dyn Error>> {
    let current_root = current_workspace_root()?;
    match Session::read_cwd(&home, &id) {
        Ok(saved_cwd) if !here => {
            if saved_workspace_root(&saved_cwd)? != current_root {
                return Err(io::Error::other("saved session belongs to another workspace").into());
            }
        }
        Ok(_) => {}
        Err(error) if error.requires_writable_recovery() => {}
        Err(error) => return Err(error.into()),
    }
    let trace_owner = TraceOwner::from_path(std::env::var_os("KIKI_TRACE_FILE"))?;
    let trace = trace_owner.handle();
    let session = Session::open(&home, &id)?;
    if !here {
        let owned_cwd = session.cwd().map_err(|error| {
            io::Error::other(format!("could not read saved session workspace: {error}"))
        });
        let workspace_matches = owned_cwd.and_then(|cwd| {
            (saved_workspace_root(&cwd)? == current_root)
                .then_some(())
                .ok_or_else(|| io::Error::other("saved session belongs to another workspace"))
        });
        if let Err(error) = workspace_matches {
            discard_open_session(session, trace_owner);
            return Err(error.into());
        }
    }
    let session_preferences = session
        .metadata()
        .ok()
        .map(|metadata| metadata.preferences().clone());
    let configuration = config::load_with_session_preferences(session_preferences);
    let startup = configuration.snapshot().clone();
    let Some(ready) = configuration.ready() else {
        return run_open_configuration(session, startup, trace_owner);
    };
    let worker_path = ready.worker_path().to_path_buf();
    let workspace_root = ready.workspace_root().to_path_buf();
    let selection = ModelSelection::new(ready.model(), ready.thinking())
        .expect("ready startup selection must be valid");
    let (settings, model_settings) = watch::channel(selection);
    let model = production_model(&ready, model_settings);
    let prompt_base = ready.prompt_base().to_owned();
    let api_key = ready.api_key().expose().to_owned();
    let runtime = match Builder::new_multi_thread().enable_all().build() {
        Ok(runtime) => runtime,
        Err(error) => {
            close_open_session(session, trace_owner)?;
            return Err(error.into());
        }
    };
    let tool_host = match runtime.block_on(ToolHost::new(&workspace_root)) {
        Ok(tool_host) => tool_host,
        Err(error) => {
            let configuration = configuration.with_workspace_construction_failure(error);
            return run_open_configuration(session, configuration.snapshot().clone(), trace_owner);
        }
    };
    let arena = match runtime.block_on(Arena::new(worker_path, tool_host)) {
        Ok(arena) => arena,
        Err(error) => {
            let configuration = configuration.with_worker_construction_failure(error);
            return run_open_configuration(session, configuration.snapshot().clone(), trace_owner);
        }
    };
    let subagents: SessionSubagents = SubagentCoordinator::new(
        SubagentPolicy::from(startup.subagents()),
        Arc::new(CompleteChildRunner::new(
            prompt_base,
            OpenAiChildModelFactory::new(api_key),
        )),
    )
    .with_trace(trace.clone());
    let relocated_cwd = here.then_some(workspace_root);
    let (controller, initial) = match runtime.block_on(RuntimeController::restore(
        &session,
        arena,
        model,
        settings,
        startup.settings().clone(),
        subagents,
        relocated_cwd,
    )) {
        Ok(restored) => restored,
        Err(error) => {
            close_open_session(session, trace_owner)?;
            return Err(error.into());
        }
    };
    if here && initial.disabled_reason.is_none() {
        eprintln!(
            "Warning: session relocated; retained paths, data, and external effects were not rewritten."
        );
    }
    let controller = controller.with_trace(trace.clone());
    let (command_sender, command_receiver) = mpsc::channel(8);
    let (event_sender, event_receiver) = mpsc::unbounded_channel::<RuntimeEvent>();
    let (progress_sender, progress_receiver) =
        mpsc::channel::<ProgressEnvelope<RuntimeEvent>>(PROGRESS_CHANNEL_CAPACITY);
    let controller = runtime.spawn(controller.run(command_receiver, event_sender, progress_sender));
    let tui_result = tui::run_restored(
        startup,
        initial,
        command_sender.clone(),
        event_receiver,
        progress_receiver,
        trace,
    );
    let _ = command_sender.blocking_send(RuntimeCommand::Shutdown);
    let controller_result = runtime.block_on(controller);
    let close_result = close_open_session(session, trace_owner);
    tui_result?;
    controller_result?;
    close_result
}

fn run() -> Result<(), Box<dyn Error>> {
    let invocation = invocation()?;
    let home = home_directory()?;
    match invocation {
        Invocation::Fresh => run_fresh(home),
        Invocation::List => list_sessions(&home),
        Invocation::Resume { id, here } => run_resume(home, id, here),
    }
}

fn main() {
    if let Err(error) = run() {
        eprintln!("{error}");
        std::process::exit(1);
    }
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
