/// Print a successful reply. Replies are already-rendered text: a few plain
/// lines, or a task's header line and `|` rows (see `commands::view`).
pub fn print(reply: &str) {
    println!("{reply}");
}

/// Print `error: <message>` to stderr and exit 1 (a command failed).
pub fn fail(err: &anyhow::Error) -> ! {
    eprintln!("error: {err}");
    std::process::exit(1);
}

/// Print a command-line usage error (clap's own text, minus its generic
/// trailer) to stderr and exit with `code`, conventionally 2.
pub fn fail_usage(message: &str, code: i32) -> ! {
    eprintln!("{}", usage_text(message));
    std::process::exit(code);
}

/// clap ends every error with a blank line and "For more information, try
/// '--help'." -- noise for an agent that already got the usage line.
fn usage_text(message: &str) -> &str {
    message
        .trim_end()
        .rsplit_once("\n\nFor more information")
        .map_or_else(|| message.trim_end(), |(head, _)| head)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn usage_text_drops_clap_trailer_but_keeps_error_and_usage() {
        let clap = "error: the following required arguments were not provided:\n  --title <TITLE>\n\nUsage: agent-kanban add --title <TITLE>\n\nFor more information, try '--help'.\n";
        assert_eq!(
            usage_text(clap),
            "error: the following required arguments were not provided:\n  --title <TITLE>\n\nUsage: agent-kanban add --title <TITLE>"
        );
    }

    #[test]
    fn usage_text_leaves_messages_without_a_trailer_alone() {
        assert_eq!(usage_text("error: boom\n"), "error: boom");
        assert_eq!(usage_text("error: boom"), "error: boom");
    }
}
