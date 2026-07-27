mod commands;
mod db;
mod output;

use clap::error::ErrorKind;
use clap::{Parser, Subcommand};
use std::path::PathBuf;

#[derive(Parser)]
#[command(
    name = "agent-kanban",
    about = "Kanban board for concurrent LLM agents",
    version
)]
struct Cli {
    /// Indent JSON output.
    #[arg(long, global = true, conflicts_with = "table")]
    pretty: bool,

    /// Render output as a human-readable table instead of JSON.
    #[arg(long, global = true)]
    table: bool,

    /// Use this exact database file instead of discovering `.kanban/` by
    /// walking up from the current directory. For `init`, creates the
    /// database here (making parent directories as needed) instead of at
    /// the default `.kanban/board.db`.
    #[arg(long, global = true, value_name = "PATH")]
    db: Option<PathBuf>,

    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Create `.kanban/board.db` in the current directory.
    Init,

    /// Manage registered agents.
    Agent {
        #[command(subcommand)]
        action: AgentAction,
    },

    /// Create a task. `--test` may be repeated; each is a JSON object
    /// `{"describe","input","output"}`. At least one `--test` is required.
    Add {
        /// Task title.
        #[arg(long)]
        title: String,
        /// Priority: low, medium, high, or urgent.
        #[arg(long)]
        priority: String,
        /// Tag to attach (repeatable).
        #[arg(long = "tag")]
        tags: Vec<String>,
        /// A test spec as JSON: {"describe","input","output"} (repeatable,
        /// at least one required).
        #[arg(long = "test", required = true)]
        tests: Vec<String>,
    },

    /// List tasks, optionally filtered and sorted.
    List {
        /// Filter by status.
        #[arg(long)]
        status: Option<String>,
        /// Filter by tag.
        #[arg(long)]
        tag: Option<String>,
        /// Filter by claiming agent's name.
        #[arg(long)]
        executor: Option<String>,
        /// Filter by priority.
        #[arg(long)]
        priority: Option<String>,
        /// Sort order: priority (by severity, not alphabetically) or `created_at`.
        #[arg(long)]
        sort: Option<String>,
    },

    /// Show a single task.
    Show {
        /// Task id.
        id: i64,
    },

    /// Claim development work for a developer.
    Claim {
        /// Task id.
        id: i64,
        /// Registered developer name.
        #[arg(long)]
        agent: String,
        /// Claim lease duration in seconds.
        #[arg(long, default_value_t = commands::lifecycle::DEFAULT_LEASE_SECONDS)]
        lease_seconds: u32,
    },

    /// Submit completed development work for review.
    SubmitReview {
        /// Task id.
        id: i64,
        /// Developer currently holding the task.
        #[arg(long)]
        agent: String,
        /// Acceptance result JSON: {"criterion":0,"status":"passed|failed","evidence":"..."}.
        #[arg(long = "result", required = true)]
        results: Vec<String>,
    },

    /// Claim a task in review for a reviewer.
    ClaimReview {
        /// Task id.
        id: i64,
        /// Registered reviewer name.
        #[arg(long)]
        agent: String,
        /// Claim lease duration in seconds.
        #[arg(long, default_value_t = commands::lifecycle::DEFAULT_LEASE_SECONDS)]
        lease_seconds: u32,
    },

    /// Approve the current review revision and finish the task.
    Approve {
        /// Task id.
        id: i64,
        /// Reviewer currently holding the task.
        #[arg(long)]
        agent: String,
        /// Review notes.
        #[arg(long, default_value = "")]
        notes: String,
    },

    /// Return the current review revision to development.
    RequestChanges {
        /// Task id.
        id: i64,
        /// Reviewer currently holding the task.
        #[arg(long)]
        agent: String,
        /// Required review findings or requested changes.
        #[arg(long)]
        notes: String,
    },

    /// Move an unclaimed task between backlog and todo.
    Move {
        /// Task id.
        id: i64,
        /// New status: backlog or todo.
        #[arg(long)]
        status: String,
    },

    /// Release a task without changing its status.
    Release {
        /// Task id.
        id: i64,
        /// Agent currently holding the task.
        #[arg(long)]
        agent: String,
    },

    /// Edit an unowned task outside review and done.
    Edit {
        /// Task id.
        id: i64,
        /// New title.
        #[arg(long)]
        title: Option<String>,
        /// New priority: low, medium, high, or urgent.
        #[arg(long)]
        priority: Option<String>,
        /// Replace tags with this set (repeatable).
        #[arg(long = "tag")]
        tags: Option<Vec<String>>,
        /// Replace tests with this set (repeatable); same shape as `add`'s
        /// `--test`.
        #[arg(long = "test")]
        tests: Option<Vec<String>>,
    },

    /// Hard-delete an unowned task outside review and done.
    Remove {
        /// Task id.
        id: i64,
    },

    /// Board overview: task counts per status column plus each registered
    /// agent's current claimed-task count.
    Status,

    /// Show the lifecycle transition table enforced by mutation commands.
    Transitions,
}

#[derive(Subcommand)]
enum AgentAction {
    /// Register a new agent name and role.
    Register {
        /// Agent name to register.
        name: String,
        /// Agent role: developer or reviewer.
        #[arg(long, default_value = "developer")]
        role: String,
    },
    /// List registered agents.
    List,
    /// Remove an agent, auto-releasing any tasks it holds.
    Remove {
        /// Agent name to remove.
        name: String,
    },
}

fn main() {
    let cli = match Cli::try_parse() {
        Ok(cli) => cli,
        Err(err) => {
            // --help/--version aren't errors -- print them exactly as clap
            // normally would (human-readable, exit 0), not as JSON.
            if matches!(
                err.kind(),
                ErrorKind::DisplayHelp
                    | ErrorKind::DisplayVersion
                    | ErrorKind::DisplayHelpOnMissingArgumentOrSubcommand
            ) {
                err.exit();
            }
            // Every other parse failure (bad flag, non-numeric id, missing
            // required argument, unknown subcommand) must still honor this
            // tool's JSON error contract -- `Cli::parse()` would otherwise
            // print clap's own multi-line human-readable text here instead,
            // which the rest of this program's error handling deliberately
            // avoids everywhere else.
            output::fail_with_code(&err.to_string(), err.exit_code(), output::Format::Compact);
        }
    };

    if let Some(path) = cli.db.clone() {
        db::set_path_override(path);
    }

    let result = match cli.command {
        Command::Init => commands::init(),
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
        } => commands::task::add(&title, &priority, &tags, &tests),
        Command::List {
            status,
            tag,
            executor,
            priority,
            sort,
        } => commands::task::list(status, tag, executor, priority, sort.as_deref()),
        Command::Show { id } => commands::task::show(id),
        Command::Claim {
            id,
            agent,
            lease_seconds,
        } => commands::lifecycle::claim(id, &agent, lease_seconds),
        Command::SubmitReview { id, agent, results } => {
            commands::lifecycle::submit_review(id, &agent, &results)
        }
        Command::ClaimReview {
            id,
            agent,
            lease_seconds,
        } => commands::lifecycle::claim_review(id, &agent, lease_seconds),
        Command::Approve { id, agent, notes } => commands::lifecycle::approve(id, &agent, &notes),
        Command::RequestChanges { id, agent, notes } => {
            commands::lifecycle::request_changes(id, &agent, &notes)
        }
        Command::Move { id, status } => commands::lifecycle::move_status(id, &status),
        Command::Release { id, agent } => commands::lifecycle::release(id, &agent),
        Command::Edit {
            id,
            title,
            priority,
            tags,
            tests,
        } => commands::task::edit(id, title, priority, tags, tests.as_deref()),
        Command::Remove { id } => commands::task::remove(id),
        Command::Status => commands::status::status(),
        Command::Transitions => Ok(commands::lifecycle::transitions()),
    };

    let format = if cli.table {
        output::Format::Table
    } else if cli.pretty {
        output::Format::Pretty
    } else {
        output::Format::Compact
    };

    match result {
        Ok(value) => output::print(&value, format),
        Err(err) => output::fail(&err, format),
    }
}
