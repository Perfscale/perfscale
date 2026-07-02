mod cli;
mod commands;
mod error;
mod update;

use clap::Parser;

use cli::{Cli, Commands};

#[tokio::main]
async fn main() {
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .init();

    let cli = Cli::parse();

    // `self-update` manages versions explicitly — no passive notice there.
    let notify_updates = !matches!(cli.command, Commands::SelfUpdate(_));

    let result = match cli.command {
        Commands::Run(args) => commands::run::run(args).await,
        Commands::Serve(args) => commands::serve::serve(args).await,
        Commands::Lint(args) => commands::lint::run(args).await,
        Commands::SelfUpdate(args) => commands::self_update::self_update(args).await,
    };

    if notify_updates {
        update::maybe_print_update_notice().await;
    }

    if let Err(e) = result {
        eprintln!("error: {e}");
        std::process::exit(1);
    }
}
