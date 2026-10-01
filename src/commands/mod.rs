pub mod agent;
pub mod deps;
pub mod lifecycle;
pub mod status;
pub mod task;
pub mod view;

use anyhow::Result;

pub const PRIORITIES: [&str; 4] = ["low", "medium", "high", "urgent"];
pub const STATUSES: [&str; 5] = ["backlog", "todo", "in_progress", "review", "done"];

/// The built-in usage guide printed by `agent-kanban guide`: everything an
/// agent needs, in far fewer tokens than the `--help` screens or the README.
pub const GUIDE: &str = include_str!("guide.txt");

pub fn init() -> Result<String> {
    crate::db::init()?;
    Ok("initialized".to_string())
}

pub fn guide() -> String {
    GUIDE.trim_end().to_string()
}

#[cfg(test)]
mod tests {
    use clap::CommandFactory;

    /// The guide is the one place an agent learns the CLI, so it must not
    /// fall behind: every subcommand and every flag has to appear in it.
    #[test]
    fn guide_mentions_every_subcommand_and_flag() {
        let guide = super::guide();
        let mut missing = Vec::new();
        let mut commands = vec![crate::Cli::command()];
        while let Some(command) = commands.pop() {
            for arg in command.get_arguments() {
                if let Some(long) = arg.get_long()
                    && !matches!(long, "help" | "version")
                    && !guide.contains(&format!("--{long}"))
                {
                    missing.push(format!("--{long}"));
                }
            }
            for sub in command.get_subcommands() {
                if sub.get_name() != "help" && !guide.contains(sub.get_name()) {
                    missing.push(sub.get_name().to_string());
                }
                commands.push(sub.clone());
            }
        }
        assert!(missing.is_empty(), "guide does not mention {missing:?}");
    }

    #[test]
    fn guide_has_no_trailing_newline_to_pay_for() {
        assert_eq!(super::guide(), super::guide().trim_end());
        assert!(super::guide().starts_with("agent-kanban: "));
    }
}
