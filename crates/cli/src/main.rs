use aura_cli::{
    CliCommand, completion_script, latest_session, list_sessions, parse_cli_args,
    run_local_exec_from_env, run_orchestrator_command, run_session_list, run_with_default_sink,
    show_session, usage,
};

#[tokio::main]
async fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let command = match parse_cli_args(&args) {
        Ok(command) => command,
        Err(err) => {
            eprintln!("error: {err}");
            eprintln!();
            eprintln!("{}", usage());
            std::process::exit(2);
        }
    };

    match command {
        CliCommand::Help => {
            println!("{}", usage());
        }
        CliCommand::Completion { shell } => {
            println!("{}", completion_script(shell));
        }
        CliCommand::SessionList(args) => match run_session_list(args.clone()).await {
            Ok(Some(outcome)) => {
                if !outcome.success {
                    std::process::exit(outcome.exit_code.unwrap_or(1));
                }
            }
            Ok(None) => {
                println!("{}", list_sessions(args));
            }
            Err(err) => {
                eprintln!("error: {err}");
                std::process::exit(1);
            }
        },
        CliCommand::SessionShow { session_id } => match show_session(&session_id) {
            Ok(output) => println!("{output}"),
            Err(err) => {
                eprintln!("error: {err}");
                std::process::exit(1);
            }
        },
        CliCommand::SessionLatest => match latest_session() {
            Ok(session_id) => println!("{session_id}"),
            Err(err) => {
                eprintln!("error: {err}");
                std::process::exit(1);
            }
        },
        CliCommand::LocalExec => {
            std::process::exit(run_local_exec_from_env().await);
        }
        CliCommand::Orchestrator(command) => {
            let cwd = std::env::current_dir().unwrap_or_else(|_| ".".into());
            if let Err(err) = run_orchestrator_command(command, cwd).await {
                eprintln!("error: {err}");
                std::process::exit(1);
            }
        }
        CliCommand::Run(options) => match run_with_default_sink(*options).await {
            Ok(outcome) => {
                if !outcome.success {
                    std::process::exit(outcome.exit_code.unwrap_or(1));
                }
            }
            Err(err) => {
                eprintln!("error: {err}");
                std::process::exit(1);
            }
        },
    }
}
