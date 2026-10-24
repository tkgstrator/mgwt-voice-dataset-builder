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
    assert!(
        String::from_utf8(output.stdout)
            .unwrap()
            .contains("--shard-audio-mib")
    );
}
