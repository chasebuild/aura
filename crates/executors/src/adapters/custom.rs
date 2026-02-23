use crate::{
    AppendPrompt, CmdOverrides, CommandBackedExecutor, CommandExecutorConfig, ExecutorCapability,
    ExecutorProfileId,
};

pub fn custom_command(
    profile_id: ExecutorProfileId,
    base_command: impl Into<String>,
    default_params: Vec<String>,
    cmd_overrides: CmdOverrides,
    capabilities: Vec<ExecutorCapability>,
) -> CommandBackedExecutor {
    CommandBackedExecutor::new(CommandExecutorConfig {
        profile_id,
        base_command: base_command.into(),
        default_params,
        append_prompt: AppendPrompt::default(),
        cmd_overrides,
        capabilities,
    })
}
