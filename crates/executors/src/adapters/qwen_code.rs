use aura_contracts::ExecutorKind;

use crate::{
    AppendPrompt, CmdOverrides, CommandBackedExecutor, CommandExecutorConfig, ExecutorCapability,
    ExecutorProfileId, PromptInputMode,
};

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct QwenCodeOptions {
    pub append_prompt: AppendPrompt,
    pub yolo: bool,
    pub cmd_overrides: CmdOverrides,
}

pub fn qwen_code(options: QwenCodeOptions) -> CommandBackedExecutor {
    let mut params = vec!["--experimental-acp".to_string()];
    if options.yolo {
        params.push("--yolo".to_string());
    }

    CommandBackedExecutor::new(CommandExecutorConfig {
        profile_id: ExecutorProfileId::new(ExecutorKind::QwenCode),
        base_command: "npx -y @qwen-code/qwen-code@0.2.1".to_string(),
        default_params: params,
        append_prompt: options.append_prompt,
        prompt_input_mode: PromptInputMode::EnvVar,
        cmd_overrides: options.cmd_overrides,
        capabilities: vec![ExecutorCapability::SessionFork],
    })
}

pub fn qwen_code_default() -> CommandBackedExecutor {
    qwen_code(QwenCodeOptions::default())
}
