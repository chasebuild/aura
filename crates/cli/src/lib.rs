use std::collections::{HashMap, HashSet, VecDeque};
use std::fs;
use std::io::{self, IsTerminal, Write};
use std::path::PathBuf;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use aura_contracts::{ExecutorKind, SessionId};
use aura_executors::adapters::{
    AmpOptions, ClaudeOptions, CodexOptions, CopilotOptions, CursorAgentOptions, DroidOptions,
    GeminiOptions, LocalOptions, OllamaOptions, OpencodeOptions, QwenCodeOptions,
};
use aura_executors::{
    AppendPrompt, CmdOverrides, ExecutionEnv, ExecutorError, ExecutorProfileId, RepoContext,
    SpawnedChild, StandardCodingAgentExecutor, adapters,
};
use aura_usage::UsageTracker;
use clap::{Args, CommandFactory, Parser, Subcommand, ValueEnum};
use clap_complete::{Shell, generate};
#[cfg(test)]
use crossterm::event::KeyModifiers;
use crossterm::{
    cursor::Show,
    event::{KeyCode, KeyEvent, KeyEventKind},
    execute,
    terminal::{EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode},
};
use duct::cmd;
use ratatui::{
    Terminal,
    backend::CrosstermBackend,
    layout::{Constraint, Direction, Layout},
    style::{Color, Style},
    text::{Line, Span, Text},
    widgets::{Block, Borders, Paragraph, Wrap},
};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use thiserror::Error;

mod local_exec;
mod runtime;
mod session_store;

#[cfg(test)]
type DisplayEvent = runtime::DisplayEvent;
#[cfg(test)]
type LogFormatter = runtime::LogFormatter;

#[derive(Debug, Clone)]
pub struct RunOptions {
    pub executor: ExecutorKind,
    pub prompt: String,
    pub cwd: PathBuf,
    pub session_id: Option<String>,
    pub resume: bool,
    pub refresh_model_cache: bool,
    pub review: bool,
    pub env_vars: HashMap<String, String>,
    pub base_command_override: Option<String>,
    pub additional_params: Vec<String>,
    pub append_prompt: Option<String>,
    pub model: Option<String>,
    pub executor_mode: Option<String>,
    pub openai_endpoint: Option<String>,
    pub(crate) local_provider: Option<LocalProvider>,
    pub yolo: bool,
    pub force: bool,
    pub trust: bool,
    pub auto_approve: bool,
    pub allow_all_tools: bool,
    pub executor_paths: HashMap<String, String>,
    pub tui: bool,
}

#[derive(Debug, Clone)]
pub(crate) struct SessionContext {
    session_id: String,
    executor: ExecutorKind,
    cwd: PathBuf,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct ModelCache {
    version: u8,
    updated_at: u64,
    models: Vec<String>,
    sources: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
struct ExecutorPathCache {
    entries: HashMap<String, String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum LocalProvider {
    Ollama,
    LmStudio,
    Custom,
}

impl Default for RunOptions {
    fn default() -> Self {
        Self {
            executor: ExecutorKind::Codex,
            prompt: String::new(),
            cwd: std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")),
            session_id: None,
            resume: false,
            refresh_model_cache: false,
            review: false,
            env_vars: HashMap::new(),
            base_command_override: None,
            additional_params: Vec::new(),
            append_prompt: None,
            model: None,
            executor_mode: None,
            openai_endpoint: None,
            local_provider: None,
            yolo: false,
            force: false,
            trust: false,
            auto_approve: true,
            allow_all_tools: false,
            executor_paths: HashMap::new(),
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
    fn on_usage_status(&mut self, _message: &str) {}
    fn on_model_unavailable(&mut self) {}
    fn flush(&mut self) {}
    fn on_exit(&mut self, code: Option<i32>);
    fn on_key_event(&mut self, _key: KeyEvent) -> SinkKeyAction {
        SinkKeyAction::None
    }
    fn on_paste(&mut self, _text: &str) {}
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SinkKeyAction {
    None,
    RequestQuit,
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
        if let Some(session_id) = &options.session_id {
            let _ = writeln!(
                io::stdout(),
                "[aura] session: {session_id} (resume with --session {session_id})"
            );
        }
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
    task_summary: String,
    status: String,
    agent_status: String,
    usage_status: String,
    input_text: String,
    queued_prompts: VecDeque<TuiPromptSubmission>,
    queue_cursor: usize,
    active_pane: TuiPane,
    executor: Option<ExecutorKind>,
    executor_mode: Option<String>,
    model_config: ModelConfiguration,
    last_redraw: Instant,
    redraw_interval: Duration,
    pending_redraw: bool,
    terminal: Option<Terminal<CrosstermBackend<io::Stdout>>>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TuiLineKind {
    User,
    Agent,
    AgentMarkdown,
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
    Queue,
    Prompt,
    Model,
}

#[derive(Debug, Clone)]
struct TuiLogEntry {
    kind: TuiLineKind,
    text: String,
}

#[derive(Debug, Clone)]
struct TuiPromptSubmission {
    prompt: String,
    model: Option<String>,
    mode: Option<String>,
}

#[derive(Debug, Clone)]
struct ModelConfiguration {
    options: Vec<String>,
    selected_index: usize,
    unavailable: bool,
}

impl ModelConfiguration {
    fn new() -> Self {
        Self {
            options: Vec::new(),
            selected_index: 0,
            unavailable: false,
        }
    }

    fn configure(
        &mut self,
        executor: &ExecutorKind,
        selected_model: Option<&str>,
        refresh_model_cache: bool,
        local_provider: Option<LocalProvider>,
    ) {
        self.options = model_options_for_executor(executor, refresh_model_cache, local_provider);
        self.unavailable = false;

        if let Some(model) = selected_model {
            let trimmed = model.trim();
            if !trimmed.is_empty() && !self.options.iter().any(|option| option == trimmed) {
                self.options.insert(0, trimmed.to_string());
            }
        }

        self.selected_index = selected_model
            .and_then(|model| self.options.iter().position(|option| option == model))
            .unwrap_or(0);
    }

    fn selected_model(&self) -> Option<String> {
        if self.unavailable {
            return None;
        }
        self.options.get(self.selected_index).cloned()
    }

    fn selected_label(&self) -> Option<String> {
        self.options
            .get(self.selected_index)
            .cloned()
            .or_else(|| self.options.first().cloned())
    }

    fn choices_label(&self) -> String {
        self.options.join(" | ")
    }

    fn cycle_next(&mut self) {
        if self.options.is_empty() {
            return;
        }
        self.selected_index = (self.selected_index + 1) % self.options.len();
    }

    fn cycle_prev(&mut self) {
        if self.options.is_empty() {
            return;
        }
        if self.selected_index == 0 {
            self.selected_index = self.options.len() - 1;
        } else {
            self.selected_index -= 1;
        }
    }

    fn mark_unavailable(&mut self) {
        self.options = vec!["unavailable".to_string()];
        self.selected_index = 0;
        self.unavailable = true;
    }

    fn has_options(&self) -> bool {
        !self.options.is_empty()
    }

    fn select_by_label(&mut self, value: &str) -> bool {
        let trimmed = value.trim();
        if trimmed.is_empty() {
            return false;
        }
        if let Some(index) = self
            .options
            .iter()
            .position(|option| option.eq_ignore_ascii_case(trimmed))
        {
            self.selected_index = index;
            true
        } else {
            false
        }
    }

    fn select_by_ordinal(&mut self, ordinal: usize) -> bool {
        if ordinal == 0 || ordinal > self.options.len() {
            return false;
        }
        self.selected_index = ordinal - 1;
        true
    }
}

fn classify_tui_line(stream: LogStream, line: &str) -> TuiLineKind {
    if line.starts_with("user: ") {
        return TuiLineKind::User;
    }
    if line.starts_with("agent_md: ") {
        return TuiLineKind::AgentMarkdown;
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
    let normalized = match kind {
        TuiLineKind::User => line.trim_start_matches("user: ").to_string(),
        TuiLineKind::AgentMarkdown => line.trim_start_matches("agent_md: ").to_string(),
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
        TuiLineKind::Thinking => normalize_thinking_text(line.trim_start_matches("thinking: ")),
        TuiLineKind::Action => line.to_string(),
        TuiLineKind::Error => line.to_string(),
        TuiLineKind::Output => line.to_string(),
    };
    sanitize_tui_text(&normalized)
}

fn normalize_thinking_text(text: &str) -> String {
    text.replace("**", "").replace("__", "")
}

fn sanitize_tui_text(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    let mut chars = input.chars().peekable();

    while let Some(ch) = chars.next() {
        if ch == '\u{1b}' {
            match chars.peek().copied() {
                // CSI: ESC [ ... <final-byte>
                Some('[') => {
                    let _ = chars.next();
                    for c in chars.by_ref() {
                        if ('@'..='~').contains(&c) {
                            break;
                        }
                    }
                }
                // OSC: ESC ] ... BEL or ESC \
                Some(']') => {
                    let _ = chars.next();
                    while let Some(c) = chars.next() {
                        if c == '\u{7}' {
                            break;
                        }
                        if c == '\u{1b}' && matches!(chars.peek(), Some('\\')) {
                            let _ = chars.next();
                            break;
                        }
                    }
                }
                Some(_) => {
                    let _ = chars.next();
                }
                None => {}
            }
            continue;
        }

        match ch {
            '\t' => out.push_str("    "),
            c if c.is_control() => {}
            _ => out.push(ch),
        }
    }

    out
}

fn normalize_executor_mode(mode: Option<&str>) -> Option<String> {
    mode.and_then(|raw| {
        let trimmed = raw.trim();
        if trimmed.is_empty() {
            None
        } else {
            Some(trimmed.to_ascii_lowercase())
        }
    })
}

fn mode_display(mode: Option<&str>) -> String {
    match normalize_executor_mode(mode) {
        None => "execute".to_string(),
        Some(value) => value,
    }
}

fn executor_supports_mode(executor: &ExecutorKind, mode: &str) -> bool {
    match normalize_executor_mode(Some(mode)).as_deref() {
        None | Some("execute") | Some("default") => true,
        Some("plan") | Some("yolo") => matches!(
            executor,
            ExecutorKind::Codex | ExecutorKind::Claude | ExecutorKind::Opencode
        ),
        Some("approvals") | Some("approval") => {
            matches!(executor, ExecutorKind::Claude | ExecutorKind::Opencode)
        }
        Some(_) => matches!(executor, ExecutorKind::Opencode),
    }
}

impl TuiLogSink {
    pub fn new() -> Self {
        let terminal = Self::init_terminal();
        let now = Instant::now();
        Self {
            started_at: now,
            lines: VecDeque::new(),
            max_lines: 5_000,
            log_scroll: 0,
            title: "AURA Executor".to_string(),
            task_summary: "No task yet".to_string(),
            status: "starting".to_string(),
            agent_status: "Initializing".to_string(),
            usage_status: "Usage: -- | Cost: --".to_string(),
            input_text: String::new(),
            queued_prompts: VecDeque::new(),
            queue_cursor: 0,
            active_pane: TuiPane::Prompt,
            executor: None,
            executor_mode: None,
            model_config: ModelConfiguration::new(),
            last_redraw: now,
            redraw_interval: Duration::from_millis(16),
            pending_redraw: false,
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
                "{} | task={} | uptime={:.1?} | status={} | agent={}",
                self.title,
                self.task_summary,
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
                    Constraint::Length(3),
                    Constraint::Min(6),
                    Constraint::Length(4),
                    Constraint::Length(4),
                    Constraint::Length(1),
                ])
                .split(size);

            let topbar = Paragraph::new(vec![
                Line::from(vec![
                    Span::styled(
                        " AURA ",
                        Style::default()
                            .fg(Color::Black)
                            .bg(Color::Cyan)
                            .add_modifier(ratatui::style::Modifier::BOLD),
                    ),
                    Span::raw(" "),
                    Span::styled(self.title.clone(), Style::default().fg(Color::White)),
                    Span::raw("  "),
                    Span::styled("agent:", Style::default().fg(Color::Gray)),
                    Span::raw(" "),
                    Span::styled(self.agent_status.clone(), Style::default().fg(Color::White)),
                    Span::raw("  "),
                    Span::styled("mode:", Style::default().fg(Color::Gray)),
                    Span::raw(" "),
                    Span::styled(
                        mode_display(self.executor_mode.as_deref()),
                        Style::default().fg(Color::White),
                    ),
                    Span::raw("  "),
                    Span::styled("queue:", Style::default().fg(Color::Gray)),
                    Span::raw(" "),
                    Span::styled(
                        self.queued_prompts.len().to_string(),
                        Style::default().fg(Color::White),
                    ),
                ]),
                Line::from(vec![
                    Span::styled("status:", Style::default().fg(Color::Gray)),
                    Span::raw(" "),
                    Span::styled(self.status.clone(), Style::default().fg(Color::White)),
                ]),
                Line::from(vec![
                    Span::styled("usage:", Style::default().fg(Color::Gray)),
                    Span::raw(" "),
                    Span::styled(self.usage_status.clone(), Style::default().fg(Color::White)),
                ]),
            ])
            .wrap(Wrap { trim: false })
            .style(Style::default().bg(Color::DarkGray));
            frame.render_widget(topbar, chunks[0]);

            let task_chunks = Layout::default()
                .direction(Direction::Horizontal)
                .constraints([Constraint::Min(20), Constraint::Length(26)])
                .split(chunks[1]);

            let task_title = Paragraph::new(vec![
                Line::from(Span::styled(
                    format!("# {}", self.task_summary),
                    Style::default()
                        .fg(Color::White)
                        .add_modifier(ratatui::style::Modifier::BOLD),
                )),
                Line::from(vec![
                    Span::styled("status ", Style::default().fg(Color::DarkGray)),
                    Span::styled(self.status.clone(), Style::default().fg(Color::White)),
                ]),
            ])
            .block(
                Block::default()
                    .borders(Borders::ALL)
                    .border_style(Style::default().fg(Color::DarkGray)),
            )
            .wrap(Wrap { trim: false });
            frame.render_widget(task_title, task_chunks[0]);

            let task_meta = Paragraph::new(vec![
                Line::from(vec![
                    Span::styled("uptime ", Style::default().fg(Color::DarkGray)),
                    Span::styled(
                        format!("{:.1?}", self.started_at.elapsed()),
                        Style::default().fg(Color::White),
                    ),
                ]),
                Line::from(vec![
                    Span::styled("focus ", Style::default().fg(Color::DarkGray)),
                    Span::styled(
                        format!("{:?}", self.active_pane).to_lowercase(),
                        Style::default().fg(Color::Cyan),
                    ),
                ]),
            ])
            .block(
                Block::default()
                    .borders(Borders::ALL)
                    .border_style(Style::default().fg(Color::DarkGray)),
            )
            .wrap(Wrap { trim: false });
            frame.render_widget(task_meta, task_chunks[1]);

            let logs_capacity = chunks[2].height.saturating_sub(2) as usize;
            let max_log_scroll = self.lines.len().saturating_sub(logs_capacity);
            let scroll = self.log_scroll.min(max_log_scroll);
            let end = self.lines.len().saturating_sub(scroll);
            let start = end.saturating_sub(logs_capacity);
            let visible_logs: Vec<TuiLogEntry> = self
                .lines
                .iter()
                .skip(start)
                .take(end - start)
                .cloned()
                .collect();
            let mut log_lines: Vec<Line<'static>> = Vec::new();
            for entry in visible_logs {
                match entry.kind {
                    TuiLineKind::User => log_lines.push(Line::from(vec![
                        Span::styled("│ ", Style::default().fg(Color::Cyan)),
                        Span::styled(
                            entry.text,
                            Style::default().fg(Color::White).bg(Color::DarkGray),
                        ),
                    ])),
                    TuiLineKind::Agent => log_lines.push(Line::from(Span::styled(
                        entry.text,
                        Style::default().fg(Color::White),
                    ))),
                    TuiLineKind::AgentMarkdown => {
                        for (index, line) in entry.text.lines().enumerate() {
                            if line.trim().is_empty() {
                                continue;
                            }
                            let content = sanitize_tui_text(line);
                            if index == 0 {
                                log_lines.push(Line::from(vec![
                                    Span::styled("assistant ", Style::default().fg(Color::DarkGray)),
                                    Span::styled(content, Style::default().fg(Color::White)),
                                ]));
                            } else {
                                log_lines.push(Line::from(Span::styled(
                                    content,
                                    Style::default().fg(Color::White),
                                )));
                            }
                        }
                    }
                    TuiLineKind::Code => log_lines.push(Line::from(vec![
                        Span::styled("> ", Style::default().fg(Color::DarkGray)),
                        Span::styled(entry.text, Style::default().fg(Color::Cyan)),
                    ])),
                    TuiLineKind::Json => log_lines.push(Line::from(vec![
                        Span::styled("> ", Style::default().fg(Color::DarkGray)),
                        Span::styled(entry.text, Style::default().fg(Color::Green)),
                    ])),
                    TuiLineKind::Diff => log_lines.push(Line::from(vec![
                        Span::styled("> ", Style::default().fg(Color::DarkGray)),
                        Span::styled(entry.text.clone(), diff_line_style(&entry.text)),
                    ])),
                    TuiLineKind::Thinking => log_lines.push(Line::from(vec![
                        Span::styled("~ ", Style::default().fg(Color::DarkGray)),
                        Span::styled(
                            entry.text,
                            Style::default()
                                .fg(Color::DarkGray)
                                .add_modifier(ratatui::style::Modifier::ITALIC),
                        ),
                    ])),
                    TuiLineKind::Action => log_lines.push(Line::from(vec![
                        Span::styled("* ", Style::default().fg(Color::DarkGray)),
                        Span::styled(entry.text, Style::default().fg(Color::Blue)),
                    ])),
                    TuiLineKind::Error => log_lines.push(Line::from(vec![
                        Span::styled("! ", Style::default().fg(Color::Red)),
                        Span::styled(entry.text, Style::default().fg(Color::Red)),
                    ])),
                    TuiLineKind::Output => {
                        log_lines.push(Line::from(Span::styled(entry.text, Style::default().fg(Color::Gray))))
                    }
                }
            }

            let logs_title = if max_log_scroll == 0 {
                "Conversation".to_string()
            } else {
                format!("Conversation ({}/{})", scroll, max_log_scroll)
            };
            let logs_border = if self.active_pane == TuiPane::Logs {
                Style::default().fg(Color::Cyan)
            } else {
                Style::default().fg(Color::DarkGray)
            };
            let logs = Paragraph::new(Text::from(log_lines))
                .block(
                    Block::default()
                        .borders(Borders::ALL)
                        .title(logs_title)
                        .border_style(logs_border),
                )
                .wrap(Wrap { trim: false });
            frame.render_widget(logs, chunks[2]);

            let queue_border = if self.active_pane == TuiPane::Queue {
                Style::default().fg(Color::Cyan)
            } else {
                Style::default().fg(Color::DarkGray)
            };
            let queue_lines: Vec<Line<'static>> = if self.queued_prompts.is_empty() {
                vec![Line::from(Span::styled(
                    "No queued prompts. Enter text in Composer and press Enter to queue.",
                    Style::default().fg(Color::DarkGray),
                ))]
            } else {
                self.queued_prompts
                    .iter()
                    .enumerate()
                    .map(|(index, item)| {
                        let marker = if index == self.queue_cursor {
                            if self.active_pane == TuiPane::Queue {
                                "▶ "
                            } else {
                                "• "
                            }
                        } else {
                            "  "
                        };
                        Line::from(vec![
                            Span::styled(marker, Style::default().fg(Color::Cyan)),
                            Span::styled(
                                format!("{}.", index + 1),
                                Style::default().fg(Color::DarkGray),
                            ),
                            Span::raw(" "),
                            Span::styled(
                                summarize_text(&item.prompt, 120),
                                Style::default().fg(Color::White),
                            ),
                        ])
                    })
                    .collect()
            };
            let queue_title = if self.queued_prompts.is_empty() {
                "Queue".to_string()
            } else {
                format!(
                    "Queue ({})  Del/Backspace: remove selected",
                    self.queued_prompts.len()
                )
            };
            let queue = Paragraph::new(Text::from(queue_lines))
                .block(
                    Block::default()
                        .borders(Borders::ALL)
                        .title(queue_title)
                        .border_style(queue_border),
                )
                .wrap(Wrap { trim: false });
            frame.render_widget(queue, chunks[3]);

            let prompt_text = if self.input_text.is_empty() {
                Line::from(Span::styled(
                    "Type a follow-up prompt and press Enter",
                    Style::default().fg(Color::DarkGray),
                ))
            } else {
                Line::from(Span::styled(
                    self.input_text.clone(),
                    Style::default().fg(Color::White),
                ))
            };
            let model_line = if !self.model_config.has_options() {
                Line::from(Span::styled(
                    "model: n/a (override unsupported for this executor)",
                    Style::default().fg(Color::DarkGray),
                ))
            } else {
                let selected = self
                    .model_config
                    .selected_label()
                    .unwrap_or_else(|| "n/a".to_string());
                let choices = summarize_text(&self.model_config.choices_label(), 80);
                if self.active_pane == TuiPane::Model {
                    Line::from(vec![
                        Span::styled("model < ", Style::default().fg(Color::Cyan)),
                        Span::styled(
                            selected,
                            Style::default()
                                .fg(Color::White)
                                .add_modifier(ratatui::style::Modifier::BOLD),
                        ),
                        Span::styled(" >  ", Style::default().fg(Color::Cyan)),
                        Span::styled("choices: ", Style::default().fg(Color::DarkGray)),
                        Span::styled(choices, Style::default().fg(Color::Gray)),
                    ])
                } else {
                    Line::from(vec![
                        Span::styled("model ", Style::default().fg(Color::DarkGray)),
                        Span::styled(selected, Style::default().fg(Color::White)),
                        Span::styled("  (Tab to focus)", Style::default().fg(Color::DarkGray)),
                    ])
                }
            };
            let composer_border = if self.active_pane == TuiPane::Prompt {
                Style::default().fg(Color::Cyan)
            } else if self.active_pane == TuiPane::Model {
                Style::default().fg(Color::Blue)
            } else {
                Style::default().fg(Color::DarkGray)
            };
            let composer = Paragraph::new(Text::from(vec![prompt_text, model_line]))
                .block(
                    Block::default()
                        .borders(Borders::ALL)
                        .title("Composer")
                        .border_style(composer_border),
                )
                .wrap(Wrap { trim: false });
            frame.render_widget(composer, chunks[4]);

            let pane_text = match self.active_pane {
                TuiPane::Logs => "Logs",
                TuiPane::Queue => "Queue",
                TuiPane::Prompt => "Prompt",
                TuiPane::Model => "Model",
            };
            let footer = Paragraph::new(Line::from(vec![
                Span::styled("focus ", Style::default().fg(Color::DarkGray)),
                Span::styled(pane_text, Style::default().fg(Color::Cyan)),
                Span::raw("  |  "),
                Span::styled(
                    "Tab switch  Queue: Up/Down+Del  Logs: Arrows/Page/Home/End  <-/-> model  Enter queue/send  Esc/Ctrl+C quit",
                    Style::default().fg(Color::Gray),
                ),
            ]))
            .style(Style::default().fg(Color::Gray).bg(Color::Black));
            frame.render_widget(footer, chunks[5]);
        });
    }

    fn request_redraw(&mut self, force: bool) {
        let now = Instant::now();
        if force || now.duration_since(self.last_redraw) >= self.redraw_interval {
            self.last_redraw = now;
            self.pending_redraw = false;
            self.redraw();
        } else {
            self.pending_redraw = true;
        }
    }

    fn set_active_pane(&mut self, pane: TuiPane) {
        self.active_pane = pane;
        self.redraw();
    }

    fn toggle_pane(&mut self) {
        self.active_pane = match self.active_pane {
            TuiPane::Logs => TuiPane::Queue,
            TuiPane::Queue => TuiPane::Prompt,
            TuiPane::Prompt => TuiPane::Model,
            TuiPane::Model => TuiPane::Logs,
        };
        self.request_redraw(true);
    }

    fn configure_model_options(
        &mut self,
        executor: &ExecutorKind,
        selected_model: Option<&str>,
        refresh_model_cache: bool,
        local_provider: Option<LocalProvider>,
    ) {
        self.model_config.configure(
            executor,
            selected_model,
            refresh_model_cache,
            local_provider,
        );
        self.request_redraw(true);
    }

    fn selected_model(&self) -> Option<String> {
        self.model_config.selected_model()
    }

    fn cycle_model_next(&mut self) {
        self.model_config.cycle_next();
        self.request_redraw(true);
    }

    fn cycle_model_prev(&mut self) {
        self.model_config.cycle_prev();
        self.request_redraw(true);
    }

    fn mark_model_unavailable(&mut self) {
        self.model_config.mark_unavailable();
        self.request_redraw(true);
    }

    fn max_log_scroll(&self) -> usize {
        self.lines.len().saturating_sub(1)
    }

    fn scroll_logs_up(&mut self, by: usize) {
        if by == 0 {
            return;
        }
        self.log_scroll = self
            .log_scroll
            .saturating_add(by)
            .min(self.max_log_scroll());
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
        if text.trim().is_empty() {
            return;
        }
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

    fn handle_prompt_edit_key(&mut self, key: KeyEvent) -> SinkKeyAction {
        match apply_prompt_input_key(&mut self.input_text, key) {
            PromptInputUpdate::Continue => {
                self.request_redraw(true);
                SinkKeyAction::None
            }
            PromptInputUpdate::Submit(prompt) => {
                self.input_text.clear();
                self.on_prompt_submit(prompt);
                SinkKeyAction::None
            }
            PromptInputUpdate::Quit => SinkKeyAction::RequestQuit,
        }
    }

    fn on_prompt_submit(&mut self, prompt: String) {
        let normalized_prompt = if prompt.starts_with("//") {
            prompt.trim_start_matches('/').to_string()
        } else {
            prompt
        };

        if self.handle_slash_command(&normalized_prompt) {
            return;
        }
        let summary = summarize_text(&normalized_prompt, 240);
        self.queued_prompts.push_back(TuiPromptSubmission {
            prompt: normalized_prompt,
            model: self.selected_model(),
            mode: self.executor_mode.clone(),
        });
        self.clamp_queue_cursor();
        self.push_log_entry(TuiLineKind::Action, format!("queued: {summary}"));
        self.status = format!("queued {} prompt(s)", self.queued_prompts.len());
        self.request_redraw(true);
    }

    fn take_queued_prompt(&mut self) -> Option<TuiPromptSubmission> {
        let item = self.queued_prompts.pop_front();
        self.clamp_queue_cursor();
        item
    }

    fn clamp_queue_cursor(&mut self) {
        if self.queued_prompts.is_empty() {
            self.queue_cursor = 0;
        } else {
            self.queue_cursor = self.queue_cursor.min(self.queued_prompts.len() - 1);
        }
    }

    fn queue_select_prev(&mut self) {
        if self.queued_prompts.is_empty() || self.queue_cursor == 0 {
            return;
        }
        self.queue_cursor -= 1;
        self.request_redraw(true);
    }

    fn queue_select_next(&mut self) {
        if self.queued_prompts.is_empty() {
            return;
        }
        self.queue_cursor = (self.queue_cursor + 1).min(self.queued_prompts.len() - 1);
        self.request_redraw(true);
    }

    fn remove_selected_queued_prompt(&mut self) {
        if self.queued_prompts.is_empty() {
            self.status = "queue is already empty".to_string();
            self.request_redraw(true);
            return;
        }
        let removed = self.queued_prompts.remove(self.queue_cursor);
        self.clamp_queue_cursor();
        if let Some(item) = removed {
            self.status = format!(
                "removed queued prompt: {}",
                summarize_text(&item.prompt, 80)
            );
        } else {
            self.status = "failed to remove queued prompt".to_string();
        }
        self.request_redraw(true);
    }

    fn set_executor_mode(&mut self, mode: Option<String>) {
        self.executor_mode = normalize_executor_mode(mode.as_deref());
        self.status = format!(
            "mode set to {}",
            mode_display(self.executor_mode.as_deref())
        );
        self.request_redraw(true);
    }

    fn handle_slash_command(&mut self, raw: &str) -> bool {
        let trimmed = raw.trim();
        if !trimmed.starts_with('/') {
            return false;
        }

        let mut parts = trimmed.split_whitespace();
        let command = parts.next().unwrap_or_default().to_ascii_lowercase();
        match command.as_str() {
            "/help" => {
                self.status = "commands: /mode [execute|plan|approvals|<custom>]  /plan  /execute  /model [name|index]  | Queue pane: Del/Backspace removes selected".to_string();
                self.request_redraw(true);
                true
            }
            "/plan" => self.try_update_mode("plan"),
            "/execute" => self.try_update_mode("execute"),
            "/mode" => {
                if let Some(value) = parts.next() {
                    self.try_update_mode(value)
                } else {
                    self.status = format!(
                        "current mode: {}",
                        mode_display(self.executor_mode.as_deref())
                    );
                    self.request_redraw(true);
                    true
                }
            }
            "/model" => {
                if let Some(value) = parts.next() {
                    self.try_update_model(value)
                } else {
                    let model = self.selected_model().unwrap_or_else(|| "n/a".to_string());
                    self.status = format!("current model: {model}");
                    self.request_redraw(true);
                    true
                }
            }
            _ => false,
        }
    }

    fn try_update_mode(&mut self, requested: &str) -> bool {
        let normalized = normalize_executor_mode(Some(requested));
        let Some(executor) = self.executor.as_ref() else {
            self.status = "mode changes are unavailable before executor starts".to_string();
            self.request_redraw(true);
            return true;
        };
        let requested_label = normalized.as_deref().unwrap_or("execute");
        if !executor_supports_mode(executor, requested_label) {
            self.status = format!(
                "mode '{requested_label}' is not supported for {:?}",
                executor
            );
            self.request_redraw(true);
            return true;
        }
        match requested_label {
            "execute" | "default" => self.set_executor_mode(None),
            _ => self.set_executor_mode(Some(requested_label.to_string())),
        }
        true
    }

    fn try_update_model(&mut self, requested: &str) -> bool {
        if !self.model_config.has_options() {
            self.status = "model selection is unavailable for this executor".to_string();
            self.request_redraw(true);
            return true;
        }

        if let Ok(ordinal) = requested.parse::<usize>()
            && self.model_config.select_by_ordinal(ordinal)
        {
            self.status = format!(
                "model set to {}",
                self.model_config
                    .selected_label()
                    .unwrap_or_else(|| "n/a".to_string())
            );
            self.request_redraw(true);
            return true;
        }
        if self.model_config.select_by_label(requested) {
            self.status = format!(
                "model set to {}",
                self.model_config
                    .selected_label()
                    .unwrap_or_else(|| "n/a".to_string())
            );
        } else {
            self.status = format!("model '{requested}' not found in choices");
        }
        self.request_redraw(true);
        true
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
        self.executor = Some(options.executor.clone());
        self.executor_mode = normalize_executor_mode(options.executor_mode.as_deref());
        self.title = format!(
            "{:?} @ {}",
            options.executor,
            summarize_text(&options.cwd.display().to_string(), 40)
        );
        self.task_summary = summarize_text(&options.prompt, 88);
        self.status = match &options.session_id {
            Some(session_id) => format!("starting (session {session_id})"),
            None => "starting".to_string(),
        };
        self.agent_status = "Initializing".to_string();
        self.usage_status = UsageTracker::new(&options.executor).initial_status();
        self.configure_model_options(
            &options.executor,
            options.model.as_deref(),
            options.refresh_model_cache,
            options.local_provider,
        );
        self.request_redraw(true);
    }

    fn on_line(&mut self, stream: LogStream, line: &str) {
        let kind = classify_tui_line(stream, line);
        self.push_log_entry(kind, normalize_tui_line_text(kind, line));
        self.status = "running".to_string();
        self.request_redraw(false);
    }

    fn on_status(&mut self, message: &str) {
        self.status = message.to_string();
        self.request_redraw(false);
    }

    fn on_agent_status(&mut self, message: &str) {
        self.agent_status = message.to_string();
        self.request_redraw(false);
    }

    fn on_usage_status(&mut self, message: &str) {
        self.usage_status = message.to_string();
        self.request_redraw(false);
    }

    fn on_model_unavailable(&mut self) {
        self.mark_model_unavailable();
    }

    fn on_exit(&mut self, code: Option<i32>) {
        self.status = format!("finished (exit={code:?})");
        if code == Some(0) {
            self.agent_status = "Idle".to_string();
        } else {
            self.agent_status = "Error".to_string();
        }
        self.request_redraw(true);
    }

    fn flush(&mut self) {
        if self.pending_redraw && self.last_redraw.elapsed() >= self.redraw_interval {
            self.request_redraw(true);
        }
    }

    fn on_key_event(&mut self, key: KeyEvent) -> SinkKeyAction {
        if !matches!(key.kind, KeyEventKind::Press | KeyEventKind::Repeat) {
            return SinkKeyAction::None;
        }
        if matches!(key.code, KeyCode::Tab) {
            self.toggle_pane();
            return SinkKeyAction::None;
        }
        match self.active_pane {
            TuiPane::Logs => {
                if is_prompt_quit_key(key) {
                    return SinkKeyAction::RequestQuit;
                }
                let _ = handle_logs_navigation_key(self, key);
                SinkKeyAction::None
            }
            TuiPane::Queue => match key.code {
                KeyCode::Up => {
                    self.queue_select_prev();
                    SinkKeyAction::None
                }
                KeyCode::Down => {
                    self.queue_select_next();
                    SinkKeyAction::None
                }
                KeyCode::Delete | KeyCode::Backspace | KeyCode::Char('x') | KeyCode::Char('X') => {
                    self.remove_selected_queued_prompt();
                    SinkKeyAction::None
                }
                _ => SinkKeyAction::None,
            },
            TuiPane::Model => match key.code {
                KeyCode::Left | KeyCode::Up => {
                    self.cycle_model_prev();
                    SinkKeyAction::None
                }
                KeyCode::Right | KeyCode::Down => {
                    self.cycle_model_next();
                    SinkKeyAction::None
                }
                _ => self.handle_prompt_edit_key(key),
            },
            TuiPane::Prompt => self.handle_prompt_edit_key(key),
        }
    }

    fn on_paste(&mut self, text: &str) {
        if matches!(self.active_pane, TuiPane::Logs | TuiPane::Queue) {
            return;
        }
        self.input_text.push_str(text);
        self.request_redraw(true);
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
    Run(Box<RunOptions>),
    SessionList(SessionListArgs),
    SessionShow { session_id: String },
    SessionLatest,
    Help,
    Completion { shell: Shell },
    LocalExec,
}

#[derive(Debug, Parser)]
#[command(
    name = "aura",
    version,
    about = "AURA executor runner",
    disable_help_subcommand = true
)]
struct AuraCli {
    #[command(subcommand)]
    command: Option<AuraSubcommand>,
}

#[derive(Debug, Subcommand)]
enum AuraSubcommand {
    Run(RunCliArgs),
    Tui(RunCliArgs),
    Session(SessionCommand),
    Help,
    Completion(CompletionArgs),
    #[command(name = "local-exec", hide = true)]
    LocalExec,
}

#[derive(Debug, Args)]
struct SessionCommand {
    #[command(subcommand)]
    command: SessionArgs,
}

#[derive(Debug, Subcommand)]
enum SessionArgs {
    List(SessionListArgs),
    Show(SessionShowArgs),
    Latest,
}

#[derive(Debug, Args)]
struct SessionShowArgs {
    session_id: String,
}

#[derive(Debug, Args, Clone)]
pub struct SessionListArgs {
    #[arg(long, value_enum)]
    executor: Option<ExecutorArg>,
    #[arg(long)]
    cwd: Option<PathBuf>,
    #[arg(long)]
    limit: Option<usize>,
}

#[derive(Debug, Clone, Copy, ValueEnum)]
enum ExecutorArg {
    Codex,
    Claude,
    #[value(name = "cursor-agent", alias = "cursor")]
    CursorAgent,
    Droid,
    Amp,
    Gemini,
    Opencode,
    Ollama,
    Lms,
    #[value(name = "qwen-code", alias = "qwen")]
    QwenCode,
    Copilot,
    Custom,
}

#[derive(Debug, Args)]
struct CompletionArgs {
    #[arg(value_enum)]
    shell: Shell,
}

#[derive(Debug, Args)]
struct RunCliArgs {
    #[arg(long, value_enum, default_value_t = ExecutorArg::Codex)]
    executor: ExecutorArg,
    #[arg(long)]
    prompt: String,
    #[arg(long)]
    cwd: Option<PathBuf>,
    #[arg(long = "session")]
    session_id: Option<String>,
    #[arg(long = "resume-latest")]
    resume_latest: bool,
    #[arg(long = "refresh-model-cache")]
    refresh_model_cache: bool,
    #[arg(long)]
    review: bool,
    #[arg(long = "var", value_name = "KEY=VALUE")]
    env_vars: Vec<String>,
    #[arg(long = "base-command")]
    base_command_override: Option<String>,
    #[arg(long = "param", allow_hyphen_values = true)]
    additional_params: Vec<String>,
    #[arg(long = "append-prompt")]
    append_prompt: Option<String>,
    #[arg(long = "model")]
    model: Option<String>,
    #[arg(long = "mode")]
    mode: Option<String>,
    #[arg(long = "openai-endpoint")]
    openai_endpoint: Option<String>,
    #[arg(long)]
    yolo: bool,
    #[arg(long, short = 'f')]
    force: bool,
    #[arg(long)]
    trust: bool,
    #[arg(long = "auto-approve")]
    auto_approve: Option<bool>,
    #[arg(long = "allow-all-tools")]
    allow_all_tools: bool,
    #[arg(long = "executor-path", value_name = "NAME=PATH")]
    executor_paths: Vec<String>,
    #[arg(long = "no-tui")]
    no_tui: bool,
}

fn executor_kind_from_arg(value: ExecutorArg) -> ExecutorKind {
    match value {
        ExecutorArg::Codex => ExecutorKind::Codex,
        ExecutorArg::Claude => ExecutorKind::Claude,
        ExecutorArg::CursorAgent => ExecutorKind::CursorAgent,
        ExecutorArg::Droid => ExecutorKind::Droid,
        ExecutorArg::Amp => ExecutorKind::Amp,
        ExecutorArg::Gemini => ExecutorKind::Gemini,
        ExecutorArg::Opencode => ExecutorKind::Opencode,
        ExecutorArg::Ollama => ExecutorKind::Local,
        ExecutorArg::Lms => ExecutorKind::Local,
        ExecutorArg::QwenCode => ExecutorKind::QwenCode,
        ExecutorArg::Copilot => ExecutorKind::Copilot,
        ExecutorArg::Custom => ExecutorKind::Local,
    }
}

fn local_provider_from_executor_arg(value: ExecutorArg) -> Option<LocalProvider> {
    match value {
        ExecutorArg::Ollama => Some(LocalProvider::Ollama),
        ExecutorArg::Lms => Some(LocalProvider::LmStudio),
        ExecutorArg::Custom => Some(LocalProvider::Custom),
        _ => None,
    }
}

fn normalize_executor_key(value: &str) -> Option<String> {
    match value.trim().to_ascii_lowercase().as_str() {
        "codex" => Some("codex".to_string()),
        "claude" => Some("claude".to_string()),
        "cursor-agent" | "cursor" => Some("cursor-agent".to_string()),
        "droid" => Some("droid".to_string()),
        "amp" => Some("amp".to_string()),
        "gemini" => Some("gemini".to_string()),
        "opencode" => Some("opencode".to_string()),
        "local" | "ollama" | "lms" | "custom" => Some("local".to_string()),
        "qwen-code" | "qwen" => Some("qwen-code".to_string()),
        "copilot" => Some("copilot".to_string()),
        _ => None,
    }
}

fn executor_key_for_kind(kind: &ExecutorKind) -> Option<&'static str> {
    match kind {
        ExecutorKind::Codex => Some("codex"),
        ExecutorKind::Claude => Some("claude"),
        ExecutorKind::CursorAgent => Some("cursor-agent"),
        ExecutorKind::Droid => Some("droid"),
        ExecutorKind::Amp => Some("amp"),
        ExecutorKind::Gemini => Some("gemini"),
        ExecutorKind::Opencode => Some("opencode"),
        ExecutorKind::Local => Some("local"),
        ExecutorKind::QwenCode => Some("qwen-code"),
        ExecutorKind::Copilot => Some("copilot"),
        ExecutorKind::Custom(_) => Some("custom"),
    }
}

fn parse_executor_path_arg(raw: &str) -> Result<(String, String), CliError> {
    let mut parts = raw.splitn(2, '=');
    let name = parts.next().unwrap_or("").trim();
    let path = parts.next().unwrap_or("").trim();
    if name.is_empty() || path.is_empty() {
        return Err(CliError::Arg(
            "--executor-path expects NAME=PATH".to_string(),
        ));
    }
    let normalized = normalize_executor_key(name).ok_or_else(|| {
        CliError::Arg(format!("unsupported executor for --executor-path: {name}"))
    })?;
    Ok((normalized, path.to_string()))
}

fn executor_path_override(
    kind: &ExecutorKind,
    overrides: &HashMap<String, String>,
) -> Option<String> {
    let key = executor_key_for_kind(kind)?;
    overrides.get(key).cloned()
}

fn prompt_for_custom_endpoint() -> Result<String, CliError> {
    if !io::stdin().is_terminal() || !io::stdout().is_terminal() {
        return Err(CliError::Arg(
            "custom executor requires --openai-endpoint when not running in a TTY".to_string(),
        ));
    }

    let endpoint: String = cliclack::input("Custom OpenAI endpoint")
        .placeholder("http://127.0.0.1:8000/v1")
        .interact()
        .map_err(|error| CliError::Runtime(format!("endpoint prompt failed: {error}")))?;

    let trimmed = endpoint.trim();
    if trimmed.is_empty() {
        return Err(CliError::Arg(
            "custom executor requires an endpoint".to_string(),
        ));
    }
    Ok(trimmed.to_string())
}

fn convert_run_cli_args(args: RunCliArgs, force_tui: bool) -> Result<RunOptions, CliError> {
    let executor_arg = args.executor;
    let legacy_custom_command =
        matches!(executor_arg, ExecutorArg::Custom) && args.base_command_override.is_some();
    let mut env_vars = HashMap::new();
    for pair in args.env_vars {
        let mut parts = pair.splitn(2, '=');
        let key = parts.next().unwrap_or("").trim();
        let value = parts.next().unwrap_or("").to_string();
        if key.is_empty() {
            return Err(CliError::Arg("--var expects KEY=VALUE format".to_string()));
        }
        env_vars.insert(key.to_string(), value);
    }

    if args.resume_latest && args.session_id.is_some() {
        return Err(CliError::Arg(
            "--resume-latest cannot be combined with --session".to_string(),
        ));
    }

    let local_provider = if legacy_custom_command {
        None
    } else {
        local_provider_from_executor_arg(executor_arg)
    };
    let mut session_id = args.session_id;
    if args.resume_latest {
        session_id = Some(session_store::latest_session_id().ok_or_else(|| {
            CliError::Arg("no saved sessions found for --resume-latest".to_string())
        })?);
    }

    let mut executor_paths = load_executor_path_cache().entries;
    let mut executor_paths_updated = false;
    for raw in args.executor_paths {
        let (name, path) = parse_executor_path_arg(&raw)?;
        executor_paths.insert(name, path);
        executor_paths_updated = true;
    }
    if executor_paths_updated {
        save_executor_path_cache(&ExecutorPathCache {
            entries: executor_paths.clone(),
        });
    }

    let resume = args.resume_latest || session_id.is_some();

    let mut openai_endpoint = args.openai_endpoint;
    if let Some(provider) = local_provider {
        openai_endpoint = match provider {
            LocalProvider::Ollama => openai_endpoint,
            LocalProvider::LmStudio => {
                openai_endpoint.or(Some("http://127.0.0.1:1234".to_string()))
            }
            LocalProvider::Custom => openai_endpoint.or_else(|| prompt_for_custom_endpoint().ok()),
        };

        if matches!(provider, LocalProvider::Custom) && openai_endpoint.is_none() {
            return Err(CliError::Arg(
                "custom executor requires --openai-endpoint or interactive input".to_string(),
            ));
        }
    }

    Ok(RunOptions {
        executor: if legacy_custom_command {
            ExecutorKind::Custom("custom".to_string())
        } else {
            executor_kind_from_arg(executor_arg)
        },
        prompt: args.prompt,
        cwd: args
            .cwd
            .unwrap_or_else(|| std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."))),
        session_id,
        resume,
        refresh_model_cache: args.refresh_model_cache,
        review: args.review,
        env_vars,
        base_command_override: args.base_command_override,
        additional_params: args.additional_params,
        append_prompt: args.append_prompt,
        model: args.model,
        executor_mode: normalize_executor_mode(args.mode.as_deref()),
        openai_endpoint,
        local_provider,
        yolo: args.yolo,
        force: args.force,
        trust: args.trust,
        auto_approve: args.auto_approve.unwrap_or(true),
        allow_all_tools: args.allow_all_tools,
        executor_paths,
        tui: if force_tui { true } else { !args.no_tui },
    })
}

pub fn usage() -> String {
    let mut cmd = AuraCli::command();
    let mut output = Vec::new();
    let _ = cmd.write_long_help(&mut output);
    String::from_utf8_lossy(&output).to_string()
}

pub fn completion_script(shell: Shell) -> String {
    let mut cmd = AuraCli::command();
    let mut output = Vec::new();
    generate(shell, &mut cmd, "aura", &mut output);
    String::from_utf8_lossy(&output).to_string()
}

pub fn parse_cli_args(args: &[String]) -> Result<CliCommand, CliError> {
    let argv = std::iter::once("aura".to_string())
        .chain(args.iter().cloned())
        .collect::<Vec<_>>();
    let parsed = AuraCli::try_parse_from(argv).map_err(|error| CliError::Arg(error.to_string()))?;

    match parsed.command.unwrap_or(AuraSubcommand::Help) {
        AuraSubcommand::Run(run_args) => Ok(CliCommand::Run(Box::new(convert_run_cli_args(
            run_args, false,
        )?))),
        AuraSubcommand::Tui(run_args) => Ok(CliCommand::Run(Box::new(convert_run_cli_args(
            run_args, true,
        )?))),
        AuraSubcommand::Session(session) => match session.command {
            SessionArgs::List(args) => Ok(CliCommand::SessionList(args)),
            SessionArgs::Show(args) => Ok(CliCommand::SessionShow {
                session_id: args.session_id,
            }),
            SessionArgs::Latest => Ok(CliCommand::SessionLatest),
        },
        AuraSubcommand::Help => Ok(CliCommand::Help),
        AuraSubcommand::Completion(args) => Ok(CliCommand::Completion { shell: args.shell }),
        AuraSubcommand::LocalExec => Ok(CliCommand::LocalExec),
    }
}

fn model_options_for_executor(
    executor: &ExecutorKind,
    refresh_local_cache: bool,
    local_provider: Option<LocalProvider>,
) -> Vec<String> {
    local_exec::model_options_for_executor(executor, refresh_local_cache, local_provider)
}

fn discover_local_model_options(
    refresh_cache: bool,
    local_provider: Option<LocalProvider>,
) -> Vec<String> {
    local_exec::discover_local_model_options(refresh_cache, local_provider)
}

#[cfg(test)]
fn parse_ollama_models(raw: &str) -> Vec<String> {
    local_exec::parse_ollama_models(raw)
}

#[cfg(test)]
fn parse_lmstudio_models(raw: &str) -> Vec<String> {
    local_exec::parse_lmstudio_models(raw)
}

fn resolve_local_model_and_endpoint(
    model: Option<&str>,
    openai_endpoint: Option<&str>,
) -> (Option<String>, Option<String>) {
    local_exec::resolve_local_model_and_endpoint(model, openai_endpoint)
}

pub async fn run_local_exec_from_env() -> i32 {
    local_exec::run_local_exec_from_env().await
}

#[cfg(test)]
fn openai_chat_completions_url(base_url: &str) -> String {
    local_exec::openai_chat_completions_url(base_url)
}

#[cfg(test)]
fn extract_openai_delta_content(value: &Value) -> Option<String> {
    local_exec::extract_openai_delta_content(value)
}

#[cfg(test)]
fn consume_openai_stream_line(
    raw_line: &str,
    full_content: &mut String,
    pending_line: &mut String,
) {
    local_exec::consume_openai_stream_line(raw_line, full_content, pending_line)
}

fn cache_dir() -> PathBuf {
    let home = std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."));
    home.join(".aura").join("cache")
}

fn model_cache_path() -> PathBuf {
    cache_dir().join("models.json")
}

fn executor_path_cache_path() -> PathBuf {
    cache_dir().join("executors.json")
}

fn now_epoch_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_secs())
        .unwrap_or(0)
}

fn load_model_cache(expected_sources: &[&str]) -> Option<Vec<String>> {
    let path = model_cache_path();
    let Ok(raw) = fs::read_to_string(path) else {
        return None;
    };
    let Ok(cache) = serde_json::from_str::<ModelCache>(&raw) else {
        return None;
    };
    if cache.version != 1 {
        return None;
    }
    let expected: HashSet<String> = expected_sources
        .iter()
        .map(|value| value.to_string())
        .collect();
    let actual: HashSet<String> = cache.sources.iter().cloned().collect();
    if expected != actual {
        return None;
    }
    let age = now_epoch_secs().saturating_sub(cache.updated_at);
    if age > 86_400 {
        return None;
    }
    if cache.models.is_empty() {
        return None;
    }
    Some(cache.models)
}

fn write_model_cache(models: &[String], sources: &[&str]) {
    let path = model_cache_path();
    if let Some(parent) = path.parent() {
        let _ = fs::create_dir_all(parent);
    }
    let cache = ModelCache {
        version: 1,
        updated_at: now_epoch_secs(),
        models: models.to_vec(),
        sources: sources.iter().map(|value| value.to_string()).collect(),
    };
    if let Ok(serialized) = serde_json::to_string(&cache) {
        let _ = fs::write(path, serialized);
    }
}

fn load_executor_path_cache() -> ExecutorPathCache {
    let path = executor_path_cache_path();
    let Ok(raw) = fs::read_to_string(path) else {
        return ExecutorPathCache::default();
    };
    serde_json::from_str::<ExecutorPathCache>(&raw).unwrap_or_default()
}

fn save_executor_path_cache(cache: &ExecutorPathCache) {
    let path = executor_path_cache_path();
    if let Some(parent) = path.parent() {
        let _ = fs::create_dir_all(parent);
    }
    if let Ok(serialized) = serde_json::to_string(cache) {
        let _ = fs::write(path, serialized);
    }
}

pub fn list_sessions(args: SessionListArgs) -> String {
    session_store::list_sessions(args)
}

pub async fn run_session_list(args: SessionListArgs) -> Result<Option<RunOutcome>, CliError> {
    session_store::run_session_list(args).await
}

pub fn latest_session() -> Result<String, CliError> {
    session_store::latest_session()
}

pub fn show_session(session_id: &str) -> Result<String, CliError> {
    session_store::show_session(session_id)
}

pub async fn run_with_default_sink(options: RunOptions) -> Result<RunOutcome, CliError> {
    let mut options = options;
    ensure_local_model_selected(&mut options)?;
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
    let mut options = options;
    ensure_local_model_selected(&mut options)?;
    run_executor_inner(options, sink, false).await
}

enum TuiPromptAction {
    Submit {
        prompt: String,
        model: Option<String>,
        mode: Option<String>,
    },
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
        if let Some(submission) = sink.take_queued_prompt() {
            options.prompt = submission.prompt;
            options.model = submission.model;
            options.executor_mode = submission.mode;
        }
        sink.on_user_prompt(&options.prompt);
        let mut outcome = run_executor_inner(options.clone(), sink, true).await?;

        if outcome.user_requested_exit {
            return Ok(outcome);
        }

        if let Some(submission) = sink.take_queued_prompt() {
            options.prompt = submission.prompt;
            options.model = submission.model;
            options.executor_mode = submission.mode;
            continue;
        }

        sink.set_active_pane(TuiPane::Prompt);
        sink.on_status("awaiting prompt (Tab=switch, Enter=send, /mode plan, Esc/Ctrl+C=quit)");
        match read_tui_prompt_action(sink)? {
            TuiPromptAction::Submit {
                prompt,
                model,
                mode,
            } => {
                options.prompt = prompt;
                options.model = model;
                options.executor_mode = mode;
            }
            TuiPromptAction::Quit => {
                outcome.user_requested_exit = true;
                return Ok(outcome);
            }
        }
    }
}

fn ensure_local_model_selected(options: &mut RunOptions) -> Result<(), CliError> {
    if !matches!(options.executor, ExecutorKind::Local) {
        return Ok(());
    }
    if options
        .model
        .as_deref()
        .map(str::trim)
        .is_some_and(|value| !value.is_empty())
    {
        return Ok(());
    }

    let models = discover_local_model_options(options.refresh_model_cache, options.local_provider);
    if models.is_empty() {
        if matches!(options.local_provider, Some(LocalProvider::Custom)) {
            return Err(CliError::Arg(
                "custom executor: model is required; pass --model explicitly".to_string(),
            ));
        }
        return Err(CliError::Arg(
            "no local models discovered from Ollama or LM Studio; pass --model explicitly"
                .to_string(),
        ));
    }
    options.model = Some(prompt_for_local_model_selection(&models)?);
    Ok(())
}

fn prompt_for_local_model_selection(models: &[String]) -> Result<String, CliError> {
    let default = models
        .first()
        .cloned()
        .ok_or_else(|| CliError::Arg("no local models available".to_string()))?;

    if !io::stdin().is_terminal() || !io::stdout().is_terminal() {
        return Ok(default);
    }

    let mut selector = cliclack::select("Select local model");
    for (index, model) in models.iter().enumerate() {
        let hint = if index == 0 { "default" } else { "" };
        selector = selector.item(model.clone(), model.clone(), hint);
    }

    let selected = selector
        .interact()
        .map_err(|error| CliError::Runtime(format!("model selection failed: {error}")))?;
    Ok(selected)
}

#[cfg(test)]
fn parse_model_selection_input(input: &str, option_count: usize) -> Option<usize> {
    if option_count == 0 {
        return None;
    }
    let trimmed = input.trim();
    if trimmed.is_empty() {
        return Some(0);
    }
    let selected = trimmed.parse::<usize>().ok()?;
    if (1..=option_count).contains(&selected) {
        Some(selected - 1)
    } else {
        None
    }
}

async fn run_executor_inner(
    options: RunOptions,
    sink: &mut dyn LogSink,
    enable_keyboard_controls: bool,
) -> Result<RunOutcome, CliError> {
    let mut options = options;
    if matches!(options.executor, ExecutorKind::Local)
        && matches!(options.local_provider, Some(LocalProvider::Ollama))
        && options
            .model
            .as_deref()
            .map(str::trim)
            .unwrap_or("")
            .is_empty()
    {
        return Err(CliError::Arg(
            "ollama executor: model is required; pass --model".to_string(),
        ));
    }
    if options.session_id.is_none() {
        if let Some(env_session) = options.env_vars.get("AURA_SESSION_ID") {
            options.session_id = Some(env_session.clone());
        } else {
            options.session_id = Some(SessionId::new().0.to_string());
        }
    }
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
    if let Some(session_id) = &options.session_id {
        env.insert("AURA_SESSION_ID", session_id.clone());
    }

    let session_context = options.session_id.clone().map(|session_id| SessionContext {
        session_id,
        executor: options.executor.clone(),
        cwd: options.cwd.clone(),
    });
    if let Some(context) = &session_context {
        session_store::write_session_metadata(context, None);
        sink.on_status(&format!(
            "session: {} (resume with --session {})",
            context.session_id, context.session_id
        ));
        if options.resume {
            sink.on_status(&format!("resuming session {}", context.session_id));
        }
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
    } else if options.resume {
        executor
            .spawn_follow_up(
                &options.cwd,
                &options.prompt,
                options
                    .session_id
                    .as_deref()
                    .ok_or_else(|| CliError::Runtime("missing session id".to_string()))?,
                &env,
            )
            .await?
    } else {
        executor
            .spawn_initial(&options.cwd, &options.prompt, &env)
            .await?
    };

    stream_spawned(
        options.executor.clone(),
        session_context,
        options.local_provider,
        spawned,
        sink,
        enable_keyboard_controls,
    )
    .await
}

fn build_executor(options: &RunOptions) -> Box<dyn StandardCodingAgentExecutor> {
    let normalized_mode = normalize_executor_mode(options.executor_mode.as_deref());
    let executor_override = executor_path_override(&options.executor, &options.executor_paths);
    let cmd_overrides = CmdOverrides {
        base_command_override: options.base_command_override.clone().or(executor_override),
        additional_params: if options.additional_params.is_empty() {
            None
        } else {
            Some(options.additional_params.clone())
        },
        env: None,
    };

    let append_prompt = AppendPrompt(options.append_prompt.clone());

    match options.executor {
        ExecutorKind::Codex => {
            let sandbox = codex_mode_settings(normalized_mode.as_deref());
            Box::new(adapters::codex(CodexOptions {
                append_prompt,
                model: options.model.clone(),
                sandbox,
                cmd_overrides,
            }))
        }
        ExecutorKind::Claude => {
            let (plan, approvals, dangerously_skip_permissions) =
                claude_mode_settings(normalized_mode.as_deref());
            Box::new(adapters::claude(ClaudeOptions {
                append_prompt,
                model: options.model.clone(),
                plan,
                approvals,
                dangerously_skip_permissions,
                claude_code_router: false,
                cmd_overrides,
            }))
        }
        ExecutorKind::Opencode => Box::new(adapters::opencode(OpencodeOptions {
            append_prompt,
            model: options.model.clone(),
            variant: None,
            mode: opencode_agent_mode(normalized_mode.as_deref()),
            auto_approve: options.auto_approve,
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
        ExecutorKind::Local => {
            if matches!(options.local_provider, Some(LocalProvider::Ollama)) {
                let (model, _) = resolve_local_model_and_endpoint(options.model.as_deref(), None);
                let model = model.unwrap_or_else(|| "llama3".to_string());
                return Box::new(adapters::ollama_cli(OllamaOptions {
                    append_prompt,
                    model,
                    cmd_overrides,
                }));
            }
            let mut local_cmd_overrides = cmd_overrides;
            if local_cmd_overrides.base_command_override.is_none() {
                local_cmd_overrides.base_command_override = std::env::current_exe()
                    .ok()
                    .map(|path| path.display().to_string());
            }
            let (model, openai_endpoint) = resolve_local_model_and_endpoint(
                options.model.as_deref(),
                options.openai_endpoint.as_deref(),
            );
            Box::new(adapters::local(LocalOptions {
                append_prompt,
                model,
                openai_endpoint,
                cmd_overrides: local_cmd_overrides,
            }))
        }
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

fn codex_mode_settings(mode: Option<&str>) -> Option<String> {
    match mode {
        Some("plan") => Some("read-only".to_string()),
        Some("yolo") => Some("danger-full-access".to_string()),
        _ => Some("workspace-write".to_string()),
    }
}

fn claude_mode_settings(mode: Option<&str>) -> (bool, bool, bool) {
    match mode {
        Some("plan") => (true, false, false),
        Some("approvals") | Some("approval") => (false, true, false),
        Some("yolo") => (false, false, true),
        _ => (false, false, false),
    }
}

fn opencode_agent_mode(mode: Option<&str>) -> Option<String> {
    match mode {
        Some("execute") | Some("default") | None => None,
        Some(other) => Some(other.to_string()),
    }
}

fn summarize_text(text: &str, max_len: usize) -> String {
    runtime::summarize_text(text, max_len)
}

pub(crate) async fn stream_spawned(
    executor_kind: ExecutorKind,
    session_context: Option<SessionContext>,
    local_provider: Option<LocalProvider>,
    spawned: SpawnedChild,
    sink: &mut dyn LogSink,
    enable_keyboard_controls: bool,
) -> Result<RunOutcome, CliError> {
    runtime::stream_spawned(
        executor_kind,
        session_context,
        local_provider,
        spawned,
        sink,
        enable_keyboard_controls,
    )
    .await
}

#[cfg(test)]
fn is_quit_key(key: KeyEvent) -> bool {
    runtime::is_quit_key(key)
}

fn apply_prompt_input_key(input: &mut String, key: KeyEvent) -> PromptInputUpdate {
    runtime::apply_prompt_input_key(input, key)
}

fn is_prompt_quit_key(key: KeyEvent) -> bool {
    runtime::is_prompt_quit_key(key)
}

fn read_tui_prompt_action(sink: &mut TuiLogSink) -> io::Result<TuiPromptAction> {
    runtime::read_tui_prompt_action(sink)
}

fn handle_logs_navigation_key(sink: &mut TuiLogSink, key: KeyEvent) -> bool {
    runtime::handle_logs_navigation_key(sink, key)
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
    fn parses_run_command_with_mode() {
        let args = vec![
            "run".to_string(),
            "--executor".to_string(),
            "claude".to_string(),
            "--prompt".to_string(),
            "hello".to_string(),
            "--mode".to_string(),
            "plan".to_string(),
        ];

        let parsed = parse_cli_args(&args).expect("parse");
        match parsed {
            CliCommand::Run(opts) => {
                assert!(matches!(opts.executor, ExecutorKind::Claude));
                assert_eq!(opts.executor_mode.as_deref(), Some("plan"));
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
    fn parses_ollama_executor() {
        let args = vec![
            "run".to_string(),
            "--executor".to_string(),
            "ollama".to_string(),
            "--prompt".to_string(),
            "hi".to_string(),
        ];

        let parsed = parse_cli_args(&args).expect("parse");
        match parsed {
            CliCommand::Run(opts) => {
                assert!(matches!(opts.executor, ExecutorKind::Local));
                assert!(matches!(opts.local_provider, Some(LocalProvider::Ollama)));
                assert_eq!(opts.prompt, "hi");
            }
            _ => panic!("expected run"),
        }
    }

    #[test]
    fn parses_custom_openai_endpoint() {
        let args = vec![
            "run".to_string(),
            "--executor".to_string(),
            "custom".to_string(),
            "--prompt".to_string(),
            "hi".to_string(),
            "--openai-endpoint".to_string(),
            "http://127.0.0.1:1234".to_string(),
        ];

        let parsed = parse_cli_args(&args).expect("parse");
        match parsed {
            CliCommand::Run(opts) => {
                assert_eq!(
                    opts.openai_endpoint.as_deref(),
                    Some("http://127.0.0.1:1234")
                );
                assert!(matches!(opts.local_provider, Some(LocalProvider::Custom)));
            }
            _ => panic!("expected run"),
        }
    }

    #[test]
    fn custom_with_base_command_uses_legacy_custom_executor() {
        let args = vec![
            "run".to_string(),
            "--executor".to_string(),
            "custom".to_string(),
            "--prompt".to_string(),
            "hi".to_string(),
            "--base-command".to_string(),
            "sh".to_string(),
        ];

        let parsed = parse_cli_args(&args).expect("parse");
        match parsed {
            CliCommand::Run(opts) => {
                assert!(matches!(opts.executor, ExecutorKind::Custom(_)));
                assert!(opts.local_provider.is_none());
            }
            _ => panic!("expected run"),
        }
    }

    #[test]
    fn local_model_resolution_strips_prefix_and_infers_endpoint() {
        let (model, endpoint) = resolve_local_model_and_endpoint(Some("lmstudio/qwen2.5"), None);
        assert_eq!(model.as_deref(), Some("qwen2.5"));
        assert_eq!(endpoint.as_deref(), Some("http://127.0.0.1:1234"));

        let (model, endpoint) = resolve_local_model_and_endpoint(Some("ollama/llama3.2:3b"), None);
        assert_eq!(model.as_deref(), Some("llama3.2:3b"));
        assert_eq!(endpoint.as_deref(), Some("http://127.0.0.1:11434/v1"));

        let (model, endpoint) = resolve_local_model_and_endpoint(
            Some("lmstudio/qwen2.5"),
            Some("http://localhost:9999"),
        );
        assert_eq!(model.as_deref(), Some("qwen2.5"));
        assert_eq!(endpoint.as_deref(), Some("http://localhost:9999"));
    }

    #[test]
    fn parses_local_model_selection_input() {
        assert_eq!(parse_model_selection_input("", 3), Some(0));
        assert_eq!(parse_model_selection_input("2", 3), Some(1));
        assert_eq!(parse_model_selection_input("0", 3), None);
        assert_eq!(parse_model_selection_input("4", 3), None);
        assert_eq!(parse_model_selection_input("abc", 3), None);
    }

    #[test]
    fn normalize_thinking_text_strips_basic_markdown_emphasis() {
        assert_eq!(
            normalize_tui_line_text(TuiLineKind::Thinking, "thinking: **Preparing plan**"),
            "Preparing plan"
        );
        assert_eq!(
            normalize_tui_line_text(TuiLineKind::Thinking, "thinking: __Analyzing__"),
            "Analyzing"
        );
    }

    #[test]
    fn sanitize_tui_text_strips_ansi_sequences() {
        let input = "\u{1b}[47mwhite-bg\u{1b}[0m plain \u{1b}]0;title\u{7}\u{1b}[2K";
        assert_eq!(sanitize_tui_text(input), "white-bg plain ");
    }

    #[test]
    fn sanitize_tui_text_replaces_tabs_and_drops_controls() {
        let input = "one\two\u{0}\u{1}three";
        assert_eq!(sanitize_tui_text(input), "one    wothree");
    }

    #[test]
    fn builds_openai_chat_completion_url() {
        assert_eq!(
            openai_chat_completions_url("http://127.0.0.1:1234"),
            "http://127.0.0.1:1234/v1/chat/completions"
        );
        assert_eq!(
            openai_chat_completions_url("http://127.0.0.1:11434/v1"),
            "http://127.0.0.1:11434/v1/chat/completions"
        );
    }

    #[test]
    fn extracts_openai_delta_content_from_stream_payload() {
        let payload: Value = serde_json::from_str(r#"{"choices":[{"delta":{"content":"hello"}}]}"#)
            .expect("valid json");
        assert_eq!(
            extract_openai_delta_content(&payload).as_deref(),
            Some("hello")
        );
    }

    #[test]
    fn extracts_openai_delta_content_from_text_parts() {
        let payload: Value = serde_json::from_str(
            r#"{"choices":[{"delta":{"content":[{"type":"output_text","text":"hello"},{"type":"output_text","text":" world"}]}}]}"#,
        )
        .expect("valid json");
        assert_eq!(
            extract_openai_delta_content(&payload).as_deref(),
            Some("hello\n world")
        );
    }

    #[test]
    fn consumes_sse_data_lines_for_local_stream() {
        let mut full_content = String::new();
        let mut pending_line = String::new();

        consume_openai_stream_line(
            r#"data: {"choices":[{"delta":{"content":"hello "}}]}"#,
            &mut full_content,
            &mut pending_line,
        );
        consume_openai_stream_line(
            r#"data: {"choices":[{"delta":{"content":"world"}}]}"#,
            &mut full_content,
            &mut pending_line,
        );
        consume_openai_stream_line("data: [DONE]", &mut full_content, &mut pending_line);

        assert_eq!(full_content, "hello world");
        assert_eq!(pending_line, "hello world");
    }

    #[test]
    fn consumes_plain_json_lines_for_local_stream_fallback() {
        let mut full_content = String::new();
        let mut pending_line = String::new();

        consume_openai_stream_line(
            r#"{"choices":[{"message":{"content":"complete"}}]}"#,
            &mut full_content,
            &mut pending_line,
        );

        assert_eq!(full_content, "complete");
        assert_eq!(pending_line, "complete");
    }

    #[test]
    fn parses_ollama_models_from_table_output() {
        let input = r#"
NAME                  ID              SIZE      MODIFIED
llama3.2:3b           abcdef          2.0 GB    2 hours ago
qwen2.5-coder:7b      fedcba          4.1 GB    1 day ago
"#;

        let models = parse_ollama_models(input);
        assert_eq!(models, vec!["llama3.2:3b", "qwen2.5-coder:7b"]);
    }

    #[test]
    fn parses_lmstudio_models_from_openai_models_payload() {
        let input = r#"{
  "object": "list",
  "data": [
    {"id": "qwen2.5-coder-7b-instruct"},
    {"id": "llama-3.1-8b-instruct"}
  ]
}"#;

        let models = parse_lmstudio_models(input);
        assert_eq!(
            models,
            vec!["qwen2.5-coder-7b-instruct", "llama-3.1-8b-instruct"]
        );
    }

    #[test]
    fn quit_key_mapping() {
        assert!(!is_quit_key(KeyEvent::new(
            KeyCode::Char('q'),
            KeyModifiers::NONE
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
            apply_prompt_input_key(
                &mut input,
                KeyEvent::new(KeyCode::Char('h'), KeyModifiers::NONE)
            ),
            PromptInputUpdate::Continue
        ));
        assert!(matches!(
            apply_prompt_input_key(
                &mut input,
                KeyEvent::new(KeyCode::Char('i'), KeyModifiers::NONE)
            ),
            PromptInputUpdate::Continue
        ));
        assert_eq!(input, "hi");

        assert!(matches!(
            apply_prompt_input_key(
                &mut input,
                KeyEvent::new(KeyCode::Char('q'), KeyModifiers::NONE)
            ),
            PromptInputUpdate::Continue
        ));
        assert_eq!(input, "hiq");

        assert!(matches!(
            apply_prompt_input_key(
                &mut input,
                KeyEvent::new(KeyCode::Backspace, KeyModifiers::NONE)
            ),
            PromptInputUpdate::Continue
        ));
        assert_eq!(input, "hi");

        let submit = apply_prompt_input_key(
            &mut input,
            KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE),
        );
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
        let mut formatter = LogFormatter::new(ExecutorKind::Codex, None);
        let events = formatter.format(
            LogStream::Stdout,
            r#"{"type":"item.completed","item":{"type":"agent_message","text":"hello from agent"}}"#,
        );

        assert!(events.iter().any(|event| matches!(
            event,
            DisplayEvent::AgentMarkdown(text) if text == "hello from agent"
        )));
        assert!(events.iter().any(|event| matches!(
            event,
            DisplayEvent::AgentStatus(status) if status == "Responding"
        )));
    }

    #[test]
    fn codex_agent_message_formats_json_and_code_blocks() {
        let mut formatter = LogFormatter::new(ExecutorKind::Codex, None);
        let events = formatter.format(
            LogStream::Stdout,
            r#"{"type":"item.completed","item":{"type":"agent_message","text":"Here is data:\n```json\n{\"ok\":true,\"count\":2}\n```\nAnd code:\n```rust\nfn main() {}\n```"}}"#,
        );

        assert!(events.iter().any(|event| matches!(
            event,
            DisplayEvent::AgentMarkdown(text) if text.contains("Here is data:")
        )));
    }

    #[test]
    fn mode_mappings_apply_expected_executor_settings() {
        assert_eq!(
            codex_mode_settings(Some("plan")),
            Some("read-only".to_string())
        );
        assert_eq!(
            codex_mode_settings(None),
            Some("workspace-write".to_string())
        );
        assert_eq!(
            codex_mode_settings(Some("yolo")),
            Some("danger-full-access".to_string())
        );
        assert_eq!(claude_mode_settings(Some("plan")), (true, false, false));
        assert_eq!(opencode_agent_mode(Some("execute")), None);
        assert_eq!(opencode_agent_mode(Some("plan")), Some("plan".to_string()));
    }

    #[test]
    fn codex_agent_message_formats_diff_blocks() {
        let mut formatter = LogFormatter::new(ExecutorKind::Codex, None);
        let events = formatter.format(
            LogStream::Stdout,
            r#"{"type":"item.completed","item":{"type":"agent_message","text":"```diff\n@@ -1 +1 @@\n-old\n+new\n```"}}"#,
        );

        assert!(events.iter().any(|event| matches!(
            event,
            DisplayEvent::AgentMarkdown(text) if text.contains("@@ -1 +1 @@")
        )));
    }

    #[test]
    fn codex_rollout_warning_is_suppressed_after_notice() {
        let mut formatter = LogFormatter::new(ExecutorKind::Codex, None);
        let warning = "2026-01-01 ERROR codex_core::rollout::list: state db missing rollout path for thread abc";

        let first = formatter.format(LogStream::Stderr, warning);
        let second = formatter.format(LogStream::Stderr, warning);

        assert_eq!(formatter.suppressed_codex_rollout_warnings, 2);
        assert_eq!(first.len(), 1);
        assert!(matches!(first[0], DisplayEvent::Status(_)));
        assert!(second.is_empty());
    }

    #[test]
    fn codex_model_refresh_timeout_sets_model_unavailable_without_log_line() {
        let mut formatter = LogFormatter::new(ExecutorKind::Codex, None);
        let warning = "2026-02-23T10:45:31.760202Z ERROR codex_core::models_manager::manager: failed to refresh available models: timeout waiting for child process to exit";

        let events = formatter.format(LogStream::Stderr, warning);

        assert_eq!(events.len(), 1);
        assert!(matches!(events[0], DisplayEvent::ModelUnavailable));
    }

    #[test]
    fn codex_reasoning_can_set_planning_status() {
        let mut formatter = LogFormatter::new(ExecutorKind::Codex, None);
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
            resume: false,
            refresh_model_cache: false,
            review: false,
            env_vars: HashMap::new(),
            base_command_override: Some("sh".to_string()),
            additional_params: vec![
                "-c".to_string(),
                "echo out_line; echo err_line 1>&2".to_string(),
            ],
            append_prompt: None,
            model: None,
            executor_mode: None,
            openai_endpoint: None,
            local_provider: None,
            yolo: false,
            force: false,
            trust: false,
            auto_approve: true,
            allow_all_tools: false,
            executor_paths: HashMap::new(),
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
            resume: true,
            refresh_model_cache: false,
            review: false,
            env_vars: HashMap::new(),
            base_command_override: Some("sh".to_string()),
            additional_params: vec![
                "-c".to_string(),
                "printf %s \"$AURA_SESSION_ID\"".to_string(),
            ],
            append_prompt: None,
            model: None,
            executor_mode: None,
            openai_endpoint: None,
            local_provider: None,
            yolo: false,
            force: false,
            trust: false,
            auto_approve: true,
            allow_all_tools: false,
            executor_paths: HashMap::new(),
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
            resume: false,
            refresh_model_cache: false,
            review: false,
            env_vars: HashMap::new(),
            base_command_override: Some("echo".to_string()),
            additional_params: Vec::new(),
            append_prompt: None,
            model: None,
            executor_mode: None,
            openai_endpoint: None,
            local_provider: None,
            yolo: true,
            force: false,
            trust: false,
            auto_approve: true,
            allow_all_tools: false,
            executor_paths: HashMap::new(),
            tui: false,
        };

        let outcome = run_executor(options, &mut sink).await.expect("run");
        assert!(outcome.success);
        assert!(sink.stdout.iter().any(|line| line.contains("--force")));
        assert!(sink.stdout.iter().any(|line| line.contains("--trust")));
    }
}
