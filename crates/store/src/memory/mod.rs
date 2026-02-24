use std::collections::HashMap;
use std::sync::Arc;

use async_trait::async_trait;
use aura_contracts::{
    ExecutionStatus, OrchAttemptId, OrchTaskId, OrchestratorTaskStatus, ProcessId, SessionId,
    WorkspaceId,
};
use chrono::Utc;
use tokio::sync::RwLock;

use crate::{
    AuraStore, BoardRulesRecord, BoardRulesStore, ExecutionRecord, ExecutionStore,
    OrchAttemptRecord, OrchEventRecord, OrchGateRecord, OrchTaskDependencyRecord, OrchTaskRecord,
    OrchestratorStore, PromptScope, PromptTemplateRecord, PromptTemplateStore, SessionRecord,
    SessionStore, StoreError, StoreResult, StoreTx, TaskRecord, TaskStore, WorkspaceRecord,
    WorkspaceStore,
};

#[derive(Default)]
struct MemoryState {
    tasks: HashMap<aura_contracts::TaskId, TaskRecord>,
    workspaces: HashMap<aura_contracts::WorkspaceId, WorkspaceRecord>,
    sessions: HashMap<aura_contracts::SessionId, SessionRecord>,
    executions: HashMap<aura_contracts::ProcessId, ExecutionRecord>,
    board_rules: Option<BoardRulesRecord>,
    templates: HashMap<(PromptScope, aura_contracts::ExecutionStage), PromptTemplateRecord>,
    orch_tasks: HashMap<OrchTaskId, OrchTaskRecord>,
    orch_task_dependencies: HashMap<OrchTaskId, Vec<OrchTaskDependencyRecord>>,
    orch_attempts: HashMap<OrchAttemptId, OrchAttemptRecord>,
    orch_gates: HashMap<OrchAttemptId, Vec<OrchGateRecord>>,
    orch_events: Vec<OrchEventRecord>,
}

#[derive(Clone, Default)]
pub struct MemoryStore {
    inner: Arc<RwLock<MemoryState>>,
}

impl MemoryStore {
    pub fn new() -> Self {
        Self::default()
    }
}

#[derive(Default)]
struct MemoryTx;

#[async_trait]
impl StoreTx for MemoryTx {
    async fn commit(self: Box<Self>) -> StoreResult<()> {
        let _ = self;
        Ok(())
    }

    async fn rollback(self: Box<Self>) -> StoreResult<()> {
        let _ = self;
        Ok(())
    }
}

#[async_trait]
impl TaskStore for MemoryStore {
    async fn upsert_task(&self, task: TaskRecord) -> StoreResult<()> {
        self.inner.write().await.tasks.insert(task.id, task);
        Ok(())
    }

    async fn get_task(&self, task_id: aura_contracts::TaskId) -> StoreResult<Option<TaskRecord>> {
        Ok(self.inner.read().await.tasks.get(&task_id).cloned())
    }

    async fn update_task_state(
        &self,
        task_id: aura_contracts::TaskId,
        canonical_state: aura_contracts::CanonicalTaskState,
        board_column_id: Option<aura_contracts::BoardColumnId>,
    ) -> StoreResult<()> {
        let mut inner = self.inner.write().await;
        let task = inner
            .tasks
            .get_mut(&task_id)
            .ok_or(StoreError::NotFound("task"))?;
        task.canonical_state = canonical_state;
        task.board_column_id = board_column_id;
        task.updated_at = Utc::now();
        Ok(())
    }
}

#[async_trait]
impl WorkspaceStore for MemoryStore {
    async fn upsert_workspace(&self, workspace: WorkspaceRecord) -> StoreResult<()> {
        self.inner
            .write()
            .await
            .workspaces
            .insert(workspace.id, workspace);
        Ok(())
    }

    async fn get_workspace(
        &self,
        workspace_id: aura_contracts::WorkspaceId,
    ) -> StoreResult<Option<WorkspaceRecord>> {
        Ok(self
            .inner
            .read()
            .await
            .workspaces
            .get(&workspace_id)
            .cloned())
    }

    async fn list_workspaces_by_task(
        &self,
        task_id: aura_contracts::TaskId,
    ) -> StoreResult<Vec<WorkspaceRecord>> {
        let inner = self.inner.read().await;
        let mut result = inner
            .workspaces
            .values()
            .filter(|w| w.task_id == task_id)
            .cloned()
            .collect::<Vec<_>>();
        result.sort_by_key(|w| w.created_at);
        Ok(result)
    }

    async fn set_workspace_archived(
        &self,
        workspace_id: aura_contracts::WorkspaceId,
        archived: bool,
    ) -> StoreResult<()> {
        let mut inner = self.inner.write().await;
        let workspace = inner
            .workspaces
            .get_mut(&workspace_id)
            .ok_or(StoreError::NotFound("workspace"))?;
        workspace.archived = archived;
        workspace.updated_at = Utc::now();
        Ok(())
    }
}

#[async_trait]
impl SessionStore for MemoryStore {
    async fn upsert_session(&self, session: SessionRecord) -> StoreResult<()> {
        self.inner
            .write()
            .await
            .sessions
            .insert(session.id, session);
        Ok(())
    }

    async fn get_session(&self, session_id: SessionId) -> StoreResult<Option<SessionRecord>> {
        Ok(self.inner.read().await.sessions.get(&session_id).cloned())
    }

    async fn latest_session_by_workspace(
        &self,
        workspace_id: WorkspaceId,
    ) -> StoreResult<Option<SessionRecord>> {
        let inner = self.inner.read().await;
        Ok(inner
            .sessions
            .values()
            .filter(|s| s.workspace_id == workspace_id)
            .max_by_key(|s| s.created_at)
            .cloned())
    }
}

#[async_trait]
impl ExecutionStore for MemoryStore {
    async fn upsert_execution(&self, execution: ExecutionRecord) -> StoreResult<()> {
        self.inner
            .write()
            .await
            .executions
            .insert(execution.id, execution);
        Ok(())
    }

    async fn get_execution(&self, process_id: ProcessId) -> StoreResult<Option<ExecutionRecord>> {
        Ok(self.inner.read().await.executions.get(&process_id).cloned())
    }

    async fn update_execution_stage_status(
        &self,
        process_id: ProcessId,
        stage: Option<aura_contracts::ExecutionStage>,
        status: aura_contracts::ExecutionStatus,
        last_error: Option<String>,
    ) -> StoreResult<()> {
        let mut inner = self.inner.write().await;
        let record = inner
            .executions
            .get_mut(&process_id)
            .ok_or(StoreError::NotFound("execution"))?;
        record.stage = stage;
        record.status = status;
        record.last_error = last_error;
        record.updated_at = Utc::now();
        if matches!(
            status,
            ExecutionStatus::Completed | ExecutionStatus::Failed | ExecutionStatus::Killed
        ) {
            record.completed_at = Some(Utc::now());
        }
        Ok(())
    }

    async fn list_executions_by_session(
        &self,
        session_id: SessionId,
    ) -> StoreResult<Vec<ExecutionRecord>> {
        let inner = self.inner.read().await;
        let mut result = inner
            .executions
            .values()
            .filter(|e| e.session_id == session_id)
            .cloned()
            .collect::<Vec<_>>();
        result.sort_by_key(|e| e.started_at);
        Ok(result)
    }

    async fn find_running_non_dev_by_workspace(
        &self,
        workspace_id: WorkspaceId,
    ) -> StoreResult<Vec<ExecutionRecord>> {
        let inner = self.inner.read().await;
        let result = inner
            .executions
            .values()
            .filter(|e| {
                e.workspace_id == workspace_id
                    && e.status == ExecutionStatus::Running
                    && e.run_reason != aura_contracts::RunReason::DevServer
            })
            .cloned()
            .collect::<Vec<_>>();
        Ok(result)
    }
}

#[async_trait]
impl BoardRulesStore for MemoryStore {
    async fn set_board_rules(&self, board_rules: BoardRulesRecord) -> StoreResult<()> {
        self.inner.write().await.board_rules = Some(board_rules);
        Ok(())
    }

    async fn get_board_rules(&self) -> StoreResult<BoardRulesRecord> {
        self.inner
            .read()
            .await
            .board_rules
            .clone()
            .ok_or(StoreError::NotFound("board_rules"))
    }
}

#[async_trait]
impl PromptTemplateStore for MemoryStore {
    async fn upsert_prompt_template(&self, template: PromptTemplateRecord) -> StoreResult<()> {
        let key = (template.scope.clone(), template.stage);
        self.inner.write().await.templates.insert(key, template);
        Ok(())
    }

    async fn get_prompt_template(
        &self,
        scope: &PromptScope,
        stage: aura_contracts::ExecutionStage,
    ) -> StoreResult<Option<PromptTemplateRecord>> {
        Ok(self
            .inner
            .read()
            .await
            .templates
            .get(&(scope.clone(), stage))
            .cloned())
    }

    async fn list_prompt_templates(&self) -> StoreResult<Vec<PromptTemplateRecord>> {
        Ok(self
            .inner
            .read()
            .await
            .templates
            .values()
            .cloned()
            .collect())
    }
}

#[async_trait]
impl OrchestratorStore for MemoryStore {
    async fn upsert_orch_task(&self, task: OrchTaskRecord) -> StoreResult<()> {
        self.inner.write().await.orch_tasks.insert(task.id, task);
        Ok(())
    }

    async fn get_orch_task(&self, task_id: OrchTaskId) -> StoreResult<Option<OrchTaskRecord>> {
        Ok(self.inner.read().await.orch_tasks.get(&task_id).cloned())
    }

    async fn list_orch_tasks(&self) -> StoreResult<Vec<OrchTaskRecord>> {
        let mut tasks = self
            .inner
            .read()
            .await
            .orch_tasks
            .values()
            .cloned()
            .collect::<Vec<_>>();
        tasks.sort_by_key(|task| task.created_at);
        Ok(tasks)
    }

    async fn set_orch_task_status(
        &self,
        task_id: OrchTaskId,
        status: OrchestratorTaskStatus,
    ) -> StoreResult<()> {
        let mut inner = self.inner.write().await;
        let task = inner
            .orch_tasks
            .get_mut(&task_id)
            .ok_or(StoreError::NotFound("orch_task"))?;
        task.status = status;
        task.updated_at = Utc::now();
        Ok(())
    }

    async fn replace_orch_task_dependencies(
        &self,
        task_id: OrchTaskId,
        dependencies: Vec<OrchTaskDependencyRecord>,
    ) -> StoreResult<()> {
        self.inner
            .write()
            .await
            .orch_task_dependencies
            .insert(task_id, dependencies);
        Ok(())
    }

    async fn list_orch_task_dependencies(
        &self,
        task_id: OrchTaskId,
    ) -> StoreResult<Vec<OrchTaskDependencyRecord>> {
        Ok(self
            .inner
            .read()
            .await
            .orch_task_dependencies
            .get(&task_id)
            .cloned()
            .unwrap_or_default())
    }

    async fn upsert_orch_attempt(&self, attempt: OrchAttemptRecord) -> StoreResult<()> {
        self.inner
            .write()
            .await
            .orch_attempts
            .insert(attempt.id, attempt);
        Ok(())
    }

    async fn get_orch_attempt(
        &self,
        attempt_id: OrchAttemptId,
    ) -> StoreResult<Option<OrchAttemptRecord>> {
        Ok(self
            .inner
            .read()
            .await
            .orch_attempts
            .get(&attempt_id)
            .cloned())
    }

    async fn list_orch_attempts_by_task(
        &self,
        task_id: OrchTaskId,
    ) -> StoreResult<Vec<OrchAttemptRecord>> {
        let mut attempts = self
            .inner
            .read()
            .await
            .orch_attempts
            .values()
            .filter(|attempt| attempt.task_id == task_id)
            .cloned()
            .collect::<Vec<_>>();
        attempts.sort_by_key(|attempt| attempt.started_at);
        Ok(attempts)
    }

    async fn replace_orch_gates(
        &self,
        task_id: OrchTaskId,
        attempt_id: OrchAttemptId,
        gates: Vec<OrchGateRecord>,
    ) -> StoreResult<()> {
        if gates
            .iter()
            .any(|gate| gate.task_id != task_id || gate.attempt_id != attempt_id)
        {
            return Err(StoreError::Validation(
                "gate task/attempt mismatch".to_string(),
            ));
        }
        self.inner
            .write()
            .await
            .orch_gates
            .insert(attempt_id, gates);
        Ok(())
    }

    async fn list_orch_gates_by_attempt(
        &self,
        attempt_id: OrchAttemptId,
    ) -> StoreResult<Vec<OrchGateRecord>> {
        Ok(self
            .inner
            .read()
            .await
            .orch_gates
            .get(&attempt_id)
            .cloned()
            .unwrap_or_default())
    }

    async fn append_orch_event(&self, event: OrchEventRecord) -> StoreResult<()> {
        self.inner.write().await.orch_events.push(event);
        Ok(())
    }

    async fn list_orch_events_by_task(
        &self,
        task_id: OrchTaskId,
    ) -> StoreResult<Vec<OrchEventRecord>> {
        let mut events = self
            .inner
            .read()
            .await
            .orch_events
            .iter()
            .filter(|event| event.task_id == task_id)
            .cloned()
            .collect::<Vec<_>>();
        events.sort_by_key(|event| event.created_at);
        Ok(events)
    }
}

#[async_trait]
impl AuraStore for MemoryStore {
    async fn begin_tx(&self) -> StoreResult<Box<dyn StoreTx>> {
        Ok(Box::<MemoryTx>::default())
    }
}
