//! `kiki login <provider>` and `kiki logout <provider>`: manage the API keys
//! stored in `~/.kiki/auth.toml` (see `config::auth`).

use std::error::Error;
use std::io::{self, BufRead, IsTerminal, Write};
use std::path::Path;

use crate::config::{auth, models::Provider};

pub(crate) fn login(home: &Path, provider: Provider) -> Result<(), Box<dyn Error>> {
    let api_key = if io::stdin().is_terminal() {
        eprint!(
            "Paste your {} API key (input hidden): ",
            provider.display_name()
        );
        io::stderr().flush()?;
        read_hidden_line()?
    } else {
        read_line()?
    };
    let api_key = api_key.trim();
    if api_key.is_empty() {
        return Err("No API key entered; nothing was saved.".into());
    }
    auth::store(home, provider, api_key)?;
    println!(
        "Saved your {} API key to {}.",
        provider.display_name(),
        auth::AUTH_FILE_LABEL
    );
    warn_if_environment_overrides(provider);
    Ok(())
}

pub(crate) fn logout(home: &Path, provider: Provider) -> Result<(), Box<dyn Error>> {
    if auth::remove(home, provider)? {
        println!(
            "Removed your {} API key from {}.",
            provider.display_name(),
            auth::AUTH_FILE_LABEL
        );
    } else {
        println!(
            "No {} API key is stored in {}.",
            provider.display_name(),
            auth::AUTH_FILE_LABEL
        );
    }
    warn_if_environment_overrides(provider);
    Ok(())
}

fn warn_if_environment_overrides(provider: Provider) {
    let variable = provider.api_key_variable();
    if std::env::var_os(variable).is_some_and(|value| !value.is_empty()) {
        eprintln!(
            "Note: {variable} is set in this environment and takes precedence over {}.",
            auth::AUTH_FILE_LABEL
        );
    }
}

fn read_line() -> io::Result<String> {
    let mut line = String::new();
    io::stdin().lock().read_line(&mut line)?;
    Ok(line)
}

/// Reads one line from the terminal with echo off.
#[cfg(unix)]
fn read_hidden_line() -> io::Result<String> {
    let descriptor = libc::STDIN_FILENO;
    let mut original = std::mem::MaybeUninit::<libc::termios>::uninit();
    // SAFETY: `tcgetattr` initialises `original` when it returns 0.
    if unsafe { libc::tcgetattr(descriptor, original.as_mut_ptr()) } != 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: checked above.
    let original = unsafe { original.assume_init() };
    let mut hidden = original;
    hidden.c_lflag &= !libc::ECHO;
    hidden.c_lflag |= libc::ECHONL;
    // SAFETY: `hidden` is a valid termios derived from the current settings.
    if unsafe { libc::tcsetattr(descriptor, libc::TCSANOW, &hidden) } != 0 {
        return Err(io::Error::last_os_error());
    }
    let line = read_line();
    // SAFETY: restores the settings read above.
    unsafe { libc::tcsetattr(descriptor, libc::TCSANOW, &original) };
    line
}

#[cfg(not(unix))]
fn read_hidden_line() -> io::Result<String> {
    read_line()
}
