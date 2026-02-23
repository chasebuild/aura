use aura_contracts::ExecutorKind;

use crate::{
    AppendPrompt, CmdOverrides, CommandBackedExecutor, CommandExecutorConfig, ExecutorCapability,
    ExecutorProfileId, PromptInputMode,
};

#[derive(Debug, Clone, PartialEq, Eq)]
#[derive(Default)]
pub struct LocalOptions {
    pub append_prompt: AppendPrompt,
    pub model: Option<String>,
    pub openai_endpoint: Option<String>,
    pub cmd_overrides: CmdOverrides,
}


pub fn local(options: LocalOptions) -> CommandBackedExecutor {
    let params = vec!["local-exec".to_string()];

    let mut cmd_overrides = options.cmd_overrides;
    let mut env = cmd_overrides.env.unwrap_or_default();
    if let Some(model) = options
        .model
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
    {
        env.insert("AURA_LOCAL_MODEL".to_string(), model.to_string());
    }
    if let Some(endpoint) = options
        .openai_endpoint
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
    {
        env.insert("OPENAI_BASE_URL".to_string(), endpoint.to_string());
        env.insert("OPENAI_API_BASE".to_string(), endpoint.to_string());
        env.entry("OPENAI_API_KEY".to_string())
            .or_insert_with(|| "local".to_string());
    }
    cmd_overrides.env = Some(env);

    CommandBackedExecutor::new(CommandExecutorConfig {
        profile_id: ExecutorProfileId::new(ExecutorKind::Local),
        base_command: "aura".to_string(),
        default_params: params,
        append_prompt: options.append_prompt,
        prompt_input_mode: PromptInputMode::EnvVar,
        cmd_overrides,
        capabilities: vec![ExecutorCapability::SessionFork],
    })
}

pub fn local_default() -> CommandBackedExecutor {
    local(LocalOptions::default())
}
