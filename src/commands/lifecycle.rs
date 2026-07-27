use anyhow::{Result, bail};
use rusqlite::{Connection, OptionalExtension, TransactionBehavior};
use serde_json::{Value, json};

pub const DEFAULT_LEASE_SECONDS: u32 = 3600;

type ClaimDiagnostic = (String, Option<String>, Option<String>);

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Transition {
    pub command: &'static str,
    pub from: &'static str,
    pub to: &'static str,
    pub required_role: Option<&'static str>,
    pub owner_after: Option<&'static str>,
}

pub const TRANSITION_TABLE: &[Transition] = &[
    Transition {
        command: "move",
        from: "todo",
        to: "backlog",
        required_role: None,
        owner_after: None,
    },
    Transition {
        command: "move",
        from: "backlog",
        to: "todo",
        required_role: None,
        owner_after: None,
    },
    Transition {
        command: "claim",
        from: "todo",
        to: "in_progress",
        required_role: Some("developer"),
        owner_after: Some("developer"),
    },
    Transition {
        command: "claim",
        from: "in_progress",
        to: "in_progress",
        required_role: Some("developer"),
        owner_after: Some("developer"),
    },
    Transition {
        command: "submit-review",
        from: "in_progress",
        to: "review",
        required_role: Some("developer"),
        owner_after: None,
    },
    Transition {
        command: "claim-review",
        from: "review",
        to: "review",
        required_role: Some("reviewer"),
        owner_after: Some("reviewer"),
    },
    Transition {
        command: "approve",
        from: "review",
        to: "done",
        required_role: Some("reviewer"),
        owner_after: None,
    },
    Transition {
        command: "request-changes",
        from: "review",
        to: "in_progress",
        required_role: Some("reviewer"),
        owner_after: None,
    },
    Transition {
        command: "release",
        from: "in_progress",
        to: "in_progress",
        required_role: Some("developer"),
        owner_after: None,
    },
    Transition {
        command: "release",
        from: "review",
        to: "review",
        required_role: Some("reviewer"),
        owner_after: None,
    },
];

pub fn transitions() -> Value {
    Value::Array(
        TRANSITION_TABLE
            .iter()
            .map(|transition| {
                json!({
                    "command": transition.command,
                    "from": transition.from,
                    "to": transition.to,
                    "required_role": transition.required_role,
                    "owner_after": transition.owner_after,
                })
            })
            .collect(),
    )
}

pub fn claim(id: i64, agent: &str, lease_seconds: u32) -> Result<Value> {
    let conn = crate::db::open_existing()?;
    claim_inner(&conn, id, agent, lease_seconds, "claim")
}

pub fn claim_review(id: i64, agent: &str, lease_seconds: u32) -> Result<Value> {
    let conn = crate::db::open_existing()?;
    claim_inner(&conn, id, agent, lease_seconds, "claim-review")
}

fn claim_inner(
    conn: &Connection,
    id: i64,
    agent: &str,
    lease_seconds: u32,
    command: &str,
) -> Result<Value> {
    if lease_seconds == 0 {
        bail!("lease duration must be at least one second");
    }

    let specs = transitions_for(command);
    let required_role = unique_required_role(&specs)?;
    let target_status = unique_target_status(&specs)?;
    let agent_id = resolve_agent(conn, agent, required_role)?;
    let allowed_statuses = json!(
        specs
            .iter()
            .map(|transition| transition.from)
            .collect::<Vec<_>>()
    )
    .to_string();

    let result = conn.execute(
        "UPDATE tasks
         SET executor = ?1,
             status = ?2,
             claimed_at = datetime('now'),
             lease_expires_at = datetime('now', printf('+%d seconds', ?3)),
             updated_at = datetime('now')
         WHERE id = ?4
           AND status IN (SELECT value FROM json_each(?5))
           AND (
             executor IS NULL
             OR lease_expires_at IS NULL
             OR lease_expires_at <= datetime('now')
             OR executor = ?1
           )",
        rusqlite::params![
            agent_id,
            target_status,
            i64::from(lease_seconds),
            id,
            allowed_statuses
        ],
    );

    let changed = match result {
        Ok(changed) => changed,
        Err(rusqlite::Error::SqliteFailure(error, _))
            if error.extended_code == rusqlite::ffi::SQLITE_CONSTRAINT_FOREIGNKEY =>
        {
            bail!(
                "agent '{agent}' is not registered; run `agent-kanban agent register {agent} \
                 --role {required_role}` first"
            )
        }
        Err(error) => return Err(error.into()),
    };

    if changed == 0 {
        return Err(diagnose_claim_failure(conn, id, command));
    }

    crate::commands::fetch_task(conn, id)
}

pub fn submit_review(id: i64, agent: &str, raw_results: &[String]) -> Result<Value> {
    let mut conn = crate::db::open_existing()?;
    let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
    let value = submit_review_inner(&tx, id, agent, raw_results)?;
    tx.commit()?;
    Ok(value)
}

fn submit_review_inner(
    conn: &Connection,
    id: i64,
    agent: &str,
    raw_results: &[String],
) -> Result<Value> {
    let spec = transition_for("submit-review", "in_progress")?;
    let agent_id = resolve_agent(conn, agent, spec.required_role.unwrap())?;
    let task = owned_task(conn, id, agent_id, agent, spec)?;
    let criteria: Value = serde_json::from_str(&task.tests)?;
    let criteria = criteria
        .as_array()
        .ok_or_else(|| anyhow::anyhow!("task {id} has invalid acceptance criteria"))?;
    let results = parse_acceptance_results(raw_results, criteria)?;
    let revision = task
        .revision
        .checked_add(1)
        .ok_or_else(|| anyhow::anyhow!("task {id} review revision overflow"))?;

    for result in results {
        conn.execute(
            "INSERT INTO acceptance_results (
               task_id, revision, criterion_index, criterion, result, evidence,
               executor, executor_name
             ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
            rusqlite::params![
                id,
                revision,
                i64::try_from(result.criterion_index)?,
                result.criterion.to_string(),
                result.status,
                result.evidence,
                agent_id,
                agent,
            ],
        )?;
    }

    let changed = conn.execute(
        "UPDATE tasks
         SET status = ?1,
             executor = NULL,
             claimed_at = NULL,
             lease_expires_at = NULL,
             revision = ?2,
             updated_at = datetime('now')
         WHERE id = ?3
           AND status = ?4
           AND executor = ?5
           AND lease_expires_at > datetime('now')
           AND revision = ?6",
        rusqlite::params![spec.to, revision, id, spec.from, agent_id, task.revision],
    )?;
    if changed != 1 {
        bail!("task {id} changed concurrently; submit-review was not applied");
    }

    crate::commands::fetch_task(conn, id)
}

pub fn approve(id: i64, agent: &str, notes: &str) -> Result<Value> {
    finish_review(id, agent, "approved", notes)
}

pub fn request_changes(id: i64, agent: &str, notes: &str) -> Result<Value> {
    if notes.trim().is_empty() {
        bail!("request-changes notes must not be empty");
    }
    finish_review(id, agent, "changes_requested", notes)
}

fn finish_review(id: i64, agent: &str, decision: &str, notes: &str) -> Result<Value> {
    let command = match decision {
        "approved" => "approve",
        "changes_requested" => "request-changes",
        _ => unreachable!(),
    };
    let mut conn = crate::db::open_existing()?;
    let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
    let spec = transition_for(command, "review")?;
    let agent_id = resolve_agent(&tx, agent, spec.required_role.unwrap())?;
    let task = owned_task(&tx, id, agent_id, agent, spec)?;
    if task.revision == 0 {
        bail!("task {id} has no submitted review revision");
    }

    if decision == "approved" {
        let expected_results = serde_json::from_str::<Value>(&task.tests)?
            .as_array()
            .map_or(0, Vec::len);
        let (recorded, failed): (i64, i64) = tx.query_row(
            "SELECT COUNT(*),
                    COUNT(*) FILTER (WHERE result = 'failed')
             FROM acceptance_results
             WHERE task_id = ?1 AND revision = ?2",
            rusqlite::params![id, task.revision],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )?;
        if usize::try_from(recorded)? != expected_results {
            bail!(
                "task {id} does not have a complete acceptance result set for revision {}",
                task.revision
            );
        }
        if failed != 0 {
            bail!(
                "task {id} has {failed} failed acceptance result(s) in revision {}; \
                 request changes instead",
                task.revision
            );
        }
    }

    tx.execute(
        "INSERT INTO review_history (
           task_id, revision, decision, notes, executor, executor_name
         ) VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
        rusqlite::params![id, task.revision, decision, notes, agent_id, agent],
    )?;

    let changed = tx.execute(
        "UPDATE tasks
         SET status = ?1,
             executor = NULL,
             claimed_at = NULL,
             lease_expires_at = NULL,
             updated_at = datetime('now')
         WHERE id = ?2
           AND status = ?3
           AND executor = ?4
           AND lease_expires_at > datetime('now')
           AND revision = ?5",
        rusqlite::params![spec.to, id, spec.from, agent_id, task.revision],
    )?;
    if changed != 1 {
        bail!("task {id} changed concurrently; {command} was not applied");
    }

    let value = crate::commands::fetch_task(&tx, id)?;
    tx.commit()?;
    Ok(value)
}

pub fn move_status(id: i64, status: &str) -> Result<Value> {
    if !matches!(status, "backlog" | "todo") {
        bail!(
            "invalid move target '{status}': move only supports backlog and todo; \
             use lifecycle review commands for in_progress, review, and done"
        );
    }

    let conn = crate::db::open_existing()?;
    let current_status: Option<String> = conn
        .query_row("SELECT status FROM tasks WHERE id = ?1", [id], |row| {
            row.get(0)
        })
        .optional()?;
    let Some(current_status) = current_status else {
        bail!("task {id} not found");
    };
    let spec = TRANSITION_TABLE
        .iter()
        .find(|transition| {
            transition.command == "move"
                && transition.from == current_status
                && transition.to == status
        })
        .ok_or_else(|| {
            anyhow::anyhow!(
                "transition {current_status} -> {status} is not allowed; \
                 inspect `agent-kanban transitions`"
            )
        })?;

    let changed = conn.execute(
        "UPDATE tasks
         SET status = ?1,
             executor = NULL,
             claimed_at = NULL,
             lease_expires_at = NULL,
             updated_at = datetime('now')
         WHERE id = ?2
           AND status = ?3
           AND (
             executor IS NULL
             OR lease_expires_at IS NULL
             OR lease_expires_at <= datetime('now')
           )",
        rusqlite::params![spec.to, id, spec.from],
    )?;
    if changed == 0 {
        return Err(diagnose_unclaimed_transition_failure(&conn, id));
    }

    crate::commands::fetch_task(&conn, id)
}

pub fn release(id: i64, agent: &str) -> Result<Value> {
    let conn = crate::db::open_existing()?;
    let role: Option<String> = conn
        .query_row("SELECT role FROM agents WHERE name = ?1", [agent], |row| {
            row.get(0)
        })
        .optional()?;
    let Some(role) = role else {
        bail!("agent '{agent}' is not registered");
    };
    let specs: Vec<&Transition> = transitions_for("release")
        .into_iter()
        .filter(|transition| transition.required_role == Some(role.as_str()))
        .collect();
    let allowed_statuses = json!(
        specs
            .iter()
            .map(|transition| transition.from)
            .collect::<Vec<_>>()
    )
    .to_string();

    let changed = conn.execute(
        "UPDATE tasks
         SET executor = NULL,
             claimed_at = NULL,
             lease_expires_at = NULL,
             updated_at = datetime('now')
         WHERE id = ?1
           AND executor = (SELECT id FROM agents WHERE name = ?2)
           AND lease_expires_at > datetime('now')
           AND status IN (SELECT value FROM json_each(?3))",
        rusqlite::params![id, agent, allowed_statuses],
    )?;
    if changed == 0 {
        return Err(diagnose_release_failure(&conn, id, agent));
    }

    crate::commands::fetch_task(&conn, id)
}

#[derive(Debug)]
struct OwnedTask {
    tests: String,
    revision: i64,
}

fn owned_task(
    conn: &Connection,
    id: i64,
    agent_id: i64,
    agent: &str,
    spec: &Transition,
) -> Result<OwnedTask> {
    let row: Option<(String, Option<i64>, bool, String, i64)> = conn
        .query_row(
            "SELECT status,
                    executor,
                    COALESCE(lease_expires_at > datetime('now'), 0),
                    tests,
                    revision
             FROM tasks WHERE id = ?1",
            [id],
            |row| {
                Ok((
                    row.get(0)?,
                    row.get(1)?,
                    row.get(2)?,
                    row.get(3)?,
                    row.get(4)?,
                ))
            },
        )
        .optional()?;
    let Some((status, executor, lease_active, tests, revision)) = row else {
        bail!("task {id} not found");
    };
    if status != spec.from {
        bail!(
            "cannot run {} for task {id} in status '{status}'; expected '{}'",
            spec.command,
            spec.from
        );
    }
    if executor != Some(agent_id) {
        bail!("task {id} is not claimed by agent '{agent}'");
    }
    if !lease_active {
        bail!("agent '{agent}' no longer holds task {id}; its lease expired");
    }
    Ok(OwnedTask { tests, revision })
}

fn resolve_agent(conn: &Connection, agent: &str, required_role: &str) -> Result<i64> {
    let row: Option<(i64, String)> = conn
        .query_row(
            "SELECT id, role FROM agents WHERE name = ?1",
            [agent],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .optional()?;
    match row {
        None => bail!(
            "agent '{agent}' is not registered; run `agent-kanban agent register {agent} \
             --role {required_role}` first"
        ),
        Some((_, role)) if role != required_role => {
            bail!("agent '{agent}' has role '{role}'; {required_role} role is required")
        }
        Some((id, _)) => Ok(id),
    }
}

fn diagnose_claim_failure(conn: &Connection, id: i64, command: &str) -> anyhow::Error {
    let row: rusqlite::Result<Option<ClaimDiagnostic>> = conn
        .query_row(
            "SELECT tasks.status, agents.name, tasks.lease_expires_at
             FROM tasks
             LEFT JOIN agents ON tasks.executor = agents.id
             WHERE tasks.id = ?1",
            [id],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )
        .optional();
    match row {
        Err(error) => error.into(),
        Ok(None) => anyhow::anyhow!("task {id} not found"),
        Ok(Some((status, _, _)))
            if !TRANSITION_TABLE
                .iter()
                .any(|transition| transition.command == command && transition.from == status) =>
        {
            anyhow::anyhow!(
                "cannot run {command} for task {id} in status '{status}'; \
                 inspect `agent-kanban transitions`"
            )
        }
        Ok(Some((_, Some(owner), Some(expires)))) => {
            anyhow::anyhow!("task {id} is already claimed by '{owner}' until {expires}")
        }
        Ok(Some(_)) => {
            anyhow::anyhow!("task {id} could not be claimed because its state changed")
        }
    }
}

fn diagnose_unclaimed_transition_failure(conn: &Connection, id: i64) -> anyhow::Error {
    let row: rusqlite::Result<Option<(Option<String>, Option<String>)>> = conn
        .query_row(
            "SELECT agents.name, tasks.lease_expires_at
             FROM tasks
             LEFT JOIN agents ON tasks.executor = agents.id
             WHERE tasks.id = ?1",
            [id],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .optional();
    match row {
        Err(error) => error.into(),
        Ok(None) => anyhow::anyhow!("task {id} not found"),
        Ok(Some((Some(owner), Some(expires)))) => {
            anyhow::anyhow!("task {id} is claimed by '{owner}' until {expires}")
        }
        Ok(Some(_)) => anyhow::anyhow!("task {id} changed concurrently; try again"),
    }
}

fn diagnose_release_failure(conn: &Connection, id: i64, agent: &str) -> anyhow::Error {
    let row: rusqlite::Result<Option<(Option<String>, bool)>> = conn
        .query_row(
            "SELECT agents.name, COALESCE(tasks.lease_expires_at > datetime('now'), 0)
             FROM tasks
             LEFT JOIN agents ON tasks.executor = agents.id
             WHERE tasks.id = ?1",
            [id],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .optional();
    match row {
        Err(error) => error.into(),
        Ok(None) => anyhow::anyhow!("task {id} not found"),
        Ok(Some((None, _))) => anyhow::anyhow!("task {id} is not claimed"),
        Ok(Some((Some(owner), _))) if owner != agent => {
            anyhow::anyhow!("task {id} is claimed by '{owner}', not '{agent}'")
        }
        Ok(Some((Some(_), false))) => {
            anyhow::anyhow!("agent '{agent}' no longer holds task {id}; its lease expired")
        }
        Ok(Some(_)) => anyhow::anyhow!(
            "task {id} cannot be released in its current status; \
             inspect `agent-kanban transitions`"
        ),
    }
}

fn transitions_for(command: &str) -> Vec<&'static Transition> {
    TRANSITION_TABLE
        .iter()
        .filter(|transition| transition.command == command)
        .collect()
}

fn transition_for(command: &str, from: &str) -> Result<&'static Transition> {
    TRANSITION_TABLE
        .iter()
        .find(|transition| transition.command == command && transition.from == from)
        .ok_or_else(|| anyhow::anyhow!("no transition for {command} from {from}"))
}

fn unique_required_role(specs: &[&Transition]) -> Result<&'static str> {
    let Some(role) = specs.first().and_then(|spec| spec.required_role) else {
        bail!("transition command has no required role");
    };
    if specs.iter().any(|spec| spec.required_role != Some(role)) {
        bail!("transition command has inconsistent required roles");
    }
    Ok(role)
}

fn unique_target_status(specs: &[&Transition]) -> Result<&'static str> {
    let Some(target) = specs.first().map(|spec| spec.to) else {
        bail!("transition command has no allowed transitions");
    };
    if specs.iter().any(|spec| spec.to != target) {
        bail!("transition command has inconsistent target statuses");
    }
    Ok(target)
}

#[derive(Debug)]
struct AcceptanceResult {
    criterion_index: usize,
    criterion: Value,
    status: String,
    evidence: String,
}

fn parse_acceptance_results(
    raw_results: &[String],
    criteria: &[Value],
) -> Result<Vec<AcceptanceResult>> {
    if raw_results.len() != criteria.len() {
        bail!(
            "submit-review requires exactly one result for each acceptance criterion \
             (expected {}, received {})",
            criteria.len(),
            raw_results.len()
        );
    }

    let mut parsed: Vec<Option<AcceptanceResult>> = std::iter::repeat_with(|| None)
        .take(criteria.len())
        .collect();
    for (position, raw) in raw_results.iter().enumerate() {
        let value: Value = serde_json::from_str(raw)
            .map_err(|error| anyhow::anyhow!("result {position}: invalid JSON: {error}"))?;
        let object = value
            .as_object()
            .ok_or_else(|| anyhow::anyhow!("result {position}: must be a JSON object"))?;
        if object.len() != 3 {
            bail!("result {position}: must contain exactly criterion, status, and evidence");
        }
        let criterion_index = object
            .get("criterion")
            .and_then(Value::as_u64)
            .ok_or_else(|| {
                anyhow::anyhow!("result {position}: field 'criterion' must be an integer")
            })
            .and_then(|index| {
                usize::try_from(index)
                    .map_err(|_| anyhow::anyhow!("result {position}: criterion is too large"))
            })?;
        if criterion_index >= criteria.len() {
            bail!(
                "result {position}: criterion {criterion_index} is out of range \
                 (task has {} criteria)",
                criteria.len()
            );
        }
        let status = object
            .get("status")
            .and_then(Value::as_str)
            .ok_or_else(|| anyhow::anyhow!("result {position}: field 'status' must be a string"))?;
        if !matches!(status, "passed" | "failed") {
            bail!("result {position}: invalid status '{status}'; must be passed or failed");
        }
        let evidence = object
            .get("evidence")
            .and_then(Value::as_str)
            .ok_or_else(|| anyhow::anyhow!("result {position}: field 'evidence' must be a string"))?
            .trim();
        if evidence.is_empty() {
            bail!("result {position}: evidence must not be empty");
        }
        if parsed[criterion_index].is_some() {
            bail!("criterion {criterion_index} has more than one result");
        }
        parsed[criterion_index] = Some(AcceptanceResult {
            criterion_index,
            criterion: criteria[criterion_index].clone(),
            status: status.to_string(),
            evidence: evidence.to_string(),
        });
    }

    parsed
        .into_iter()
        .enumerate()
        .map(|(index, result)| {
            result.ok_or_else(|| anyhow::anyhow!("criterion {index} has no result"))
        })
        .collect()
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

    fn register(conn: &Connection, name: &str, role: &str) {
        conn.execute(
            "INSERT INTO agents (name, role) VALUES (?1, ?2)",
            rusqlite::params![name, role],
        )
        .unwrap();
    }

    fn add_task(conn: &Connection) -> i64 {
        conn.execute(
            "INSERT INTO tasks (title, priority, tests)
             VALUES ('task', 'medium', '[{\"describe\":\"d\",\"input\":\"i\",\"output\":\"o\"}]')",
            [],
        )
        .unwrap();
        conn.last_insert_rowid()
    }

    fn result(status: &str) -> Vec<String> {
        vec![format!(
            r#"{{"criterion":0,"status":"{status}","evidence":"cargo test"}}"#
        )]
    }

    #[test]
    fn transition_table_contains_only_expected_lifecycle_edges() {
        let edges: Vec<(&str, &str, &str)> = TRANSITION_TABLE
            .iter()
            .map(|transition| (transition.command, transition.from, transition.to))
            .collect();
        assert_eq!(
            edges,
            vec![
                ("move", "todo", "backlog"),
                ("move", "backlog", "todo"),
                ("claim", "todo", "in_progress"),
                ("claim", "in_progress", "in_progress"),
                ("submit-review", "in_progress", "review"),
                ("claim-review", "review", "review"),
                ("approve", "review", "done"),
                ("request-changes", "review", "in_progress"),
                ("release", "in_progress", "in_progress"),
                ("release", "review", "review"),
            ]
        );
    }

    #[test]
    fn full_review_cycle_records_results_and_history() {
        let conn = setup();
        register(&conn, "dev", "developer");
        register(&conn, "reviewer", "reviewer");
        let id = add_task(&conn);

        let claimed = claim_inner(&conn, id, "dev", 60, "claim").unwrap();
        assert_eq!(claimed["status"], "in_progress");
        assert_eq!(claimed["executor_role"], "developer");

        let submitted = submit_review_inner(&conn, id, "dev", &result("passed")).unwrap();
        assert_eq!(submitted["status"], "review");
        assert_eq!(submitted["executor"], Value::Null);
        assert_eq!(submitted["revision"], 1);
        assert_eq!(submitted["acceptance_results"][0]["status"], "passed");

        let claimed = claim_inner(&conn, id, "reviewer", 60, "claim-review").unwrap();
        assert_eq!(claimed["status"], "review");
        assert_eq!(claimed["executor_role"], "reviewer");

        let spec = transition_for("approve", "review").unwrap();
        let reviewer_id = resolve_agent(&conn, "reviewer", "reviewer").unwrap();
        let task = owned_task(&conn, id, reviewer_id, "reviewer", spec).unwrap();
        conn.execute(
            "INSERT INTO review_history
             (task_id, revision, decision, notes, executor, executor_name)
             VALUES (?1, ?2, 'approved', 'looks good', ?3, 'reviewer')",
            rusqlite::params![id, task.revision, reviewer_id],
        )
        .unwrap();
        conn.execute(
            "UPDATE tasks
             SET status = 'done', executor = NULL, claimed_at = NULL,
                 lease_expires_at = NULL
             WHERE id = ?1",
            [id],
        )
        .unwrap();
        let approved = crate::commands::fetch_task(&conn, id).unwrap();
        assert_eq!(approved["status"], "done");
        assert_eq!(approved["executor"], Value::Null);
        assert_eq!(approved["review_history"][0]["decision"], "approved");
    }

    #[test]
    fn roles_are_enforced_for_both_claim_stages() {
        let conn = setup();
        register(&conn, "dev", "developer");
        register(&conn, "reviewer", "reviewer");
        let id = add_task(&conn);

        let error = claim_inner(&conn, id, "reviewer", 60, "claim").unwrap_err();
        assert!(error.to_string().contains("developer role is required"));

        claim_inner(&conn, id, "dev", 60, "claim").unwrap();
        submit_review_inner(&conn, id, "dev", &result("passed")).unwrap();
        let error = claim_inner(&conn, id, "dev", 60, "claim-review").unwrap_err();
        assert!(error.to_string().contains("reviewer role is required"));
    }

    #[test]
    fn expired_lease_can_be_reclaimed_but_cannot_submit() {
        let conn = setup();
        register(&conn, "dev-a", "developer");
        register(&conn, "dev-b", "developer");
        let id = add_task(&conn);
        claim_inner(&conn, id, "dev-a", 60, "claim").unwrap();
        conn.execute(
            "UPDATE tasks SET lease_expires_at = datetime('now', '-1 second') WHERE id = ?1",
            [id],
        )
        .unwrap();

        let error = submit_review_inner(&conn, id, "dev-a", &result("passed")).unwrap_err();
        assert!(error.to_string().contains("lease expired"));

        let reclaimed = claim_inner(&conn, id, "dev-b", 60, "claim").unwrap();
        assert_eq!(reclaimed["executor"], "dev-b");
    }

    #[test]
    fn acceptance_results_require_complete_unique_evidence() {
        let criteria = vec![json!({"describe":"d","input":"i","output":"o"})];
        let duplicate = vec![
            r#"{"criterion":0,"status":"passed","evidence":"one"}"#.to_string(),
            r#"{"criterion":0,"status":"passed","evidence":"two"}"#.to_string(),
        ];
        assert!(parse_acceptance_results(&duplicate, &criteria).is_err());

        let empty_evidence =
            vec![r#"{"criterion":0,"status":"passed","evidence":" "}"#.to_string()];
        assert!(
            parse_acceptance_results(&empty_evidence, &criteria)
                .unwrap_err()
                .to_string()
                .contains("evidence")
        );
    }
}
