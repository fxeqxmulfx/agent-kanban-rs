mod commands;
mod db;
mod output;

use anyhow::{Result, anyhow};
use clap::error::ErrorKind;
use clap::{Args, Parser, Subcommand};
use std::path::PathBuf;

use commands::guide::Part;
use commands::lifecycle::{self, DEFAULT_LEASE_SECONDS};
use commands::task::{DEFAULT_LIST_LIMIT, ListFilter, NewTask, TaskEdit};

#[derive(Parser)]
#[command(
    name = "agent-kanban",
    about = "Task board for concurrent LLM agents. Run `agent-kanban guide` first",
    version
)]
struct Cli {
    /// Database file; default: the nearest `.kanban/board.db` above the
    /// current directory (`init` creates it here)
    #[arg(long, global = true, value_name = "PATH")]
    db: Option<PathBuf>,

    #[command(subcommand)]
    command: Command,
}

/// Who is acting. Every command that claims or finishes work needs it.
#[derive(Args)]
struct Who {
    /// Your registered agent name
    #[arg(long, env = "AGENT_KANBAN_AGENT", value_name = "NAME")]
    agent: Option<String>,
}

impl Who {
    fn name(&self) -> Result<&str> {
        self.agent
            .as_deref()
            .map(str::trim)
            .filter(|name| !name.is_empty())
            .ok_or_else(|| anyhow!("no agent name: pass --agent NAME or set AGENT_KANBAN_AGENT"))
    }
}

#[derive(Args)]
struct Lease {
    /// Seconds before another agent may take the task over; claiming again
    /// renews it
    #[arg(long, value_name = "SECS", default_value_t = DEFAULT_LEASE_SECONDS)]
    lease: u32,
}

#[derive(Subcommand)]
enum Command {
    /// Create `.kanban/board.db` in the current directory
    Init,

    /// Print the usage guide, or one part of it (read this first)
    Guide {
        /// Only the part for this job; default: the whole guide
        part: Option<Part>,
    },

    /// Register, list and remove agents
    Agent {
        #[command(subcommand)]
        action: AgentAction,
    },

    /// Create a task
    Add {
        /// Task title
        #[arg(long, allow_hyphen_values = true)]
        title: String,
        /// low, medium, high or urgent
        #[arg(long, default_value = "medium")]
        priority: String,
        /// Tag to attach (repeatable)
        #[arg(long = "tag", value_name = "TAG", allow_hyphen_values = true)]
        tags: Vec<String>,
        /// Acceptance test: what it checks, its input, its expected output
        /// (repeatable; at least one)
        #[arg(
            long = "test",
            num_args = 3,
            value_names = ["DESC", "INPUT", "OUTPUT"],
            required = true,
            allow_hyphen_values = true
        )]
        tests: Vec<String>,
        /// Ids of tasks this one waits for (comma-separated or repeated)
        #[arg(long, value_delimiter = ',', value_parser = parse_id, value_name = "IDS")]
        after: Vec<i64>,
    },

    /// List tasks, one line each
    List {
        /// Only tasks in this status
        #[arg(long)]
        status: Option<String>,
        /// Only tasks with this tag
        #[arg(long)]
        tag: Option<String>,
        /// Only tasks this agent holds
        #[arg(long)]
        executor: Option<String>,
        /// Only tasks with this priority
        #[arg(long)]
        priority: Option<String>,
        /// Include done tasks
        #[arg(long)]
        all: bool,
        /// Print at most N tasks; 0 prints all of them
        #[arg(long, value_name = "N", default_value_t = DEFAULT_LIST_LIMIT)]
        limit: usize,
    },

    /// Show one task: a header line, then its tags, notes and tests
    Show {
        /// Task id
        #[arg(value_parser = parse_id)]
        id: i64,
        /// Also list every past revision with its results and review
        #[arg(long)]
        history: bool,
    },

    /// Claim one task and print its work order
    Claim {
        /// Task id
        #[arg(value_parser = parse_id)]
        id: i64,
        #[command(flatten)]
        who: Who,
        #[command(flatten)]
        lease: Lease,
    },

    /// Claim the best available task, or print `idle`
    ClaimNext {
        #[command(flatten)]
        who: Who,
        #[command(flatten)]
        lease: Lease,
    },

    /// Hand finished work to a reviewer
    SubmitReview {
        /// Task id
        #[arg(value_parser = parse_id)]
        id: i64,
        #[command(flatten)]
        who: Who,
        /// Test IDX passed; EVIDENCE says how you know (repeatable)
        #[arg(
            long,
            num_args = 2,
            value_names = ["IDX", "EVIDENCE"],
            allow_hyphen_values = true
        )]
        pass: Vec<String>,
        /// Test IDX failed; EVIDENCE says what went wrong (repeatable)
        #[arg(
            long,
            num_args = 2,
            value_names = ["IDX", "EVIDENCE"],
            allow_hyphen_values = true
        )]
        fail: Vec<String>,
    },

    /// Approve reviewed work; the task becomes done
    Approve {
        /// Task id
        #[arg(value_parser = parse_id)]
        id: i64,
        #[command(flatten)]
        who: Who,
        /// Remarks to keep in the history
        #[arg(long, default_value = "", allow_hyphen_values = true)]
        notes: String,
    },

    /// Send reviewed work back to development
    RequestChanges {
        /// Task id
        #[arg(value_parser = parse_id)]
        id: i64,
        #[command(flatten)]
        who: Who,
        /// What must change; the developer sees this when claiming
        #[arg(long, allow_hyphen_values = true)]
        notes: String,
    },

    /// Park an unclaimed task in backlog, or put it back in todo
    Move {
        /// Task id
        #[arg(value_parser = parse_id)]
        id: i64,
        /// backlog or todo
        status: String,
    },

    /// Give a claimed task back, keeping its status
    Release {
        /// Task id
        #[arg(value_parser = parse_id)]
        id: i64,
        #[command(flatten)]
        who: Who,
    },

    /// Change an unclaimed task that is not in review or done
    Edit {
        /// Task id
        #[arg(value_parser = parse_id)]
        id: i64,
        /// New title
        #[arg(long, allow_hyphen_values = true)]
        title: Option<String>,
        /// New priority: low, medium, high or urgent
        #[arg(long)]
        priority: Option<String>,
        /// Replace all tags with these (repeatable)
        #[arg(long = "tag", value_name = "TAG", allow_hyphen_values = true)]
        tags: Option<Vec<String>>,
        /// Replace all tests with these (repeatable; same shape as in `add`)
        #[arg(
            long = "test",
            num_args = 3,
            value_names = ["DESC", "INPUT", "OUTPUT"],
            allow_hyphen_values = true
        )]
        tests: Option<Vec<String>>,
        /// Also wait for these tasks (comma-separated or repeated)
        #[arg(long, value_delimiter = ',', value_parser = parse_id, value_name = "IDS")]
        after: Vec<i64>,
        /// Stop waiting for these tasks
        #[arg(
            long = "drop-after",
            value_delimiter = ',',
            value_parser = parse_id,
            value_name = "IDS"
        )]
        drop_after: Vec<i64>,
    },

    /// Delete an unclaimed task that is not in review or done
    Remove {
        /// Task id
        #[arg(value_parser = parse_id)]
        id: i64,
    },

    /// Counts per status, and the tasks each agent holds
    Status,
}

#[derive(Subcommand)]
enum AgentAction {
    /// Register an agent name
    Register {
        /// Agent name (no spaces)
        name: String,
        /// developer or reviewer
        #[arg(long, default_value = "developer")]
        role: String,
    },
    /// List agents, one `NAME ROLE` line each
    List,
    /// Remove an agent, releasing the tasks it holds
    Remove {
        /// Agent name
        name: String,
    },
}

/// Task ids are plain numbers, but replies print them as `#7`, so accept that
/// spelling too.
fn parse_id(text: &str) -> Result<i64, String> {
    text.trim()
        .trim_start_matches('#')
        .parse::<i64>()
        .ok()
        .filter(|id| *id > 0)
        .ok_or_else(|| format!("'{text}' is not a task id"))
}

/// clap hands a repeatable multi-value flag over as one flat list; each use
/// of the flag contributed exactly `size` values, so cut it back into uses.
fn group(values: &[String], size: usize) -> Vec<Vec<String>> {
    values.chunks(size).map(<[String]>::to_vec).collect()
}

fn run(command: Command) -> Result<String> {
    match command {
        Command::Init => commands::init(),
        Command::Guide { part } => Ok(commands::guide::text(part)),
        Command::Agent { action } => match action {
            AgentAction::Register { name, role } => commands::agent::register(&name, &role),
            AgentAction::List => commands::agent::list(),
            AgentAction::Remove { name } => commands::agent::remove(&name),
        },
        Command::Add {
            title,
            priority,
            tags,
            tests,
            after,
        } => commands::task::add(&NewTask {
            title,
            priority,
            tags,
            tests: group(&tests, 3),
            after,
        }),
        Command::List {
            status,
            tag,
            executor,
            priority,
            all,
            limit,
        } => commands::task::list(&ListFilter {
            status,
            tag,
            executor,
            priority,
            all,
            limit,
        }),
        Command::Show { id, history } => commands::task::show(id, history),
        Command::Edit {
            id,
            title,
            priority,
            tags,
            tests,
            after,
            drop_after,
        } => commands::task::edit(
            id,
            &TaskEdit {
                title,
                priority,
                tags,
                tests: tests.map(|tests| group(&tests, 3)),
                after,
                drop_after,
            },
        ),
        Command::Remove { id } => commands::task::remove(id),
        Command::Status => commands::status::status(),
        Command::Move { id, status } => lifecycle::move_status(id, &status),
        Command::Claim { id, who, lease } => lifecycle::claim(id, who.name()?, lease.lease),
        Command::ClaimNext { who, lease } => lifecycle::claim_next(who.name()?, lease.lease),
        Command::SubmitReview {
            id,
            who,
            pass,
            fail,
        } => lifecycle::submit_review(
            id,
            who.name()?,
            &lifecycle::parse_verdicts(&group(&pass, 2), &group(&fail, 2))?,
        ),
        Command::Approve { id, who, notes } => lifecycle::approve(id, who.name()?, &notes),
        Command::RequestChanges { id, who, notes } => {
            lifecycle::request_changes(id, who.name()?, &notes)
        }
        Command::Release { id, who } => lifecycle::release(id, who.name()?),
    }
}

fn main() {
    let cli = match Cli::try_parse() {
        Ok(cli) => cli,
        Err(err) => {
            // --help and --version are not errors: print them the way clap
            // normally does and exit 0.
            if matches!(
                err.kind(),
                ErrorKind::DisplayHelp
                    | ErrorKind::DisplayVersion
                    | ErrorKind::DisplayHelpOnMissingArgumentOrSubcommand
            ) {
                err.exit();
            }
            output::fail_usage(&err.to_string(), err.exit_code());
        }
    };

    if let Some(path) = cli.db {
        db::set_path_override(path);
    }

    match run(cli.command) {
        Ok(reply) => output::print(&reply),
        Err(err) => output::fail(&err),
    }
}
