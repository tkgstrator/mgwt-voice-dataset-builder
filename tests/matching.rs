use mgwt_voice_dataset_builder::{
    matching::{
        AuditState, CandidateKind, SourceTotals, match_candidates as checked_match_candidates,
    },
    model::{
        AudioAssetRef, AudioMetadata, CandidateProvenance, CandidateRejection, CodecKind,
        CodecLayoutSignature, MetadataCompleteness, ParsedScriptReference, PayloadLayout,
        PredecodeFrameCount, ScriptProvenance, ScriptReference,
    },
    normalize::NormalizationResult,
    speaker::SpeakerResolution,
    unity::PayloadBinding,
};

fn match_candidates(
    scripts: &[ScriptReference],
    audio: &[AudioAssetRef],
    prior: &[CandidateRejection],
) -> Result<
    mgwt_voice_dataset_builder::matching::MatchPlan,
    mgwt_voice_dataset_builder::error::AppError,
> {
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

fn provenance(bundle: &str, command: usize) -> ScriptProvenance {
    ScriptProvenance {
        bundle_path: bundle.into(),
        file_index: 0,
        object_index: 0,
        path_id: 1,
        container_path: "story".into(),
        command_index: command,
    }
}

fn script(
    id: &str,
    voice: &str,
    text: &str,
    speaker: &str,
    bundle: &str,
    command: usize,
) -> ScriptReference {
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
            provenance: provenance(bundle, command),
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

fn audio(voice: &str, bundle: &str, path_id: i64) -> AudioAssetRef {
    AudioAssetRef {
        bundle_path: bundle.into(),
        file_index: 0,
        object_index: 0,
        path_id,
        name: voice.into(),
        container: Some("Assets/voice".into()),
        binding: PayloadBinding::Inline {
            declared_offset: 0,
            start: 0,
            size: 0,
        },
        metadata: AudioMetadata {
            channels: 1,
            sample_rate: 44100,
            bits_per_sample: 16,
            duration_seconds: 0.0,
            compression_format: 0,
            subsound_index: 0,
        },
        metadata_completeness: MetadataCompleteness::RequiresMeasurement,
        predecode_frames: PredecodeFrameCount::Unavailable,
        signature: CodecLayoutSignature {
            codec: CodecKind::ExistingWave,
            layout: PayloadLayout::Inline,
            compression_format: 0,
            channels: 1,
            sample_rate: 44100,
            bits_per_sample: 16,
        },
    }
}

fn reason(
    plan: &mgwt_voice_dataset_builder::matching::MatchPlan,
    id: &str,
) -> Option<&'static str> {
    plan.audit_records
        .iter()
        .find(|a| a.candidate_id == id)
        .and_then(|a| a.reason)
}

#[test]
fn exact_global_join_keeps_multiple_audio_pending_and_is_order_independent() {
    let scripts = vec![
        script("s2", "A", "same", "Coco", "script/z", 2),
        script("s1", "A", "same", "Coco", "script/a", 1),
    ];
    let audio = vec![audio("A.wav", "audio/z", 2), audio("A", "audio/a", 1)];
    let first = match_candidates(&scripts, &audio, &[]).unwrap();
    let second = match_candidates(
        &scripts.iter().cloned().rev().collect::<Vec<_>>(),
        &audio.iter().cloned().rev().collect::<Vec<_>>(),
        &[],
    )
    .unwrap();
    assert_eq!(format!("{first:?}"), format!("{second:?}"));
    assert_eq!(first.pending_groups.len(), 1);
    assert_eq!(first.pending_groups[0].script_references.len(), 2);
    assert_eq!(first.pending_groups[0].audio_refs.len(), 2);
    assert_eq!(
        first.pending_groups[0].script_references[0]
            .parsed
            .candidate_id,
        "s1"
    );
    assert_eq!(first.pending_groups[0].audio_refs[0].bundle_path, "audio/a");
    assert!(first.candidate_rejections.is_empty());
    assert_eq!(first.accounting.source_total, 4);
    assert_eq!(first.accounting.pending_total, 4);
}

#[test]
fn normalized_text_agrees_but_conflicts_reject_before_missing_side() {
    let mut wrapped = script("s1", "A", "<b>text</b>", "Coco", "s", 0);
    wrapped.parsed.transcription.text = "text".into();
    let plain = script("s2", "A", "text", "Coco", "s", 1);
    assert_eq!(
        match_candidates(
            &[wrapped.clone(), plain.clone()],
            &[audio("A", "a", 1)],
            &[]
        )
        .unwrap()
        .pending_groups
        .len(),
        1
    );
    let conflicting = script("s3", "A", "other", "Coco", "s", 2);
    let plan = match_candidates(&[wrapped, conflicting], &[], &[]).unwrap();
    assert_eq!(reason(&plan, "s1"), Some("transcription_conflict"));
    assert_eq!(
        plan.group_explanations[0].terminal_stage,
        "script_group_conflict"
    );
}

#[test]
fn speaker_conflict_and_missing_sides_reject_complete_groups() {
    let plan = match_candidates(
        &[
            script("s1", "A", "t", "Coco", "s", 0),
            script("s2", "A", "t", "Ema", "s", 1),
            script("s3", "B", "t", "Coco", "s", 2),
        ],
        &[audio("A", "a", 1), audio("C", "a", 2)],
        &[],
    )
    .unwrap();
    assert_eq!(reason(&plan, "s1"), Some("speaker_conflict"));
    assert_eq!(reason(&plan, "s3"), Some("missing_audio"));
    assert!(
        plan.audit_records
            .iter()
            .any(|a| a.group_id.as_deref() == Some("C") && a.reason == Some("missing_script"))
    );
    assert_eq!(plan.accounting.source_total, 5);
    assert_eq!(plan.accounting.rejected_total, 5);
}

#[test]
fn one_prior_rejection_blocks_two_groups_but_counts_once() {
    let prior = CandidateRejection {
        candidate_id: "bad".into(),
        candidate_kind: CandidateKind::ScriptReference,
        group_id: None,
        terminal_stage: "asset_shape",
        reason: "ambiguous_dialogue",
        provenance: CandidateProvenance::Script(provenance("s", 9)),
        blocking_group_ids: vec!["A".into(), "B".into()],
    };
    let plan = match_candidates(
        &[
            script("s1", "A", "t", "Coco", "s", 0),
            script("s2", "B", "t", "Coco", "s", 1),
        ],
        &[audio("A", "a", 1), audio("B", "a", 2)],
        &[prior],
    )
    .unwrap();
    assert_eq!(plan.group_explanations.len(), 2);
    assert!(
        plan.group_explanations
            .iter()
            .all(|g| g.terminal_stage == "asset_shape")
    );
    assert_eq!(
        plan.candidate_rejections
            .iter()
            .filter(|r| r.candidate_id == "bad")
            .count(),
        1
    );
    assert_eq!(plan.accounting.source_total, 5);
    assert_eq!(plan.accounting.rejected_total, 5);
}

#[test]
fn invalid_audio_name_is_rejected_and_source_identity_collision_is_fatal() {
    let mut invalid = audio("../A", "a", 1);
    let plan = match_candidates(&[], &[invalid.clone()], &[]).unwrap();
    assert_eq!(plan.candidate_rejections.len(), 1);
    assert_eq!(plan.candidate_rejections[0].terminal_stage, "voice_id");
    assert_eq!(
        plan.audit_records[0].candidate_kind,
        CandidateKind::AudioAsset
    );
    assert_eq!(plan.audit_records[0].state, AuditState::PreliminaryRejected);
    assert_eq!(plan.accounting.source_total, 1);
    invalid.name = "A".into();
    assert!(match_candidates(&[], &[invalid.clone(), invalid], &[]).is_err());
}

#[test]
fn invalid_prior_stage_and_duplicate_prior_identity_are_fatal() {
    let mut prior = CandidateRejection {
        candidate_id: "bad".into(),
        candidate_kind: CandidateKind::ScriptReference,
        group_id: None,
        terminal_stage: "made_up",
        reason: "bad",
        provenance: CandidateProvenance::Script(provenance("s", 0)),
        blocking_group_ids: vec![],
    };
    assert!(match_candidates(&[], &[], &[prior.clone()]).is_err());
    prior.terminal_stage = "asset_shape";
    assert!(match_candidates(&[], &[], &[prior.clone(), prior]).is_err());
}

#[test]
fn supplied_source_totals_must_match_actual_inputs() {
    let expected = SourceTotals {
        script_references: 1,
        audio_assets: 0,
        prior_rejections: 0,
    };
    assert!(checked_match_candidates(&[], &[], &[], expected).is_err());
}

#[test]
fn audio_rejection_preserves_audio_provenance() {
    let plan = checked_match_candidates(
        &[],
        &[audio("../bad", "bundle", 9)],
        &[],
        SourceTotals {
            script_references: 0,
            audio_assets: 1,
            prior_rejections: 0,
        },
    )
    .unwrap();
    let CandidateProvenance::Audio(source) = &plan.candidate_rejections[0].provenance else {
        panic!("expected audio provenance")
    };
    assert_eq!(source.bundle_path, "bundle");
    assert_eq!(source.path_id, 9);
}

#[test]
fn prior_audio_blocker_keeps_audio_kind_in_explanation() {
    let rejected_audio = audio("A", "audio/earlier", 7);
    let prior = CandidateRejection {
        candidate_id: "prior-audio".into(),
        candidate_kind: CandidateKind::AudioAsset,
        group_id: Some("A".into()),
        terminal_stage: "voice_id",
        reason: "invalid_voice_id",
        provenance: CandidateProvenance::Audio(
            mgwt_voice_dataset_builder::model::AudioProvenance {
                bundle_path: rejected_audio.bundle_path,
                file_index: rejected_audio.file_index,
                object_index: rejected_audio.object_index,
                path_id: rejected_audio.path_id,
                container_path: rejected_audio.container.unwrap(),
            },
        ),
        blocking_group_ids: vec!["A".into()],
    };
    let plan = match_candidates(
        &[script("script-a", "A", "text", "Coco", "script", 0)],
        &[],
        &[prior],
    )
    .unwrap();
    assert_eq!(
        plan.group_explanations[0].script_candidate_ids,
        vec!["script-a"]
    );
    assert_eq!(
        plan.group_explanations[0].audio_candidate_ids,
        vec!["prior-audio"]
    );
}
