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
