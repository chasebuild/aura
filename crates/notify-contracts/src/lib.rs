use async_trait::async_trait;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum NotificationEvent {
    PrReadyForReview,
    RetryExhausted,
    DaemonUnhealthy,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct NotificationMessage {
    pub event: NotificationEvent,
    pub task_id: String,
    pub title: String,
    pub body: String,
    pub metadata: serde_json::Value,
    pub created_at: DateTime<Utc>,
}

#[async_trait]
pub trait NotificationSink: Send + Sync {
    async fn notify(&self, message: NotificationMessage) -> Result<(), NotificationError>;
}

#[derive(Debug, thiserror::Error)]
pub enum NotificationError {
    #[error("notification transport error: {0}")]
    Transport(String),
}

#[derive(Debug, Default, Clone)]
pub struct NoopNotificationSink;

#[async_trait]
impl NotificationSink for NoopNotificationSink {
    async fn notify(&self, _message: NotificationMessage) -> Result<(), NotificationError> {
        Ok(())
    }
}
