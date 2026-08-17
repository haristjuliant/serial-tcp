use anyhow::Result;
use clap::Parser;

use serial_tcp::cli::{Cli, Command};
use serial_tcp::{client, dashboard, list, server};

fn main() -> Result<()> {
    let cli = Cli::parse();

    let level = if cli.verbose { "debug" } else { "info" };
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or(level))
        .format_timestamp_millis()
        .init();

    match cli.command {
        Command::List(args) => list::run(args),
        Command::Serve(args) => server::run(args),
        Command::Connect(args) => client::run(args),
        Command::Dashboard(args) => dashboard::run(args),
    }
}
