#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct ClipboardError;

pub(crate) trait ClipboardPort {
    fn write_text(&mut self, text: &str) -> Result<(), ClipboardError>;
}

pub(crate) struct LocalClipboard {
    clipboard: Option<arboard::Clipboard>,
}

impl LocalClipboard {
    pub(crate) fn new() -> Self {
        Self {
            clipboard: arboard::Clipboard::new().ok(),
        }
    }

    #[cfg(test)]
    fn unavailable() -> Self {
        Self { clipboard: None }
    }
}

impl ClipboardPort for LocalClipboard {
    fn write_text(&mut self, text: &str) -> Result<(), ClipboardError> {
        self.clipboard
            .as_mut()
            .ok_or(ClipboardError)?
            .set_text(text)
            .map_err(|_| ClipboardError)
    }
}

#[cfg(test)]
mod tests {
    use super::{ClipboardError, ClipboardPort, LocalClipboard};

    struct FakeClipboard {
        result: Result<(), ClipboardError>,
        writes: Vec<String>,
    }

    impl ClipboardPort for FakeClipboard {
        fn write_text(&mut self, text: &str) -> Result<(), ClipboardError> {
            self.writes.push(text.to_owned());
            self.result
        }
    }

    #[test]
    fn unavailable_native_clipboard_fails_without_initialising_a_desktop_clipboard() {
        let mut clipboard = LocalClipboard::unavailable();

        assert_eq!(clipboard.write_text("text"), Err(ClipboardError));
    }

    #[test]
    fn available_port_writes_text() {
        let mut clipboard = FakeClipboard {
            result: Ok(()),
            writes: Vec::new(),
        };

        assert_eq!(clipboard.write_text("text"), Ok(()));
        assert_eq!(clipboard.writes, ["text"]);
    }
}
