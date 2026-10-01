//! Task dependencies: tasks form a DAG. `--after` declares what a task waits
//! for, developers cannot claim blocked work, cycles are refused, and `approve`
//! reports what it unblocked. Everything runs through the compiled binary.

mod common;

use common::{add_task, add_task_after, argv, db, fail, initialized, race, register, run};
use tempfile::TempDir;

const ROUNDS: usize = 20;

/// An initialized board with one developer (`dev`) and one reviewer (`rev`).
fn board() -> TempDir {
    let dir = initialized();
    register(&dir, "dev", "developer");
    register(&dir, "rev", "reviewer");
    dir
}

/// Take task `id` through development and review; the reply of `approve`.
fn finish(dir: &TempDir, id: i64) -> String {
    let id = id.to_string();
    run(dir, &["claim", &id, "--agent", "dev"]);
    run(
        dir,
        &["submit-review", &id, "--agent", "dev", "--pass", "0", "ok"],
    );
    run(dir, &["claim", &id, "--agent", "rev"]);
    run(dir, &["approve", &id, "--agent", "rev"])
}

/// The counts line of `status`, without the agents line.
fn counts(dir: &TempDir) -> String {
    run(dir, &["status"]).lines().next().unwrap().to_string()
}

/// Every `(task, prerequisite)` row, read straight from the database.
fn edges(dir: &TempDir) -> Vec<(i64, i64)> {
    let conn = db(dir);
    let mut stmt = conn
        .prepare("SELECT task_id, depends_on FROM task_deps ORDER BY 1, 2")
        .unwrap();
    stmt.query_map([], |row| Ok((row.get(0)?, row.get(1)?)))
        .unwrap()
        .map(Result::unwrap)
        .collect()
}

/// The graph in the database has no cycle and no edge to a missing task.
fn assert_sound(dir: &TempDir) {
    let conn = db(dir);
    let cyclic: i64 = conn
        .query_row(
            "WITH RECURSIVE reach(origin, id) AS (
               SELECT task_id, depends_on FROM task_deps
               UNION
               SELECT r.origin, d.depends_on FROM reach r JOIN task_deps d ON d.task_id = r.id
             )
             SELECT COUNT(*) FROM reach WHERE origin = id",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(cyclic, 0, "the dependency graph has a cycle");
    let mut stmt = conn.prepare("PRAGMA foreign_key_check").unwrap();
    let dangling = stmt.query([]).unwrap().mapped(|_| Ok(())).count();
    assert_eq!(dangling, 0, "an edge points at a missing task");
}

// ---------------------------------------------------------------------------
// Declaring prerequisites
// ---------------------------------------------------------------------------

#[test]
fn prerequisites_are_listed_ascending_without_duplicates_in_either_flag_form() {
    let dir = board();
    for title in ["a", "b", "c"] {
        add_task(&dir, title, "medium");
    }
    let add = |title: &str, after: &[&str]| {
        let mut args = vec!["add", "--title", title, "--test", "d", "i", "o"];
        for ids in after {
            args.extend(["--after", ids]);
        }
        run(&dir, &args)
    };

    assert_eq!(add("x", &["3,1,2"]), "#4 todo after:1,2,3");
    assert_eq!(add("y", &["2", "1"]), "#5 todo after:1,2");
    assert_eq!(add("z", &["1,1", "1"]), "#6 todo after:1");

    assert_eq!(
        edges(&dir),
        [(4, 1), (4, 2), (4, 3), (5, 1), (5, 2), (6, 1)]
    );
}

#[test]
fn an_unknown_prerequisite_creates_nothing() {
    let dir = board();
    add_task(&dir, "a", "medium");

    assert_eq!(
        fail(
            &dir,
            &[
                "add", "--title", "x", "--test", "d", "i", "o", "--after", "1,99"
            ]
        ),
        "error: task 99 not found"
    );

    assert_eq!(run(&dir, &["list"]), "#1 medium todo a");
    assert!(edges(&dir).is_empty());
    // The id the failed add would have used is still free.
    assert_eq!(add_task(&dir, "b", "medium"), 2);
}

#[test]
fn edit_adds_and_drops_single_prerequisites() {
    let dir = board();
    for title in ["a", "b", "c"] {
        add_task(&dir, title, "medium");
    }

    assert_eq!(run(&dir, &["edit", "3", "--after", "1"]), "#3 todo after:1");
    // --after adds to what is there, it does not replace it.
    assert_eq!(
        run(&dir, &["edit", "3", "--after", "2"]),
        "#3 todo after:1,2"
    );
    assert_eq!(
        run(&dir, &["edit", "3", "--drop-after", "1"]),
        "#3 todo after:2"
    );
    assert_eq!(
        run(&dir, &["edit", "3", "--after", "1", "--drop-after", "2"]),
        "#3 todo after:1"
    );
    assert_eq!(run(&dir, &["edit", "3", "--drop-after", "1"]), "#3 todo");
    assert!(edges(&dir).is_empty());
}

#[test]
fn edit_refuses_to_drop_an_edge_that_is_not_there_or_add_an_unknown_task() {
    let dir = board();
    add_task(&dir, "a", "medium");
    add_task(&dir, "b", "medium");

    assert_eq!(
        fail(&dir, &["edit", "2", "--drop-after", "1"]),
        "error: task 2 does not come after 1"
    );
    assert_eq!(
        fail(&dir, &["edit", "2", "--after", "99"]),
        "error: task 99 not found"
    );
    assert!(edges(&dir).is_empty());
}

#[test]
fn a_waiting_task_can_still_be_edited_and_parked() {
    let dir = board();
    add_task(&dir, "a", "medium");
    add_task_after(&dir, "b", &[1]);

    assert_eq!(
        run(
            &dir,
            &["edit", "2", "--title", "renamed", "--priority", "high"]
        ),
        "#2 todo after:1"
    );
    assert_eq!(run(&dir, &["move", "2", "backlog"]), "#2 backlog");
    assert_eq!(
        run(&dir, &["edit", "2", "--tag", "x"]),
        "#2 backlog after:1"
    );
    assert_eq!(
        run(&dir, &["list"]),
        "#1 medium todo a\n#2 high backlog after:1 renamed"
    );
}

// ---------------------------------------------------------------------------
// Cycles
// ---------------------------------------------------------------------------

#[test]
fn cycles_are_refused_whatever_their_length() {
    let dir = board();
    add_task(&dir, "t1", "medium");
    for n in 2..=6 {
        add_task_after(&dir, &format!("t{n}"), &[n - 1]);
    }
    let before = edges(&dir);

    assert_eq!(
        fail(&dir, &["edit", "1", "--after", "1"]),
        "error: task 1 cannot come after itself"
    );
    assert_eq!(
        fail(&dir, &["edit", "1", "--after", "2"]),
        "error: task 1 cannot come after 2: 2 already comes after 1 (cycle)"
    );
    assert_eq!(
        fail(&dir, &["edit", "1", "--after", "6"]),
        "error: task 1 cannot come after 6: 6 already comes after 1 (cycle)"
    );
    assert_eq!(
        fail(&dir, &["edit", "3", "--after", "6"]),
        "error: task 3 cannot come after 6: 6 already comes after 3 (cycle)"
    );

    assert_eq!(edges(&dir), before);
    assert_sound(&dir);
}

#[test]
fn shortcuts_and_diamonds_are_not_cycles() {
    let dir = board();
    add_task(&dir, "t1", "medium");
    add_task_after(&dir, "t2", &[1]);
    add_task_after(&dir, "t3", &[1]);

    assert_eq!(
        run(
            &dir,
            &[
                "add", "--title", "join", "--test", "d", "i", "o", "--after", "2,3"
            ]
        ),
        "#4 todo after:2,3"
    );
    // A shortcut across a path that already exists.
    assert_eq!(
        run(&dir, &["edit", "4", "--after", "1"]),
        "#4 todo after:1,2,3"
    );
    assert_sound(&dir);
}

#[test]
fn a_refused_edit_applies_none_of_its_changes() {
    let dir = board();
    for title in ["a", "b", "c"] {
        add_task(&dir, title, "medium");
    }
    run(&dir, &["edit", "3", "--after", "1"]);
    let shown = run(&dir, &["show", "1"]);
    let before = edges(&dir);

    // `--after 2` is fine and is applied first; `--after 3` closes a cycle.
    assert_eq!(
        fail(
            &dir,
            &[
                "edit",
                "1",
                "--title",
                "new",
                "--priority",
                "urgent",
                "--tag",
                "t",
                "--after",
                "2",
                "--after",
                "3",
            ]
        ),
        "error: task 1 cannot come after 3: 3 already comes after 1 (cycle)"
    );

    assert_eq!(run(&dir, &["show", "1"]), shown);
    assert_eq!(edges(&dir), before);
}

// ---------------------------------------------------------------------------
// Gating work
// ---------------------------------------------------------------------------

#[test]
fn developers_cannot_claim_a_task_until_all_its_prerequisites_are_done() {
    let dir = board();
    let a = add_task(&dir, "a", "medium");
    let b = add_task(&dir, "b", "medium");
    add_task_after(&dir, "c", &[a, b]);

    assert_eq!(
        fail(&dir, &["claim", "3", "--agent", "dev"]),
        "error: task 3 is blocked by unfinished tasks 1,2"
    );
    finish(&dir, a);
    assert_eq!(
        fail(&dir, &["claim", "3", "--agent", "dev"]),
        "error: task 3 is blocked by unfinished tasks 2"
    );
    finish(&dir, b);

    let order = common::task(&dir, &["claim", "3", "--agent", "dev"]);
    assert_eq!(order["id"], 3);
}

/// A prerequisite counts only once it is *done*: being claimed, submitted or
/// under review is not enough.
#[test]
fn a_prerequisite_that_is_claimed_or_in_review_still_blocks() {
    let dir = board();
    register(&dir, "dev2", "developer");
    let a = add_task(&dir, "a", "medium");
    add_task_after(&dir, "b", &[a]);
    let blocked = "error: task 2 is blocked by unfinished tasks 1";

    run(&dir, &["claim", "1", "--agent", "dev"]);
    assert_eq!(fail(&dir, &["claim", "2", "--agent", "dev2"]), blocked);
    run(
        &dir,
        &["submit-review", "1", "--agent", "dev", "--pass", "0", "ok"],
    );
    assert_eq!(fail(&dir, &["claim", "2", "--agent", "dev2"]), blocked);
    run(&dir, &["claim", "1", "--agent", "rev"]);
    assert_eq!(fail(&dir, &["claim", "2", "--agent", "dev2"]), blocked);
    assert_eq!(
        run(&dir, &["approve", "1", "--agent", "rev"]),
        "#1 done unblocked:2"
    );

    assert_eq!(
        common::task(&dir, &["claim", "2", "--agent", "dev2"])["id"],
        2
    );
}

#[test]
fn a_parked_prerequisite_blocks_like_any_other_unfinished_task() {
    let dir = board();
    add_task(&dir, "a", "medium");
    add_task_after(&dir, "b", &[1]);
    run(&dir, &["move", "1", "backlog"]);

    assert_eq!(
        fail(&dir, &["claim", "2", "--agent", "dev"]),
        "error: task 2 is blocked by unfinished tasks 1"
    );
    assert_eq!(
        fail(&dir, &["claim", "1", "--agent", "dev"]),
        "error: task 1 is backlog; claim needs todo or in_progress"
    );
    run(&dir, &["move", "1", "todo"]);
    finish(&dir, 1);
    assert_eq!(
        common::task(&dir, &["claim", "2", "--agent", "dev"])["id"],
        2
    );
}

#[test]
fn parking_a_waiting_task_keeps_its_prerequisites() {
    let dir = board();
    add_task(&dir, "a", "medium");
    add_task_after(&dir, "b", &[1]);

    assert_eq!(run(&dir, &["move", "2", "backlog"]), "#2 backlog");
    assert_eq!(run(&dir, &["move", "2", "todo"]), "#2 todo");

    assert_eq!(
        common::task(&dir, &["show", "2"])["after"],
        serde_json::json!([1])
    );
    assert_eq!(
        fail(&dir, &["claim", "2", "--agent", "dev"]),
        "error: task 2 is blocked by unfinished tasks 1"
    );
}

#[test]
fn claim_next_never_picks_blocked_work_however_urgent() {
    let dir = board();
    register(&dir, "dev2", "developer");
    add_task(&dir, "prerequisite", "low");
    assert_eq!(
        run(
            &dir,
            &[
                "add",
                "--title",
                "urgent but waiting",
                "--priority",
                "urgent",
                "--test",
                "d",
                "i",
                "o",
                "--after",
                "1",
            ]
        ),
        "#2 todo after:1"
    );

    let first = common::task(&dir, &["claim-next", "--agent", "dev"]);
    assert_eq!(first["id"], 1);
    // Task 1 is held and task 2 waits for it: nothing for the second developer.
    assert_eq!(
        run(&dir, &["claim-next", "--agent", "dev2"]),
        "idle, 2 open"
    );

    finish(&dir, 1);
    let second = common::task(&dir, &["claim-next", "--agent", "dev2"]);
    assert_eq!(second["id"], 2);
    assert_eq!(run(&dir, &["claim-next", "--agent", "dev"]), "idle, 1 open");
}

#[test]
fn a_task_sent_back_for_rework_can_be_made_to_wait() {
    let dir = board();
    add_task(&dir, "a", "medium");
    add_task(&dir, "b", "medium");
    run(&dir, &["claim", "2", "--agent", "dev"]);
    run(
        &dir,
        &["submit-review", "2", "--agent", "dev", "--pass", "0", "ok"],
    );
    run(&dir, &["claim", "2", "--agent", "rev"]);
    run(
        &dir,
        &[
            "request-changes",
            "2",
            "--agent",
            "rev",
            "--notes",
            "fix it",
        ],
    );

    assert_eq!(
        run(&dir, &["edit", "2", "--after", "1"]),
        "#2 in_progress after:1"
    );
    assert_eq!(
        fail(&dir, &["claim", "2", "--agent", "dev"]),
        "error: task 2 is blocked by unfinished tasks 1"
    );
    // Rework normally comes first, but not while it waits.
    assert_eq!(
        common::task(&dir, &["claim-next", "--agent", "dev"])["id"],
        1
    );
    assert_eq!(finish(&dir, 1), "#1 done unblocked:2");

    let order = common::task(&dir, &["claim", "2", "--agent", "dev"]);
    assert_eq!(order["changes"], "fix it");
}

// ---------------------------------------------------------------------------
// Reporting
// ---------------------------------------------------------------------------

#[test]
fn approve_reports_exactly_the_tasks_it_unblocked() {
    let dir = board();
    add_task(&dir, "root", "medium");
    add_task_after(&dir, "left", &[1]);
    add_task_after(&dir, "right", &[1]);
    add_task_after(&dir, "join", &[2, 3]);
    add_task_after(&dir, "parked", &[1]);
    run(&dir, &["move", "5", "backlog"]);

    // `parked` waits for the root too, but it is not claimable anyway.
    assert_eq!(finish(&dir, 1), "#1 done unblocked:2,3");
    // `join` still waits for `right`.
    assert_eq!(finish(&dir, 2), "#2 done");
    assert_eq!(finish(&dir, 3), "#3 done unblocked:4");
    assert_eq!(finish(&dir, 4), "#4 done");
    run(&dir, &["move", "5", "todo"]);
    assert_eq!(
        common::task(&dir, &["claim", "5", "--agent", "dev"])["id"],
        5
    );
}

#[test]
fn list_show_and_status_agree_on_who_is_waiting() {
    let dir = board();
    add_task(&dir, "t1", "medium");
    add_task_after(&dir, "t2", &[1]);
    add_task_after(&dir, "t3", &[1, 2]);
    let after = |id: &str| common::task(&dir, &["show", id]).get("after").cloned();
    let blocks = |id: &str| common::task(&dir, &["show", id]).get("blocks").cloned();

    assert_eq!(
        run(&dir, &["list"]),
        "#1 medium todo t1\n#2 medium todo after:1 t2\n#3 medium todo after:1,2 t3"
    );
    assert_eq!(blocks("1"), Some(serde_json::json!([2, 3])));
    assert_eq!(after("2"), Some(serde_json::json!([1])));
    assert_eq!(blocks("2"), Some(serde_json::json!([3])));
    assert_eq!(after("3"), Some(serde_json::json!([1, 2])));
    assert_eq!(
        counts(&dir),
        "backlog 0, todo 3, in_progress 0, review 0, done 0; blocked 2"
    );

    finish(&dir, 1);

    // A finished prerequisite is no longer anything to wait for.
    assert_eq!(
        run(&dir, &["list"]),
        "#2 medium todo t2\n#3 medium todo after:2 t3\n+1 done hidden (--all)"
    );
    assert_eq!(after("2"), None);
    assert_eq!(after("3"), Some(serde_json::json!([2])));
    assert_eq!(blocks("1"), None);
    assert_eq!(blocks("2"), Some(serde_json::json!([3])));
    assert_eq!(
        counts(&dir),
        "backlog 0, todo 2, in_progress 0, review 0, done 1; blocked 1"
    );

    finish(&dir, 2);

    assert_eq!(
        run(&dir, &["list", "--all"]),
        "#3 medium todo t3\n#1 medium done t1\n#2 medium done t2"
    );
    assert_eq!(
        counts(&dir),
        "backlog 0, todo 1, in_progress 0, review 0, done 2"
    );
}

#[test]
fn status_counts_blocked_work_but_not_parked_tasks() {
    let dir = board();
    for title in ["a", "b", "c", "d"] {
        add_task(&dir, title, "medium");
    }
    // 4 goes through review and comes back for rework.
    run(&dir, &["claim", "4", "--agent", "dev"]);
    run(
        &dir,
        &["submit-review", "4", "--agent", "dev", "--pass", "0", "ok"],
    );
    run(&dir, &["claim", "4", "--agent", "rev"]);
    run(
        &dir,
        &["request-changes", "4", "--agent", "rev", "--notes", "again"],
    );
    run(&dir, &["edit", "2", "--after", "1"]);
    run(&dir, &["edit", "3", "--after", "1"]);
    run(&dir, &["move", "3", "backlog"]);
    run(&dir, &["edit", "4", "--after", "1"]);

    // 2 (todo) and 4 (rework) are blocked; 3 is parked.
    assert_eq!(
        counts(&dir),
        "backlog 1, todo 2, in_progress 1, review 0, done 0; blocked 2"
    );

    finish(&dir, 1);

    assert_eq!(
        counts(&dir),
        "backlog 1, todo 1, in_progress 1, review 0, done 1"
    );
}

// ---------------------------------------------------------------------------
// Removing tasks that take part in the graph
// ---------------------------------------------------------------------------

#[test]
fn a_prerequisite_cannot_be_removed_while_others_wait_for_it() {
    let dir = board();
    add_task(&dir, "a", "medium");
    add_task_after(&dir, "b", &[1]);
    add_task_after(&dir, "c", &[1]);

    assert_eq!(
        fail(&dir, &["remove", "1"]),
        "error: task 1 is a prerequisite of 2,3; remove those tasks or detach them \
         (edit --drop-after 1) first"
    );
    run(&dir, &["edit", "2", "--drop-after", "1"]);
    assert_eq!(
        fail(&dir, &["remove", "1"]),
        "error: task 1 is a prerequisite of 3; remove those tasks or detach them \
         (edit --drop-after 1) first"
    );
    run(&dir, &["edit", "3", "--drop-after", "1"]);

    assert_eq!(run(&dir, &["remove", "1"]), "#1 removed");
}

#[test]
fn removing_a_waiting_task_frees_its_prerequisites_and_leaves_no_edges() {
    let dir = board();
    add_task(&dir, "a", "medium");
    add_task_after(&dir, "b", &[1]);
    add_task_after(&dir, "c", &[2]);

    assert_eq!(run(&dir, &["remove", "3"]), "#3 removed");

    assert_eq!(edges(&dir), [(2, 1)]);
    assert!(common::task(&dir, &["show", "2"]).get("blocks").is_none());
    assert_eq!(run(&dir, &["remove", "2"]), "#2 removed");
    assert!(edges(&dir).is_empty());
    assert_eq!(run(&dir, &["remove", "1"]), "#1 removed");
    assert_eq!(run(&dir, &["list"]), "no tasks");
    assert_sound(&dir);
}

// ---------------------------------------------------------------------------
// Concurrency: the cycle check and the write are one atomic step
// ---------------------------------------------------------------------------

#[test]
fn opposing_edits_never_close_a_cycle() {
    let dir = board();

    for round in 0..ROUNDS {
        let a = add_task(&dir, &format!("a{round}"), "medium").to_string();
        let b = add_task(&dir, &format!("b{round}"), "medium").to_string();

        let outcomes = race(
            &dir,
            &[
                argv(&["edit", &a, "--after", &b]),
                argv(&["edit", &b, "--after", &a]),
            ],
        );

        let winners = outcomes.iter().filter(|outcome| outcome.ok()).count();
        assert_eq!(winners, 1, "round {round}: {outcomes:?}");
        let loser = outcomes.iter().find(|outcome| !outcome.ok()).unwrap();
        assert_eq!(loser.code, Some(1), "round {round}: {loser:?}");
        assert!(
            loser.stderr.contains("(cycle)"),
            "round {round}: {}",
            loser.stderr
        );
    }
    assert_sound(&dir);
}

#[test]
fn a_ring_of_edits_always_loses_exactly_one_edge() {
    let dir = board();

    for round in 0..ROUNDS {
        let ids: Vec<String> = (0..3)
            .map(|n| add_task(&dir, &format!("ring{round}-{n}"), "medium").to_string())
            .collect();

        // 0 after 1, 1 after 2, 2 after 0: whichever lands last closes the ring.
        let outcomes = race(
            &dir,
            &[
                argv(&["edit", &ids[0], "--after", &ids[1]]),
                argv(&["edit", &ids[1], "--after", &ids[2]]),
                argv(&["edit", &ids[2], "--after", &ids[0]]),
            ],
        );

        let refused: Vec<_> = outcomes.iter().filter(|outcome| !outcome.ok()).collect();
        assert_eq!(refused.len(), 1, "round {round}: {outcomes:?}");
        assert!(
            refused[0].stderr.contains("(cycle)"),
            "round {round}: {}",
            refused[0].stderr
        );
    }
    assert_sound(&dir);
}

/// Adding a dependent and removing its prerequisite exclude each other: either
/// the dependent exists and the removal is refused, or the prerequisite is gone
/// and the new task was never created.
#[test]
fn adding_a_dependent_racing_the_removal_of_its_prerequisite() {
    let dir = board();

    for round in 0..ROUNDS {
        let prerequisite = add_task(&dir, &format!("prerequisite {round}"), "medium").to_string();
        // Ids of removed top tasks are handed out again; a task above the
        // prerequisite keeps the new one from taking over its id.
        add_task(&dir, &format!("spacer {round}"), "medium");

        let outcomes = race(
            &dir,
            &[
                argv(&[
                    "add",
                    "--title",
                    "dependent",
                    "--test",
                    "d",
                    "i",
                    "o",
                    "--after",
                    &prerequisite,
                ]),
                argv(&["remove", &prerequisite]),
            ],
        );
        let (add, remove) = (&outcomes[0], &outcomes[1]);

        assert_ne!(add.ok(), remove.ok(), "round {round}: {outcomes:?}");
        if add.ok() {
            let dependent = common::id_of(&add.stdout);
            assert_eq!(
                remove.stderr,
                format!(
                    "error: task {prerequisite} is a prerequisite of {dependent}; remove those \
                     tasks or detach them (edit --drop-after {prerequisite}) first"
                ),
                "round {round}"
            );
        } else {
            assert_eq!(
                add.stderr,
                format!("error: task {prerequisite} not found"),
                "round {round}"
            );
            assert_eq!(remove.stdout, format!("#{prerequisite} removed"));
        }
    }
    assert_sound(&dir);
}

/// A claim and a new prerequisite on the same task: readiness is decided inside
/// the claim itself, so blocked work can never be started.
#[test]
fn claim_racing_a_new_prerequisite_never_starts_blocked_work() {
    let dir = board();

    for round in 0..ROUNDS {
        let prerequisite = add_task(&dir, &format!("prerequisite {round}"), "low").to_string();
        let task = add_task(&dir, &format!("task {round}"), "medium").to_string();

        let outcomes = race(
            &dir,
            &[
                argv(&["claim", &task, "--agent", "dev"]),
                argv(&["edit", &task, "--after", &prerequisite]),
            ],
        );
        let (claim, edit) = (&outcomes[0], &outcomes[1]);

        assert_ne!(claim.ok(), edit.ok(), "round {round}: {outcomes:?}");
        let shown = common::task(&dir, &["show", &task]);
        if claim.ok() {
            assert_eq!(shown["executor"], "dev", "round {round}");
            assert!(shown.get("after").is_none(), "round {round}: {shown}");
            assert_eq!(
                edit.stderr,
                format!("error: task {task} is claimed; release it before editing"),
                "round {round}"
            );
        } else {
            assert_eq!(
                shown["after"],
                serde_json::json!([prerequisite.parse::<i64>().unwrap()])
            );
            assert!(shown.get("executor").is_none(), "round {round}: {shown}");
            assert_eq!(
                claim.stderr,
                format!("error: task {task} is blocked by unfinished tasks {prerequisite}"),
                "round {round}"
            );
        }
    }
}

// ---------------------------------------------------------------------------
// Larger graphs
// ---------------------------------------------------------------------------

#[test]
fn a_long_chain_unblocks_one_task_at_a_time_and_refuses_to_close_the_loop() {
    const LENGTH: i64 = 25;
    let dir = board();
    add_task(&dir, "step 1", "medium");
    for step in 2..=LENGTH {
        add_task_after(&dir, &format!("step {step}"), &[step - 1]);
    }

    assert_eq!(
        fail(&dir, &["edit", "1", "--after", &LENGTH.to_string()]),
        format!("error: task 1 cannot come after {LENGTH}: {LENGTH} already comes after 1 (cycle)")
    );
    assert_eq!(
        counts(&dir),
        format!(
            "backlog 0, todo {LENGTH}, in_progress 0, review 0, done 0; blocked {}",
            LENGTH - 1
        )
    );
    for step in 1..LENGTH {
        assert_eq!(
            finish(&dir, step),
            format!("#{step} done unblocked:{}", step + 1)
        );
    }
    assert_eq!(finish(&dir, LENGTH), format!("#{LENGTH} done"));

    assert_eq!(
        counts(&dir),
        format!("backlog 0, todo 0, in_progress 0, review 0, done {LENGTH}")
    );
    assert_sound(&dir);
}

#[test]
fn a_task_with_many_prerequisites_is_unblocked_by_the_last_one_only() {
    const WIDTH: i64 = 20;
    let dir = board();
    let prerequisites: Vec<i64> = (1..=WIDTH)
        .map(|n| add_task(&dir, &format!("prerequisite {n}"), "medium"))
        .collect();
    let all = prerequisites
        .iter()
        .map(i64::to_string)
        .collect::<Vec<_>>()
        .join(",");
    let waiting = WIDTH + 1;

    assert_eq!(
        run(
            &dir,
            &[
                "add",
                "--title",
                "waits for everything",
                "--test",
                "d",
                "i",
                "o",
                "--after",
                &all,
            ]
        ),
        format!("#{waiting} todo after:{all}")
    );
    for &id in &prerequisites[..prerequisites.len() - 1] {
        assert_eq!(finish(&dir, id), format!("#{id} done"));
    }
    assert_eq!(
        fail(&dir, &["claim", &waiting.to_string(), "--agent", "dev"]),
        format!("error: task {waiting} is blocked by unfinished tasks {WIDTH}")
    );
    assert_eq!(
        finish(&dir, WIDTH),
        format!("#{WIDTH} done unblocked:{waiting}")
    );

    let order = common::task(&dir, &["claim", &waiting.to_string(), "--agent", "dev"]);
    assert_eq!(order["id"], waiting);
}
