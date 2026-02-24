use super::*;
use std::path::Path;

#[derive(Debug, Clone, Serialize, Deserialize)]
struct SessionMetadata {
    session_id: String,
    executor: ExecutorKind,
    cwd: String,
    created_at: u64,
    thread_id: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct SessionIndexEntry {
    session_id: String,
    executor: ExecutorKind,
    cwd: String,
    created_at: u64,
    thread_id: Option<String>,
}

fn local_session_file(session_id: &str) -> PathBuf {
    let home = std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."));
    home.join(".aura")
        .join("local_sessions")
        .join(format!("{session_id}.json"))
}

fn session_metadata_dir() -> PathBuf {
    let home = std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."));
    home.join(".aura").join("sessions")
}

fn session_metadata_path(session_id: &str) -> PathBuf {
    session_metadata_dir().join(format!("{session_id}.json"))
}

pub(super) fn write_session_metadata(context: &SessionContext, thread_id: Option<String>) {
    let existing_thread_id = session_metadata_path(&context.session_id)
        .exists()
        .then(|| session_metadata_path(&context.session_id))
        .and_then(|path| fs::read_to_string(path).ok())
        .and_then(|raw| serde_json::from_str::<SessionMetadata>(&raw).ok())
        .and_then(|metadata| metadata.thread_id);
    let metadata = SessionMetadata {
        session_id: context.session_id.clone(),
        executor: context.executor.clone(),
        cwd: context.cwd.display().to_string(),
        created_at: now_epoch_secs(),
        thread_id: thread_id.or(existing_thread_id),
    };
    let path = session_metadata_path(&metadata.session_id);
    if let Some(parent) = path.parent() {
        let _ = fs::create_dir_all(parent);
    }
    if let Ok(serialized) = serde_json::to_string(&metadata) {
        let _ = fs::write(path, serialized);
    }
    upsert_session_index(SessionIndexEntry {
        session_id: metadata.session_id.clone(),
        executor: metadata.executor.clone(),
        cwd: metadata.cwd.clone(),
        created_at: metadata.created_at,
        thread_id: metadata.thread_id.clone(),
    });
}

pub(super) fn update_session_thread_id(context: &SessionContext, thread_id: &str) {
    let path = session_metadata_path(&context.session_id);
    let updated = if let Ok(raw) = fs::read_to_string(&path) {
        if let Ok(mut metadata) = serde_json::from_str::<SessionMetadata>(&raw) {
            metadata.thread_id = Some(thread_id.to_string());
            metadata
        } else {
            SessionMetadata {
                session_id: context.session_id.clone(),
                executor: context.executor.clone(),
                cwd: context.cwd.display().to_string(),
                created_at: now_epoch_secs(),
                thread_id: Some(thread_id.to_string()),
            }
        }
    } else {
        SessionMetadata {
            session_id: context.session_id.clone(),
            executor: context.executor.clone(),
            cwd: context.cwd.display().to_string(),
            created_at: now_epoch_secs(),
            thread_id: Some(thread_id.to_string()),
        }
    };

    if let Ok(serialized) = serde_json::to_string(&updated) {
        let _ = fs::write(path, serialized);
    }
    upsert_session_index(SessionIndexEntry {
        session_id: updated.session_id.clone(),
        executor: updated.executor.clone(),
        cwd: updated.cwd.clone(),
        created_at: updated.created_at,
        thread_id: updated.thread_id.clone(),
    });
}

fn session_index_path() -> PathBuf {
    session_metadata_dir().join("index.json")
}

fn read_session_index_entries() -> Option<Vec<SessionIndexEntry>> {
    let path = session_index_path();
    let Ok(raw) = fs::read_to_string(path) else {
        return None;
    };
    serde_json::from_str::<Vec<SessionIndexEntry>>(&raw).ok()
}

fn write_session_index_entries(entries: &[SessionIndexEntry]) {
    let path = session_index_path();
    if let Some(parent) = path.parent() {
        let _ = fs::create_dir_all(parent);
    }
    if let Ok(serialized) = serde_json::to_string(entries) {
        let _ = fs::write(path, serialized);
    }
}

fn upsert_session_index(entry: SessionIndexEntry) {
    let mut entries = read_session_index_entries().unwrap_or_default();
    if let Some(existing) = entries
        .iter_mut()
        .find(|existing| existing.session_id == entry.session_id)
    {
        existing.executor = entry.executor;
        existing.cwd = entry.cwd;
        if existing.created_at == 0 {
            existing.created_at = entry.created_at;
        }
        if entry.thread_id.is_some() {
            existing.thread_id = entry.thread_id;
        }
    } else {
        entries.push(entry);
    }
    write_session_index_entries(&entries);
}

fn read_session_metadata_entries() -> Vec<SessionMetadata> {
    let dir = session_metadata_dir();
    let Ok(entries) = fs::read_dir(dir) else {
        return Vec::new();
    };

    let mut out = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|ext| ext.to_str()) != Some("json") {
            continue;
        }
        let Ok(raw) = fs::read_to_string(&path) else {
            continue;
        };
        let Ok(metadata) = serde_json::from_str::<SessionMetadata>(&raw) else {
            continue;
        };
        out.push(metadata);
    }
    out
}

fn read_session_entries_from_index_or_disk() -> Vec<SessionMetadata> {
    if let Some(entries) = read_session_index_entries()
        && !entries.is_empty()
    {
        return entries
            .into_iter()
            .map(|entry| SessionMetadata {
                session_id: entry.session_id,
                executor: entry.executor,
                cwd: entry.cwd,
                created_at: entry.created_at,
                thread_id: entry.thread_id,
            })
            .collect();
    }
    read_session_metadata_entries()
}

fn select_latest_session(entries: &[SessionMetadata]) -> Option<SessionMetadata> {
    entries.iter().max_by_key(|entry| entry.created_at).cloned()
}

pub(super) fn latest_session_id() -> Option<String> {
    let entries = read_session_entries_from_index_or_disk();
    select_latest_session(&entries).map(|entry| entry.session_id)
}

fn session_cwd_matches(entry_cwd: &str, filter_cwd: &Path) -> bool {
    let entry_path = PathBuf::from(entry_cwd);
    let entry_canon = entry_path.canonicalize().unwrap_or(entry_path.clone());
    let filter_canon = filter_cwd
        .canonicalize()
        .unwrap_or_else(|_| filter_cwd.to_path_buf());
    entry_canon == filter_canon || entry_cwd == filter_cwd.display().to_string()
}

fn filtered_session_entries(args: &SessionListArgs) -> Vec<SessionMetadata> {
    let mut entries = read_session_entries_from_index_or_disk();
    if let Some(executor) = args.executor {
        let executor_kind = executor_kind_from_arg(executor);
        entries.retain(|entry| entry.executor == executor_kind);
    }
    if let Some(cwd) = args.cwd.as_ref() {
        entries.retain(|entry| session_cwd_matches(&entry.cwd, cwd));
    }

    entries.sort_by_key(|entry| std::cmp::Reverse(entry.created_at));
    if let Some(limit) = args.limit {
        entries.truncate(limit);
    }
    entries
}

fn format_session_list(entries: &[SessionMetadata]) -> String {
    if entries.is_empty() {
        return "no saved sessions found".to_string();
    }

    let mut lines = Vec::new();
    lines.push("session_id | executor | created_at | cwd | thread_id".to_string());
    for entry in entries {
        let thread = entry.thread_id.clone().unwrap_or_else(|| "-".to_string());
        lines.push(format!(
            "{} | {:?} | {} | {} | {}",
            entry.session_id, entry.executor, entry.created_at, entry.cwd, thread
        ));
    }
    lines.join("\n")
}

pub(super) fn list_sessions(args: SessionListArgs) -> String {
    let entries = filtered_session_entries(&args);
    format_session_list(&entries)
}

fn prompt_for_session_selection(entries: &[SessionMetadata]) -> Result<SessionMetadata, CliError> {
    let mut selector = cliclack::select("Select session to resume");
    for (index, entry) in entries.iter().enumerate() {
        let label = format!(
            "{} | {:?} | {} | {}",
            entry.session_id, entry.executor, entry.created_at, entry.cwd
        );
        let thread = entry.thread_id.as_deref().unwrap_or("no thread");
        let hint = if index == 0 {
            format!("latest • thread: {thread}")
        } else {
            format!("thread: {thread}")
        };
        selector = selector.item(entry.session_id.clone(), label, hint);
    }

    let selected_session_id = selector
        .interact()
        .map_err(|error| CliError::Runtime(format!("session selection failed: {error}")))?;
    entries
        .iter()
        .find(|entry| entry.session_id == selected_session_id)
        .cloned()
        .ok_or_else(|| {
            CliError::Runtime(format!("selected session not found: {selected_session_id}"))
        })
}

pub(super) async fn run_session_list(
    args: SessionListArgs,
) -> Result<Option<RunOutcome>, CliError> {
    if !io::stdin().is_terminal() || !io::stdout().is_terminal() {
        return Ok(None);
    }

    let entries = filtered_session_entries(&args);
    if entries.is_empty() {
        return Ok(None);
    }

    let selected = prompt_for_session_selection(&entries)?;
    let options = RunOptions {
        executor: selected.executor,
        prompt: "Continue from this session.".to_string(),
        cwd: PathBuf::from(selected.cwd),
        session_id: Some(selected.session_id),
        resume: true,
        ..RunOptions::default()
    };
    run_with_default_sink(options).await.map(Some)
}

pub(super) fn latest_session() -> Result<String, CliError> {
    latest_session_id().ok_or_else(|| CliError::Arg("no saved sessions found".to_string()))
}

pub(super) fn show_session(session_id: &str) -> Result<String, CliError> {
    let path = session_metadata_path(session_id);
    let raw = fs::read_to_string(&path)
        .map_err(|_| CliError::Arg(format!("session not found: {session_id}")))?;
    let metadata = serde_json::from_str::<SessionMetadata>(&raw)
        .map_err(|_| CliError::Arg(format!("session metadata is invalid: {session_id}")))?;
    Ok(format!(
        "Session: {}\nExecutor: {:?}\nCreated: {}\nCwd: {}\nThread: {}",
        metadata.session_id,
        metadata.executor,
        metadata.created_at,
        metadata.cwd,
        metadata
            .thread_id
            .clone()
            .unwrap_or_else(|| "n/a".to_string())
    ))
}

pub(super) fn load_local_session_history(session_id: &str) -> Vec<Value> {
    let path = local_session_file(session_id);
    let Ok(raw) = fs::read_to_string(path) else {
        return Vec::new();
    };
    let Ok(history) = serde_json::from_str::<Vec<Value>>(&raw) else {
        return Vec::new();
    };
    history
}

pub(super) fn save_local_session_history(session_id: &str, history: &[Value]) {
    let path = local_session_file(session_id);
    if let Some(parent) = path.parent() {
        let _ = fs::create_dir_all(parent);
    }
    let keep_from = history.len().saturating_sub(20);
    let trimmed = &history[keep_from..];
    if let Ok(serialized) = serde_json::to_string(trimmed) {
        let _ = fs::write(path, serialized);
    }
}
