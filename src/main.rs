use anyhow::Result;
use clap::Parser;

use serial_tcp::cli::{Cli, Command};
use serial_tcp::{client, dashboard, list, logging, server};

fn main() -> Result<()> {
    let cli = Cli::parse();

    let log_file = if cli.no_log_file {
        None
    } else {
        Some(cli.log_file.as_path())
    };
    logging::init(cli.verbose, log_file)?;

    match cli.command {
        Command::List(args) => list::run(args),
        Command::Serve(args) => server::run(args),
        Command::Connect(args) => client::run(args),
        Command::Dashboard(args) => dashboard::run(args),
    }
}
