//! `kiki login <provider>` and `kiki logout <provider>`: manage the API keys
//! stored in `~/.kiki/auth.toml` (see `config::auth`).

use std::error::Error;
use std::io::{self, BufRead, IsTerminal, Write};
use std::path::Path;

use crate::config::{auth, models::Provider};

#[cfg(unix)]
const PROMPT_NOTE: &str = "input hidden; Ctrl-C cancels";
#[cfg(not(unix))]
const PROMPT_NOTE: &str = "input is shown";

pub(crate) fn login(home: &Path, provider: Provider) -> Result<(), Box<dyn Error>> {
    let api_key = if io::stdin().is_terminal() {
        eprint!(
            "Paste your {} API key ({PROMPT_NOTE}): ",
            provider.display_name()
        );
        io::stderr().flush()?;
        match read_hidden_line()? {
            Some(api_key) => api_key,
            None => return Err("Cancelled; nothing was saved.".into()),
        }
    } else {
        read_line()?
    };
    let api_key = api_key.trim();
    if api_key.is_empty() {
        return Err("No API key entered; nothing was saved.".into());
    }
    auth::store(home, provider, api_key).map_err(with_remediation)?;
    println!(
        "Saved your {} API key to {}.",
        provider.display_name(),
        auth::AUTH_FILE_LABEL
    );
    warn_if_environment_overrides(provider);
    Ok(())
}

pub(crate) fn logout(home: &Path, provider: Provider) -> Result<(), Box<dyn Error>> {
    if auth::remove(home, provider).map_err(with_remediation)? {
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

fn with_remediation(error: auth::AuthError) -> Box<dyn Error> {
    format!("{error}. {}", error.remediation()).into()
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

/// Reads one line from the terminal without echo. Returns `None` if the user
/// cancels with Ctrl-C (or Ctrl-D on an empty line).
///
/// The terminal is put in non-canonical mode with echo and signal keys off, and
/// a guard restores it on every exit path. Ctrl-C therefore arrives as a byte
/// rather than a SIGINT that would kill Kiki with echo still disabled.
#[cfg(unix)]
fn read_hidden_line() -> io::Result<Option<String>> {
    use std::io::Read;

    let terminal = HiddenTerminal::enter()?;
    let mut stdin = io::stdin().lock();
    let mut input = Vec::new();
    let mut byte = [0_u8; 1];
    let outcome = loop {
        if stdin.read(&mut byte)? == 0 {
            break Some(input);
        }
        match byte[0] {
            b'\r' | b'\n' => break Some(input),
            0x03 => break None,
            0x04 if input.is_empty() => break None,
            0x7f | 0x08 => pop_char(&mut input),
            0x15 => input.clear(),
            control if control < 0x20 => {}
            other => input.push(other),
        }
    };
    drop(terminal);
    eprintln!();
    outcome
        .map(|input| {
            String::from_utf8(input).map_err(|_| {
                io::Error::new(io::ErrorKind::InvalidData, "API key is not valid UTF-8")
            })
        })
        .transpose()
}

#[cfg(not(unix))]
fn read_hidden_line() -> io::Result<Option<String>> {
    read_line().map(Some)
}

/// Removes the last UTF-8 character from `input`.
#[cfg(unix)]
fn pop_char(input: &mut Vec<u8>) {
    while let Some(byte) = input.pop() {
        if byte & 0b1100_0000 != 0b1000_0000 {
            break;
        }
    }
}

/// Terminal settings for hidden input; the original settings are restored on drop.
#[cfg(unix)]
struct HiddenTerminal {
    original: libc::termios,
}

#[cfg(unix)]
impl HiddenTerminal {
    fn enter() -> io::Result<Self> {
        let mut original = std::mem::MaybeUninit::<libc::termios>::uninit();
        // SAFETY: `tcgetattr` initialises `original` when it returns 0.
        if unsafe { libc::tcgetattr(libc::STDIN_FILENO, original.as_mut_ptr()) } != 0 {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: checked above.
        let original = unsafe { original.assume_init() };
        let mut hidden = original;
        hidden.c_lflag &= !(libc::ECHO | libc::ICANON | libc::ISIG | libc::IEXTEN);
        hidden.c_cc[libc::VMIN] = 1;
        hidden.c_cc[libc::VTIME] = 0;
        // SAFETY: `hidden` is a valid termios derived from the current settings.
        if unsafe { libc::tcsetattr(libc::STDIN_FILENO, libc::TCSANOW, &hidden) } != 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(Self { original })
    }
}

#[cfg(unix)]
impl Drop for HiddenTerminal {
    fn drop(&mut self) {
        // SAFETY: restores the settings read in `enter`.
        unsafe { libc::tcsetattr(libc::STDIN_FILENO, libc::TCSANOW, &self.original) };
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::pop_char;

    #[test]
    fn backspace_removes_whole_utf8_characters() {
        let mut input = "k\u{e9}\u{20ac}".as_bytes().to_vec();
        pop_char(&mut input);
        assert_eq!(input, "k\u{e9}".as_bytes());
        pop_char(&mut input);
        assert_eq!(input, b"k");
        pop_char(&mut input);
        pop_char(&mut input);
        assert!(input.is_empty());
    }
}
