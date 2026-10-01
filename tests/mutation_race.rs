//! Real OS-process races between commands that change the same task or agent.
//! `claim_race.rs` covers claim against claim; here `remove`, `edit`, `move`,
//! `release`, `submit-review`, `approve`, `request-changes` and `agent remove`
//! fight each other or `claim`. Every command is one transaction that checks and
//! writes together, so each race has a small set of valid outcomes. Tests
//! assert that the outcome is one of them and that the board agrees with what
//! the processes reported, never which process wins. `common::race` also fails
//! the test if any process leaks a raw `SQLite` error.

mod common;

use common::{Outcome, argv, race};

const ROUNDS: usize = 20;

fn not_found(id: i64) -> String {
    format!("error: task {id} not found")
}

/// Task `id` claimed by `agent`; returns the id and its argument form.
fn claimed_task(dir: &tempfile::TempDir, title: &str, agent: &str) -> (i64, String) {
    let id = common::add_task(dir, title, "medium");
    let id_arg = id.to_string();
    common::run(dir, &["claim", &id_arg, "--agent", agent]);
    (id, id_arg)
}

fn pair(outcomes: &[Outcome]) -> (&Outcome, &Outcome) {
    assert_eq!(outcomes.len(), 2);
    (&outcomes[0], &outcomes[1])
}

/// `remove` and `claim` exclude each other on an unclaimed task: either the
/// task is deleted and the claim finds nothing, or it is claimed and the removal
/// is refused.
#[test]
fn remove_racing_a_claim_never_corrupts_state() {
    let dir = common::initialized();
    common::register(&dir, "agent-x", "developer");

    for round in 0..ROUNDS {
        let id = common::add_task(&dir, &format!("race task round {round}"), "medium");
        let id_arg = id.to_string();

        let outcomes = race(
            &dir,
            &[
                argv(&["remove", &id_arg]),
                argv(&["claim", &id_arg, "--agent", "agent-x"]),
            ],
        );
        let (remove, claim) = pair(&outcomes);

        assert_ne!(remove.ok(), claim.ok(), "round {round}: {outcomes:?}");
        if remove.ok() {
            assert_eq!(remove.stdout, format!("#{id} removed"));
            assert_eq!(claim.stderr, not_found(id), "round {round}");
            assert_eq!(common::fail(&dir, &["show", &id_arg]), not_found(id));
        } else {
            assert_eq!(
                remove.stderr,
                format!("error: task {id} is claimed; release it before removing"),
                "round {round}"
            );
            let shown = common::task(&dir, &["show", &id_arg]);
            assert_eq!(shown["executor"], "agent-x", "round {round}");
            assert_eq!(shown["status"], "in_progress", "round {round}");
        }
    }
}

/// `release` by the holder always succeeds. A claim by someone else either runs
/// first (refused: still held) or after the release (succeeds).
#[test]
fn release_racing_a_claim_never_corrupts_state() {
    let dir = common::initialized();
    common::register(&dir, "agent-a", "developer");
    common::register(&dir, "agent-b", "developer");

    for round in 0..ROUNDS {
        let (id, id_arg) = claimed_task(&dir, &format!("race task round {round}"), "agent-a");

        let outcomes = race(
            &dir,
            &[
                argv(&["release", &id_arg, "--agent", "agent-a"]),
                argv(&["claim", &id_arg, "--agent", "agent-b"]),
            ],
        );
        let (release, claim) = pair(&outcomes);

        assert!(release.ok(), "round {round}: {outcomes:?}");
        assert_eq!(release.stdout, format!("#{id} in_progress"));
        let shown = common::task(&dir, &["show", &id_arg]);
        if claim.ok() {
            assert_eq!(shown["executor"], "agent-b", "round {round}");
        } else {
            assert!(
                claim.stderr.starts_with(&format!(
                    "error: task {id} is already claimed by 'agent-a' until "
                )),
                "round {round}: {}",
                claim.stderr
            );
            assert!(shown.get("executor").is_none(), "round {round}: {shown}");
        }
        assert_eq!(shown["status"], "in_progress", "round {round}");
    }
}

/// `agent remove` against a `claim` by the very agent being removed. `agent
/// remove` always succeeds, and it releases the task if and only if the claim
/// got in first; a claim that loses fails cleanly with "not registered" and
/// never with a raw foreign-key error. No task may be left pointing at a deleted
/// agent, checked on the raw database because `show` would render a dangling id
/// as nothing.
#[test]
fn agent_remove_racing_a_claim_never_leaves_a_dangling_owner() {
    let dir = common::initialized();

    for round in 0..ROUNDS {
        let name = format!("agent-{round}");
        common::register(&dir, &name, "developer");
        let id = common::add_task(&dir, &format!("race task round {round}"), "medium");
        let id_arg = id.to_string();

        let outcomes = race(
            &dir,
            &[
                argv(&["agent", "remove", &name]),
                argv(&["claim", &id_arg, "--agent", &name]),
            ],
        );
        let (removed, claim) = pair(&outcomes);

        assert!(removed.ok(), "round {round}: {outcomes:?}");
        let shown = common::task(&dir, &["show", &id_arg]);
        if claim.ok() {
            assert_eq!(removed.stdout, format!("{name} removed, released #{id}"));
            assert_eq!(shown["status"], "in_progress", "round {round}");
        } else {
            assert_eq!(removed.stdout, format!("{name} removed"));
            assert!(
                claim
                    .stderr
                    .starts_with(&format!("error: agent '{name}' is not registered")),
                "round {round}: {}",
                claim.stderr
            );
            assert_eq!(shown["status"], "todo", "round {round}");
        }
        assert!(shown.get("executor").is_none(), "round {round}: {shown}");

        let dangling: i64 = common::db(&dir)
            .query_row(
                "SELECT COUNT(*) FROM tasks LEFT JOIN agents ON tasks.executor = agents.id
                 WHERE tasks.executor IS NOT NULL AND agents.id IS NULL",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(
            dangling, 0,
            "round {round}: a task points at a deleted agent"
        );
    }
}

/// Two removals of the same agent: exactly one wins, the other is told the agent
/// is gone.
#[test]
fn removing_the_same_agent_twice_at_once_has_exactly_one_winner() {
    let dir = common::initialized();

    for round in 0..ROUNDS {
        let name = format!("agent-{round}");
        common::register(&dir, &name, "developer");

        let outcomes = race(
            &dir,
            &[
                argv(&["agent", "remove", &name]),
                argv(&["agent", "remove", &name]),
            ],
        );
        let (first, second) = pair(&outcomes);

        assert_ne!(first.ok(), second.ok(), "round {round}: {outcomes:?}");
        let (winner, loser) = if first.ok() {
            (first, second)
        } else {
            (second, first)
        };
        assert_eq!(winner.stdout, format!("{name} removed"), "round {round}");
        assert_eq!(
            loser.stderr,
            format!("error: agent '{name}' not found"),
            "round {round}"
        );
    }
}

/// `edit` against `claim` of an unclaimed task: the claim always succeeds; the
/// edit succeeds only if it landed first, and then the work order must already
/// carry the new title. An edit that loses is refused whole.
#[test]
fn edit_racing_a_claim_never_half_applies() {
    let dir = common::initialized();
    common::register(&dir, "dev", "developer");

    for round in 0..ROUNDS {
        let title = format!("original round {round}");
        let id = common::add_task(&dir, &title, "medium");
        let id_arg = id.to_string();

        let outcomes = race(
            &dir,
            &[
                argv(&["edit", &id_arg, "--title", "edited", "--priority", "urgent"]),
                argv(&["claim", &id_arg, "--agent", "dev"]),
            ],
        );
        let (edit, claim) = pair(&outcomes);

        assert!(claim.ok(), "round {round}: {outcomes:?}");
        let order = common::parse_order(&claim.stdout);
        let shown = common::task(&dir, &["show", &id_arg]);
        if edit.ok() {
            assert_eq!(edit.stdout, format!("#{id} todo"));
            assert_eq!(order["title"], "edited", "round {round}");
            assert_eq!(shown["title"], "edited", "round {round}");
            assert_eq!(shown["priority"], "urgent", "round {round}");
        } else {
            assert_eq!(
                edit.stderr,
                format!("error: task {id} is claimed; release it before editing"),
                "round {round}"
            );
            assert_eq!(order["title"], title.as_str(), "round {round}");
            assert_eq!(shown["title"], title.as_str(), "round {round}");
            assert_eq!(shown["priority"], "medium", "round {round}");
        }
    }
}

/// `remove` against `edit` of an unclaimed task: the removal always succeeds,
/// and an edit that came second finds nothing.
#[test]
fn remove_racing_an_edit_leaves_no_task_behind() {
    let dir = common::initialized();

    for round in 0..ROUNDS {
        let id = common::add_task(&dir, &format!("race task round {round}"), "medium");
        let id_arg = id.to_string();

        let outcomes = race(
            &dir,
            &[
                argv(&["remove", &id_arg]),
                argv(&["edit", &id_arg, "--title", "edited"]),
            ],
        );
        let (remove, edit) = pair(&outcomes);

        assert!(remove.ok(), "round {round}: {outcomes:?}");
        if !edit.ok() {
            assert_eq!(edit.stderr, not_found(id), "round {round}");
        }
        assert_eq!(common::fail(&dir, &["show", &id_arg]), not_found(id));
    }
}

/// The developer submits while also releasing the same task: exactly one gets
/// through. A submit that wins stores its verdicts and moves the task to
/// review; one that loses stores nothing.
#[test]
fn submit_review_racing_a_release_leaves_one_consistent_outcome() {
    let dir = common::initialized();
    common::register(&dir, "dev", "developer");

    for round in 0..ROUNDS {
        let (id, id_arg) = claimed_task(&dir, &format!("race task round {round}"), "dev");

        let outcomes = race(
            &dir,
            &[
                argv(&[
                    "submit-review",
                    &id_arg,
                    "--agent",
                    "dev",
                    "--pass",
                    "0",
                    "ok",
                ]),
                argv(&["release", &id_arg, "--agent", "dev"]),
            ],
        );
        let (submit, release) = pair(&outcomes);

        assert_ne!(submit.ok(), release.ok(), "round {round}: {outcomes:?}");
        let shown = common::task(&dir, &["show", &id_arg]);
        let stored: i64 = common::db(&dir)
            .query_row(
                "SELECT COUNT(*) FROM acceptance_results WHERE task_id = ?1",
                [id],
                |row| row.get(0),
            )
            .unwrap();
        let not_claimed = format!("error: task {id} is not claimed by 'dev'");
        if submit.ok() {
            assert_eq!(submit.stdout, format!("#{id} review rev1"));
            assert_eq!(release.stderr, not_claimed, "round {round}");
            assert_eq!(shown["status"], "review", "round {round}");
            assert_eq!(shown["tests"][0]["result"], "passed", "round {round}");
            assert_eq!(stored, 1, "round {round}");
        } else {
            assert_eq!(release.stdout, format!("#{id} in_progress"));
            assert_eq!(submit.stderr, not_claimed, "round {round}");
            assert_eq!(shown["status"], "in_progress", "round {round}");
            assert!(shown.get("rev").is_none(), "round {round}: {shown}");
            assert_eq!(stored, 0, "round {round}: a refused submit left results");
        }
    }
}

/// One reviewer both approves and requests changes at the same time: exactly one
/// decision is recorded, and the loser is told the task is no longer in review.
#[test]
fn approve_racing_request_changes_records_exactly_one_decision() {
    let dir = common::initialized();
    common::register(&dir, "dev", "developer");
    common::register(&dir, "rev", "reviewer");

    for round in 0..ROUNDS {
        let (id, id_arg) = claimed_task(&dir, &format!("race task round {round}"), "dev");
        common::run(
            &dir,
            &[
                "submit-review",
                &id_arg,
                "--agent",
                "dev",
                "--pass",
                "0",
                "ok",
            ],
        );
        common::run(&dir, &["claim", &id_arg, "--agent", "rev"]);

        let outcomes = race(
            &dir,
            &[
                argv(&["approve", &id_arg, "--agent", "rev"]),
                argv(&[
                    "request-changes",
                    &id_arg,
                    "--agent",
                    "rev",
                    "--notes",
                    "please fix",
                ]),
            ],
        );
        let (approve, changes) = pair(&outcomes);

        assert_ne!(approve.ok(), changes.ok(), "round {round}: {outcomes:?}");
        let shown = common::task(&dir, &["show", &id_arg, "--history"]);
        let history = shown["history"].as_array().unwrap();
        assert_eq!(history.len(), 1, "round {round}: {shown}");
        if approve.ok() {
            assert_eq!(approve.stdout, format!("#{id} done"));
            assert_eq!(
                changes.stderr,
                format!("error: task {id} is done; request-changes needs review"),
                "round {round}"
            );
            assert_eq!(shown["status"], "done", "round {round}");
            assert_eq!(history[0]["decision"], "approved", "round {round}");
        } else {
            assert_eq!(changes.stdout, format!("#{id} in_progress"));
            assert_eq!(
                approve.stderr,
                format!("error: task {id} is in_progress; approve needs review"),
                "round {round}"
            );
            assert_eq!(shown["status"], "in_progress", "round {round}");
            assert_eq!(shown["changes"], "please fix", "round {round}");
            assert_eq!(history[0]["decision"], "changes_requested", "round {round}");
        }
    }
}
