//! `dump --full` and `load`: the board as text to keep in git, and the board
//! back from that text.
//!
//! `dump` is for reading and leaves out what an agent never needs. A full dump
//! leaves out nothing: agents, times, leases, every dependency (done
//! prerequisites too), the test each result was given against and the id
//! counter, so that `load` rebuilds the same board. Its order is fixed (agents
//! and tasks by id, a task's history by revision): the same board always
//! prints the same text, and a change to the board changes only its own lines.
//!
//! ```text
//! agent-kanban full dump 1          the version of this format
//! next|8                            the id the next `add` gets
//! agent|ID|NAME|ROLE|CREATED        one line per agent
//!                                   a blank line before each task
//! task|ID|PRIORITY|STATUS|REVISION|CREATED|UPDATED|TITLE
//! lease|AGENT|CLAIMED|EXPIRES       while an agent holds the task
//! tags|TAG|TAG                      when there are tags
//! after|3,5                         every prerequisite, done ones too
//! IDX|DESCRIBE|INPUT|OUTPUT         one line per test, as `show` prints it
//! result|REV|IDX|RESULT|EVIDENCE|AGENT|BY|AT[|DESCRIBE|INPUT|OUTPUT]
//! review|REV|DECISION|NOTES|AGENT|BY|AT
//!                                   a blank line
//! end                               the file is whole
//! ```
//!
//! AGENT is an agent's id, empty once that agent has been removed; BY is the
//! name recorded with the result or review. A result repeats the test it
//! judged only if that test has changed since. Times are UTC, as SQLite writes
//! them. Cells are escaped as in every reply (see [`view::cell`]).
//!
//! `load` checks the whole file before it writes anything, then writes it in
//! one transaction: the board becomes the file, or stays as it was. `dump
//! --full` runs the same checks on what it prints, so a dump that printed
//! loads.

use std::collections::{HashMap, HashSet};
use std::io::Read;
use std::path::Path;

use anyhow::{Result, anyhow, bail};
use rusqlite::{Connection, TransactionBehavior};
use serde_json::{Value, json};

use super::{PRIORITIES, STATUSES, deps, view};

/// The first line of a full dump, up to its version.
const HEADER: &str = "agent-kanban full dump";
/// The version of the format this build writes, and the newest it reads.
const FORMAT: u32 = 1;

const ROLES: [&str; 2] = ["developer", "reviewer"];
const RESULTS: [&str; 2] = ["passed", "failed"];
const DECISIONS: [&str; 2] = ["approved", "changes_requested"];

/// Everything a board holds.
#[derive(Default)]
struct Board {
    /// The id the next `add` gets: one past the highest id ever handed out.
    next: i64,
    agents: Vec<Agent>,
    tasks: Vec<Task>,
}

struct Agent {
    id: i64,
    name: String,
    role: String,
    created: String,
}

struct Task {
    id: i64,
    priority: String,
    status: String,
    revision: i64,
    created: String,
    updated: String,
    title: String,
    lease: Option<Lease>,
    tags: Vec<String>,
    /// Every prerequisite, done or not.
    after: Vec<i64>,
    tests: Vec<Spec>,
    results: Vec<Verdict>,
    reviews: Vec<Review>,
}

/// One acceptance test.
#[derive(Clone, Default, PartialEq, Eq)]
struct Spec {
    describe: String,
    input: String,
    output: String,
}

impl Spec {
    /// The JSON the CLI stores for a test.
    fn json(&self) -> Value {
        json!({"describe": self.describe, "input": self.input, "output": self.output})
    }

    /// A test as the CLI stores it: an object of exactly these three strings.
    fn from_json(value: &Value) -> Option<Self> {
        let fields = value.as_object().filter(|fields| fields.len() == 3)?;
        let text = |key: &str| fields.get(key)?.as_str().map(str::to_string);
        Some(Self {
            describe: text("describe")?,
            input: text("input")?,
            output: text("output")?,
        })
    }
}

/// The agent holding a task, and until when.
struct Lease {
    agent: i64,
    claimed: String,
    expires: String,
}

/// A developer's result for one test of one revision.
struct Verdict {
    revision: i64,
    index: i64,
    result: String,
    evidence: String,
    /// The developer's agent id; `None` once that agent has been removed.
    agent: Option<i64>,
    by: String,
    at: String,
    /// The test as it was when it was judged.
    criterion: Spec,
}

/// A reviewer's decision on one revision.
struct Review {
    revision: i64,
    decision: String,
    notes: String,
    agent: Option<i64>,
    by: String,
    at: String,
}

/// `dump --full` -> the whole board in the format above. It is read in one
/// transaction, like `dump`, so it is the board at one moment.
pub fn dump() -> Result<String> {
    let mut conn = crate::db::open_existing()?;
    let snapshot = conn.transaction()?;
    let text = render(&read(&snapshot)?);
    // Only a database edited by hand holds what `load` refuses. Say so now,
    // not on the day the file is needed back.
    if let Err((line, err)) = parse_lines(&text) {
        let shown = text.lines().nth(line - 1).unwrap_or_default();
        bail!("cannot dump: `load` would refuse `{shown}`: {err}");
    }
    Ok(text)
}

/// `load FILE [--replace]` -> `loaded tasks N, agents M`; `-` is standard
/// input. Fills an empty board; `replace` lets it replace one that is not.
pub fn load(file: &Path, replace: bool) -> Result<String> {
    let mut conn = crate::db::open_existing()?;
    let board = parse(&read_file(file)?)?;
    let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
    restore(&tx, &board, replace)?;
    tx.commit()?;
    Ok(format!(
        "loaded tasks {}, agents {}",
        board.tasks.len(),
        board.agents.len()
    ))
}

fn read_file(file: &Path) -> Result<String> {
    let text = if file == Path::new("-") {
        let mut text = String::new();
        std::io::stdin().read_to_string(&mut text).map(|_| text)
    } else {
        std::fs::read_to_string(file)
    };
    text.map_err(|err| anyhow!("cannot read {}: {err}", file.display()))
}

/// Put `board` in place of what the database holds, inside the caller's
/// transaction.
fn restore(tx: &Connection, board: &Board, replace: bool) -> Result<()> {
    let (tasks, agents): (i64, i64) = tx.query_row(
        "SELECT (SELECT COUNT(*) FROM tasks), (SELECT COUNT(*) FROM agents)",
        [],
        |row| Ok((row.get(0)?, row.get(1)?)),
    )?;
    if tasks + agents > 0 {
        if !replace {
            bail!(
                "the board is not empty (tasks {tasks}, agents {agents}); \
                 `load --replace FILE` replaces it"
            );
        }
        tx.execute_batch(
            "DELETE FROM task_deps; DELETE FROM acceptance_results; DELETE FROM review_history;
             DELETE FROM tasks; DELETE FROM agents;",
        )?;
    }
    // An id this database has handed out stays spent, even when the file is
    // older, so that a stale `#7` an agent still holds never names a task
    // that came from the file.
    let spent = last_id(tx)?;
    write(tx, board)?;
    let last = (board.next - 1).max(spent);
    let set = tx.execute(
        "UPDATE sqlite_sequence SET seq = ?1 WHERE name = 'tasks'",
        [last],
    )?;
    if set == 0 && last > 0 {
        tx.execute(
            "INSERT INTO sqlite_sequence (name, seq) VALUES ('tasks', ?1)",
            [last],
        )?;
    }
    Ok(())
}

/// The highest task id ever handed out, or 0: SQLite keeps it in
/// `sqlite_sequence` for an `AUTOINCREMENT` table.
fn last_id(conn: &Connection) -> Result<i64> {
    Ok(conn.query_row(
        "SELECT COALESCE(MAX(seq), 0) FROM sqlite_sequence WHERE name = 'tasks'",
        [],
        |row| row.get(0),
    )?)
}

/// Insert every row of `board` into empty tables. [`parse`] has checked all
/// that the schema checks, so this does not fail for a board it read.
fn write(tx: &Connection, board: &Board) -> Result<()> {
    let mut insert_agent =
        tx.prepare("INSERT INTO agents (id, name, role, created_at) VALUES (?1, ?2, ?3, ?4)")?;
    for agent in &board.agents {
        insert_agent.execute(rusqlite::params![
            agent.id,
            agent.name,
            agent.role,
            agent.created
        ])?;
    }
    let mut insert_task = tx.prepare(
        "INSERT INTO tasks (id, priority, status, revision, created_at, updated_at, title,
                            executor, claimed_at, lease_expires_at, tags, tests)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12)",
    )?;
    for task in &board.tasks {
        let lease = task.lease.as_ref();
        let tests: Vec<Value> = task.tests.iter().map(Spec::json).collect();
        insert_task.execute(rusqlite::params![
            task.id,
            task.priority,
            task.status,
            task.revision,
            task.created,
            task.updated,
            task.title,
            lease.map(|lease| lease.agent),
            lease.map(|lease| &lease.claimed),
            lease.map(|lease| &lease.expires),
            json!(task.tags).to_string(),
            Value::Array(tests).to_string(),
        ])?;
    }
    // Every task is in before an edge points at it.
    let mut insert_edge =
        tx.prepare("INSERT INTO task_deps (task_id, depends_on) VALUES (?1, ?2)")?;
    let mut insert_result = tx.prepare(
        "INSERT INTO acceptance_results (task_id, revision, criterion_index, criterion, result,
                                         evidence, executor, executor_name, verified_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
    )?;
    let mut insert_review = tx.prepare(
        "INSERT INTO review_history (task_id, revision, decision, notes, executor,
                                     executor_name, created_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
    )?;
    for task in &board.tasks {
        for prerequisite in &task.after {
            insert_edge.execute([task.id, *prerequisite])?;
        }
        for result in &task.results {
            insert_result.execute(rusqlite::params![
                task.id,
                result.revision,
                result.index,
                result.criterion.json().to_string(),
                result.result,
                result.evidence,
                result.agent,
                result.by,
                result.at,
            ])?;
        }
        for review in &task.reviews {
            insert_review.execute(rusqlite::params![
                task.id,
                review.revision,
                review.decision,
                review.notes,
                review.agent,
                review.by,
                review.at,
            ])?;
        }
    }
    Ok(())
}

/// Everything the database holds.
fn read(conn: &Connection) -> Result<Board> {
    let agents = conn
        .prepare("SELECT id, name, role, created_at FROM agents ORDER BY id")?
        .query_map([], |row| {
            Ok(Agent {
                id: row.get(0)?,
                name: row.get(1)?,
                role: row.get(2)?,
                created: row.get(3)?,
            })
        })?
        .collect::<rusqlite::Result<_>>()?;
    let mut tasks = read_tasks(conn)?;
    read_history(conn, &mut tasks)?;
    Ok(Board {
        next: last_id(conn)? + 1,
        agents,
        tasks,
    })
}

fn read_tasks(conn: &Connection) -> Result<Vec<Task>> {
    let mut stmt = conn.prepare(
        "SELECT id, priority, status, revision, created_at, updated_at, title,
                executor, claimed_at, lease_expires_at, tags, tests
         FROM tasks ORDER BY id",
    )?;
    let mut rows = stmt.query([])?;
    let mut tasks = Vec::new();
    while let Some(row) = rows.next()? {
        let id = row.get(0)?;
        // The schema sets the three together or not at all.
        let lease = row
            .get::<_, Option<i64>>(7)?
            .zip(row.get::<_, Option<String>>(8)?)
            .zip(row.get::<_, Option<String>>(9)?)
            .map(|((agent, claimed), expires)| Lease {
                agent,
                claimed,
                expires,
            });
        let tags: String = row.get(10)?;
        let tests: String = row.get(11)?;
        tasks.push(Task {
            id,
            priority: row.get(1)?,
            status: row.get(2)?,
            revision: row.get(3)?,
            created: row.get(4)?,
            updated: row.get(5)?,
            title: row.get(6)?,
            lease,
            tags: serde_json::from_str(&tags).map_err(|_| foreign(id, "tags", &tags))?,
            after: Vec::new(),
            tests: serde_json::from_str::<Vec<Value>>(&tests)
                .ok()
                .and_then(|tests| tests.iter().map(Spec::from_json).collect())
                .ok_or_else(|| foreign(id, "tests", &tests))?,
            results: Vec::new(),
            reviews: Vec::new(),
        });
    }
    Ok(tasks)
}

/// Each task's prerequisites, results and reviews.
fn read_history(conn: &Connection, tasks: &mut [Task]) -> Result<()> {
    let mut after =
        conn.prepare("SELECT depends_on FROM task_deps WHERE task_id = ?1 ORDER BY depends_on")?;
    let mut results = conn.prepare(
        "SELECT revision, criterion_index, result, evidence, executor, executor_name,
                verified_at, criterion
         FROM acceptance_results WHERE task_id = ?1 ORDER BY revision, criterion_index",
    )?;
    let mut reviews = conn.prepare(
        "SELECT revision, decision, notes, executor, executor_name, created_at
         FROM review_history WHERE task_id = ?1 ORDER BY revision",
    )?;
    for task in tasks {
        task.after = after
            .query_map([task.id], |row| row.get(0))?
            .collect::<rusqlite::Result<_>>()?;
        task.reviews = reviews
            .query_map([task.id], |row| {
                Ok(Review {
                    revision: row.get(0)?,
                    decision: row.get(1)?,
                    notes: row.get(2)?,
                    agent: row.get(3)?,
                    by: row.get(4)?,
                    at: row.get(5)?,
                })
            })?
            .collect::<rusqlite::Result<_>>()?;
        let mut rows = results.query([task.id])?;
        while let Some(row) = rows.next()? {
            let criterion: String = row.get(7)?;
            task.results.push(Verdict {
                revision: row.get(0)?,
                index: row.get(1)?,
                result: row.get(2)?,
                evidence: row.get(3)?,
                agent: row.get(4)?,
                by: row.get(5)?,
                at: row.get(6)?,
                criterion: serde_json::from_str(&criterion)
                    .ok()
                    .as_ref()
                    .and_then(Spec::from_json)
                    .ok_or_else(|| foreign(task.id, "a result's test", &criterion))?,
            });
        }
    }
    Ok(())
}

/// JSON that the CLI never writes, which only editing the database by hand
/// leaves behind: a full dump could not give it back as it is.
fn foreign(id: i64, what: &str, json: &str) -> anyhow::Error {
    anyhow!(
        "task {id} has {what} that agent-kanban never writes ({json}); a full dump cannot keep it"
    )
}

fn render(board: &Board) -> String {
    let mut lines = vec![format!("{HEADER} {FORMAT}"), format!("next|{}", board.next)];
    for agent in &board.agents {
        lines.push(row(&[
            "agent",
            &agent.id.to_string(),
            &agent.name,
            &agent.role,
            &agent.created,
        ]));
    }
    for task in &board.tasks {
        lines.push(String::new());
        render_task(task, &mut lines);
    }
    lines.extend([String::new(), "end".to_string()]);
    lines.join("\n")
}

/// Cells joined into a `|` line, each escaped.
fn row(cells: &[&str]) -> String {
    cells
        .iter()
        .map(|text| view::cell(text))
        .collect::<Vec<_>>()
        .join("|")
}

fn render_task(task: &Task, lines: &mut Vec<String>) {
    lines.push(row(&[
        "task",
        &task.id.to_string(),
        &task.priority,
        &task.status,
        &task.revision.to_string(),
        &task.created,
        &task.updated,
        &task.title,
    ]));
    if let Some(lease) = &task.lease {
        lines.push(row(&[
            "lease",
            &lease.agent.to_string(),
            &lease.claimed,
            &lease.expires,
        ]));
    }
    if !task.tags.is_empty() {
        let mut cells = vec!["tags"];
        cells.extend(task.tags.iter().map(String::as_str));
        lines.push(row(&cells));
    }
    if !task.after.is_empty() {
        lines.push(format!("after|{}", deps::join(&task.after)));
    }
    for (index, test) in task.tests.iter().enumerate() {
        lines.push(row(&[
            &index.to_string(),
            &test.describe,
            &test.input,
            &test.output,
        ]));
    }
    // Each revision's results, then its review.
    let mut reviews = task.reviews.iter().peekable();
    for result in &task.results {
        while let Some(review) = reviews.next_if(|review| review.revision < result.revision) {
            lines.push(review_line(review));
        }
        lines.push(result_line(task, result));
    }
    lines.extend(reviews.map(review_line));
}

fn result_line(task: &Task, result: &Verdict) -> String {
    let (revision, index, agent) = (
        result.revision.to_string(),
        result.index.to_string(),
        agent_cell(result.agent),
    );
    let mut cells = vec![
        "result",
        revision.as_str(),
        index.as_str(),
        result.result.as_str(),
        result.evidence.as_str(),
        agent.as_str(),
        result.by.as_str(),
        result.at.as_str(),
    ];
    let current = usize::try_from(result.index)
        .ok()
        .and_then(|index| task.tests.get(index));
    if current != Some(&result.criterion) {
        let judged = &result.criterion;
        cells.extend([
            judged.describe.as_str(),
            judged.input.as_str(),
            judged.output.as_str(),
        ]);
    }
    row(&cells)
}

fn review_line(review: &Review) -> String {
    row(&[
        "review",
        &review.revision.to_string(),
        &review.decision,
        &review.notes,
        &agent_cell(review.agent),
        &review.by,
        &review.at,
    ])
}

fn agent_cell(agent: Option<i64>) -> String {
    agent.map(|id| id.to_string()).unwrap_or_default()
}

/// Read a full dump back, or say on which line it goes wrong, and why.
fn parse(text: &str) -> Result<Board> {
    parse_lines(text).map_err(|(line, err)| anyhow!("line {line}: {err}"))
}

/// A refusal, and the number of the line it is about.
type Refusal = (usize, anyhow::Error);

/// [`parse`], with the line of a refusal kept apart from its reason.
fn parse_lines(text: &str) -> Result<Board, Refusal> {
    let text = text.strip_prefix('\u{feff}').unwrap_or(text);
    let mut lines = (1..).zip(text.lines());
    header(lines.next().map_or("", |(_, first)| first)).map_err(|err| (1, err))?;
    let mut reader = Reader::default();
    for (number, line) in lines {
        if !line.trim().is_empty() {
            reader.record(number, line).map_err(|err| (number, err))?;
        }
    }
    reader.finish(text.lines().count())
}

/// The first line: `agent-kanban full dump N`, N a version this build reads.
fn header(line: &str) -> Result<()> {
    if line.starts_with('#') || line == "no tasks" {
        bail!("this is a `dump` for reading; load reads what `dump --full` writes");
    }
    let version = line
        .strip_prefix(HEADER)
        .and_then(|rest| rest.strip_prefix(' '))
        .and_then(|version| version.parse::<u32>().ok())
        .filter(|version| *version > 0)
        .ok_or_else(|| anyhow!("not a full dump: its first line is `{HEADER} {FORMAT}`"))?;
    if version > FORMAT {
        bail!(
            "written by a newer agent-kanban (format {version}; this build reads {FORMAT}); \
             upgrade agent-kanban"
        );
    }
    Ok(())
}

/// Reads a full dump one line at a time. Each value is checked as the schema
/// would check it, so that writing what was read cannot fail halfway; what
/// spans lines is checked once every line is in.
#[derive(Default)]
struct Reader {
    board: Board,
    /// The line of the `next` record, once read.
    next_line: Option<usize>,
    /// The line of each task's `task` record.
    task_lines: HashMap<i64, usize>,
    /// The line of each task's `after` record.
    after_lines: HashMap<i64, usize>,
    /// Results that leave out the test they judged, which is then the task's
    /// test with their index: (line, task, result), the last two positions.
    unnamed: Vec<(usize, usize, usize)>,
    /// Whether the `end` line has been read.
    ended: bool,
}

impl Reader {
    fn record(&mut self, number: usize, line: &str) -> Result<()> {
        if self.ended {
            bail!("a line after `end`");
        }
        let mut cells = view::split(line)?.into_iter();
        let kind = cells.next().unwrap_or_default();
        let values: Vec<String> = cells.collect();
        match kind.as_str() {
            "end" if values.is_empty() => {
                self.ended = true;
                Ok(())
            }
            "end" => Err(arity("end", "no values", &values)),
            "next" => self.next(number, &values),
            "agent" => self.agent(&values),
            "task" => self.task(number, &values),
            "lease" => self.lease(&values),
            "tags" => self.tags(&values),
            "after" => self.after(number, &values),
            "result" => self.result(number, &values),
            "review" => self.review(&values),
            index if !index.is_empty() && index.bytes().all(|byte| byte.is_ascii_digit()) => {
                self.test(index, &values)
            }
            other => bail!("unknown record '{other}'"),
        }
    }

    /// `next` and `agent` describe the whole board, so they come first.
    fn before_tasks(&self, kind: &str) -> Result<()> {
        if self.board.tasks.is_empty() {
            Ok(())
        } else {
            bail!("`{kind}` lines come before the first task")
        }
    }

    /// The position of the task a line belongs to: the last one begun.
    fn current(&self, what: &str) -> Result<usize> {
        self.board
            .tasks
            .len()
            .checked_sub(1)
            .ok_or_else(|| anyhow!("{what} must follow a `task` line"))
    }

    fn next(&mut self, number: usize, values: &[String]) -> Result<()> {
        self.before_tasks("next")?;
        let [next] = values else {
            return Err(arity("next", "1 value", values));
        };
        if self.next_line.replace(number).is_some() {
            bail!("a second `next` line");
        }
        self.board.next = id_number(next)?;
        Ok(())
    }

    fn agent(&mut self, values: &[String]) -> Result<()> {
        self.before_tasks("agent")?;
        let [id, name, role, created] = values else {
            return Err(arity("agent", "4 values", values));
        };
        let id = id_number(id)?;
        if self.board.agents.iter().any(|agent| agent.id == id) {
            bail!("a second agent {id}");
        }
        if self.board.agents.iter().any(|agent| agent.name == *name) {
            bail!("a second agent named '{name}'");
        }
        self.board.agents.push(Agent {
            id,
            name: name.clone(),
            role: one_of("role", role, &ROLES)?,
            created: time(created)?,
        });
        Ok(())
    }

    fn task(&mut self, number: usize, values: &[String]) -> Result<()> {
        let [id, priority, status, revision, created, updated, title] = values else {
            return Err(arity("task", "7 values", values));
        };
        let id = id_number(id)?;
        if let Some(first) = self.task_lines.insert(id, number) {
            bail!("task {id} is already on line {first}");
        }
        let revision = integer(revision)?;
        if revision < 0 {
            bail!("revision {revision} is below 0");
        }
        self.board.tasks.push(Task {
            id,
            priority: one_of("priority", priority, &PRIORITIES)?,
            status: one_of("status", status, &STATUSES)?,
            revision,
            created: time(created)?,
            updated: time(updated)?,
            title: title.clone(),
            lease: None,
            tags: Vec::new(),
            after: Vec::new(),
            tests: Vec::new(),
            results: Vec::new(),
            reviews: Vec::new(),
        });
        Ok(())
    }

    fn lease(&mut self, values: &[String]) -> Result<()> {
        let position = self.current("`lease`")?;
        let [agent, claimed, expires] = values else {
            return Err(arity("lease", "3 values", values));
        };
        let lease = Lease {
            agent: self.agent_id(agent)?,
            claimed: time(claimed)?,
            expires: time(expires)?,
        };
        let task = &mut self.board.tasks[position];
        if task.status == "done" {
            bail!("task {} is done, and nobody holds a done task", task.id);
        }
        if task.lease.replace(lease).is_some() {
            bail!("a second `lease` line for task {}", task.id);
        }
        Ok(())
    }

    fn tags(&mut self, values: &[String]) -> Result<()> {
        let position = self.current("`tags`")?;
        let task = &mut self.board.tasks[position];
        if values.is_empty() {
            bail!("`tags` needs at least one tag");
        }
        if !task.tags.is_empty() {
            bail!("a second `tags` line for task {}", task.id);
        }
        task.tags = values.to_vec();
        Ok(())
    }

    fn after(&mut self, number: usize, values: &[String]) -> Result<()> {
        let position = self.current("`after`")?;
        let [ids] = values else {
            return Err(arity("after", "1 value", values));
        };
        let task = &mut self.board.tasks[position];
        if !task.after.is_empty() {
            bail!("a second `after` line for task {}", task.id);
        }
        for prerequisite in ids.split(',') {
            let prerequisite = integer(prerequisite)?;
            if prerequisite == task.id {
                bail!("task {prerequisite} cannot come after itself");
            }
            if task.after.contains(&prerequisite) {
                bail!("task {} comes after {prerequisite} twice", task.id);
            }
            task.after.push(prerequisite);
        }
        let id = task.id;
        self.after_lines.insert(id, number);
        Ok(())
    }

    fn test(&mut self, index: &str, values: &[String]) -> Result<()> {
        let position = self.current("a test line")?;
        let [describe, input, output] = values else {
            return Err(arity(index, "3 values", values));
        };
        let task = &mut self.board.tasks[position];
        let expected = task.tests.len().to_string();
        if index != expected {
            bail!("test {index} where test {expected} comes next");
        }
        task.tests.push(Spec {
            describe: describe.clone(),
            input: input.clone(),
            output: output.clone(),
        });
        Ok(())
    }

    fn result(&mut self, number: usize, values: &[String]) -> Result<()> {
        let position = self.current("`result`")?;
        let [
            revision,
            index,
            result,
            evidence,
            agent,
            by,
            at,
            judged @ ..,
        ] = values
        else {
            return Err(arity("result", "7 or 10 values", values));
        };
        let criterion = match judged {
            [] => None,
            [describe, input, output] => Some(Spec {
                describe: describe.clone(),
                input: input.clone(),
                output: output.clone(),
            }),
            _ => return Err(arity("result", "7 or 10 values", values)),
        };
        let revision = history_revision(revision)?;
        let index = integer(index)?;
        if index < 0 {
            bail!("test index {index} is below 0");
        }
        let result = one_of("result", result, &RESULTS)?;
        // As the schema has it: SQLite's trim() takes off spaces only.
        if evidence.trim_matches(' ').is_empty() {
            bail!("evidence must not be empty");
        }
        let agent = self.optional_agent(agent)?;
        let at = time(at)?;
        let task = &mut self.board.tasks[position];
        if task
            .results
            .iter()
            .any(|known| known.revision == revision && known.index == index)
        {
            bail!(
                "a second result for test {index} of task {} rev{revision}",
                task.id
            );
        }
        if criterion.is_none() {
            self.unnamed.push((number, position, task.results.len()));
        }
        task.results.push(Verdict {
            revision,
            index,
            result,
            evidence: evidence.clone(),
            agent,
            by: by.clone(),
            at,
            criterion: criterion.unwrap_or_default(),
        });
        Ok(())
    }

    fn review(&mut self, values: &[String]) -> Result<()> {
        let position = self.current("`review`")?;
        let [revision, decision, notes, agent, by, at] = values else {
            return Err(arity("review", "6 values", values));
        };
        let revision = history_revision(revision)?;
        let decision = one_of("decision", decision, &DECISIONS)?;
        let agent = self.optional_agent(agent)?;
        let at = time(at)?;
        let task = &mut self.board.tasks[position];
        if task.reviews.iter().any(|known| known.revision == revision) {
            bail!("a second review of task {} rev{revision}", task.id);
        }
        task.reviews.push(Review {
            revision,
            decision,
            notes: notes.clone(),
            agent,
            by: by.clone(),
            at,
        });
        Ok(())
    }

    /// An agent the file has already listed.
    fn agent_id(&self, text: &str) -> Result<i64> {
        let id = integer(text)?;
        if self.board.agents.iter().any(|agent| agent.id == id) {
            Ok(id)
        } else {
            bail!("agent {id} is not in the file")
        }
    }

    /// The same, or nothing for an agent that has been removed.
    fn optional_agent(&self, text: &str) -> Result<Option<i64>> {
        if text.is_empty() {
            Ok(None)
        } else {
            self.agent_id(text).map(Some)
        }
    }

    /// The checks that need every line; `lines` is how many the file has.
    fn finish(mut self, lines: usize) -> Result<Board, Refusal> {
        // A file cut short loses its last tasks without a trace, and `load
        // --replace` would then drop them from the board.
        if !self.ended {
            return Err((lines + 1, anyhow!("no `end` line: the file is cut short")));
        }
        let next_line = self
            .next_line
            .ok_or_else(|| (2, anyhow!("no `next` line")))?;
        for task in &self.board.tasks {
            if task.tests.is_empty() {
                let line = self.task_lines[&task.id];
                return Err((line, anyhow!("task {} has no tests", task.id)));
            }
        }
        for &(line, task, result) in &self.unnamed {
            let task = &mut self.board.tasks[task];
            let index = task.results[result].index;
            let test = usize::try_from(index)
                .ok()
                .and_then(|index| task.tests.get(index))
                .cloned()
                .ok_or_else(|| {
                    let reason = anyhow!(
                        "task {} has no test {index}, so the result must name the test it judged",
                        task.id
                    );
                    (line, reason)
                })?;
            task.results[result].criterion = test;
        }
        if let Some(last) = self.board.tasks.iter().map(|task| task.id).max()
            && self.board.next <= last
        {
            let reason = anyhow!("next {} is not above task {last}", self.board.next);
            return Err((next_line, reason));
        }
        self.check_dependencies()?;
        Ok(self.board)
    }

    /// Every prerequisite is a task of the file, and no task waits for itself
    /// through others: what `add --after` makes sure of.
    fn check_dependencies(&self) -> Result<(), Refusal> {
        let after: HashMap<i64, &[i64]> = self
            .board
            .tasks
            .iter()
            .map(|task| (task.id, task.after.as_slice()))
            .collect();
        for task in &self.board.tasks {
            if let Some(missing) = task.after.iter().find(|id| !after.contains_key(id)) {
                let line = self.after_lines[&task.id];
                return Err((line, anyhow!("task {missing} is not in the file")));
            }
        }
        if let Some((task, prerequisite)) = closing_edge(&self.board.tasks, &after) {
            let reason = anyhow!(
                "task {task} cannot come after {prerequisite}: {prerequisite} already comes \
                 after {task} (cycle)"
            );
            return Err((self.after_lines[&task], reason));
        }
        Ok(())
    }
}

/// A prerequisite that closes a cycle, as (task, prerequisite), if there is
/// one. Each task's prerequisites are walked depth first; one that is still on
/// the path being walked already waits, through that path, for the task.
/// Every task is walked once, so a long chain costs no more than its length.
fn closing_edge(tasks: &[Task], after: &HashMap<i64, &[i64]>) -> Option<(i64, i64)> {
    let mut finished = HashSet::new();
    for task in tasks {
        // Each task on the path, with how many of its prerequisites have been
        // taken; the same tasks as a set.
        let mut path = vec![(task.id, 0)];
        let mut on_path = HashSet::from([task.id]);
        while let Some((id, taken)) = path.last_mut() {
            let id = *id;
            let Some(&prerequisite) = after[&id].get(*taken) else {
                path.pop();
                on_path.remove(&id);
                finished.insert(id);
                continue;
            };
            *taken += 1;
            if on_path.contains(&prerequisite) {
                return Some((id, prerequisite));
            }
            if !finished.contains(&prerequisite) {
                path.push((prerequisite, 0));
                on_path.insert(prerequisite);
            }
        }
    }
    None
}

fn arity(kind: &str, count: &str, values: &[String]) -> anyhow::Error {
    anyhow!("`{kind}` takes {count} after it, not {}", values.len())
}

fn integer(text: &str) -> Result<i64> {
    text.parse()
        .map_err(|_| anyhow!("'{text}' is not a whole number"))
}

/// Ids, of tasks and of agents, start at 1.
fn id_number(text: &str) -> Result<i64> {
    let id = integer(text)?;
    if id < 1 {
        bail!("'{id}' is not an id: ids start at 1");
    }
    Ok(id)
}

/// Results and reviews belong to a revision, and revisions start at 1.
fn history_revision(text: &str) -> Result<i64> {
    let revision = integer(text)?;
    if revision < 1 {
        bail!("revision {revision} is below 1");
    }
    Ok(revision)
}

fn one_of(what: &str, value: &str, allowed: &[&str]) -> Result<String> {
    if allowed.contains(&value) {
        Ok(value.to_string())
    } else {
        bail!(
            "invalid {what} '{value}': must be one of {}",
            allowed.join(", ")
        )
    }
}

/// A time as SQLite's `datetime()` writes it: `YYYY-MM-DD HH:MM:SS`.
fn time(text: &str) -> Result<String> {
    let shaped = text.len() == 19
        && text.bytes().enumerate().all(|(at, byte)| match at {
            4 | 7 => byte == b'-',
            10 => byte == b' ',
            13 | 16 => byte == b':',
            _ => byte.is_ascii_digit(),
        });
    if shaped {
        Ok(text.to_string())
    } else {
        bail!("'{text}' is not a time like 2026-01-31 23:59:00")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A full dump with every kind of line, exactly as `dump --full > FILE`
    /// writes it.
    const FULL: &str = include_str!("../../tests/fixtures/full_dump.txt");

    /// A full dump of an empty board that has never handed out an id.
    const EMPTY: &str = "agent-kanban full dump 1\nnext|1\n\nend\n";

    fn setup() -> Connection {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch("PRAGMA foreign_keys=ON;").unwrap();
        conn.execute_batch(crate::db::SCHEMA).unwrap();
        conn
    }

    /// What `dump --full` prints for the board in `conn`, final newline
    /// included.
    fn dumped(conn: &Connection) -> String {
        render(&read(conn).unwrap()) + "\n"
    }

    fn reread(text: &str) -> String {
        match parse(text) {
            Ok(board) => render(&board) + "\n",
            Err(err) => panic!("{err}\n{text}"),
        }
    }

    fn refusal(text: &str) -> String {
        match parse(text) {
            Ok(_) => panic!("accepted:\n{text}"),
            Err(err) => err.to_string(),
        }
    }

    /// The id the next task added to `conn` gets.
    fn next_id(conn: &Connection) -> i64 {
        conn.execute(
            "INSERT INTO tasks (title, priority, tests) VALUES ('t', 'low', '[1]')",
            [],
        )
        .unwrap();
        conn.last_insert_rowid()
    }

    #[test]
    fn a_full_dump_reads_back_to_the_same_text() {
        assert_eq!(reread(FULL), FULL);
        assert_eq!(reread(EMPTY), EMPTY);
    }

    /// What editors and git may do to a file: a byte order mark, `\r\n` line
    /// ends, blank lines lost or added, no final newline.
    #[test]
    fn line_ends_and_blank_lines_change_nothing() {
        for text in [
            format!("\u{feff}{}", FULL.replace('\n', "\r\n")),
            FULL.replace("\n\n", "\n"),
            FULL.replace("\n\n", "\n\n  \n\n"),
            FULL.trim_end().to_string(),
        ] {
            assert_eq!(reread(&text), FULL, "{text}");
        }
    }

    #[test]
    fn what_is_written_reads_back_the_same() {
        let conn = setup();

        restore(&conn, &parse(FULL).unwrap(), false).unwrap();

        assert_eq!(dumped(&conn), FULL);
        assert_eq!(next_id(&conn), 10);
    }

    /// Edits to [`FULL`] that a hand edit or a bad merge could make, and what
    /// `load` says about each: (text, its replacement, the refusal). The line
    /// numbers are those of the edited file.
    const REFUSALS: &[(&str, &str, &str)] = &[
        (
            "agent-kanban full dump 1",
            "agent-kanban full dump 2",
            "line 1: written by a newer agent-kanban (format 2; this build reads 1); \
                 upgrade agent-kanban",
        ),
        (
            "agent-kanban full dump 1",
            "agent-kanban full dump 0",
            "line 1: not a full dump: its first line is `agent-kanban full dump 1`",
        ),
        (
            "agent-kanban full dump 1",
            "agent-kanban full dump",
            "line 1: not a full dump: its first line is `agent-kanban full dump 1`",
        ),
        (
            "agent-kanban full dump 1",
            "#1 high done rev2 Parser",
            "line 1: this is a `dump` for reading; load reads what `dump --full` writes",
        ),
        ("next|10", "next|7", "line 2: next 7 is not above task 7"),
        (
            "next|10",
            "next|0",
            "line 2: '0' is not an id: ids start at 1",
        ),
        ("next|10\n", "", "line 2: no `next` line"),
        (
            "next|10",
            "next|10|11",
            "line 2: `next` takes 1 value after it, not 2",
        ),
        (
            "agent|1|",
            "next|10\nagent|1|",
            "line 3: a second `next` line",
        ),
        (
            "\nend\n",
            "\nagent|9|late|developer|2026-10-01 09:00:00\nend\n",
            "line 28: `agent` lines come before the first task",
        ),
        ("agent|3|", "agent|1|", "line 4: a second agent 1"),
        (
            r"agent|3|rev \| lead",
            "agent|3|dev",
            "line 4: a second agent named 'dev'",
        ),
        (
            "|developer|",
            "|boss|",
            "line 3: invalid role 'boss': must be one of developer, reviewer",
        ),
        (
            "agent|1|dev|developer|",
            "agent|1|dev|",
            "line 3: `agent` takes 4 values after it, not 3",
        ),
        (
            "2026-10-01 09:00:05",
            "yesterday",
            "line 4: 'yesterday' is not a time like 2026-01-31 23:59:00",
        ),
        (
            "2026-10-01 09:00:05",
            "2026-10-01T09:00:05",
            "line 4: '2026-10-01T09:00:05' is not a time like 2026-01-31 23:59:00",
        ),
        (
            "task|7|low|backlog|0|",
            "task|7|low|backlog|",
            "line 24: `task` takes 7 values after it, not 6",
        ),
        (
            "task|7|",
            "task|4|",
            "line 24: task 4 is already on line 18",
        ),
        ("task|7|", "task|x|", "line 24: 'x' is not a whole number"),
        (
            "task|7|",
            "task|0|",
            "line 24: '0' is not an id: ids start at 1",
        ),
        (
            "task|7|low|",
            "task|7|huge|",
            "line 24: invalid priority 'huge': must be one of low, medium, high, urgent",
        ),
        (
            "|backlog|",
            "|waiting|",
            "line 24: invalid status 'waiting': must be one of backlog, todo, in_progress, \
                 review, done",
        ),
        (
            "|backlog|0|",
            "|backlog|-1|",
            "line 24: revision -1 is below 0",
        ),
        (
            "\ntask|1|",
            "\nlease|1|2026-10-01 11:00:00|2026-10-01 12:00:00\ntask|1|",
            "line 6: `lease` must follow a `task` line",
        ),
        (
            "\ntask|1|",
            "\n0|a|b|c\ntask|1|",
            "line 6: a test line must follow a `task` line",
        ),
        (
            "tags|api",
            "lease|1|2026-10-01 11:00:00|2026-10-01 12:00:00\ntags|api",
            "line 7: task 1 is done, and nobody holds a done task",
        ),
        (
            "after|1\n",
            "lease|1|2026-10-01 11:00:00|2026-10-01 12:00:00\nafter|1\n",
            "line 20: a second `lease` line for task 4",
        ),
        (
            "lease|3|",
            "lease|5|",
            "line 19: agent 5 is not in the file",
        ),
        (
            "lease|3|2026-10-01 11:00:00|",
            "lease|3|",
            "line 19: `lease` takes 3 values after it, not 2",
        ),
        (
            "tags|api|needs review",
            "tags",
            "line 7: `tags` needs at least one tag",
        ),
        (
            "0|parses",
            "tags|more\n0|parses",
            "line 8: a second `tags` line for task 1",
        ),
        (
            "after|1,4",
            "after|1,7",
            "line 25: task 7 cannot come after itself",
        ),
        (
            "after|1,4",
            "after|1,1",
            "line 25: task 7 comes after 1 twice",
        ),
        (
            "after|1,4",
            "after|1,8",
            "line 25: task 8 is not in the file",
        ),
        (
            "after|1,4",
            "after|1,x",
            "line 25: 'x' is not a whole number",
        ),
        (
            "after|1,4",
            "after|1|4",
            "line 25: `after` takes 1 value after it, not 2",
        ),
        (
            "after|1\n",
            "after|1\nafter|7\n",
            "line 21: a second `after` line for task 4",
        ),
        (
            "tags|api",
            "after|7\ntags|api",
            "line 26: task 7 cannot come after 1: 1 already comes after 7 (cycle)",
        ),
        (
            "1|fails cleanly",
            "2|fails cleanly",
            "line 9: test 2 where test 1 comes next",
        ),
        (
            "0|parses|1+1|2",
            "0|parses|1+1",
            "line 8: `0` takes 3 values after it, not 2",
        ),
        (
            "after|1,4\n0|works|in|out\n",
            "after|1,4\n",
            "line 24: task 7 has no tests",
        ),
        (
            "result|1|0|passed|ok|1|dev|2026-10-01 09:10:00",
            "result|1|0|passed|ok|1|dev|2026-10-01 09:10:00|x",
            "line 10: `result` takes 7 or 10 values after it, not 8",
        ),
        (
            "result|1|0|passed|ok|1|dev|2026-10-01 09:10:00",
            "result|1|0|passed|ok|1|dev",
            "line 10: `result` takes 7 or 10 values after it, not 6",
        ),
        (
            "result|1|0|",
            "result|0|0|",
            "line 10: revision 0 is below 1",
        ),
        (
            "result|1|0|",
            "result|1|-1|",
            "line 10: test index -1 is below 0",
        ),
        (
            "|passed|ok|",
            "|skipped|ok|",
            "line 10: invalid result 'skipped': must be one of passed, failed",
        ),
        (
            "|passed|ok|",
            "|passed|  |",
            "line 10: evidence must not be empty",
        ),
        (
            "result|1|0|passed|ok|1|",
            "result|1|0|passed|ok|2|",
            "line 10: agent 2 is not in the file",
        ),
        (
            "result|1|1|",
            "result|1|0|",
            "line 11: a second result for test 0 of task 1 rev1",
        ),
        (
            "|dropped test|1|dev|2026-10-01 09:30:00|old|in|out",
            "|dropped test|1|dev|2026-10-01 09:30:00",
            "line 15: task 1 has no test 2, so the result must name the test it judged",
        ),
        (
            "review|2|",
            "review|1|",
            "line 16: a second review of task 1 rev1",
        ),
        (
            "review|2|approved|",
            "review|2|maybe|",
            "line 16: invalid decision 'maybe': must be one of approved, changes_requested",
        ),
        (
            "review|2|approved||3|",
            "review|2|approved||5|",
            "line 16: agent 5 is not in the file",
        ),
        (
            "review|2|approved||3|",
            "review|2|approved||",
            "line 16: `review` takes 6 values after it, not 5",
        ),
        ("tags|api", "oops|api", "line 7: unknown record 'oops'"),
        (
            "tags|api",
            r"tags|a\pi",
            r"line 7: a \ must start \\, \|, \n or \r",
        ),
        (
            "\nend\n",
            "\n",
            "line 28: no `end` line: the file is cut short",
        ),
        (
            "\nend\n",
            "\nend|now\n",
            "line 28: `end` takes no values after it, not 1",
        ),
        ("\nend\n", "\nend\nend\n", "line 29: a line after `end`"),
    ];

    #[test]
    fn what_the_board_could_not_hold_is_refused_with_its_line() {
        for &(from, to, expected) in REFUSALS {
            assert!(FULL.contains(from), "{from}");
            let text = FULL.replacen(from, to, 1);
            assert_eq!(refusal(&text), expected, "{from} -> {to}");
        }
        for (text, expected) in [
            (
                "",
                "line 1: not a full dump: its first line is `agent-kanban full dump 1`",
            ),
            (
                "no tasks\n",
                "line 1: this is a `dump` for reading; load reads what `dump --full` writes",
            ),
        ] {
            assert_eq!(refusal(text), expected, "{text:?}");
        }
    }

    /// What the schema takes, `load` takes: no rule of its own may make a
    /// board that `dump --full` printed impossible to load.
    #[test]
    fn what_the_schema_allows_is_not_refused() {
        // SQLite's trim() takes off spaces only.
        let tab = FULL.replacen("|passed|ok|", "|passed|\t|", 1);
        // An unnamed result whose test was changed since: the current test
        // is taken for the one it judged.
        let unnamed = FULL.replacen(
            "|panics|1|dev|2026-10-01 09:10:00|fails|1/0|err",
            "|panics|1|dev|2026-10-01 09:10:00",
            1,
        );
        // Names and titles with spaces, empty notes and evidence of one
        // character, as 0.1.x boards may hold.
        let loose = FULL
            .replacen("agent|1|dev|", "agent|1| dev two |", 1)
            .replacen("|Someday", "|", 1);
        for text in [tab, unnamed, loose] {
            assert_eq!(reread(&text), text);
        }
    }

    #[test]
    fn a_board_that_is_not_empty_is_replaced_only_when_asked() {
        let board = parse(FULL).unwrap();
        let conn = setup();
        restore(&conn, &board, false).unwrap();

        let err = restore(&conn, &parse(EMPTY).unwrap(), false).unwrap_err();
        assert_eq!(
            err.to_string(),
            "the board is not empty (tasks 3, agents 2); `load --replace FILE` replaces it"
        );
        // Agents alone make a board that is not empty.
        let agents_only = setup();
        agents_only
            .execute("INSERT INTO agents (name) VALUES ('dev')", [])
            .unwrap();
        assert!(restore(&agents_only, &board, false).is_err());

        // Every row goes, whatever points at it: leases, history, edges.
        restore(&conn, &parse(EMPTY).unwrap(), true).unwrap();
        for table in [
            "agents",
            "tasks",
            "task_deps",
            "acceptance_results",
            "review_history",
        ] {
            let rows: i64 = conn
                .query_row(&format!("SELECT COUNT(*) FROM {table}"), [], |row| {
                    row.get(0)
                })
                .unwrap();
            assert_eq!(rows, 0, "{table}");
        }
        restore(&conn, &board, true).unwrap();
        assert_eq!(dumped(&conn), FULL);
    }

    /// The counter only goes up: a file from before tasks were added does
    /// not let the board hand their ids out again.
    #[test]
    fn an_id_the_board_handed_out_is_never_handed_out_again() {
        let ahead = FULL.replacen("next|10", "next|30", 1);
        let conn = setup();

        restore(&conn, &parse(&ahead).unwrap(), false).unwrap();
        assert_eq!(last_id(&conn).unwrap(), 29);
        restore(&conn, &parse(FULL).unwrap(), true).unwrap();
        assert_eq!(last_id(&conn).unwrap(), 29);
        restore(&conn, &parse(EMPTY).unwrap(), true).unwrap();
        assert_eq!(dumped(&conn), "agent-kanban full dump 1\nnext|30\n\nend\n");
        assert_eq!(next_id(&conn), 30);

        let fresh = setup();
        restore(&fresh, &parse(EMPTY).unwrap(), false).unwrap();
        assert_eq!(next_id(&fresh), 1);

        // Every task removed, their ids spent all the same.
        let emptied = EMPTY.replacen("next|1", "next|8", 1);
        let fresh = setup();
        restore(&fresh, &parse(&emptied).unwrap(), false).unwrap();
        assert_eq!(dumped(&fresh), emptied);
        assert_eq!(next_id(&fresh), 8);
    }

    /// JSON that agent-kanban never writes cannot be put back as it was, so
    /// it stops the dump rather than turn into something else.
    #[test]
    fn json_the_cli_never_writes_stops_a_full_dump() {
        let test = r#"[{"describe":"d","input":"i","output":"o"}]"#;
        for (tags, tests, what, shown) in [
            ("[1]", test, "tags", "[1]"),
            (r#"{"a":"b"}"#, test, "tags", r#"{"a":"b"}"#),
            ("[]", r#"["c"]"#, "tests", r#"["c"]"#),
            (
                "[]",
                r#"[{"describe":"d","input":"i","output":"o","x":"y"}]"#,
                "tests",
                r#"[{"describe":"d","input":"i","output":"o","x":"y"}]"#,
            ),
            (
                "[]",
                r#"[{"describe":"d","input":"i","output":1}]"#,
                "tests",
                r#"[{"describe":"d","input":"i","output":1}]"#,
            ),
        ] {
            let conn = setup();
            conn.execute(
                "INSERT INTO tasks (title, priority, tags, tests) VALUES ('t', 'low', ?1, ?2)",
                [tags, tests],
            )
            .unwrap();
            let err = read(&conn).err().unwrap();
            assert_eq!(
                err.to_string(),
                format!(
                    "task 1 has {what} that agent-kanban never writes ({shown}); a full dump \
                     cannot keep it"
                )
            );
        }
        let conn = setup();
        conn.execute(
            "INSERT INTO tasks (title, priority, tests) VALUES ('t', 'low', ?1)",
            [test],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO acceptance_results (task_id, revision, criterion_index, criterion, result,
                                             evidence, executor_name)
             VALUES (1, 1, 0, '\"c\"', 'passed', 'ok', 'dev')",
            [],
        )
        .unwrap();
        assert_eq!(
            read(&conn).err().unwrap().to_string(),
            "task 1 has a result's test that agent-kanban never writes (\"c\"); a full dump \
             cannot keep it"
        );
    }

    /// Tasks 1..=`count`, each after the one before it.
    fn chain(count: i64) -> String {
        let mut lines = vec![format!("{HEADER} {FORMAT}"), format!("next|{}", count + 1)];
        for id in 1..=count {
            lines.push(String::new());
            lines.push(format!(
                "task|{id}|low|todo|0|2026-10-01 09:00:00|2026-10-01 09:00:00|t"
            ));
            if id > 1 {
                lines.push(format!("after|{}", id - 1));
            }
            lines.push("0|a|b|c".to_string());
        }
        lines.extend([String::new(), "end".to_string(), String::new()]);
        lines.join("\n")
    }

    /// A long chain is walked once, not once per edge, and closing it is
    /// found however far the cycle reaches.
    #[test]
    fn a_cycle_is_found_at_any_depth() {
        let text = chain(20_000);
        assert!(
            reread(&text) == text,
            "the chain does not read back as it was"
        );

        let closed = text.replacen("|t\n0|a|b|c\n", "|t\nafter|20000\n0|a|b|c\n", 1);
        assert_eq!(
            refusal(&closed),
            "line 9: task 2 cannot come after 1: 1 already comes after 2 (cycle)"
        );
    }
}
