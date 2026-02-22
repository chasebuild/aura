use async_trait::async_trait;
use aura_contracts::{ExecutionAction, ProcessId};
use aura_worker_protocol::{
    InterruptExecutionRequest, StartExecutionRequest, StartExecutionResponse,
};
use thiserror::Error;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RunPath {
    Local,
    Remote { worker_id: String },
}

#[derive(Debug, Clone)]
pub struct RunStartRequest {
    pub process_id: ProcessId,
    pub action: ExecutionAction,
    pub cwd: Option<String>,
    pub env: std::collections::HashMap<String, String>,
}

#[derive(Debug, Clone)]
pub struct RunHandle {
    pub process_id: ProcessId,
    pub path: RunPath,
}

#[derive(Debug, Error)]
pub enum RunCoordinatorError {
    #[error("local runner error: {0}")]
    Local(String),
    #[error("remote runner error: {0}")]
    Remote(String),
    #[error("remote execution rejected: {0}")]
    Rejected(String),
}

#[async_trait]
pub trait LocalRunner: Send + Sync {
    async fn start_local(&self, request: &RunStartRequest) -> Result<(), String>;
    async fn interrupt_local(&self, process_id: ProcessId) -> Result<(), String>;
}

#[async_trait]
pub trait RemoteRunner: Send + Sync {
    async fn start_remote(
        &self,
        worker_id: &str,
        request: StartExecutionRequest,
    ) -> Result<StartExecutionResponse, String>;
    async fn interrupt_remote(
        &self,
        worker_id: &str,
        request: InterruptExecutionRequest,
    ) -> Result<(), String>;
}

pub struct RunCoordinator<L, R> {
    local: L,
    remote: R,
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Mutex};

    #[derive(Default)]
    struct MockLocal {
        started: Arc<Mutex<Vec<ProcessId>>>,
        interrupted: Arc<Mutex<Vec<ProcessId>>>,
    }

    #[async_trait]
    impl LocalRunner for MockLocal {
        async fn start_local(&self, request: &RunStartRequest) -> Result<(), String> {
            self.started.lock().expect("lock").push(request.process_id);
            Ok(())
        }

        async fn interrupt_local(&self, process_id: ProcessId) -> Result<(), String> {
            self.interrupted.lock().expect("lock").push(process_id);
            Ok(())
        }
    }

    #[derive(Default)]
    struct MockRemote {
        started: Arc<Mutex<Vec<ProcessId>>>,
        interrupted: Arc<Mutex<Vec<ProcessId>>>,
    }

    #[async_trait]
    impl RemoteRunner for MockRemote {
        async fn start_remote(
            &self,
            _worker_id: &str,
            request: StartExecutionRequest,
        ) -> Result<StartExecutionResponse, String> {
            self.started
                .lock()
                .expect("lock")
                .push(request.execution_id);
            Ok(StartExecutionResponse {
                accepted: true,
                reason: None,
            })
        }

        async fn interrupt_remote(
            &self,
            _worker_id: &str,
            request: InterruptExecutionRequest,
        ) -> Result<(), String> {
            self.interrupted
                .lock()
                .expect("lock")
                .push(request.execution_id);
            Ok(())
        }
    }

    #[tokio::test]
    async fn remote_start_and_interrupt_round_trip() {
        let local = MockLocal::default();
        let remote = MockRemote::default();
        let coordinator = RunCoordinator::new(local, remote);
        let process_id = ProcessId::new();

        let handle = coordinator
            .start(
                RunPath::Remote {
                    worker_id: "worker-a".to_string(),
                },
                RunStartRequest {
                    process_id,
                    action: ExecutionAction::RunCodingInitial,
                    cwd: None,
                    env: std::collections::HashMap::new(),
                },
            )
            .await
            .expect("start");

        coordinator.interrupt(&handle).await.expect("interrupt");
    }
}

impl<L, R> RunCoordinator<L, R>
where
    L: LocalRunner,
    R: RemoteRunner,
{
    pub fn new(local: L, remote: R) -> Self {
        Self { local, remote }
    }

    pub async fn start(
        &self,
        path: RunPath,
        request: RunStartRequest,
    ) -> Result<RunHandle, RunCoordinatorError> {
        match &path {
            RunPath::Local => self
                .local
                .start_local(&request)
                .await
                .map_err(RunCoordinatorError::Local)?,
            RunPath::Remote { worker_id } => {
                let response = self
                    .remote
                    .start_remote(
                        worker_id,
                        StartExecutionRequest {
                            execution_id: request.process_id,
                            action: request.action.clone(),
                            cwd: request.cwd.clone(),
                            env: request.env.clone(),
                        },
                    )
                    .await
                    .map_err(RunCoordinatorError::Remote)?;
                if !response.accepted {
                    return Err(RunCoordinatorError::Rejected(
                        response.reason.unwrap_or_else(|| "unknown".to_string()),
                    ));
                }
            }
        }

        Ok(RunHandle {
            process_id: request.process_id,
            path,
        })
    }

    pub async fn interrupt(&self, handle: &RunHandle) -> Result<(), RunCoordinatorError> {
        match &handle.path {
            RunPath::Local => self
                .local
                .interrupt_local(handle.process_id)
                .await
                .map_err(RunCoordinatorError::Local),
            RunPath::Remote { worker_id } => self
                .remote
                .interrupt_remote(
                    worker_id,
                    InterruptExecutionRequest {
                        execution_id: handle.process_id,
                    },
                )
                .await
                .map_err(RunCoordinatorError::Remote),
        }
    }
}
