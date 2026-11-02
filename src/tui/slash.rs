use std::ops::Range;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Action {
    Compact,
    Model,
    Name,
    Settings,
    Skill,
    Theme,
    Todos,
    Tree,
    Thinking,
}

#[derive(Clone, Copy, PartialEq, Eq)]
struct Command {
    token: &'static str,
    purpose: &'static str,
    action: Action,
    takes_argument: bool,
}

const CATALOGUE: [Command; 8] = [
    Command {
        token: "/compact",
        purpose: "Compact context",
        action: Action::Compact,
        takes_argument: true,
    },
    Command {
        token: "/model",
        purpose: "Choose model",
        action: Action::Model,
        takes_argument: true,
    },
    Command {
        token: "/name",
        purpose: "Name session",
        action: Action::Name,
        takes_argument: true,
    },
    Command {
        token: "/settings",
        purpose: "Open settings",
        action: Action::Settings,
        takes_argument: false,
    },
    Command {
        token: "/theme",
        purpose: "Choose theme",
        action: Action::Theme,
        takes_argument: false,
    },
    Command {
        token: "/thinking",
        purpose: "Choose thinking level",
        action: Action::Thinking,
        takes_argument: true,
    },
    Command {
        token: "/todos",
        purpose: "Inspect TODO lists",
        action: Action::Todos,
        takes_argument: false,
    },
    Command {
        token: "/tree",
        purpose: "Navigate session history",
        action: Action::Tree,
        takes_argument: true,
    },
];

pub(super) struct Parsed<'a> {
    pub(super) action: Action,
    pub(super) argument: &'a str,
    pub(super) skill_name: Option<&'a str>,
    token_range: Range<usize>,
}

pub(super) fn parse(draft: &str) -> Option<Parsed<'_>> {
    if let Some(rest) = draft.strip_prefix("/skill:") {
        let name_end = rest.find(char::is_whitespace).unwrap_or(rest.len());
        return Some(Parsed {
            action: Action::Skill,
            argument: "",
            skill_name: Some(&rest[..name_end]),
            token_range: 0.."/skill:".len() + name_end,
        });
    }
    if draft.contains('\n') {
        return None;
    }
    CATALOGUE.iter().find_map(|command| {
        draft
            .strip_prefix(command.token)
            .and_then(slash_argument)
            .filter(|argument| command.takes_argument || argument.is_empty())
            .map(|argument| Parsed {
                action: command.action,
                argument,
                skill_name: None,
                token_range: 0..command.token.len(),
            })
    })
}

pub(super) fn recognised_token_range(draft: &str) -> Option<Range<usize>> {
    parse(draft).map(|command| command.token_range)
}

pub(super) struct Completion {
    commands: Vec<CompletionCommand>,
    matches: Vec<usize>,
    selected: usize,
}

struct CompletionCommand {
    token: String,
    purpose: String,
}

pub(super) struct CompletionRow<'a> {
    pub(super) token: &'a str,
    pub(super) purpose: &'a str,
    pub(super) selected: bool,
}

impl Default for Completion {
    fn default() -> Self {
        Self::with_skills(std::iter::empty::<(&str, &str)>())
    }
}

impl Completion {
    pub(super) fn with_skills<'a>(skills: impl IntoIterator<Item = (&'a str, &'a str)>) -> Self {
        let mut commands = CATALOGUE
            .iter()
            .map(|command| CompletionCommand {
                token: command.token.to_owned(),
                purpose: command.purpose.to_owned(),
            })
            .collect::<Vec<_>>();
        let mut skills = skills
            .into_iter()
            .map(|(name, description)| CompletionCommand {
                token: format!("/skill:{name}"),
                purpose: description.to_owned(),
            })
            .collect::<Vec<_>>();
        skills.sort_by(|left, right| left.token.cmp(&right.token));
        commands.extend(skills);
        Self {
            commands,
            matches: Vec::new(),
            selected: 0,
        }
    }

    pub(super) fn sync(&mut self, draft: &str, caret_at_end: bool) -> bool {
        let selected = self.selected_token().map(str::to_owned);
        let matches = prefix_matches(&self.commands, draft, caret_at_end);
        let next_selected = selected
            .and_then(|token| {
                matches
                    .iter()
                    .position(|index| self.commands[*index].token == token)
            })
            .unwrap_or(0);
        let changed = self.selected != next_selected || self.matches != matches;
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

    pub(super) fn selected_token(&self) -> Option<&str> {
        self.matches
            .get(self.selected)
            .map(|index| self.commands[*index].token.as_str())
    }

    pub(super) fn visible_rows(&self, height: usize) -> Vec<CompletionRow<'_>> {
        if height == 0 || self.matches.is_empty() {
            return Vec::new();
        }
        let start = self.selected.saturating_add(1).saturating_sub(height);
        self.matches[start..]
            .iter()
            .enumerate()
            .take(height)
            .map(|(index, command)| CompletionRow {
                token: self.commands[*command].token.as_str(),
                purpose: self.commands[*command].purpose.as_str(),
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

fn prefix_matches(commands: &[CompletionCommand], draft: &str, caret_at_end: bool) -> Vec<usize> {
    let Some(name_prefix) = draft.strip_prefix('/') else {
        return Vec::new();
    };
    if !caret_at_end
        || draft.chars().any(char::is_whitespace)
        || CATALOGUE.iter().any(|command| command.token == draft)
    {
        return Vec::new();
    }
    commands
        .iter()
        .enumerate()
        .filter(|(_, command)| {
            (draft.len() < command.token.len() && command.token.starts_with(draft))
                || command
                    .token
                    .strip_prefix("/skill:")
                    .is_some_and(|name| name.starts_with(name_prefix))
        })
        .map(|(index, _)| index)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::{Action, Completion, parse, recognised_token_range};

    #[test]
    fn compact_preserves_optional_focus_text() {
        for (draft, focus) in [
            ("/compact", ""),
            ("/compact \t ", ""),
            (
                "/compact  preserve the Python state \t",
                "preserve the Python state",
            ),
        ] {
            let command = parse(draft).expect("recognised compact command");
            assert_eq!(command.action, Action::Compact);
            assert_eq!(command.argument, focus);
        }
    }

    #[test]
    fn direct_skill_command_accepts_bare_and_multiline_requests() {
        let bare = parse("/skill:review").unwrap();
        assert_eq!(bare.action, Action::Skill);
        assert_eq!(bare.skill_name, Some("review"));
        let request = parse("/skill:review inspect this\n  and this").unwrap();
        assert_eq!(request.skill_name, Some("review"));
        assert_eq!(
            parse("/skill:review-more").unwrap().skill_name,
            Some("review-more")
        );
        assert_eq!(parse("/skill:").unwrap().skill_name, Some(""));
    }

    #[test]
    fn skill_completion_matches_names_and_canonical_tokens_without_duplicates() {
        let mut completion =
            Completion::with_skills([("review-more", "Review more"), ("review", "Review code")]);
        for draft in ["/", "/revi", "/review", "/skill:", "/skill:revi"] {
            completion.sync(draft, true);
            let rows = completion.visible_rows(usize::MAX);
            let skills = rows
                .iter()
                .filter(|row| row.token.starts_with("/skill:"))
                .map(|row| (row.token, row.purpose))
                .collect::<Vec<_>>();
            assert_eq!(
                skills,
                [
                    ("/skill:review", "Review code"),
                    ("/skill:review-more", "Review more"),
                ],
                "{draft}"
            );
        }
        completion.sync("/review-more", true);
        assert_eq!(completion.selected_token(), Some("/skill:review-more"));
        assert_eq!(completion.visible_rows(usize::MAX).len(), 1);
        completion.sync("/skill:review-more", true);
        assert!(completion.visible_rows(usize::MAX).is_empty());
    }

    #[test]
    fn exact_builtins_take_priority_over_skill_name_matches() {
        let mut skills = super::CATALOGUE
            .iter()
            .map(|command| (command.token.strip_prefix('/').unwrap(), "Same name"))
            .collect::<Vec<_>>();
        skills.push(("model-review", "Longer name"));
        let mut completion = Completion::with_skills(skills);
        completion.sync("/mo", true);
        assert_eq!(
            completion
                .visible_rows(usize::MAX)
                .iter()
                .map(|row| row.token)
                .collect::<Vec<_>>(),
            ["/model", "/skill:model", "/skill:model-review"]
        );
        for command in super::CATALOGUE {
            completion.sync(command.token, true);
            assert!(
                completion.visible_rows(usize::MAX).is_empty(),
                "{}",
                command.token
            );
        }
        completion.sync("/skill:mo", true);
        assert_eq!(completion.selected_token(), Some("/skill:model"));
    }

    #[test]
    fn skill_name_completion_keeps_token_boundaries_and_case_sensitivity() {
        let mut completion = Completion::with_skills([("review", "Review code")]);
        for (draft, caret_at_end) in [
            ("/Revi", true),
            ("/evi", true),
            ("/reviewing", true),
            ("revi", true),
            (" /revi", true),
            ("/revi ", true),
            ("/revi inspect this", true),
            ("/revi\n", true),
            ("/revi\t", true),
            ("/revi", false),
        ] {
            completion.sync(draft, caret_at_end);
            assert!(completion.visible_rows(usize::MAX).is_empty(), "{draft:?}");
        }
    }

    #[test]
    fn skill_selection_is_preserved_between_name_and_canonical_searches() {
        let mut completion =
            Completion::with_skills([("review", "Review code"), ("revision", "Revise code")]);
        completion.sync("/rev", true);
        assert_eq!(completion.selected_token(), Some("/skill:review"));
        assert!(completion.move_selection(1));
        assert_eq!(completion.selected_token(), Some("/skill:revision"));
        completion.sync("/skill:rev", true);
        assert_eq!(completion.selected_token(), Some("/skill:revision"));
        completion.sync("/review", true);
        assert_eq!(completion.selected_token(), Some("/skill:review"));
    }

    #[test]
    fn direct_skill_completion_shows_only_catalog_entries_and_descriptions() {
        let mut completion = Completion::with_skills([
            ("review", "Review code"),
            ("direct", "Only direct invocation"),
        ]);
        completion.sync("/skill:", true);
        assert_eq!(
            completion
                .visible_rows(usize::MAX)
                .iter()
                .map(|row| (row.token, row.purpose))
                .collect::<Vec<_>>(),
            [
                ("/skill:direct", "Only direct invocation"),
                ("/skill:review", "Review code"),
            ]
        );
        completion.sync("/skill:direct", true);
        assert!(completion.visible_rows(usize::MAX).is_empty());
    }

    #[test]
    fn completion_rows_keep_catalogue_order_and_purposes() {
        let mut completion = Completion::default();
        completion.sync("/", true);

        assert_eq!(
            completion
                .visible_rows(usize::MAX)
                .iter()
                .map(|row| (row.token, row.purpose))
                .collect::<Vec<_>>(),
            [
                ("/compact", "Compact context"),
                ("/model", "Choose model"),
                ("/name", "Name session"),
                ("/settings", "Open settings"),
                ("/theme", "Choose theme"),
                ("/thinking", "Choose thinking level"),
                ("/todos", "Inspect TODO lists"),
                ("/tree", "Navigate session history"),
            ]
        );
    }

    #[test]
    fn exact_parsing_keeps_existing_arguments_and_token_ranges() {
        for (draft, action, argument, token_range) in [
            ("/compact", Action::Compact, "", 0..8),
            (
                "/compact  focus Python state\t",
                Action::Compact,
                "focus Python state",
                0..8,
            ),
            ("/model", Action::Model, "", 0..6),
            ("/model  terra\t", Action::Model, "terra", 0..6),
            ("/name  A session  ", Action::Name, "A session", 0..5),
            ("/settings", Action::Settings, "", 0..9),
            ("/settings \t ", Action::Settings, "", 0..9),
            ("/theme", Action::Theme, "", 0..6),
            ("/theme \t ", Action::Theme, "", 0..6),
            ("/thinking\t high", Action::Thinking, "high", 0..9),
            ("/tree", Action::Tree, "", 0..5),
            ("/tree  auth\t", Action::Tree, "auth", 0..5),
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
            "/compact-more",
            "/namex",
            "/settings theme",
            "/settings-more",
            "/settings\n",
            "/theme dark",
            "/theme-more",
            "/theme\n",
            "/thinking-more",
            "/tree-more",
            "/tree\nauth",
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
            (
                "/",
                true,
                vec![
                    "/compact",
                    "/model",
                    "/name",
                    "/settings",
                    "/theme",
                    "/thinking",
                    "/todos",
                    "/tree",
                ],
            ),
            ("/co", true, vec!["/compact"]),
            ("/mo", true, vec!["/model"]),
            ("/model", true, vec![]),
            ("/model ", true, vec![]),
            ("/se", true, vec!["/settings"]),
            ("/settings", true, vec![]),
            ("/settings ", true, vec![]),
            ("/th", true, vec!["/theme", "/thinking"]),
            ("/the", true, vec!["/theme"]),
            ("/theme", true, vec![]),
            ("/theme ", true, vec![]),
            ("/tr", true, vec!["/tree"]),
            ("/tree auth", true, vec![]),
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
        assert_eq!(completion.selected_token(), Some("/compact"));
        assert!(completion.move_selection(1));
        assert_eq!(completion.selected_token(), Some("/model"));

        completion.sync("/n", true);
        assert_eq!(completion.selected_token(), Some("/name"));
        completion.sync("/m", true);
        assert_eq!(completion.selected_token(), Some("/model"));
        assert!(!completion.move_selection(-1));

        completion.sync("/", true);
        assert!(completion.move_selection(99));
        assert_eq!(completion.selected_token(), Some("/tree"));
        assert!(!completion.move_selection(1));
        completion.sync("/t", true);
        assert_eq!(completion.selected_token(), Some("/tree"));

        let rows = completion.visible_rows(1);
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].token, "/tree");
        assert!(rows[0].selected);
    }
}
