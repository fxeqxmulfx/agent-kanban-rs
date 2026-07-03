//! Tests for the global `--db <path>` override, which points every command
//! at an exact database file instead of the usual `.kanban/` directory
//! discovery.

use assert_cmd::Command;
use serde_json::Value;
use tempfile::TempDir;

fn kanban() -> Command {
    Command::cargo_bin("agent-kanban").unwrap()
}

fn run_json(db_path: &std::path::Path, args: &[&str]) -> Value {
    let mut full_args = vec!["--db", db_path.to_str().unwrap()];
    full_args.extend_from_slice(args);
    let output = kanban().args(&full_args).output().unwrap();
    assert!(
        output.status.success(),
        "command {:?} failed: stdout={} stderr={}",
        full_args,
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    serde_json::from_slice(&output.stdout).unwrap_or_else(|e| {
        panic!(
            "invalid JSON from {:?}: {e}\nstdout={}",
            full_args,
            String::from_utf8_lossy(&output.stdout)
        )
    })
}

/// `--db` points `init` at an exact file, creating parent directories as
/// needed, instead of the default `.kanban/board.db`. Normal (no `--db`)
/// discovery run from the same directory afterward must find nothing --
/// the override never creates a `.kanban/` directory.
#[test]
fn db_flag_creates_exact_path_and_parent_dirs() {
    let dir = TempDir::new().unwrap();
    let db_path = dir.path().join("nested").join("my-board.db");
    assert!(!db_path.parent().unwrap().exists());

    run_json(&db_path, &["init"]);
    assert!(db_path.is_file());

    // Discovery without --db, run from the same directory, must not find
    // this file -- the override doesn't create a `.kanban/` anywhere.
    let mut cmd = kanban();
    cmd.current_dir(&dir).arg("list");
    cmd.assert()
        .failure()
        .stderr(predicates::str::contains("not a kanban project"));
}

/// The full task lifecycle works normally through the override, not just
/// `init`.
#[test]
fn db_flag_supports_full_lifecycle() {
    let dir = TempDir::new().unwrap();
    let db_path = dir.path().join("board.db");

    run_json(&db_path, &["init"]);
    run_json(&db_path, &["agent", "register", "alice"]);
    let created = run_json(
        &db_path,
        &[
            "add",
            "--title",
            "t",
            "--priority",
            "low",
            "--test",
            r#"{"describe":"d","input":"i","output":"o"}"#,
        ],
    );
    let id = created["id"].as_i64().unwrap().to_string();

    let claimed = run_json(&db_path, &["claim", &id, "--agent", "alice"]);
    assert_eq!(claimed["executor"], "alice");
    assert_eq!(claimed["status"], "in_progress");

    let listed = run_json(&db_path, &["list"]);
    assert_eq!(listed.as_array().unwrap().len(), 1);
}

/// Running a non-`init` command against a `--db` path that doesn't exist
/// yet must fail with a clean error, not silently create an empty,
/// schema-less database file via `SQLite`'s default auto-create behavior
/// (confirmed this was the actual failure mode before the fix: it created
/// a 0-byte-schema file and then surfaced a raw "no such table: tasks").
#[test]
fn db_flag_on_nonexistent_file_fails_cleanly_without_creating_it() {
    let dir = TempDir::new().unwrap();
    let db_path = dir.path().join("does-not-exist.db");

    let mut cmd = kanban();
    cmd.args(["--db", db_path.to_str().unwrap(), "list"]);
    cmd.assert()
        .failure()
        .stderr(predicates::str::contains("not found"));

    assert!(
        !db_path.exists(),
        "a failed lookup must not leave behind an empty database file"
    );
}

/// If `--db`'s parent directory can't actually be created (e.g. permission
/// denied), `init` must propagate that error rather than silently
/// succeeding or panicking. Unix-only (relies on chmod). Not defended
/// against running as root (which bypasses permission checks entirely) --
/// not a scenario this suite otherwise needs to accommodate.
#[test]
#[cfg(unix)]
fn db_flag_init_propagates_parent_dir_creation_failure() {
    use std::os::unix::fs::PermissionsExt;

    let dir = TempDir::new().unwrap();
    let readonly_parent = dir.path().join("readonly");
    std::fs::create_dir(&readonly_parent).unwrap();
    std::fs::set_permissions(&readonly_parent, std::fs::Permissions::from_mode(0o555)).unwrap();

    let db_path = readonly_parent.join("nested").join("board.db");
    let mut cmd = kanban();
    cmd.args(["--db", db_path.to_str().unwrap(), "init"]);
    cmd.assert()
        .failure()
        .stderr(predicates::str::contains("Permission denied"));

    // Restore write permission so TempDir can clean itself up.
    std::fs::set_permissions(&readonly_parent, std::fs::Permissions::from_mode(0o755)).unwrap();
}

/// `init`'s retry loop only retries errors whose message contains "database
/// is locked" -- any other error (like a read-only *existing* target file,
/// as opposed to an unwritable parent directory) must propagate immediately
/// on the first attempt, not be silently swallowed by the retry logic.
/// Deliberately not a lock-contention scenario: forcing genuine exhaustion
/// of all 10 retries would require holding a lock for tens of seconds
/// (each attempt gets its own 5-second `busy_timeout` budget before the outer
/// retry loop even sees an error) -- confirmed by direct experimentation
/// that a short held lock just makes `init` succeed once `busy_timeout`'s own
/// retry absorbs the wait, rather than exhausting our loop. This test hits
/// the same non-retriable-error code path via a fast, reliable, non-lock
/// failure instead.
#[test]
#[cfg(unix)]
fn db_flag_init_propagates_non_lock_errors_without_retrying() {
    use std::os::unix::fs::PermissionsExt;

    let dir = TempDir::new().unwrap();
    let db_path = dir.path().join("existing.db");
    std::fs::File::create(&db_path).unwrap();
    std::fs::set_permissions(&db_path, std::fs::Permissions::from_mode(0o444)).unwrap();

    let mut cmd = kanban();
    cmd.args(["--db", db_path.to_str().unwrap(), "init"]);
    cmd.assert()
        .failure()
        .stderr(predicates::str::contains("readonly"));

    std::fs::set_permissions(&db_path, std::fs::Permissions::from_mode(0o644)).unwrap();
}

/// `--db` and `--table`/`--pretty` are independent flags and compose fine
/// together.
#[test]
fn db_flag_composes_with_pretty() {
    let dir = TempDir::new().unwrap();
    let db_path = dir.path().join("board.db");

    let mut init_cmd = kanban();
    init_cmd.args(["--db", db_path.to_str().unwrap(), "init"]);
    init_cmd.assert().success();

    let mut cmd = kanban();
    cmd.args([
        "--pretty",
        "--db",
        db_path.to_str().unwrap(),
        "agent",
        "list",
    ]);
    let output = cmd.output().unwrap();
    assert!(output.status.success());
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains('\n'),
        "expected pretty multi-line output, got: {stdout:?}"
    );
}

/// Global flags (`--db`, `--pretty`, `--table`) are declared with
/// `global = true` specifically so they work in any position, not just
/// before the subcommand -- README examples always show them first, but
/// nothing previously locked in that `agent-kanban <subcommand> --db <path>`
/// works identically to `agent-kanban --db <path> <subcommand>`. Worth
/// covering explicitly after finding that a different "clap should just
/// handle this" assumption (--version) was silently broken by how
/// `#[command(...)]` was written.
#[test]
fn global_flags_work_after_the_subcommand_too() {
    let dir = TempDir::new().unwrap();
    let db_path = dir.path().join("board.db");

    // --db placed after the subcommand.
    let mut init_cmd = kanban();
    init_cmd.args(["init", "--db", db_path.to_str().unwrap()]);
    init_cmd.assert().success();
    assert!(db_path.is_file());

    // --pretty placed after the subcommand and its own arguments.
    let mut cmd = kanban();
    cmd.args([
        "agent",
        "register",
        "alice",
        "--db",
        db_path.to_str().unwrap(),
        "--pretty",
    ]);
    let output = cmd.output().unwrap();
    assert!(
        output.status.success(),
        "stdout={} stderr={}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains('\n'),
        "expected pretty multi-line output, got: {stdout:?}"
    );
}
