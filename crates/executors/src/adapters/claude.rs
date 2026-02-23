use aura_contracts::ExecutorKind;

use crate::{
    AppendPrompt, CmdOverrides, CommandBackedExecutor, CommandExecutorConfig, ExecutorCapability,
    ExecutorProfileId, PromptInputMode,
};

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ClaudeOptions {
    pub append_prompt: AppendPrompt,
    pub model: Option<String>,
    pub plan: bool,
    pub approvals: bool,
    pub dangerously_skip_permissions: bool,
    pub claude_code_router: bool,
    pub cmd_overrides: CmdOverrides,
}

pub fn claude(options: ClaudeOptions) -> CommandBackedExecutor {
    let base = if options.claude_code_router {
        "npx -y @musistudio/claude-code-router@1.0.66 code"
    } else {
        "npx -y @anthropic-ai/claude-code@2.1.12"
    };

    let mut params = vec![
        "-p".to_string(),
        "--verbose".to_string(),
        "--output-format=stream-json".to_string(),
        "--include-partial-messages".to_string(),
        "--disallowedTools=AskUserQuestion".to_string(),
    ];

    if options.plan || options.approvals {
        params.push("--permission-prompt-tool=stdio".to_string());
        params.push("--permission-mode=bypassPermissions".to_string());
    }
    if options.dangerously_skip_permissions {
        params.push("--dangerously-skip-permissions".to_string());
    }
    if let Some(model) = options.model {
        params.extend(["--model".to_string(), model]);
    }

    CommandBackedExecutor::new(CommandExecutorConfig {
        profile_id: ExecutorProfileId::new(ExecutorKind::Claude),
        base_command: base.to_string(),
        default_params: params,
        append_prompt: options.append_prompt,
        prompt_input_mode: PromptInputMode::Arg,
        cmd_overrides: options.cmd_overrides,
        capabilities: vec![ExecutorCapability::SessionFork],
    })
}

pub fn claude_default() -> CommandBackedExecutor {
    claude(ClaudeOptions::default())
}
