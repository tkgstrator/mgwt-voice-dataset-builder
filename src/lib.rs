#![forbid(unsafe_code)]
//! Guarded CLI foundation for a locally authorized voice dataset build.

pub mod cli;
pub mod error;
pub mod paths;

use cli::{Cli, Command};
use error::AppError;
use paths::{ValidatedAppPath, ValidatedOutputPath, validate_separation};

/// The successful result of a completed command. Later phases add details.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RunOutcome {
    Complete,
}

/// Validate command paths before handing work to later pipeline phases.
pub fn run(cli: Cli) -> Result<RunOutcome, AppError> {
    match cli.command {
        Command::Inspect { app, output } | Command::Build { app, output } => {
            validate_separation(&app, &output)?;
            let app = ValidatedAppPath::new(&app)?;
            let _output = ValidatedOutputPath::new(&output, &app)?;
            Err(AppError::processing(
                "command pipeline is not implemented yet",
            ))
        }
        Command::Verify { input } => {
            let _input = ValidatedAppPath::new(&input)?;
            Err(AppError::processing(
                "verification pipeline is not implemented yet",
            ))
        }
    }
}
