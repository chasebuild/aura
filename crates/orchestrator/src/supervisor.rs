use std::collections::HashMap;
use std::fs::OpenOptions;
use std::path::PathBuf;
use std::sync::Arc;

use aura_contracts::{
    ExecutionEvent, ExecutionStage, GateStatus, OrchAttemptId, OrchTaskId,
    OrchestratorAttemptStatus, OrchestratorTaskStatus, ProcessId,
};
use aura_engine::state_machine::EngineStateMachine;
use aura_notify_contracts::{NotificationEvent, NotificationMessage, NotificationSink};
use aura_store::{
    OrchAttemptRecord, OrchEventRecord, OrchGateRecord, OrchTaskDependencyRecord, OrchTaskRecord,
    OrchestratorStore,
};
use chrono::Utc;

use crate::{
    AgentRouter, ConfigError, ContextProvider, GitHostProvider, OrchestratorConfig, PlannerError,
    RetryPolicy, SubmitRequest, parse_planner_output, select_runnable_tasks,
};

pub type DefaultSupervisor = Supervisor<
    aura_store::sqlite::SqliteStore,
    crate::providers::DefaultAgentRouter,
    crate::providers::GhGitHostProvider,
    crate::retry::AdaptiveRetryPolicy,
    crate::providers::PassthroughContextProvider,
    aura_notify_contracts::NoopNotificationSink,
>;

pub async fn build_default_supervisor(
    cwd: &std::path::Path,
) -> Result<DefaultSupervisor, SupervisorError> {
    let config = OrchestratorConfig::load(cwd)?;
    let store = aura_store::sqlite::SqliteStore::connect(
        aura_store::sqlite::SqliteStoreConfig::new(config.database_path.clone()),
    )
    .await
    .map_err(SupervisorError::Store)?;

    Ok(Supervisor::new(
        Arc::new(store),
        crate::providers::DefaultAgentRouter,
        crate::providers::GhGitHostProvider,
        crate::retry::AdaptiveRetryPolicy {
            max_attempts: config.max_attempts,
        },
        crate::providers::PassthroughContextProvider,
        aura_notify_contracts::NoopNotificationSink,
        config,
    ))
}

#[derive(Debug, Clone, Default, serde::Serialize)]
pub struct TickSummary {
    pub processed: usize,
    pub retried: usize,
    pub ready_for_review: usize,
    pub needs_human: usize,
    pub skipped_due_to_lock: bool,
}

pub struct Supervisor<S, R, G, P, C, N> {
    store: Arc<S>,
    router: R,
    git_host: G,
    retry_policy: P,
    context_provider: C,
    notifier: N,
    config: OrchestratorConfig,
}

impl<S, R, G, P, C, N> Supervisor<S, R, G, P, C, N>
where
    S: OrchestratorStore + Send + Sync,
    R: AgentRouter,
    G: GitHostProvider,
    P: RetryPolicy,
    C: ContextProvider,
    N: NotificationSink,
{
    pub fn new(
        store: Arc<S>,
        router: R,
        git_host: G,
        retry_policy: P,
        context_provider: C,
        notifier: N,
        config: OrchestratorConfig,
    ) -> Self {
        Self {
            store,
            router,
            git_host,
            retry_policy,
            context_provider,
            notifier,
            config,
        }
    }

    pub fn config(&self) -> &OrchestratorConfig {
        &self.config
    }

    pub async fn submit(&self, request: SubmitRequest) -> Result<OrchTaskId, SupervisorError> {
        let now = Utc::now();
        let task_id = OrchTaskId::new();

        self.store
            .upsert_orch_task(OrchTaskRecord {
                id: task_id,
                title: request.title,
                intent: request.intent,
                status: OrchestratorTaskStatus::Pending,
                priority: request.priority,
                planner_enabled: request.planner_enabled,
                context_json: request.context_json,
                created_at: now,
                updated_at: now,
            })
            .await
            .map_err(SupervisorError::Store)?;

        let deps = request
            .dependencies
            .iter()
            .copied()
            .map(|dep| OrchTaskDependencyRecord {
                task_id,
                depends_on_task_id: dep,
            })
            .collect::<Vec<_>>();
        self.store
            .replace_orch_task_dependencies(task_id, deps)
            .await
            .map_err(SupervisorError::Store)?;

        self.store
            .append_orch_event(OrchEventRecord {
                task_id,
                attempt_id: None,
                event_type: "task_submitted".to_string(),
                payload_json: serde_json::json!({"planner_enabled": request.planner_enabled}),
                created_at: now,
            })
            .await
            .map_err(SupervisorError::Store)?;

        Ok(task_id)
    }

    pub async fn retry_task(&self, task_id: OrchTaskId) -> Result<(), SupervisorError> {
        self.store
            .set_orch_task_status(task_id, OrchestratorTaskStatus::Pending)
            .await
            .map_err(SupervisorError::Store)?;

        self.store
            .append_orch_event(OrchEventRecord {
                task_id,
                attempt_id: None,
                event_type: "manual_retry_requested".to_string(),
                payload_json: serde_json::json!({}),
                created_at: Utc::now(),
            })
            .await
            .map_err(SupervisorError::Store)
    }

    pub async fn status_json(&self) -> Result<serde_json::Value, SupervisorError> {
        let mut tasks_json = Vec::new();
        let tasks = self
            .store
            .list_orch_tasks()
            .await
            .map_err(SupervisorError::Store)?;

        for task in tasks {
            let attempts = self
                .store
                .list_orch_attempts_by_task(task.id)
                .await
                .map_err(SupervisorError::Store)?;
            let deps = self
                .store
                .list_orch_task_dependencies(task.id)
                .await
                .map_err(SupervisorError::Store)?;
            let events = self
                .store
                .list_orch_events_by_task(task.id)
                .await
                .map_err(SupervisorError::Store)?;

            let mut attempt_json = Vec::new();
            for attempt in attempts {
                let gates = self
                    .store
                    .list_orch_gates_by_attempt(attempt.id)
                    .await
                    .map_err(SupervisorError::Store)?;
                attempt_json.push(serde_json::json!({
                    "attempt": attempt,
                    "gates": gates,
                }));
            }

            tasks_json.push(serde_json::json!({
                "task": task,
                "dependencies": deps,
                "attempts": attempt_json,
                "events": events,
            }));
        }

        Ok(serde_json::json!({"tasks": tasks_json}))
    }

    pub async fn tick(&self) -> Result<TickSummary, SupervisorError> {
        let lock = match TickLockGuard::acquire(self.config.tick_lock_path.clone())? {
            Some(lock) => lock,
            None => {
                return Ok(TickSummary {
                    skipped_due_to_lock: true,
                    ..TickSummary::default()
                });
            }
        };

        let mut summary = TickSummary::default();
        let tasks = self
            .store
            .list_orch_tasks()
            .await
            .map_err(SupervisorError::Store)?;

        let running = tasks
            .iter()
            .filter(|task| task.status == OrchestratorTaskStatus::Running)
            .count();
        if running >= self.config.max_concurrency {
            drop(lock);
            return Ok(summary);
        }
        let available = self.config.max_concurrency.saturating_sub(running);

        let mut dep_map: HashMap<OrchTaskId, Vec<OrchTaskDependencyRecord>> = HashMap::new();
        for task in &tasks {
            dep_map.insert(
                task.id,
                self.store
                    .list_orch_task_dependencies(task.id)
                    .await
                    .map_err(SupervisorError::Store)?,
            );
        }

        let runnable = select_runnable_tasks(&tasks, &dep_map, available);
        for task in runnable {
            summary.processed += 1;
            let outcome = self.run_task(task).await?;
            summary.retried += usize::from(outcome == TaskOutcome::Retried);
            summary.ready_for_review += usize::from(outcome == TaskOutcome::ReadyForReview);
            summary.needs_human += usize::from(outcome == TaskOutcome::NeedsHuman);
        }

        drop(lock);
        Ok(summary)
    }

    pub async fn daemon(&self) -> Result<(), SupervisorError> {
        loop {
            if let Err(error) = self.tick().await {
                let _ = self
                    .notifier
                    .notify(NotificationMessage {
                        event: NotificationEvent::DaemonUnhealthy,
                        task_id: "daemon".to_string(),
                        title: "AURA orchestrator daemon unhealthy".to_string(),
                        body: error.to_string(),
                        metadata: serde_json::json!({}),
                        created_at: Utc::now(),
                    })
                    .await;
            }

            let sleep = tokio::time::sleep(std::time::Duration::from_secs(
                self.config.tick_interval_secs,
            ));
            tokio::pin!(sleep);

            tokio::select! {
                _ = &mut sleep => {}
                _ = tokio::signal::ctrl_c() => {
                    return Ok(());
                }
            }
        }
    }

    async fn run_task(&self, task: OrchTaskRecord) -> Result<TaskOutcome, SupervisorError> {
        self.store
            .set_orch_task_status(task.id, OrchestratorTaskStatus::Running)
            .await
            .map_err(SupervisorError::Store)?;

        let attempts_before = self
            .store
            .list_orch_attempts_by_task(task.id)
            .await
            .map_err(SupervisorError::Store)?;
        let attempt_idx = attempts_before.len();

        let route = self.router.route(&task);
        let process_id = ProcessId::new();
        let attempt_id = OrchAttemptId::new();
        let now = Utc::now();

        if task.planner_enabled && attempt_idx == 0 {
            self.evaluate_planner_hint(&task).await?;
        }

        let enriched_context = self
            .context_provider
            .enrich_context(&task, &task.context_json)
            .await
            .map_err(SupervisorError::Provider)?;

        let base_prompt = format!(
            "Task: {}\nIntent: {}\nContext: {}",
            task.title, task.intent, enriched_context
        );

        self.store
            .upsert_orch_attempt(OrchAttemptRecord {
                id: attempt_id,
                task_id: task.id,
                process_id: Some(process_id),
                executor_profile: route.executor_profile,
                prompt_snapshot: base_prompt.clone(),
                status: OrchestratorAttemptStatus::Running,
                retry_index: attempt_idx as i32,
                started_at: now,
                completed_at: None,
            })
            .await
            .map_err(SupervisorError::Store)?;

        // Reuse existing deterministic engine state machine for staged progression.
        let worker_failed = task
            .context_json
            .get("simulate_worker_failure")
            .and_then(serde_json::Value::as_bool)
            .unwrap_or(false);

        let engine_ok = drive_engine_state_machine(!worker_failed);

        let mut gate_records = Vec::new();
        if self.config.strict_done_gate {
            let attempt = self
                .store
                .get_orch_attempt(attempt_id)
                .await
                .map_err(SupervisorError::Store)?
                .ok_or(SupervisorError::Store(aura_store::StoreError::NotFound(
                    "orch_attempt",
                )))?;

            let evaluations = self
                .git_host
                .evaluate_gates(&task, &attempt, &self.config)
                .await
                .map_err(SupervisorError::Provider)?;

            gate_records = evaluations
                .into_iter()
                .map(|gate| OrchGateRecord {
                    task_id: task.id,
                    attempt_id,
                    gate_name: gate.gate_name,
                    status: gate.status,
                    details_json: gate.details_json,
                    checked_at: Utc::now(),
                })
                .collect::<Vec<_>>();
        }

        if !engine_ok {
            gate_records.push(OrchGateRecord {
                task_id: task.id,
                attempt_id,
                gate_name: "worker_execution".to_string(),
                status: GateStatus::Failed,
                details_json: serde_json::json!({"reason":"simulated worker failure"}),
                checked_at: Utc::now(),
            });
        }

        self.store
            .replace_orch_gates(task.id, attempt_id, gate_records.clone())
            .await
            .map_err(SupervisorError::Store)?;

        let all_pass = gate_records
            .iter()
            .all(|gate| gate.status == GateStatus::Passed);

        let outcome = if all_pass {
            self.store
                .upsert_orch_attempt(OrchAttemptRecord {
                    id: attempt_id,
                    task_id: task.id,
                    process_id: Some(process_id),
                    executor_profile: self.router.route(&task).executor_profile,
                    prompt_snapshot: base_prompt,
                    status: OrchestratorAttemptStatus::Succeeded,
                    retry_index: attempt_idx as i32,
                    started_at: now,
                    completed_at: Some(Utc::now()),
                })
                .await
                .map_err(SupervisorError::Store)?;
            self.store
                .set_orch_task_status(task.id, OrchestratorTaskStatus::ReadyForReview)
                .await
                .map_err(SupervisorError::Store)?;

            let _ = self
                .notifier
                .notify(NotificationMessage {
                    event: NotificationEvent::PrReadyForReview,
                    task_id: task.id.0.to_string(),
                    title: task.title.clone(),
                    body: "Task passed strict gates and is ready for review".to_string(),
                    metadata: serde_json::json!({"attempt_id": attempt_id.0.to_string()}),
                    created_at: Utc::now(),
                })
                .await;

            TaskOutcome::ReadyForReview
        } else {
            self.store
                .upsert_orch_attempt(OrchAttemptRecord {
                    id: attempt_id,
                    task_id: task.id,
                    process_id: Some(process_id),
                    executor_profile: self.router.route(&task).executor_profile,
                    prompt_snapshot: base_prompt,
                    status: OrchestratorAttemptStatus::Failed,
                    retry_index: attempt_idx as i32,
                    started_at: now,
                    completed_at: Some(Utc::now()),
                })
                .await
                .map_err(SupervisorError::Store)?;

            let attempt_count = attempt_idx + 1;
            let failed = gate_records
                .iter()
                .filter(|gate| gate.status == GateStatus::Failed)
                .cloned()
                .collect::<Vec<_>>();

            if self.retry_policy.should_retry(attempt_count) {
                let delta = self.retry_policy.build_retry_prompt_delta(&failed);
                self.store
                    .set_orch_task_status(task.id, OrchestratorTaskStatus::Pending)
                    .await
                    .map_err(SupervisorError::Store)?;
                self.store
                    .append_orch_event(OrchEventRecord {
                        task_id: task.id,
                        attempt_id: Some(attempt_id),
                        event_type: "retry_scheduled".to_string(),
                        payload_json: serde_json::json!({"attempt_count": attempt_count, "delta": delta}),
                        created_at: Utc::now(),
                    })
                    .await
                    .map_err(SupervisorError::Store)?;
                TaskOutcome::Retried
            } else {
                self.store
                    .set_orch_task_status(task.id, OrchestratorTaskStatus::NeedsHuman)
                    .await
                    .map_err(SupervisorError::Store)?;
                let _ = self
                    .notifier
                    .notify(NotificationMessage {
                        event: NotificationEvent::RetryExhausted,
                        task_id: task.id.0.to_string(),
                        title: task.title,
                        body: "Task exhausted retries and requires intervention".to_string(),
                        metadata: serde_json::json!({"attempt_count": attempt_count}),
                        created_at: Utc::now(),
                    })
                    .await;
                TaskOutcome::NeedsHuman
            }
        };

        Ok(outcome)
    }

    async fn evaluate_planner_hint(&self, task: &OrchTaskRecord) -> Result<(), SupervisorError> {
        let maybe_output = task
            .context_json
            .get("planner_output")
            .and_then(serde_json::Value::as_str);

        let Some(output) = maybe_output else {
            return Ok(());
        };

        match parse_planner_output(output) {
            Ok(plan) => {
                self.store
                    .append_orch_event(OrchEventRecord {
                        task_id: task.id,
                        attempt_id: None,
                        event_type: "planner_validated".to_string(),
                        payload_json: serde_json::json!({"subtask_count": plan.subtasks.len()}),
                        created_at: Utc::now(),
                    })
                    .await
                    .map_err(SupervisorError::Store)?;
            }
            Err(error) => {
                self.store
                    .append_orch_event(OrchEventRecord {
                        task_id: task.id,
                        attempt_id: None,
                        event_type: "planner_fallback_single_task".to_string(),
                        payload_json: serde_json::json!({"reason": error.to_string()}),
                        created_at: Utc::now(),
                    })
                    .await
                    .map_err(SupervisorError::Store)?;
            }
        }

        Ok(())
    }
}

fn drive_engine_state_machine(changes_made: bool) -> bool {
    let machine = EngineStateMachine;
    let state = aura_contracts::MachineState {
        process_id: ProcessId::new(),
        workspace_id: aura_contracts::WorkspaceId::new(),
        task_id: aura_contracts::TaskId::new(),
        stage: None,
        status: aura_contracts::ExecutionStatus::Queued,
        task_state: aura_contracts::CanonicalTaskState::Ready,
    };

    let Ok(state) = machine
        .apply(state, ExecutionEvent::StartRequested)
        .map(|t| t.state)
    else {
        return false;
    };
    let Ok(state) = machine
        .apply(state, ExecutionEvent::WorkspacePrepared)
        .map(|t| t.state)
    else {
        return false;
    };
    let Ok(state) = machine
        .apply(state, ExecutionEvent::SetupCompleted)
        .map(|t| t.state)
    else {
        return false;
    };
    let Ok(state) = machine
        .apply(state, ExecutionEvent::CodingCompleted { changes_made })
        .map(|t| t.state)
    else {
        return false;
    };

    if state.stage == Some(ExecutionStage::Cleanup) {
        let Ok(state) = machine
            .apply(state, ExecutionEvent::CleanupCompleted)
            .map(|t| t.state)
        else {
            return false;
        };
        return machine
            .apply(state, ExecutionEvent::ReviewCompleted)
            .is_ok();
    }

    machine
        .apply(state, ExecutionEvent::ReviewCompleted)
        .is_ok()
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TaskOutcome {
    ReadyForReview,
    Retried,
    NeedsHuman,
}

struct TickLockGuard {
    path: PathBuf,
}

impl TickLockGuard {
    fn acquire(path: PathBuf) -> Result<Option<Self>, SupervisorError> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(SupervisorError::Io)?;
        }

        match OpenOptions::new().create_new(true).write(true).open(&path) {
            Ok(mut file) => {
                use std::io::Write;
                file.write_all(b"lock").map_err(SupervisorError::Io)?;
                Ok(Some(Self { path }))
            }
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => Ok(None),
            Err(error) => Err(SupervisorError::Io(error)),
        }
    }
}

impl Drop for TickLockGuard {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}

#[derive(Debug, thiserror::Error)]
pub enum SupervisorError {
    #[error("store error: {0}")]
    Store(aura_store::StoreError),
    #[error("provider error: {0}")]
    Provider(crate::providers::ProviderError),
    #[error("config error: {0}")]
    Config(#[from] ConfigError),
    #[error("planner error: {0}")]
    Planner(#[from] PlannerError),
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
}

#[cfg(test)]
mod tests {
    use async_trait::async_trait;
    use aura_contracts::{
        GateStatus, OrchTaskId, OrchestratorTaskPriority, OrchestratorTaskStatus,
    };
    use aura_notify_contracts::NoopNotificationSink;
    use aura_store::memory::MemoryStore;

    use super::*;

    struct MockGitHost {
        pass: bool,
    }

    #[async_trait]
    impl GitHostProvider for MockGitHost {
        async fn evaluate_gates(
            &self,
            _task: &OrchTaskRecord,
            _attempt: &OrchAttemptRecord,
            _config: &OrchestratorConfig,
        ) -> Result<Vec<crate::providers::GateEvaluation>, crate::providers::ProviderError>
        {
            Ok(vec![crate::providers::GateEvaluation {
                gate_name: "ci_passed".to_string(),
                status: if self.pass {
                    GateStatus::Passed
                } else {
                    GateStatus::Failed
                },
                details_json: serde_json::json!({}),
            }])
        }
    }

    struct MockContext;
    #[async_trait]
    impl ContextProvider for MockContext {
        async fn enrich_context(
            &self,
            _task: &OrchTaskRecord,
            base_context: &serde_json::Value,
        ) -> Result<serde_json::Value, crate::providers::ProviderError> {
            Ok(base_context.clone())
        }
    }

    struct NoRetry;
    impl RetryPolicy for NoRetry {
        fn should_retry(&self, _attempt_count: usize) -> bool {
            false
        }

        fn build_retry_prompt_delta(&self, _failed_gates: &[OrchGateRecord]) -> String {
            String::new()
        }
    }

    #[tokio::test]
    async fn tick_marks_task_ready_when_gate_passes() {
        let store = Arc::new(MemoryStore::new());
        let cfg = OrchestratorConfig {
            tick_lock_path: std::env::temp_dir()
                .join(format!("aura-orch-test-lock-{}", OrchTaskId::new().0)),
            ..OrchestratorConfig::default()
        };
        let supervisor = Supervisor::new(
            store.clone(),
            crate::providers::DefaultAgentRouter,
            MockGitHost { pass: true },
            NoRetry,
            MockContext,
            NoopNotificationSink,
            cfg,
        );

        let task_id = supervisor
            .submit(crate::providers::SubmitRequest {
                title: "T".to_string(),
                intent: "backend".to_string(),
                priority: OrchestratorTaskPriority::Normal,
                planner_enabled: false,
                context_json: serde_json::json!({}),
                dependencies: vec![],
            })
            .await
            .expect("submit");

        let out = supervisor.tick().await.expect("tick");
        assert_eq!(out.ready_for_review, 1);

        let task = store
            .get_orch_task(task_id)
            .await
            .expect("get")
            .expect("exists");
        assert_eq!(task.status, OrchestratorTaskStatus::ReadyForReview);
    }

    #[tokio::test]
    async fn retry_exhaustion_marks_needs_human() {
        let store = Arc::new(MemoryStore::new());
        let cfg = OrchestratorConfig {
            max_attempts: 1,
            tick_lock_path: std::env::temp_dir()
                .join(format!("aura-orch-test-lock-{}", OrchTaskId::new().0)),
            ..OrchestratorConfig::default()
        };
        let supervisor = Supervisor::new(
            store.clone(),
            crate::providers::DefaultAgentRouter,
            MockGitHost { pass: false },
            crate::retry::AdaptiveRetryPolicy { max_attempts: 1 },
            MockContext,
            NoopNotificationSink,
            cfg,
        );

        let task_id = supervisor
            .submit(crate::providers::SubmitRequest {
                title: "T".to_string(),
                intent: "backend".to_string(),
                priority: OrchestratorTaskPriority::Normal,
                planner_enabled: false,
                context_json: serde_json::json!({}),
                dependencies: vec![],
            })
            .await
            .expect("submit");

        let out = supervisor.tick().await.expect("tick");
        assert_eq!(out.needs_human, 1);

        let task = store
            .get_orch_task(task_id)
            .await
            .expect("get")
            .expect("exists");
        assert_eq!(task.status, OrchestratorTaskStatus::NeedsHuman);
    }

    #[tokio::test]
    async fn tick_skips_when_lock_file_exists() {
        let store = Arc::new(MemoryStore::new());
        let lock_path =
            std::env::temp_dir().join(format!("aura-orch-test-lock-{}", OrchTaskId::new().0));
        let cfg = OrchestratorConfig {
            tick_lock_path: lock_path.clone(),
            ..OrchestratorConfig::default()
        };
        std::fs::write(&lock_path, "locked").expect("write lock");

        let supervisor = Supervisor::new(
            store,
            crate::providers::DefaultAgentRouter,
            MockGitHost { pass: true },
            NoRetry,
            MockContext,
            NoopNotificationSink,
            cfg,
        );

        let out = supervisor.tick().await.expect("tick");
        assert!(out.skipped_due_to_lock);

        let _ = std::fs::remove_file(lock_path);
    }
}
