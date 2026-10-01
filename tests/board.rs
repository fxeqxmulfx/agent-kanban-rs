//! The board as data: `add`, `edit`, `remove`, `list`, `show`, `status`, agents,
//! and how a project is found, created and upgraded. Everything runs through the
//! compiled binary; replies are asserted as exact strings.

mod common;

use common::{add_task, fail, initialized, project, register, run};

// ---------------------------------------------------------------------------
// add
// ---------------------------------------------------------------------------

#[test]
fn add_replies_with_the_new_id_and_status() {
    let dir = initialized();

    assert_eq!(
        run(&dir, &["add", "--title", "first", "--test", "d", "i", "o"]),
        "#1 todo"
    );
    assert_eq!(
        run(&dir, &["add", "--title", "second", "--test", "d", "i", "o"]),
        "#2 todo"
    );
}

#[test]
fn add_rejects_bad_input_and_creates_nothing() {
    let dir = initialized();
    let test = ["--test", "d", "i", "o"];
    let add = |args: &[&str]| {
        let mut all = vec!["add"];
        all.extend_from_slice(args);
        all.extend_from_slice(&test);
        fail(&dir, &all)
    };

    assert_eq!(
        add(&["--title", "t", "--priority", "urgentish"]),
        "error: invalid priority 'urgentish': must be one of low, medium, high, urgent"
    );
    assert_eq!(
        add(&["--title", "t", "--priority", "HIGH"]),
        "error: invalid priority 'HIGH': must be one of low, medium, high, urgent"
    );
    assert_eq!(add(&["--title", ""]), "error: title must not be empty");
    assert_eq!(
        add(&["--title", " \t\n "]),
        "error: title must not be empty"
    );

    assert_eq!(run(&dir, &["list"]), "no tasks");
}

#[test]
fn every_priority_is_accepted_and_medium_is_the_default() {
    let dir = initialized();
    for priority in ["low", "medium", "high", "urgent"] {
        add_task(&dir, priority, priority);
    }
    run(
        &dir,
        &["add", "--title", "default", "--test", "d", "i", "o"],
    );

    assert_eq!(
        run(&dir, &["list"]),
        "#4 urgent todo urgent\n#3 high todo high\n#2 medium todo medium\n#5 medium todo default\n#1 low todo low"
    );
}

#[test]
fn titles_have_their_whitespace_collapsed() {
    let dir = initialized();
    run(
        &dir,
        &[
            "add",
            "--title",
            "  spaced \t out\n title  ",
            "--test",
            "d",
            "i",
            "o",
        ],
    );

    assert_eq!(run(&dir, &["list"]), "#1 medium todo spaced out title");
    assert_eq!(
        common::task(&dir, &["show", "1"])["title"],
        "spaced out title"
    );
}

#[test]
fn awkward_text_survives_the_round_trip_unchanged() {
    let dir = initialized();
    let title = "robert'); DROP TABLE tasks; --";
    let tag = "'; DROP TABLE agents; --";
    let describe = "line one\nline two";
    let input = "tab\there \"quoted\" back\\slash \u{1} {\"json\": [1, 2]}";
    let output = "Привет, мир — 你好 🚀";
    run(
        &dir,
        &[
            "add", "--title", title, "--tag", tag, "--test", describe, input, output,
        ],
    );

    let shown = common::task(&dir, &["show", "1"]);

    assert_eq!(shown["title"], title);
    assert_eq!(shown["tags"][0], tag);
    assert_eq!(shown["tests"][0]["describe"], describe);
    assert_eq!(shown["tests"][0]["input"], input);
    assert_eq!(shown["tests"][0]["output"], output);
    // The SQL in the text above was data, not code: the tables are all intact
    // and a tag filter with the same text finds the task.
    assert_eq!(
        run(&dir, &["list", "--tag", tag]),
        format!("#1 medium todo {title}")
    );
    register(&dir, "alice", "developer");
    assert_eq!(run(&dir, &["list"]), format!("#1 medium todo {title}"));
}

#[test]
fn very_long_text_is_stored_whole() {
    let dir = initialized();
    let title = "x".repeat(5000);
    let input = "y".repeat(50_000);
    run(
        &dir,
        &["add", "--title", &title, "--test", "d", &input, "o"],
    );

    let shown = common::task(&dir, &["show", "1"]);

    assert_eq!(shown["title"], title.as_str());
    assert_eq!(shown["tests"][0]["input"], input.as_str());
}

// ---------------------------------------------------------------------------
// show
// ---------------------------------------------------------------------------

#[test]
fn show_leaves_out_everything_that_is_empty() {
    let dir = initialized();
    run(&dir, &["add", "--title", "bare", "--test", "d", "i", "o"]);

    assert_eq!(run(&dir, &["show", "1"]), "#1 medium todo bare\n0|d|i|o");
}

#[test]
fn show_reports_holder_tags_and_revision_in_a_fixed_order() {
    let dir = initialized();
    register(&dir, "dev", "developer");
    run(
        &dir,
        &[
            "add",
            "--title",
            "t",
            "--priority",
            "high",
            "--tag",
            "b",
            "--tag",
            "a",
            "--test",
            "d",
            "i",
            "o",
        ],
    );
    run(&dir, &["claim", "1", "--agent", "dev"]);

    assert_eq!(
        run(&dir, &["show", "1"]),
        "#1 high in_progress@dev t\ntags|b|a\n0|d|i|o"
    );
}

#[test]
fn show_history_adds_nothing_for_a_task_that_was_never_submitted() {
    let dir = initialized();
    run(&dir, &["add", "--title", "t", "--test", "d", "i", "o"]);

    assert_eq!(
        run(&dir, &["show", "1", "--history"]),
        run(&dir, &["show", "1"])
    );
}

#[test]
fn show_of_an_unknown_task_is_refused() {
    let dir = initialized();

    assert_eq!(fail(&dir, &["show", "99"]), "error: task 99 not found");
}

// ---------------------------------------------------------------------------
// list
// ---------------------------------------------------------------------------

/// alice holds #1 (urgent, tag alpha); #2 low/beta, #3 medium/alpha, #4 high,
/// #5 parked in backlog.
fn list_board() -> tempfile::TempDir {
    let dir = initialized();
    register(&dir, "alice", "developer");
    for (title, priority, tag) in [
        ("t1", "urgent", "alpha"),
        ("t2", "low", "beta"),
        ("t3", "medium", "alpha"),
        ("t4", "high", ""),
        ("t5", "high", "beta"),
    ] {
        let mut args = vec!["add", "--title", title, "--priority", priority];
        if !tag.is_empty() {
            args.extend(["--tag", tag]);
        }
        args.extend(["--test", "d", "i", "o"]);
        run(&dir, &args);
    }
    run(&dir, &["claim", "1", "--agent", "alice"]);
    run(&dir, &["move", "5", "backlog"]);
    dir
}

#[test]
fn list_orders_by_status_then_priority_then_id() {
    let dir = list_board();

    assert_eq!(
        run(&dir, &["list"]),
        "#1 urgent in_progress@alice t1\n#4 high todo t4\n#3 medium todo t3\n#2 low todo t2\n#5 high backlog t5"
    );
}

#[test]
fn list_filters_combine() {
    let dir = list_board();

    assert_eq!(
        run(&dir, &["list", "--status", "todo"]),
        "#4 high todo t4\n#3 medium todo t3\n#2 low todo t2"
    );
    assert_eq!(
        run(&dir, &["list", "--status", "backlog"]),
        "#5 high backlog t5"
    );
    assert_eq!(
        run(&dir, &["list", "--tag", "alpha"]),
        "#1 urgent in_progress@alice t1\n#3 medium todo t3"
    );
    assert_eq!(
        run(&dir, &["list", "--priority", "high"]),
        "#4 high todo t4\n#5 high backlog t5"
    );
    assert_eq!(
        run(&dir, &["list", "--executor", "alice"]),
        "#1 urgent in_progress@alice t1"
    );
    assert_eq!(
        run(&dir, &["list", "--tag", "alpha", "--status", "todo"]),
        "#3 medium todo t3"
    );
    assert_eq!(
        run(&dir, &["list", "--priority", "high", "--tag", "beta"]),
        "#5 high backlog t5"
    );
    assert_eq!(
        run(&dir, &["list", "--executor", "alice", "--tag", "beta"]),
        "no tasks"
    );
    assert_eq!(run(&dir, &["list", "--executor", "nobody"]), "no tasks");
    assert_eq!(run(&dir, &["list", "--tag", "missing"]), "no tasks");
}

#[test]
fn list_rejects_unknown_status_and_priority_values() {
    let dir = list_board();

    assert_eq!(
        fail(&dir, &["list", "--status", "bogus"]),
        "error: invalid status 'bogus': must be one of backlog, todo, in_progress, review, done"
    );
    assert_eq!(
        fail(&dir, &["list", "--priority", "bogus"]),
        "error: invalid priority 'bogus': must be one of low, medium, high, urgent"
    );
}

#[test]
fn list_hides_done_tasks_until_asked() {
    let dir = initialized();
    register(&dir, "dev", "developer");
    register(&dir, "rev", "reviewer");
    for title in ["a", "b", "c"] {
        add_task(&dir, title, "medium");
    }
    for id in ["1", "2"] {
        run(&dir, &["claim", id, "--agent", "dev"]);
        run(
            &dir,
            &["submit-review", id, "--agent", "dev", "--pass", "0", "ok"],
        );
        run(&dir, &["claim", id, "--agent", "rev"]);
        run(&dir, &["approve", id, "--agent", "rev"]);
    }

    assert_eq!(
        run(&dir, &["list"]),
        "#3 medium todo c\n+2 done hidden (--all)"
    );
    assert_eq!(
        run(&dir, &["list", "--all"]),
        "#3 medium todo c\n#1 medium done a\n#2 medium done b"
    );
    assert_eq!(
        run(&dir, &["list", "--status", "done"]),
        "#1 medium done a\n#2 medium done b"
    );
    assert_eq!(run(&dir, &["list", "--status", "todo"]), "#3 medium todo c");
}

#[test]
fn the_hidden_done_count_respects_the_other_filters() {
    let dir = initialized();
    register(&dir, "dev", "developer");
    register(&dir, "rev", "reviewer");
    run(
        &dir,
        &[
            "add", "--title", "x", "--tag", "keep", "--test", "d", "i", "o",
        ],
    );
    run(&dir, &["add", "--title", "y", "--test", "d", "i", "o"]);
    run(
        &dir,
        &[
            "add", "--title", "open", "--tag", "keep", "--test", "d", "i", "o",
        ],
    );
    for id in ["1", "2"] {
        run(&dir, &["claim", id, "--agent", "dev"]);
        run(
            &dir,
            &["submit-review", id, "--agent", "dev", "--pass", "0", "ok"],
        );
        run(&dir, &["claim", id, "--agent", "rev"]);
        run(&dir, &["approve", id, "--agent", "rev"]);
    }

    // Only #1 is both done and tagged: the footer must not count #2.
    assert_eq!(
        run(&dir, &["list", "--tag", "keep"]),
        "#3 medium todo open\n+1 done hidden (--all)"
    );
}

#[test]
fn list_limit_cuts_the_list_and_says_how_much_is_left() {
    let dir = list_board();

    assert_eq!(
        run(&dir, &["list", "--limit", "2"]),
        "#1 urgent in_progress@alice t1\n#4 high todo t4\n+3 more (--limit 0 = all)"
    );
    assert_eq!(
        run(&dir, &["list", "--limit", "5"]),
        "#1 urgent in_progress@alice t1\n#4 high todo t4\n#3 medium todo t3\n#2 low todo t2\n#5 high backlog t5"
    );
    // 0 means "no limit".
    assert_eq!(run(&dir, &["list", "--limit", "0"]), run(&dir, &["list"]));
}

#[test]
fn list_footers_count_hidden_done_and_cut_off_tasks_separately() {
    let dir = initialized();
    register(&dir, "dev", "developer");
    register(&dir, "rev", "reviewer");
    for title in ["a", "b", "c", "d"] {
        add_task(&dir, title, "medium");
    }
    run(&dir, &["claim", "1", "--agent", "dev"]);
    run(
        &dir,
        &["submit-review", "1", "--agent", "dev", "--pass", "0", "ok"],
    );
    run(&dir, &["claim", "1", "--agent", "rev"]);
    run(&dir, &["approve", "1", "--agent", "rev"]);

    assert_eq!(
        run(&dir, &["list", "--limit", "1"]),
        "#2 medium todo b\n+2 more (--limit 0 = all)\n+1 done hidden (--all)"
    );
}

/// One careless `list` must not flood an agent's context: a big board answers
/// with a bounded reply, and the footer says how to get the rest.
#[test]
fn list_prints_twenty_rows_by_default_and_says_how_to_see_the_rest() {
    let dir = initialized();
    for n in 1..=25 {
        add_task(&dir, &format!("t{n}"), "medium");
    }

    let default = run(&dir, &["list"]);

    let rows: Vec<&str> = default.lines().collect();
    assert_eq!(rows.len(), 21, "{default}");
    assert_eq!(rows[0], "#1 medium todo t1");
    assert_eq!(rows[19], "#20 medium todo t20");
    assert_eq!(rows[20], "+5 more (--limit 0 = all)");

    // The footer's advice works: `--limit 0` is everything, with no footer.
    let everything = run(&dir, &["list", "--limit", "0"]);
    assert_eq!(everything.lines().count(), 25);
    assert!(everything.starts_with(&format!("{}\n", rows[..20].join("\n"))));
    assert!(everything.ends_with("#25 medium todo t25"), "{everything}");
    // A limit is a plain number too: above the cap it shows more.
    assert_eq!(run(&dir, &["list", "--limit", "25"]), everything);
    assert_eq!(
        run(&dir, &["list", "--limit", "24"]).lines().last(),
        Some("+1 more (--limit 0 = all)")
    );
}

/// The cap cuts, it does not sort: the most actionable rows stay.
#[test]
fn the_default_cap_keeps_the_most_urgent_rows() {
    let dir = initialized();
    for n in 1..=21 {
        add_task(&dir, &format!("low{n}"), "low");
    }
    let urgent = add_task(&dir, "late but urgent", "urgent");

    let rows: Vec<String> = run(&dir, &["list"]).lines().map(String::from).collect();

    assert_eq!(rows.len(), 21);
    assert_eq!(rows[0], format!("#{urgent} urgent todo late but urgent"));
    assert_eq!(rows[1], "#1 low todo low1");
    assert_eq!(rows[19], "#19 low todo low19");
    // 22 tasks, 20 shown.
    assert_eq!(rows[20], "+2 more (--limit 0 = all)");
}

#[test]
fn exactly_twenty_tasks_fit_without_a_footer() {
    let dir = initialized();
    for n in 1..=20 {
        add_task(&dir, &format!("t{n}"), "medium");
    }

    let out = run(&dir, &["list"]);

    assert_eq!(out.lines().count(), 20);
    assert!(!out.contains("more"), "{out}");
}

/// The default applies to every filtered view as well: each prints its first
/// twenty matches, and counts only the matches it left out.
#[test]
fn the_default_cap_applies_to_filtered_lists_and_counts_only_matches() {
    let dir = initialized();
    for n in 1..=22 {
        add_task(&dir, &format!("a{n}"), "high");
    }
    for n in 1..=5 {
        add_task(&dir, &format!("b{n}"), "low");
    }

    let high = run(&dir, &["list", "--priority", "high"]);
    let low = run(&dir, &["list", "--priority", "low"]);

    assert_eq!(high.lines().count(), 21, "{high}");
    assert_eq!(high.lines().last(), Some("+2 more (--limit 0 = all)"));
    assert_eq!(low.lines().count(), 5, "{low}");
    assert!(!low.contains("more"), "{low}");
}

#[test]
fn list_on_an_empty_board_says_so() {
    let dir = initialized();

    assert_eq!(run(&dir, &["list"]), "no tasks");
    assert_eq!(run(&dir, &["list", "--all"]), "no tasks");
}

// ---------------------------------------------------------------------------
// status
// ---------------------------------------------------------------------------

#[test]
fn status_counts_every_status_and_shows_what_each_agent_holds() {
    let dir = initialized();
    register(&dir, "bob", "developer");
    register(&dir, "alice", "developer");
    register(&dir, "carol", "developer");
    register(&dir, "reviewer", "reviewer");
    for title in ["t1", "t2", "t3", "t4"] {
        add_task(&dir, title, "low");
    }
    run(&dir, &["claim", "1", "--agent", "alice"]);
    run(&dir, &["claim", "2", "--agent", "bob"]);
    run(
        &dir,
        &["submit-review", "2", "--agent", "bob", "--pass", "0", "ok"],
    );
    run(&dir, &["claim", "2", "--agent", "reviewer"]);
    run(&dir, &["approve", "2", "--agent", "reviewer"]);
    run(&dir, &["move", "4", "backlog"]);

    // Agents are listed by name, with the tasks they hold or `-`.
    assert_eq!(
        run(&dir, &["status"]),
        "backlog 1, todo 1, in_progress 1, review 0, done 1\nagents: alice 1; bob -; carol -; reviewer -"
    );
}

#[test]
fn status_lists_several_tasks_of_one_agent_by_id() {
    let dir = initialized();
    register(&dir, "alice", "developer");
    for title in ["a", "b", "c"] {
        add_task(&dir, title, "medium");
    }
    for id in ["3", "1"] {
        run(&dir, &["claim", id, "--agent", "alice"]);
    }

    assert_eq!(
        run(&dir, &["status"]),
        "backlog 0, todo 1, in_progress 2, review 0, done 0\nagents: alice 1,3"
    );
}

#[test]
fn status_of_an_empty_board_has_no_agents_line() {
    let dir = initialized();

    assert_eq!(
        run(&dir, &["status"]),
        "backlog 0, todo 0, in_progress 0, review 0, done 0"
    );
}

#[test]
fn a_reviewer_holding_a_task_shows_up_in_status() {
    let dir = initialized();
    register(&dir, "dev", "developer");
    register(&dir, "rev", "reviewer");
    add_task(&dir, "t", "medium");
    run(&dir, &["claim", "1", "--agent", "dev"]);
    run(
        &dir,
        &["submit-review", "1", "--agent", "dev", "--pass", "0", "ok"],
    );
    run(&dir, &["claim", "1", "--agent", "rev"]);

    assert_eq!(
        run(&dir, &["status"]),
        "backlog 0, todo 0, in_progress 0, review 1, done 0\nagents: dev -; rev 1"
    );
}

// ---------------------------------------------------------------------------
// edit and remove
// ---------------------------------------------------------------------------

#[test]
fn edit_changes_only_the_fields_it_is_given() {
    let dir = initialized();
    run(
        &dir,
        &[
            "add",
            "--title",
            "old",
            "--priority",
            "low",
            "--tag",
            "a",
            "--tag",
            "b",
            "--test",
            "d1",
            "i1",
            "o1",
        ],
    );

    assert_eq!(run(&dir, &["edit", "1", "--title", "new"]), "#1 todo");
    assert_eq!(
        run(&dir, &["show", "1"]),
        "#1 low todo new\ntags|a|b\n0|d1|i1|o1"
    );
    assert_eq!(run(&dir, &["edit", "1", "--priority", "urgent"]), "#1 todo");
    assert_eq!(run(&dir, &["list"]), "#1 urgent todo new");
    // --tag and --test replace everything that was there.
    run(&dir, &["edit", "1", "--tag", "c"]);
    run(
        &dir,
        &[
            "edit", "1", "--test", "d2", "i2", "o2", "--test", "d3", "i3", "o3",
        ],
    );
    let shown = common::task(&dir, &["show", "1"]);
    assert_eq!(shown["tags"], serde_json::json!(["c"]));
    assert_eq!(shown["tests"][0]["describe"], "d2");
    assert_eq!(shown["tests"][1]["describe"], "d3");
    assert_eq!(shown["tests"].as_array().unwrap().len(), 2);
    assert_eq!(shown["title"], "new");
}

#[test]
fn edit_can_change_everything_in_one_call() {
    let dir = initialized();
    add_task(&dir, "old", "low");

    assert_eq!(
        run(
            &dir,
            &[
                "edit",
                "1",
                "--title",
                "  all   new ",
                "--priority",
                "high",
                "--tag",
                "x",
                "--test",
                "d",
                "i",
                "o",
            ]
        ),
        "#1 todo"
    );

    assert_eq!(
        run(&dir, &["show", "1"]),
        "#1 high todo all new\ntags|x\n0|d|i|o"
    );
}

#[test]
fn edit_keeps_the_parked_status() {
    let dir = initialized();
    add_task(&dir, "t", "medium");
    run(&dir, &["move", "1", "backlog"]);

    assert_eq!(run(&dir, &["edit", "1", "--title", "u"]), "#1 backlog");
}

#[test]
fn edit_validates_like_add_and_changes_nothing_when_refused() {
    let dir = initialized();
    add_task(&dir, "keep", "medium");

    assert_eq!(
        fail(&dir, &["edit", "1"]),
        "error: nothing to change: pass --title, --priority, --tag, --test, --after or --drop-after"
    );
    assert_eq!(
        fail(&dir, &["edit", "1", "--priority", "nope"]),
        "error: invalid priority 'nope': must be one of low, medium, high, urgent"
    );
    assert_eq!(
        fail(&dir, &["edit", "1", "--title", "ok", "--priority", "nope"]),
        "error: invalid priority 'nope': must be one of low, medium, high, urgent"
    );
    assert_eq!(
        fail(&dir, &["edit", "1", "--title", " "]),
        "error: title must not be empty"
    );
    assert_eq!(
        fail(&dir, &["edit", "99", "--title", "x"]),
        "error: task 99 not found"
    );

    // The refused edits above must not have applied any of their fields.
    assert_eq!(run(&dir, &["list"]), "#1 medium todo keep");
}

#[test]
fn remove_deletes_the_task_for_good() {
    let dir = initialized();
    add_task(&dir, "keep", "medium");
    add_task(&dir, "drop", "medium");

    assert_eq!(run(&dir, &["remove", "2"]), "#2 removed");

    assert_eq!(run(&dir, &["list"]), "#1 medium todo keep");
    assert_eq!(fail(&dir, &["show", "2"]), "error: task 2 not found");
    assert_eq!(fail(&dir, &["remove", "2"]), "error: task 2 not found");
    assert_eq!(fail(&dir, &["remove", "99"]), "error: task 99 not found");
}

#[test]
fn a_backlog_task_can_be_removed() {
    let dir = initialized();
    add_task(&dir, "t", "medium");
    run(&dir, &["move", "1", "backlog"]);

    assert_eq!(run(&dir, &["remove", "1"]), "#1 removed");
}

// ---------------------------------------------------------------------------
// agents
// ---------------------------------------------------------------------------

#[test]
fn agents_register_with_a_role_and_list_by_name() {
    let dir = initialized();

    assert_eq!(run(&dir, &["agent", "list"]), "no agents");
    assert_eq!(run(&dir, &["agent", "register", "zed"]), "zed developer");
    assert_eq!(
        run(&dir, &["agent", "register", "amy", "--role", "reviewer"]),
        "amy reviewer"
    );
    assert_eq!(
        run(&dir, &["agent", "register", "kim", "--role", "developer"]),
        "kim developer"
    );

    assert_eq!(
        run(&dir, &["agent", "list"]),
        "amy reviewer\nkim developer\nzed developer"
    );
}

#[test]
fn registering_bad_agents_is_refused() {
    let dir = initialized();
    register(&dir, "dup", "developer");

    assert_eq!(
        fail(&dir, &["agent", "register", "dup"]),
        "error: agent 'dup' already exists"
    );
    assert_eq!(
        fail(&dir, &["agent", "register", "dup", "--role", "reviewer"]),
        "error: agent 'dup' already exists"
    );
    assert_eq!(
        fail(&dir, &["agent", "register", "new", "--role", "manager"]),
        "error: invalid role 'manager': must be developer or reviewer"
    );
    assert_eq!(
        fail(&dir, &["agent", "register", "two words"]),
        "error: agent name 'two words' must not contain whitespace"
    );
    assert_eq!(
        fail(&dir, &["agent", "register", "  "]),
        "error: agent name must not be empty or whitespace-only"
    );
    assert_eq!(run(&dir, &["agent", "list"]), "dup developer");
}

#[test]
fn agent_names_may_contain_quotes_and_unicode() {
    let dir = initialized();

    assert_eq!(
        run(&dir, &["agent", "register", "o'brien"]),
        "o'brien developer"
    );
    assert_eq!(
        run(&dir, &["agent", "register", "агент-1"]),
        "агент-1 developer"
    );
    add_task(&dir, "t", "medium");
    run(&dir, &["claim", "1", "--agent", "o'brien"]);

    assert_eq!(
        run(&dir, &["list", "--executor", "o'brien"]),
        "#1 medium in_progress@o'brien t"
    );
}

#[test]
fn removing_an_agent_releases_its_tasks_but_keeps_their_status() {
    let dir = initialized();
    register(&dir, "alice", "developer");
    register(&dir, "bob", "developer");
    for title in ["a", "b", "c"] {
        add_task(&dir, title, "medium");
    }
    run(&dir, &["claim", "3", "--agent", "alice"]);
    run(&dir, &["claim", "1", "--agent", "alice"]);
    run(&dir, &["claim", "2", "--agent", "bob"]);

    assert_eq!(
        run(&dir, &["agent", "remove", "alice"]),
        "alice removed, released #1,#3"
    );

    assert_eq!(
        run(&dir, &["list"]),
        "#1 medium in_progress a\n#2 medium in_progress@bob b\n#3 medium in_progress c"
    );
    assert_eq!(run(&dir, &["agent", "list"]), "bob developer");
    // The released tasks are claimable by someone else right away.
    run(&dir, &["claim", "1", "--agent", "bob"]);
}

#[test]
fn a_removed_agent_is_gone_for_every_command() {
    let dir = initialized();
    register(&dir, "alice", "developer");
    add_task(&dir, "t", "medium");
    assert_eq!(run(&dir, &["agent", "remove", "alice"]), "alice removed");

    assert_eq!(
        fail(&dir, &["claim", "1", "--agent", "alice"]),
        "error: agent 'alice' is not registered; run `agent-kanban agent register alice --role developer|reviewer` first"
    );
    assert_eq!(
        fail(&dir, &["agent", "remove", "alice"]),
        "error: agent 'alice' not found"
    );
    // The name can be registered again, as a fresh agent.
    assert_eq!(
        run(&dir, &["agent", "register", "alice", "--role", "reviewer"]),
        "alice reviewer"
    );
}

#[test]
fn removing_an_agent_never_touches_other_agents_or_done_tasks() {
    let dir = initialized();
    register(&dir, "dev", "developer");
    register(&dir, "other", "developer");
    register(&dir, "rev", "reviewer");
    add_task(&dir, "done one", "medium");
    add_task(&dir, "held by other", "medium");
    run(&dir, &["claim", "1", "--agent", "dev"]);
    run(
        &dir,
        &["submit-review", "1", "--agent", "dev", "--pass", "0", "ok"],
    );
    run(&dir, &["claim", "1", "--agent", "rev"]);
    run(&dir, &["approve", "1", "--agent", "rev"]);
    run(&dir, &["claim", "2", "--agent", "other"]);

    // `dev` holds nothing now, so nothing is released.
    assert_eq!(run(&dir, &["agent", "remove", "dev"]), "dev removed");

    assert_eq!(
        run(&dir, &["list", "--all"]),
        "#2 medium in_progress@other held by other\n#1 medium done done one"
    );
}

// ---------------------------------------------------------------------------
// Finding, creating and upgrading the board
// ---------------------------------------------------------------------------

#[test]
fn commands_before_init_fail_cleanly() {
    let dir = project();

    for args in [
        &["list"][..],
        &["agent", "list"],
        &["status"],
        &["show", "1"],
    ] {
        assert_eq!(
            fail(&dir, args),
            "error: not a kanban project (no .kanban/ found); run `agent-kanban init`",
            "{args:?}"
        );
    }
}

#[test]
fn init_twice_is_harmless_and_keeps_the_data() {
    let dir = initialized();
    register(&dir, "alice", "developer");
    add_task(&dir, "keep me", "high");

    assert_eq!(run(&dir, &["init"]), "initialized");

    assert_eq!(run(&dir, &["list"]), "#1 high todo keep me");
    assert_eq!(run(&dir, &["agent", "list"]), "alice developer");
}

#[test]
fn init_stamps_the_current_schema_version() {
    let dir = initialized();

    let version: i64 = common::db(&dir)
        .query_row("PRAGMA user_version", [], |row| row.get(0))
        .unwrap();

    assert_eq!(version, 4);
    assert!(common::db_path(&dir).is_file());
}

#[test]
fn the_board_is_found_from_a_subdirectory() {
    let dir = initialized();
    add_task(&dir, "top level task", "low");
    let deep = dir.path().join("a").join("b").join("c");
    std::fs::create_dir_all(&deep).unwrap();

    assert_eq!(run(&deep, &["list"]), "#1 low todo top level task");
    assert_eq!(
        run(
            &deep,
            &["add", "--title", "from below", "--test", "d", "i", "o"]
        ),
        "#2 todo"
    );
    assert_eq!(
        run(&dir, &["list"]),
        "#2 medium todo from below\n#1 low todo top level task"
    );
    // init from a subdirectory makes a new board there, it does not reuse the parent's.
    assert_eq!(run(&deep, &["init"]), "initialized");
    assert!(deep.join(".kanban").join("board.db").is_file());
    assert_eq!(run(&deep, &["list"]), "no tasks");
}

/// Discovery uses the closest `.kanban/` going up and never merges with a
/// parent's board.
#[test]
fn a_nested_project_is_independent_of_its_parent() {
    let root = initialized();
    add_task(&root, "parent task", "low");
    let child = root.path().join("child");
    std::fs::create_dir_all(&child).unwrap();
    run(&child, &["init"]);
    run(
        &child,
        &["add", "--title", "child task", "--test", "d", "i", "o"],
    );

    assert_eq!(run(&child, &["list"]), "#1 medium todo child task");
    assert_eq!(run(&root, &["list"]), "#1 low todo parent task");
}

#[test]
fn a_database_file_without_a_schema_is_refused() {
    let dir = project();
    std::fs::create_dir(dir.path().join(".kanban")).unwrap();
    std::fs::File::create(common::db_path(&dir)).unwrap();

    assert_eq!(
        fail(&dir, &["list"]),
        "error: database is not initialized; run `agent-kanban init`"
    );
    // init fixes it.
    assert_eq!(run(&dir, &["init"]), "initialized");
    assert_eq!(run(&dir, &["list"]), "no tasks");
}

#[test]
fn a_newer_schema_is_refused_instead_of_misread() {
    let dir = initialized();
    common::db(&dir)
        .execute_batch("PRAGMA user_version = 999;")
        .unwrap();

    let message = "error: this project's schema version (999) is newer than this build of agent-kanban supports (4); upgrade agent-kanban";
    assert_eq!(fail(&dir, &["agent", "list"]), message);
    assert_eq!(fail(&dir, &["init"]), message);
}

#[test]
fn only_show_and_claim_reply_with_a_task_block() {
    let dir = initialized();
    register(&dir, "dev", "developer");
    add_task(&dir, "t", "medium");

    let plain = [
        run(&dir, &["list"]),
        run(&dir, &["status"]),
        run(&dir, &["agent", "list"]),
        run(&dir, &["move", "1", "backlog"]),
    ];
    for reply in plain {
        assert!(!reply.contains('|'), "{reply}");
    }
    run(&dir, &["move", "1", "todo"]);
    for args in [&["show", "1"][..], &["claim", "1", "--agent", "dev"]] {
        let reply = run(&dir, args);
        let mut lines = reply.lines();
        assert!(
            lines.next().unwrap().starts_with("#1 "),
            "{args:?}: {reply}"
        );
        assert_eq!(lines.last(), Some("0|works|in|out"), "{args:?}: {reply}");
    }
}
