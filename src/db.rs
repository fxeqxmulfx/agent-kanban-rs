use anyhow::{Result, bail};
use rusqlite::Connection;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

pub const DB_DIR: &str = ".kanban";
pub const DB_FILE: &str = "board.db";

/// Process-wide override for the database file path, set at most once from
/// `main.rs` right after parsing the global `--db <path>` flag, before any
/// command runs. When set, it replaces the usual `.kanban/`
/// directory-discovery (for existing projects) and the default
/// `.kanban/board.db` location (for `init`) with this exact file path.
static PATH_OVERRIDE: OnceLock<PathBuf> = OnceLock::new();

/// Set the `--db <path>` override for this process. Must be called at most
/// once, before any other function in this module runs.
pub fn set_path_override(path: PathBuf) {
    let _ = PATH_OVERRIDE.set(path);
}

/// Tracked via `PRAGMA user_version`, `SQLite`'s built-in per-database integer
/// slot meant for exactly this — no bespoke schema-version table needed.
/// Bump this and add a migration step in `try_init`/`open_existing` when the
/// schema ever actually changes; there's only ever been one shape so far.
pub const SCHEMA_VERSION: i32 = 1;

const SCHEMA: &str = r"
CREATE TABLE IF NOT EXISTS agents (
  id INTEGER PRIMARY KEY,
  name TEXT NOT NULL UNIQUE,
  created_at TEXT NOT NULL DEFAULT (datetime('now'))
);

CREATE TABLE IF NOT EXISTS tasks (
  id INTEGER PRIMARY KEY,
  title TEXT NOT NULL,
  priority TEXT NOT NULL CHECK (priority IN ('low','medium','high','urgent')),
  status TEXT NOT NULL DEFAULT 'todo' CHECK (status IN ('backlog','todo','in_progress','review','done')),
  executor INTEGER REFERENCES agents(id),
  tags TEXT NOT NULL DEFAULT '[]' CHECK (json_valid(tags)),
  tests TEXT NOT NULL CHECK (json_valid(tests) AND json_array_length(tests) > 0),
  created_at TEXT NOT NULL DEFAULT (datetime('now')),
  updated_at TEXT NOT NULL DEFAULT (datetime('now'))
);
";

/// Walk up from cwd looking for `.kanban/board.db`, like git looks for `.git`.
/// Closest match wins; does not merge with a parent `.kanban/`.
pub fn discover() -> Option<PathBuf> {
    let mut dir = std::env::current_dir().ok()?;
    loop {
        let candidate = dir.join(DB_DIR).join(DB_FILE);
        if candidate.exists() {
            return Some(candidate);
        }
        if !dir.pop() {
            return None;
        }
    }
}

fn open_at(path: &Path) -> Result<Connection> {
    let conn = Connection::open(path)?;
    // busy_timeout must be set first: setting journal_mode=WAL on a
    // brand-new file needs a brief exclusive lock, and any lock contention
    // during that pragma only benefits from busy_timeout's retry behavior
    // if busy_timeout was already active when it runs.
    conn.execute_batch(
        "PRAGMA busy_timeout=5000; PRAGMA journal_mode=WAL; PRAGMA foreign_keys=ON;",
    )?;
    Ok(conn)
}

/// Open the board for the current project. Fails with a clear error if `agent-kanban init`
/// hasn't been run (no `.kanban/` found walking up from cwd), unless a `--db
/// <path>` override is set, in which case that exact path is used directly.
pub fn open_existing() -> Result<Connection> {
    let path = match PATH_OVERRIDE.get() {
        Some(path) => {
            // `Connection::open` silently creates a missing file (SQLite's
            // default behavior), which would otherwise turn a typo'd --db
            // path into a confusing raw "no such table" error instead of a
            // clean one -- checked explicitly, matching the message shape
            // `discover()` already uses for the no-override case.
            if !path.exists() {
                bail!(
                    "database file {} not found; run `agent-kanban --db {} init` first",
                    path.display(),
                    path.display()
                );
            }
            path.clone()
        }
        None => discover().ok_or_else(|| {
            anyhow::anyhow!("not a kanban project (no .kanban/ found); run `agent-kanban init`")
        })?,
    };
    let conn = open_at(&path)?;
    check_schema_version(&conn)?;
    Ok(conn)
}

/// A database with `user_version` unset (0) predates this check or was
/// created by a version of `agent-kanban` from before schema versioning
/// existed — harmless, since the schema shape has never actually changed, so
/// there's nothing to migrate. Only bail if the version is *higher* than
/// this binary understands: that means the project was set up by a newer,
/// incompatible `agent-kanban`, and silently proceeding could misinterpret a
/// schema this binary doesn't know about.
fn check_schema_version(conn: &Connection) -> Result<()> {
    let version: i32 = conn.query_row("PRAGMA user_version", [], |row| row.get(0))?;
    if version > SCHEMA_VERSION {
        bail!(
            "this project's schema version ({version}) is newer than this build of \
             agent-kanban supports ({SCHEMA_VERSION}); upgrade agent-kanban"
        );
    }
    Ok(())
}

/// Create `.kanban/board.db` in the current directory and apply the schema,
/// or, if a `--db <path>` override is set, create the database at that exact
/// path instead (creating parent directories as needed).
pub fn init() -> Result<()> {
    const MAX_ATTEMPTS: u32 = 10;

    let path = if let Some(path) = PATH_OVERRIDE.get() {
        if let Some(parent) = path.parent().filter(|p| !p.as_os_str().is_empty()) {
            std::fs::create_dir_all(parent)?;
        }
        path.clone()
    } else {
        let dir = std::env::current_dir()?.join(DB_DIR);
        std::fs::create_dir_all(&dir)?;
        dir.join(DB_FILE)
    };

    // Converting a brand-new database file to WAL mode for the first time
    // needs a brief exclusive lock. If two `agent-kanban init` calls race in a
    // fresh directory, one can observe "database is locked" here even with
    // busy_timeout set and ordered first -- confirmed directly: racing two
    // concurrent `init` calls in a fresh directory produced this in ~30-40%
    // of trials regardless of pragma ordering, unlike every other guarded
    // operation in this codebase (claim/edit/remove/release), where
    // busy_timeout reliably covers ordinary lock contention. This appears
    // to be a real gap specific to the one-time WAL conversion pragma.
    // Since every statement here is idempotent (CREATE TABLE IF NOT
    // EXISTS), the whole operation is safe to retry, so do that a handful
    // of times with a short backoff rather than surface a spurious failure
    // for two agents harmlessly racing to set up the same project.
    //
    // `MAX_ATTEMPTS` retries all happen inside this `loop`, which
    // deliberately has no code after it: every arm either returns
    // (success, a non-lock error, or the final attempt's error however it
    // looks) or continues looping, so the loop itself always exits via
    // `return` and never falls through -- an earlier version tracked a
    // `last_err` and returned it after the loop "in case all attempts were
    // exhausted", but that trailing code was provably unreachable (the
    // final attempt, index MAX_ATTEMPTS - 1, always takes the unconditional
    // `return Err(e)` arm below since `attempt + 1 < MAX_ATTEMPTS` is false
    // for it), so it's been removed rather than kept as dead code.
    let mut attempt = 0;
    loop {
        match try_init(&path) {
            Ok(()) => return Ok(()),
            Err(e)
                if attempt + 1 < MAX_ATTEMPTS && e.to_string().contains("database is locked") =>
            {
                attempt += 1;
                std::thread::sleep(std::time::Duration::from_millis(u64::from(20 * attempt)));
            }
            Err(e) => return Err(e),
        }
    }
}

fn try_init(path: &Path) -> Result<()> {
    let conn = open_at(path)?;
    conn.execute_batch(SCHEMA)?;
    // PRAGMA statements don't support `?` bind parameters in SQLite; this is
    // safe to format directly since SCHEMA_VERSION is a fixed constant, not
    // user input.
    conn.execute_batch(&format!("PRAGMA user_version = {SCHEMA_VERSION};"))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn schema_version_at_current_is_accepted() {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(&format!("PRAGMA user_version = {SCHEMA_VERSION};"))
            .unwrap();
        check_schema_version(&conn).unwrap();
    }

    #[test]
    fn schema_version_zero_legacy_is_accepted() {
        // A fresh in-memory connection defaults to user_version = 0, matching
        // a database created before schema versioning existed.
        let conn = Connection::open_in_memory().unwrap();
        check_schema_version(&conn).unwrap();
    }

    #[test]
    fn schema_version_newer_than_supported_is_rejected() {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(&format!("PRAGMA user_version = {};", SCHEMA_VERSION + 1))
            .unwrap();
        let err = check_schema_version(&conn).unwrap_err();
        assert!(err.to_string().contains("newer than this build"));
    }
}
