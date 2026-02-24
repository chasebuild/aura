use aura_store::OrchGateRecord;

pub trait RetryPolicy: Send + Sync {
    fn should_retry(&self, attempt_count: usize) -> bool;
    fn build_retry_prompt_delta(&self, failed_gates: &[OrchGateRecord]) -> String;
}

#[derive(Debug, Clone)]
pub struct AdaptiveRetryPolicy {
    pub max_attempts: usize,
}

impl RetryPolicy for AdaptiveRetryPolicy {
    fn should_retry(&self, attempt_count: usize) -> bool {
        attempt_count < self.max_attempts
    }

    fn build_retry_prompt_delta(&self, failed_gates: &[OrchGateRecord]) -> String {
        let mut lines = vec![
            "Follow-up required due to failed readiness gates:".to_string(),
            "Address only the blockers below, then re-run validations.".to_string(),
        ];

        for gate in failed_gates {
            lines.push(format!(
                "- gate={} status={:?} details={}",
                gate.gate_name, gate.status, gate.details_json
            ));
        }

        lines.join("\n")
    }
}

#[cfg(test)]
mod tests {
    use aura_contracts::{GateStatus, OrchAttemptId, OrchTaskId};
    use chrono::Utc;

    use super::*;

    #[test]
    fn retry_policy_respects_attempt_limit() {
        let policy = AdaptiveRetryPolicy { max_attempts: 3 };
        assert!(policy.should_retry(0));
        assert!(policy.should_retry(2));
        assert!(!policy.should_retry(3));
    }

    #[test]
    fn retry_prompt_delta_contains_gate_details() {
        let policy = AdaptiveRetryPolicy { max_attempts: 3 };
        let delta = policy.build_retry_prompt_delta(&[OrchGateRecord {
            task_id: OrchTaskId::new(),
            attempt_id: OrchAttemptId::new(),
            gate_name: "ci_passed".to_string(),
            status: GateStatus::Failed,
            details_json: serde_json::json!({"reason":"tests failed"}),
            checked_at: Utc::now(),
        }]);

        assert!(delta.contains("ci_passed"));
        assert!(delta.contains("tests failed"));
    }
}
