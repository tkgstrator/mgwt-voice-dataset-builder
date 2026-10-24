//! Command-line argument definitions.

use clap::{Parser, Subcommand};
use std::path::PathBuf;

#[derive(Debug, Parser)]
#[command(
    name = "mgwt-voice-dataset-builder",
    about = "Build a local voice dataset from authorized input"
)]
pub struct Cli {
    #[command(subcommand)]
    pub command: Command,
}

#[derive(Debug, Subcommand)]
pub enum Command {
    /// Inspect approved consumed inputs and write candidate inventory and diagnostics.
    Inspect {
        #[arg(long)]
        app: PathBuf,
        #[arg(long)]
        output: PathBuf,
    },
    /// Build the dataset from approved configuration.
    Build {
        #[arg(long)]
        app: PathBuf,
        #[arg(long)]
        output: PathBuf,
        #[arg(long, default_value_t = 500, value_parser = clap::value_parser!(u64).range(1..=1024))]
        shard_audio_mib: u64,
    },
    /// Verify a completed local dataset.
    Verify {
        #[arg(long)]
        input: PathBuf,
    },
}
