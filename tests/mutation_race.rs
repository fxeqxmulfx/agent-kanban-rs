//! Real OS-process concurrency tests for `remove`/`release`/`agent remove`
//! racing against `claim` on the same task. `claim_race.rs` already proves
//! the atomic compare-and-swap mechanism (`UPDATE ... WHERE <guard>`) is
//! genuinely exclusive under real cross-process WAL contention for
//! claim-vs-claim; `remove`/`edit`/`release` were fixed with the identical
//! mechanism (see src/commands/task.rs and src/commands/lifecycle.rs), but
//! until now that was only proven at the single-process SQL level, never
//! empirically under real concurrent processes. These tests close that
//! asymmetry for the operations with cleanly well-defined outcomes (remove
//! and release are mutually exclusive with claim on the `executor` column,
//! unlike edit, which doesn't touch it and can legitimately succeed
//! alongside a concurrent claim). The `agent_remove_vs_claim` test below
//! covers the original TOCTOU fix (claim's FK-violation catch) under real
//! concurrency for the first time, and is what surfaced the separate
//! "database is locked" transaction-behavior bug fixed alongside it.

use assert_cmd::Command as AssertCommand;
use serde_json::Value;
use std::process::{Command, Stdio};
use tempfile::TempDir;

const NUM_ROUNDS: usize = 20;

const fn kanban_bin() -> &'static str {
    env!("CARGO_BIN_EXE_agent-kanban")
}

fn run_json(dir: &TempDir, args: &[&str]) -> Value {
    let mut cmd = AssertCommand::cargo_bin("agent-kanban").unwrap();
    let output = cmd.current_dir(dir).args(args).output().unwrap();
    assert!(
        output.status.success(),
        "command {:?} failed: stdout={} stderr={}",
        args,
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    serde_json::from_slice(&output.stdout).unwrap()
}

fn run_json_expect_failure(dir: &TempDir, args: &[&str]) -> Value {
    let mut cmd = AssertCommand::cargo_bin("agent-kanban").unwrap();
    let output = cmd.current_dir(dir).args(args).output().unwrap();
    assert!(
        !output.status.success(),
        "command {args:?} unexpectedly succeeded"
    );
    serde_json::from_slice(&output.stderr).unwrap()
}

/// Race `remove <id>` against `claim <id> --agent X` on a fresh unclaimed
/// task. The two guarded statements are mutually exclusive on `executor`,
/// so there are exactly two valid outcomes depending purely on which one's
/// atomic statement commits first:
///   (a) remove wins: task deleted, claim then fails "task {id} not found"
///   (b) claim wins: task claimed, remove then fails "... is claimed ..."
/// Anything else (both succeed, neither succeeds, or the final state
/// doesn't match whichever process reported success) indicates the guard
/// isn't actually atomic/exclusive.
#[test]
fn remove_vs_claim_race_never_corrupts_state() {
    let dir = TempDir::new().unwrap();
    run_json(&dir, &["init"]);
    run_json(&dir, &["agent", "register", "agent-x"]);

    for round in 0..NUM_ROUNDS {
        let created = run_json(
            &dir,
            &[
                "add",
                "--title",
                &format!("race task round {round}"),
                "--priority",
                "medium",
                "--test",
                r#"{"describe":"d","input":"i","output":"o"}"#,
            ],
        );
        let id = created["id"].as_i64().unwrap();
        let id_str = id.to_string();

        // Spawn both before waiting on either, so they actually race.
        let remove_child = Command::new(kanban_bin())
            .args(["remove", &id_str])
            .current_dir(&dir)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        let claim_child = Command::new(kanban_bin())
            .args(["claim", &id_str, "--agent", "agent-x"])
            .current_dir(&dir)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();

        let remove_out = remove_child.wait_with_output().unwrap();
        let claim_out = claim_child.wait_with_output().unwrap();

        let remove_won = remove_out.status.success();
        let claim_won = claim_out.status.success();

        assert_ne!(
            remove_won,
            claim_won,
            "round {round}: exactly one of remove/claim must win, got remove_won={remove_won} \
             claim_won={claim_won} (remove stdout/stderr: {}/{}, claim stdout/stderr: {}/{})",
            String::from_utf8_lossy(&remove_out.stdout),
            String::from_utf8_lossy(&remove_out.stderr),
            String::from_utf8_lossy(&claim_out.stdout),
            String::from_utf8_lossy(&claim_out.stderr),
        );

        if remove_won {
            let claim_stderr = String::from_utf8_lossy(&claim_out.stderr);
            assert!(
                claim_stderr.contains("not found"),
                "round {round}: remove won but claim's failure wasn't 'not found': {claim_stderr}"
            );
            let err = run_json_expect_failure(&dir, &["show", &id_str]);
            assert!(err["error"].as_str().unwrap().contains("not found"));
        } else {
            let remove_stderr = String::from_utf8_lossy(&remove_out.stderr);
            assert!(
                remove_stderr.contains("claimed"),
                "round {round}: claim won but remove's failure wasn't 'claimed': {remove_stderr}"
            );
            let shown = run_json(&dir, &["show", &id_str]);
            assert_eq!(shown["executor"], "agent-x");
            assert_eq!(shown["status"], "in_progress");
        }
    }
}

/// Race `release <id>` against `claim <id> --agent B` on a task already
/// claimed by agent A. The release names agent A and therefore always
/// succeeds here; the only question is timing relative to claim:
///   (a) release commits first: executor -> NULL, then claim sees NULL and
///       succeeds too (agent B now holds it, status `in_progress`).
///   (b) claim's statement runs while A still holds it: claim fails
///       "already claimed"; release still succeeds, clearing executor.
/// Both are valid, well-defined outcomes; anything else is corruption.
#[test]
fn release_vs_claim_race_never_corrupts_state() {
    let dir = TempDir::new().unwrap();
    run_json(&dir, &["init"]);
    run_json(&dir, &["agent", "register", "agent-a"]);
    run_json(&dir, &["agent", "register", "agent-b"]);

    for round in 0..NUM_ROUNDS {
        let created = run_json(
            &dir,
            &[
                "add",
                "--title",
                &format!("race task round {round}"),
                "--priority",
                "medium",
                "--test",
                r#"{"describe":"d","input":"i","output":"o"}"#,
            ],
        );
        let id = created["id"].as_i64().unwrap();
        let id_str = id.to_string();
        run_json(&dir, &["claim", &id_str, "--agent", "agent-a"]);

        let release_child = Command::new(kanban_bin())
            .args(["release", &id_str, "--agent", "agent-a"])
            .current_dir(&dir)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        let claim_child = Command::new(kanban_bin())
            .args(["claim", &id_str, "--agent", "agent-b"])
            .current_dir(&dir)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();

        let release_out = release_child.wait_with_output().unwrap();
        let claim_out = claim_child.wait_with_output().unwrap();

        assert!(
            release_out.status.success(),
            "round {round}: release must always succeed here (task started claimed by \
             agent-a); stdout/stderr: {}/{}",
            String::from_utf8_lossy(&release_out.stdout),
            String::from_utf8_lossy(&release_out.stderr),
        );

        let shown = run_json(&dir, &["show", &id_str]);
        if claim_out.status.success() {
            // Outcome (a): release cleared it, claim then took it.
            assert_eq!(shown["executor"], "agent-b");
        } else {
            // Outcome (b): claim ran while agent-a still held it.
            let claim_stderr = String::from_utf8_lossy(&claim_out.stderr);
            assert!(
                claim_stderr.contains("already claimed"),
                "round {round}: claim's failure wasn't 'already claimed': {claim_stderr}"
            );
            // release still went on to clear it independently.
            assert_eq!(shown["executor"], Value::Null);
        }
        assert_eq!(shown["status"], "in_progress");
    }
}

/// Race `agent remove <name>` against `claim <id> --agent <name>` for the
/// SAME agent -- the original TOCTOU this whole review thread started from
/// (see src/commands/lifecycle.rs's `claim_with_agent_id`), which until now
/// only had a deterministic single-process simulation, never a real
/// multi-process proof. Investigating that exact gap surfaced a second,
/// unrelated bug: `agent::remove`'s transaction used the default `Deferred`
/// behavior, which caused spurious "database is locked" errors under
/// completely ordinary concurrency (confirmed: ~83% failure rate in a
/// manual repro) despite `busy_timeout` being set, because a deferred
/// transaction upgrading a read snapshot to a writer mid-transaction can hit
/// `SQLITE_BUSY` in a way `busy_timeout`'s retry loop doesn't cover. Fixed by
/// switching to `TransactionBehavior::Immediate`.
///
/// This test guards both: `agent remove` must never surface "database is
/// locked" under this race, and `claim` must never leak a raw
/// "FOREIGN KEY constraint failed" (it must always fail cleanly with "is not
/// registered" if it loses the race after the agent is already gone). It
/// also verifies directly against the raw DB (bypassing the `LEFT JOIN` in
/// `show`, which would silently render a dangling reference as `null`
/// instead of surfacing it) that no task is ever left pointing at a deleted
/// agent.
#[test]
fn agent_remove_vs_claim_race_never_locks_or_leaks_fk() {
    let dir = TempDir::new().unwrap();
    run_json(&dir, &["init"]);

    for round in 0..NUM_ROUNDS {
        let agent_name = format!("agent-{round}");
        run_json(&dir, &["agent", "register", &agent_name]);
        let created = run_json(
            &dir,
            &[
                "add",
                "--title",
                &format!("race task round {round}"),
                "--priority",
                "medium",
                "--test",
                r#"{"describe":"d","input":"i","output":"o"}"#,
            ],
        );
        let id = created["id"].as_i64().unwrap();
        let id_str = id.to_string();

        let remove_child = Command::new(kanban_bin())
            .args(["agent", "remove", &agent_name])
            .current_dir(&dir)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        let claim_child = Command::new(kanban_bin())
            .args(["claim", &id_str, "--agent", &agent_name])
            .current_dir(&dir)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();

        let remove_out = remove_child.wait_with_output().unwrap();
        let claim_out = claim_child.wait_with_output().unwrap();
        let remove_stderr = String::from_utf8_lossy(&remove_out.stderr).to_string();
        let claim_stderr = String::from_utf8_lossy(&claim_out.stderr).to_string();

        // agent remove only ever races a single claim for the agent it's
        // about to delete, and nothing else is concurrently modifying this
        // agent's row, so it must always succeed -- in particular, it must
        // never surface "database is locked".
        assert!(
            remove_out.status.success(),
            "round {round}: agent remove must always succeed here; stderr={remove_stderr}"
        );
        assert!(
            !remove_stderr.contains("database is locked"),
            "round {round}: agent remove hit spurious lock contention: {remove_stderr}"
        );

        if !claim_out.status.success() {
            assert!(
                !claim_stderr.contains("FOREIGN KEY"),
                "round {round}: raw FK error leaked from claim: {claim_stderr}"
            );
            assert!(
                claim_stderr.contains("not registered"),
                "round {round}: unexpected claim failure: {claim_stderr}"
            );
        }

        // Regardless of outcome, there must be no dangling executor
        // reference. Checked against the raw DB, not `show`'s output --
        // `show`'s LEFT JOIN would silently render a dangling id as `null`,
        // masking exactly the bug this test exists to catch.
        let db_path = dir.path().join(".kanban").join("board.db");
        let conn = rusqlite::Connection::open(&db_path).unwrap();
        let dangling: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM tasks LEFT JOIN agents ON tasks.executor = agents.id \
                 WHERE tasks.id = ?1 AND tasks.executor IS NOT NULL AND agents.id IS NULL",
                [id],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(
            dangling, 0,
            "round {round}: task has a dangling executor reference to a deleted agent"
        );
    }
}

/// Race two `agent remove <same-name>` calls against each other. Both use
/// the `Immediate`-transaction fix from `agent_remove_vs_claim`; this
/// exercises that same fix under a different pairing (write-write
/// contention on the same agent row, rather than agent-remove vs. claim).
/// Exactly one must win (exit 0); the other must fail cleanly with
/// "not found" (its DELETE affects 0 rows since the winner already removed
/// it) -- never both winning, never a spurious "database is locked".
#[test]
fn double_agent_remove_race_has_exactly_one_winner() {
    let dir = TempDir::new().unwrap();
    run_json(&dir, &["init"]);

    for round in 0..NUM_ROUNDS {
        let name = format!("agent-{round}");
        run_json(&dir, &["agent", "register", &name]);

        let child_a = Command::new(kanban_bin())
            .args(["agent", "remove", &name])
            .current_dir(&dir)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        let child_b = Command::new(kanban_bin())
            .args(["agent", "remove", &name])
            .current_dir(&dir)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();

        let out_a = child_a.wait_with_output().unwrap();
        let out_b = child_b.wait_with_output().unwrap();
        let a_won = out_a.status.success();
        let b_won = out_b.status.success();

        assert_ne!(
            a_won, b_won,
            "round {round}: exactly one of the two agent-remove calls must win"
        );

        let loser_stderr = if a_won {
            String::from_utf8_lossy(&out_b.stderr).to_string()
        } else {
            String::from_utf8_lossy(&out_a.stderr).to_string()
        };
        assert!(
            !loser_stderr.contains("locked"),
            "round {round}: loser hit lock contention instead of a clean error: {loser_stderr}"
        );
        assert!(
            loser_stderr.contains("not found"),
            "round {round}: loser's failure wasn't 'not found': {loser_stderr}"
        );
    }
}
