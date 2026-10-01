//! What the commands print. Replies are small on purpose: an agent pays for
//! every token it reads and already knows what it just asked for. Mutations
//! answer with one line (`#7 review rev1`); only the commands that hand out
//! work or inspect a task return content: a header line shaped like a `list`
//! row, then one `|`-separated line per fact.
//!
//! ```text
//! #7 high review@bob rev1 after:3 blocks:9 Title   `show`; a claim has only `#7 rev1 Title`
//! tags|backend|area-2                              only when there are tags
//! changes|the reviewer's outstanding notes         only after a rejection
//! 0|DESCRIBE|INPUT|OUTPUT[|RESULT|EVIDENCE]        one line per test, 0-based
//! rev1|alice|passed: ok|failed: broken             `show --history`: one line per revision
//! review|bob|changes_requested|NOTES               and its decision, once there is one
//! ```
//!
//! Free text is written by [`cell`], which escapes what would break the
//! layout, so every line splits cleanly on `|`. JSON said the same with a key
//! and quotes around every value, and escaped the quotes inside the text too.

use std::collections::BTreeMap;

use anyhow::{Result, anyhow};
use rusqlite::{Connection, OptionalExtension};
use serde_json::Value;

use super::agent::Role;
use super::deps;

/// Free text as one cell of a `|` line: `\`, `|` and line breaks are written
/// `\\`, `\|`, `\n` and `\r`, so a line always splits cleanly on `|` and a
/// record never spans two lines.
pub fn cell(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for ch in text.chars() {
        match ch {
            '\\' => out.push_str("\\\\"),
            '|' => out.push_str("\\|"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            other => out.push(other),
        }
    }
    out
}

/// A task reply under construction: the header line, then `|` rows.
struct Reply(Vec<String>);

impl Reply {
    fn new(header: String) -> Self {
        Self(vec![header])
    }

    /// Add a row; every cell is free text and gets escaped.
    fn row<'a>(&mut self, cells: impl IntoIterator<Item = &'a str>) {
        let cells: Vec<String> = cells.into_iter().map(cell).collect();
        self.0.push(cells.join("|"));
    }

    /// One row per criterion, `IDX|DESCRIBE|INPUT|OUTPUT`, followed by
    /// `|RESULT|EVIDENCE` when the developer has given a verdict.
    fn tests(&mut self, tests: &[Value], verdicts: &BTreeMap<usize, Verdict>) {
        for (index, spec) in tests.iter().enumerate() {
            let text = |key: &str| spec.get(key).and_then(Value::as_str).unwrap_or_default();
            let label = index.to_string();
            let mut cells = vec![
                label.as_str(),
                text("describe"),
                text("input"),
                text("output"),
            ];
            if let Some(verdict) = verdicts.get(&index) {
                cells.push(&verdict.result);
                cells.push(&verdict.evidence);
            }
            self.row(cells);
        }
    }

    fn finish(self) -> String {
        self.0.join("\n")
    }
}

/// `#7 review`: the acknowledgement most mutations print.
pub fn ack(id: i64, status: &str) -> String {
    format!("#{id} {status}")
}

/// A task as stored. `executor` is the agent that holds a live lease on it;
/// an expired lease counts as no executor.
pub struct Task {
    pub id: i64,
    pub title: String,
    pub priority: String,
    pub status: String,
    pub executor: Option<String>,
    pub tags: Vec<String>,
    pub tests: Vec<Value>,
    pub revision: i64,
}

pub fn load(conn: &Connection, id: i64) -> Result<Task> {
    conn.query_row(
        "SELECT t.id, t.title, t.priority, t.status,
                CASE WHEN t.status != 'done' AND t.lease_expires_at > datetime('now')
                     THEN a.name END,
                t.tags, t.tests, t.revision
         FROM tasks t LEFT JOIN agents a ON t.executor = a.id
         WHERE t.id = ?1",
        [id],
        |row| {
            let tags: String = row.get(5)?;
            let tests: String = row.get(6)?;
            Ok(Task {
                id: row.get(0)?,
                title: row.get(1)?,
                priority: row.get(2)?,
                status: row.get(3)?,
                executor: row.get(4)?,
                // The schema guarantees valid JSON, but SQLite and serde_json
                // are different parsers: degrade to empty rather than fail.
                tags: serde_json::from_str(&tags).unwrap_or_default(),
                tests: serde_json::from_str(&tests).unwrap_or_default(),
                revision: row.get(7)?,
            })
        },
    )
    .optional()?
    .ok_or_else(|| anyhow!("task {id} not found"))
}

/// One developer verdict on one criterion.
struct Verdict {
    result: String,
    evidence: String,
}

/// The verdicts recorded for `revision`, by criterion index.
fn verdicts(conn: &Connection, id: i64, revision: i64) -> Result<BTreeMap<usize, Verdict>> {
    let mut stmt = conn.prepare(
        "SELECT criterion_index, result, evidence FROM acceptance_results
         WHERE task_id = ?1 AND revision = ?2",
    )?;
    let rows = stmt.query_map([id, revision], |row| {
        Ok((
            row.get::<_, i64>(0)?,
            Verdict {
                result: row.get(1)?,
                evidence: row.get(2)?,
            },
        ))
    })?;
    let mut by_index = BTreeMap::new();
    for row in rows {
        let (index, verdict) = row?;
        by_index.insert(usize::try_from(index)?, verdict);
    }
    Ok(by_index)
}

/// The reviewer's notes from the latest review, while they are still
/// outstanding: a task sent back for changes. Once approved, nothing is.
fn changes(conn: &Connection, id: i64) -> Result<Option<String>> {
    let latest: Option<(String, String)> = conn
        .query_row(
            "SELECT decision, notes FROM review_history
             WHERE task_id = ?1 ORDER BY revision DESC LIMIT 1",
            [id],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .optional()?;
    Ok(latest
        .filter(|(decision, notes)| decision == "changes_requested" && !notes.is_empty())
        .map(|(_, notes)| notes))
}

/// What `claim` / `claim-next` hand back: the task to work on.
///
/// A developer gets the criteria to satisfy and, when this is a rework, the
/// revision and the reviewer's notes. A reviewer gets the same criteria with
/// the developer's verdict and evidence merged in, and the notes they left
/// last time so they can check they were addressed.
pub fn work_order(conn: &Connection, id: i64, role: Role) -> Result<String> {
    let task = load(conn, id)?;
    let revision = if role == Role::Reviewer || task.revision > 0 {
        format!(" rev{}", task.revision)
    } else {
        String::new()
    };
    let mut reply = Reply::new(format!("#{}{revision} {}", task.id, cell(&task.title)));
    if let Some(notes) = changes(conn, id)? {
        reply.row(["changes", notes.as_str()]);
    }
    let verdicts = match role {
        Role::Reviewer => verdicts(conn, id, task.revision)?,
        Role::Developer => BTreeMap::new(),
    };
    reply.tests(&task.tests, &verdicts);
    Ok(reply.finish())
}

/// `show`: everything about one task that is still relevant, leaving out
/// empty fields. Pass `with_history` for every past revision as well.
pub fn show(conn: &Connection, id: i64, with_history: bool) -> Result<String> {
    let task = load(conn, id)?;
    let executor = task
        .executor
        .as_ref()
        .map_or_else(String::new, |name| format!("@{name}"));
    let revision = if task.revision > 0 {
        format!(" rev{}", task.revision)
    } else {
        String::new()
    };
    let after = deps::labeled("after", &deps::open_prerequisites(conn, id)?);
    let blocks = deps::labeled("blocks", &deps::open_dependents(conn, id)?);
    let mut reply = Reply::new(format!(
        "#{} {} {}{executor}{revision}{after}{blocks} {}",
        task.id,
        task.priority,
        task.status,
        cell(&task.title)
    ));
    if !task.tags.is_empty() {
        reply.row(std::iter::once("tags").chain(task.tags.iter().map(String::as_str)));
    }
    if let Some(notes) = changes(conn, id)? {
        reply.row(["changes", notes.as_str()]);
    }
    // Verdicts belong next to the criteria once the work is submitted; while
    // it is being redone they are history.
    let verdicts = if matches!(task.status.as_str(), "review" | "done") {
        verdicts(conn, id, task.revision)?
    } else {
        BTreeMap::new()
    };
    reply.tests(&task.tests, &verdicts);
    if with_history {
        history(conn, id, &mut reply)?;
    }
    Ok(reply.finish())
}

/// One entry per revision: the developer's verdicts as `passed: evidence`
/// cells (in criterion order), then the review decision if there is one.
fn history(conn: &Connection, id: i64, reply: &mut Reply) -> Result<()> {
    struct Revision {
        developer: String,
        verdicts: Vec<String>,
        review: Option<(String, String, String)>,
    }
    let mut revisions: BTreeMap<i64, Revision> = BTreeMap::new();
    let blank = || Revision {
        developer: String::new(),
        verdicts: Vec::new(),
        review: None,
    };

    let mut stmt = conn.prepare(
        "SELECT revision, result, evidence, executor_name FROM acceptance_results
         WHERE task_id = ?1 ORDER BY revision, criterion_index",
    )?;
    let rows = stmt.query_map([id], |row| {
        Ok((
            row.get::<_, i64>(0)?,
            row.get::<_, String>(1)?,
            row.get::<_, String>(2)?,
            row.get::<_, String>(3)?,
        ))
    })?;
    for row in rows {
        let (revision, result, evidence, developer) = row?;
        let entry = revisions.entry(revision).or_insert_with(blank);
        entry.developer = developer;
        entry.verdicts.push(format!("{result}: {evidence}"));
    }

    let mut stmt = conn.prepare(
        "SELECT revision, decision, notes, executor_name FROM review_history
         WHERE task_id = ?1",
    )?;
    let rows = stmt.query_map([id], |row| {
        Ok((
            row.get::<_, i64>(0)?,
            row.get::<_, String>(1)?,
            row.get::<_, String>(2)?,
            row.get::<_, String>(3)?,
        ))
    })?;
    for row in rows {
        let (revision, decision, notes, reviewer) = row?;
        revisions.entry(revision).or_insert_with(blank).review = Some((decision, notes, reviewer));
    }

    for (revision, entry) in &revisions {
        let label = format!("rev{revision}");
        reply.row(
            [label.as_str(), entry.developer.as_str()]
                .into_iter()
                .chain(entry.verdicts.iter().map(String::as_str)),
        );
        if let Some((decision, notes, reviewer)) = &entry.review {
            let mut cells = vec!["review", reviewer.as_str(), decision.as_str()];
            if !notes.is_empty() {
                cells.push(notes);
            }
            reply.row(cells);
        }
    }
    Ok(())
}

/// One row of `list`.
pub struct Row {
    pub id: i64,
    pub priority: String,
    pub status: String,
    pub executor: Option<String>,
    /// Prerequisites that are not done yet.
    pub after: Vec<i64>,
    pub title: String,
}

/// `#7 high in_progress@alice after:3,5 Title`. The title comes last so the
/// optional parts before it never make a line ambiguous.
pub fn line(row: &Row) -> String {
    let executor = row
        .executor
        .as_ref()
        .map_or_else(String::new, |name| format!("@{name}"));
    format!(
        "#{} {} {}{executor}{} {}",
        row.id,
        row.priority,
        row.status,
        deps::labeled("after", &row.after),
        row.title
    )
}

/// Test-only inverse of the renderers: reads a task reply back into the JSON
/// shape earlier versions printed, so tests can assert on facts instead of
/// layout. It follows the grammar in the module docs, not the code that
/// writes it, and panics on anything the grammar does not allow.
#[cfg(test)]
pub mod testing {
    use serde_json::{Map, Value, json};

    /// A `show` reply: `#7 high review@bob rev1 after:3 blocks:9 Title`, rows.
    pub fn parse_show(reply: &str) -> Value {
        read(reply, true)
    }

    /// A `claim` / `claim-next` reply: `#7 rev1 Title`, rows.
    pub fn parse_order(reply: &str) -> Value {
        read(reply, false)
    }

    fn read(reply: &str, with_state: bool) -> Value {
        let mut lines = reply.lines();
        let mut task = header(lines.next().expect("an empty reply"), with_state);
        let mut tests = Vec::new();
        let mut history: Vec<Value> = Vec::new();
        for line in lines {
            let cells = cells(line);
            let kind = cells[0].as_str();
            if !kind.is_empty() && kind.bytes().all(|b| b.is_ascii_digit()) {
                // IDX|DESCRIBE|INPUT|OUTPUT[|RESULT|EVIDENCE]
                assert_eq!(
                    kind,
                    tests.len().to_string(),
                    "tests count up from 0: {line}"
                );
                assert!(matches!(cells.len(), 4 | 6), "{line}");
                let mut test = json!({"describe": cells[1], "input": cells[2], "output": cells[3]});
                if cells.len() == 6 {
                    test["result"] = json!(cells[4]);
                    test["evidence"] = json!(cells[5]);
                }
                tests.push(test);
            } else if kind == "tags" {
                task["tags"] = json!(&cells[1..]);
            } else if kind == "changes" {
                assert_eq!(cells.len(), 2, "{line}");
                task["changes"] = json!(cells[1]);
            } else if kind == "review" {
                // review|REVIEWER|DECISION[|NOTES]
                assert!(matches!(cells.len(), 3 | 4), "{line}");
                let entry = history.last_mut().expect("a review before any revision");
                entry["decision"] = json!(cells[2]);
                if let Some(notes) = cells.get(3) {
                    entry["notes"] = json!(notes);
                }
                entry["reviewer"] = json!(cells[1]);
            } else if let Some(revision) = kind.strip_prefix("rev") {
                // revN|DEVELOPER|RESULT: EVIDENCE|...
                let revision: i64 = revision.parse().unwrap_or_else(|_| panic!("{line}"));
                history.push(json!({"rev": revision, "by": cells[1], "results": &cells[2..]}));
            } else {
                panic!("unknown row: {line}");
            }
        }
        task["tests"] = Value::Array(tests);
        if !history.is_empty() {
            task["history"] = Value::Array(history);
        }
        task
    }

    fn header(line: &str, with_state: bool) -> Value {
        let mut rest = line
            .strip_prefix('#')
            .unwrap_or_else(|| panic!("a header starts with #: {line}"));
        let mut task = Map::new();
        let id: i64 = word(&mut rest).parse().expect("a numeric id");
        task.insert("id".into(), json!(id));
        if with_state {
            task.insert("priority".into(), json!(word(&mut rest)));
            let status = word(&mut rest);
            let (status, executor) = status
                .split_once('@')
                .map_or((status, None), |(s, who)| (s, Some(who)));
            task.insert("status".into(), json!(status));
            if let Some(executor) = executor {
                task.insert("executor".into(), json!(executor));
            }
        }
        // Optional labelled words, then the title, which is the rest of the line.
        loop {
            let (next, tail) = rest.split_once(' ').unwrap_or((rest, ""));
            let ids = |label: &str| {
                next.strip_prefix(label).map(|ids| {
                    ids.split(',')
                        .map(|id| id.parse::<i64>().expect("ids"))
                        .collect::<Vec<_>>()
                })
            };
            if let Some(revision) = next.strip_prefix("rev").and_then(|n| n.parse::<i64>().ok()) {
                task.insert("rev".into(), json!(revision));
            } else if let Some(ids) = ids("after:") {
                task.insert("after".into(), json!(ids));
            } else if let Some(ids) = ids("blocks:") {
                task.insert("blocks".into(), json!(ids));
            } else {
                break;
            }
            rest = tail;
        }
        let mut title = cells(rest);
        assert_eq!(title.len(), 1, "a title is one escaped cell: {line}");
        task.insert("title".into(), json!(title.remove(0)));
        Value::Object(task)
    }

    fn word<'a>(rest: &mut &'a str) -> &'a str {
        let (word, tail) = rest.split_once(' ').unwrap_or((*rest, ""));
        *rest = tail;
        word
    }

    /// Split a row at its unescaped `|` and undo the escaping of each cell.
    pub fn cells(line: &str) -> Vec<String> {
        let mut cells = Vec::new();
        let mut current = String::new();
        let mut chars = line.chars();
        while let Some(ch) = chars.next() {
            match ch {
                '\\' => match chars.next() {
                    Some('\\') => current.push('\\'),
                    Some('|') => current.push('|'),
                    Some('n') => current.push('\n'),
                    Some('r') => current.push('\r'),
                    other => panic!("bad escape \\{other:?} in {line:?}"),
                },
                '|' => cells.push(std::mem::take(&mut current)),
                other => current.push(other),
            }
        }
        cells.push(current);
        cells
    }
}

#[cfg(test)]
mod tests {
    use super::testing::{cells, parse_order, parse_show};
    use super::*;
    use serde_json::json;

    fn setup() -> Connection {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch("PRAGMA foreign_keys=ON;").unwrap();
        conn.execute_batch(crate::db::SCHEMA).unwrap();
        conn.execute_batch(
            "INSERT INTO agents (id, name, role) VALUES (1, 'dev', 'developer'), (2, 'rev', 'reviewer');",
        )
        .unwrap();
        conn
    }

    const TESTS: &str = r#"[{"describe":"adds","input":"1+1","output":"2"},{"describe":"subtracts","input":"3-1","output":"2"}]"#;

    fn insert_task(conn: &Connection, status: &str, revision: i64) -> i64 {
        conn.execute(
            "INSERT INTO tasks (title, priority, status, tests, revision, tags)
             VALUES ('Calc', 'high', ?1, ?2, ?3, '[\"math\",\"core\"]')",
            rusqlite::params![status, TESTS, revision],
        )
        .unwrap();
        conn.last_insert_rowid()
    }

    fn verdict(
        conn: &Connection,
        task: i64,
        revision: i64,
        index: i64,
        result: &str,
        evidence: &str,
    ) {
        conn.execute(
            "INSERT INTO acceptance_results
               (task_id, revision, criterion_index, criterion, result, evidence, executor, executor_name)
             VALUES (?1, ?2, ?3, '{}', ?4, ?5, 1, 'dev')",
            rusqlite::params![task, revision, index, result, evidence],
        )
        .unwrap();
    }

    fn decision(conn: &Connection, task: i64, revision: i64, decision: &str, notes: &str) {
        conn.execute(
            "INSERT INTO review_history (task_id, revision, decision, notes, executor, executor_name)
             VALUES (?1, ?2, ?3, ?4, 2, 'rev')",
            rusqlite::params![task, revision, decision, notes],
        )
        .unwrap();
    }

    fn first_line(reply: &str) -> &str {
        reply.lines().next().unwrap()
    }

    #[test]
    fn cell_escapes_what_would_break_the_layout() {
        assert_eq!(cell("plain, \"quoted\" é 日本"), "plain, \"quoted\" é 日本");
        assert_eq!(cell("a|b"), r"a\|b");
        assert_eq!(cell(r"back\slash"), r"back\\slash");
        assert_eq!(cell("two\nlines\r\nhere"), r"two\nlines\r\nhere");
        assert_eq!(cell(""), "");
    }

    #[test]
    fn escaped_cells_split_back_exactly() {
        let nasty = [
            "",
            "|",
            "||",
            "\\",
            "\\|",
            "\\n",
            "ends with \\",
            "line\nbreak",
            "tab\there",
            "\"quoted\"",
            "é 日本 🙂",
            "{\"json\": [1, 2]}",
            "trailing|",
            "tags",
            "123",
        ];
        let row = nasty
            .iter()
            .map(|text| cell(text))
            .collect::<Vec<_>>()
            .join("|");
        assert!(!row.contains(['\n', '\r']), "{row}");
        assert_eq!(cells(&row), nasty);
        for text in nasty {
            assert_eq!(cells(&cell(text)), [text], "{text:?}");
        }
    }

    #[test]
    fn ack_and_line_formats() {
        assert_eq!(ack(7, "review"), "#7 review");
        let mut row = Row {
            id: 9,
            priority: "high".into(),
            status: "todo".into(),
            executor: None,
            after: vec![],
            title: "Refactor X".into(),
        };
        assert_eq!(line(&row), "#9 high todo Refactor X");
        row.status = "in_progress".into();
        row.executor = Some("alice".into());
        row.after = vec![3, 5];
        assert_eq!(line(&row), "#9 high in_progress@alice after:3,5 Refactor X");
    }

    #[test]
    fn developer_work_order_has_only_what_is_needed_to_start() {
        let conn = setup();
        let id = insert_task(&conn, "in_progress", 0);
        assert_eq!(
            work_order(&conn, id, Role::Developer).unwrap(),
            format!("#{id} Calc\n0|adds|1+1|2\n1|subtracts|3-1|2")
        );
    }

    #[test]
    fn rework_order_carries_revision_and_outstanding_changes() {
        let conn = setup();
        let id = insert_task(&conn, "in_progress", 1);
        verdict(&conn, id, 1, 0, "passed", "ok");
        verdict(&conn, id, 1, 1, "failed", "off by one");
        decision(&conn, id, 1, "changes_requested", "fix subtraction");

        // Old verdicts are not repeated to the developer.
        assert_eq!(
            work_order(&conn, id, Role::Developer).unwrap(),
            format!("#{id} rev1 Calc\nchanges|fix subtraction\n0|adds|1+1|2\n1|subtracts|3-1|2")
        );
    }

    #[test]
    fn reviewer_packet_merges_current_verdicts_and_keeps_last_notes() {
        let conn = setup();
        let id = insert_task(&conn, "review", 2);
        verdict(&conn, id, 1, 1, "failed", "old failure");
        decision(&conn, id, 1, "changes_requested", "fix subtraction");
        verdict(&conn, id, 2, 0, "passed", "cargo test adds");
        verdict(&conn, id, 2, 1, "passed", "cargo test subtracts");

        // The specs appear once, not once per revision.
        assert_eq!(
            work_order(&conn, id, Role::Reviewer).unwrap(),
            format!(
                "#{id} rev2 Calc\nchanges|fix subtraction\n\
                 0|adds|1+1|2|passed|cargo test adds\n\
                 1|subtracts|3-1|2|passed|cargo test subtracts"
            )
        );
    }

    #[test]
    fn show_omits_empty_fields_and_merges_results_only_after_submission() {
        let conn = setup();
        let id = conn
            .execute(
                "INSERT INTO tasks (title, priority, tests) VALUES ('Bare', 'low', '[\"c\"]')",
                [],
            )
            .map(|_| conn.last_insert_rowid())
            .unwrap();
        assert_eq!(
            show(&conn, id, false).unwrap(),
            format!("#{id} low todo Bare\n0|||")
        );

        let reviewed = insert_task(&conn, "review", 1);
        verdict(&conn, reviewed, 1, 0, "passed", "ok");
        assert_eq!(
            show(&conn, reviewed, false).unwrap(),
            format!(
                "#{reviewed} high review rev1 Calc\ntags|math|core\n\
                 0|adds|1+1|2|passed|ok\n1|subtracts|3-1|2"
            )
        );

        // Sent back for rework: verdicts are history, notes are outstanding.
        decision(&conn, reviewed, 1, "changes_requested", "more tests");
        conn.execute(
            "UPDATE tasks SET status = 'in_progress' WHERE id = ?1",
            [reviewed],
        )
        .unwrap();
        assert_eq!(
            show(&conn, reviewed, false).unwrap(),
            format!(
                "#{reviewed} high in_progress rev1 Calc\ntags|math|core\n\
                 changes|more tests\n0|adds|1+1|2\n1|subtracts|3-1|2"
            )
        );
    }

    #[test]
    fn show_names_the_holder_of_a_live_lease_only() {
        let conn = setup();
        let id = insert_task(&conn, "in_progress", 0);
        conn.execute(
            "UPDATE tasks SET executor = 1, claimed_at = datetime('now'),
                 lease_expires_at = datetime('now', '+1 hour') WHERE id = ?1",
            [id],
        )
        .unwrap();
        let shown = show(&conn, id, false).unwrap();
        assert_eq!(
            first_line(&shown),
            format!("#{id} high in_progress@dev Calc")
        );
        assert_eq!(parse_show(&shown)["executor"], "dev");

        conn.execute(
            "UPDATE tasks SET lease_expires_at = datetime('now', '-1 second') WHERE id = ?1",
            [id],
        )
        .unwrap();
        let shown = show(&conn, id, false).unwrap();
        assert_eq!(first_line(&shown), format!("#{id} high in_progress Calc"));
    }

    #[test]
    fn show_reports_unfinished_prerequisites_and_dependents_only() {
        let conn = setup();
        let (a, b, c) = (
            insert_task(&conn, "todo", 0),
            insert_task(&conn, "todo", 0),
            insert_task(&conn, "todo", 0),
        );
        deps::add_prerequisites(&conn, b, &[a]).unwrap();
        deps::add_prerequisites(&conn, c, &[b]).unwrap();
        let middle = show(&conn, b, false).unwrap();
        assert_eq!(
            first_line(&middle),
            format!("#{b} high todo after:{a} blocks:{c} Calc")
        );
        let parsed = parse_show(&middle);
        assert_eq!(parsed["after"], json!([a]));
        assert_eq!(parsed["blocks"], json!([c]));

        conn.execute("UPDATE tasks SET status = 'done' WHERE id = ?1", [a])
            .unwrap();
        // `a` is finished: it no longer holds `b` back, and nothing waits on it.
        assert_eq!(
            first_line(&show(&conn, b, false).unwrap()),
            format!("#{b} high todo blocks:{c} Calc")
        );
        assert_eq!(
            first_line(&show(&conn, a, false).unwrap()),
            format!("#{a} high done Calc")
        );

        // Several of each are comma-joined, in id order.
        let d = insert_task(&conn, "todo", 0);
        deps::add_prerequisites(&conn, d, &[c, b]).unwrap();
        assert_eq!(
            first_line(&show(&conn, d, false).unwrap()),
            format!("#{d} high todo after:{b},{c} Calc")
        );
    }

    #[test]
    fn done_task_does_not_resurface_resolved_changes() {
        let conn = setup();
        let id = insert_task(&conn, "done", 2);
        decision(&conn, id, 1, "changes_requested", "fix it");
        decision(&conn, id, 2, "approved", "");
        let shown = show(&conn, id, false).unwrap();
        assert!(parse_show(&shown).get("changes").is_none(), "{shown}");
        assert!(!shown.contains("changes|"), "{shown}");
    }

    #[test]
    fn history_lists_every_revision_with_verdicts_and_decision() {
        let conn = setup();
        let id = insert_task(&conn, "done", 2);
        verdict(&conn, id, 1, 0, "passed", "ok");
        verdict(&conn, id, 1, 1, "failed", "broken");
        decision(&conn, id, 1, "changes_requested", "fix subtraction");
        verdict(&conn, id, 2, 0, "passed", "ok");
        verdict(&conn, id, 2, 1, "passed", "fixed");
        decision(&conn, id, 2, "approved", "");

        let reply = show(&conn, id, true).unwrap();
        assert_eq!(
            reply,
            format!(
                "#{id} high done rev2 Calc\ntags|math|core\n\
                 0|adds|1+1|2|passed|ok\n1|subtracts|3-1|2|passed|fixed\n\
                 rev1|dev|passed: ok|failed: broken\n\
                 review|rev|changes_requested|fix subtraction\n\
                 rev2|dev|passed: ok|passed: fixed\n\
                 review|rev|approved"
            )
        );
        assert_eq!(
            parse_show(&reply)["history"],
            json!([
                {"rev": 1, "by": "dev", "results": ["passed: ok", "failed: broken"],
                 "decision": "changes_requested", "notes": "fix subtraction", "reviewer": "rev"},
                {"rev": 2, "by": "dev", "results": ["passed: ok", "passed: fixed"],
                 "decision": "approved", "reviewer": "rev"},
            ])
        );
        // Without the flag there is no history.
        assert!(!show(&conn, id, false).unwrap().contains("rev1|"));
    }

    #[test]
    fn history_of_a_task_in_review_has_no_decision_yet() {
        let conn = setup();
        let id = insert_task(&conn, "review", 1);
        verdict(&conn, id, 1, 0, "passed", "ok");
        let reply = show(&conn, id, true).unwrap();
        assert_eq!(reply.lines().last(), Some("rev1|dev|passed: ok"));
        assert_eq!(
            parse_show(&reply)["history"],
            json!([{"rev": 1, "by": "dev", "results": ["passed: ok"]}])
        );
    }

    /// Free text is arbitrary; whatever it holds, a reply stays one line per
    /// record and reads back exactly.
    #[test]
    fn free_text_survives_every_reader() {
        let conn = setup();
        let tests = json!([
            {"describe": "pipes | and \\ backslashes", "input": "one\ntwo\r\n{\"k\": [1, 2]}", "output": ""},
            {"describe": "é 日本 🙂", "input": "||", "output": "\\n is not a newline"},
        ]);
        let title = "a | b \\ c \"d\"\nsecond line";
        let tags = json!(["x y", "p|q", "", "back\\slash"]);
        conn.execute(
            "INSERT INTO tasks (title, priority, status, tests, tags, revision)
             VALUES (?1, 'high', 'review', ?2, ?3, 1)",
            rusqlite::params![title, tests.to_string(), tags.to_string()],
        )
        .unwrap();
        let id = conn.last_insert_rowid();
        verdict(&conn, id, 1, 0, "passed", "evidence | with\nbreaks \\");
        verdict(&conn, id, 1, 1, "failed", "|");
        decision(&conn, id, 1, "changes_requested", "notes | with\nbreaks");

        let shown = show(&conn, id, true).unwrap();
        // header, tags, changes, 2 tests, revision, review: no stray lines.
        assert_eq!(shown.lines().count(), 7, "{shown}");
        let parsed = parse_show(&shown);
        assert_eq!(parsed["title"], title);
        assert_eq!(parsed["tags"], tags);
        assert_eq!(parsed["changes"], "notes | with\nbreaks");
        assert_eq!(parsed["tests"][0]["input"], tests[0]["input"]);
        assert_eq!(parsed["tests"][0]["evidence"], "evidence | with\nbreaks \\");
        assert_eq!(parsed["tests"][1]["output"], tests[1]["output"]);
        assert_eq!(parsed["tests"][1]["evidence"], "|");
        assert_eq!(
            parsed["history"][0]["results"],
            json!(["passed: evidence | with\nbreaks \\", "failed: |"])
        );
        assert_eq!(parsed["history"][0]["notes"], "notes | with\nbreaks");

        for role in [Role::Developer, Role::Reviewer] {
            let order = work_order(&conn, id, role).unwrap();
            assert_eq!(order.lines().count(), 4, "{order}");
            let parsed = parse_order(&order);
            assert_eq!(parsed["title"], title);
            assert_eq!(parsed["changes"], "notes | with\nbreaks");
            assert_eq!(parsed["tests"][0]["describe"], tests[0]["describe"]);
        }
    }

    #[test]
    fn unknown_task_is_reported_by_every_reader() {
        let conn = setup();
        assert_eq!(
            load(&conn, 99).err().unwrap().to_string(),
            "task 99 not found"
        );
        assert!(show(&conn, 99, false).is_err());
        assert!(work_order(&conn, 99, Role::Developer).is_err());
    }

    /// The DB `CHECK` guarantees `tags`/`tests` are valid JSON via SQLite's
    /// `json_valid()`, a different parser than `serde_json`; `load` falls
    /// back to empty rather than erroring if the two ever disagree. Driven
    /// with a synthetic row since no insert can get past the `CHECK`.
    #[test]
    fn load_degrades_to_empty_on_unparseable_json() {
        let conn = setup();
        conn.execute_batch("PRAGMA ignore_check_constraints = ON;")
            .unwrap();
        conn.execute(
            "INSERT INTO tasks (title, priority, tests, tags) VALUES ('t', 'low', 'nope', 'nope')",
            [],
        )
        .unwrap();
        let task = load(&conn, conn.last_insert_rowid()).unwrap();
        assert!(task.tags.is_empty());
        assert!(task.tests.is_empty());
        assert_eq!(task.title, "t");
    }

    #[test]
    fn expired_lease_means_no_executor() {
        let conn = setup();
        let id = insert_task(&conn, "in_progress", 0);
        conn.execute(
            "UPDATE tasks SET executor = 1, claimed_at = datetime('now'),
                 lease_expires_at = datetime('now', '+1 hour') WHERE id = ?1",
            [id],
        )
        .unwrap();
        assert_eq!(load(&conn, id).unwrap().executor.as_deref(), Some("dev"));
        conn.execute(
            "UPDATE tasks SET lease_expires_at = datetime('now', '-1 second') WHERE id = ?1",
            [id],
        )
        .unwrap();
        assert_eq!(load(&conn, id).unwrap().executor, None);
    }
}
