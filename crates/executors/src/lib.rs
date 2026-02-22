use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::Arc;

use async_trait::async_trait;
use aura_contracts::ExecutorKind;
use serde::{Deserialize, Serialize};
use thiserror::Error;
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
        cmd.args(parts.args)
            .current_dir(current_dir)
            .env("AURA_PROMPT", combined_prompt)
            .env(
                "AURA_COMMIT_REMINDER",
                merged_env.commit_reminder.to_string(),
            )
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .stdin(Stdio::null());

        if review_mode {
            cmd.env("AURA_REVIEW_MODE", "1");
        }

        if let Some(id) = session_id {
            cmd.env("AURA_SESSION_ID", id);
        }

        for (k, v) in &merged_env.vars {
            cmd.env(k, v);
        }

        let child = cmd.spawn()?;
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

pub mod adapters {
    use aura_contracts::ExecutorKind;

    use super::{
        AppendPrompt, CmdOverrides, CommandBackedExecutor, CommandExecutorConfig,
        ExecutorCapability, ExecutorProfileId,
    };

    #[derive(Debug, Clone, PartialEq, Eq, Default)]
    pub struct CodexOptions {
        pub append_prompt: AppendPrompt,
        pub model: Option<String>,
        pub sandbox: Option<String>,
        pub ask_for_approval: Option<String>,
        pub cmd_overrides: CmdOverrides,
    }

    pub fn codex(options: CodexOptions) -> CommandBackedExecutor {
        let mut params = vec!["app-server".to_string()];
        if let Some(model) = options.model {
            params.extend(["--model".to_string(), model]);
        }
        if let Some(sandbox) = options.sandbox {
            params.extend(["--sandbox".to_string(), sandbox]);
        }
        if let Some(policy) = options.ask_for_approval {
            params.extend(["--ask-for-approval".to_string(), policy]);
        }

        CommandBackedExecutor::new(CommandExecutorConfig {
            profile_id: ExecutorProfileId::new(ExecutorKind::Codex),
            base_command: "npx -y @openai/codex@0.86.0".to_string(),
            default_params: params,
            append_prompt: options.append_prompt,
            cmd_overrides: options.cmd_overrides,
            capabilities: vec![
                ExecutorCapability::SessionFork,
                ExecutorCapability::SetupHelper,
            ],
        })
    }

    pub fn codex_default() -> CommandBackedExecutor {
        codex(CodexOptions::default())
    }

    #[derive(Debug, Clone, PartialEq, Eq, Default)]
    pub struct ClaudeOptions {
        pub append_prompt: AppendPrompt,
        pub model: Option<String>,
        pub plan: bool,
        pub approvals: bool,
        pub dangerously_skip_permissions: bool,
        pub claude_code_router: bool,
        pub cmd_overrides: CmdOverrides,
    }

    pub fn claude(options: ClaudeOptions) -> CommandBackedExecutor {
        let base = if options.claude_code_router {
            "npx -y @musistudio/claude-code-router@1.0.66 code"
        } else {
            "npx -y @anthropic-ai/claude-code@2.1.12"
        };

        let mut params = vec![
            "-p".to_string(),
            "--verbose".to_string(),
            "--output-format=stream-json".to_string(),
            "--input-format=stream-json".to_string(),
            "--include-partial-messages".to_string(),
            "--disallowedTools=AskUserQuestion".to_string(),
        ];

        if options.plan || options.approvals {
            params.push("--permission-prompt-tool=stdio".to_string());
            params.push("--permission-mode=bypassPermissions".to_string());
        }
        if options.dangerously_skip_permissions {
            params.push("--dangerously-skip-permissions".to_string());
        }
        if let Some(model) = options.model {
            params.extend(["--model".to_string(), model]);
        }

        CommandBackedExecutor::new(CommandExecutorConfig {
            profile_id: ExecutorProfileId::new(ExecutorKind::Claude),
            base_command: base.to_string(),
            default_params: params,
            append_prompt: options.append_prompt,
            cmd_overrides: options.cmd_overrides,
            capabilities: vec![ExecutorCapability::SessionFork],
        })
    }

    pub fn claude_default() -> CommandBackedExecutor {
        claude(ClaudeOptions::default())
    }

    #[derive(Debug, Clone, PartialEq, Eq, Default)]
    pub struct CursorAgentOptions {
        pub append_prompt: AppendPrompt,
        pub force: bool,
        pub model: Option<String>,
        pub cmd_overrides: CmdOverrides,
    }

    pub fn cursor_agent(options: CursorAgentOptions) -> CommandBackedExecutor {
        let mut params = vec!["-p".to_string(), "--output-format=stream-json".to_string()];
        if options.force {
            params.push("--force".to_string());
        }
        if let Some(model) = options.model {
            params.extend(["--model".to_string(), model]);
        }

        CommandBackedExecutor::new(CommandExecutorConfig {
            profile_id: ExecutorProfileId::new(ExecutorKind::CursorAgent),
            base_command: "cursor-agent".to_string(),
            default_params: params,
            append_prompt: options.append_prompt,
            cmd_overrides: options.cmd_overrides,
            capabilities: vec![ExecutorCapability::SetupHelper],
        })
    }

    pub fn cursor_default() -> CommandBackedExecutor {
        cursor_agent(CursorAgentOptions::default())
    }

    #[derive(Debug, Clone, PartialEq, Eq, Default)]
    pub struct DroidOptions {
        pub append_prompt: AppendPrompt,
        pub autonomy: Option<String>,
        pub model: Option<String>,
        pub reasoning_effort: Option<String>,
        pub cmd_overrides: CmdOverrides,
    }

    pub fn droid(options: DroidOptions) -> CommandBackedExecutor {
        let mut params = vec![
            "exec".to_string(),
            "--output-format".to_string(),
            "stream-json".to_string(),
        ];
        match options.autonomy.as_deref() {
            Some("low") => params.extend(["--auto".to_string(), "low".to_string()]),
            Some("medium") => params.extend(["--auto".to_string(), "medium".to_string()]),
            Some("high") => params.extend(["--auto".to_string(), "high".to_string()]),
            Some("skip-permissions-unsafe") | None => {
                params.push("--skip-permissions-unsafe".to_string())
            }
            Some(_) => {}
        }
        if let Some(model) = options.model {
            params.extend(["--model".to_string(), model]);
        }
        if let Some(reasoning) = options.reasoning_effort {
            params.extend(["--reasoning-effort".to_string(), reasoning]);
        }

        CommandBackedExecutor::new(CommandExecutorConfig {
            profile_id: ExecutorProfileId::new(ExecutorKind::Droid),
            base_command: "droid".to_string(),
            default_params: params,
            append_prompt: options.append_prompt,
            cmd_overrides: options.cmd_overrides,
            capabilities: vec![ExecutorCapability::SessionFork],
        })
    }

    pub fn droid_default() -> CommandBackedExecutor {
        droid(DroidOptions::default())
    }

    #[derive(Debug, Clone, PartialEq, Eq, Default)]
    pub struct AmpOptions {
        pub append_prompt: AppendPrompt,
        pub dangerously_allow_all: bool,
        pub cmd_overrides: CmdOverrides,
    }

    pub fn amp(options: AmpOptions) -> CommandBackedExecutor {
        let mut params = vec!["--execute".to_string(), "--stream-json".to_string()];
        if options.dangerously_allow_all {
            params.push("--dangerously-allow-all".to_string());
        }
        CommandBackedExecutor::new(CommandExecutorConfig {
            profile_id: ExecutorProfileId::new(ExecutorKind::Amp),
            base_command: "npx -y @sourcegraph/amp@0.0.1764777697-g907e30".to_string(),
            default_params: params,
            append_prompt: options.append_prompt,
            cmd_overrides: options.cmd_overrides,
            capabilities: vec![ExecutorCapability::SessionFork],
        })
    }

    pub fn amp_default() -> CommandBackedExecutor {
        amp(AmpOptions::default())
    }

    #[derive(Debug, Clone, PartialEq, Eq, Default)]
    pub struct GeminiOptions {
        pub append_prompt: AppendPrompt,
        pub model: Option<String>,
        pub yolo: bool,
        pub cmd_overrides: CmdOverrides,
    }

    pub fn gemini(options: GeminiOptions) -> CommandBackedExecutor {
        let mut params = vec!["--experimental-acp".to_string()];
        if let Some(model) = options.model {
            params.extend(["--model".to_string(), model]);
        }
        if options.yolo {
            params.extend([
                "--yolo".to_string(),
                "--allowed-tools".to_string(),
                "run_shell_command".to_string(),
            ]);
        }
        CommandBackedExecutor::new(CommandExecutorConfig {
            profile_id: ExecutorProfileId::new(ExecutorKind::Gemini),
            base_command: "npx -y @google/gemini-cli@0.23.0".to_string(),
            default_params: params,
            append_prompt: options.append_prompt,
            cmd_overrides: options.cmd_overrides,
            capabilities: vec![ExecutorCapability::SessionFork],
        })
    }

    pub fn gemini_default() -> CommandBackedExecutor {
        gemini(GeminiOptions::default())
    }

    #[derive(Debug, Clone, PartialEq, Eq)]
    pub struct OpencodeOptions {
        pub append_prompt: AppendPrompt,
        pub model: Option<String>,
        pub variant: Option<String>,
        pub mode: Option<String>,
        pub auto_approve: bool,
        pub cmd_overrides: CmdOverrides,
    }

    impl Default for OpencodeOptions {
        fn default() -> Self {
            Self {
                append_prompt: AppendPrompt::default(),
                model: None,
                variant: None,
                mode: None,
                auto_approve: true,
                cmd_overrides: CmdOverrides::default(),
            }
        }
    }

    pub fn opencode(options: OpencodeOptions) -> CommandBackedExecutor {
        let mut params = vec![
            "serve".to_string(),
            "--hostname".to_string(),
            "127.0.0.1".to_string(),
            "--port".to_string(),
            "0".to_string(),
        ];
        if let Some(model) = options.model {
            params.extend(["--model".to_string(), model]);
        }
        if let Some(variant) = options.variant {
            params.extend(["--variant".to_string(), variant]);
        }
        if let Some(mode) = options.mode {
            params.extend(["--agent".to_string(), mode]);
        }

        let mut cmd_overrides = options.cmd_overrides;
        let mut env = cmd_overrides.env.unwrap_or_default();
        if !env.contains_key("OPENCODE_PERMISSION") {
            if options.auto_approve {
                env.insert(
                    "OPENCODE_PERMISSION".to_string(),
                    "{\"question\":\"deny\"}".to_string(),
                );
            } else {
                env.insert(
                    "OPENCODE_PERMISSION".to_string(),
                    "{\"edit\":\"ask\",\"bash\":\"ask\",\"webfetch\":\"ask\",\"doom_loop\":\"ask\",\"external_directory\":\"ask\",\"question\":\"deny\"}".to_string(),
                );
            }
        }
        cmd_overrides.env = Some(env);

        CommandBackedExecutor::new(CommandExecutorConfig {
            profile_id: ExecutorProfileId::new(ExecutorKind::Opencode),
            base_command: "npx -y opencode-ai@1.1.25".to_string(),
            default_params: params,
            append_prompt: options.append_prompt,
            cmd_overrides,
            capabilities: vec![ExecutorCapability::SessionFork],
        })
    }

    pub fn opencode_default() -> CommandBackedExecutor {
        opencode(OpencodeOptions::default())
    }

    #[derive(Debug, Clone, PartialEq, Eq, Default)]
    pub struct QwenCodeOptions {
        pub append_prompt: AppendPrompt,
        pub yolo: bool,
        pub cmd_overrides: CmdOverrides,
    }

    pub fn qwen_code(options: QwenCodeOptions) -> CommandBackedExecutor {
        let mut params = vec!["--experimental-acp".to_string()];
        if options.yolo {
            params.push("--yolo".to_string());
        }
        CommandBackedExecutor::new(CommandExecutorConfig {
            profile_id: ExecutorProfileId::new(ExecutorKind::QwenCode),
            base_command: "npx -y @qwen-code/qwen-code@0.2.1".to_string(),
            default_params: params,
            append_prompt: options.append_prompt,
            cmd_overrides: options.cmd_overrides,
            capabilities: vec![ExecutorCapability::SessionFork],
        })
    }

    pub fn qwen_code_default() -> CommandBackedExecutor {
        qwen_code(QwenCodeOptions::default())
    }

    #[derive(Debug, Clone, PartialEq, Eq, Default)]
    pub struct CopilotOptions {
        pub append_prompt: AppendPrompt,
        pub model: Option<String>,
        pub allow_all_tools: bool,
        pub allow_tool: Option<String>,
        pub deny_tool: Option<String>,
        pub add_dir: Option<Vec<String>>,
        pub disable_mcp_server: Option<Vec<String>>,
        pub cmd_overrides: CmdOverrides,
    }

    pub fn copilot(options: CopilotOptions) -> CommandBackedExecutor {
        let mut params = vec![
            "--no-color".to_string(),
            "--log-level".to_string(),
            "debug".to_string(),
        ];
        if options.allow_all_tools {
            params.push("--allow-all-tools".to_string());
        }
        if let Some(model) = options.model {
            params.extend(["--model".to_string(), model]);
        }
        if let Some(tool) = options.allow_tool {
            params.extend(["--allow-tool".to_string(), tool]);
        }
        if let Some(tool) = options.deny_tool {
            params.extend(["--deny-tool".to_string(), tool]);
        }
        if let Some(dirs) = options.add_dir {
            for dir in dirs {
                params.extend(["--add-dir".to_string(), dir]);
            }
        }
        if let Some(servers) = options.disable_mcp_server {
            for server in servers {
                params.extend(["--disable-mcp-server".to_string(), server]);
            }
        }
        CommandBackedExecutor::new(CommandExecutorConfig {
            profile_id: ExecutorProfileId::new(ExecutorKind::Copilot),
            base_command: "npx -y @github/copilot@0.0.375".to_string(),
            default_params: params,
            append_prompt: options.append_prompt,
            cmd_overrides: options.cmd_overrides,
            capabilities: vec![ExecutorCapability::SessionFork],
        })
    }

    pub fn copilot_default() -> CommandBackedExecutor {
        copilot(CopilotOptions::default())
    }

    pub fn custom_command(
        profile_id: ExecutorProfileId,
        base_command: impl Into<String>,
        default_params: Vec<String>,
        cmd_overrides: CmdOverrides,
        capabilities: Vec<ExecutorCapability>,
    ) -> CommandBackedExecutor {
        CommandBackedExecutor::new(CommandExecutorConfig {
            profile_id,
            base_command: base_command.into(),
            default_params,
            append_prompt: AppendPrompt::default(),
            cmd_overrides,
            capabilities,
        })
    }
}

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
        let opencode = adapters::opencode_default();
        let qwen = adapters::qwen_code_default();
        let copilot = adapters::copilot_default();

        assert!(matches!(amp.profile_id().executor, ExecutorKind::Amp));
        assert!(matches!(gemini.profile_id().executor, ExecutorKind::Gemini));
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
}
