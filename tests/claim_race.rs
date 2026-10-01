//! Real OS-process races for `claim`. This is the test that matters most for a
//! tool built for concurrent agents: separate `agent-kanban` processes, released
//! together, fight over the same task through `SQLite`'s cross-process WAL
//! locking (not threads sharing one connection). Each race runs for many rounds
//! on fresh tasks so that at least some rounds interleave at the OS level even
//! on a fast, idle machine. Tests assert invariants (exactly one winner, a
//! consistent final state), never *who* wins.

mod common;

use common::{Outcome, argv, race};

const CONTENDERS: usize = 8;
const ROUNDS: usize = 20;

fn agent_names(prefix: &str) -> Vec<String> {
    (0..CONTENDERS).map(|i| format!("{prefix}-{i}")).collect()
}

/// Index of the one command that succeeded; every other one must have been
/// refused (exit 1) with `refusal` in the message.
fn only_winner(names: &[String], outcomes: &[Outcome], refusal: &str, round: usize) -> usize {
    let winners: Vec<usize> = (0..names.len()).filter(|&i| outcomes[i].ok()).collect();
    assert_eq!(
        winners.len(),
        1,
        "round {round}: expected exactly one winner, got {:?}",
        winners.iter().map(|&i| &names[i]).collect::<Vec<_>>()
    );
    for (name, outcome) in names.iter().zip(outcomes) {
        if !outcome.ok() {
            assert_eq!(outcome.code, Some(1), "round {round}: {name}: {outcome:?}");
            assert!(
                outcome.stderr.contains(refusal),
                "round {round}: {name} did not fail with '{refusal}': {}",
                outcome.stderr
            );
        }
    }
    winners[0]
}

#[test]
fn exactly_one_developer_wins_a_contested_task() {
    let dir = common::initialized();
    let names = agent_names("dev");
    for name in &names {
        common::register(&dir, name, "developer");
    }

    for round in 0..ROUNDS {
        let id = common::add_task(&dir, &format!("race task round {round}"), "medium");
        let id_arg = id.to_string();
        let commands: Vec<_> = names
            .iter()
            .map(|name| argv(&["claim", &id_arg, "--agent", name]))
            .collect();

        let outcomes = race(&dir, &commands);

        let winner = only_winner(&names, &outcomes, "already claimed", round);
        // The winner is handed the work order for exactly this task...
        let order = common::parse_order(&outcomes[winner].stdout);
        assert_eq!(order["id"], id, "round {round}");
        // ...and the board agrees with the process that won.
        let shown = common::task(&dir, &["show", &id_arg]);
        assert_eq!(shown["executor"], names[winner].as_str(), "round {round}");
        assert_eq!(shown["status"], "in_progress", "round {round}");
    }
}

#[test]
fn exactly_one_reviewer_wins_a_task_in_review() {
    let dir = common::initialized();
    common::register(&dir, "dev", "developer");
    let names = agent_names("rev");
    for name in &names {
        common::register(&dir, name, "reviewer");
    }

    for round in 0..ROUNDS {
        let id = common::add_task(&dir, &format!("review race round {round}"), "medium");
        let id_arg = id.to_string();
        common::run(&dir, &["claim", &id_arg, "--agent", "dev"]);
        common::run(
            &dir,
            &[
                "submit-review",
                &id_arg,
                "--agent",
                "dev",
                "--pass",
                "0",
                "verified",
            ],
        );
        let commands: Vec<_> = names
            .iter()
            .map(|name| argv(&["claim", &id_arg, "--agent", name]))
            .collect();

        let outcomes = race(&dir, &commands);

        let winner = only_winner(&names, &outcomes, "already claimed", round);
        // The reviewer's packet carries the developer's verdicts.
        let packet = common::parse_order(&outcomes[winner].stdout);
        assert_eq!(packet["rev"], 1, "round {round}");
        assert_eq!(packet["tests"][0]["result"], "passed", "round {round}");
        assert_eq!(packet["tests"][0]["evidence"], "verified", "round {round}");
        let shown = common::task(&dir, &["show", &id_arg]);
        assert_eq!(shown["status"], "review", "round {round}");
        assert_eq!(shown["executor"], names[winner].as_str(), "round {round}");
    }
}

/// Claiming a task you already hold renews the lease, so the same agent racing
/// itself (a retry that overlaps its first attempt) succeeds every time and
/// still ends up as the single holder.
#[test]
fn the_same_agent_claiming_concurrently_always_succeeds() {
    let dir = common::initialized();
    common::register(&dir, "alice", "developer");

    for round in 0..ROUNDS {
        let id = common::add_task(&dir, &format!("self race round {round}"), "medium");
        let id_arg = id.to_string();
        let commands = vec![argv(&["claim", &id_arg, "--agent", "alice"]); 6];

        let outcomes = race(&dir, &commands);

        for outcome in &outcomes {
            assert!(outcome.ok(), "round {round}: {outcome:?}");
            assert_eq!(outcome.stdout, outcomes[0].stdout, "round {round}");
        }
        let shown = common::task(&dir, &["show", &id_arg]);
        assert_eq!(shown["executor"], "alice", "round {round}");
        assert_eq!(shown["status"], "in_progress", "round {round}");
    }
}

#[test]
fn developers_claiming_different_tasks_do_not_block_each_other() {
    let dir = common::initialized();
    let names = agent_names("dev");
    for name in &names {
        common::register(&dir, name, "developer");
    }

    for round in 0..ROUNDS / 2 {
        let ids: Vec<String> = names
            .iter()
            .map(|name| common::add_task(&dir, &format!("{name} round {round}"), "low").to_string())
            .collect();
        let commands: Vec<_> = names
            .iter()
            .zip(&ids)
            .map(|(name, id)| argv(&["claim", id, "--agent", name]))
            .collect();

        let outcomes = race(&dir, &commands);

        for ((name, id), outcome) in names.iter().zip(&ids).zip(&outcomes) {
            assert!(outcome.ok(), "round {round}: {name}: {outcome:?}");
            let shown = common::task(&dir, &["show", id]);
            assert_eq!(shown["executor"], name.as_str(), "round {round}");
        }
    }
}

/// `claim` against `move ... backlog` on the same unclaimed task. The two are
/// mutually exclusive, so whichever commits first wins and the other is
/// refused; the task must never end up both parked and claimed.
#[test]
fn claim_racing_a_move_to_backlog_leaves_a_consistent_task() {
    let dir = common::initialized();
    common::register(&dir, "dev", "developer");

    for round in 0..ROUNDS {
        let id = common::add_task(&dir, &format!("move race round {round}"), "medium");
        let id_arg = id.to_string();

        let outcomes = race(
            &dir,
            &[
                argv(&["claim", &id_arg, "--agent", "dev"]),
                argv(&["move", &id_arg, "backlog"]),
            ],
        );
        let (claim, parked) = (&outcomes[0], &outcomes[1]);

        assert_ne!(claim.ok(), parked.ok(), "round {round}: {outcomes:?}");
        let shown = common::task(&dir, &["show", &id_arg]);
        if claim.ok() {
            assert_eq!(shown["status"], "in_progress", "round {round}");
            assert_eq!(shown["executor"], "dev", "round {round}");
            assert_eq!(
                parked.stderr,
                format!(
                    "error: task {id} is in_progress; move only works on backlog and todo tasks"
                ),
                "round {round}"
            );
        } else {
            assert_eq!(shown["status"], "backlog", "round {round}");
            assert!(shown.get("executor").is_none(), "round {round}: {shown}");
            assert_eq!(
                claim.stderr,
                format!("error: task {id} is backlog; claim needs todo or in_progress"),
                "round {round}"
            );
        }
    }
}
