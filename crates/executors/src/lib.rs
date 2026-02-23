use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::Arc;

use async_trait::async_trait;
use aura_contracts::ExecutorKind;
use serde::{Deserialize, Serialize};
use thiserror::Error;
use tokio::io::AsyncWriteExt;
use tokio::process::{Child, Command};
use tokio::sync::{RwLock, oneshot};

#[derive(Debug, Error)]
pub enum ExecutorError {
    #[error("invalid command: {0}")]
    InvalidCommand(String),
    #[error("unknown executor profile: {0}")]
    UnknownExecutorProfile(String),
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
    #[error("approval denied for command: {0}")]
    ApprovalDenied(String),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExecutorExitResult {
    Success,
    Failure,
}

pub type ExecutorExitSignal = oneshot::Receiver<ExecutorExitResult>;
pub type InterruptSender = oneshot::Sender<()>;

#[derive(Debug)]
pub struct SpawnedChild {
    pub child: Child,
    pub exit_signal: Option<ExecutorExitSignal>,
    pub interrupt_sender: Option<InterruptSender>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RepoContext {
    pub workspace_root: PathBuf,
    pub repo_names: Vec<String>,
}

impl RepoContext {
    pub fn repo_paths(&self) -> Vec<PathBuf> {
        self.repo_names
            .iter()
            .map(|name| self.workspace_root.join(name))
            .collect()
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExecutionEnv {
    pub vars: HashMap<String, String>,
    pub repo_context: RepoContext,
    pub commit_reminder: bool,
}

impl ExecutionEnv {
    pub fn new(repo_context: RepoContext, commit_reminder: bool) -> Self {
        Self {
            vars: HashMap::new(),
            repo_context,
            commit_reminder,
        }
    }

    pub fn insert(&mut self, key: impl Into<String>, value: impl Into<String>) {
        self.vars.insert(key.into(), value.into());
    }

    pub fn merge(&mut self, other: &HashMap<String, String>) {
        self.vars
            .extend(other.iter().map(|(k, v)| (k.clone(), v.clone())));
    }

    pub fn with_overrides(mut self, other: &HashMap<String, String>) -> Self {
        self.merge(other);
        self
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Hash)]
pub struct ExecutorProfileId {
    pub executor: ExecutorKind,
    pub variant: Option<String>,
}

impl ExecutorProfileId {
    pub fn new(executor: ExecutorKind) -> Self {
        Self {
            executor,
            variant: None,
        }
    }

    pub fn cache_key(&self) -> String {
        match &self.variant {
            Some(v) => format!("{:?}:{v}", self.executor),
            None => format!("{:?}:DEFAULT", self.executor),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct CmdOverrides {
    pub base_command_override: Option<String>,
    pub additional_params: Option<Vec<String>>,
    pub env: Option<HashMap<String, String>>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(transparent)]
pub struct AppendPrompt(pub Option<String>);

impl AppendPrompt {
    pub fn combine_prompt(&self, prompt: &str) -> String {
        match &self.0 {
            Some(extra) => format!("{prompt}{extra}"),
            None => prompt.to_string(),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum PromptInputMode {
    #[default]
    EnvVar,
    Arg,
    Stdin,
}

#[derive(Debug, Clone)]
pub struct CommandParts {
    pub program: String,
    pub args: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CommandBuilder {
    pub base: String,
    pub params: Vec<String>,
}

impl CommandBuilder {
    pub fn new(base: impl Into<String>) -> Self {
        Self {
            base: base.into(),
            params: Vec::new(),
        }
    }

    pub fn params<I, S>(mut self, params: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        self.params.extend(params.into_iter().map(Into::into));
        self
    }

    pub fn override_base(mut self, base: impl Into<String>) -> Self {
        self.base = base.into();
        self
    }

    pub fn extend_params<I, S>(mut self, params: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        self.params.extend(params.into_iter().map(Into::into));
        self
    }

    pub fn build_initial(&self) -> Result<CommandParts, ExecutorError> {
        self.build(&[])
    }

    pub fn build_follow_up(
        &self,
        additional_args: &[String],
    ) -> Result<CommandParts, ExecutorError> {
        self.build(additional_args)
    }

    fn build(&self, additional_args: &[String]) -> Result<CommandParts, ExecutorError> {
        let mut parts = shlex::split(&self.base)
            .ok_or_else(|| ExecutorError::InvalidCommand(self.base.clone()))?;
        parts.extend(self.params.clone());
        parts.extend(additional_args.to_vec());

        if parts.is_empty() {
            return Err(ExecutorError::InvalidCommand("empty command".to_string()));
        }

        let program = parts.remove(0);
        Ok(CommandParts {
            program,
            args: parts,
        })
    }
}

pub fn apply_overrides(mut builder: CommandBuilder, overrides: &CmdOverrides) -> CommandBuilder {
    if let Some(base) = &overrides.base_command_override {
        builder = builder.override_base(base.clone());
    }
    if let Some(extra) = &overrides.additional_params {
        builder = builder.extend_params(extra.clone());
    }
    builder
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ExecutorCapability {
    SessionFork,
    SetupHelper,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum AvailabilityInfo {
    LoginDetected { last_auth_timestamp: i64 },
    InstallationFound,
    NotFound,
}

impl AvailabilityInfo {
    pub fn is_available(&self) -> bool {
        matches!(self, Self::LoginDetected { .. } | Self::InstallationFound)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CodingAgentInitialRequest {
    pub prompt: String,
    pub executor_profile_id: ExecutorProfileId,
    pub working_dir: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CodingAgentFollowUpRequest {
    pub prompt: String,
    pub session_id: String,
    pub executor_profile_id: ExecutorProfileId,
    pub working_dir: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReviewRequest {
    pub prompt: String,
    pub session_id: Option<String>,
    pub executor_profile_id: ExecutorProfileId,
    pub working_dir: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ScriptRequest {
    pub script: String,
    pub language: ScriptLanguage,
    pub working_dir: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ScriptLanguage {
    Bash,
    Sh,
    Zsh,
    Pwsh,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ExecutorActionType {
    CodingAgentInitialRequest(CodingAgentInitialRequest),
    CodingAgentFollowUpRequest(CodingAgentFollowUpRequest),
    ReviewRequest(ReviewRequest),
    ScriptRequest(ScriptRequest),
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExecutorAction {
    pub typ: ExecutorActionType,
    pub next_action: Option<Box<ExecutorAction>>,
}

impl ExecutorAction {
    pub fn new(typ: ExecutorActionType, next_action: Option<Box<ExecutorAction>>) -> Self {
        Self { typ, next_action }
    }

    pub fn append_action(mut self, action: ExecutorAction) -> Self {
        if let Some(next) = self.next_action.take() {
            self.next_action = Some(Box::new(next.append_action(action)));
        } else {
            self.next_action = Some(Box::new(action));
        }
        self
    }
}

#[async_trait]
pub trait ExecutorApprovalService: Send + Sync {
    async fn request_approval(&self, program: &str, args: &[String])
    -> Result<bool, ExecutorError>;
}

#[derive(Debug, Clone, Default)]
pub struct NoopApprovalService;

#[async_trait]
impl ExecutorApprovalService for NoopApprovalService {
    async fn request_approval(
        &self,
        _program: &str,
        _args: &[String],
    ) -> Result<bool, ExecutorError> {
        Ok(true)
    }
}

#[async_trait]
pub trait StandardCodingAgentExecutor: Send + Sync {
    fn profile_id(&self) -> &ExecutorProfileId;
    fn capabilities(&self) -> Vec<ExecutorCapability>;
    fn availability(&self) -> AvailabilityInfo;

    async fn spawn_initial(
        &self,
        current_dir: &Path,
        prompt: &str,
        env: &ExecutionEnv,
    ) -> Result<SpawnedChild, ExecutorError>;

    async fn spawn_follow_up(
        &self,
        current_dir: &Path,
        prompt: &str,
        session_id: &str,
        env: &ExecutionEnv,
    ) -> Result<SpawnedChild, ExecutorError>;

    async fn spawn_review(
        &self,
        current_dir: &Path,
        prompt: &str,
        session_id: Option<&str>,
        env: &ExecutionEnv,
    ) -> Result<SpawnedChild, ExecutorError>;

    fn normalize_logs(&self, raw_logs: &str) -> String;
}

#[async_trait]
pub trait ExecutorRegistry: Send + Sync {
    async fn register(&self, executor: Arc<dyn StandardCodingAgentExecutor>);
    async fn get(
        &self,
        profile_id: &ExecutorProfileId,
    ) -> Option<Arc<dyn StandardCodingAgentExecutor>>;
}

#[derive(Default)]
pub struct InMemoryExecutorRegistry {
    executors: RwLock<HashMap<String, Arc<dyn StandardCodingAgentExecutor>>>,
}

impl InMemoryExecutorRegistry {
    pub fn new() -> Self {
        Self::default()
    }
}

#[async_trait]
impl ExecutorRegistry for InMemoryExecutorRegistry {
    async fn register(&self, executor: Arc<dyn StandardCodingAgentExecutor>) {
        self.executors
            .write()
            .await
            .insert(executor.profile_id().cache_key(), executor);
    }

    async fn get(
        &self,
        profile_id: &ExecutorProfileId,
    ) -> Option<Arc<dyn StandardCodingAgentExecutor>> {
        self.executors
            .read()
            .await
            .get(&profile_id.cache_key())
            .cloned()
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CommandExecutorConfig {
    pub profile_id: ExecutorProfileId,
    pub base_command: String,
    pub default_params: Vec<String>,
    pub append_prompt: AppendPrompt,
    #[serde(default)]
    pub prompt_input_mode: PromptInputMode,
    pub cmd_overrides: CmdOverrides,
    pub capabilities: Vec<ExecutorCapability>,
}

impl CommandExecutorConfig {
    pub fn command_builder(&self) -> CommandBuilder {
        let builder =
            CommandBuilder::new(self.base_command.clone()).params(self.default_params.clone());
        apply_overrides(builder, &self.cmd_overrides)
    }
}

#[derive(Debug, Clone)]
pub struct CommandBackedExecutor {
    config: CommandExecutorConfig,
}

impl CommandBackedExecutor {
    pub fn new(config: CommandExecutorConfig) -> Self {
        Self { config }
    }

    async fn spawn_with_session(
        &self,
        current_dir: &Path,
        prompt: &str,
        env: &ExecutionEnv,
        session_id: Option<&str>,
        review_mode: bool,
    ) -> Result<SpawnedChild, ExecutorError> {
        let mut merged_env = env.clone();
        if let Some(override_env) = &self.config.cmd_overrides.env {
            merged_env.merge(override_env);
        }
        let combined_prompt = self.config.append_prompt.combine_prompt(prompt);

        let parts = self.config.command_builder().build_initial()?;
        let mut cmd = Command::new(parts.program);
        cmd.args(parts.args).current_dir(current_dir).env(
            "AURA_COMMIT_REMINDER",
            merged_env.commit_reminder.to_string(),
        );

        match self.config.prompt_input_mode {
            PromptInputMode::EnvVar => {
                cmd.env("AURA_PROMPT", combined_prompt.clone())
                    .stdin(Stdio::null());
            }
            PromptInputMode::Arg => {
                cmd.env("AURA_PROMPT", combined_prompt.clone())
                    .arg(combined_prompt.clone())
                    .stdin(Stdio::null());
            }
            PromptInputMode::Stdin => {
                cmd.env("AURA_PROMPT", combined_prompt.clone())
                    .stdin(Stdio::piped());
            }
        }

        cmd.stdout(Stdio::piped()).stderr(Stdio::piped());

        if review_mode {
            cmd.env("AURA_REVIEW_MODE", "1");
        }

        if let Some(id) = session_id {
            cmd.env("AURA_SESSION_ID", id);
        }

        for (k, v) in &merged_env.vars {
            cmd.env(k, v);
        }

        let mut child = cmd.spawn()?;
        if matches!(self.config.prompt_input_mode, PromptInputMode::Stdin)
            && let Some(mut stdin) = child.stdin.take()
        {
            tokio::spawn(async move {
                let _ = stdin.write_all(combined_prompt.as_bytes()).await;
                let _ = stdin.write_all(b"\n").await;
                let _ = stdin.shutdown().await;
            });
        }
        Ok(SpawnedChild {
            child,
            exit_signal: None,
            interrupt_sender: None,
        })
    }
}

#[async_trait]
impl StandardCodingAgentExecutor for CommandBackedExecutor {
    fn profile_id(&self) -> &ExecutorProfileId {
        &self.config.profile_id
    }

    fn capabilities(&self) -> Vec<ExecutorCapability> {
        self.config.capabilities.clone()
    }

    fn availability(&self) -> AvailabilityInfo {
        AvailabilityInfo::InstallationFound
    }

    async fn spawn_initial(
        &self,
        current_dir: &Path,
        prompt: &str,
        env: &ExecutionEnv,
    ) -> Result<SpawnedChild, ExecutorError> {
        self.spawn_with_session(current_dir, prompt, env, None, false)
            .await
    }

    async fn spawn_follow_up(
        &self,
        current_dir: &Path,
        prompt: &str,
        session_id: &str,
        env: &ExecutionEnv,
    ) -> Result<SpawnedChild, ExecutorError> {
        self.spawn_with_session(current_dir, prompt, env, Some(session_id), false)
            .await
    }

    async fn spawn_review(
        &self,
        current_dir: &Path,
        prompt: &str,
        session_id: Option<&str>,
        env: &ExecutionEnv,
    ) -> Result<SpawnedChild, ExecutorError> {
        self.spawn_with_session(current_dir, prompt, env, session_id, true)
            .await
    }

    fn normalize_logs(&self, raw_logs: &str) -> String {
        raw_logs
            .lines()
            .map(str::trim_end)
            .collect::<Vec<_>>()
            .join("\n")
    }
}

pub mod adapters;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn command_builder_applies_overrides() {
        let builder = CommandBuilder::new("echo").params(["hello"]);
        let overrides = CmdOverrides {
            base_command_override: Some("printf".to_string()),
            additional_params: Some(vec!["%s".to_string(), "world".to_string()]),
            env: None,
        };

        let parts = apply_overrides(builder, &overrides)
            .build_initial()
            .expect("build command");

        assert_eq!(parts.program, "printf");
        assert_eq!(parts.args, vec!["hello", "%s", "world"]);
    }

    #[test]
    fn execution_env_merges_overrides() {
        let mut env = ExecutionEnv::new(
            RepoContext {
                workspace_root: PathBuf::from("/tmp/work"),
                repo_names: vec!["a".to_string()],
            },
            false,
        );
        env.insert("FOO", "one");
        let mut other = HashMap::new();
        other.insert("FOO".to_string(), "two".to_string());
        other.insert("BAR".to_string(), "three".to_string());
        env.merge(&other);

        assert_eq!(env.vars.get("FOO"), Some(&"two".to_string()));
        assert_eq!(env.vars.get("BAR"), Some(&"three".to_string()));
    }

    #[tokio::test]
    async fn prompt_mode_arg_passes_prompt_as_argument() {
        let executor = CommandBackedExecutor::new(CommandExecutorConfig {
            profile_id: ExecutorProfileId::new(ExecutorKind::Custom("arg-test".to_string())),
            base_command: "python".to_string(),
            default_params: vec![
                "-c".to_string(),
                "import sys; print(sys.argv[1])".to_string(),
            ],
            append_prompt: AppendPrompt::default(),
            prompt_input_mode: PromptInputMode::Arg,
            cmd_overrides: CmdOverrides::default(),
            capabilities: vec![],
        });

        let child = executor
            .spawn_initial(
                Path::new("."),
                "hello-from-arg",
                &ExecutionEnv::new(
                    RepoContext {
                        workspace_root: PathBuf::from("."),
                        repo_names: vec![],
                    },
                    false,
                ),
            )
            .await
            .expect("spawn")
            .child;

        let output = child.wait_with_output().await.expect("wait");
        let stdout = String::from_utf8_lossy(&output.stdout);
        assert_eq!(stdout.trim(), "hello-from-arg");
    }

    #[tokio::test]
    async fn prompt_mode_stdin_writes_prompt_to_stdin() {
        let executor = CommandBackedExecutor::new(CommandExecutorConfig {
            profile_id: ExecutorProfileId::new(ExecutorKind::Custom("stdin-test".to_string())),
            base_command: "sh".to_string(),
            default_params: vec!["-lc".to_string(), "cat".to_string()],
            append_prompt: AppendPrompt::default(),
            prompt_input_mode: PromptInputMode::Stdin,
            cmd_overrides: CmdOverrides::default(),
            capabilities: vec![],
        });

        let child = executor
            .spawn_initial(
                Path::new("."),
                "hello-from-stdin",
                &ExecutionEnv::new(
                    RepoContext {
                        workspace_root: PathBuf::from("."),
                        repo_names: vec![],
                    },
                    false,
                ),
            )
            .await
            .expect("spawn")
            .child;

        let output = child.wait_with_output().await.expect("wait");
        let stdout = String::from_utf8_lossy(&output.stdout);
        assert_eq!(stdout.trim(), "hello-from-stdin");
    }

    #[tokio::test]
    async fn follow_up_sets_session_env() {
        let executor = adapters::custom_command(
            ExecutorProfileId::new(ExecutorKind::Custom("test".to_string())),
            "sh",
            vec![
                "-lc".to_string(),
                "printf %s \"$AURA_SESSION_ID\"".to_string(),
            ],
            CmdOverrides::default(),
            vec![ExecutorCapability::SessionFork],
        );

        let child = executor
            .spawn_follow_up(
                Path::new("."),
                "prompt",
                "session-123",
                &ExecutionEnv::new(
                    RepoContext {
                        workspace_root: PathBuf::from("."),
                        repo_names: vec![],
                    },
                    false,
                ),
            )
            .await
            .expect("spawn")
            .child;

        let output = child.wait_with_output().await.expect("wait");
        let stdout = String::from_utf8_lossy(&output.stdout);
        assert_eq!(stdout, "session-123");
    }

    #[test]
    fn adapters_cover_reference_providers() {
        let amp = adapters::amp_default();
        let gemini = adapters::gemini_default();
        let local = adapters::local_default();
        let opencode = adapters::opencode_default();
        let qwen = adapters::qwen_code_default();
        let copilot = adapters::copilot_default();

        assert!(matches!(amp.profile_id().executor, ExecutorKind::Amp));
        assert!(matches!(gemini.profile_id().executor, ExecutorKind::Gemini));
        assert!(matches!(local.profile_id().executor, ExecutorKind::Local));
        assert!(matches!(
            opencode.profile_id().executor,
            ExecutorKind::Opencode
        ));
        assert!(matches!(qwen.profile_id().executor, ExecutorKind::QwenCode));
        assert!(matches!(
            copilot.profile_id().executor,
            ExecutorKind::Copilot
        ));
    }

    #[tokio::test]
    async fn codex_adapter_uses_executor_path_with_prompt_argument() {
        let executor = adapters::codex(adapters::CodexOptions {
            append_prompt: AppendPrompt::default(),
            model: None,
            sandbox: None,
            ask_for_approval: None,
            cmd_overrides: CmdOverrides {
                base_command_override: Some("echo".to_string()),
                additional_params: None,
                env: None,
            },
        });

        let child = executor
            .spawn_initial(
                Path::new("."),
                "hello-codex",
                &ExecutionEnv::new(
                    RepoContext {
                        workspace_root: PathBuf::from("."),
                        repo_names: vec![],
                    },
                    false,
                ),
            )
            .await
            .expect("spawn")
            .child;

        let output = child.wait_with_output().await.expect("wait");
        let stdout = String::from_utf8_lossy(&output.stdout);
        assert!(stdout.contains("exec"));
        assert!(stdout.contains("--json"));
        assert!(stdout.contains("hello-codex"));
    }

    #[tokio::test]
    async fn gemini_adapter_passes_prompt_as_argument_without_acp_mode() {
        let executor = adapters::gemini(adapters::GeminiOptions {
            append_prompt: AppendPrompt::default(),
            model: Some("gemini-2.5-pro".to_string()),
            yolo: false,
            cmd_overrides: CmdOverrides {
                base_command_override: Some("echo".to_string()),
                additional_params: None,
                env: None,
            },
        });

        let child = executor
            .spawn_initial(
                Path::new("."),
                "hello-gemini",
                &ExecutionEnv::new(
                    RepoContext {
                        workspace_root: PathBuf::from("."),
                        repo_names: vec![],
                    },
                    false,
                ),
            )
            .await
            .expect("spawn")
            .child;

        let output = child.wait_with_output().await.expect("wait");
        let stdout = String::from_utf8_lossy(&output.stdout);
        assert!(stdout.contains("--output-format=stream-json"));
        assert!(stdout.contains("--model gemini-2.5-pro"));
        assert!(stdout.contains("hello-gemini"));
        assert!(!stdout.contains("--experimental-acp"));
    }

    #[tokio::test]
    async fn claude_adapter_passes_prompt_as_argument_without_stream_json_input_mode() {
        let executor = adapters::claude(adapters::ClaudeOptions {
            append_prompt: AppendPrompt::default(),
            model: Some("claude-sonnet-4-5".to_string()),
            plan: false,
            approvals: false,
            dangerously_skip_permissions: false,
            claude_code_router: false,
            cmd_overrides: CmdOverrides {
                base_command_override: Some("echo".to_string()),
                additional_params: None,
                env: None,
            },
        });

        let child = executor
            .spawn_initial(
                Path::new("."),
                "hello-claude",
                &ExecutionEnv::new(
                    RepoContext {
                        workspace_root: PathBuf::from("."),
                        repo_names: vec![],
                    },
                    false,
                ),
            )
            .await
            .expect("spawn")
            .child;

        let output = child.wait_with_output().await.expect("wait");
        let stdout = String::from_utf8_lossy(&output.stdout);
        assert!(stdout.contains("-p"));
        assert!(stdout.contains("--output-format=stream-json"));
        assert!(stdout.contains("--model claude-sonnet-4-5"));
        assert!(stdout.contains("hello-claude"));
        assert!(!stdout.contains("--input-format=stream-json"));
    }
}
