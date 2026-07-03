use anyhow::{Result, bail};
use rusqlite::{Connection, OptionalExtension};
use serde_json::{Value, json};

const PRIORITIES: [&str; 4] = ["low", "medium", "high", "urgent"];

fn validate_priority(priority: &str) -> Result<()> {
    if !PRIORITIES.contains(&priority) {
        bail!("invalid priority '{priority}': must be one of low, medium, high, urgent");
    }
    Ok(())
}

/// Parse and validate the raw `--test` JSON strings into a JSON array of
/// `{"describe","input","output"}` objects. Returns the serialized array
/// text ready for storage in the `tests` column.
fn build_tests_json(tests: &[String]) -> Result<String> {
    if tests.is_empty() {
        bail!("at least one test is required");
    }
    let mut parsed = Vec::with_capacity(tests.len());
    for (i, raw) in tests.iter().enumerate() {
        let value: Value = serde_json::from_str(raw)
            .map_err(|e| anyhow::anyhow!("test {i}: invalid JSON: {e}"))?;
        let obj = value
            .as_object()
            .ok_or_else(|| anyhow::anyhow!("test {i}: must be a JSON object"))?;
        for field in ["describe", "input", "output"] {
            match obj.get(field) {
                Some(Value::String(_)) => {}
                Some(_) => bail!("test {i}: field '{field}' must be a string"),
                None => bail!("test {i}: missing required field '{field}'"),
            }
        }
        if obj.len() != 3 {
            bail!("test {i}: must contain exactly the fields describe, input, output");
        }
        parsed.push(value);
    }
    Ok(json!(parsed).to_string())
}

/// `agent-kanban add --title T --priority P [--tag t]... --test '<json>' [--test '<json>']...`
/// Each `--test` value is a JSON object `{"describe","input","output"}` — validate
/// that shape in application code (the DB CHECK only guarantees non-empty valid
/// JSON array, not element shape). `tags` needs the same array-of-strings shape
/// check. See todo.md section 2/3.
pub fn add(title: &str, priority: &str, tags: &[String], tests: &[String]) -> Result<Value> {
    let conn = crate::db::open_existing()?;
    add_inner(&conn, title, priority, tags, tests)
}

fn add_inner(
    conn: &Connection,
    title: &str,
    priority: &str,
    tags: &[String],
    tests: &[String],
) -> Result<Value> {
    validate_priority(priority)?;
    let tests_json = build_tests_json(tests)?;
    let tags_json = json!(tags).to_string();

    conn.execute(
        "INSERT INTO tasks (title, priority, tags, tests) VALUES (?1,?2,?3,?4)",
        rusqlite::params![title, priority, tags_json, tests_json],
    )?;
    crate::commands::fetch_task(conn, conn.last_insert_rowid())
}

/// `agent-kanban list [--status S] [--tag T] [--executor A] [--priority P] [--sort priority|created_at]`
/// `--executor` filters by agent NAME but `tasks.executor` stores an integer id —
/// join/subquery through `agents.name`, don't compare the column directly to the
/// string. Sorting by `priority` needs a CASE expression (severity order), not a
/// plain alphabetical ORDER BY. See todo.md section 3.
pub fn list(
    status: Option<String>,
    tag: Option<String>,
    executor: Option<String>,
    priority: Option<String>,
    sort: Option<&str>,
) -> Result<Value> {
    let conn = crate::db::open_existing()?;
    list_inner(&conn, status, tag, executor, priority, sort)
}

fn list_inner(
    conn: &Connection,
    status: Option<String>,
    tag: Option<String>,
    executor: Option<String>,
    priority: Option<String>,
    sort: Option<&str>,
) -> Result<Value> {
    let mut conditions: Vec<String> = Vec::new();
    let mut params: Vec<Box<dyn rusqlite::ToSql>> = Vec::new();

    if let Some(status) = status {
        params.push(Box::new(status));
        conditions.push(format!("tasks.status = ?{}", params.len()));
    }
    if let Some(priority) = priority {
        params.push(Box::new(priority));
        conditions.push(format!("tasks.priority = ?{}", params.len()));
    }
    if let Some(tag) = tag {
        params.push(Box::new(tag));
        conditions.push(format!(
            "EXISTS (SELECT 1 FROM json_each(tasks.tags) je WHERE je.value = ?{})",
            params.len()
        ));
    }
    if let Some(executor) = executor {
        params.push(Box::new(executor));
        conditions.push(format!("agents.name = ?{}", params.len()));
    }

    let mut sql = crate::commands::TASK_SELECT.to_string();
    if !conditions.is_empty() {
        sql.push_str(" WHERE ");
        sql.push_str(&conditions.join(" AND "));
    }

    match sort {
        Some("priority") => {
            sql.push_str(
                " ORDER BY CASE tasks.priority WHEN 'urgent' THEN 0 WHEN 'high' THEN 1 \
                 WHEN 'medium' THEN 2 WHEN 'low' THEN 3 END",
            );
        }
        Some("created_at") => sql.push_str(" ORDER BY tasks.created_at"),
        Some(other) => bail!("invalid sort '{other}': must be 'priority' or 'created_at'"),
        None => sql.push_str(" ORDER BY tasks.id"),
    }

    let mut stmt = conn.prepare(&sql)?;
    let rows = stmt.query_map(rusqlite::params_from_iter(params.iter()), |row| {
        crate::commands::task_from_row(row)
    })?;
    let mut tasks = Vec::new();
    for row in rows {
        tasks.push(row?);
    }
    Ok(json!(tasks))
}

/// `agent-kanban show <id>`
pub fn show(id: i64) -> Result<Value> {
    let conn = crate::db::open_existing()?;
    show_inner(&conn, id)
}

fn show_inner(conn: &Connection, id: i64) -> Result<Value> {
    crate::commands::fetch_task(conn, id)
}

/// `agent-kanban edit <id> [--title T] [--priority P] [--tag t]... [--test '<json>']...`
/// Replaces the given field(s) in place — any `--tag`/`--test` flags replace the
/// whole array, not append. Fails if `executor IS NOT NULL` (must `release`
/// first) OR `status = 'done'` (finished tasks are immutable). Same `tests`
/// shape validation as `add`. Must bump `updated_at`. See todo.md section 3.
pub fn edit(
    id: i64,
    title: Option<String>,
    priority: Option<String>,
    tags: Option<Vec<String>>,
    tests: Option<&[String]>,
) -> Result<Value> {
    let conn = crate::db::open_existing()?;
    edit_inner(&conn, id, title, priority, tags, tests)
}

fn edit_inner(
    conn: &Connection,
    id: i64,
    title: Option<String>,
    priority: Option<String>,
    tags: Option<Vec<String>>,
    tests: Option<&[String]>,
) -> Result<Value> {
    if let Some(priority) = &priority {
        validate_priority(priority)?;
    }
    let tests_json = match tests {
        Some(tests) => Some(build_tests_json(tests)?),
        None => None,
    };
    let tags_json = tags.map(|t| json!(t).to_string());

    let mut sets: Vec<String> = Vec::new();
    let mut params: Vec<Box<dyn rusqlite::ToSql>> = Vec::new();

    if let Some(title) = title {
        params.push(Box::new(title));
        sets.push(format!("title = ?{}", params.len()));
    }
    if let Some(priority) = priority {
        params.push(Box::new(priority));
        sets.push(format!("priority = ?{}", params.len()));
    }
    if let Some(tags_json) = tags_json {
        params.push(Box::new(tags_json));
        sets.push(format!("tags = ?{}", params.len()));
    }
    if let Some(tests_json) = tests_json {
        params.push(Box::new(tests_json));
        sets.push(format!("tests = ?{}", params.len()));
    }
    sets.push("updated_at = datetime('now')".to_string());

    // The claimed/done guard is baked into this same atomic UPDATE rather
    // than a separate check-then-act: a concurrent `claim` landing between a
    // precondition check and the mutation would otherwise let an edit slip
    // through silently. Reproduced directly against `remove`'s analogous
    // DELETE (see remove_inner) before fixing both.
    params.push(Box::new(id));
    let sql = format!(
        "UPDATE tasks SET {} WHERE id = ?{} AND executor IS NULL AND status != 'done'",
        sets.join(", "),
        params.len()
    );
    let changed = conn.execute(&sql, rusqlite::params_from_iter(params.iter()))?;

    if changed == 0 {
        return Err(diagnose_mutation_guard_failure(
            conn,
            id,
            "editing",
            &format!("task {id} is done; finished tasks are immutable"),
        ));
    }

    crate::commands::fetch_task(conn, id)
}

/// After an atomically-guarded `UPDATE`/`DELETE` on `tasks` affects 0 rows,
/// figure out why: missing, claimed, or done. `verb` is used in the
/// "claimed" message ("editing"/"removing"); `done_msg` is the full,
/// already-formatted "done" error text (the two callers phrase it
/// differently). If the row's state changed yet again between the failed
/// guarded statement and this diagnostic read (an extremely narrow window),
/// report that plainly rather than guessing.
fn diagnose_mutation_guard_failure(
    conn: &Connection,
    id: i64,
    verb: &str,
    done_msg: &str,
) -> anyhow::Error {
    let row: rusqlite::Result<Option<(Option<i64>, String)>> = conn
        .query_row(
            "SELECT executor, status FROM tasks WHERE id = ?1",
            [id],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .optional();
    match row {
        Err(e) => e.into(),
        Ok(None) => anyhow::anyhow!("task {id} not found"),
        Ok(Some((executor, _))) if executor.is_some() => {
            anyhow::anyhow!("task {id} is claimed; release it before {verb}")
        }
        Ok(Some((_, status))) if status == "done" => anyhow::anyhow!("{done_msg}"),
        Ok(Some(_)) => {
            anyhow::anyhow!(
                "task {id} could not be modified (state changed concurrently; try again)"
            )
        }
    }
}

/// `agent-kanban remove <id>`
/// Hard-delete the task row. Fails if `executor IS NOT NULL` (must `release`
/// first) OR `status = 'done'` (finished tasks can't be deleted). See todo.md
/// section 3.
pub fn remove(id: i64) -> Result<Value> {
    let conn = crate::db::open_existing()?;
    remove_inner(&conn, id)
}

fn remove_inner(conn: &Connection, id: i64) -> Result<Value> {
    // The claimed/done guard is baked into this same atomic DELETE rather
    // than a separate check-then-act: a plain `SELECT` check followed by an
    // unconditional `DELETE FROM tasks WHERE id = ?1` has a real race window
    // — if a concurrent `claim` lands between the two statements, the
    // DELETE still fires unconditionally and silently destroys an actively
    // claimed task. Confirmed by direct reproduction before this fix.
    let changed = conn.execute(
        "DELETE FROM tasks WHERE id = ?1 AND executor IS NULL AND status != 'done'",
        [id],
    )?;

    if changed == 0 {
        return Err(diagnose_mutation_guard_failure(
            conn,
            id,
            "removing",
            &format!("task {id} is done; finished tasks can't be removed"),
        ));
    }

    Ok(json!({ "removed": id }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use rusqlite::Connection;

    fn setup() -> Connection {
        let conn = Connection::open_in_memory().unwrap();
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
               priority TEXT NOT NULL CHECK (priority IN ('low','medium','high','urgent')),
               status TEXT NOT NULL DEFAULT 'todo' CHECK (status IN ('backlog','todo','in_progress','review','done')),
               executor INTEGER REFERENCES agents(id),
               tags TEXT NOT NULL DEFAULT '[]' CHECK (json_valid(tags)),
               tests TEXT NOT NULL CHECK (json_valid(tests) AND json_array_length(tests) > 0),
               created_at TEXT NOT NULL DEFAULT (datetime('now')),
               updated_at TEXT NOT NULL DEFAULT (datetime('now'))
             );",
        )
        .unwrap();
        conn
    }

    fn sample_test(describe: &str) -> String {
        json!({"describe": describe, "input": "in", "output": "out"}).to_string()
    }

    #[test]
    fn add_rejects_bad_priority() {
        let conn = setup();
        let err = add_inner(&conn, "title", "urgentish", &[], &[sample_test("d")]).unwrap_err();
        assert!(err.to_string().contains("invalid priority"));
    }

    #[test]
    fn add_rejects_empty_tests() {
        let conn = setup();
        let err = add_inner(&conn, "title", "low", &[], &[]).unwrap_err();
        assert!(err.to_string().contains("at least one test"));
    }

    #[test]
    fn add_rejects_test_missing_field() {
        let conn = setup();
        let bad = json!({"describe": "d", "input": "i"}).to_string();
        let err = add_inner(&conn, "title", "low", &[], &[bad]).unwrap_err();
        assert!(err.to_string().contains("test 0"));
        assert!(err.to_string().contains("output"));
    }

    #[test]
    fn add_rejects_test_with_wrong_field_type() {
        let conn = setup();
        // "describe" present but a number, not a string.
        let bad = json!({"describe": 123, "input": "i", "output": "o"}).to_string();
        let err = add_inner(&conn, "title", "low", &[], &[bad]).unwrap_err();
        assert!(err.to_string().contains("test 0"));
        assert!(err.to_string().contains("describe"));
        assert!(err.to_string().contains("must be a string"));
    }

    #[test]
    fn add_rejects_test_with_extra_field() {
        let conn = setup();
        let bad = json!({"describe": "d", "input": "i", "output": "o", "extra": "x"}).to_string();
        let err = add_inner(&conn, "title", "low", &[], &[bad]).unwrap_err();
        assert!(err.to_string().contains("test 0"));
        assert!(err.to_string().contains("exactly the fields"));
    }

    #[test]
    fn add_and_show_round_trip_preserves_json_arrays() {
        let conn = setup();
        let created = add_inner(
            &conn,
            "my task",
            "high",
            &["backend".to_string(), "urgent-fix".to_string()],
            &[sample_test("d1"), sample_test("d2")],
        )
        .unwrap();
        let id = created["id"].as_i64().unwrap();

        let fetched = show_inner(&conn, id).unwrap();
        assert_eq!(fetched["title"], "my task");
        assert_eq!(fetched["priority"], "high");
        assert_eq!(fetched["status"], "todo");
        assert!(fetched["tags"].is_array());
        assert_eq!(fetched["tags"].as_array().unwrap().len(), 2);
        assert!(fetched["tests"].is_array());
        assert_eq!(fetched["tests"].as_array().unwrap().len(), 2);
        assert_eq!(fetched["tests"][0]["describe"], "d1");
    }

    #[test]
    fn list_filters_by_status() {
        let conn = setup();
        add_inner(&conn, "t1", "low", &[], &[sample_test("d")]).unwrap();
        let created2 = add_inner(&conn, "t2", "low", &[], &[sample_test("d")]).unwrap();
        conn.execute(
            "UPDATE tasks SET status = 'in_progress' WHERE id = ?1",
            [created2["id"].as_i64().unwrap()],
        )
        .unwrap();

        let result = list_inner(
            &conn,
            Some("in_progress".to_string()),
            None,
            None,
            None,
            None,
        )
        .unwrap();
        let arr = result.as_array().unwrap();
        assert_eq!(arr.len(), 1);
        assert_eq!(arr[0]["title"], "t2");
    }

    #[test]
    fn list_filters_by_tag() {
        let conn = setup();
        add_inner(
            &conn,
            "t1",
            "low",
            &["alpha".to_string()],
            &[sample_test("d")],
        )
        .unwrap();
        add_inner(
            &conn,
            "t2",
            "low",
            &["beta".to_string()],
            &[sample_test("d")],
        )
        .unwrap();

        let result = list_inner(&conn, None, Some("alpha".to_string()), None, None, None).unwrap();
        let arr = result.as_array().unwrap();
        assert_eq!(arr.len(), 1);
        assert_eq!(arr[0]["title"], "t1");
    }

    #[test]
    fn list_filters_by_executor_name() {
        let conn = setup();
        conn.execute("INSERT INTO agents (name) VALUES ('agent-a')", [])
            .unwrap();
        conn.execute("INSERT INTO agents (name) VALUES ('agent-b')", [])
            .unwrap();
        let t1 = add_inner(&conn, "t1", "low", &[], &[sample_test("d")]).unwrap();
        add_inner(&conn, "t2", "low", &[], &[sample_test("d")]).unwrap();
        conn.execute(
            "UPDATE tasks SET executor = (SELECT id FROM agents WHERE name = 'agent-a') WHERE id = ?1",
            [t1["id"].as_i64().unwrap()],
        )
        .unwrap();

        let result =
            list_inner(&conn, None, None, Some("agent-a".to_string()), None, None).unwrap();
        let arr = result.as_array().unwrap();
        assert_eq!(arr.len(), 1);
        assert_eq!(arr[0]["title"], "t1");
    }

    #[test]
    fn list_filters_by_priority() {
        let conn = setup();
        add_inner(&conn, "t1", "urgent", &[], &[sample_test("d")]).unwrap();
        add_inner(&conn, "t2", "low", &[], &[sample_test("d")]).unwrap();

        let result = list_inner(&conn, None, None, None, Some("urgent".to_string()), None).unwrap();
        let arr = result.as_array().unwrap();
        assert_eq!(arr.len(), 1);
        assert_eq!(arr[0]["title"], "t1");
    }

    #[test]
    fn list_sorts_by_priority_severity() {
        let conn = setup();
        add_inner(&conn, "low-task", "low", &[], &[sample_test("d")]).unwrap();
        add_inner(&conn, "urgent-task", "urgent", &[], &[sample_test("d")]).unwrap();
        add_inner(&conn, "medium-task", "medium", &[], &[sample_test("d")]).unwrap();
        add_inner(&conn, "high-task", "high", &[], &[sample_test("d")]).unwrap();

        let result = list_inner(&conn, None, None, None, None, Some("priority")).unwrap();
        let arr = result.as_array().unwrap();
        let titles: Vec<&str> = arr.iter().map(|t| t["title"].as_str().unwrap()).collect();
        assert_eq!(
            titles,
            vec!["urgent-task", "high-task", "medium-task", "low-task"]
        );
    }

    #[test]
    fn list_rejects_bad_sort() {
        let conn = setup();
        let err = list_inner(&conn, None, None, None, None, Some("bogus")).unwrap_err();
        assert!(err.to_string().contains("invalid sort"));
    }

    #[test]
    fn edit_blocked_when_claimed() {
        let conn = setup();
        conn.execute("INSERT INTO agents (name) VALUES ('agent-a')", [])
            .unwrap();
        let created = add_inner(&conn, "t1", "low", &[], &[sample_test("d")]).unwrap();
        let id = created["id"].as_i64().unwrap();
        conn.execute(
            "UPDATE tasks SET executor = (SELECT id FROM agents WHERE name = 'agent-a') WHERE id = ?1",
            [id],
        )
        .unwrap();

        let err =
            edit_inner(&conn, id, Some("new title".to_string()), None, None, None).unwrap_err();
        assert!(err.to_string().contains("claimed"));
    }

    #[test]
    fn edit_blocked_when_done() {
        let conn = setup();
        let created = add_inner(&conn, "t1", "low", &[], &[sample_test("d")]).unwrap();
        let id = created["id"].as_i64().unwrap();
        conn.execute("UPDATE tasks SET status = 'done' WHERE id = ?1", [id])
            .unwrap();

        let err =
            edit_inner(&conn, id, Some("new title".to_string()), None, None, None).unwrap_err();
        assert!(err.to_string().contains("immutable"));
    }

    #[test]
    fn edit_replaces_tags_and_tests_and_bumps_updated_at() {
        let conn = setup();
        let created = add_inner(
            &conn,
            "t1",
            "low",
            &["old-tag".to_string()],
            &[sample_test("old")],
        )
        .unwrap();
        let id = created["id"].as_i64().unwrap();
        let old_updated_at = created["updated_at"].as_str().unwrap().to_string();

        // force a distinguishable updated_at by backdating created row's updated_at
        conn.execute(
            "UPDATE tasks SET updated_at = '2000-01-01T00:00:00' WHERE id = ?1",
            [id],
        )
        .unwrap();

        let result = edit_inner(
            &conn,
            id,
            None,
            None,
            Some(vec!["new-tag".to_string()]),
            Some(&[sample_test("new")]),
        )
        .unwrap();

        assert_eq!(result["tags"].as_array().unwrap().len(), 1);
        assert_eq!(result["tags"][0], "new-tag");
        assert_eq!(result["tests"].as_array().unwrap().len(), 1);
        assert_eq!(result["tests"][0]["describe"], "new");
        assert_ne!(
            result["updated_at"].as_str().unwrap(),
            "2000-01-01T00:00:00"
        );
        let _ = old_updated_at;
    }

    #[test]
    fn edit_applies_valid_priority_change() {
        let conn = setup();
        let created = add_inner(&conn, "t1", "low", &[], &[sample_test("d")]).unwrap();
        let id = created["id"].as_i64().unwrap();

        let result = edit_inner(&conn, id, None, Some("urgent".to_string()), None, None).unwrap();
        assert_eq!(result["priority"], "urgent");

        // Confirm it actually persisted, not just returned.
        let fetched = show_inner(&conn, id).unwrap();
        assert_eq!(fetched["priority"], "urgent");
    }

    #[test]
    fn edit_rejects_invalid_priority() {
        let conn = setup();
        let created = add_inner(&conn, "t1", "low", &[], &[sample_test("d")]).unwrap();
        let id = created["id"].as_i64().unwrap();

        let err =
            edit_inner(&conn, id, None, Some("urgentish".to_string()), None, None).unwrap_err();
        assert!(err.to_string().contains("invalid priority"));

        // Must not have been partially applied.
        let fetched = show_inner(&conn, id).unwrap();
        assert_eq!(fetched["priority"], "low");
    }

    #[test]
    fn edit_nonexistent_task_fails_with_not_found() {
        let conn = setup();
        let err =
            edit_inner(&conn, 999, Some("new title".to_string()), None, None, None).unwrap_err();
        assert!(err.to_string().contains("not found"));
    }

    #[test]
    fn remove_nonexistent_task_fails_with_not_found() {
        let conn = setup();
        let err = remove_inner(&conn, 999).unwrap_err();
        assert!(err.to_string().contains("not found"));
    }

    #[test]
    fn remove_blocked_when_claimed() {
        let conn = setup();
        conn.execute("INSERT INTO agents (name) VALUES ('agent-a')", [])
            .unwrap();
        let created = add_inner(&conn, "t1", "low", &[], &[sample_test("d")]).unwrap();
        let id = created["id"].as_i64().unwrap();
        conn.execute(
            "UPDATE tasks SET executor = (SELECT id FROM agents WHERE name = 'agent-a') WHERE id = ?1",
            [id],
        )
        .unwrap();

        let err = remove_inner(&conn, id).unwrap_err();
        assert!(err.to_string().contains("claimed"));
    }

    #[test]
    fn remove_blocked_when_done() {
        let conn = setup();
        let created = add_inner(&conn, "t1", "low", &[], &[sample_test("d")]).unwrap();
        let id = created["id"].as_i64().unwrap();
        conn.execute("UPDATE tasks SET status = 'done' WHERE id = ?1", [id])
            .unwrap();

        let err = remove_inner(&conn, id).unwrap_err();
        assert!(err.to_string().contains("can't be removed"));
    }

    #[test]
    fn diagnose_mutation_guard_failure_propagates_non_missing_row_errors_uncaught() {
        // The `Ok(None) => "not found"` mapping only applies when the
        // diagnostic SELECT finds no *row* -- confirmed here by forcing a
        // different kind of failure (the table itself is gone, not just the
        // row) so it comes through as its own genuine error rather than
        // being misreported as "not found".
        let conn = setup();
        let created = add_inner(&conn, "t1", "low", &[], &[sample_test("d")]).unwrap();
        let id = created["id"].as_i64().unwrap();
        conn.execute_batch("DROP TABLE tasks;").unwrap();

        let err = diagnose_mutation_guard_failure(&conn, id, "editing", "done");
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
    fn diagnose_mutation_guard_failure_reports_state_changed_concurrently() {
        // The catch-all last arm fires when the row is neither claimed nor
        // done -- i.e. the diagnostic read finds nothing that explains why
        // the guarded UPDATE/DELETE it followed affected 0 rows. In
        // production that means the row's state changed yet again between
        // the two statements (an extremely narrow window); here it's driven
        // directly against a task that's simply in its normal unclaimed,
        // not-done state, since this function only reports on what it reads
        // and doesn't care whether it was actually called after a real
        // guard failure.
        let conn = setup();
        let created = add_inner(&conn, "t1", "low", &[], &[sample_test("d")]).unwrap();
        let id = created["id"].as_i64().unwrap();

        let err = diagnose_mutation_guard_failure(&conn, id, "editing", "done");
        assert!(
            err.to_string().contains("state changed concurrently"),
            "unexpected message: {err}"
        );
    }

    #[test]
    fn mutation_guard_sql_rejects_claimed_or_done_rows_atomically() {
        // Regression test for the TOCTOU this fix closes: a naive
        // check-then-act (SELECT, then an unconditional DELETE/UPDATE WHERE
        // id = ?1) has a window where a concurrent `claim` between the two
        // statements would let the DELETE/UPDATE proceed anyway. Confirmed
        // by direct reproduction against the old code before this fix. This
        // test drives the exact guarded SQL `edit_inner`/`remove_inner` now
        // use directly, proving the guard is enforced atomically within the
        // single statement rather than relying on a prior check — against
        // the OLD unconditional `DELETE FROM tasks WHERE id = ?1`, this test
        // would fail (both rows would be deleted).
        const GUARDED_DELETE: &str =
            "DELETE FROM tasks WHERE id = ?1 AND executor IS NULL AND status != 'done'";

        let conn = setup();
        conn.execute("INSERT INTO agents (name) VALUES ('agent-a')", [])
            .unwrap();

        let claimed = add_inner(&conn, "claimed", "low", &[], &[sample_test("d")]).unwrap();
        let claimed_id = claimed["id"].as_i64().unwrap();
        conn.execute(
            "UPDATE tasks SET executor = (SELECT id FROM agents WHERE name = 'agent-a') WHERE id = ?1",
            [claimed_id],
        )
        .unwrap();

        let done = add_inner(&conn, "done", "low", &[], &[sample_test("d")]).unwrap();
        let done_id = done["id"].as_i64().unwrap();
        conn.execute("UPDATE tasks SET status = 'done' WHERE id = ?1", [done_id])
            .unwrap();

        let changed_claimed = conn.execute(GUARDED_DELETE, [claimed_id]).unwrap();
        assert_eq!(
            changed_claimed, 0,
            "claimed task must not be deleted by the guarded statement"
        );

        let changed_done = conn.execute(GUARDED_DELETE, [done_id]).unwrap();
        assert_eq!(
            changed_done, 0,
            "done task must not be deleted by the guarded statement"
        );

        // Both rows must genuinely still exist afterward, not just report
        // changed == 0 while something else silently happened.
        assert!(show_inner(&conn, claimed_id).is_ok());
        assert!(show_inner(&conn, done_id).is_ok());
    }

    #[test]
    fn remove_succeeds_otherwise() {
        let conn = setup();
        let created = add_inner(&conn, "t1", "low", &[], &[sample_test("d")]).unwrap();
        let id = created["id"].as_i64().unwrap();

        let result = remove_inner(&conn, id).unwrap();
        assert_eq!(result["removed"], id);

        let err = show_inner(&conn, id).unwrap_err();
        assert!(err.to_string().contains("not found"));
    }

    #[test]
    fn edit_rejects_empty_tests() {
        let conn = setup();
        let created = add_inner(&conn, "t1", "low", &[], &[sample_test("d")]).unwrap();
        let id = created["id"].as_i64().unwrap();

        let err = edit_inner(&conn, id, None, None, None, Some(&[])).unwrap_err();
        assert!(err.to_string().contains("at least one test"));
    }

    #[test]
    fn db_check_rejects_invalid_priority() {
        let conn = setup();
        let result = conn.execute(
            "INSERT INTO tasks (title, priority, tests) VALUES ('t', 'bogus-priority', '[\"x\"]')",
            [],
        );
        assert_constraint_violation(result);
    }

    fn assert_constraint_violation(result: rusqlite::Result<usize>) {
        match result {
            Err(rusqlite::Error::SqliteFailure(ffi_err, _)) => {
                assert_eq!(ffi_err.code, rusqlite::ErrorCode::ConstraintViolation);
            }
            other => panic!("expected SqliteFailure constraint violation, got {other:?}"),
        }
    }

    #[test]
    fn db_check_rejects_empty_tests_array() {
        let conn = setup();
        let result = conn.execute(
            "INSERT INTO tasks (title, priority, tests) VALUES ('t', 'low', '[]')",
            [],
        );
        assert_constraint_violation(result);
    }

    #[test]
    fn db_check_rejects_invalid_json_tests() {
        let conn = setup();
        let result = conn.execute(
            "INSERT INTO tasks (title, priority, tests) VALUES ('t', 'low', 'not valid json')",
            [],
        );
        assert_constraint_violation(result);
    }

    #[test]
    fn db_check_rejects_non_array_tests() {
        let conn = setup();
        let result = conn.execute(
            "INSERT INTO tasks (title, priority, tests) VALUES ('t', 'low', '{\"not\":\"an array\"}')",
            [],
        );
        assert_constraint_violation(result);
    }
}
