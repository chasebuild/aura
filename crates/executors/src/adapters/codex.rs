use aura_contracts::ExecutorKind;

use crate::{
    AppendPrompt, CmdOverrides, CommandBackedExecutor, CommandExecutorConfig, ExecutorCapability,
    ExecutorProfileId, PromptInputMode,
};

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct CodexOptions {
    pub append_prompt: AppendPrompt,
    pub model: Option<String>,
    pub sandbox: Option<String>,
    pub ask_for_approval: Option<String>,
    pub cmd_overrides: CmdOverrides,
}

pub fn codex(options: CodexOptions) -> CommandBackedExecutor {
    let mut params = vec![
        "exec".to_string(),
        "--json".to_string(),
        "--skip-git-repo-check".to_string(),
    ];
    if let Some(model) = options.model {
        params.extend(["--model".to_string(), model]);
    }
    if let Some(sandbox) = options.sandbox {
        params.extend(["--sandbox".to_string(), sandbox]);
    }
    if let Some(policy) = options.ask_for_approval {
        params.extend(["--ask-for-approval".to_string(), policy]);
    }

    CommandBackedExecutor::new(CommandExecutorConfig {
        profile_id: ExecutorProfileId::new(ExecutorKind::Codex),
        base_command: "codex".to_string(),
        default_params: params,
        append_prompt: options.append_prompt,
        prompt_input_mode: PromptInputMode::Arg,
        cmd_overrides: options.cmd_overrides,
        capabilities: vec![
            ExecutorCapability::SessionFork,
            ExecutorCapability::SetupHelper,
        ],
    })
}

pub fn codex_default() -> CommandBackedExecutor {
    codex(CodexOptions::default())
}
