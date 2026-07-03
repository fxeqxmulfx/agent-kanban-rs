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

Requires a Rust toolchain (stable, edition 2024).

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

$ agent-kanban agent register alice
{"created_at":"2026-07-03 12:00:00","id":1,"name":"alice"}

$ agent-kanban add \
    --title "Add input validation to /login" \
    --priority high \
    --tag backend \
    --tag security \
    --test '{"describe":"rejects empty password","input":"{\"password\":\"\"}","output":"400 error"}'
{
  "created_at": "2026-07-03 12:00:00",
  "executor": null,
  "id": 1,
  "priority": "high",
  "status": "todo",
  "tags": ["backend", "security"],
  "tests": [
    {
      "describe": "rejects empty password",
      "input": "{\"password\":\"\"}",
      "output": "400 error"
    }
  ],
  "title": "Add input validation to /login",
  "updated_at": "2026-07-03 12:00:00"
}

$ agent-kanban claim 1 --agent alice
{..., "executor": "alice", "id": 1, "status": "in_progress", ...}
```

Claiming sets `status` to `in_progress` in the same atomic step — no separate "start
work" call needed.

```sh
# ... alice implements the change, runs the test above by hand, confirms it passes ...

$ agent-kanban move 1 --status done
{..., "executor": "alice", "id": 1, "status": "done", ...}
```

A second agent racing for the same task gets a clean failure instead of silently
overwriting the first — claiming is atomic, so exactly one caller ever wins, even if
both request it at the same instant:

```sh
$ agent-kanban agent register bob
{"created_at":"2026-07-03 12:00:00","id":2,"name":"bob"}

$ agent-kanban claim 1 --agent bob
{"error": "task 1 is already claimed"}
```

Other lifecycle operations, shown on a second task:

```sh
$ agent-kanban add --title "Write onboarding docs" --priority low \
    --test '{"describe":"docs exist","input":"n/a","output":"file present"}'
{..., "id": 2, "status": "todo", ...}

$ agent-kanban claim 2 --agent alice
{..., "executor": "alice", "status": "in_progress", ...}

# Un-claim a task instead of finishing it (resets status to todo, clears executor)
$ agent-kanban release 2
{..., "executor": null, "status": "todo", ...}

# Edit a task (only allowed while unclaimed and not done)
$ agent-kanban edit 2 --priority medium
{..., "priority": "medium", ...}

# Delete a task (only allowed while unclaimed and not done)
$ agent-kanban remove 2
{"removed": 2}

# Board overview: counts per status column, plus each agent's current workload
$ agent-kanban status
{"agents":{"alice":1,"bob":0},"backlog":0,"done":1,"in_progress":0,"review":0,"todo":0,"total":1}
```

## Command reference

| Command | Flags | Behavior / restrictions |
|---|---|---|
| `agent-kanban init` | | Creates `.kanban/` (and `board.db`) in the current directory. |
| `agent-kanban agent register <name>` | | Registers an agent name. Must be done before that name can claim work. |
| `agent-kanban agent list` | | Lists all registered agents. |
| `agent-kanban agent remove <name>` | | Deletes the agent. Any tasks it currently holds are auto-released (executor cleared, status reset to `todo`) first, in the same transaction — an agent is never left dangling as a claim-holder that no longer exists. |
| `agent-kanban add` | `--title T`, `--priority P`, `--tag t` (repeatable), `--test '<json>'` (repeatable, required, ≥1) | Creates a task. New tasks start at status `todo`. Each `--test` must be a JSON object with exactly `describe`, `input`, `output` string fields; at least one is mandatory (enforced by a DB `CHECK` constraint and by application-level validation). |
| `agent-kanban list` | `--status S`, `--tag T`, `--executor A`, `--priority P`, `--sort priority\|created_at` | Lists tasks, filters combinable. `--executor` matches by agent *name*. `--sort priority` orders by real severity (`urgent` > `high` > `medium` > `low`), not alphabetically; `--sort created_at` orders chronologically; with no `--sort`, tasks come back in creation order. |
| `agent-kanban show <id>` | | Prints a single task. |
| `agent-kanban claim <id> --agent <name>` | `--agent <name>` | Atomically assigns the task to `<name>` via a compare-and-swap update, only if the task is currently unclaimed. Exactly one caller wins under concurrent contention; losers get a clean `{"error": ...}`. Fails if `<name>` isn't a registered agent, without touching the task. |
| `agent-kanban move <id> --status S` | `--status S` | Changes a task's status. |
| `agent-kanban release <id>` | | Un-claims a task: clears `executor` and resets `status` back to `todo`. |
| `agent-kanban edit <id>` | `--title T`, `--priority P`, `--tag t` (repeatable), `--test '<json>'` (repeatable) | Updates a task's fields. Refuses to act on a task that is currently claimed (release it first) or whose status is `done`. A task can never be edited down to zero tests. |
| `agent-kanban remove <id>` | | Deletes a task. Same restriction as `edit`: refuses on a claimed or `done` task. |
| `agent-kanban status` | | Board overview: a task count for each status column (`backlog`, `todo`, `in_progress`, `review`, `done` — all five, even at zero) plus `total`, and an `agents` object with every registered agent's current claimed-task count (including agents holding nothing). |

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
silently misinterpreting a schema it doesn't understand.

The part that actually matters for correctness is that every state-changing
operation on a task is a single, self-contained SQL statement with its precondition
baked into the same `WHERE` clause — not a read-then-write in application code, which
would leave a window for another process to change the row in between:

- `agent-kanban claim` — `UPDATE tasks SET executor = ?, ... WHERE id = ? AND executor IS NULL`.
  If two agent processes race to claim the same task, the database serializes the two
  `UPDATE`s; the one that runs first flips `executor` from `NULL` to its name and
  reports success, and the second one's `WHERE` clause no longer matches anything, so
  it affects zero rows and `agent-kanban` reports a clean "already claimed" error.
- `agent-kanban release` — the mirror image: `... WHERE id = ? AND executor IS NOT NULL`,
  so a stale `release` can never clobber a task that was released and re-claimed by
  someone else in the meantime.
- `agent-kanban edit` / `agent-kanban remove` — both require `executor IS NULL AND status != 'done'`
  in the same guarded statement, so a `claim` landing between a caller's mental model
  and the actual edit/delete can't silently slip through.

Each of these has been verified under real multi-process contention (hundreds of
concurrent attempts across test runs, spawning actual separate OS processes against
the same database file, not just threads), always with exactly one well-defined
winner and never a corrupted or split-brain outcome.

## Tests are specs, not executables

The `tests` attached to a task are **acceptance-criteria specifications**, not code
`agent-kanban` runs. Each one is a JSON object with three string fields:

- `describe` — what behavior is being checked
- `input` — the input/scenario
- `output` — the expected result

`agent-kanban` stores and validates the *shape* of these specs; it never executes them.
Confirming a task's tests actually pass is the responsibility of the agent doing the
work, before it moves the task to `done`.

Every task must have at least one test, and this is non-negotiable: it's enforced by
a database `CHECK` constraint (a non-empty JSON array) and re-validated in
application code on every `add` and `edit`. A task can never be edited down to zero
tests. This exists because TDD matters — a task isn't really "done" unless it had a
concrete, checkable definition of done attached from the moment it was created, not
invented retroactively after the code was written.

## License

MIT — see [LICENSE](LICENSE).
