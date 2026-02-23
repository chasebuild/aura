use aura_contracts::ExecutorKind;

use crate::{
    AppendPrompt, CmdOverrides, CommandBackedExecutor, CommandExecutorConfig, ExecutorCapability,
    ExecutorProfileId,
};

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct DroidOptions {
    pub append_prompt: AppendPrompt,
    pub autonomy: Option<String>,
    pub model: Option<String>,
    pub reasoning_effort: Option<String>,
    pub cmd_overrides: CmdOverrides,
}

pub fn droid(options: DroidOptions) -> CommandBackedExecutor {
    let mut params = vec![
        "exec".to_string(),
        "--output-format".to_string(),
        "stream-json".to_string(),
    ];
    match options.autonomy.as_deref() {
        Some("low") => params.extend(["--auto".to_string(), "low".to_string()]),
        Some("medium") => params.extend(["--auto".to_string(), "medium".to_string()]),
        Some("high") => params.extend(["--auto".to_string(), "high".to_string()]),
        Some("skip-permissions-unsafe") | None => {
            params.push("--skip-permissions-unsafe".to_string())
        }
        Some(_) => {}
    }
    if let Some(model) = options.model {
        params.extend(["--model".to_string(), model]);
    }
    if let Some(reasoning) = options.reasoning_effort {
        params.extend(["--reasoning-effort".to_string(), reasoning]);
    }

    CommandBackedExecutor::new(CommandExecutorConfig {
        profile_id: ExecutorProfileId::new(ExecutorKind::Droid),
        base_command: "droid".to_string(),
        default_params: params,
        append_prompt: options.append_prompt,
        cmd_overrides: options.cmd_overrides,
        capabilities: vec![ExecutorCapability::SessionFork],
    })
}

pub fn droid_default() -> CommandBackedExecutor {
    droid(DroidOptions::default())
}
