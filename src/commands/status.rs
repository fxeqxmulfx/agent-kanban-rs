use anyhow::Result;
use rusqlite::Connection;
use serde_json::{Map, Value, json};

const STATUSES: [&str; 5] = ["backlog", "todo", "in_progress", "review", "done"];

/// `agent-kanban status`
/// Board overview: a task count per status column (always all five, even at
/// zero, so an empty/fresh board reads the same shape as a busy one), a
/// `total`, and each registered agent's current claimed-task count (also
/// always all registered agents, including ones holding nothing).
pub fn status() -> Result<Value> {
    let conn = crate::db::open_existing()?;
    status_inner(&conn)
}

fn status_inner(conn: &Connection) -> Result<Value> {
    let mut fields = Map::new();
    let mut total = 0i64;
    for status in STATUSES {
        let count: i64 = conn.query_row(
            "SELECT COUNT(*) FROM tasks WHERE status = ?1",
            [status],
            |row| row.get(0),
        )?;
        fields.insert(status.to_string(), json!(count));
        total += count;
    }
    fields.insert("total".to_string(), json!(total));

    let mut stmt = conn.prepare(
        "SELECT agents.name, COUNT(tasks.id) FROM agents \
         LEFT JOIN tasks ON tasks.executor = agents.id \
         GROUP BY agents.id ORDER BY agents.name",
    )?;
    let rows = stmt.query_map([], |row| {
        let name: String = row.get(0)?;
        let claimed: i64 = row.get(1)?;
        Ok((name, claimed))
    })?;
    let mut agents = Map::new();
    for row in rows {
        let (name, claimed) = row?;
        agents.insert(name, json!(claimed));
    }
    fields.insert("agents".to_string(), Value::Object(agents));

    Ok(Value::Object(fields))
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

    fn insert_agent(conn: &Connection, name: &str) -> i64 {
        conn.execute("INSERT INTO agents (name) VALUES (?1)", [name])
            .unwrap();
        conn.last_insert_rowid()
    }

    fn insert_task(conn: &Connection, status: &str, executor: Option<i64>) {
        conn.execute(
            "INSERT INTO tasks (title, priority, status, executor, tests) \
             VALUES ('t', 'low', ?1, ?2, '[\"x\"]')",
            rusqlite::params![status, executor],
        )
        .unwrap();
    }

    #[test]
    fn empty_board_reports_all_statuses_at_zero() {
        let conn = setup();
        let result = status_inner(&conn).unwrap();
        for status in STATUSES {
            assert_eq!(result[status], 0, "expected {status} to be 0");
        }
        assert_eq!(result["total"], 0);
        assert_eq!(result["agents"], json!({}));
    }

    #[test]
    fn counts_tasks_per_status_and_total() {
        let conn = setup();
        insert_task(&conn, "todo", None);
        insert_task(&conn, "todo", None);
        insert_task(&conn, "in_progress", None);
        insert_task(&conn, "done", None);

        let result = status_inner(&conn).unwrap();
        assert_eq!(result["todo"], 2);
        assert_eq!(result["in_progress"], 1);
        assert_eq!(result["done"], 1);
        assert_eq!(result["backlog"], 0);
        assert_eq!(result["review"], 0);
        assert_eq!(result["total"], 4);
    }

    #[test]
    fn reports_every_registered_agent_including_zero_claims() {
        let conn = setup();
        let alice = insert_agent(&conn, "alice");
        insert_agent(&conn, "bob");
        insert_task(&conn, "in_progress", Some(alice));
        insert_task(&conn, "in_progress", Some(alice));

        let result = status_inner(&conn).unwrap();
        assert_eq!(result["agents"]["alice"], 2);
        assert_eq!(result["agents"]["bob"], 0);
    }

    #[test]
    fn done_tasks_still_count_toward_the_agent_that_finished_them() {
        // executor isn't cleared on completion (only release/agent-remove
        // clear it), so a done task still counts against its agent here --
        // this reports current DB state, not "currently active work".
        let conn = setup();
        let alice = insert_agent(&conn, "alice");
        insert_task(&conn, "done", Some(alice));

        let result = status_inner(&conn).unwrap();
        assert_eq!(result["agents"]["alice"], 1);
        assert_eq!(result["done"], 1);
    }
}
