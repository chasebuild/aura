use std::path::PathBuf;

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
}

impl SqliteStore {
    pub fn new(config: SqliteStoreConfig) -> Self {
        Self { config }
    }
}
