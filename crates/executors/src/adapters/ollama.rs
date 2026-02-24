use aura_contracts::ExecutorKind;

use crate::{
    AppendPrompt, CmdOverrides, CommandBackedExecutor, CommandExecutorConfig, ExecutorCapability,
    ExecutorProfileId, PromptInputMode,
};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OllamaOptions {
    pub append_prompt: AppendPrompt,
    pub model: String,
    pub cmd_overrides: CmdOverrides,
}

impl Default for OllamaOptions {
    fn default() -> Self {
        Self {
            append_prompt: AppendPrompt::default(),
            model: "llama3".to_string(),
            cmd_overrides: CmdOverrides::default(),
        }
    }
}

pub fn ollama_cli(options: OllamaOptions) -> CommandBackedExecutor {
    let params = vec!["run".to_string(), options.model];

    CommandBackedExecutor::new(CommandExecutorConfig {
        profile_id: ExecutorProfileId::new(ExecutorKind::Local),
        base_command: "ollama".to_string(),
        default_params: params,
        append_prompt: options.append_prompt,
        prompt_input_mode: PromptInputMode::Arg,
        cmd_overrides: options.cmd_overrides,
        capabilities: vec![ExecutorCapability::SessionFork],
    })
}

pub fn ollama_default() -> CommandBackedExecutor {
    ollama_cli(OllamaOptions::default())
}
