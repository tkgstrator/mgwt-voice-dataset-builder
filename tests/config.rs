use serde_json::Value;
use sha2::{Digest, Sha256};

#[test]
fn portable_approval_is_bound_and_contains_no_absolute_source_path() {
    let manifest_bytes = std::fs::read("config/manosaba-inputs-v1.json").unwrap();
    let manifest: Value = serde_json::from_slice(&manifest_bytes).unwrap();
    let speakers: Value =
        serde_json::from_slice(&std::fs::read("config/manosaba-speakers-v1.json").unwrap())
            .unwrap();
    let migration: Value =
        serde_json::from_slice(&std::fs::read("config/manosaba-config-migration-v1.json").unwrap())
            .unwrap();
    assert!(manifest.get("app_realpath").is_none());
    assert_eq!(manifest["schema_version"], "manosaba-inputs-v1-portable");
    assert_eq!(manifest["script_bundle_count"], 24);
    assert_eq!(manifest["voice_bundle_count"], 35);
    assert_eq!(speakers["speaker_ids"].as_array().unwrap().len(), 26);
    assert_eq!(
        speakers["input_manifest_sha256"],
        hex::encode(Sha256::digest(&manifest_bytes))
    );
    assert_eq!(
        speakers["input_manifest_identity"],
        serde_json::json!({"app_identity":manifest["app_identity"],"unity_version":manifest["unity_version"]})
    );
    assert_eq!(
        migration["former_manifest_sha256"],
        "b6fc6867ae2d32c59783c83a7fcd34dbdd7b660d855b4bbc432c5e1729a17e61"
    );
    assert_eq!(
        migration["former_full_input_fingerprint"],
        "fe209715f41b88d7b1c54c76b22aaa46819d5f1110effb02fa2677fb48b5b1c6"
    );
}

#[test]
fn approval_rejects_mutated_speaker_evidence_and_unsafe_manifest_paths() {
    use mgwt_voice_dataset_builder::config::ApprovedManifest;
    let manifest = std::fs::read("config/manosaba-inputs-v1.json").unwrap();
    let mut bad: Value = serde_json::from_slice(&manifest).unwrap();
    bad["bundles"][0]["relative_path"] = "../escape".into();
    assert!(ApprovedManifest::parse(&serde_json::to_vec(&bad).unwrap()).is_err());
    bad = serde_json::from_slice(&manifest).unwrap();
    bad["unexpected"] = true.into();
    assert!(ApprovedManifest::parse(&serde_json::to_vec(&bad).unwrap()).is_err());
    let mut speakers: Value =
        serde_json::from_slice(&std::fs::read("config/manosaba-speakers-v1.json").unwrap())
            .unwrap();
    speakers["speakers"][0]["evidence_sha256"] =
        "0000000000000000000000000000000000000000000000000000000000000000".into();
    let migration = std::fs::read("config/manosaba-config-migration-v1.json").unwrap();
    assert!(
        mgwt_voice_dataset_builder::config::load_approval(
            &manifest,
            &serde_json::to_vec(&speakers).unwrap(),
            &migration
        )
        .is_err()
    );
}

#[test]
fn approval_rejects_extra_nested_evidence_and_duplicate_speaker_ids() {
    let manifest = std::fs::read("config/manosaba-inputs-v1.json").unwrap();
    let speaker_bytes = std::fs::read("config/manosaba-speakers-v1.json").unwrap();
    let migration = std::fs::read("config/manosaba-config-migration-v1.json").unwrap();
    let mut speakers: Value = serde_json::from_slice(&speaker_bytes).unwrap();
    speakers["counts"]["unreviewed"] = 1.into();
    let altered = serde_json::to_vec(&speakers).unwrap();
    let mut migration_value: Value = serde_json::from_slice(&migration).unwrap();
    migration_value["portable_speakers_sha256"] = hex::encode(Sha256::digest(&altered)).into();
    assert!(
        mgwt_voice_dataset_builder::config::load_approval(
            &manifest,
            &altered,
            &serde_json::to_vec(&migration_value).unwrap()
        )
        .is_err()
    );
    let mut speakers: Value = serde_json::from_slice(&speaker_bytes).unwrap();
    speakers["speaker_ids"][1] = speakers["speaker_ids"][0].clone();
    assert!(
        mgwt_voice_dataset_builder::config::load_approval(
            &manifest,
            &serde_json::to_vec(&speakers).unwrap(),
            &migration
        )
        .is_err()
    );
}

#[test]
fn manifest_rejects_duplicate_unsorted_absolute_and_malformed_entries() {
    use mgwt_voice_dataset_builder::config::ApprovedManifest;
    let original: Value =
        serde_json::from_slice(&std::fs::read("config/manosaba-inputs-v1.json").unwrap()).unwrap();
    for edit in 0..5 {
        let mut value = original.clone();
        match edit {
            0 => {
                value["bundles"][1]["relative_path"] = value["bundles"][0]["relative_path"].clone()
            }
            1 => value["bundles"][0]["relative_path"] = "/absolute.bundle".into(),
            2 => value["bundles"][0]["sha256"] = "ABC".into(),
            3 => value["bundles"][0]["relative_path"] = "x".repeat(513).into(),
            _ => value["bundles"].as_array_mut().unwrap().swap(0, 1),
        }
        assert!(
            ApprovedManifest::parse(&serde_json::to_vec(&value).unwrap()).is_err(),
            "case {edit}"
        );
    }
}

#[test]
fn reviewed_app_identity_cannot_drift_with_valid_file_hashes() {
    use mgwt_voice_dataset_builder::config::ApprovedManifest;
    let mut manifest: Value =
        serde_json::from_slice(&std::fs::read("config/manosaba-inputs-v1.json").unwrap()).unwrap();
    manifest["app_identity"]["bundle_identifier"] = "another.game".into();
    assert!(ApprovedManifest::parse(&serde_json::to_vec(&manifest).unwrap()).is_err());
}

#[test]
fn reviewed_structural_counts_cannot_drift_with_rebound_hash() {
    let manifest = std::fs::read("config/manosaba-inputs-v1.json").unwrap();
    let mut speakers: Value =
        serde_json::from_slice(&std::fs::read("config/manosaba-speakers-v1.json").unwrap())
            .unwrap();
    speakers["counts"]["candidate_count"] = 1.into();
    let altered = serde_json::to_vec(&speakers).unwrap();
    let mut migration: Value =
        serde_json::from_slice(&std::fs::read("config/manosaba-config-migration-v1.json").unwrap())
            .unwrap();
    migration["portable_speakers_sha256"] = hex::encode(Sha256::digest(&altered)).into();
    assert!(
        mgwt_voice_dataset_builder::config::load_approval(
            &manifest,
            &altered,
            &serde_json::to_vec(&migration).unwrap()
        )
        .is_err()
    );
}
