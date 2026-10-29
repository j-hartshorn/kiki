use super::tests::{arena, arena_with_read_gate, complete, execute_checked};
use super::{
    Arena, ArenaError, CancellationToken, Checkpoint, Execution, ExecutionOutcome, RuntimeIdentity,
    ToolValue, TurnEvent, bootstrap, on_print_sync, result_values_from_monty, type_check_stubs,
};
use std::collections::BTreeMap;

// Feasibility fixture only, not a production TODO tool or a private-state security boundary.
const TODO_FIXTURE: &str = r#"
_kiki_todo_enabled = True

class TodoList:
    def __init__(self):
        self._reset()

    def add(self, text, status="pending"):
        if not _kiki_todo_enabled:
            raise RuntimeError("TODO proof disabled")
        item_id = self._next_id
        self._items.append({"id": item_id, "text": text, "status": status})
        self._next_id += 1
        return item_id

    def snapshot(self):
        if not _kiki_todo_enabled:
            raise RuntimeError("TODO proof disabled")
        return [dict(item) for item in self._items]

    def _reset(self):
        self._items = []
        self._next_id = 1

todos = TodoList()
_kiki_host_names = set(_kiki_locals())
"#;

const TODO_STUBS: &str = r#"
class TodoList:
    def __init__(self) -> None: ...
    def add(self, text: str, status: str = "pending") -> int: ...
    def snapshot(self) -> list[dict[str, object]]: ...

todos: TodoList
"#;

async fn install_fixture(arena: &mut Arena) {
    arena.repl_config.type_check_stubs = Some(format!("{}\n{TODO_STUBS}", type_check_stubs()));
    arena.checkout.take();
    let mut checkout = arena.pool.checkout(&arena.repl_config).await.unwrap();
    bootstrap(&mut checkout).await.unwrap();
    arena.checkout = Some(checkout);
    trusted_commit(arena, TODO_FIXTURE).await;
}

async fn proof_arena(workspace: &tempfile::TempDir) -> Arena {
    let mut arena = arena(workspace).await;
    install_fixture(&mut arena).await;
    arena
}

async fn trusted_commit(arena: &mut Arena, source: &str) {
    let mut on_print = on_print_sync(|_, _| {});
    let checkout = arena.checkout.as_mut().unwrap();
    let event = checkout
        .feed(source, vec![], vec![], true, &mut on_print)
        .await
        .unwrap();
    assert!(matches!(event, TurnEvent::Complete(_)), "{event:?}");
    arena.checkpoint.state = checkout.dump().await.unwrap();
    arena.bindings = None;
}

async fn apply_run_policy(arena: &mut Arena, enabled: bool) {
    trusted_commit(
        arena,
        if enabled {
            "_kiki_todo_enabled = True"
        } else {
            "_kiki_todo_enabled = False"
        },
    )
    .await;
}

async fn restore_for_run(arena: &mut Arena, checkpoint: Checkpoint, enabled: bool) {
    arena.restore(checkpoint).await.unwrap();
    apply_run_policy(arena, enabled).await;
}

async fn child_for_run(
    parent: &Arena,
    checkpoint: &Checkpoint,
    read_only: bool,
    enabled: bool,
) -> Arena {
    let mut progress = |_| {};
    let mut child = parent
        .child_source()
        .child(checkpoint, read_only, &mut progress)
        .await
        .unwrap();
    trusted_commit(&mut child, "todos._reset()").await;
    apply_run_policy(&mut child, enabled).await;
    child
}

async fn checked(arena: &mut Arena, source: &str) -> Execution {
    let execution = execute_checked(arena, source).await.unwrap();
    complete(&execution);
    execution
}

async fn projection(arena: &Arena) -> ToolValue {
    let before = arena.checkpoint();
    arena.validate_checkpoint(&before).unwrap();
    let pool = Arena::open_pool(arena.worker_path.clone()).await.unwrap();
    let mut checkout = pool.checkout(&arena.repl_config).await.unwrap();
    Arena::restore_checkout(&mut checkout, &before)
        .await
        .unwrap();
    let mut on_print = on_print_sync(|_, _| {});
    let event = checkout
        .feed(
            "{'todos': todos.snapshot()}",
            vec![],
            vec![],
            true,
            &mut on_print,
        )
        .await
        .unwrap();
    let TurnEvent::Complete(value) = event else {
        panic!("{event:?}")
    };
    let mut values = result_values_from_monty(value).unwrap();
    assert_eq!(arena.checkpoint(), before);
    values.remove("todos").unwrap()
}

fn items(texts: &[&str]) -> ToolValue {
    ToolValue::List(
        texts
            .iter()
            .enumerate()
            .map(|(index, text)| {
                ToolValue::Dictionary(BTreeMap::from([
                    ("id".to_owned(), ToolValue::Integer(index as i64 + 1)),
                    ("text".to_owned(), ToolValue::String((*text).to_owned())),
                    ("status".to_owned(), ToolValue::String("pending".to_owned())),
                ]))
            })
            .collect(),
    )
}

#[tokio::test]
async fn todo_proof_checked_sync_methods_hidden_mutation_and_independent_snapshot() {
    let workspace = tempfile::tempdir().unwrap();
    let mut arena = proof_arena(&workspace).await;
    assert!(arena.repl_config.type_check);
    assert!(!arena.model_type_check_disabled);
    checked(&mut arena, "fresh: TodoList = TodoList()\nassert fresh.snapshot() == []\nassert isinstance(todos, TodoList)").await;
    let before = arena
        .current_bindings(&CancellationToken::new())
        .await
        .unwrap();
    assert!(!before.iter().any(|binding| binding.name == "todos"));
    let mutation = checked(&mut arena, "todos.add('first')").await;
    assert_eq!(complete(&mutation).unwrap().head, "1");
    assert!(
        mutation.changed_bindings.is_empty(),
        "{:?}",
        mutation.changed_bindings
    );
    assert_eq!(projection(&arena).await, items(&["first"]));
    checked(&mut arena, "copy = todos.snapshot()\ncopy[0]['text'] = 'not live'\ncopy.append({'id': 99, 'text': 'not live', 'status': 'pending'})").await;
    assert_eq!(projection(&arena).await, items(&["first"]));
    checked(&mut arena, "result_values = {'snapshot': todos.snapshot()}").await;
    assert_eq!(
        arena.collect_result_values().await.unwrap(),
        BTreeMap::from([("snapshot".to_owned(), items(&["first"]))])
    );
    let invalid = execute_checked(&mut arena, "todos.add(42)").await.unwrap();
    assert!(
        matches!(invalid.outcome, ExecutionOutcome::TypeCheckError(_)),
        "{invalid:?}"
    );
    assert_eq!(projection(&arena).await, items(&["first"]));
}

#[tokio::test]
async fn todo_proof_projection_preserves_checkpoint_bindings_and_continuation() {
    let workspace = tempfile::tempdir().unwrap();
    let mut arena = proof_arena(&workspace).await;
    checked(
        &mut arena,
        "other = [40]\nresult_values = {'untouched': 7}\ntodos.add('first')",
    )
    .await;
    let checkpoint = arena.checkpoint();
    let bindings = arena
        .current_bindings(&CancellationToken::new())
        .await
        .unwrap();
    assert_eq!(projection(&arena).await, items(&["first"]));
    assert_eq!(projection(&arena).await, items(&["first"]));
    assert_eq!(arena.checkpoint(), checkpoint);
    assert_eq!(
        arena
            .current_bindings(&CancellationToken::new())
            .await
            .unwrap(),
        bindings
    );
    checked(&mut arena, "other[0] += 2\nassert other == [42]\nassert result_values == {'untouched': 7}\nassert todos.add('second') == 2").await;
    assert_eq!(projection(&arena).await, items(&["first", "second"]));
}

#[tokio::test]
async fn todo_proof_ordinary_exception_commits_object_mutation() {
    let workspace = tempfile::tempdir().unwrap();
    let mut arena = proof_arena(&workspace).await;
    checked(&mut arena, "todos.add('first')").await;
    let execution = execute_checked(
        &mut arena,
        "todos.add('before error')\nraise ValueError('ordinary failure')",
    )
    .await
    .unwrap();
    let ExecutionOutcome::PythonError(error) = execution.outcome else {
        panic!("{execution:?}")
    };
    assert!(
        error.text.contains("ValueError: ordinary failure"),
        "{}",
        error.text
    );
    assert!(execution.changed_bindings.is_empty());
    assert_eq!(projection(&arena).await, items(&["first", "before error"]));
    checked(&mut arena, "assert todos.add('after error') == 3").await;
}

#[tokio::test]
async fn todo_proof_cancellation_discards_object_mutation_and_allocator() {
    let workspace = tempfile::tempdir().unwrap();
    std::fs::write(workspace.path().join("file"), "read").unwrap();
    let (mut arena, gate) = arena_with_read_gate(&workspace).await;
    install_fixture(&mut arena).await;
    checked(&mut arena, "todos.add('committed')").await;
    let checkpoint = arena.checkpoint();
    let cancellation = CancellationToken::new();
    let mut progress = |_| {};
    let mut execution = Box::pin(arena.execute_model_code(
        "todos.add('discarded')\nawait read('file')",
        &cancellation,
        &mut progress,
        None,
        &[],
    ));
    tokio::select! {
        _ = gate.wait_until_read_started() => {}
        result = &mut execution => panic!("execution finished before read: {result:?}"),
    }
    cancellation.cancel();
    gate.resume_read();
    assert!(matches!(execution.await, Err(ArenaError::Cancelled { .. })));
    assert_eq!(arena.checkpoint(), checkpoint);
    assert_eq!(projection(&arena).await, items(&["committed"]));
    checked(&mut arena, "assert todos.add('continued') == 2").await;
    assert_eq!(projection(&arena).await, items(&["committed", "continued"]));
}

#[tokio::test]
async fn todo_proof_restored_branches_keep_independent_state_and_allocator() {
    let workspace = tempfile::tempdir().unwrap();
    let mut arena = proof_arena(&workspace).await;
    checked(&mut arena, "todos.add('base')").await;
    let base = arena.checkpoint();
    checked(&mut arena, "assert todos.add('left') == 2").await;
    let left = arena.checkpoint();
    restore_for_run(&mut arena, base, true).await;
    checked(&mut arena, "assert todos.add('right') == 2").await;
    let right = arena.checkpoint();
    assert_eq!(projection(&arena).await, items(&["base", "right"]));
    restore_for_run(&mut arena, left, true).await;
    assert_eq!(projection(&arena).await, items(&["base", "left"]));
    checked(&mut arena, "assert todos.add('left continuation') == 3").await;
    restore_for_run(&mut arena, right, true).await;
    assert_eq!(projection(&arena).await, items(&["base", "right"]));
    let invalid = execute_checked(&mut arena, "todos.add(42)").await.unwrap();
    assert!(
        matches!(invalid.outcome, ExecutionOutcome::TypeCheckError(_)),
        "{invalid:?}"
    );
}

#[tokio::test]
async fn todo_proof_child_reset_preserves_other_bindings_parent_and_read_only_policy() {
    let workspace = tempfile::tempdir().unwrap();
    let mut parent = proof_arena(&workspace).await;
    checked(&mut parent, "other = [40]\ndef helper():\n    return other[0]\nalias = helper\nretained_add = todos.add\ntodos.add('parent')\nresult_values = {'parent': 'kept'}").await;
    let checkpoint = parent.checkpoint();
    let mut child = child_for_run(&parent, &checkpoint, true, true).await;
    assert_eq!(parent.checkpoint(), checkpoint);
    assert_eq!(
        child.collect_result_values().await.unwrap(),
        BTreeMap::new()
    );
    assert_eq!(projection(&child).await, items(&[]));
    checked(&mut child, "assert alias() == 40\nother[0] = 42\nassert alias() == 42\nassert retained_add('child') == 1").await;
    assert_eq!(projection(&child).await, items(&["child"]));
    let denied = execute_checked(&mut child, "await write('forbidden', 'no')")
        .await
        .unwrap();
    let ExecutionOutcome::PythonError(error) = denied.outcome else {
        panic!("{denied:?}")
    };
    assert!(error.text.contains("PermissionError"), "{}", error.text);
    assert!(!workspace.path().join("forbidden").exists());
    let mut sibling = child_for_run(&parent, &checkpoint, false, false).await;
    let unavailable = execute_checked(&mut sibling, "retained_add('disabled sibling')")
        .await
        .unwrap();
    let ExecutionOutcome::PythonError(error) = unavailable.outcome else {
        panic!("{unavailable:?}")
    };
    assert!(
        error.text.contains("RuntimeError: TODO proof disabled"),
        "{}",
        error.text
    );
    apply_run_policy(&mut sibling, true).await;
    assert_eq!(projection(&sibling).await, items(&[]));
    checked(&mut sibling, "assert todos.add('sibling') == 1").await;
    let invalid = execute_checked(&mut child, "todos.add(42)").await.unwrap();
    assert!(
        matches!(invalid.outcome, ExecutionOutcome::TypeCheckError(_)),
        "{invalid:?}"
    );
    let mut grandchild = child_for_run(&child, &child.checkpoint(), false, true).await;
    assert_eq!(projection(&grandchild).await, items(&[]));
    checked(
        &mut grandchild,
        "assert alias() == 42\nother[0] = 99\nassert todos.add('grandchild') == 1",
    )
    .await;
    assert_eq!(projection(&grandchild).await, items(&["grandchild"]));
    assert_eq!(projection(&sibling).await, items(&["sibling"]));
    checked(&mut child, "assert alias() == 42").await;
    assert_eq!(projection(&child).await, items(&["child"]));
    assert_eq!(projection(&parent).await, items(&["parent"]));
    checked(
        &mut parent,
        "assert alias() == 40\nassert result_values == {'parent': 'kept'}",
    )
    .await;
}

#[tokio::test]
async fn todo_proof_run_policy_overrides_enabled_restore_and_retained_references() {
    let workspace = tempfile::tempdir().unwrap();
    let mut arena = proof_arena(&workspace).await;
    checked(&mut arena, "retained = todos\nretained_add = todos.add\nretained_snapshot = todos.snapshot\ntodos.add('kept')").await;
    let enabled = arena.checkpoint();
    for source in [
        "todos.add('forbidden')",
        "retained.add('forbidden')",
        "retained_add('forbidden')",
        "retained_snapshot()",
        "retained.snapshot()",
    ] {
        restore_for_run(&mut arena, enabled.clone(), false).await;
        let execution = execute_checked(&mut arena, source).await.unwrap();
        let ExecutionOutcome::PythonError(error) = execution.outcome else {
            panic!("{source}: {execution:?}")
        };
        assert!(
            error.text.contains("RuntimeError: TODO proof disabled"),
            "{source}: {}",
            error.text
        );
        apply_run_policy(&mut arena, true).await;
        assert_eq!(projection(&arena).await, items(&["kept"]));
    }
    checked(&mut arena, "assert retained_add('reenabled') == 2").await;
    assert_eq!(projection(&arena).await, items(&["kept", "reenabled"]));
}

#[tokio::test]
async fn todo_proof_pre_fixture_checkpoint_does_not_gain_bootstrap_or_stubs_on_restore() {
    let workspace = tempfile::tempdir().unwrap();
    let mut arena = arena(&workspace).await;
    checked(&mut arena, "other = [42]").await;
    let old = arena.checkpoint();
    install_fixture(&mut arena).await;
    checked(&mut arena, "todos.add('new fixture')").await;
    assert_eq!(arena.checkpoint().identity, old.identity);
    let before = arena.checkpoint();
    let mut foreign = old.clone();
    foreign.identity = RuntimeIdentity("foreign-todo-proof".to_owned());
    assert!(matches!(
        arena.restore(foreign).await,
        Err(ArenaError::IncompatibleCheckpoint { .. })
    ));
    assert_eq!(arena.checkpoint(), before);
    arena.restore(old).await.unwrap();
    checked(&mut arena, "assert other == [42]").await;
    trusted_commit(
        &mut arena,
        "assert 'todos' not in _kiki_locals()\nassert 'TodoList' not in _kiki_locals()",
    )
    .await;
    let missing = execute_checked(&mut arena, "todos.snapshot()")
        .await
        .unwrap();
    let ExecutionOutcome::TypeCheckError(error) = missing.outcome else {
        panic!("{missing:?}")
    };
    assert!(
        error.text.contains("unresolved-reference"),
        "{}",
        error.text
    );
    assert!(error.text.contains("todos"), "{}", error.text);
}
