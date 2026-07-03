use anyhow::{Result, bail};
use rusqlite::{Connection, OptionalExtension};
use serde_json::Value;

/// `agent-kanban claim <id> --agent <name>`
/// Must FIRST verify `--agent` is a registered name with a separate SELECT and
/// return a clear error if not — do NOT rely solely on the claim UPDATE's
/// `changes()` count to detect an unknown agent. Pitfall documented in todo.md
/// section 4: `UPDATE tasks SET executor = (SELECT id FROM agents WHERE
/// name=?) WHERE id=? AND executor IS NULL` still matches the row and reports
/// changes()==1 when the subquery returns NULL for an unregistered name,
/// silently leaving executor NULL while looking like success.
/// Then the claim itself is a single atomic
/// `UPDATE ... WHERE executor IS NULL`; check changes()==1 to detect
/// "someone else already claimed it". Must bump `updated_at`.
pub fn claim(id: i64, agent: &str) -> Result<Value> {
    let conn = crate::db::open_existing()?;
    claim_inner(&conn, id, agent)
}

fn claim_inner(conn: &Connection, id: i64, agent: &str) -> Result<Value> {
    // Step 1: resolve the agent id as its own query. Bail early if the agent
    // isn't registered — never let an unresolved id silently flow into the
    // UPDATE's WHERE clause.
    let agent_id: Option<i64> = conn
        .query_row("SELECT id FROM agents WHERE name = ?1", [agent], |row| {
            row.get(0)
        })
        .optional()?;
    let Some(agent_id) = agent_id else {
        bail!("agent '{agent}' is not registered; run `agent-kanban agent register {agent}` first");
    };

    claim_with_agent_id(conn, id, agent_id, agent)
}

/// Step 2 of claim, split out so tests can drive it with an already-resolved
/// (and possibly stale) `agent_id` without needing real concurrency: the
/// atomic claim UPDATE using the already-resolved integer agent id.
///
/// Between step 1's SELECT and this UPDATE, a concurrent `agent-kanban
/// agent remove <agent>` can delete the agent row, making `agent_id` stale. The
/// UPDATE then violates the `executor REFERENCES agents(id)` foreign key;
/// catch that and translate it into the same clean "not registered" error
/// as the upfront check, since that's exactly what happened — the agent
/// became unregistered between the check and the claim. `SQLite` aborts and
/// rolls back just the failed statement, so `executor` is guaranteed to
/// remain untouched (still NULL) when this happens.
fn claim_with_agent_id(conn: &Connection, id: i64, agent_id: i64, agent: &str) -> Result<Value> {
    let result = conn.execute(
        "UPDATE tasks SET executor = ?1, status = 'in_progress', updated_at = datetime('now') \
         WHERE id = ?2 AND executor IS NULL",
        rusqlite::params![agent_id, id],
    );

    let changed = match result {
        Ok(changed) => changed,
        Err(rusqlite::Error::SqliteFailure(ffi_err, _))
            if ffi_err.extended_code == rusqlite::ffi::SQLITE_CONSTRAINT_FOREIGNKEY =>
        {
            bail!(
                "agent '{agent}' is not registered; run `agent-kanban agent register {agent}` first"
            )
        }
        Err(e) => return Err(e.into()),
    };

    if changed == 0 {
        // Distinguish "doesn't exist" from "already claimed by someone else".
        let executor: Option<Option<i64>> = conn
            .query_row("SELECT executor FROM tasks WHERE id = ?1", [id], |row| {
                row.get::<_, Option<i64>>(0)
            })
            .optional()?;
        match executor {
            None => bail!("task {id} not found"),
            Some(_) => bail!("task {id} is already claimed"),
        }
    }

    crate::commands::fetch_task(conn, id)
}

/// `agent-kanban move <id> --status S` — must also bump `updated_at`.
pub fn move_status(id: i64, status: &str) -> Result<Value> {
    let conn = crate::db::open_existing()?;
    move_status_inner(&conn, id, status)
}

// Intentionally unrestricted, including out of 'done': unlike edit/remove
// (task.rs), which block once a task is 'done' to keep it immutable, move
// is allowed to transition a 'done' task back to any other status. This
// asymmetry is deliberate — do not "fix" it to match edit/remove.
fn move_status_inner(conn: &Connection, id: i64, status: &str) -> Result<Value> {
    const VALID: [&str; 5] = ["backlog", "todo", "in_progress", "review", "done"];
    if !VALID.contains(&status) {
        bail!("invalid status '{status}': must be one of backlog, todo, in_progress, review, done");
    }

    let changed = conn.execute(
        "UPDATE tasks SET status = ?1, updated_at = datetime('now') WHERE id = ?2",
        rusqlite::params![status, id],
    )?;
    if changed == 0 {
        bail!("task {id} not found");
    }

    crate::commands::fetch_task(conn, id)
}

/// `agent-kanban release <id>` — un-claim: set executor = NULL AND reset status
/// (e.g. back to 'todo'), not just clear the executor. Must bump `updated_at`.
pub fn release(id: i64) -> Result<Value> {
    let conn = crate::db::open_existing()?;
    release_inner(&conn, id)
}

fn release_inner(conn: &Connection, id: i64) -> Result<Value> {
    // The "must currently be claimed" guard is baked into this same atomic
    // UPDATE rather than a separate check-then-act: a plain SELECT check
    // followed by an unconditional UPDATE WHERE id = ?1 has a real race —
    // if the task is released and re-claimed by someone else between the
    // two statements, the stale UPDATE still fires and silently wipes the
    // new claim. Confirmed by direct reproduction before this fix: claim as
    // alice, release, claim as bob, then run the old unconditional UPDATE —
    // it clobbered bob's fresh claim back to unclaimed with no error.
    let changed = conn.execute(
        "UPDATE tasks SET executor = NULL, status = 'todo', updated_at = datetime('now') \
         WHERE id = ?1 AND executor IS NOT NULL",
        [id],
    )?;

    if changed == 0 {
        return Err(diagnose_release_guard_failure(conn, id));
    }

    crate::commands::fetch_task(conn, id)
}

/// After the guarded release `UPDATE` affects 0 rows, figure out why:
/// missing, already unclaimed, or (an extremely narrow window) claimed again
/// by the time of this read. Split out, like
/// `task::diagnose_mutation_guard_failure`, so that last, hardest-to-reach
/// case can be driven directly in a test without needing genuine concurrency.
fn diagnose_release_guard_failure(conn: &Connection, id: i64) -> anyhow::Error {
    let executor: rusqlite::Result<Option<Option<i64>>> = conn
        .query_row("SELECT executor FROM tasks WHERE id = ?1", [id], |row| {
            row.get::<_, Option<i64>>(0)
        })
        .optional();
    match executor {
        Err(e) => e.into(),
        Ok(None) => anyhow::anyhow!("task {id} not found"),
        Ok(Some(None)) => anyhow::anyhow!("task {id} is not claimed"),
        Ok(Some(Some(_))) => anyhow::anyhow!(
            "task {id} could not be released (state changed concurrently; try again)"
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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

    #[test]
    fn claim_propagates_non_constraint_errors_uncaught() {
        // The FK-violation catch is a specific pattern match, not a blanket
        // "any error means unregistered" -- confirmed by forcing a
        // *different* kind of SQLite error (a read-only file, not a
        // constraint) and checking it comes through as its own genuine
        // error instead of being misreported as "not registered".
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("board.db");
        let setup_conn = Connection::open(&path).unwrap();
        setup_conn.execute_batch("PRAGMA foreign_keys=ON;").unwrap();
        setup_conn.execute_batch(SCHEMA).unwrap();
        let agent_id = insert_agent(&setup_conn, "agent-a");
        let task_id = insert_task(&setup_conn, "do thing");
        drop(setup_conn);

        let mut perms = std::fs::metadata(&path).unwrap().permissions();
        perms.set_readonly(true);
        std::fs::set_permissions(&path, perms).unwrap();

        let conn = Connection::open(&path).unwrap();
        let err = claim_with_agent_id(&conn, task_id, agent_id, "agent-a").unwrap_err();
        assert!(
            !err.to_string().contains("not registered"),
            "unexpected message: {err}"
        );
        assert!(
            err.to_string().contains("readonly"),
            "expected a readonly-database error, got: {err}"
        );

        let mut perms = std::fs::metadata(&path).unwrap().permissions();
        #[allow(clippy::permissions_set_readonly_false)]
        perms.set_readonly(false);
        std::fs::set_permissions(&path, perms).unwrap();
    }

    fn insert_agent(conn: &Connection, name: &str) -> i64 {
        conn.execute("INSERT INTO agents (name) VALUES (?1)", [name])
            .unwrap();
        conn.last_insert_rowid()
    }

    fn insert_task(conn: &Connection, title: &str) -> i64 {
        conn.execute(
            "INSERT INTO tasks (title, priority, tests) VALUES (?1, 'medium', '[\"t\"]')",
            [title],
        )
        .unwrap();
        conn.last_insert_rowid()
    }

    fn get_raw_executor(conn: &Connection, id: i64) -> Option<i64> {
        conn.query_row("SELECT executor FROM tasks WHERE id = ?1", [id], |row| {
            row.get(0)
        })
        .unwrap()
    }

    #[test]
    fn claim_succeeds_and_sets_executor_and_status() {
        let conn = setup();
        insert_agent(&conn, "agent-a");
        let task_id = insert_task(&conn, "do thing");

        let result = claim_inner(&conn, task_id, "agent-a").unwrap();
        assert_eq!(result["executor"], "agent-a");
        assert_eq!(result["status"], "in_progress");
    }

    #[test]
    fn claim_unregistered_agent_fails_and_leaves_executor_null() {
        let conn = setup();
        let task_id = insert_task(&conn, "do thing");

        let result = claim_inner(&conn, task_id, "ghost-agent");
        assert!(result.is_err());
        let msg = result.unwrap_err().to_string();
        assert!(msg.contains("not registered"), "unexpected message: {msg}");

        // The critical assertion: raw DB state, not just the error message.
        assert_eq!(get_raw_executor(&conn, task_id), None);
    }

    #[test]
    fn claim_already_claimed_task_fails_and_does_not_change_holder() {
        let conn = setup();
        let a1 = insert_agent(&conn, "agent-a");
        insert_agent(&conn, "agent-b");
        let task_id = insert_task(&conn, "do thing");

        claim_inner(&conn, task_id, "agent-a").unwrap();

        let result = claim_inner(&conn, task_id, "agent-b");
        assert!(result.is_err());
        let msg = result.unwrap_err().to_string();
        assert!(msg.contains("already claimed"), "unexpected message: {msg}");

        assert_eq!(get_raw_executor(&conn, task_id), Some(a1));
    }

    #[test]
    fn claim_nonexistent_task_fails_with_not_found() {
        let conn = setup();
        insert_agent(&conn, "agent-a");

        let result = claim_inner(&conn, 999, "agent-a");
        assert!(result.is_err());
        let msg = result.unwrap_err().to_string();
        assert!(msg.contains("not found"), "unexpected message: {msg}");
    }

    #[test]
    fn claim_with_stale_agent_id_fails_cleanly_and_leaves_executor_null() {
        // Simulates the TOCTOU race: the agent id was resolved (step 1), but
        // the agent row was removed concurrently before the claim UPDATE
        // (step 2) runs. Drives claim_with_agent_id directly with a stale
        // id so the race window is deterministic instead of relying on real
        // concurrency.
        let conn = setup();
        let stale_agent_id = insert_agent(&conn, "agent-a");
        let task_id = insert_task(&conn, "do thing");

        conn.execute("DELETE FROM agents WHERE id = ?1", [stale_agent_id])
            .unwrap();

        let result = claim_with_agent_id(&conn, task_id, stale_agent_id, "agent-a");
        assert!(result.is_err());
        let msg = result.unwrap_err().to_string();
        assert!(
            !msg.contains("FOREIGN KEY"),
            "raw sqlite error leaked: {msg}"
        );
        assert!(msg.contains("not registered"), "unexpected message: {msg}");

        assert_eq!(get_raw_executor(&conn, task_id), None);
    }

    #[test]
    fn move_status_allows_transition_out_of_done() {
        let conn = setup();
        let task_id = insert_task(&conn, "do thing");

        let done = move_status_inner(&conn, task_id, "done").unwrap();
        assert_eq!(done["status"], "done");

        let back = move_status_inner(&conn, task_id, "todo").unwrap();
        assert_eq!(back["status"], "todo");
    }

    #[test]
    fn move_status_updates_status_and_bumps_updated_at() {
        let conn = setup();
        let task_id = insert_task(&conn, "do thing");
        conn.execute(
            "UPDATE tasks SET updated_at = '2000-01-01 00:00:00' WHERE id = ?1",
            [task_id],
        )
        .unwrap();

        let result = move_status_inner(&conn, task_id, "review").unwrap();
        assert_eq!(result["status"], "review");
        assert_ne!(result["updated_at"], "2000-01-01 00:00:00");
    }

    #[test]
    fn move_status_rejects_invalid_status() {
        let conn = setup();
        let task_id = insert_task(&conn, "do thing");

        let result = move_status_inner(&conn, task_id, "bogus");
        assert!(result.is_err());
        let msg = result.unwrap_err().to_string();
        assert!(msg.contains("invalid status"), "unexpected message: {msg}");
    }

    #[test]
    fn move_status_nonexistent_task_fails() {
        let conn = setup();
        let result = move_status_inner(&conn, 999, "todo");
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("not found"));
    }

    #[test]
    fn release_clears_executor_and_resets_status() {
        let conn = setup();
        insert_agent(&conn, "agent-a");
        let task_id = insert_task(&conn, "do thing");
        claim_inner(&conn, task_id, "agent-a").unwrap();

        let result = release_inner(&conn, task_id).unwrap();
        assert_eq!(result["executor"], Value::Null);
        assert_eq!(result["status"], "todo");
        assert_eq!(get_raw_executor(&conn, task_id), None);
    }

    #[test]
    fn release_guard_sql_is_noop_on_unclaimed_task() {
        // Regression test for the TOCTOU this fix closes: a naive
        // check-then-act (SELECT, then an unconditional UPDATE WHERE
        // id = ?1) would unconditionally clear/reset a task regardless of
        // its actual state at write time -- e.g. a task released and
        // re-claimed by someone else between the check and the write would
        // still get clobbered. This drives the exact guarded UPDATE
        // release_inner now uses directly against an already-unclaimed
        // task, proving it's a no-op (changed == 0). Against the OLD
        // unconditional `UPDATE ... WHERE id = ?1`, this same statement
        // would have reported changed == 1 regardless of whether the task
        // was actually claimed.
        const GUARDED_RELEASE: &str = "UPDATE tasks SET executor = NULL, status = 'todo', \
             updated_at = datetime('now') WHERE id = ?1 AND executor IS NOT NULL";

        let conn = setup();
        let task_id = insert_task(&conn, "do thing");
        let changed = conn.execute(GUARDED_RELEASE, [task_id]).unwrap();
        assert_eq!(
            changed, 0,
            "releasing an already-unclaimed task must be a no-op"
        );
    }

    #[test]
    fn release_unclaimed_task_errors() {
        let conn = setup();
        let task_id = insert_task(&conn, "do thing");

        let result = release_inner(&conn, task_id);
        assert!(result.is_err());
        let msg = result.unwrap_err().to_string();
        assert!(msg.contains("not claimed"), "unexpected message: {msg}");
    }

    #[test]
    fn release_nonexistent_task_errors() {
        let conn = setup();
        let result = release_inner(&conn, 999);
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("not found"));
    }

    #[test]
    fn diagnose_release_guard_failure_propagates_non_missing_row_errors_uncaught() {
        // The `Ok(None) => "not found"` mapping only applies when the
        // diagnostic SELECT finds no *row* -- confirmed here by forcing a
        // different kind of failure (the table itself is gone, not just the
        // row) so it comes through as its own genuine error rather than
        // being misreported as "not found".
        let conn = setup();
        let task_id = insert_task(&conn, "do thing");
        conn.execute_batch("DROP TABLE tasks;").unwrap();

        let err = diagnose_release_guard_failure(&conn, task_id);
        assert!(
            !err.to_string().contains("not found"),
            "unexpected message: {err}"
        );
        assert!(
            err.to_string().contains("no such table"),
            "expected a 'no such table' error, got: {err}"
        );
    }

    #[test]
    fn diagnose_release_guard_failure_reports_state_changed_concurrently() {
        // The last arm fires when the diagnostic read finds the task
        // claimed after all -- i.e. it's neither missing nor already
        // unclaimed, the only two cases the guarded UPDATE's failure would
        // normally mean. In production that means a concurrent claim landed
        // in the narrow window between the failed release UPDATE and this
        // read; here it's driven directly against a task that's simply
        // claimed, since this function only reports on what it reads and
        // doesn't care whether it was actually called after a real guard
        // failure.
        let conn = setup();
        insert_agent(&conn, "agent-a");
        let task_id = insert_task(&conn, "do thing");
        claim_inner(&conn, task_id, "agent-a").unwrap();

        let err = diagnose_release_guard_failure(&conn, task_id);
        assert!(
            err.to_string().contains("state changed concurrently"),
            "unexpected message: {err}"
        );
    }
}
