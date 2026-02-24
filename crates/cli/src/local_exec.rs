use super::*;

pub(super) fn model_options_for_executor(
    executor: &ExecutorKind,
    refresh_local_cache: bool,
    local_provider: Option<LocalProvider>,
) -> Vec<String> {
    match executor {
        ExecutorKind::Codex => vec![
            "gpt-5".to_string(),
            "gpt-5-mini".to_string(),
            "gpt-5-nano".to_string(),
        ],
        ExecutorKind::Claude => vec![
            "claude-sonnet-4-5".to_string(),
            "claude-opus-4-1".to_string(),
            "claude-3-7-sonnet-latest".to_string(),
        ],
        ExecutorKind::CursorAgent => vec![
            "gpt-5".to_string(),
            "claude-sonnet-4-5".to_string(),
            "gemini-2.5-pro".to_string(),
        ],
        ExecutorKind::Droid => vec![
            "gpt-5".to_string(),
            "gpt-5-mini".to_string(),
            "gemini-2.5-pro".to_string(),
        ],
        ExecutorKind::Gemini => vec!["gemini-2.5-pro".to_string(), "gemini-2.5-flash".to_string()],
        ExecutorKind::Opencode => vec![
            "gpt-5".to_string(),
            "claude-sonnet-4-5".to_string(),
            "gemini-2.5-pro".to_string(),
        ],
        ExecutorKind::Local => discover_local_model_options(refresh_local_cache, local_provider),
        ExecutorKind::QwenCode => vec!["qwen3-coder-plus".to_string(), "qwen3-coder".to_string()],
        ExecutorKind::Copilot => vec![
            "gpt-4.1".to_string(),
            "claude-sonnet-4-5".to_string(),
            "o3".to_string(),
        ],
        ExecutorKind::Amp => Vec::new(),
        ExecutorKind::Custom(_) => Vec::new(),
    }
}

pub(super) fn discover_local_model_options(
    refresh_cache: bool,
    local_provider: Option<LocalProvider>,
) -> Vec<String> {
    let sources = match local_provider {
        Some(LocalProvider::Ollama) => vec!["ollama"],
        Some(LocalProvider::LmStudio) => vec!["lmstudio"],
        Some(LocalProvider::Custom) => Vec::new(),
        None => vec!["ollama", "lmstudio"],
    };

    if sources.is_empty() {
        return Vec::new();
    }

    if !refresh_cache {
        if let Some(cached) = load_model_cache(&sources) {
            return cached;
        }
    }

    let mut options = Vec::new();
    let mut seen = HashSet::new();

    if sources.contains(&"ollama") {
        for model in discover_ollama_models() {
            let value = format!("ollama/{model}");
            if seen.insert(value.clone()) {
                options.push(value);
            }
        }
    }

    if sources.contains(&"lmstudio") {
        for model in discover_lmstudio_models() {
            let value = format!("lmstudio/{model}");
            if seen.insert(value.clone()) {
                options.push(value);
            }
        }
    }

    if !options.is_empty() {
        write_model_cache(&options, &sources);
    }

    options
}

fn discover_ollama_models() -> Vec<String> {
    let output = read_command_output("ollama", &["list"])
        .or_else(|| read_command_output("ollama", &["ls"]))
        .unwrap_or_default();
    parse_ollama_models(&output)
}

fn discover_lmstudio_models() -> Vec<String> {
    let output = read_command_output(
        "curl",
        &[
            "--silent",
            "--show-error",
            "--fail",
            "--max-time",
            "1",
            "http://127.0.0.1:1234/v1/models",
        ],
    )
    .unwrap_or_default();
    parse_lmstudio_models(&output)
}

fn read_command_output(program: &str, args: &[&str]) -> Option<String> {
    cmd(program, args).stderr_null().read().ok()
}

pub(super) fn parse_ollama_models(raw: &str) -> Vec<String> {
    let mut models = Vec::new();
    let mut seen = HashSet::new();

    for line in raw.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }

        if trimmed.starts_with('{')
            && let Ok(value) = serde_json::from_str::<Value>(trimmed)
            && let Some(name) = value
                .get("name")
                .or_else(|| value.get("model"))
                .and_then(Value::as_str)
                .map(str::trim)
                .filter(|name| !name.is_empty())
        {
            let candidate = name.to_string();
            if seen.insert(candidate.clone()) {
                models.push(candidate);
            }
            continue;
        }

        let first = trimmed.split_whitespace().next().unwrap_or_default();
        if first.is_empty() || first.eq_ignore_ascii_case("name") {
            continue;
        }
        let candidate = first.trim().to_string();
        if seen.insert(candidate.clone()) {
            models.push(candidate);
        }
    }

    models
}

pub(super) fn parse_lmstudio_models(raw: &str) -> Vec<String> {
    let Ok(value) = serde_json::from_str::<Value>(raw) else {
        return Vec::new();
    };
    let Some(data) = value.get("data").and_then(Value::as_array) else {
        return Vec::new();
    };

    let mut models = Vec::new();
    let mut seen = HashSet::new();
    for item in data {
        let Some(model_id) = item
            .get("id")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|id| !id.is_empty())
        else {
            continue;
        };
        let candidate = model_id.to_string();
        if seen.insert(candidate.clone()) {
            models.push(candidate);
        }
    }
    models
}

pub(super) fn resolve_local_model_and_endpoint(
    model: Option<&str>,
    openai_endpoint: Option<&str>,
) -> (Option<String>, Option<String>) {
    let explicit_endpoint = openai_endpoint
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_string);
    let selected_model = model
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_string);

    let Some(selected_model) = selected_model else {
        return (None, explicit_endpoint);
    };

    let (normalized_model, inferred_endpoint) =
        if let Some(model_name) = selected_model.strip_prefix("ollama/") {
            (
                Some(model_name.to_string()),
                Some("http://127.0.0.1:11434/v1".to_string()),
            )
        } else if let Some(model_name) = selected_model.strip_prefix("lmstudio/") {
            (
                Some(model_name.to_string()),
                Some("http://127.0.0.1:1234".to_string()),
            )
        } else {
            (Some(selected_model), None)
        };

    (normalized_model, explicit_endpoint.or(inferred_endpoint))
}

pub(super) async fn run_local_exec_from_env() -> i32 {
    let prompt = std::env::var("AURA_PROMPT").unwrap_or_default();
    let prompt = prompt.trim().to_string();
    if prompt.is_empty() {
        eprintln!("local executor: empty prompt");
        return 1;
    }

    let model = std::env::var("AURA_LOCAL_MODEL").unwrap_or_default();
    let model = model.trim().to_string();
    if model.is_empty() {
        eprintln!("local executor: model is required; pass --model");
        return 1;
    }

    let base_url = normalize_openai_base_url(
        std::env::var("OPENAI_BASE_URL")
            .ok()
            .or_else(|| std::env::var("OPENAI_API_BASE").ok()),
    );
    let api_key = std::env::var("OPENAI_API_KEY").unwrap_or_else(|_| "local".to_string());
    let session_id = std::env::var("AURA_SESSION_ID")
        .ok()
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty());

    let mut history = match &session_id {
        Some(id) => session_store::load_local_session_history(id),
        None => Vec::new(),
    };
    history.push(json!({
        "role": "user",
        "content": prompt
    }));

    let payload = json!({
        "model": model,
        "messages": history,
        "stream": true
    });

    let client = match reqwest::Client::builder()
        .timeout(Duration::from_secs(300))
        .build()
    {
        Ok(client) => client,
        Err(error) => {
            eprintln!("local executor: failed to build HTTP client: {error}");
            return 1;
        }
    };

    let response = match client
        .post(openai_chat_completions_url(&base_url))
        .bearer_auth(api_key)
        .json(&payload)
        .send()
        .await
    {
        Ok(response) => response,
        Err(error) => {
            eprintln!("local executor: request failed: {error}");
            return 1;
        }
    };

    let status = response.status();
    if !status.is_success() {
        let raw_body = match response.text().await {
            Ok(body) => body,
            Err(error) => {
                eprintln!("local executor: failed to read response body: {error}");
                return 1;
            }
        };
        eprintln!(
            "local executor: request failed ({status}): {}",
            summarize_text(&raw_body, 400)
        );
        return 1;
    }

    let content_type = response
        .headers()
        .get(reqwest::header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .unwrap_or("")
        .to_ascii_lowercase();

    let content = if content_type.contains("application/json") {
        let raw_body = match response.text().await {
            Ok(body) => body,
            Err(error) => {
                eprintln!("local executor: failed to read response body: {error}");
                return 1;
            }
        };
        let parsed: Value = match serde_json::from_str(&raw_body) {
            Ok(value) => value,
            Err(error) => {
                eprintln!("local executor: invalid JSON response: {error}");
                eprintln!("{}", summarize_text(&raw_body, 400));
                return 1;
            }
        };
        match extract_openai_message_content(&parsed) {
            Some(content) if !content.trim().is_empty() => {
                for line in content.lines() {
                    println!("agent: {line}");
                }
                let _ = io::stdout().flush();
                content
            }
            _ => {
                eprintln!("local executor: empty response content");
                return 1;
            }
        }
    } else {
        let mut full_content = String::new();
        let mut pending_line = String::new();
        let mut pending_sse = String::new();

        let mut response = response;
        loop {
            let next_chunk = match response.chunk().await {
                Ok(value) => value,
                Err(error) => {
                    eprintln!("local executor: failed to read response stream: {error}");
                    return 1;
                }
            };
            let Some(chunk) = next_chunk else {
                break;
            };

            pending_sse.push_str(&String::from_utf8_lossy(&chunk));
            while let Some(newline_index) = pending_sse.find('\n') {
                let mut line = pending_sse[..newline_index].to_string();
                pending_sse.drain(..=newline_index);
                line = line.trim_end_matches('\r').to_string();
                consume_openai_stream_line(&line, &mut full_content, &mut pending_line);
            }
        }

        if !pending_sse.trim().is_empty() {
            consume_openai_stream_line(
                pending_sse.trim_end_matches('\r'),
                &mut full_content,
                &mut pending_line,
            );
        }
        if !pending_line.trim().is_empty() {
            println!("agent: {}", pending_line.trim_end_matches('\r'));
            let _ = io::stdout().flush();
        }
        if full_content.trim().is_empty() {
            eprintln!("local executor: empty response content");
            return 1;
        }
        full_content
    };

    if let Some(id) = session_id {
        history.push(json!({
            "role": "assistant",
            "content": content
        }));
        session_store::save_local_session_history(&id, &history);
    }

    0
}

fn normalize_openai_base_url(value: Option<String>) -> String {
    let value = value.unwrap_or_else(|| "http://127.0.0.1:1234".to_string());
    let trimmed = value.trim().trim_end_matches('/');
    if trimmed.is_empty() {
        "http://127.0.0.1:1234".to_string()
    } else {
        trimmed.to_string()
    }
}

pub(super) fn openai_chat_completions_url(base_url: &str) -> String {
    if base_url.ends_with("/v1") {
        format!("{base_url}/chat/completions")
    } else {
        format!("{base_url}/v1/chat/completions")
    }
}

pub(super) fn extract_openai_message_content(value: &Value) -> Option<String> {
    let first = value.get("choices")?.as_array()?.first()?;
    let message = first.get("message")?;
    extract_openai_text_content(message.get("content")?)
}

pub(super) fn extract_openai_delta_content(value: &Value) -> Option<String> {
    let first = value.get("choices")?.as_array()?.first()?;
    let delta = first.get("delta")?;
    extract_openai_text_content(delta.get("content")?)
}

fn extract_openai_text_content(content: &Value) -> Option<String> {
    match content {
        Value::String(text) => Some(text.to_string()),
        Value::Array(parts) => {
            let text_parts = parts
                .iter()
                .filter_map(|part| {
                    part.get("text")
                        .and_then(Value::as_str)
                        .map(ToString::to_string)
                })
                .filter(|text| !text.trim().is_empty())
                .collect::<Vec<_>>();
            if text_parts.is_empty() {
                None
            } else {
                Some(text_parts.join("\n"))
            }
        }
        _ => None,
    }
}

pub(super) fn consume_openai_stream_line(
    raw_line: &str,
    full_content: &mut String,
    pending_line: &mut String,
) {
    let line = raw_line.trim();
    if line.is_empty() {
        return;
    }
    let payload = if let Some(value) = line.strip_prefix("data:") {
        value.trim()
    } else {
        line
    };
    if payload.is_empty() || payload == "[DONE]" {
        return;
    }
    let Ok(value) = serde_json::from_str::<Value>(payload) else {
        return;
    };
    let Some(delta) =
        extract_openai_delta_content(&value).or_else(|| extract_openai_message_content(&value))
    else {
        return;
    };
    if delta.trim().is_empty() {
        return;
    }

    full_content.push_str(&delta);
    pending_line.push_str(&delta);

    while let Some(newline_index) = pending_line.find('\n') {
        let line = pending_line[..newline_index].trim_end_matches('\r');
        if !line.is_empty() {
            println!("agent: {line}");
            let _ = io::stdout().flush();
        }
        pending_line.drain(..=newline_index);
    }
}
