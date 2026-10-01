//! Boards written by older versions are upgraded in place the first time a new
//! build opens them, and nothing on them is lost. Each fixture is a board as
//! that version left it: v1 (0.1.x), v2 (0.2) and v3 (0.2.x and 0.3.0, in
//! `tests/fixtures/board_v3.sql`). The upgrade to v4 rebuilds `tasks` so that
//! task ids are never reused (see `src/db.rs`); what the rebuild must not
//! touch is everything that points at `tasks`.

mod common;

use common::{add_task, fail, project, race, run};
use tempfile::TempDir;

/// The tables exactly as 0.1.x created them: no roles, leases, reviews or
/// dependencies.
const V1_SCHEMA: &str = r#"
CREATE TABLE agents (
  id INTEGER PRIMARY KEY,
  name TEXT NOT NULL UNIQUE,
  created_at TEXT NOT NULL DEFAULT (datetime('now'))
);
CREATE TABLE tasks (
  id INTEGER PRIMARY KEY,
  title TEXT NOT NULL,
  priority TEXT NOT NULL CHECK (priority IN ('low','medium','high','urgent')),
  status TEXT NOT NULL DEFAULT 'todo' CHECK (status IN ('backlog','todo','in_progress','review','done')),
  executor INTEGER REFERENCES agents(id),
  tags TEXT NOT NULL DEFAULT '[]' CHECK (json_valid(tags)),
  tests TEXT NOT NULL CHECK (json_valid(tests) AND json_array_length(tests) > 0),
  created_at TEXT NOT NULL DEFAULT (datetime('now')),
  updated_at TEXT NOT NULL DEFAULT (datetime('now'))
);
INSERT INTO agents (name) VALUES ('alice');
INSERT INTO tasks (title, priority, status, executor, tests) VALUES
  ('t1', 'medium', 'todo',        NULL, '[{"describe":"d","input":"i","output":"o"}]'),
  ('t2', 'medium', 'in_progress', 1,    '[{"describe":"d","input":"i","output":"o"}]'),
  ('t3', 'medium', 'review',      1,    '[{"describe":"d","input":"i","output":"o"}]'),
  ('t4', 'medium', 'done',        1,    '[{"describe":"d","input":"i","output":"o"}]'),
  ('t5', 'medium', 'backlog',     NULL, '[{"describe":"d","input":"i","output":"o"}]');
"#;

/// A v3 board with rows in every table, ids 4 and 6 gone, task 7 the newest.
const BOARD_V3: &str = include_str!("fixtures/board_v3.sql");

/// A project directory holding the v3 fixture, or, for `version == 2`, the
/// board 0.2 would have left: the same tables without the dependency table.
/// Like every board `init` made, it is in WAL mode, which sticks to the file.
fn old_board(version: i64) -> TempDir {
    let dir = project();
    std::fs::create_dir(dir.path().join(".kanban")).unwrap();
    let conn = rusqlite::Connection::open(common::db_path(&dir)).unwrap();
    conn.execute_batch("PRAGMA journal_mode=WAL;").unwrap();
    conn.execute_batch(BOARD_V3).unwrap();
    if version == 2 {
        conn.execute_batch("DROP TABLE task_deps; PRAGMA user_version = 2;")
            .unwrap();
    }
    dir
}

fn schema_version(dir: &TempDir) -> i64 {
    common::db(dir)
        .query_row("PRAGMA user_version", [], |row| row.get(0))
        .unwrap()
}

/// Rows of `PRAGMA foreign_key_check`: references that point at nothing.
fn dangling_references(dir: &TempDir) -> usize {
    common::db(dir)
        .prepare("PRAGMA foreign_key_check")
        .unwrap()
        .query_map([], |_| Ok(()))
        .unwrap()
        .count()
}

fn count(dir: &TempDir, table: &str) -> i64 {
    common::db(dir)
        .query_row(&format!("SELECT COUNT(*) FROM {table}"), [], |row| {
            row.get(0)
        })
        .unwrap()
}

/// Boards written by 0.1.x (schema version 1, or 0 for the very first builds)
/// are upgraded on first use. Their claims get a fresh one-hour lease; tasks
/// that sat in the old, role-less `review` go back to development; finished
/// tasks lose their owner. Nothing is lost.
#[test]
fn a_board_from_0_1_is_upgraded_on_first_use() {
    for version in [0, 1] {
        let dir = project();
        std::fs::create_dir(dir.path().join(".kanban")).unwrap();
        let conn = rusqlite::Connection::open(common::db_path(&dir)).unwrap();
        conn.execute_batch(V1_SCHEMA).unwrap();
        conn.execute_batch(&format!("PRAGMA user_version = {version};"))
            .unwrap();
        drop(conn);

        assert_eq!(
            run(&dir, &["list"]),
            "#2 medium in_progress@alice t2\n#3 medium in_progress t3\n#1 medium todo t1\n\
             #5 medium backlog t5\n+1 done hidden (--all)",
            "version {version}"
        );
        assert_eq!(run(&dir, &["agent", "list"]), "alice developer");
        assert_eq!(
            run(&dir, &["status"]),
            "backlog 1, todo 1, in_progress 2, review 0, done 1\nagents: alice 2"
        );
        assert_eq!(schema_version(&dir), 4, "version {version}");
        assert_eq!(dangling_references(&dir), 0, "version {version}");
        // The upgraded board is fully usable: alice's old claim can be handed in.
        assert_eq!(
            run(
                &dir,
                &[
                    "submit-review",
                    "2",
                    "--agent",
                    "alice",
                    "--pass",
                    "0",
                    "ok"
                ]
            ),
            "#2 review rev1",
            "version {version}"
        );
        // ... and its ids are never reused from now on.
        assert_eq!(add_task(&dir, "new", "low"), 6, "version {version}");
        run(&dir, &["remove", "6"]);
        assert_eq!(add_task(&dir, "newer", "low"), 7, "version {version}");
    }
}

/// A board created by 0.2 (schema 2, no dependency table) is upgraded in place
/// the first time the new binary opens it, and keeps its data.
#[test]
fn a_version_2_board_is_upgraded_in_place() {
    let dir = old_board(2);

    assert_eq!(
        run(&dir, &["list", "--all"]),
        "#5 urgent in_progress@alice held\n#3 low review in review\n#2 medium todo next\n\
         #7 low backlog newest\n#1 high done base"
    );

    assert_eq!(schema_version(&dir), 4);
    assert_eq!(dangling_references(&dir), 0);
    // The new table works: dependencies can be added right away.
    assert_eq!(
        run(
            &dir,
            &[
                "add", "--title", "new", "--test", "d", "i", "o", "--after", "2"
            ]
        ),
        "#8 todo after:2"
    );
    assert_eq!(
        run(&dir, &["agent", "list"]),
        "alice developer\nrita reviewer"
    );
}

/// The rebuild of `tasks` drops and renames a table that three others refer
/// to. Every row of every table must come through, and the replies must be
/// exactly what the build that wrote the board printed for it (these strings
/// were produced by 0.3.0 on this very fixture).
#[test]
fn a_version_3_board_is_upgraded_in_place_and_keeps_everything() {
    let dir = old_board(3);
    assert_eq!(
        [count(&dir, "agents"), count(&dir, "tasks")],
        [2, 5],
        "the fixture"
    );

    assert_eq!(
        run(&dir, &["list", "--all"]),
        "#5 urgent in_progress@alice held\n#3 low review in review\n#2 medium todo next\n\
         #7 low backlog after:5 newest\n#1 high done base"
    );
    assert_eq!(
        run(&dir, &["show", "1", "--history"]),
        "#1 high done rev2 base\ntags|core\n0|parses|a|b|passed|green\n\
         rev1|alice|failed: red\nreview|rita|changes_requested|fix it\n\
         rev2|alice|passed: green\nreview|rita|approved|good"
    );
    assert_eq!(
        run(&dir, &["show", "3", "--history"]),
        "#3 low review rev1 in review\n0|d|i|o|passed|ok\nrev1|alice|passed: ok"
    );
    assert_eq!(
        run(&dir, &["show", "5"]),
        "#5 urgent in_progress@alice blocks:7 held\n0|d|i|o"
    );
    assert_eq!(
        run(&dir, &["status"]),
        "backlog 1, todo 1, in_progress 1, review 1, done 1\nagents: alice 5; rita -"
    );

    assert_eq!(schema_version(&dir), 4);
    assert_eq!(dangling_references(&dir), 0);
    for (table, rows) in [
        ("agents", 2),
        ("tasks", 5),
        ("review_history", 2),
        ("acceptance_results", 3),
        ("task_deps", 2),
    ] {
        assert_eq!(count(&dir, table), rows, "{table}");
    }
    // The upgraded board keeps working: the held task goes to review.
    assert_eq!(
        run(
            &dir,
            &[
                "submit-review",
                "5",
                "--agent",
                "alice",
                "--pass",
                "0",
                "ok"
            ]
        ),
        "#5 review rev1"
    );
}

/// The point of the upgrade: an id that was handed out is never handed out
/// again, from the highest one in the old board onwards.
#[test]
fn an_upgraded_board_never_reuses_an_id() {
    let dir = old_board(3);

    assert_eq!(add_task(&dir, "first after the upgrade", "low"), 8);
    run(&dir, &["remove", "8"]);
    assert_eq!(add_task(&dir, "second after the upgrade", "low"), 9);
    // A stale reference to the removed #8 finds nothing instead of this task.
    assert_eq!(
        fail(&dir, &["show", "8"]),
        "error: task 8 not found",
        "#8 stays dead"
    );
}

/// `init` on an old board runs the same upgrade as any other command.
#[test]
fn init_upgrades_an_old_board_too() {
    let dir = old_board(3);

    assert_eq!(run(&dir, &["init"]), "initialized");

    assert_eq!(schema_version(&dir), 4);
    assert_eq!(dangling_references(&dir), 0);
    assert_eq!(count(&dir, "tasks"), 5);
    assert_eq!(add_task(&dir, "new", "low"), 8);
}

/// Agents start together after an upgrade of the shared project: the one that
/// gets the write lock rebuilds the table, the others wait and then find it
/// done. Nobody sees an error, and the board comes out as after a lone upgrade.
#[test]
fn processes_upgrading_one_board_at_once_all_succeed() {
    for round in 0..10 {
        let dir = old_board(3);
        let commands: Vec<Vec<String>> = (0..6).map(|_| common::argv(&["list", "--all"])).collect();

        let outcomes = race(&dir, &commands);

        for outcome in &outcomes {
            assert!(outcome.ok(), "round {round}: {outcome:?}");
            assert_eq!(outcome.stderr, "", "round {round}");
            assert_eq!(outcome.stdout, outcomes[0].stdout, "round {round}");
        }
        assert_eq!(schema_version(&dir), 4, "round {round}");
        assert_eq!(dangling_references(&dir), 0, "round {round}");
        assert_eq!(count(&dir, "tasks"), 5, "round {round}");
        assert_eq!(count(&dir, "acceptance_results"), 3, "round {round}");
        assert_eq!(count(&dir, "task_deps"), 2, "round {round}");
    }
}
