use anyhow::{Result, bail};
use rusqlite::{Connection, OptionalExtension, Transaction};

/// What an agent is allowed to do: developers claim and submit work,
/// reviewers claim submitted work and decide on it.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Role {
    Developer,
    Reviewer,
}

impl Role {
    pub fn parse(role: &str) -> Option<Self> {
        match role {
            "developer" => Some(Self::Developer),
            "reviewer" => Some(Self::Reviewer),
            _ => None,
        }
    }

    pub const fn name(self) -> &'static str {
        match self {
            Self::Developer => "developer",
            Self::Reviewer => "reviewer",
        }
    }

    /// The statuses a task can be claimed from (reviewers: one, listed twice).
    pub const fn claim_from(self) -> (&'static str, &'static str) {
        match self {
            Self::Developer => ("todo", "in_progress"),
            Self::Reviewer => ("review", "review"),
        }
    }

    /// The status of a task while this role holds it; claiming moves it here
    /// and `release` requires it.
    pub const fn holds(self) -> &'static str {
        match self {
            Self::Developer => "in_progress",
            Self::Reviewer => "review",
        }
    }

    /// Developers may only start work whose prerequisites are done.
    pub const fn needs_ready(self) -> bool {
        matches!(self, Self::Developer)
    }
}

/// `agent register NAME [--role R]` -> `NAME ROLE`.
pub fn register(name: &str, role: &str) -> Result<String> {
    let conn = crate::db::open_existing()?;
    register_inner(&conn, name, role)
}

fn register_inner(conn: &Connection, name: &str, role: &str) -> Result<String> {
    let name = name.trim();
    if name.is_empty() {
        bail!("agent name must not be empty or whitespace-only");
    }
    if name.contains(char::is_whitespace) {
        bail!("agent name '{name}' must not contain whitespace");
    }
    let Some(role) = Role::parse(role) else {
        bail!("invalid role '{role}': must be developer or reviewer");
    };

    let result = conn.execute(
        "INSERT INTO agents (name, role) VALUES (?1, ?2)",
        rusqlite::params![name, role.name()],
    );

    match result {
        Ok(_) => Ok(format!("{name} {}", role.name())),
        Err(rusqlite::Error::SqliteFailure(ffi_err, _))
            if ffi_err.code == rusqlite::ErrorCode::ConstraintViolation =>
        {
            bail!("agent '{name}' already exists")
        }
        Err(e) => Err(e.into()),
    }
}

/// `agent list` -> one `NAME ROLE` line per agent.
pub fn list() -> Result<String> {
    let conn = crate::db::open_existing()?;
    list_inner(&conn)
}

fn list_inner(conn: &Connection) -> Result<String> {
    let mut stmt = conn.prepare("SELECT name, role FROM agents ORDER BY name")?;
    let rows = stmt.query_map([], |row| {
        Ok(format!(
            "{} {}",
            row.get::<_, String>(0)?,
            row.get::<_, String>(1)?
        ))
    })?;
    let lines = rows.collect::<std::result::Result<Vec<String>, rusqlite::Error>>()?;
    if lines.is_empty() {
        return Ok("no agents".to_string());
    }
    Ok(lines.join("\n"))
}

/// `agent remove NAME` -> `NAME removed[, released #IDS]`.
pub fn remove(name: &str) -> Result<String> {
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

fn remove_inner(tx: &Transaction, name: &str) -> Result<String> {
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
            "UPDATE tasks
             SET executor = NULL,
                 claimed_at = NULL,
                 lease_expires_at = NULL,
                 updated_at = datetime('now')
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

    let released = if released_tasks.is_empty() {
        String::new()
    } else {
        let ids: Vec<String> = released_tasks.iter().map(|id| format!("#{id}")).collect();
        format!(", released {}", ids.join(","))
    };
    Ok(format!("{name} removed{released}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use rusqlite::Connection;

    fn setup() -> Connection {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch("PRAGMA foreign_keys=ON;").unwrap();
        conn.execute_batch(crate::db::SCHEMA).unwrap();
        conn
    }

    fn agent_id(conn: &Connection, name: &str) -> i64 {
        conn.query_row("SELECT id FROM agents WHERE name = ?1", [name], |row| {
            row.get(0)
        })
        .unwrap()
    }

    /// A file-backed (not in-memory) connection, needed for tests that rely
    /// on OS file permissions to force a non-constraint error -- permissions
    /// don't apply to `:memory:` databases.
    fn setup_file_backed() -> (tempfile::TempDir, Connection) {
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("board.db");
        let conn = Connection::open(&path).unwrap();
        conn.execute_batch("PRAGMA foreign_keys=ON;").unwrap();
        conn.execute_batch(crate::db::SCHEMA).unwrap();
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
        let err = register_inner(&conn, "agent-1", "developer").unwrap_err();
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
    fn register_replies_with_name_and_role() {
        let conn = setup();
        assert_eq!(
            register_inner(&conn, "agent-1", "developer").unwrap(),
            "agent-1 developer"
        );
        assert_eq!(
            register_inner(&conn, "agent-2", "reviewer").unwrap(),
            "agent-2 reviewer"
        );
    }

    #[test]
    fn register_rejects_names_that_would_break_line_output() {
        let conn = setup();
        let err = register_inner(&conn, "two words", "developer").unwrap_err();
        assert!(err.to_string().contains("whitespace"), "{err}");
        let err = register_inner(&conn, "tab\tbed", "developer").unwrap_err();
        assert!(err.to_string().contains("whitespace"), "{err}");
    }

    #[test]
    fn role_knows_what_it_claims_and_holds() {
        assert_eq!(Role::parse("developer"), Some(Role::Developer));
        assert_eq!(Role::parse("reviewer"), Some(Role::Reviewer));
        assert_eq!(Role::parse("writer"), None);
        assert_eq!(Role::Developer.claim_from(), ("todo", "in_progress"));
        assert_eq!(Role::Reviewer.claim_from(), ("review", "review"));
        assert_eq!(Role::Developer.holds(), "in_progress");
        assert_eq!(Role::Reviewer.holds(), "review");
        assert!(Role::Developer.needs_ready());
        assert!(!Role::Reviewer.needs_ready());
        assert_eq!(Role::parse(Role::Reviewer.name()), Some(Role::Reviewer));
    }

    #[test]
    fn register_rejects_empty_name() {
        let conn = setup();
        let err = register_inner(&conn, "   ", "developer").unwrap_err();
        assert!(err.to_string().contains("empty"));
    }

    #[test]
    fn register_rejects_unknown_role() {
        let conn = setup();
        let err = register_inner(&conn, "agent-1", "writer").unwrap_err();
        assert!(err.to_string().contains("developer or reviewer"));
    }

    #[test]
    fn register_duplicate_returns_clean_error() {
        let conn = setup();
        register_inner(&conn, "agent-1", "developer").unwrap();
        let err = register_inner(&conn, "agent-1", "reviewer").unwrap_err();
        assert_eq!(err.to_string(), "agent 'agent-1' already exists");
    }

    #[test]
    fn list_returns_registered_agents() {
        let conn = setup();
        register_inner(&conn, "bravo", "reviewer").unwrap();
        register_inner(&conn, "alpha", "developer").unwrap();
        // one line per agent, ordered by name
        assert_eq!(
            list_inner(&conn).unwrap(),
            "alpha developer\nbravo reviewer"
        );
    }

    #[test]
    fn list_without_agents_says_so() {
        let conn = setup();
        assert_eq!(list_inner(&conn).unwrap(), "no agents");
    }

    #[test]
    fn remove_cascades_and_releases_tasks() {
        let mut conn = setup();
        register_inner(&conn, "agent-1", "developer").unwrap();
        let agent_id = agent_id(&conn, "agent-1");

        conn.execute(
            "INSERT INTO tasks (
               id, title, priority, status, executor, tests, claimed_at,
               lease_expires_at, updated_at
             ) VALUES (
               1, 'do thing', 'medium', 'in_progress', ?1, '[\"test\"]',
               datetime('now'), datetime('now', '+1 hour'), '2000-01-01 00:00:00'
             )",
            [agent_id],
        )
        .unwrap();

        let tx = conn.transaction().unwrap();
        let result = remove_inner(&tx, "agent-1").unwrap();
        tx.commit().unwrap();

        assert_eq!(result, "agent-1 removed, released #1");

        let (executor, status, updated_at): (Option<i64>, String, String) = conn
            .query_row(
                "SELECT executor, status, updated_at FROM tasks WHERE id = 1",
                [],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .unwrap();
        assert_eq!(executor, None);
        assert_eq!(status, "in_progress");
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
        register_inner(&conn, "agent-1", "developer").unwrap();
        let agent_id = agent_id(&conn, "agent-1");
        conn.execute(
            "INSERT INTO tasks (
               id, title, priority, status, executor, tests, claimed_at, lease_expires_at
             ) VALUES (
               1, 'do thing', 'medium', 'in_progress', ?1, '[\"test\"]',
               datetime('now'), datetime('now', '+1 hour')
             )",
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
        assert_eq!(
            register_inner(&conn, "  alice  ", "developer").unwrap(),
            "alice developer"
        );
        assert_eq!(list_inner(&conn).unwrap(), "alice developer");
    }

    #[test]
    fn remove_without_tasks_reports_nothing_released() {
        let mut conn = setup();
        register_inner(&conn, "agent-1", "developer").unwrap();
        let tx = conn.transaction().unwrap();
        assert_eq!(remove_inner(&tx, "agent-1").unwrap(), "agent-1 removed");
    }
}
