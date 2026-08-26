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
        /// Initial PTY row/col count to open the workload's terminal at,
        /// already reserved-row-adjusted by the spawning client (see
        /// `reserved_rows` in src/bin/a.rs). Passed only when the client is
        /// about to attach immediately (`a start --attach` / `a -`), so the
        /// workload sees its real, final terminal size from the very first
        /// moment it runs instead of starting at a stale default and being
        /// resized out from under it a moment later -- see the doc comment
        /// on `run_worker`'s `initial_size` parameter for why that race
        /// matters. Both flags must be given together or not at all.
        #[arg(long)]
        rows: Option<u16>,
        #[arg(long)]
        cols: Option<u16>,
    },
}
fn main() -> Result<()> {
    let args = Args::parse();
    match args.command {
        Command::Worker { id, rows, cols } => {
            let initial_size = rows.zip(cols);
            aplexer::worker::run_worker(id, initial_size)
        }
    }
}
