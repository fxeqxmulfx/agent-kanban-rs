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

It is also built to be cheap to talk to. An agent pays for every token it reads and
writes, so replies are a few plain words (`#7 review rev1`), a task is a header line plus
one `|`-separated row per test (no JSON keys, no quotes), and the whole manual is one
command (`agent-kanban guide`, about 450 tokens, or about 200 for just one job's part).
See [Token cost](#token-cost) for measured numbers.

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
initialized

$ agent-kanban agent register alice          # developer is the default role
alice developer

$ agent-kanban agent register bob --role reviewer
bob reviewer

$ agent-kanban add --title "Validate /login input" --priority high --tag backend \
    --test "rejects empty password" '{"password":""}' "400 error"
#1 todo

# #2 may only be started once #1 is done
$ agent-kanban add --title "Rate-limit /login" \
    --test "6th attempt within a minute" "6 x POST /login" "429" --after 1
#2 todo after:1

$ agent-kanban list
#1 high todo Validate /login input
#2 medium todo after:1 Rate-limit /login

# From here on every agent says who it is once, through the environment.
$ export AGENT_KANBAN_AGENT=alice

# Alice takes the best task she may start: a work order, one row per test.
$ agent-kanban claim-next
#1 Validate /login input
0|rejects empty password|{"password":""}|400 error

# She implements it, then reports one verdict per test, with evidence.
$ agent-kanban submit-review 1 --pass 0 "cargo test login_empty_password"
#1 review rev1

# Bob (a reviewer) is handed the same task, now with Alice's evidence.
$ AGENT_KANBAN_AGENT=bob agent-kanban claim-next
#1 rev1 Validate /login input
0|rejects empty password|{"password":""}|400 error|passed|cargo test login_empty_password

$ AGENT_KANBAN_AGENT=bob agent-kanban approve 1 --notes "behavior and test evidence verified"
#1 done unblocked:2

# Finishing #1 made #2 startable.
$ agent-kanban claim-next
#2 Rate-limit /login
0|6th attempt within a minute|6 x POST /login|429

$ agent-kanban status
backlog 0, todo 0, in_progress 1, review 0, done 1
agents: alice 2; bob -
```

A reviewer who finds a problem runs `request-changes ID --notes "..."` instead of
`approve`. The task goes back to `in_progress`; the next `claim`/`claim-next` of it shows
the reviewer's notes on a `changes|...` row, and the next `submit-review` creates a new
revision (`rev2`).

## The guide (what an agent reads)

`agent-kanban guide` prints the complete manual for an agent. It is much smaller than the
`--help` screens put together, and a unit test fails the build if a subcommand or flag is
ever missing from it:

```text
agent-kanban: task board for LLM agents. Replies are short text; errors go to stderr, exit 1.
Say who you are with --agent NAME or env AGENT_KANBAN_AGENT. Ids are numbers (replies show #7).
WORK
  claim-next -> #7 [rev2] TITLE, changes|NOTES (rework only), then a line per test: IDX|DESCRIBE|INPUT|OUTPUT (a reviewer's lines end |RESULT|EVIDENCE); or `idle` (`idle, N open`: tasks exist but are held, blocked or in review)
  claim ID: the same for one task. Claiming again renews the lease (--lease SECS, default 3600).
DEVELOPER
  submit-review ID --pass IDX EVIDENCE | --fail IDX EVIDENCE   one per test -> #7 review rev1
  changes = the reviewer's notes from a rejection: fix, submit again. release ID gives a task back.
REVIEWER
  approve ID [--notes T] -> #7 done [unblocked:IDS]     request-changes ID --notes T
PLANNING
  add --title T [--priority low|medium|high|urgent] [--tag T]... --test DESC INPUT OUTPUT (1+) [--after IDS]
  edit ID [same flags, --tag/--test replace all] [--drop-after IDS]   only unclaimed tasks outside review/done
  move ID backlog|todo    remove ID    agent register NAME [--role developer|reviewer]
  --after IDS: tasks that must be done first (a DAG, cycles refused); developers cannot claim before that.
BOARD
  list [--status S] [--tag T] [--executor NAME] [--priority P] [--all] [--limit N] -> `#7 high in_progress@alice after:3 Title`; done hidden without --all
  show ID [--history]   status   agent list|remove NAME   init   guide [developer|reviewer|planning|board]   --db PATH
In `|` lines \| \\ \n mean | \ and newline.
```

An agent that does one job does not need the whole manual.
`agent-kanban guide developer` (or `reviewer`, `planning`, `board`) prints just that part:
the two opening lines and the closing escape rule, plus the sections of the job (`WORK` is
shared by developers and reviewers). A part has no text of its own, only lines of the full
manual in the same order, so it cannot drift from it. Measured (`o200k_base`):

| `guide` | whole | `developer` | `reviewer` | `planning` | `board` |
|---|---|---|---|---|---|
| tokens | 456 | 223 (−51%) | 199 (−56%) | 184 (−60%) | 156 (−66%) |

Whoever starts an agent can put `agent-kanban guide developer` in its prompt and save about
230 tokens in every session of that agent.

## Command reference

| Command | Reply | Behavior / restrictions |
|---|---|---|
| `init` | `initialized` | Creates `.kanban/board.db` in the current directory (or the file given by `--db`, making parent directories). Safe to repeat and to run concurrently. |
| `guide [developer\|reviewer\|planning\|board]` | the text above, or one part of it | The built-in manual. Needs no board. |
| `agent register NAME [--role developer\|reviewer]` | `NAME ROLE` | Registers a named agent. The default role is `developer`. Names must not contain whitespace. |
| `agent list` | `NAME ROLE` per line, or `no agents` | |
| `agent remove NAME` | `NAME removed` or `NAME removed, released #1,#2` | Releases the tasks the agent holds (their statuses are preserved) and deletes the agent in one transaction. Review and result history keep the agent's name. |
| `add --title T [--priority P] [--tag X]... --test DESC INPUT OUTPUT [--test ...]... [--after IDS]` | `#7 todo` or `#7 todo after:3,5` | Creates a task in `todo`. Priority defaults to `medium`. At least one `--test` is required; each takes exactly three values. `--after` lists prerequisites (comma-separated or repeated). |
| `list [--status S] [--tag T] [--executor NAME] [--priority P] [--all] [--limit N]` | one `#ID PRIORITY STATUS[@agent][ after:IDS] TITLE` line per task | Ordered `in_progress`, `review`, `todo`, `backlog`, `done`, then by priority, then id. Done tasks are hidden unless `--all` or `--status done`. `--executor` only matches a live (unexpired) claim. Prints at most 20 rows unless `--limit N` says otherwise (`--limit 0` prints all), so one careless call cannot flood an agent's context. Footers: `+N more (--limit 0 = all)`, `+N done hidden (--all)`; an empty board prints `no tasks`. |
| `show ID [--history]` | header line and `\|` rows ([format](#task-replies)) | Everything about one task; what is empty is left out. `--history` adds every past revision with its results and the review decision. |
| `claim ID [--agent A] [--lease SECS]` | work order (header line and `\|` rows) | A **developer** takes a `todo`, or an unclaimed/expired `in_progress`, task whose prerequisites are all done; it becomes `in_progress`. A **reviewer** takes a task in `review`; it stays in `review`. Claiming again as the holder renews the lease (default 3600 s). |
| `claim-next [--agent A] [--lease SECS]` | work order, `idle` or `idle, N open` | Atomically claims the best task for the caller's role: the task it already holds first, then `in_progress` before `todo`, then priority (urgent first), then lowest id; never a task with unfinished prerequisites. Calling it again returns the task you already hold. `idle, N open` means tasks exist that are held by others, blocked, or (for developers) waiting in review. |
| `submit-review ID [--agent A] (--pass IDX EVIDENCE \| --fail IDX EVIDENCE)...` | `#7 review rev1` | The developer holding the task gives exactly one verdict per test (index from 0) with non-empty evidence. Results, the new revision and the status change commit together. |
| `approve ID [--agent A] [--notes T]` | `#7 done` or `#7 done unblocked:9,12` | The reviewer holding the task approves; refused while any test of the current revision failed. `unblocked` lists dependents that can start now. |
| `request-changes ID [--agent A] --notes T` | `#7 in_progress` | The reviewer sends the task back with required changes; ownership is cleared. |
| `release ID [--agent A]` | `#7 STATUS` | The holder gives the task back; its status does not change. |
| `move ID backlog\|todo` | `#7 STATUS` | Parks an unclaimed task in `backlog` or returns it to `todo`. Lifecycle statuses cannot be reached with `move`. |
| `edit ID [--title T] [--priority P] [--tag X]... [--test D I O]... [--after IDS] [--drop-after IDS]` | `#7 STATUS` (plus `after:IDS`) | Updates an unclaimed task outside `review`/`done`. `--tag` and `--test` replace everything; `--after` adds prerequisites, `--drop-after` removes them. A task can never be edited down to zero tests. |
| `remove ID` | `#7 removed` | Deletes an unclaimed task outside `review`/`done` that no other task waits for. Its id is never handed out again, so a stale `#7` finds "not found", never a different task. |
| `status` | `backlog 0, todo 3, in_progress 1, review 0, done 5` | Adds `; blocked N` when `todo` tasks wait for prerequisites, and a second line `agents: alice 7,9; bob -` with the tasks each agent holds. |

Global flag: `--db <path>` uses that exact database file instead of discovering `.kanban/`
by walking up from the current directory — useful for scripting against a specific project
without `cd`-ing into it, or for keeping the database somewhere other than
`.kanban/board.db`. It can go anywhere on the command line.

Who is acting: the commands that claim or finish work take `--agent NAME` or read the
`AGENT_KANBAN_AGENT` environment variable (the flag wins). Task ids are plain numbers;
the `#7` spelling that replies use is accepted too.

### Replies, errors and exit codes

Success: the reply on stdout, exit 0. Almost every reply is one short line; only task
details (`show`, `claim`, `claim-next`) are longer, see [Task replies](#task-replies).

A command that is understood but refused prints `error: <reason>` on stderr and exits 1,
nothing on stdout. The reason says what to do next:

```text
$ agent-kanban claim 2 --agent alice
error: task 2 is blocked by unfinished tasks 1
$ agent-kanban claim 1 --agent carol
error: task 1 is already claimed by 'alice' until 2026-10-01 12:00:00 UTC
$ agent-kanban submit-review 1 --agent alice --pass 0 ok
error: task 1 has 2 tests; no result for 1
$ agent-kanban claim-next --agent bob
error: agent 'bob' is not registered; run `agent-kanban agent register bob --role developer|reviewer` first
```

A malformed command line (unknown flag, missing argument, bad id) prints clap's usage
text on stderr and exits 2.

### Task replies

`show`, `claim` and `claim-next` describe a task as a header line shaped like a `list`
line, then one row per fact with the cells separated by `|`:

```text
$ agent-kanban show 2 --history
#2 high in_progress@alice rev1 blocks:3 Add rate limiting
tags|api|needs review
changes|handle the empty-key case
0|rejects a burst|11 requests in 1 s|429 on the 11th
1|allows normal use|5 requests in 1 s|200 on all
rev1|alice|passed: cargo test burst|failed: panics on an empty key
review|bob|changes_requested|handle the empty-key case
```

| Row | Present | Cells |
|---|---|---|
| header | always | `#ID`; for `show` also `PRIORITY STATUS[@HOLDER]`; then `revN` once work was submitted, `after:IDS` (unfinished prerequisites), `blocks:IDS` (unfinished dependents); then the title, which runs to the end of the line. A claim has just `#ID [revN] TITLE`. |
| `tags\|A\|B` | the task has tags | one cell per tag |
| `changes\|NOTES` | a reviewer sent the task back and it has not been approved since | |
| `IDX\|DESCRIBE\|INPUT\|OUTPUT` | one per test | `IDX` counts from 0 and is the number `--pass` and `--fail` take. A reviewer's order, and `show` of a task in `review` or `done`, append `\|RESULT\|EVIDENCE`. |
| `revN\|DEV\|RESULT: EVIDENCE\|...` | `show --history` | one per revision, verdicts in test order |
| `review\|REVIEWER\|DECISION[\|NOTES]` | `show --history` | follows the revision it decided |

Free text (titles, tags, test fields, evidence, notes) is escaped so a row always splits
on `|` and a record never spans two lines: `\|`, `\\`, `\n` and `\r` stand for a pipe, a
backslash and line breaks. Quotes and everything else stay as typed, which is part of why
this is cheaper than JSON. The one thing a row cannot protect is whitespace at the very end
of a reply, where no `|` follows it and readers tend to trim.

## Task dependencies

Tasks can wait for other tasks, which makes the board a directed acyclic graph:

```sh
$ agent-kanban add --title "Ship it" --test "deployed" "run deploy" "ok" --after 3,5
#6 todo after:3,5
$ agent-kanban edit 6 --drop-after 5       # stop waiting for #5
#6 todo after:3
$ agent-kanban edit 3 --after 6            # #6 already waits for #3
error: task 3 cannot come after 6: 6 already comes after 3 (cycle)
$ agent-kanban remove 3
error: task 3 is a prerequisite of 5,6; remove those tasks or detach them (edit --drop-after 3) first
```

* A developer cannot `claim` a task until every prerequisite is `done`
  (`error: task 6 is blocked by unfinished tasks 3`), and `claim-next` skips such tasks.
* `list` shows `after:IDS` with the prerequisites that are still unfinished, `show` puts
  `after:IDS` and `blocks:IDS` in its header, and `status` counts blocked tasks.
* `approve` reports the tasks the approval made startable: `#3 done unblocked:6`.
* Cycles (including a task waiting for itself) are refused. The check and the write happen
  in one `IMMEDIATE` transaction, so two agents adding opposite edges at the same moment
  cannot both succeed.
* A task other tasks still wait for cannot be removed
  (the error names the dependents); removing a task drops its own edges.

## Concurrency model

`agent-kanban` stores its state in a single SQLite database (`.kanban/board.db`) opened in
**WAL mode** with `foreign_keys=ON` and a `busy_timeout` of 5000ms. WAL mode lets
readers and writers proceed concurrently without blocking each other, and the busy
timeout means a writer that arrives while another transaction holds the write lock
waits and retries instead of failing immediately — so many `agent-kanban` processes
(potentially one per agent) can hit the same file at once without hand-rolled
locking.

Status and ownership are independent columns. Every command that changes state
(`claim`, `claim-next`, `submit-review`, `approve`, `request-changes`, `release`,
`move`, `edit`, `remove`, `agent remove`) runs in **one `IMMEDIATE` transaction** in which
the preconditions (role, status, current owner, lease expiry, prerequisites) are checked
and the change is written. Nothing can change between the check and the write, so exactly
one of several contenders wins, and the loser's error describes the state it really lost
to. `claim-next` picks and claims in a single `UPDATE ... RETURNING`.
Expired claims are inactive and can be replaced atomically. Results or review history and
the task transition commit together or roll back together.

This has been verified under real multi-process contention (hundreds of concurrent
attempts across test runs, spawning actual separate OS processes against the same
database file, not just threads), including races between developers, between reviewers,
between `claim-next` loops, and between opposite dependency edits.

The schema version is stamped via `PRAGMA user_version`; opening a project created by a
newer, incompatible `agent-kanban` fails with a clear error instead of silently
misinterpreting a schema it doesn't understand.

## Acceptance tests and results

The `tests` attached to a task are **acceptance-criteria specifications**, not code
`agent-kanban` runs. Each one has three text fields, given on the command line as
`--test DESC INPUT OUTPUT`:

- `describe` — what behavior is being checked
- `input` — the input/scenario
- `output` — the expected result

`agent-kanban` stores them and never executes them. The developer must supply one verdict
for every test when submitting a revision, `--pass IDX EVIDENCE` or
`--fail IDX EVIDENCE`, where `IDX` is the test's position (0 for the first) and the
evidence is a non-empty note on how it was verified (for example a test command and its
outcome). Approval is blocked while the current revision contains a failed result.

Every task must have at least one test, and this is non-negotiable: it's enforced by
a database `CHECK` constraint (a non-empty JSON array) and re-validated in
application code on every `add` and `edit`. A task can never be edited down to zero
tests. Review decisions likewise keep the revision, decision, notes and reviewer-name
snapshot even if that agent is later removed.

## Token cost

Measured with a 35-command scenario (three agents, eight tasks with three tests each, one
rejected-and-reworked review, a lost claim race and a few refused commands), counting the
tokens of every command line the agent writes and every reply it reads. The tokenizer is
`tiktoken` `o200k_base`, a stand-in for the real one, so read the ratios rather than the
absolute counts:

| | commands written | replies read | total |
|---|---|---|---|
| 0.2.x (JSON replies) | 1,502 | 13,022 | 14,524 |
| 0.3 | 1,025 | 1,430 | 2,455 (−83%) |

Where the saving comes from: replies no longer echo the whole task (specs, timestamps, ids
of agents, nulls, review history) after every change, and just acknowledge it. The replies
that do carry a task (`show`, `claim`, `claim-next`) were JSON in the first 0.3 draft and
cost 1,194 of those tokens in this scenario; as `|` rows they cost 940 (−21%). JSON pays
for every field twice, once for the key and once for the quotes and escapes around the
value. Re-rendering the same fields as labelled text (`status: review`) had made things
about 10% *worse*; what helps is dropping the labels altogether and fixing the column
order, which `guide` states once. `claim-next` replaces a "list, pick, claim" round trip
with one call, and `guide` (about 450 tokens; one job's part about 150–220) replaces
reading the `--help` screens (about 2,300 tokens in total).

The scenario's board is small. Output that grows with the board is the one cost nothing
above bounds: `list` on 300 tasks prints 300 rows, about 5,400 tokens, more than the whole
scenario. So `list` stops at 20 rows unless told otherwise: 20 rows and the footer
`+280 more (--limit 0 = all)` cost about 370 tokens (−93%), and the board still answers
every question through filters (`--status`, `--tag`, `--priority`, `--executor`) or
`--limit 0`.

### Fewer turns

What an agent pays for is often not the words of a command but the *turn* it takes: every
call makes the model read the conversation so far again. Fewer calls beat shorter calls, and
the shell already gives you what is needed, so `agent-kanban` has no flags for it:

* **Chain with `&&`.** Hand work in and take the next task in one call. If the first command
  is refused the chain stops and nothing is claimed:

  ```sh
  agent-kanban submit-review 7 --pass 0 "cargo test: ok" && agent-kanban claim-next
  ```

* **Wait with a bounded loop** when `claim-next` says `idle`, instead of one call per poll.
  This one asks every 20 seconds, five times; it prints the work order as soon as there is
  one, or the last `idle`, and it ends at once on an error such as an unregistered agent,
  whose `error: ...` goes to stderr as usual:

  ```sh
  for i in $(seq 5); do o=$(agent-kanban claim-next); case "$o" in idle*) sleep 20;; *) break;; esac; done; echo "$o"
  ```

  Keep the total (about 100 s here) below the command time limit of whatever runs the agent
  (two minutes by default in Claude Code), and call it again if it still says `idle`.

* **Say who you are once.** Whoever starts an agent can `export AGENT_KANBAN_AGENT=alice`;
  from then on no command needs `--agent NAME`.

## Upgrading from 0.2

Boards are migrated automatically (v2 → v3 adds the dependencies table; v3 → v4 rebuilds
`tasks` so that ids are never reused); all tasks, agents and history are kept. A board
touched by 0.3 cannot be opened by older builds. The command line was simplified and
**the old forms were removed, not deprecated**:

| Removed | Use instead |
|---|---|
| `--pretty`, `--table`, JSON replies | plain replies (see above); `show`, `claim` and `claim-next` print a header and `\|` rows |
| JSON errors (`{"error": ...}`) | `error: ...` on stderr, exit 1 (exit 2 for usage mistakes) |
| `--test '<json>'` | `--test DESC INPUT OUTPUT` |
| `--result '<json>'` | `--pass IDX EVIDENCE` / `--fail IDX EVIDENCE` |
| `--lease-seconds N` | `--lease SECS` |
| `claim-review ID` | `claim ID` as a reviewer, or `claim-next` |
| `move ID --status S` | `move ID S` |
| `list --sort ...` | `list` is always in working order |
| `list` printing every task | `list` prints 20 rows; `--limit 0` prints all |
| `transitions` | the lifecycle is described by `guide` |
| timestamps, agent ids, duplicated specs and review history in replies | `show ID [--history]` |

A board from 0.1.x (schema v1) is migrated the same way, in one transaction straight to the
current schema: existing agents become developers; active legacy claims on `in_progress`
receive a one-hour lease; legacy `review` tasks return to unowned `in_progress` because v1
has no recorded acceptance evidence to review; `done` keeps its status while any stale
owner is cleared, so completed work never becomes active again.

Task ids used to be recycled: removing the newest task freed its number, the next `add`
took it, and a `#7` another agent still held then named a different task. Ids now only
grow. The upgrade starts counting after the highest id on the board; a number freed
*before* the upgrade is gone for good and may be handed out once more.

## License

MIT — see [LICENSE](LICENSE).
