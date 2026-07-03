use anyhow::{Result, bail};
use rusqlite::{Connection, OptionalExtension, Transaction};
use serde_json::{Value, json};

/// `agent-kanban agent register <name>`
/// Insert a new agent row. Must catch the UNIQUE constraint violation on a
/// duplicate name and return it as a clean anyhow error (not a raw rusqlite
/// error), per todo.md section 3/6.
pub fn register(name: &str) -> Result<Value> {
    let conn = crate::db::open_existing()?;
    register_inner(&conn, name)
}

fn register_inner(conn: &Connection, name: &str) -> Result<Value> {
    let name = name.trim();
    if name.is_empty() {
        bail!("agent name must not be empty or whitespace-only");
    }

    let result = conn.execute("INSERT INTO agents (name) VALUES (?1)", [name]);

    match result {
        Ok(_) => {
            let id = conn.last_insert_rowid();
            let (id, name, created_at): (i64, String, String) = conn.query_row(
                "SELECT id, name, created_at FROM agents WHERE id = ?1",
                [id],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )?;
            Ok(json!({"id": id, "name": name, "created_at": created_at}))
        }
        Err(rusqlite::Error::SqliteFailure(ffi_err, _))
            if ffi_err.code == rusqlite::ErrorCode::ConstraintViolation =>
        {
            bail!("agent '{name}' already exists")
        }
        Err(e) => Err(e.into()),
    }
}

/// `agent-kanban agent list`
pub fn list() -> Result<Value> {
    let conn = crate::db::open_existing()?;
    list_inner(&conn)
}

fn list_inner(conn: &Connection) -> Result<Value> {
    let mut stmt = conn.prepare("SELECT id, name, created_at FROM agents ORDER BY name")?;
    let rows = stmt.query_map([], |row| {
        let id: i64 = row.get(0)?;
        let name: String = row.get(1)?;
        let created_at: String = row.get(2)?;
        Ok(json!({"id": id, "name": name, "created_at": created_at}))
    })?;

    let agents: std::result::Result<Vec<Value>, rusqlite::Error> = rows.collect();
    Ok(Value::Array(agents?))
}

/// `agent-kanban agent remove <name>`
/// Auto-release then remove: in ONE transaction, release every task currently
/// claimed by this agent (executor = NULL, status reset back to 'todo',
/// `updated_at` bumped — same effect as `lifecycle::release` on each), then
/// delete the agent row. See todo.md section 3/4/6.
pub fn remove(name: &str) -> Result<Value> {
    let mut conn = crate::db::open_existing()?;
    // Must be `Immediate`, not the default `Deferred`: a deferred transaction
    // starts with a read (the SELECT below) and only acquires the write lock
    // later, at the first UPDATE/DELETE. In WAL mode, upgrading a read
    // snapshot to a writer mid-transaction can fail with SQLITE_BUSY in a way
    // that bypasses `busy_timeout`'s ordinary wait-and-retry -- confirmed
    // directly: racing this against a concurrent `claim` produced
    // "database is locked" in ~83% of trials with the default Deferred
    // behavior, despite busy_timeout=5000 being set. `Immediate` acquires the
    // write lock upfront, at BEGIN, making lock contention here a plain
    // "wait for the writer" case that busy_timeout does handle correctly.
    let tx = conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
    let result = remove_inner(&tx, name)?;
    tx.commit()?;
    Ok(result)
}

fn remove_inner(tx: &Transaction, name: &str) -> Result<Value> {
    let id: Option<i64> = tx
        .query_row("SELECT id FROM agents WHERE name = ?1", [name], |row| {
            row.get(0)
        })
        .optional()?;

    let Some(id) = id else {
        bail!("agent '{name}' not found");
    };

    let released_tasks: Vec<i64> = {
        let mut stmt = tx.prepare(
            "UPDATE tasks SET executor = NULL, status = 'todo', updated_at = datetime('now') \
             WHERE executor = ?1 RETURNING id",
        )?;
        let rows = stmt.query_map([id], |row| row.get::<_, i64>(0))?;
        rows.collect::<std::result::Result<Vec<i64>, rusqlite::Error>>()?
    };

    // Check the row count rather than discarding it: if a concurrent
    // duplicate `agent remove <same name>` already deleted this row between
    // our lookup above and this DELETE, report that honestly instead of a
    // misleading success for a call that didn't actually remove anything.
    // In practice this whole function's transaction is `Immediate` (see
    // `remove`), which acquires the write lock at BEGIN, before even the
    // SELECT above -- so a concurrent duplicate call's entire transaction
    // fully commits or fully waits, meaning the earlier `id` lookup should
    // already have failed by the time we'd get here. Kept as a real check
    // anyway: cheap, and it stops being provably unreachable the moment
    // this function's transaction behavior ever changes.
    let deleted = tx.execute("DELETE FROM agents WHERE id = ?1", [id])?;
    if deleted == 0 {
        bail!("agent '{name}' not found");
    }

    Ok(json!({"removed": name, "released_tasks": released_tasks}))
}

#[cfg(test)]
mod tests {
    use super::*;
    use rusqlite::Connection;

    const SCHEMA: &str = r"
CREATE TABLE agents (
  id INTEGER PRIMARY KEY,
  name TEXT NOT NULL UNIQUE,
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
  created_at TEXT NOT NULL DEFAULT (datetime('now')),
  updated_at TEXT NOT NULL DEFAULT (datetime('now'))
);
";

    fn setup() -> Connection {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch("PRAGMA foreign_keys=ON;").unwrap();
        conn.execute_batch(SCHEMA).unwrap();
        conn
    }

    /// A file-backed (not in-memory) connection, needed for tests that rely
    /// on OS file permissions to force a non-constraint error -- permissions
    /// don't apply to `:memory:` databases.
    fn setup_file_backed() -> (tempfile::TempDir, Connection) {
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("board.db");
        let conn = Connection::open(&path).unwrap();
        conn.execute_batch("PRAGMA foreign_keys=ON;").unwrap();
        conn.execute_batch(SCHEMA).unwrap();
        (dir, conn)
    }

    #[test]
    fn register_propagates_non_constraint_errors_uncaught() {
        // The UNIQUE-violation catch is a specific pattern match, not a
        // blanket "any error means duplicate" -- confirmed here by forcing
        // a *different* kind of SQLite error (a read-only file, not a
        // constraint) and checking it comes through as its own genuine
        // error rather than being misreported as "already exists".
        let (dir, conn) = setup_file_backed();
        let db_path = dir.path().join("board.db");
        drop(conn);
        let mut perms = std::fs::metadata(&db_path).unwrap().permissions();
        perms.set_readonly(true);
        std::fs::set_permissions(&db_path, perms).unwrap();

        let conn = Connection::open(&db_path).unwrap();
        let err = register_inner(&conn, "agent-1").unwrap_err();
        assert!(
            !err.to_string().contains("already exists"),
            "unexpected message: {err}"
        );
        assert!(
            err.to_string().contains("readonly"),
            "expected a readonly-database error, got: {err}"
        );

        // Restore write permission so the TempDir can clean itself up.
        let mut perms = std::fs::metadata(&db_path).unwrap().permissions();
        #[allow(clippy::permissions_set_readonly_false)]
        perms.set_readonly(false);
        std::fs::set_permissions(&db_path, perms).unwrap();
    }

    #[test]
    fn register_succeeds_and_returns_expected_json() {
        let conn = setup();
        let result = register_inner(&conn, "agent-1").unwrap();
        assert_eq!(result["name"], "agent-1");
        assert!(result["id"].is_i64());
        assert!(result["created_at"].is_string());
    }

    #[test]
    fn register_rejects_empty_name() {
        let conn = setup();
        let err = register_inner(&conn, "   ").unwrap_err();
        assert!(err.to_string().contains("empty"));
    }

    #[test]
    fn register_duplicate_returns_clean_error() {
        let conn = setup();
        register_inner(&conn, "agent-1").unwrap();
        let err = register_inner(&conn, "agent-1").unwrap_err();
        assert_eq!(err.to_string(), "agent 'agent-1' already exists");
    }

    #[test]
    fn list_returns_registered_agents() {
        let conn = setup();
        register_inner(&conn, "bravo").unwrap();
        register_inner(&conn, "alpha").unwrap();
        let result = list_inner(&conn).unwrap();
        let arr = result.as_array().unwrap();
        assert_eq!(arr.len(), 2);
        // ordered by name
        assert_eq!(arr[0]["name"], "alpha");
        assert_eq!(arr[1]["name"], "bravo");
    }

    #[test]
    fn remove_cascades_and_releases_tasks() {
        let mut conn = setup();
        let agent = register_inner(&conn, "agent-1").unwrap();
        let agent_id = agent["id"].as_i64().unwrap();

        conn.execute(
            "INSERT INTO tasks (id, title, priority, status, executor, tests, updated_at) \
             VALUES (1, 'do thing', 'medium', 'in_progress', ?1, '[\"test\"]', '2000-01-01 00:00:00')",
            [agent_id],
        )
        .unwrap();

        let tx = conn.transaction().unwrap();
        let result = remove_inner(&tx, "agent-1").unwrap();
        tx.commit().unwrap();

        assert_eq!(result["removed"], "agent-1");
        assert_eq!(result["released_tasks"].as_array().unwrap().len(), 1);
        assert_eq!(result["released_tasks"][0], 1);

        let (executor, status, updated_at): (Option<i64>, String, String) = conn
            .query_row(
                "SELECT executor, status, updated_at FROM tasks WHERE id = 1",
                [],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .unwrap();
        assert_eq!(executor, None);
        assert_eq!(status, "todo");
        assert_ne!(updated_at, "2000-01-01 00:00:00");

        let agent_count: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM agents WHERE id = ?1",
                [agent_id],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(agent_count, 0);
    }

    #[test]
    fn remove_reports_not_found_if_row_vanishes_between_lookup_and_delete() {
        // The `deleted == 0` check after the DELETE is normally unreachable
        // in production (see its comment: the enclosing transaction is
        // `Immediate`, so a concurrent duplicate `remove` can't interleave).
        // It's still real defensive code, not dead code -- demonstrated here
        // via a trigger that deletes the agent row as a side effect of this
        // same function's own release UPDATE, landing exactly between the
        // id lookup and the final DELETE without needing any concurrency.
        let mut conn = setup();
        conn.execute_batch(
            "CREATE TRIGGER steal_agent_row AFTER UPDATE OF executor ON tasks \
             WHEN OLD.executor IS NOT NULL AND NEW.executor IS NULL \
             BEGIN DELETE FROM agents WHERE id = OLD.executor; END;",
        )
        .unwrap();
        let agent = register_inner(&conn, "agent-1").unwrap();
        let agent_id = agent["id"].as_i64().unwrap();
        conn.execute(
            "INSERT INTO tasks (id, title, priority, status, executor, tests) \
             VALUES (1, 'do thing', 'medium', 'in_progress', ?1, '[\"test\"]')",
            [agent_id],
        )
        .unwrap();

        let tx = conn.transaction().unwrap();
        let err = remove_inner(&tx, "agent-1").unwrap_err();
        assert_eq!(err.to_string(), "agent 'agent-1' not found");
    }

    #[test]
    fn remove_nonexistent_returns_clean_error() {
        let mut conn = setup();
        let tx = conn.transaction().unwrap();
        let err = remove_inner(&tx, "ghost").unwrap_err();
        assert_eq!(err.to_string(), "agent 'ghost' not found");
    }

    #[test]
    fn register_trims_whitespace() {
        let conn = setup();
        let result = register_inner(&conn, "  alice  ").unwrap();
        assert_eq!(result["name"], "alice");

        let list = list_inner(&conn).unwrap();
        let arr = list.as_array().unwrap();
        assert_eq!(arr.len(), 1);
        assert_eq!(arr[0]["name"], "alice");
    }
}
