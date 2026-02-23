use aura_contracts::ExecutorKind;

use crate::{
    AppendPrompt, CmdOverrides, CommandBackedExecutor, CommandExecutorConfig, ExecutorCapability,
    ExecutorProfileId, PromptInputMode,
};

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct AmpOptions {
    pub append_prompt: AppendPrompt,
    pub dangerously_allow_all: bool,
    pub cmd_overrides: CmdOverrides,
}

pub fn amp(options: AmpOptions) -> CommandBackedExecutor {
    let mut params = vec!["--execute".to_string(), "--stream-json".to_string()];
    if options.dangerously_allow_all {
        params.push("--dangerously-allow-all".to_string());
    }

    CommandBackedExecutor::new(CommandExecutorConfig {
        profile_id: ExecutorProfileId::new(ExecutorKind::Amp),
        base_command: "npx -y @sourcegraph/amp@0.0.1764777697-g907e30".to_string(),
        default_params: params,
        append_prompt: options.append_prompt,
        prompt_input_mode: PromptInputMode::EnvVar,
        cmd_overrides: options.cmd_overrides,
        capabilities: vec![ExecutorCapability::SessionFork],
    })
}

pub fn amp_default() -> CommandBackedExecutor {
    amp(AmpOptions::default())
}
