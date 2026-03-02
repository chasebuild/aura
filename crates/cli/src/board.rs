use std::collections::{HashMap, HashSet};
use std::io;
use std::process::Command;
use std::thread;
use std::time::{Duration, Instant};

use crossterm::{
    cursor::Show,
    event::{self, Event, KeyCode, KeyEvent, KeyEventKind},
    execute,
    terminal::{EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode},
};
use ratatui::{
    Terminal,
    backend::CrosstermBackend,
    layout::{Constraint, Direction, Layout},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Cell, Paragraph, Row, Table, TableState},
};

use super::session_store::{self, RuntimeRunState, RuntimeSessionState};
use super::{BoardArgs, CliError, ExecutorKind, now_epoch_secs};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RowSource {
    AuraManaged,
    ExternalDetected,
}

#[derive(Debug, Clone)]
struct BoardRow {
    source: RowSource,
    executor: ExecutorKind,
    session_id: Option<String>,
    pid: Option<u32>,
    cwd: Option<String>,
    state: RuntimeRunState,
    started_at: u64,
    updated_at: u64,
    finished_at: Option<u64>,
    status: Option<String>,
    command: Option<String>,
}

#[derive(Debug, Clone)]
struct ExternalProcess {
    pid: u32,
    executor: ExecutorKind,
    elapsed_secs: u64,
    command: String,
}

pub(crate) fn run_board(args: BoardArgs) -> Result<(), CliError> {
    let mut app = BoardApp::new(args);
    app.run()
}

struct BoardApp {
    poll_interval: Duration,
    include_external: bool,
    show_finished: bool,
    rows: Vec<BoardRow>,
    selected: usize,
    status_line: String,
    confirm_stop: bool,
    last_refresh: Instant,
    terminal: Option<Terminal<CrosstermBackend<io::Stdout>>>,
}

impl BoardApp {
    fn new(args: BoardArgs) -> Self {
        Self {
            poll_interval: Duration::from_millis(args.poll_ms.max(250)),
            include_external: !args.no_external,
            show_finished: args.show_finished,
            rows: Vec::new(),
            selected: 0,
            status_line: "Loading sessions...".to_string(),
            confirm_stop: false,
            last_refresh: Instant::now()
                .checked_sub(Duration::from_secs(60))
                .unwrap_or_else(Instant::now),
            terminal: None,
        }
    }

    fn run(&mut self) -> Result<(), CliError> {
        self.init_terminal()?;
        self.refresh_rows();

        loop {
            self.draw()?;

            if event::poll(Duration::from_millis(200))?
                && let Event::Key(key) = event::read()?
                && self.handle_key(key)?
            {
                break;
            }

            if self.last_refresh.elapsed() >= self.poll_interval {
                self.refresh_rows();
            }
        }

        Ok(())
    }

    fn init_terminal(&mut self) -> Result<(), CliError> {
        enable_raw_mode()?;
        execute!(io::stdout(), EnterAlternateScreen)?;
        let backend = CrosstermBackend::new(io::stdout());
        let terminal = Terminal::new(backend)?;
        self.terminal = Some(terminal);
        Ok(())
    }

    fn draw(&mut self) -> Result<(), CliError> {
        let Some(terminal) = self.terminal.as_mut() else {
            return Ok(());
        };

        let rows = self.rows.clone();
        let selected = self.selected.min(rows.len().saturating_sub(1));
        let status = self.status_line.clone();
        let include_external = self.include_external;
        let show_finished = self.show_finished;

        terminal.draw(|frame| {
            let chunks = Layout::default()
                .direction(Direction::Vertical)
                .constraints([
                    Constraint::Length(3),
                    Constraint::Min(8),
                    Constraint::Length(6),
                    Constraint::Length(2),
                ])
                .split(frame.area());

            let running = rows
                .iter()
                .filter(|row| row.state == RuntimeRunState::Running)
                .count();
            let finished = rows
                .iter()
                .filter(|row| row.state == RuntimeRunState::Finished)
                .count();
            let external = rows
                .iter()
                .filter(|row| row.source == RowSource::ExternalDetected)
                .count();
            let summary = Line::from(vec![
                Span::styled("AURA Mission Control", Style::default().fg(Color::Cyan)),
                Span::raw("  "),
                Span::styled(
                    format!("running:{running}"),
                    Style::default().fg(Color::Green),
                ),
                Span::raw("  "),
                Span::styled(
                    format!("finished:{finished}"),
                    Style::default().fg(Color::Yellow),
                ),
                Span::raw("  "),
                Span::styled(
                    format!("external:{external}"),
                    Style::default().fg(Color::Magenta),
                ),
                Span::raw("  "),
                Span::raw(format!(
                    "poll:{}ms ext:{} show_finished:{}",
                    self.poll_interval.as_millis(),
                    if include_external { "on" } else { "off" },
                    if show_finished { "on" } else { "off" }
                )),
            ]);
            let header = Paragraph::new(summary)
                .block(Block::default().borders(Borders::ALL).title("Overview"));
            frame.render_widget(header, chunks[0]);

            let table_rows: Vec<Row<'_>> = rows
                .iter()
                .map(|row| {
                    let state_style = match row.state {
                        RuntimeRunState::Running => Style::default().fg(Color::Green),
                        RuntimeRunState::Finished => Style::default().fg(Color::Yellow),
                    };
                    Row::new(vec![
                        Cell::from(executor_label(&row.executor)),
                        Cell::from(match row.source {
                            RowSource::AuraManaged => "aura",
                            RowSource::ExternalDetected => "external",
                        }),
                        Cell::from(match row.state {
                            RuntimeRunState::Running => "running",
                            RuntimeRunState::Finished => "finished",
                        })
                        .style(state_style),
                        Cell::from(
                            row.session_id
                                .as_deref()
                                .map(short_session)
                                .unwrap_or("-".to_string()),
                        ),
                        Cell::from(
                            row.pid
                                .map(|value| value.to_string())
                                .unwrap_or_else(|| "-".to_string()),
                        ),
                        Cell::from(
                            row.cwd
                                .as_deref()
                                .map(short_cwd)
                                .unwrap_or_else(|| "-".to_string()),
                        ),
                        Cell::from(relative_age(row.updated_at)),
                    ])
                })
                .collect();

            let table = Table::new(
                table_rows,
                [
                    Constraint::Length(10),
                    Constraint::Length(10),
                    Constraint::Length(10),
                    Constraint::Length(18),
                    Constraint::Length(8),
                    Constraint::Percentage(55),
                    Constraint::Length(9),
                ],
            )
            .header(
                Row::new(vec![
                    "executor", "source", "state", "session", "pid", "cwd", "updated",
                ])
                .style(Style::default().add_modifier(Modifier::BOLD)),
            )
            .block(Block::default().borders(Borders::ALL).title("Sessions"))
            .row_highlight_style(Style::default().bg(Color::DarkGray));
            let mut table_state = TableState::default().with_selected(Some(selected));
            frame.render_stateful_widget(table, chunks[1], &mut table_state);

            let detail = rows.get(selected);
            let mut detail_lines = Vec::new();
            if let Some(row) = detail {
                detail_lines.push(Line::from(format!(
                    "executor={} source={} state={} started={} finished={} pid={}",
                    executor_label(&row.executor),
                    match row.source {
                        RowSource::AuraManaged => "aura",
                        RowSource::ExternalDetected => "external",
                    },
                    match row.state {
                        RuntimeRunState::Running => "running",
                        RuntimeRunState::Finished => "finished",
                    },
                    relative_age(row.started_at),
                    row.finished_at
                        .map(relative_age)
                        .unwrap_or_else(|| "-".to_string()),
                    row.pid
                        .map(|value| value.to_string())
                        .unwrap_or_else(|| "-".to_string())
                )));
                if let Some(session_id) = &row.session_id {
                    detail_lines.push(Line::from(format!("session: {session_id}")));
                }
                if let Some(status) = &row.status {
                    detail_lines.push(Line::from(format!("status: {status}")));
                }
                if let Some(command) = &row.command {
                    detail_lines.push(Line::from(format!("cmd: {}", truncate(command, 120))));
                }
                detail_lines.push(Line::from(format!("open: {}", open_command_for_row(row))));
            } else {
                detail_lines.push(Line::from("No sessions found."));
            }
            let detail_widget = Paragraph::new(detail_lines)
                .block(Block::default().borders(Borders::ALL).title("Detail"));
            frame.render_widget(detail_widget, chunks[2]);

            let footer = Paragraph::new(status)
                .block(Block::default().borders(Borders::ALL).title(
                "Keys: j/k move  r refresh  o open command  x stop  y confirm  n cancel  q quit",
            ));
            frame.render_widget(footer, chunks[3]);
        })?;
        Ok(())
    }

    fn refresh_rows(&mut self) {
        let now = now_epoch_secs();
        let index = session_store::list_session_index_entries()
            .into_iter()
            .map(|entry| {
                let key = entry.session_id.clone();
                (key, entry)
            })
            .collect::<HashMap<_, _>>();

        let mut aura_rows = Vec::new();
        let mut aura_pids = HashSet::new();
        for runtime in session_store::list_runtime_states() {
            let row = normalize_runtime_row(runtime, &index, now);
            if let Some(pid) = row.pid {
                aura_pids.insert(pid);
            }
            aura_rows.push(row);
        }

        let mut rows = aura_rows;
        if self.include_external {
            for process in detect_external_processes() {
                if aura_pids.contains(&process.pid) {
                    continue;
                }
                rows.push(BoardRow {
                    source: RowSource::ExternalDetected,
                    executor: process.executor,
                    session_id: None,
                    pid: Some(process.pid),
                    cwd: None,
                    state: RuntimeRunState::Running,
                    started_at: now.saturating_sub(process.elapsed_secs),
                    updated_at: now,
                    finished_at: None,
                    status: Some("externally detected process".to_string()),
                    command: Some(process.command),
                });
            }
        }

        if !self.show_finished {
            rows.retain(|row| row.state == RuntimeRunState::Running);
        }

        rows.sort_by(|left, right| {
            let left_running = left.state == RuntimeRunState::Running;
            let right_running = right.state == RuntimeRunState::Running;
            right_running
                .cmp(&left_running)
                .then_with(|| right.updated_at.cmp(&left.updated_at))
        });

        self.rows = rows;
        if self.selected >= self.rows.len() {
            self.selected = self.rows.len().saturating_sub(1);
        }
        self.last_refresh = Instant::now();
    }

    fn handle_key(&mut self, key: KeyEvent) -> Result<bool, CliError> {
        if !matches!(key.kind, KeyEventKind::Press | KeyEventKind::Repeat) {
            return Ok(false);
        }
        match key.code {
            KeyCode::Char('q') | KeyCode::Esc => Ok(true),
            KeyCode::Down | KeyCode::Char('j') => {
                if !self.rows.is_empty() {
                    self.selected = (self.selected + 1).min(self.rows.len() - 1);
                }
                Ok(false)
            }
            KeyCode::Up | KeyCode::Char('k') => {
                self.selected = self.selected.saturating_sub(1);
                Ok(false)
            }
            KeyCode::Char('r') => {
                self.refresh_rows();
                self.status_line = "Refreshed session list".to_string();
                Ok(false)
            }
            KeyCode::Char('o') => {
                if let Some(row) = self.rows.get(self.selected) {
                    self.status_line = open_command_for_row(row);
                } else {
                    self.status_line = "No selected row".to_string();
                }
                Ok(false)
            }
            KeyCode::Char('x') => {
                if self.rows.get(self.selected).is_some() {
                    self.confirm_stop = true;
                    self.status_line =
                        "Confirm stop: press 'y' to kill selected process or 'n' to cancel"
                            .to_string();
                } else {
                    self.status_line = "No selected row".to_string();
                }
                Ok(false)
            }
            KeyCode::Char('y') => {
                if !self.confirm_stop {
                    return Ok(false);
                }
                self.confirm_stop = false;
                if let Some(row) = self.rows.get(self.selected).cloned() {
                    self.status_line = stop_row(&row);
                    self.refresh_rows();
                }
                Ok(false)
            }
            KeyCode::Char('n') => {
                if self.confirm_stop {
                    self.confirm_stop = false;
                    self.status_line = "Stop cancelled".to_string();
                }
                Ok(false)
            }
            _ => Ok(false),
        }
    }
}

impl Drop for BoardApp {
    fn drop(&mut self) {
        if let Some(mut terminal) = self.terminal.take() {
            let _ = terminal.show_cursor();
            let _ = disable_raw_mode();
            let _ = execute!(io::stdout(), LeaveAlternateScreen, Show);
        }
    }
}

fn normalize_runtime_row(
    mut runtime: RuntimeSessionState,
    index: &HashMap<String, session_store::SessionIndexEntry>,
    now: u64,
) -> BoardRow {
    if runtime.run_state == RuntimeRunState::Running
        && runtime.pid > 0
        && !process_alive(runtime.pid)
    {
        runtime.run_state = RuntimeRunState::Finished;
        runtime.finished_at = Some(now);
    }

    let fallback = index.get(&runtime.session_id);
    let cwd = if runtime.cwd.is_empty() {
        fallback.map(|entry| entry.cwd.clone())
    } else {
        Some(runtime.cwd.clone())
    };

    BoardRow {
        source: RowSource::AuraManaged,
        executor: runtime.executor.clone(),
        session_id: Some(runtime.session_id.clone()),
        pid: (runtime.pid > 0).then_some(runtime.pid),
        cwd,
        state: runtime.run_state,
        started_at: runtime.started_at,
        updated_at: runtime.updated_at,
        finished_at: runtime.finished_at,
        status: runtime.last_agent_status.or(runtime.last_status),
        command: None,
    }
}

fn stop_row(row: &BoardRow) -> String {
    let Some(pid) = row.pid else {
        return "Cannot stop: selected row has no PID".to_string();
    };
    if !process_alive(pid) {
        return format!("Process {pid} already exited");
    }

    let _ = Command::new("kill")
        .arg("-INT")
        .arg(pid.to_string())
        .status();
    thread::sleep(Duration::from_millis(700));

    if process_alive(pid) {
        let _ = Command::new("kill")
            .arg("-KILL")
            .arg(pid.to_string())
            .status();
        format!("Sent SIGINT then SIGKILL to pid {pid}")
    } else {
        format!("Sent SIGINT to pid {pid}")
    }
}

fn process_alive(pid: u32) -> bool {
    Command::new("kill")
        .arg("-0")
        .arg(pid.to_string())
        .status()
        .is_ok_and(|status| status.success())
}

fn detect_external_processes() -> Vec<ExternalProcess> {
    let output = Command::new("ps")
        .args(["-axo", "pid=,ppid=,etime=,command="])
        .output();

    let Ok(output) = output else {
        return Vec::new();
    };
    if !output.status.success() {
        return Vec::new();
    }

    let mut processes = Vec::new();
    let text = String::from_utf8_lossy(&output.stdout);
    for line in text.lines() {
        let Some((pid, _ppid, elapsed, command)) = parse_ps_line(line) else {
            continue;
        };
        let lower = command.to_ascii_lowercase();
        if lower.contains("aura board") || lower == "ps -axo pid=,ppid=,etime=,command=" {
            continue;
        }
        let Some(executor) = detect_executor(&lower) else {
            continue;
        };
        let elapsed_secs = parse_etime(elapsed).unwrap_or(0);
        processes.push(ExternalProcess {
            pid,
            executor,
            elapsed_secs,
            command: command.to_string(),
        });
    }

    processes
}

fn parse_ps_line(line: &str) -> Option<(u32, u32, &str, &str)> {
    let mut parts = line.split_whitespace();
    let pid = parts.next()?.parse::<u32>().ok()?;
    let ppid = parts.next()?.parse::<u32>().ok()?;
    let elapsed = parts.next()?;
    let command_start = line.find(elapsed)? + elapsed.len();
    let command = line[command_start..].trim();
    if command.is_empty() {
        return None;
    }
    Some((pid, ppid, elapsed, command))
}

fn parse_etime(raw: &str) -> Option<u64> {
    let (days, hms) = if let Some((day, rest)) = raw.split_once('-') {
        (day.parse::<u64>().ok()?, rest)
    } else {
        (0, raw)
    };

    let nums = hms
        .split(':')
        .map(|value| value.parse::<u64>().ok())
        .collect::<Option<Vec<_>>>()?;
    let secs = match nums.as_slice() {
        [mm, ss] => mm * 60 + ss,
        [hh, mm, ss] => hh * 3600 + mm * 60 + ss,
        _ => return None,
    };
    Some(days * 86_400 + secs)
}

fn detect_executor(lower_command: &str) -> Option<ExecutorKind> {
    if lower_command.contains("codex exec")
        || lower_command.starts_with("codex ")
        || lower_command == "codex"
        || lower_command.contains("/bin/codex")
        || lower_command.contains("/codex/codex")
        || lower_command.contains("@openai/codex")
        || lower_command.contains("/applications/codex.app/")
        || lower_command.contains("codex app-server")
    {
        return Some(ExecutorKind::Codex);
    }
    if lower_command.contains("@anthropic-ai/claude-code")
        || lower_command.contains("claude code")
        || lower_command.contains("claude -p")
    {
        return Some(ExecutorKind::Claude);
    }
    if lower_command.contains("@google/gemini-cli") || lower_command.contains("gemini-cli") {
        return Some(ExecutorKind::Gemini);
    }
    if lower_command.contains("opencode-ai") || lower_command.contains(" opencode ") {
        return Some(ExecutorKind::Opencode);
    }
    None
}

fn open_command_for_row(row: &BoardRow) -> String {
    match row.source {
        RowSource::AuraManaged => {
            let session_id = row.session_id.as_deref().unwrap_or("<session-id>");
            format!(
                "aura run --executor {} --session {} --prompt \"Continue from this session.\"",
                executor_arg(&row.executor),
                session_id
            )
        }
        RowSource::ExternalDetected => {
            let pid = row
                .pid
                .map(|value| value.to_string())
                .unwrap_or_else(|| "?".to_string());
            format!("external process pid={pid}; attach manually in your terminal")
        }
    }
}

fn executor_label(executor: &ExecutorKind) -> &'static str {
    match executor {
        ExecutorKind::Codex => "codex",
        ExecutorKind::Claude => "claude",
        ExecutorKind::CursorAgent => "cursor",
        ExecutorKind::Droid => "droid",
        ExecutorKind::Amp => "amp",
        ExecutorKind::Gemini => "gemini",
        ExecutorKind::Opencode => "opencode",
        ExecutorKind::Local => "local",
        ExecutorKind::QwenCode => "qwen",
        ExecutorKind::Copilot => "copilot",
        ExecutorKind::Custom(_) => "custom",
    }
}

fn executor_arg(executor: &ExecutorKind) -> &'static str {
    match executor {
        ExecutorKind::Codex => "codex",
        ExecutorKind::Claude => "claude",
        ExecutorKind::Gemini => "gemini",
        ExecutorKind::Opencode => "opencode",
        ExecutorKind::CursorAgent => "cursor-agent",
        ExecutorKind::Droid => "droid",
        ExecutorKind::Amp => "amp",
        ExecutorKind::Local => "custom",
        ExecutorKind::QwenCode => "qwen-code",
        ExecutorKind::Copilot => "copilot",
        ExecutorKind::Custom(_) => "custom",
    }
}

fn short_session(input: &str) -> String {
    if input.len() <= 16 {
        input.to_string()
    } else {
        format!("{}…", &input[..15])
    }
}

fn short_cwd(input: &str) -> String {
    truncate(input, 48)
}

fn truncate(input: &str, max: usize) -> String {
    if input.chars().count() <= max {
        input.to_string()
    } else {
        let mut output = input.chars().take(max).collect::<String>();
        output.push('…');
        output
    }
}

fn relative_age(ts: u64) -> String {
    let now = now_epoch_secs();
    let delta = now.saturating_sub(ts);
    if delta < 60 {
        format!("{delta}s")
    } else if delta < 3600 {
        format!("{}m", delta / 60)
    } else if delta < 86_400 {
        format!("{}h", delta / 3600)
    } else {
        format!("{}d", delta / 86_400)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_etime_variants() {
        assert_eq!(parse_etime("00:25"), Some(25));
        assert_eq!(parse_etime("01:00:00"), Some(3600));
        assert_eq!(parse_etime("2-03:04:05"), Some(183_845));
        assert_eq!(parse_etime("abc"), None);
    }

    #[test]
    fn detects_supported_executors() {
        assert!(matches!(
            detect_executor("codex exec --json"),
            Some(ExecutorKind::Codex)
        ));
        assert!(matches!(
            detect_executor("node /Users/x/.nvm/versions/node/v23.9.0/bin/codex"),
            Some(ExecutorKind::Codex)
        ));
        assert!(matches!(
            detect_executor(
                "/Users/x/.nvm/versions/node/v23.9.0/lib/node_modules/@openai/codex/node_modules/@openai/codex-darwin-arm64/vendor/aarch64-apple-darwin/codex/codex"
            ),
            Some(ExecutorKind::Codex)
        ));
        assert!(matches!(
            detect_executor("/Applications/Codex.app/Contents/Resources/codex app-server"),
            Some(ExecutorKind::Codex)
        ));
        assert!(matches!(
            detect_executor("npx -y @anthropic-ai/claude-code@2.1.12 -p"),
            Some(ExecutorKind::Claude)
        ));
        assert!(matches!(
            detect_executor("npx -y @google/gemini-cli@0.23.0"),
            Some(ExecutorKind::Gemini)
        ));
        assert!(matches!(
            detect_executor("npx -y opencode-ai@1.1.25 serve"),
            Some(ExecutorKind::Opencode)
        ));
    }
}
