//! The `fbk` CLI for running coding agents in Firebrick microVM sandboxes.

/// Runs the `fbk` command line.
fn main() -> anyhow::Result<()> {
    firebrick_cli::app::main()
}
