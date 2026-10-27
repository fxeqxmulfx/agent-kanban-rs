//! Waiting without a loop. `claim-next --wait SECS` keeps looking while there
//! is nothing to claim and ends as soon as there is work or mail; `wait IDS`
//! ends once those tasks are done, or mail comes for whoever waits.
//! Everything runs through the compiled binary.

mod common;

use common::{Outcome, add_task, fail, initialized, kanban, register, run, usage_error};
use std::thread;
use std::time::{Duration, Instant};
use tempfile::TempDir;

/// Long enough that a wait which ends early cannot have run out of time.
const LONG: &str = "30";
const EARLY: Duration = Duration::from_secs(15);

/// A board with a developer, a reviewer and no tasks.
fn board() -> TempDir {
    let dir = initialized();
    register(&dir, "dev", "developer");
    register(&dir, "rev", "reviewer");
    dir
}

/// Run every one of `waits` in its own process, and `meanwhile` once they
/// have had time to start looking; each wait's outcome and how long it took.
fn waiting(dir: &TempDir, waits: &[&[&str]], meanwhile: impl FnOnce()) -> Vec<(Outcome, Duration)> {
    thread::scope(|scope| {
        // Every wait starts before `meanwhile` runs.
        let mut handles = Vec::new();
        for args in waits {
            handles.push(scope.spawn(move || {
                let start = Instant::now();
                let output = kanban(dir).args(*args).output().unwrap();
                let text = |bytes: &[u8]| String::from_utf8_lossy(bytes).trim_end().to_string();
                let outcome = Outcome {
                    code: output.status.code(),
                    stdout: text(&output.stdout),
                    stderr: text(&output.stderr),
                };
                (outcome, start.elapsed())
            }));
        }
        thread::sleep(Duration::from_millis(500));
        meanwhile();
        handles.into_iter().map(|h| h.join().unwrap()).collect()
    })
}

fn timed(dir: &TempDir, args: &[&str]) -> (String, Duration) {
    let start = Instant::now();
    let reply = run(dir, args);
    (reply, start.elapsed())
}

#[test]
fn a_waiting_agent_gets_the_first_task_it_can_claim() {
    let dir = board();
    let outcomes = waiting(
        &dir,
        &[&["claim-next", "--wait", LONG, "--agent", "dev"]],
        || {
            add_task(&dir, "Login", "high");
        },
    );

    let (outcome, took) = &outcomes[0];
    assert_eq!(outcome.stdout, "#1 Login\n0|works|in|out", "{outcome:?}");
    assert!(*took < EARLY, "took {took:?}");
    assert_eq!(
        run(&dir, &["list"]),
        "#1 high in_progress@dev Login",
        "the task is claimed, not just announced"
    );
}

#[test]
fn mail_ends_the_wait() {
    let dir = board();
    let outcomes = waiting(
        &dir,
        &[&["claim-next", "--wait", LONG, "--agent", "rev"]],
        || {
            run(&dir, &["send", "rev", "are you there?", "--agent", "dev"]);
        },
    );

    let (outcome, took) = &outcomes[0];
    assert_eq!(outcome.stdout, "idle\nmail|1", "{outcome:?}");
    assert!(*took < EARLY, "took {took:?}");
}

/// Unread mail ends a wait before it starts: an agent reads its mail first.
#[test]
fn mail_already_waiting_ends_the_wait_at_once() {
    let dir = board();
    run(&dir, &["send", "dev", "first read this", "--agent", "rev"]);

    let (reply, took) = timed(&dir, &["claim-next", "--wait", LONG, "--agent", "dev"]);
    assert_eq!(reply, "idle\nmail|1");
    assert!(took < EARLY, "took {took:?}");

    run(&dir, &["inbox", "--agent", "dev"]);
    let (reply, took) = timed(&dir, &["claim-next", "--wait", "1", "--agent", "dev"]);
    assert_eq!(reply, "idle");
    assert!(took >= Duration::from_secs(1), "waited again: {took:?}");
}

#[test]
fn a_wait_ends_with_the_last_idle_when_its_time_is_up() {
    let dir = board();
    add_task(&dir, "Held", "low");
    run(&dir, &["claim", "1", "--agent", "dev"]);

    let (reply, took) = timed(&dir, &["claim-next", "--wait", "2", "--agent", "rev"]);
    assert_eq!(reply, "idle, 1 open");
    assert!(
        took >= Duration::from_secs(2) && took < EARLY,
        "took {took:?}"
    );

    let (reply, took) = timed(&dir, &["claim-next", "--wait", "0", "--agent", "rev"]);
    assert_eq!(reply, "idle, 1 open", "--wait 0 does not wait");
    assert!(took < Duration::from_secs(1), "took {took:?}");
}

/// Work that is there already, including the task an agent holds, comes back
/// at once, as from a plain `claim-next`.
#[test]
fn work_at_hand_does_not_wait() {
    let dir = board();
    add_task(&dir, "Login", "high");

    for _ in 0..2 {
        let (reply, took) = timed(&dir, &["claim-next", "--wait", LONG, "--agent", "dev"]);
        assert_eq!(reply, "#1 Login\n0|works|in|out");
        assert!(took < EARLY, "took {took:?}");
    }
}

/// Agents waiting together for one new task: one gets it, the others wait on
/// until their time is up.
#[test]
fn of_several_waiting_agents_exactly_one_gets_a_new_task() {
    let dir = board();
    register(&dir, "dev2", "developer");
    register(&dir, "dev3", "developer");
    let waits: Vec<Vec<&str>> = ["dev", "dev2", "dev3"]
        .iter()
        .map(|agent| vec!["claim-next", "--wait", "3", "--agent", agent])
        .collect();
    let waits: Vec<&[&str]> = waits.iter().map(Vec::as_slice).collect();

    let outcomes = waiting(&dir, &waits, || {
        add_task(&dir, "Only one", "low");
    });

    let mut replies: Vec<&str> = outcomes.iter().map(|(o, _)| o.stdout.as_str()).collect();
    replies.sort_unstable();
    assert_eq!(
        replies,
        [
            "#1 Only one\n0|works|in|out",
            "idle, 1 open",
            "idle, 1 open"
        ],
        "{outcomes:?}"
    );
}

/// An error ends a wait at once, also one that comes up while waiting.
#[test]
fn an_agent_removed_while_waiting_stops_waiting() {
    let dir = board();
    let outcomes = waiting(
        &dir,
        &[&["claim-next", "--wait", LONG, "--agent", "dev"]],
        || {
            run(&dir, &["agent", "remove", "dev"]);
        },
    );

    let (outcome, took) = &outcomes[0];
    assert_eq!(outcome.code, Some(1), "{outcome:?}");
    assert!(
        outcome
            .stderr
            .starts_with("error: agent 'dev' is not registered"),
        "{outcome:?}"
    );
    assert!(*took < EARLY, "took {took:?}");
}

/// A board with tasks #1 (high) and #2 (low), and a lead registered for mail.
fn planned() -> TempDir {
    let dir = board();
    register(&dir, "lead", "developer");
    add_task(&dir, "First", "high");
    add_task(&dir, "Second", "low");
    dir
}

/// The whole road of task `id` to done: claimed, submitted, approved.
fn finish(dir: &TempDir, id: &str) {
    run(dir, &["claim", id, "--agent", "dev"]);
    run(
        dir,
        &["submit-review", id, "--pass", "0", "ok", "--agent", "dev"],
    );
    run(dir, &["claim", id, "--agent", "rev"]);
    run(dir, &["approve", id, "--agent", "rev"]);
}

#[test]
fn a_lead_waits_until_every_task_is_done() {
    let dir = planned();
    let outcomes = waiting(&dir, &[&["wait", "1,2", "--timeout", LONG]], || {
        finish(&dir, "2");
        finish(&dir, "1");
    });

    let (outcome, took) = &outcomes[0];
    assert_eq!(
        outcome.stdout, "#1 high done First\n#2 low done Second",
        "{outcome:?}"
    );
    assert!(*took < EARLY, "took {took:?}");
}

/// One task done is not all of them; when the time is up, the lines say how
/// far the work got.
#[test]
fn a_wait_that_runs_out_of_time_shows_where_the_tasks_are() {
    let dir = planned();
    finish(&dir, "2");
    run(&dir, &["claim", "1", "--agent", "dev"]);

    let (reply, took) = timed(&dir, &["wait", "2", "1", "--timeout", "1"]);
    assert_eq!(reply, "#1 high in_progress@dev First\n#2 low done Second");
    assert!(
        took >= Duration::from_secs(1) && took < EARLY,
        "took {took:?}"
    );

    finish(&dir, "1");
    let (reply, took) = timed(&dir, &["wait", "#1", "--timeout", LONG]);
    assert_eq!(reply, "#1 high done First", "done already: no wait");
    assert!(took < EARLY, "took {took:?}");
}

/// Named by `--agent` or the environment, the waiter is also woken by its
/// mail, which a worker may send to ask a question; unnamed, it is not.
#[test]
fn mail_ends_the_wait_of_a_named_waiter() {
    let dir = planned();
    let outcomes = waiting(
        &dir,
        &[&["wait", "1", "--timeout", LONG, "--agent", "lead"]],
        || {
            run(
                &dir,
                &["send", "lead", "per IP?", "--task", "1", "--agent", "dev"],
            );
        },
    );
    let (outcome, took) = &outcomes[0];
    assert_eq!(outcome.stdout, "#1 high todo First\nmail|1", "{outcome:?}");
    assert!(*took < EARLY, "took {took:?}");

    let output = kanban(&dir)
        .env("AGENT_KANBAN_AGENT", "lead")
        .args(["wait", "1", "--timeout", LONG])
        .output()
        .unwrap();
    assert_eq!(
        String::from_utf8(output.stdout).unwrap(),
        "#1 high todo First\nmail|1\n",
        "the mail is still unread"
    );

    let (reply, took) = timed(&dir, &["wait", "1", "--timeout", "1"]);
    assert_eq!(reply, "#1 high todo First", "nobody named, no mail");
    assert!(took >= Duration::from_secs(1), "took {took:?}");
}

#[test]
fn what_cannot_be_waited_for_is_refused() {
    let dir = planned();
    assert_eq!(
        fail(&dir, &["wait", "1,9", "--timeout", LONG]),
        "error: task 9 not found"
    );
    assert!(
        fail(&dir, &["wait", "1", "--agent", "zed"])
            .starts_with("error: agent 'zed' is not registered"),
    );
    assert!(usage_error(&dir, &["wait"]).contains("<IDS>"));
    assert!(usage_error(&dir, &["wait", "0"]).contains("'0' is not a task id"));
}

/// A task removed while it is waited for can never be done: the wait ends
/// with an error instead of running out of time.
#[test]
fn a_task_removed_while_waited_for_ends_the_wait() {
    let dir = planned();
    let outcomes = waiting(&dir, &[&["wait", "1,2", "--timeout", LONG]], || {
        run(&dir, &["remove", "2"]);
    });

    let (outcome, took) = &outcomes[0];
    assert_eq!(outcome.code, Some(1), "{outcome:?}");
    assert_eq!(outcome.stderr, "error: task 2 not found");
    assert!(*took < EARLY, "took {took:?}");
}
