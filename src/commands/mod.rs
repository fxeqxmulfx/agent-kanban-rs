pub mod agent;
pub mod lifecycle;
pub mod status;
pub mod task;

use anyhow::{Result, anyhow};
use rusqlite::{Connection, OptionalExtension};
use serde_json::{Value, json};

pub fn init() -> Result<Value> {
    crate::db::init()?;
    Ok(json!({ "status": "initialized" }))
}

/// Shared task SELECT: joins `executor` (an agent id) to the agent's name so
/// callers never see the raw id. Append a WHERE/ORDER BY clause and pass to
/// `query_row`/`query_map` with `task_from_row` as the row mapper.
pub const TASK_SELECT: &str = "SELECT tasks.id, tasks.title, tasks.priority, tasks.status, \
     agents.name, tasks.tags, tasks.tests, tasks.created_at, tasks.updated_at \
     FROM tasks LEFT JOIN agents ON tasks.executor = agents.id";

/// Row mapper for `TASK_SELECT`'s column order. `tags`/`tests` are stored as
/// JSON-text columns and are parsed back into real JSON arrays here so every
/// command returns the same shape.
pub fn task_from_row(row: &rusqlite::Row) -> rusqlite::Result<Value> {
    let tags_json: String = row.get(5)?;
    let tests_json: String = row.get(6)?;
    Ok(json!({
        "id": row.get::<_, i64>(0)?,
        "title": row.get::<_, String>(1)?,
        "priority": row.get::<_, String>(2)?,
        "status": row.get::<_, String>(3)?,
        "executor": row.get::<_, Option<String>>(4)?,
        "tags": serde_json::from_str::<Value>(&tags_json).unwrap_or_else(|_| json!([])),
        "tests": serde_json::from_str::<Value>(&tests_json).unwrap_or_else(|_| json!([])),
        "created_at": row.get::<_, String>(7)?,
        "updated_at": row.get::<_, String>(8)?,
    }))
}

/// Fetch a single task by id using `TASK_SELECT`/`task_from_row`. Returns an
/// error (not `Ok(None)`) if the task doesn't exist, since every caller needs
/// "task not found" to be a clean error either way.
pub fn fetch_task(conn: &Connection, id: i64) -> Result<Value> {
    let sql = format!("{TASK_SELECT} WHERE tasks.id = ?1");
    conn.query_row(&sql, [id], task_from_row)
        .optional()?
        .ok_or_else(|| anyhow!("task {id} not found"))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The DB `CHECK` constraint guarantees `tags`/`tests` are valid JSON at
    /// insert/update time via `SQLite`'s own `json_valid()`, but that's a
    /// different parser than `serde_json` -- a dialect mismatch between the
    /// two could in principle let something `SQLite` accepts fail to parse
    /// here. `task_from_row` falls back to an empty array rather than
    /// erroring or panicking; verified directly with a synthetic row (a
    /// literal `SELECT`, no table needed) since no normal insert path can
    /// actually produce unparseable content past the CHECK constraint.
    #[test]
    fn task_from_row_falls_back_to_empty_array_on_unparseable_tags_or_tests() {
        let conn = Connection::open_in_memory().unwrap();
        let value = conn
            .query_row(
                "SELECT 1 as id, 'title' as title, 'low' as priority, 'todo' as status, \
                 NULL as executor, 'not valid json' as tags, 'also not valid' as tests, \
                 'ts' as created_at, 'ts' as updated_at",
                [],
                task_from_row,
            )
            .unwrap();
        assert_eq!(value["tags"], json!([]));
        assert_eq!(value["tests"], json!([]));
        // Everything else still comes through normally.
        assert_eq!(value["id"], 1);
        assert_eq!(value["title"], "title");
        assert_eq!(value["executor"], Value::Null);
    }
}
