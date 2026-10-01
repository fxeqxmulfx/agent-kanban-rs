//! Task dependencies: tasks form a DAG. The edge `task_deps(task_id,
//! depends_on)` means `task_id` cannot be claimed by a developer until
//! `depends_on` is done.

use anyhow::{Result, bail};
use rusqlite::Connection;

/// SQL predicate: every prerequisite of the task aliased `task` is done (or it
/// has none). Shared by the claim statements so readiness is decided inside
/// the same atomic `UPDATE` that claims.
pub fn ready_predicate(task: &str) -> String {
    format!(
        "NOT EXISTS (SELECT 1 FROM task_deps d JOIN tasks p ON p.id = d.depends_on \
         WHERE d.task_id = {task}.id AND p.status != 'done')"
    )
}

/// `1,2,3`.
pub fn join(ids: &[i64]) -> String {
    ids.iter().map(i64::to_string).collect::<Vec<_>>().join(",")
}

/// ` label:1,2,3` (with the leading space), or nothing when `ids` is empty.
pub fn labeled(label: &str, ids: &[i64]) -> String {
    if ids.is_empty() {
        String::new()
    } else {
        format!(" {label}:{}", join(ids))
    }
}

fn id_column(conn: &Connection, sql: &str, id: i64) -> Result<Vec<i64>> {
    let mut stmt = conn.prepare(sql)?;
    let rows = stmt.query_map([id], |row| row.get::<_, i64>(0))?;
    Ok(rows.collect::<std::result::Result<Vec<_>, _>>()?)
}

/// Prerequisites of `task_id` that are not done yet, ascending.
pub fn open_prerequisites(conn: &Connection, task_id: i64) -> Result<Vec<i64>> {
    id_column(
        conn,
        "SELECT d.depends_on FROM task_deps d JOIN tasks p ON p.id = d.depends_on
         WHERE d.task_id = ?1 AND p.status != 'done' ORDER BY d.depends_on",
        task_id,
    )
}

/// Tasks still waiting on `task_id`, ascending: neither they nor `task_id`
/// are done (once it is done, nothing waits on it any more).
pub fn open_dependents(conn: &Connection, task_id: i64) -> Result<Vec<i64>> {
    id_column(
        conn,
        "SELECT d.task_id FROM task_deps d
           JOIN tasks c ON c.id = d.task_id JOIN tasks p ON p.id = d.depends_on
         WHERE d.depends_on = ?1 AND c.status != 'done' AND p.status != 'done'
         ORDER BY d.task_id",
        task_id,
    )
}

/// Dependents of `task_id` that can be claimed now that it is done: still
/// open, and with no other unfinished prerequisite.
pub fn unblocked_by(conn: &Connection, task_id: i64) -> Result<Vec<i64>> {
    id_column(
        conn,
        &format!(
            "SELECT c.id FROM task_deps d JOIN tasks c ON c.id = d.task_id
             WHERE d.depends_on = ?1 AND c.status IN ('todo', 'in_progress')
               AND {} ORDER BY c.id",
            ready_predicate("c")
        ),
        task_id,
    )
}

/// Whether `from` depends on `target`, directly or through other tasks.
fn depends_transitively(conn: &Connection, from: i64, target: i64) -> Result<bool> {
    Ok(conn.query_row(
        "WITH RECURSIVE up(id) AS (
           SELECT depends_on FROM task_deps WHERE task_id = ?1
           UNION
           SELECT d.depends_on FROM task_deps d JOIN up ON d.task_id = up.id
         )
         SELECT EXISTS (SELECT 1 FROM up WHERE id = ?2)",
        [from, target],
        |row| row.get(0),
    )?)
}

/// Make `task_id` wait for each of `prerequisites`. Rejects unknown tasks,
/// self-reference and cycles. The cycle check reads the graph and then
/// writes, so callers must hold a write lock for the whole operation (an
/// `IMMEDIATE` transaction); otherwise two concurrent calls (`A after B`,
/// `B after A`) could each pass the check and close a cycle together.
pub fn add_prerequisites(conn: &Connection, task_id: i64, prerequisites: &[i64]) -> Result<()> {
    for &prerequisite in prerequisites {
        if prerequisite == task_id {
            bail!("task {task_id} cannot come after itself");
        }
        let exists: bool = conn.query_row(
            "SELECT EXISTS (SELECT 1 FROM tasks WHERE id = ?1)",
            [prerequisite],
            |row| row.get(0),
        )?;
        if !exists {
            bail!("task {prerequisite} not found");
        }
        if depends_transitively(conn, prerequisite, task_id)? {
            bail!(
                "task {task_id} cannot come after {prerequisite}: {prerequisite} already comes \
                 after {task_id} (cycle)"
            );
        }
        conn.execute(
            "INSERT OR IGNORE INTO task_deps (task_id, depends_on) VALUES (?1, ?2)",
            [task_id, prerequisite],
        )?;
    }
    Ok(())
}

/// Remove the edges `task_id -> prerequisite`; an edge that does not exist is
/// an error so a typo'd id does not pass silently.
pub fn drop_prerequisites(conn: &Connection, task_id: i64, prerequisites: &[i64]) -> Result<()> {
    for &prerequisite in prerequisites {
        let removed = conn.execute(
            "DELETE FROM task_deps WHERE task_id = ?1 AND depends_on = ?2",
            [task_id, prerequisite],
        )?;
        if removed == 0 {
            bail!("task {task_id} does not come after {prerequisite}");
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn setup() -> Connection {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch("PRAGMA foreign_keys=ON;").unwrap();
        conn.execute_batch(crate::db::SCHEMA).unwrap();
        conn
    }

    fn task(conn: &Connection, status: &str) -> i64 {
        conn.execute(
            "INSERT INTO tasks (title, priority, status, tests) VALUES ('t', 'low', ?1, '[\"c\"]')",
            [status],
        )
        .unwrap();
        conn.last_insert_rowid()
    }

    fn set_status(conn: &Connection, id: i64, status: &str) {
        conn.execute("UPDATE tasks SET status = ?1 WHERE id = ?2", (status, id))
            .unwrap();
    }

    #[test]
    fn formats_id_lists() {
        assert_eq!(join(&[3, 5, 8]), "3,5,8");
        assert_eq!(labeled("after", &[3, 5]), " after:3,5");
        assert_eq!(labeled("after", &[]), "");
    }

    #[test]
    fn only_unfinished_prerequisites_and_dependents_are_reported() {
        let conn = setup();
        let (a, b, c) = (
            task(&conn, "todo"),
            task(&conn, "todo"),
            task(&conn, "todo"),
        );
        add_prerequisites(&conn, c, &[a, b]).unwrap();
        assert_eq!(open_prerequisites(&conn, c).unwrap(), vec![a, b]);
        assert_eq!(open_dependents(&conn, a).unwrap(), vec![c]);

        set_status(&conn, a, "done");
        assert_eq!(open_prerequisites(&conn, c).unwrap(), vec![b]);
        // Nothing waits on a finished task, even though `c` is still open.
        assert!(open_dependents(&conn, a).unwrap().is_empty());
        assert_eq!(open_dependents(&conn, b).unwrap(), vec![c]);

        set_status(&conn, c, "done");
        assert!(open_dependents(&conn, b).unwrap().is_empty());
    }

    #[test]
    fn rejects_self_unknown_and_duplicate_free_edges() {
        let conn = setup();
        let a = task(&conn, "todo");
        let b = task(&conn, "todo");
        assert!(
            add_prerequisites(&conn, a, &[a])
                .unwrap_err()
                .to_string()
                .contains("after itself")
        );
        assert_eq!(
            add_prerequisites(&conn, a, &[999]).unwrap_err().to_string(),
            "task 999 not found"
        );
        // Adding the same edge twice is harmless.
        add_prerequisites(&conn, a, &[b, b]).unwrap();
        add_prerequisites(&conn, a, &[b]).unwrap();
        assert_eq!(open_prerequisites(&conn, a).unwrap(), vec![b]);
    }

    #[test]
    fn rejects_direct_and_transitive_cycles_without_writing_the_edge() {
        let conn = setup();
        let (a, b, c) = (
            task(&conn, "todo"),
            task(&conn, "todo"),
            task(&conn, "todo"),
        );
        add_prerequisites(&conn, b, &[a]).unwrap(); // b after a
        add_prerequisites(&conn, c, &[b]).unwrap(); // c after b

        let direct = add_prerequisites(&conn, a, &[b]).unwrap_err().to_string();
        assert!(direct.contains("cycle"), "{direct}");
        let transitive = add_prerequisites(&conn, a, &[c]).unwrap_err().to_string();
        assert!(transitive.contains("cycle"), "{transitive}");

        assert!(open_prerequisites(&conn, a).unwrap().is_empty());
        // A diamond is not a cycle.
        let d = task(&conn, "todo");
        add_prerequisites(&conn, d, &[b, c]).unwrap();
    }

    #[test]
    fn cycle_check_ignores_finished_work_no_more_than_unfinished_work() {
        // Finished prerequisites still count as edges: the graph shape, not
        // the statuses, decides what is a cycle.
        let conn = setup();
        let (a, b) = (task(&conn, "todo"), task(&conn, "todo"));
        add_prerequisites(&conn, b, &[a]).unwrap();
        set_status(&conn, a, "done");
        assert!(add_prerequisites(&conn, a, &[b]).is_err());
    }

    #[test]
    fn drop_removes_edges_and_reports_missing_ones() {
        let conn = setup();
        let (a, b) = (task(&conn, "todo"), task(&conn, "todo"));
        add_prerequisites(&conn, b, &[a]).unwrap();
        drop_prerequisites(&conn, b, &[a]).unwrap();
        assert!(open_prerequisites(&conn, b).unwrap().is_empty());
        assert_eq!(
            drop_prerequisites(&conn, b, &[a]).unwrap_err().to_string(),
            format!("task {b} does not come after {a}")
        );
    }

    #[test]
    fn unblocked_by_lists_only_dependents_that_became_claimable() {
        let conn = setup();
        let (a, b) = (task(&conn, "todo"), task(&conn, "todo"));
        let only_a = task(&conn, "todo");
        let a_and_b = task(&conn, "todo");
        let parked = task(&conn, "backlog");
        add_prerequisites(&conn, only_a, &[a]).unwrap();
        add_prerequisites(&conn, a_and_b, &[a, b]).unwrap();
        add_prerequisites(&conn, parked, &[a]).unwrap();

        set_status(&conn, a, "done");
        assert_eq!(unblocked_by(&conn, a).unwrap(), vec![only_a]);

        set_status(&conn, b, "done");
        assert_eq!(unblocked_by(&conn, b).unwrap(), vec![a_and_b]);
    }

    #[test]
    fn ready_predicate_is_true_without_prerequisites_and_after_they_finish() {
        let conn = setup();
        let (a, b) = (task(&conn, "todo"), task(&conn, "todo"));
        add_prerequisites(&conn, b, &[a]).unwrap();
        let sql = format!(
            "SELECT COUNT(*) FROM tasks t WHERE t.id = ?1 AND {}",
            ready_predicate("t")
        );
        let ready = |id: i64| -> i64 { conn.query_row(&sql, [id], |row| row.get(0)).unwrap() };
        assert_eq!(ready(a), 1);
        assert_eq!(ready(b), 0);
        set_status(&conn, a, "done");
        assert_eq!(ready(b), 1);
    }

    #[test]
    fn the_schema_forbids_self_edges_and_deleting_a_prerequisite() {
        let conn = setup();
        let (a, b) = (task(&conn, "todo"), task(&conn, "todo"));
        assert!(
            conn.execute("INSERT INTO task_deps VALUES (?1, ?1)", [a])
                .is_err()
        );
        add_prerequisites(&conn, b, &[a]).unwrap();
        assert!(
            conn.execute("DELETE FROM tasks WHERE id = ?1", [a])
                .is_err()
        );
        // Deleting the waiting task is fine and takes its edges with it.
        conn.execute("DELETE FROM tasks WHERE id = ?1", [b])
            .unwrap();
        conn.execute("DELETE FROM tasks WHERE id = ?1", [a])
            .unwrap();
    }
}
