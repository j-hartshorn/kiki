PRAGMA user_version = 2;

CREATE TABLE session (
    singleton INTEGER PRIMARY KEY CHECK (singleton = 1),
    id TEXT NOT NULL UNIQUE,
    created_at_ms INTEGER NOT NULL,
    updated_at_ms INTEGER NOT NULL,
    cwd TEXT NOT NULL,
    name TEXT,
    active_entry_id INTEGER NOT NULL REFERENCES entries(id) DEFERRABLE INITIALLY DEFERRED,
    preferences_json TEXT NOT NULL,
    revision INTEGER NOT NULL CHECK (revision > 0)
);

CREATE TABLE checkpoints (
    id INTEGER PRIMARY KEY,
    runtime_identity TEXT NOT NULL,
    arena_state BLOB NOT NULL,
    authorisations_json TEXT NOT NULL
);

CREATE TABLE entries (
    id INTEGER PRIMARY KEY,
    parent_id INTEGER REFERENCES entries(id),
    run_id INTEGER REFERENCES runs(id),
    kind TEXT NOT NULL,
    checkpoint_id INTEGER NOT NULL REFERENCES checkpoints(id),
    payload_json TEXT NOT NULL,
    provider_replay BLOB,
    created_at_ms INTEGER NOT NULL,
    revision INTEGER NOT NULL CHECK (revision > 0),
    CHECK (
        (kind = 'session_start' AND parent_id IS NULL AND run_id IS NULL)
        OR (kind <> 'session_start' AND parent_id IS NOT NULL AND run_id IS NOT NULL)
    )
);

CREATE UNIQUE INDEX entries_one_session_start ON entries(kind) WHERE kind = 'session_start';
CREATE INDEX entries_by_parent ON entries(parent_id);
CREATE INDEX entries_by_run ON entries(run_id);

CREATE TABLE runs (
    id INTEGER PRIMARY KEY,
    terminal_entry_id INTEGER REFERENCES entries(id)
);

CREATE UNIQUE INDEX runs_one_open ON runs((1)) WHERE terminal_entry_id IS NULL;

CREATE TABLE model_steps (
    id INTEGER PRIMARY KEY,
    run_id INTEGER NOT NULL REFERENCES runs(id),
    settled_entry_id INTEGER REFERENCES entries(id)
);

CREATE INDEX model_steps_by_run ON model_steps(run_id);

CREATE TABLE subagent_tasks (
    id INTEGER PRIMARY KEY,
    parent_task_id INTEGER REFERENCES subagent_tasks(id),
    origin_step_id INTEGER NOT NULL REFERENCES model_steps(id),
    state TEXT NOT NULL CHECK (
        state IN ('starting', 'running', 'stopping', 'completed', 'failed', 'cancelled', 'timed_out')
    ),
    admitted_revision INTEGER NOT NULL CHECK (admitted_revision > 0),
    terminal_revision INTEGER CHECK (
        terminal_revision IS NULL OR terminal_revision > admitted_revision
    ),
    task_revision INTEGER NOT NULL CHECK (task_revision > 0),
    details_json TEXT NOT NULL,
    CHECK (
        (terminal_revision IS NULL AND state IN ('starting', 'running', 'stopping'))
        OR (terminal_revision IS NOT NULL AND state IN ('completed', 'failed', 'cancelled', 'timed_out'))
    )
);

CREATE INDEX subagent_tasks_by_parent ON subagent_tasks(parent_task_id);
CREATE INDEX subagent_tasks_by_origin_step ON subagent_tasks(origin_step_id);

CREATE TRIGGER session_revision_must_increase
BEFORE UPDATE ON session
WHEN NEW.revision <= OLD.revision
BEGIN
    SELECT RAISE(ABORT, 'session revision must increase');
END;

CREATE TRIGGER session_id_is_immutable
BEFORE UPDATE OF id ON session
WHEN NEW.id IS NOT OLD.id
BEGIN
    SELECT RAISE(ABORT, 'session ID is immutable');
END;

CREATE TRIGGER checkpoints_are_immutable_before_update
BEFORE UPDATE ON checkpoints
BEGIN
    SELECT RAISE(ABORT, 'checkpoints are immutable');
END;

CREATE TRIGGER checkpoints_are_immutable_before_delete
BEFORE DELETE ON checkpoints
BEGIN
    SELECT RAISE(ABORT, 'checkpoints are immutable');
END;

CREATE TRIGGER entries_are_immutable_before_update
BEFORE UPDATE ON entries
BEGIN
    SELECT RAISE(ABORT, 'entries are immutable');
END;

CREATE TRIGGER entries_are_immutable_before_delete
BEFORE DELETE ON entries
BEGIN
    SELECT RAISE(ABORT, 'entries are immutable');
END;

CREATE TRIGGER entries_parent_must_be_older
AFTER INSERT ON entries
WHEN NEW.parent_id IS NOT NULL
    AND (
        NEW.parent_id >= NEW.id
        OR (SELECT revision FROM entries WHERE id = NEW.parent_id) >= NEW.revision
    )
BEGIN
    SELECT RAISE(ABORT, 'entry parent must be older');
END;

CREATE TRIGGER runs_terminal_entry_matches_run
BEFORE UPDATE OF terminal_entry_id ON runs
WHEN NEW.terminal_entry_id IS NOT NULL
    AND (SELECT run_id FROM entries WHERE id = NEW.terminal_entry_id) IS NOT NEW.id
BEGIN
    SELECT RAISE(ABORT, 'terminal entry belongs to another run');
END;

CREATE TRIGGER runs_settle_once
BEFORE UPDATE OF terminal_entry_id ON runs
WHEN OLD.terminal_entry_id IS NOT NULL OR NEW.terminal_entry_id IS NULL
BEGIN
    SELECT RAISE(ABORT, 'run settlement is final');
END;

CREATE TRIGGER runs_cannot_close_with_pending_steps
BEFORE UPDATE OF terminal_entry_id ON runs
WHEN NEW.terminal_entry_id IS NOT NULL
    AND EXISTS (
        SELECT 1 FROM model_steps
        WHERE run_id = NEW.id AND settled_entry_id IS NULL
    )
BEGIN
    SELECT RAISE(ABORT, 'closed run has pending model steps');
END;

CREATE TRIGGER model_steps_cannot_start_in_closed_run
BEFORE INSERT ON model_steps
WHEN EXISTS (SELECT 1 FROM runs WHERE id = NEW.run_id AND terminal_entry_id IS NOT NULL)
BEGIN
    SELECT RAISE(ABORT, 'model step belongs to closed run');
END;

CREATE TRIGGER model_steps_run_is_immutable
BEFORE UPDATE OF run_id ON model_steps
BEGIN
    SELECT RAISE(ABORT, 'model step run is immutable');
END;

CREATE TRIGGER model_steps_settled_entry_matches_run
BEFORE UPDATE OF settled_entry_id ON model_steps
WHEN NEW.settled_entry_id IS NOT NULL
    AND (SELECT run_id FROM entries WHERE id = NEW.settled_entry_id) IS NOT NEW.run_id
BEGIN
    SELECT RAISE(ABORT, 'settled entry belongs to another run');
END;

CREATE TRIGGER model_steps_settle_once
BEFORE UPDATE OF settled_entry_id ON model_steps
WHEN OLD.settled_entry_id IS NOT NULL OR NEW.settled_entry_id IS NULL
BEGIN
    SELECT RAISE(ABORT, 'model step settlement is final');
END;

CREATE TRIGGER subagent_tasks_parent_is_older
AFTER INSERT ON subagent_tasks
WHEN NEW.parent_task_id IS NOT NULL AND NEW.parent_task_id >= NEW.id
BEGIN
    SELECT RAISE(ABORT, 'subagent task parent must be older');
END;

CREATE TRIGGER subagent_tasks_parent_must_share_origin_step
BEFORE INSERT ON subagent_tasks
WHEN NEW.parent_task_id IS NOT NULL
    AND NEW.origin_step_id IS NOT (
        SELECT origin_step_id
        FROM subagent_tasks
        WHERE id = NEW.parent_task_id
    )
BEGIN
    SELECT RAISE(ABORT, 'subagent task parent has another origin step');
END;

CREATE TRIGGER subagent_tasks_parent_is_immutable
BEFORE UPDATE OF parent_task_id ON subagent_tasks
BEGIN
    SELECT RAISE(ABORT, 'subagent task parent is immutable');
END;

CREATE TRIGGER subagent_tasks_origin_is_immutable
BEFORE UPDATE OF origin_step_id ON subagent_tasks
BEGIN
    SELECT RAISE(ABORT, 'subagent task origin is immutable');
END;

CREATE TRIGGER subagent_tasks_admission_is_immutable
BEFORE UPDATE OF admitted_revision ON subagent_tasks
BEGIN
    SELECT RAISE(ABORT, 'subagent task admission is immutable');
END;

CREATE TRIGGER subagent_tasks_settled_are_immutable
BEFORE UPDATE ON subagent_tasks
WHEN OLD.terminal_revision IS NOT NULL
BEGIN
    SELECT RAISE(ABORT, 'settled subagent task is immutable');
END;

CREATE TRIGGER subagent_tasks_revision_must_increase
BEFORE UPDATE ON subagent_tasks
WHEN NEW.task_revision <= OLD.task_revision
BEGIN
    SELECT RAISE(ABORT, 'subagent task revision must increase');
END;

CREATE TRIGGER subagent_tasks_settle_once
BEFORE UPDATE OF terminal_revision ON subagent_tasks
WHEN OLD.terminal_revision IS NOT NULL OR NEW.terminal_revision IS NULL
BEGIN
    SELECT RAISE(ABORT, 'subagent task settlement is final');
END;
