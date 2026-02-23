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
    pub user_requested_exit: bool,
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
    fn on_key_event(&mut self, _key: KeyEvent) {}
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
    log_scroll: usize,
    title: String,
    status: String,
    agent_status: String,
    input_text: String,
    active_pane: TuiPane,
    terminal: Option<Terminal<CrosstermBackend<io::Stdout>>>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TuiLineKind {
    User,
    Agent,
    Code,
    Json,
    Diff,
    Thinking,
    Action,
    Error,
    Output,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TuiPane {
    Logs,
    Prompt,
}

#[derive(Debug, Clone)]
struct TuiLogEntry {
    kind: TuiLineKind,
    text: String,
}

fn classify_tui_line(stream: LogStream, line: &str) -> TuiLineKind {
    if line.starts_with("user: ") {
        return TuiLineKind::User;
    }
    if line.starts_with("code: ") || line.starts_with("code| ") {
        return TuiLineKind::Code;
    }
    if line.starts_with("json: ") || line.starts_with("json| ") {
        return TuiLineKind::Json;
    }
    if line.starts_with("diff| ") {
        return TuiLineKind::Diff;
    }
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
        TuiLineKind::User => line.trim_start_matches("user: ").to_string(),
        TuiLineKind::Agent => line.trim_start_matches("agent: ").to_string(),
        TuiLineKind::Code => line
            .trim_start_matches("code: ")
            .trim_start_matches("code| ")
            .to_string(),
        TuiLineKind::Json => line
            .trim_start_matches("json: ")
            .trim_start_matches("json| ")
            .to_string(),
        TuiLineKind::Diff => line.trim_start_matches("diff| ").to_string(),
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
            log_scroll: 0,
            title: "AURA Executor".to_string(),
            status: "starting".to_string(),
            agent_status: "Initializing".to_string(),
            input_text: String::new(),
            active_pane: TuiPane::Prompt,
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
                    Constraint::Min(6),
                    Constraint::Length(3),
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
            let max_log_scroll = self.lines.len().saturating_sub(logs_capacity);
            let scroll = self.log_scroll.min(max_log_scroll);
            let end = self.lines.len().saturating_sub(scroll);
            let start = end.saturating_sub(logs_capacity);
            let visible_logs: Vec<TuiLogEntry> =
                self.lines.iter().skip(start).take(end - start).cloned().collect();
            let log_lines: Vec<Line> = visible_logs
                .into_iter()
                .map(|entry| match entry.kind {
                    TuiLineKind::User => Line::from(vec![
                        Span::styled(" USER ", Style::default().fg(Color::Black).bg(Color::White)),
                        Span::raw(" "),
                        Span::styled(entry.text, Style::default().fg(Color::White)),
                    ]),
                    TuiLineKind::Agent => Line::from(vec![
                        Span::styled(" AGENT ", Style::default().fg(Color::White).bg(Color::Cyan)),
                        Span::raw(" "),
                        Span::styled(entry.text, Style::default().fg(Color::White)),
                    ]),
                    TuiLineKind::Code => Line::from(vec![
                        Span::styled(
                            " CODE ",
                            Style::default().fg(Color::White).bg(Color::DarkGray),
                        ),
                        Span::raw(" "),
                        Span::styled(entry.text, Style::default().fg(Color::Gray)),
                    ]),
                    TuiLineKind::Json => Line::from(vec![
                        Span::styled(" JSON ", Style::default().fg(Color::Black).bg(Color::Green)),
                        Span::raw(" "),
                        Span::styled(entry.text, Style::default().fg(Color::Green)),
                    ]),
                    TuiLineKind::Diff => Line::from(vec![
                        Span::styled(" DIFF ", Style::default().fg(Color::White).bg(Color::Magenta)),
                        Span::raw(" "),
                        Span::styled(entry.text.clone(), diff_line_style(&entry.text)),
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

            let logs_title = if max_log_scroll == 0 {
                "Logs".to_string()
            } else {
                format!(
                    "Logs ({}/{})",
                    scroll,
                    max_log_scroll
                )
            };
            let logs_border = if self.active_pane == TuiPane::Logs {
                Style::default().fg(Color::Yellow)
            } else {
                Style::default()
            };
            let logs = Paragraph::new(Text::from(log_lines))
                .block(
                    Block::default()
                        .borders(Borders::ALL)
                        .title(logs_title)
                        .border_style(logs_border),
                )
                .wrap(Wrap { trim: false });
            frame.render_widget(logs, chunks[1]);

            let prompt_text = if self.input_text.is_empty() {
                Text::from(Line::from(Span::styled(
                    "Type a follow-up prompt and press Enter",
                    Style::default().fg(Color::DarkGray),
                )))
            } else {
                Text::from(self.input_text.clone())
            };
            let prompt_border = if self.active_pane == TuiPane::Prompt {
                Style::default().fg(Color::Yellow)
            } else {
                Style::default()
            };
            let prompt = Paragraph::new(prompt_text)
                .block(
                    Block::default()
                        .borders(Borders::ALL)
                        .title("Prompt")
                        .border_style(prompt_border),
                )
                .wrap(Wrap { trim: false });
            frame.render_widget(prompt, chunks[2]);

            let pane_text = match self.active_pane {
                TuiPane::Logs => "Logs",
                TuiPane::Prompt => "Prompt",
            };
            let bar = Paragraph::new(Line::from(vec![
                Span::styled("Run: ", Style::default().fg(Color::Yellow)),
                Span::raw(self.status.clone()),
                Span::raw("  |  "),
                Span::styled("Agent: ", Style::default().fg(Color::Yellow)),
                Span::raw(self.agent_status.clone()),
                Span::raw("  |  "),
                Span::styled("Pane: ", Style::default().fg(Color::Yellow)),
                Span::raw(pane_text),
                Span::raw("  |  Tab=switch  Arrows/Page/Home/End=scroll  Enter=send  Esc/Ctrl+C=quit"),
            ]))
            .style(Style::default().fg(Color::White).bg(Color::DarkGray));
            frame.render_widget(bar, chunks[3]);
        });
    }

    fn set_input_text(&mut self, input: String) {
        self.input_text = input;
        self.redraw();
    }

    fn set_active_pane(&mut self, pane: TuiPane) {
        self.active_pane = pane;
        self.redraw();
    }

    fn active_pane(&self) -> TuiPane {
        self.active_pane
    }

    fn toggle_pane(&mut self) {
        self.active_pane = match self.active_pane {
            TuiPane::Logs => TuiPane::Prompt,
            TuiPane::Prompt => TuiPane::Logs,
        };
        self.redraw();
    }

    fn max_log_scroll(&self) -> usize {
        self.lines.len().saturating_sub(1)
    }

    fn scroll_logs_up(&mut self, by: usize) {
        if by == 0 {
            return;
        }
        self.log_scroll = self.log_scroll.saturating_add(by).min(self.max_log_scroll());
        self.redraw();
    }

    fn scroll_logs_down(&mut self, by: usize) {
        if by == 0 {
            return;
        }
        self.log_scroll = self.log_scroll.saturating_sub(by);
        self.redraw();
    }

    fn scroll_logs_to_top(&mut self) {
        self.log_scroll = self.max_log_scroll();
        self.redraw();
    }

    fn scroll_logs_to_bottom(&mut self) {
        self.log_scroll = 0;
        self.redraw();
    }

    fn push_log_entry(&mut self, kind: TuiLineKind, text: String) {
        if self.log_scroll > 0 {
            self.log_scroll = self.log_scroll.saturating_add(1);
        }
        self.lines.push_back(TuiLogEntry { kind, text });
        while self.lines.len() > self.max_lines {
            let _ = self.lines.pop_front();
        }
        self.log_scroll = self.log_scroll.min(self.max_log_scroll());
    }

    fn on_user_prompt(&mut self, prompt: &str) {
        self.push_log_entry(TuiLineKind::User, summarize_text(prompt, 240));
        self.redraw();
    }
}

fn diff_line_style(line: &str) -> Style {
    let text = line.trim_start();
    if text.starts_with("@@") {
        return Style::default().fg(Color::Yellow);
    }
    if text.starts_with("diff --git")
        || text.starts_with("index ")
        || text.starts_with("--- ")
        || text.starts_with("+++ ")
    {
        return Style::default().fg(Color::Cyan);
    }
    if text.starts_with('+') {
        return Style::default().fg(Color::Green);
    }
    if text.starts_with('-') {
        return Style::default().fg(Color::Red);
    }
    Style::default().fg(Color::Magenta)
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
        self.push_log_entry(
            kind,
            normalize_tui_line_text(kind, line),
        );
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

    fn on_key_event(&mut self, key: KeyEvent) {
        if !matches!(key.kind, KeyEventKind::Press | KeyEventKind::Repeat) {
            return;
        }
        if matches!(key.code, KeyCode::Tab) {
            self.toggle_pane();
            return;
        }
        if self.active_pane == TuiPane::Logs {
            let _ = handle_logs_navigation_key(self, key);
        }
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
        run_tui_session(options, &mut sink).await
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

enum TuiPromptAction {
    Submit(String),
    Quit,
}

enum PromptInputUpdate {
    Continue,
    Submit(String),
    Quit,
}

async fn run_tui_session(
    mut options: RunOptions,
    sink: &mut TuiLogSink,
) -> Result<RunOutcome, CliError> {
    loop {
        sink.set_input_text(String::new());
        sink.on_user_prompt(&options.prompt);
        let mut outcome = run_executor_inner(options.clone(), sink, true).await?;

        if outcome.user_requested_exit {
            return Ok(outcome);
        }

        sink.set_active_pane(TuiPane::Prompt);
        sink.on_status("awaiting prompt (Tab=switch, Enter=send, Esc/Ctrl+C=quit)");
        match read_tui_prompt_action(sink)? {
            TuiPromptAction::Submit(prompt) => {
                options.prompt = prompt;
            }
            TuiPromptAction::Quit => {
                outcome.user_requested_exit = true;
                return Ok(outcome);
            }
        }
    }
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
                    let mut events = format_agent_message(text);
                    events.push(DisplayEvent::AgentStatus("Responding".to_string()));
                    Some(events)
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

fn format_agent_message(text: &str) -> Vec<DisplayEvent> {
    let mut events = Vec::new();
    let mut prose_lines: Vec<String> = Vec::new();
    let mut fence_lines: Vec<String> = Vec::new();
    let mut in_fence = false;
    let mut fence_lang = String::new();

    for raw_line in text.lines() {
        let line = raw_line.trim_end();
        let marker = line.trim_start();
        if marker.starts_with("```") {
            if in_fence {
                emit_code_fence(&mut events, &fence_lang, &fence_lines);
                fence_lines.clear();
                in_fence = false;
                fence_lang.clear();
            } else {
                emit_prose_block(&mut events, &prose_lines);
                prose_lines.clear();
                in_fence = true;
                fence_lang = marker.trim_start_matches("```").trim().to_string();
            }
            continue;
        }

        if in_fence {
            fence_lines.push(line.to_string());
        } else {
            prose_lines.push(line.to_string());
        }
    }

    if in_fence {
        emit_code_fence(&mut events, &fence_lang, &fence_lines);
    } else {
        emit_prose_block(&mut events, &prose_lines);
    }

    if events.is_empty() {
        events.push(DisplayEvent::Line(
            LogStream::Stdout,
            "agent: (empty message)".to_string(),
        ));
    }
    events
}

fn emit_prose_block(events: &mut Vec<DisplayEvent>, lines: &[String]) {
    if lines.is_empty() {
        return;
    }
    let non_empty: Vec<String> = lines
        .iter()
        .map(|line| line.trim().to_string())
        .filter(|line| !line.is_empty())
        .collect();
    if non_empty.is_empty() {
        return;
    }

    if looks_like_diff_block(&non_empty) {
        for line in non_empty {
            events.push(DisplayEvent::Line(
                LogStream::Stdout,
                format!("diff| {line}"),
            ));
        }
        return;
    }

    for line in non_empty {
        events.push(DisplayEvent::Line(
            LogStream::Stdout,
            format!("agent: {}", summarize_text(&line, 220)),
        ));
    }
}

fn emit_code_fence(events: &mut Vec<DisplayEvent>, lang: &str, lines: &[String]) {
    if lines.is_empty() {
        return;
    }
    let content = lines.join("\n");
    let lang_lower = lang.trim().to_ascii_lowercase();

    if lang_lower == "json" || lang_lower == "jsonc" {
        if let Ok(value) = serde_json::from_str::<Value>(&content) {
            events.push(DisplayEvent::Line(LogStream::Stdout, "json: block".to_string()));
            if let Ok(pretty) = serde_json::to_string_pretty(&value) {
                for line in pretty.lines() {
                    events.push(DisplayEvent::Line(
                        LogStream::Stdout,
                        format!("json| {line}"),
                    ));
                }
                return;
            }
        }
    }

    if lang_lower.contains("diff")
        || lang_lower.contains("patch")
        || looks_like_diff_block(lines)
    {
        for line in lines {
            events.push(DisplayEvent::Line(
                LogStream::Stdout,
                format!("diff| {line}"),
            ));
        }
        return;
    }

    let code_header = if lang.trim().is_empty() {
        "code: block".to_string()
    } else {
        format!("code: {}", lang.trim())
    };
    events.push(DisplayEvent::Line(LogStream::Stdout, code_header));
    for line in lines {
        events.push(DisplayEvent::Line(
            LogStream::Stdout,
            format!("code| {line}"),
        ));
    }
}

fn looks_like_diff_block(lines: &[String]) -> bool {
    if lines.is_empty() {
        return false;
    }

    let mut score = 0usize;
    for line in lines {
        let trimmed = line.trim_start();
        if trimmed.starts_with("diff --git")
            || trimmed.starts_with("@@")
            || trimmed.starts_with("--- ")
            || trimmed.starts_with("+++ ")
            || trimmed.starts_with("index ")
        {
            score += 2;
            continue;
        }
        if trimmed.starts_with('+') || trimmed.starts_with('-') {
            score += 1;
        }
    }
    score >= 3
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
    let mut user_requested_exit = false;

    loop {
        if enable_keyboard_controls {
            if let Ok(Some(key)) = read_keyboard_event() {
                sink.on_key_event(key);
                if is_quit_key(key) {
                    if quit_requested_at.is_none() {
                        user_requested_exit = true;
                        sink.on_status("stopping subprocess (user requested exit)");
                        if let Some(interrupt_sender) = spawned.interrupt_sender.take() {
                            let _ = interrupt_sender.send(());
                        }
                    }
                    quit_requested_at.get_or_insert_with(Instant::now);
                    let _ = child.start_kill();
                }
            }
        }

        if !got_exit_status && let Some(status) = child.try_wait()? {
            got_exit_status = true;
            exit_code = status.code();
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
            if got_exit_status && streams_closed >= expected_streams {
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
        user_requested_exit,
    })
}

fn read_keyboard_event() -> io::Result<Option<KeyEvent>> {
    if !event::poll(Duration::from_millis(0))? {
        return Ok(None);
    }
    let Event::Key(key) = event::read()? else {
        return Ok(None);
    };
    Ok(Some(key))
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

fn apply_prompt_input_key(input: &mut String, key: KeyEvent) -> PromptInputUpdate {
    if !matches!(key.kind, KeyEventKind::Press | KeyEventKind::Repeat) {
        return PromptInputUpdate::Continue;
    }

    if is_prompt_quit_key(key) {
        return PromptInputUpdate::Quit;
    }

    match key.code {
        KeyCode::Enter => {
            let prompt = input.trim();
            if prompt.is_empty() {
                PromptInputUpdate::Continue
            } else {
                let prompt = prompt.to_string();
                input.clear();
                PromptInputUpdate::Submit(prompt)
            }
        }
        KeyCode::Backspace => {
            input.pop();
            PromptInputUpdate::Continue
        }
        KeyCode::Tab => {
            input.push('\t');
            PromptInputUpdate::Continue
        }
        KeyCode::Char(c) => {
            if key
                .modifiers
                .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT)
            {
                PromptInputUpdate::Continue
            } else {
                input.push(c);
                PromptInputUpdate::Continue
            }
        }
        _ => PromptInputUpdate::Continue,
    }
}

fn is_prompt_quit_key(key: KeyEvent) -> bool {
    if !matches!(key.kind, KeyEventKind::Press | KeyEventKind::Repeat) {
        return false;
    }

    matches!(key.code, KeyCode::Esc)
        || matches!(key.code, KeyCode::Char('c') | KeyCode::Char('C'))
            && key.modifiers.contains(KeyModifiers::CONTROL)
}

fn read_tui_prompt_action(sink: &mut TuiLogSink) -> io::Result<TuiPromptAction> {
    let mut input = String::new();
    sink.set_input_text(String::new());

    loop {
        if !event::poll(Duration::from_millis(100))? {
            continue;
        }

        match event::read()? {
            Event::Key(key) => {
                if !matches!(key.kind, KeyEventKind::Press | KeyEventKind::Repeat) {
                    continue;
                }
                if matches!(key.code, KeyCode::Tab) {
                    sink.toggle_pane();
                    continue;
                }

                if sink.active_pane() == TuiPane::Logs {
                    if is_prompt_quit_key(key) {
                        sink.set_input_text(String::new());
                        return Ok(TuiPromptAction::Quit);
                    }
                    if handle_logs_navigation_key(sink, key) {
                        continue;
                    }
                    continue;
                }

                match apply_prompt_input_key(&mut input, key) {
                    PromptInputUpdate::Continue => sink.set_input_text(input.clone()),
                    PromptInputUpdate::Submit(prompt) => {
                        sink.set_input_text(String::new());
                        return Ok(TuiPromptAction::Submit(prompt));
                    }
                    PromptInputUpdate::Quit => {
                        sink.set_input_text(String::new());
                        return Ok(TuiPromptAction::Quit);
                    }
                }
            }
            Event::Paste(pasted) => {
                if sink.active_pane() == TuiPane::Prompt {
                    input.push_str(&pasted);
                    sink.set_input_text(input.clone());
                }
            }
            _ => {}
        }
    }
}

fn handle_logs_navigation_key(sink: &mut TuiLogSink, key: KeyEvent) -> bool {
    match key.code {
        KeyCode::Up => {
            sink.scroll_logs_up(1);
            true
        }
        KeyCode::Down => {
            sink.scroll_logs_down(1);
            true
        }
        KeyCode::PageUp => {
            sink.scroll_logs_up(10);
            true
        }
        KeyCode::PageDown => {
            sink.scroll_logs_down(10);
            true
        }
        KeyCode::Home => {
            sink.scroll_logs_to_top();
            true
        }
        KeyCode::End => {
            sink.scroll_logs_to_bottom();
            true
        }
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
    fn prompt_input_keys_support_edit_submit_and_quit() {
        let mut input = String::new();
        assert!(matches!(
            apply_prompt_input_key(&mut input, KeyEvent::new(KeyCode::Char('h'), KeyModifiers::NONE)),
            PromptInputUpdate::Continue
        ));
        assert!(matches!(
            apply_prompt_input_key(&mut input, KeyEvent::new(KeyCode::Char('i'), KeyModifiers::NONE)),
            PromptInputUpdate::Continue
        ));
        assert_eq!(input, "hi");

        assert!(matches!(
            apply_prompt_input_key(&mut input, KeyEvent::new(KeyCode::Char('q'), KeyModifiers::NONE)),
            PromptInputUpdate::Continue
        ));
        assert_eq!(input, "hiq");

        assert!(matches!(
            apply_prompt_input_key(&mut input, KeyEvent::new(KeyCode::Backspace, KeyModifiers::NONE)),
            PromptInputUpdate::Continue
        ));
        assert_eq!(input, "hi");

        let submit = apply_prompt_input_key(&mut input, KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        match submit {
            PromptInputUpdate::Submit(prompt) => assert_eq!(prompt, "hi"),
            _ => panic!("expected submit"),
        }
        assert!(input.is_empty());

        assert!(matches!(
            apply_prompt_input_key(&mut input, KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE)),
            PromptInputUpdate::Quit
        ));
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
    fn codex_agent_message_formats_json_and_code_blocks() {
        let mut formatter = LogFormatter::new(ExecutorKind::Codex);
        let events = formatter.format(
            LogStream::Stdout,
            r#"{"type":"item.completed","item":{"type":"agent_message","text":"Here is data:\n```json\n{\"ok\":true,\"count\":2}\n```\nAnd code:\n```rust\nfn main() {}\n```"}}"#,
        );

        assert!(events.iter().any(|event| matches!(
            event,
            DisplayEvent::Line(LogStream::Stdout, line) if line == "agent: Here is data:"
        )));
        assert!(events.iter().any(|event| matches!(
            event,
            DisplayEvent::Line(LogStream::Stdout, line) if line.starts_with("json| {")
        )));
        assert!(events.iter().any(|event| matches!(
            event,
            DisplayEvent::Line(LogStream::Stdout, line) if line == "code: rust"
        )));
        assert!(events.iter().any(|event| matches!(
            event,
            DisplayEvent::Line(LogStream::Stdout, line) if line.contains("code| fn main() {}")
        )));
    }

    #[test]
    fn codex_agent_message_formats_diff_blocks() {
        let mut formatter = LogFormatter::new(ExecutorKind::Codex);
        let events = formatter.format(
            LogStream::Stdout,
            r#"{"type":"item.completed","item":{"type":"agent_message","text":"```diff\n@@ -1 +1 @@\n-old\n+new\n```"}}"#,
        );

        assert!(events.iter().any(|event| matches!(
            event,
            DisplayEvent::Line(LogStream::Stdout, line) if line == "diff| @@ -1 +1 @@"
        )));
        assert!(events.iter().any(|event| matches!(
            event,
            DisplayEvent::Line(LogStream::Stdout, line) if line == "diff| -old"
        )));
        assert!(events.iter().any(|event| matches!(
            event,
            DisplayEvent::Line(LogStream::Stdout, line) if line == "diff| +new"
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
