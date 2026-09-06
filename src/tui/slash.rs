use std::ops::Range;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Action {
    Model,
    Name,
    Thinking,
}

#[derive(Clone, Copy)]
struct Command {
    token: &'static str,
    purpose: &'static str,
    action: Action,
}

const CATALOGUE: [Command; 3] = [
    Command {
        token: "/model",
        purpose: "Choose model",
        action: Action::Model,
    },
    Command {
        token: "/name",
        purpose: "Name session",
        action: Action::Name,
    },
    Command {
        token: "/thinking",
        purpose: "Choose thinking level",
        action: Action::Thinking,
    },
];

pub(super) struct Parsed<'a> {
    pub(super) action: Action,
    pub(super) argument: &'a str,
    token_range: Range<usize>,
}

pub(super) fn parse(draft: &str) -> Option<Parsed<'_>> {
    if draft.contains('\n') {
        return None;
    }
    CATALOGUE.iter().find_map(|command| {
        draft
            .strip_prefix(command.token)
            .and_then(slash_argument)
            .map(|argument| Parsed {
                action: command.action,
                argument,
                token_range: 0..command.token.len(),
            })
    })
}

pub(super) fn recognised_token_range(draft: &str) -> Option<Range<usize>> {
    parse(draft).map(|command| command.token_range)
}

#[derive(Default)]
pub(super) struct Completion {
    matches: Vec<&'static Command>,
    selected: usize,
}

pub(super) struct CompletionRow {
    pub(super) token: &'static str,
    pub(super) purpose: &'static str,
    pub(super) selected: bool,
}

impl Completion {
    pub(super) fn sync(&mut self, draft: &str, caret_at_end: bool) -> bool {
        let selected = self.selected_token();
        let matches = prefix_matches(draft, caret_at_end);
        let next_selected = selected
            .and_then(|token| matches.iter().position(|command| command.token == token))
            .unwrap_or(0);
        let changed = self.selected != next_selected
            || self.matches.len() != matches.len()
            || self
                .matches
                .iter()
                .zip(&matches)
                .any(|(current, next)| current.token != next.token);
        self.matches = matches;
        self.selected = next_selected;
        changed
    }

    pub(super) fn move_selection(&mut self, offset: isize) -> bool {
        let Some(last) = self.matches.len().checked_sub(1) else {
            return false;
        };
        let selected = self.selected.saturating_add_signed(offset).min(last);
        if self.selected == selected {
            false
        } else {
            self.selected = selected;
            true
        }
    }

    pub(super) fn selected_token(&self) -> Option<&'static str> {
        self.matches.get(self.selected).map(|command| command.token)
    }

    pub(super) fn visible_rows(&self, height: usize) -> Vec<CompletionRow> {
        if height == 0 || self.matches.is_empty() {
            return Vec::new();
        }
        let start = self.selected.saturating_add(1).saturating_sub(height);
        self.matches[start..]
            .iter()
            .enumerate()
            .take(height)
            .map(|(index, command)| CompletionRow {
                token: command.token,
                purpose: command.purpose,
                selected: start + index == self.selected,
            })
            .collect()
    }
}

fn slash_argument(rest: &str) -> Option<&str> {
    if rest.is_empty() {
        Some("")
    } else {
        rest.chars()
            .next()
            .is_some_and(char::is_whitespace)
            .then(|| rest.trim())
    }
}

fn prefix_matches(draft: &str, caret_at_end: bool) -> Vec<&'static Command> {
    if !caret_at_end || draft.contains('\n') || !draft.starts_with('/') {
        return Vec::new();
    }
    CATALOGUE
        .iter()
        .filter(|command| draft.len() < command.token.len() && command.token.starts_with(draft))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::{Action, CATALOGUE, Completion, parse, recognised_token_range};

    #[test]
    fn catalogue_orders_tokens_purposes_and_actions() {
        let commands = CATALOGUE
            .iter()
            .map(|command| (command.token, command.purpose, command.action))
            .collect::<Vec<_>>();

        assert_eq!(
            commands,
            [
                ("/model", "Choose model", Action::Model),
                ("/name", "Name session", Action::Name),
                ("/thinking", "Choose thinking level", Action::Thinking),
            ]
        );
    }

    #[test]
    fn exact_parsing_keeps_existing_arguments_and_token_ranges() {
        for (draft, action, argument, token_range) in [
            ("/model", Action::Model, "", 0..6),
            ("/model  terra\t", Action::Model, "terra", 0..6),
            ("/name  A session  ", Action::Name, "A session", 0..5),
            ("/thinking\t high", Action::Thinking, "high", 0..9),
        ] {
            let parsed = parse(draft).expect("recognised command");
            assert_eq!(parsed.action, action);
            assert_eq!(parsed.argument, argument);
            assert_eq!(recognised_token_range(draft), Some(token_range));
        }

        for draft in [
            "",
            " /model",
            "/Model",
            "/models",
            "/namex",
            "/thinking-more",
            "/model\nterra",
        ] {
            assert!(parse(draft).is_none(), "{draft}");
            assert_eq!(recognised_token_range(draft), None, "{draft}");
        }
    }

    #[test]
    fn completion_requires_a_strict_prefix_at_the_end_of_a_single_line() {
        let mut completion = Completion::default();
        for (draft, caret_at_end, expected) in [
            ("/", true, vec!["/model", "/name", "/thinking"]),
            ("/mo", true, vec!["/model"]),
            ("/model", true, vec![]),
            ("/model ", true, vec![]),
            ("/M", true, vec![]),
            ("/future", true, vec![]),
            ("/mo", false, vec![]),
            ("/mo\n", true, vec![]),
        ] {
            completion.sync(draft, caret_at_end);
            assert_eq!(
                completion
                    .visible_rows(usize::MAX)
                    .iter()
                    .map(|row| row.token)
                    .collect::<Vec<_>>(),
                expected,
                "{draft:?}, caret at end: {caret_at_end}"
            );
        }
    }

    #[test]
    fn completion_selects_the_first_match_preserves_it_or_resets_and_clamps() {
        let mut completion = Completion::default();
        completion.sync("/", true);
        assert_eq!(completion.selected_token(), Some("/model"));
        assert!(completion.move_selection(1));
        assert_eq!(completion.selected_token(), Some("/name"));

        completion.sync("/n", true);
        assert_eq!(completion.selected_token(), Some("/name"));
        completion.sync("/m", true);
        assert_eq!(completion.selected_token(), Some("/model"));
        assert!(!completion.move_selection(-1));

        completion.sync("/", true);
        assert!(completion.move_selection(99));
        assert_eq!(completion.selected_token(), Some("/thinking"));
        assert!(!completion.move_selection(1));
        completion.sync("/t", true);
        assert_eq!(completion.selected_token(), Some("/thinking"));

        let rows = completion.visible_rows(1);
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].token, "/thinking");
        assert!(rows[0].selected);
    }
}
