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
use crossterm::{
    cursor::Show,
    event::{self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers},
    execute,
    terminal::{EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode},
};
use ratatui::{
    Terminal,
    backend::CrosstermBackend,
    layout::{Constraint, Direction, Layout},
    style::{Color, Style},
    text::{Line, Span, Text},
    widgets::{Block, Borders, Paragraph, Wrap},
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
    fn on_agent_status(&mut self, _message: &str) {}
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

    fn on_agent_status(&mut self, message: &str) {
        let _ = writeln!(io::stdout(), "[agent] {message}");
    }

    fn on_exit(&mut self, code: Option<i32>) {
        let _ = writeln!(io::stdout(), "[aura] process exited with {:?}", code);
    }
}

pub struct TuiLogSink {
    started_at: Instant,
    lines: VecDeque<TuiLogEntry>,
    max_lines: usize,
    title: String,
    status: String,
    agent_status: String,
    terminal: Option<Terminal<CrosstermBackend<io::Stdout>>>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TuiLineKind {
    Agent,
    Thinking,
    Action,
    Error,
    Output,
}

#[derive(Debug, Clone)]
struct TuiLogEntry {
    kind: TuiLineKind,
    text: String,
}

fn classify_tui_line(stream: LogStream, line: &str) -> TuiLineKind {
    if line.starts_with("agent: ") {
        return TuiLineKind::Agent;
    }
    if line.starts_with("thinking: ") {
        return TuiLineKind::Thinking;
    }
    if line.starts_with("cmd > ") || line.starts_with("cmd ✓ ") {
        return TuiLineKind::Action;
    }
    if stream == LogStream::Stderr || line.starts_with("codex error: ") {
        return TuiLineKind::Error;
    }
    TuiLineKind::Output
}

fn normalize_tui_line_text(kind: TuiLineKind, line: &str) -> String {
    match kind {
        TuiLineKind::Agent => line.trim_start_matches("agent: ").to_string(),
        TuiLineKind::Thinking => line.trim_start_matches("thinking: ").to_string(),
        TuiLineKind::Action => line.to_string(),
        TuiLineKind::Error => line.to_string(),
        TuiLineKind::Output => line.to_string(),
    }
}

impl TuiLogSink {
    pub fn new() -> Self {
        let terminal = Self::init_terminal();
        Self {
            started_at: Instant::now(),
            lines: VecDeque::new(),
            max_lines: 500,
            title: "AURA Executor".to_string(),
            status: "starting".to_string(),
            agent_status: "Initializing".to_string(),
            terminal,
        }
    }

    fn init_terminal() -> Option<Terminal<CrosstermBackend<io::Stdout>>> {
        if enable_raw_mode().is_err() {
            return None;
        }
        let mut stdout = io::stdout();
        if execute!(stdout, EnterAlternateScreen).is_err() {
            let _ = disable_raw_mode();
            return None;
        }
        let backend = CrosstermBackend::new(stdout);
        Terminal::new(backend).ok()
    }

    fn redraw(&mut self) {
        let Some(terminal) = self.terminal.as_mut() else {
            let _ = writeln!(
                io::stdout(),
                "{} | uptime={:.1?} | status={} | agent={}",
                self.title,
                self.started_at.elapsed(),
                self.status,
                self.agent_status
            );
            return;
        };

        let _ = terminal.draw(|frame| {
            let size = frame.area();
            let chunks = Layout::default()
                .direction(Direction::Vertical)
                .constraints([
                    Constraint::Length(3),
                    Constraint::Min(8),
                    Constraint::Length(1),
                ])
                .split(size);

            let header = Paragraph::new(vec![
                Line::from(Span::styled(
                    self.title.clone(),
                    Style::default().fg(Color::Cyan),
                )),
                Line::from(format!(
                    "uptime: {:.1?} | status: {}",
                    self.started_at.elapsed(),
                    self.status
                )),
            ])
            .block(Block::default().borders(Borders::ALL).title("AURA"));
            frame.render_widget(header, chunks[0]);

            let logs_capacity = chunks[1].height.saturating_sub(2) as usize;
            let visible_logs: Vec<TuiLogEntry> = self
                .lines
                .iter()
                .rev()
                .take(logs_capacity)
                .rev()
                .cloned()
                .collect();
            let log_lines: Vec<Line> = visible_logs
                .into_iter()
                .map(|entry| match entry.kind {
                    TuiLineKind::Agent => Line::from(vec![
                        Span::styled(" AGENT ", Style::default().fg(Color::White).bg(Color::Cyan)),
                        Span::raw(" "),
                        Span::styled(entry.text, Style::default().fg(Color::White)),
                    ]),
                    TuiLineKind::Thinking => Line::from(vec![
                        Span::styled(
                            " THINK ",
                            Style::default().fg(Color::Black).bg(Color::Yellow),
                        ),
                        Span::raw(" "),
                        Span::styled(entry.text, Style::default().fg(Color::Yellow)),
                    ]),
                    TuiLineKind::Action => Line::from(vec![
                        Span::styled(
                            " ACTION ",
                            Style::default().fg(Color::White).bg(Color::Blue),
                        ),
                        Span::raw(" "),
                        Span::styled(entry.text, Style::default().fg(Color::Blue)),
                    ]),
                    TuiLineKind::Error => Line::from(vec![
                        Span::styled(" ERROR ", Style::default().fg(Color::White).bg(Color::Red)),
                        Span::raw(" "),
                        Span::styled(entry.text, Style::default().fg(Color::Red)),
                    ]),
                    TuiLineKind::Output => Line::from(vec![
                        Span::styled(" OUT ", Style::default().fg(Color::Black).bg(Color::Green)),
                        Span::raw(" "),
                        Span::raw(entry.text),
                    ]),
                })
                .collect();

            let logs = Paragraph::new(Text::from(log_lines))
                .block(Block::default().borders(Borders::ALL).title("Logs"))
                .wrap(Wrap { trim: false });
            frame.render_widget(logs, chunks[1]);

            let bar = Paragraph::new(Line::from(vec![
                Span::styled("Run: ", Style::default().fg(Color::Yellow)),
                Span::raw(self.status.clone()),
                Span::raw("  |  "),
                Span::styled("Agent: ", Style::default().fg(Color::Yellow)),
                Span::raw(self.agent_status.clone()),
                Span::raw("  |  q/Esc/Ctrl+C"),
            ]))
            .style(Style::default().fg(Color::White).bg(Color::DarkGray));
            frame.render_widget(bar, chunks[2]);
        });
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
        self.status = "starting".to_string();
        self.agent_status = "Initializing".to_string();
        self.redraw();
    }

    fn on_line(&mut self, stream: LogStream, line: &str) {
        let kind = classify_tui_line(stream, line);
        self.lines.push_back(TuiLogEntry {
            kind,
            text: normalize_tui_line_text(kind, line),
        });
        while self.lines.len() > self.max_lines {
            let _ = self.lines.pop_front();
        }
        self.status = "running".to_string();
        self.redraw();
    }

    fn on_status(&mut self, message: &str) {
        self.status = message.to_string();
        self.redraw();
    }

    fn on_agent_status(&mut self, message: &str) {
        self.agent_status = message.to_string();
        self.redraw();
    }

    fn on_exit(&mut self, code: Option<i32>) {
        self.status = format!("finished (exit={code:?})");
        if code == Some(0) {
            self.agent_status = "Idle".to_string();
        } else {
            self.agent_status = "Error".to_string();
        }
        self.redraw();
    }
}

impl Drop for TuiLogSink {
    fn drop(&mut self) {
        if let Some(mut terminal) = self.terminal.take() {
            let _ = terminal.show_cursor();
            let _ = disable_raw_mode();
            let _ = execute!(io::stdout(), LeaveAlternateScreen, Show);
        }
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
        run_executor_inner(options, &mut sink, true).await
    } else {
        let mut sink = PlainLogSink;
        run_executor_inner(options, &mut sink, false).await
    }
}

pub async fn run_executor(
    options: RunOptions,
    sink: &mut dyn LogSink,
) -> Result<RunOutcome, CliError> {
    run_executor_inner(options, sink, false).await
}

async fn run_executor_inner(
    options: RunOptions,
    sink: &mut dyn LogSink,
    enable_keyboard_controls: bool,
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

    stream_spawned(
        options.executor.clone(),
        spawned,
        sink,
        enable_keyboard_controls,
    )
    .await
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
    ExitSignal(aura_executors::ExecutorExitResult),
}

enum DisplayEvent {
    Line(LogStream, String),
    Status(String),
    AgentStatus(String),
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

        if line.contains("Workspace Trust Required") {
            return vec![
                DisplayEvent::Line(stream, line.to_string()),
                DisplayEvent::AgentStatus("Blocked: workspace trust required".to_string()),
            ];
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
        "thread.started" => Some(vec![
            DisplayEvent::Status("thread started".to_string()),
            DisplayEvent::AgentStatus("Initializing".to_string()),
        ]),
        "turn.started" => Some(vec![
            DisplayEvent::Status("turn started".to_string()),
            DisplayEvent::AgentStatus("Thinking".to_string()),
        ]),
        "turn.completed" => Some(vec![
            DisplayEvent::Status("turn completed".to_string()),
            DisplayEvent::AgentStatus("Idle".to_string()),
        ]),
        "turn.failed" => {
            let message = value
                .get("error")
                .and_then(|e| e.get("message"))
                .and_then(Value::as_str)
                .unwrap_or("unknown error");
            Some(vec![
                DisplayEvent::Status(format!("turn failed: {}", summarize_text(message, 160))),
                DisplayEvent::AgentStatus("Error".to_string()),
            ])
        }
        "error" => {
            let message = value
                .get("message")
                .and_then(Value::as_str)
                .unwrap_or("unknown error");
            Some(vec![
                DisplayEvent::Line(
                    LogStream::Stderr,
                    format!("codex error: {}", summarize_text(message, 180)),
                ),
                DisplayEvent::AgentStatus("Error".to_string()),
            ])
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
                return Some(vec![
                    DisplayEvent::Line(
                        LogStream::Stdout,
                        format!("cmd > {}", summarize_text(command, 180)),
                    ),
                    DisplayEvent::AgentStatus("Executing command".to_string()),
                ]);
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
                    Some(vec![
                        DisplayEvent::Line(
                            LogStream::Stdout,
                            format!("cmd ✓ (exit={exit_code}) {}", summarize_text(command, 160)),
                        ),
                        DisplayEvent::AgentStatus("Thinking".to_string()),
                    ])
                }
                "agent_message" => {
                    let text = item.get("text").and_then(Value::as_str).unwrap_or("");
                    if text.is_empty() {
                        return None;
                    }
                    Some(vec![
                        DisplayEvent::Line(
                            LogStream::Stdout,
                            format!("agent: {}", summarize_text(text, 200)),
                        ),
                        DisplayEvent::AgentStatus("Responding".to_string()),
                    ])
                }
                "reasoning" => {
                    let text = item.get("text").and_then(Value::as_str).unwrap_or("");
                    if text.is_empty() {
                        return None;
                    }
                    let lower = text.to_ascii_lowercase();
                    let state = if lower.contains("plan") {
                        "Planning"
                    } else {
                        "Thinking"
                    };
                    Some(vec![
                        DisplayEvent::Line(
                            LogStream::Stdout,
                            format!("thinking: {}", summarize_text(text, 140)),
                        ),
                        DisplayEvent::AgentStatus(state.to_string()),
                    ])
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
    enable_keyboard_controls: bool,
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

    let mut child = spawned.child;

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
    let mut last_wait_status_at = Instant::now();
    let mut quit_requested_at: Option<Instant> = None;
    let mut completion_reported = false;

    loop {
        if enable_keyboard_controls && should_quit_from_keyboard().unwrap_or(false) {
            if got_exit_status && streams_closed >= expected_streams {
                break;
            }
            if quit_requested_at.is_none() {
                sink.on_status("stopping subprocess (user requested exit)");
                if let Some(interrupt_sender) = spawned.interrupt_sender.take() {
                    let _ = interrupt_sender.send(());
                }
            }
            quit_requested_at.get_or_insert_with(Instant::now);
            let _ = child.start_kill();
        }

        if !got_exit_status && let Some(status) = child.try_wait()? {
            got_exit_status = true;
            exit_code = status.code();
        }

        if got_exit_status && streams_closed >= expected_streams && !completion_reported {
            sink.on_exit(exit_code);
            completion_reported = true;
        }

        if let Some(requested_at) = quit_requested_at {
            if got_exit_status {
                break;
            }
            if requested_at.elapsed() >= Duration::from_secs(2) {
                sink.on_status("forcing exit after quit request");
                if exit_code.is_none() {
                    exit_code = Some(130);
                }
                break;
            }
        }

        let event = tokio::time::timeout(Duration::from_millis(200), rx.recv()).await;
        let Some(event) = (match event {
            Ok(value) => value,
            Err(_) => {
                if !got_exit_status && last_wait_status_at.elapsed() >= Duration::from_secs(5) {
                    sink.on_status("waiting for subprocess output...");
                    last_wait_status_at = Instant::now();
                }
                continue;
            }
        }) else {
            if got_exit_status
                && streams_closed >= expected_streams
                && (!enable_keyboard_controls || quit_requested_at.is_some())
            {
                break;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
            continue;
        };

        match event {
            ChildEvent::Line(stream, line) => {
                for display in formatter.format(stream, &line) {
                    match display {
                        DisplayEvent::Line(stream, line) => sink.on_line(stream, &line),
                        DisplayEvent::Status(status) => sink.on_status(&status),
                        DisplayEvent::AgentStatus(status) => sink.on_agent_status(&status),
                    }
                }
            }
            ChildEvent::StreamClosed => streams_closed += 1,
            ChildEvent::ExitSignal(result) => {
                sink.on_status("executor requested graceful exit");
                if !got_exit_status {
                    exit_code = Some(match result {
                        aura_executors::ExecutorExitResult::Success => 0,
                        aura_executors::ExecutorExitResult::Failure => 1,
                    });
                    got_exit_status = true;
                }
            }
        }

        if got_exit_status
            && streams_closed >= expected_streams
            && (!enable_keyboard_controls || quit_requested_at.is_some())
        {
            break;
        }
    }

    if formatter.suppressed_codex_rollout_warnings > 0 {
        sink.on_status(&format!(
            "suppressed {} repeated codex rollout-path warnings",
            formatter.suppressed_codex_rollout_warnings
        ));
    }

    if !completion_reported {
        sink.on_exit(exit_code);
    }
    Ok(RunOutcome {
        success: exit_code == Some(0),
        exit_code,
    })
}

fn should_quit_from_keyboard() -> io::Result<bool> {
    if !event::poll(Duration::from_millis(0))? {
        return Ok(false);
    }
    let Event::Key(key) = event::read()? else {
        return Ok(false);
    };
    Ok(is_quit_key(key))
}

fn is_quit_key(key: KeyEvent) -> bool {
    if !matches!(key.kind, KeyEventKind::Press | KeyEventKind::Repeat) {
        return false;
    }

    match key.code {
        KeyCode::Esc => true,
        KeyCode::Char('q') | KeyCode::Char('Q') => true,
        KeyCode::Char('c') | KeyCode::Char('C') => key.modifiers.contains(KeyModifiers::CONTROL),
        _ => false,
    }
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
    fn quit_key_mapping() {
        assert!(is_quit_key(KeyEvent::new(
            KeyCode::Char('q'),
            KeyModifiers::NONE
        )));
        assert!(is_quit_key(
            KeyEvent::new_with_kind(
                KeyCode::Char('q'),
                KeyModifiers::NONE,
                crossterm::event::KeyEventKind::Repeat
            )
        ));
        assert!(!is_quit_key(KeyEvent::new_with_kind(
            KeyCode::Char('q'),
            KeyModifiers::NONE,
            crossterm::event::KeyEventKind::Release
        )));
        assert!(is_quit_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE)));
        assert!(is_quit_key(KeyEvent::new(
            KeyCode::Char('c'),
            KeyModifiers::CONTROL
        )));
        assert!(!is_quit_key(KeyEvent::new(
            KeyCode::Char('x'),
            KeyModifiers::NONE
        )));
    }

    #[test]
    fn codex_json_logs_are_humanized() {
        let mut formatter = LogFormatter::new(ExecutorKind::Codex);
        let events = formatter.format(
            LogStream::Stdout,
            r#"{"type":"item.completed","item":{"type":"agent_message","text":"hello from agent"}}"#,
        );

        assert!(events.iter().any(|event| matches!(
            event,
            DisplayEvent::Line(LogStream::Stdout, line) if line.contains("agent: hello from agent")
        )));
        assert!(events.iter().any(|event| matches!(
            event,
            DisplayEvent::AgentStatus(status) if status == "Responding"
        )));
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
    fn codex_reasoning_can_set_planning_status() {
        let mut formatter = LogFormatter::new(ExecutorKind::Codex);
        let events = formatter.format(
            LogStream::Stdout,
            r#"{"type":"item.completed","item":{"type":"reasoning","text":"**Planning fixes**"}}"#,
        );
        assert!(events.iter().any(|event| matches!(
            event,
            DisplayEvent::AgentStatus(status) if status == "Planning"
        )));
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
