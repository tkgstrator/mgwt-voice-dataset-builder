#![forbid(unsafe_code)]

use clap::Parser;
use mgwt_voice_dataset_builder::{cli::Cli, error::ExitClass, run};

fn main() {
    let cli = Cli::parse();
    match run(cli) {
        Ok(_) => std::process::exit(ExitClass::Success.code()),
        Err(error) => {
            eprintln!("{error}");
            std::process::exit(error.exit_class().code());
        }
    }
}
