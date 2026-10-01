use anyhow::Result;
use rusqlite::Connection;

use super::{STATUSES, deps};

/// `status` -> the board at a glance, in one or two lines:
///
/// ```text
/// backlog 0, todo 3, in_progress 1, review 0, done 5; blocked 1
/// agents: alice 7,9; bob -
/// ```
///
/// All five columns always appear, even at zero, so a fresh board reads the
/// same as a busy one. `blocked` counts open (todo / `in_progress`) tasks that
/// wait on unfinished prerequisites and is left out when zero. Each agent is
/// followed by the ids it currently holds (`-` for none); an expired lease or
/// a done task is not held.
pub fn status() -> Result<String> {
    let conn = crate::db::open_existing()?;
    status_inner(&conn)
}

fn status_inner(conn: &Connection) -> Result<String> {
    let mut counts = Vec::with_capacity(STATUSES.len());
    for status in STATUSES {
        let count: i64 = conn.query_row(
            "SELECT COUNT(*) FROM tasks WHERE status = ?1",
            [status],
            |row| row.get(0),
        )?;
        counts.push(format!("{status} {count}"));
    }
    let mut text = counts.join(", ");

    let blocked: i64 = conn.query_row(
        &format!(
            "SELECT COUNT(*) FROM tasks t
             WHERE t.status IN ('todo', 'in_progress') AND NOT {}",
            deps::ready_predicate("t")
        ),
        [],
        |row| row.get(0),
    )?;
    if blocked > 0 {
        text = format!("{text}; blocked {blocked}");
    }

    let mut stmt = conn.prepare(
        "SELECT agents.name, tasks.id FROM agents
         LEFT JOIN tasks ON tasks.executor = agents.id
          AND tasks.status != 'done'
          AND tasks.lease_expires_at > datetime('now')
         ORDER BY agents.name, tasks.id",
    )?;
    let rows = stmt.query_map([], |row| {
        Ok((row.get::<_, String>(0)?, row.get::<_, Option<i64>>(1)?))
    })?;
    let mut agents: Vec<(String, Vec<i64>)> = Vec::new();
    for row in rows {
        let (name, held) = row?;
        match agents.last_mut() {
            Some((last, ids)) if *last == name => ids.extend(held),
            _ => agents.push((name, held.into_iter().collect())),
        }
    }
    if !agents.is_empty() {
        let parts: Vec<String> = agents
            .iter()
            .map(|(name, ids)| {
                let held = if ids.is_empty() {
                    "-".to_string()
                } else {
                    deps::join(ids)
                };
                format!("{name} {held}")
            })
            .collect();
        text = format!("{text}\nagents: {}", parts.join("; "));
    }
    Ok(text)
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

    fn insert_agent(conn: &Connection, name: &str) -> i64 {
        conn.execute(
            "INSERT INTO agents (name, role) VALUES (?1, 'developer')",
            [name],
        )
        .unwrap();
        conn.last_insert_rowid()
    }

    fn insert_task(conn: &Connection, status: &str, executor: Option<i64>) -> i64 {
        conn.execute(
            "INSERT INTO tasks (
               title, priority, status, executor, tests, claimed_at, lease_expires_at
             ) VALUES (
               't', 'low', ?1, ?2, '[\"x\"]',
               CASE WHEN ?2 IS NULL THEN NULL ELSE datetime('now') END,
               CASE WHEN ?2 IS NULL THEN NULL ELSE datetime('now', '+1 hour') END
             )",
            rusqlite::params![status, executor],
        )
        .unwrap();
        conn.last_insert_rowid()
    }

    #[test]
    fn empty_board_reports_all_statuses_at_zero() {
        let conn = setup();
        assert_eq!(
            status_inner(&conn).unwrap(),
            "backlog 0, todo 0, in_progress 0, review 0, done 0"
        );
    }

    #[test]
    fn counts_tasks_per_status() {
        let conn = setup();
        insert_task(&conn, "todo", None);
        insert_task(&conn, "todo", None);
        insert_task(&conn, "in_progress", None);
        insert_task(&conn, "done", None);
        assert_eq!(
            status_inner(&conn).unwrap(),
            "backlog 0, todo 2, in_progress 1, review 0, done 1"
        );
    }

    #[test]
    fn lists_every_registered_agent_with_the_ids_it_holds() {
        let conn = setup();
        let alice = insert_agent(&conn, "alice");
        insert_agent(&conn, "bob");
        let first = insert_task(&conn, "in_progress", Some(alice));
        let second = insert_task(&conn, "in_progress", Some(alice));

        assert_eq!(
            status_inner(&conn).unwrap(),
            format!(
                "backlog 0, todo 0, in_progress 2, review 0, done 0\nagents: alice {first},{second}; bob -"
            )
        );
    }

    #[test]
    fn done_tasks_and_expired_leases_are_not_held() {
        let conn = setup();
        let alice = insert_agent(&conn, "alice");
        insert_task(&conn, "done", None);
        let stale = insert_task(&conn, "in_progress", Some(alice));
        conn.execute(
            "UPDATE tasks SET lease_expires_at = datetime('now', '-1 second') WHERE id = ?1",
            [stale],
        )
        .unwrap();
        assert!(status_inner(&conn).unwrap().ends_with("agents: alice -"));
    }

    #[test]
    fn blocked_counts_open_tasks_waiting_on_unfinished_prerequisites() {
        let conn = setup();
        let first = insert_task(&conn, "todo", None);
        let second = insert_task(&conn, "todo", None);
        let parked = insert_task(&conn, "backlog", None);
        deps::add_prerequisites(&conn, second, &[first]).unwrap();
        deps::add_prerequisites(&conn, parked, &[first]).unwrap();
        // Backlog tasks are parked, not blocked.
        assert_eq!(
            status_inner(&conn).unwrap(),
            "backlog 1, todo 2, in_progress 0, review 0, done 0; blocked 1"
        );

        conn.execute("UPDATE tasks SET status = 'done' WHERE id = ?1", [first])
            .unwrap();
        assert_eq!(
            status_inner(&conn).unwrap(),
            "backlog 1, todo 1, in_progress 0, review 0, done 1"
        );
    }
}
