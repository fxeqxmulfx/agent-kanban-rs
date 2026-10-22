//! `dump`: the whole board in one reply, for an agent to read or to save in a
//! file. Every task, done ones too, in id order, each exactly as `show ID
//! --history` prints it, a blank line apart. It is read in one transaction,
//! so it shows the board at one moment even while other agents change it.

mod common;

use common::{add_task, add_task_after, argv, id_of, initialized, parse_show, race, register, run};
use std::path::Path;

/// The tasks of a dump, split at the blank lines between them.
fn blocks(dump: &str) -> Vec<&str> {
    dump.split("\n\n").collect()
}

/// What `show ID --history` prints for every task `list --all` knows, in id
/// order, a blank line apart: what a dump must equal.
fn shows(dir: &Path) -> String {
    let mut ids: Vec<i64> = run(dir, &["list", "--all", "--limit", "0"])
        .lines()
        .map(id_of)
        .collect();
    ids.sort_unstable();
    ids.iter()
        .map(|id| run(dir, &["show", &id.to_string(), "--history"]))
        .collect::<Vec<_>>()
        .join("\n\n")
}

/// `count` one-test tasks written straight into the table, so that a board is
/// big enough for a dump to take a while.
fn filler(dir: &Path, count: usize) {
    common::db(dir)
        .execute(
            "WITH RECURSIVE n(i) AS (SELECT 1 UNION ALL SELECT i + 1 FROM n WHERE i < ?1)
             INSERT INTO tasks (title, priority, tests)
             SELECT 'filler ' || i, 'low', '[{\"describe\":\"d\",\"input\":\"i\",\"output\":\"o\"}]'
             FROM n",
            [i64::try_from(count).unwrap()],
        )
        .unwrap();
}

#[test]
fn an_empty_board_dumps_as_no_tasks() {
    let dir = initialized();

    assert_eq!(run(&dir, &["dump"]), "no tasks");
}

/// A done task with its history, a task sent back for changes, one waiting
/// for it, a parked one, and a removed #3.
fn board_of_every_kind() -> tempfile::TempDir {
    let dir = initialized();
    register(&dir, "dev", "developer");
    register(&dir, "rev", "reviewer");
    add_task(&dir, "Base", "medium");
    run(
        &dir,
        &[
            "add",
            "--title",
            "Feature X",
            "--priority",
            "high",
            "--tag",
            "api",
            "--tag",
            "needs review",
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
    run(&dir, &["remove", "3"]);
    let dev = |args: &[&str]| run(&dir, &[args, &["--agent", "dev"]].concat());
    let rev = |args: &[&str]| run(&dir, &[args, &["--agent", "rev"]].concat());
    dev(&["claim", "1"]);
    dev(&["submit-review", "1", "--pass", "0", "ok"]);
    rev(&["claim", "1"]);
    rev(&["approve", "1", "--notes", "good"]);
    dev(&["claim", "2"]);
    dev(&[
        "submit-review",
        "2",
        "--pass",
        "0",
        "adds: ok",
        "--fail",
        "1",
        "panics",
    ]);
    rev(&["claim", "2"]);
    rev(&["request-changes", "2", "--notes", "handle division by zero"]);
    dev(&["claim", "2"]);
    dir
}

/// The exact text. The removed #3 leaves no trace.
#[test]
fn dump_prints_every_task_as_show_history_a_blank_line_apart() {
    let dir = board_of_every_kind();

    let dump = run(&dir, &["dump"]);

    assert_eq!(
        dump,
        "#1 medium done rev1 Base\n\
         0|works|in|out|passed|ok\n\
         rev1|dev|passed: ok\n\
         review|rev|approved|good\n\
         \n\
         #2 high in_progress@dev rev1 blocks:4 Feature X\n\
         tags|api|needs review\n\
         changes|handle division by zero\n\
         0|adds|1+1|2\n\
         1|fails cleanly|1/0|error\n\
         rev1|dev|passed: adds: ok|failed: panics\n\
         review|rev|changes_requested|handle division by zero\n\
         \n\
         #4 medium todo after:2 Follow-up\n\
         0|works|in|out\n\
         \n\
         #5 low backlog Someday\n\
         0|works|in|out"
    );
    assert_eq!(dump, shows(dir.path()));
}

/// Line breaks inside free text are escaped, so no text can fake the blank
/// line between two tasks, or a task header after it.
#[test]
fn free_text_cannot_fake_a_task_boundary() {
    let dir = initialized();
    register(&dir, "dev", "developer");
    register(&dir, "rev", "reviewer");
    let fake = "x\n\n#9 urgent todo fake\n0|a|b|c\n";
    run(
        &dir,
        &[
            "add", "--title", "real", "--tag", fake, "--test", fake, fake, fake,
        ],
    );
    run(&dir, &["claim", "1", "--agent", "dev"]);
    run(
        &dir,
        &["submit-review", "1", "--agent", "dev", "--pass", "0", fake],
    );
    run(&dir, &["claim", "1", "--agent", "rev"]);
    run(
        &dir,
        &["request-changes", "1", "--agent", "rev", "--notes", fake],
    );
    run(&dir, &["add", "--title", "next", "--test", "d", "i", "o"]);

    let dump = run(&dir, &["dump"]);

    let tasks = blocks(&dump);
    assert_eq!(tasks.len(), 2, "{dump}");
    assert_eq!(dump.lines().filter(|line| line.starts_with('#')).count(), 2);
    assert_eq!(dump, shows(dir.path()));
    let first = parse_show(tasks[0]);
    assert_eq!(first["tags"][0], fake);
    assert_eq!(first["changes"], fake.trim());
    assert_eq!(first["tests"][0]["input"], fake);
    assert_eq!(first["history"][0]["notes"], fake.trim());
    assert_eq!(parse_show(tasks[1])["title"], "next");
}

/// Removals racing a dump. Read task by task outside one transaction, a task
/// removed after the dump had listed the ids would come out as "task N not
/// found" and fail the whole dump.
#[test]
fn a_task_removed_during_a_dump_is_in_it_whole_or_not_at_all() {
    const ROUNDS: usize = 8;
    let dir = initialized();
    filler(dir.path(), 300);

    for round in 0..ROUNDS {
        // The newest tasks are the ones read last.
        let doomed: Vec<i64> = (0..4).map(|_| add_task(&dir, "doomed", "low")).collect();
        let before = run(&dir, &["dump"]);
        let mut commands = vec![argv(&["dump"])];
        commands.extend(doomed.iter().map(|id| argv(&["remove", &id.to_string()])));

        let outcomes = race(&dir, &commands);

        for outcome in &outcomes[1..] {
            assert!(outcome.ok(), "round {round}: {outcome:?}");
        }
        let dump = &outcomes[0];
        assert!(dump.ok(), "round {round}: {dump:?}");
        // Every task in it is exactly what it was before, and every task that
        // was not removed is in it.
        let old = blocks(&before);
        let new = blocks(&dump.stdout);
        for task in &new {
            assert!(old.contains(task), "round {round}: {task}");
        }
        for task in old {
            assert!(
                new.contains(&task) || doomed.contains(&id_of(task)),
                "round {round}: lost {task}"
            );
        }
    }
}

/// Approvals racing a dump. Each approved task has a dependent that waits for
/// it, with three hundred tasks between them. A dump that read them at
/// different moments could show the task in review and its dependent no
/// longer waiting, or the task done and its dependent still waiting.
#[test]
fn a_dump_never_mixes_the_board_before_and_after_an_approval() {
    const ROUNDS: usize = 8;
    const PAIRS: usize = 3;
    for round in 0..ROUNDS {
        let dir = initialized();
        register(&dir, "dev", "developer");
        register(&dir, "rev", "reviewer");
        let approved: Vec<i64> = (0..PAIRS)
            .map(|_| add_task(&dir, "approved", "high"))
            .collect();
        filler(dir.path(), 300);
        let dependents: Vec<i64> = approved
            .iter()
            .map(|id| add_task_after(&dir, "dependent", &[*id]))
            .collect();
        for id in &approved {
            let id = id.to_string();
            run(&dir, &["claim", &id, "--agent", "dev"]);
            run(
                &dir,
                &["submit-review", &id, "--agent", "dev", "--pass", "0", "ok"],
            );
            run(&dir, &["claim", &id, "--agent", "rev"]);
        }
        let mut commands = vec![argv(&["dump"])];
        commands.extend(
            approved
                .iter()
                .map(|id| argv(&["approve", &id.to_string(), "--agent", "rev"])),
        );

        let outcomes = race(&dir, &commands);

        for outcome in &outcomes[1..] {
            assert!(outcome.ok(), "round {round}: {outcome:?}");
        }
        let dump = &outcomes[0];
        assert!(dump.ok(), "round {round}: {dump:?}");
        let tasks: Vec<serde_json::Value> =
            blocks(&dump.stdout).into_iter().map(parse_show).collect();
        let task = |id: i64| {
            tasks
                .iter()
                .find(|task| task["id"] == id)
                .unwrap_or_else(|| panic!("round {round}: #{id} missing"))
        };
        for (&id, &dependent) in approved.iter().zip(&dependents) {
            let done = task(id)["status"] == "done";
            let waits = task(dependent)
                .get("after")
                .is_some_and(|after| after == &serde_json::json!([id]));
            let blocking = task(id)
                .get("blocks")
                .is_some_and(|blocks| blocks == &serde_json::json!([dependent]));
            assert_eq!(
                (waits, blocking),
                (!done, !done),
                "round {round}: #{id} done={done}, #{dependent} waits={waits}, \
                 #{id} blocks={blocking}"
            );
        }
    }
}
