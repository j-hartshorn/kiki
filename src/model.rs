#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ModelAction {
    Code { source: String },
    Message { text: String },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ModelContextEntry {
    ReadCapability { description: String },
    UserRequest { text: String },
    Execution { source: String, feedback: String },
    FinalMessage { text: String },
}

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum ModelError {
    #[error("scripted model exhausted before returning a final message")]
    Exhausted,
}

pub const READ_CAPABILITY_DESCRIPTION: &str = "read(path) returns the accepted file as complete exact lines with endings retained. Use standard Python print(\"\".join(lines[start:end]), end=\"\") to inspect a selection.";

pub struct ScriptedModel {
    actions: Vec<ModelAction>,
    cursor: usize,
    #[cfg(test)]
    contexts: Vec<Vec<ModelContextEntry>>,
}

impl ScriptedModel {
    pub fn new(actions: Vec<ModelAction>) -> Self {
        Self {
            actions,
            cursor: 0,
            #[cfg(test)]
            contexts: Vec::new(),
        }
    }

    pub fn reset(&mut self) {
        self.cursor = 0;
    }

    pub fn next_action(
        &mut self,
        context: &[ModelContextEntry],
    ) -> Result<ModelAction, ModelError> {
        #[cfg(test)]
        self.contexts.push(context.to_vec());
        #[cfg(not(test))]
        let _ = context;
        let action = self
            .actions
            .get(self.cursor)
            .cloned()
            .ok_or(ModelError::Exhausted)?;
        self.cursor += 1;
        Ok(action)
    }

    #[cfg(test)]
    pub(crate) fn contexts(&self) -> &[Vec<ModelContextEntry>] {
        &self.contexts
    }

    #[cfg(test)]
    pub(crate) fn position(&self) -> usize {
        self.cursor
    }
}

impl Default for ScriptedModel {
    fn default() -> Self {
        Self::new(vec![
            ModelAction::Code {
                source: "try:\n    request_count += 1\nexcept NameError:\n    request_count = 1\nlines = await read(\"Cargo.toml\")".to_owned(),
            },
            ModelAction::Code {
                source: "print(\"\".join(lines[0:20]), end=\"\")\nrequest_count".to_owned(),
            },
            ModelAction::Message {
                text: "I inspected Cargo.toml.".to_owned(),
            },
        ])
    }
}

#[cfg(test)]
mod tests {
    use super::{ModelAction, ModelError, ScriptedModel};

    #[test]
    fn returns_actions_in_order_resets_per_run_and_reports_exhaustion() {
        let actions = vec![
            ModelAction::Code {
                source: "first".to_owned(),
            },
            ModelAction::Message {
                text: "done".to_owned(),
            },
        ];
        let mut model = ScriptedModel::new(actions.clone());

        assert_eq!(model.next_action(&[]).unwrap(), actions[0]);
        assert_eq!(model.next_action(&[]).unwrap(), actions[1]);
        assert_eq!(model.next_action(&[]), Err(ModelError::Exhausted));

        model.reset();

        assert_eq!(model.next_action(&[]).unwrap(), actions[0]);
    }
}
