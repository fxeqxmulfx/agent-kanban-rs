use anyhow::{Result, bail};
use rusqlite::{Connection, OptionalExtension, TransactionBehavior};
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

pub const DB_DIR: &str = ".kanban";
pub const DB_FILE: &str = "board.db";

/// Process-wide override for the database file path, set at most once from
/// `main.rs` right after parsing the global `--db <path>` flag, before any
/// command runs. When set, it replaces the usual `.kanban/`
/// directory-discovery (for existing projects) and the default
/// `.kanban/board.db` location (for `init`) with this exact file path.
static PATH_OVERRIDE: OnceLock<PathBuf> = OnceLock::new();

/// Set the `--db <path>` override for this process. Must be called at most
/// once, before any other function in this module runs.
pub fn set_path_override(path: PathBuf) {
    let _ = PATH_OVERRIDE.set(path);
}

/// 1: 0.1.x. 2: roles, leases and review storage. 3: task dependencies.
/// 4: task ids are never reused (`AUTOINCREMENT`).
pub const SCHEMA_VERSION: i32 = 4;

/// The columns and constraints of `tasks`, written once for both the schema
/// script and the v4 rebuild, which creates the table under a temporary name.
///
/// `AUTOINCREMENT` makes SQLite remember the highest id it ever handed out.
/// Without it the next id is `max(id) + 1`, so removing the newest task let the
/// next `add` reuse its id, and a stale `#7` held by another agent could then
/// name a different task.
macro_rules! tasks_body {
    () => {
        r"(
  id INTEGER PRIMARY KEY AUTOINCREMENT,
  title TEXT NOT NULL,
  priority TEXT NOT NULL CHECK (priority IN ('low','medium','high','urgent')),
  status TEXT NOT NULL DEFAULT 'todo' CHECK (status IN ('backlog','todo','in_progress','review','done')),
  executor INTEGER REFERENCES agents(id),
  tags TEXT NOT NULL DEFAULT '[]' CHECK (json_valid(tags)),
  tests TEXT NOT NULL CHECK (json_valid(tests) AND json_array_length(tests) > 0),
  revision INTEGER NOT NULL DEFAULT 0 CHECK (revision >= 0),
  claimed_at TEXT,
  lease_expires_at TEXT,
  created_at TEXT NOT NULL DEFAULT (datetime('now')),
  updated_at TEXT NOT NULL DEFAULT (datetime('now')),
  CHECK (
    (executor IS NULL AND claimed_at IS NULL AND lease_expires_at IS NULL)
    OR
    (executor IS NOT NULL AND claimed_at IS NOT NULL AND lease_expires_at IS NOT NULL)
  ),
  CHECK (status != 'done' OR executor IS NULL)
)"
    };
}

pub const SCHEMA: &str = concat!(
    r"
CREATE TABLE IF NOT EXISTS agents (
  id INTEGER PRIMARY KEY,
  name TEXT NOT NULL UNIQUE,
  role TEXT NOT NULL DEFAULT 'developer' CHECK (role IN ('developer','reviewer')),
  created_at TEXT NOT NULL DEFAULT (datetime('now'))
);

CREATE TABLE IF NOT EXISTS tasks ",
    tasks_body!(),
    r";

CREATE TABLE IF NOT EXISTS review_history (
  id INTEGER PRIMARY KEY,
  task_id INTEGER NOT NULL REFERENCES tasks(id) ON DELETE CASCADE,
  revision INTEGER NOT NULL CHECK (revision > 0),
  decision TEXT NOT NULL CHECK (decision IN ('approved','changes_requested')),
  notes TEXT NOT NULL DEFAULT '',
  executor INTEGER REFERENCES agents(id) ON DELETE SET NULL,
  executor_name TEXT NOT NULL,
  created_at TEXT NOT NULL DEFAULT (datetime('now')),
  UNIQUE (task_id, revision)
);

CREATE TABLE IF NOT EXISTS acceptance_results (
  id INTEGER PRIMARY KEY,
  task_id INTEGER NOT NULL REFERENCES tasks(id) ON DELETE CASCADE,
  revision INTEGER NOT NULL CHECK (revision > 0),
  criterion_index INTEGER NOT NULL CHECK (criterion_index >= 0),
  criterion TEXT NOT NULL CHECK (json_valid(criterion)),
  result TEXT NOT NULL CHECK (result IN ('passed','failed')),
  evidence TEXT NOT NULL CHECK (length(trim(evidence)) > 0),
  executor INTEGER REFERENCES agents(id) ON DELETE SET NULL,
  executor_name TEXT NOT NULL,
  verified_at TEXT NOT NULL DEFAULT (datetime('now')),
  UNIQUE (task_id, revision, criterion_index)
);

-- `task_id` waits until `depends_on` is done. Deleting a task drops its own
-- edges, but a task that others still wait on cannot be deleted.
CREATE TABLE IF NOT EXISTS task_deps (
  task_id INTEGER NOT NULL REFERENCES tasks(id) ON DELETE CASCADE,
  depends_on INTEGER NOT NULL REFERENCES tasks(id) ON DELETE RESTRICT,
  PRIMARY KEY (task_id, depends_on),
  CHECK (task_id != depends_on)
);

CREATE INDEX IF NOT EXISTS review_history_task_revision
  ON review_history(task_id, revision);
CREATE INDEX IF NOT EXISTS acceptance_results_task_revision
  ON acceptance_results(task_id, revision, criterion_index);
CREATE INDEX IF NOT EXISTS task_deps_depends_on
  ON task_deps(depends_on);
"
);

/// Walk up from cwd looking for `.kanban/board.db`, like git looks for `.git`.
/// Closest match wins; does not merge with a parent `.kanban/`.
pub fn discover() -> Option<PathBuf> {
    let mut dir = std::env::current_dir().ok()?;
    loop {
        let candidate = dir.join(DB_DIR).join(DB_FILE);
        if candidate.exists() {
            return Some(candidate);
        }
        if !dir.pop() {
            return None;
        }
    }
}

fn open_at(path: &Path) -> Result<Connection> {
    let conn = Connection::open(path)?;
    // busy_timeout must be set first: setting journal_mode=WAL on a
    // brand-new file needs a brief exclusive lock, and any lock contention
    // during that pragma only benefits from busy_timeout's retry behavior
    // if busy_timeout was already active when it runs.
    conn.execute_batch(
        "PRAGMA busy_timeout=5000; PRAGMA journal_mode=WAL; PRAGMA foreign_keys=ON;",
    )?;
    Ok(conn)
}

/// Open the board for the current project. Fails with a clear error if `agent-kanban init`
/// hasn't been run (no `.kanban/` found walking up from cwd), unless a `--db
/// <path>` override is set, in which case that exact path is used directly.
pub fn open_existing() -> Result<Connection> {
    let path = match PATH_OVERRIDE.get() {
        Some(path) => {
            // `Connection::open` silently creates a missing file (SQLite's
            // default behavior), which would otherwise turn a typo'd --db
            // path into a confusing raw "no such table" error instead of a
            // clean one -- checked explicitly, matching the message shape
            // `discover()` already uses for the no-override case.
            if !path.exists() {
                bail!(
                    "database file {} not found; run `agent-kanban --db {} init` first",
                    path.display(),
                    path.display()
                );
            }
            path.clone()
        }
        None => discover().ok_or_else(|| {
            anyhow::anyhow!("not a kanban project (no .kanban/ found); run `agent-kanban init`")
        })?,
    };
    let mut conn = open_at(&path)?;
    migrate_schema(&mut conn)?;
    Ok(conn)
}

fn check_schema_version(conn: &Connection) -> Result<()> {
    let version = user_version(conn)?;
    if version > SCHEMA_VERSION {
        bail!(
            "this project's schema version ({version}) is newer than this build of \
             agent-kanban supports ({SCHEMA_VERSION}); upgrade agent-kanban"
        );
    }
    Ok(())
}

/// Create `.kanban/board.db` in the current directory and apply the schema,
/// or, if a `--db <path>` override is set, create the database at that exact
/// path instead (creating parent directories as needed).
pub fn init() -> Result<()> {
    const MAX_ATTEMPTS: u32 = 10;

    let path = if let Some(path) = PATH_OVERRIDE.get() {
        if let Some(parent) = path.parent().filter(|p| !p.as_os_str().is_empty()) {
            std::fs::create_dir_all(parent)?;
        }
        path.clone()
    } else {
        let dir = std::env::current_dir()?.join(DB_DIR);
        std::fs::create_dir_all(&dir)?;
        dir.join(DB_FILE)
    };

    // Converting a brand-new database file to WAL mode for the first time
    // needs a brief exclusive lock. If two `agent-kanban init` calls race in a
    // fresh directory, one can observe "database is locked" here even with
    // busy_timeout set and ordered first -- confirmed directly: racing two
    // concurrent `init` calls in a fresh directory produced this in ~30-40%
    // of trials regardless of pragma ordering, unlike every other guarded
    // operation in this codebase (claim/edit/remove/release), where
    // busy_timeout reliably covers ordinary lock contention. This appears
    // to be a real gap specific to the one-time WAL conversion pragma.
    // Since every statement here is idempotent (CREATE TABLE IF NOT
    // EXISTS), the whole operation is safe to retry, so do that a handful
    // of times with a short backoff rather than surface a spurious failure
    // for two agents harmlessly racing to set up the same project.
    //
    // `MAX_ATTEMPTS` retries all happen inside this `loop`, which
    // deliberately has no code after it: every arm either returns
    // (success, a non-lock error, or the final attempt's error however it
    // looks) or continues looping, so the loop itself always exits via
    // `return` and never falls through -- an earlier version tracked a
    // `last_err` and returned it after the loop "in case all attempts were
    // exhausted", but that trailing code was provably unreachable (the
    // final attempt, index MAX_ATTEMPTS - 1, always takes the unconditional
    // `return Err(e)` arm below since `attempt + 1 < MAX_ATTEMPTS` is false
    // for it), so it's been removed rather than kept as dead code.
    let mut attempt = 0;
    loop {
        match try_init(&path) {
            Ok(()) => return Ok(()),
            Err(e)
                if attempt + 1 < MAX_ATTEMPTS && e.to_string().contains("database is locked") =>
            {
                attempt += 1;
                std::thread::sleep(std::time::Duration::from_millis(u64::from(20 * attempt)));
            }
            Err(e) => return Err(e),
        }
    }
}

fn try_init(path: &Path) -> Result<()> {
    let mut conn = open_at(path)?;
    check_schema_version(&conn)?;
    conn.execute_batch(SCHEMA)?;
    migrate_schema(&mut conn)?;
    Ok(())
}

fn user_version(conn: &Connection) -> Result<i32> {
    Ok(conn.query_row("PRAGMA user_version", [], |row| row.get(0))?)
}

fn migrate_schema(conn: &mut Connection) -> Result<()> {
    check_schema_version(conn)?;
    if user_version(conn)? == SCHEMA_VERSION {
        return Ok(());
    }

    // Foreign keys are off for the whole migration: the v4 rebuild drops
    // `tasks`, and with them on that would cascade into every review, result
    // and dependency row that points at it. The pragma does nothing inside a
    // transaction, so it brackets the transaction instead. A failed migration
    // reports its own error, not the one from switching the keys back on.
    conn.execute_batch("PRAGMA foreign_keys=OFF;")?;
    let migrated = migrate_in_transaction(conn);
    let restored = conn.execute_batch("PRAGMA foreign_keys=ON;");
    migrated?;
    Ok(restored?)
}

fn migrate_in_transaction(conn: &mut Connection) -> Result<()> {
    let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
    let version = user_version(&tx)?;
    if version > SCHEMA_VERSION {
        bail!(
            "this project's schema version ({version}) is newer than this build of \
             agent-kanban supports ({SCHEMA_VERSION}); upgrade agent-kanban"
        );
    }
    if !table_exists(&tx, "agents")? || !table_exists(&tx, "tasks")? {
        bail!("database is not initialized; run `agent-kanban init`");
    }
    if version == SCHEMA_VERSION {
        tx.commit()?;
        return Ok(());
    }

    // Each step runs only for boards older than the version that introduced
    // it: the v1 data fix below resets every `review` task, which would wreck
    // a live v2+ board if it ran again.
    if version < 2 {
        migrate_v1_to_v2(&tx)?;
    }
    // Every later table (review storage in v2, `task_deps` in v3) is created
    // by the idempotent schema script.
    tx.execute_batch(SCHEMA)?;
    rebuild_tasks_with_autoincrement(&tx)?;
    tx.execute_batch(&format!("PRAGMA user_version = {SCHEMA_VERSION};"))?;
    tx.commit()?;
    Ok(())
}

/// v1 had no roles, leases or revisions: add the columns, clear owners that
/// make no sense in the new model, and give legacy claims a one-hour lease.
fn migrate_v1_to_v2(tx: &Connection) -> Result<()> {
    if !column_exists(tx, "agents", "role")? {
        tx.execute_batch(
            "ALTER TABLE agents ADD COLUMN role TEXT NOT NULL DEFAULT 'developer'
             CHECK (role IN ('developer','reviewer'));",
        )?;
    }
    if !column_exists(tx, "tasks", "revision")? {
        tx.execute_batch(
            "ALTER TABLE tasks ADD COLUMN revision INTEGER NOT NULL DEFAULT 0
             CHECK (revision >= 0);",
        )?;
    }
    if !column_exists(tx, "tasks", "claimed_at")? {
        tx.execute_batch("ALTER TABLE tasks ADD COLUMN claimed_at TEXT;")?;
    }
    if !column_exists(tx, "tasks", "lease_expires_at")? {
        tx.execute_batch("ALTER TABLE tasks ADD COLUMN lease_expires_at TEXT;")?;
    }

    tx.execute(
        "UPDATE tasks
         SET status = CASE WHEN status = 'review' THEN 'in_progress' ELSE status END,
             executor = NULL,
             claimed_at = NULL,
             lease_expires_at = NULL
         WHERE status = 'review'
            OR (status != 'in_progress' AND executor IS NOT NULL)",
        [],
    )?;
    tx.execute(
        "UPDATE tasks
         SET claimed_at = COALESCE(claimed_at, datetime('now')),
             lease_expires_at = COALESCE(
               lease_expires_at,
               datetime('now', '+3600 seconds')
             )
         WHERE executor IS NOT NULL AND status = 'in_progress'",
        [],
    )?;
    Ok(())
}

/// v4: give `tasks.id` `AUTOINCREMENT`. SQLite cannot add that to an existing
/// column, so the table is rebuilt the way SQLite documents
/// (<https://www.sqlite.org/lang_altertable.html#otheralter>): create the new
/// table, copy every row with its id, drop the old table, rename the new one.
/// Foreign keys must be off, see `migrate_schema`. The copy starts the id
/// counter at the highest id in use, so no live id is handed out again; ids
/// freed before the upgrade are gone for good and cannot be told apart.
///
/// A board is recognised by the shape of its table, not by its version: a
/// brand-new board is stamped 0 yet already has the v4 table.
fn rebuild_tasks_with_autoincrement(tx: &Connection) -> Result<()> {
    if table_is_autoincrement(tx, "tasks")? {
        return Ok(());
    }
    // The columns are named, not `SELECT *`: a table that came through the v1
    // upgrade has `revision`, `claimed_at` and `lease_expires_at` last.
    tx.execute_batch(&format!(
        "CREATE TABLE tasks_new {};
         INSERT INTO tasks_new (id, title, priority, status, executor, tags, tests, revision,
                                claimed_at, lease_expires_at, created_at, updated_at)
           SELECT id, title, priority, status, executor, tags, tests, revision,
                  claimed_at, lease_expires_at, created_at, updated_at
           FROM tasks;
         DROP TABLE tasks;
         ALTER TABLE tasks_new RENAME TO tasks;",
        tasks_body!()
    ))?;
    Ok(())
}

fn table_is_autoincrement(conn: &Connection, table: &str) -> Result<bool> {
    let sql: String = conn.query_row(
        "SELECT sql FROM sqlite_schema WHERE type = 'table' AND name = ?1",
        [table],
        |row| row.get(0),
    )?;
    Ok(sql.to_ascii_uppercase().contains("AUTOINCREMENT"))
}

fn table_exists(conn: &Connection, table: &str) -> Result<bool> {
    Ok(conn
        .query_row(
            "SELECT 1 FROM sqlite_schema WHERE type = 'table' AND name = ?1",
            [table],
            |_| Ok(()),
        )
        .optional()?
        .is_some())
}

fn column_exists(conn: &Connection, table: &str, column: &str) -> Result<bool> {
    let mut stmt = conn.prepare(&format!("PRAGMA table_info({table})"))?;
    let columns = stmt.query_map([], |row| row.get::<_, String>(1))?;
    for name in columns {
        if name? == column {
            return Ok(true);
        }
    }
    Ok(false)
}

#[cfg(test)]
mod tests {
    use super::*;
    use rusqlite::types::Value;

    #[test]
    fn schema_version_at_current_is_accepted() {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(&format!("PRAGMA user_version = {SCHEMA_VERSION};"))
            .unwrap();
        check_schema_version(&conn).unwrap();
    }

    #[test]
    fn schema_version_zero_legacy_is_accepted() {
        // A fresh in-memory connection defaults to user_version = 0, matching
        // a database created before schema versioning existed.
        let conn = Connection::open_in_memory().unwrap();
        check_schema_version(&conn).unwrap();
    }

    #[test]
    fn schema_version_newer_than_supported_is_rejected() {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(&format!("PRAGMA user_version = {};", SCHEMA_VERSION + 1))
            .unwrap();
        let err = check_schema_version(&conn).unwrap_err();
        assert!(err.to_string().contains("newer than this build"));
    }

    #[test]
    fn version_one_migration_adds_roles_leases_and_review_storage() {
        let mut conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "PRAGMA foreign_keys=ON;
             CREATE TABLE agents (
               id INTEGER PRIMARY KEY,
               name TEXT NOT NULL UNIQUE,
               created_at TEXT NOT NULL DEFAULT (datetime('now'))
             );
             CREATE TABLE tasks (
               id INTEGER PRIMARY KEY,
               title TEXT NOT NULL,
               priority TEXT NOT NULL,
               status TEXT NOT NULL,
               executor INTEGER REFERENCES agents(id),
               tags TEXT NOT NULL DEFAULT '[]',
               tests TEXT NOT NULL,
               created_at TEXT NOT NULL DEFAULT (datetime('now')),
               updated_at TEXT NOT NULL DEFAULT (datetime('now'))
             );
             INSERT INTO agents (id, name) VALUES (1, 'legacy-agent');
             INSERT INTO tasks (id, title, priority, status, executor, tests)
               VALUES (1, 'active', 'high', 'in_progress', 1, '[\"criterion\"]');
             INSERT INTO tasks (id, title, priority, status, executor, tests)
               VALUES (2, 'finished', 'low', 'done', 1, '[\"criterion\"]');
             INSERT INTO tasks (id, title, priority, status, executor, tests)
               VALUES (3, 'awaiting review', 'medium', 'review', 1, '[\"criterion\"]');
             INSERT INTO tasks (id, title, priority, status, executor, tests)
               VALUES (4, 'unowned review', 'medium', 'review', NULL, '[\"criterion\"]');
             PRAGMA user_version = 1;",
        )
        .unwrap();

        migrate_schema(&mut conn).unwrap();
        migrate_schema(&mut conn).unwrap();

        let version: i32 = conn
            .query_row("PRAGMA user_version", [], |row| row.get(0))
            .unwrap();
        assert_eq!(version, SCHEMA_VERSION);
        let role: String = conn
            .query_row("SELECT role FROM agents WHERE id = 1", [], |row| row.get(0))
            .unwrap();
        assert_eq!(role, "developer");

        let (status, executor, active): (String, Option<i64>, bool) = conn
            .query_row(
                "SELECT status, executor, lease_expires_at > datetime('now')
                 FROM tasks WHERE id = 1",
                [],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .unwrap();
        assert_eq!(status, "in_progress");
        assert_eq!(executor, Some(1));
        assert!(active);

        let (status, executor): (String, Option<i64>) = conn
            .query_row(
                "SELECT status, executor FROM tasks WHERE id = 2",
                [],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap();
        assert_eq!(status, "done");
        assert_eq!(executor, None);
        let (status, executor): (String, Option<i64>) = conn
            .query_row(
                "SELECT status, executor FROM tasks WHERE id = 3",
                [],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap();
        assert_eq!(status, "in_progress");
        assert_eq!(executor, None);
        let status: String = conn
            .query_row("SELECT status FROM tasks WHERE id = 4", [], |row| {
                row.get(0)
            })
            .unwrap();
        assert_eq!(status, "in_progress");
        assert!(table_exists(&conn, "review_history").unwrap());
        assert!(table_exists(&conn, "acceptance_results").unwrap());
        assert!(table_exists(&conn, "task_deps").unwrap());
        // The v1 table has its added columns last; the rebuild must not care,
        // and must keep every row under its own id.
        assert!(table_is_autoincrement(&conn, "tasks").unwrap());
        let ids = dump(&conn, "SELECT id, title FROM tasks ORDER BY id");
        assert_eq!(ids.len(), 4);
        assert_eq!(ids[3][1], Value::Text("unowned review".into()));
        assert!(foreign_key_problems(&conn).is_empty());
    }

    /// The schema exactly as version 2 shipped it (no `task_deps`).
    const V2_SCHEMA: &str = r"
        CREATE TABLE agents (
          id INTEGER PRIMARY KEY,
          name TEXT NOT NULL UNIQUE,
          role TEXT NOT NULL DEFAULT 'developer' CHECK (role IN ('developer','reviewer')),
          created_at TEXT NOT NULL DEFAULT (datetime('now'))
        );
        CREATE TABLE tasks (
          id INTEGER PRIMARY KEY,
          title TEXT NOT NULL,
          priority TEXT NOT NULL CHECK (priority IN ('low','medium','high','urgent')),
          status TEXT NOT NULL DEFAULT 'todo' CHECK (status IN ('backlog','todo','in_progress','review','done')),
          executor INTEGER REFERENCES agents(id),
          tags TEXT NOT NULL DEFAULT '[]' CHECK (json_valid(tags)),
          tests TEXT NOT NULL CHECK (json_valid(tests) AND json_array_length(tests) > 0),
          revision INTEGER NOT NULL DEFAULT 0 CHECK (revision >= 0),
          claimed_at TEXT,
          lease_expires_at TEXT,
          created_at TEXT NOT NULL DEFAULT (datetime('now')),
          updated_at TEXT NOT NULL DEFAULT (datetime('now')),
          CHECK (
            (executor IS NULL AND claimed_at IS NULL AND lease_expires_at IS NULL)
            OR
            (executor IS NOT NULL AND claimed_at IS NOT NULL AND lease_expires_at IS NOT NULL)
          ),
          CHECK (status != 'done' OR executor IS NULL)
        );
        CREATE TABLE review_history (
          id INTEGER PRIMARY KEY,
          task_id INTEGER NOT NULL REFERENCES tasks(id) ON DELETE CASCADE,
          revision INTEGER NOT NULL CHECK (revision > 0),
          decision TEXT NOT NULL CHECK (decision IN ('approved','changes_requested')),
          notes TEXT NOT NULL DEFAULT '',
          executor INTEGER REFERENCES agents(id) ON DELETE SET NULL,
          executor_name TEXT NOT NULL,
          created_at TEXT NOT NULL DEFAULT (datetime('now')),
          UNIQUE (task_id, revision)
        );
        CREATE TABLE acceptance_results (
          id INTEGER PRIMARY KEY,
          task_id INTEGER NOT NULL REFERENCES tasks(id) ON DELETE CASCADE,
          revision INTEGER NOT NULL CHECK (revision > 0),
          criterion_index INTEGER NOT NULL CHECK (criterion_index >= 0),
          criterion TEXT NOT NULL CHECK (json_valid(criterion)),
          result TEXT NOT NULL CHECK (result IN ('passed','failed')),
          evidence TEXT NOT NULL CHECK (length(trim(evidence)) > 0),
          executor INTEGER REFERENCES agents(id) ON DELETE SET NULL,
          executor_name TEXT NOT NULL,
          verified_at TEXT NOT NULL DEFAULT (datetime('now')),
          UNIQUE (task_id, revision, criterion_index)
        );
        PRAGMA user_version = 2;";

    /// Regression test for the v1 data fix (it resets every `review` task to
    /// `in_progress`): that fix must run only for boards older than v2, so a
    /// v2 board keeps its review state and its recorded results.
    #[test]
    fn version_two_migration_adds_task_deps_and_keeps_review_state() {
        let mut conn = Connection::open_in_memory().unwrap();
        conn.execute_batch("PRAGMA foreign_keys=ON;").unwrap();
        conn.execute_batch(V2_SCHEMA).unwrap();
        conn.execute_batch(
            "INSERT INTO agents (id, name, role) VALUES (1, 'dev', 'developer'), (2, 'rev', 'reviewer');
             INSERT INTO tasks (id, title, priority, status, tests, revision)
               VALUES (1, 'in review', 'high', 'review', '[{\"describe\":\"d\",\"input\":\"i\",\"output\":\"o\"}]', 1);
             INSERT INTO acceptance_results
               (task_id, revision, criterion_index, criterion, result, evidence, executor, executor_name)
               VALUES (1, 1, 0, '{}', 'passed', 'cargo test', 1, 'dev');
             INSERT INTO tasks (id, title, priority, status, tests, executor, claimed_at, lease_expires_at, revision)
               VALUES (2, 'being reviewed', 'low', 'review', '[\"c\"]', 2,
                       datetime('now'), datetime('now', '+1 hour'), 1);
             INSERT INTO tasks (id, title, priority, status, tests)
               VALUES (3, 'open', 'low', 'todo', '[\"c\"]');",
        )
        .unwrap();
        assert!(!table_exists(&conn, "task_deps").unwrap());

        migrate_schema(&mut conn).unwrap();
        migrate_schema(&mut conn).unwrap();

        let version: i32 = conn
            .query_row("PRAGMA user_version", [], |row| row.get(0))
            .unwrap();
        assert_eq!(version, SCHEMA_VERSION);
        assert!(table_exists(&conn, "task_deps").unwrap());

        let status_of = |id: i64| -> (String, Option<i64>) {
            conn.query_row(
                "SELECT status, executor FROM tasks WHERE id = ?1",
                [id],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap()
        };
        assert_eq!(status_of(1), ("review".to_string(), None));
        assert_eq!(status_of(2), ("review".to_string(), Some(2)));
        assert_eq!(status_of(3), ("todo".to_string(), None));
        let results: i64 = conn
            .query_row("SELECT COUNT(*) FROM acceptance_results", [], |row| {
                row.get(0)
            })
            .unwrap();
        assert_eq!(results, 1);

        // The new table is usable and enforces its constraints.
        conn.execute(
            "INSERT INTO task_deps (task_id, depends_on) VALUES (3, 1)",
            [],
        )
        .unwrap();
        assert!(
            conn.execute(
                "INSERT INTO task_deps (task_id, depends_on) VALUES (3, 3)",
                []
            )
            .is_err(),
            "a task cannot depend on itself"
        );
        assert!(
            conn.execute("DELETE FROM tasks WHERE id = 1", []).is_err(),
            "a prerequisite cannot be deleted while a task still waits on it"
        );
        assert!(table_is_autoincrement(&conn, "tasks").unwrap());
        assert!(foreign_key_problems(&conn).is_empty());
    }

    /// What version 3 added to version 2: the dependency table.
    const V3_DEPS: &str = r"
        CREATE TABLE task_deps (
          task_id INTEGER NOT NULL REFERENCES tasks(id) ON DELETE CASCADE,
          depends_on INTEGER NOT NULL REFERENCES tasks(id) ON DELETE RESTRICT,
          PRIMARY KEY (task_id, depends_on),
          CHECK (task_id != depends_on)
        );
        PRAGMA user_version = 3;";

    /// A version 3 board, the last one with a plain `INTEGER PRIMARY KEY`: task
    /// ids 4 and 6 were removed, 7 is the newest, and every table that points
    /// at `tasks` has a row.
    fn board_v3() -> Connection {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch("PRAGMA foreign_keys=ON;").unwrap();
        conn.execute_batch(V2_SCHEMA).unwrap();
        conn.execute_batch(V3_DEPS).unwrap();
        conn.execute_batch(
            "INSERT INTO agents (id, name, role) VALUES (1, 'dev', 'developer'), (2, 'rev', 'reviewer');
             INSERT INTO tasks (id, title, priority, status, tests, revision) VALUES
               (1, 'base', 'high', 'done', '[\"c\"]', 1),
               (2, 'next', 'low', 'todo', '[\"c\"]', 0),
               (3, 'in review', 'medium', 'review', '[\"c\"]', 1),
               (7, 'newest', 'urgent', 'backlog', '[\"c\"]', 0);
             INSERT INTO tasks (id, title, priority, status, tests, executor, claimed_at, lease_expires_at)
               VALUES (5, 'held', 'low', 'in_progress', '[\"c\"]', 1,
                       datetime('now'), datetime('now', '+1 hour'));
             INSERT INTO review_history (task_id, revision, decision, notes, executor, executor_name)
               VALUES (1, 1, 'approved', 'ok', 2, 'rev');
             INSERT INTO acceptance_results
               (task_id, revision, criterion_index, criterion, result, evidence, executor, executor_name)
               VALUES (1, 1, 0, '{}', 'passed', 'cargo test', 1, 'dev'),
                      (3, 1, 0, '{}', 'failed', 'red', 1, 'dev');
             INSERT INTO task_deps (task_id, depends_on) VALUES (2, 1), (7, 5);",
        )
        .unwrap();
        conn
    }

    /// Every row of a query, as SQLite stores it.
    fn dump(conn: &Connection, sql: &str) -> Vec<Vec<Value>> {
        let mut stmt = conn.prepare(sql).unwrap();
        let columns = stmt.column_count();
        stmt.query_map([], |row| (0..columns).map(|i| row.get(i)).collect())
            .unwrap()
            .map(Result::unwrap)
            .collect()
    }

    /// Everything the board holds, table by table, for before/after comparisons.
    fn contents(conn: &Connection) -> Vec<Vec<Vec<Value>>> {
        [
            "SELECT * FROM agents ORDER BY id",
            "SELECT id, title, priority, status, executor, tags, tests, revision,
                    claimed_at, lease_expires_at, created_at, updated_at
             FROM tasks ORDER BY id",
            "SELECT * FROM review_history ORDER BY id",
            "SELECT * FROM acceptance_results ORDER BY id",
            "SELECT * FROM task_deps ORDER BY task_id, depends_on",
        ]
        .map(|sql| dump(conn, sql))
        .to_vec()
    }

    /// Rows of `PRAGMA foreign_key_check`: references that point at nothing.
    fn foreign_key_problems(conn: &Connection) -> Vec<Vec<Value>> {
        dump(conn, "PRAGMA foreign_key_check")
    }

    fn pragma(conn: &Connection, name: &str) -> i64 {
        conn.query_row(&format!("PRAGMA {name}"), [], |row| row.get(0))
            .unwrap()
    }

    /// Insert a task and return the id SQLite gave it.
    fn add_task(conn: &Connection) -> i64 {
        conn.execute(
            "INSERT INTO tasks (title, priority, tests) VALUES ('n', 'low', '[\"c\"]')",
            [],
        )
        .unwrap();
        conn.last_insert_rowid()
    }

    #[test]
    fn a_new_board_never_hands_out_an_id_twice() {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(SCHEMA).unwrap();
        assert_eq!(add_task(&conn), 1);
        assert_eq!(add_task(&conn), 2);
        conn.execute("DELETE FROM tasks WHERE id = 2", []).unwrap();
        assert_eq!(add_task(&conn), 3, "the newest id was removed, not freed");
        conn.execute("DELETE FROM tasks", []).unwrap();
        assert_eq!(add_task(&conn), 4, "even an empty board counts on");
    }

    #[test]
    fn version_three_upgrade_rebuilds_tasks_and_keeps_every_row() {
        let mut conn = board_v3();
        assert!(!table_is_autoincrement(&conn, "tasks").unwrap());
        let before = contents(&conn);
        assert_eq!(before[1].len(), 5, "the fixture has five tasks");

        migrate_schema(&mut conn).unwrap();
        migrate_schema(&mut conn).unwrap();

        assert_eq!(pragma(&conn, "user_version"), i64::from(SCHEMA_VERSION));
        assert!(table_is_autoincrement(&conn, "tasks").unwrap());
        assert_eq!(contents(&conn), before);
        assert!(foreign_key_problems(&conn).is_empty());
        assert_eq!(pragma(&conn, "foreign_keys"), 1, "switched back on");
        assert!(!table_exists(&conn, "tasks_new").unwrap());
    }

    #[test]
    fn version_three_upgrade_counts_on_from_the_highest_id() {
        let mut conn = board_v3();
        migrate_schema(&mut conn).unwrap();

        assert_eq!(add_task(&conn), 8);
        conn.execute("DELETE FROM tasks WHERE id = 8", []).unwrap();
        assert_eq!(add_task(&conn), 9, "a removed id is not handed out again");
    }

    #[test]
    fn version_three_upgrade_keeps_the_foreign_keys_working() {
        let mut conn = board_v3();
        migrate_schema(&mut conn).unwrap();

        assert!(
            conn.execute("DELETE FROM tasks WHERE id = 1", []).is_err(),
            "task 2 still waits on task 1"
        );
        conn.execute("DELETE FROM tasks WHERE id = 3", []).unwrap();
        let results = dump(&conn, "SELECT task_id FROM acceptance_results");
        assert_eq!(
            results,
            [[Value::Integer(1)]],
            "task 3's result went with it"
        );
        assert!(
            conn.execute(
                "UPDATE tasks SET executor = 99, claimed_at = datetime('now'),
                        lease_expires_at = datetime('now') WHERE id = 2",
                [],
            )
            .is_err(),
            "an owner that does not exist is refused"
        );
    }

    /// A table that cannot be copied (here a row the new constraints refuse,
    /// as a board that came through the v1 upgrade could hold) stops the whole
    /// upgrade: nothing is half-done and the foreign keys come back on.
    #[test]
    fn a_failed_rebuild_changes_nothing() {
        let mut conn = Connection::open_in_memory().unwrap();
        conn.execute_batch("PRAGMA foreign_keys=ON;").unwrap();
        let lenient =
            V2_SCHEMA.replace("CHECK (status != 'done' OR executor IS NULL)", "CHECK (1)");
        assert_ne!(lenient, V2_SCHEMA, "the fixture must drop the constraint");
        conn.execute_batch(&lenient).unwrap();
        conn.execute_batch(
            "INSERT INTO agents (id, name) VALUES (1, 'dev');
             INSERT INTO tasks (id, title, priority, status, tests, executor, claimed_at, lease_expires_at)
               VALUES (1, 'finished but owned', 'low', 'done', '[\"c\"]', 1,
                       datetime('now'), datetime('now'));",
        )
        .unwrap();

        let err = migrate_schema(&mut conn).unwrap_err();

        assert!(err.to_string().contains("CHECK constraint failed"), "{err}");
        assert_eq!(pragma(&conn, "user_version"), 2);
        assert!(!table_is_autoincrement(&conn, "tasks").unwrap());
        assert!(!table_exists(&conn, "tasks_new").unwrap());
        assert!(
            !table_exists(&conn, "task_deps").unwrap(),
            "rolled back too"
        );
        assert_eq!(dump(&conn, "SELECT id FROM tasks"), [[Value::Integer(1)]]);
        assert_eq!(pragma(&conn, "foreign_keys"), 1, "switched back on");
    }
}
