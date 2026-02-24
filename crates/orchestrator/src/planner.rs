use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct PlannerPlan {
    pub subtasks: Vec<PlannerSubtask>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct PlannerSubtask {
    pub id: String,
    pub title: String,
    pub intent: String,
    #[serde(default)]
    pub dependencies: Vec<String>,
}

pub fn parse_planner_output(raw: &str) -> Result<PlannerPlan, PlannerError> {
    let plan: PlannerPlan =
        serde_json::from_str(raw).map_err(|e| PlannerError::InvalidJson(e.to_string()))?;

    if plan.subtasks.is_empty() {
        return Err(PlannerError::InvalidSchema(
            "subtasks must not be empty".to_string(),
        ));
    }

    for subtask in &plan.subtasks {
        if subtask.id.trim().is_empty() {
            return Err(PlannerError::InvalidSchema(
                "subtask id must not be empty".to_string(),
            ));
        }
        if subtask.title.trim().is_empty() {
            return Err(PlannerError::InvalidSchema(
                "subtask title must not be empty".to_string(),
            ));
        }
        if subtask.intent.trim().is_empty() {
            return Err(PlannerError::InvalidSchema(
                "subtask intent must not be empty".to_string(),
            ));
        }
    }

    Ok(plan)
}

#[derive(Debug, thiserror::Error)]
pub enum PlannerError {
    #[error("planner output invalid json: {0}")]
    InvalidJson(String),
    #[error("planner output invalid schema: {0}")]
    InvalidSchema(String),
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn planner_output_validates_required_fields() {
        let err = parse_planner_output(r#"{"subtasks":[{"id":"","title":"A","intent":"B"}] }"#)
            .expect_err("must fail");
        assert!(matches!(err, PlannerError::InvalidSchema(_)));
    }

    #[test]
    fn planner_output_parses_valid_payload() {
        let plan = parse_planner_output(
            r#"{
              "subtasks": [
                {"id":"api","title":"Build API","intent":"Implement API","dependencies":[]}
              ]
            }"#,
        )
        .expect("valid");
        assert_eq!(plan.subtasks.len(), 1);
    }
}
