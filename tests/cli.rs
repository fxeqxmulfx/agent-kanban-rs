//! Black-box integration tests that exercise the compiled `agent-kanban` binary as
//! a subprocess, using `assert_cmd`. Every test gets its own `TempDir` and
//! runs all commands with `.current_dir(&dir)` so tests never interfere with
//! each other or with the repo's own working directory.

use assert_cmd::Command;
use predicates::str::contains;
use serde_json::Value;
use std::path::Path;
use tempfile::TempDir;

/// Build a `kanban` command rooted at `dir`.
fn kanban(dir: &TempDir) -> Command {
    let mut cmd = Command::cargo_bin("agent-kanban").unwrap();
    cmd.current_dir(dir);
    cmd
}

/// Build a `kanban` command rooted at an arbitrary path (not necessarily a
/// `TempDir` root), for tests that need to run from a subdirectory.
fn kanban_at(dir: impl AsRef<Path>) -> Command {
    let mut cmd = Command::cargo_bin("agent-kanban").unwrap();
    cmd.current_dir(dir.as_ref());
    cmd
}

/// Run a kanban command rooted at an arbitrary path and parse its stdout as
/// JSON, asserting success.
fn run_json_at(dir: impl AsRef<Path>, args: &[&str]) -> Value {
    let output = kanban_at(dir.as_ref()).args(args).output().unwrap();
    assert!(
        output.status.success(),
        "command {:?} failed: stdout={} stderr={}",
        args,
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    serde_json::from_slice(&output.stdout).unwrap_or_else(|e| {
        panic!(
            "invalid JSON from {:?}: {e}\nstdout={}",
            args,
            String::from_utf8_lossy(&output.stdout)
        )
    })
}

/// Run a kanban command and parse its stdout as JSON, asserting success.
fn run_json(dir: &TempDir, args: &[&str]) -> Value {
    let output = kanban(dir).args(args).output().unwrap();
    assert!(
        output.status.success(),
        "command {:?} failed: stdout={} stderr={}",
        args,
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    serde_json::from_slice(&output.stdout).unwrap_or_else(|e| {
        panic!(
            "invalid JSON from {:?}: {e}\nstdout={}",
            args,
            String::from_utf8_lossy(&output.stdout)
        )
    })
}

fn init(dir: &TempDir) {
    kanban(dir).arg("init").assert().success();
}

fn register(dir: &TempDir, name: &str) {
    register_role(dir, name, "developer");
}

fn register_role(dir: &TempDir, name: &str, role: &str) {
    kanban(dir)
        .args(["agent", "register", name, "--role", role])
        .assert()
        .success();
}

fn remove_agent_with_no_claims(dir: &TempDir, name: &str) {
    let removed = run_json(dir, &["agent", "remove", name]);
    assert_eq!(removed["removed"], name);
    assert_eq!(removed["released_tasks"].as_array().unwrap().len(), 0);
}

#[test]
fn golden_path_full_lifecycle() {
    let dir = TempDir::new().unwrap();

    // init
    kanban(&dir)
        .arg("init")
        .assert()
        .success()
        .stdout(contains("initialized"));

    register(&dir, "agent-alpha");
    register_role(&dir, "reviewer-beta", "reviewer");

    // add (with tags + a test)
    let test_json = r#"{"describe":"basic add","input":"2+2","output":"4"}"#;
    let created = run_json(
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
            test_json,
        ],
    );
    let task_id = created["id"].as_i64().unwrap();
    assert_eq!(created["title"], "Implement feature X");
    assert_eq!(created["priority"], "high");
    assert_eq!(created["status"], "todo");
    assert_eq!(created["executor"], Value::Null);
    assert_eq!(created["tags"].as_array().unwrap().len(), 2);
    assert_eq!(created["tests"].as_array().unwrap().len(), 1);

    // list shows it
    let listed = run_json(&dir, &["list"]);
    let arr = listed.as_array().unwrap();
    assert_eq!(arr.len(), 1);
    assert_eq!(arr[0]["id"].as_i64().unwrap(), task_id);

    // show round-trips it
    let shown = run_json(&dir, &["show", &task_id.to_string()]);
    assert_eq!(shown["title"], "Implement feature X");
    assert_eq!(shown["tags"][0], "backend");
    assert_eq!(shown["tags"][1], "urgent-fix");
    assert_eq!(shown["tests"][0]["describe"], "basic add");

    // claim succeeds and flips status to in_progress
    let claimed = run_json(
        &dir,
        &["claim", &task_id.to_string(), "--agent", "agent-alpha"],
    );
    assert_eq!(claimed["executor"], "agent-alpha");
    assert_eq!(claimed["status"], "in_progress");

    let submitted = run_json(
        &dir,
        &[
            "submit-review",
            &task_id.to_string(),
            "--agent",
            "agent-alpha",
            "--result",
            r#"{"criterion":0,"status":"passed","evidence":"cargo test: basic add passed"}"#,
        ],
    );
    assert_eq!(submitted["status"], "review");
    assert_eq!(submitted["executor"], Value::Null);
    assert_eq!(submitted["revision"], 1);
    assert_eq!(submitted["acceptance_results"][0]["status"], "passed");

    kanban(&dir)
        .args(["edit", &task_id.to_string(), "--title", "renamed"])
        .assert()
        .failure()
        .stderr(contains("in review"));

    let review_claim = run_json(
        &dir,
        &[
            "claim-review",
            &task_id.to_string(),
            "--agent",
            "reviewer-beta",
        ],
    );
    assert_eq!(review_claim["status"], "review");
    assert_eq!(review_claim["executor"], "reviewer-beta");
    assert_eq!(review_claim["executor_role"], "reviewer");

    let approved = run_json(
        &dir,
        &[
            "approve",
            &task_id.to_string(),
            "--agent",
            "reviewer-beta",
            "--notes",
            "verified",
        ],
    );
    assert_eq!(approved["status"], "done");
    assert_eq!(approved["executor"], Value::Null);
    assert_eq!(approved["review_history"][0]["decision"], "approved");
    assert_eq!(approved["review_history"][0]["executor"], "reviewer-beta");

    remove_agent_with_no_claims(&dir, "agent-alpha");
    remove_agent_with_no_claims(&dir, "reviewer-beta");

    let shown = run_json(&dir, &["show", &task_id.to_string()]);
    assert_eq!(shown["status"], "done");
    assert_eq!(shown["acceptance_results"][0]["executor"], "agent-alpha");
    assert_eq!(shown["review_history"][0]["executor"], "reviewer-beta");
}

#[test]
fn request_changes_starts_a_new_revision_and_preserves_review_history() {
    let dir = TempDir::new().unwrap();
    init(&dir);
    register(&dir, "developer");
    register_role(&dir, "reviewer", "reviewer");
    let created = run_json(
        &dir,
        &[
            "add",
            "--title",
            "review twice",
            "--priority",
            "high",
            "--test",
            r#"{"describe":"behavior","input":"scenario","output":"expected"}"#,
        ],
    );
    let id = created["id"].as_i64().unwrap().to_string();

    run_json(&dir, &["claim", &id, "--agent", "developer"]);
    run_json(
        &dir,
        &[
            "submit-review",
            &id,
            "--agent",
            "developer",
            "--result",
            r#"{"criterion":0,"status":"failed","evidence":"integration test failed"}"#,
        ],
    );
    run_json(&dir, &["claim-review", &id, "--agent", "reviewer"]);

    kanban(&dir)
        .args(["approve", &id, "--agent", "reviewer"])
        .assert()
        .failure()
        .stderr(contains("failed acceptance"));

    let changes = run_json(
        &dir,
        &[
            "request-changes",
            &id,
            "--agent",
            "reviewer",
            "--notes",
            "fix the integration failure",
        ],
    );
    assert_eq!(changes["status"], "in_progress");
    assert_eq!(changes["executor"], Value::Null);
    assert_eq!(changes["revision"], 1);
    assert_eq!(
        changes["review_history"][0]["decision"],
        "changes_requested"
    );
    assert_eq!(
        changes["review_history"][0]["notes"],
        "fix the integration failure"
    );

    run_json(&dir, &["claim", &id, "--agent", "developer"]);
    let second_submission = run_json(
        &dir,
        &[
            "submit-review",
            &id,
            "--agent",
            "developer",
            "--result",
            r#"{"criterion":0,"status":"passed","evidence":"integration test passed"}"#,
        ],
    );
    assert_eq!(second_submission["revision"], 2);
    assert_eq!(
        second_submission["acceptance_results"]
            .as_array()
            .unwrap()
            .len(),
        2
    );
    run_json(&dir, &["claim-review", &id, "--agent", "reviewer"]);
    let approved = run_json(&dir, &["approve", &id, "--agent", "reviewer"]);
    assert_eq!(approved["status"], "done");
    assert_eq!(approved["review_history"][0]["revision"], 1);
    assert_eq!(approved["review_history"][1]["revision"], 2);
    assert_eq!(approved["review_history"][1]["decision"], "approved");
}

#[test]
fn expired_lease_is_inactive_and_can_be_reclaimed() {
    let dir = TempDir::new().unwrap();
    init(&dir);
    register(&dir, "developer-a");
    register(&dir, "developer-b");
    let created = run_json(
        &dir,
        &[
            "add",
            "--title",
            "lease task",
            "--priority",
            "medium",
            "--test",
            r#"{"describe":"d","input":"i","output":"o"}"#,
        ],
    );
    let id = created["id"].as_i64().unwrap();
    let id_string = id.to_string();
    run_json(
        &dir,
        &[
            "claim",
            &id_string,
            "--agent",
            "developer-a",
            "--lease-seconds",
            "60",
        ],
    );

    let db_path = dir.path().join(".kanban").join("board.db");
    let conn = rusqlite::Connection::open(db_path).unwrap();
    conn.execute(
        "UPDATE tasks SET lease_expires_at = datetime('now', '-1 second') WHERE id = ?1",
        [id],
    )
    .unwrap();
    drop(conn);

    let expired = run_json(&dir, &["show", &id_string]);
    assert_eq!(expired["status"], "in_progress");
    assert_eq!(expired["executor"], Value::Null);
    let status = run_json(&dir, &["status"]);
    assert_eq!(status["agents"]["developer-a"], 0);

    let reclaimed = run_json(&dir, &["claim", &id_string, "--agent", "developer-b"]);
    assert_eq!(reclaimed["executor"], "developer-b");

    kanban(&dir)
        .args(["release", &id_string, "--agent", "developer-a"])
        .assert()
        .failure()
        .stderr(contains("developer-b"));
    let released = run_json(&dir, &["release", &id_string, "--agent", "developer-b"]);
    assert_eq!(released["executor"], Value::Null);
    assert_eq!(released["status"], "in_progress");
}

#[test]
fn transition_table_is_exposed_and_generic_move_cannot_bypass_review() {
    let dir = TempDir::new().unwrap();
    init(&dir);
    let transitions = run_json(&dir, &["transitions"]);
    assert!(transitions.as_array().unwrap().iter().any(|transition| {
        transition["command"] == "approve"
            && transition["from"] == "review"
            && transition["to"] == "done"
    }));
    assert!(
        !transitions
            .as_array()
            .unwrap()
            .iter()
            .any(|transition| { transition["command"] == "move" && transition["to"] == "done" })
    );

    let created = run_json(
        &dir,
        &[
            "add",
            "--title",
            "cannot skip review",
            "--priority",
            "low",
            "--test",
            r#"{"describe":"d","input":"i","output":"o"}"#,
        ],
    );
    let id = created["id"].as_i64().unwrap().to_string();
    kanban(&dir)
        .args(["move", &id, "--status", "done"])
        .assert()
        .failure()
        .stderr(contains("lifecycle review commands"));
    let shown = run_json(&dir, &["show", &id]);
    assert_eq!(shown["status"], "todo");
}

#[test]
fn registering_duplicate_agent_fails_cleanly() {
    let dir = TempDir::new().unwrap();
    init(&dir);
    register(&dir, "dup-agent");

    kanban(&dir)
        .args(["agent", "register", "dup-agent"])
        .assert()
        .failure()
        .stderr(contains("already exists"));
}

#[test]
fn add_with_invalid_priority_fails() {
    let dir = TempDir::new().unwrap();
    init(&dir);

    kanban(&dir)
        .args([
            "add",
            "--title",
            "bad task",
            "--priority",
            "urgentish",
            "--test",
            r#"{"describe":"d","input":"i","output":"o"}"#,
        ])
        .assert()
        .failure()
        .stderr(contains("invalid priority"));
}

#[test]
fn add_with_test_missing_required_field_fails() {
    let dir = TempDir::new().unwrap();
    init(&dir);

    kanban(&dir)
        .args([
            "add",
            "--title",
            "bad task",
            "--priority",
            "low",
            "--test",
            r#"{"describe":"d","input":"i"}"#, // missing "output"
        ])
        .assert()
        .failure()
        .stderr(contains("output"));
}

#[test]
fn claim_unregistered_agent_fails_and_leaves_executor_null() {
    let dir = TempDir::new().unwrap();
    init(&dir);
    let created = run_json(
        &dir,
        &[
            "add",
            "--title",
            "t1",
            "--priority",
            "low",
            "--test",
            r#"{"describe":"d","input":"i","output":"o"}"#,
        ],
    );
    let id = created["id"].as_i64().unwrap();

    kanban(&dir)
        .args(["claim", &id.to_string(), "--agent", "ghost-agent"])
        .assert()
        .failure()
        .stderr(contains("not registered"));

    // Critical: task's executor is still null afterward.
    let shown = run_json(&dir, &["show", &id.to_string()]);
    assert_eq!(shown["executor"], Value::Null);
    assert_eq!(shown["status"], "todo");
}

#[test]
fn claim_on_already_claimed_task_fails() {
    let dir = TempDir::new().unwrap();
    init(&dir);
    register(&dir, "agent-a");
    register(&dir, "agent-b");
    let created = run_json(
        &dir,
        &[
            "add",
            "--title",
            "t1",
            "--priority",
            "low",
            "--test",
            r#"{"describe":"d","input":"i","output":"o"}"#,
        ],
    );
    let id = created["id"].as_i64().unwrap();

    run_json(&dir, &["claim", &id.to_string(), "--agent", "agent-a"]);

    kanban(&dir)
        .args(["claim", &id.to_string(), "--agent", "agent-b"])
        .assert()
        .failure()
        .stderr(contains("already claimed"));

    let shown = run_json(&dir, &["show", &id.to_string()]);
    assert_eq!(shown["executor"], "agent-a");
}

#[test]
fn edit_and_remove_blocked_while_claimed() {
    let dir = TempDir::new().unwrap();
    init(&dir);
    register(&dir, "agent-a");
    let created = run_json(
        &dir,
        &[
            "add",
            "--title",
            "t1",
            "--priority",
            "low",
            "--test",
            r#"{"describe":"d","input":"i","output":"o"}"#,
        ],
    );
    let id = created["id"].as_i64().unwrap();
    run_json(&dir, &["claim", &id.to_string(), "--agent", "agent-a"]);

    kanban(&dir)
        .args(["edit", &id.to_string(), "--title", "x"])
        .assert()
        .failure()
        .stderr(contains("claimed"));

    kanban(&dir)
        .args(["remove", &id.to_string()])
        .assert()
        .failure()
        .stderr(contains("claimed"));
}

#[test]
fn edit_and_remove_blocked_when_done() {
    let dir = TempDir::new().unwrap();
    init(&dir);
    register(&dir, "developer");
    register_role(&dir, "reviewer", "reviewer");
    let created = run_json(
        &dir,
        &[
            "add",
            "--title",
            "t1",
            "--priority",
            "low",
            "--test",
            r#"{"describe":"d","input":"i","output":"o"}"#,
        ],
    );
    let id = created["id"].as_i64().unwrap();
    run_json(&dir, &["claim", &id.to_string(), "--agent", "developer"]);
    run_json(
        &dir,
        &[
            "submit-review",
            &id.to_string(),
            "--agent",
            "developer",
            "--result",
            r#"{"criterion":0,"status":"passed","evidence":"verified"}"#,
        ],
    );
    run_json(
        &dir,
        &["claim-review", &id.to_string(), "--agent", "reviewer"],
    );
    run_json(&dir, &["approve", &id.to_string(), "--agent", "reviewer"]);

    kanban(&dir)
        .args(["edit", &id.to_string(), "--title", "x"])
        .assert()
        .failure()
        .stderr(contains("immutable"));

    kanban(&dir)
        .args(["remove", &id.to_string()])
        .assert()
        .failure()
        .stderr(contains("can't be removed"));
}

#[test]
fn commands_before_init_fail_cleanly() {
    let dir = TempDir::new().unwrap();
    // no init() call here

    kanban(&dir)
        .args(["agent", "list"])
        .assert()
        .failure()
        .stderr(contains("agent-kanban init"));

    kanban(&dir)
        .args(["list"])
        .assert()
        .failure()
        .stderr(contains("agent-kanban init"));
}

#[test]
fn agent_remove_cascade_releases_claimed_task() {
    let dir = TempDir::new().unwrap();
    init(&dir);
    register(&dir, "agent-a");
    let created = run_json(
        &dir,
        &[
            "add",
            "--title",
            "t1",
            "--priority",
            "low",
            "--test",
            r#"{"describe":"d","input":"i","output":"o"}"#,
        ],
    );
    let id = created["id"].as_i64().unwrap();
    run_json(&dir, &["claim", &id.to_string(), "--agent", "agent-a"]);

    let removed = run_json(&dir, &["agent", "remove", "agent-a"]);
    assert_eq!(removed["removed"], "agent-a");
    assert_eq!(removed["released_tasks"][0].as_i64().unwrap(), id);

    let shown = run_json(&dir, &["show", &id.to_string()]);
    assert_eq!(shown["executor"], Value::Null);
    assert_eq!(shown["status"], "in_progress");
}

#[test]
fn list_filtering_and_sorting() {
    let dir = TempDir::new().unwrap();
    init(&dir);
    register(&dir, "agent-a");

    // t1: urgent, tag "alpha"
    let t1 = run_json(
        &dir,
        &[
            "add",
            "--title",
            "t1",
            "--priority",
            "urgent",
            "--tag",
            "alpha",
            "--test",
            r#"{"describe":"d","input":"i","output":"o"}"#,
        ],
    );
    // t2: low, tag "beta"
    run_json(
        &dir,
        &[
            "add",
            "--title",
            "t2",
            "--priority",
            "low",
            "--tag",
            "beta",
            "--test",
            r#"{"describe":"d","input":"i","output":"o"}"#,
        ],
    );
    // t3: medium, tag "alpha"
    run_json(
        &dir,
        &[
            "add",
            "--title",
            "t3",
            "--priority",
            "medium",
            "--tag",
            "alpha",
            "--test",
            r#"{"describe":"d","input":"i","output":"o"}"#,
        ],
    );
    // t4: high
    run_json(
        &dir,
        &[
            "add",
            "--title",
            "t4",
            "--priority",
            "high",
            "--test",
            r#"{"describe":"d","input":"i","output":"o"}"#,
        ],
    );

    let t1_id = t1["id"].as_i64().unwrap();
    run_json(&dir, &["claim", &t1_id.to_string(), "--agent", "agent-a"]);

    // --status
    let by_status = run_json(&dir, &["list", "--status", "in_progress"]);
    let arr = by_status.as_array().unwrap();
    assert_eq!(arr.len(), 1);
    assert_eq!(arr[0]["title"], "t1");

    // --tag
    let by_tag = run_json(&dir, &["list", "--tag", "alpha"]);
    let arr = by_tag.as_array().unwrap();
    let titles: Vec<&str> = arr.iter().map(|t| t["title"].as_str().unwrap()).collect();
    assert_eq!(titles.len(), 2);
    assert!(titles.contains(&"t1"));
    assert!(titles.contains(&"t3"));

    // --priority
    let by_priority = run_json(&dir, &["list", "--priority", "medium"]);
    let arr = by_priority.as_array().unwrap();
    assert_eq!(arr.len(), 1);
    assert_eq!(arr[0]["title"], "t3");

    // --executor
    let by_executor = run_json(&dir, &["list", "--executor", "agent-a"]);
    let arr = by_executor.as_array().unwrap();
    assert_eq!(arr.len(), 1);
    assert_eq!(arr[0]["title"], "t1");

    // --sort priority: urgent, high, medium, low (not alphabetical)
    let sorted = run_json(&dir, &["list", "--sort", "priority"]);
    let arr = sorted.as_array().unwrap();
    let titles: Vec<&str> = arr.iter().map(|t| t["title"].as_str().unwrap()).collect();
    assert_eq!(titles, vec!["t1", "t4", "t3", "t2"]);
}

/// Discovery must find the *closest* `.kanban/` walking up from cwd, and must
/// not merge with or fall through to a parent project's `.kanban/`.
#[test]
fn nested_project_discovery_child_wins() {
    let root = TempDir::new().unwrap();

    // init at the root and add a distinctive task there.
    init(&root);
    run_json(
        &root,
        &[
            "add",
            "--title",
            "parent task",
            "--priority",
            "low",
            "--test",
            r#"{"describe":"d","input":"i","output":"o"}"#,
        ],
    );

    // A separate, independently-initialized project nested inside the root's
    // directory tree.
    let child = root.path().join("child");
    std::fs::create_dir_all(&child).unwrap();
    run_json_at(&child, &["init"]);
    run_json_at(
        &child,
        &[
            "add",
            "--title",
            "child task",
            "--priority",
            "low",
            "--test",
            r#"{"describe":"d","input":"i","output":"o"}"#,
        ],
    );

    // Run `list` from an even deeper, un-initialized directory. Discovery
    // should walk up only as far as `child/.kanban`, not the parent's.
    let deeper = child.join("deeper");
    std::fs::create_dir_all(&deeper).unwrap();

    let listed = run_json_at(&deeper, &["list"]);
    let arr = listed.as_array().unwrap();
    let titles: Vec<&str> = arr.iter().map(|t| t["title"].as_str().unwrap()).collect();
    assert_eq!(titles, vec!["child task"]);
    assert!(!titles.contains(&"parent task"));
}

/// Commands must work from any subdirectory of an initialized project, not
/// just its root, by walking up to find the nearest `.kanban/`.
#[test]
fn runs_from_subdirectory_of_initialized_project() {
    let root = TempDir::new().unwrap();
    init(&root);
    let created = run_json(
        &root,
        &[
            "add",
            "--title",
            "root task",
            "--priority",
            "low",
            "--test",
            r#"{"describe":"d","input":"i","output":"o"}"#,
        ],
    );
    let id = created["id"].as_i64().unwrap();

    let sub = root.path().join("subdir");
    std::fs::create_dir_all(&sub).unwrap();

    let listed = run_json_at(&sub, &["list"]);
    let arr = listed.as_array().unwrap();
    assert_eq!(arr.len(), 1);
    assert_eq!(arr[0]["id"].as_i64().unwrap(), id);
    assert_eq!(arr[0]["title"], "root task");
}

/// `--pretty` is a global flag that must precede the subcommand. It should
/// produce indented, multi-line JSON, unlike the default compact output.
#[test]
fn pretty_flag_produces_indented_output() {
    let dir = TempDir::new().unwrap();
    init(&dir);

    let test_json = r#"{"describe":"d","input":"i","output":"o"}"#;

    // Compact (default) output: a single line of JSON plus one trailing
    // newline from `println!`.
    let compact_output = kanban(&dir)
        .args([
            "add",
            "--title",
            "t",
            "--priority",
            "low",
            "--test",
            test_json,
        ])
        .output()
        .unwrap();
    assert!(compact_output.status.success());
    let compact_stdout = String::from_utf8_lossy(&compact_output.stdout).to_string();
    assert_eq!(
        compact_stdout.matches('\n').count(),
        1,
        "compact JSON should have exactly one trailing newline, got: {compact_stdout:?}"
    );

    // Pretty output: `--pretty` must come before the subcommand.
    let pretty_output = kanban(&dir)
        .args([
            "--pretty",
            "add",
            "--title",
            "t2",
            "--priority",
            "low",
            "--test",
            test_json,
        ])
        .output()
        .unwrap();
    assert!(pretty_output.status.success());
    let pretty_stdout = String::from_utf8_lossy(&pretty_output.stdout).to_string();
    assert!(
        pretty_stdout.matches('\n').count() > 1,
        "pretty JSON should be multi-line, got: {pretty_stdout:?}"
    );
    // The indentation itself: a newline followed by leading whitespace.
    assert!(
        pretty_stdout.contains("\n ") || pretty_stdout.contains("\n\t"),
        "pretty JSON should contain indented lines, got: {pretty_stdout:?}"
    );
}

/// Running `init` a second time on an already-initialized project must
/// succeed (exit 0) and must not wipe existing data.
#[test]
fn init_twice_is_idempotent_and_preserves_data() {
    let dir = TempDir::new().unwrap();
    init(&dir);
    register(&dir, "agent-a");

    // Second init should still succeed.
    kanban(&dir).arg("init").assert().success();

    // The agent registered before the second init must still be present.
    let agents = run_json(&dir, &["agent", "list"]);
    let arr = agents.as_array().unwrap();
    let has_agent_a = arr.iter().any(|a| {
        a.get("name")
            .and_then(|n| n.as_str())
            .unwrap_or_else(|| a.as_str().unwrap())
            == "agent-a"
    });
    assert!(
        has_agent_a,
        "expected agent-a to survive a second init, got: {arr:?}"
    );
}

/// Every error must be `{"error": "..."}` JSON on stderr per this tool's
/// documented contract -- including clap's own parse-level failures (bad
/// argument types, missing required flags, unknown subcommands), which by
/// default would otherwise print clap's plain multi-line human-readable
/// text instead of going through the rest of the program's error handling.
#[test]
fn clap_parse_errors_are_still_json() {
    let dir = TempDir::new().unwrap();
    init(&dir);

    // Non-numeric id: fails clap's i64 value parser.
    let output = kanban(&dir).args(["show", "abc"]).output().unwrap();
    assert!(!output.status.success());
    assert_eq!(output.status.code(), Some(2));
    let stderr: Value = serde_json::from_slice(&output.stderr).unwrap_or_else(|e| {
        panic!(
            "stderr was not valid JSON: {e}\nstderr={}",
            String::from_utf8_lossy(&output.stderr)
        )
    });
    assert!(stderr["error"].as_str().unwrap().contains("invalid digit"));

    // Missing a required argument.
    let output = kanban(&dir).args(["add", "--title", "t"]).output().unwrap();
    assert!(!output.status.success());
    let stderr: Value = serde_json::from_slice(&output.stderr).unwrap_or_else(|e| {
        panic!(
            "stderr was not valid JSON: {e}\nstderr={}",
            String::from_utf8_lossy(&output.stderr)
        )
    });
    assert!(stderr["error"].as_str().unwrap().contains("required"));

    // Unknown subcommand.
    let output = kanban(&dir).args(["bogus-command"]).output().unwrap();
    assert!(!output.status.success());
    let stderr: Value = serde_json::from_slice(&output.stderr).unwrap_or_else(|e| {
        panic!(
            "stderr was not valid JSON: {e}\nstderr={}",
            String::from_utf8_lossy(&output.stderr)
        )
    });
    assert!(
        stderr["error"]
            .as_str()
            .unwrap()
            .contains("unrecognized subcommand")
    );
}

/// `--help`/`--version` are not errors and must remain clap's normal
/// human-readable plain text (exit 0), not JSON -- this test guards against
/// the JSON-error fix above accidentally over-broadening to cover these too.
#[test]
fn help_flag_remains_plain_text_not_json() {
    let dir = TempDir::new().unwrap();
    init(&dir);

    let output = kanban(&dir).arg("--help").output().unwrap();
    assert!(output.status.success());
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("Usage:"));
    assert!(
        serde_json::from_str::<Value>(&stdout).is_err(),
        "--help output should not be JSON: {stdout:?}"
    );
}

/// `--version` must be a registered flag (clap's `#[derive(Parser)]` needs
/// the bare `version` keyword in `#[command(...)]` to auto-populate it from
/// Cargo.toml once any other field is set explicitly -- confirmed missing
/// entirely before that fix: clap reported "unexpected argument" instead of
/// printing a version, since it was never registered as a valid flag at
/// all). Not an error, so it must remain plain text (exit 0), not JSON.
#[test]
fn version_flag_is_registered_and_plain_text() {
    let dir = TempDir::new().unwrap();
    init(&dir);

    let output = kanban(&dir).arg("--version").output().unwrap();
    assert!(
        output.status.success(),
        "stdout={} stderr={}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("agent-kanban"));
    assert!(
        serde_json::from_str::<Value>(&stdout).is_err(),
        "--version output should not be JSON: {stdout:?}"
    );
}

/// `--pretty` must apply to error output too (a command-logic failure after
/// a successful parse), not just success output -- otherwise the two output
/// paths are inconsistent about honoring the flag.
#[test]
fn pretty_flag_applies_to_error_output_too() {
    let dir = TempDir::new().unwrap();
    init(&dir);

    let compact = kanban(&dir).args(["show", "999"]).output().unwrap();
    assert!(!compact.status.success());
    let compact_stderr = String::from_utf8_lossy(&compact.stderr).to_string();
    assert_eq!(
        compact_stderr.matches('\n').count(),
        1,
        "compact error JSON should have exactly one trailing newline, got: {compact_stderr:?}"
    );

    let pretty = kanban(&dir)
        .args(["--pretty", "show", "999"])
        .output()
        .unwrap();
    assert!(!pretty.status.success());
    let pretty_stderr = String::from_utf8_lossy(&pretty.stderr).to_string();
    assert!(
        pretty_stderr.matches('\n').count() > 1,
        "pretty error JSON should be multi-line, got: {pretty_stderr:?}"
    );
    let parsed: Value = serde_json::from_str(&pretty_stderr).unwrap();
    assert_eq!(parsed["error"], "task 999 not found");
}

/// Every value that flows into a SQL statement (title, tags, filter values)
/// is bound via parameters rather than string-concatenated -- this proves
/// that empirically with adversarial-looking input, rather than relying on
/// "the code looks parameterized" from reading it. A naively-concatenated
/// query would corrupt/misbehave on these; a properly parameterized one
/// treats them as inert data.
#[test]
fn sql_special_characters_are_treated_as_inert_data() {
    let dir = TempDir::new().unwrap();
    init(&dir);

    let evil_title = "robert'); DROP TABLE tasks; --";
    let evil_tag = "'; DROP TABLE agents; --";
    let created = run_json(
        &dir,
        &[
            "add",
            "--title",
            evil_title,
            "--priority",
            "low",
            "--tag",
            evil_tag,
            "--test",
            r#"{"describe":"d","input":"i","output":"o"}"#,
        ],
    );
    assert_eq!(created["title"], evil_title);
    let id = created["id"].as_i64().unwrap();

    // Filtering by the exact adversarial tag value must find it, and the
    // tables must obviously still exist (both DROP TABLE attempts, if they
    // were ever interpreted as SQL rather than data, would have destroyed
    // the schema and every subsequent command in this test would fail).
    let by_tag = run_json(&dir, &["list", "--tag", evil_tag]);
    let arr = by_tag.as_array().unwrap();
    assert_eq!(arr.len(), 1);
    assert_eq!(arr[0]["id"].as_i64().unwrap(), id);

    let shown = run_json(&dir, &["show", &id.to_string()]);
    assert_eq!(shown["title"], evil_title);

    // Prove the tables are intact by continuing to use them normally.
    run_json(&dir, &["agent", "register", "alice"]);
    let all = run_json(&dir, &["list"]);
    assert_eq!(all.as_array().unwrap().len(), 1);
}

/// `list`'s filters and `--sort` must work correctly when combined in a
/// single call, not just independently -- the dynamic query builds a
/// `WHERE ... ORDER BY ...` clause from whichever flags are present.
#[test]
fn list_combines_filter_and_sort_in_one_call() {
    let dir = TempDir::new().unwrap();
    init(&dir);

    for (title, priority, status) in [
        ("backlog-low", "low", "backlog"),
        ("todo-urgent", "urgent", "todo"),
        ("todo-high", "high", "todo"),
        ("todo-low", "low", "todo"),
    ] {
        let created = run_json(
            &dir,
            &[
                "add",
                "--title",
                title,
                "--priority",
                priority,
                "--test",
                r#"{"describe":"d","input":"i","output":"o"}"#,
            ],
        );
        if status != "todo" {
            let id = created["id"].as_i64().unwrap().to_string();
            run_json(&dir, &["move", &id, "--status", status]);
        }
    }

    // --status todo (excludes backlog-low) AND --sort priority together:
    // must both filter out the backlog task and order the remaining three
    // by severity, not alphabetically and not unfiltered.
    let result = run_json(&dir, &["list", "--status", "todo", "--sort", "priority"]);
    let arr = result.as_array().unwrap();
    let titles: Vec<&str> = arr.iter().map(|t| t["title"].as_str().unwrap()).collect();
    assert_eq!(titles, vec!["todo-urgent", "todo-high", "todo-low"]);
}

/// `--table` on an array result (`list`) renders an aligned table with
/// uppercase column headers, not JSON.
#[test]
fn table_flag_renders_list_as_aligned_table() {
    let dir = TempDir::new().unwrap();
    init(&dir);
    run_json(
        &dir,
        &[
            "add",
            "--title",
            "fix bug",
            "--priority",
            "high",
            "--test",
            r#"{"describe":"d","input":"i","output":"o"}"#,
        ],
    );

    let output = kanban(&dir).args(["--table", "list"]).output().unwrap();
    assert!(output.status.success());
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("TITLE"));
    assert!(stdout.contains("PRIORITY"));
    assert!(stdout.contains("fix bug"));
    assert!(stdout.contains("high"));
    assert!(
        serde_json::from_str::<Value>(&stdout).is_err(),
        "--table output should not be JSON: {stdout:?}"
    );
}

/// `--table` on a single-object result (`show`) renders a FIELD/VALUE table.
#[test]
fn table_flag_renders_single_object_as_key_value_table() {
    let dir = TempDir::new().unwrap();
    init(&dir);
    let created = run_json(
        &dir,
        &[
            "add",
            "--title",
            "fix bug",
            "--priority",
            "high",
            "--test",
            r#"{"describe":"d","input":"i","output":"o"}"#,
        ],
    );
    let id = created["id"].as_i64().unwrap().to_string();

    let output = kanban(&dir)
        .args(["--table", "show", &id])
        .output()
        .unwrap();
    assert!(output.status.success());
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("FIELD"));
    assert!(stdout.contains("VALUE"));
    assert!(stdout.contains("title"));
    assert!(stdout.contains("fix bug"));
}

/// `status` returns a flat object, same shape as `show`/`add`, so `--table`
/// renders it through the same FIELD/VALUE path -- including the nested
/// `agents` object, which must render as inline compact JSON within its
/// cell rather than a nested table.
#[test]
fn table_flag_renders_status_as_key_value_table_with_inline_agents() {
    let dir = TempDir::new().unwrap();
    init(&dir);
    register(&dir, "alice");

    let output = kanban(&dir).args(["--table", "status"]).output().unwrap();
    assert!(output.status.success());
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("FIELD"));
    assert!(stdout.contains("VALUE"));
    assert!(stdout.contains("total"));
    assert!(stdout.contains("agents"));
    assert!(stdout.contains(r#"{"alice":0}"#));
}

/// `--table` on an empty `list` result prints a friendly message rather
/// than an empty/malformed table (there are no rows to infer columns from).
#[test]
fn table_flag_renders_empty_list_as_friendly_message() {
    let dir = TempDir::new().unwrap();
    init(&dir);

    kanban(&dir)
        .args(["--table", "list"])
        .assert()
        .success()
        .stdout(contains("no results"));
}

/// `--table` applies to error output too, consistently with `--pretty`.
#[test]
fn table_flag_renders_error_as_table() {
    let dir = TempDir::new().unwrap();
    init(&dir);

    let output = kanban(&dir)
        .args(["--table", "show", "999"])
        .output()
        .unwrap();
    assert!(!output.status.success());
    let stdout_and_stderr = format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(stdout_and_stderr.contains("error"));
    assert!(stdout_and_stderr.contains("not found"));
    assert!(
        serde_json::from_str::<Value>(&stdout_and_stderr).is_err(),
        "--table error output should not be JSON: {stdout_and_stderr:?}"
    );
}

/// `--pretty` and `--table` are mutually exclusive output modes; combining
/// them must be a clean, clap-level usage error (still JSON, per the
/// clap-parse-error contract established earlier).
#[test]
fn pretty_and_table_are_mutually_exclusive() {
    let dir = TempDir::new().unwrap();
    init(&dir);

    let output = kanban(&dir)
        .args(["--pretty", "--table", "list"])
        .output()
        .unwrap();
    assert!(!output.status.success());
    assert_eq!(output.status.code(), Some(2));
    let stderr: Value = serde_json::from_slice(&output.stderr).unwrap();
    assert!(
        stderr["error"]
            .as_str()
            .unwrap()
            .contains("cannot be used with")
    );
}

/// `agent-kanban init` must stamp the schema version via `PRAGMA user_version`,
/// not leave it at `SQLite`'s default of 0.
#[test]
fn init_sets_schema_version_pragma() {
    let dir = TempDir::new().unwrap();
    init(&dir);

    let db_path = dir.path().join(".kanban").join("board.db");
    let conn = rusqlite::Connection::open(&db_path).unwrap();
    let version: i32 = conn
        .query_row("PRAGMA user_version", [], |row| row.get(0))
        .unwrap();
    assert_eq!(version, 2);
}

/// Opening a project whose schema version is newer than this binary
/// understands must fail cleanly, rather than silently misinterpreting a
/// schema shape it doesn't actually know about.
#[test]
fn opening_project_with_newer_schema_version_fails_cleanly() {
    let dir = TempDir::new().unwrap();
    init(&dir);

    let db_path = dir.path().join(".kanban").join("board.db");
    let conn = rusqlite::Connection::open(&db_path).unwrap();
    conn.execute_batch("PRAGMA user_version = 999;").unwrap();
    drop(conn);

    kanban(&dir)
        .args(["agent", "list"])
        .assert()
        .failure()
        .stderr(contains("newer than this build"));
}

/// `status` reports per-status task counts (all five columns, even at zero)
/// and per-agent claimed-task counts (all registered agents, even at zero),
/// exercised end to end through the real binary and a real board.
#[test]
fn status_reports_task_counts_and_agent_workload() {
    let dir = TempDir::new().unwrap();
    init(&dir);
    register(&dir, "alice");
    register(&dir, "bob");
    register(&dir, "carol");
    register_role(&dir, "reviewer", "reviewer");

    let test_json = r#"{"describe":"d","input":"i","output":"o"}"#;
    for title in ["t1", "t2", "t3"] {
        run_json(
            &dir,
            &[
                "add",
                "--title",
                title,
                "--priority",
                "low",
                "--test",
                test_json,
            ],
        );
    }
    kanban(&dir)
        .args(["claim", "1", "--agent", "alice"])
        .assert()
        .success();
    kanban(&dir)
        .args(["claim", "2", "--agent", "bob"])
        .assert()
        .success();
    run_json(
        &dir,
        &[
            "submit-review",
            "2",
            "--agent",
            "bob",
            "--result",
            r#"{"criterion":0,"status":"passed","evidence":"verified"}"#,
        ],
    );
    run_json(&dir, &["claim-review", "2", "--agent", "reviewer"]);
    run_json(&dir, &["approve", "2", "--agent", "reviewer"]);

    let status = run_json(&dir, &["status"]);
    assert_eq!(status["backlog"], 0);
    assert_eq!(status["todo"], 1);
    assert_eq!(status["in_progress"], 1);
    assert_eq!(status["review"], 0);
    assert_eq!(status["done"], 1);
    assert_eq!(status["total"], 3);
    assert_eq!(status["agents"]["alice"], 1);
    assert_eq!(status["agents"]["bob"], 0);
    assert_eq!(status["agents"]["carol"], 0);
    assert_eq!(status["agents"]["reviewer"], 0);
}
