use std::collections::HashMap;

use aura_contracts::{ExecutionStage, ProjectId, TaskId, WorkspaceId};
use aura_store::{PromptScope, PromptTemplateStore};
use regex::Regex;
use thiserror::Error;

#[derive(Debug, Clone, Default)]
pub struct PromptTemplateRegistry {
    defaults: HashMap<ExecutionStage, String>,
}

#[derive(Debug, Clone, Copy, Default)]
pub struct PromptScopeContext {
    pub project_id: Option<ProjectId>,
    pub task_id: Option<TaskId>,
    pub workspace_id: Option<WorkspaceId>,
}

#[derive(Debug, Error)]
pub enum PromptTemplateError {
    #[error("no template found for stage: {0:?}")]
    TemplateNotFound(ExecutionStage),
    #[error("missing template variable: {0}")]
    MissingVariable(String),
    #[error("store error: {0}")]
    Store(String),
}

impl PromptTemplateRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn register_default(&mut self, stage: ExecutionStage, template: impl Into<String>) {
        self.defaults.insert(stage, template.into());
    }

    pub async fn render<S: PromptTemplateStore + Send + Sync>(
        &self,
        store: &S,
        stage: ExecutionStage,
        vars: &HashMap<String, String>,
        scope: PromptScopeContext,
    ) -> Result<String, PromptTemplateError> {
        let resolved = self.resolve_template(store, stage, scope).await?;
        render_strict(&resolved, vars)
    }

    async fn resolve_template<S: PromptTemplateStore + Send + Sync>(
        &self,
        store: &S,
        stage: ExecutionStage,
        scope: PromptScopeContext,
    ) -> Result<String, PromptTemplateError> {
        let mut search = Vec::new();
        if let Some(workspace_id) = scope.workspace_id {
            search.push(PromptScope::Workspace(workspace_id));
        }
        if let Some(task_id) = scope.task_id {
            search.push(PromptScope::Task(task_id));
        }
        if let Some(project_id) = scope.project_id {
            search.push(PromptScope::Project(project_id));
        }
        search.push(PromptScope::Global);

        for scope in search {
            let found = store
                .get_prompt_template(&scope, stage)
                .await
                .map_err(|e| PromptTemplateError::Store(e.to_string()))?;
            if let Some(template) = found {
                return Ok(template.template);
            }
        }

        self.defaults
            .get(&stage)
            .cloned()
            .ok_or(PromptTemplateError::TemplateNotFound(stage))
    }
}

fn render_strict(
    template: &str,
    vars: &HashMap<String, String>,
) -> Result<String, PromptTemplateError> {
    let re = Regex::new(r"\{\{\s*([a-zA-Z0-9_\.]+)\s*\}\}").expect("regex is valid");
    let mut output = String::with_capacity(template.len());
    let mut cursor = 0;

    for captures in re.captures_iter(template) {
        let m = captures.get(0).expect("capture exists");
        output.push_str(&template[cursor..m.start()]);

        let key = captures.get(1).expect("key exists").as_str();
        let value = vars
            .get(key)
            .ok_or_else(|| PromptTemplateError::MissingVariable(key.to_string()))?;
        output.push_str(value);
        cursor = m.end();
    }

    output.push_str(&template[cursor..]);
    Ok(output)
}

#[cfg(test)]
mod tests {
    use aura_store::{PromptTemplateRecord, PromptTemplateStore, memory::MemoryStore};
    use chrono::Utc;

    use super::*;

    #[tokio::test]
    async fn workspace_override_takes_precedence() {
        let store = MemoryStore::new();
        let workspace_id = WorkspaceId::new();
        let task_id = TaskId::new();
        let project_id = ProjectId::new();

        store
            .upsert_prompt_template(PromptTemplateRecord {
                scope: PromptScope::Global,
                stage: ExecutionStage::Setup,
                template: "global {{name}}".to_string(),
                updated_at: Utc::now(),
            })
            .await
            .expect("template");

        store
            .upsert_prompt_template(PromptTemplateRecord {
                scope: PromptScope::Workspace(workspace_id),
                stage: ExecutionStage::Setup,
                template: "workspace {{name}}".to_string(),
                updated_at: Utc::now(),
            })
            .await
            .expect("template");

        let mut vars = HashMap::new();
        vars.insert("name".to_string(), "alpha".to_string());

        let registry = PromptTemplateRegistry::new();
        let output = registry
            .render(
                &store,
                ExecutionStage::Setup,
                &vars,
                PromptScopeContext {
                    project_id: Some(project_id),
                    task_id: Some(task_id),
                    workspace_id: Some(workspace_id),
                },
            )
            .await
            .expect("render");

        assert_eq!(output, "workspace alpha");
    }

    #[tokio::test]
    async fn missing_variable_fails_in_strict_mode() {
        let store = MemoryStore::new();
        let mut registry = PromptTemplateRegistry::new();
        registry.register_default(ExecutionStage::Review, "review {{missing}}");
        let vars = HashMap::new();

        let err = registry
            .render(
                &store,
                ExecutionStage::Review,
                &vars,
                PromptScopeContext::default(),
            )
            .await
            .expect_err("must fail");

        assert!(matches!(err, PromptTemplateError::MissingVariable(_)));
    }
}
