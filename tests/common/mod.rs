//! Helpers shared by the integration tests. Every test runs the compiled
//! `agent-kanban` binary as a subprocess inside its own temporary directory,
//! so tests never interfere with each other or with the repository.

// Each test crate compiles this module and uses a different subset of it.
#![allow(dead_code)]

use assert_cmd::Command;
use rusqlite::Connection;
use serde_json::{Value, json};
use std::path::{Path, PathBuf};
use std::sync::Barrier;
use std::time::Duration;
use tempfile::TempDir;

/// A fresh, empty directory (no board yet).
pub fn project() -> TempDir {
    TempDir::new().unwrap()
}

/// A directory with an initialized board.
pub fn initialized() -> TempDir {
    let dir = project();
    run(&dir, &["init"]);
    dir
}

/// `agent-kanban` rooted at `dir`. The agent identity variable is removed so
/// a test never inherits one from the environment it runs in.
pub fn kanban(dir: impl AsRef<Path>) -> Command {
    let mut cmd = Command::cargo_bin("agent-kanban").unwrap();
    cmd.current_dir(dir.as_ref());
    cmd.env_remove("AGENT_KANBAN_AGENT");
    cmd
}

fn text(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes).trim_end().to_string()
}

/// Run a command that must succeed; its stdout without the final newline.
pub fn run(dir: impl AsRef<Path>, args: &[&str]) -> String {
    let output = kanban(dir).args(args).output().unwrap();
    assert!(
        output.status.success(),
        "{args:?} failed: stdout={} stderr={}",
        text(&output.stdout),
        text(&output.stderr)
    );
    text(&output.stdout)
}

/// Run a command that must fail with exit code 1 (the command was understood
/// but refused); its stderr without the final newline. Nothing goes to stdout.
pub fn fail(dir: impl AsRef<Path>, args: &[&str]) -> String {
    let output = kanban(dir).args(args).output().unwrap();
    assert_eq!(
        output.status.code(),
        Some(1),
        "{args:?} should be refused: stdout={} stderr={}",
        text(&output.stdout),
        text(&output.stderr)
    );
    assert!(
        output.stdout.is_empty(),
        "{args:?} failed but wrote stdout: {}",
        text(&output.stdout)
    );
    text(&output.stderr)
}

/// Run a command that must be rejected by argument parsing (exit code 2); its
/// stderr without the final newline.
pub fn usage_error(dir: impl AsRef<Path>, args: &[&str]) -> String {
    let output = kanban(dir).args(args).output().unwrap();
    assert_eq!(
        output.status.code(),
        Some(2),
        "{args:?} should be a usage error: stdout={} stderr={}",
        text(&output.stdout),
        text(&output.stderr)
    );
    text(&output.stderr)
}

/// Like [`run`], for the commands that describe a task (`show`, `claim`,
/// `claim-next`): the reply read back as a JSON object, so tests assert on
/// facts and not on layout. Exact layouts are pinned by their own tests.
pub fn task(dir: impl AsRef<Path>, args: &[&str]) -> Value {
    let reply = run(dir, args);
    if args[0] == "show" {
        parse_show(&reply)
    } else {
        parse_order(&reply)
    }
}

/// A `show` reply: `#7 high review@bob rev1 after:3 blocks:9 Title`, then
/// `tags|..`, `changes|..`, one `IDX|DESCRIBE|INPUT|OUTPUT[|RESULT|EVIDENCE]`
/// row per test and, with `--history`, `revN|DEV|RESULT: EVIDENCE|..` and
/// `review|WHO|DECISION[|NOTES]` rows.
pub fn parse_show(reply: &str) -> Value {
    parse_task(reply, true)
}

/// A `claim` / `claim-next` reply: `#7 [revN] Title`, then the same rows.
pub fn parse_order(reply: &str) -> Value {
    parse_task(reply, false)
}

fn parse_task(reply: &str, with_state: bool) -> Value {
    let mut lines = reply.lines();
    let mut task = parse_header(lines.next().expect("an empty reply"), with_state);
    let mut tests = Vec::new();
    let mut history: Vec<Value> = Vec::new();
    for line in lines {
        let cells = split_cells(line);
        let kind = cells[0].as_str();
        if !kind.is_empty() && kind.bytes().all(|b| b.is_ascii_digit()) {
            assert_eq!(kind, tests.len().to_string(), "tests count from 0: {line}");
            assert!(
                matches!(cells.len(), 4 | 6),
                "a test row has 4 or 6 cells: {line}"
            );
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
            assert!(matches!(cells.len(), 3 | 4), "{line}");
            let entry = history
                .last_mut()
                .expect("a review row before any revision");
            entry["reviewer"] = json!(cells[1]);
            entry["decision"] = json!(cells[2]);
            if let Some(notes) = cells.get(3) {
                entry["notes"] = json!(notes);
            }
        } else if let Some(revision) = kind.strip_prefix("rev") {
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

fn parse_header(line: &str, with_state: bool) -> Value {
    let mut rest = line
        .strip_prefix('#')
        .unwrap_or_else(|| panic!("a task reply starts with #ID: {line}"));
    let mut task = serde_json::Map::new();
    let id: i64 = next_word(&mut rest).parse().expect("a numeric id");
    task.insert("id".into(), json!(id));
    if with_state {
        task.insert("priority".into(), json!(next_word(&mut rest)));
        let status = next_word(&mut rest);
        let (status, executor) = match status.split_once('@') {
            Some((status, executor)) => (status, Some(executor)),
            None => (status, None),
        };
        task.insert("status".into(), json!(status));
        if let Some(executor) = executor {
            task.insert("executor".into(), json!(executor));
        }
    }
    // Optional labelled words in front of the title, which is the rest of the line.
    loop {
        let (word, tail) = rest.split_once(' ').unwrap_or((rest, ""));
        let ids = |label: &str| {
            word.strip_prefix(label).map(|ids| {
                ids.split(',')
                    .map(|id| id.parse::<i64>().expect("ids"))
                    .collect::<Vec<_>>()
            })
        };
        if let Some(revision) = word.strip_prefix("rev").and_then(|n| n.parse::<i64>().ok()) {
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
    let mut title = split_cells(rest);
    assert_eq!(title.len(), 1, "a title is one escaped cell: {line}");
    task.insert("title".into(), json!(title.remove(0)));
    Value::Object(task)
}

fn next_word<'a>(rest: &mut &'a str) -> &'a str {
    let (word, tail) = rest.split_once(' ').unwrap_or((*rest, ""));
    *rest = tail;
    word
}

/// Split a row at its unescaped `|` and undo the escaping (`\\`, `\|`, `\n`,
/// `\r`) of every cell.
pub fn split_cells(line: &str) -> Vec<String> {
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

pub fn register(dir: impl AsRef<Path>, name: &str, role: &str) {
    run(dir, &["agent", "register", name, "--role", role]);
}

/// The id in a reply that begins `#N ...`.
pub fn id_of(reply: &str) -> i64 {
    reply
        .strip_prefix('#')
        .and_then(|rest| rest.split(' ').next())
        .and_then(|id| id.parse().ok())
        .unwrap_or_else(|| panic!("reply does not start with #ID: {reply}"))
}

/// Add a task with one test and return its id.
pub fn add_task(dir: impl AsRef<Path>, title: &str, priority: &str) -> i64 {
    id_of(&run(
        dir,
        &[
            "add",
            "--title",
            title,
            "--priority",
            priority,
            "--test",
            "works",
            "in",
            "out",
        ],
    ))
}

/// Add a one-test task that waits for `after`; returns its id.
pub fn add_task_after(dir: impl AsRef<Path>, title: &str, after: &[i64]) -> i64 {
    let after = after
        .iter()
        .map(i64::to_string)
        .collect::<Vec<_>>()
        .join(",");
    id_of(&run(
        dir,
        &[
            "add", "--title", title, "--test", "works", "in", "out", "--after", &after,
        ],
    ))
}

pub fn db_path(dir: impl AsRef<Path>) -> PathBuf {
    dir.as_ref().join(".kanban").join("board.db")
}

/// Open the board's database directly, to arrange or inspect state the CLI
/// cannot (an expired lease, a raw row).
pub fn db(dir: impl AsRef<Path>) -> Connection {
    let conn = Connection::open(db_path(dir)).unwrap();
    conn.busy_timeout(Duration::from_secs(5)).unwrap();
    conn.execute_batch("PRAGMA foreign_keys=ON;").unwrap();
    conn
}

/// Make the lease on task `id` run out one second ago.
pub fn expire_lease(dir: impl AsRef<Path>, id: i64) {
    db(dir)
        .execute(
            "UPDATE tasks SET lease_expires_at = datetime('now', '-1 second') WHERE id = ?1",
            [id],
        )
        .unwrap();
}

/// What one finished `agent-kanban` process did.
#[derive(Debug)]
pub struct Outcome {
    pub code: Option<i32>,
    pub stdout: String,
    pub stderr: String,
}

impl Outcome {
    pub fn ok(&self) -> bool {
        self.code == Some(0)
    }
}

/// `["claim", "7"]` as the owned argument list [`race`] wants.
pub fn argv(args: &[&str]) -> Vec<String> {
    args.iter().map(ToString::to_string).collect()
}

/// Run every command in its own process, all released at the same moment, and
/// return the outcomes in the order given.
///
/// Panics when a process leaks a raw `SQLite` error (lock, busy, constraint)
/// instead of a documented message: contention must never be visible to the
/// caller. Everything else is for the test to assert, normally an invariant
/// that holds whoever wins.
pub fn race(dir: impl AsRef<Path>, commands: &[Vec<String>]) -> Vec<Outcome> {
    let dir = dir.as_ref();
    let barrier = Barrier::new(commands.len());
    let outcomes: Vec<Outcome> = std::thread::scope(|scope| {
        let handles: Vec<_> = commands
            .iter()
            .map(|args| {
                let barrier = &barrier;
                scope.spawn(move || {
                    barrier.wait();
                    let output = kanban(dir).args(args).output().unwrap();
                    Outcome {
                        code: output.status.code(),
                        stdout: text(&output.stdout),
                        stderr: text(&output.stderr),
                    }
                })
            })
            .collect();
        handles
            .into_iter()
            .map(|handle| handle.join().unwrap())
            .collect()
    });
    for (args, outcome) in commands.iter().zip(&outcomes) {
        // "blocked by unfinished tasks" is a documented refusal and contains
        // "locked"; take it out before looking for database lock errors.
        let stderr = outcome.stderr.to_lowercase().replace("blocked", "");
        let leak = ["locked", "busy", "foreign key", "constraint", "sqlite"]
            .into_iter()
            .find(|word| stderr.contains(word));
        assert!(
            leak.is_none(),
            "{args:?} leaked a raw database error ({leak:?}): {}",
            outcome.stderr
        );
    }
    outcomes
}
