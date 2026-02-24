use std::process::Command;

fn aura_bin() -> &'static str {
    env!("CARGO_BIN_EXE_aura")
}

#[test]
fn run_custom_executor_streams_stdout_and_stderr() {
    let output = Command::new(aura_bin())
        .args([
            "run",
            "--executor",
            "custom",
            "--model",
            "dummy-model",
            "--openai-endpoint",
            "http://127.0.0.1:11434/v1",
            "--prompt",
            "ignored",
            "--base-command",
            "sh",
            "--param",
            "-c",
            "--param",
            "echo out_line; echo err_line 1>&2",
            "--no-tui",
        ])
        .output()
        .expect("execute aura");

    assert!(
        output.status.success(),
        "status was {:?}",
        output.status.code()
    );

    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("[stdout] out_line"), "stdout was: {stdout}");
    assert!(stdout.contains("[stderr] err_line"), "stdout was: {stdout}");
    assert!(
        stdout.contains("[aura] process exited with Some(0)"),
        "stdout was: {stdout}"
    );
}

#[test]
fn run_custom_executor_propagates_failure_exit_code() {
    let output = Command::new(aura_bin())
        .args([
            "run",
            "--executor",
            "custom",
            "--model",
            "dummy-model",
            "--openai-endpoint",
            "http://127.0.0.1:11434/v1",
            "--prompt",
            "ignored",
            "--base-command",
            "sh",
            "--param",
            "-c",
            "--param",
            "echo failing 1>&2; exit 7",
            "--no-tui",
        ])
        .output()
        .expect("execute aura");

    assert_eq!(output.status.code(), Some(7));

    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("[stderr] failing"), "stdout was: {stdout}");
    assert!(
        stdout.contains("[aura] process exited with Some(7)"),
        "stdout was: {stdout}"
    );
}

#[test]
fn orchestrator_submit_and_tick_commands_work() {
    let home = tempfile::tempdir().expect("tmp home");

    let submit_output = Command::new(aura_bin())
        .args([
            "orchestrator",
            "submit",
            "--title",
            "Ship feature",
            "--intent",
            "Fix backend bug",
        ])
        .env("HOME", home.path())
        .output()
        .expect("submit");
    assert!(
        submit_output.status.success(),
        "submit status {:?}",
        submit_output.status.code()
    );

    let tick_output = Command::new(aura_bin())
        .args(["orchestrator", "tick"])
        .env("HOME", home.path())
        .output()
        .expect("tick");
    assert!(
        tick_output.status.success(),
        "tick status {:?}",
        tick_output.status.code()
    );
    let stdout = String::from_utf8_lossy(&tick_output.stdout);
    assert!(stdout.contains("\"processed\""), "stdout was: {stdout}");
}

#[test]
fn orchestrator_retry_missing_task_returns_error() {
    let home = tempfile::tempdir().expect("tmp home");
    let output = Command::new(aura_bin())
        .args([
            "orchestrator",
            "retry",
            "--task-id",
            "11111111-1111-1111-1111-111111111111",
        ])
        .env("HOME", home.path())
        .output()
        .expect("retry");

    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("orch_task"),
        "stderr did not include not found marker: {stderr}"
    );
}
