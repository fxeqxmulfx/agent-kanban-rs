//! `agent-kanban guide [PART]`: the manual an agent reads, whole or one part.
//!
//! `guide.txt` is the whole manual. A section starts at a heading line made of
//! capital letters (`WORK`, `DEVELOPER`, ...) and runs over the indented lines
//! below it; the unindented lines around the sections (the opening identity
//! line, the closing escape rule) belong to every part. A part is a subset of
//! the manual's own lines, in the manual's order. It has no text of its own,
//! so a part cannot drift from the whole.

use clap::ValueEnum;

/// The manual printed by `agent-kanban guide`: everything an agent needs, in
/// far fewer tokens than the `--help` screens or the README.
const GUIDE: &str = include_str!("guide.txt");

/// The part of the manual one job needs. Each name is the heading of its main
/// section.
#[derive(Clone, Copy, Debug, ValueEnum)]
pub enum Part {
    Developer,
    Reviewer,
    Planning,
    Board,
}

impl Part {
    /// Headings of the sections this part is made of.
    const fn sections(self) -> &'static [&'static str] {
        match self {
            Self::Developer => &["WORK", "DEVELOPER"],
            Self::Reviewer => &["WORK", "REVIEWER"],
            Self::Planning => &["PLANNING"],
            Self::Board => &["BOARD"],
        }
    }
}

fn is_heading(line: &str) -> bool {
    !line.is_empty() && line.bytes().all(|byte| byte.is_ascii_uppercase())
}

/// The whole manual, or just `part` of it.
pub fn text(part: Option<Part>) -> String {
    let manual = GUIDE.trim_end();
    let Some(part) = part else {
        return manual.to_string();
    };
    let mut keep = true;
    let mut lines = Vec::new();
    for line in manual.lines() {
        if is_heading(line) {
            keep = part.sections().contains(&line);
        } else if !line.starts_with(' ') {
            keep = true;
        }
        if keep {
            lines.push(line);
        }
    }
    lines.join("\n")
}

#[cfg(test)]
mod tests {
    use super::{Part, is_heading, text};
    use clap::{CommandFactory, ValueEnum};

    fn manual() -> Vec<String> {
        text(None).lines().map(String::from).collect()
    }

    fn part_lines(part: Part) -> Vec<String> {
        text(Some(part)).lines().map(String::from).collect()
    }

    /// The guide is the one place an agent learns the CLI, so it must not
    /// fall behind: every subcommand and every flag has to appear in it.
    #[test]
    fn guide_mentions_every_subcommand_and_flag() {
        let guide = text(None);
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
        assert_eq!(text(None), text(None).trim_end());
        assert!(text(None).starts_with("agent-kanban: "));
        for part in Part::value_variants() {
            assert_eq!(text(Some(*part)), text(Some(*part)).trim_end());
        }
    }

    #[test]
    fn headings_are_capitals_alone_on_their_line() {
        for heading in ["WORK", "BOARD", "A"] {
            assert!(is_heading(heading), "{heading}");
        }
        for other in [
            "",
            "Board",
            "  BOARD",
            "BOARD ",
            "WORK:",
            "In `|` lines",
            "  list",
        ] {
            assert!(!is_heading(other), "{other:?}");
        }
    }

    #[test]
    fn every_part_is_made_of_the_manuals_own_lines_in_its_order() {
        let manual = manual();
        for part in Part::value_variants() {
            let mut rest = manual.iter();
            for line in part_lines(*part) {
                assert!(
                    rest.any(|candidate| *candidate == line),
                    "{part:?}: {line:?} is not in the manual, or is out of order"
                );
            }
        }
    }

    #[test]
    fn the_parts_together_cover_every_line_of_the_manual() {
        let mut covered: Vec<String> = Part::value_variants()
            .iter()
            .flat_map(|p| part_lines(*p))
            .collect();
        covered.sort();
        covered.dedup();
        let mut all = manual();
        all.sort();
        all.dedup();
        assert_eq!(covered, all);
    }

    #[test]
    fn every_heading_of_the_manual_belongs_to_some_part() {
        let belonging: Vec<&str> = Part::value_variants()
            .iter()
            .flat_map(|part| part.sections().iter().copied())
            .collect();
        let headings: Vec<String> = manual().into_iter().filter(|l| is_heading(l)).collect();

        assert!(headings.len() >= 5, "{headings:?}");
        for heading in &headings {
            assert!(belonging.contains(&heading.as_str()), "{heading}");
        }
        for heading in belonging {
            assert!(
                headings.iter().any(|h| h == heading),
                "no section {heading}"
            );
        }
    }

    /// The word typed after `guide` is the heading of the part's own section.
    #[test]
    fn a_part_is_named_after_its_main_section() {
        for part in Part::value_variants() {
            let name = part.to_possible_value().unwrap().get_name().to_uppercase();
            assert!(part.sections().contains(&name.as_str()), "{part:?}");
        }
    }

    #[test]
    fn every_part_opens_and_closes_like_the_manual() {
        let manual = manual();
        for part in Part::value_variants() {
            let lines = part_lines(*part);
            assert_eq!(lines[..2], manual[..2], "{part:?}");
            assert_eq!(lines.last(), manual.last(), "{part:?}");
        }
    }

    #[test]
    fn each_part_carries_its_own_commands_and_not_the_others() {
        let cases: [(Part, &[&str], &[&str]); 4] = [
            (
                Part::Developer,
                &[
                    "claim-next",
                    "claim ID",
                    "--lease",
                    "submit-review",
                    "--pass",
                    "release",
                ],
                &[
                    "approve",
                    "request-changes",
                    "add --title",
                    "list [",
                    "agent register",
                    "skill",
                ],
            ),
            (
                Part::Reviewer,
                &[
                    "claim-next",
                    "claim ID",
                    "|RESULT|EVIDENCE",
                    "approve",
                    "request-changes",
                ],
                &[
                    "submit-review",
                    "release ID",
                    "add --title",
                    "list [",
                    "skill",
                ],
            ),
            (
                Part::Planning,
                &[
                    "add --title",
                    "edit ID",
                    "move ID",
                    "remove ID",
                    "agent register",
                    "--after",
                ],
                &["claim-next", "submit-review", "approve", "list [", "skill"],
            ),
            (
                Part::Board,
                &[
                    "list [",
                    "--limit",
                    "show ID",
                    "status",
                    "agent list",
                    "init",
                    "guide|skill [",
                    "--db",
                ],
                &["claim-next", "submit-review", "approve", "add --title"],
            ),
        ];
        for (part, present, absent) in cases {
            let text = text(Some(part));
            for needle in present {
                assert!(text.contains(needle), "{part:?} lacks {needle:?}:\n{text}");
            }
            for needle in absent {
                assert!(!text.contains(needle), "{part:?} has {needle:?}:\n{text}");
            }
        }
    }

    /// A part is only worth asking for if it is much smaller than the whole.
    #[test]
    fn a_part_is_much_shorter_than_the_manual() {
        let whole = text(None).len();
        for part in Part::value_variants() {
            let size = text(Some(*part)).len();
            assert!(
                size * 100 <= whole * 70,
                "{part:?}: {size} of {whole} bytes"
            );
        }
    }
}
