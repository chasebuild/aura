use std::sync::Arc;

use aura_contracts::{
    BoardColumnId, CanonicalTaskState, ExecutionEvent, MachineState, ProcessId, RunReason,
};
use aura_store::{AuraStore, StoreError};
use thiserror::Error;

use crate::state_machine::{EngineStateMachine, StateMachineError, Transition};

#[derive(Debug, Error)]
pub enum OrchestratorError {
    #[error("store error: {0}")]
    Store(String),
    #[error("state machine error: {0}")]
    StateMachine(#[from] StateMachineError),
    #[error("running execution already exists for workspace")]
    RunningExecutionExists,
    #[error("board transition forbidden: {0}")]
    BoardTransitionForbidden(String),
}

pub struct EngineOrchestrator<S: AuraStore> {
    store: Arc<S>,
    machine: EngineStateMachine,
}

impl<S: AuraStore> EngineOrchestrator<S> {
    pub fn new(store: Arc<S>) -> Self {
        Self {
            store,
            machine: EngineStateMachine,
        }
    }

    pub async fn ensure_can_start(&self, process_id: ProcessId) -> Result<(), OrchestratorError> {
        let execution = self
            .store
            .get_execution(process_id)
            .await
            .map_err(|e| OrchestratorError::Store(e.to_string()))?
            .ok_or_else(|| OrchestratorError::Store("execution not found".to_string()))?;

        let running = self
            .store
            .find_running_non_dev_by_workspace(execution.workspace_id)
            .await
            .map_err(|e| OrchestratorError::Store(e.to_string()))?;

        if running.iter().any(|p| p.id != process_id) {
            return Err(OrchestratorError::RunningExecutionExists);
        }

        Ok(())
    }

    pub async fn start_execution(
        &self,
        process_id: ProcessId,
    ) -> Result<Transition, OrchestratorError> {
        self.ensure_can_start(process_id).await?;
        self.apply_event(process_id, ExecutionEvent::StartRequested)
            .await
    }

    pub async fn apply_event(
        &self,
        process_id: ProcessId,
        event: ExecutionEvent,
    ) -> Result<Transition, OrchestratorError> {
        let execution = self
            .store
            .get_execution(process_id)
            .await
            .map_err(|e| OrchestratorError::Store(e.to_string()))?
            .ok_or_else(|| OrchestratorError::Store("execution not found".to_string()))?;

        let mut transition = self.machine.apply(
            MachineState {
                process_id: execution.id,
                workspace_id: execution.workspace_id,
                task_id: execution.task_id,
                stage: execution.stage,
                status: execution.status,
                task_state: self
                    .store
                    .get_task(execution.task_id)
                    .await
                    .map_err(|e| OrchestratorError::Store(e.to_string()))?
                    .map(|t| t.canonical_state)
                    .unwrap_or(CanonicalTaskState::Ready),
            },
            event.clone(),
        )?;

        let error_text = match event {
            ExecutionEvent::StageFailed { message } => Some(message),
            ExecutionEvent::WorkerLost => Some("worker lost".to_string()),
            _ => None,
        };

        self.store
            .update_execution_stage_status(
                process_id,
                transition.state.stage,
                transition.state.status,
                error_text,
            )
            .await
            .map_err(|e| OrchestratorError::Store(e.to_string()))?;

        self.store
            .update_task_state(
                transition.state.task_id,
                transition.state.task_state.clone(),
                None,
            )
            .await
            .map_err(|e| OrchestratorError::Store(e.to_string()))?;

        transition
            .actions
            .retain(|action| !matches!(action, aura_contracts::ExecutionAction::Noop));

        Ok(transition)
    }

    pub async fn can_transition_board_column(
        &self,
        task_id: aura_contracts::TaskId,
        from_column_id: &BoardColumnId,
        to_column_id: &BoardColumnId,
        is_clean_git: bool,
    ) -> Result<(), OrchestratorError> {
        let board_rules = self
            .store
            .get_board_rules()
            .await
            .map_err(|e| OrchestratorError::Store(e.to_string()))?;

        let rule = board_rules
            .transition_rules
            .iter()
            .find(|r| &r.from_column_id == from_column_id && &r.to_column_id == to_column_id)
            .ok_or_else(|| {
                OrchestratorError::BoardTransitionForbidden("no matching rule".to_string())
            })?;

        if rule.requires_clean_git && !is_clean_git {
            return Err(OrchestratorError::BoardTransitionForbidden(
                "clean git required".to_string(),
            ));
        }

        if rule.requires_no_running_process {
            let workspaces = self
                .store
                .list_workspaces_by_task(task_id)
                .await
                .map_err(|e| OrchestratorError::Store(e.to_string()))?;
            for workspace in workspaces {
                let running = self
                    .store
                    .find_running_non_dev_by_workspace(workspace.id)
                    .await
                    .map_err(|e| OrchestratorError::Store(e.to_string()))?;
                if !running.is_empty() {
                    return Err(OrchestratorError::BoardTransitionForbidden(
                        "running execution exists".to_string(),
                    ));
                }
            }
        }

        Ok(())
    }

    pub async fn apply_board_transition(
        &self,
        task_id: aura_contracts::TaskId,
        to_column_id: BoardColumnId,
    ) -> Result<(), OrchestratorError> {
        let board_rules = self
            .store
            .get_board_rules()
            .await
            .map_err(|e| OrchestratorError::Store(e.to_string()))?;

        let target_col = board_rules
            .columns
            .iter()
            .find(|c| c.id == to_column_id)
            .ok_or_else(|| {
                OrchestratorError::BoardTransitionForbidden("unknown column".to_string())
            })?;

        self.store
            .update_task_state(
                task_id,
                target_col.canonical_state.clone(),
                Some(target_col.id.clone()),
            )
            .await
            .map_err(|e| OrchestratorError::Store(e.to_string()))
    }

    pub fn derive_run_reason(stage: aura_contracts::ExecutionStage) -> RunReason {
        match stage {
            aura_contracts::ExecutionStage::PrepareWorkspace => RunReason::Setup,
            aura_contracts::ExecutionStage::Setup => RunReason::Setup,
            aura_contracts::ExecutionStage::CodingInitial => RunReason::Coding,
            aura_contracts::ExecutionStage::CodingFollowUp => RunReason::Coding,
            aura_contracts::ExecutionStage::Review => RunReason::Review,
            aura_contracts::ExecutionStage::Cleanup => RunReason::Cleanup,
        }
    }
}

impl From<StoreError> for OrchestratorError {
    fn from(value: StoreError) -> Self {
        Self::Store(value.to_string())
    }
}
