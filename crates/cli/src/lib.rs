use std::collections::{HashMap, VecDeque};
use std::io::{self, IsTerminal, Write};
use std::path::PathBuf;
use std::time::{Duration, Instant};

use aura_contracts::ExecutorKind;
use aura_executors::adapters::{
    AmpOptions, ClaudeOptions, CodexOptions, CopilotOptions, CursorAgentOptions, DroidOptions,
    GeminiOptions, OpencodeOptions, QwenCodeOptions,
};
use aura_executors::{
    AppendPrompt, CmdOverrides, ExecutionEnv, ExecutorError, ExecutorProfileId, RepoContext,
    SpawnedChild, StandardCodingAgentExecutor, adapters,
};
use serde_json::Value;
use thiserror::Error;
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::sync::mpsc;

#[derive(Debug, Clone)]
pub struct RunOptions {
    pub executor: ExecutorKind,
    pub prompt: String,
    pub cwd: PathBuf,
    pub session_id: Option<String>,
    pub review: bool,
    pub env_vars: HashMap<String, String>,
    pub base_command_override: Option<String>,
    pub additional_params: Vec<String>,
    pub append_prompt: Option<String>,
    pub model: Option<String>,
    pub yolo: bool,
    pub force: bool,
    pub trust: bool,
    pub auto_approve: bool,
    pub allow_all_tools: bool,
    pub tui: bool,
}

impl Default for RunOptions {
    fn default() -> Self {
        Self {
            executor: ExecutorKind::Codex,
            prompt: String::new(),
            cwd: std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")),
            session_id: None,
            review: false,
            env_vars: HashMap::new(),
            base_command_override: None,
            additional_params: Vec::new(),
            append_prompt: None,
            model: None,
            yolo: false,
            force: false,
            trust: false,
            auto_approve: true,
            allow_all_tools: false,
            tui: true,
        }
    }
}

#[derive(Debug, Clone)]
pub struct RunOutcome {
    pub exit_code: Option<i32>,
    pub success: bool,
}

#[derive(Debug, Error)]
pub enum CliError {
    #[error("argument error: {0}")]
    Arg(String),
    #[error(transparent)]
    Executor(#[from] ExecutorError),
    #[error("runtime error: {0}")]
    Runtime(String),
    #[error("io error: {0}")]
    Io(#[from] io::Error),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LogStream {
    Stdout,
    Stderr,
}

pub trait LogSink {
    fn on_start(&mut self, options: &RunOptions);
    fn on_line(&mut self, stream: LogStream, line: &str);
    fn on_status(&mut self, message: &str);
    fn on_exit(&mut self, code: Option<i32>);
}

pub struct PlainLogSink;

impl LogSink for PlainLogSink {
    fn on_start(&mut self, options: &RunOptions) {
        let _ = writeln!(
            io::stdout(),
            "[aura] running {:?} in {}",
            options.executor,
            options.cwd.display()
        );
    }

    fn on_line(&mut self, stream: LogStream, line: &str) {
        let prefix = match stream {
            LogStream::Stdout => "stdout",
            LogStream::Stderr => "stderr",
        };
        let _ = writeln!(io::stdout(), "[{prefix}] {line}");
    }

    fn on_status(&mut self, message: &str) {
        let _ = writeln!(io::stdout(), "[status] {message}");
    }

    fn on_exit(&mut self, code: Option<i32>) {
        let _ = writeln!(io::stdout(), "[aura] process exited with {:?}", code);
    }
}

pub struct TuiLogSink {
    started_at: Instant,
    lines: VecDeque<(LogStream, String)>,
    max_lines: usize,
    title: String,
}

impl TuiLogSink {
    pub fn new() -> Self {
        Self {
            started_at: Instant::now(),
            lines: VecDeque::new(),
            max_lines: 200,
            title: "AURA Executor".to_string(),
        }
    }

    fn redraw(&mut self, status: &str) {
        let mut stdout = io::stdout();
        let _ = write!(stdout, "\x1b[2J\x1b[H");
        let _ = writeln!(stdout, "{}", self.title);
        let _ = writeln!(stdout, "uptime: {:.1?}", self.started_at.elapsed());
        let _ = writeln!(stdout, "status: {status}");
        let _ = writeln!(stdout, "{}", "-".repeat(72));

        let mut term_lines = std::env::var("LINES")
            .ok()
            .and_then(|v| v.parse::<usize>().ok())
            .unwrap_or(35);
        if term_lines < 12 {
            term_lines = 12;
        }
        let visible = term_lines.saturating_sub(6);

        for (stream, line) in self.lines.iter().rev().take(visible).rev() {
            match stream {
                LogStream::Stdout => {
                    let _ = writeln!(stdout, "\x1b[32mOUT\x1b[0m {line}");
                }
                LogStream::Stderr => {
                    let _ = writeln!(stdout, "\x1b[31mERR\x1b[0m {line}");
                }
            }
        }

        let _ = stdout.flush();
    }
}

impl Default for TuiLogSink {
    fn default() -> Self {
        Self::new()
    }
}

impl LogSink for TuiLogSink {
    fn on_start(&mut self, options: &RunOptions) {
        self.started_at = Instant::now();
        self.title = format!("AURA Executor: {:?}", options.executor);
        self.redraw("starting");
    }

    fn on_line(&mut self, stream: LogStream, line: &str) {
        self.lines.push_back((stream, line.to_string()));
        while self.lines.len() > self.max_lines {
            let _ = self.lines.pop_front();
        }
        self.redraw("running");
    }

    fn on_status(&mut self, message: &str) {
        self.redraw(message);
    }

    fn on_exit(&mut self, code: Option<i32>) {
        self.redraw(&format!("finished (exit={code:?})"));
    }
}

#[derive(Debug, Clone)]
pub enum CliCommand {
    Run(RunOptions),
    Help,
}

pub fn usage() -> &'static str {
    "Usage:\n  aura run --executor <name> --prompt <text> [options]\n  aura tui --executor <name> --prompt <text> [options]\n  aura help\n\nExecutors:\n  codex | claude | cursor-agent | droid | amp | gemini | opencode | qwen-code | copilot | custom\n\nOptions:\n  --cwd <path>\n  --session <id>\n  --review\n  --var KEY=VALUE      (repeatable)\n  --base-command <cmd>\n  --param <arg>        (repeatable)\n  --append-prompt <text>\n  --model <name>\n  --yolo\n  --force | -f\n  --trust\n  --auto-approve <true|false>\n  --allow-all-tools\n  --no-tui\n"
}

pub fn parse_cli_args(args: &[String]) -> Result<CliCommand, CliError> {
    if args.is_empty() {
        return Ok(CliCommand::Help);
    }

    match args[0].as_str() {
        "help" | "--help" | "-h" => Ok(CliCommand::Help),
        "run" => Ok(CliCommand::Run(parse_run_options(args)?)),
        "tui" => {
            let mut options = parse_run_options(args)?;
            options.tui = true;
            Ok(CliCommand::Run(options))
        }
        other => Err(CliError::Arg(format!("unknown command: {other}"))),
    }
}

fn parse_run_options(args: &[String]) -> Result<RunOptions, CliError> {
    let mut opts = RunOptions::default();
    let mut i = 1;
    while i < args.len() {
        let key = &args[i];
        match key.as_str() {
            "--executor" => {
                i += 1;
                let value = args
                    .get(i)
                    .ok_or_else(|| CliError::Arg("missing value for --executor".to_string()))?;
                opts.executor = parse_executor(value)?;
            }
            "--prompt" => {
                i += 1;
                opts.prompt = args
                    .get(i)
                    .ok_or_else(|| CliError::Arg("missing value for --prompt".to_string()))?
                    .clone();
            }
            "--cwd" => {
                i += 1;
                opts.cwd = PathBuf::from(
                    args.get(i)
                        .ok_or_else(|| CliError::Arg("missing value for --cwd".to_string()))?,
                );
            }
            "--session" => {
                i += 1;
                opts.session_id = Some(
                    args.get(i)
                        .ok_or_else(|| CliError::Arg("missing value for --session".to_string()))?
                        .clone(),
                );
            }
            "--review" => opts.review = true,
            "--var" => {
                i += 1;
                let pair = args
                    .get(i)
                    .ok_or_else(|| CliError::Arg("missing value for --var".to_string()))?;
                let mut parts = pair.splitn(2, '=');
                let key = parts.next().unwrap_or("").trim();
                let value = parts.next().unwrap_or("").to_string();
                if key.is_empty() {
                    return Err(CliError::Arg("--var expects KEY=VALUE format".to_string()));
                }
                opts.env_vars.insert(key.to_string(), value);
            }
            "--base-command" => {
                i += 1;
                opts.base_command_override = Some(
                    args.get(i)
                        .ok_or_else(|| {
                            CliError::Arg("missing value for --base-command".to_string())
                        })?
                        .clone(),
                );
            }
            "--param" => {
                i += 1;
                opts.additional_params.push(
                    args.get(i)
                        .ok_or_else(|| CliError::Arg("missing value for --param".to_string()))?
                        .clone(),
                );
            }
            "--append-prompt" => {
                i += 1;
                opts.append_prompt = Some(
                    args.get(i)
                        .ok_or_else(|| {
                            CliError::Arg("missing value for --append-prompt".to_string())
                        })?
                        .clone(),
                );
            }
            "--model" => {
                i += 1;
                opts.model = Some(
                    args.get(i)
                        .ok_or_else(|| CliError::Arg("missing value for --model".to_string()))?
                        .clone(),
                );
            }
            "--yolo" => opts.yolo = true,
            "--force" | "-f" => opts.force = true,
            "--trust" => opts.trust = true,
            "--allow-all-tools" => opts.allow_all_tools = true,
            "--auto-approve" => {
                i += 1;
                let value = args
                    .get(i)
                    .ok_or_else(|| CliError::Arg("missing value for --auto-approve".to_string()))?;
                opts.auto_approve = match value.as_str() {
                    "true" => true,
                    "false" => false,
                    _ => {
                        return Err(CliError::Arg(
                            "--auto-approve expects true|false".to_string(),
                        ));
                    }
                }
            }
            "--no-tui" => opts.tui = false,
            other => {
                return Err(CliError::Arg(format!("unknown argument: {other}")));
            }
        }
        i += 1;
    }

    if opts.prompt.trim().is_empty() {
        return Err(CliError::Arg("--prompt is required".to_string()));
    }

    Ok(opts)
}

fn parse_executor(raw: &str) -> Result<ExecutorKind, CliError> {
    match raw.to_ascii_lowercase().as_str() {
        "codex" => Ok(ExecutorKind::Codex),
        "claude" => Ok(ExecutorKind::Claude),
        "cursor" | "cursor-agent" => Ok(ExecutorKind::CursorAgent),
        "droid" => Ok(ExecutorKind::Droid),
        "amp" => Ok(ExecutorKind::Amp),
        "gemini" => Ok(ExecutorKind::Gemini),
        "opencode" => Ok(ExecutorKind::Opencode),
        "qwen" | "qwen-code" => Ok(ExecutorKind::QwenCode),
        "copilot" => Ok(ExecutorKind::Copilot),
        "custom" => Ok(ExecutorKind::Custom("custom".to_string())),
        _ => Err(CliError::Arg(format!("unsupported executor: {raw}"))),
    }
}

pub async fn run_with_default_sink(options: RunOptions) -> Result<RunOutcome, CliError> {
    let use_tui = options.tui && io::stdout().is_terminal();
    if use_tui {
        let mut sink = TuiLogSink::new();
        run_executor(options, &mut sink).await
    } else {
        let mut sink = PlainLogSink;
        run_executor(options, &mut sink).await
    }
}

pub async fn run_executor(
    options: RunOptions,
    sink: &mut dyn LogSink,
) -> Result<RunOutcome, CliError> {
    sink.on_start(&options);
    if matches!(options.executor, ExecutorKind::CursorAgent)
        && !(options.force || options.trust || options.yolo)
    {
        sink.on_status("cursor may require trust; rerun with --trust (or --yolo/-f)");
    }

    let mut env = ExecutionEnv::new(
        RepoContext {
            workspace_root: options.cwd.clone(),
            repo_names: Vec::new(),
        },
        false,
    );
    for (k, v) in &options.env_vars {
        env.insert(k.clone(), v.clone());
    }

    let executor = build_executor(&options);

    let spawned = if options.review {
        executor
            .spawn_review(
                &options.cwd,
                &options.prompt,
                options.session_id.as_deref(),
                &env,
            )
            .await?
    } else if let Some(session_id) = &options.session_id {
        executor
            .spawn_follow_up(&options.cwd, &options.prompt, session_id, &env)
            .await?
    } else {
        executor
            .spawn_initial(&options.cwd, &options.prompt, &env)
            .await?
    };

    stream_spawned(options.executor.clone(), spawned, sink).await
}

fn build_executor(options: &RunOptions) -> Box<dyn StandardCodingAgentExecutor> {
    let cmd_overrides = CmdOverrides {
        base_command_override: options.base_command_override.clone(),
        additional_params: if options.additional_params.is_empty() {
            None
        } else {
            Some(options.additional_params.clone())
        },
        env: None,
    };

    let append_prompt = AppendPrompt(options.append_prompt.clone());

    match options.executor {
        ExecutorKind::Codex => Box::new(adapters::codex(CodexOptions {
            append_prompt,
            model: options.model.clone(),
            sandbox: None,
            ask_for_approval: None,
            cmd_overrides,
        })),
        ExecutorKind::Claude => Box::new(adapters::claude(ClaudeOptions {
            append_prompt,
            model: options.model.clone(),
            plan: false,
            approvals: false,
            dangerously_skip_permissions: false,
            claude_code_router: false,
            cmd_overrides,
        })),
        ExecutorKind::CursorAgent => Box::new(adapters::cursor_agent(CursorAgentOptions {
            append_prompt,
            force: options.force || options.yolo,
            trust: options.trust || options.force || options.yolo,
            model: options.model.clone(),
            cmd_overrides,
        })),
        ExecutorKind::Droid => Box::new(adapters::droid(DroidOptions {
            append_prompt,
            autonomy: None,
            model: options.model.clone(),
            reasoning_effort: None,
            cmd_overrides,
        })),
        ExecutorKind::Amp => Box::new(adapters::amp(AmpOptions {
            append_prompt,
            dangerously_allow_all: options.yolo,
            cmd_overrides,
        })),
        ExecutorKind::Gemini => Box::new(adapters::gemini(GeminiOptions {
            append_prompt,
            model: options.model.clone(),
            yolo: options.yolo,
            cmd_overrides,
        })),
        ExecutorKind::Opencode => Box::new(adapters::opencode(OpencodeOptions {
            append_prompt,
            model: options.model.clone(),
            variant: None,
            mode: None,
            auto_approve: options.auto_approve,
            cmd_overrides,
        })),
        ExecutorKind::QwenCode => Box::new(adapters::qwen_code(QwenCodeOptions {
            append_prompt,
            yolo: options.yolo,
            cmd_overrides,
        })),
        ExecutorKind::Copilot => Box::new(adapters::copilot(CopilotOptions {
            append_prompt,
            model: options.model.clone(),
            allow_all_tools: options.allow_all_tools,
            allow_tool: None,
            deny_tool: None,
            add_dir: None,
            disable_mcp_server: None,
            cmd_overrides,
        })),
        ExecutorKind::Custom(_) => Box::new(adapters::custom_command(
            ExecutorProfileId::new(ExecutorKind::Custom("custom".to_string())),
            options
                .base_command_override
                .clone()
                .unwrap_or_else(|| "sh".to_string()),
            Vec::new(),
            CmdOverrides {
                base_command_override: None,
                additional_params: if options.additional_params.is_empty() {
                    None
                } else {
                    Some(options.additional_params.clone())
                },
                env: None,
            },
            vec![aura_executors::ExecutorCapability::SessionFork],
        )),
    }
}

enum ChildEvent {
    Line(LogStream, String),
    StreamClosed,
    ExitStatus(Option<i32>),
    ExitSignal(aura_executors::ExecutorExitResult),
}

enum DisplayEvent {
    Line(LogStream, String),
    Status(String),
}

struct LogFormatter {
    executor: ExecutorKind,
    suppressed_codex_rollout_warnings: usize,
    emitted_codex_warning_notice: bool,
}

impl LogFormatter {
    fn new(executor: ExecutorKind) -> Self {
        Self {
            executor,
            suppressed_codex_rollout_warnings: 0,
            emitted_codex_warning_notice: false,
        }
    }

    fn format(&mut self, stream: LogStream, raw_line: &str) -> Vec<DisplayEvent> {
        let line = raw_line.trim_end();
        if line.is_empty() {
            return Vec::new();
        }

        if matches!(self.executor, ExecutorKind::Codex) {
            if stream == LogStream::Stderr
                && line.contains("state db missing rollout path for thread")
            {
                self.suppressed_codex_rollout_warnings += 1;
                if self.emitted_codex_warning_notice {
                    return Vec::new();
                }
                self.emitted_codex_warning_notice = true;
                return vec![DisplayEvent::Status(
                    "codex: suppressing noisy local rollout-path warnings".to_string(),
                )];
            }

            if let Some(formatted) = format_codex_event(line) {
                return formatted;
            }
        }

        vec![DisplayEvent::Line(stream, line.to_string())]
    }
}

fn format_codex_event(line: &str) -> Option<Vec<DisplayEvent>> {
    let value: Value = serde_json::from_str(line).ok()?;
    let event_type = value.get("type")?.as_str()?;

    match event_type {
        "thread.started" => Some(vec![DisplayEvent::Status("thread started".to_string())]),
        "turn.started" => Some(vec![DisplayEvent::Status("turn started".to_string())]),
        "turn.completed" => Some(vec![DisplayEvent::Status("turn completed".to_string())]),
        "turn.failed" => {
            let message = value
                .get("error")
                .and_then(|e| e.get("message"))
                .and_then(Value::as_str)
                .unwrap_or("unknown error");
            Some(vec![DisplayEvent::Status(format!(
                "turn failed: {}",
                summarize_text(message, 160)
            ))])
        }
        "error" => {
            let message = value
                .get("message")
                .and_then(Value::as_str)
                .unwrap_or("unknown error");
            Some(vec![DisplayEvent::Line(
                LogStream::Stderr,
                format!("codex error: {}", summarize_text(message, 180)),
            )])
        }
        "item.started" => {
            let item = value.get("item")?;
            let item_type = item
                .get("type")
                .and_then(Value::as_str)
                .unwrap_or("unknown");
            if item_type == "command_execution" {
                let command = item
                    .get("command")
                    .and_then(Value::as_str)
                    .unwrap_or("<command>");
                return Some(vec![DisplayEvent::Line(
                    LogStream::Stdout,
                    format!("cmd > {}", summarize_text(command, 180)),
                )]);
            }
            None
        }
        "item.completed" => {
            let item = value.get("item")?;
            let item_type = item
                .get("type")
                .and_then(Value::as_str)
                .unwrap_or("unknown");
            match item_type {
                "command_execution" => {
                    let command = item
                        .get("command")
                        .and_then(Value::as_str)
                        .unwrap_or("<command>");
                    let exit_code = item
                        .get("exit_code")
                        .and_then(Value::as_i64)
                        .map(|code| code.to_string())
                        .unwrap_or_else(|| "?".to_string());
                    Some(vec![DisplayEvent::Line(
                        LogStream::Stdout,
                        format!("cmd ✓ (exit={exit_code}) {}", summarize_text(command, 160)),
                    )])
                }
                "agent_message" => {
                    let text = item.get("text").and_then(Value::as_str).unwrap_or("");
                    if text.is_empty() {
                        return None;
                    }
                    Some(vec![DisplayEvent::Line(
                        LogStream::Stdout,
                        format!("agent: {}", summarize_text(text, 200)),
                    )])
                }
                "reasoning" => {
                    let text = item.get("text").and_then(Value::as_str).unwrap_or("");
                    if text.is_empty() {
                        return None;
                    }
                    Some(vec![DisplayEvent::Line(
                        LogStream::Stdout,
                        format!("thinking: {}", summarize_text(text, 140)),
                    )])
                }
                _ => None,
            }
        }
        _ => None,
    }
}

fn summarize_text(text: &str, max_len: usize) -> String {
    let compact = text.split_whitespace().collect::<Vec<_>>().join(" ");
    let total = compact.chars().count();
    if total <= max_len {
        compact
    } else {
        let mut out: String = compact.chars().take(max_len).collect();
        out.push_str("...");
        out
    }
}

pub async fn stream_spawned(
    executor_kind: ExecutorKind,
    mut spawned: SpawnedChild,
    sink: &mut dyn LogSink,
) -> Result<RunOutcome, CliError> {
    let (tx, mut rx) = mpsc::unbounded_channel::<ChildEvent>();

    let mut expected_streams = 0usize;

    if let Some(stdout) = spawned.child.stdout.take() {
        expected_streams += 1;
        spawn_reader(stdout, LogStream::Stdout, tx.clone());
    }

    if let Some(stderr) = spawned.child.stderr.take() {
        expected_streams += 1;
        spawn_reader(stderr, LogStream::Stderr, tx.clone());
    }

    let mut child_for_wait = spawned.child;
    let tx_wait = tx.clone();
    tokio::spawn(async move {
        let status = child_for_wait.wait().await.ok().and_then(|s| s.code());
        let _ = tx_wait.send(ChildEvent::ExitStatus(status));
    });

    if let Some(exit_signal) = spawned.exit_signal {
        let tx_signal = tx.clone();
        tokio::spawn(async move {
            if let Ok(signal) = exit_signal.await {
                let _ = tx_signal.send(ChildEvent::ExitSignal(signal));
            }
        });
    }

    drop(tx);

    let mut streams_closed = 0usize;
    let mut exit_code: Option<i32> = None;
    let mut got_exit_status = false;
    let mut formatter = LogFormatter::new(executor_kind);

    loop {
        let event = tokio::time::timeout(Duration::from_secs(5), rx.recv()).await;
        let Some(event) = (match event {
            Ok(value) => value,
            Err(_) => {
                if !got_exit_status {
                    sink.on_status("waiting for subprocess output...");
                }
                continue;
            }
        }) else {
            break;
        };

        match event {
            ChildEvent::Line(stream, line) => {
                for display in formatter.format(stream, &line) {
                    match display {
                        DisplayEvent::Line(stream, line) => sink.on_line(stream, &line),
                        DisplayEvent::Status(status) => sink.on_status(&status),
                    }
                }
            }
            ChildEvent::StreamClosed => streams_closed += 1,
            ChildEvent::ExitStatus(code) => {
                got_exit_status = true;
                exit_code = code;
            }
            ChildEvent::ExitSignal(result) => {
                sink.on_status("executor requested graceful exit");
                if !got_exit_status {
                    exit_code = Some(match result {
                        aura_executors::ExecutorExitResult::Success => 0,
                        aura_executors::ExecutorExitResult::Failure => 1,
                    });
                }
            }
        }

        if got_exit_status && streams_closed >= expected_streams {
            break;
        }
    }

    if formatter.suppressed_codex_rollout_warnings > 0 {
        sink.on_status(&format!(
            "suppressed {} repeated codex rollout-path warnings",
            formatter.suppressed_codex_rollout_warnings
        ));
    }

    sink.on_exit(exit_code);
    Ok(RunOutcome {
        success: exit_code == Some(0),
        exit_code,
    })
}

fn spawn_reader<T>(reader: T, stream: LogStream, tx: mpsc::UnboundedSender<ChildEvent>)
where
    T: tokio::io::AsyncRead + Unpin + Send + 'static,
{
    tokio::spawn(async move {
        let mut lines = BufReader::new(reader).lines();
        while let Ok(Some(line)) = lines.next_line().await {
            let _ = tx.send(ChildEvent::Line(stream, line));
        }
        let _ = tx.send(ChildEvent::StreamClosed);
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Default)]
    struct CollectingSink {
        stdout: Vec<String>,
        stderr: Vec<String>,
        status: Vec<String>,
        exit: Option<i32>,
    }

    impl LogSink for CollectingSink {
        fn on_start(&mut self, _options: &RunOptions) {}

        fn on_line(&mut self, stream: LogStream, line: &str) {
            match stream {
                LogStream::Stdout => self.stdout.push(line.to_string()),
                LogStream::Stderr => self.stderr.push(line.to_string()),
            }
        }

        fn on_status(&mut self, message: &str) {
            self.status.push(message.to_string());
        }

        fn on_exit(&mut self, code: Option<i32>) {
            self.exit = code;
        }
    }

    #[test]
    fn parses_run_command() {
        let args = vec![
            "run".to_string(),
            "--executor".to_string(),
            "gemini".to_string(),
            "--prompt".to_string(),
            "hello".to_string(),
            "--yolo".to_string(),
            "--no-tui".to_string(),
        ];

        let parsed = parse_cli_args(&args).expect("parse");
        match parsed {
            CliCommand::Run(opts) => {
                assert!(matches!(opts.executor, ExecutorKind::Gemini));
                assert_eq!(opts.prompt, "hello");
                assert!(opts.yolo);
                assert!(!opts.tui);
            }
            _ => panic!("expected run"),
        }
    }

    #[test]
    fn parses_tui_command() {
        let args = vec![
            "tui".to_string(),
            "--executor".to_string(),
            "codex".to_string(),
            "--prompt".to_string(),
            "ship it".to_string(),
        ];

        let parsed = parse_cli_args(&args).expect("parse");
        match parsed {
            CliCommand::Run(opts) => {
                assert!(matches!(opts.executor, ExecutorKind::Codex));
                assert_eq!(opts.prompt, "ship it");
                assert!(opts.tui);
            }
            _ => panic!("expected run"),
        }
    }

    #[test]
    fn codex_json_logs_are_humanized() {
        let mut formatter = LogFormatter::new(ExecutorKind::Codex);
        let events = formatter.format(
            LogStream::Stdout,
            r#"{"type":"item.completed","item":{"type":"agent_message","text":"hello from agent"}}"#,
        );

        assert_eq!(events.len(), 1);
        match &events[0] {
            DisplayEvent::Line(LogStream::Stdout, line) => {
                assert!(line.contains("agent: hello from agent"));
            }
            _ => panic!("expected formatted stdout line"),
        }
    }

    #[test]
    fn codex_rollout_warning_is_suppressed_after_notice() {
        let mut formatter = LogFormatter::new(ExecutorKind::Codex);
        let warning = "2026-01-01 ERROR codex_core::rollout::list: state db missing rollout path for thread abc";

        let first = formatter.format(LogStream::Stderr, warning);
        let second = formatter.format(LogStream::Stderr, warning);

        assert_eq!(formatter.suppressed_codex_rollout_warnings, 2);
        assert_eq!(first.len(), 1);
        assert!(matches!(first[0], DisplayEvent::Status(_)));
        assert!(second.is_empty());
    }

    #[test]
    fn parses_cursor_trust_aliases() {
        let args = vec![
            "run".to_string(),
            "--executor".to_string(),
            "cursor-agent".to_string(),
            "--prompt".to_string(),
            "hello".to_string(),
            "--trust".to_string(),
        ];
        let parsed = parse_cli_args(&args).expect("parse");
        match parsed {
            CliCommand::Run(opts) => {
                assert!(opts.trust);
                assert!(!opts.force);
            }
            _ => panic!("expected run"),
        }

        let args_short = vec![
            "run".to_string(),
            "--executor".to_string(),
            "cursor-agent".to_string(),
            "--prompt".to_string(),
            "hello".to_string(),
            "-f".to_string(),
        ];
        let parsed_short = parse_cli_args(&args_short).expect("parse short");
        match parsed_short {
            CliCommand::Run(opts) => assert!(opts.force),
            _ => panic!("expected run"),
        }
    }

    #[tokio::test]
    async fn runs_custom_executor_and_collects_logs() {
        let mut sink = CollectingSink::default();
        let options = RunOptions {
            executor: ExecutorKind::Custom("custom".to_string()),
            prompt: "ignored".to_string(),
            cwd: PathBuf::from("."),
            session_id: None,
            review: false,
            env_vars: HashMap::new(),
            base_command_override: Some("sh".to_string()),
            additional_params: vec![
                "-lc".to_string(),
                "echo out_line; echo err_line 1>&2".to_string(),
            ],
            append_prompt: None,
            model: None,
            yolo: false,
            force: false,
            trust: false,
            auto_approve: true,
            allow_all_tools: false,
            tui: false,
        };

        let outcome = run_executor(options, &mut sink).await.expect("run");
        assert!(outcome.success);
        assert!(sink.stdout.iter().any(|line| line.contains("out_line")));
        assert!(sink.stderr.iter().any(|line| line.contains("err_line")));
        assert_eq!(sink.exit, Some(0));
    }

    #[tokio::test]
    async fn follow_up_passes_session_id() {
        let mut sink = CollectingSink::default();
        let options = RunOptions {
            executor: ExecutorKind::Custom("custom".to_string()),
            prompt: "ignored".to_string(),
            cwd: PathBuf::from("."),
            session_id: Some("session-xyz".to_string()),
            review: false,
            env_vars: HashMap::new(),
            base_command_override: Some("sh".to_string()),
            additional_params: vec![
                "-lc".to_string(),
                "printf %s \"$AURA_SESSION_ID\"".to_string(),
            ],
            append_prompt: None,
            model: None,
            yolo: false,
            force: false,
            trust: false,
            auto_approve: true,
            allow_all_tools: false,
            tui: false,
        };

        let outcome = run_executor(options, &mut sink).await.expect("run");
        assert!(outcome.success);
        assert!(sink.stdout.iter().any(|line| line.contains("session-xyz")));
    }

    #[tokio::test]
    async fn cursor_yolo_implies_force_flag() {
        let mut sink = CollectingSink::default();
        let options = RunOptions {
            executor: ExecutorKind::CursorAgent,
            prompt: "ignored".to_string(),
            cwd: PathBuf::from("."),
            session_id: None,
            review: false,
            env_vars: HashMap::new(),
            base_command_override: Some("echo".to_string()),
            additional_params: Vec::new(),
            append_prompt: None,
            model: None,
            yolo: true,
            force: false,
            trust: false,
            auto_approve: true,
            allow_all_tools: false,
            tui: false,
        };

        let outcome = run_executor(options, &mut sink).await.expect("run");
        assert!(outcome.success);
        assert!(sink.stdout.iter().any(|line| line.contains("--force")));
        assert!(sink.stdout.iter().any(|line| line.contains("--trust")));
    }
}
