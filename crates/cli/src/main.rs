use aura_cli::{CliCommand, parse_cli_args, run_with_default_sink, usage};

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
        CliCommand::Run(options) => match run_with_default_sink(options).await {
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
