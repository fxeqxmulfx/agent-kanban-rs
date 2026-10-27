//! Mail between agents: `send NAME TEXT [--task ID]` leaves a message on the
//! board, and `inbox` hands an agent its new messages, oldest first, each one
//! once. Agents that share a board and nothing else talk this way.

mod common;

use common::{add_task, argv, fail, initialized, kanban, race, register, run, usage_error};
use tempfile::TempDir;

/// A board with a developer, a reviewer and task 1.
fn board() -> TempDir {
    let dir = initialized();
    register(&dir, "dev", "developer");
    register(&dir, "rev", "reviewer");
    add_task(&dir, "Login", "high");
    dir
}

/// `send ARGS --agent FROM`.
fn send(dir: &TempDir, from: &str, args: &[&str]) -> String {
    run(dir, &[&["send"], args, &["--agent", from]].concat())
}

fn inbox(dir: &TempDir, me: &str) -> String {
    run(dir, &["inbox", "--agent", me])
}

/// Each line of `inbox` replies, the `no mail` ones left out.
fn delivered<'a>(replies: impl IntoIterator<Item = &'a str>) -> Vec<String> {
    replies
        .into_iter()
        .filter(|reply| *reply != "no mail")
        .flat_map(str::lines)
        .map(String::from)
        .collect()
}

#[test]
fn mail_reaches_its_agent_once_oldest_first() {
    let dir = board();
    assert_eq!(inbox(&dir, "rev"), "no mail");

    assert_eq!(
        send(&dir, "dev", &["rev", "Is the limit per IP?", "--task", "1"]),
        "sent"
    );
    send(&dir, "dev", &["rev", "or per user?"]);

    assert_eq!(inbox(&dir, "dev"), "no mail", "mail is for its addressee");
    assert_eq!(
        inbox(&dir, "rev"),
        "dev|#1|Is the limit per IP?\ndev||or per user?"
    );
    assert_eq!(inbox(&dir, "rev"), "no mail");

    send(&dir, "rev", &["dev", "per IP", "--task", "1"]);
    assert_eq!(inbox(&dir, "dev"), "rev|#1|per IP");
}

/// As everywhere else, the agent may be named by the environment.
#[test]
fn the_environment_names_the_agent() {
    let dir = board();
    let developer = |args: &[&str]| {
        let output = kanban(&dir)
            .env("AGENT_KANBAN_AGENT", "dev")
            .args(args)
            .output()
            .unwrap();
        assert!(output.status.success(), "{output:?}");
        String::from_utf8(output.stdout).unwrap()
    };

    assert_eq!(developer(&["send", "rev", "hi"]), "sent\n");
    assert_eq!(inbox(&dir, "rev"), "dev||hi");
    send(&dir, "rev", &["dev", "hello"]);
    assert_eq!(developer(&["inbox"]), "rev||hello\n");
}

/// Text is kept as it was sent and comes back escaped like every cell of a
/// reply. A leading `-` is text; after `--`, so is a flag's name.
#[test]
fn any_text_arrives_as_it_was_sent() {
    let dir = board();

    send(&dir, "dev", &["rev", "line one\nline | two \\ end"]);
    send(&dir, "dev", &["rev", "-5 is not a flag"]);
    run(&dir, &["send", "--agent", "dev", "rev", "--", "--task"]);

    assert_eq!(
        inbox(&dir, "rev"),
        [
            r"dev||line one\nline \| two \\ end",
            "dev||-5 is not a flag",
            "dev||--task",
        ]
        .join("\n")
    );
}

#[test]
fn what_cannot_be_delivered_is_refused_and_nothing_is_kept() {
    let dir = board();
    let unknown = "error: agent 'zed' is not registered; run `agent-kanban agent register zed \
                   --role developer|reviewer` first";

    for (args, expected) in [
        (
            &["send", "carol", "hi", "--agent", "dev"][..],
            "error: agent 'carol' not found; `agent-kanban agent list` shows who is registered",
        ),
        (
            &["send", "rev", " \n\t", "--agent", "dev"],
            "error: message must not be empty",
        ),
        (
            &["send", "rev", "hi", "--task", "2", "--agent", "dev"],
            "error: task 2 not found",
        ),
        (&["send", "rev", "hi", "--agent", "zed"], unknown),
        (&["inbox", "--agent", "zed"], unknown),
        (
            &["send", "rev", "hi"],
            "error: no agent name: pass --agent NAME or set AGENT_KANBAN_AGENT",
        ),
    ] {
        assert_eq!(fail(&dir, args), expected, "{args:?}");
    }
    let zero = usage_error(
        &dir,
        &["send", "rev", "hi", "--task", "0", "--agent", "dev"],
    );
    assert!(zero.contains("'0' is not a task id"), "{zero}");
    let no_text = usage_error(&dir, &["send", "rev", "--agent", "dev"]);
    assert!(no_text.contains("<TEXT>"), "{no_text}");

    assert_eq!(inbox(&dir, "rev"), "no mail");
}

/// Mail is kept by name: it outlives the agent that sent it and the task it
/// is about, and mail for a removed agent waits for its name to come back.
#[test]
fn mail_outlives_the_agents_and_tasks_it_names() {
    let dir = board();
    register(&dir, "temp", "developer");
    send(&dir, "temp", &["rev", "handing #1 back", "--task", "1"]);
    send(&dir, "rev", &["temp", "thanks", "--task", "1"]);

    run(&dir, &["agent", "remove", "temp"]);
    run(&dir, &["remove", "1"]);

    assert_eq!(inbox(&dir, "rev"), "temp|#1|handing #1 back");
    assert_eq!(
        fail(&dir, &["send", "temp", "still there?", "--agent", "rev"]),
        "error: agent 'temp' not found; `agent-kanban agent list` shows who is registered"
    );
    register(&dir, "temp", "reviewer");
    assert_eq!(inbox(&dir, "temp"), "rev|#1|thanks");
    // Ids are never reused, so the #1 of that mail names no other task.
    assert_eq!(add_task(&dir, "after", "low"), 2);
}

/// Agents send and read at once: every message arrives, and none twice,
/// because `inbox` marks what it prints as read in the same transaction.
#[test]
fn mail_sent_and_read_at_once_arrives_exactly_once() {
    const ROUNDS: usize = 5;
    const SENDS: usize = 6;
    for round in 0..ROUNDS {
        let dir = board();
        let texts: Vec<String> = (0..SENDS).map(|n| format!("message {n}")).collect();
        let mut commands: Vec<Vec<String>> = texts
            .iter()
            .map(|text| argv(&["send", "rev", text, "--agent", "dev"]))
            .collect();
        commands.extend((0..3).map(|_| argv(&["inbox", "--agent", "rev"])));

        let outcomes = race(&dir, &commands);

        for outcome in &outcomes {
            assert!(outcome.ok(), "round {round}: {outcome:?}");
        }
        for outcome in &outcomes[..SENDS] {
            assert_eq!(outcome.stdout, "sent", "round {round}");
        }
        let rest = inbox(&dir, "rev");
        let replies = outcomes[SENDS..]
            .iter()
            .map(|outcome| outcome.stdout.as_str())
            .chain([rest.as_str()]);
        let mut got = delivered(replies);
        got.sort();
        let expected: Vec<String> = texts.iter().map(|text| format!("dev||{text}")).collect();
        assert_eq!(got, expected, "round {round}");
        assert_eq!(inbox(&dir, "rev"), "no mail", "round {round}");
    }
}
