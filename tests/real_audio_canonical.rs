//! Opt-in single-clip FSB5 Vorbis stream check; no Python oracle or audio artifact.
use mgwt_voice_dataset_builder::{
    config::{InputEntry, checked_in_approval},
    inventory::{VerifiedBundleInputs, VerifiedInputFile},
    model::{AudioLimits, CodecKind, FrameCountSource},
    paths::ValidatedAppPath,
    unity::{BundleSession, UnityBackend, UnityReader},
};
use std::path::Path;

#[test]
#[ignore = "requires MGWT_APP_PATH to the approved app"]
fn one_approved_vorbis_clip_streams_through_canonicalizer() {
    let app = ValidatedAppPath::new(Path::new(
        &std::env::var_os("MGWT_APP_PATH").expect("MGWT_APP_PATH"),
    ))
    .unwrap();
    let approval = checked_in_approval().unwrap();
    let bundle = approval
        .manifest
        .bundles
        .iter()
        .find(|entry| {
            entry
                .relative_path
                .contains("general-voice-act01_chapter01_trial_assets_all")
        })
        .unwrap();
    let approved = VerifiedBundleInputs {
        bundle: VerifiedInputFile::open(
            &app,
            &InputEntry {
                relative_path: bundle.relative_path.clone(),
                size: bundle.size,
                sha256: bundle.sha256.clone(),
            },
        )
        .unwrap(),
        resources: vec![],
    };
    let mut session = UnityReader::new(std::env::temp_dir())
        .open_bundle(&approved)
        .unwrap();
    let refs = session.audio_refs().unwrap();
    let reference = refs
        .iter()
        .find(|reference| reference.signature.codec == CodecKind::Fsb5Vorbis)
        .unwrap();
    let receipt = session
        .stream_audio(reference, &mut std::io::sink(), &AudioLimits::default())
        .unwrap();
    assert_eq!(receipt.channels, 2);
    assert_eq!(receipt.sample_rate, 48_000);
    assert_eq!(receipt.bits_per_sample, 16);
    assert!(receipt.wav_bytes > 44);
    assert_eq!(
        receipt.frame_count_source,
        FrameCountSource::ValidatedPayloadHeaderAndWav
    );
    assert_eq!(session.metrics().bundle_opens, 1);
    session.finish().unwrap();
    eprintln!(
        "one Vorbis clip canonicalized: bytes={} frames={} opens=1",
        receipt.wav_bytes, receipt.frame_count
    );
}
