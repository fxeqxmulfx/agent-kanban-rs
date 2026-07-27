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

pub const TASK_SELECT: &str = "SELECT tasks.id, tasks.title, tasks.priority, tasks.status, \
     CASE WHEN tasks.status != 'done' AND tasks.lease_expires_at > datetime('now') \
          THEN agents.name END, \
     CASE WHEN tasks.status != 'done' AND tasks.lease_expires_at > datetime('now') \
          THEN agents.role END, \
     tasks.tags, tasks.tests, tasks.revision, \
     CASE WHEN tasks.status != 'done' AND tasks.lease_expires_at > datetime('now') \
          THEN tasks.claimed_at END, \
     CASE WHEN tasks.status != 'done' AND tasks.lease_expires_at > datetime('now') \
          THEN tasks.lease_expires_at END, \
     tasks.created_at, tasks.updated_at \
     FROM tasks LEFT JOIN agents ON tasks.executor = agents.id";

pub fn task_from_row(row: &rusqlite::Row) -> rusqlite::Result<Value> {
    let tags_json: String = row.get(6)?;
    let tests_json: String = row.get(7)?;
    Ok(json!({
        "id": row.get::<_, i64>(0)?,
        "title": row.get::<_, String>(1)?,
        "priority": row.get::<_, String>(2)?,
        "status": row.get::<_, String>(3)?,
        "executor": row.get::<_, Option<String>>(4)?,
        "executor_role": row.get::<_, Option<String>>(5)?,
        "tags": serde_json::from_str::<Value>(&tags_json).unwrap_or_else(|_| json!([])),
        "tests": serde_json::from_str::<Value>(&tests_json).unwrap_or_else(|_| json!([])),
        "revision": row.get::<_, i64>(8)?,
        "claimed_at": row.get::<_, Option<String>>(9)?,
        "lease_expires_at": row.get::<_, Option<String>>(10)?,
        "created_at": row.get::<_, String>(11)?,
        "updated_at": row.get::<_, String>(12)?,
    }))
}

pub fn fetch_task(conn: &Connection, id: i64) -> Result<Value> {
    let sql = format!("{TASK_SELECT} WHERE tasks.id = ?1");
    let mut task = conn
        .query_row(&sql, [id], task_from_row)
        .optional()?
        .ok_or_else(|| anyhow!("task {id} not found"))?;
    let object = task
        .as_object_mut()
        .ok_or_else(|| anyhow!("task {id} could not be rendered"))?;
    object.insert(
        "acceptance_results".to_string(),
        fetch_acceptance_results(conn, id)?,
    );
    object.insert(
        "review_history".to_string(),
        fetch_review_history(conn, id)?,
    );
    Ok(task)
}

fn fetch_acceptance_results(conn: &Connection, id: i64) -> Result<Value> {
    let mut stmt = conn.prepare(
        "SELECT revision, criterion_index, criterion, result, evidence,
                executor_name, verified_at
         FROM acceptance_results
         WHERE task_id = ?1
         ORDER BY revision, criterion_index",
    )?;
    let rows = stmt.query_map([id], |row| {
        let criterion: String = row.get(2)?;
        Ok(json!({
            "revision": row.get::<_, i64>(0)?,
            "criterion": row.get::<_, i64>(1)?,
            "specification": serde_json::from_str::<Value>(&criterion)
                .unwrap_or(Value::Null),
            "status": row.get::<_, String>(3)?,
            "evidence": row.get::<_, String>(4)?,
            "executor": row.get::<_, String>(5)?,
            "verified_at": row.get::<_, String>(6)?,
        }))
    })?;
    let values: std::result::Result<Vec<Value>, rusqlite::Error> = rows.collect();
    Ok(Value::Array(values?))
}

fn fetch_review_history(conn: &Connection, id: i64) -> Result<Value> {
    let mut stmt = conn.prepare(
        "SELECT revision, decision, notes, executor_name, created_at
         FROM review_history
         WHERE task_id = ?1
         ORDER BY revision",
    )?;
    let rows = stmt.query_map([id], |row| {
        Ok(json!({
            "revision": row.get::<_, i64>(0)?,
            "decision": row.get::<_, String>(1)?,
            "notes": row.get::<_, String>(2)?,
            "executor": row.get::<_, String>(3)?,
            "created_at": row.get::<_, String>(4)?,
        }))
    })?;
    let values: std::result::Result<Vec<Value>, rusqlite::Error> = rows.collect();
    Ok(Value::Array(values?))
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
                 NULL as executor, NULL as executor_role, 'not valid json' as tags, \
                 'also not valid' as tests, 0 as revision, NULL as claimed_at, \
                 NULL as lease_expires_at, 'ts' as created_at, 'ts' as updated_at",
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
