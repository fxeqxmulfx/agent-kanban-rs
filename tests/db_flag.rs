//! Tests for the global `--db <path>` flag, which points every command at an
//! exact database file instead of the usual `.kanban/` directory discovery.

mod common;

use std::path::Path;

/// `args` prefixed with `--db PATH`.
fn with_db<'a>(db: &'a Path, args: &[&'a str]) -> Vec<&'a str> {
    let mut full = vec!["--db", db.to_str().unwrap()];
    full.extend_from_slice(args);
    full
}

/// Run `args` against the board at `db`, from a directory that has no board of
/// its own.
fn run_db(cwd: &Path, db: &Path, args: &[&str]) -> String {
    common::run(cwd, &with_db(db, args))
}

/// `--db` points `init` at an exact file, creating parent directories as
/// needed, instead of the default `.kanban/board.db`. Normal discovery from the
/// same directory afterwards must find nothing: the override never creates a
/// `.kanban/` directory.
#[test]
fn db_flag_creates_the_exact_path_and_its_parent_directories() {
    let dir = common::project();
    let db = dir.path().join("nested").join("my-board.db");
    assert!(!db.parent().unwrap().exists());

    assert_eq!(run_db(dir.path(), &db, &["init"]), "initialized");

    assert!(db.is_file());
    assert!(!dir.path().join(".kanban").exists());
    assert!(
        common::fail(&dir, &["list"]).starts_with("error: not a kanban project"),
        "discovery must not find a board created with --db"
    );
}

#[test]
fn the_whole_lifecycle_works_through_the_override() {
    let dir = common::project();
    let db = dir.path().join("board.db");

    run_db(dir.path(), &db, &["init"]);
    assert_eq!(
        run_db(dir.path(), &db, &["agent", "register", "alice"]),
        "alice developer"
    );
    assert_eq!(
        run_db(
            dir.path(),
            &db,
            &[
                "add",
                "--title",
                "t",
                "--priority",
                "low",
                "--test",
                "d",
                "i",
                "o"
            ]
        ),
        "#1 todo"
    );

    let order = run_db(dir.path(), &db, &["claim", "1", "--agent", "alice"]);
    assert_eq!(order, "#1 t\n0|d|i|o");
    assert_eq!(
        run_db(dir.path(), &db, &["list"]),
        "#1 low in_progress@alice t"
    );
    assert_eq!(
        run_db(
            dir.path(),
            &db,
            &[
                "submit-review",
                "1",
                "--agent",
                "alice",
                "--pass",
                "0",
                "ok"
            ]
        ),
        "#1 review rev1"
    );
}

/// A non-`init` command against a `--db` path that does not exist must fail
/// cleanly, not silently create an empty schema-less file (`SQLite`'s default)
/// and then surface a raw "no such table".
#[test]
fn a_missing_database_file_fails_cleanly_and_is_not_created() {
    let dir = common::project();
    let db = dir.path().join("does-not-exist.db");

    let err = common::fail(&dir, &with_db(&db, &["list"]));

    assert!(err.starts_with("error: database file "), "{err}");
    assert!(err.contains("not found; run `agent-kanban --db "), "{err}");
    assert!(err.ends_with("init` first"), "{err}");
    assert!(!db.exists(), "a failed lookup must not leave a file behind");
}

/// `--db` beats a board found by walking up from the working directory, and
/// the two boards never see each other's data.
#[test]
fn the_flag_wins_over_a_discovered_board_and_boards_stay_independent() {
    let project = common::initialized();
    common::register(&project, "alice", "developer");
    common::add_task(&project, "in the project board", "medium");

    let elsewhere = common::project();
    let other = elsewhere.path().join("other.db");
    run_db(elsewhere.path(), &other, &["init"]);
    run_db(
        elsewhere.path(),
        &other,
        &[
            "add",
            "--title",
            "in the other board",
            "--test",
            "d",
            "i",
            "o",
        ],
    );

    // Same working directory (which has `.kanban/`), different boards.
    assert_eq!(
        common::run(&project, &["list"]),
        "#1 medium todo in the project board"
    );
    assert_eq!(
        run_db(project.path(), &other, &["list"]),
        "#1 medium todo in the other board"
    );
    assert_eq!(
        run_db(project.path(), &other, &["agent", "list"]),
        "no agents"
    );
    assert_eq!(common::run(&project, &["agent", "list"]), "alice developer");
}

#[test]
fn relative_paths_resolve_against_the_working_directory() {
    let dir = common::project();

    common::run(&dir, &["--db", "sub/rel.db", "init"]);
    assert!(dir.path().join("sub").join("rel.db").is_file());
    common::run(
        &dir,
        &[
            "--db",
            "sub/rel.db",
            "add",
            "--title",
            "x",
            "--test",
            "d",
            "i",
            "o",
        ],
    );

    let nested = dir.path().join("sub");
    assert_eq!(
        common::run(&nested, &["--db", "rel.db", "list"]),
        "#1 medium todo x"
    );
}

#[test]
fn paths_with_spaces_and_unicode_work() {
    let dir = common::project();
    let db = dir.path().join("my boards").join("доска №1.db");

    run_db(dir.path(), &db, &["init"]);
    run_db(
        dir.path(),
        &db,
        &["add", "--title", "задача", "--test", "d", "i", "o"],
    );

    assert_eq!(run_db(dir.path(), &db, &["list"]), "#1 medium todo задача");
}

/// `--db` is declared `global`, so it works in any position, not only before
/// the subcommand.
#[test]
fn the_flag_works_before_and_after_the_subcommand() {
    let dir = common::project();
    let db = dir.path().join("board.db");
    let db = db.to_str().unwrap();

    assert_eq!(common::run(&dir, &["init", "--db", db]), "initialized");
    assert_eq!(
        common::run(&dir, &["agent", "register", "alice", "--db", db]),
        "alice developer"
    );
    assert_eq!(
        common::run(&dir, &["--db", db, "agent", "list"]),
        "alice developer"
    );
    assert_eq!(
        common::run(&dir, &["agent", "--db", db, "list"]),
        "alice developer"
    );
}

#[test]
fn db_flag_without_a_value_is_a_usage_error() {
    let dir = common::project();

    let err = common::usage_error(&dir, &["list", "--db"]);

    assert_eq!(
        err,
        "error: a value is required for '--db <PATH>' but none was supplied"
    );
}

/// If the parent directory of `--db` cannot be created, `init` must report the
/// OS error instead of succeeding or panicking. Unix-only (relies on chmod).
#[test]
#[cfg(unix)]
fn init_reports_a_parent_directory_that_cannot_be_created() {
    use std::os::unix::fs::PermissionsExt;

    let dir = common::project();
    let readonly_parent = dir.path().join("readonly");
    std::fs::create_dir(&readonly_parent).unwrap();
    std::fs::set_permissions(&readonly_parent, std::fs::Permissions::from_mode(0o555)).unwrap();

    let db = readonly_parent.join("nested").join("board.db");
    let err = common::fail(&dir, &with_db(&db, &["init"]));

    // Restore write permission so the TempDir can clean itself up.
    std::fs::set_permissions(&readonly_parent, std::fs::Permissions::from_mode(0o755)).unwrap();
    assert!(err.starts_with("error: "), "{err}");
    assert!(err.contains("Permission denied"), "{err}");
}

/// `init` retries only "database is locked"; any other failure (here a
/// read-only *existing* file) must surface on the first attempt. A lock is not
/// used on purpose: exhausting the retries would need a lock held for tens of
/// seconds, and a short one is simply absorbed by `busy_timeout`.
#[test]
#[cfg(unix)]
fn init_reports_a_read_only_database_without_retrying() {
    use std::os::unix::fs::PermissionsExt;

    let dir = common::project();
    let db = dir.path().join("existing.db");
    std::fs::File::create(&db).unwrap();
    std::fs::set_permissions(&db, std::fs::Permissions::from_mode(0o444)).unwrap();

    let started = std::time::Instant::now();
    let err = common::fail(&dir, &with_db(&db, &["init"]));

    std::fs::set_permissions(&db, std::fs::Permissions::from_mode(0o644)).unwrap();
    assert!(err.contains("readonly"), "{err}");
    assert!(
        started.elapsed() < std::time::Duration::from_secs(4),
        "a non-lock error must not go through the retry/backoff loop"
    );
}
