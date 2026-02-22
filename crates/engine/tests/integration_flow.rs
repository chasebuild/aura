use std::path::PathBuf;
use std::sync::Arc;

use aura_contracts::{
    BoardColumn, BoardColumnId, CanonicalTaskState, ExecutionEvent, ExecutionStage,
    ExecutionStatus, ProcessId, ProjectId, RunReason, SessionId, TaskId, TransitionRule,
    WorkspaceId,
};
use aura_engine::orchestrator::EngineOrchestrator;
use aura_store::{
    BoardRulesRecord, BoardRulesStore, ExecutionRecord, ExecutionStore, SessionRecord,
    SessionStore, TaskRecord, TaskStore, WorkspaceRecord, WorkspaceStore, memory::MemoryStore,
};
use chrono::Utc;

async fn seed_store() -> (Arc<MemoryStore>, ProcessId, TaskId, WorkspaceId) {
    let store = Arc::new(MemoryStore::new());

    let task_id = TaskId::new();
    let workspace_id = WorkspaceId::new();
    let session_id = SessionId::new();
    let process_id = ProcessId::new();
    let now = Utc::now();

    store
        .upsert_task(TaskRecord {
            id: task_id,
            project_id: ProjectId::new(),
            title: "Implement feature".to_string(),
            description: None,
            canonical_state: CanonicalTaskState::Ready,
            board_column_id: None,
            metadata: Default::default(),
            created_at: now,
            updated_at: now,
        })
        .await
        .expect("task");

    store
        .upsert_workspace(WorkspaceRecord {
            id: workspace_id,
            task_id,
            root_path: PathBuf::from("/tmp/ws"),
            branch: "aura/feature".to_string(),
            repo_names: vec!["repo".to_string()],
            archived: false,
            created_at: now,
            updated_at: now,
        })
        .await
        .expect("workspace");

    store
        .upsert_session(SessionRecord {
            id: session_id,
            workspace_id,
            executor_profile_id: "Codex:DEFAULT".to_string(),
            created_at: now,
        })
        .await
        .expect("session");

    store
        .upsert_execution(ExecutionRecord {
            id: process_id,
            session_id,
            workspace_id,
            task_id,
            stage: None,
            status: ExecutionStatus::Queued,
            run_reason: RunReason::Coding,
            last_error: None,
            started_at: now,
            completed_at: None,
            updated_at: now,
        })
        .await
        .expect("execution");

    (store, process_id, task_id, workspace_id)
}

#[tokio::test]
async fn setup_coding_cleanup_review_success_flow() {
    let (store, process_id, task_id, _workspace_id) = seed_store().await;
    let orchestrator = EngineOrchestrator::new(store.clone());

    orchestrator
        .start_execution(process_id)
        .await
        .expect("start");
    orchestrator
        .apply_event(process_id, ExecutionEvent::WorkspacePrepared)
        .await
        .expect("prep");
    orchestrator
        .apply_event(process_id, ExecutionEvent::SetupCompleted)
        .await
        .expect("setup");
    orchestrator
        .apply_event(
            process_id,
            ExecutionEvent::CodingCompleted { changes_made: true },
        )
        .await
        .expect("coding");
    orchestrator
        .apply_event(process_id, ExecutionEvent::CleanupCompleted)
        .await
        .expect("cleanup");
    orchestrator
        .apply_event(process_id, ExecutionEvent::ReviewCompleted)
        .await
        .expect("review");

    let execution = store
        .get_execution(process_id)
        .await
        .expect("get")
        .expect("exists");
    assert_eq!(execution.status, ExecutionStatus::Completed);

    let task = store
        .get_task(task_id)
        .await
        .expect("get task")
        .expect("task");
    assert_eq!(task.canonical_state, CanonicalTaskState::Done);
}

#[tokio::test]
async fn coding_with_no_changes_skips_cleanup() {
    let (store, process_id, _task_id, _workspace_id) = seed_store().await;
    let orchestrator = EngineOrchestrator::new(store.clone());

    orchestrator
        .start_execution(process_id)
        .await
        .expect("start");
    orchestrator
        .apply_event(process_id, ExecutionEvent::WorkspacePrepared)
        .await
        .expect("prep");
    orchestrator
        .apply_event(process_id, ExecutionEvent::SetupCompleted)
        .await
        .expect("setup");
    orchestrator
        .apply_event(
            process_id,
            ExecutionEvent::CodingCompleted {
                changes_made: false,
            },
        )
        .await
        .expect("coding");

    let execution = store
        .get_execution(process_id)
        .await
        .expect("get")
        .expect("execution");
    assert_eq!(execution.stage, Some(ExecutionStage::Review));
}

#[tokio::test]
async fn setup_failure_marks_process_failed() {
    let (store, process_id, task_id, _workspace_id) = seed_store().await;
    let orchestrator = EngineOrchestrator::new(store.clone());

    orchestrator
        .start_execution(process_id)
        .await
        .expect("start");
    orchestrator
        .apply_event(process_id, ExecutionEvent::WorkspacePrepared)
        .await
        .expect("prep");
    orchestrator
        .apply_event(
            process_id,
            ExecutionEvent::StageFailed {
                message: "setup failed".to_string(),
            },
        )
        .await
        .expect("fail");

    let execution = store
        .get_execution(process_id)
        .await
        .expect("get")
        .expect("execution");
    assert_eq!(execution.status, ExecutionStatus::Failed);

    let task = store
        .get_task(task_id)
        .await
        .expect("get task")
        .expect("task");
    assert_eq!(task.canonical_state, CanonicalTaskState::Blocked);
}

#[tokio::test]
async fn queued_follow_up_after_completion() {
    let (store, process_id, task_id, _workspace_id) = seed_store().await;
    let orchestrator = EngineOrchestrator::new(store.clone());

    orchestrator
        .start_execution(process_id)
        .await
        .expect("start");
    orchestrator
        .apply_event(process_id, ExecutionEvent::WorkspacePrepared)
        .await
        .expect("prep");
    orchestrator
        .apply_event(process_id, ExecutionEvent::SetupCompleted)
        .await
        .expect("setup");
    orchestrator
        .apply_event(
            process_id,
            ExecutionEvent::CodingCompleted {
                changes_made: false,
            },
        )
        .await
        .expect("coding");
    orchestrator
        .apply_event(process_id, ExecutionEvent::ReviewCompleted)
        .await
        .expect("review done");

    orchestrator
        .apply_event(process_id, ExecutionEvent::FollowUpRequested)
        .await
        .expect("follow-up");

    let execution = store
        .get_execution(process_id)
        .await
        .expect("get")
        .expect("execution");
    assert_eq!(execution.stage, Some(ExecutionStage::CodingFollowUp));
    assert_eq!(execution.status, ExecutionStatus::Running);

    let task = store
        .get_task(task_id)
        .await
        .expect("get task")
        .expect("task");
    assert_eq!(task.canonical_state, CanonicalTaskState::Running);
}

#[tokio::test]
async fn cancellation_interrupts_and_marks_cancelled() {
    let (store, process_id, task_id, _workspace_id) = seed_store().await;
    let orchestrator = EngineOrchestrator::new(store.clone());

    orchestrator
        .start_execution(process_id)
        .await
        .expect("start");
    orchestrator
        .apply_event(process_id, ExecutionEvent::CancelRequested)
        .await
        .expect("cancel");

    let execution = store
        .get_execution(process_id)
        .await
        .expect("get")
        .expect("execution");
    assert_eq!(execution.status, ExecutionStatus::Killed);

    let task = store
        .get_task(task_id)
        .await
        .expect("get task")
        .expect("task");
    assert_eq!(task.canonical_state, CanonicalTaskState::Cancelled);
}

#[tokio::test]
async fn hybrid_status_rules_block_invalid_transition_when_running() {
    let (store, process_id, task_id, workspace_id) = seed_store().await;
    let orchestrator = EngineOrchestrator::new(store.clone());

    store
        .set_board_rules(BoardRulesRecord {
            columns: vec![
                BoardColumn {
                    id: BoardColumnId::new("todo"),
                    name: "Todo".to_string(),
                    canonical_state: CanonicalTaskState::Ready,
                    order: 0,
                    wip_limit: None,
                },
                BoardColumn {
                    id: BoardColumnId::new("review"),
                    name: "Review".to_string(),
                    canonical_state: CanonicalTaskState::Review,
                    order: 1,
                    wip_limit: None,
                },
            ],
            transition_rules: vec![TransitionRule {
                from_column_id: BoardColumnId::new("todo"),
                to_column_id: BoardColumnId::new("review"),
                requires_clean_git: true,
                requires_no_running_process: true,
            }],
        })
        .await
        .expect("rules");

    store
        .update_execution_stage_status(
            process_id,
            Some(ExecutionStage::Setup),
            ExecutionStatus::Running,
            None,
        )
        .await
        .expect("running");

    let err = orchestrator
        .can_transition_board_column(
            task_id,
            &BoardColumnId::new("todo"),
            &BoardColumnId::new("review"),
            true,
        )
        .await
        .expect_err("should be blocked");

    assert!(err.to_string().contains("running execution"));

    store
        .update_execution_stage_status(
            process_id,
            Some(ExecutionStage::Setup),
            ExecutionStatus::Completed,
            None,
        )
        .await
        .expect("done");

    orchestrator
        .can_transition_board_column(
            task_id,
            &BoardColumnId::new("todo"),
            &BoardColumnId::new("review"),
            true,
        )
        .await
        .expect("allowed now");

    orchestrator
        .apply_board_transition(task_id, BoardColumnId::new("review"))
        .await
        .expect("apply transition");

    let task = store
        .get_task(task_id)
        .await
        .expect("task")
        .expect("exists");
    assert_eq!(task.canonical_state, CanonicalTaskState::Review);
    assert_eq!(task.board_column_id, Some(BoardColumnId::new("review")));

    let ws = store
        .get_workspace(workspace_id)
        .await
        .expect("workspace")
        .expect("exists");
    assert!(!ws.archived);
}
