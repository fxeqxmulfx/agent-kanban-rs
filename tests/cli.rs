//! The command-line surface: the guide, help and version screens, how bad input
//! is rejected, the 0.2 interface being gone, and how an agent says who it is.
//! What the commands do to the board is tested in the other files.

mod common;

use common::{add_task, fail, initialized, kanban, project, register, run, usage_error};
use std::collections::HashSet;
use std::path::Path;

const GUIDE: &str = include_str!("../src/commands/guide.txt");
const NO_IDENTITY: &str = "error: no agent name: pass --agent NAME or set AGENT_KANBAN_AGENT";
const REQUIRED: &str = "error: the following required arguments were not provided:";

/// Commands that act as an agent, so need `--agent` or `AGENT_KANBAN_AGENT`.
const AGENT_COMMANDS: [&str; 6] = [
    "claim",
    "claim-next",
    "release",
    "submit-review",
    "approve",
    "request-changes",
];

// ---------------------------------------------------------------------------
// Reading the help screens
// ---------------------------------------------------------------------------

/// Names in a help screen's `Commands:` section (`help` left out).
fn listed_commands(help: &str) -> Vec<String> {
    help.lines()
        .skip_while(|line| *line != "Commands:")
        .skip(1)
        .take_while(|line| !line.is_empty())
        .filter_map(|line| line.split_whitespace().next())
        .filter(|name| *name != "help")
        .map(String::from)
        .collect()
}

/// Every command the binary has, as the words typed to reach it: `["init"]`,
/// `["agent"]`, `["agent", "register"]`, ...
fn command_paths(dir: &Path) -> Vec<Vec<String>> {
    let mut paths = Vec::new();
    for name in listed_commands(&run(dir, &["--help"])) {
        paths.push(vec![name.clone()]);
        for sub in listed_commands(&run(dir, &[&name, "--help"])) {
            paths.push(vec![name.clone(), sub]);
        }
    }
    paths
}

/// The root screen plus one for every command.
fn all_screens(dir: &Path) -> Vec<Vec<String>> {
    let mut screens = vec![vec![]];
    screens.extend(command_paths(dir));
    screens
}

/// `--help` of the command reached by `path` (the root when empty).
fn help_of(dir: &Path, path: &[String]) -> String {
    let mut args: Vec<&str> = path.iter().map(String::as_str).collect();
    args.push("--help");
    run(dir, &args)
}

/// The `--long` flags a help screen documents, `--help` aside.
fn long_flags(help: &str) -> Vec<String> {
    help.lines()
        .filter(|line| line.starts_with("  ") && line.trim_start().starts_with('-'))
        .flat_map(|line| {
            let names = line.trim_start().split("  ").next().unwrap_or_default();
            names
                .split([',', ' '])
                .filter(|word| word.starts_with("--") && *word != "--help")
                .map(String::from)
                .collect::<Vec<_>>()
        })
        .collect()
}

// ---------------------------------------------------------------------------
// The guide
// ---------------------------------------------------------------------------

#[test]
fn the_guide_prints_the_embedded_text_and_needs_no_board() {
    let dir = project();

    assert_eq!(run(&dir, &["guide"]), GUIDE.trim_end());
}

/// Every agent reads the guide at the start of every session, so its size is
/// part of the design, not an accident.
#[test]
fn the_guide_stays_short() {
    assert!(
        GUIDE.len() <= 2_000,
        "the guide grew to {} bytes",
        GUIDE.len()
    );
}

/// The guide is the only documentation an agent is told to read, so it must not
/// drift from the commands the binary actually has.
#[test]
fn the_guide_mentions_every_command_and_flag() {
    let dir = project();
    let words: HashSet<&str> = GUIDE
        .split(|c: char| !(c.is_alphanumeric() || c == '-'))
        .collect();

    for path in command_paths(dir.path()) {
        let name = path.last().unwrap();
        assert!(
            words.contains(name.as_str()),
            "the guide never mentions `{name}`"
        );
        for flag in long_flags(&help_of(dir.path(), &path)) {
            assert!(
                words.contains(flag.as_str()),
                "the guide never mentions `{flag}` (of {path:?})"
            );
        }
    }
}

/// The checks above would pass vacuously if these readers found nothing.
#[test]
fn the_help_readers_find_the_commands_and_flags() {
    let dir = project();

    let paths = command_paths(dir.path());

    assert!(paths.len() >= 19, "{paths:?}");
    assert!(paths.contains(&vec!["claim-next".to_string()]));
    assert!(paths.contains(&vec!["agent".to_string(), "register".to_string()]));
    assert_eq!(
        long_flags(&help_of(dir.path(), &["add".to_string()])),
        [
            "--db",
            "--title",
            "--priority",
            "--tag",
            "--test",
            "--after"
        ]
    );
}

// ---------------------------------------------------------------------------
// Help and version
// ---------------------------------------------------------------------------

#[test]
fn every_help_screen_is_plain_text_on_stdout() {
    let dir = project();

    for path in all_screens(dir.path()) {
        let mut args: Vec<&str> = path.iter().map(String::as_str).collect();
        args.push("--help");
        let output = kanban(&dir)
            .env_remove("CLICOLOR_FORCE")
            .env_remove("FORCE_COLOR")
            .args(&args)
            .output()
            .unwrap();

        assert_eq!(output.status.code(), Some(0), "{args:?}");
        assert!(output.stderr.is_empty(), "{args:?}");
        let stdout = String::from_utf8(output.stdout).unwrap();
        assert!(stdout.contains("Usage: agent-kanban"), "{args:?}: {stdout}");
        assert!(!stdout.contains('\u{1b}'), "{args:?} printed colour codes");
    }
}

#[test]
fn the_help_command_and_the_short_flag_show_the_same_screens() {
    let dir = project();

    assert_eq!(run(&dir, &["-h"]), run(&dir, &["--help"]));
    assert_eq!(run(&dir, &["help"]), run(&dir, &["--help"]));
    assert_eq!(run(&dir, &["claim", "-h"]), run(&dir, &["claim", "--help"]));
    assert_eq!(
        run(&dir, &["help", "claim"]),
        run(&dir, &["claim", "--help"])
    );
    assert_eq!(
        run(&dir, &["help", "agent", "register"]),
        run(&dir, &["agent", "register", "--help"])
    );
}

#[test]
fn every_command_option_and_argument_is_described() {
    let dir = project();

    for path in all_screens(dir.path()) {
        let help = help_of(dir.path(), &path);

        assert!(
            !help.starts_with("Usage:"),
            "{path:?} has no description line"
        );
        for line in help.lines().filter(|line| line.starts_with("  ")) {
            let described = line
                .trim_start()
                .split_once("  ")
                .is_some_and(|(_, text)| !text.trim().is_empty());
            assert!(described, "{path:?}: no description in `{line}`");
        }
    }
}

#[test]
fn commands_that_act_as_an_agent_document_how_to_name_one() {
    let dir = project();

    for command in AGENT_COMMANDS {
        let help = run(&dir, &[command, "--help"]);

        assert!(help.contains("--agent <NAME>"), "{command}: {help}");
        assert!(
            help.contains("[env: AGENT_KANBAN_AGENT="),
            "{command}: {help}"
        );
    }
}

#[test]
fn the_version_flags_print_the_package_version() {
    let dir = project();
    let expected = format!("agent-kanban {}", env!("CARGO_PKG_VERSION"));

    assert_eq!(run(&dir, &["--version"]), expected);
    assert_eq!(run(&dir, &["-V"]), expected);
}

#[test]
fn no_arguments_is_a_usage_error_that_lists_the_commands() {
    let dir = project();

    for (args, usage) in [
        (&[][..], "Usage: agent-kanban [OPTIONS] <COMMAND>"),
        (
            &["agent"][..],
            "Usage: agent-kanban agent [OPTIONS] <COMMAND>",
        ),
    ] {
        let output = kanban(&dir).args(args).output().unwrap();

        assert_eq!(output.status.code(), Some(2), "{args:?}");
        assert!(output.stdout.is_empty(), "{args:?}");
        let stderr = String::from_utf8(output.stderr).unwrap();
        assert!(stderr.contains(usage), "{args:?}: {stderr}");
        assert!(stderr.contains("Commands:"), "{args:?}: {stderr}");
    }
}

// ---------------------------------------------------------------------------
// Rejected input
// ---------------------------------------------------------------------------

#[test]
fn bad_arguments_are_usage_errors_with_a_clear_first_line() {
    let dir = initialized();
    let test = ["--test", "d", "i", "o"];
    let with_test = |extra: &[&'static str]| -> Vec<&'static str> {
        let mut args = vec!["add", "--title", "t"];
        args.extend(test);
        args.extend_from_slice(extra);
        args
    };
    let cases: Vec<(Vec<&str>, &str)> = vec![
        (
            vec!["add", "--bogus"],
            "error: unexpected argument '--bogus' found",
        ),
        (vec!["add", "--title", "t"], REQUIRED),
        (vec!["add", "--test", "d", "i", "o"], REQUIRED),
        (
            vec!["add", "--title", "t", "--test", "d", "i"],
            "error: 3 values required for '--test <DESC> <INPUT> <OUTPUT>' but 2 were provided",
        ),
        (
            with_test(&["extra"]),
            "error: unexpected argument 'extra' found",
        ),
        (
            with_test(&["--after", "x"]),
            "error: invalid value 'x' for '--after <IDS>': 'x' is not a task id",
        ),
        (
            vec!["submit-review", "1", "--pass", "0"],
            "error: 2 values required for '--pass <IDX> <EVIDENCE>' but 1 was provided",
        ),
        (
            vec!["submit-review", "1", "--fail"],
            "error: a value is required for '--fail <IDX> <EVIDENCE>' but none was supplied",
        ),
        (vec!["request-changes", "1"], REQUIRED),
        (vec!["move", "1"], REQUIRED),
        (vec!["show"], REQUIRED),
        (
            vec!["show", "abc"],
            "error: invalid value 'abc' for '<ID>': 'abc' is not a task id",
        ),
        (
            vec!["show", "0"],
            "error: invalid value '0' for '<ID>': '0' is not a task id",
        ),
        (
            vec!["show", "--", "-3"],
            "error: invalid value '-3' for '<ID>': '-3' is not a task id",
        ),
        (
            vec!["show", "1.5"],
            "error: invalid value '1.5' for '<ID>': '1.5' is not a task id",
        ),
        (
            vec!["list", "--limit", "x"],
            "error: invalid value 'x' for '--limit <N>': invalid digit found in string",
        ),
        (
            vec!["claim", "1", "--lease", "x"],
            "error: invalid value 'x' for '--lease <SECS>': invalid digit found in string",
        ),
        (vec!["nosuch"], "error: unrecognized subcommand 'nosuch'"),
        (
            vec!["agent", "nosuch"],
            "error: unrecognized subcommand 'nosuch'",
        ),
    ];

    for (args, first_line) in &cases {
        let message = usage_error(&dir, args);

        assert_eq!(message.lines().next(), Some(*first_line), "{args:?}");
        assert!(
            !message.contains("For more information"),
            "{args:?}: {message}"
        );
    }
}

#[test]
fn a_usage_error_ends_with_the_usage_line_of_the_command() {
    let dir = initialized();

    assert_eq!(
        usage_error(&dir, &["add", "--title", "t"]),
        "error: the following required arguments were not provided:\n  \
         --test <DESC> <INPUT> <OUTPUT>\n\n\
         Usage: agent-kanban add --title <TITLE> --test <DESC> <INPUT> <OUTPUT>"
    );
    assert_eq!(
        usage_error(&dir, &["nosuch"]),
        "error: unrecognized subcommand 'nosuch'\n\nUsage: agent-kanban [OPTIONS] <COMMAND>"
    );
}

#[test]
fn usage_errors_carry_no_colour_codes_even_when_colour_is_forced() {
    let dir = initialized();

    let output = kanban(&dir)
        .env("CLICOLOR_FORCE", "1")
        .args(["add", "--bogus"])
        .output()
        .unwrap();

    assert_eq!(output.status.code(), Some(2));
    assert!(!String::from_utf8(output.stderr).unwrap().contains('\u{1b}'));
}

/// Everything 0.2 had that 0.3 deliberately dropped must now be refused by the
/// parser, not silently ignored.
#[test]
fn the_0_2_interface_is_gone() {
    let dir = initialized();
    let json_test = r#"{"describe":"d","input":"i","output":"o"}"#;
    let removed: &[&[&str]] = &[
        &["list", "--pretty"],
        &["--pretty", "list"],
        &["list", "--table"],
        &["transitions"],
        &["claim-review", "1"],
        &["list", "--sort", "priority"],
        &["claim", "1", "--lease-seconds", "5"],
        &["move", "1", "--status", "todo"],
        &["submit-review", "1", "--result", "[]"],
        &["add", "--title", "t", "--test", json_test],
    ];

    for args in removed {
        usage_error(&dir, args);
    }
}

// ---------------------------------------------------------------------------
// Argument forms
// ---------------------------------------------------------------------------

/// Replies print `#7`; an agent can paste that straight back into a command.
#[test]
fn ids_may_be_written_the_way_replies_print_them() {
    let dir = initialized();
    register(&dir, "dev", "developer");
    add_task(&dir, "first", "medium");
    add_task(&dir, "second", "medium");

    assert_eq!(run(&dir, &["show", "#1"]), run(&dir, &["show", "1"]));
    assert_eq!(
        run(
            &dir,
            &[
                "add", "--title", "third", "--test", "d", "i", "o", "--after", "#1,#2"
            ]
        ),
        "#3 todo after:1,2"
    );
    assert_eq!(
        run(&dir, &["edit", "#3", "--drop-after", "#2"]),
        "#3 todo after:1"
    );
    assert_eq!(run(&dir, &["move", "#2", "backlog"]), "#2 backlog");
    assert_eq!(
        common::task(&dir, &["claim", "#1", "--agent", "dev"])["id"],
        1
    );
    assert_eq!(
        run(&dir, &["release", "#1", "--agent", "dev"]),
        "#1 in_progress"
    );
    assert_eq!(run(&dir, &["remove", "#2"]), "#2 removed");
}

#[test]
fn text_values_that_start_with_a_dash_are_taken_literally() {
    let dir = initialized();
    register(&dir, "dev", "developer");
    register(&dir, "rev", "reviewer");

    assert_eq!(
        run(
            &dir,
            &[
                "add", "--title", "-x", "--tag", "-t", "--test", "-d", "-1", "-2"
            ]
        ),
        "#1 todo"
    );
    run(&dir, &["claim", "1", "--agent", "dev"]);
    assert_eq!(
        run(
            &dir,
            &["submit-review", "1", "--agent", "dev", "--pass", "0", "-v"]
        ),
        "#1 review rev1"
    );
    let packet = common::task(&dir, &["claim", "1", "--agent", "rev"]);
    assert_eq!(packet["title"], "-x");
    assert_eq!(packet["tests"][0]["describe"], "-d");
    assert_eq!(packet["tests"][0]["input"], "-1");
    assert_eq!(packet["tests"][0]["output"], "-2");
    assert_eq!(packet["tests"][0]["evidence"], "-v");
    assert_eq!(
        run(
            &dir,
            &["request-changes", "1", "--agent", "rev", "--notes", "-n"]
        ),
        "#1 in_progress"
    );
    assert_eq!(
        common::task(&dir, &["claim", "1", "--agent", "dev"])["changes"],
        "-n"
    );
}

// ---------------------------------------------------------------------------
// Who is acting
// ---------------------------------------------------------------------------

#[test]
fn the_agent_name_may_come_from_the_environment() {
    let dir = initialized();
    register(&dir, "dev", "developer");
    add_task(&dir, "t", "medium");

    let output = kanban(&dir)
        .env("AGENT_KANBAN_AGENT", "dev")
        .args(["claim", "1"])
        .output()
        .unwrap();

    assert!(output.status.success(), "{output:?}");
    assert_eq!(run(&dir, &["list"]), "#1 medium in_progress@dev t");
}

#[test]
fn the_flag_beats_the_environment_variable() {
    let dir = initialized();
    register(&dir, "dev", "developer");
    register(&dir, "other", "developer");
    add_task(&dir, "t", "medium");

    let output = kanban(&dir)
        .env("AGENT_KANBAN_AGENT", "other")
        .args(["claim", "1", "--agent", "dev"])
        .output()
        .unwrap();

    assert!(output.status.success(), "{output:?}");
    assert_eq!(run(&dir, &["list"]), "#1 medium in_progress@dev t");
}

#[test]
fn a_padded_agent_name_is_trimmed() {
    let dir = initialized();
    register(&dir, "dev", "developer");
    add_task(&dir, "t", "medium");

    run(&dir, &["claim", "1", "--agent", "  dev "]);

    assert_eq!(run(&dir, &["list"]), "#1 medium in_progress@dev t");
}

/// Identity is checked before the board is even opened, so the message is the
/// same with or without a project, and whatever the task id.
#[test]
fn a_missing_or_blank_agent_name_is_refused_by_every_agent_command() {
    let dir = project();
    let commands: &[&[&str]] = &[
        &["claim", "1"],
        &["claim-next"],
        &["release", "1"],
        &["submit-review", "1", "--pass", "0", "ok"],
        &["approve", "1"],
        &["request-changes", "1", "--notes", "n"],
    ];
    assert_eq!(commands.len(), AGENT_COMMANDS.len());

    for args in commands {
        assert_eq!(fail(&dir, args), NO_IDENTITY, "{args:?}");
        for blank in ["", "   "] {
            let mut with_blank = args.to_vec();
            with_blank.extend(["--agent", blank]);
            assert_eq!(fail(&dir, &with_blank), NO_IDENTITY, "{with_blank:?}");
        }
        for blank in ["", "   "] {
            let output = kanban(&dir)
                .env("AGENT_KANBAN_AGENT", blank)
                .args(*args)
                .output()
                .unwrap();
            assert_eq!(output.status.code(), Some(1), "{args:?} env {blank:?}");
            assert_eq!(
                String::from_utf8(output.stderr).unwrap().trim_end(),
                NO_IDENTITY,
                "{args:?} env {blank:?}"
            );
        }
    }
}

// ---------------------------------------------------------------------------
// Shape of the output
// ---------------------------------------------------------------------------

#[test]
fn every_reply_ends_with_exactly_one_newline() {
    let dir = initialized();
    register(&dir, "dev", "developer");
    add_task(&dir, "t", "medium");
    let commands: &[&[&str]] = &[
        &["add", "--title", "u", "--test", "d", "i", "o"],
        &["list"],
        &["show", "1"],
        &["status"],
        &["agent", "list"],
        &["guide"],
        &["claim", "1", "--agent", "dev"],
        &["claim-next", "--agent", "dev"],
        &["release", "1", "--agent", "dev"],
        &["init"],
        &["list", "--status", "done"],
    ];

    for args in commands {
        let output = kanban(&dir).args(*args).output().unwrap();

        assert!(output.status.success(), "{args:?}: {output:?}");
        assert!(output.stdout.ends_with(b"\n"), "{args:?}");
        assert!(!output.stdout.ends_with(b"\n\n"), "{args:?}");
        assert!(output.stderr.is_empty(), "{args:?}");
    }
}

#[test]
fn a_refusal_is_one_line_on_stderr_and_nothing_on_stdout() {
    let dir = initialized();

    let output = kanban(&dir).args(["show", "99"]).output().unwrap();

    assert_eq!(output.status.code(), Some(1));
    assert!(output.stdout.is_empty());
    assert_eq!(
        String::from_utf8(output.stderr).unwrap(),
        "error: task 99 not found\n"
    );
}
