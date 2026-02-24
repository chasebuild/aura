use chrono::{DateTime, Utc};
use indexmap::IndexMap;
use serde::{Deserialize, Serialize};
use uuid::Uuid;

macro_rules! id_newtype {
    ($name:ident) => {
        #[derive(
            Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, PartialOrd, Ord,
        )]
        #[serde(transparent)]
        pub struct $name(pub Uuid);

        impl $name {
            pub fn new() -> Self {
                Self(Uuid::new_v4())
            }
        }

        impl Default for $name {
            fn default() -> Self {
                Self::new()
            }
        }
    };
}

id_newtype!(TaskId);
id_newtype!(WorkspaceId);
id_newtype!(SessionId);
id_newtype!(ProcessId);
id_newtype!(ProjectId);
id_newtype!(OrchTaskId);
id_newtype!(OrchAttemptId);

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CanonicalTaskState {
    Backlog,
    Ready,
    Running,
    Review,
    Done,
    Cancelled,
    Blocked,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct BoardColumnId(pub String);

impl BoardColumnId {
    pub fn new(value: impl Into<String>) -> Self {
        Self(value.into())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BoardColumn {
    pub id: BoardColumnId,
    pub name: String,
    pub canonical_state: CanonicalTaskState,
    pub order: i32,
    pub wip_limit: Option<u32>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TransitionRule {
    pub from_column_id: BoardColumnId,
    pub to_column_id: BoardColumnId,
    pub requires_clean_git: bool,
    pub requires_no_running_process: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ExecutorKind {
    Amp,
    Gemini,
    Codex,
    Claude,
    Opencode,
    Local,
    QwenCode,
    Copilot,
    CursorAgent,
    Droid,
    Custom(String),
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RunReason {
    Setup,
    Coding,
    Review,
    Cleanup,
    DevServer,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ExecutionStage {
    PrepareWorkspace,
    Setup,
    CodingInitial,
    CodingFollowUp,
    Review,
    Cleanup,
}

impl ExecutionStage {
    pub fn template_key(self) -> &'static str {
        match self {
            Self::PrepareWorkspace => "prepare_workspace",
            Self::Setup => "setup",
            Self::CodingInitial => "coding_initial",
            Self::CodingFollowUp => "coding_follow_up",
            Self::Review => "review",
            Self::Cleanup => "cleanup",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ExecutionStatus {
    Queued,
    Running,
    Completed,
    Failed,
    Killed,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ExecutionEvent {
    StartRequested,
    WorkspacePrepared,
    SetupCompleted,
    CodingCompleted { changes_made: bool },
    FollowUpRequested,
    ReviewCompleted,
    CleanupCompleted,
    StageFailed { message: String },
    CancelRequested,
    Interrupted,
    WorkerLost,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ExecutionAction {
    PrepareWorkspace,
    RunSetup,
    RunCodingInitial,
    RunCodingFollowUp,
    RunReview,
    RunCleanup,
    MarkTaskState { state: CanonicalTaskState },
    PersistEvent,
    InterruptExecution,
    FailAsWorkerLost,
    CompleteExecution,
    Noop,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MachineState {
    pub process_id: ProcessId,
    pub workspace_id: WorkspaceId,
    pub task_id: TaskId,
    pub stage: Option<ExecutionStage>,
    pub status: ExecutionStatus,
    pub task_state: CanonicalTaskState,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProcessSnapshot {
    pub process_id: ProcessId,
    pub session_id: SessionId,
    pub task_id: TaskId,
    pub workspace_id: WorkspaceId,
    pub run_reason: RunReason,
    pub stage: Option<ExecutionStage>,
    pub status: ExecutionStatus,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
    pub metadata: IndexMap<String, serde_json::Value>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OrchestratorTaskStatus {
    Pending,
    Running,
    ReadyForReview,
    NeedsHuman,
    Completed,
    Failed,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum OrchestratorTaskPriority {
    Low,
    #[default]
    Normal,
    High,
    Critical,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OrchestratorAttemptStatus {
    Pending,
    Running,
    Succeeded,
    Failed,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GateStatus {
    Pending,
    Passed,
    Failed,
}
