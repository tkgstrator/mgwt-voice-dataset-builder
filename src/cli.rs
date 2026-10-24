//! Command-line argument definitions.

use clap::{Parser, Subcommand};
use std::{io, num::NonZeroUsize, path::PathBuf, thread};

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
        /// Maximum voice bundles staged concurrently (defaults to available logical CPUs).
        #[arg(long)]
        bundle_workers: Option<NonZeroUsize>,
    },
    /// Verify a completed local dataset.
    Verify {
        #[arg(long)]
        input: PathBuf,
    },
}

/// Resolve the requested limit before handing it to the build pipeline.
pub fn bundle_worker_count(requested: Option<NonZeroUsize>) -> usize {
    bundle_worker_count_with(requested, thread::available_parallelism)
}

fn bundle_worker_count_with<F>(requested: Option<NonZeroUsize>, available: F) -> usize
where
    F: FnOnce() -> io::Result<NonZeroUsize>,
{
    requested.map_or_else(
        || available().map(NonZeroUsize::get).unwrap_or(1),
        NonZeroUsize::get,
    )
}

#[cfg(test)]
mod tests {
    use super::bundle_worker_count_with;
    use std::{io, num::NonZeroUsize};

    #[test]
    fn omitted_worker_count_uses_reported_logical_parallelism() {
        assert_eq!(
            bundle_worker_count_with(None, || Ok(NonZeroUsize::new(12).unwrap())),
            12
        );
    }

    #[test]
    fn failed_parallelism_probe_falls_back_to_one() {
        assert_eq!(
            bundle_worker_count_with(None, || Err(io::Error::other("unavailable"))),
            1
        );
    }

    #[test]
    fn explicit_worker_count_does_not_probe_parallelism() {
        assert_eq!(
            bundle_worker_count_with(Some(NonZeroUsize::new(6).unwrap()), || panic!(
                "parallelism should not be probed"
            )),
            6
        );
    }
}
