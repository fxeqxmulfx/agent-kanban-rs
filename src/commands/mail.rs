//! Mail between agents: `send` leaves a message on the board for another
//! agent, and `inbox` hands an agent its new messages, each one once.
//!
//! Mail is addressed by name, so it outlives the agents that sent or got it:
//! `agent remove` keeps it, and whoever registers the name again finds what
//! was left for it. A message may name the task it is about; ids are never
//! reused, so its `#7` stays right even after that task is removed.

use anyhow::{Result, bail};
use rusqlite::{Connection, OptionalExtension};

use super::{lifecycle, view};

/// `send NAME TEXT [--task ID]` -> `sent`.
pub fn send(from: &str, to: &str, text: &str, task: Option<i64>) -> Result<String> {
    lifecycle::write(|tx| send_inner(tx, from, to, text, task))
}

fn send_inner(
    conn: &Connection,
    from: &str,
    to: &str,
    text: &str,
    task: Option<i64>,
) -> Result<String> {
    lifecycle::resolve_agent(conn, from, None)?;
    let to = to.trim();
    if !is_registered(conn, to)? {
        bail!("agent '{to}' not found; `agent-kanban agent list` shows who is registered");
    }
    // Kept trimmed, like a review's notes.
    let text = text.trim();
    if text.is_empty() {
        bail!("message must not be empty");
    }
    if let Some(id) = task
        && !task_exists(conn, id)?
    {
        bail!("task {id} not found");
    }
    conn.execute(
        "INSERT INTO messages (sender, recipient, task_id, body) VALUES (?1, ?2, ?3, ?4)",
        rusqlite::params![from, to, task, text],
    )?;
    Ok("sent".to_string())
}

/// `inbox` -> one `FROM|[#ID]|TEXT` row per new message, oldest first, or
/// `no mail`. What it prints is marked read in the same transaction, so two
/// `inbox` calls never hand out the same message.
pub fn inbox(me: &str) -> Result<String> {
    lifecycle::write(|tx| inbox_inner(tx, me))
}

fn inbox_inner(conn: &Connection, me: &str) -> Result<String> {
    lifecycle::resolve_agent(conn, me, None)?;
    let mut stmt = conn.prepare(
        "UPDATE messages SET read_at = datetime('now')
         WHERE recipient = ?1 AND read_at IS NULL
         RETURNING id, sender, task_id, body",
    )?;
    let mut mail = stmt
        .query_map([me], |row| {
            Ok((
                row.get::<_, i64>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, Option<i64>>(2)?,
                row.get::<_, String>(3)?,
            ))
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    if mail.is_empty() {
        return Ok("no mail".to_string());
    }
    // SQLite hands back the updated rows in no promised order.
    mail.sort_unstable_by_key(|(id, ..)| *id);
    let rows: Vec<String> = mail
        .iter()
        .map(|(_, from, task, body)| {
            let task = task.map(|id| format!("#{id}")).unwrap_or_default();
            format!("{}|{task}|{}", view::cell(from), view::cell(body))
        })
        .collect();
    Ok(rows.join("\n"))
}

fn is_registered(conn: &Connection, name: &str) -> Result<bool> {
    Ok(conn
        .query_row("SELECT 1 FROM agents WHERE name = ?1", [name], |_| Ok(()))
        .optional()?
        .is_some())
}

fn task_exists(conn: &Connection, id: i64) -> Result<bool> {
    Ok(conn
        .query_row("SELECT 1 FROM tasks WHERE id = ?1", [id], |_| Ok(()))
        .optional()?
        .is_some())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A board with a developer, a reviewer and task 1.
    fn setup() -> Connection {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch("PRAGMA foreign_keys=ON;").unwrap();
        conn.execute_batch(crate::db::SCHEMA).unwrap();
        conn.execute_batch(
            "INSERT INTO agents (name, role) VALUES ('alice', 'developer'), ('bob', 'reviewer');
             INSERT INTO tasks (title, priority, tests) VALUES ('t', 'low', '[1]');",
        )
        .unwrap();
        conn
    }

    #[test]
    fn mail_arrives_once_oldest_first() {
        let conn = setup();
        assert_eq!(inbox_inner(&conn, "bob").unwrap(), "no mail");

        assert_eq!(
            send_inner(&conn, "alice", "bob", "first", Some(1)).unwrap(),
            "sent"
        );
        send_inner(&conn, "alice", "bob", "second", None).unwrap();

        assert_eq!(
            inbox_inner(&conn, "bob").unwrap(),
            "alice|#1|first\nalice||second"
        );
        assert_eq!(inbox_inner(&conn, "bob").unwrap(), "no mail");
        send_inner(&conn, "alice", "bob", "third", None).unwrap();
        assert_eq!(inbox_inner(&conn, "bob").unwrap(), "alice||third");
    }

    #[test]
    fn each_agent_gets_only_its_own_mail() {
        let conn = setup();
        send_inner(&conn, "alice", "bob", "for bob", None).unwrap();
        send_inner(&conn, "bob", "alice", "for alice", None).unwrap();

        assert_eq!(inbox_inner(&conn, "alice").unwrap(), "bob||for alice");
        assert_eq!(inbox_inner(&conn, "bob").unwrap(), "alice||for bob");
    }

    #[test]
    fn text_is_escaped_like_every_cell() {
        let conn = setup();
        send_inner(&conn, "alice", "bob", "a|b\\c\nnext line", None).unwrap();

        assert_eq!(
            inbox_inner(&conn, "bob").unwrap(),
            r"alice||a\|b\\c\nnext line"
        );
    }

    #[test]
    fn names_and_text_are_trimmed() {
        let conn = setup();
        send_inner(&conn, "alice", " bob\t", "\n  padded \n", None).unwrap();

        assert_eq!(inbox_inner(&conn, "bob").unwrap(), "alice||padded");
    }

    #[test]
    fn what_cannot_be_delivered_is_refused() {
        let conn = setup();
        let refusal = |from, to, text, task| {
            send_inner(&conn, from, to, text, task)
                .unwrap_err()
                .to_string()
        };

        assert_eq!(
            refusal("alice", "carol", "hi", None),
            "agent 'carol' not found; `agent-kanban agent list` shows who is registered"
        );
        assert_eq!(
            refusal("alice", "bob", " \n\t", None),
            "message must not be empty"
        );
        assert_eq!(refusal("alice", "bob", "hi", Some(2)), "task 2 not found");
        assert!(
            refusal("zed", "bob", "hi", None).starts_with("agent 'zed' is not registered"),
            "the sender must be registered too"
        );
        let stored: i64 = conn
            .query_row("SELECT COUNT(*) FROM messages", [], |row| row.get(0))
            .unwrap();
        assert_eq!(stored, 0);
    }
}
