use aura_contracts::ExecutorKind;

use crate::{
    AppendPrompt, CmdOverrides, CommandBackedExecutor, CommandExecutorConfig, ExecutorCapability,
    ExecutorProfileId, PromptInputMode,
};

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct GeminiOptions {
    pub append_prompt: AppendPrompt,
    pub model: Option<String>,
    pub yolo: bool,
    pub cmd_overrides: CmdOverrides,
}

pub fn gemini(options: GeminiOptions) -> CommandBackedExecutor {
    let mut params = vec!["--output-format=stream-json".to_string()];
    if let Some(model) = options.model {
        params.extend(["--model".to_string(), model]);
    }
    if options.yolo {
        params.extend([
            "--yolo".to_string(),
            "--allowed-tools".to_string(),
            "run_shell_command".to_string(),
        ]);
    }

    CommandBackedExecutor::new(CommandExecutorConfig {
        profile_id: ExecutorProfileId::new(ExecutorKind::Gemini),
        base_command: "npx -y @google/gemini-cli@0.23.0".to_string(),
        default_params: params,
        append_prompt: options.append_prompt,
        prompt_input_mode: PromptInputMode::Arg,
        cmd_overrides: options.cmd_overrides,
        capabilities: vec![ExecutorCapability::SessionFork],
    })
}

pub fn gemini_default() -> CommandBackedExecutor {
    gemini(GeminiOptions::default())
}
