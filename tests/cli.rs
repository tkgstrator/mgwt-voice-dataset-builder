use clap::Parser;
use mgwt_voice_dataset_builder::cli::{Cli, Command as CliCommand};
use std::process::Command;

fn binary() -> Command {
    Command::new(env!("CARGO_BIN_EXE_mgwt-voice-dataset-builder"))
}

#[test]
fn help_lists_the_three_supported_commands() {
    let output = binary().arg("--help").output().unwrap();
    assert!(output.status.success());
    let help = String::from_utf8(output.stdout).unwrap();
    for command in ["inspect", "build", "verify"] {
        assert!(help.contains(command), "missing {command}: {help}");
    }
}

#[test]
fn missing_required_paths_use_validation_exit_code() {
    for args in [&["inspect"][..], &["build"][..], &["verify"][..]] {
        let output = binary().args(args).output().unwrap();
        assert_eq!(output.status.code(), Some(2), "{args:?}");
    }
}

#[test]
fn error_classes_have_stable_process_codes() {
    use mgwt_voice_dataset_builder::error::ExitClass;
    assert_eq!(ExitClass::Success.code(), 0);
    assert_eq!(ExitClass::Processing.code(), 1);
    assert_eq!(ExitClass::Validation.code(), 2);
}

#[test]
fn build_accepts_explicit_shard_audio_target() {
    let output = binary().args(["build", "--help"]).output().unwrap();
    assert!(output.status.success());
    let help = String::from_utf8(output.stdout).unwrap();
    assert!(help.contains("--shard-audio-mib"));
    assert!(help.contains("--bundle-workers"));
}

#[test]
fn bundle_worker_count_defaults_to_available_cpus_and_accepts_positive_usize() {
    let parsed =
        Cli::try_parse_from(["mgwt", "build", "--app", "/app", "--output", "/out"]).unwrap();
    assert!(matches!(
        parsed.command,
        CliCommand::Build {
            bundle_workers: None,
            ..
        }
    ));
    for valid in ["1", "5", "32", "18446744073709551615"] {
        let parsed = Cli::try_parse_from([
            "mgwt",
            "build",
            "--app",
            "/app",
            "--output",
            "/out",
            "--bundle-workers",
            valid,
        ])
        .unwrap();
        assert!(matches!(
            parsed.command,
            CliCommand::Build {
                bundle_workers: Some(_),
                ..
            }
        ));
    }
    for invalid in ["0", "18446744073709551616"] {
        assert!(
            Cli::try_parse_from([
                "mgwt",
                "build",
                "--app",
                "/app",
                "--output",
                "/out",
                "--bundle-workers",
                invalid,
            ])
            .is_err()
        );
    }
}
