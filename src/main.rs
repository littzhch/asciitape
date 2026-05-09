mod cast;
mod terminal;
mod tui;

use std::path::PathBuf;

use anyhow::Result;
use clap::Parser;

#[derive(Parser)]
#[command(version, about = "TUI asciinema cast player")]
struct Args {
    /// Path to an asciinema v2 or v3 .cast recording.
    cast: PathBuf,

    /// Start paused instead of playing immediately.
    #[arg(long)]
    paused: bool,

    /// Start in fullscreen mode, hiding borders, progress, and controls.
    #[arg(long)]
    fullscreen: bool,
}

fn main() -> Result<()> {
    let args = Args::parse();
    let cast = cast::load_cast(&args.cast)?;
    tui::run(cast, args.paused, args.fullscreen)
}
