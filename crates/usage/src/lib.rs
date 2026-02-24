use aura_contracts::ExecutorKind;
use chrono::{DateTime, Duration as ChronoDuration, Utc};
use fuelcheck_core::reports::{
    codex::CodexReportOptions, normalize_model_name, types::CostReportKind, types::ProviderReport,
    types::SessionReportRow, types::split_usage_tokens,
};
use serde_json::Value;
use std::process::Command;
use std::time::{Duration, Instant};

pub struct UsageTracker {
    base_status: String,
    inner: UsageTrackerImpl,
    resources: ResourceTracker,
}

enum UsageTrackerImpl {
    Codex(CodexUsageTracker),
    Unsupported,
}

impl UsageTracker {
    pub fn new(executor: &ExecutorKind) -> Self {
        let base_status = match executor {
            ExecutorKind::Codex => "collecting usage...".to_string(),
            _ => format!("{}: n/a", executor_display_name(executor)),
        };
        let inner = match executor {
            ExecutorKind::Codex => UsageTrackerImpl::Codex(CodexUsageTracker::default()),
            _ => UsageTrackerImpl::Unsupported,
        };
        Self {
            base_status,
            inner,
            resources: ResourceTracker::default(),
        }
    }

    pub fn initial_status(&self) -> String {
        self.compose_status()
    }

    pub fn on_process_started(&mut self, pid: u32) -> Option<String> {
        self.resources.attach_process(pid);
        self.resources.sample_if_due(true);
        Some(self.compose_status())
    }

    pub fn on_tick(&mut self) -> Option<String> {
        if self.resources.sample_if_due(false) {
            return Some(self.compose_status());
        }
        None
    }

    pub fn final_status(&mut self) -> Option<String> {
        self.resources.sample_if_due(true);
        self.resources
            .summary_text()
            .map(|summary| format!("{} | {summary}", self.base_status))
    }

    pub fn on_event(&mut self, event_type: &str, value: &Value) -> Option<String> {
        let updated = match &mut self.inner {
            UsageTrackerImpl::Codex(tracker) => tracker.on_event(event_type, value),
            UsageTrackerImpl::Unsupported => None,
        };
        if let Some(status) = updated {
            self.base_status = status;
            self.resources.sample_if_due(false);
            return Some(self.compose_status());
        }
        None
    }

    fn compose_status(&self) -> String {
        format!("{} | {}", self.base_status, self.resources.live_text())
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

#[derive(Default)]
struct ResourceTracker {
    pid: Option<u32>,
    last_sample_at: Option<Instant>,
    last_gpu_sample_at: Option<Instant>,
    last_snapshot: ResourceSnapshot,
    stats: ResourceStats,
}

#[derive(Default, Clone, Copy)]
struct ResourceSnapshot {
    cpu_percent: Option<f32>,
    gpu_percent: Option<f32>,
    rss_kb: Option<u64>,
}

#[derive(Default)]
struct ResourceStats {
    cpu_sum: f64,
    cpu_count: u64,
    cpu_peak: f32,
    gpu_sum: f64,
    gpu_count: u64,
    gpu_peak: f32,
    rss_peak_kb: u64,
}

impl ResourceTracker {
    fn attach_process(&mut self, pid: u32) {
        self.pid = Some(pid);
    }

    fn sample_if_due(&mut self, force: bool) -> bool {
        let Some(pid) = self.pid else {
            return false;
        };
        let now = Instant::now();
        let due = force
            || self
                .last_sample_at
                .is_none_or(|last| now.saturating_duration_since(last) >= Duration::from_secs(1));
        if !due {
            return false;
        }

        let mut updated = false;
        if let Some((cpu_percent, rss_kb)) = sample_process_cpu_rss(pid) {
            self.last_snapshot.cpu_percent = Some(cpu_percent);
            self.last_snapshot.rss_kb = Some(rss_kb);
            self.stats.cpu_sum += f64::from(cpu_percent);
            self.stats.cpu_count += 1;
            self.stats.cpu_peak = self.stats.cpu_peak.max(cpu_percent);
            self.stats.rss_peak_kb = self.stats.rss_peak_kb.max(rss_kb);
            updated = true;
        }

        let gpu_due = force
            || self
                .last_gpu_sample_at
                .is_none_or(|last| now.saturating_duration_since(last) >= Duration::from_secs(3));
        if gpu_due {
            if let Some(gpu_percent) = sample_gpu_percent() {
                self.last_snapshot.gpu_percent = Some(gpu_percent);
                self.stats.gpu_sum += f64::from(gpu_percent);
                self.stats.gpu_count += 1;
                self.stats.gpu_peak = self.stats.gpu_peak.max(gpu_percent);
                updated = true;
            }
            self.last_gpu_sample_at = Some(now);
        }

        self.last_sample_at = Some(now);
        updated
    }

    fn live_text(&self) -> String {
        let cpu = self
            .last_snapshot
            .cpu_percent
            .map(|value| format!("{value:.1}%"))
            .unwrap_or_else(|| "--".to_string());
        let gpu = self
            .last_snapshot
            .gpu_percent
            .map(|value| format!("{value:.1}%"))
            .unwrap_or_else(|| "--".to_string());
        let mem = self
            .last_snapshot
            .rss_kb
            .map(format_kib_as_mib)
            .unwrap_or_else(|| "--".to_string());
        format!("cpu={cpu} gpu={gpu} mem={mem}")
    }

    fn summary_text(&self) -> Option<String> {
        if self.stats.cpu_count == 0 && self.stats.gpu_count == 0 && self.stats.rss_peak_kb == 0 {
            return None;
        }
        let cpu_avg = if self.stats.cpu_count > 0 {
            self.stats.cpu_sum / self.stats.cpu_count as f64
        } else {
            0.0
        };
        let gpu_avg = if self.stats.gpu_count > 0 {
            self.stats.gpu_sum / self.stats.gpu_count as f64
        } else {
            0.0
        };
        Some(format!(
            "resource: cpu avg/peak={cpu_avg:.1}%/{:.1}% | gpu avg/peak={gpu_avg:.1}%/{:.1}% | mem peak={}",
            self.stats.cpu_peak,
            self.stats.gpu_peak,
            format_kib_as_mib(self.stats.rss_peak_kb)
        ))
    }
}

fn sample_process_cpu_rss(pid: u32) -> Option<(f32, u64)> {
    let output = Command::new("ps")
        .arg("-p")
        .arg(pid.to_string())
        .arg("-o")
        .arg("%cpu=")
        .arg("-o")
        .arg("rss=")
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let text = String::from_utf8_lossy(&output.stdout);
    let mut parts = text.split_whitespace();
    let cpu = parts.next()?.parse::<f32>().ok()?;
    let rss_kb = parts.next()?.parse::<u64>().ok()?;
    Some((cpu, rss_kb))
}

fn sample_gpu_percent() -> Option<f32> {
    #[cfg(target_os = "macos")]
    {
        let output = Command::new("ioreg")
            .arg("-r")
            .arg("-d")
            .arg("1")
            .arg("-c")
            .arg("AGXAccelerator")
            .output()
            .ok()?;
        if !output.status.success() {
            return None;
        }
        let text = String::from_utf8_lossy(&output.stdout);
        for line in text.lines() {
            if !line.contains("Device Utilization %") {
                continue;
            }
            let digits: String = line
                .chars()
                .skip_while(|ch| !ch.is_ascii_digit())
                .take_while(|ch| ch.is_ascii_digit())
                .collect();
            if let Ok(value) = digits.parse::<f32>() {
                return Some(value.min(100.0));
            }
        }
        None
    }
    #[cfg(not(target_os = "macos"))]
    {
        None
    }
}

fn format_kib_as_mib(kib: u64) -> String {
    format!("{:.0}MiB", kib as f64 / 1024.0)
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
        ExecutorKind::Local => "local".to_string(),
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
        let status = tracker.initial_status();
        assert!(status.contains("claude: n/a"));
        assert!(status.contains("cpu="));
        assert!(status.contains("gpu="));
        assert!(status.contains("mem="));
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
