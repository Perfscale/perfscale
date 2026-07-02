mod cli;
mod commands;
mod error;

use clap::Parser;

use cli::{Cli, Commands};

#[tokio::main]
async fn main() {
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .init();

    let cli = Cli::parse();

    let result = match cli.command {
        Commands::Run(args) => commands::run::run(args).await,
        Commands::Serve(args) => commands::serve::serve(args).await,
        Commands::Bench(args) => commands::bench::bench(args).await,
        Commands::Lint(args) => commands::lint::run(args).await,
    };

    if let Err(e) = result {
        eprintln!("error: {e}");
        std::process::exit(1);
    }
}
