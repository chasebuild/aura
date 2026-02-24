use std::path::PathBuf;

use async_trait::async_trait;
use aura_contracts::{
    BoardColumnId, CanonicalTaskState, ExecutionStage, ExecutionStatus, OrchAttemptId, OrchTaskId,
    OrchestratorTaskStatus, ProcessId, ProjectId, SessionId, TaskId, WorkspaceId,
};
use chrono::{DateTime, Utc};
use serde::{Serialize, de::DeserializeOwned};
use serde_json::Value;
use sqlx::{
    Row, SqlitePool,
    sqlite::{SqliteConnectOptions, SqlitePoolOptions},
};
use uuid::Uuid;

use crate::{
    AuraStore, BoardRulesRecord, BoardRulesStore, ExecutionRecord, ExecutionStore,
    OrchAttemptRecord, OrchEventRecord, OrchGateRecord, OrchTaskDependencyRecord, OrchTaskRecord,
    OrchestratorStore, PromptScope, PromptTemplateRecord, PromptTemplateStore, SessionRecord,
    SessionStore, StoreError, StoreResult, StoreTx, TaskRecord, TaskStore, WorkspaceRecord,
    WorkspaceStore,
};

pub const SQLITE_SCHEMA: &str = r#"
CREATE TABLE IF NOT EXISTS tasks (
  id TEXT PRIMARY KEY,
  project_id TEXT NOT NULL,
  title TEXT NOT NULL,
  description TEXT,
  canonical_state TEXT NOT NULL,
  board_column_id TEXT,
  metadata_json TEXT NOT NULL,
  created_at TEXT NOT NULL,
  updated_at TEXT NOT NULL
);

CREATE TABLE IF NOT EXISTS workspaces (
  id TEXT PRIMARY KEY,
  task_id TEXT NOT NULL,
  root_path TEXT NOT NULL,
  branch TEXT NOT NULL,
  repo_names_json TEXT NOT NULL,
  archived INTEGER NOT NULL,
  created_at TEXT NOT NULL,
  updated_at TEXT NOT NULL
);

CREATE TABLE IF NOT EXISTS sessions (
  id TEXT PRIMARY KEY,
  workspace_id TEXT NOT NULL,
  executor_profile_id TEXT NOT NULL,
  created_at TEXT NOT NULL
);

CREATE TABLE IF NOT EXISTS executions (
  id TEXT PRIMARY KEY,
  session_id TEXT NOT NULL,
  workspace_id TEXT NOT NULL,
  task_id TEXT NOT NULL,
  stage TEXT,
  status TEXT NOT NULL,
  run_reason TEXT NOT NULL,
  last_error TEXT,
  started_at TEXT NOT NULL,
  completed_at TEXT,
  updated_at TEXT NOT NULL
);

CREATE TABLE IF NOT EXISTS board_rules (
  singleton INTEGER PRIMARY KEY CHECK (singleton = 1),
  columns_json TEXT NOT NULL,
  transition_rules_json TEXT NOT NULL
);

CREATE TABLE IF NOT EXISTS prompt_templates (
  scope_kind TEXT NOT NULL,
  scope_id TEXT,
  stage TEXT NOT NULL,
  template TEXT NOT NULL,
  updated_at TEXT NOT NULL,
  PRIMARY KEY (scope_kind, scope_id, stage)
);

CREATE TABLE IF NOT EXISTS orch_tasks (
  id TEXT PRIMARY KEY,
  title TEXT NOT NULL,
  intent TEXT NOT NULL,
  status TEXT NOT NULL,
  priority TEXT NOT NULL,
  planner_enabled INTEGER NOT NULL,
  context_json TEXT NOT NULL,
  created_at TEXT NOT NULL,
  updated_at TEXT NOT NULL
);

CREATE TABLE IF NOT EXISTS orch_task_dependencies (
  task_id TEXT NOT NULL,
  depends_on_task_id TEXT NOT NULL,
  PRIMARY KEY (task_id, depends_on_task_id)
);

CREATE TABLE IF NOT EXISTS orch_attempts (
  id TEXT PRIMARY KEY,
  task_id TEXT NOT NULL,
  process_id TEXT,
  executor_profile TEXT NOT NULL,
  prompt_snapshot TEXT NOT NULL,
  status TEXT NOT NULL,
  retry_index INTEGER NOT NULL,
  started_at TEXT NOT NULL,
  completed_at TEXT
);

CREATE TABLE IF NOT EXISTS orch_gates (
  task_id TEXT NOT NULL,
  attempt_id TEXT NOT NULL,
  gate_name TEXT NOT NULL,
  status TEXT NOT NULL,
  details_json TEXT NOT NULL,
  checked_at TEXT NOT NULL,
  PRIMARY KEY (attempt_id, gate_name)
);

CREATE TABLE IF NOT EXISTS orch_events (
  id INTEGER PRIMARY KEY AUTOINCREMENT,
  task_id TEXT NOT NULL,
  attempt_id TEXT,
  event_type TEXT NOT NULL,
  payload_json TEXT NOT NULL,
  created_at TEXT NOT NULL
);
"#;

#[derive(Debug, Clone)]
pub struct SqliteStoreConfig {
    pub database_path: PathBuf,
}

impl SqliteStoreConfig {
    pub fn new(database_path: PathBuf) -> Self {
        Self { database_path }
    }
}

#[derive(Debug)]
pub struct SqliteStore {
    pub config: SqliteStoreConfig,
    pool: SqlitePool,
}

impl SqliteStore {
    pub async fn connect(config: SqliteStoreConfig) -> StoreResult<Self> {
        if let Some(parent) = config.database_path.parent() {
            tokio::fs::create_dir_all(parent)
                .await
                .map_err(|e| StoreError::Internal(e.to_string()))?;
        }

        let connect_options = SqliteConnectOptions::new()
            .filename(&config.database_path)
            .create_if_missing(true);
        let pool = SqlitePoolOptions::new()
            .max_connections(1)
            .connect_with(connect_options)
            .await
            .map_err(map_sqlx)?;

        apply_schema(&pool).await?;

        Ok(Self { config, pool })
    }

    pub fn pool(&self) -> &SqlitePool {
        &self.pool
    }
}

#[derive(Default)]
struct SqliteTx;

#[async_trait]
impl StoreTx for SqliteTx {
    async fn commit(self: Box<Self>) -> StoreResult<()> {
        let _ = self;
        Ok(())
    }

    async fn rollback(self: Box<Self>) -> StoreResult<()> {
        let _ = self;
        Ok(())
    }
}

fn map_sqlx(error: sqlx::Error) -> StoreError {
    StoreError::Internal(error.to_string())
}

fn encode_enum<T: Serialize>(value: &T) -> StoreResult<String> {
    serde_json::to_value(value)
        .map_err(|e| StoreError::Internal(e.to_string()))?
        .as_str()
        .map(ToString::to_string)
        .ok_or_else(|| StoreError::Validation("enum failed to serialize as string".to_string()))
}

fn decode_enum<T: DeserializeOwned>(value: &str) -> StoreResult<T> {
    serde_json::from_value(Value::String(value.to_string()))
        .map_err(|e| StoreError::Validation(e.to_string()))
}

fn encode_json<T: Serialize>(value: &T) -> StoreResult<String> {
    serde_json::to_string(value).map_err(|e| StoreError::Internal(e.to_string()))
}

fn decode_json<T: DeserializeOwned>(value: &str) -> StoreResult<T> {
    serde_json::from_str(value).map_err(|e| StoreError::Validation(e.to_string()))
}

fn encode_dt(value: DateTime<Utc>) -> String {
    value.to_rfc3339()
}

fn decode_dt(value: &str) -> StoreResult<DateTime<Utc>> {
    DateTime::parse_from_rfc3339(value)
        .map(|v| v.with_timezone(&Utc))
        .map_err(|e| StoreError::Validation(e.to_string()))
}

fn parse_uuid(value: &str) -> StoreResult<Uuid> {
    Uuid::parse_str(value).map_err(|e| StoreError::Validation(e.to_string()))
}

fn scope_to_db(scope: &PromptScope) -> (String, Option<String>) {
    match scope {
        PromptScope::Global => ("global".to_string(), None),
        PromptScope::Project(id) => ("project".to_string(), Some(id.0.to_string())),
        PromptScope::Task(id) => ("task".to_string(), Some(id.0.to_string())),
        PromptScope::Workspace(id) => ("workspace".to_string(), Some(id.0.to_string())),
    }
}

fn scope_from_db(kind: &str, id: Option<&str>) -> StoreResult<PromptScope> {
    match kind {
        "global" => Ok(PromptScope::Global),
        "project" => {
            let id = id.ok_or(StoreError::Validation(
                "missing project scope id".to_string(),
            ))?;
            Ok(PromptScope::Project(ProjectId(parse_uuid(id)?)))
        }
        "task" => {
            let id = id.ok_or(StoreError::Validation("missing task scope id".to_string()))?;
            Ok(PromptScope::Task(TaskId(parse_uuid(id)?)))
        }
        "workspace" => {
            let id = id.ok_or(StoreError::Validation(
                "missing workspace scope id".to_string(),
            ))?;
            Ok(PromptScope::Workspace(WorkspaceId(parse_uuid(id)?)))
        }
        _ => Err(StoreError::Validation(format!(
            "unsupported prompt scope kind: {kind}"
        ))),
    }
}

async fn apply_schema(pool: &SqlitePool) -> StoreResult<()> {
    for statement in SQLITE_SCHEMA.split(';') {
        let statement = statement.trim();
        if statement.is_empty() {
            continue;
        }
        sqlx::query(statement)
            .execute(pool)
            .await
            .map_err(map_sqlx)?;
    }
    Ok(())
}

#[async_trait]
impl TaskStore for SqliteStore {
    async fn upsert_task(&self, task: TaskRecord) -> StoreResult<()> {
        sqlx::query(
            "INSERT INTO tasks (id, project_id, title, description, canonical_state, board_column_id, metadata_json, created_at, updated_at)
             VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?)
             ON CONFLICT(id) DO UPDATE SET
               project_id=excluded.project_id,
               title=excluded.title,
               description=excluded.description,
               canonical_state=excluded.canonical_state,
               board_column_id=excluded.board_column_id,
               metadata_json=excluded.metadata_json,
               created_at=excluded.created_at,
               updated_at=excluded.updated_at",
        )
        .bind(task.id.0.to_string())
        .bind(task.project_id.0.to_string())
        .bind(task.title)
        .bind(task.description)
        .bind(encode_enum(&task.canonical_state)?)
        .bind(task.board_column_id.map(|v| v.0))
        .bind(encode_json(&task.metadata)?)
        .bind(encode_dt(task.created_at))
        .bind(encode_dt(task.updated_at))
        .execute(&self.pool)
        .await
        .map_err(map_sqlx)?;
        Ok(())
    }

    async fn get_task(&self, task_id: TaskId) -> StoreResult<Option<TaskRecord>> {
        let row = sqlx::query(
            "SELECT id, project_id, title, description, canonical_state, board_column_id, metadata_json, created_at, updated_at
             FROM tasks WHERE id = ?",
        )
        .bind(task_id.0.to_string())
        .fetch_optional(&self.pool)
        .await
        .map_err(map_sqlx)?;

        row.map(task_from_row).transpose()
    }

    async fn update_task_state(
        &self,
        task_id: TaskId,
        canonical_state: CanonicalTaskState,
        board_column_id: Option<BoardColumnId>,
    ) -> StoreResult<()> {
        let result = sqlx::query(
            "UPDATE tasks SET canonical_state = ?, board_column_id = ?, updated_at = ? WHERE id = ?",
        )
        .bind(encode_enum(&canonical_state)?)
        .bind(board_column_id.map(|v| v.0))
        .bind(encode_dt(Utc::now()))
        .bind(task_id.0.to_string())
        .execute(&self.pool)
        .await
        .map_err(map_sqlx)?;

        if result.rows_affected() == 0 {
            return Err(StoreError::NotFound("task"));
        }

        Ok(())
    }
}

fn task_from_row(row: sqlx::sqlite::SqliteRow) -> StoreResult<TaskRecord> {
    Ok(TaskRecord {
        id: TaskId(parse_uuid(row.get::<String, _>("id").as_str())?),
        project_id: ProjectId(parse_uuid(row.get::<String, _>("project_id").as_str())?),
        title: row.get("title"),
        description: row.get("description"),
        canonical_state: decode_enum(row.get::<String, _>("canonical_state").as_str())?,
        board_column_id: row
            .get::<Option<String>, _>("board_column_id")
            .map(BoardColumnId::new),
        metadata: decode_json(row.get::<String, _>("metadata_json").as_str())?,
        created_at: decode_dt(row.get::<String, _>("created_at").as_str())?,
        updated_at: decode_dt(row.get::<String, _>("updated_at").as_str())?,
    })
}

#[async_trait]
impl WorkspaceStore for SqliteStore {
    async fn upsert_workspace(&self, workspace: WorkspaceRecord) -> StoreResult<()> {
        sqlx::query(
            "INSERT INTO workspaces (id, task_id, root_path, branch, repo_names_json, archived, created_at, updated_at)
             VALUES (?, ?, ?, ?, ?, ?, ?, ?)
             ON CONFLICT(id) DO UPDATE SET
               task_id=excluded.task_id,
               root_path=excluded.root_path,
               branch=excluded.branch,
               repo_names_json=excluded.repo_names_json,
               archived=excluded.archived,
               created_at=excluded.created_at,
               updated_at=excluded.updated_at",
        )
        .bind(workspace.id.0.to_string())
        .bind(workspace.task_id.0.to_string())
        .bind(workspace.root_path.display().to_string())
        .bind(workspace.branch)
        .bind(encode_json(&workspace.repo_names)?)
        .bind(i64::from(workspace.archived))
        .bind(encode_dt(workspace.created_at))
        .bind(encode_dt(workspace.updated_at))
        .execute(&self.pool)
        .await
        .map_err(map_sqlx)?;

        Ok(())
    }

    async fn get_workspace(
        &self,
        workspace_id: WorkspaceId,
    ) -> StoreResult<Option<WorkspaceRecord>> {
        let row = sqlx::query(
            "SELECT id, task_id, root_path, branch, repo_names_json, archived, created_at, updated_at
             FROM workspaces WHERE id = ?",
        )
        .bind(workspace_id.0.to_string())
        .fetch_optional(&self.pool)
        .await
        .map_err(map_sqlx)?;

        row.map(workspace_from_row).transpose()
    }

    async fn list_workspaces_by_task(&self, task_id: TaskId) -> StoreResult<Vec<WorkspaceRecord>> {
        let rows = sqlx::query(
            "SELECT id, task_id, root_path, branch, repo_names_json, archived, created_at, updated_at
             FROM workspaces WHERE task_id = ? ORDER BY created_at ASC",
        )
        .bind(task_id.0.to_string())
        .fetch_all(&self.pool)
        .await
        .map_err(map_sqlx)?;

        rows.into_iter().map(workspace_from_row).collect()
    }

    async fn set_workspace_archived(
        &self,
        workspace_id: WorkspaceId,
        archived: bool,
    ) -> StoreResult<()> {
        let result = sqlx::query("UPDATE workspaces SET archived = ?, updated_at = ? WHERE id = ?")
            .bind(i64::from(archived))
            .bind(encode_dt(Utc::now()))
            .bind(workspace_id.0.to_string())
            .execute(&self.pool)
            .await
            .map_err(map_sqlx)?;

        if result.rows_affected() == 0 {
            return Err(StoreError::NotFound("workspace"));
        }

        Ok(())
    }
}

fn workspace_from_row(row: sqlx::sqlite::SqliteRow) -> StoreResult<WorkspaceRecord> {
    Ok(WorkspaceRecord {
        id: WorkspaceId(parse_uuid(row.get::<String, _>("id").as_str())?),
        task_id: TaskId(parse_uuid(row.get::<String, _>("task_id").as_str())?),
        root_path: PathBuf::from(row.get::<String, _>("root_path")),
        branch: row.get("branch"),
        repo_names: decode_json(row.get::<String, _>("repo_names_json").as_str())?,
        archived: row.get::<i64, _>("archived") != 0,
        created_at: decode_dt(row.get::<String, _>("created_at").as_str())?,
        updated_at: decode_dt(row.get::<String, _>("updated_at").as_str())?,
    })
}

#[async_trait]
impl SessionStore for SqliteStore {
    async fn upsert_session(&self, session: SessionRecord) -> StoreResult<()> {
        sqlx::query(
            "INSERT INTO sessions (id, workspace_id, executor_profile_id, created_at)
             VALUES (?, ?, ?, ?)
             ON CONFLICT(id) DO UPDATE SET
               workspace_id=excluded.workspace_id,
               executor_profile_id=excluded.executor_profile_id,
               created_at=excluded.created_at",
        )
        .bind(session.id.0.to_string())
        .bind(session.workspace_id.0.to_string())
        .bind(session.executor_profile_id)
        .bind(encode_dt(session.created_at))
        .execute(&self.pool)
        .await
        .map_err(map_sqlx)?;

        Ok(())
    }

    async fn get_session(&self, session_id: SessionId) -> StoreResult<Option<SessionRecord>> {
        let row = sqlx::query(
            "SELECT id, workspace_id, executor_profile_id, created_at FROM sessions WHERE id = ?",
        )
        .bind(session_id.0.to_string())
        .fetch_optional(&self.pool)
        .await
        .map_err(map_sqlx)?;

        row.map(session_from_row).transpose()
    }

    async fn latest_session_by_workspace(
        &self,
        workspace_id: WorkspaceId,
    ) -> StoreResult<Option<SessionRecord>> {
        let row = sqlx::query(
            "SELECT id, workspace_id, executor_profile_id, created_at
             FROM sessions WHERE workspace_id = ? ORDER BY created_at DESC LIMIT 1",
        )
        .bind(workspace_id.0.to_string())
        .fetch_optional(&self.pool)
        .await
        .map_err(map_sqlx)?;

        row.map(session_from_row).transpose()
    }
}

fn session_from_row(row: sqlx::sqlite::SqliteRow) -> StoreResult<SessionRecord> {
    Ok(SessionRecord {
        id: SessionId(parse_uuid(row.get::<String, _>("id").as_str())?),
        workspace_id: WorkspaceId(parse_uuid(row.get::<String, _>("workspace_id").as_str())?),
        executor_profile_id: row.get("executor_profile_id"),
        created_at: decode_dt(row.get::<String, _>("created_at").as_str())?,
    })
}

#[async_trait]
impl ExecutionStore for SqliteStore {
    async fn upsert_execution(&self, execution: ExecutionRecord) -> StoreResult<()> {
        sqlx::query(
            "INSERT INTO executions (id, session_id, workspace_id, task_id, stage, status, run_reason, last_error, started_at, completed_at, updated_at)
             VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)
             ON CONFLICT(id) DO UPDATE SET
               session_id=excluded.session_id,
               workspace_id=excluded.workspace_id,
               task_id=excluded.task_id,
               stage=excluded.stage,
               status=excluded.status,
               run_reason=excluded.run_reason,
               last_error=excluded.last_error,
               started_at=excluded.started_at,
               completed_at=excluded.completed_at,
               updated_at=excluded.updated_at",
        )
        .bind(execution.id.0.to_string())
        .bind(execution.session_id.0.to_string())
        .bind(execution.workspace_id.0.to_string())
        .bind(execution.task_id.0.to_string())
        .bind(execution.stage.map(|stage| encode_enum(&stage)).transpose()?)
        .bind(encode_enum(&execution.status)?)
        .bind(encode_enum(&execution.run_reason)?)
        .bind(execution.last_error)
        .bind(encode_dt(execution.started_at))
        .bind(execution.completed_at.map(encode_dt))
        .bind(encode_dt(execution.updated_at))
        .execute(&self.pool)
        .await
        .map_err(map_sqlx)?;

        Ok(())
    }

    async fn get_execution(&self, process_id: ProcessId) -> StoreResult<Option<ExecutionRecord>> {
        let row = sqlx::query(
            "SELECT id, session_id, workspace_id, task_id, stage, status, run_reason, last_error, started_at, completed_at, updated_at
             FROM executions WHERE id = ?",
        )
        .bind(process_id.0.to_string())
        .fetch_optional(&self.pool)
        .await
        .map_err(map_sqlx)?;

        row.map(execution_from_row).transpose()
    }

    async fn update_execution_stage_status(
        &self,
        process_id: ProcessId,
        stage: Option<ExecutionStage>,
        status: ExecutionStatus,
        last_error: Option<String>,
    ) -> StoreResult<()> {
        let mut completed_at = None;
        if matches!(
            status,
            ExecutionStatus::Completed | ExecutionStatus::Failed | ExecutionStatus::Killed
        ) {
            completed_at = Some(Utc::now());
        }

        let result = sqlx::query(
            "UPDATE executions
             SET stage = ?, status = ?, last_error = ?, updated_at = ?,
                 completed_at = COALESCE(?, completed_at)
             WHERE id = ?",
        )
        .bind(stage.map(|v| encode_enum(&v)).transpose()?)
        .bind(encode_enum(&status)?)
        .bind(last_error)
        .bind(encode_dt(Utc::now()))
        .bind(completed_at.map(encode_dt))
        .bind(process_id.0.to_string())
        .execute(&self.pool)
        .await
        .map_err(map_sqlx)?;

        if result.rows_affected() == 0 {
            return Err(StoreError::NotFound("execution"));
        }

        Ok(())
    }

    async fn list_executions_by_session(
        &self,
        session_id: SessionId,
    ) -> StoreResult<Vec<ExecutionRecord>> {
        let rows = sqlx::query(
            "SELECT id, session_id, workspace_id, task_id, stage, status, run_reason, last_error, started_at, completed_at, updated_at
             FROM executions WHERE session_id = ? ORDER BY started_at ASC",
        )
        .bind(session_id.0.to_string())
        .fetch_all(&self.pool)
        .await
        .map_err(map_sqlx)?;

        rows.into_iter().map(execution_from_row).collect()
    }

    async fn find_running_non_dev_by_workspace(
        &self,
        workspace_id: WorkspaceId,
    ) -> StoreResult<Vec<ExecutionRecord>> {
        let running = encode_enum(&ExecutionStatus::Running)?;
        let dev = encode_enum(&aura_contracts::RunReason::DevServer)?;
        let rows = sqlx::query(
            "SELECT id, session_id, workspace_id, task_id, stage, status, run_reason, last_error, started_at, completed_at, updated_at
             FROM executions WHERE workspace_id = ? AND status = ? AND run_reason != ?",
        )
        .bind(workspace_id.0.to_string())
        .bind(running)
        .bind(dev)
        .fetch_all(&self.pool)
        .await
        .map_err(map_sqlx)?;

        rows.into_iter().map(execution_from_row).collect()
    }
}

fn execution_from_row(row: sqlx::sqlite::SqliteRow) -> StoreResult<ExecutionRecord> {
    let stage_raw: Option<String> = row.get("stage");
    let completed_at_raw: Option<String> = row.get("completed_at");

    Ok(ExecutionRecord {
        id: ProcessId(parse_uuid(row.get::<String, _>("id").as_str())?),
        session_id: SessionId(parse_uuid(row.get::<String, _>("session_id").as_str())?),
        workspace_id: WorkspaceId(parse_uuid(row.get::<String, _>("workspace_id").as_str())?),
        task_id: TaskId(parse_uuid(row.get::<String, _>("task_id").as_str())?),
        stage: stage_raw.as_deref().map(decode_enum).transpose()?,
        status: decode_enum(row.get::<String, _>("status").as_str())?,
        run_reason: decode_enum(row.get::<String, _>("run_reason").as_str())?,
        last_error: row.get("last_error"),
        started_at: decode_dt(row.get::<String, _>("started_at").as_str())?,
        completed_at: completed_at_raw.as_deref().map(decode_dt).transpose()?,
        updated_at: decode_dt(row.get::<String, _>("updated_at").as_str())?,
    })
}

#[async_trait]
impl BoardRulesStore for SqliteStore {
    async fn set_board_rules(&self, board_rules: BoardRulesRecord) -> StoreResult<()> {
        sqlx::query(
            "INSERT INTO board_rules (singleton, columns_json, transition_rules_json)
             VALUES (1, ?, ?)
             ON CONFLICT(singleton) DO UPDATE SET
               columns_json=excluded.columns_json,
               transition_rules_json=excluded.transition_rules_json",
        )
        .bind(encode_json(&board_rules.columns)?)
        .bind(encode_json(&board_rules.transition_rules)?)
        .execute(&self.pool)
        .await
        .map_err(map_sqlx)?;

        Ok(())
    }

    async fn get_board_rules(&self) -> StoreResult<BoardRulesRecord> {
        let row = sqlx::query(
            "SELECT columns_json, transition_rules_json FROM board_rules WHERE singleton = 1",
        )
        .fetch_optional(&self.pool)
        .await
        .map_err(map_sqlx)?
        .ok_or(StoreError::NotFound("board_rules"))?;

        Ok(BoardRulesRecord {
            columns: decode_json(row.get::<String, _>("columns_json").as_str())?,
            transition_rules: decode_json(row.get::<String, _>("transition_rules_json").as_str())?,
        })
    }
}

#[async_trait]
impl PromptTemplateStore for SqliteStore {
    async fn upsert_prompt_template(&self, template: PromptTemplateRecord) -> StoreResult<()> {
        let (scope_kind, scope_id) = scope_to_db(&template.scope);
        sqlx::query(
            "INSERT INTO prompt_templates (scope_kind, scope_id, stage, template, updated_at)
             VALUES (?, ?, ?, ?, ?)
             ON CONFLICT(scope_kind, scope_id, stage) DO UPDATE SET
               template=excluded.template,
               updated_at=excluded.updated_at",
        )
        .bind(scope_kind)
        .bind(scope_id)
        .bind(encode_enum(&template.stage)?)
        .bind(template.template)
        .bind(encode_dt(template.updated_at))
        .execute(&self.pool)
        .await
        .map_err(map_sqlx)?;

        Ok(())
    }

    async fn get_prompt_template(
        &self,
        scope: &PromptScope,
        stage: ExecutionStage,
    ) -> StoreResult<Option<PromptTemplateRecord>> {
        let (scope_kind, scope_id) = scope_to_db(scope);
        let row = sqlx::query(
            "SELECT scope_kind, scope_id, stage, template, updated_at
             FROM prompt_templates
             WHERE scope_kind = ? AND scope_id IS ? AND stage = ?",
        )
        .bind(scope_kind)
        .bind(scope_id)
        .bind(encode_enum(&stage)?)
        .fetch_optional(&self.pool)
        .await
        .map_err(map_sqlx)?;

        row.map(prompt_template_from_row).transpose()
    }

    async fn list_prompt_templates(&self) -> StoreResult<Vec<PromptTemplateRecord>> {
        let rows = sqlx::query(
            "SELECT scope_kind, scope_id, stage, template, updated_at FROM prompt_templates",
        )
        .fetch_all(&self.pool)
        .await
        .map_err(map_sqlx)?;

        rows.into_iter().map(prompt_template_from_row).collect()
    }
}

fn prompt_template_from_row(row: sqlx::sqlite::SqliteRow) -> StoreResult<PromptTemplateRecord> {
    let scope_kind: String = row.get("scope_kind");
    let scope_id: Option<String> = row.get("scope_id");

    Ok(PromptTemplateRecord {
        scope: scope_from_db(&scope_kind, scope_id.as_deref())?,
        stage: decode_enum(row.get::<String, _>("stage").as_str())?,
        template: row.get("template"),
        updated_at: decode_dt(row.get::<String, _>("updated_at").as_str())?,
    })
}

#[async_trait]
impl OrchestratorStore for SqliteStore {
    async fn upsert_orch_task(&self, task: OrchTaskRecord) -> StoreResult<()> {
        sqlx::query(
            "INSERT INTO orch_tasks (id, title, intent, status, priority, planner_enabled, context_json, created_at, updated_at)
             VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?)
             ON CONFLICT(id) DO UPDATE SET
               title=excluded.title,
               intent=excluded.intent,
               status=excluded.status,
               priority=excluded.priority,
               planner_enabled=excluded.planner_enabled,
               context_json=excluded.context_json,
               created_at=excluded.created_at,
               updated_at=excluded.updated_at",
        )
        .bind(task.id.0.to_string())
        .bind(task.title)
        .bind(task.intent)
        .bind(encode_enum(&task.status)?)
        .bind(encode_enum(&task.priority)?)
        .bind(i64::from(task.planner_enabled))
        .bind(encode_json(&task.context_json)?)
        .bind(encode_dt(task.created_at))
        .bind(encode_dt(task.updated_at))
        .execute(&self.pool)
        .await
        .map_err(map_sqlx)?;

        Ok(())
    }

    async fn get_orch_task(&self, task_id: OrchTaskId) -> StoreResult<Option<OrchTaskRecord>> {
        let row = sqlx::query(
            "SELECT id, title, intent, status, priority, planner_enabled, context_json, created_at, updated_at
             FROM orch_tasks WHERE id = ?",
        )
        .bind(task_id.0.to_string())
        .fetch_optional(&self.pool)
        .await
        .map_err(map_sqlx)?;

        row.map(orch_task_from_row).transpose()
    }

    async fn list_orch_tasks(&self) -> StoreResult<Vec<OrchTaskRecord>> {
        let rows = sqlx::query(
            "SELECT id, title, intent, status, priority, planner_enabled, context_json, created_at, updated_at
             FROM orch_tasks ORDER BY created_at ASC",
        )
        .fetch_all(&self.pool)
        .await
        .map_err(map_sqlx)?;

        rows.into_iter().map(orch_task_from_row).collect()
    }

    async fn set_orch_task_status(
        &self,
        task_id: OrchTaskId,
        status: OrchestratorTaskStatus,
    ) -> StoreResult<()> {
        let result = sqlx::query("UPDATE orch_tasks SET status = ?, updated_at = ? WHERE id = ?")
            .bind(encode_enum(&status)?)
            .bind(encode_dt(Utc::now()))
            .bind(task_id.0.to_string())
            .execute(&self.pool)
            .await
            .map_err(map_sqlx)?;

        if result.rows_affected() == 0 {
            return Err(StoreError::NotFound("orch_task"));
        }

        Ok(())
    }

    async fn replace_orch_task_dependencies(
        &self,
        task_id: OrchTaskId,
        dependencies: Vec<OrchTaskDependencyRecord>,
    ) -> StoreResult<()> {
        let mut tx = self.pool.begin().await.map_err(map_sqlx)?;

        sqlx::query("DELETE FROM orch_task_dependencies WHERE task_id = ?")
            .bind(task_id.0.to_string())
            .execute(&mut *tx)
            .await
            .map_err(map_sqlx)?;

        for dep in dependencies {
            sqlx::query(
                "INSERT INTO orch_task_dependencies (task_id, depends_on_task_id) VALUES (?, ?)",
            )
            .bind(dep.task_id.0.to_string())
            .bind(dep.depends_on_task_id.0.to_string())
            .execute(&mut *tx)
            .await
            .map_err(map_sqlx)?;
        }

        tx.commit().await.map_err(map_sqlx)?;
        Ok(())
    }

    async fn list_orch_task_dependencies(
        &self,
        task_id: OrchTaskId,
    ) -> StoreResult<Vec<OrchTaskDependencyRecord>> {
        let rows = sqlx::query(
            "SELECT task_id, depends_on_task_id FROM orch_task_dependencies WHERE task_id = ?",
        )
        .bind(task_id.0.to_string())
        .fetch_all(&self.pool)
        .await
        .map_err(map_sqlx)?;

        rows.into_iter()
            .map(|row| {
                Ok(OrchTaskDependencyRecord {
                    task_id: OrchTaskId(parse_uuid(row.get::<String, _>("task_id").as_str())?),
                    depends_on_task_id: OrchTaskId(parse_uuid(
                        row.get::<String, _>("depends_on_task_id").as_str(),
                    )?),
                })
            })
            .collect()
    }

    async fn upsert_orch_attempt(&self, attempt: OrchAttemptRecord) -> StoreResult<()> {
        sqlx::query(
            "INSERT INTO orch_attempts (id, task_id, process_id, executor_profile, prompt_snapshot, status, retry_index, started_at, completed_at)
             VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?)
             ON CONFLICT(id) DO UPDATE SET
               task_id=excluded.task_id,
               process_id=excluded.process_id,
               executor_profile=excluded.executor_profile,
               prompt_snapshot=excluded.prompt_snapshot,
               status=excluded.status,
               retry_index=excluded.retry_index,
               started_at=excluded.started_at,
               completed_at=excluded.completed_at",
        )
        .bind(attempt.id.0.to_string())
        .bind(attempt.task_id.0.to_string())
        .bind(attempt.process_id.map(|v| v.0.to_string()))
        .bind(attempt.executor_profile)
        .bind(attempt.prompt_snapshot)
        .bind(encode_enum(&attempt.status)?)
        .bind(attempt.retry_index)
        .bind(encode_dt(attempt.started_at))
        .bind(attempt.completed_at.map(encode_dt))
        .execute(&self.pool)
        .await
        .map_err(map_sqlx)?;
        Ok(())
    }

    async fn get_orch_attempt(
        &self,
        attempt_id: OrchAttemptId,
    ) -> StoreResult<Option<OrchAttemptRecord>> {
        let row = sqlx::query(
            "SELECT id, task_id, process_id, executor_profile, prompt_snapshot, status, retry_index, started_at, completed_at
             FROM orch_attempts WHERE id = ?",
        )
        .bind(attempt_id.0.to_string())
        .fetch_optional(&self.pool)
        .await
        .map_err(map_sqlx)?;

        row.map(orch_attempt_from_row).transpose()
    }

    async fn list_orch_attempts_by_task(
        &self,
        task_id: OrchTaskId,
    ) -> StoreResult<Vec<OrchAttemptRecord>> {
        let rows = sqlx::query(
            "SELECT id, task_id, process_id, executor_profile, prompt_snapshot, status, retry_index, started_at, completed_at
             FROM orch_attempts WHERE task_id = ? ORDER BY started_at ASC",
        )
        .bind(task_id.0.to_string())
        .fetch_all(&self.pool)
        .await
        .map_err(map_sqlx)?;

        rows.into_iter().map(orch_attempt_from_row).collect()
    }

    async fn replace_orch_gates(
        &self,
        task_id: OrchTaskId,
        attempt_id: OrchAttemptId,
        gates: Vec<OrchGateRecord>,
    ) -> StoreResult<()> {
        let mut tx = self.pool.begin().await.map_err(map_sqlx)?;

        sqlx::query("DELETE FROM orch_gates WHERE attempt_id = ?")
            .bind(attempt_id.0.to_string())
            .execute(&mut *tx)
            .await
            .map_err(map_sqlx)?;

        for gate in gates {
            if gate.task_id != task_id || gate.attempt_id != attempt_id {
                return Err(StoreError::Validation(
                    "gate task/attempt mismatch".to_string(),
                ));
            }
            sqlx::query(
                "INSERT INTO orch_gates (task_id, attempt_id, gate_name, status, details_json, checked_at)
                 VALUES (?, ?, ?, ?, ?, ?)",
            )
            .bind(gate.task_id.0.to_string())
            .bind(gate.attempt_id.0.to_string())
            .bind(gate.gate_name)
            .bind(encode_enum(&gate.status)?)
            .bind(encode_json(&gate.details_json)?)
            .bind(encode_dt(gate.checked_at))
            .execute(&mut *tx)
            .await
            .map_err(map_sqlx)?;
        }

        tx.commit().await.map_err(map_sqlx)?;
        Ok(())
    }

    async fn list_orch_gates_by_attempt(
        &self,
        attempt_id: OrchAttemptId,
    ) -> StoreResult<Vec<OrchGateRecord>> {
        let rows = sqlx::query(
            "SELECT task_id, attempt_id, gate_name, status, details_json, checked_at
             FROM orch_gates WHERE attempt_id = ? ORDER BY gate_name ASC",
        )
        .bind(attempt_id.0.to_string())
        .fetch_all(&self.pool)
        .await
        .map_err(map_sqlx)?;

        rows.into_iter()
            .map(|row| {
                Ok(OrchGateRecord {
                    task_id: OrchTaskId(parse_uuid(row.get::<String, _>("task_id").as_str())?),
                    attempt_id: OrchAttemptId(parse_uuid(
                        row.get::<String, _>("attempt_id").as_str(),
                    )?),
                    gate_name: row.get("gate_name"),
                    status: decode_enum(row.get::<String, _>("status").as_str())?,
                    details_json: decode_json(row.get::<String, _>("details_json").as_str())?,
                    checked_at: decode_dt(row.get::<String, _>("checked_at").as_str())?,
                })
            })
            .collect()
    }

    async fn append_orch_event(&self, event: OrchEventRecord) -> StoreResult<()> {
        sqlx::query(
            "INSERT INTO orch_events (task_id, attempt_id, event_type, payload_json, created_at)
             VALUES (?, ?, ?, ?, ?)",
        )
        .bind(event.task_id.0.to_string())
        .bind(event.attempt_id.map(|v| v.0.to_string()))
        .bind(event.event_type)
        .bind(encode_json(&event.payload_json)?)
        .bind(encode_dt(event.created_at))
        .execute(&self.pool)
        .await
        .map_err(map_sqlx)?;

        Ok(())
    }

    async fn list_orch_events_by_task(
        &self,
        task_id: OrchTaskId,
    ) -> StoreResult<Vec<OrchEventRecord>> {
        let rows = sqlx::query(
            "SELECT task_id, attempt_id, event_type, payload_json, created_at
             FROM orch_events WHERE task_id = ? ORDER BY id ASC",
        )
        .bind(task_id.0.to_string())
        .fetch_all(&self.pool)
        .await
        .map_err(map_sqlx)?;

        rows.into_iter()
            .map(|row| {
                let attempt_id_raw: Option<String> = row.get("attempt_id");
                Ok(OrchEventRecord {
                    task_id: OrchTaskId(parse_uuid(row.get::<String, _>("task_id").as_str())?),
                    attempt_id: attempt_id_raw
                        .as_deref()
                        .map(parse_uuid)
                        .transpose()?
                        .map(OrchAttemptId),
                    event_type: row.get("event_type"),
                    payload_json: decode_json(row.get::<String, _>("payload_json").as_str())?,
                    created_at: decode_dt(row.get::<String, _>("created_at").as_str())?,
                })
            })
            .collect()
    }
}

fn orch_task_from_row(row: sqlx::sqlite::SqliteRow) -> StoreResult<OrchTaskRecord> {
    Ok(OrchTaskRecord {
        id: OrchTaskId(parse_uuid(row.get::<String, _>("id").as_str())?),
        title: row.get("title"),
        intent: row.get("intent"),
        status: decode_enum(row.get::<String, _>("status").as_str())?,
        priority: decode_enum(row.get::<String, _>("priority").as_str())?,
        planner_enabled: row.get::<i64, _>("planner_enabled") != 0,
        context_json: decode_json(row.get::<String, _>("context_json").as_str())?,
        created_at: decode_dt(row.get::<String, _>("created_at").as_str())?,
        updated_at: decode_dt(row.get::<String, _>("updated_at").as_str())?,
    })
}

fn orch_attempt_from_row(row: sqlx::sqlite::SqliteRow) -> StoreResult<OrchAttemptRecord> {
    let process_id_raw: Option<String> = row.get("process_id");
    let completed_at_raw: Option<String> = row.get("completed_at");

    Ok(OrchAttemptRecord {
        id: OrchAttemptId(parse_uuid(row.get::<String, _>("id").as_str())?),
        task_id: OrchTaskId(parse_uuid(row.get::<String, _>("task_id").as_str())?),
        process_id: process_id_raw
            .as_deref()
            .map(parse_uuid)
            .transpose()?
            .map(ProcessId),
        executor_profile: row.get("executor_profile"),
        prompt_snapshot: row.get("prompt_snapshot"),
        status: decode_enum(row.get::<String, _>("status").as_str())?,
        retry_index: row.get("retry_index"),
        started_at: decode_dt(row.get::<String, _>("started_at").as_str())?,
        completed_at: completed_at_raw.as_deref().map(decode_dt).transpose()?,
    })
}

#[async_trait]
impl AuraStore for SqliteStore {
    async fn begin_tx(&self) -> StoreResult<Box<dyn StoreTx>> {
        Ok(Box::<SqliteTx>::default())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use aura_contracts::{OrchestratorAttemptStatus, OrchestratorTaskPriority};

    #[tokio::test]
    async fn sqlite_orchestrator_round_trip() {
        let temp = tempfile::tempdir().expect("tmp");
        let db = temp.path().join("aura-test.db");
        let store = SqliteStore::connect(SqliteStoreConfig::new(db))
            .await
            .expect("connect");

        let task_id = OrchTaskId::new();
        let attempt_id = OrchAttemptId::new();
        let now = Utc::now();

        store
            .upsert_orch_task(OrchTaskRecord {
                id: task_id,
                title: "Task".to_string(),
                intent: "Intent".to_string(),
                status: OrchestratorTaskStatus::Pending,
                priority: OrchestratorTaskPriority::High,
                planner_enabled: true,
                context_json: serde_json::json!({"k":"v"}),
                created_at: now,
                updated_at: now,
            })
            .await
            .expect("task");

        store
            .upsert_orch_attempt(OrchAttemptRecord {
                id: attempt_id,
                task_id,
                process_id: None,
                executor_profile: "Codex:DEFAULT".to_string(),
                prompt_snapshot: "hello".to_string(),
                status: OrchestratorAttemptStatus::Pending,
                retry_index: 0,
                started_at: now,
                completed_at: None,
            })
            .await
            .expect("attempt");

        let task = store
            .get_orch_task(task_id)
            .await
            .expect("get")
            .expect("exists");
        assert_eq!(task.title, "Task");

        let attempts = store
            .list_orch_attempts_by_task(task_id)
            .await
            .expect("attempts");
        assert_eq!(attempts.len(), 1);
    }
}
