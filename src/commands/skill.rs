//! `agent-kanban skill [PART]`: the guide as a Claude Code skill file.
//!
//! A skill is a `SKILL.md`: a frontmatter whose `name` and `description` are
//! always in the model's context and decide when the skill is used, then a body
//! that is read only once it is. The body here is the guide itself, whole or
//! one part, so the skill cannot drift from the manual: save the output again
//! after an upgrade.

use super::guide::{self, Part};

/// The skill's name: the CLI's name, and for a part the part's name too. It
/// is also the name of the directory the file is saved in.
const fn name(part: Option<Part>) -> &'static str {
    match part {
        None => "agent-kanban",
        Some(Part::Developer) => "agent-kanban-developer",
        Some(Part::Reviewer) => "agent-kanban-reviewer",
        Some(Part::Planning) => "agent-kanban-planning",
        Some(Part::Board) => "agent-kanban-board",
    }
}

/// What the skill is for and when to use it, which is all the model has to go
/// on when it chooses. Plain words only (no colon, quote or hash), so that the
/// line is valid YAML without quoting.
const fn description(part: Option<Part>) -> &'static str {
    match part {
        None => {
            "Use the agent-kanban task board. Claim the next task, hand it in for review with \
             evidence, review the work of other agents, plan tasks and dependencies, inspect the \
             board. Use when the project has a .kanban directory or when asked to pick up, \
             review, plan or list tasks."
        }
        Some(Part::Developer) => {
            "Do tasks from the agent-kanban board as a developer. Claim the next task, work on \
             it, hand it in for review with test evidence and release it if stuck. Use when the \
             project has a .kanban directory or when asked to pick up the next task."
        }
        Some(Part::Reviewer) => {
            "Review tasks on the agent-kanban board as a reviewer. Claim the next task in \
             review, check it against its tests, then approve it or request changes. Use when \
             the project has a .kanban directory or when asked to review work."
        }
        Some(Part::Planning) => {
            "Plan work on the agent-kanban board. Add, edit, prioritize, order (with \
             dependencies) and remove tasks and register agents. Use when asked to create or \
             organize tasks on the board."
        }
        Some(Part::Board) => {
            "Look at the agent-kanban board. List and show tasks, see the status counts, which \
             agent holds what and what is blocked. Use when asked what is on the board or who \
             is working on what."
        }
    }
}

/// The skill file for the whole guide, or for just `part` of it.
pub fn text(part: Option<Part>) -> String {
    format!(
        "---\nname: {}\ndescription: {}\n---\n\n{}",
        name(part),
        description(part),
        guide::text(part)
    )
}

#[cfg(test)]
mod tests {
    use super::{Part, description, guide, name, text};
    use clap::ValueEnum;

    /// Every skill there is: the whole guide, then each part.
    fn all() -> Vec<Option<Part>> {
        std::iter::once(None)
            .chain(Part::value_variants().iter().copied().map(Some))
            .collect()
    }

    /// The frontmatter lines between the two `---` fences, and the body.
    fn split(file: &str) -> (Vec<&str>, &str) {
        let rest = file.strip_prefix("---\n").expect("opens with a fence");
        let (front, body) = rest.split_once("\n---\n\n").expect("closes with a fence");
        (front.lines().collect(), body)
    }

    #[test]
    fn a_skill_is_a_frontmatter_and_then_the_guide() {
        for part in all() {
            let file = text(part);
            let (front, body) = split(&file);

            assert_eq!(front.len(), 2, "{part:?}: only name and description");
            assert_eq!(front[0], format!("name: {}", name(part)), "{part:?}");
            assert_eq!(
                front[1],
                format!("description: {}", description(part)),
                "{part:?}"
            );
            assert_eq!(body, guide::text(part), "{part:?}");
            assert_eq!(file, file.trim_end(), "{part:?}: output adds the newline");
        }
    }

    /// The limits Claude Code puts on a name: lowercase letters, digits and
    /// hyphens, at most 64 characters, and neither of the reserved words.
    #[test]
    fn names_follow_the_rules_for_skill_names() {
        let mut seen = Vec::new();
        for part in all() {
            let name = name(part);

            assert!(name.len() <= 64, "{name}");
            assert!(
                name.bytes()
                    .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-'),
                "{name}"
            );
            assert!(!name.starts_with('-') && !name.ends_with('-'), "{name}");
            for reserved in ["anthropic", "claude"] {
                assert!(!name.contains(reserved), "{name}");
            }
            assert!(!seen.contains(&name), "two skills are named {name}");
            seen.push(name);
        }
    }

    /// The whole guide is `agent-kanban`; a part adds its own name, which is
    /// what the part is called on the command line.
    #[test]
    fn a_part_skill_is_named_after_its_part() {
        assert_eq!(name(None), "agent-kanban");
        for part in Part::value_variants() {
            let word = part.to_possible_value().unwrap().get_name().to_string();
            assert_eq!(name(Some(*part)), format!("agent-kanban-{word}"));
        }
    }

    /// The limits on a description: not empty, at most 1,024 characters, no
    /// tags. Beyond them: one line of plain words, safe as an unquoted YAML
    /// value, saying what the skill is for and when to use it.
    #[test]
    fn descriptions_say_what_for_and_when_in_plain_words() {
        let mut seen = Vec::new();
        for part in all() {
            let text = description(part);

            assert!(!text.is_empty() && text.len() <= 1024, "{part:?}");
            assert!(
                text.chars()
                    .all(|c| c.is_ascii_alphanumeric() || " .,-()".contains(c)),
                "{part:?}: {text}"
            );
            assert!(text.starts_with(|c: char| c.is_ascii_uppercase()), "{text}");
            assert!(text.ends_with('.'), "{text}");
            assert!(text.contains("agent-kanban"), "{text}");
            assert!(text.contains("Use when"), "{text}");
            assert!(!seen.contains(&text), "two skills share a description");
            seen.push(text);
        }
    }

    /// The model reads the description in every session, so keep it short.
    #[test]
    fn a_description_is_a_few_lines_not_a_page() {
        for part in all() {
            assert!(description(part).len() <= 300, "{part:?}");
        }
    }
}
