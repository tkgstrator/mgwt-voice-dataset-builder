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
    /// Inspect an authorized app and write candidate metadata.
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
    },
    /// Verify a completed local dataset.
    Verify {
        #[arg(long)]
        input: PathBuf,
    },
}
