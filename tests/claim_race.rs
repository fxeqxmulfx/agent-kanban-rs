//! Real OS-process concurrency test for `agent-kanban claim`. This is the test
//! that matters most for a "multiple concurrent agents" tool: it spawns N
//! separate `agent-kanban` subprocesses that all race to claim the same task at
//! (as close to) the same wall-clock instant as possible, exercising
//! `SQLite`'s actual cross-process WAL locking rather than in-process thread
//! interleaving over a single shared connection.
//!
//! We run the race a number of times (fresh task each round, in a shared
//! project dir) to make it likely that at least one round genuinely
//! interleaves at the OS/SQLite level, even on a fast/idle machine.

use assert_cmd::Command as AssertCommand;
use serde_json::Value;
use std::process::{Child, Command, Stdio};
use tempfile::TempDir;

const NUM_AGENTS: usize = 8;
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

#[test]
fn claim_race_exactly_one_winner() {
    let dir = TempDir::new().unwrap();

    run_json(&dir, &["init"]);
    let agent_names: Vec<String> = (0..NUM_AGENTS).map(|i| format!("agent-{i}")).collect();
    for name in &agent_names {
        run_json(&dir, &["agent", "register", name]);
    }

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
        let task_id = created["id"].as_i64().unwrap();
        let task_id_str = task_id.to_string();

        // Build all child commands first, then spawn them all before waiting
        // on any of them, so they actually race each other.
        let mut children: Vec<(String, Child)> = Vec::with_capacity(NUM_AGENTS);
        for name in &agent_names {
            let child = Command::new(kanban_bin())
                .args(["claim", &task_id_str, "--agent", name])
                .current_dir(&dir)
                .stdout(Stdio::piped())
                .stderr(Stdio::piped())
                .spawn()
                .expect("failed to spawn agent-kanban claim subprocess");
            children.push((name.clone(), child));
        }

        let mut winners: Vec<String> = Vec::new();
        let mut losers: Vec<(String, String)> = Vec::new(); // (agent, stderr)

        for (name, child) in children {
            let output = child.wait_with_output().unwrap();
            if output.status.success() {
                winners.push(name);
            } else {
                let stderr = String::from_utf8_lossy(&output.stderr).to_string();
                losers.push((name, stderr));
            }
        }

        assert_eq!(
            winners.len(),
            1,
            "round {round}: expected exactly one winner, got {} winners ({winners:?}); \
             this indicates that the atomic ownership guard admitted two developers",
            winners.len()
        );

        for (name, stderr) in &losers {
            assert!(
                stderr.contains("already claimed"),
                "round {round}: loser agent {name} did not fail with 'already claimed': {stderr}"
            );
        }
        assert_eq!(
            losers.len(),
            NUM_AGENTS - 1,
            "round {round}: expected {} losers, got {}",
            NUM_AGENTS - 1,
            losers.len()
        );

        // A normal follow-up call: `show` must agree with exactly the
        // winning agent and reflect the in_progress transition.
        let shown = run_json(&dir, &["show", &task_id_str]);
        assert_eq!(
            shown["executor"].as_str().unwrap(),
            winners[0],
            "round {round}: show's executor does not match the process that won the race"
        );
        assert_eq!(shown["status"], "in_progress");
    }
}

#[test]
fn review_claim_race_exactly_one_reviewer_wins() {
    let dir = TempDir::new().unwrap();
    run_json(&dir, &["init"]);
    run_json(
        &dir,
        &["agent", "register", "developer", "--role", "developer"],
    );
    let reviewer_names: Vec<String> = (0..NUM_AGENTS).map(|i| format!("reviewer-{i}")).collect();
    for name in &reviewer_names {
        run_json(&dir, &["agent", "register", name, "--role", "reviewer"]);
    }

    for round in 0..NUM_ROUNDS {
        let created = run_json(
            &dir,
            &[
                "add",
                "--title",
                &format!("review race task round {round}"),
                "--priority",
                "medium",
                "--test",
                r#"{"describe":"d","input":"i","output":"o"}"#,
            ],
        );
        let task_id = created["id"].as_i64().unwrap().to_string();
        run_json(&dir, &["claim", &task_id, "--agent", "developer"]);
        run_json(
            &dir,
            &[
                "submit-review",
                &task_id,
                "--agent",
                "developer",
                "--result",
                r#"{"criterion":0,"status":"passed","evidence":"verified"}"#,
            ],
        );

        let mut children: Vec<(String, Child)> = Vec::with_capacity(NUM_AGENTS);
        for name in &reviewer_names {
            let child = Command::new(kanban_bin())
                .args(["claim-review", &task_id, "--agent", name])
                .current_dir(&dir)
                .stdout(Stdio::piped())
                .stderr(Stdio::piped())
                .spawn()
                .expect("failed to spawn claim-review subprocess");
            children.push((name.clone(), child));
        }

        let mut winners = Vec::new();
        for (name, child) in children {
            let output = child.wait_with_output().unwrap();
            if output.status.success() {
                winners.push(name);
            } else {
                let stderr = String::from_utf8_lossy(&output.stderr);
                assert!(
                    stderr.contains("already claimed"),
                    "round {round}: unexpected claim-review failure for {name}: {stderr}"
                );
            }
        }

        assert_eq!(
            winners.len(),
            1,
            "round {round}: expected exactly one review owner, got {winners:?}"
        );
        let shown = run_json(&dir, &["show", &task_id]);
        assert_eq!(shown["status"], "review");
        assert_eq!(shown["executor"], winners[0]);
        assert_eq!(shown["executor_role"], "reviewer");
    }
}
