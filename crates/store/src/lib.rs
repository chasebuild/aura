pub mod memory;
#[cfg(feature = "sqlite")]
pub mod sqlite;

use std::path::PathBuf;

use async_trait::async_trait;
use aura_contracts::{
    BoardColumn, BoardColumnId, CanonicalTaskState, ExecutionStage, ExecutionStatus, GateStatus,
    OrchAttemptId, OrchTaskId, OrchestratorAttemptStatus, OrchestratorTaskPriority,
    OrchestratorTaskStatus, ProcessId, ProjectId, SessionId, TaskId, TransitionRule, WorkspaceId,
};
use chrono::{DateTime, Utc};
use indexmap::IndexMap;
use serde::{Deserialize, Serialize};
use thiserror::Error;

#[derive(Debug, Error)]
pub enum StoreError {
    #[error("entity not found: {0}")]
    NotFound(&'static str),
    #[error("conflict: {0}")]
    Conflict(String),
    #[error("validation error: {0}")]
    Validation(String),
    #[error("internal store error: {0}")]
    Internal(String),
}

pub type StoreResult<T> = Result<T, StoreError>;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TaskRecord {
    pub id: TaskId,
    pub project_id: ProjectId,
    pub title: String,
    pub description: Option<String>,
    pub canonical_state: CanonicalTaskState,
    pub board_column_id: Option<BoardColumnId>,
    pub metadata: IndexMap<String, serde_json::Value>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WorkspaceRecord {
    pub id: WorkspaceId,
    pub task_id: TaskId,
    pub root_path: PathBuf,
    pub branch: String,
    pub repo_names: Vec<String>,
    pub archived: bool,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionRecord {
    pub id: SessionId,
    pub workspace_id: WorkspaceId,
    pub executor_profile_id: String,
    pub created_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExecutionRecord {
    pub id: ProcessId,
    pub session_id: SessionId,
    pub workspace_id: WorkspaceId,
    pub task_id: TaskId,
    pub stage: Option<ExecutionStage>,
    pub status: ExecutionStatus,
    pub run_reason: aura_contracts::RunReason,
    pub last_error: Option<String>,
    pub started_at: DateTime<Utc>,
    pub completed_at: Option<DateTime<Utc>>,
    pub updated_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BoardRulesRecord {
    pub columns: Vec<BoardColumn>,
    pub transition_rules: Vec<TransitionRule>,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(tag = "scope", content = "id", rename_all = "snake_case")]
pub enum PromptScope {
    Global,
    Project(ProjectId),
    Task(TaskId),
    Workspace(WorkspaceId),
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PromptTemplateRecord {
    pub scope: PromptScope,
    pub stage: ExecutionStage,
    pub template: String,
    pub updated_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OrchTaskRecord {
    pub id: OrchTaskId,
    pub title: String,
    pub intent: String,
    pub status: OrchestratorTaskStatus,
    pub priority: OrchestratorTaskPriority,
    pub planner_enabled: bool,
    pub context_json: serde_json::Value,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OrchTaskDependencyRecord {
    pub task_id: OrchTaskId,
    pub depends_on_task_id: OrchTaskId,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OrchAttemptRecord {
    pub id: OrchAttemptId,
    pub task_id: OrchTaskId,
    pub process_id: Option<ProcessId>,
    pub executor_profile: String,
    pub prompt_snapshot: String,
    pub status: OrchestratorAttemptStatus,
    pub retry_index: i32,
    pub started_at: DateTime<Utc>,
    pub completed_at: Option<DateTime<Utc>>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OrchGateRecord {
    pub task_id: OrchTaskId,
    pub attempt_id: OrchAttemptId,
    pub gate_name: String,
    pub status: GateStatus,
    pub details_json: serde_json::Value,
    pub checked_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OrchEventRecord {
    pub task_id: OrchTaskId,
    pub attempt_id: Option<OrchAttemptId>,
    pub event_type: String,
    pub payload_json: serde_json::Value,
    pub created_at: DateTime<Utc>,
}

#[async_trait]
pub trait StoreTx: Send + Sync {
    async fn commit(self: Box<Self>) -> StoreResult<()>;
    async fn rollback(self: Box<Self>) -> StoreResult<()>;
}

#[async_trait]
pub trait TaskStore: Send + Sync {
    async fn upsert_task(&self, task: TaskRecord) -> StoreResult<()>;
    async fn get_task(&self, task_id: TaskId) -> StoreResult<Option<TaskRecord>>;
    async fn update_task_state(
        &self,
        task_id: TaskId,
        canonical_state: CanonicalTaskState,
        board_column_id: Option<BoardColumnId>,
    ) -> StoreResult<()>;
}

#[async_trait]
pub trait WorkspaceStore: Send + Sync {
    async fn upsert_workspace(&self, workspace: WorkspaceRecord) -> StoreResult<()>;
    async fn get_workspace(
        &self,
        workspace_id: WorkspaceId,
    ) -> StoreResult<Option<WorkspaceRecord>>;
    async fn list_workspaces_by_task(&self, task_id: TaskId) -> StoreResult<Vec<WorkspaceRecord>>;
    async fn set_workspace_archived(
        &self,
        workspace_id: WorkspaceId,
        archived: bool,
    ) -> StoreResult<()>;
}

#[async_trait]
pub trait SessionStore: Send + Sync {
    async fn upsert_session(&self, session: SessionRecord) -> StoreResult<()>;
    async fn get_session(&self, session_id: SessionId) -> StoreResult<Option<SessionRecord>>;
    async fn latest_session_by_workspace(
        &self,
        workspace_id: WorkspaceId,
    ) -> StoreResult<Option<SessionRecord>>;
}

#[async_trait]
pub trait ExecutionStore: Send + Sync {
    async fn upsert_execution(&self, execution: ExecutionRecord) -> StoreResult<()>;
    async fn get_execution(&self, process_id: ProcessId) -> StoreResult<Option<ExecutionRecord>>;
    async fn update_execution_stage_status(
        &self,
        process_id: ProcessId,
        stage: Option<ExecutionStage>,
        status: ExecutionStatus,
        last_error: Option<String>,
    ) -> StoreResult<()>;
    async fn list_executions_by_session(
        &self,
        session_id: SessionId,
    ) -> StoreResult<Vec<ExecutionRecord>>;
    async fn find_running_non_dev_by_workspace(
        &self,
        workspace_id: WorkspaceId,
    ) -> StoreResult<Vec<ExecutionRecord>>;
}

#[async_trait]
pub trait BoardRulesStore: Send + Sync {
    async fn set_board_rules(&self, board_rules: BoardRulesRecord) -> StoreResult<()>;
    async fn get_board_rules(&self) -> StoreResult<BoardRulesRecord>;
}

#[async_trait]
pub trait PromptTemplateStore: Send + Sync {
    async fn upsert_prompt_template(&self, template: PromptTemplateRecord) -> StoreResult<()>;
    async fn get_prompt_template(
        &self,
        scope: &PromptScope,
        stage: ExecutionStage,
    ) -> StoreResult<Option<PromptTemplateRecord>>;
    async fn list_prompt_templates(&self) -> StoreResult<Vec<PromptTemplateRecord>>;
}

#[async_trait]
pub trait OrchestratorStore: Send + Sync {
    async fn upsert_orch_task(&self, task: OrchTaskRecord) -> StoreResult<()>;
    async fn get_orch_task(&self, task_id: OrchTaskId) -> StoreResult<Option<OrchTaskRecord>>;
    async fn list_orch_tasks(&self) -> StoreResult<Vec<OrchTaskRecord>>;
    async fn set_orch_task_status(
        &self,
        task_id: OrchTaskId,
        status: OrchestratorTaskStatus,
    ) -> StoreResult<()>;

    async fn replace_orch_task_dependencies(
        &self,
        task_id: OrchTaskId,
        dependencies: Vec<OrchTaskDependencyRecord>,
    ) -> StoreResult<()>;
    async fn list_orch_task_dependencies(
        &self,
        task_id: OrchTaskId,
    ) -> StoreResult<Vec<OrchTaskDependencyRecord>>;

    async fn upsert_orch_attempt(&self, attempt: OrchAttemptRecord) -> StoreResult<()>;
    async fn get_orch_attempt(
        &self,
        attempt_id: OrchAttemptId,
    ) -> StoreResult<Option<OrchAttemptRecord>>;
    async fn list_orch_attempts_by_task(
        &self,
        task_id: OrchTaskId,
    ) -> StoreResult<Vec<OrchAttemptRecord>>;

    async fn replace_orch_gates(
        &self,
        task_id: OrchTaskId,
        attempt_id: OrchAttemptId,
        gates: Vec<OrchGateRecord>,
    ) -> StoreResult<()>;
    async fn list_orch_gates_by_attempt(
        &self,
        attempt_id: OrchAttemptId,
    ) -> StoreResult<Vec<OrchGateRecord>>;

    async fn append_orch_event(&self, event: OrchEventRecord) -> StoreResult<()>;
    async fn list_orch_events_by_task(
        &self,
        task_id: OrchTaskId,
    ) -> StoreResult<Vec<OrchEventRecord>>;
}

#[async_trait]
pub trait AuraStore:
    TaskStore
    + WorkspaceStore
    + SessionStore
    + ExecutionStore
    + BoardRulesStore
    + PromptTemplateStore
    + OrchestratorStore
{
    async fn begin_tx(&self) -> StoreResult<Box<dyn StoreTx>>;
}
