//! `init` racing itself. Converting a brand-new database file to WAL mode needs
//! a brief exclusive lock and (confirmed by direct reproduction) can surface
//! "database is locked" even with `busy_timeout` set; `db::init()` retries that
//! one error because every statement it runs is idempotent. These tests prove
//! that several `init` runs started at the same moment never leak that error,
//! build the schema exactly once, and leave a board that really works.

mod common;

use std::path::Path;
use std::process::{Child, Command, Stdio};

const ROUNDS: usize = 20;
const CONTENDERS: usize = 4;

fn spawn_init(dir: &Path) -> Child {
    Command::new(env!("CARGO_BIN_EXE_agent-kanban"))
        .arg("init")
        .current_dir(dir)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap()
}

/// Wait for every contender and check that each one succeeded quietly.
fn assert_all_initialized(children: Vec<Child>, round: usize) {
    for (n, child) in children.into_iter().enumerate() {
        let out = child.wait_with_output().unwrap();
        let stdout = String::from_utf8_lossy(&out.stdout);
        let stderr = String::from_utf8_lossy(&out.stderr);
        assert!(
            out.status.success(),
            "round {round}: init #{n} failed: stdout={stdout} stderr={stderr}"
        );
        assert_eq!(stdout, "initialized\n", "round {round}: init #{n}");
        assert!(
            !stderr.contains("locked"),
            "round {round}: init #{n} surfaced lock contention: {stderr}"
        );
    }
}

#[test]
fn concurrent_init_in_a_fresh_directory_succeeds_everywhere_and_builds_the_schema_once() {
    for round in 0..ROUNDS {
        let dir = common::project();
        let children = (0..CONTENDERS).map(|_| spawn_init(dir.path())).collect();
        assert_all_initialized(children, round);

        let conn = common::db(&dir);
        let version: i64 = conn
            .query_row("PRAGMA user_version", [], |row| row.get(0))
            .unwrap();
        assert_eq!(version, 3, "round {round}");
        let tables: Vec<String> = conn
            .prepare(
                "SELECT name FROM sqlite_master
                 WHERE type = 'table' AND name NOT LIKE 'sqlite_%' ORDER BY name",
            )
            .unwrap()
            .query_map([], |row| row.get(0))
            .unwrap()
            .map(Result::unwrap)
            .collect();
        assert_eq!(
            tables,
            [
                "acceptance_results",
                "agents",
                "review_history",
                "task_deps",
                "tasks"
            ],
            "round {round}"
        );

        // Not just "no error": the board must actually work afterwards.
        common::register(&dir, "alice", "developer");
        assert_eq!(common::add_task(&dir, "t", "low"), 1, "round {round}");
    }
}

#[test]
fn concurrent_init_on_a_board_with_data_keeps_every_row() {
    let dir = common::initialized();
    common::register(&dir, "alice", "developer");
    let first = common::add_task(&dir, "keep me", "high");
    let second = common::add_task_after(&dir, "me too", &[first]);

    for round in 0..ROUNDS {
        let children = (0..CONTENDERS).map(|_| spawn_init(dir.path())).collect();
        assert_all_initialized(children, round);
    }

    assert_eq!(
        common::run(&dir, &["list"]),
        format!("#{first} high todo keep me\n#{second} medium todo after:{first} me too")
    );
    assert_eq!(common::run(&dir, &["agent", "list"]), "alice developer");
}
