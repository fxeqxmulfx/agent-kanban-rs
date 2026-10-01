//! The text of the replies that describe a task (`show`, `claim`,
//! `claim-next`): a header line, then one `|`-separated row per fact, with
//! free text escaped so that no content can change what a row means. Everything
//! runs through the compiled binary.

mod common;

use common::{initialized, parse_order, parse_show, register, run, split_cells};
use std::path::Path;

/// Two agents and three tasks: `Base`, `Feature X` (after 1) and `Follow-up`
/// (after 2), with `Base` already done.
fn board_with_a_feature() -> tempfile::TempDir {
    let dir = initialized();
    register(&dir, "dev", "developer");
    register(&dir, "rev", "reviewer");
    run(
        &dir,
        &[
            "add",
            "--title",
            "Base",
            "--test",
            "base works",
            "in",
            "out",
        ],
    );
    run(
        &dir,
        &[
            "add",
            "--title",
            "Feature X",
            "--priority",
            "high",
            "--tag",
            "api",
            "--tag",
            "needs review",
            "--test",
            "adds",
            "1+1",
            "2",
            "--test",
            "fails cleanly",
            "1/0",
            "error",
            "--after",
            "1",
        ],
    );
    run(
        &dir,
        &[
            "add",
            "--title",
            "Follow-up",
            "--test",
            "t",
            "i",
            "o",
            "--after",
            "2",
        ],
    );
    run(&dir, &["claim", "1", "--agent", "dev"]);
    run(
        &dir,
        &["submit-review", "1", "--agent", "dev", "--pass", "0", "ok"],
    );
    run(&dir, &["claim", "1", "--agent", "rev"]);
    assert_eq!(
        run(&dir, &["approve", "1", "--agent", "rev"]),
        "#1 done unblocked:2"
    );
    dir
}

#[test]
fn show_prints_a_header_then_one_row_per_fact() {
    let dir = initialized();
    register(&dir, "dev", "developer");
    run(
        &dir,
        &[
            "add",
            "--title",
            "Base",
            "--test",
            "base works",
            "in",
            "out",
        ],
    );
    run(
        &dir,
        &[
            "add",
            "--title",
            "Feature X",
            "--priority",
            "high",
            "--tag",
            "api",
            "--tag",
            "needs review",
            "--test",
            "adds",
            "1+1",
            "2",
            "--test",
            "fails cleanly",
            "1/0",
            "error",
            "--after",
            "1",
        ],
    );
    run(
        &dir,
        &[
            "add",
            "--title",
            "Follow-up",
            "--test",
            "t",
            "i",
            "o",
            "--after",
            "2",
        ],
    );

    // A task that waits for one and is waited for by another: both are in the header.
    assert_eq!(
        run(&dir, &["show", "2"]),
        "#2 high todo after:1 blocks:3 Feature X\n\
         tags|api|needs review\n\
         0|adds|1+1|2\n\
         1|fails cleanly|1/0|error"
    );
    // Nothing is printed for what is empty: no tags row, no after, no blocks.
    assert_eq!(
        run(&dir, &["show", "3"]),
        "#3 medium todo after:2 Follow-up\n0|t|i|o"
    );
}

#[test]
fn a_reworked_task_shows_notes_verdicts_and_history() {
    let dir = board_with_a_feature();

    // The developer's order has no priority, tags or status: only the work.
    assert_eq!(
        run(&dir, &["claim", "2", "--agent", "dev"]),
        "#2 Feature X\n0|adds|1+1|2\n1|fails cleanly|1/0|error"
    );
    assert_eq!(
        run(&dir, &["show", "2"]),
        "#2 high in_progress@dev blocks:3 Feature X\n\
         tags|api|needs review\n\
         0|adds|1+1|2\n\
         1|fails cleanly|1/0|error"
    );
    run(
        &dir,
        &[
            "submit-review",
            "2",
            "--agent",
            "dev",
            "--pass",
            "0",
            "cargo test adds: ok",
            "--fail",
            "1",
            "panics instead of an error",
        ],
    );

    // The reviewer's packet is the same rows with the verdicts appended.
    assert_eq!(
        run(&dir, &["claim", "2", "--agent", "rev"]),
        "#2 rev1 Feature X\n\
         0|adds|1+1|2|passed|cargo test adds: ok\n\
         1|fails cleanly|1/0|error|failed|panics instead of an error"
    );
    assert_eq!(
        run(
            &dir,
            &[
                "request-changes",
                "2",
                "--agent",
                "rev",
                "--notes",
                "handle division by zero"
            ]
        ),
        "#2 in_progress"
    );

    // Back with the developer: the revision and the notes lead, old verdicts are gone.
    assert_eq!(
        run(&dir, &["claim", "2", "--agent", "dev"]),
        "#2 rev1 Feature X\n\
         changes|handle division by zero\n\
         0|adds|1+1|2\n\
         1|fails cleanly|1/0|error"
    );
    // `--history` appends one row per revision, then its review decision.
    assert_eq!(
        run(&dir, &["show", "2", "--history"]),
        "#2 high in_progress@dev rev1 blocks:3 Feature X\n\
         tags|api|needs review\n\
         changes|handle division by zero\n\
         0|adds|1+1|2\n\
         1|fails cleanly|1/0|error\n\
         rev1|dev|passed: cargo test adds: ok|failed: panics instead of an error\n\
         review|rev|changes_requested|handle division by zero"
    );
}

#[test]
fn an_approved_revision_closes_the_history_and_drops_the_notes() {
    let dir = board_with_a_feature();
    run(&dir, &["claim", "2", "--agent", "dev"]);
    run(
        &dir,
        &[
            "submit-review",
            "2",
            "--agent",
            "dev",
            "--pass",
            "0",
            "a",
            "--pass",
            "1",
            "b",
        ],
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
            "once more",
        ],
    );
    run(&dir, &["claim", "2", "--agent", "dev"]);
    run(
        &dir,
        &[
            "submit-review",
            "2",
            "--agent",
            "dev",
            "--pass",
            "0",
            "c",
            "--pass",
            "1",
            "d",
        ],
    );
    run(&dir, &["claim", "2", "--agent", "rev"]);
    assert_eq!(
        run(&dir, &["approve", "2", "--agent", "rev", "--notes", "good"]),
        "#2 done unblocked:3"
    );

    // Done: no holder, no outstanding notes, the last verdicts beside the tests.
    assert_eq!(
        run(&dir, &["show", "2", "--history"]),
        "#2 high done rev2 Feature X\n\
         tags|api|needs review\n\
         0|adds|1+1|2|passed|c\n\
         1|fails cleanly|1/0|error|passed|d\n\
         rev1|dev|passed: a|passed: b\n\
         review|rev|changes_requested|once more\n\
         rev2|dev|passed: c|passed: d\n\
         review|rev|approved|good"
    );
}

// ---------------------------------------------------------------------------
// Free text
// ---------------------------------------------------------------------------

/// Text that would break a naive layout, spelled out so a failure is readable.
#[test]
fn free_text_is_escaped_and_reads_back_exactly() {
    let dir = initialized();
    register(&dir, "dev", "developer");
    register(&dir, "rev", "reviewer");
    let input = "line one\nline two\r\n{\"k\": [1, 2]}";
    run(
        &dir,
        &[
            "add",
            "--title",
            "a | b \\ c \"d\"",
            "--tag",
            "x y",
            "--tag",
            "p|q",
            "--tag",
            "a,b",
            "--tag",
            "",
            "--test",
            "pipes | and \\ slashes",
            input,
            "",
            "--test",
            "é 日本 🙂",
            "||",
            "\\n is not a newline",
        ],
    );

    let expected = concat!(
        "#1 medium todo a \\| b \\\\ c \"d\"\n",
        "tags|x y|p\\|q|a,b|\n",
        "0|pipes \\| and \\\\ slashes|line one\\nline two\\r\\n{\"k\": [1, 2]}|\n",
        "1|é 日本 🙂|\\|\\||\\\\n is not a newline"
    );
    let shown = run(&dir, &["show", "1"]);
    assert_eq!(shown, expected);
    // Header, tags and two tests: the line breaks inside the text did not leak out.
    assert_eq!(shown.lines().count(), 4);

    let parsed = parse_show(&shown);
    assert_eq!(parsed["title"], "a | b \\ c \"d\"");
    assert_eq!(parsed["tags"], serde_json::json!(["x y", "p|q", "a,b", ""]));
    assert_eq!(parsed["tests"][0]["describe"], "pipes | and \\ slashes");
    assert_eq!(parsed["tests"][0]["input"], input);
    assert_eq!(parsed["tests"][0]["output"], "");
    assert_eq!(parsed["tests"][1]["output"], "\\n is not a newline");

    // Evidence and notes go through the same escaping, in every view.
    let evidence = "ran `a | b`\nthen \\ again";
    let notes = "fix | this\nand \\ that";
    run(&dir, &["claim", "1", "--agent", "dev"]);
    run(
        &dir,
        &[
            "submit-review",
            "1",
            "--agent",
            "dev",
            "--pass",
            "0",
            evidence,
            "--fail",
            "1",
            "|",
        ],
    );
    let packet = run(&dir, &["claim", "1", "--agent", "rev"]);
    assert_eq!(packet.lines().count(), 3, "{packet}");
    let packet = parse_order(&packet);
    assert_eq!(packet["tests"][0]["evidence"], evidence);
    assert_eq!(packet["tests"][1]["result"], "failed");
    assert_eq!(packet["tests"][1]["evidence"], "|");

    run(
        &dir,
        &["request-changes", "1", "--agent", "rev", "--notes", notes],
    );
    let order = run(&dir, &["claim", "1", "--agent", "dev"]);
    assert_eq!(order.lines().count(), 4, "{order}");
    assert_eq!(parse_order(&order)["changes"], notes);

    let history = run(&dir, &["show", "1", "--history"]);
    // Header, tags, changes, two tests, the revision row and its review.
    assert_eq!(history.lines().count(), 7, "{history}");
    let history = parse_show(&history);
    assert_eq!(history["changes"], notes);
    assert_eq!(
        history["history"],
        serde_json::json!([{
            "rev": 1,
            "by": "dev",
            "results": [format!("passed: {evidence}"), "failed: |"],
            "decision": "changes_requested",
            "notes": notes,
            "reviewer": "rev",
        }])
    );
}

/// A small deterministic generator: a failing case reproduces from its seed.
struct Rng(u64);

impl Rng {
    fn pick(&mut self, bound: usize) -> usize {
        self.0 = self
            .0
            .wrapping_mul(6_364_136_223_846_793_005)
            .wrapping_add(1_442_695_040_888_963_407);
        usize::try_from((self.0 >> 33) % u64::try_from(bound).unwrap()).unwrap()
    }

    /// Up to 12 pieces drawn from characters that matter to the layout. No `-`,
    /// so a value is never mistaken for a flag.
    fn text(&mut self, whitespace: bool) -> String {
        const PIECES: [&str; 22] = [
            "a", "Z", "7", "|", "\\", "\"", "'", ",", ":", "#", "é", "日", "🙂", "{", "}", "[",
            "]", "n", "r", "@", "after:", "rev1",
        ];
        const SPACES: [&str; 4] = [" ", "\n", "\r", "\t"];
        let len = self.pick(13);
        (0..len)
            .map(|_| {
                if whitespace && self.pick(5) == 0 {
                    SPACES[self.pick(SPACES.len())]
                } else {
                    PIECES[self.pick(PIECES.len())]
                }
            })
            .collect()
    }

    /// Text that survives trimming: evidence and notes are stored trimmed.
    fn trimmed(&mut self) -> String {
        format!("x{}x", self.text(true))
    }
}

fn run_strings(dir: &Path, args: &[String]) -> String {
    let args: Vec<&str> = args.iter().map(String::as_str).collect();
    run(dir, &args)
}

/// Whatever the free text holds, every reply stays one row per record and reads
/// back exactly: titles, tags, test cells, evidence and notes, all views.
#[test]
fn generated_free_text_reads_back_exactly() {
    const TESTS: usize = 20;
    for seed in 1..=6_u64 {
        let mut rng = Rng(seed);
        let dir = initialized();
        register(&dir, "dev", "developer");
        register(&dir, "rev", "reviewer");

        // A title is stored with its whitespace collapsed, so build one from
        // two words. It starts with `T` so it can never pass for a header label.
        let title = format!("T{} {}x", rng.text(false), rng.text(false));
        let tags: Vec<String> = (0..4).map(|_| rng.text(true)).collect();
        let mut tests: Vec<[String; 3]> = (0..TESTS)
            .map(|_| [rng.text(true), rng.text(true), rng.text(true)])
            .collect();
        // The one thing a row cannot protect is whitespace at the very end of a
        // reply: no `|` follows it, and readers (this harness included) trim the
        // end of what they receive. Every other cell keeps its spaces.
        tests[TESTS - 1][2].push('x');

        let mut add = vec!["add".to_string(), "--title".into(), title.clone()];
        for tag in &tags {
            add.extend(["--tag".into(), tag.clone()]);
        }
        for test in &tests {
            add.push("--test".into());
            add.extend(test.iter().cloned());
        }
        assert_eq!(run_strings(dir.path(), &add), "#1 todo", "seed {seed}");

        let shown = run(&dir, &["show", "1"]);
        // Header, the tags row and one row per test: no text started a new line.
        assert_eq!(shown.lines().count(), 2 + TESTS, "seed {seed}: {shown}");
        let parsed = parse_show(&shown);
        assert_eq!(parsed["title"], title, "seed {seed}");
        assert_eq!(parsed["tags"], serde_json::json!(tags), "seed {seed}");
        for (index, test) in tests.iter().enumerate() {
            let row = &parsed["tests"][index];
            assert_eq!(row["describe"], test[0], "seed {seed} test {index}");
            assert_eq!(row["input"], test[1], "seed {seed} test {index}");
            assert_eq!(row["output"], test[2], "seed {seed} test {index}");
        }
        // Every row splits into exactly the cells it should.
        for (row, test) in shown.lines().skip(2).zip(&tests) {
            let cells = split_cells(row);
            assert_eq!(cells.len(), 4, "seed {seed}: {row}");
            assert_eq!(cells[1..], test[..], "seed {seed}: {row}");
        }

        // The developer's order carries the same rows.
        let order = run(&dir, &["claim", "1", "--agent", "dev"]);
        assert_eq!(order.lines().count(), 1 + TESTS, "seed {seed}: {order}");
        assert_eq!(parse_order(&order)["tests"], parsed["tests"], "seed {seed}");

        // Evidence and notes: one verdict per test, then a rejection.
        let evidence: Vec<String> = (0..TESTS).map(|_| rng.trimmed()).collect();
        let mut submit = vec![
            "submit-review".to_string(),
            "1".into(),
            "--agent".into(),
            "dev".into(),
        ];
        for (index, text) in evidence.iter().enumerate() {
            submit.extend([
                if index % 3 == 0 { "--fail" } else { "--pass" }.to_string(),
                index.to_string(),
                text.clone(),
            ]);
        }
        assert_eq!(
            run_strings(dir.path(), &submit),
            "#1 review rev1",
            "seed {seed}"
        );

        let packet = run(&dir, &["claim", "1", "--agent", "rev"]);
        assert_eq!(packet.lines().count(), 1 + TESTS, "seed {seed}: {packet}");
        let packet = parse_order(&packet);
        for (index, text) in evidence.iter().enumerate() {
            let row = &packet["tests"][index];
            assert_eq!(row["evidence"], *text, "seed {seed} test {index}");
            let result = if index % 3 == 0 { "failed" } else { "passed" };
            assert_eq!(row["result"], result, "seed {seed} test {index}");
            assert_eq!(row["describe"], tests[index][0], "seed {seed} test {index}");
        }

        let notes = rng.trimmed();
        let reply = run(
            &dir,
            &["request-changes", "1", "--agent", "rev", "--notes", &notes],
        );
        assert_eq!(reply, "#1 in_progress", "seed {seed}");
        let order = run(&dir, &["claim", "1", "--agent", "dev"]);
        assert_eq!(order.lines().count(), 2 + TESTS, "seed {seed}: {order}");
        assert_eq!(parse_order(&order)["changes"], notes, "seed {seed}");

        let history = run(&dir, &["show", "1", "--history"]);
        // Header, tags, changes, the tests, one revision row and its review.
        assert_eq!(
            history.lines().count(),
            3 + TESTS + 2,
            "seed {seed}: {history}"
        );
        let history = parse_show(&history);
        let results: Vec<String> = evidence
            .iter()
            .enumerate()
            .map(|(index, text)| {
                let result = if index % 3 == 0 { "failed" } else { "passed" };
                format!("{result}: {text}")
            })
            .collect();
        assert_eq!(
            history["history"][0]["results"],
            serde_json::json!(results),
            "seed {seed}"
        );
        assert_eq!(history["history"][0]["notes"], notes, "seed {seed}");
    }
}
