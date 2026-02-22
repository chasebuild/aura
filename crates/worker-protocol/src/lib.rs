use aura_contracts::{ExecutionStatus, ExecutorKind, ProcessId, RunReason};
use chrono::{DateTime, Duration, Utc};
use serde::{Deserialize, Serialize};
use thiserror::Error;

pub const JSONRPC_VERSION: &str = "2.0";

pub const METHOD_WORKER_HELLO: &str = "worker.hello";
pub const METHOD_START_EXECUTION: &str = "execution.start";
pub const METHOD_INTERRUPT_EXECUTION: &str = "execution.interrupt";
pub const METHOD_HEARTBEAT: &str = "worker.heartbeat";
pub const METHOD_LEASE_EXPIRED: &str = "worker.lease_expired";
pub const METHOD_EXECUTION_LOG: &str = "execution.log";
pub const METHOD_EXECUTION_STATE: &str = "execution.state";

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct JsonRpcRequest<P> {
    pub jsonrpc: String,
    pub id: String,
    pub method: String,
    pub params: P,
}

impl<P> JsonRpcRequest<P> {
    pub fn new(id: impl Into<String>, method: impl Into<String>, params: P) -> Self {
        Self {
            jsonrpc: JSONRPC_VERSION.to_string(),
            id: id.into(),
            method: method.into(),
            params,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct JsonRpcResponse<R> {
    pub jsonrpc: String,
    pub id: String,
    pub result: Option<R>,
    pub error: Option<JsonRpcError>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct JsonRpcError {
    pub code: i32,
    pub message: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct WorkerCapabilities {
    pub executors: Vec<ExecutorKind>,
    pub supports_interrupt: bool,
    pub max_concurrency: u16,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct WorkerHello {
    pub worker_id: String,
    pub capabilities: WorkerCapabilities,
    pub heartbeat_interval_secs: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct StartExecutionRequest {
    pub execution_id: ProcessId,
    pub action: aura_contracts::ExecutionAction,
    pub cwd: Option<String>,
    pub env: std::collections::HashMap<String, String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct StartExecutionResponse {
    pub accepted: bool,
    pub reason: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct InterruptExecutionRequest {
    pub execution_id: ProcessId,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ExecutionLogStream {
    Stdout,
    Stderr,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ExecutionLogEvent {
    pub execution_id: ProcessId,
    pub stream: ExecutionLogStream,
    pub line: String,
    pub timestamp: DateTime<Utc>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ExecutionFailureReason {
    WorkerLost,
    StageError,
    Cancelled,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ExecutionStateEvent {
    pub execution_id: ProcessId,
    pub status: ExecutionStatus,
    pub run_reason: RunReason,
    pub failure_reason: Option<ExecutionFailureReason>,
    pub message: Option<String>,
    pub timestamp: DateTime<Utc>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Heartbeat {
    pub worker_id: String,
    pub sent_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct LeaseExpired {
    pub worker_id: String,
    pub reason: String,
    pub expired_at: DateTime<Utc>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HeartbeatPolicy {
    pub interval: Duration,
    pub timeout: Duration,
}

impl Default for HeartbeatPolicy {
    fn default() -> Self {
        Self {
            interval: Duration::seconds(5),
            timeout: Duration::seconds(15),
        }
    }
}

#[derive(Debug, Clone)]
pub struct WorkerLeaseTracker {
    worker_id: String,
    policy: HeartbeatPolicy,
    last_heartbeat: DateTime<Utc>,
}

impl WorkerLeaseTracker {
    pub fn new(worker_id: impl Into<String>, now: DateTime<Utc>) -> Self {
        Self {
            worker_id: worker_id.into(),
            policy: HeartbeatPolicy::default(),
            last_heartbeat: now,
        }
    }

    pub fn with_policy(
        worker_id: impl Into<String>,
        now: DateTime<Utc>,
        policy: HeartbeatPolicy,
    ) -> Self {
        Self {
            worker_id: worker_id.into(),
            policy,
            last_heartbeat: now,
        }
    }

    pub fn record_heartbeat(&mut self, at: DateTime<Utc>) {
        self.last_heartbeat = at;
    }

    pub fn is_offline(&self, now: DateTime<Utc>) -> bool {
        now - self.last_heartbeat > self.policy.timeout
    }

    pub fn to_lease_expired(&self, now: DateTime<Utc>) -> LeaseExpired {
        LeaseExpired {
            worker_id: self.worker_id.clone(),
            reason: "heartbeat timeout".to_string(),
            expired_at: now,
        }
    }

    pub fn worker_lost_event(
        &self,
        execution_id: ProcessId,
        run_reason: RunReason,
        now: DateTime<Utc>,
    ) -> ExecutionStateEvent {
        ExecutionStateEvent {
            execution_id,
            status: ExecutionStatus::Failed,
            run_reason,
            failure_reason: Some(ExecutionFailureReason::WorkerLost),
            message: Some("worker lease expired".to_string()),
            timestamp: now,
        }
    }
}

#[derive(Debug, Error)]
pub enum ProtocolError {
    #[error("json error: {0}")]
    Json(#[from] serde_json::Error),
}

pub fn encode_request<P: Serialize>(request: &JsonRpcRequest<P>) -> Result<String, ProtocolError> {
    Ok(serde_json::to_string(request)?)
}

pub fn decode_request<P: for<'de> Deserialize<'de>>(
    payload: &str,
) -> Result<JsonRpcRequest<P>, ProtocolError> {
    Ok(serde_json::from_str(payload)?)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn heartbeat_timeout_marks_worker_offline() {
        let now = Utc::now();
        let tracker = WorkerLeaseTracker::new("worker-1", now);
        assert!(!tracker.is_offline(now + Duration::seconds(10)));
        assert!(tracker.is_offline(now + Duration::seconds(16)));
    }

    #[test]
    fn jsonrpc_round_trip() {
        let req = JsonRpcRequest::new(
            "1",
            METHOD_WORKER_HELLO,
            WorkerHello {
                worker_id: "w".to_string(),
                capabilities: WorkerCapabilities {
                    executors: vec![ExecutorKind::Codex],
                    supports_interrupt: true,
                    max_concurrency: 4,
                },
                heartbeat_interval_secs: 5,
            },
        );
        let encoded = encode_request(&req).expect("encode");
        let decoded: JsonRpcRequest<WorkerHello> = decode_request(&encoded).expect("decode");
        assert_eq!(decoded.method, METHOD_WORKER_HELLO);
        assert_eq!(decoded.params.worker_id, "w");
    }
}
