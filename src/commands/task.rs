use anyhow::{Result, bail};
use rusqlite::{Connection, OptionalExtension, TransactionBehavior};
use serde_json::{Value, json};

use super::{PRIORITIES, STATUSES, deps, view};

fn validate_priority(priority: &str) -> Result<()> {
    if !PRIORITIES.contains(&priority) {
        bail!("invalid priority '{priority}': must be one of low, medium, high, urgent");
    }
    Ok(())
}

fn validate_status(status: &str) -> Result<()> {
    if !STATUSES.contains(&status) {
        bail!("invalid status '{status}': must be one of backlog, todo, in_progress, review, done");
    }
    Ok(())
}

/// Titles are shown one per line, so whitespace (newlines included) is
/// collapsed to single spaces.
fn clean_title(title: &str) -> Result<String> {
    let title = title.split_whitespace().collect::<Vec<_>>().join(" ");
    if title.is_empty() {
        bail!("title must not be empty");
    }
    Ok(title)
}

/// Turn the `--test DESC INPUT OUTPUT` triples into the JSON array of
/// `{"describe","input","output"}` stored in the `tests` column.
fn tests_json(tests: &[Vec<String>]) -> Result<String> {
    if tests.is_empty() {
        bail!("at least one test is required");
    }
    let mut specs = Vec::with_capacity(tests.len());
    for (index, test) in tests.iter().enumerate() {
        let [describe, input, output] = test.as_slice() else {
            bail!("test {index}: needs DESC INPUT OUTPUT");
        };
        specs.push(json!({"describe": describe, "input": input, "output": output}));
    }
    Ok(Value::Array(specs).to_string())
}

pub struct NewTask {
    pub title: String,
    pub priority: String,
    pub tags: Vec<String>,
    /// `[DESC, INPUT, OUTPUT]` per acceptance test; at least one.
    pub tests: Vec<Vec<String>>,
    /// Prerequisite task ids.
    pub after: Vec<i64>,
}

/// `add` -> `#ID todo[ after:IDS]`, where `after` lists prerequisites that
/// are not done yet.
pub fn add(task: &NewTask) -> Result<String> {
    let mut conn = crate::db::open_existing()?;
    add_inner(&mut conn, task)
}

fn add_inner(conn: &mut Connection, task: &NewTask) -> Result<String> {
    validate_priority(&task.priority)?;
    let title = clean_title(&task.title)?;
    let tests = tests_json(&task.tests)?;
    let tags = json!(task.tags).to_string();

    // One transaction: if a prerequisite is unknown the task is not created.
    let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
    tx.execute(
        "INSERT INTO tasks (title, priority, tags, tests) VALUES (?1, ?2, ?3, ?4)",
        rusqlite::params![title, task.priority, tags, tests],
    )?;
    let id = tx.last_insert_rowid();
    deps::add_prerequisites(&tx, id, &task.after)?;
    let open = deps::open_prerequisites(&tx, id)?;
    tx.commit()?;
    Ok(format!(
        "{}{}",
        view::ack(id, "todo"),
        deps::labeled("after", &open)
    ))
}

/// Rows `list` prints when `--limit` is not given. A big board must not flood
/// an agent's context because of one careless `list`: 300 tasks cost about
/// 5,400 tokens, these rows about 370.
pub const DEFAULT_LIST_LIMIT: usize = 20;

pub struct ListFilter {
    pub status: Option<String>,
    pub tag: Option<String>,
    /// Only tasks this agent currently holds.
    pub executor: Option<String>,
    pub priority: Option<String>,
    /// Include done tasks even without `--status done`.
    pub all: bool,
    /// Most rows to print; 0 means no limit.
    pub limit: usize,
}

/// `list` -> one line per task (see [`view::line`]), most actionable first:
/// `in_progress`, review, todo, backlog, done; by priority, then id. Done
/// tasks are hidden unless `--all` or `--status done`; footers say how many
/// rows were left out, and how to see them.
pub fn list(filter: &ListFilter) -> Result<String> {
    let conn = crate::db::open_existing()?;
    list_inner(&conn, filter)
}

const LIST_FROM: &str = "FROM tasks t LEFT JOIN agents a ON t.executor = a.id";

/// The SQL conditions a [`ListFilter`] turns into, and their parameters.
struct Scope {
    /// Conditions every row must meet (priority, tag, executor).
    matching: String,
    /// Which statuses are shown.
    visible: String,
    /// Whether done tasks are left out only because nothing asked for them.
    hides_done: bool,
    params: Vec<String>,
}

fn scope(filter: &ListFilter) -> Result<Scope> {
    let mut conditions: Vec<String> = Vec::new();
    let mut params: Vec<String> = Vec::new();

    if let Some(priority) = &filter.priority {
        validate_priority(priority)?;
        params.push(priority.clone());
        conditions.push(format!("t.priority = ?{}", params.len()));
    }
    if let Some(tag) = &filter.tag {
        params.push(tag.clone());
        conditions.push(format!(
            "EXISTS (SELECT 1 FROM json_each(t.tags) je WHERE je.value = ?{})",
            params.len()
        ));
    }
    if let Some(executor) = &filter.executor {
        params.push(executor.clone());
        conditions.push(format!(
            "a.name = ?{} AND t.status != 'done' AND t.lease_expires_at > datetime('now')",
            params.len()
        ));
    }
    let matching = if conditions.is_empty() {
        "1".to_string()
    } else {
        conditions.join(" AND ")
    };

    let mut hides_done = false;
    let visible = if let Some(status) = &filter.status {
        validate_status(status)?;
        params.push(status.clone());
        format!("t.status = ?{}", params.len())
    } else if filter.all {
        "1".to_string()
    } else {
        hides_done = true;
        "t.status != 'done'".to_string()
    };
    Ok(Scope {
        matching,
        visible,
        hides_done,
        params,
    })
}

fn list_inner(conn: &Connection, filter: &ListFilter) -> Result<String> {
    let scope = scope(filter)?;
    let count = |extra: &str| -> Result<i64> {
        Ok(conn.query_row(
            &format!(
                "SELECT COUNT(*) {LIST_FROM} WHERE {} AND {extra}",
                scope.matching
            ),
            rusqlite::params_from_iter(scope.params.iter()),
            |row| row.get(0),
        )?)
    };
    let total = count(&scope.visible)?;
    let hidden_done = if scope.hides_done {
        count("t.status = 'done'")?
    } else {
        0
    };

    let mut lines: Vec<String> = rows(conn, &scope, filter.limit)?
        .iter()
        .map(view::line)
        .collect();
    let shown = i64::try_from(lines.len())?;
    if total > shown {
        lines.push(format!("+{} more (--limit 0 = all)", total - shown));
    }
    if hidden_done > 0 {
        lines.push(format!("+{hidden_done} done hidden (--all)"));
    }
    if lines.is_empty() {
        lines.push("no tasks".to_string());
    }
    Ok(lines.join("\n"))
}

/// The rows to print, most actionable first; `limit` 0 means all of them.
fn rows(conn: &Connection, scope: &Scope, limit: usize) -> Result<Vec<view::Row>> {
    let limit = if limit == 0 {
        -1
    } else {
        i64::try_from(limit)?
    };
    let mut stmt = conn.prepare(&format!(
        "SELECT t.id, t.priority, t.status,
                CASE WHEN t.status != 'done' AND t.lease_expires_at > datetime('now')
                     THEN a.name END,
                t.title,
                (SELECT group_concat(d.depends_on) FROM task_deps d
                   JOIN tasks p ON p.id = d.depends_on
                  WHERE d.task_id = t.id AND p.status != 'done')
         {LIST_FROM}
         WHERE {} AND {}
         ORDER BY CASE t.status WHEN 'in_progress' THEN 0 WHEN 'review' THEN 1
                                WHEN 'todo' THEN 2 WHEN 'backlog' THEN 3 ELSE 4 END,
                  CASE t.priority WHEN 'urgent' THEN 0 WHEN 'high' THEN 1
                                  WHEN 'medium' THEN 2 ELSE 3 END,
                  t.id
         LIMIT {limit}",
        scope.matching, scope.visible
    ))?;
    let rows = stmt.query_map(rusqlite::params_from_iter(scope.params.iter()), |row| {
        let after: Option<String> = row.get(5)?;
        let mut after: Vec<i64> = after
            .iter()
            .flat_map(|ids| ids.split(','))
            .filter_map(|id| id.parse().ok())
            .collect();
        after.sort_unstable();
        Ok(view::Row {
            id: row.get(0)?,
            priority: row.get(1)?,
            status: row.get(2)?,
            executor: row.get(3)?,
            title: row.get(4)?,
            after,
        })
    })?;
    Ok(rows.collect::<std::result::Result<_, _>>()?)
}

/// `show ID [--history]` -> a header line and `|` rows (see [`view::show`]).
pub fn show(id: i64, history: bool) -> Result<String> {
    let conn = crate::db::open_existing()?;
    view::show(&conn, id, history)
}

#[derive(Default)]
pub struct TaskEdit {
    pub title: Option<String>,
    pub priority: Option<String>,
    /// Replaces the whole tag set.
    pub tags: Option<Vec<String>>,
    /// Replaces the whole test set (`[DESC, INPUT, OUTPUT]` each).
    pub tests: Option<Vec<Vec<String>>>,
    /// Prerequisites to add.
    pub after: Vec<i64>,
    /// Prerequisites to remove.
    pub drop_after: Vec<i64>,
}

impl TaskEdit {
    const fn is_empty(&self) -> bool {
        self.title.is_none()
            && self.priority.is_none()
            && self.tags.is_none()
            && self.tests.is_none()
            && self.after.is_empty()
            && self.drop_after.is_empty()
    }
}

/// `edit ID ...` -> `#ID STATUS[ after:IDS]`. Fields that are given replace
/// the old value; `--after` / `--drop-after` add / remove single
/// prerequisites. All changes apply together or not at all.
pub fn edit(id: i64, edit: &TaskEdit) -> Result<String> {
    let mut conn = crate::db::open_existing()?;
    edit_inner(&mut conn, id, edit)
}

fn edit_inner(conn: &mut Connection, id: i64, edit: &TaskEdit) -> Result<String> {
    if edit.is_empty() {
        bail!(
            "nothing to change: pass --title, --priority, --tag, --test, --after or --drop-after"
        );
    }
    if let Some(priority) = &edit.priority {
        validate_priority(priority)?;
    }
    let title = edit.title.as_deref().map(clean_title).transpose()?;
    let tests = edit.tests.as_deref().map(tests_json).transpose()?;
    let tags = edit.tags.as_ref().map(|tags| json!(tags).to_string());

    let mut sets: Vec<String> = Vec::new();
    let mut params: Vec<Box<dyn rusqlite::ToSql>> = Vec::new();
    let mut set = |column: &str, value: String| {
        params.push(Box::new(value));
        sets.push(format!("{column} = ?{}", params.len()));
    };
    if let Some(title) = title {
        set("title", title);
    }
    if let Some(priority) = &edit.priority {
        set("priority", priority.clone());
    }
    if let Some(tags) = tags {
        set("tags", tags);
    }
    if let Some(tests) = tests {
        set("tests", tests);
    }
    sets.push("updated_at = datetime('now')".to_string());
    params.push(Box::new(id));

    // The guard (unclaimed, not in review, not done) is part of the UPDATE
    // itself so a concurrent claim cannot slip in between a check and the
    // write; it also gates edits that only touch prerequisites, since the
    // UPDATE always runs.
    let sql = format!(
        "UPDATE tasks SET {} WHERE id = ?{}
         AND (executor IS NULL OR lease_expires_at IS NULL OR lease_expires_at <= datetime('now'))
         AND status NOT IN ('review', 'done')
         RETURNING status",
        sets.join(", "),
        params.len()
    );
    let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
    let status: Option<String> = tx
        .query_row(&sql, rusqlite::params_from_iter(params.iter()), |row| {
            row.get(0)
        })
        .optional()?;
    let Some(status) = status else {
        return Err(diagnose_mutation_guard_failure(
            &tx,
            id,
            "editing",
            &format!("task {id} is done; finished tasks are immutable"),
        ));
    };
    deps::add_prerequisites(&tx, id, &edit.after)?;
    deps::drop_prerequisites(&tx, id, &edit.drop_after)?;
    let open = deps::open_prerequisites(&tx, id)?;
    tx.commit()?;
    Ok(format!(
        "{}{}",
        view::ack(id, &status),
        deps::labeled("after", &open)
    ))
}

/// Why a guarded UPDATE/DELETE changed nothing, when the task's own state
/// explains it. `Ok(None)`: the state does not (it must have changed again
/// after the statement ran).
fn explain_guard_failure(
    conn: &Connection,
    id: i64,
    verb: &str,
    done_msg: &str,
) -> Result<Option<String>> {
    let row: Option<(bool, String)> = conn
        .query_row(
            "SELECT executor IS NOT NULL AND lease_expires_at > datetime('now'), status
             FROM tasks WHERE id = ?1",
            [id],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .optional()?;
    Ok(match row {
        None => Some(format!("task {id} not found")),
        Some((true, _)) => Some(format!("task {id} is claimed; release it before {verb}")),
        Some((_, status)) if status == "done" => Some(done_msg.to_string()),
        Some((_, status)) if status == "review" => Some(format!(
            "task {id} is in review; request changes before {verb}"
        )),
        Some(_) => None,
    })
}

fn diagnose_mutation_guard_failure(
    conn: &Connection,
    id: i64,
    verb: &str,
    done_msg: &str,
) -> anyhow::Error {
    match explain_guard_failure(conn, id, verb, done_msg) {
        Err(e) => e,
        Ok(Some(message)) => anyhow::anyhow!(message),
        Ok(None) => anyhow::anyhow!(
            "task {id} could not be modified (state changed concurrently; try again)"
        ),
    }
}

/// `remove ID` -> `#ID removed`.
pub fn remove(id: i64) -> Result<String> {
    let conn = crate::db::open_existing()?;
    remove_inner(&conn, id)
}

fn remove_inner(conn: &Connection, id: i64) -> Result<String> {
    let changed = conn.execute(
        "DELETE FROM tasks WHERE id = ?1
         AND (executor IS NULL OR lease_expires_at IS NULL OR lease_expires_at <= datetime('now'))
         AND status NOT IN ('review', 'done')
         AND NOT EXISTS (SELECT 1 FROM task_deps WHERE depends_on = ?1)",
        [id],
    )?;
    if changed == 0 {
        return Err(diagnose_remove_failure(conn, id));
    }
    Ok(view::ack(id, "removed"))
}

fn diagnose_remove_failure(conn: &Connection, id: i64) -> anyhow::Error {
    let done_msg = format!("task {id} is done; finished tasks can't be removed");
    match explain_guard_failure(conn, id, "removing", &done_msg) {
        Err(e) => e,
        Ok(Some(message)) => anyhow::anyhow!(message),
        Ok(None) => match deps::open_dependents(conn, id) {
            Err(e) => e,
            Ok(waiting) if !waiting.is_empty() => anyhow::anyhow!(
                "task {id} is a prerequisite of {}; remove those tasks or detach them \
                 (edit --drop-after {id}) first",
                deps::join(&waiting)
            ),
            Ok(_) => anyhow::anyhow!(
                "task {id} could not be removed (state changed concurrently; try again)"
            ),
        },
    }
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

    fn spec(describe: &str) -> Vec<String> {
        vec![describe.to_string(), "in".to_string(), "out".to_string()]
    }

    fn new_task(title: &str, priority: &str) -> NewTask {
        NewTask {
            title: title.to_string(),
            priority: priority.to_string(),
            tags: vec![],
            tests: vec![spec("d")],
            after: vec![],
        }
    }

    /// The id in an `add` reply such as `#3 todo after:1`. (Not
    /// `last_insert_rowid`: adding prerequisites inserts further rows.)
    fn id_in(reply: &str) -> i64 {
        reply[1..].split(' ').next().unwrap().parse().unwrap()
    }

    /// Add a task and return its id.
    fn add_id(conn: &mut Connection, title: &str, priority: &str) -> i64 {
        id_in(&add_inner(conn, &new_task(title, priority)).unwrap())
    }

    /// Add a task waiting for `after` and return its id.
    fn add_after(conn: &mut Connection, title: &str, after: &[i64]) -> i64 {
        let mut task = new_task(title, "low");
        task.after = after.to_vec();
        id_in(&add_inner(conn, &task).unwrap())
    }

    fn no_filter() -> ListFilter {
        ListFilter {
            status: None,
            tag: None,
            executor: None,
            priority: None,
            all: false,
            limit: 0,
        }
    }

    fn register(conn: &Connection, name: &str) -> i64 {
        conn.execute(
            "INSERT INTO agents (name, role) VALUES (?1, 'developer')",
            [name],
        )
        .unwrap();
        conn.last_insert_rowid()
    }

    fn claim_directly(conn: &Connection, task: i64, agent: i64) {
        conn.execute(
            "UPDATE tasks SET executor = ?2, status = 'in_progress', claimed_at = datetime('now'),
                 lease_expires_at = datetime('now', '+1 hour') WHERE id = ?1",
            [task, agent],
        )
        .unwrap();
    }

    fn set_status(conn: &Connection, id: i64, status: &str) {
        conn.execute("UPDATE tasks SET status = ?1 WHERE id = ?2", (status, id))
            .unwrap();
    }

    fn lines(reply: &str) -> Vec<&str> {
        reply.lines().collect()
    }

    // ---- add ----

    #[test]
    fn add_replies_with_id_and_status() {
        let mut conn = setup();
        assert_eq!(
            add_inner(&mut conn, &new_task("one", "low")).unwrap(),
            "#1 todo"
        );
        assert_eq!(
            add_inner(&mut conn, &new_task("two", "low")).unwrap(),
            "#2 todo"
        );
    }

    #[test]
    fn add_rejects_bad_priority() {
        let mut conn = setup();
        let err = add_inner(&mut conn, &new_task("title", "urgentish")).unwrap_err();
        assert!(err.to_string().contains("invalid priority"));
    }

    #[test]
    fn add_rejects_empty_tests() {
        let mut conn = setup();
        let mut task = new_task("title", "low");
        task.tests.clear();
        let err = add_inner(&mut conn, &task).unwrap_err();
        assert!(err.to_string().contains("at least one test"));
    }

    #[test]
    fn add_rejects_a_test_that_is_not_a_triple() {
        let mut conn = setup();
        let mut task = new_task("title", "low");
        task.tests = vec![vec!["only".into(), "two".into()]];
        let err = add_inner(&mut conn, &task).unwrap_err();
        assert!(err.to_string().contains("test 0"), "{err}");
    }

    #[test]
    fn add_rejects_blank_title_and_flattens_whitespace() {
        let mut conn = setup();
        let err = add_inner(&mut conn, &new_task("  \n\t ", "low")).unwrap_err();
        assert_eq!(err.to_string(), "title must not be empty");

        let id = add_id(&mut conn, "  two\nlines\t here ", "low");
        let title: String = conn
            .query_row("SELECT title FROM tasks WHERE id = ?1", [id], |r| r.get(0))
            .unwrap();
        assert_eq!(title, "two lines here");
    }

    #[test]
    fn add_stores_tags_and_tests_as_json_arrays() {
        let mut conn = setup();
        let mut task = new_task("my task", "high");
        task.tags = vec!["backend".into(), "urgent-fix".into()];
        task.tests = vec![spec("d1"), spec("d2")];
        add_inner(&mut conn, &task).unwrap();

        let shown = view::testing::parse_show(&view::show(&conn, 1, false).unwrap());
        assert_eq!(shown["title"], "my task");
        assert_eq!(shown["priority"], "high");
        assert_eq!(shown["status"], "todo");
        assert_eq!(shown["tags"], json!(["backend", "urgent-fix"]));
        assert_eq!(shown["tests"].as_array().unwrap().len(), 2);
        assert_eq!(shown["tests"][0]["describe"], "d1");
        assert_eq!(shown["tests"][1]["input"], "in");
        assert_eq!(shown["tests"][1]["output"], "out");
    }

    #[test]
    fn add_after_reports_only_unfinished_prerequisites() {
        let mut conn = setup();
        let first = add_id(&mut conn, "first", "low");
        let second = add_id(&mut conn, "second", "low");
        set_status(&conn, second, "done");

        let mut task = new_task("third", "low");
        task.after = vec![first, second];
        assert_eq!(
            add_inner(&mut conn, &task).unwrap(),
            format!("#3 todo after:{first}")
        );

        task.after = vec![second];
        assert_eq!(add_inner(&mut conn, &task).unwrap(), "#4 todo");
    }

    #[test]
    fn add_with_unknown_prerequisite_creates_nothing() {
        let mut conn = setup();
        let mut task = new_task("orphan", "low");
        task.after = vec![42];
        let err = add_inner(&mut conn, &task).unwrap_err();
        assert_eq!(err.to_string(), "task 42 not found");
        let count: i64 = conn
            .query_row("SELECT COUNT(*) FROM tasks", [], |r| r.get(0))
            .unwrap();
        assert_eq!(count, 0);
    }

    // ---- list ----

    #[test]
    fn list_lines_show_priority_status_executor_and_open_prerequisites() {
        let mut conn = setup();
        let alice = register(&conn, "alice");
        let first = add_id(&mut conn, "Fix auth", "urgent");
        let second = add_id(&mut conn, "Add paging", "high");
        let mut third = new_task("Refactor", "medium");
        third.after = vec![first, second];
        add_inner(&mut conn, &third).unwrap();
        claim_directly(&conn, first, alice);

        assert_eq!(
            list_inner(&conn, &no_filter()).unwrap(),
            "#1 urgent in_progress@alice Fix auth\n\
             #2 high todo Add paging\n\
             #3 medium todo after:1,2 Refactor"
        );
    }

    #[test]
    fn list_orders_by_column_then_priority_then_id() {
        let mut conn = setup();
        let low_todo = add_id(&mut conn, "low-todo", "low");
        let urgent_todo = add_id(&mut conn, "urgent-todo", "urgent");
        let high_todo = add_id(&mut conn, "high-todo", "high");
        let high_todo_later = add_id(&mut conn, "high-todo-later", "high");
        let parked = add_id(&mut conn, "parked", "urgent");
        let working = add_id(&mut conn, "working", "low");
        let reviewing = add_id(&mut conn, "reviewing", "low");
        set_status(&conn, parked, "backlog");
        set_status(&conn, working, "in_progress");
        set_status(&conn, reviewing, "review");

        let reply = list_inner(&conn, &no_filter()).unwrap();
        let ids: Vec<&str> = lines(&reply)
            .iter()
            .map(|line| line.split(' ').next().unwrap())
            .collect();
        let expected: Vec<String> = [
            working,
            reviewing,
            urgent_todo,
            high_todo,
            high_todo_later,
            low_todo,
            parked,
        ]
        .iter()
        .map(|id| format!("#{id}"))
        .collect();
        assert_eq!(ids, expected);
    }

    #[test]
    fn list_hides_done_unless_all_or_status_done() {
        let mut conn = setup();
        add_id(&mut conn, "open", "low");
        let done = add_id(&mut conn, "finished", "low");
        let done2 = add_id(&mut conn, "finished too", "low");
        set_status(&conn, done, "done");
        set_status(&conn, done2, "done");

        assert_eq!(
            list_inner(&conn, &no_filter()).unwrap(),
            "#1 low todo open\n+2 done hidden (--all)"
        );

        let mut all = no_filter();
        all.all = true;
        assert_eq!(
            list_inner(&conn, &all).unwrap(),
            "#1 low todo open\n#2 low done finished\n#3 low done finished too"
        );

        let mut only_done = no_filter();
        only_done.status = Some("done".into());
        assert_eq!(
            list_inner(&conn, &only_done).unwrap(),
            "#2 low done finished\n#3 low done finished too"
        );
    }

    #[test]
    fn list_counts_hidden_done_within_the_other_filters() {
        let mut conn = setup();
        let mut tagged = new_task("tagged done", "low");
        tagged.tags = vec!["x".into()];
        add_inner(&mut conn, &tagged).unwrap();
        set_status(&conn, 1, "done");
        let other = add_id(&mut conn, "untagged done", "low");
        set_status(&conn, other, "done");

        let mut filter = no_filter();
        filter.tag = Some("x".into());
        assert_eq!(
            list_inner(&conn, &filter).unwrap(),
            "+1 done hidden (--all)"
        );
    }

    #[test]
    fn list_limit_truncates_and_reports_the_rest() {
        let mut conn = setup();
        for n in 0..5 {
            add_id(&mut conn, &format!("t{n}"), "low");
        }
        let mut filter = no_filter();
        filter.limit = 2;
        assert_eq!(
            list_inner(&conn, &filter).unwrap(),
            "#1 low todo t0\n#2 low todo t1\n+3 more (--limit 0 = all)"
        );
        filter.limit = 5;
        assert_eq!(lines(&list_inner(&conn, &filter).unwrap()).len(), 5);
    }

    #[test]
    fn list_of_an_empty_board_says_so() {
        let conn = setup();
        assert_eq!(list_inner(&conn, &no_filter()).unwrap(), "no tasks");
    }

    #[test]
    fn list_filters_by_status() {
        let mut conn = setup();
        add_id(&mut conn, "t1", "low");
        let second = add_id(&mut conn, "t2", "low");
        set_status(&conn, second, "in_progress");

        let mut filter = no_filter();
        filter.status = Some("in_progress".into());
        assert_eq!(list_inner(&conn, &filter).unwrap(), "#2 low in_progress t2");
    }

    #[test]
    fn list_filters_by_tag() {
        let mut conn = setup();
        for (title, tag) in [("t1", "alpha"), ("t2", "beta")] {
            let mut task = new_task(title, "low");
            task.tags = vec![tag.to_string()];
            add_inner(&mut conn, &task).unwrap();
        }
        let mut filter = no_filter();
        filter.tag = Some("alpha".into());
        assert_eq!(list_inner(&conn, &filter).unwrap(), "#1 low todo t1");
    }

    #[test]
    fn list_filters_by_executor_name_and_ignores_expired_leases() {
        let mut conn = setup();
        let a = register(&conn, "agent-a");
        register(&conn, "agent-b");
        let held = add_id(&mut conn, "t1", "low");
        let stale = add_id(&mut conn, "t2", "low");
        add_id(&mut conn, "t3", "low");
        claim_directly(&conn, held, a);
        claim_directly(&conn, stale, a);
        conn.execute(
            "UPDATE tasks SET lease_expires_at = datetime('now', '-1 second') WHERE id = ?1",
            [stale],
        )
        .unwrap();

        let mut filter = no_filter();
        filter.executor = Some("agent-a".into());
        assert_eq!(
            list_inner(&conn, &filter).unwrap(),
            "#1 low in_progress@agent-a t1"
        );
        filter.executor = Some("agent-b".into());
        assert_eq!(list_inner(&conn, &filter).unwrap(), "no tasks");
    }

    #[test]
    fn list_filters_by_priority_and_combines_filters() {
        let mut conn = setup();
        let mut urgent = new_task("t1", "urgent");
        urgent.tags = vec!["x".into()];
        add_inner(&mut conn, &urgent).unwrap();
        add_id(&mut conn, "t2", "low");
        let mut urgent_untagged = new_task("t3", "urgent");
        urgent_untagged.tags = vec!["y".into()];
        add_inner(&mut conn, &urgent_untagged).unwrap();

        let mut filter = no_filter();
        filter.priority = Some("urgent".into());
        assert_eq!(lines(&list_inner(&conn, &filter).unwrap()).len(), 2);
        filter.tag = Some("x".into());
        assert_eq!(list_inner(&conn, &filter).unwrap(), "#1 urgent todo t1");
    }

    #[test]
    fn list_rejects_unknown_status_and_priority() {
        let conn = setup();
        let mut filter = no_filter();
        filter.status = Some("doing".into());
        let err = list_inner(&conn, &filter).unwrap_err();
        assert!(err.to_string().contains("invalid status 'doing'"), "{err}");

        let mut filter = no_filter();
        filter.priority = Some("asap".into());
        let err = list_inner(&conn, &filter).unwrap_err();
        assert!(err.to_string().contains("invalid priority"), "{err}");
    }

    // ---- edit ----

    #[test]
    fn edit_applies_fields_and_replies_with_status() {
        let mut conn = setup();
        let id = add_id(&mut conn, "t1", "low");
        let reply = edit_inner(
            &mut conn,
            id,
            &TaskEdit {
                title: Some("new title".into()),
                priority: Some("urgent".into()),
                ..TaskEdit::default()
            },
        )
        .unwrap();
        assert_eq!(reply, "#1 todo");

        let shown = view::testing::parse_show(&view::show(&conn, id, false).unwrap());
        assert_eq!(shown["title"], "new title");
        assert_eq!(shown["priority"], "urgent");
    }

    #[test]
    fn edit_replaces_tags_and_tests_and_bumps_updated_at() {
        let mut conn = setup();
        let mut task = new_task("t1", "low");
        task.tags = vec!["old-tag".into()];
        task.tests = vec![spec("old")];
        add_inner(&mut conn, &task).unwrap();
        conn.execute("UPDATE tasks SET updated_at = '2000-01-01 00:00:00'", [])
            .unwrap();

        edit_inner(
            &mut conn,
            1,
            &TaskEdit {
                tags: Some(vec!["new-tag".into()]),
                tests: Some(vec![spec("new"), spec("newer")]),
                ..TaskEdit::default()
            },
        )
        .unwrap();

        let shown = view::testing::parse_show(&view::show(&conn, 1, false).unwrap());
        assert_eq!(shown["tags"], json!(["new-tag"]));
        assert_eq!(shown["tests"].as_array().unwrap().len(), 2);
        assert_eq!(shown["tests"][0]["describe"], "new");
        let updated: String = conn
            .query_row("SELECT updated_at FROM tasks WHERE id = 1", [], |r| {
                r.get(0)
            })
            .unwrap();
        assert_ne!(updated, "2000-01-01 00:00:00");
    }

    #[test]
    fn edit_rejects_invalid_input_without_partial_changes() {
        let mut conn = setup();
        let id = add_id(&mut conn, "t1", "low");

        let err = edit_inner(
            &mut conn,
            id,
            &TaskEdit {
                title: Some("changed".into()),
                priority: Some("urgentish".into()),
                ..TaskEdit::default()
            },
        )
        .unwrap_err();
        assert!(err.to_string().contains("invalid priority"));

        let err = edit_inner(
            &mut conn,
            id,
            &TaskEdit {
                tests: Some(vec![]),
                ..TaskEdit::default()
            },
        )
        .unwrap_err();
        assert!(err.to_string().contains("at least one test"));

        let err = edit_inner(
            &mut conn,
            id,
            &TaskEdit {
                title: Some(" ".into()),
                ..TaskEdit::default()
            },
        )
        .unwrap_err();
        assert_eq!(err.to_string(), "title must not be empty");

        let shown = view::testing::parse_show(&view::show(&conn, id, false).unwrap());
        assert_eq!(shown["title"], "t1");
        assert_eq!(shown["priority"], "low");
    }

    #[test]
    fn edit_without_any_change_is_an_error() {
        let mut conn = setup();
        let id = add_id(&mut conn, "t1", "low");
        let err = edit_inner(&mut conn, id, &TaskEdit::default()).unwrap_err();
        assert!(err.to_string().contains("nothing to change"), "{err}");
    }

    #[test]
    fn edit_is_blocked_while_claimed_in_review_or_done() {
        let mut conn = setup();
        let agent = register(&conn, "agent-a");
        let change = TaskEdit {
            title: Some("new title".into()),
            ..TaskEdit::default()
        };

        let claimed = add_id(&mut conn, "claimed", "low");
        claim_directly(&conn, claimed, agent);
        let err = edit_inner(&mut conn, claimed, &change).unwrap_err();
        assert!(err.to_string().contains("claimed"), "{err}");

        let reviewed = add_id(&mut conn, "reviewed", "low");
        set_status(&conn, reviewed, "review");
        let err = edit_inner(&mut conn, reviewed, &change).unwrap_err();
        assert!(err.to_string().contains("in review"), "{err}");

        let done = add_id(&mut conn, "done", "low");
        set_status(&conn, done, "done");
        let err = edit_inner(&mut conn, done, &change).unwrap_err();
        assert!(err.to_string().contains("immutable"), "{err}");
    }

    #[test]
    fn edit_that_only_touches_prerequisites_is_guarded_too() {
        let mut conn = setup();
        let agent = register(&conn, "agent-a");
        let prerequisite = add_id(&mut conn, "first", "low");
        let claimed = add_id(&mut conn, "claimed", "low");
        claim_directly(&conn, claimed, agent);

        let err = edit_inner(
            &mut conn,
            claimed,
            &TaskEdit {
                after: vec![prerequisite],
                ..TaskEdit::default()
            },
        )
        .unwrap_err();
        assert!(err.to_string().contains("claimed"), "{err}");
        assert!(deps::open_prerequisites(&conn, claimed).unwrap().is_empty());
    }

    #[test]
    fn edit_nonexistent_task_fails_with_not_found() {
        let mut conn = setup();
        let err = edit_inner(
            &mut conn,
            999,
            &TaskEdit {
                title: Some("x".into()),
                ..TaskEdit::default()
            },
        )
        .unwrap_err();
        assert_eq!(err.to_string(), "task 999 not found");
    }

    #[test]
    fn edit_adds_and_drops_prerequisites() {
        let mut conn = setup();
        let first = add_id(&mut conn, "first", "low");
        let second = add_id(&mut conn, "second", "low");
        let third = add_id(&mut conn, "third", "low");

        let reply = edit_inner(
            &mut conn,
            third,
            &TaskEdit {
                after: vec![first, second],
                ..TaskEdit::default()
            },
        )
        .unwrap();
        assert_eq!(reply, format!("#{third} todo after:{first},{second}"));

        let reply = edit_inner(
            &mut conn,
            third,
            &TaskEdit {
                drop_after: vec![first],
                ..TaskEdit::default()
            },
        )
        .unwrap();
        assert_eq!(reply, format!("#{third} todo after:{second}"));
    }

    #[test]
    fn edit_rejects_cycles_and_rolls_back_the_other_changes() {
        let mut conn = setup();
        let first = add_id(&mut conn, "first", "low");
        let second = add_id(&mut conn, "second", "low");
        edit_inner(
            &mut conn,
            second,
            &TaskEdit {
                after: vec![first],
                ..TaskEdit::default()
            },
        )
        .unwrap();

        let err = edit_inner(
            &mut conn,
            first,
            &TaskEdit {
                title: Some("renamed".into()),
                after: vec![second],
                ..TaskEdit::default()
            },
        )
        .unwrap_err();
        assert!(err.to_string().contains("cycle"), "{err}");

        let title: String = conn
            .query_row("SELECT title FROM tasks WHERE id = ?1", [first], |r| {
                r.get(0)
            })
            .unwrap();
        assert_eq!(
            title, "first",
            "the rename must be rolled back with the failed edge"
        );
        assert!(deps::open_prerequisites(&conn, first).unwrap().is_empty());
    }

    #[test]
    fn edit_reports_the_current_status_of_a_rework_task() {
        let mut conn = setup();
        let id = add_id(&mut conn, "t1", "low");
        set_status(&conn, id, "in_progress");
        let reply = edit_inner(
            &mut conn,
            id,
            &TaskEdit {
                priority: Some("high".into()),
                ..TaskEdit::default()
            },
        )
        .unwrap();
        assert_eq!(reply, "#1 in_progress");
    }

    // ---- remove ----

    #[test]
    fn remove_succeeds_otherwise() {
        let mut conn = setup();
        let id = add_id(&mut conn, "t1", "low");
        assert_eq!(remove_inner(&conn, id).unwrap(), "#1 removed");
        let err = view::show(&conn, id, false).unwrap_err();
        assert!(err.to_string().contains("not found"));
    }

    #[test]
    fn remove_nonexistent_task_fails_with_not_found() {
        let conn = setup();
        let err = remove_inner(&conn, 999).unwrap_err();
        assert_eq!(err.to_string(), "task 999 not found");
    }

    #[test]
    fn remove_is_blocked_while_claimed_in_review_or_done() {
        let mut conn = setup();
        let agent = register(&conn, "agent-a");

        let claimed = add_id(&mut conn, "claimed", "low");
        claim_directly(&conn, claimed, agent);
        let err = remove_inner(&conn, claimed).unwrap_err();
        assert!(err.to_string().contains("claimed"), "{err}");

        let reviewed = add_id(&mut conn, "reviewed", "low");
        set_status(&conn, reviewed, "review");
        let err = remove_inner(&conn, reviewed).unwrap_err();
        assert!(err.to_string().contains("in review"), "{err}");

        let done = add_id(&mut conn, "done", "low");
        set_status(&conn, done, "done");
        let err = remove_inner(&conn, done).unwrap_err();
        assert!(err.to_string().contains("can't be removed"), "{err}");

        // Nothing was deleted by the guarded statements.
        for id in [claimed, reviewed, done] {
            assert!(view::load(&conn, id).is_ok());
        }
    }

    #[test]
    fn remove_is_blocked_while_other_tasks_wait_on_it() {
        let mut conn = setup();
        let prerequisite = add_id(&mut conn, "first", "low");
        let waiting_id = add_after(&mut conn, "second", &[prerequisite]);

        let err = remove_inner(&conn, prerequisite).unwrap_err();
        assert_eq!(
            err.to_string(),
            format!(
                "task {prerequisite} is a prerequisite of {waiting_id}; remove those tasks or \
                 detach them (edit --drop-after {prerequisite}) first"
            )
        );
        assert!(view::load(&conn, prerequisite).is_ok());

        // Removing the waiting task first frees the prerequisite.
        remove_inner(&conn, waiting_id).unwrap();
        remove_inner(&conn, prerequisite).unwrap();
    }

    #[test]
    fn removing_a_task_drops_its_own_prerequisite_edges() {
        let mut conn = setup();
        let prerequisite = add_id(&mut conn, "first", "low");
        let waiting = add_after(&mut conn, "second", &[prerequisite]);
        remove_inner(&conn, waiting).unwrap();
        let edges: i64 = conn
            .query_row("SELECT COUNT(*) FROM task_deps", [], |r| r.get(0))
            .unwrap();
        assert_eq!(edges, 0);
    }

    // ---- diagnostics ----

    #[test]
    fn diagnose_propagates_non_missing_row_errors_uncaught() {
        // "not found" only applies when the diagnostic SELECT finds no row;
        // a different failure (the table itself is gone) comes through as
        // its own error rather than being misreported as "not found".
        let mut conn = setup();
        let id = add_id(&mut conn, "t1", "low");
        conn.execute_batch("DROP TABLE task_deps; DROP TABLE tasks;")
            .unwrap();

        let err = diagnose_mutation_guard_failure(&conn, id, "editing", "done");
        assert!(!err.to_string().contains("not found"), "{err}");
        assert!(err.to_string().contains("no such table"), "{err}");
    }

    #[test]
    fn diagnose_reports_state_changed_concurrently_when_state_explains_nothing() {
        // Reached when the guarded statement changed nothing but the task is
        // neither claimed, done nor in review: its state changed again
        // between the two statements. Driven directly on a normal task.
        let mut conn = setup();
        let id = add_id(&mut conn, "t1", "low");

        let err = diagnose_mutation_guard_failure(&conn, id, "editing", "done");
        assert!(
            err.to_string().contains("state changed concurrently"),
            "{err}"
        );
        let err = diagnose_remove_failure(&conn, id);
        assert!(
            err.to_string().contains("state changed concurrently"),
            "{err}"
        );
    }

    // ---- schema ----

    fn assert_constraint_violation(result: rusqlite::Result<usize>) {
        match result {
            Err(rusqlite::Error::SqliteFailure(ffi_err, _)) => {
                assert_eq!(ffi_err.code, rusqlite::ErrorCode::ConstraintViolation);
            }
            other => panic!("expected SqliteFailure constraint violation, got {other:?}"),
        }
    }

    #[test]
    fn db_check_rejects_invalid_priority() {
        let conn = setup();
        assert_constraint_violation(conn.execute(
            "INSERT INTO tasks (title, priority, tests) VALUES ('t', 'bogus-priority', '[\"x\"]')",
            [],
        ));
    }

    #[test]
    fn db_check_rejects_empty_tests_array() {
        let conn = setup();
        assert_constraint_violation(conn.execute(
            "INSERT INTO tasks (title, priority, tests) VALUES ('t', 'low', '[]')",
            [],
        ));
    }

    #[test]
    fn db_check_rejects_invalid_json_tests() {
        let conn = setup();
        assert_constraint_violation(conn.execute(
            "INSERT INTO tasks (title, priority, tests) VALUES ('t', 'low', 'not valid json')",
            [],
        ));
    }

    #[test]
    fn db_check_rejects_non_array_tests() {
        let conn = setup();
        assert_constraint_violation(conn.execute(
            "INSERT INTO tasks (title, priority, tests) VALUES ('t', 'low', '{\"not\":\"an array\"}')",
            [],
        ));
    }
}
