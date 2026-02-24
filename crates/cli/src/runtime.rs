use super::*;
use crossterm::event::{self, Event, KeyModifiers};
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::sync::mpsc;

pub(crate) fn summarize_text(text: &str, max_len: usize) -> String {
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

enum ChildEvent {
    Line(LogStream, String),
    StreamClosed,
    ExitSignal(aura_executors::ExecutorExitResult),
}

pub(crate) enum DisplayEvent {
    Line(LogStream, String),
    Status(String),
    AgentStatus(String),
    UsageStatus(String),
    ModelUnavailable,
    SessionThreadId(String),
    AgentMarkdown(String),
}

pub(crate) struct LogFormatter {
    executor: ExecutorKind,
    pub(crate) suppressed_codex_rollout_warnings: usize,
    emitted_codex_warning_notice: bool,
    usage_tracker: UsageTracker,
    ollama_in_think: bool,
    ollama_mode: bool,
}

impl LogFormatter {
    pub(crate) fn new(executor: ExecutorKind, local_provider: Option<LocalProvider>) -> Self {
        Self {
            usage_tracker: UsageTracker::new(&executor),
            executor,
            suppressed_codex_rollout_warnings: 0,
            emitted_codex_warning_notice: false,
            ollama_in_think: false,
            ollama_mode: matches!(local_provider, Some(LocalProvider::Ollama)),
        }
    }

    pub(crate) fn format(&mut self, stream: LogStream, raw_line: &str) -> Vec<DisplayEvent> {
        let line = raw_line.trim_end().to_string();
        if line.is_empty() {
            return Vec::new();
        }

        if self.ollama_mode && stream == LogStream::Stdout {
            let mut events = Vec::new();
            for line in self.format_ollama_line(&line) {
                events.push(DisplayEvent::Line(stream, line));
            }
            return events;
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

            if stream == LogStream::Stderr && Self::is_model_refresh_timeout(&line) {
                return vec![DisplayEvent::ModelUnavailable];
            }

            if let Some(formatted) = self.format_codex_event(&line) {
                return formatted;
            }
        }

        vec![DisplayEvent::Line(stream, line)]
    }

    fn format_ollama_line(&mut self, line: &str) -> Vec<String> {
        let mut out = Vec::new();
        let mut remaining = line;

        loop {
            if self.ollama_in_think {
                if let Some(end_idx) = remaining.find("</think>") {
                    let chunk = &remaining[..end_idx];
                    self.emit_prefixed_lines("thinking: ", chunk, &mut out);
                    remaining = &remaining[end_idx + "</think>".len()..];
                    self.ollama_in_think = false;
                    if remaining.is_empty() {
                        break;
                    }
                    continue;
                }
                self.emit_prefixed_lines("thinking: ", remaining, &mut out);
                break;
            } else if let Some(start_idx) = remaining.find("<think>") {
                let before = &remaining[..start_idx];
                self.emit_prefixed_lines("agent: ", before, &mut out);
                remaining = &remaining[start_idx + "<think>".len()..];
                self.ollama_in_think = true;
                if remaining.is_empty() {
                    break;
                }
                continue;
            } else {
                self.emit_prefixed_lines("agent: ", remaining, &mut out);
                break;
            }
        }

        out
    }

    fn emit_prefixed_lines(&self, prefix: &str, text: &str, out: &mut Vec<String>) {
        for line in text.lines() {
            let trimmed = line.trim();
            if trimmed.is_empty() {
                continue;
            }
            out.push(format!("{prefix}{trimmed}"));
        }
    }

    fn format_codex_event(&mut self, line: &str) -> Option<Vec<DisplayEvent>> {
        let value: Value = serde_json::from_str(line).ok()?;
        let event_type = value.get("type")?.as_str()?;

        let mut events = match event_type {
            "thread.started" => {
                let thread_id = value
                    .get("thread_id")
                    .and_then(Value::as_str)
                    .map(|value| value.to_string());
                let mut events = vec![
                    DisplayEvent::Status(match &thread_id {
                        Some(id) => format!("thread started (id={id})"),
                        None => "thread started".to_string(),
                    }),
                    DisplayEvent::AgentStatus("Initializing".to_string()),
                ];
                if let Some(id) = thread_id {
                    events.push(DisplayEvent::SessionThreadId(id));
                }
                events
            }
            "turn.started" => vec![
                DisplayEvent::Status("turn started".to_string()),
                DisplayEvent::AgentStatus("Thinking".to_string()),
            ],
            "turn.completed" => vec![
                DisplayEvent::Status("turn completed".to_string()),
                DisplayEvent::AgentStatus("Idle".to_string()),
            ],
            "turn.failed" => {
                let message = value
                    .get("error")
                    .and_then(|e| e.get("message"))
                    .and_then(Value::as_str)
                    .unwrap_or("unknown error");
                vec![
                    DisplayEvent::Status(format!("turn failed: {}", summarize_text(message, 160))),
                    DisplayEvent::AgentStatus("Error".to_string()),
                ]
            }
            "error" => {
                let message = value
                    .get("message")
                    .and_then(Value::as_str)
                    .unwrap_or("unknown error");
                vec![
                    DisplayEvent::Line(
                        LogStream::Stderr,
                        format!("codex error: {}", summarize_text(message, 180)),
                    ),
                    DisplayEvent::AgentStatus("Error".to_string()),
                ]
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
                    vec![
                        DisplayEvent::Line(
                            LogStream::Stdout,
                            format!("cmd > {}", summarize_text(command, 180)),
                        ),
                        DisplayEvent::AgentStatus("Executing command".to_string()),
                    ]
                } else {
                    return None;
                }
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
                        vec![
                            DisplayEvent::Line(
                                LogStream::Stdout,
                                format!(
                                    "cmd ✓ (exit={exit_code}) {}",
                                    summarize_text(command, 160)
                                ),
                            ),
                            DisplayEvent::AgentStatus("Thinking".to_string()),
                        ]
                    }
                    "agent_message" => {
                        let text = item.get("text").and_then(Value::as_str).unwrap_or("");
                        if text.is_empty() {
                            return None;
                        }
                        vec![
                            DisplayEvent::AgentMarkdown(text.to_string()),
                            DisplayEvent::AgentStatus("Responding".to_string()),
                        ]
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
                        vec![
                            DisplayEvent::Line(
                                LogStream::Stdout,
                                format!("thinking: {}", summarize_text(text, 140)),
                            ),
                            DisplayEvent::AgentStatus(state.to_string()),
                        ]
                    }
                    _ => return None,
                }
            }
            _ => return None,
        };

        if let Some(status) = self.usage_tracker.on_event(event_type, &value) {
            events.push(DisplayEvent::UsageStatus(status));
        }

        Some(events)
    }

    fn is_model_refresh_timeout(line: &str) -> bool {
        line.contains("failed to refresh available models")
            && line.contains("timeout waiting for child process to exit")
    }

    fn on_process_started(&mut self, pid: u32) -> Option<String> {
        self.usage_tracker.on_process_started(pid)
    }

    fn on_usage_tick(&mut self) -> Option<String> {
        self.usage_tracker.on_tick()
    }

    fn on_process_finished(&mut self) -> Option<String> {
        self.usage_tracker.final_status()
    }
}

pub(crate) async fn stream_spawned(
    executor_kind: ExecutorKind,
    session_context: Option<SessionContext>,
    local_provider: Option<LocalProvider>,
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
    let mut formatter = LogFormatter::new(executor_kind, local_provider);
    if let Some(pid) = child.id()
        && let Some(status) = formatter.on_process_started(pid)
    {
        sink.on_usage_status(&status);
    }
    let mut last_wait_status_at = Instant::now();
    let mut quit_requested_at: Option<Instant> = None;
    let mut user_requested_exit = false;

    loop {
        if enable_keyboard_controls {
            if let Ok(Some(terminal_event)) = read_terminal_event() {
                let requested_quit = match terminal_event {
                    Event::Key(key) => {
                        let key_action = sink.on_key_event(key);
                        matches!(key_action, SinkKeyAction::RequestQuit) || is_quit_key(key)
                    }
                    Event::Paste(text) => {
                        sink.on_paste(&text);
                        false
                    }
                    _ => false,
                };

                if requested_quit {
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
                if let Some(status) = formatter.on_usage_tick() {
                    sink.on_usage_status(&status);
                }
                sink.flush();
                continue;
            }
        }) else {
            if got_exit_status && streams_closed >= expected_streams {
                break;
            }
            if let Some(status) = formatter.on_usage_tick() {
                sink.on_usage_status(&status);
            }
            sink.flush();
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
                        DisplayEvent::UsageStatus(status) => sink.on_usage_status(&status),
                        DisplayEvent::ModelUnavailable => sink.on_model_unavailable(),
                        DisplayEvent::SessionThreadId(thread_id) => {
                            if let Some(context) = &session_context {
                                session_store::update_session_thread_id(context, &thread_id);
                            }
                        }
                        DisplayEvent::AgentMarkdown(markdown) => {
                            sink.on_line(LogStream::Stdout, &format!("agent_md: {markdown}"));
                        }
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

        if let Some(status) = formatter.on_usage_tick() {
            sink.on_usage_status(&status);
        }
        sink.flush();
    }

    if formatter.suppressed_codex_rollout_warnings > 0 {
        sink.on_status(&format!(
            "suppressed {} repeated codex rollout-path warnings",
            formatter.suppressed_codex_rollout_warnings
        ));
    }
    if let Some(status) = formatter.on_process_finished() {
        sink.on_usage_status(&status);
    }

    sink.on_exit(exit_code);
    Ok(RunOutcome {
        success: exit_code == Some(0),
        exit_code,
        user_requested_exit,
    })
}

fn read_terminal_event() -> io::Result<Option<Event>> {
    if !event::poll(Duration::from_millis(0))? {
        return Ok(None);
    }
    Ok(Some(event::read()?))
}

pub(crate) fn is_quit_key(key: KeyEvent) -> bool {
    if !matches!(key.kind, KeyEventKind::Press | KeyEventKind::Repeat) {
        return false;
    }

    match key.code {
        KeyCode::Esc => true,
        KeyCode::Char('c') | KeyCode::Char('C') => key.modifiers.contains(KeyModifiers::CONTROL),
        _ => false,
    }
}

pub(crate) fn apply_prompt_input_key(input: &mut String, key: KeyEvent) -> PromptInputUpdate {
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

pub(crate) fn is_prompt_quit_key(key: KeyEvent) -> bool {
    if !matches!(key.kind, KeyEventKind::Press | KeyEventKind::Repeat) {
        return false;
    }

    matches!(key.code, KeyCode::Esc)
        || matches!(key.code, KeyCode::Char('c') | KeyCode::Char('C'))
            && key.modifiers.contains(KeyModifiers::CONTROL)
}

pub(crate) fn read_tui_prompt_action(sink: &mut TuiLogSink) -> io::Result<TuiPromptAction> {
    loop {
        if let Some(submission) = sink.take_queued_prompt() {
            return Ok(TuiPromptAction::Submit {
                prompt: submission.prompt,
                model: submission.model,
                mode: submission.mode,
            });
        }

        if !event::poll(Duration::from_millis(100))? {
            continue;
        }

        match event::read()? {
            Event::Key(key) => {
                if matches!(sink.on_key_event(key), SinkKeyAction::RequestQuit) {
                    return Ok(TuiPromptAction::Quit);
                }
            }
            Event::Paste(pasted) => sink.on_paste(&pasted),
            _ => {}
        }
    }
}

pub(crate) fn handle_logs_navigation_key(sink: &mut TuiLogSink, key: KeyEvent) -> bool {
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
