#![forbid(unsafe_code)]
//! Guarded CLI foundation for a locally authorized voice dataset build.

pub mod audio;
pub mod cli;
pub mod config;
pub mod error;
pub mod hash;
pub mod inventory;
pub mod matching;
pub mod model;
pub mod naninovel;
pub mod normalize;
pub mod paths;
pub mod speaker;
pub mod unity;

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
        Command::Inspect { app, output } => {
            validate_separation(&app, &output)?;
            let app = ValidatedAppPath::new(&app)?;
            let output = ValidatedOutputPath::new(&output, &app)?;
            let approval = config::checked_in_approval()?;
            let inspection = inventory::inspect_entries(&app, &approval.manifest.entries())?;
            inventory::write_inspection_reports(&output, &inspection)?;
            Ok(RunOutcome::Complete)
        }
        Command::Build { app, output } => {
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
