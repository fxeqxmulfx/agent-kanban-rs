# agent-kanban

A command-line kanban board built for **multiple concurrent LLM-agent processes** to
coordinate on tasks without stepping on each other. There's no server, no daemon, and
no shared in-memory state to get out of sync — just a single SQLite file per project
that any number of `agent-kanban` invocations (from any number of agents/processes, at once)
can safely read and write directly.

It exists because "just have the agents write to a shared TODO.md" doesn't survive
contact with concurrency: two agents can read the same file, both see a task as
unclaimed, and both start working on it. `agent-kanban` makes claiming a task an atomic,
race-free database operation, so exactly one agent ever wins a given task — verified
under real multi-process contention.

Project discovery works like `git`: `agent-kanban init` creates a `.kanban/` directory in
the current folder, and every other command walks up from the current working
directory to find it, so you can run `agent-kanban` from any subdirectory of a project.
Pass `--db <path>` to point at an exact database file instead, bypassing discovery
entirely.

## Install / Build

Requires Rust 1.95 or newer (stable, edition 2024).

Install from [crates.io](https://crates.io/crates/agent-kanban) straight to `~/.cargo/bin`
(already on `PATH` for most Rust setups):

```sh
cargo install agent-kanban
```

Or build from this checked-out source instead — useful for unreleased changes not on
crates.io yet:

```sh
cargo install --path .
```

`cargo install --path .` always rebuilds and overwrites the existing install, even with no
changes. `cargo install agent-kanban` upgrades automatically when a newer version is
published, but reinstalling the exact same version again needs `--force`.

Or just build it without installing anywhere:

```sh
cargo build --release
```

The binary is produced at `target/release/agent-kanban`. Put it on your `PATH`, or invoke it
directly:

```sh
./target/release/agent-kanban --help
```

## Quick start

```sh
$ agent-kanban init
{"status":"initialized"}

$ agent-kanban agent register alice --role developer
{"created_at":"2026-07-03 12:00:00","id":1,"name":"alice","role":"developer"}

$ agent-kanban agent register bob --role reviewer
{"created_at":"2026-07-03 12:00:00","id":2,"name":"bob","role":"reviewer"}

$ agent-kanban add --title "Validate /login input" --priority high \
    --tag backend \
    --test '{"describe":"rejects empty password","input":"{\"password\":\"\"}","output":"400 error"}'
{..., "id":1, "revision":0, "status":"todo", "executor":null, ...}

$ agent-kanban claim 1 --agent alice --lease-seconds 3600
{..., "executor": "alice", "status": "in_progress", ...}

# Alice implements and verifies the acceptance criterion.
$ agent-kanban submit-review 1 --agent alice \
    --result '{"criterion":0,"status":"passed","evidence":"cargo test login_empty_password"}'
{..., "executor":null, "revision":1, "status":"review",
 "acceptance_results":[{"criterion":0,"status":"passed","evidence":"cargo test login_empty_password",...}], ...}

$ agent-kanban claim-review 1 --agent bob --lease-seconds 1800
{..., "executor":"bob", "executor_role":"reviewer", "status":"review", ...}

$ agent-kanban approve 1 --agent bob --notes "behavior and test evidence verified"
{..., "executor":null, "status":"done",
 "review_history":[{"decision":"approved","executor":"bob","revision":1,...}], ...}

$ agent-kanban status
{"agents":{"alice":0,"bob":0},"backlog":0,"done":1,"in_progress":0,"review":0,"todo":0,"total":1}
```

`submit-review` changes the status and releases the developer in one transaction.
`claim-review` assigns a reviewer without changing `review`. `approve` records the
decision and clears the reviewer while moving the task to `done`. A reviewer can use
`request-changes --notes "..."` instead; that records the decision, returns the task
to unowned `in_progress`, and the next developer submission creates a new revision.

## Command reference

| Command | Flags | Behavior / restrictions |
|---|---|---|
| `agent-kanban init` | | Creates `.kanban/` (and `board.db`) in the current directory. |
| `agent-kanban agent register <name>` | `--role developer\|reviewer` | Registers a named role. The default role is `developer` for v1 compatibility. |
| `agent-kanban agent list` | | Lists registered agents and their roles. |
| `agent-kanban agent remove <name>` | | Clears that agent's active ownership and deletes it in one transaction. Task statuses are preserved, including `done`; review and acceptance history keep the executor name snapshot. |
| `agent-kanban add` | `--title T`, `--priority P`, `--tag t` (repeatable), `--test '<json>'` (repeatable, required, ≥1) | Creates a task. New tasks start at status `todo`. Each `--test` must be a JSON object with exactly `describe`, `input`, `output` string fields; at least one is mandatory (enforced by a DB `CHECK` constraint and by application-level validation). |
| `agent-kanban list` | `--status S`, `--tag T`, `--executor A`, `--priority P`, `--sort priority\|created_at` | Lists tasks with combinable filters. `--executor` only matches a non-expired active lease. |
| `agent-kanban show <id>` | | Prints the task, current revision, active owner/lease, acceptance results, and review history. |
| `agent-kanban claim <id>` | `--agent <developer>`, `--lease-seconds N` | Atomically claims `todo` or unowned/expired `in_progress` work for a developer and sets/keeps `in_progress`. Repeating it as the same owner renews the lease. |
| `agent-kanban submit-review <id>` | `--agent <developer>`, `--result '<json>'` (once per criterion) | Requires the active developer lease. Stores a complete `passed`/`failed` result set with non-empty evidence, increments `revision`, moves to `review`, and releases the developer atomically. |
| `agent-kanban claim-review <id>` | `--agent <reviewer>`, `--lease-seconds N` | Atomically claims an unowned or expired task in `review`; status remains `review`. |
| `agent-kanban approve <id>` | `--agent <reviewer>`, `--notes T` | Requires the active reviewer lease and no failed result in the current revision. Records the decision, moves to `done`, and clears ownership atomically. |
| `agent-kanban request-changes <id>` | `--agent <reviewer>`, `--notes T` | Records required changes, moves back to unowned `in_progress`, and clears reviewer ownership atomically. |
| `agent-kanban release <id>` | `--agent <current-owner>` | Clears only that agent's active ownership without changing task status. |
| `agent-kanban move <id>` | `--status backlog\|todo` | Administrative move for unowned tasks. Lifecycle statuses cannot be bypassed with generic `move`. |
| `agent-kanban edit <id>` | `--title T`, `--priority P`, `--tag t` (repeatable), `--test '<json>'` (repeatable) | Updates an unowned task outside `review`/`done`. A task can never be edited down to zero tests. |
| `agent-kanban remove <id>` | | Deletes an unowned task outside `review`/`done`. |
| `agent-kanban status` | | Counts each status and active, non-expired claims per agent. A `done` task is never active. |
| `agent-kanban transitions` | | Returns the exact lifecycle transition table enforced by mutation commands. |

Global flags:

- `--pretty` and `--table` are mutually exclusive output modes. `--pretty` prints
  indented JSON for humans; without it, output is compact JSON on a single line,
  intended for other programs/agents to parse. `--table` renders a human-readable
  table instead of JSON — an aligned table for list-shaped results (`list`, `agent
  list`), a two-column FIELD/VALUE table for a single result (`show`, `add`, ...).
  Nested values (`tags`, `tests`) render as inline compact JSON within their cell
  rather than a nested table.
- `--db <path>` uses that exact database file instead of discovering `.kanban/` by
  walking up from the current directory — useful for scripting against a specific
  project without `cd`-ing into it, or keeping the database somewhere other than
  `.kanban/board.db`. For `init`, creates the file there (making parent directories
  as needed) instead of the default location.

Every command prints JSON to stdout on success (or a table with `--table`). On
failure, it prints `{"error": "message"}` to stderr and exits non-zero — the
primary consumer of output is other programs, not a human reading prose. Both
`--pretty` and `--table` apply to error output too, for consistency.

## Concurrency model

`agent-kanban` stores its state in a single SQLite database (`.kanban/board.db`) opened in
**WAL mode** with `foreign_keys=ON` and a `busy_timeout` of 5000ms. WAL mode lets
readers and writers proceed concurrently without blocking each other, and the busy
timeout means a writer that arrives while another transaction holds the write lock
waits and retries instead of failing immediately — so many `agent-kanban` processes
(potentially one per agent) can hit the same file at once without hand-rolled
locking. The schema version is stamped via `PRAGMA user_version` on `init`; opening a
project created by a newer, incompatible `agent-kanban` fails with a clear error instead of
silently misinterpreting a schema it doesn't understand. Opening a v1 board migrates
it transactionally to v2. Existing agents become developers; active legacy claims
on `in_progress` receive a one-hour lease. Legacy `review` tasks return to unowned
`in_progress` because v1 has no recorded acceptance evidence to review; `done`
keeps its status while any stale owner is cleared, so completed work never becomes
active again.

Status and ownership are independent columns. Every claim/release has its
preconditions in the same guarded `UPDATE`, including role, allowed status, current
owner, and lease expiry. Expired claims are inactive and can be replaced atomically.
Multi-row operations (`submit-review`, `approve`, and `request-changes`) use an
immediate SQLite transaction: acceptance results or review history and the task
transition commit together or roll back together.

The lifecycle table returned by `agent-kanban transitions` is also the table used by
the command implementation. Generic `move` only exposes `backlog ↔ todo`, so it
cannot jump directly to `review` or `done`.

Each of these has been verified under real multi-process contention (hundreds of
concurrent attempts across test runs, spawning actual separate OS processes against
the same database file, not just threads), including races between developers and
between reviewers.

## Acceptance specs and results

The `tests` attached to a task are **acceptance-criteria specifications**, not code
`agent-kanban` runs. Each one is a JSON object with three string fields:

- `describe` — what behavior is being checked
- `input` — the input/scenario
- `output` — the expected result

`agent-kanban` stores and validates the shape of these specs; it never executes them.
The developer must supply one result for every criterion when submitting a revision:

```json
{"criterion":0,"status":"passed","evidence":"cargo test login_empty_password"}
```

`status` is exactly `passed` or `failed`, and `evidence` must be non-empty. Results
store the criterion snapshot, revision, developer name, and verification time.
Approval is blocked while the current revision contains a failed result.

Every task must have at least one test, and this is non-negotiable: it's enforced by
a database `CHECK` constraint (a non-empty JSON array) and re-validated in
application code on every `add` and `edit`. A task can never be edited down to zero
tests. Review decisions likewise keep the revision, decision, notes, reviewer-name
snapshot, and timestamp even if that agent is later removed.

## License

MIT — see [LICENSE](LICENSE).
