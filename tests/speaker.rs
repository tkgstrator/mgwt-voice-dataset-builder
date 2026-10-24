use mgwt_voice_dataset_builder::{config::checked_in_approval, speaker::resolve_speaker};

#[test]
fn exact_finite_evidence_resolves_only_approved_agreement() {
    let approval = checked_in_approval().unwrap();
    assert_eq!(approval.speakers.speaker_ids.len(), 26);
    let approved = &approval.speakers;
    assert_eq!(
        resolve_speaker(Some("Coco"), "Scene_Coco001", approved)
            .speaker
            .as_deref(),
        Some("Coco")
    );
    assert_eq!(
        resolve_speaker(Some("Ema"), "Scene_Coco001", approved).reason,
        Some("speaker_conflict")
    );
    assert!(
        resolve_speaker(None, "Scene_NewVoice001", approved)
            .speaker
            .is_none()
    );
}
