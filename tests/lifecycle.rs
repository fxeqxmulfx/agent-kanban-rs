//! A task's life through the real binary: claim, submit-review, review
//! decisions, rework, leases, release, move, and the guards around edit and
//! remove. Replies are asserted as exact strings; they are the interface agents
//! parse.

mod common;

use common::{add_task, fail, initialized, register, run};
use tempfile::TempDir;

const ONE_TEST: &str = r#"[{"describe":"works","input":"in","output":"out"}]"#;

/// Board with two developers and a reviewer.
fn board() -> TempDir {
    let dir = initialized();
    register(&dir, "dev", "developer");
    register(&dir, "dev2", "developer");
    register(&dir, "rev", "reviewer");
    dir
}

/// A one-test task, claimed by `dev`.
fn claimed(dir: &TempDir) -> String {
    let id = add_task(dir, "t", "medium").to_string();
    run(dir, &["claim", &id, "--agent", "dev"]);
    id
}

/// A one-test task `dev` has finished: now waiting in review.
fn submitted(dir: &TempDir) -> String {
    let id = claimed(dir);
    run(
        dir,
        &["submit-review", &id, "--agent", "dev", "--pass", "0", "ok"],
    );
    id
}

/// A task with `count` tests, claimed by `dev`.
fn claimed_with_tests(dir: &TempDir, count: usize) -> String {
    let mut args = vec!["add", "--title", "many"];
    for _ in 0..count {
        args.extend(["--test", "d", "i", "o"]);
    }
    let id = common::id_of(&run(dir, &args)).to_string();
    run(dir, &["claim", &id, "--agent", "dev"]);
    id
}

fn lease_of(dir: &TempDir, id: &str) -> String {
    common::db(dir)
        .query_row(
            "SELECT lease_expires_at FROM tasks WHERE id = ?1",
            [id],
            |row| row.get(0),
        )
        .unwrap()
}

// ---------------------------------------------------------------------------
// The golden path
// ---------------------------------------------------------------------------

#[test]
fn golden_path_from_add_to_done() {
    let dir = initialized();
    register(&dir, "agent-alpha", "developer");
    register(&dir, "reviewer-beta", "reviewer");

    assert_eq!(
        run(
            &dir,
            &[
                "add",
                "--title",
                "Implement feature X",
                "--priority",
                "high",
                "--tag",
                "backend",
                "--tag",
                "urgent-fix",
                "--test",
                "basic add",
                "2+2",
                "4",
                "--test",
                "edge",
                "0+0",
                "0",
            ]
        ),
        "#1 todo"
    );
    assert_eq!(run(&dir, &["list"]), "#1 high todo Implement feature X");
    let tests = "0|basic add|2+2|4\n1|edge|0+0|0";
    assert_eq!(
        run(&dir, &["show", "1"]),
        format!("#1 high todo Implement feature X\ntags|backend|urgent-fix\n{tests}")
    );

    // The developer's work order is the task's title and tests, nothing else.
    assert_eq!(
        run(&dir, &["claim", "1", "--agent", "agent-alpha"]),
        format!("#1 Implement feature X\n{tests}")
    );
    assert_eq!(
        run(&dir, &["list"]),
        "#1 high in_progress@agent-alpha Implement feature X"
    );

    assert_eq!(
        run(
            &dir,
            &[
                "submit-review",
                "1",
                "--agent",
                "agent-alpha",
                "--pass",
                "0",
                "cargo test: basic add passed",
                "--pass",
                "1",
                "cargo test: edge passed",
            ]
        ),
        "#1 review rev1"
    );
    assert_eq!(
        fail(&dir, &["edit", "1", "--title", "renamed"]),
        "error: task 1 is in review; request changes before editing"
    );

    // The reviewer's packet carries the developer's verdicts and evidence.
    assert_eq!(
        run(&dir, &["claim", "1", "--agent", "reviewer-beta"]),
        concat!(
            "#1 rev1 Implement feature X\n",
            "0|basic add|2+2|4|passed|cargo test: basic add passed\n",
            "1|edge|0+0|0|passed|cargo test: edge passed"
        )
    );
    assert_eq!(
        run(
            &dir,
            &[
                "approve",
                "1",
                "--agent",
                "reviewer-beta",
                "--notes",
                "verified"
            ]
        ),
        "#1 done"
    );
    assert_eq!(
        run(&dir, &["status"]),
        "backlog 0, todo 0, in_progress 0, review 0, done 1\nagents: agent-alpha -; reviewer-beta -"
    );
}

/// Finished work keeps its evidence and the names of the people involved even
/// after those agents are removed.
#[test]
fn finished_work_outlives_the_agents_that_did_it() {
    let dir = board();
    let id = claimed_with_tests(&dir, 2);
    run(
        &dir,
        &[
            "submit-review",
            &id,
            "--agent",
            "dev",
            "--pass",
            "0",
            "first ok",
            "--pass",
            "1",
            "second ok",
        ],
    );
    run(&dir, &["claim", &id, "--agent", "rev"]);
    run(
        &dir,
        &["approve", &id, "--agent", "rev", "--notes", "verified"],
    );

    assert_eq!(run(&dir, &["agent", "remove", "dev"]), "dev removed");
    assert_eq!(run(&dir, &["agent", "remove", "rev"]), "rev removed");

    let shown = common::task(&dir, &["show", &id, "--history"]);
    assert_eq!(shown["status"], "done");
    assert_eq!(shown["tests"][1]["evidence"], "second ok");
    assert_eq!(shown["history"][0]["by"], "dev");
    assert_eq!(shown["history"][0]["reviewer"], "rev");
    assert_eq!(shown["history"][0]["decision"], "approved");
    assert_eq!(shown["history"][0]["notes"], "verified");
}

#[test]
fn failed_tests_are_recorded_and_shown_to_the_reviewer() {
    let dir = board();
    let id = claimed_with_tests(&dir, 2);

    run(
        &dir,
        &[
            "submit-review",
            &id,
            "--agent",
            "dev",
            "--pass",
            "0",
            "fine",
            "--fail",
            "1",
            "cargo test b: expected 200 got 500",
        ],
    );

    let packet = run(&dir, &["claim", &id, "--agent", "rev"]);
    let packet = common::parse_order(&packet);
    assert_eq!(packet["tests"][0]["result"], "passed");
    assert_eq!(packet["tests"][1]["result"], "failed");
    assert_eq!(
        packet["tests"][1]["evidence"],
        "cargo test b: expected 200 got 500"
    );
}

// ---------------------------------------------------------------------------
// Review decisions and rework
// ---------------------------------------------------------------------------

#[test]
fn rework_loop_starts_a_new_revision_and_keeps_every_decision() {
    let dir = board();
    let id = claimed(&dir);
    run(&dir, &["release", &id, "--agent", "dev"]);
    run(&dir, &["claim", &id, "--agent", "dev"]);
    assert_eq!(
        run(
            &dir,
            &[
                "submit-review",
                &id,
                "--agent",
                "dev",
                "--fail",
                "0",
                "integration test failed"
            ]
        ),
        "#1 review rev1"
    );
    run(&dir, &["claim", &id, "--agent", "rev"]);
    assert_eq!(
        fail(&dir, &["approve", &id, "--agent", "rev"]),
        "error: task 1 has 1 failed test(s) in rev1; request changes instead"
    );

    assert_eq!(
        run(
            &dir,
            &[
                "request-changes",
                &id,
                "--agent",
                "rev",
                "--notes",
                "fix the integration failure"
            ]
        ),
        "#1 in_progress"
    );

    // Back with the developer, who is told what to fix and which revision it was.
    let order = run(&dir, &["claim", &id, "--agent", "dev"]);
    assert_eq!(
        order,
        "#1 rev1 t\nchanges|fix the integration failure\n0|works|in|out"
    );
    assert_eq!(
        run(
            &dir,
            &[
                "submit-review",
                &id,
                "--agent",
                "dev",
                "--pass",
                "0",
                "now passes"
            ]
        ),
        "#1 review rev2"
    );
    run(&dir, &["claim", &id, "--agent", "rev"]);
    assert_eq!(run(&dir, &["approve", &id, "--agent", "rev"]), "#1 done");

    let shown = common::task(&dir, &["show", &id, "--history"]);
    assert_eq!(shown["status"], "done");
    assert!(
        shown.get("changes").is_none(),
        "resolved notes must not resurface: {shown}"
    );
    let history = shown["history"].as_array().unwrap();
    assert_eq!(history.len(), 2);
    assert_eq!(history[0]["rev"], 1);
    assert_eq!(history[0]["decision"], "changes_requested");
    assert_eq!(history[0]["notes"], "fix the integration failure");
    assert_eq!(history[0]["results"][0], "failed: integration test failed");
    assert_eq!(history[1]["rev"], 2);
    assert_eq!(history[1]["decision"], "approved");
    assert_eq!(history[1]["results"][0], "passed: now passes");
}

#[test]
fn request_changes_notes_follow_the_task_until_it_is_resubmitted() {
    let dir = board();
    let id = submitted(&dir);
    run(&dir, &["claim", &id, "--agent", "rev"]);
    run(
        &dir,
        &[
            "request-changes",
            &id,
            "--agent",
            "rev",
            "--notes",
            "add a test",
        ],
    );

    let shown = common::task(&dir, &["show", &id]);
    assert_eq!(shown["status"], "in_progress");
    assert_eq!(shown["changes"], "add a test");
    assert_eq!(shown["rev"], 1);
    assert!(shown.get("executor").is_none());
    // Last revision's verdicts are history now, not part of the live criteria.
    assert!(shown["tests"][0].get("result").is_none());

    // Any developer can pick the rework up, and gets the notes.
    let order = common::task(&dir, &["claim", &id, "--agent", "dev2"]);
    assert_eq!(order["changes"], "add a test");
}

#[test]
fn request_changes_needs_non_empty_notes() {
    let dir = board();
    let id = submitted(&dir);
    run(&dir, &["claim", &id, "--agent", "rev"]);

    for notes in ["", "   "] {
        assert_eq!(
            fail(
                &dir,
                &["request-changes", &id, "--agent", "rev", "--notes", notes]
            ),
            "error: request-changes needs --notes saying what to fix"
        );
    }
    let usage = common::usage_error(&dir, &["request-changes", &id, "--agent", "rev"]);
    assert!(usage.contains("--notes <NOTES>"), "{usage}");
    assert_eq!(common::task(&dir, &["show", &id])["status"], "review");
}

#[test]
fn only_the_reviewer_holding_the_task_can_decide() {
    let dir = board();
    register(&dir, "rev2", "reviewer");
    let id = submitted(&dir);

    // Nobody holds it yet.
    assert_eq!(
        fail(&dir, &["approve", &id, "--agent", "rev"]),
        "error: task 1 is not claimed by 'rev'"
    );
    run(&dir, &["claim", &id, "--agent", "rev"]);
    assert_eq!(
        fail(&dir, &["approve", &id, "--agent", "rev2"]),
        "error: task 1 is claimed by 'rev', not 'rev2'"
    );
    assert_eq!(
        fail(
            &dir,
            &["request-changes", &id, "--agent", "rev2", "--notes", "x"]
        ),
        "error: task 1 is claimed by 'rev', not 'rev2'"
    );
    assert_eq!(common::task(&dir, &["show", &id])["status"], "review");
}

#[test]
fn decisions_need_a_task_in_review() {
    let dir = board();
    let id = claimed(&dir);

    assert_eq!(
        fail(&dir, &["approve", &id, "--agent", "rev"]),
        "error: task 1 is in_progress; approve needs review"
    );
    assert_eq!(
        fail(
            &dir,
            &["request-changes", &id, "--agent", "rev", "--notes", "x"]
        ),
        "error: task 1 is in_progress; request-changes needs review"
    );
    assert_eq!(
        fail(&dir, &["approve", "99", "--agent", "rev"]),
        "error: task 99 not found"
    );
}

#[test]
fn a_reviewer_who_let_go_of_the_task_can_no_longer_decide() {
    let dir = board();
    let id = submitted(&dir);
    run(&dir, &["claim", &id, "--agent", "rev"]);

    assert_eq!(run(&dir, &["release", &id, "--agent", "rev"]), "#1 review");

    assert_eq!(
        fail(&dir, &["approve", &id, "--agent", "rev"]),
        "error: task 1 is not claimed by 'rev'"
    );
    // The task is available to reviewers again.
    run(&dir, &["claim", &id, "--agent", "rev"]);
    assert_eq!(run(&dir, &["approve", &id, "--agent", "rev"]), "#1 done");
}

// ---------------------------------------------------------------------------
// Roles
// ---------------------------------------------------------------------------

#[test]
fn each_role_can_only_claim_its_own_kind_of_task() {
    let dir = board();
    let todo = add_task(&dir, "fresh", "medium").to_string();
    let review = submitted(&dir);

    assert_eq!(
        fail(&dir, &["claim", &todo, "--agent", "rev"]),
        format!("error: task {todo} is todo; claim needs review")
    );
    assert_eq!(
        fail(&dir, &["claim", &review, "--agent", "dev2"]),
        format!("error: task {review} is review; claim needs todo or in_progress")
    );
    run(&dir, &["claim", &review, "--agent", "rev"]);
    run(&dir, &["approve", &review, "--agent", "rev"]);
    assert_eq!(
        fail(&dir, &["claim", &review, "--agent", "dev2"]),
        format!("error: task {review} is done; claim needs todo or in_progress")
    );
    assert_eq!(
        fail(&dir, &["claim", &review, "--agent", "rev"]),
        format!("error: task {review} is done; claim needs review")
    );
}

#[test]
fn developers_cannot_decide_and_reviewers_cannot_submit() {
    let dir = board();
    let id = claimed(&dir);

    assert_eq!(
        fail(&dir, &["approve", &id, "--agent", "dev"]),
        "error: agent 'dev' has role 'developer'; reviewer role is required"
    );
    assert_eq!(
        fail(
            &dir,
            &["request-changes", &id, "--agent", "dev", "--notes", "x"]
        ),
        "error: agent 'dev' has role 'developer'; reviewer role is required"
    );
    assert_eq!(
        fail(
            &dir,
            &["submit-review", &id, "--agent", "rev", "--pass", "0", "ok"]
        ),
        "error: agent 'rev' has role 'reviewer'; developer role is required"
    );
}

#[test]
fn unregistered_agents_are_refused_everywhere_and_change_nothing() {
    let dir = board();
    let id = add_task(&dir, "t", "low").to_string();
    // The hint names the role to register when the command needs a specific one.
    let hint = |role: &str| {
        format!(
            "error: agent 'ghost' is not registered; run `agent-kanban agent register ghost --role {role}` first"
        )
    };
    let either = hint("developer|reviewer");

    for (args, refusal) in [
        (vec!["claim", &id, "--agent", "ghost"], &either),
        (vec!["claim-next", "--agent", "ghost"], &either),
        (vec!["release", &id, "--agent", "ghost"], &either),
        (
            vec![
                "submit-review",
                &id,
                "--agent",
                "ghost",
                "--pass",
                "0",
                "ok",
            ],
            &hint("developer"),
        ),
        (vec!["approve", &id, "--agent", "ghost"], &hint("reviewer")),
        (
            vec!["request-changes", &id, "--agent", "ghost", "--notes", "x"],
            &hint("reviewer"),
        ),
    ] {
        assert_eq!(&fail(&dir, &args), refusal, "{args:?}");
    }

    let shown = common::task(&dir, &["show", &id]);
    assert_eq!(shown["status"], "todo");
    assert!(shown.get("executor").is_none());
}

// ---------------------------------------------------------------------------
// Claims and leases
// ---------------------------------------------------------------------------

#[test]
fn a_claimed_task_cannot_be_taken_by_someone_else() {
    let dir = board();
    let id = claimed(&dir);

    let refusal = fail(&dir, &["claim", &id, "--agent", "dev2"]);

    assert!(
        refusal.starts_with("error: task 1 is already claimed by 'dev' until 20"),
        "{refusal}"
    );
    assert!(refusal.ends_with(" UTC"), "{refusal}");
    let shown = common::task(&dir, &["show", &id]);
    assert_eq!(shown["executor"], "dev");
}

#[test]
fn claiming_unknown_tasks_is_refused() {
    let dir = board();
    assert_eq!(
        fail(&dir, &["claim", "99", "--agent", "dev"]),
        "error: task 99 not found"
    );
}

#[test]
fn claiming_your_own_task_again_renews_the_lease() {
    let dir = board();
    let id = add_task(&dir, "t", "medium").to_string();
    run(&dir, &["claim", &id, "--agent", "dev", "--lease", "60"]);
    let short = lease_of(&dir, &id);

    let again = run(&dir, &["claim", &id, "--agent", "dev", "--lease", "7200"]);

    assert!(again.starts_with("#1 "), "{again}");
    assert!(lease_of(&dir, &id) > short, "the lease must move forward");
    assert_eq!(common::task(&dir, &["show", &id])["executor"], "dev");
}

#[test]
fn the_default_lease_is_one_hour_and_zero_is_refused() {
    let dir = board();
    let id = add_task(&dir, "t", "medium").to_string();
    assert_eq!(
        fail(&dir, &["claim", &id, "--agent", "dev", "--lease", "0"]),
        "error: lease must be at least one second"
    );
    run(&dir, &["claim", &id, "--agent", "dev"]);

    let seconds: i64 = common::db(&dir)
        .query_row(
            "SELECT CAST(strftime('%s', lease_expires_at) AS INTEGER)
                  - CAST(strftime('%s', claimed_at) AS INTEGER)
             FROM tasks WHERE id = ?1",
            [&id],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(seconds, 3600);
}

#[test]
fn an_expired_lease_is_inactive_and_the_task_can_be_taken_over() {
    let dir = board();
    let id = claimed(&dir);
    common::expire_lease(&dir, id.parse().unwrap());

    // The expired claim no longer shows up anywhere.
    assert_eq!(run(&dir, &["list", "--executor", "dev"]), "no tasks");
    assert_eq!(
        run(&dir, &["status"]),
        "backlog 0, todo 0, in_progress 1, review 0, done 0\nagents: dev -; dev2 -; rev -"
    );
    assert!(common::task(&dir, &["show", &id]).get("executor").is_none());

    let order = run(&dir, &["claim", &id, "--agent", "dev2"]);
    assert!(order.starts_with("#1 "), "{order}");

    // The old holder has lost the task for good.
    assert_eq!(
        fail(&dir, &["release", &id, "--agent", "dev"]),
        "error: task 1 is claimed by 'dev2', not 'dev'"
    );
    assert_eq!(
        fail(
            &dir,
            &["submit-review", &id, "--agent", "dev", "--pass", "0", "ok"]
        ),
        "error: task 1 is claimed by 'dev2', not 'dev'"
    );
    assert_eq!(
        run(&dir, &["release", &id, "--agent", "dev2"]),
        "#1 in_progress"
    );
}

#[test]
fn a_holder_whose_lease_ran_out_must_claim_again_before_submitting() {
    let dir = board();
    let id = claimed(&dir);
    common::expire_lease(&dir, id.parse().unwrap());

    assert_eq!(
        fail(
            &dir,
            &["submit-review", &id, "--agent", "dev", "--pass", "0", "ok"]
        ),
        "error: agent 'dev' no longer holds task 1; its lease expired"
    );

    run(&dir, &["claim", &id, "--agent", "dev"]);
    assert_eq!(
        run(
            &dir,
            &["submit-review", &id, "--agent", "dev", "--pass", "0", "ok"]
        ),
        "#1 review rev1"
    );
}

// ---------------------------------------------------------------------------
// release
// ---------------------------------------------------------------------------

#[test]
fn release_gives_the_task_back_and_keeps_its_status() {
    let dir = board();
    let id = claimed(&dir);

    assert_eq!(
        run(&dir, &["release", &id, "--agent", "dev"]),
        "#1 in_progress"
    );

    assert_eq!(run(&dir, &["list"]), "#1 medium in_progress t");
    let shown = common::task(&dir, &["show", &id]);
    assert!(shown.get("executor").is_none());
    assert_eq!(shown["status"], "in_progress");
    // A released in_progress task is still claimable by any developer.
    run(&dir, &["claim", &id, "--agent", "dev2"]);
}

#[test]
fn release_is_refused_for_anyone_but_the_holder() {
    let dir = board();
    let id = claimed(&dir);

    assert_eq!(
        fail(&dir, &["release", &id, "--agent", "dev2"]),
        "error: task 1 is claimed by 'dev', not 'dev2'"
    );
    run(&dir, &["release", &id, "--agent", "dev"]);
    assert_eq!(
        fail(&dir, &["release", &id, "--agent", "dev"]),
        "error: task 1 is not claimed by 'dev'"
    );
    assert_eq!(
        fail(&dir, &["release", "99", "--agent", "dev"]),
        "error: task 99 not found"
    );
}

#[test]
fn a_developer_cannot_release_a_task_in_review() {
    let dir = board();
    let id = submitted(&dir);

    assert_eq!(
        fail(&dir, &["release", &id, "--agent", "dev"]),
        "error: task 1 is not claimed by 'dev'"
    );
}

// ---------------------------------------------------------------------------
// submit-review validation
// ---------------------------------------------------------------------------

#[test]
fn submit_review_needs_exactly_one_verdict_per_test() {
    let dir = board();
    let id = claimed_with_tests(&dir, 3);
    let submit = |extra: &[&str]| {
        let mut args = vec!["submit-review", id.as_str(), "--agent", "dev"];
        args.extend_from_slice(extra);
        fail(&dir, &args)
    };

    assert_eq!(
        submit(&[]),
        "error: give one --pass IDX EVIDENCE or --fail IDX EVIDENCE for every test"
    );
    assert_eq!(
        submit(&["--pass", "0", "ok"]),
        "error: task 1 has 3 tests; no result for 1,2"
    );
    assert_eq!(
        submit(&["--pass", "0", "a", "--fail", "2", "b"]),
        "error: task 1 has 3 tests; no result for 1"
    );
    assert_eq!(
        submit(&["--pass", "0", "a", "--pass", "1", "b", "--pass", "3", "c"]),
        "error: test 3 does not exist; task 1 has tests 0-2"
    );
    assert_eq!(
        submit(&[
            "--pass", "0", "a", "--fail", "0", "b", "--pass", "1", "c", "--pass", "2", "d"
        ]),
        "error: test 0 has more than one result"
    );
    assert_eq!(
        submit(&["--pass", "0", "a", "--pass", "1", "", "--pass", "2", "c"]),
        "error: evidence for test 1 must not be empty"
    );
    assert_eq!(
        submit(&["--pass", "0", "a", "--pass", "1", "b", "--fail", "2", "   "]),
        "error: evidence for test 2 must not be empty"
    );
    assert_eq!(
        submit(&["--pass", "x", "a"]),
        "error: --pass: test index 'x' must be a number"
    );
    assert_eq!(
        submit(&["--fail", "-1", "a"]),
        "error: --fail: test index '-1' must be a number"
    );
}

#[test]
fn a_refused_submit_changes_nothing() {
    let dir = board();
    let id = claimed_with_tests(&dir, 2);
    let before = lease_of(&dir, &id);

    fail(
        &dir,
        &[
            "submit-review",
            &id,
            "--agent",
            "dev",
            "--pass",
            "0",
            "only one",
        ],
    );

    let shown = common::task(&dir, &["show", &id]);
    assert_eq!(shown["status"], "in_progress");
    assert_eq!(shown["executor"], "dev");
    assert!(shown.get("rev").is_none());
    assert_eq!(lease_of(&dir, &id), before);
    let stored: i64 = common::db(&dir)
        .query_row("SELECT COUNT(*) FROM acceptance_results", [], |row| {
            row.get(0)
        })
        .unwrap();
    assert_eq!(stored, 0);
    // The developer can still finish properly afterwards.
    assert_eq!(
        run(
            &dir,
            &[
                "submit-review",
                &id,
                "--agent",
                "dev",
                "--pass",
                "0",
                "a",
                "--pass",
                "1",
                "b"
            ]
        ),
        "#1 review rev1"
    );
}

#[test]
fn only_the_holder_can_submit_and_only_from_in_progress() {
    let dir = board();
    let id = add_task(&dir, "t", "medium").to_string();

    assert_eq!(
        fail(
            &dir,
            &["submit-review", &id, "--agent", "dev", "--pass", "0", "ok"]
        ),
        "error: task 1 is todo; submit-review needs in_progress"
    );
    run(&dir, &["claim", &id, "--agent", "dev"]);
    assert_eq!(
        fail(
            &dir,
            &["submit-review", &id, "--agent", "dev2", "--pass", "0", "ok"]
        ),
        "error: task 1 is claimed by 'dev', not 'dev2'"
    );
    run(
        &dir,
        &["submit-review", &id, "--agent", "dev", "--pass", "0", "ok"],
    );
    assert_eq!(
        fail(
            &dir,
            &["submit-review", &id, "--agent", "dev", "--pass", "0", "ok"]
        ),
        "error: task 1 is review; submit-review needs in_progress"
    );
    assert_eq!(
        fail(
            &dir,
            &["submit-review", "99", "--agent", "dev", "--pass", "0", "ok"]
        ),
        "error: task 99 not found"
    );
}

// ---------------------------------------------------------------------------
// move
// ---------------------------------------------------------------------------

#[test]
fn move_parks_a_task_in_backlog_and_brings_it_back() {
    let dir = board();
    let id = add_task(&dir, "t", "medium").to_string();

    assert_eq!(run(&dir, &["move", &id, "backlog"]), "#1 backlog");
    assert_eq!(run(&dir, &["move", &id, "backlog"]), "#1 backlog");
    assert_eq!(run(&dir, &["list"]), "#1 medium backlog t");
    // Nobody can start a parked task.
    assert_eq!(
        fail(&dir, &["claim", &id, "--agent", "dev"]),
        "error: task 1 is backlog; claim needs todo or in_progress"
    );
    assert_eq!(run(&dir, &["claim-next", "--agent", "dev"]), "idle");

    assert_eq!(run(&dir, &["move", &id, "todo"]), "#1 todo");
    assert_eq!(run(&dir, &["move", &id, "todo"]), "#1 todo");
    run(&dir, &["claim", &id, "--agent", "dev"]);
}

#[test]
fn move_cannot_reach_lifecycle_statuses_or_touch_busy_tasks() {
    let dir = board();
    let id = add_task(&dir, "t", "medium").to_string();

    for status in ["done", "review", "in_progress", "bogus"] {
        assert_eq!(
            fail(&dir, &["move", &id, status]),
            format!("error: cannot move to '{status}': move only works between backlog and todo")
        );
    }
    assert_eq!(common::task(&dir, &["show", &id])["status"], "todo");

    run(&dir, &["claim", &id, "--agent", "dev"]);
    assert_eq!(
        fail(&dir, &["move", &id, "backlog"]),
        "error: task 1 is in_progress; move only works on backlog and todo tasks"
    );
    run(
        &dir,
        &["submit-review", &id, "--agent", "dev", "--pass", "0", "ok"],
    );
    assert_eq!(
        fail(&dir, &["move", &id, "todo"]),
        "error: task 1 is review; move only works on backlog and todo tasks"
    );
    run(&dir, &["claim", &id, "--agent", "rev"]);
    run(&dir, &["approve", &id, "--agent", "rev"]);
    assert_eq!(
        fail(&dir, &["move", &id, "backlog"]),
        "error: task 1 is done; move only works on backlog and todo tasks"
    );
    assert_eq!(
        fail(&dir, &["move", "99", "todo"]),
        "error: task 99 not found"
    );
}

// ---------------------------------------------------------------------------
// edit and remove guards
// ---------------------------------------------------------------------------

#[test]
fn edit_and_remove_are_refused_while_the_task_is_claimed() {
    let dir = board();
    let id = claimed(&dir);

    assert_eq!(
        fail(&dir, &["edit", &id, "--title", "x"]),
        "error: task 1 is claimed; release it before editing"
    );
    assert_eq!(
        fail(&dir, &["remove", &id]),
        "error: task 1 is claimed; release it before removing"
    );

    run(&dir, &["release", &id, "--agent", "dev"]);
    assert_eq!(run(&dir, &["edit", &id, "--title", "x"]), "#1 in_progress");
    assert_eq!(run(&dir, &["remove", &id]), "#1 removed");
}

#[test]
fn edit_and_remove_are_refused_in_review() {
    let dir = board();
    let id = submitted(&dir);

    assert_eq!(
        fail(&dir, &["edit", &id, "--title", "x"]),
        "error: task 1 is in review; request changes before editing"
    );
    assert_eq!(
        fail(&dir, &["remove", &id]),
        "error: task 1 is in review; request changes before removing"
    );
}

#[test]
fn edit_and_remove_are_refused_once_done() {
    let dir = board();
    let id = submitted(&dir);
    run(&dir, &["claim", &id, "--agent", "rev"]);
    run(&dir, &["approve", &id, "--agent", "rev"]);

    assert_eq!(
        fail(&dir, &["edit", &id, "--title", "x"]),
        "error: task 1 is done; finished tasks are immutable"
    );
    assert_eq!(
        fail(&dir, &["remove", &id]),
        "error: task 1 is done; finished tasks can't be removed"
    );
    assert_eq!(common::task(&dir, &["show", &id])["title"], "t");
}

#[test]
fn a_task_sent_back_for_rework_can_be_edited_again() {
    let dir = board();
    let id = submitted(&dir);
    run(&dir, &["claim", &id, "--agent", "rev"]);
    run(
        &dir,
        &[
            "request-changes",
            &id,
            "--agent",
            "rev",
            "--notes",
            "tests too thin",
        ],
    );

    assert_eq!(
        run(
            &dir,
            &[
                "edit", &id, "--test", "new", "a", "b", "--test", "newer", "c", "d"
            ]
        ),
        "#1 in_progress"
    );

    let order = common::task(&dir, &["claim", &id, "--agent", "dev"]);
    assert_eq!(order["tests"].as_array().unwrap().len(), 2);
    assert_eq!(order["changes"], "tests too thin");
}

#[test]
fn the_initial_test_json_is_stored_exactly() {
    // Guards the `tests` column format the unit tests and `show` rely on.
    let dir = initialized();
    run(
        &dir,
        &["add", "--title", "t", "--test", "works", "in", "out"],
    );

    let stored: String = common::db(&dir)
        .query_row("SELECT tests FROM tasks WHERE id = 1", [], |row| row.get(0))
        .unwrap();

    assert_eq!(stored, ONE_TEST);
}
