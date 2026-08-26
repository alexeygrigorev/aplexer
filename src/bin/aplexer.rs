use anyhow::Result;
use clap::{Parser, Subcommand};
use uuid::Uuid;

#[derive(Parser)]
#[command(name = "aplexer", version, about = "Independent aplexer PTY worker")]
struct Args {
    #[command(subcommand)]
    command: Command,
}
#[derive(Subcommand)]
enum Command {
    Worker {
        #[arg(long)]
        id: Uuid,
    },
}
fn main() -> Result<()> {
    let args = Args::parse();
    match args.command {
        Command::Worker { id } => aplexer::worker::run_worker(id),
    }
}
