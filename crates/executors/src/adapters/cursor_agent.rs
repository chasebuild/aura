use aura_contracts::ExecutorKind;

use crate::{
    AppendPrompt, CmdOverrides, CommandBackedExecutor, CommandExecutorConfig, ExecutorCapability,
    ExecutorProfileId, PromptInputMode,
};

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct CursorAgentOptions {
    pub append_prompt: AppendPrompt,
    pub force: bool,
    pub trust: bool,
    pub model: Option<String>,
    pub cmd_overrides: CmdOverrides,
}

pub fn cursor_agent(options: CursorAgentOptions) -> CommandBackedExecutor {
    let mut params = vec!["-p".to_string(), "--output-format=stream-json".to_string()];
    if options.force {
        params.push("--force".to_string());
    }
    if options.trust {
        params.push("--trust".to_string());
    }
    if let Some(model) = options.model {
        params.extend(["--model".to_string(), model]);
    }

    CommandBackedExecutor::new(CommandExecutorConfig {
        profile_id: ExecutorProfileId::new(ExecutorKind::CursorAgent),
        base_command: "cursor-agent".to_string(),
        default_params: params,
        append_prompt: options.append_prompt,
        prompt_input_mode: PromptInputMode::Stdin,
        cmd_overrides: options.cmd_overrides,
        capabilities: vec![ExecutorCapability::SetupHelper],
    })
}

pub fn cursor_default() -> CommandBackedExecutor {
    cursor_agent(CursorAgentOptions::default())
}
