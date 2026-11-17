//! The tasks of the suite, from easy to hard.

mod basics;
mod data;
mod durable;
mod recovery;
mod repair;

pub use recovery::ONBOARDING;
pub use repair::{BUG_DIGEST, SHIP_ORDERS};

use crate::task::Task;

/// Every task, easiest first.
pub fn all() -> Vec<Task> {
    let mut tasks = basics::tasks();
    tasks.extend(data::tasks());
    tasks.extend(durable::tasks());
    tasks.extend(recovery::tasks());
    tasks.extend(repair::tasks());
    tasks.sort_by_key(|task| task.difficulty);
    tasks
}

/// The tasks whose id is in `ids`, or every task when `ids` is empty. An unknown id is an
/// error naming it.
pub fn select(ids: &[String]) -> Result<Vec<Task>, String> {
    let tasks = all();
    if ids.is_empty() {
        return Ok(tasks);
    }
    ids.iter()
        .map(|id| {
            tasks
                .iter()
                .find(|task| task.id == id)
                .cloned()
                .ok_or_else(|| format!("unknown task {id:?}; `evals list` lists them"))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;

    use super::*;
    use crate::fake;

    #[test]
    fn the_suite_has_at_least_twenty_distinct_tasks_on_known_servers() {
        let tasks = all();
        assert!(tasks.len() >= 20, "{} tasks", tasks.len());
        let ids: BTreeSet<&str> = tasks.iter().map(|task| task.id).collect();
        assert_eq!(ids.len(), tasks.len(), "task ids are unique");
        for task in &tasks {
            assert!(
                task.id.chars().all(|c| c.is_ascii_lowercase() || c == '_'),
                "{}",
                task.id
            );
            let world = (task.world)();
            assert!(!world.servers.is_empty(), "{}", task.id);
            for server in world.servers {
                assert!(fake::SERVERS.contains(&server), "{}: {server}", task.id);
            }
            assert!(!(task.prompt)(&[]).is_empty());
        }
    }

    #[test]
    fn selection_names_unknown_tasks() {
        assert_eq!(select(&["open_issue".into()]).unwrap()[0].id, "open_issue");
        assert!(select(&["nope".into()]).unwrap_err().contains("nope"));
    }
}
