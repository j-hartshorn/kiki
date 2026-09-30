//! `kiki subprocess`: the embedded Monty worker.
//!
//! monty-pool starts every worker as `<binary> subprocess` and then speaks the
//! framed monty-proto protocol over stdio. By answering that subcommand itself,
//! the `kiki` executable doubles as its own worker, so an installation is a
//! single binary and `MONTY_BIN` is only an override.
//!
//! The loop is adapted from `monty-runtime` 1.0.0 `src/subprocess.rs`
//! (MIT licence, Samuel Colvin / Pydantic). Keep it protocol-identical to
//! `monty subprocess`, and keep `monty-proto`/`monty-alloc` pinned to the same
//! version as `monty-pool`.
//!
//! In this mode stdout carries only protocol frames; diagnostics go to stderr.
//! The parent treats a worker that exits without a `FatalError` event as
//! crashed, which is how stack overflows and allocator aborts are reported.

use std::ffi::OsStr;
use std::{io, panic, process::ExitCode, thread};

use monty_proto::{
    FrameError, FrameReader, pb,
    worker::{Child, EventSink, HandleOutcome, fatal_error_event, protocol_violation},
    write_frame,
};
use monty_types::memory_limit_with_headroom;

/// The argument monty-pool passes when it spawns a worker.
const SUBCOMMAND: &str = "subprocess";

/// BSD `sysexits.h` "remote error in protocol".
const EX_PROTOCOL: u8 = 76;

/// Whether this process was started as a Monty worker (`kiki subprocess`).
pub(crate) fn requested() -> bool {
    let mut arguments = std::env::args_os().skip(1);
    arguments.next().as_deref() == Some(OsStr::new(SUBCOMMAND)) && arguments.next().is_none()
}

/// The interpreter's container `repr`, `==` and value exporter recurse once
/// per nesting level up to the 1000-frame recursion limit; give every OS the
/// same reserved stack rather than relying on the main thread's.
const WORKER_STACK_SIZE: usize = 16 * 1024 * 1024;

/// Runs the worker loop until EOF, `Shutdown`, or a fatal error.
pub(crate) fn run() -> ExitCode {
    thread::Builder::new()
        .name("monty-worker".to_owned())
        .stack_size(WORKER_STACK_SIZE)
        .spawn(serve)
        .expect("failed to spawn the worker thread")
        .join()
        // A panic has already written its `FatalError` frame in the panic hook.
        .unwrap_or(ExitCode::from(101))
}

fn serve() -> ExitCode {
    install_panic_hook();
    let mut reader = FrameReader::new(io::stdin().lock());
    let mut child = Child::default();
    let mut sink = StdoutSink;

    loop {
        match reader.read::<pb::ParentRequest>() {
            Ok(Some(request)) => {
                let outcome = child.handle(request, &mut sink);
                apply_memory_limit(&child);
                match outcome {
                    Ok(HandleOutcome::Continue) => {}
                    Ok(HandleOutcome::Shutdown) => return ExitCode::SUCCESS,
                    // The child emitted a FatalError (e.g. version skew) and cannot keep serving.
                    Ok(HandleOutcome::Fatal) => return ExitCode::from(4),
                    // An oversize event was rejected before any bytes hit the wire, so the
                    // stream is still in sync and the parent can receive a last gasp.
                    Err(FrameError::FrameTooLarge { len, max }) => {
                        fatal(
                            &child,
                            &mut sink,
                            &format!(
                                "response frame of {len} bytes exceeds maximum of {max} bytes"
                            ),
                        );
                        return ExitCode::from(EX_PROTOCOL);
                    }
                    // Writing to stdout failed: the parent is gone.
                    Err(_) => return ExitCode::from(3),
                }
            }
            // Clean EOF at a frame boundary: the parent closed stdin.
            Ok(None) => return ExitCode::SUCCESS,
            // The frame arrived intact but its payload did not decode. The stream is
            // still in sync, so answer with a turn-ending error and keep serving.
            Err(FrameError::Decode(error)) => {
                if sink
                    .send(&protocol_violation(&format!("malformed request: {error}")))
                    .is_err()
                {
                    return ExitCode::from(3);
                }
            }
            Err(error) => {
                // The stream is desynchronised, which is unrecoverable by design.
                fatal(
                    &child,
                    &mut sink,
                    &format!("malformed request frame: {error}"),
                );
                return ExitCode::from(EX_PROTOCOL);
            }
        }
    }
}

/// Applies the memory limit of the session the child now holds, after every
/// request. Reading the child's state (not the request) keeps rejected
/// `Configure`s, `Load`s and `Reset`s honest.
fn apply_memory_limit(child: &Child) {
    let budget = child.session_budget();
    let hard_memory_limit = memory_limit_with_headroom(budget.max_memory, budget.type_check);
    monty_alloc::set_hard_limit(hard_memory_limit)
        .expect("kiki must install monty_alloc::LimitedAllocator as its global allocator");
}

/// Writes framed child events to stdout.
struct StdoutSink;

impl EventSink for StdoutSink {
    fn send(&mut self, event: &pb::ChildEvent) -> Result<(), FrameError> {
        write_frame(&mut io::stdout(), event)
    }
}

/// Emits a best-effort `FatalError` event, duplicated to stderr.
fn fatal(child: &Child, sink: &mut impl EventSink, message: &str) {
    eprintln!("kiki subprocess fatal error: {message}");
    let _ = sink.send(&child.fatal_event(message));
}

/// Emits a best-effort `FatalError` frame before the default panic handling.
fn install_panic_hook() {
    let default_hook = panic::take_hook();
    panic::set_hook(Box::new(move |info| {
        let _ = write_frame(
            &mut io::stdout(),
            &fatal_error_event(&format!("child panicked: {info}")),
        );
        default_hook(info);
    }));
}
