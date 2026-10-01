//! The task lifecycle:
//!
//! ```text
//! todo --claim--> in_progress --submit-review--> review --approve--> done
//!                      ^                            |
//!                      +-------request-changes------+
//! ```
//!
//! `backlog` parks a task outside the flow (`move` goes between it and
//! `todo`). `claim` follows the caller's role: developers take `todo` /
//! `in_progress` work whose prerequisites are done, reviewers take `review`
//! work. Every command is one `IMMEDIATE` transaction: the write lock comes
//! first, so what a command checked is still true when it writes, and a
//! refusal can say exactly why.

use anyhow::{Context, Result, anyhow, bail};
use rusqlite::{Connection, OptionalExtension, TransactionBehavior};
use serde_json::Value;

use super::agent::Role;
use super::{deps, view};

pub const DEFAULT_LEASE_SECONDS: u32 = 3600;

/// Run `f` inside one `IMMEDIATE` transaction, committed if `f` succeeds.
fn write<T>(f: impl FnOnce(&Connection) -> Result<T>) -> Result<T> {
    let mut conn = crate::db::open_existing()?;
    let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
    let value = f(&tx)?;
    tx.commit()?;
    Ok(value)
}

struct Agent {
    id: i64,
    role: Role,
}

/// Look `name` up; with `required`, also insist on that role.
fn resolve_agent(conn: &Connection, name: &str, required: Option<Role>) -> Result<Agent> {
    let row: Option<(i64, String)> = conn
        .query_row(
            "SELECT id, role FROM agents WHERE name = ?1",
            [name],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .optional()?;
    let Some((id, role)) = row else {
        let hint = required.map_or("developer|reviewer", Role::name);
        bail!(
            "agent '{name}' is not registered; run `agent-kanban agent register {name} \
             --role {hint}` first"
        );
    };
    let role =
        Role::parse(&role).ok_or_else(|| anyhow!("agent '{name}' has unknown role '{role}'"))?;
    if let Some(required) = required
        && role != required
    {
        bail!(
            "agent '{name}' has role '{}'; {} role is required",
            role.name(),
            required.name()
        );
    }
    Ok(Agent { id, role })
}

fn check_lease(seconds: u32) -> Result<()> {
    if seconds == 0 {
        bail!("lease must be at least one second");
    }
    Ok(())
}

/// SQL condition for the task aliased `t` that `role` may claim, with the
/// claimer's agent id as `?1`: nobody holds it (or the lease ran out, or the
/// claimer holds it already) and, for developers, every prerequisite is done.
fn claimable(t: &str, role: Role) -> String {
    let ready = if role.needs_ready() {
        deps::ready_predicate(t)
    } else {
        "1".to_string()
    };
    format!(
        "({t}.executor IS NULL OR {t}.lease_expires_at IS NULL \
         OR {t}.lease_expires_at <= datetime('now') OR {t}.executor = ?1) AND {ready}"
    )
}

/// A task as the commands in this module judge who holds it.
struct Snapshot {
    status: String,
    /// The agent named in `executor`, even if its lease has run out.
    owner: Option<(i64, String)>,
    lease_expires_at: Option<String>,
    lease_live: bool,
    tests: String,
    revision: i64,
}

fn snapshot(conn: &Connection, id: i64) -> Result<Option<Snapshot>> {
    Ok(conn
        .query_row(
            "SELECT t.status, t.executor, a.name, t.lease_expires_at,
                    COALESCE(t.lease_expires_at > datetime('now'), 0), t.tests, t.revision
             FROM tasks t LEFT JOIN agents a ON a.id = t.executor
             WHERE t.id = ?1",
            [id],
            |row| {
                let executor: Option<i64> = row.get(1)?;
                let name: Option<String> = row.get(2)?;
                Ok(Snapshot {
                    status: row.get(0)?,
                    owner: executor.zip(name),
                    lease_expires_at: row.get(3)?,
                    lease_live: row.get(4)?,
                    tests: row.get(5)?,
                    revision: row.get(6)?,
                })
            },
        )
        .optional()?)
}

/// Why `agent` is not the holder of `task`, or `None` if it is.
fn not_held(id: i64, agent: &Agent, name: &str, task: &Snapshot) -> Option<String> {
    match &task.owner {
        Some((owner, _)) if *owner == agent.id => (!task.lease_live)
            .then(|| format!("agent '{name}' no longer holds task {id}; its lease expired")),
        Some((_, owner)) if task.lease_live => {
            Some(format!("task {id} is claimed by '{owner}', not '{name}'"))
        }
        _ => Some(format!("task {id} is not claimed by '{name}'")),
    }
}

/// `claim ID` -> the work order (see [`view::work_order`]).
pub fn claim(id: i64, agent: &str, lease_seconds: u32) -> Result<String> {
    write(|conn| claim_inner(conn, id, agent, lease_seconds))
}

fn claim_inner(conn: &Connection, id: i64, name: &str, lease_seconds: u32) -> Result<String> {
    check_lease(lease_seconds)?;
    let agent = resolve_agent(conn, name, None)?;
    let (from, also_from) = agent.role.claim_from();
    let changed = conn.execute(
        &format!(
            "UPDATE tasks AS t
             SET executor = ?1, status = ?2, claimed_at = datetime('now'),
                 lease_expires_at = datetime('now', printf('+%d seconds', ?3)),
                 updated_at = datetime('now')
             WHERE t.id = ?4 AND t.status IN (?5, ?6) AND {}",
            claimable("t", agent.role)
        ),
        rusqlite::params![
            agent.id,
            agent.role.holds(),
            i64::from(lease_seconds),
            id,
            from,
            also_from
        ],
    )?;
    if changed == 0 {
        bail!("{}", why_not_claimed(conn, id, &agent)?);
    }
    view::work_order(conn, id, agent.role)
}

fn why_not_claimed(conn: &Connection, id: i64, agent: &Agent) -> Result<String> {
    let Some(task) = snapshot(conn, id)? else {
        return Ok(format!("task {id} not found"));
    };
    let (from, also_from) = agent.role.claim_from();
    if task.status != from && task.status != also_from {
        let expected = if from == also_from {
            from.to_string()
        } else {
            format!("{from} or {also_from}")
        };
        return Ok(format!(
            "task {id} is {}; claim needs {expected}",
            task.status
        ));
    }
    if let (true, Some((_, owner)), Some(until)) =
        (task.lease_live, &task.owner, &task.lease_expires_at)
    {
        return Ok(format!(
            "task {id} is already claimed by '{owner}' until {until} UTC"
        ));
    }
    let open = deps::open_prerequisites(conn, id)?;
    if agent.role.needs_ready() && !open.is_empty() {
        return Ok(format!(
            "task {id} is blocked by unfinished tasks {}",
            deps::join(&open)
        ));
    }
    Ok(format!(
        "task {id} could not be claimed; it changed, try again"
    ))
}

/// `claim-next` -> the work order of the best task this agent can take, or
/// `idle`. The pick and the claim are one statement, so concurrent agents
/// never get the same task.
pub fn claim_next(agent: &str, lease_seconds: u32) -> Result<String> {
    write(|conn| claim_next_inner(conn, agent, lease_seconds))
}

fn claim_next_inner(conn: &Connection, name: &str, lease_seconds: u32) -> Result<String> {
    check_lease(lease_seconds)?;
    let agent = resolve_agent(conn, name, None)?;
    let (from, also_from) = agent.role.claim_from();
    // Best first: what the agent already holds (so a repeated call, or a
    // restarted agent, gets its task back), then unfinished work and rework
    // before fresh work, then by priority, then oldest.
    let id: Option<i64> = conn
        .query_row(
            &format!(
                "UPDATE tasks
                 SET executor = ?1, status = ?2, claimed_at = datetime('now'),
                     lease_expires_at = datetime('now', printf('+%d seconds', ?3)),
                     updated_at = datetime('now')
                 WHERE id = (
                   SELECT c.id FROM tasks c
                   WHERE c.status IN (?4, ?5) AND {}
                   ORDER BY CASE WHEN c.executor = ?1 AND c.lease_expires_at > datetime('now')
                                 THEN 0 ELSE 1 END,
                            CASE c.status WHEN 'in_progress' THEN 0 ELSE 1 END,
                            CASE c.priority WHEN 'urgent' THEN 0 WHEN 'high' THEN 1
                                            WHEN 'medium' THEN 2 ELSE 3 END,
                            c.id
                   LIMIT 1)
                 RETURNING id",
                claimable("c", agent.role)
            ),
            rusqlite::params![
                agent.id,
                agent.role.holds(),
                i64::from(lease_seconds),
                from,
                also_from
            ],
            |row| row.get(0),
        )
        .optional()?;
    id.map_or_else(|| idle(conn), |id| view::work_order(conn, id, agent.role))
}

/// Nothing to claim: `idle` when the board has no unfinished work at all,
/// `idle, N open` when unfinished tasks exist but are held, blocked or
/// waiting for someone else.
fn idle(conn: &Connection) -> Result<String> {
    let open: i64 = conn.query_row(
        "SELECT COUNT(*) FROM tasks WHERE status IN ('todo', 'in_progress', 'review')",
        [],
        |row| row.get(0),
    )?;
    Ok(if open == 0 {
        "idle".to_string()
    } else {
        format!("idle, {open} open")
    })
}

/// A developer's verdict on one test.
pub struct Verdict {
    pub index: usize,
    pub passed: bool,
    pub evidence: String,
}

/// Turn the `--pass IDX EVIDENCE` and `--fail IDX EVIDENCE` flags into
/// verdicts.
pub fn parse_verdicts(pass: &[Vec<String>], fail: &[Vec<String>]) -> Result<Vec<Verdict>> {
    let mut verdicts = Vec::with_capacity(pass.len() + fail.len());
    for (passed, flag, group) in [(true, "--pass", pass), (false, "--fail", fail)] {
        for pair in group {
            let [index, evidence] = pair.as_slice() else {
                bail!("{flag} needs IDX EVIDENCE");
            };
            let index = index
                .trim()
                .parse()
                .map_err(|_| anyhow!("{flag}: test index '{index}' must be a number"))?;
            verdicts.push(Verdict {
                index,
                passed,
                evidence: evidence.clone(),
            });
        }
    }
    if verdicts.is_empty() {
        bail!("give one --pass IDX EVIDENCE or --fail IDX EVIDENCE for every test");
    }
    Ok(verdicts)
}

/// `submit-review ID --pass/--fail ...` -> `#ID review revN`.
pub fn submit_review(id: i64, agent: &str, verdicts: &[Verdict]) -> Result<String> {
    write(|conn| submit_review_inner(conn, id, agent, verdicts))
}

fn submit_review_inner(
    conn: &Connection,
    id: i64,
    name: &str,
    verdicts: &[Verdict],
) -> Result<String> {
    let agent = resolve_agent(conn, name, Some(Role::Developer))?;
    let task = held_task(conn, id, &agent, name, "submit-review", "in_progress")?;
    let ordered = order_verdicts(id, verdicts, task.tests.len())?;
    let revision = task.revision + 1;
    for (index, verdict) in ordered.into_iter().enumerate() {
        conn.execute(
            "INSERT INTO acceptance_results (
               task_id, revision, criterion_index, criterion, result, evidence,
               executor, executor_name
             ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
            rusqlite::params![
                id,
                revision,
                i64::try_from(index)?,
                task.tests[index].to_string(),
                if verdict.passed { "passed" } else { "failed" },
                verdict.evidence.trim(),
                agent.id,
                name,
            ],
        )?;
    }
    conn.execute(
        "UPDATE tasks
         SET status = 'review', executor = NULL, claimed_at = NULL, lease_expires_at = NULL,
             revision = ?1, updated_at = datetime('now')
         WHERE id = ?2",
        rusqlite::params![revision, id],
    )?;
    Ok(format!("{} rev{revision}", view::ack(id, "review")))
}

/// One verdict per test, in test order.
fn order_verdicts(id: i64, verdicts: &[Verdict], tests: usize) -> Result<Vec<&Verdict>> {
    let mut slots: Vec<Option<&Verdict>> = vec![None; tests];
    for verdict in verdicts {
        let Some(slot) = slots.get_mut(verdict.index) else {
            bail!(
                "test {} does not exist; task {id} has tests 0-{}",
                verdict.index,
                tests.saturating_sub(1)
            );
        };
        if slot.is_some() {
            bail!("test {} has more than one result", verdict.index);
        }
        if verdict.evidence.trim().is_empty() {
            bail!("evidence for test {} must not be empty", verdict.index);
        }
        *slot = Some(verdict);
    }
    let missing: Vec<String> = slots
        .iter()
        .enumerate()
        .filter(|(_, slot)| slot.is_none())
        .map(|(index, _)| index.to_string())
        .collect();
    if !missing.is_empty() {
        bail!(
            "task {id} has {tests} tests; no result for {}",
            missing.join(",")
        );
    }
    Ok(slots.into_iter().flatten().collect())
}

struct Held {
    tests: Vec<Value>,
    revision: i64,
}

/// The task `id`, checked to be in `expected` status and held by `agent` with
/// a live lease.
fn held_task(
    conn: &Connection,
    id: i64,
    agent: &Agent,
    name: &str,
    command: &str,
    expected: &str,
) -> Result<Held> {
    let Some(task) = snapshot(conn, id)? else {
        bail!("task {id} not found");
    };
    if task.status != expected {
        bail!("task {id} is {}; {command} needs {expected}", task.status);
    }
    if let Some(problem) = not_held(id, agent, name, &task) {
        bail!("{problem}");
    }
    let tests = serde_json::from_str(&task.tests)
        .with_context(|| format!("task {id} has unreadable tests"))?;
    Ok(Held {
        tests,
        revision: task.revision,
    })
}

#[derive(Clone, Copy, Eq, PartialEq)]
enum Decision {
    Approve,
    RequestChanges,
}

impl Decision {
    const fn command(self) -> &'static str {
        match self {
            Self::Approve => "approve",
            Self::RequestChanges => "request-changes",
        }
    }

    /// The value stored in `review_history.decision`.
    const fn recorded(self) -> &'static str {
        match self {
            Self::Approve => "approved",
            Self::RequestChanges => "changes_requested",
        }
    }

    /// Where the task goes next.
    const fn next_status(self) -> &'static str {
        match self {
            Self::Approve => "done",
            Self::RequestChanges => "in_progress",
        }
    }
}

/// `approve ID [--notes T]` -> `#ID done[ unblocked:IDS]`, where `unblocked`
/// lists tasks that became claimable because this one is finished.
pub fn approve(id: i64, agent: &str, notes: &str) -> Result<String> {
    write(|conn| decide(conn, id, agent, Decision::Approve, notes))
}

/// `request-changes ID --notes T` -> `#ID in_progress`.
pub fn request_changes(id: i64, agent: &str, notes: &str) -> Result<String> {
    write(|conn| decide(conn, id, agent, Decision::RequestChanges, notes))
}

fn decide(
    conn: &Connection,
    id: i64,
    name: &str,
    decision: Decision,
    notes: &str,
) -> Result<String> {
    let notes = notes.trim();
    if decision == Decision::RequestChanges && notes.is_empty() {
        bail!("request-changes needs --notes saying what to fix");
    }
    let agent = resolve_agent(conn, name, Some(Role::Reviewer))?;
    let task = held_task(conn, id, &agent, name, decision.command(), "review")?;
    if decision == Decision::Approve {
        check_approvable(conn, id, &task)?;
    }
    conn.execute(
        "INSERT INTO review_history (task_id, revision, decision, notes, executor, executor_name)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
        rusqlite::params![
            id,
            task.revision,
            decision.recorded(),
            notes,
            agent.id,
            name
        ],
    )?;
    conn.execute(
        "UPDATE tasks
         SET status = ?1, executor = NULL, claimed_at = NULL, lease_expires_at = NULL,
             updated_at = datetime('now')
         WHERE id = ?2",
        rusqlite::params![decision.next_status(), id],
    )?;
    let unblocked = if decision == Decision::Approve {
        deps::unblocked_by(conn, id)?
    } else {
        Vec::new()
    };
    Ok(format!(
        "{}{}",
        view::ack(id, decision.next_status()),
        deps::labeled("unblocked", &unblocked)
    ))
}

/// Approval needs a complete set of results for the revision under review,
/// none of them failed.
fn check_approvable(conn: &Connection, id: i64, task: &Held) -> Result<()> {
    let (recorded, failed): (i64, i64) = conn.query_row(
        "SELECT COUNT(*), COUNT(*) FILTER (WHERE result = 'failed')
         FROM acceptance_results WHERE task_id = ?1 AND revision = ?2",
        rusqlite::params![id, task.revision],
        |row| Ok((row.get(0)?, row.get(1)?)),
    )?;
    if usize::try_from(recorded)? != task.tests.len() {
        bail!("task {id} has no complete results for rev{}", task.revision);
    }
    if failed != 0 {
        bail!(
            "task {id} has {failed} failed test(s) in rev{}; request changes instead",
            task.revision
        );
    }
    Ok(())
}

/// `move ID backlog|todo` -> `#ID STATUS`. Idempotent.
pub fn move_status(id: i64, status: &str) -> Result<String> {
    write(|conn| move_inner(conn, id, status))
}

fn move_inner(conn: &Connection, id: i64, status: &str) -> Result<String> {
    if !matches!(status, "backlog" | "todo") {
        bail!("cannot move to '{status}': move only works between backlog and todo");
    }
    let changed = conn.execute(
        "UPDATE tasks
         SET status = ?1, executor = NULL, claimed_at = NULL, lease_expires_at = NULL,
             updated_at = datetime('now')
         WHERE id = ?2 AND status IN ('backlog', 'todo')
           AND (executor IS NULL OR lease_expires_at IS NULL OR lease_expires_at <= datetime('now'))",
        rusqlite::params![status, id],
    )?;
    if changed == 0 {
        bail!("{}", why_not_moved(conn, id)?);
    }
    Ok(view::ack(id, status))
}

fn why_not_moved(conn: &Connection, id: i64) -> Result<String> {
    let Some(task) = snapshot(conn, id)? else {
        return Ok(format!("task {id} not found"));
    };
    if !matches!(task.status.as_str(), "backlog" | "todo") {
        return Ok(format!(
            "task {id} is {}; move only works on backlog and todo tasks",
            task.status
        ));
    }
    Ok(match (&task.owner, task.lease_live) {
        (Some((_, owner)), true) => format!("task {id} is claimed by '{owner}'"),
        _ => format!("task {id} could not be moved; it changed, try again"),
    })
}

/// `release ID` -> `#ID STATUS`: the agent gives the task back, status
/// unchanged.
pub fn release(id: i64, agent: &str) -> Result<String> {
    write(|conn| release_inner(conn, id, agent))
}

fn release_inner(conn: &Connection, id: i64, name: &str) -> Result<String> {
    let agent = resolve_agent(conn, name, None)?;
    let status: Option<String> = conn
        .query_row(
            "UPDATE tasks
             SET executor = NULL, claimed_at = NULL, lease_expires_at = NULL,
                 updated_at = datetime('now')
             WHERE id = ?1 AND executor = ?2 AND lease_expires_at > datetime('now')
               AND status = ?3
             RETURNING status",
            rusqlite::params![id, agent.id, agent.role.holds()],
            |row| row.get(0),
        )
        .optional()?;
    match status {
        Some(status) => Ok(view::ack(id, &status)),
        None => bail!("{}", why_not_released(conn, id, &agent, name)?),
    }
}

fn why_not_released(conn: &Connection, id: i64, agent: &Agent, name: &str) -> Result<String> {
    let Some(task) = snapshot(conn, id)? else {
        return Ok(format!("task {id} not found"));
    };
    Ok(not_held(id, agent, name, &task).unwrap_or_else(|| {
        format!(
            "task {id} is {}; release needs {}",
            task.status,
            agent.role.holds()
        )
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn setup() -> Connection {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch("PRAGMA foreign_keys=ON;").unwrap();
        conn.execute_batch(crate::db::SCHEMA).unwrap();
        conn
    }

    fn agent(conn: &Connection, name: &str, role: &str) {
        conn.execute(
            "INSERT INTO agents (name, role) VALUES (?1, ?2)",
            [name, role],
        )
        .unwrap();
    }

    fn dev(conn: &Connection, name: &str) {
        agent(conn, name, "developer");
    }

    fn reviewer(conn: &Connection, name: &str) {
        agent(conn, name, "reviewer");
    }

    /// A task with `tests` acceptance tests.
    fn task_with(conn: &Connection, priority: &str, status: &str, tests: usize) -> i64 {
        let specs: Vec<Value> = (0..tests)
            .map(|n| json!({"describe": format!("d{n}"), "input": format!("i{n}"), "output": format!("o{n}")}))
            .collect();
        conn.execute(
            "INSERT INTO tasks (title, priority, status, tests) VALUES ('t', ?1, ?2, ?3)",
            rusqlite::params![priority, status, Value::Array(specs).to_string()],
        )
        .unwrap();
        conn.last_insert_rowid()
    }

    fn task(conn: &Connection) -> i64 {
        task_with(conn, "medium", "todo", 1)
    }

    fn verdict(index: usize, passed: bool, evidence: &str) -> Verdict {
        Verdict {
            index,
            passed,
            evidence: evidence.to_string(),
        }
    }

    fn pass(index: usize) -> Verdict {
        verdict(index, true, "cargo test")
    }

    fn all_pass(tests: usize) -> Vec<Verdict> {
        (0..tests).map(pass).collect()
    }

    /// A claim reply read back as the JSON shape earlier versions printed.
    fn parse(reply: &str) -> Value {
        view::testing::parse_order(reply)
    }

    /// The stored `criterion` column, which is JSON.
    fn criterion(stored: &str) -> Value {
        serde_json::from_str(stored).unwrap()
    }

    fn err<T>(result: Result<T>) -> String {
        match result {
            Ok(_) => panic!("expected an error"),
            Err(e) => e.to_string(),
        }
    }

    fn status_of(conn: &Connection, id: i64) -> String {
        conn.query_row("SELECT status FROM tasks WHERE id = ?1", [id], |r| r.get(0))
            .unwrap()
    }

    fn holder_of(conn: &Connection, id: i64) -> Option<String> {
        conn.query_row(
            "SELECT a.name FROM tasks t LEFT JOIN agents a ON a.id = t.executor WHERE t.id = ?1",
            [id],
            |r| r.get(0),
        )
        .unwrap()
    }

    fn expire_lease(conn: &Connection, id: i64) {
        conn.execute(
            "UPDATE tasks SET lease_expires_at = datetime('now', '-1 second') WHERE id = ?1",
            [id],
        )
        .unwrap();
    }

    fn set_status(conn: &Connection, id: i64, status: &str) {
        conn.execute("UPDATE tasks SET status = ?1 WHERE id = ?2", (status, id))
            .unwrap();
    }

    /// A developer claims `id` and submits `tests` passing results.
    fn submit(conn: &Connection, id: i64, developer: &str, tests: usize) {
        claim_inner(conn, id, developer, 60).unwrap();
        submit_review_inner(conn, id, developer, &all_pass(tests)).unwrap();
    }

    // ---- claim ----

    #[test]
    fn developer_claim_returns_the_work_order_and_holds_the_task() {
        let conn = setup();
        dev(&conn, "alice");
        let id = task_with(&conn, "high", "todo", 2);

        let order = parse(&claim_inner(&conn, id, "alice", 60).unwrap());
        assert_eq!(order["id"], id);
        assert_eq!(order["tests"][1]["describe"], "d1");
        assert!(order.get("rev").is_none());
        assert_eq!(status_of(&conn, id), "in_progress");
        assert_eq!(holder_of(&conn, id).as_deref(), Some("alice"));
    }

    #[test]
    fn claiming_again_renews_the_lease_and_returns_the_same_order() {
        let conn = setup();
        dev(&conn, "alice");
        let id = task(&conn);
        let first = claim_inner(&conn, id, "alice", 60).unwrap();
        conn.execute(
            "UPDATE tasks SET lease_expires_at = datetime('now', '+5 seconds') WHERE id = ?1",
            [id],
        )
        .unwrap();
        assert_eq!(claim_inner(&conn, id, "alice", 3600).unwrap(), first);
        let remaining: i64 = conn
            .query_row(
                "SELECT strftime('%s', lease_expires_at) - strftime('%s', 'now') FROM tasks WHERE id = ?1",
                [id],
                |r| r.get(0),
            )
            .unwrap();
        assert!(remaining > 3000, "lease was not renewed: {remaining}s left");
    }

    #[test]
    fn claim_is_refused_while_someone_else_holds_a_live_lease() {
        let conn = setup();
        dev(&conn, "alice");
        dev(&conn, "carol");
        let id = task(&conn);
        claim_inner(&conn, id, "alice", 60).unwrap();

        let message = err(claim_inner(&conn, id, "carol", 60));
        assert!(
            message.starts_with(&format!("task {id} is already claimed by 'alice' until ")),
            "{message}"
        );
        assert!(message.ends_with(" UTC"), "{message}");
        assert_eq!(holder_of(&conn, id).as_deref(), Some("alice"));

        expire_lease(&conn, id);
        claim_inner(&conn, id, "carol", 60).unwrap();
        assert_eq!(holder_of(&conn, id).as_deref(), Some("carol"));
    }

    #[test]
    fn claim_errors_name_the_status_the_command_needs() {
        let conn = setup();
        dev(&conn, "alice");
        reviewer(&conn, "rita");
        assert_eq!(
            err(claim_inner(&conn, 99, "alice", 60)),
            "task 99 not found"
        );

        for status in ["backlog", "review", "done"] {
            let id = task_with(&conn, "low", status, 1);
            assert_eq!(
                err(claim_inner(&conn, id, "alice", 60)),
                format!("task {id} is {status}; claim needs todo or in_progress")
            );
        }
        let todo = task(&conn);
        assert_eq!(
            err(claim_inner(&conn, todo, "rita", 60)),
            format!("task {todo} is todo; claim needs review")
        );
    }

    #[test]
    fn developers_cannot_claim_blocked_tasks_but_reviewers_ignore_prerequisites() {
        let conn = setup();
        dev(&conn, "alice");
        reviewer(&conn, "rita");
        let first = task(&conn);
        let second = task(&conn);
        deps::add_prerequisites(&conn, second, &[first]).unwrap();

        assert_eq!(
            err(claim_inner(&conn, second, "alice", 60)),
            format!("task {second} is blocked by unfinished tasks {first}")
        );
        assert_eq!(status_of(&conn, second), "todo");

        // Work submitted before a prerequisite was added is still reviewable.
        set_status(&conn, second, "review");
        conn.execute("UPDATE tasks SET revision = 1 WHERE id = ?1", [second])
            .unwrap();
        claim_inner(&conn, second, "rita", 60).unwrap();
    }

    #[test]
    fn a_task_is_claimable_once_its_prerequisites_are_done() {
        let conn = setup();
        dev(&conn, "alice");
        let first = task(&conn);
        let second = task(&conn);
        deps::add_prerequisites(&conn, second, &[first]).unwrap();
        set_status(&conn, first, "done");
        claim_inner(&conn, second, "alice", 60).unwrap();
    }

    #[test]
    fn unregistered_agents_are_told_how_to_register() {
        let conn = setup();
        let id = task(&conn);
        assert_eq!(
            err(claim_inner(&conn, id, "zed", 60)),
            "agent 'zed' is not registered; run `agent-kanban agent register zed \
             --role developer|reviewer` first"
        );
        assert_eq!(
            err(submit_review_inner(&conn, id, "zed", &all_pass(1))),
            "agent 'zed' is not registered; run `agent-kanban agent register zed \
             --role developer` first"
        );
        assert_eq!(status_of(&conn, id), "todo");
        assert_eq!(holder_of(&conn, id), None);
    }

    #[test]
    fn a_zero_second_lease_is_rejected_before_anything_else() {
        let conn = setup();
        dev(&conn, "alice");
        let id = task(&conn);
        assert_eq!(
            err(claim_inner(&conn, id, "alice", 0)),
            "lease must be at least one second"
        );
        assert_eq!(
            err(claim_next_inner(&conn, "alice", 0)),
            "lease must be at least one second"
        );
        assert_eq!(status_of(&conn, id), "todo");
    }

    #[test]
    fn an_unknown_role_in_the_database_is_reported() {
        let conn = setup();
        conn.execute_batch("PRAGMA ignore_check_constraints = ON;")
            .unwrap();
        agent(&conn, "odd", "wizard");
        assert_eq!(
            err(resolve_agent(&conn, "odd", None)),
            "agent 'odd' has unknown role 'wizard'"
        );
    }

    #[test]
    fn expected_failures_fall_back_to_a_retry_hint_when_the_state_explains_nothing() {
        // Under the write lock the claim statement and the explanation see the
        // same state, so the fallbacks are only reachable by asking for the
        // explanation of a task that was in fact claimable or movable.
        let conn = setup();
        dev(&conn, "alice");
        let id = task(&conn);
        let alice = resolve_agent(&conn, "alice", None).unwrap();
        assert_eq!(
            why_not_claimed(&conn, id, &alice).unwrap(),
            format!("task {id} could not be claimed; it changed, try again")
        );
        assert_eq!(
            why_not_moved(&conn, id).unwrap(),
            format!("task {id} could not be moved; it changed, try again")
        );
    }

    // ---- reviewer claims ----

    #[test]
    fn reviewer_claim_returns_the_developers_evidence_next_to_each_test() {
        let conn = setup();
        dev(&conn, "alice");
        reviewer(&conn, "rita");
        let id = task_with(&conn, "low", "todo", 2);
        claim_inner(&conn, id, "alice", 60).unwrap();
        submit_review_inner(
            &conn,
            id,
            "alice",
            &[
                verdict(1, false, "broke on empty input"),
                verdict(0, true, "ok"),
            ],
        )
        .unwrap();

        let packet = parse(&claim_inner(&conn, id, "rita", 60).unwrap());
        assert_eq!(packet["rev"], 1);
        assert_eq!(packet["tests"][0]["result"], "passed");
        assert_eq!(packet["tests"][1]["result"], "failed");
        assert_eq!(packet["tests"][1]["evidence"], "broke on empty input");
        assert_eq!(status_of(&conn, id), "review");
        assert_eq!(holder_of(&conn, id).as_deref(), Some("rita"));
    }

    // ---- claim-next ----

    fn claimed_id(reply: &str) -> i64 {
        parse(reply)["id"].as_i64().unwrap()
    }

    #[test]
    fn claim_next_prefers_unfinished_work_then_priority_then_age() {
        let conn = setup();
        dev(&conn, "alice");
        let low = task_with(&conn, "low", "todo", 1);
        let urgent = task_with(&conn, "urgent", "todo", 1);
        let high_first = task_with(&conn, "high", "todo", 1);
        let high_second = task_with(&conn, "high", "todo", 1);
        let rework = task_with(&conn, "low", "in_progress", 1);

        let mut order = Vec::new();
        for _ in 0..5 {
            let reply = claim_next_inner(&conn, "alice", 60).unwrap();
            let id = claimed_id(&reply);
            order.push(id);
            // Hand the task over so the next call picks a different one.
            release_inner(&conn, id, "alice").unwrap();
            set_status(&conn, id, "done");
        }
        assert_eq!(order, vec![rework, urgent, high_first, high_second, low]);
    }

    #[test]
    fn claim_next_returns_the_task_the_agent_already_holds() {
        let conn = setup();
        dev(&conn, "alice");
        task_with(&conn, "low", "todo", 1);
        let urgent = task_with(&conn, "urgent", "todo", 1);
        // Alice holds a low-priority task already; an urgent one has arrived.
        let held = task_with(&conn, "low", "todo", 1);
        claim_inner(&conn, held, "alice", 60).unwrap();

        let first = claim_next_inner(&conn, "alice", 60).unwrap();
        assert_eq!(claimed_id(&first), held);
        assert_eq!(claim_next_inner(&conn, "alice", 60).unwrap(), first);
        assert_eq!(status_of(&conn, urgent), "todo");
    }

    #[test]
    fn claim_next_skips_blocked_tasks_and_tasks_with_live_leases() {
        let conn = setup();
        dev(&conn, "alice");
        dev(&conn, "carol");
        let prerequisite = task_with(&conn, "low", "todo", 1);
        let blocked = task_with(&conn, "urgent", "todo", 1);
        deps::add_prerequisites(&conn, blocked, &[prerequisite]).unwrap();
        let held = task_with(&conn, "urgent", "todo", 1);
        claim_inner(&conn, held, "carol", 60).unwrap();

        // Both urgent tasks are out of reach, so the low one is next.
        let picked = claimed_id(&claim_next_inner(&conn, "alice", 60).unwrap());
        assert_eq!(picked, prerequisite);
        assert_eq!(status_of(&conn, blocked), "todo");
        assert_eq!(holder_of(&conn, blocked), None);
        assert_eq!(holder_of(&conn, held).as_deref(), Some("carol"));
    }

    #[test]
    fn claim_next_offers_a_task_once_its_prerequisites_are_done() {
        let conn = setup();
        dev(&conn, "alice");
        reviewer(&conn, "rita");
        let first = task(&conn);
        let second = task(&conn);
        deps::add_prerequisites(&conn, second, &[first]).unwrap();

        assert_eq!(
            claimed_id(&claim_next_inner(&conn, "alice", 60).unwrap()),
            first
        );
        submit_review_inner(&conn, first, "alice", &all_pass(1)).unwrap();
        claim_inner(&conn, first, "rita", 60).unwrap();
        // Submitted is not finished: the second task is still waiting.
        assert_eq!(
            claim_next_inner(&conn, "alice", 60).unwrap(),
            "idle, 2 open"
        );
        approve_notes(&conn, first, "rita", "");
        assert_eq!(
            claimed_id(&claim_next_inner(&conn, "alice", 60).unwrap()),
            second
        );
    }

    #[test]
    fn claim_next_takes_over_a_task_whose_lease_expired() {
        let conn = setup();
        dev(&conn, "alice");
        dev(&conn, "carol");
        let id = task(&conn);
        claim_inner(&conn, id, "carol", 60).unwrap();
        assert_eq!(
            claim_next_inner(&conn, "alice", 60).unwrap(),
            "idle, 1 open"
        );

        expire_lease(&conn, id);
        assert_eq!(
            claimed_id(&claim_next_inner(&conn, "alice", 60).unwrap()),
            id
        );
        assert_eq!(holder_of(&conn, id).as_deref(), Some("alice"));
    }

    #[test]
    fn claim_next_for_a_reviewer_only_considers_tasks_in_review() {
        let conn = setup();
        dev(&conn, "alice");
        reviewer(&conn, "rita");
        let todo = task_with(&conn, "urgent", "todo", 1);
        let working = task_with(&conn, "urgent", "in_progress", 1);
        let low = task_with(&conn, "low", "todo", 1);
        let high = task_with(&conn, "high", "todo", 1);
        submit(&conn, low, "alice", 1);
        submit(&conn, high, "alice", 1);

        assert_eq!(
            claimed_id(&claim_next_inner(&conn, "rita", 60).unwrap()),
            high
        );
        // Holding one, the reviewer is handed it again rather than a new one.
        assert_eq!(
            claimed_id(&claim_next_inner(&conn, "rita", 60).unwrap()),
            high
        );
        assert_eq!(status_of(&conn, todo), "todo");
        assert_eq!(status_of(&conn, working), "in_progress");
        assert_eq!(holder_of(&conn, low), None);
    }

    #[test]
    fn claim_next_says_idle_and_how_much_unfinished_work_remains() {
        let conn = setup();
        dev(&conn, "alice");
        assert_eq!(claim_next_inner(&conn, "alice", 60).unwrap(), "idle");

        let done = task(&conn);
        set_status(&conn, done, "done");
        let parked = task(&conn);
        set_status(&conn, parked, "backlog");
        assert_eq!(claim_next_inner(&conn, "alice", 60).unwrap(), "idle");

        let first = task(&conn);
        let second = task(&conn);
        deps::add_prerequisites(&conn, second, &[first]).unwrap();
        claim_inner(&conn, first, "alice", 60).unwrap();
        dev(&conn, "carol");
        // carol cannot take `first` (held) or `second` (blocked).
        assert_eq!(
            claim_next_inner(&conn, "carol", 60).unwrap(),
            "idle, 2 open"
        );
    }

    // ---- submit-review ----

    #[test]
    fn submit_review_stores_one_result_per_test_and_hands_the_task_over() {
        let conn = setup();
        dev(&conn, "alice");
        let id = task_with(&conn, "low", "todo", 2);
        claim_inner(&conn, id, "alice", 60).unwrap();

        let reply = submit_review_inner(
            &conn,
            id,
            "alice",
            &[verdict(1, false, " second failed "), pass(0)],
        )
        .unwrap();
        assert_eq!(reply, format!("#{id} review rev1"));
        assert_eq!(status_of(&conn, id), "review");
        assert_eq!(holder_of(&conn, id), None);

        let rows: Vec<(i64, i64, String, String, String, String)> = conn
            .prepare(
                "SELECT revision, criterion_index, criterion, result, evidence, executor_name
                 FROM acceptance_results WHERE task_id = ?1 ORDER BY criterion_index",
            )
            .unwrap()
            .query_map([id], |r| {
                Ok((
                    r.get(0)?,
                    r.get(1)?,
                    r.get(2)?,
                    r.get(3)?,
                    r.get(4)?,
                    r.get(5)?,
                ))
            })
            .unwrap()
            .collect::<std::result::Result<_, _>>()
            .unwrap();
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].0, 1);
        assert_eq!(criterion(&rows[0].2)["describe"], "d0");
        assert_eq!(rows[0].3, "passed");
        assert_eq!(rows[1].1, 1);
        assert_eq!(criterion(&rows[1].2)["describe"], "d1");
        assert_eq!(rows[1].3, "failed");
        assert_eq!(rows[1].4, "second failed", "evidence is trimmed");
        assert_eq!(rows[1].5, "alice");
    }

    #[test]
    fn submit_review_rejects_incomplete_or_malformed_results_and_stores_nothing() {
        let conn = setup();
        dev(&conn, "alice");
        let id = task_with(&conn, "low", "todo", 3);
        claim_inner(&conn, id, "alice", 60).unwrap();

        let cases: Vec<(Vec<Verdict>, String)> = vec![
            (
                vec![pass(0)],
                format!("task {id} has 3 tests; no result for 1,2"),
            ),
            (
                vec![pass(0), pass(1), pass(2), pass(3)],
                format!("test 3 does not exist; task {id} has tests 0-2"),
            ),
            (
                vec![pass(0), pass(0), pass(1), pass(2)],
                "test 0 has more than one result".to_string(),
            ),
            (
                vec![pass(0), pass(1), verdict(2, true, "  ")],
                "evidence for test 2 must not be empty".to_string(),
            ),
            (
                Vec::new(),
                format!("task {id} has 3 tests; no result for 0,1,2"),
            ),
        ];
        for (verdicts, expected) in cases {
            assert_eq!(
                err(submit_review_inner(&conn, id, "alice", &verdicts)),
                expected
            );
        }

        let stored: i64 = conn
            .query_row("SELECT COUNT(*) FROM acceptance_results", [], |r| r.get(0))
            .unwrap();
        assert_eq!(stored, 0);
        assert_eq!(status_of(&conn, id), "in_progress");
        assert_eq!(holder_of(&conn, id).as_deref(), Some("alice"));
    }

    #[test]
    fn only_the_developer_holding_the_task_may_submit_it() {
        let conn = setup();
        dev(&conn, "alice");
        dev(&conn, "carol");
        reviewer(&conn, "rita");
        let id = task(&conn);

        assert_eq!(
            err(submit_review_inner(&conn, 99, "alice", &all_pass(1))),
            "task 99 not found"
        );
        assert_eq!(
            err(submit_review_inner(&conn, id, "alice", &all_pass(1))),
            format!("task {id} is todo; submit-review needs in_progress")
        );

        claim_inner(&conn, id, "alice", 60).unwrap();
        assert_eq!(
            err(submit_review_inner(&conn, id, "rita", &all_pass(1))),
            "agent 'rita' has role 'reviewer'; developer role is required"
        );
        assert_eq!(
            err(submit_review_inner(&conn, id, "carol", &all_pass(1))),
            format!("task {id} is claimed by 'alice', not 'carol'")
        );

        expire_lease(&conn, id);
        assert_eq!(
            err(submit_review_inner(&conn, id, "alice", &all_pass(1))),
            format!("agent 'alice' no longer holds task {id}; its lease expired")
        );
        assert_eq!(
            err(submit_review_inner(&conn, id, "carol", &all_pass(1))),
            format!("task {id} is not claimed by 'carol'")
        );
        assert_eq!(status_of(&conn, id), "in_progress");
    }

    #[test]
    fn verdict_flags_are_parsed_into_indexed_verdicts() {
        let pass = vec![vec!["1".to_string(), "ok".to_string()]];
        let fail = vec![
            vec!["0".to_string(), "bad".to_string()],
            vec![" 2 ".to_string(), "worse".to_string()],
        ];
        let verdicts = parse_verdicts(&pass, &fail).unwrap();
        let summary: Vec<(usize, bool, &str)> = verdicts
            .iter()
            .map(|v| (v.index, v.passed, v.evidence.as_str()))
            .collect();
        assert_eq!(
            summary,
            vec![(1, true, "ok"), (0, false, "bad"), (2, false, "worse")]
        );
    }

    #[test]
    fn verdict_flags_reject_non_numeric_indexes_and_missing_evidence() {
        let bad_index = vec![vec!["first".to_string(), "ok".to_string()]];
        assert_eq!(
            err(parse_verdicts(&bad_index, &[])),
            "--pass: test index 'first' must be a number"
        );
        assert_eq!(
            err(parse_verdicts(&[], &bad_index)),
            "--fail: test index 'first' must be a number"
        );
        assert_eq!(
            err(parse_verdicts(&[vec!["0".to_string()]], &[])),
            "--pass needs IDX EVIDENCE"
        );
        assert_eq!(
            err(parse_verdicts(&[], &[])),
            "give one --pass IDX EVIDENCE or --fail IDX EVIDENCE for every test"
        );
    }

    // ---- approve / request-changes ----

    #[test]
    fn approve_finishes_the_task_and_reports_what_it_unblocked() {
        let conn = setup();
        dev(&conn, "alice");
        reviewer(&conn, "rita");
        let first = task(&conn);
        let second = task(&conn);
        let third = task(&conn);
        let parked = task(&conn);
        deps::add_prerequisites(&conn, second, &[first]).unwrap();
        deps::add_prerequisites(&conn, third, &[first, second]).unwrap();
        deps::add_prerequisites(&conn, parked, &[first]).unwrap();
        set_status(&conn, parked, "backlog");
        submit(&conn, first, "alice", 1);
        claim_inner(&conn, first, "rita", 60).unwrap();

        let reply = approve_notes(&conn, first, "rita", "looks right");
        assert_eq!(reply, format!("#{first} done unblocked:{second}"));
        assert_eq!(status_of(&conn, first), "done");
        assert_eq!(holder_of(&conn, first), None);
        let (decision, notes, reviewer_name): (String, String, String) = conn
            .query_row(
                "SELECT decision, notes, executor_name FROM review_history WHERE task_id = ?1",
                [first],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .unwrap();
        assert_eq!(
            (decision.as_str(), notes.as_str(), reviewer_name.as_str()),
            ("approved", "looks right", "rita")
        );
    }

    fn approve_notes(conn: &Connection, id: i64, reviewer: &str, notes: &str) -> String {
        decide(conn, id, reviewer, Decision::Approve, notes).unwrap()
    }

    #[test]
    fn approve_without_dependents_has_no_unblocked_part() {
        let conn = setup();
        dev(&conn, "alice");
        reviewer(&conn, "rita");
        let id = task(&conn);
        submit(&conn, id, "alice", 1);
        claim_inner(&conn, id, "rita", 60).unwrap();
        assert_eq!(approve_notes(&conn, id, "rita", ""), format!("#{id} done"));
    }

    #[test]
    fn approve_refuses_failed_or_missing_results() {
        let conn = setup();
        dev(&conn, "alice");
        reviewer(&conn, "rita");
        let id = task_with(&conn, "low", "todo", 2);
        claim_inner(&conn, id, "alice", 60).unwrap();
        submit_review_inner(&conn, id, "alice", &[pass(0), verdict(1, false, "broken")]).unwrap();
        claim_inner(&conn, id, "rita", 60).unwrap();

        assert_eq!(
            err(decide(&conn, id, "rita", Decision::Approve, "")),
            format!("task {id} has 1 failed test(s) in rev1; request changes instead")
        );
        assert_eq!(status_of(&conn, id), "review");

        conn.execute(
            "DELETE FROM acceptance_results WHERE criterion_index = 1",
            [],
        )
        .unwrap();
        assert_eq!(
            err(decide(&conn, id, "rita", Decision::Approve, "")),
            format!("task {id} has no complete results for rev1")
        );
        let decisions: i64 = conn
            .query_row("SELECT COUNT(*) FROM review_history", [], |r| r.get(0))
            .unwrap();
        assert_eq!(decisions, 0, "a refused decision leaves no history");
    }

    #[test]
    fn request_changes_sends_the_task_back_and_the_rework_cycle_completes() {
        let conn = setup();
        dev(&conn, "alice");
        reviewer(&conn, "rita");
        let id = task_with(&conn, "low", "todo", 2);
        claim_inner(&conn, id, "alice", 60).unwrap();
        submit_review_inner(
            &conn,
            id,
            "alice",
            &[pass(0), verdict(1, false, "off by one")],
        )
        .unwrap();
        claim_inner(&conn, id, "rita", 60).unwrap();

        let reply = decide(&conn, id, "rita", Decision::RequestChanges, " fix test 1 ").unwrap();
        assert_eq!(reply, format!("#{id} in_progress"));
        assert_eq!(holder_of(&conn, id), None);

        // The developer sees the notes and the revision to improve on.
        let order = parse(&claim_inner(&conn, id, "alice", 60).unwrap());
        assert_eq!(order["rev"], 1);
        assert_eq!(order["changes"], "fix test 1");
        submit_review_inner(&conn, id, "alice", &all_pass(2)).unwrap();

        // The reviewer sees the new verdicts and the last notes, then approves.
        let packet = parse(&claim_inner(&conn, id, "rita", 60).unwrap());
        assert_eq!(packet["rev"], 2);
        assert_eq!(packet["changes"], "fix test 1");
        assert_eq!(packet["tests"][1]["result"], "passed");
        assert_eq!(approve_notes(&conn, id, "rita", ""), format!("#{id} done"));
    }

    #[test]
    fn request_changes_needs_notes() {
        let conn = setup();
        reviewer(&conn, "rita");
        let id = task(&conn);
        assert_eq!(
            err(decide(&conn, id, "rita", Decision::RequestChanges, "  ")),
            "request-changes needs --notes saying what to fix"
        );
    }

    #[test]
    fn only_the_reviewer_holding_the_task_may_decide() {
        let conn = setup();
        dev(&conn, "alice");
        reviewer(&conn, "rita");
        reviewer(&conn, "ron");
        let id = task(&conn);
        submit(&conn, id, "alice", 1);

        assert_eq!(
            err(decide(&conn, id, "rita", Decision::Approve, "")),
            format!("task {id} is not claimed by 'rita'")
        );
        claim_inner(&conn, id, "rita", 60).unwrap();
        assert_eq!(
            err(decide(&conn, id, "ron", Decision::Approve, "")),
            format!("task {id} is claimed by 'rita', not 'ron'")
        );
        assert_eq!(
            err(decide(&conn, id, "alice", Decision::Approve, "")),
            "agent 'alice' has role 'developer'; reviewer role is required"
        );
        let todo = task(&conn);
        assert_eq!(
            err(decide(&conn, todo, "rita", Decision::Approve, "")),
            format!("task {todo} is todo; approve needs review")
        );
        assert_eq!(
            err(decide(&conn, todo, "rita", Decision::RequestChanges, "x")),
            format!("task {todo} is todo; request-changes needs review")
        );
    }

    // ---- release ----

    #[test]
    fn release_gives_the_task_back_without_changing_its_status() {
        let conn = setup();
        dev(&conn, "alice");
        reviewer(&conn, "rita");
        let id = task(&conn);
        claim_inner(&conn, id, "alice", 60).unwrap();
        assert_eq!(
            release_inner(&conn, id, "alice").unwrap(),
            format!("#{id} in_progress")
        );
        assert_eq!(holder_of(&conn, id), None);

        submit(&conn, id, "alice", 1);
        claim_inner(&conn, id, "rita", 60).unwrap();
        assert_eq!(
            release_inner(&conn, id, "rita").unwrap(),
            format!("#{id} review")
        );
        assert_eq!(status_of(&conn, id), "review");
    }

    #[test]
    fn release_explains_why_it_does_not_apply() {
        let conn = setup();
        dev(&conn, "alice");
        dev(&conn, "carol");
        let id = task(&conn);
        assert_eq!(err(release_inner(&conn, 99, "alice")), "task 99 not found");
        assert_eq!(
            err(release_inner(&conn, id, "alice")),
            format!("task {id} is not claimed by 'alice'")
        );
        claim_inner(&conn, id, "alice", 60).unwrap();
        assert_eq!(
            err(release_inner(&conn, id, "carol")),
            format!("task {id} is claimed by 'alice', not 'carol'")
        );
        expire_lease(&conn, id);
        assert_eq!(
            err(release_inner(&conn, id, "alice")),
            format!("agent 'alice' no longer holds task {id}; its lease expired")
        );
    }

    #[test]
    fn release_reports_a_status_the_role_does_not_hold() {
        // Only reachable with a hand-edited board: a developer holding a task
        // that is not in progress.
        let conn = setup();
        dev(&conn, "alice");
        let id = task(&conn);
        claim_inner(&conn, id, "alice", 60).unwrap();
        set_status(&conn, id, "review");
        assert_eq!(
            err(release_inner(&conn, id, "alice")),
            format!("task {id} is review; release needs in_progress")
        );
    }

    // ---- move ----

    #[test]
    fn move_goes_between_backlog_and_todo_and_is_idempotent() {
        let conn = setup();
        let id = task(&conn);
        assert_eq!(
            move_inner(&conn, id, "backlog").unwrap(),
            format!("#{id} backlog")
        );
        assert_eq!(status_of(&conn, id), "backlog");
        assert_eq!(
            move_inner(&conn, id, "backlog").unwrap(),
            format!("#{id} backlog")
        );
        assert_eq!(
            move_inner(&conn, id, "todo").unwrap(),
            format!("#{id} todo")
        );
        assert_eq!(status_of(&conn, id), "todo");
    }

    #[test]
    fn move_refuses_other_targets_and_other_sources() {
        let conn = setup();
        dev(&conn, "alice");
        let id = task(&conn);
        assert_eq!(
            err(move_inner(&conn, id, "done")),
            "cannot move to 'done': move only works between backlog and todo"
        );
        assert_eq!(err(move_inner(&conn, 99, "todo")), "task 99 not found");

        claim_inner(&conn, id, "alice", 60).unwrap();
        assert_eq!(
            err(move_inner(&conn, id, "backlog")),
            format!("task {id} is in_progress; move only works on backlog and todo tasks")
        );
    }

    #[test]
    fn move_refuses_a_task_with_a_live_claim() {
        // A claimed task is always in_progress, so this needs a hand-edited
        // board: a todo task that nevertheless has a live lease.
        let conn = setup();
        dev(&conn, "alice");
        let id = task(&conn);
        claim_inner(&conn, id, "alice", 60).unwrap();
        set_status(&conn, id, "todo");
        assert_eq!(
            err(move_inner(&conn, id, "backlog")),
            format!("task {id} is claimed by 'alice'")
        );
    }
}
