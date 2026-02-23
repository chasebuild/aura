use aura_contracts::ExecutorKind;

use crate::{
    AppendPrompt, CmdOverrides, CommandBackedExecutor, CommandExecutorConfig, ExecutorCapability,
    ExecutorProfileId, PromptInputMode,
};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OpencodeOptions {
    pub append_prompt: AppendPrompt,
    pub model: Option<String>,
    pub variant: Option<String>,
    pub mode: Option<String>,
    pub auto_approve: bool,
    pub cmd_overrides: CmdOverrides,
}

impl Default for OpencodeOptions {
    fn default() -> Self {
        Self {
            append_prompt: AppendPrompt::default(),
            model: None,
            variant: None,
            mode: None,
            auto_approve: true,
            cmd_overrides: CmdOverrides::default(),
        }
    }
}

pub fn opencode(options: OpencodeOptions) -> CommandBackedExecutor {
    let mut params = vec![
        "serve".to_string(),
        "--hostname".to_string(),
        "127.0.0.1".to_string(),
        "--port".to_string(),
        "0".to_string(),
    ];
    if let Some(model) = options.model {
        params.extend(["--model".to_string(), model]);
    }
    if let Some(variant) = options.variant {
        params.extend(["--variant".to_string(), variant]);
    }
    if let Some(mode) = options.mode {
        params.extend(["--agent".to_string(), mode]);
    }

    let mut cmd_overrides = options.cmd_overrides;
    let mut env = cmd_overrides.env.unwrap_or_default();
    if !env.contains_key("OPENCODE_PERMISSION") {
        if options.auto_approve {
            env.insert(
                "OPENCODE_PERMISSION".to_string(),
                "{\"question\":\"deny\"}".to_string(),
            );
        } else {
            env.insert(
                "OPENCODE_PERMISSION".to_string(),
                "{\"edit\":\"ask\",\"bash\":\"ask\",\"webfetch\":\"ask\",\"doom_loop\":\"ask\",\"external_directory\":\"ask\",\"question\":\"deny\"}".to_string(),
            );
        }
    }
    cmd_overrides.env = Some(env);

    CommandBackedExecutor::new(CommandExecutorConfig {
        profile_id: ExecutorProfileId::new(ExecutorKind::Opencode),
        base_command: "npx -y opencode-ai@1.1.25".to_string(),
        default_params: params,
        append_prompt: options.append_prompt,
        prompt_input_mode: PromptInputMode::EnvVar,
        cmd_overrides,
        capabilities: vec![ExecutorCapability::SessionFork],
    })
}

pub fn opencode_default() -> CommandBackedExecutor {
    opencode(OpencodeOptions::default())
}
