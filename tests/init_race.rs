//! Real OS-process concurrency test for `agent-kanban init`. Unlike every other
//! guarded operation in this codebase, `init`'s race isn't about a
//! compare-and-swap guard -- it's about converting a brand-new database
//! file to WAL mode for the first time, which needs a brief exclusive lock
//! and (confirmed by direct reproduction) can surface "database is locked"
//! even with `busy_timeout` set, in a way plain pragma ordering alone didn't
//! fully close. `db::init()` mitigates this with a small idempotent retry
//! loop (CREATE TABLE IF NOT EXISTS makes blind retry safe). This test
//! proves two concurrent `init` calls in a fresh directory never surface
//! that error to the caller, and that the resulting project is fully
//! functional afterward, not just "didn't print an error."

use assert_cmd::Command as AssertCommand;
use std::process::{Command, Stdio};
use tempfile::TempDir;

const NUM_ROUNDS: usize = 25;

const fn kanban_bin() -> &'static str {
    env!("CARGO_BIN_EXE_agent-kanban")
}

#[test]
fn concurrent_init_never_surfaces_database_locked() {
    for round in 0..NUM_ROUNDS {
        let dir = TempDir::new().unwrap();

        let child_a = Command::new(kanban_bin())
            .arg("init")
            .current_dir(&dir)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        let child_b = Command::new(kanban_bin())
            .arg("init")
            .current_dir(&dir)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();

        let out_a = child_a.wait_with_output().unwrap();
        let out_b = child_b.wait_with_output().unwrap();

        for (label, out) in [("a", &out_a), ("b", &out_b)] {
            assert!(
                out.status.success(),
                "round {round}: init {label} failed: stdout={} stderr={}",
                String::from_utf8_lossy(&out.stdout),
                String::from_utf8_lossy(&out.stderr),
            );
            assert!(
                !String::from_utf8_lossy(&out.stderr).contains("locked"),
                "round {round}: init {label} surfaced lock contention: {}",
                String::from_utf8_lossy(&out.stderr),
            );
        }

        // Not just "no error" -- the resulting project must actually work.
        let mut register = AssertCommand::cargo_bin("agent-kanban").unwrap();
        register
            .current_dir(&dir)
            .args(["agent", "register", "alice"])
            .assert()
            .success();

        let mut add = AssertCommand::cargo_bin("agent-kanban").unwrap();
        add.current_dir(&dir)
            .args([
                "add",
                "--title",
                "t",
                "--priority",
                "low",
                "--test",
                r#"{"describe":"d","input":"i","output":"o"}"#,
            ])
            .assert()
            .success();
    }
}
