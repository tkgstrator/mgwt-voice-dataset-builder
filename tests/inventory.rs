use mgwt_voice_dataset_builder::{
    config::InputEntry,
    inventory::{VerifiedInputFile, inventory_entries},
    paths::ValidatedAppPath,
};
use std::fs;

fn entry(name: &str, bytes: &[u8]) -> InputEntry {
    InputEntry {
        relative_path: name.into(),
        size: bytes.len() as u64,
        sha256: hex::encode(sha2::Sha256::digest(bytes)),
    }
}
use sha2::Digest;

#[test]
fn inventory_hashes_bound_bytes_and_reports_drift() {
    let root = tempfile::tempdir_in(".").unwrap();
    fs::create_dir(root.path().join("Contents")).unwrap();
    fs::write(root.path().join("Contents/one.bundle"), b"abc").unwrap();
    let app = ValidatedAppPath::new(root.path()).unwrap();
    let want = entry("Contents/one.bundle", b"abc");
    let snapshot = inventory_entries(&app, std::slice::from_ref(&want)).unwrap();
    assert_eq!(snapshot.entries.len(), 1);
    let mut verified = VerifiedInputFile::open(&app, &want).unwrap();
    let scratch = tempfile::tempdir_in(".").unwrap();
    let held = mgwt_voice_dataset_builder::inventory::VerifiedSnapshot::copy_once(
        &mut verified,
        scratch.path(),
        3,
    )
    .unwrap();
    assert_eq!(
        held.with_verified_path(|path| fs::read(path))
            .unwrap()
            .unwrap(),
        b"abc"
    );
    fs::write(root.path().join("Contents/one.bundle"), b"abd").unwrap();
    assert!(inventory_entries(&app, &[want]).is_err());
}

#[test]
fn swapped_leaf_is_rejected_even_after_app_validation() {
    let root = tempfile::tempdir_in(".").unwrap();
    fs::create_dir(root.path().join("Contents")).unwrap();
    let app = ValidatedAppPath::new(root.path()).unwrap();
    let outside = tempfile::tempdir_in(".").unwrap();
    fs::write(outside.path().join("evil"), b"abc").unwrap();
    std::os::unix::fs::symlink(
        outside.path().join("evil"),
        root.path().join("Contents/one.bundle"),
    )
    .unwrap();
    assert!(VerifiedInputFile::open(&app, &entry("Contents/one.bundle", b"abc")).is_err());
}

#[test]
fn snapshot_copies_verified_bytes_and_cleans_owned_directory() {
    use mgwt_voice_dataset_builder::inventory::VerifiedSnapshot;
    let root = tempfile::tempdir_in(".").unwrap();
    fs::create_dir(root.path().join("Contents")).unwrap();
    fs::write(root.path().join("Contents/one.bundle"), b"abc").unwrap();
    let scratch = tempfile::tempdir_in(".").unwrap();
    let app = ValidatedAppPath::new(root.path()).unwrap();
    let mut input = VerifiedInputFile::open(&app, &entry("Contents/one.bundle", b"abc")).unwrap();
    let snapshot = VerifiedSnapshot::copy_once(&mut input, scratch.path(), 3).unwrap();
    assert_eq!(
        snapshot
            .with_verified_path(|path| fs::read(path))
            .unwrap()
            .unwrap(),
        b"abc"
    );
    assert!(
        snapshot
            .with_verified_path(|path| fs::metadata(path).unwrap().permissions().readonly())
            .unwrap()
    );
    assert_eq!(fs::read_dir(scratch.path()).unwrap().count(), 1);
    drop(snapshot);
    assert_eq!(fs::read_dir(scratch.path()).unwrap().count(), 0);
}

#[test]
fn candidate_report_is_deterministic_and_keeps_reviewed_config_untouched() {
    use mgwt_voice_dataset_builder::{
        inventory::{InventoryEntry, InventorySnapshot, write_candidate_reports},
        paths::ValidatedOutputPath,
    };
    let app_root = tempfile::tempdir_in(".").unwrap();
    let app = ValidatedAppPath::new(app_root.path()).unwrap();
    let out_root = tempfile::tempdir_in(".").unwrap();
    let destination = out_root.path().join("candidate");
    let bound = ValidatedOutputPath::new(&destination, &app).unwrap();
    let before = fs::read("config/manosaba-inputs-v1.json").unwrap();
    let snapshot = InventorySnapshot {
        entries: vec![InventoryEntry {
            relative_path: "Contents/one.bundle".into(),
            size: 3,
            sha256: hex::encode(sha2::Sha256::digest(b"abc")),
        }],
    };
    write_candidate_reports(&bound, &snapshot).unwrap();
    assert_eq!(
        fs::read(destination.join("candidate-inventory.json")).unwrap(),
        snapshot.canonical_bytes().unwrap()
    );
    assert_eq!(fs::read("config/manosaba-inputs-v1.json").unwrap(), before);
}

#[test]
fn candidate_inspection_reports_missing_added_and_changed_consumed_files() {
    use mgwt_voice_dataset_builder::inventory::inspect_entries;
    let root = tempfile::tempdir_in(".").unwrap();
    let folder = root
        .path()
        .join("Contents/Resources/Data/StreamingAssets/aa/StandaloneOSX");
    fs::create_dir_all(&folder).unwrap();
    fs::write(folder.join("naninovel-scripts_one.bundle"), b"changed").unwrap();
    fs::write(folder.join("general-voice-added.bundle"), b"extra").unwrap();
    let app = ValidatedAppPath::new(root.path()).unwrap();
    let approved = [
        entry(
            "Contents/Resources/Data/StreamingAssets/aa/StandaloneOSX/naninovel-scripts_one.bundle",
            b"original",
        ),
        entry(
            "Contents/Resources/Data/StreamingAssets/aa/StandaloneOSX/general-voice_missing.bundle",
            b"gone",
        ),
    ];
    let result = inspect_entries(&app, &approved).unwrap();
    assert_eq!(
        result.added,
        vec!["Contents/Resources/Data/StreamingAssets/aa/StandaloneOSX/general-voice-added.bundle"]
    );
    assert_eq!(
        result.missing,
        vec![
            "Contents/Resources/Data/StreamingAssets/aa/StandaloneOSX/general-voice_missing.bundle"
        ]
    );
    assert_eq!(
        result.changed,
        vec![
            "Contents/Resources/Data/StreamingAssets/aa/StandaloneOSX/naninovel-scripts_one.bundle"
        ]
    );
    assert_eq!(result.inventory.entries.len(), 2);
}

#[test]
fn inspect_diagnostic_records_drift_in_separate_file() {
    use mgwt_voice_dataset_builder::{
        inventory::{CandidateInspection, InventorySnapshot, write_inspection_reports},
        paths::ValidatedOutputPath,
    };
    let root = tempfile::tempdir_in(".").unwrap();
    let app = ValidatedAppPath::new(root.path()).unwrap();
    let out = tempfile::tempdir_in(".").unwrap();
    let location = out.path().join("candidate");
    let bound = ValidatedOutputPath::new(&location, &app).unwrap();
    let report = CandidateInspection {
        inventory: InventorySnapshot { entries: vec![] },
        added: vec!["added".into()],
        missing: vec![],
        changed: vec![],
    };
    write_inspection_reports(&bound, &report).unwrap();
    let diagnostic: serde_json::Value =
        serde_json::from_slice(&fs::read(location.join("diagnostic.json")).unwrap()).unwrap();
    assert_eq!(diagnostic["added"], serde_json::json!(["added"]));
    assert_eq!(diagnostic["status"], "drift");
}

#[test]
fn verified_handle_rejects_same_bytes_replacement_before_handoff() {
    let root = tempfile::tempdir_in(".").unwrap();
    fs::create_dir(root.path().join("Contents")).unwrap();
    let path = root.path().join("Contents/one.bundle");
    fs::write(&path, b"abc").unwrap();
    let app = ValidatedAppPath::new(root.path()).unwrap();
    let mut verified =
        VerifiedInputFile::open(&app, &entry("Contents/one.bundle", b"abc")).unwrap();
    fs::rename(&path, root.path().join("Contents/old.bundle")).unwrap();
    fs::write(&path, b"abc").unwrap();
    assert!(verified.recheck().is_err());
}

#[test]
fn snapshot_requires_exact_reservation_and_cleans_on_unwind() {
    use mgwt_voice_dataset_builder::inventory::VerifiedSnapshot;
    let root = tempfile::tempdir_in(".").unwrap();
    fs::create_dir(root.path().join("Contents")).unwrap();
    fs::write(root.path().join("Contents/one.bundle"), b"abc").unwrap();
    let scratch = tempfile::tempdir_in(".").unwrap();
    let app = ValidatedAppPath::new(root.path()).unwrap();
    let mut input = VerifiedInputFile::open(&app, &entry("Contents/one.bundle", b"abc")).unwrap();
    assert!(VerifiedSnapshot::copy_once(&mut input, scratch.path(), 2).is_err());
    let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let _snapshot = VerifiedSnapshot::copy_once(&mut input, scratch.path(), 3).unwrap();
        panic!("test unwind");
    }));
    assert_eq!(fs::read_dir(scratch.path()).unwrap().count(), 0);
}

#[test]
fn snapshot_rejects_scratch_inside_the_bound_app() {
    use mgwt_voice_dataset_builder::inventory::VerifiedSnapshot;
    let root = tempfile::tempdir_in(".").unwrap();
    fs::create_dir(root.path().join("Contents")).unwrap();
    fs::write(root.path().join("Contents/one.bundle"), b"abc").unwrap();
    let app = ValidatedAppPath::new(root.path()).unwrap();
    let mut input = VerifiedInputFile::open(&app, &entry("Contents/one.bundle", b"abc")).unwrap();
    assert!(VerifiedSnapshot::copy_once(&mut input, root.path(), 3).is_err());
}

#[test]
fn bundle_snapshot_reserves_exact_bundle_and_resource_sum() {
    use mgwt_voice_dataset_builder::inventory::VerifiedBundleInputs;
    let root = tempfile::tempdir_in(".").unwrap();
    fs::create_dir(root.path().join("Contents")).unwrap();
    fs::write(root.path().join("Contents/one.bundle"), b"abc").unwrap();
    fs::write(root.path().join("Contents/resource"), b"data").unwrap();
    let scratch = tempfile::tempdir_in(".").unwrap();
    let app = ValidatedAppPath::new(root.path()).unwrap();
    let bundle = VerifiedInputFile::open(&app, &entry("Contents/one.bundle", b"abc")).unwrap();
    let resource = VerifiedInputFile::open(&app, &entry("Contents/resource", b"data")).unwrap();
    let mut inputs = VerifiedBundleInputs {
        bundle,
        resources: vec![resource],
    };
    let snapshots = inputs.snapshot_set(scratch.path()).unwrap();
    assert_eq!(snapshots.reserved_bytes, 7);
    assert_eq!(
        snapshots
            .bundle
            .with_verified_path(|path| fs::read(path))
            .unwrap()
            .unwrap(),
        b"abc"
    );
    assert_eq!(
        snapshots.resources[0]
            .with_verified_path(|path| fs::read(path))
            .unwrap()
            .unwrap(),
        b"data"
    );
    assert_eq!(fs::read_dir(scratch.path()).unwrap().count(), 2);
    drop(snapshots);
    assert_eq!(fs::read_dir(scratch.path()).unwrap().count(), 0);
}

#[test]
fn candidate_publication_rejects_a_swapped_requested_parent() {
    use mgwt_voice_dataset_builder::{
        inventory::{InventorySnapshot, write_candidate_reports},
        paths::ValidatedOutputPath,
    };
    let app_root = tempfile::tempdir_in(".").unwrap();
    let app = ValidatedAppPath::new(app_root.path()).unwrap();
    let outside = tempfile::tempdir_in(".").unwrap();
    let parent = outside.path().join("parent");
    fs::create_dir(&parent).unwrap();
    let requested = parent.join("candidate");
    let bound = ValidatedOutputPath::new(&requested, &app).unwrap();
    fs::rename(&parent, outside.path().join("moved")).unwrap();
    fs::create_dir(&parent).unwrap();
    let snapshot = InventorySnapshot { entries: vec![] };
    assert!(write_candidate_reports(&bound, &snapshot).is_err());
    assert!(!requested.exists());
}

#[test]
fn verified_input_cannot_be_handed_directly_to_a_parser() {
    trybuild::TestCases::new().compile_fail("tests/ui/verified_input_not_reader.rs");
}

#[test]
fn snapshot_creation_stays_on_bound_scratch_after_path_swap() {
    use mgwt_voice_dataset_builder::{
        error::AppError,
        inventory::{SnapshotPhase, VerifiedSnapshot},
    };
    let root = tempfile::tempdir_in(".").unwrap();
    fs::create_dir(root.path().join("Contents")).unwrap();
    fs::write(root.path().join("Contents/one.bundle"), b"abc").unwrap();
    let app = ValidatedAppPath::new(root.path()).unwrap();
    let mut input = VerifiedInputFile::open(&app, &entry("Contents/one.bundle", b"abc")).unwrap();
    let scratch = tempfile::tempdir_in(".").unwrap();
    let old = scratch.path().to_path_buf();
    let moved = old.with_extension("moved");
    let mut saw_stage = false;
    let result = VerifiedSnapshot::copy_once_with_checkpoint(&mut input, &old, 3, |phase| {
        match phase {
            SnapshotPhase::ScratchBound => {
                fs::rename(&old, &moved).unwrap();
                std::os::unix::fs::symlink(root.path(), &old).unwrap();
            }
            SnapshotPhase::StageCreated => {
                saw_stage = true;
                assert!(fs::read_dir(&moved).unwrap().next().is_some());
                assert_eq!(fs::read_dir(root.path()).unwrap().count(), 1);
                return Err(AppError::processing("test interruption"));
            }
            _ => {}
        }
        Ok(())
    });
    assert!(result.is_err());
    assert!(saw_stage);
    assert_eq!(fs::read_dir(root.path()).unwrap().count(), 1);
    assert_eq!(fs::read_dir(&moved).unwrap().count(), 0);
    fs::remove_file(&old).unwrap();
    fs::remove_dir(&moved).unwrap();
}

#[test]
fn snapshot_rejects_growth_without_exceeding_reserved_bytes() {
    use mgwt_voice_dataset_builder::inventory::{SnapshotPhase, VerifiedSnapshot};
    let root = tempfile::tempdir_in(".").unwrap();
    fs::create_dir(root.path().join("Contents")).unwrap();
    let path = root.path().join("Contents/one.bundle");
    let original = vec![b'a'; 128 * 1024];
    fs::write(&path, &original).unwrap();
    let app = ValidatedAppPath::new(root.path()).unwrap();
    let mut input =
        VerifiedInputFile::open(&app, &entry("Contents/one.bundle", &original)).unwrap();
    let scratch = tempfile::tempdir_in(".").unwrap();
    let mut appended = false;
    let mut peak = 0;
    let result = VerifiedSnapshot::copy_once_with_checkpoint(
        &mut input,
        scratch.path(),
        original.len() as u64,
        |phase| {
            if let SnapshotPhase::Copied(bytes) = phase {
                let stage = fs::read_dir(scratch.path())
                    .unwrap()
                    .next()
                    .unwrap()
                    .unwrap()
                    .path();
                let copied_file = fs::read_dir(stage).unwrap().next().unwrap().unwrap().path();
                peak = peak.max(fs::metadata(copied_file).unwrap().len());
                assert!(bytes <= original.len() as u64);
                if !appended {
                    use std::io::Write;
                    std::fs::OpenOptions::new()
                        .append(true)
                        .open(&path)
                        .unwrap()
                        .write_all(b"z")
                        .unwrap();
                    appended = true;
                }
            }
            Ok(())
        },
    );
    assert!(appended);
    assert!(result.is_err());
    assert!(peak <= original.len() as u64);
    assert_eq!(fs::read_dir(scratch.path()).unwrap().count(), 0);
}

#[test]
fn immutable_snapshot_survives_post_verification_source_mutation() {
    use mgwt_voice_dataset_builder::inventory::VerifiedSnapshot;
    let root = tempfile::tempdir_in(".").unwrap();
    fs::create_dir(root.path().join("Contents")).unwrap();
    let source = root.path().join("Contents/one.bundle");
    fs::write(&source, b"abc").unwrap();
    let app = ValidatedAppPath::new(root.path()).unwrap();
    let mut input = VerifiedInputFile::open(&app, &entry("Contents/one.bundle", b"abc")).unwrap();
    let scratch = tempfile::tempdir_in(".").unwrap();
    let snapshot = VerifiedSnapshot::copy_once(&mut input, scratch.path(), 3).unwrap();
    fs::write(&source, b"abd").unwrap();
    assert_eq!(
        snapshot
            .with_verified_path(|path| fs::read(path))
            .unwrap()
            .unwrap(),
        b"abc"
    );
}

#[test]
fn bundle_snapshot_rejects_resources_from_another_app_root() {
    use mgwt_voice_dataset_builder::inventory::VerifiedBundleInputs;
    let one = tempfile::tempdir_in(".").unwrap();
    let two = tempfile::tempdir_in(".").unwrap();
    fs::create_dir(one.path().join("Contents")).unwrap();
    fs::create_dir(two.path().join("Contents")).unwrap();
    fs::write(one.path().join("Contents/bundle"), b"abc").unwrap();
    fs::write(two.path().join("Contents/resource"), b"data").unwrap();
    let app_one = ValidatedAppPath::new(one.path()).unwrap();
    let app_two = ValidatedAppPath::new(two.path()).unwrap();
    let bundle = VerifiedInputFile::open(&app_one, &entry("Contents/bundle", b"abc")).unwrap();
    let resource = VerifiedInputFile::open(&app_two, &entry("Contents/resource", b"data")).unwrap();
    let mut group = VerifiedBundleInputs {
        bundle,
        resources: vec![resource],
    };
    let scratch = tempfile::tempdir_in(".").unwrap();
    assert!(group.snapshot_set(scratch.path()).is_err());
    assert_eq!(fs::read_dir(scratch.path()).unwrap().count(), 0);
}
