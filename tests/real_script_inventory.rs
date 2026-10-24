//! Opt-in, metadata-only complete approved script accounting.
use mgwt_voice_dataset_builder::{
    config::{BundleKind, InputEntry, checked_in_approval},
    hash::sha256_bytes,
    inventory::{VerifiedBundleInputs, VerifiedInputFile, inventory_app},
    naninovel::{parse_script_object, resolve_script_references},
    paths::ValidatedAppPath,
    unity::{BundleSession, UnityBackend, UnityReader},
};
use serde_json::{Value, json};
use std::{
    collections::{BTreeMap, BTreeSet},
    path::Path,
    time::Instant,
};
type EvidenceRow = (String, i64, usize, String, &'static str, Value);

#[test]
#[ignore = "requires MGWT_APP_PATH to the approved app; metadata only"]
fn approved_scripts_match_complete_evidence() {
    let app = ValidatedAppPath::new(Path::new(
        &std::env::var_os("MGWT_APP_PATH").expect("MGWT_APP_PATH"),
    ))
    .unwrap();
    let approval = checked_in_approval().unwrap();
    let pre = inventory_app(&app, &approval.manifest).unwrap();
    let reader = UnityReader::new(std::env::temp_dir());
    let start = Instant::now();
    let mut results = Vec::new();
    let mut opens = 0;
    for bundle in approval
        .manifest
        .bundles
        .iter()
        .filter(|b| b.kind == BundleKind::Script)
    {
        let input = VerifiedBundleInputs {
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
        let mut session = reader.open_bundle(&input).unwrap();
        let objects = session.raw_script_objects().unwrap();
        opens += session.metrics().bundle_opens;
        for raw in objects {
            results.push(parse_script_object(&raw).unwrap_or_else(|e| {
                panic!(
                    "{}:{}:{}: {e}",
                    raw.bundle_path, raw.file_index, raw.object_index
                )
            }));
        }
    }
    let counts = &approval.speakers.counts;
    assert_eq!(results.len() as u64, counts.object_count);
    assert_eq!(
        results.iter().map(|r| r.command_count as u64).sum::<u64>(),
        counts.command_count
    );
    assert_eq!(
        results
            .iter()
            .map(|r| r.reference_count as u64)
            .sum::<u64>(),
        counts.reference_count
    );
    assert_eq!(
        results
            .iter()
            .map(|r| r.text_entry_count as u64)
            .sum::<u64>(),
        counts.text_entry_count
    );
    assert_eq!(
        results
            .iter()
            .map(|r| r.parsed_references.len() as u64)
            .sum::<u64>(),
        counts.parsed_reference_count
    );
    assert_eq!(
        results
            .iter()
            .map(|r| r.candidate_rejections.len() as u64)
            .sum::<u64>(),
        counts.candidate_rejection_count
    );
    assert_eq!(
        results
            .iter()
            .map(|r| r.speaker_evidence.len() as u64)
            .sum::<u64>(),
        counts.speaker_evidence_count
    );
    let mut reasons = BTreeMap::new();
    let mut evidence: BTreeMap<&str, Vec<EvidenceRow>> = BTreeMap::new();
    for result in &results {
        for rejected in &result.candidate_rejections {
            *reasons.entry(rejected.reason).or_insert(0_u64) += 1;
        }
        for item in &result.speaker_evidence {
            let p = &item.provenance;
            let record = json!({"bundle_relpath":p.bundle_path,"path_id":p.path_id,"command_index":p.command_index,"voice_id_sha256":item.voice_id_sha256,"source":item.source});
            evidence.entry(&item.speaker).or_default().push((
                p.bundle_path.clone(),
                p.path_id,
                p.command_index,
                item.voice_id_sha256.clone().unwrap_or_default(),
                item.source,
                record,
            ));
        }
    }
    assert_eq!(
        reasons.get("invalid_voice_id"),
        Some(&approval.speakers.rejection_reasons.invalid_voice_id)
    );
    assert_eq!(
        reasons.get("non_lexical_transcription"),
        Some(
            &approval
                .speakers
                .rejection_reasons
                .non_lexical_transcription
        )
    );
    assert_eq!(
        reasons.get("unsupported_asset_shape"),
        Some(&approval.speakers.rejection_reasons.unsupported_asset_shape)
    );
    assert_eq!(
        reasons.values().sum::<u64>(),
        counts.candidate_rejection_count
    );
    let ids: BTreeSet<_> = evidence.keys().copied().collect();
    assert_eq!(
        ids,
        approval
            .speakers
            .speaker_ids
            .iter()
            .map(String::as_str)
            .collect()
    );
    for approved in &approval.speakers.speakers {
        let records = evidence.get_mut(approved.id.as_str()).unwrap();
        records.sort_by(|a, b| (&a.0, a.1, a.2, &a.3, a.4).cmp(&(&b.0, b.1, b.2, &b.3, b.4)));
        let canonical: Vec<_> = records.iter().map(|r| &r.5).collect();
        let bytes = serde_json::to_vec(&canonical).unwrap();
        assert_eq!(
            records.len() as u64,
            approved.evidence_count,
            "{} evidence count",
            approved.id
        );
        assert_eq!(
            sha256_bytes(&bytes),
            approved.evidence_sha256,
            "{} evidence digest",
            approved.id
        );
    }
    let resolved = resolve_script_references(&results, &approval.speakers).unwrap();
    assert_eq!(resolved.references.len(), 30744);
    assert_eq!(resolved.candidate_rejections.len(), 3308 + 18);
    assert_eq!(opens, 24);
    assert_eq!(inventory_app(&app, &approval.manifest).unwrap(), pre);
    eprintln!(
        "script inventory: 24 bundles, {} objects, {} parsed, {} rejected, {} speaker evidence, {} resolved; {} opens; {:?}",
        results.len(),
        counts.parsed_reference_count,
        counts.candidate_rejection_count,
        counts.speaker_evidence_count,
        resolved.references.len(),
        opens,
        start.elapsed()
    );
}
