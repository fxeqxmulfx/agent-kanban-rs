//! A task id is never handed out twice. With a plain `INTEGER PRIMARY KEY` the
//! next id is `max(id) + 1`, so removing the newest task freed its number for
//! the next `add`: a `#7` that another agent still held then named a different
//! task, and `claim 7` silently took it. Boards from before the fix are
//! covered in `upgrade.rs`.

mod common;

use common::{add_task, argv, fail, id_of, initialized, race, register, run};
use std::collections::BTreeSet;

const ROUNDS: usize = 20;

#[test]
fn removing_the_newest_task_does_not_free_its_id() {
    let dir = initialized();
    for title in ["a", "b", "c"] {
        add_task(&dir, title, "medium");
    }

    assert_eq!(run(&dir, &["remove", "3"]), "#3 removed");

    assert_eq!(add_task(&dir, "d", "medium"), 4);
    assert_eq!(
        run(&dir, &["list"]),
        "#1 medium todo a\n#2 medium todo b\n#4 medium todo d"
    );
}

#[test]
fn removing_every_task_does_not_restart_the_ids() {
    let dir = initialized();
    add_task(&dir, "a", "medium");
    run(&dir, &["remove", "1"]);
    assert_eq!(run(&dir, &["list"]), "no tasks");

    assert_eq!(add_task(&dir, "b", "medium"), 2);
}

/// What an agent holding a stale `#2` meets after #2 was removed and another
/// task was added: nothing, instead of the new task.
#[test]
fn a_stale_id_finds_nothing_instead_of_a_newer_task() {
    let dir = initialized();
    register(&dir, "bob", "developer");
    add_task(&dir, "one", "medium");
    add_task(&dir, "two", "medium");
    run(&dir, &["remove", "2"]);
    assert_eq!(add_task(&dir, "three", "medium"), 3);

    let gone = "error: task 2 not found";
    assert_eq!(fail(&dir, &["show", "2"]), gone);
    assert_eq!(fail(&dir, &["claim", "2", "--agent", "bob"]), gone);
    assert_eq!(fail(&dir, &["remove", "2"]), gone);
    assert_eq!(
        run(&dir, &["list"]),
        "#1 medium todo one\n#3 medium todo three"
    );
}

/// The refusal that used to come out as "task 2 cannot come after itself",
/// because the task being added had just been given the removed id.
#[test]
fn a_dependency_on_a_removed_task_says_not_found() {
    let dir = initialized();
    add_task(&dir, "one", "medium");
    add_task(&dir, "two", "medium");
    run(&dir, &["remove", "2"]);

    let refused = fail(
        &dir,
        &[
            "add", "--title", "x", "--test", "d", "i", "o", "--after", "2",
        ],
    );

    assert_eq!(refused, "error: task 2 not found");
    // The refused add did not use up an id.
    assert_eq!(add_task(&dir, "three", "medium"), 3);
}

/// `add` against `remove` of the newest task, as separate processes: the
/// removal can land before, between or after the adds, and no id may come out
/// twice either way. With the old table, an add that ran after the removal got
/// the removed task's number again.
#[test]
fn adds_racing_the_removal_of_the_newest_task_never_return_an_id_twice() {
    let dir = initialized();
    let mut newest = add_task(&dir, "seed", "low");
    let mut seen = BTreeSet::from([newest]);

    for round in 0..ROUNDS {
        let add = argv(&["add", "--title", "racer", "--test", "d", "i", "o"]);
        let outcomes = race(
            &dir,
            &[
                argv(&["remove", &newest.to_string()]),
                add.clone(),
                add.clone(),
                add,
            ],
        );

        assert_eq!(
            outcomes[0].stdout,
            format!("#{newest} removed"),
            "round {round}"
        );
        for outcome in &outcomes[1..] {
            assert!(outcome.ok(), "round {round}: {outcome:?}");
            let id = id_of(&outcome.stdout);
            assert!(seen.insert(id), "round {round}: #{id} was handed out twice");
            newest = newest.max(id);
        }
    }
}
