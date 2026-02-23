use aura_contracts::ExecutorKind;

use crate::{
    AppendPrompt, CmdOverrides, CommandBackedExecutor, CommandExecutorConfig, ExecutorCapability,
    ExecutorProfileId, PromptInputMode,
};

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct CopilotOptions {
    pub append_prompt: AppendPrompt,
    pub model: Option<String>,
    pub allow_all_tools: bool,
    pub allow_tool: Option<String>,
    pub deny_tool: Option<String>,
    pub add_dir: Option<Vec<String>>,
    pub disable_mcp_server: Option<Vec<String>>,
    pub cmd_overrides: CmdOverrides,
}

pub fn copilot(options: CopilotOptions) -> CommandBackedExecutor {
    let mut params = vec![
        "--no-color".to_string(),
        "--log-level".to_string(),
        "debug".to_string(),
    ];
    if options.allow_all_tools {
        params.push("--allow-all-tools".to_string());
    }
    if let Some(model) = options.model {
        params.extend(["--model".to_string(), model]);
    }
    if let Some(tool) = options.allow_tool {
        params.extend(["--allow-tool".to_string(), tool]);
    }
    if let Some(tool) = options.deny_tool {
        params.extend(["--deny-tool".to_string(), tool]);
    }
    if let Some(dirs) = options.add_dir {
        for dir in dirs {
            params.extend(["--add-dir".to_string(), dir]);
        }
    }
    if let Some(servers) = options.disable_mcp_server {
        for server in servers {
            params.extend(["--disable-mcp-server".to_string(), server]);
        }
    }

    CommandBackedExecutor::new(CommandExecutorConfig {
        profile_id: ExecutorProfileId::new(ExecutorKind::Copilot),
        base_command: "npx -y @github/copilot@0.0.375".to_string(),
        default_params: params,
        append_prompt: options.append_prompt,
        prompt_input_mode: PromptInputMode::EnvVar,
        cmd_overrides: options.cmd_overrides,
        capabilities: vec![ExecutorCapability::SessionFork],
    })
}

pub fn copilot_default() -> CommandBackedExecutor {
    copilot(CopilotOptions::default())
}
