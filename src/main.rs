//! Thin binary entry point: parse CLI args, then hand off to the session + REPL
//! (or script mode). All real logic lives in the `cli_ent` library so integration
//! tests can drive it without a terminal.

use anyhow::Result;
use clap::Parser;

use cli_ent::cli::Args;

fn main() -> Result<()> {
    let args = Args::parse();

    // Milestone 0 scaffold: argument parsing is wired; the session task, REPL,
    // and script driver are filled in across milestones 1–10.
    eprintln!("cli-ent {} — scaffold", env!("CARGO_PKG_VERSION"));
    eprintln!("parsed args: {args:?}");

    Ok(())
}
