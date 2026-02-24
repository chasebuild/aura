use std::collections::{HashMap, HashSet};

use aura_contracts::OrchestratorTaskStatus;
use aura_store::{OrchTaskDependencyRecord, OrchTaskRecord};

pub fn select_runnable_tasks(
    tasks: &[OrchTaskRecord],
    deps: &HashMap<aura_contracts::OrchTaskId, Vec<OrchTaskDependencyRecord>>,
    max_concurrency: usize,
) -> Vec<OrchTaskRecord> {
    let done_states = [
        OrchestratorTaskStatus::Completed,
        OrchestratorTaskStatus::ReadyForReview,
    ];

    let done = tasks
        .iter()
        .filter(|task| done_states.contains(&task.status))
        .map(|task| task.id)
        .collect::<HashSet<_>>();

    let mut runnable = tasks
        .iter()
        .filter(|task| task.status == OrchestratorTaskStatus::Pending)
        .filter(|task| {
            deps.get(&task.id)
                .cloned()
                .unwrap_or_default()
                .iter()
                .all(|dep| done.contains(&dep.depends_on_task_id))
        })
        .cloned()
        .collect::<Vec<_>>();

    runnable.sort_by_key(|task| (priority_rank(task.priority), task.created_at));
    runnable.truncate(max_concurrency);
    runnable
}

fn priority_rank(priority: aura_contracts::OrchestratorTaskPriority) -> i32 {
    match priority {
        aura_contracts::OrchestratorTaskPriority::Critical => 0,
        aura_contracts::OrchestratorTaskPriority::High => 1,
        aura_contracts::OrchestratorTaskPriority::Normal => 2,
        aura_contracts::OrchestratorTaskPriority::Low => 3,
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use aura_contracts::{OrchTaskId, OrchestratorTaskPriority, OrchestratorTaskStatus};
    use chrono::Utc;

    use super::*;

    fn task(id: OrchTaskId, status: OrchestratorTaskStatus) -> OrchTaskRecord {
        OrchTaskRecord {
            id,
            title: "t".to_string(),
            intent: "i".to_string(),
            status,
            priority: OrchestratorTaskPriority::Normal,
            planner_enabled: false,
            context_json: serde_json::json!({}),
            created_at: Utc::now(),
            updated_at: Utc::now(),
        }
    }

    #[test]
    fn dependency_blocks_task_until_parent_done() {
        let parent = OrchTaskId::new();
        let child = OrchTaskId::new();
        let tasks = vec![
            task(parent, OrchestratorTaskStatus::Pending),
            task(child, OrchestratorTaskStatus::Pending),
        ];

        let mut deps = HashMap::new();
        deps.insert(
            child,
            vec![OrchTaskDependencyRecord {
                task_id: child,
                depends_on_task_id: parent,
            }],
        );

        let runnable = select_runnable_tasks(&tasks, &deps, 10);
        assert_eq!(runnable.len(), 1);
        assert_eq!(runnable[0].id, parent);
    }
}
