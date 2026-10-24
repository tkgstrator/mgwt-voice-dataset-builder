//! Opt-in two-clip resolver smoke with synthetic script references only.
use mgwt_voice_dataset_builder::{
    config::{InputEntry, checked_in_approval},
    hash::sha256_bytes,
    inventory::{VerifiedBundleInputs, VerifiedInputFile},
    matching::{SourceTotals, match_candidates},
    model::{AudioLimits, CodecKind, ParsedScriptReference, ScriptProvenance, ScriptReference},
    normalize::{NormalizationResult, normalize_voice_id},
    paths::ValidatedAppPath,
    resolution::resolve,
    speaker::SpeakerResolution,
    unity::{BundleSession, UnityBackend, UnityReader},
};
use std::{collections::BTreeMap, path::Path};

#[test]
#[ignore = "requires MGWT_APP_PATH to the approved app"]
fn two_approved_clips_resolve_into_owned_staging() {
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
    let reader = UnityReader::new(std::env::temp_dir());
    let mut session = reader.open_bundle(&approved).unwrap();
    let audio: Vec<_> = session
        .audio_refs()
        .unwrap()
        .into_iter()
        .filter(|r| r.signature.codec == CodecKind::Fsb5Vorbis)
        .take(2)
        .collect();
    drop(session);
    assert_eq!(audio.len(), 2);
    let scripts: Vec<_> = audio
        .iter()
        .enumerate()
        .map(|(i, a)| {
            let id = normalize_voice_id(&a.name).unwrap();
            ScriptReference {
                parsed: ParsedScriptReference {
                    candidate_id: sha256_bytes(format!("synthetic-script-{i}").as_bytes()),
                    voice_id: id,
                    raw_author_id: Some("Coco".into()),
                    raw_display_text: "synthetic dialogue".into(),
                    transcription: NormalizationResult {
                        text: "synthetic dialogue".into(),
                        operations: vec![],
                        is_lexical: true,
                    },
                    provenance: ScriptProvenance {
                        bundle_path: "synthetic-script.bundle".into(),
                        file_index: 0,
                        object_index: i,
                        path_id: i as i64,
                        container_path: "synthetic".into(),
                        command_index: i,
                    },
                    script_path: "synthetic".into(),
                    script_name: "synthetic".into(),
                    neighbor_commands: vec![],
                },
                speaker_resolution: SpeakerResolution {
                    speaker: Some("Coco".into()),
                    evidence: vec!["author_id", "voice_id"],
                    reason: None,
                },
            }
        })
        .collect();
    let plan = match_candidates(
        &scripts,
        &audio,
        &[],
        SourceTotals {
            script_references: 2,
            audio_assets: 2,
            prior_rejections: 0,
        },
    )
    .unwrap();
    let approved_map = BTreeMap::from([(bundle.relative_path.clone(), approved)]);
    let staging = tempfile::tempdir().unwrap();
    let result = resolve(
        &plan,
        &reader,
        &approved_map,
        staging.path(),
        &AudioLimits::default(),
        |_, _| Ok(()),
    )
    .unwrap();
    assert_eq!(result.bundle_health[0].eligible_attempts, 2);
    assert_eq!(result.bundle_health[0].successful_decodes, 2);
    assert_eq!(result.terminal_sources.len(), 4);
    assert!(!result.rows.is_empty());
    assert!(result.exact_staged_wav_bytes > 44);
    assert_eq!(
        std::fs::read_dir(staging.path()).unwrap().count(),
        result.rows.len()
    );
    eprintln!(
        "two-clip resolution: rows={} staged_bytes={} health_success=2",
        result.rows.len(),
        result.exact_staged_wav_bytes
    );
}
