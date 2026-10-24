use mgwt_voice_dataset_builder::{
    config::InputEntry,
    error::{AppError, UnityErrorKind},
    hash::sha256_bytes,
    inventory::{VerifiedBundleInputs, VerifiedInputFile},
    matching::{SourceTotals, audio_candidate_id, match_candidates as checked_match_candidates},
    model::{
        AudioAssetRef, AudioLimits, AudioMetadata, CodecKind, CodecLayoutSignature,
        DecodedAudioReceipt, FrameCountSource, MetadataCompleteness, ParsedScriptReference,
        PayloadLayout, PredecodeFrameCount, ScriptProvenance, ScriptReference,
    },
    normalize::NormalizationResult,
    paths::ValidatedAppPath,
    resolution::{TerminalKind, resolve},
    speaker::SpeakerResolution,
    unity::{BundleSession, PayloadBinding, UnityBackend},
};

fn match_candidates(
    scripts: &[ScriptReference],
    audio: &[AudioAssetRef],
    prior: &[mgwt_voice_dataset_builder::model::CandidateRejection],
) -> Result<mgwt_voice_dataset_builder::matching::MatchPlan, AppError> {
    checked_match_candidates(
        scripts,
        audio,
        prior,
        SourceTotals {
            script_references: scripts.len(),
            audio_assets: audio.len(),
            prior_rejections: prior.len(),
        },
    )
}
use std::{
    collections::BTreeMap,
    io::Write,
    sync::{Arc, Mutex},
};

fn script(id: &str, voice: &str, text: &str, speaker: &str) -> ScriptReference {
    ScriptReference {
        parsed: ParsedScriptReference {
            candidate_id: id.into(),
            voice_id: voice.into(),
            raw_author_id: Some(speaker.into()),
            raw_display_text: text.into(),
            transcription: NormalizationResult {
                text: text.into(),
                operations: vec![],
                is_lexical: true,
            },
            provenance: ScriptProvenance {
                bundle_path: "script".into(),
                file_index: 0,
                object_index: 0,
                path_id: 1,
                container_path: "story".into(),
                command_index: id.len(),
            },
            script_path: "story".into(),
            script_name: "story".into(),
            neighbor_commands: vec![],
        },
        speaker_resolution: SpeakerResolution {
            speaker: Some(speaker.into()),
            evidence: vec!["author_id"],
            reason: None,
        },
    }
}
fn audio(voice: &str, bundle: &str, path_id: i64, measured: bool) -> AudioAssetRef {
    AudioAssetRef {
        bundle_path: bundle.into(),
        file_index: 0,
        object_index: path_id as usize,
        path_id,
        name: voice.into(),
        container: Some(format!("Assets/{path_id}")),
        binding: PayloadBinding::Inline {
            declared_offset: 0,
            start: 0,
            size: 0,
        },
        metadata: AudioMetadata {
            channels: 1,
            sample_rate: 8000,
            bits_per_sample: 16,
            duration_seconds: 0.000125,
            compression_format: 0,
            subsound_index: 0,
        },
        metadata_completeness: if measured {
            MetadataCompleteness::RequiresMeasurement
        } else {
            MetadataCompleteness::CompleteFromValidatedHeader
        },
        predecode_frames: if measured {
            PredecodeFrameCount::Unavailable
        } else {
            PredecodeFrameCount::ValidatedPayloadHeader(1)
        },
        signature: CodecLayoutSignature {
            codec: CodecKind::Fsb5Vorbis,
            layout: PayloadLayout::Inline,
            compression_format: 0,
            channels: 1,
            sample_rate: 8000,
            bits_per_sample: 16,
        },
    }
}
fn wave(sample: i16) -> Vec<u8> {
    let mut out = b"RIFF".to_vec();
    out.extend_from_slice(&38_u32.to_le_bytes());
    out.extend_from_slice(b"WAVEfmt ");
    out.extend_from_slice(&16_u32.to_le_bytes());
    out.extend_from_slice(&1_u16.to_le_bytes());
    out.extend_from_slice(&1_u16.to_le_bytes());
    out.extend_from_slice(&8000_u32.to_le_bytes());
    out.extend_from_slice(&16000_u32.to_le_bytes());
    out.extend_from_slice(&2_u16.to_le_bytes());
    out.extend_from_slice(&16_u16.to_le_bytes());
    out.extend_from_slice(b"data");
    out.extend_from_slice(&2_u32.to_le_bytes());
    out.extend_from_slice(&sample.to_le_bytes());
    out
}
#[derive(Default)]
struct State {
    opens: BTreeMap<String, usize>,
    streams: BTreeMap<String, usize>,
}
struct FakeBackend {
    state: Arc<Mutex<State>>,
    wavs: BTreeMap<String, Vec<u8>>,
    failures: BTreeMap<String, UnityErrorKind>,
    receipt_hash_override: BTreeMap<String, String>,
}
struct FakeSession {
    bundle: String,
    state: Arc<Mutex<State>>,
    wavs: BTreeMap<String, Vec<u8>>,
    failures: BTreeMap<String, UnityErrorKind>,
    receipt_hash_override: BTreeMap<String, String>,
}
impl UnityBackend for FakeBackend {
    type Session = FakeSession;
    fn open_bundle(&self, approved: &VerifiedBundleInputs) -> Result<Self::Session, AppError> {
        self.state
            .lock()
            .unwrap()
            .opens
            .entry(approved.bundle.relative_path.clone())
            .and_modify(|n| *n += 1)
            .or_insert(1);
        Ok(FakeSession {
            bundle: approved.bundle.relative_path.clone(),
            state: self.state.clone(),
            wavs: self.wavs.clone(),
            failures: self.failures.clone(),
            receipt_hash_override: self.receipt_hash_override.clone(),
        })
    }
}
impl BundleSession for FakeSession {
    fn raw_script_objects(
        &mut self,
    ) -> Result<Vec<mgwt_voice_dataset_builder::model::RawScriptObject>, AppError> {
        Ok(vec![])
    }
    fn audio_refs(&mut self) -> Result<Vec<AudioAssetRef>, AppError> {
        Ok(vec![])
    }
    fn stream_audio(
        &mut self,
        a: &AudioAssetRef,
        sink: &mut dyn Write,
        _: &AudioLimits,
    ) -> Result<DecodedAudioReceipt, AppError> {
        assert_eq!(self.bundle, a.bundle_path);
        let id = audio_candidate_id(a);
        self.state
            .lock()
            .unwrap()
            .streams
            .entry(id.clone())
            .and_modify(|n| *n += 1)
            .or_insert(1);
        if let Some(kind) = self.failures.get(&id) {
            if let Some(wav) = self.wavs.get(&id) {
                sink.write_all(&wav[..wav.len() - 1]).unwrap();
            }
            return Err(AppError::unity(*kind, "fake decode failure"));
        }
        let wav = &self.wavs[&id];
        for part in wav.chunks(3) {
            sink.write_all(part)
                .map_err(|e| AppError::unity(UnityErrorKind::CandidateDecode, e.to_string()))?;
        }
        Ok(DecodedAudioReceipt {
            wav_bytes: wav.len() as u64,
            wav_sha256: self
                .receipt_hash_override
                .get(&id)
                .cloned()
                .unwrap_or_else(|| sha256_bytes(wav)),
            channels: 1,
            sample_rate: 8000,
            bits_per_sample: 16,
            frame_count: 1,
            frame_count_source: FrameCountSource::ValidatedDecodedWav,
            peak_live_buffer_bytes: 128,
        })
    }
}
fn approved(names: &[&str]) -> (tempfile::TempDir, BTreeMap<String, VerifiedBundleInputs>) {
    let dir = tempfile::tempdir_in(".").unwrap();
    for name in names {
        std::fs::write(dir.path().join(name), b"approved").unwrap();
    }
    let app = ValidatedAppPath::new(dir.path()).unwrap();
    let map = names
        .iter()
        .map(|name| {
            let input = VerifiedInputFile::open(
                &app,
                &InputEntry {
                    relative_path: (*name).into(),
                    size: 8,
                    sha256: sha256_bytes(b"approved"),
                },
            )
            .unwrap();
            (
                (*name).into(),
                VerifiedBundleInputs {
                    bundle: input,
                    resources: vec![],
                },
            )
        })
        .collect();
    (dir, map)
}
fn backend(
    pairs: &[(&AudioAssetRef, i16)],
    failures: &[(&AudioAssetRef, UnityErrorKind)],
) -> FakeBackend {
    FakeBackend {
        state: Arc::new(Mutex::new(State::default())),
        wavs: pairs
            .iter()
            .map(|(a, n)| (audio_candidate_id(a), wave(*n)))
            .collect(),
        failures: failures
            .iter()
            .map(|(a, kind)| (audio_candidate_id(a), *kind))
            .collect(),
        receipt_hash_override: BTreeMap::new(),
    }
}
#[test]
fn interleaved_ids_open_each_bundle_once_per_pass_and_finalize_original_order() {
    let a = audio("A", "b", 1, true);
    let b = audio("B", "a", 2, true);
    let c = audio("C", "b", 3, true);
    let plan = match_candidates(
        &[
            script("s-a", "A", "a", "Coco"),
            script("s-b", "B", "b", "Coco"),
            script("s-c", "C", "c", "Coco"),
        ],
        &[a.clone(), b.clone(), c.clone()],
        &[],
    )
    .unwrap();
    let (_app, approved) = approved(&["a", "b"]);
    let staging = tempfile::tempdir().unwrap();
    let fake = backend(&[(&a, 1), (&b, 2), (&c, 3)], &[]);
    let mut checks = Vec::new();
    let result = resolve(
        &plan,
        &fake,
        &approved,
        staging.path(),
        &AudioLimits::default(),
        |already, next| {
            checks.push((already, next));
            Ok(())
        },
    )
    .unwrap();
    assert_eq!(
        result
            .rows
            .iter()
            .map(|r| r.voice_ids[0].as_str())
            .collect::<Vec<_>>(),
        ["A", "B", "C"]
    );
    assert_eq!(
        fake.state.lock().unwrap().opens,
        BTreeMap::from([("a".into(), 2), ("b".into(), 2)])
    );
    assert_eq!(
        fake.state
            .lock()
            .unwrap()
            .streams
            .values()
            .copied()
            .collect::<Vec<_>>(),
        [2, 2, 2]
    );
    assert_eq!(result.exact_staged_wav_bytes, 138);
    assert!(checks.contains(&(0, 46)) && checks.contains(&(46, 46)) && checks.contains(&(92, 46)));
    assert_eq!(result.terminal_sources.len(), 6);
    assert_eq!(std::fs::read_dir(staging.path()).unwrap().count(), 3);
}
#[test]
fn local_failure_falls_back_to_later_candidate_and_removes_partial_stage() {
    let a = audio("A", "bundle", 1, false);
    let b = audio("A", "bundle", 2, false);
    let plan = match_candidates(
        &[script("script", "A", "line", "Coco")],
        &[a.clone(), b.clone()],
        &[],
    )
    .unwrap();
    let (_app, approved) = approved(&["bundle"]);
    let staging = tempfile::tempdir().unwrap();
    let fake = backend(
        &[(&a, 6), (&b, 7)],
        &[(&a, UnityErrorKind::CandidateDecode)],
    );
    let result = resolve(
        &plan,
        &fake,
        &approved,
        staging.path(),
        &AudioLimits::default(),
        |_, _| Ok(()),
    )
    .unwrap();
    assert_eq!(result.rows.len(), 1);
    assert_eq!(result.candidate_rejections.len(), 1);
    assert_eq!(
        result.candidate_rejections[0].candidate_id,
        audio_candidate_id(&a)
    );
    assert_eq!(result.bundle_health[0].successful_decodes, 1);
    assert_eq!(std::fs::read_dir(staging.path()).unwrap().count(), 1);
}
#[test]
fn aliases_conflicts_and_cross_id_duplicate_rules_balance_sources() {
    let a = audio("A", "bundle", 1, false);
    let b = audio("A", "bundle", 2, false);
    let c = audio("B", "bundle", 3, false);
    let (_app, approved) = approved(&["bundle"]);
    let staging = tempfile::tempdir().unwrap();
    let scripts = [
        script("s-a", "A", "same", "Coco"),
        script("s-b", "B", "same", "Coco"),
    ];
    let plan = match_candidates(&scripts, &[a.clone(), b.clone(), c.clone()], &[]).unwrap();
    let fake = backend(&[(&a, 1), (&b, 1), (&c, 1)], &[]);
    let result = resolve(
        &plan,
        &fake,
        &approved,
        staging.path(),
        &AudioLimits::default(),
        |_, _| Ok(()),
    )
    .unwrap();
    assert_eq!(result.rows.len(), 1);
    assert_eq!(result.rows[0].voice_ids, ["A", "B"]);
    assert_eq!(result.duplicate_aliases.len(), 2);
    assert_eq!(result.exact_staged_wav_bytes, 46);
    assert_eq!(
        result
            .terminal_sources
            .iter()
            .filter(|s| s.kind == TerminalKind::DuplicateAlias)
            .count(),
        2
    );
    assert_eq!(std::fs::read_dir(staging.path()).unwrap().count(), 1);
    let conflict_stage = tempfile::tempdir().unwrap();
    let conflicting = backend(&[(&a, 1), (&b, 2), (&c, 1)], &[]);
    let result = resolve(
        &plan,
        &conflicting,
        &approved,
        conflict_stage.path(),
        &AudioLimits::default(),
        |_, _| Ok(()),
    )
    .unwrap();
    assert_eq!(result.rows.len(), 1); // A rejected; B still stands
    assert!(
        result
            .candidate_rejections
            .iter()
            .any(|r| r.reason == "different_audio_hashes")
    );
    let cross_stage = tempfile::tempdir().unwrap();
    let cross_plan = match_candidates(
        &[scripts[0].clone(), script("s-b", "B", "different", "Coco")],
        &[a.clone(), c.clone()],
        &[],
    )
    .unwrap();
    let cross = backend(&[(&a, 1), (&c, 1)], &[]);
    let result = resolve(
        &cross_plan,
        &cross,
        &approved,
        cross_stage.path(),
        &AudioLimits::default(),
        |_, _| Ok(()),
    )
    .unwrap();
    assert!(result.rows.is_empty());
    assert_eq!(result.candidate_rejections.len(), 4);
    assert_eq!(std::fs::read_dir(cross_stage.path()).unwrap().count(), 0);
}
#[test]
fn all_eligible_fail_is_fatal_but_unused_bundle_nonfatal_and_infrastructure_cleans_up() {
    let a = audio("A", "bundle", 1, false);
    let plan = match_candidates(
        &[script("script", "A", "line", "Coco")],
        std::slice::from_ref(&a),
        &[],
    )
    .unwrap();
    let (_app, approved) = approved(&["bundle", "unused"]);
    let staging = tempfile::tempdir().unwrap();
    let local = backend(&[], &[(&a, UnityErrorKind::CandidateDecode)]);
    assert!(
        resolve(
            &plan,
            &local,
            &approved,
            staging.path(),
            &AudioLimits::default(),
            |_, _| Ok(())
        )
        .is_err()
    );
    assert_eq!(std::fs::read_dir(staging.path()).unwrap().count(), 0);
    let okay = backend(&[(&a, 1)], &[]);
    let result = resolve(
        &plan,
        &okay,
        &approved,
        staging.path(),
        &AudioLimits::default(),
        |_, _| Ok(()),
    )
    .unwrap();
    assert_eq!(
        result
            .bundle_health
            .iter()
            .find(|b| b.bundle_path == "unused")
            .unwrap()
            .eligible_attempts,
        0
    );
    assert!(!okay.state.lock().unwrap().opens.contains_key("unused"));
    let failure_stage = tempfile::tempdir().unwrap();
    let fatal = backend(&[], &[(&a, UnityErrorKind::Io)]);
    assert!(
        resolve(
            &plan,
            &fatal,
            &approved,
            failure_stage.path(),
            &AudioLimits::default(),
            |_, _| Ok(())
        )
        .is_err()
    );
    assert_eq!(std::fs::read_dir(failure_stage.path()).unwrap().count(), 0);
}

#[test]
fn disk_callback_failure_after_one_clip_removes_every_owned_file() {
    let a = audio("A", "bundle", 1, false);
    let b = audio("B", "bundle", 2, false);
    let plan = match_candidates(
        &[
            script("s-a", "A", "a", "Coco"),
            script("s-b", "B", "b", "Coco"),
        ],
        &[a.clone(), b.clone()],
        &[],
    )
    .unwrap();
    let (_app, approved) = approved(&["bundle"]);
    let staging = tempfile::tempdir().unwrap();
    let fake = backend(&[(&a, 1), (&b, 2)], &[]);
    let failure = resolve(
        &plan,
        &fake,
        &approved,
        staging.path(),
        &AudioLimits::default(),
        |already, next| {
            if already + next > 46 {
                Err(AppError::processing("disk reservation refused"))
            } else {
                Ok(())
            }
        },
    );
    assert!(failure.is_err());
    assert_eq!(std::fs::read_dir(staging.path()).unwrap().count(), 0);
}

#[test]
fn disk_check_precedes_staging_bundle_open() {
    let a = audio("A", "bundle", 1, false);
    let plan = match_candidates(
        &[script("s-a", "A", "line", "Coco")],
        std::slice::from_ref(&a),
        &[],
    )
    .unwrap();
    let (_app, approved) = approved(&["bundle"]);
    let staging = tempfile::tempdir().unwrap();
    let fake = backend(&[(&a, 1)], &[]);
    let result = resolve(
        &plan,
        &fake,
        &approved,
        staging.path(),
        &AudioLimits::default(),
        |_, _| Err(AppError::processing("disk check refused")),
    );
    assert!(result.is_err());
    assert!(fake.state.lock().unwrap().opens.is_empty());
    assert_eq!(std::fs::read_dir(staging.path()).unwrap().count(), 0);
}

#[test]
fn decoder_receipt_hash_must_match_staged_wav_and_cleanup_on_mismatch() {
    let a = audio("A", "bundle", 1, false);
    let plan = match_candidates(
        &[script("s-a", "A", "line", "Coco")],
        std::slice::from_ref(&a),
        &[],
    )
    .unwrap();
    let (_app, approved) = approved(&["bundle"]);
    let staging = tempfile::tempdir().unwrap();
    let mut fake = backend(&[(&a, 1)], &[]);
    fake.receipt_hash_override
        .insert(audio_candidate_id(&a), "0".repeat(64));
    let result = resolve(
        &plan,
        &fake,
        &approved,
        staging.path(),
        &AudioLimits::default(),
        |_, _| Ok(()),
    );
    assert!(result.is_err());
    assert_eq!(std::fs::read_dir(staging.path()).unwrap().count(), 0);
}
