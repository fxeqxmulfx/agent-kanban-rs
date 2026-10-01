pub mod agent;
pub mod deps;
pub mod guide;
pub mod lifecycle;
pub mod status;
pub mod task;
pub mod view;

use anyhow::Result;

pub const PRIORITIES: [&str; 4] = ["low", "medium", "high", "urgent"];
pub const STATUSES: [&str; 5] = ["backlog", "todo", "in_progress", "review", "done"];

pub fn init() -> Result<String> {
    crate::db::init()?;
    Ok("initialized".to_string())
}
