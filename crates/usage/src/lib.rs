use aura_contracts::ExecutorKind;
use chrono::{DateTime, Duration as ChronoDuration, Utc};
use fuelcheck_core::reports::{
    codex::CodexReportOptions, normalize_model_name, types::CostReportKind, types::ProviderReport,
    types::SessionReportRow, types::split_usage_tokens,
};
use serde_json::Value;

pub struct UsageTracker {
    inner: UsageTrackerImpl,
}

enum UsageTrackerImpl {
    Codex(CodexUsageTracker),
    Unsupported(UnsupportedUsageTracker),
}

impl UsageTracker {
    pub fn new(executor: &ExecutorKind) -> Self {
        let inner = match executor {
            ExecutorKind::Codex => UsageTrackerImpl::Codex(CodexUsageTracker::default()),
            _ => UsageTrackerImpl::Unsupported(UnsupportedUsageTracker {
                executor_label: executor_display_name(executor),
            }),
        };
        Self { inner }
    }

    pub fn initial_status(&self) -> String {
        match &self.inner {
            UsageTrackerImpl::Codex(_) => "collecting usage...".to_string(),
            UsageTrackerImpl::Unsupported(tracker) => format!("{}: n/a", tracker.executor_label),
        }
    }

    pub fn on_event(&mut self, event_type: &str, value: &Value) -> Option<String> {
        match &mut self.inner {
            UsageTrackerImpl::Codex(tracker) => tracker.on_event(event_type, value),
            UsageTrackerImpl::Unsupported(_) => None,
        }
    }
}

#[derive(Default)]
struct CodexUsageTracker {
    thread_id: Option<String>,
    session_hint: Option<String>,
    run_started_at_utc: Option<DateTime<Utc>>,
}

impl CodexUsageTracker {
    fn on_event(&mut self, event_type: &str, value: &Value) -> Option<String> {
        match event_type {
            "thread.started" => {
                if let Some(thread_id) = value.get("thread_id").and_then(Value::as_str) {
                    self.thread_id = Some(thread_id.to_string());
                    self.run_started_at_utc = Some(Utc::now());
                    return Some(format!(
                        "thread={} | collecting usage...",
                        summarize_text(thread_id, 12)
                    ));
                }
                None
            }
            "turn.completed" | "turn.failed" => {
                let thread_id = self.thread_id.as_deref()?;
                let run_started_at_utc = self.run_started_at_utc.unwrap_or_else(Utc::now);
                let (status, session_hint) = build_codex_usage_status(
                    thread_id,
                    run_started_at_utc,
                    self.session_hint.as_deref(),
                );
                self.session_hint = session_hint;
                if self.session_hint.is_none() && status.contains("collecting usage...") {
                    return Some(format!(
                        "thread={} | usage unavailable (no session usage found)",
                        summarize_text(thread_id, 12)
                    ));
                }
                Some(status)
            }
            _ => None,
        }
    }
}

struct UnsupportedUsageTracker {
    executor_label: String,
}

fn build_codex_usage_status(
    thread_id: &str,
    run_started_at_utc: DateTime<Utc>,
    session_hint: Option<&str>,
) -> (String, Option<String>) {
    let options = CodexReportOptions {
        report: CostReportKind::Session,
        since: None,
        until: None,
        timezone: None,
    };

    let report = match fuelcheck_core::reports::codex::build_report(&options) {
        Ok(report) => report,
        Err(error) => {
            return (
                format!(
                    "thread={} | usage unavailable ({})",
                    summarize_text(thread_id, 12),
                    summarize_text(&error.to_string(), 64)
                ),
                None,
            );
        }
    };
    let ProviderReport::Session(response) = report else {
        return (
            format!(
                "thread={} | usage unavailable",
                summarize_text(thread_id, 12)
            ),
            None,
        );
    };

    let Some(row) = find_codex_usage_row(
        &response.sessions,
        thread_id,
        run_started_at_utc,
        session_hint,
    ) else {
        return (
            format!(
                "thread={} | collecting usage...",
                summarize_text(thread_id, 12)
            ),
            None,
        );
    };

    let split = split_usage_tokens(
        row.input_tokens,
        row.cached_input_tokens,
        row.output_tokens,
        row.reasoning_output_tokens,
    );
    let model = row
        .models
        .keys()
        .next()
        .map(|raw| normalize_model_name(raw))
        .unwrap_or_else(|| "unknown".to_string());

    (
        format!(
            "in={} cache={} out={} reason={} total={} | cost=${:.4} | model={}",
            format_count(split.input_tokens),
            format_count(split.cache_read_tokens),
            format_count(split.output_tokens),
            format_count(split.reasoning_tokens),
            format_count(row.total_tokens),
            row.cost_usd,
            model
        ),
        Some(row.session_id.clone()),
    )
}

fn find_codex_usage_row<'a>(
    sessions: &'a [SessionReportRow],
    thread_id: &str,
    run_started_at_utc: DateTime<Utc>,
    session_hint: Option<&str>,
) -> Option<&'a SessionReportRow> {
    if let Some(hint) = session_hint
        && let Some(session) = sessions.iter().find(|session| session.session_id == hint)
    {
        return Some(session);
    }

    if let Some(session) = sessions
        .iter()
        .find(|session| codex_session_matches_thread(session, thread_id))
    {
        return Some(session);
    }

    let recency_floor = run_started_at_utc - ChronoDuration::minutes(2);
    sessions.iter().rev().find(|session| {
        parse_rfc3339_utc(&session.last_activity).is_some_and(|ts| ts >= recency_floor)
    })
}

fn codex_session_matches_thread(session: &SessionReportRow, thread_id: &str) -> bool {
    if session.session_id == thread_id
        || session.session_id.ends_with(thread_id)
        || session.session_id.contains(thread_id)
    {
        return true;
    }

    extract_uuid_like_fragment(thread_id).is_some_and(|fragment| {
        session.session_id.contains(fragment) || session.session_file.contains(fragment)
    })
}

fn extract_uuid_like_fragment(value: &str) -> Option<&str> {
    value
        .split(|ch: char| !(ch.is_ascii_hexdigit() || ch == '-'))
        .find(|part| part.len() >= 32 && part.chars().filter(|ch| *ch == '-').count() >= 4)
}

fn parse_rfc3339_utc(value: &str) -> Option<DateTime<Utc>> {
    DateTime::parse_from_rfc3339(value)
        .ok()
        .map(|time| time.with_timezone(&Utc))
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

fn format_count(value: u64) -> String {
    let raw = value.to_string();
    let mut out = String::new();
    for (index, ch) in raw.chars().rev().enumerate() {
        if index > 0 && index % 3 == 0 {
            out.push(',');
        }
        out.push(ch);
    }
    out.chars().rev().collect()
}

fn executor_display_name(executor: &ExecutorKind) -> String {
    match executor {
        ExecutorKind::Amp => "amp".to_string(),
        ExecutorKind::Gemini => "gemini".to_string(),
        ExecutorKind::Codex => "codex".to_string(),
        ExecutorKind::Claude => "claude".to_string(),
        ExecutorKind::Opencode => "opencode".to_string(),
        ExecutorKind::QwenCode => "qwen-code".to_string(),
        ExecutorKind::Copilot => "copilot".to_string(),
        ExecutorKind::CursorAgent => "cursor-agent".to_string(),
        ExecutorKind::Droid => "droid".to_string(),
        ExecutorKind::Custom(name) => format!("custom({})", name),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unsupported_executor_reports_na() {
        let tracker = UsageTracker::new(&ExecutorKind::Claude);
        assert_eq!(tracker.initial_status(), "claude: n/a");
    }

    #[test]
    fn codex_tracker_reports_collecting_on_thread_start() {
        let mut tracker = UsageTracker::new(&ExecutorKind::Codex);
        let value: Value = serde_json::json!({
            "type": "thread.started",
            "thread_id": "abcd-1234"
        });
        let status = tracker.on_event("thread.started", &value);
        assert!(status.is_some());
        assert!(status.unwrap_or_default().contains("collecting usage"));
    }
}
