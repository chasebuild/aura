use std::process::Command;

use async_trait::async_trait;
use aura_contracts::{GateStatus, OrchestratorTaskPriority};
use aura_store::{OrchAttemptRecord, OrchTaskRecord};
use regex::Regex;

use crate::config::OrchestratorConfig;

#[derive(Debug, Clone)]
pub struct SubmitRequest {
    pub title: String,
    pub intent: String,
    pub priority: OrchestratorTaskPriority,
    pub planner_enabled: bool,
    pub context_json: serde_json::Value,
    pub dependencies: Vec<aura_contracts::OrchTaskId>,
}

#[async_trait]
pub trait TaskSource: Send + Sync {
    async fn fetch_tasks(&self) -> Result<Vec<SubmitRequest>, ProviderError>;
}

#[async_trait]
pub trait ContextProvider: Send + Sync {
    async fn enrich_context(
        &self,
        task: &OrchTaskRecord,
        base_context: &serde_json::Value,
    ) -> Result<serde_json::Value, ProviderError>;
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RouteDecision {
    pub executor_profile: String,
    pub model: Option<String>,
}

pub trait AgentRouter: Send + Sync {
    fn route(&self, task: &OrchTaskRecord) -> RouteDecision;
}

#[derive(Debug, Clone, Default)]
pub struct DefaultAgentRouter;

impl AgentRouter for DefaultAgentRouter {
    fn route(&self, task: &OrchTaskRecord) -> RouteDecision {
        let intent = task.intent.to_ascii_lowercase();
        if contains_any(&intent, &["frontend", "ui", "css", "component", "layout"]) {
            return RouteDecision {
                executor_profile: "Claude:DEFAULT".to_string(),
                model: Some("claude-sonnet-4-5".to_string()),
            };
        }
        if contains_any(&intent, &["design", "mock", "visual", "dashboard"]) {
            return RouteDecision {
                executor_profile: "Gemini:DEFAULT".to_string(),
                model: Some("gemini-2.5-pro".to_string()),
            };
        }
        RouteDecision {
            executor_profile: "Codex:DEFAULT".to_string(),
            model: Some("gpt-5".to_string()),
        }
    }
}

fn contains_any(text: &str, needles: &[&str]) -> bool {
    needles.iter().any(|needle| text.contains(needle))
}

#[derive(Debug, Clone)]
pub struct GateEvaluation {
    pub gate_name: String,
    pub status: GateStatus,
    pub details_json: serde_json::Value,
}

#[async_trait]
pub trait GitHostProvider: Send + Sync {
    async fn evaluate_gates(
        &self,
        task: &OrchTaskRecord,
        attempt: &OrchAttemptRecord,
        config: &OrchestratorConfig,
    ) -> Result<Vec<GateEvaluation>, ProviderError>;
}

#[derive(Debug, Clone, Default)]
pub struct GhGitHostProvider;

#[async_trait]
impl GitHostProvider for GhGitHostProvider {
    async fn evaluate_gates(
        &self,
        task: &OrchTaskRecord,
        _attempt: &OrchAttemptRecord,
        config: &OrchestratorConfig,
    ) -> Result<Vec<GateEvaluation>, ProviderError> {
        let mut command = Command::new("gh");
        command
            .arg("pr")
            .arg("view")
            .arg("--json")
            .arg("number,mergeStateStatus,reviewDecision,body,statusCheckRollup,headRefName");

        if let Some(branch) = task
            .context_json
            .get("branch")
            .and_then(serde_json::Value::as_str)
        {
            command.arg("--head").arg(branch);
        }

        let output = command.output().map_err(|e| ProviderError::Command {
            command: "gh pr view".to_string(),
            detail: e.to_string(),
        })?;

        if !output.status.success() {
            return Ok(failed_gate_bundle(
                format!(
                    "gh pr view failed: {}",
                    String::from_utf8_lossy(&output.stderr).trim()
                )
                .as_str(),
                config,
            ));
        }

        let payload: serde_json::Value = serde_json::from_slice(&output.stdout)
            .map_err(|e| ProviderError::Parse(format!("failed to parse gh payload: {e}")))?;

        let pr_exists = payload
            .get("number")
            .and_then(serde_json::Value::as_i64)
            .is_some();
        let merge_state = payload
            .get("mergeStateStatus")
            .and_then(serde_json::Value::as_str)
            .unwrap_or("UNKNOWN");
        let review_decision = payload
            .get("reviewDecision")
            .and_then(serde_json::Value::as_str)
            .unwrap_or("UNKNOWN");

        let ci_passed = payload
            .get("statusCheckRollup")
            .and_then(serde_json::Value::as_array)
            .map(|checks| {
                checks.iter().all(|check| {
                    let conclusion = check
                        .get("conclusion")
                        .and_then(serde_json::Value::as_str)
                        .unwrap_or("UNKNOWN");
                    matches!(conclusion, "SUCCESS" | "SKIPPED" | "NEUTRAL")
                })
            })
            .unwrap_or(false);

        let mut gates = vec![
            GateEvaluation {
                gate_name: "pr_created".to_string(),
                status: pass_fail(pr_exists),
                details_json: serde_json::json!({"pr_exists": pr_exists}),
            },
            GateEvaluation {
                gate_name: "branch_synced".to_string(),
                status: pass_fail(matches!(merge_state, "CLEAN" | "HAS_HOOKS")),
                details_json: serde_json::json!({"merge_state": merge_state}),
            },
            GateEvaluation {
                gate_name: "ci_passed".to_string(),
                status: pass_fail(ci_passed),
                details_json: serde_json::json!({"ci_passed": ci_passed}),
            },
        ];

        for reviewer in &config.required_reviewers {
            gates.push(GateEvaluation {
                gate_name: format!("{}_review_passed", reviewer),
                status: pass_fail(review_decision == "APPROVED"),
                details_json: serde_json::json!({"review_decision": review_decision}),
            });
        }

        let ui_changed = task
            .context_json
            .get("ui_changed")
            .and_then(serde_json::Value::as_bool)
            .unwrap_or(false);
        let body = payload
            .get("body")
            .and_then(serde_json::Value::as_str)
            .unwrap_or("");

        let screenshot_pass = evaluate_screenshot_gate(ui_changed, body, &config.screenshot_regex)?;

        gates.push(GateEvaluation {
            gate_name: "screenshot_present".to_string(),
            status: pass_fail(screenshot_pass),
            details_json: serde_json::json!({"ui_changed": ui_changed}),
        });

        Ok(gates)
    }
}

fn pass_fail(v: bool) -> GateStatus {
    if v {
        GateStatus::Passed
    } else {
        GateStatus::Failed
    }
}

fn evaluate_screenshot_gate(
    ui_changed: bool,
    body: &str,
    screenshot_regex: &str,
) -> Result<bool, ProviderError> {
    if !ui_changed {
        return Ok(true);
    }
    let re = Regex::new(screenshot_regex).map_err(|e| ProviderError::Parse(e.to_string()))?;
    Ok(re.is_match(body))
}

fn failed_gate_bundle(reason: &str, config: &OrchestratorConfig) -> Vec<GateEvaluation> {
    let mut gates = vec![
        GateEvaluation {
            gate_name: "pr_created".to_string(),
            status: GateStatus::Failed,
            details_json: serde_json::json!({"reason": reason}),
        },
        GateEvaluation {
            gate_name: "branch_synced".to_string(),
            status: GateStatus::Failed,
            details_json: serde_json::json!({"reason": reason}),
        },
        GateEvaluation {
            gate_name: "ci_passed".to_string(),
            status: GateStatus::Failed,
            details_json: serde_json::json!({"reason": reason}),
        },
    ];

    for reviewer in &config.required_reviewers {
        gates.push(GateEvaluation {
            gate_name: format!("{}_review_passed", reviewer),
            status: GateStatus::Failed,
            details_json: serde_json::json!({"reason": reason}),
        });
    }

    gates.push(GateEvaluation {
        gate_name: "screenshot_present".to_string(),
        status: GateStatus::Failed,
        details_json: serde_json::json!({"reason": reason}),
    });

    gates
}

#[derive(Debug, thiserror::Error)]
pub enum ProviderError {
    #[error("command failed: {command}: {detail}")]
    Command { command: String, detail: String },
    #[error("provider parse error: {0}")]
    Parse(String),
    #[error("provider error: {0}")]
    Other(String),
}

#[derive(Debug, Default, Clone)]
pub struct PassthroughContextProvider;

#[async_trait]
impl ContextProvider for PassthroughContextProvider {
    async fn enrich_context(
        &self,
        _task: &OrchTaskRecord,
        base_context: &serde_json::Value,
    ) -> Result<serde_json::Value, ProviderError> {
        Ok(base_context.clone())
    }
}

#[cfg(test)]
mod tests {
    use aura_contracts::{OrchTaskId, OrchestratorTaskStatus};
    use chrono::Utc;

    use super::*;

    fn task(intent: &str) -> OrchTaskRecord {
        OrchTaskRecord {
            id: OrchTaskId::new(),
            title: "task".to_string(),
            intent: intent.to_string(),
            status: OrchestratorTaskStatus::Pending,
            priority: OrchestratorTaskPriority::Normal,
            planner_enabled: false,
            context_json: serde_json::json!({}),
            created_at: Utc::now(),
            updated_at: Utc::now(),
        }
    }

    #[test]
    fn router_picks_frontend_for_ui_intent() {
        let router = DefaultAgentRouter;
        let decision = router.route(&task("Fix UI button style"));
        assert!(decision.executor_profile.starts_with("Claude"));
    }

    #[test]
    fn router_picks_default_codex_for_backend_intent() {
        let router = DefaultAgentRouter;
        let decision = router.route(&task("Fix billing race condition"));
        assert!(decision.executor_profile.starts_with("Codex"));
    }

    #[test]
    fn screenshot_gate_checks_markdown_image_when_ui_changed() {
        let regex = r"!\[[^\]]*\]\([^\)]+\)";
        let ok = evaluate_screenshot_gate(true, "Updated UI\\n![shot](https://x/y.png)", regex)
            .expect("eval");
        assert!(ok);

        let fail = evaluate_screenshot_gate(true, "No screenshot", regex).expect("eval");
        assert!(!fail);
    }
}
