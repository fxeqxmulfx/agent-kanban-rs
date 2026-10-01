//! `claim-next`: an agent asks the board for its next task instead of choosing
//! one. The pick and the claim are one atomic step, so a crowd of agents never
//! gets the same task. Everything runs through the compiled binary.

mod common;

use common::{add_task, argv, db, expire_lease, fail, initialized, race, register, run};
use std::path::Path;
use std::sync::Barrier;
use std::time::Duration;
use tempfile::TempDir;

/// What `add_task` gives every task as its single acceptance test.
fn work_order(id: i64, title: &str) -> String {
    format!("#{id} {title}\n0|works|in|out")
}

/// A board with developers `dev1..=devN` and reviewers `rev` and `rev2`.
fn board(developers: usize) -> TempDir {
    let dir = initialized();
    for n in 1..=developers {
        register(&dir, &format!("dev{n}"), "developer");
    }
    register(&dir, "rev", "reviewer");
    register(&dir, "rev2", "reviewer");
    dir
}

fn claim_next(dir: &Path, agent: &str) -> String {
    run(dir, &["claim-next", "--agent", agent])
}

/// The id in a `claim-next` reply that is a work order.
fn claimed_id(reply: &str) -> i64 {
    common::parse_order(reply)["id"].as_i64().unwrap()
}

/// The developer hands task `id` in, with its only test passed.
fn submit(dir: &Path, agent: &str, id: i64) {
    run(
        dir,
        &[
            "submit-review",
            &id.to_string(),
            "--agent",
            agent,
            "--pass",
            "0",
            "ok",
        ],
    );
}

// ---------------------------------------------------------------------------
// What gets picked
// ---------------------------------------------------------------------------

/// Priority first, then the oldest; parked tasks are never offered.
#[test]
fn each_agent_gets_the_best_task_that_is_left() {
    let dir = board(5);
    add_task(&dir, "low1", "low");
    add_task(&dir, "urgent2", "urgent");
    add_task(&dir, "high3", "high");
    add_task(&dir, "urgent4", "urgent");
    add_task(&dir, "parked5", "urgent");
    run(&dir, &["move", "5", "backlog"]);

    assert_eq!(claim_next(dir.path(), "dev1"), work_order(2, "urgent2"));
    assert_eq!(claim_next(dir.path(), "dev2"), work_order(4, "urgent4"));
    assert_eq!(claim_next(dir.path(), "dev3"), work_order(3, "high3"));
    assert_eq!(claim_next(dir.path(), "dev4"), work_order(1, "low1"));

    assert_eq!(claim_next(dir.path(), "dev5"), "idle, 4 open");
    assert_eq!(
        run(&dir, &["list"]),
        "#2 urgent in_progress@dev1 urgent2\n#4 urgent in_progress@dev2 urgent4\n\
         #3 high in_progress@dev3 high3\n#1 low in_progress@dev4 low1\n#5 urgent backlog parked5"
    );
}

#[test]
fn the_work_order_is_the_same_one_claim_gives() {
    let dir = board(1);
    add_task(&dir, "t", "medium");

    let order = claim_next(dir.path(), "dev1");

    assert_eq!(order, run(&dir, &["claim", "1", "--agent", "dev1"]));
}

/// A restarted or forgetful agent that asks again gets the task it still holds,
/// even if something more urgent arrived in the meantime.
#[test]
fn an_agent_gets_its_own_task_back_before_anything_else() {
    let dir = board(2);
    add_task(&dir, "mine", "low");
    assert_eq!(claim_next(dir.path(), "dev1"), work_order(1, "mine"));
    add_task(&dir, "urgent", "urgent");

    assert_eq!(claim_next(dir.path(), "dev1"), work_order(1, "mine"));
    assert_eq!(claim_next(dir.path(), "dev1"), work_order(1, "mine"));
    // Someone else is offered the urgent one.
    assert_eq!(claim_next(dir.path(), "dev2"), work_order(2, "urgent"));
}

/// Work that came back from review is unfinished work: it goes before fresh
/// tasks, however urgent they are.
#[test]
fn rework_comes_before_fresh_work_whatever_the_priority() {
    let dir = board(2);
    add_task(&dir, "rework", "low");
    claim_next(dir.path(), "dev1");
    submit(dir.path(), "dev1", 1);
    run(&dir, &["claim", "1", "--agent", "rev"]);
    run(
        &dir,
        &[
            "request-changes",
            "1",
            "--agent",
            "rev",
            "--notes",
            "fix it",
        ],
    );
    add_task(&dir, "fresh", "urgent");

    let order = common::parse_order(&claim_next(dir.path(), "dev1"));

    assert_eq!(order["id"], 1);
    assert_eq!(order["changes"], "fix it");
    assert_eq!(claim_next(dir.path(), "dev2"), work_order(2, "fresh"));
}

// ---------------------------------------------------------------------------
// Nothing to pick
// ---------------------------------------------------------------------------

#[test]
fn idle_says_whether_anything_unfinished_is_left() {
    let dir = board(2);

    assert_eq!(claim_next(dir.path(), "dev1"), "idle");

    // Parked tasks are neither offered nor counted.
    add_task(&dir, "parked", "medium");
    run(&dir, &["move", "1", "backlog"]);
    assert_eq!(claim_next(dir.path(), "dev1"), "idle");

    // A task someone else is working on.
    add_task(&dir, "held", "medium");
    claim_next(dir.path(), "dev1");
    assert_eq!(claim_next(dir.path(), "dev2"), "idle, 1 open");

    // A task waiting for review is not for developers either.
    submit(dir.path(), "dev1", 2);
    assert_eq!(claim_next(dir.path(), "dev1"), "idle, 1 open");
    assert_eq!(claim_next(dir.path(), "dev2"), "idle, 1 open");

    // Once it is done nothing is left.
    run(&dir, &["claim", "2", "--agent", "rev"]);
    run(&dir, &["approve", "2", "--agent", "rev"]);
    assert_eq!(claim_next(dir.path(), "dev2"), "idle");
}

#[test]
fn an_unregistered_agent_is_refused_with_a_hint() {
    let dir = board(1);
    add_task(&dir, "t", "medium");

    assert_eq!(
        fail(&dir, &["claim-next", "--agent", "ghost"]),
        "error: agent 'ghost' is not registered; run `agent-kanban agent register ghost \
         --role developer|reviewer` first"
    );
    assert_eq!(run(&dir, &["list"]), "#1 medium todo t");
}

// ---------------------------------------------------------------------------
// Reviewers
// ---------------------------------------------------------------------------

#[test]
fn a_reviewer_is_offered_only_tasks_in_review_best_first() {
    let dir = board(1);
    add_task(&dir, "todo", "urgent");
    add_task(&dir, "low review", "low");
    add_task(&dir, "high review", "high");
    for id in [2, 3] {
        run(&dir, &["claim", &id.to_string(), "--agent", "dev1"]);
        run(
            &dir,
            &[
                "submit-review",
                &id.to_string(),
                "--agent",
                "dev1",
                "--pass",
                "0",
                &format!("evidence {id}"),
            ],
        );
    }

    assert_eq!(
        claim_next(dir.path(), "rev"),
        "#3 rev1 high review\n0|works|in|out|passed|evidence 3"
    );
    // Asking again returns the same task; a second reviewer gets the other one.
    assert_eq!(claimed_id(&claim_next(dir.path(), "rev")), 3);
    assert_eq!(claimed_id(&claim_next(dir.path(), "rev2")), 2);
    assert_eq!(claimed_id(&claim_next(dir.path(), "rev2")), 2);
}

#[test]
fn a_reviewer_with_nothing_in_review_is_idle_while_work_is_open() {
    let dir = board(1);

    assert_eq!(claim_next(dir.path(), "rev"), "idle");

    add_task(&dir, "t", "medium");
    assert_eq!(claim_next(dir.path(), "rev"), "idle, 1 open");
    claim_next(dir.path(), "dev1");
    assert_eq!(claim_next(dir.path(), "rev"), "idle, 1 open");
}

// ---------------------------------------------------------------------------
// Leases
// ---------------------------------------------------------------------------

#[test]
fn a_task_whose_lease_ran_out_goes_to_the_next_agent() {
    let dir = board(2);
    add_task(&dir, "abandoned", "medium");
    claim_next(dir.path(), "dev1");
    expire_lease(&dir, 1);

    assert_eq!(claim_next(dir.path(), "dev2"), work_order(1, "abandoned"));

    // The first holder lost the task and is told so rather than overwriting.
    assert_eq!(
        fail(
            &dir,
            &["submit-review", "1", "--agent", "dev1", "--pass", "0", "ok"]
        ),
        "error: task 1 is claimed by 'dev2', not 'dev1'"
    );
    assert_eq!(claim_next(dir.path(), "dev1"), "idle, 1 open");
}

fn lease_seconds(dir: &TempDir, id: i64) -> i64 {
    db(dir)
        .query_row(
            "SELECT CAST(strftime('%s', lease_expires_at) AS INTEGER)
                  - CAST(strftime('%s', claimed_at) AS INTEGER)
             FROM tasks WHERE id = ?1",
            [id],
            |row| row.get(0),
        )
        .unwrap()
}

#[test]
fn the_lease_defaults_to_an_hour_and_the_flag_changes_it() {
    let dir = board(3);
    for title in ["a", "b", "c"] {
        add_task(&dir, title, "medium");
    }

    claim_next(dir.path(), "dev1");
    run(&dir, &["claim-next", "--agent", "dev2", "--lease", "7"]);
    assert_eq!(lease_seconds(&dir, 1), 3600);
    assert_eq!(lease_seconds(&dir, 2), 7);

    // Asking again renews the lease with the new length.
    run(&dir, &["claim-next", "--agent", "dev1", "--lease", "90"]);

    assert_eq!(lease_seconds(&dir, 1), 90);
    assert_eq!(
        fail(&dir, &["claim-next", "--agent", "dev3", "--lease", "0"]),
        "error: lease must be at least one second"
    );
}

/// Leases run out on the clock, not only when a test edits the database.
#[test]
fn a_short_lease_really_expires() {
    let dir = board(2);
    add_task(&dir, "t", "medium");
    run(&dir, &["claim-next", "--agent", "dev1", "--lease", "1"]);

    std::thread::sleep(Duration::from_millis(1500));

    assert_eq!(claim_next(dir.path(), "dev2"), work_order(1, "t"));
}

#[test]
fn releasing_or_removing_the_holder_puts_the_task_back_in_the_pool() {
    let dir = board(2);
    add_task(&dir, "t", "medium");
    claim_next(dir.path(), "dev1");
    assert_eq!(claim_next(dir.path(), "dev2"), "idle, 1 open");

    run(&dir, &["release", "1", "--agent", "dev1"]);
    assert_eq!(claim_next(dir.path(), "dev2"), work_order(1, "t"));

    assert_eq!(
        run(&dir, &["agent", "remove", "dev2"]),
        "dev2 removed, released #1"
    );
    assert_eq!(claim_next(dir.path(), "dev1"), work_order(1, "t"));
}

// ---------------------------------------------------------------------------
// A whole board worked through `claim-next` alone
// ---------------------------------------------------------------------------

#[test]
fn a_developer_and_a_reviewer_work_the_board_in_priority_order() {
    let dir = board(1);
    for (title, priority) in [
        ("t1", "low"),
        ("t2", "urgent"),
        ("t3", "medium"),
        ("t4", "high"),
        ("t5", "medium"),
        ("t6", "urgent"),
    ] {
        add_task(&dir, title, priority);
    }
    let expected = [2, 6, 4, 3, 5, 1];

    let mut developed = Vec::new();
    loop {
        let reply = claim_next(dir.path(), "dev1");
        if reply.starts_with("idle") {
            break;
        }
        let id = claimed_id(&reply);
        submit(dir.path(), "dev1", id);
        developed.push(id);
    }
    assert_eq!(developed, expected);
    assert_eq!(claim_next(dir.path(), "dev1"), "idle, 6 open");

    let mut reviewed = Vec::new();
    loop {
        let reply = claim_next(dir.path(), "rev");
        if reply.starts_with("idle") {
            break;
        }
        let id = claimed_id(&reply);
        run(&dir, &["approve", &id.to_string(), "--agent", "rev"]);
        reviewed.push(id);
    }
    assert_eq!(reviewed, expected);

    assert_eq!(claim_next(dir.path(), "dev1"), "idle");
    assert_eq!(claim_next(dir.path(), "rev"), "idle");
    assert_eq!(
        run(&dir, &["status"]).lines().next().unwrap(),
        "backlog 0, todo 0, in_progress 0, review 0, done 6"
    );
}

// ---------------------------------------------------------------------------
// Concurrency
// ---------------------------------------------------------------------------

/// More agents than tasks, all asking at once: every task goes to exactly one
/// agent and the rest are told there is nothing for them.
#[test]
fn simultaneous_claim_next_calls_never_share_a_task() {
    const AGENTS: usize = 8;
    const TASKS: usize = 5;

    for round in 0..15 {
        let dir = board(AGENTS);
        for n in 0..TASKS {
            let priority = ["low", "medium", "high", "urgent", "medium"][n];
            add_task(&dir, &format!("round {round} task {n}"), priority);
        }
        let commands: Vec<_> = (1..=AGENTS)
            .map(|n| argv(&["claim-next", "--agent", &format!("dev{n}")]))
            .collect();

        let outcomes = race(&dir, &commands);

        let mut won: Vec<i64> = Vec::new();
        for outcome in &outcomes {
            assert!(outcome.ok(), "round {round}: {outcome:?}");
            if outcome.stdout.starts_with('#') {
                won.push(claimed_id(&outcome.stdout));
            } else {
                assert_eq!(outcome.stdout, "idle, 5 open", "round {round}");
            }
        }
        won.sort_unstable();
        assert_eq!(won, [1, 2, 3, 4, 5], "round {round}: {outcomes:?}");
        // The board agrees: five tasks held by five different agents.
        let holders = db(&dir)
            .query_row(
                "SELECT COUNT(DISTINCT executor) FROM tasks WHERE status = 'in_progress'",
                [],
                |row| row.get::<_, i64>(0),
            )
            .unwrap();
        assert_eq!(holders, 5, "round {round}");
    }
}

/// Run `work(n)` on `workers` threads released together; results in order.
fn together<T: Send>(workers: usize, work: impl Fn(usize) -> T + Sync) -> Vec<T> {
    let barrier = Barrier::new(workers);
    std::thread::scope(|scope| {
        let handles: Vec<_> = (0..workers)
            .map(|n| {
                let (barrier, work) = (&barrier, &work);
                scope.spawn(move || {
                    barrier.wait();
                    work(n)
                })
            })
            .collect();
        handles
            .into_iter()
            .map(|handle| handle.join().unwrap())
            .collect()
    })
}

/// Crowds of developers and then reviewers empty a board using nothing but
/// `claim-next`: each task is developed once and approved once, none is lost.
#[test]
fn crowds_of_workers_drain_the_board_without_doubling_up_or_dropping_tasks() {
    const TASKS: i64 = 30;
    const DEVELOPERS: usize = 6;
    let dir = initialized();
    for n in 0..DEVELOPERS {
        register(&dir, &format!("dev{n}"), "developer");
    }
    for n in 0..4 {
        register(&dir, &format!("rev{n}"), "reviewer");
    }
    for (n, priority) in (1..=TASKS).zip(["low", "medium", "high"].into_iter().cycle()) {
        add_task(&dir, &format!("task {n}"), priority);
    }

    let developed = together(DEVELOPERS, |n| {
        let name = format!("dev{n}");
        let mut mine = Vec::new();
        loop {
            let reply = claim_next(dir.path(), &name);
            if reply.starts_with("idle") {
                return mine;
            }
            let id = claimed_id(&reply);
            submit(dir.path(), &name, id);
            mine.push(id);
        }
    });
    let mut all: Vec<i64> = developed.into_iter().flatten().collect();
    all.sort_unstable();
    assert_eq!(all, (1..=TASKS).collect::<Vec<_>>());

    let reviewed = together(4, |n| {
        let name = format!("rev{n}");
        let mut mine = Vec::new();
        loop {
            let reply = claim_next(dir.path(), &name);
            if reply.starts_with("idle") {
                return mine;
            }
            let id = claimed_id(&reply);
            run(&dir, &["approve", &id.to_string(), "--agent", &name]);
            mine.push(id);
        }
    });
    let mut all: Vec<i64> = reviewed.into_iter().flatten().collect();
    all.sort_unstable();
    assert_eq!(all, (1..=TASKS).collect::<Vec<_>>());

    assert_eq!(
        run(&dir, &["status"]).lines().next().unwrap(),
        format!("backlog 0, todo 0, in_progress 0, review 0, done {TASKS}")
    );
    let decisions: i64 = db(&dir)
        .query_row(
            "SELECT COUNT(*) FROM review_history WHERE decision = 'approved'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(decisions, TASKS);
}
