use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct OrchestratorConfig {
    pub tick_interval_secs: u64,
    pub max_concurrency: usize,
    pub max_attempts: usize,
    pub strict_done_gate: bool,
    pub required_reviewers: Vec<String>,
    pub ui_change_globs: Vec<String>,
    pub screenshot_regex: String,
    pub database_path: PathBuf,
    pub tick_lock_path: PathBuf,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
struct OrchestratorConfigPatch {
    tick_interval_secs: Option<u64>,
    max_concurrency: Option<usize>,
    max_attempts: Option<usize>,
    strict_done_gate: Option<bool>,
    required_reviewers: Option<Vec<String>>,
    ui_change_globs: Option<Vec<String>>,
    screenshot_regex: Option<String>,
    database_path: Option<PathBuf>,
    tick_lock_path: Option<PathBuf>,
}

impl Default for OrchestratorConfig {
    fn default() -> Self {
        let home = std::env::var_os("HOME")
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from("."));
        let aura_home = home.join(".aura");

        Self {
            tick_interval_secs: 600,
            max_concurrency: 4,
            max_attempts: 3,
            strict_done_gate: true,
            required_reviewers: vec![
                "codex".to_string(),
                "claude".to_string(),
                "gemini".to_string(),
            ],
            ui_change_globs: vec![
                "**/*.tsx".to_string(),
                "**/*.jsx".to_string(),
                "**/*.css".to_string(),
                "**/*.scss".to_string(),
                "**/*.html".to_string(),
            ],
            screenshot_regex: "!\\[[^\\]]*\\]\\([^\\)]+\\)".to_string(),
            database_path: aura_home.join("orchestrator.db"),
            tick_lock_path: aura_home.join("orchestrator.tick.lock"),
        }
    }
}

impl OrchestratorConfig {
    pub fn load(cwd: &Path) -> Result<Self, ConfigError> {
        let global = global_config_path();
        let workspace = cwd.join(".aura").join("orchestrator.toml");
        Self::load_from_paths(Some(global.as_path()), Some(workspace.as_path()))
    }

    fn load_from_paths(
        global_path: Option<&Path>,
        workspace_path: Option<&Path>,
    ) -> Result<Self, ConfigError> {
        let mut cfg = Self::default();

        if let Some(global_path) = global_path
            && global_path.exists()
        {
            cfg.apply_patch(read_patch(global_path)?);
        }

        if let Some(workspace_path) = workspace_path
            && workspace_path.exists()
        {
            cfg.apply_patch(read_patch(workspace_path)?);
        }

        Ok(cfg)
    }

    fn apply_patch(&mut self, patch: OrchestratorConfigPatch) {
        if let Some(value) = patch.tick_interval_secs {
            self.tick_interval_secs = value;
        }
        if let Some(value) = patch.max_concurrency {
            self.max_concurrency = value;
        }
        if let Some(value) = patch.max_attempts {
            self.max_attempts = value;
        }
        if let Some(value) = patch.strict_done_gate {
            self.strict_done_gate = value;
        }
        if let Some(value) = patch.required_reviewers {
            self.required_reviewers = value;
        }
        if let Some(value) = patch.ui_change_globs {
            self.ui_change_globs = value;
        }
        if let Some(value) = patch.screenshot_regex {
            self.screenshot_regex = value;
        }
        if let Some(value) = patch.database_path {
            self.database_path = value;
        }
        if let Some(value) = patch.tick_lock_path {
            self.tick_lock_path = value;
        }
    }
}

fn global_config_path() -> PathBuf {
    let home = std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."));
    home.join(".aura").join("orchestrator.toml")
}

fn read_patch(path: &Path) -> Result<OrchestratorConfigPatch, ConfigError> {
    let raw = std::fs::read_to_string(path).map_err(|source| ConfigError::Read {
        path: path.to_path_buf(),
        source,
    })?;
    toml::from_str::<OrchestratorConfigPatch>(&raw).map_err(|source| ConfigError::Parse {
        path: path.to_path_buf(),
        source,
    })
}

#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    #[error("failed to read config {path}: {source}")]
    Read {
        path: PathBuf,
        source: std::io::Error,
    },
    #[error("failed to parse config {path}: {source}")]
    Parse {
        path: PathBuf,
        source: toml::de::Error,
    },
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_match_expected_values() {
        let cfg = OrchestratorConfig::default();
        assert_eq!(cfg.tick_interval_secs, 600);
        assert_eq!(cfg.max_concurrency, 4);
        assert_eq!(cfg.max_attempts, 3);
        assert!(cfg.strict_done_gate);
    }

    #[test]
    fn patch_overrides_defaults() {
        let mut cfg = OrchestratorConfig::default();
        cfg.apply_patch(OrchestratorConfigPatch {
            tick_interval_secs: Some(30),
            max_concurrency: Some(2),
            ..OrchestratorConfigPatch::default()
        });

        assert_eq!(cfg.tick_interval_secs, 30);
        assert_eq!(cfg.max_concurrency, 2);
        assert_eq!(cfg.max_attempts, 3);
    }

    #[test]
    fn workspace_config_overrides_global_config() {
        let temp = tempfile::tempdir().expect("tmp");
        let global = temp.path().join("global.toml");
        let workspace = temp.path().join("workspace.toml");

        std::fs::write(
            &global,
            r#"
tick_interval_secs = 300
max_concurrency = 2
"#,
        )
        .expect("write global");
        std::fs::write(
            &workspace,
            r#"
tick_interval_secs = 45
"#,
        )
        .expect("write workspace");

        let cfg = OrchestratorConfig::load_from_paths(Some(&global), Some(&workspace))
            .expect("load merged");

        assert_eq!(cfg.tick_interval_secs, 45);
        assert_eq!(cfg.max_concurrency, 2);
    }
}
