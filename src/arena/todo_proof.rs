use super::tests::{arena, arena_with_read_gate, complete, execute_checked};
use super::{
    Arena, ArenaError, CancellationToken, Checkpoint, Execution, ExecutionOutcome, RuntimeIdentity,
    ToolValue,
};
use std::collections::BTreeMap;

async fn proof_arena(workspace: &tempfile::TempDir) -> Arena {
    arena(workspace).await
}

async fn checked(arena: &mut Arena, source: &str) -> Execution {
    let execution = execute_checked(arena, source).await.unwrap();
    complete(&execution);
    execution
}

async fn projection(arena: &Arena) -> ToolValue {
    ToolValue::List(
        arena
            .todos()
            .iter()
            .map(|item| {
                ToolValue::Dictionary(BTreeMap::from([
                    ("id".to_owned(), ToolValue::Integer(item.id)),
                    ("text".to_owned(), ToolValue::String(item.text.clone())),
                    (
                        "status".to_owned(),
                        ToolValue::String(
                            match item.status {
                                crate::todos::TodoStatus::Pending => "pending",
                                crate::todos::TodoStatus::InProgress => "in_progress",
                                crate::todos::TodoStatus::Completed => "completed",
                            }
                            .to_owned(),
                        ),
                    ),
                ]))
            })
            .collect(),
    )
}

async fn apply_run_policy(arena: &mut Arena, enabled: bool) {
    arena
        .apply_todo_policy(crate::todos::TodoPolicy::capture(enabled))
        .await
        .unwrap();
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
    let mut child = parent.child(checkpoint, read_only).await.unwrap();
    child.reset_todos().await.unwrap();
    apply_run_policy(&mut child, enabled).await;
    child
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
    assert_eq!(execution.todos.as_ref(), Some(arena.todos()));
    assert_eq!(projection(&arena).await, items(&["first", "before error"]));
    checked(&mut arena, "assert todos.add('after error') == 3").await;
}

#[tokio::test]
async fn todo_proof_cancellation_discards_object_mutation_and_allocator() {
    let workspace = tempfile::tempdir().unwrap();
    std::fs::write(workspace.path().join("file"), "read").unwrap();
    let (mut arena, gate) = arena_with_read_gate(&workspace).await;
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
        error.text.contains("RuntimeError: TODO list is disabled"),
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
    checked(&mut arena, "retained = todos\nretained_add = todos.add\nretained_snapshot = todos.snapshot\nretained_update = todos.update\nretained_remove = todos.remove\nretained_replace = todos.replace\ntodos.add('kept')").await;
    let enabled = arena.checkpoint();
    for source in [
        "todos.add('forbidden')",
        "retained.add('forbidden')",
        "retained_add('forbidden')",
        "retained_snapshot()",
        "retained.snapshot()",
        "retained_update(1, text='forbidden')",
        "retained_remove(1)",
        "retained_replace([])",
    ] {
        restore_for_run(&mut arena, enabled.clone(), false).await;
        let execution = execute_checked(&mut arena, source).await.unwrap();
        let ExecutionOutcome::PythonError(error) = execution.outcome else {
            panic!("{source}: {execution:?}")
        };
        assert!(
            error.text.contains("RuntimeError: TODO list is disabled"),
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
async fn todo_production_identity_replacement_and_copies() {
    let workspace = tempfile::tempdir().unwrap();
    let mut arena = proof_arena(&workspace).await;
    checked(&mut arena, r#"
assert todos.add('one') == 1
assert todos.add('two', 'in_progress') == 2
todos.update(1, text='edited', status='completed')
assert todos.update(2) is None
todos.update(2, text='two edited')
assert todos.snapshot()[1]['status'] == 'in_progress'
todos.update(2, status='pending')
assert todos.snapshot()[1]['text'] == 'two edited'
todos.update(2, text='two', status='in_progress')
assert todos.snapshot() == [{'id': 1, 'text': 'edited', 'status': 'completed'}, {'id': 2, 'text': 'two', 'status': 'in_progress'}]
todos.remove(1)
assert todos.snapshot() == [{'id': 2, 'text': 'two', 'status': 'in_progress'}]
assert todos.add('three') == 3
accepted = todos.replace(todos.snapshot())
assert accepted == [{'id': 4, 'text': 'two', 'status': 'in_progress'}, {'id': 5, 'text': 'three', 'status': 'pending'}]
accepted[0]['text'] = 'copy only'
assert todos.snapshot()[0]['text'] == 'two'
assert todos.replace([]) == []
assert todos.add('after clear') == 6
for stale in [1, 2, 3, 4, 5]:
    try:
        todos.update(stale, text='stale')
        raise AssertionError('stale update accepted')
    except KeyError:
        pass
    try:
        todos.remove(stale)
        raise AssertionError('stale remove accepted')
    except KeyError:
        pass
assert todos.snapshot() == [{'id': 6, 'text': 'after clear', 'status': 'pending'}]
"#).await;
    assert_eq!(arena.todos()[0].id, 6);
    let before = arena.checkpoint();
    let snapshot = arena.todos().clone();
    assert_eq!(arena.todos(), &snapshot);
    assert_eq!(arena.checkpoint(), before);
}

#[tokio::test]
async fn todo_production_validation_is_atomic_and_unicode_bounded() {
    let workspace = tempfile::tempdir().unwrap();
    let mut arena = proof_arena(&workspace).await;
    checked(&mut arena, r#"
from typing import Any
assert todos.add('🦀' * 512) == 1
before = todos.snapshot()
invalid_texts: list[Any] = ['', ' \t\n\u2003', '🦀' * 513, None, 42, ['nested']]
for invalid_text in invalid_texts:
    try:
        todos.add(invalid_text)
        raise AssertionError('invalid add accepted')
    except ValueError:
        pass
    try:
        todos.update(1, text=invalid_text)
        raise AssertionError('invalid update accepted')
    except ValueError:
        pass
    assert todos.snapshot() == before
invalid_statuses: list[Any] = ['', 'done', 'PENDING', None, 1, ['pending']]
for invalid_status in invalid_statuses:
    try:
        todos.add('invalid', invalid_status)
        raise AssertionError('invalid status accepted')
    except ValueError:
        pass
    try:
        todos.update(1, text='must not change', status=invalid_status)
        raise AssertionError('invalid update status accepted')
    except ValueError:
        pass
    assert todos.snapshot() == before
bad_replacements: list[Any] = [None, {}, 'text', [42], [{'text': 'missing'}], [{'status': 'pending'}], [{'text': 'extra', 'status': 'pending', 'phase': 'no'}], [{'text': ' ', 'status': 'pending'}], [{'text': 'bad', 'status': 'done'}], [{'text': '🦀' * 513, 'status': 'pending'}]]
for invalid_replacement in bad_replacements:
    try:
        todos.replace([{'text': 'valid first', 'status': 'pending'}, *invalid_replacement] if isinstance(invalid_replacement, list) else invalid_replacement)
        raise AssertionError('invalid replacement accepted')
    except ValueError:
        pass
    assert todos.snapshot() == before
assert todos.add('allocator unchanged') == 2
assert todos.replace([{'id': 'ignored', 'text': 'fresh', 'status': 'completed'}]) == [{'id': 3, 'text': 'fresh', 'status': 'completed'}]
"#).await;
    assert_eq!(arena.todos()[0].id, 3);
}

#[tokio::test]
async fn todo_production_all_statuses_count_toward_capacity() {
    let workspace = tempfile::tempdir().unwrap();
    let mut arena = proof_arena(&workspace).await;
    checked(&mut arena, r#"
statuses = ['pending', 'in_progress', 'completed']
for index in range(64):
    assert todos.add(str(index), statuses[index % 3]) == index + 1
before = todos.snapshot()
try:
    todos.add('65')
    raise AssertionError('oversized add accepted')
except ValueError:
    pass
try:
    todos.replace([{'text': str(index), 'status': statuses[index % 3]} for index in range(65)])
    raise AssertionError('oversized replacement accepted')
except ValueError:
    pass
assert todos.snapshot() == before
todos.remove(1)
assert todos.add('not consumed') == 65
accepted = todos.replace([{'text': '界' * 512, 'status': statuses[index % 3]} for index in range(64)])
assert len(accepted) == 64
assert accepted[0]['id'] == 66
assert accepted[63]['id'] == 129
"#).await;
    assert_eq!(arena.todos().len(), 64);
}

#[tokio::test]
async fn todo_production_v3_and_foreign_checkpoints_rejected_even_disabled() {
    let workspace = tempfile::tempdir().unwrap();
    let mut arena = proof_arena(&workspace).await;
    checked(&mut arena, "other = [42]\ntodos.add('kept')").await;
    apply_run_policy(&mut arena, false).await;
    let before = arena.checkpoint();
    for identity in ["monty-dump-13+type-check-v3", "foreign-todo-proof"] {
        let mut old = before.clone();
        old.identity = RuntimeIdentity(identity.to_owned());
        assert!(matches!(
            arena.restore(old).await,
            Err(ArenaError::IncompatibleCheckpoint { .. })
        ));
        assert_eq!(arena.checkpoint(), before);
    }
    assert!(arena.todo_reference().is_empty());
    assert!(
        !arena
            .child_source()
            .prompt_reference(false, true, crate::todos::TodoPolicy::capture(false))
            .contains("todos.add")
    );
    apply_run_policy(&mut arena, true).await;
    assert!(arena.todo_reference().contains("todos.add"));
    assert!(
        arena
            .child_source()
            .prompt_reference(false, true, crate::todos::TodoPolicy::capture(true))
            .contains("todos.add")
    );
    checked(
        &mut arena,
        "assert other == [42]\nassert todos.add('continued') == 2",
    )
    .await;
}

#[tokio::test]
async fn todo_production_collection_failure_discards_attempt_without_stale_publication() {
    let workspace = tempfile::tempdir().unwrap();
    let mut arena = proof_arena(&workspace).await;
    checked(&mut arena, "other = [42]\ntodos.add('committed')").await;
    let before = arena.checkpoint();
    let projection_before = arena.todos().clone();
    let failed = arena
        .execute(
            "todos.add('discarded')\n_kiki_todos._items = 42",
            &CancellationToken::new(),
        )
        .await;
    assert!(
        matches!(failed, Err(ArenaError::Infrastructure { ref message, .. }) if message.contains("TODO projection failed: not a list")),
        "{failed:?}"
    );
    assert_eq!(arena.checkpoint(), before);
    assert_eq!(arena.todos(), &projection_before);
    checked(
        &mut arena,
        "assert other == [42]\nassert todos.add('continued') == 2",
    )
    .await;
    let enabled = arena.checkpoint();
    apply_run_policy(&mut arena, false).await;
    arena.restore(enabled).await.unwrap();
    arena.checkout.take();
    let disabled = execute_checked(&mut arena, "todos.snapshot()")
        .await
        .unwrap();
    assert!(
        matches!(disabled.outcome, ExecutionOutcome::PythonError(ref error) if error.text.contains("RuntimeError: TODO list is disabled"))
    );
    assert_eq!(
        arena.todo_policy(),
        crate::todos::TodoPolicy::capture(false)
    );
    apply_run_policy(&mut arena, true).await;
    checked(&mut arena, "assert todos.add('after recovery') == 3").await;
}

#[tokio::test]
async fn todo_production_checked_snapshot_ids_and_replacement_variables() {
    let workspace = tempfile::tempdir().unwrap();
    let mut arena = proof_arena(&workspace).await;
    checked(
        &mut arena,
        r#"
items = [{"text": "first", "status": "pending"}, {"text": "second", "status": "pending"}]
accepted = todos.replace(items)
first_id = accepted[0]["id"]
todos.update(first_id, status="completed")
copy = todos.snapshot()
todos.remove(copy[1]["id"])
assert todos.snapshot() == [{"id": first_id, "text": "first", "status": "completed"}]
"#,
    )
    .await;
}
