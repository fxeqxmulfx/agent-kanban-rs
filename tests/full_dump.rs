//! `dump --full` and `load`: the whole board as a text file to keep in git,
//! and the same board back from it. Loaded into an empty board, a full dump
//! gives back every row of every table, so every command replies as it did on
//! the board that was dumped, and work in progress carries on.

mod common;

use common::{
    add_task, add_task_after, argv, expire_lease, fail, initialized, kanban, project, race,
    register, run,
};
use std::fs;
use std::path::{Path, PathBuf};
use tempfile::TempDir;

/// A board as 0.3.0 left it before ids were never reused.
const BOARD_V3: &str = include_str!("fixtures/board_v3.sql");

/// `dump --full > kanban.txt` in `dir`: the file, exactly as the shell would
/// write it.
fn save(dir: &TempDir) -> PathBuf {
    let output = kanban(dir).args(["dump", "--full"]).output().unwrap();
    assert!(output.status.success(), "{output:?}");
    let file = dir.path().join("kanban.txt");
    fs::write(&file, &output.stdout).unwrap();
    file
}

fn path(file: &Path) -> &str {
    file.to_str().unwrap()
}

/// Every row of every table, the `sqlite_sequence` counter included, sorted.
/// Results and reviews are numbered anew by a load, and nothing reads those
/// numbers, so they are left out.
fn rows(dir: &Path) -> Vec<String> {
    let conn = common::db(dir);
    let tables: Vec<String> = conn
        .prepare("SELECT name FROM sqlite_master WHERE type = 'table'")
        .unwrap()
        .query_map([], |row| row.get(0))
        .unwrap()
        .collect::<Result<_, _>>()
        .unwrap();
    let mut rows = Vec::new();
    for table in tables {
        let mut stmt = conn.prepare(&format!("SELECT * FROM {table}")).unwrap();
        let names: Vec<String> = stmt.column_names().map(String::from).collect();
        let renumbered = matches!(table.as_str(), "review_history" | "acceptance_results");
        let mut query = stmt.query([]).unwrap();
        while let Some(row) = query.next().unwrap() {
            let cells: Vec<String> = names
                .iter()
                .enumerate()
                .filter(|(_, name)| !(renumbered && *name == "id"))
                .map(|(index, name)| {
                    let value: rusqlite::types::Value = row.get(index).unwrap();
                    format!("{name}={value:?}")
                })
                .collect();
            rows.push(format!("{table}: {}", cells.join(", ")));
        }
    }
    rows.sort();
    rows
}

/// Every kind of row a board holds: agents of both roles, one removed after
/// it worked and its name registered again, a done task, a task sent back
/// and claimed again (a live lease), a lease that ran out, a test changed
/// after it was judged, edges to done and open tasks, tags, text that needs
/// escaping, a removed task, and mail: read and not, about a task that was
/// removed since, from and to the agent that was removed.
fn board_of_every_kind() -> TempDir {
    let dir = initialized();
    register(&dir, "dev", "developer");
    register(&dir, "rev", "reviewer");
    register(&dir, "temp", "developer");
    let as_agent = |agent: &str, args: &[&str]| run(&dir, &[args, &["--agent", agent]].concat());
    add_task(&dir, "Base", "medium");
    run(
        &dir,
        &[
            "add",
            "--title",
            "Feature | X\nline two",
            "--priority",
            "high",
            "--tag",
            "api",
            "--tag",
            r"needs\review",
            "--test",
            "adds",
            "1+1",
            "2",
            "--test",
            "fails cleanly",
            "1/0",
            "error",
            "--after",
            "1",
        ],
    );
    add_task(&dir, "Gone", "medium");
    add_task_after(&dir, "Follow-up", &[2]);
    add_task(&dir, "Someday", "low");
    run(&dir, &["move", "5", "backlog"]);
    as_agent(
        "rev",
        &["send", "dev", "is #3 still needed?", "--task", "3"],
    );
    run(&dir, &["remove", "3"]);
    add_task(&dir, "Held, then left", "urgent");
    add_task(&dir, "Rework", "high");

    as_agent("dev", &["claim", "1"]);
    as_agent("dev", &["submit-review", "1", "--pass", "0", "ok"]);
    as_agent("rev", &["claim", "1"]);
    as_agent("rev", &["approve", "1", "--notes", "good"]);

    as_agent("dev", &["claim", "2"]);
    as_agent(
        "dev",
        &[
            "submit-review",
            "2",
            "--pass",
            "0",
            "adds: ok",
            "--fail",
            "1",
            "panics",
        ],
    );
    as_agent("rev", &["claim", "2"]);
    as_agent(
        "rev",
        &[
            "request-changes",
            "2",
            "--notes",
            "handle\ndivision by zero",
        ],
    );
    as_agent("dev", &["claim", "2"]);

    as_agent("temp", &["claim", "7"]);
    as_agent("temp", &["submit-review", "7", "--pass", "0", "by temp"]);
    as_agent("rev", &["claim", "7"]);
    as_agent("rev", &["request-changes", "7", "--notes", "redo"]);
    as_agent("rev", &["send", "temp", "see my notes", "--task", "7"]);
    as_agent(
        "temp",
        &["send", "dev", "I may not finish #7", "--task", "7"],
    );
    run(&dir, &["agent", "remove", "temp"]);
    register(&dir, "temp", "developer");
    run(&dir, &["edit", "7", "--test", "works better", "in", "out2"]);

    as_agent("dev", &["claim", "6"]);
    expire_lease(&dir, 6);
    as_agent("dev", &["inbox"]);
    as_agent(
        "dev",
        &["send", "rev", "Feature | X\nis ready", "--task", "2"],
    );
    dir
}

#[test]
fn a_board_loaded_from_its_full_dump_is_the_same_board() {
    let old = board_of_every_kind();
    let file = save(&old);
    let new = initialized();

    assert_eq!(
        run(&new, &["load", path(&file)]),
        "loaded tasks 6, agents 3, mail 4"
    );

    assert_eq!(rows(new.path()), rows(old.path()));
    for args in [
        &["dump", "--full"][..],
        &["dump"],
        &["list", "--all"],
        &["status"],
        &["agent", "list"],
        // Mail that waited on the old board waits on the new one, once.
        &["inbox", "--agent", "rev"],
        &["inbox", "--agent", "temp"],
        &["inbox", "--agent", "dev"],
        &["inbox", "--agent", "rev"],
    ] {
        assert_eq!(run(&new, args), run(&old, args), "{args:?}");
    }
    assert_eq!(add_task(&new, "next", "low"), add_task(&old, "next", "low"));
}

/// The judged test of task 7 differs from its test now, the result of the
/// removed agent has no agent id, and the lease that ran out is still there.
#[test]
fn a_full_dump_keeps_what_a_dump_for_reading_leaves_out() {
    let dir = board_of_every_kind();

    let full = run(&dir, &["dump", "--full"]);

    let lines: Vec<&str> = full.lines().collect();
    assert_eq!(lines[..2], ["agent-kanban full dump 2", "next|8"]);
    assert_eq!(lines.last(), Some(&"end"));
    // Mail in the order it was sent, read or not; `inbox` shows none of
    // the times, nor whom a message was for.
    let mail: Vec<Vec<String>> = lines
        .iter()
        .filter(|line| line.starts_with("mail|"))
        .map(|line| common::split_cells(line))
        .collect();
    let shape: Vec<[&str; 4]> = mail
        .iter()
        .map(|cells| [&cells[1], &cells[2], &cells[3], &cells[6]].map(String::as_str))
        .collect();
    assert_eq!(
        shape,
        [
            ["rev", "dev", "3", "is #3 still needed?"],
            ["rev", "temp", "7", "see my notes"],
            ["temp", "dev", "7", "I may not finish #7"],
            ["dev", "rev", "2", "Feature | X\nis ready"],
        ]
    );
    let read: Vec<bool> = mail.iter().map(|cells| !cells[5].is_empty()).collect();
    assert_eq!(read, [true, false, true, false], "{full}");
    for line in [
        "after|2",
        r"tags|api|needs\\review",
        "0|works better|in|out2",
    ] {
        assert!(lines.contains(&line), "{line}\n{full}");
    }
    let judged = lines
        .iter()
        .find(|line| line.starts_with("result|1|0|passed|by temp|"))
        .unwrap();
    assert!(
        judged.starts_with("result|1|0|passed|by temp||temp|"),
        "{judged}"
    );
    assert!(judged.ends_with("|works|in|out"), "{judged}");
    assert_eq!(
        lines
            .iter()
            .filter(|line| line.starts_with("lease|"))
            .count(),
        2,
        "{full}"
    );
    assert!(full.contains(r"task|2|high|in_progress|1|"), "{full}");
    assert!(full.contains(r"|Feature \| X line two"), "{full}");
    assert!(full.contains(r"|handle\ndivision by zero|"), "{full}");
}

#[test]
fn load_fills_an_empty_board_and_replaces_one_only_when_told() {
    let old = board_of_every_kind();
    let file = save(&old);
    let other = initialized();
    register(&other, "someone", "developer");
    let before = rows(other.path());

    assert_eq!(
        fail(&other, &["load", path(&file)]),
        "error: the board is not empty (tasks 0, agents 1); `load --replace FILE` replaces it"
    );
    assert_eq!(rows(other.path()), before);

    assert_eq!(
        run(&other, &["load", "--replace", path(&file)]),
        "loaded tasks 6, agents 3, mail 4"
    );
    assert_eq!(rows(other.path()), rows(old.path()));
}

/// 0.4 wrote format 1, which has no mail: its files load as they always did.
#[test]
fn a_file_from_0_4_still_loads() {
    let format_1 = "agent-kanban full dump 1\nnext|3\nagent|1|dev|developer|2026-10-01 09:00:00\n\n\
                    task|2|low|todo|0|2026-10-01 09:01:00|2026-10-01 09:01:00|Old\n0|d|i|o\n\nend\n";
    let dir = initialized();
    fs::write(dir.path().join("old.txt"), format_1).unwrap();

    assert_eq!(run(&dir, &["load", "old.txt"]), "loaded tasks 1, agents 1");

    assert_eq!(
        run(&dir, &["dump", "--full"]),
        format_1.replacen("dump 1", "dump 2", 1).trim_end()
    );
}

/// The whole file is checked before anything is written, so a bad line
/// leaves the board as it was, even with `--replace`.
#[test]
fn a_bad_file_names_its_line_and_changes_nothing() {
    let dir = board_of_every_kind();
    let text = fs::read_to_string(save(&dir)).unwrap();
    let line = text.lines().position(|line| line == "after|2").unwrap() + 1;
    fs::write(
        dir.path().join("bad.txt"),
        text.replacen("after|2", "after|3", 1),
    )
    .unwrap();
    // Cut between two tasks, so that every line left is whole.
    let cut = &text[..=text[..text.len() / 2].rfind("\n\n").unwrap()];
    fs::write(dir.path().join("cut.txt"), cut).unwrap();
    let before = rows(dir.path());

    assert_eq!(
        fail(&dir, &["load", "--replace", "bad.txt"]),
        format!("error: line {line}: task 3 is not in the file")
    );
    assert_eq!(
        fail(&dir, &["load", "--replace", "cut.txt"]),
        format!(
            "error: line {}: no `end` line: the file is cut short",
            cut.lines().count() + 1
        )
    );
    assert_eq!(rows(dir.path()), before);
}

#[test]
fn files_that_are_not_a_full_dump_are_refused() {
    let dir = board_of_every_kind();
    let reading = run(&dir, &["dump"]);
    fs::write(dir.path().join("reading.txt"), reading).unwrap();
    fs::write(dir.path().join("binary.txt"), [0xff, 0xfe, 0x00]).unwrap();
    let new = initialized();

    for (file, expected) in [
        (
            "reading.txt",
            "error: line 1: this is a `dump` for reading; load reads what `dump --full` writes",
        ),
        (
            "binary.txt",
            "error: cannot read binary.txt: stream did not contain valid UTF-8",
        ),
        (
            "missing.txt",
            "error: cannot read missing.txt: No such file or directory (os error 2)",
        ),
    ] {
        let file = dir.path().join(file);
        let expected = expected.replace(
            &format!("read {}", file.file_name().unwrap().to_str().unwrap()),
            &format!("read {}", path(&file)),
        );
        assert_eq!(fail(&new, &["load", path(&file)]), expected);
    }
    assert_eq!(
        run(&new, &["dump", "--full"]),
        "agent-kanban full dump 2\nnext|1\n\nend"
    );
}

#[test]
fn a_dash_reads_the_file_from_standard_input() {
    let old = board_of_every_kind();
    let text = fs::read(save(&old)).unwrap();
    let new = initialized();

    let output = kanban(&new)
        .args(["load", "-"])
        .write_stdin(text)
        .output()
        .unwrap();

    assert!(output.status.success(), "{output:?}");
    assert_eq!(output.stdout, b"loaded tasks 6, agents 3, mail 4\n");
    assert_eq!(rows(new.path()), rows(old.path()));
}

#[test]
fn load_and_dump_use_the_board_db_names() {
    let old = board_of_every_kind();
    let file = save(&old);
    let dir = project();
    run(&dir, &["--db", "elsewhere.db", "init"]);

    run(&dir, &["--db", "elsewhere.db", "load", path(&file)]);

    assert_eq!(
        run(&dir, &["--db", "elsewhere.db", "dump", "--full"]),
        run(&old, &["dump", "--full"])
    );
}

/// A developer who held a task when the board was dumped finishes it on the
/// restored board; a lease that had run out can be taken over.
#[test]
fn work_in_progress_carries_on_after_a_load() {
    let old = board_of_every_kind();
    let new = initialized();
    run(&new, &["load", path(&save(&old))]);
    let as_agent = |agent: &str, args: &[&str]| run(&new, &[args, &["--agent", agent]].concat());

    assert_eq!(
        as_agent(
            "dev",
            &[
                "submit-review",
                "2",
                "--pass",
                "0",
                "ok",
                "--pass",
                "1",
                "fixed"
            ]
        ),
        "#2 review rev2"
    );
    as_agent("rev", &["claim", "2"]);
    assert_eq!(as_agent("rev", &["approve", "2"]), "#2 done unblocked:4");
    assert!(as_agent("temp", &["claim", "6"]).starts_with("#6 "));
}

/// The board's counter never goes down: ids handed out after the dump was
/// made stay spent, so an id an agent still holds never names another task.
#[test]
fn ids_handed_out_after_the_dump_are_not_handed_out_again() {
    let dir = initialized();
    for title in ["a", "b", "c"] {
        add_task(&dir, title, "low");
    }
    let file = save(&dir);
    add_task(&dir, "d", "low");
    add_task(&dir, "e", "low");

    run(&dir, &["load", "--replace", path(&file)]);

    assert_eq!(fail(&dir, &["show", "4"]), "error: task 4 not found");
    assert_eq!(add_task(&dir, "f", "low"), 6);
}

/// The dump checks what it prints as `load` would, so a board edited by hand
/// into something `load` refuses is caught when it is dumped, not when the
/// file is needed back.
#[test]
fn a_board_that_load_would_refuse_is_not_dumped() {
    let dir = initialized();
    add_task(&dir, "a", "low");
    add_task(&dir, "b", "low");
    let sql = |sql: &str| common::db(&dir).execute_batch(sql).unwrap();

    sql("UPDATE tasks SET created_at = 'yesterday' WHERE id = 2");
    let message = fail(&dir, &["dump", "--full"]);
    assert!(
        message
            .starts_with("error: cannot dump: `load` would refuse `task|2|low|todo|0|yesterday|"),
        "{message}"
    );
    assert!(
        message.ends_with("|b`: 'yesterday' is not a time like 2026-01-31 23:59:00"),
        "{message}"
    );

    sql("UPDATE tasks SET created_at = updated_at;
         INSERT INTO task_deps (task_id, depends_on) VALUES (1, 2), (2, 1);");
    assert_eq!(
        fail(&dir, &["dump", "--full"]),
        "error: cannot dump: `load` would refuse `after|1`: task 2 cannot come after 1: 1 \
         already comes after 2 (cycle)"
    );

    sql("DELETE FROM task_deps; UPDATE tasks SET tags = '[1]' WHERE id = 1;");
    assert_eq!(
        fail(&dir, &["dump", "--full"]),
        "error: task 1 has tags that agent-kanban never writes ([1]); a full dump cannot keep it"
    );
    // The dump for reading still shows the board as it is.
    run(&dir, &["dump"]);
}

#[test]
fn a_board_from_an_older_version_round_trips() {
    let old = project();
    fs::create_dir(old.path().join(".kanban")).unwrap();
    let conn = rusqlite::Connection::open(common::db_path(&old)).unwrap();
    conn.execute_batch("PRAGMA journal_mode=WAL;").unwrap();
    conn.execute_batch(BOARD_V3).unwrap();
    drop(conn);
    let file = save(&old);
    let new = initialized();

    assert_eq!(
        run(&new, &["load", path(&file)]),
        "loaded tasks 5, agents 2"
    );

    assert_eq!(rows(new.path()), rows(old.path()));
}

/// `count` one-test tasks written straight into the table, so that a board
/// is big enough for a dump and a load to take a while.
fn filler(dir: &TempDir, count: i64, title: &str) {
    common::db(dir)
        .execute(
            "WITH RECURSIVE n(i) AS (SELECT 1 UNION ALL SELECT i + 1 FROM n WHERE i < ?1)
             INSERT INTO tasks (title, priority, tests)
             SELECT ?2 || i, 'low', '[{\"describe\":\"d\",\"input\":\"i\",\"output\":\"o\"}]'
             FROM n",
            (count, title),
        )
        .unwrap();
}

/// A dump racing a load that replaces the board sees the board before the
/// load or after it, never a mix of the two. The board is big and the file
/// small, so the load commits while the dump is still reading; read outside
/// one transaction, the dump would end with the new board's counter after
/// the old board's tasks.
#[test]
fn a_dump_racing_a_load_sees_one_board_or_the_other() {
    const ROUNDS: usize = 8;
    let source = board_of_every_kind();
    let file = save(&source);
    // The board keeps its own, higher counter.
    let after = run(&source, &["dump", "--full"]).replacen("next|8", "next|2001", 1);

    for round in 0..ROUNDS {
        let dir = initialized();
        filler(&dir, 2000, "old ");
        let before = run(&dir, &["dump", "--full"]);

        let outcomes = race(
            &dir,
            &[
                argv(&["dump", "--full"]),
                argv(&["load", "--replace", path(&file)]),
            ],
        );

        assert!(outcomes[1].ok(), "round {round}: {:?}", outcomes[1]);
        let dump = &outcomes[0];
        assert!(dump.ok(), "round {round}: {dump:?}");
        assert!(
            dump.stdout == before || dump.stdout == after,
            "round {round}: a mix of two boards"
        );
        assert_eq!(run(&dir, &["dump", "--full"]), after, "round {round}");
    }
}

/// A load and an `add` on an empty board: either the load came first and the
/// new task gets the next id after the file's, or the task came first and the
/// load finds the board no longer empty.
#[test]
fn a_load_racing_an_add_never_mixes_them() {
    const ROUNDS: usize = 8;
    let source = board_of_every_kind();
    let file = save(&source);

    for round in 0..ROUNDS {
        let dir = initialized();

        let outcomes = race(
            &dir,
            &[
                argv(&["load", path(&file)]),
                argv(&["add", "--title", "racer", "--test", "d", "i", "o"]),
            ],
        );

        let (load, add) = (&outcomes[0], &outcomes[1]);
        assert!(add.ok(), "round {round}: {add:?}");
        if load.ok() {
            assert_eq!(add.stdout, "#8 todo", "round {round}");
        } else {
            assert_eq!(add.stdout, "#1 todo", "round {round}");
            assert_eq!(
                load.stderr,
                "error: the board is not empty (tasks 1, agents 0); `load --replace FILE` \
                 replaces it",
                "round {round}"
            );
        }
    }
}
