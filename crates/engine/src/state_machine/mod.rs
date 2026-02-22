use aura_contracts::{
    CanonicalTaskState, ExecutionAction, ExecutionEvent, ExecutionStage, ExecutionStatus,
    MachineState,
};
use thiserror::Error;

#[derive(Debug, Error)]
pub enum StateMachineError {
    #[error("invalid transition from stage {stage:?} status {status:?} on event {event:?}")]
    InvalidTransition {
        stage: Option<ExecutionStage>,
        status: ExecutionStatus,
        event: ExecutionEvent,
    },
}

#[derive(Debug, Clone)]
pub struct Transition {
    pub state: MachineState,
    pub actions: Vec<ExecutionAction>,
}

#[derive(Debug, Default, Clone)]
pub struct EngineStateMachine;

impl EngineStateMachine {
    pub fn apply(
        &self,
        mut state: MachineState,
        event: ExecutionEvent,
    ) -> Result<Transition, StateMachineError> {
        let mut actions = vec![ExecutionAction::PersistEvent];

        match event.clone() {
            ExecutionEvent::StartRequested
                if state.status == ExecutionStatus::Queued && state.stage.is_none() =>
            {
                state.status = ExecutionStatus::Running;
                state.stage = Some(ExecutionStage::PrepareWorkspace);
                state.task_state = CanonicalTaskState::Running;
                actions.push(ExecutionAction::MarkTaskState {
                    state: CanonicalTaskState::Running,
                });
                actions.push(ExecutionAction::PrepareWorkspace);
            }
            ExecutionEvent::WorkspacePrepared
                if state.status == ExecutionStatus::Running
                    && state.stage == Some(ExecutionStage::PrepareWorkspace) =>
            {
                state.stage = Some(ExecutionStage::Setup);
                actions.push(ExecutionAction::RunSetup);
            }
            ExecutionEvent::SetupCompleted
                if state.status == ExecutionStatus::Running
                    && state.stage == Some(ExecutionStage::Setup) =>
            {
                state.stage = Some(ExecutionStage::CodingInitial);
                actions.push(ExecutionAction::RunCodingInitial);
            }
            ExecutionEvent::CodingCompleted { changes_made }
                if state.status == ExecutionStatus::Running
                    && (state.stage == Some(ExecutionStage::CodingInitial)
                        || state.stage == Some(ExecutionStage::CodingFollowUp)) =>
            {
                if changes_made {
                    state.stage = Some(ExecutionStage::Cleanup);
                    actions.push(ExecutionAction::RunCleanup);
                } else {
                    state.stage = Some(ExecutionStage::Review);
                    actions.push(ExecutionAction::RunReview);
                }
            }
            ExecutionEvent::CleanupCompleted
                if state.status == ExecutionStatus::Running
                    && state.stage == Some(ExecutionStage::Cleanup) =>
            {
                state.stage = Some(ExecutionStage::Review);
                actions.push(ExecutionAction::RunReview);
            }
            ExecutionEvent::ReviewCompleted
                if state.status == ExecutionStatus::Running
                    && state.stage == Some(ExecutionStage::Review) =>
            {
                state.status = ExecutionStatus::Completed;
                state.task_state = CanonicalTaskState::Done;
                actions.push(ExecutionAction::MarkTaskState {
                    state: CanonicalTaskState::Done,
                });
                actions.push(ExecutionAction::CompleteExecution);
            }
            ExecutionEvent::FollowUpRequested
                if state.stage == Some(ExecutionStage::Review)
                    && (state.status == ExecutionStatus::Running
                        || state.status == ExecutionStatus::Completed) =>
            {
                state.status = ExecutionStatus::Running;
                state.task_state = CanonicalTaskState::Running;
                state.stage = Some(ExecutionStage::CodingFollowUp);
                actions.push(ExecutionAction::MarkTaskState {
                    state: CanonicalTaskState::Running,
                });
                actions.push(ExecutionAction::RunCodingFollowUp);
            }
            ExecutionEvent::StageFailed { .. } if state.status == ExecutionStatus::Running => {
                state.status = ExecutionStatus::Failed;
                state.task_state = CanonicalTaskState::Blocked;
                actions.push(ExecutionAction::MarkTaskState {
                    state: CanonicalTaskState::Blocked,
                });
            }
            ExecutionEvent::WorkerLost if state.status == ExecutionStatus::Running => {
                state.status = ExecutionStatus::Failed;
                state.task_state = CanonicalTaskState::Blocked;
                actions.push(ExecutionAction::FailAsWorkerLost);
                actions.push(ExecutionAction::MarkTaskState {
                    state: CanonicalTaskState::Blocked,
                });
            }
            ExecutionEvent::CancelRequested
                if state.status == ExecutionStatus::Queued
                    || state.status == ExecutionStatus::Running =>
            {
                state.status = ExecutionStatus::Killed;
                state.task_state = CanonicalTaskState::Cancelled;
                actions.push(ExecutionAction::InterruptExecution);
                actions.push(ExecutionAction::MarkTaskState {
                    state: CanonicalTaskState::Cancelled,
                });
            }
            ExecutionEvent::Interrupted if state.status == ExecutionStatus::Running => {
                state.status = ExecutionStatus::Killed;
                state.task_state = CanonicalTaskState::Cancelled;
                actions.push(ExecutionAction::MarkTaskState {
                    state: CanonicalTaskState::Cancelled,
                });
            }
            _ => {
                return Err(StateMachineError::InvalidTransition {
                    stage: state.stage,
                    status: state.status,
                    event,
                });
            }
        }

        Ok(Transition { state, actions })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use aura_contracts::{ProcessId, TaskId, WorkspaceId};

    fn base_state() -> MachineState {
        MachineState {
            process_id: ProcessId::new(),
            workspace_id: WorkspaceId::new(),
            task_id: TaskId::new(),
            stage: None,
            status: ExecutionStatus::Queued,
            task_state: CanonicalTaskState::Ready,
        }
    }

    #[test]
    fn cleanup_skipped_when_no_changes() {
        let sm = EngineStateMachine;
        let s = sm
            .apply(base_state(), ExecutionEvent::StartRequested)
            .expect("start")
            .state;
        let s = sm
            .apply(s, ExecutionEvent::WorkspacePrepared)
            .expect("prep")
            .state;
        let s = sm
            .apply(s, ExecutionEvent::SetupCompleted)
            .expect("setup")
            .state;

        let out = sm
            .apply(
                s,
                ExecutionEvent::CodingCompleted {
                    changes_made: false,
                },
            )
            .expect("coding");
        assert_eq!(out.state.stage, Some(ExecutionStage::Review));
    }
}
