pub mod agent;
mod anthropic;
pub mod arena;
mod clipboard;
mod compaction;
pub mod config;
mod highlight;
pub mod model;
mod openai;
mod presentation;
pub mod pricing;
pub mod prompt;
mod prompt_editor;
mod provider;
pub mod runtime;
mod selection;
mod session;
mod skills;
#[cfg(test)]
mod sse_fixture;
mod subagents;
mod telemetry;
pub mod tools;
mod transcript;
pub mod tui;

use std::error::Error;
use std::ffi::OsStr;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use time::OffsetDateTime;
use time::format_description::well_known::Rfc3339;
use tokio::runtime::Builder;
use tokio::sync::mpsc;
use unicode_segmentation::UnicodeSegmentation;

use crate::agent::Agent;
use crate::arena::Arena;
use crate::config::{
    ApiKey, ModelSelection,
    models::Provider,
    settings::{self, CacheTtl, CrossProviderReasoning},
};
use crate::provider::ProviderModel;
use crate::runtime::{
    PROGRESS_CHANNEL_CAPACITY, ProgressEnvelope, ResumeOptions, RuntimeCommand, RuntimeController,
    RuntimeEvent,
};
use crate::session::{Session, SessionListRow};
use crate::skills::{
    ProjectScope, SkillCatalog, SkillProjectionPolicy, project_trust_prompt_required,
};
use crate::subagents::{
    CompleteChildRunner, SessionSubagents, SubagentCoordinator, SubagentPolicy,
};
use crate::telemetry::TraceOwner;
use crate::tools::ToolHost;

fn production_model(
    ready: &config::ReadyStartup<'_>,
    catalog: Option<&SkillCatalog>,
    anthropic_cache_ttl: CacheTtl,
    cross_provider_reasoning: CrossProviderReasoning,
) -> ProviderModel {
    let system_prompt =
        production_system_prompt(ready.prompt_base(), ready.system_prompt(), catalog);
    ProviderModel::new(
        &system_prompt,
        ready.api_key(Provider::OpenAi).map(ApiKey::expose),
        ready.api_key(Provider::Anthropic).map(ApiKey::expose),
        anthropic_cache_ttl,
        cross_provider_reasoning,
    )
}

fn production_system_prompt(
    base: &str,
    resolved_without_skills: &str,
    catalog: Option<&SkillCatalog>,
) -> String {
    catalog
        .filter(|catalog| catalog.rendered_catalog().is_some())
        .map_or_else(
            || resolved_without_skills.to_owned(),
            |catalog| prompt::root_prompt_with_catalog(base, catalog),
        )
}

enum Invocation {
    Fresh,
    List,
    Continue,
    Resume { id: String, here: bool },
}

fn usage() -> io::Error {
    io::Error::new(
        io::ErrorKind::InvalidInput,
        "Usage: kiki [sessions | --continue | --resume <32-lowerhex-id> [--here]]",
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
        [command] if command == "--continue" => Ok(Invocation::Continue),
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

fn requested_type_check_disabled(value: Option<&OsStr>) -> Result<bool, io::Error> {
    match value {
        None => Ok(false),
        Some(value) if value == "on" => Ok(false),
        Some(value) if value == "off" => Ok(true),
        Some(_) => Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "KIKI_TYPE_CHECK must be `on` or `off`",
        )),
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

fn displayed_sessions(home: &Path) -> Result<Vec<(SessionListRow, String)>, Box<dyn Error>> {
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
    Ok(rows
        .into_iter()
        .filter_map(|row| match session_timestamp(row.updated_at_ms()) {
            Ok(timestamp) => Some((row, timestamp)),
            Err(_) => {
                eprintln!("Warning: could not read session {}.", row.id());
                None
            }
        })
        .collect())
}

fn list_sessions(home: &Path) -> Result<(), Box<dyn Error>> {
    for (row, timestamp) in displayed_sessions(home)? {
        println!(
            "{} {} {}",
            row.id(),
            timestamp,
            session_description(row.name(), row.first_request())
        );
    }
    Ok(())
}

fn continue_latest_session(home: PathBuf) -> Result<(), Box<dyn Error>> {
    let Some((latest, _)) = displayed_sessions(&home)?.into_iter().next() else {
        return Err(io::Error::other("no saved session for the current workspace").into());
    };
    run_resume(home, latest.id().to_owned(), false)
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

fn startup_skill_catalog(
    scope: &ProjectScope,
    home: &Path,
    guidance: &str,
    has_active_project_skill_in_scope: bool,
    prompt: impl FnOnce(&Path) -> Result<bool, tui::TuiError>,
) -> Result<(Arc<SkillCatalog>, bool), Box<dyn Error>> {
    let saved_trust = settings::is_project_skill_scope_trusted(home, &scope.canonical_scope);
    let include_project =
        if project_trust_prompt_required(scope, saved_trust, has_active_project_skill_in_scope) {
            prompt(&scope.canonical_scope)?
                && settings::is_project_skill_scope_trusted(home, &scope.canonical_scope)
        } else {
            saved_trust
        };
    Ok((
        Arc::new(SkillCatalog::discover(
            scope,
            home,
            include_project,
            guidance,
        )),
        include_project,
    ))
}

fn run_fresh(home: PathBuf) -> Result<(), Box<dyn Error>> {
    let disable_type_check =
        requested_type_check_disabled(std::env::var_os("KIKI_TYPE_CHECK").as_deref())?;
    let trace_owner = TraceOwner::from_path(std::env::var_os("KIKI_TRACE_FILE"))?;
    let trace = trace_owner.handle();
    let configuration = config::load();
    let Some(ready) = configuration.ready() else {
        return run_configuration(configuration.snapshot().clone(), trace_owner);
    };
    let startup = configuration.snapshot().clone();
    let worker_path = ready.worker_path().to_path_buf();
    let workspace_root = ready.workspace_root().to_path_buf();
    let skill_scope = ProjectScope::discover(&workspace_root)?;
    let (skills_catalog, project_skills_trusted) = match startup_skill_catalog(
        &skill_scope,
        &home,
        ready.skills_catalog_guidance(),
        false,
        |scope| {
            tui::run_project_skill_trust(scope, || {
                settings::save_project_skill_scope_trust(&home, scope)
                    .map_err(|error| error.to_string())
            })
        },
    ) {
        Ok(catalog) => catalog,
        Err(error) => {
            let _ = trace_owner.shutdown();
            return Err(error);
        }
    };
    let selection = ModelSelection::new(ready.model(), ready.thinking())
        .expect("ready startup selection must be valid");
    let model = production_model(
        &ready,
        Some(&skills_catalog),
        startup.settings().anthropic_cache_ttl(),
        startup.settings().cross_provider_reasoning(),
    );
    let providers = model.providers();
    let prompt_base = ready.prompt_base().to_owned();

    let runtime = Builder::new_multi_thread().enable_all().build()?;
    let tool_host = match runtime.block_on(ToolHost::new(&workspace_root)) {
        Ok(tool_host) => tool_host,
        Err(error) => {
            let configuration = configuration.with_workspace_construction_failure(error);
            return run_configuration(configuration.snapshot().clone(), trace_owner);
        }
    };
    let mut arena = match runtime.block_on(Arena::new(
        worker_path,
        tool_host.with_skill_catalog(skills_catalog.clone()),
    )) {
        Ok(arena) => arena,
        Err(error) => {
            let configuration = configuration.with_worker_construction_failure(error);
            return run_configuration(configuration.snapshot().clone(), trace_owner);
        }
    };
    let session = Session::create_with_type_check_disabled(
        &home,
        &workspace_root,
        selection,
        arena.checkpoint(),
        disable_type_check,
    )?;
    session.configure_skill_projection(SkillProjectionPolicy::new(
        project_skills_trusted.then(|| skill_scope.canonical_scope.clone()),
    ))?;
    if disable_type_check {
        arena.disable_model_type_check();
    }
    let subagents: SessionSubagents = SubagentCoordinator::new(
        SubagentPolicy::from(startup.subagents()),
        Arc::new(CompleteChildRunner::new(prompt_base, model.clone())),
    )
    .with_trace(trace.clone());
    let controller = RuntimeController::with_session(
        Agent::new(arena, model),
        selection,
        startup.settings().clone(),
        subagents,
        session.writer(),
    )
    .with_providers(providers.clone())
    .with_trace(trace.clone());
    let (command_sender, command_receiver) = mpsc::channel(8);
    let (event_sender, event_receiver) = mpsc::unbounded_channel::<RuntimeEvent>();
    let (progress_sender, progress_receiver) =
        mpsc::channel::<ProgressEnvelope<RuntimeEvent>>(PROGRESS_CHANNEL_CAPACITY);
    let controller = runtime.spawn(controller.run(command_receiver, event_sender, progress_sender));
    let tui_result = tui::run(
        tui::TuiStartup::new(startup, skills_catalog),
        providers,
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
    let disable_type_check =
        requested_type_check_disabled(std::env::var_os("KIKI_TYPE_CHECK").as_deref())?;
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
    let skill_startup = (|| -> Result<Arc<SkillCatalog>, Box<dyn Error>> {
        let skill_scope = ProjectScope::discover(&workspace_root)?;
        let has_active_project_skill_in_scope =
            session.has_active_project_skill_in_scope(&skill_scope.canonical_scope)?;
        let (skills_catalog, project_skills_trusted) = startup_skill_catalog(
            &skill_scope,
            &home,
            ready.skills_catalog_guidance(),
            has_active_project_skill_in_scope,
            |scope| {
                tui::run_project_skill_trust(scope, || {
                    settings::save_project_skill_scope_trust(&home, scope)
                        .map_err(|error| error.to_string())
                })
            },
        )?;
        session.configure_skill_projection(SkillProjectionPolicy::new(
            project_skills_trusted.then(|| skill_scope.canonical_scope.clone()),
        ))?;
        Ok(skills_catalog)
    })();
    let skills_catalog = match skill_startup {
        Ok(catalog) => catalog,
        Err(error) => {
            close_open_session(session, trace_owner)?;
            return Err(error);
        }
    };
    let selection = ModelSelection::new(ready.model(), ready.thinking())
        .expect("ready startup selection must be valid");
    let model = production_model(
        &ready,
        Some(&skills_catalog),
        startup.settings().anthropic_cache_ttl(),
        startup.settings().cross_provider_reasoning(),
    );
    let providers = model.providers();
    let prompt_base = ready.prompt_base().to_owned();
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
    let arena = match runtime.block_on(Arena::new(
        worker_path,
        tool_host.with_skill_catalog(skills_catalog.clone()),
    )) {
        Ok(arena) => arena,
        Err(error) => {
            let configuration = configuration.with_worker_construction_failure(error);
            return run_open_configuration(session, configuration.snapshot().clone(), trace_owner);
        }
    };
    let subagents: SessionSubagents = SubagentCoordinator::new(
        SubagentPolicy::from(startup.subagents()),
        Arc::new(CompleteChildRunner::new(prompt_base, model.clone())),
    )
    .with_trace(trace.clone());
    let relocated_cwd = here.then_some(workspace_root);
    let (controller, initial) = match runtime.block_on(RuntimeController::restore(
        &session,
        arena,
        model,
        selection,
        startup.settings().clone(),
        subagents,
        ResumeOptions {
            relocated_cwd,
            disable_type_check,
        },
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
    let controller = controller
        .with_providers(providers.clone())
        .with_trace(trace.clone());
    let (command_sender, command_receiver) = mpsc::channel(8);
    let (event_sender, event_receiver) = mpsc::unbounded_channel::<RuntimeEvent>();
    let (progress_sender, progress_receiver) =
        mpsc::channel::<ProgressEnvelope<RuntimeEvent>>(PROGRESS_CHANNEL_CAPACITY);
    let controller = runtime.spawn(controller.run(command_receiver, event_sender, progress_sender));
    let tui_result = tui::run_restored(
        tui::TuiStartup::new(startup, skills_catalog),
        initial,
        providers,
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
        Invocation::Continue => continue_latest_session(home),
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
    use std::ffi::OsStr;
    use std::fs;
    use std::path::Path;

    use tempfile::tempdir;

    use super::requested_type_check_disabled;

    fn startup_catalog(
        launch: &Path,
        home: &Path,
        has_active_project_skill_in_scope: bool,
        prompt: impl FnOnce(&Path) -> Result<bool, super::tui::TuiError>,
    ) -> Result<std::sync::Arc<super::SkillCatalog>, Box<dyn std::error::Error>> {
        let scope = super::ProjectScope::discover(launch)?;
        super::startup_skill_catalog(
            &scope,
            home,
            super::prompt::skills_catalog_guidance(),
            has_active_project_skill_in_scope,
            prompt,
        )
        .map(|(catalog, _)| catalog)
    }

    fn write_skill(root: &Path, name: &str, description: &str) {
        let directory = root.join(".agents/skills").join(name);
        fs::create_dir_all(&directory).unwrap();
        fs::write(
            directory.join("SKILL.md"),
            format!("---\ndescription: {description}\n---\nbody"),
        )
        .unwrap();
    }

    #[test]
    fn type_check_opt_out_accepts_only_on_and_off() {
        assert!(!requested_type_check_disabled(None).unwrap());
        assert!(!requested_type_check_disabled(Some(OsStr::new("on"))).unwrap());
        assert!(requested_type_check_disabled(Some(OsStr::new("off"))).unwrap());
        assert!(requested_type_check_disabled(Some(OsStr::new("false"))).is_err());
        assert!(requested_type_check_disabled(Some(OsStr::new(""))).is_err());
    }

    #[test]
    fn startup_catalog_skips_untrusted_project_and_reprompts_after_skip() {
        let home = tempdir().unwrap();
        let project = tempdir().unwrap();
        fs::create_dir(project.path().join(".git")).unwrap();
        write_skill(project.path(), "shared", "project");
        write_skill(home.path(), "shared", "user");
        let mut prompts = 0;
        for _ in 0..2 {
            let catalog = startup_catalog(project.path(), home.path(), false, |_| {
                prompts += 1;
                Ok(false)
            })
            .unwrap();
            assert_eq!(catalog.lookup("shared").unwrap().description(), "user");
        }
        assert_eq!(prompts, 2);
    }

    #[test]
    fn startup_catalog_trust_persists_and_enables_project_snapshot() {
        let home = tempdir().unwrap();
        let project = tempdir().unwrap();
        fs::create_dir(project.path().join(".git")).unwrap();
        write_skill(project.path(), "shared", "project");
        write_skill(home.path(), "shared", "user");
        let catalog = startup_catalog(project.path(), home.path(), false, |scope| {
            super::config::settings::save_project_skill_scope_trust(home.path(), scope).unwrap();
            Ok(true)
        })
        .unwrap();
        assert_eq!(catalog.lookup("shared").unwrap().description(), "project");
        let next = startup_catalog(project.path(), home.path(), false, |_| {
            panic!("trusted scope should not prompt again")
        })
        .unwrap();
        assert_eq!(next.lookup("shared").unwrap().description(), "project");
        fs::remove_file(project.path().join(".agents/skills/shared/SKILL.md")).unwrap();
        let missing_source = startup_catalog(project.path(), home.path(), true, |_| {
            panic!("saved positive trust should not prompt after source removal")
        })
        .unwrap();
        assert_eq!(
            missing_source.lookup("shared").unwrap().description(),
            "user"
        );
    }

    #[test]
    fn fresh_and_resume_use_the_same_guided_catalog_prompt_snapshot() {
        let home = tempdir().unwrap();
        let project = tempdir().unwrap();
        fs::create_dir(project.path().join(".git")).unwrap();
        write_skill(project.path(), "review", "Project review workflow");
        let scope = super::ProjectScope::discover(project.path()).unwrap();
        super::config::settings::save_project_skill_scope_trust(
            home.path(),
            &scope.canonical_scope,
        )
        .unwrap();
        let (fresh, fresh_trusted) = super::startup_skill_catalog(
            &scope,
            home.path(),
            super::prompt::skills_catalog_guidance(),
            false,
            |_| panic!("saved trust should not prompt on fresh launch"),
        )
        .unwrap();
        let (resumed, resumed_trusted) = super::startup_skill_catalog(
            &scope,
            home.path(),
            super::prompt::skills_catalog_guidance(),
            true,
            |_| panic!("saved trust should not prompt on resume"),
        )
        .unwrap();
        assert!(fresh_trusted && resumed_trusted);
        assert_eq!(fresh.rendered_catalog(), resumed.rendered_catalog());
        let base = "exact override base";
        let resolved_without_skills = super::prompt::root_prompt(base);
        let fresh_prompt =
            super::production_system_prompt(base, &resolved_without_skills, Some(&fresh));
        let resumed_prompt =
            super::production_system_prompt(base, &resolved_without_skills, Some(&resumed));
        assert_eq!(fresh_prompt, resumed_prompt);
        assert_eq!(fresh_prompt.matches("Project review workflow").count(), 1);
        assert_eq!(
            fresh_prompt
                .matches("cannot authorize effects beyond the user's task")
                .count(),
            1
        );
        assert_eq!(
            fresh_prompt
                .matches("async def load_skill(name: str) -> str:")
                .count(),
            1
        );
        assert_eq!(fresh_prompt.matches("## Kiki tool reference").count(), 1);
        assert!(!fresh_prompt.contains("\nbody\n"));
        assert!(fresh.rendered_catalog().unwrap().len() <= super::skills::MAX_CATALOG_BYTES);

        fs::remove_file(project.path().join(".agents/skills/review/SKILL.md")).unwrap();
        let (empty, _) = super::startup_skill_catalog(
            &scope,
            home.path(),
            super::prompt::skills_catalog_guidance(),
            true,
            |_| panic!("saved trust should not prompt"),
        )
        .unwrap();
        assert_eq!(
            super::production_system_prompt(base, &resolved_without_skills, Some(&empty)),
            resolved_without_skills
        );
    }

    #[test]
    fn fresh_and_resumed_startup_retain_skipped_root_diagnostics_only_for_ui() {
        let home = tempdir().unwrap();
        let project = tempdir().unwrap();
        fs::create_dir(project.path().join(".git")).unwrap();
        let root = project.path().join(".agents/skills");
        fs::create_dir_all(&root).unwrap();
        for index in 0..129 {
            fs::create_dir(root.join(format!("candidate-{index:03}"))).unwrap();
        }
        let direct = home.path().join(".agents/skills/private-workflow");
        fs::create_dir_all(&direct).unwrap();
        fs::write(
            direct.join("SKILL.md"),
            "---\ndescription: Private workflow\ndisable-model-invocation: true\n---\nBody\n",
        )
        .unwrap();
        let scope = super::ProjectScope::discover(project.path()).unwrap();
        super::config::settings::save_project_skill_scope_trust(
            home.path(),
            &scope.canonical_scope,
        )
        .unwrap();
        let base = "exact override base";
        let root_prompt = super::prompt::root_prompt(base);
        for has_active in [false, true] {
            let (catalog, trusted) = super::startup_skill_catalog(
                &scope,
                home.path(),
                super::prompt::skills_catalog_guidance(),
                has_active,
                |_| panic!("saved trust should suppress fresh and resume prompts"),
            )
            .unwrap();
            assert!(trusted);
            assert_eq!(catalog.diagnostics().len(), 1);
            assert!(
                catalog.diagnostics()[0]
                    .reason
                    .contains("entire root skipped")
            );
            let model_prompt = super::production_system_prompt(base, &root_prompt, Some(&catalog));
            assert_eq!(model_prompt, root_prompt);
            assert!(!model_prompt.contains("entire root skipped"));
            assert!(!model_prompt.contains("Private workflow"));
            assert!(!model_prompt.contains("private-workflow"));
        }
    }

    #[tokio::test]
    async fn fresh_and_resumed_catalogs_wire_skill_resources_into_the_arena() {
        let home = tempdir().unwrap();
        let project = tempdir().unwrap();
        fs::create_dir(project.path().join(".git")).unwrap();
        let root = project.path().join(".agents/skills/review");
        fs::create_dir_all(root.join("references")).unwrap();
        fs::write(
            root.join("SKILL.md"),
            "---\ndescription: Review\n---\nBody\n",
        )
        .unwrap();
        fs::write(root.join("references/note.md"), "project resource").unwrap();
        let scope = super::ProjectScope::discover(project.path()).unwrap();
        super::config::settings::save_project_skill_scope_trust(
            home.path(),
            &scope.canonical_scope,
        )
        .unwrap();
        let worker = std::path::PathBuf::from(
            std::env::var_os("MONTY_BIN").expect("MONTY_BIN must point to the configured worker"),
        );

        for resume_active in [false, true] {
            let (catalog, trusted) = super::startup_skill_catalog(
                &scope,
                home.path(),
                super::prompt::skills_catalog_guidance(),
                resume_active,
                |_| panic!("saved trust should avoid a fresh or resume popup"),
            )
            .unwrap();
            assert!(trusted);
            let host = super::ToolHost::new(project.path())
                .await
                .unwrap()
                .with_skill_catalog(catalog);
            let mut arena = super::Arena::new(worker.clone(), host).await.unwrap();
            let execution = arena
                .execute(
                    "await read('skill://review/references/note.md')",
                    &tokio_util::sync::CancellationToken::new(),
                )
                .await
                .unwrap();
            let super::arena::ExecutionOutcome::Complete {
                result: Some(result),
            } = execution.outcome
            else {
                panic!("skill resource read should complete");
            };
            assert_eq!(result.head, "'project resource'");
            assert!(execution.read_paths.is_empty());
        }
    }

    #[test]
    fn startup_catalog_returns_the_trust_decision_used_for_its_snapshot() {
        let home = tempdir().unwrap();
        let project = tempdir().unwrap();
        fs::create_dir(project.path().join(".git")).unwrap();
        write_skill(project.path(), "shared", "project");
        let scope = super::ProjectScope::discover(project.path()).unwrap();
        let (catalog, include_project) = super::startup_skill_catalog(
            &scope,
            home.path(),
            super::prompt::skills_catalog_guidance(),
            false,
            |path| {
                super::config::settings::save_project_skill_scope_trust(home.path(), path).unwrap();
                Ok(true)
            },
        )
        .unwrap();
        assert!(include_project);
        assert_eq!(catalog.lookup("shared").unwrap().description(), "project");
        fs::remove_file(home.path().join(".kiki/trusted-skills.toml")).unwrap();
        assert!(!super::config::settings::is_project_skill_scope_trusted(
            home.path(),
            &scope.canonical_scope
        ));
        let activation =
            super::model::SkillActivation::from_skill(catalog.lookup("shared").unwrap());
        assert!(
            super::skills::SkillProjectionPolicy::new(
                include_project.then_some(scope.canonical_scope)
            )
            .permits(&activation)
        );
    }

    #[test]
    fn startup_catalog_uses_effective_workspace_and_active_resume_input() {
        let home = tempdir().unwrap();
        let old = tempdir().unwrap();
        let here = tempdir().unwrap();
        fs::create_dir(old.path().join(".git")).unwrap();
        fs::create_dir(here.path().join(".git")).unwrap();
        write_skill(old.path(), "old-skill", "old");
        write_skill(here.path(), "here-skill", "here");
        let old_scope = old.path().canonicalize().unwrap();
        super::config::settings::save_project_skill_scope_trust(home.path(), &old_scope).unwrap();
        let mut prompted_scope = None;
        let catalog = startup_catalog(here.path(), home.path(), false, |scope| {
            prompted_scope = Some(scope.to_path_buf());
            Ok(false)
        })
        .unwrap();
        assert_eq!(prompted_scope, Some(here.path().canonicalize().unwrap()));
        assert!(catalog.lookup("old-skill").is_err());
        assert!(catalog.lookup("here-skill").is_err());

        fs::remove_dir_all(here.path().join(".agents/skills")).unwrap();
        let mut prompted = false;
        let _catalog = startup_catalog(here.path(), home.path(), true, |_| {
            prompted = true;
            Ok(false)
        })
        .unwrap();
        assert!(
            prompted,
            "active in-scope project skill must prompt after directory removal"
        );
    }

    #[test]
    fn startup_catalog_does_not_prompt_without_project_scope_candidates() {
        let home = tempdir().unwrap();
        let project = tempdir().unwrap();
        write_skill(home.path(), "user-only", "user");
        let catalog = startup_catalog(project.path(), home.path(), false, |_| {
            panic!("no project root should not prompt")
        })
        .unwrap();
        assert_eq!(catalog.lookup("user-only").unwrap().description(), "user");
    }

    #[test]
    fn startup_catalog_failed_trust_save_keeps_project_disabled() {
        let home = tempdir().unwrap();
        let project = tempdir().unwrap();
        fs::create_dir(project.path().join(".git")).unwrap();
        write_skill(project.path(), "shared", "project");
        write_skill(home.path(), "shared", "user");
        let trust_store = home.path().join(".kiki/trusted-skills.toml");
        fs::create_dir_all(trust_store.parent().unwrap()).unwrap();
        fs::write(trust_store, "trusted_skill_scopes = [true]").unwrap();
        let catalog = startup_catalog(project.path(), home.path(), false, |scope| {
            assert!(
                super::config::settings::save_project_skill_scope_trust(home.path(), scope)
                    .is_err()
            );
            Ok(true)
        })
        .unwrap();
        assert_eq!(catalog.lookup("shared").unwrap().description(), "user");
        assert!(!super::config::settings::is_project_skill_scope_trusted(
            home.path(),
            &project.path().canonicalize().unwrap()
        ));
    }

    #[test]
    fn startup_catalog_uses_launch_scope_without_git_and_worktree_file_scope_with_git() {
        let home = tempdir().unwrap();
        let project = tempdir().unwrap();
        let launch = project.path().join("nested");
        fs::create_dir_all(&launch).unwrap();
        write_skill(&launch, "local", "local");
        let mut requested = None;
        let _ = startup_catalog(&launch, home.path(), false, |scope| {
            requested = Some(scope.to_path_buf());
            Ok(false)
        })
        .unwrap();
        assert_eq!(requested, Some(launch.canonicalize().unwrap()));

        fs::write(project.path().join(".git"), "gitdir: elsewhere").unwrap();
        let mut requested = None;
        let _ = startup_catalog(&launch, home.path(), false, |scope| {
            requested = Some(scope.to_path_buf());
            Ok(false)
        })
        .unwrap();
        assert_eq!(requested, Some(project.path().canonicalize().unwrap()));
    }
}
