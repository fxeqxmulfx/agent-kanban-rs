//! `wait IDS`: whoever hands out work waits for it in one call, instead of
//! asking `show` again and again until it says `done`.

use std::thread;
use std::time::{Duration, Instant};

use anyhow::{Result, bail};
use rusqlite::Connection;

use super::{lifecycle, mail, task, view};

/// How often `wait` looks again.
const POLL: Duration = Duration::from_secs(1);

/// `wait IDS [--timeout SECS]` -> the `list` lines of the tasks once every
/// one of them is done. Named by `--agent`, the waiter is also told of its
/// mail: the wait ends as soon as mail waits for it. When the time is up the
/// lines say how far the work got.
///
/// It only reads, once a second, and holds nothing in between, so waiting
/// never gets in the way of the agents doing the work. A task removed while
/// it is waited for ends the wait with an error, since it can never be done.
pub fn wait(ids: &[i64], timeout_seconds: Option<u32>, me: Option<&str>) -> Result<String> {
    let conn = crate::db::open_existing()?;
    if let Some(me) = me {
        lifecycle::resolve_agent(&conn, me, None)?;
    }
    let deadline =
        timeout_seconds.map(|seconds| Instant::now() + Duration::from_secs(u64::from(seconds)));
    loop {
        let (lines, over) = look(&conn, ids, me)?;
        let left = deadline.map(|deadline| deadline.saturating_duration_since(Instant::now()));
        if over || left.is_some_and(|left| left.is_zero()) {
            return Ok(lines);
        }
        thread::sleep(left.map_or(POLL, |left| left.min(POLL)));
    }
}

/// The tasks' lines, and whether waiting is over: all of them are done, or
/// mail waits for `me`.
fn look(conn: &Connection, ids: &[i64], me: Option<&str>) -> Result<(String, bool)> {
    let rows = task::rows_of(conn, ids)?;
    if let Some(missing) = ids.iter().find(|id| !rows.iter().any(|row| row.id == **id)) {
        bail!("task {missing} not found");
    }
    let done = rows.iter().all(|row| row.status == "done");
    let mail = match me {
        Some(me) => mail::unread(conn, me)? > 0,
        None => false,
    };
    let lines: Vec<String> = rows.iter().map(view::line).collect();
    Ok((lines.join("\n"), done || mail))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Task 1 done, task 2 in progress with alice, and bob with no mail.
    fn setup() -> Connection {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch("PRAGMA foreign_keys=ON;").unwrap();
        conn.execute_batch(crate::db::SCHEMA).unwrap();
        conn.execute_batch(
            "INSERT INTO agents (name, role) VALUES ('alice', 'developer'), ('bob', 'reviewer');
             INSERT INTO tasks (title, priority, tests, status) VALUES ('Old', 'low', '[1]', 'done');
             INSERT INTO tasks (title, priority, tests, status, executor, claimed_at,
                                lease_expires_at)
               VALUES ('New', 'high', '[1]', 'in_progress', 1, datetime('now'),
                       datetime('now', '+1 hour'));",
        )
        .unwrap();
        conn
    }

    #[test]
    fn waiting_is_over_once_every_task_is_done() {
        let conn = setup();
        assert_eq!(
            look(&conn, &[1], None).unwrap(),
            ("#1 low done Old".to_string(), true)
        );
        assert_eq!(
            look(&conn, &[2, 1], None).unwrap(),
            (
                "#2 high in_progress@alice New\n#1 low done Old".to_string(),
                false
            ),
            "in `list` order"
        );
        assert_eq!(
            look(&conn, &[1, 1], None).unwrap().0,
            "#1 low done Old",
            "one line per task"
        );
    }

    #[test]
    fn mail_ends_the_wait_of_whoever_it_is_for() {
        let conn = setup();
        conn.execute(
            "INSERT INTO messages (sender, recipient, body) VALUES ('alice', 'bob', 'hi')",
            [],
        )
        .unwrap();

        assert!(look(&conn, &[2], Some("bob")).unwrap().1);
        assert!(!look(&conn, &[2], Some("alice")).unwrap().1);
        assert!(!look(&conn, &[2], None).unwrap().1);
    }

    #[test]
    fn a_task_that_is_not_there_cannot_be_waited_for() {
        let conn = setup();
        assert_eq!(
            look(&conn, &[1, 3], None).unwrap_err().to_string(),
            "task 3 not found"
        );
    }
}
