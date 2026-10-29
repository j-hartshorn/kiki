use serde::{Deserialize, Serialize};

pub(crate) const BOOTSTRAP: &str = include_str!("todos.py");
pub(crate) const STUBS: &str = r#"
class TodoList:
    def __init__(self) -> None: ...
    def add(self, text: str, status: str = "pending") -> int: ...
    def update(self, item_id: int, *, text: str = ..., status: str = ...) -> None: ...
    def remove(self, item_id: int) -> None: ...
    def replace(self, items: list[dict]) -> list[dict]: ...
    def snapshot(self) -> list[dict]: ...

todos: TodoList
"#;
const GUIDANCE: &str = "## TODO list\n\nFor substantial multi-step work, track your own ordered list with synchronous `todos` methods (no await). Use `todos.add(text, status=\"pending\")` to get an integer ID; `todos.update(item_id, text=..., status=...)` preserves ID/order; `todos.remove(item_id)` removes it. `todos.replace(items)` starts a revised list with fresh IDs (snapshot IDs are ignored); `[]` clears without resetting IDs. `todos.snapshot()` returns independent dictionaries with id/text/status. Status is pending, in_progress, or completed; maximum 64 items and 512 Unicode code points of nonblank text per item. Invalid input raises ValueError; unknown IDs raise KeyError. Prefer methods over private-field mutation.\n";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TodoPolicy {
    enabled: bool,
}

impl TodoPolicy {
    pub fn capture(enabled: bool) -> Self {
        Self { enabled }
    }
    pub fn enabled(self) -> bool {
        self.enabled
    }
    pub fn reference(self) -> &'static str {
        if self.enabled { GUIDANCE } else { "" }
    }
    pub(crate) fn source(self) -> &'static str {
        if self.enabled {
            "_kiki_todo_enabled = True"
        } else {
            "_kiki_todo_enabled = False"
        }
    }
}

impl Default for TodoPolicy {
    fn default() -> Self {
        Self::capture(true)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TodoStatus {
    Pending,
    InProgress,
    Completed,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TodoItem {
    pub id: i64,
    pub text: String,
    pub status: TodoStatus,
}

pub type TodoSnapshot = Vec<TodoItem>;
