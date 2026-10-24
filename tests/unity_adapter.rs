use mgwt_voice_dataset_builder::unity::{
    PayloadBinding, UnityBackend, UnityFailureKind, UnityReader, resolve_payload_region,
};
use unity_rs_core::{loader::LoadedResource, source::Region};

#[test]
fn unity_errors_keep_five_typed_causes_and_stable_exit_codes() {
    use mgwt_voice_dataset_builder::error::{AppError, ExitClass, UnityErrorKind};
    for (kind, exit) in [
        (UnityErrorKind::ApprovedInput, ExitClass::Validation),
        (UnityErrorKind::CandidateDecode, ExitClass::Processing),
        (
            UnityErrorKind::UnsupportedLayoutOrCodec,
            ExitClass::Processing,
        ),
        (UnityErrorKind::Io, ExitClass::Processing),
        (UnityErrorKind::Unexpected, ExitClass::Processing),
    ] {
        let error = AppError::unity(kind, "bounded test");
        assert_eq!(error.unity_kind(), Some(kind));
        assert_eq!(error.exit_class(), exit);
    }
}

fn resource(path: &str, bytes: &'static [u8]) -> LoadedResource {
    LoadedResource {
        path: path.into(),
        region: Region::from_bytes(bytes.to_vec()),
    }
}

#[test]
fn archive_uri_binds_only_the_exact_owning_bundle_entry() {
    const CAB: &str = "0123456789abcdef0123456789abcdef";
    let name = format!("CAB-{CAB}.resource");
    let uri = format!("archive:/CAB-{CAB}/{name}");
    let resources = vec![
        resource(&format!("other.bundle::{name}"), b"earlier-wrong"),
        resource(&format!("voice.bundle::{name}"), b"abcdWAVxyz"),
    ];
    let (binding, region) =
        resolve_payload_region(&uri, 4, 3, None, "voice.bundle", &resources, &[]).unwrap();
    assert!(matches!(binding, PayloadBinding::BundleResource { .. }));
    assert_eq!(region.read_to_vec(3).unwrap(), b"WAV");

    for bad in [
        format!("archive:/CAB-{}x/{name}", &CAB[..31]),
        format!("archive:/CAB-{CAB}/CAB-ffffffffffffffffffffffffffffffff.resource"),
        format!("archive:/cab-{CAB}/{name}"),
        format!("archive:/CAB-{CAB}/{name}/trailing"),
    ] {
        assert_eq!(
            resolve_payload_region(&bad, 0, 1, None, "voice.bundle", &resources, &[])
                .unwrap_err()
                .kind,
            UnityFailureKind::UnsupportedLayout
        );
    }
    assert_eq!(
        resolve_payload_region(&uri, 0, 1, None, "absent.bundle", &resources, &[])
            .unwrap_err()
            .kind,
        UnityFailureKind::ApprovedInput
    );
    let mut duplicate = resources.clone();
    duplicate.push(resource(&format!("voice.bundle::{name}"), b"duplicate"));
    assert_eq!(
        resolve_payload_region(&uri, 0, 1, None, "voice.bundle", &duplicate, &[])
            .unwrap_err()
            .kind,
        UnityFailureKind::ApprovedInput
    );
}

#[test]
fn external_source_uses_exact_full_path_and_declared_range() {
    let resources = vec![
        resource("other/path/shared.resS", b"wrong-first"),
        resource("owned/path/shared.resS", b"xxxxRIGHTxxxx"),
    ];
    let (binding, selected) = resolve_payload_region(
        "owned/path/shared.resS",
        4,
        5,
        None,
        "voice.bundle",
        &resources,
        &["owned/path/shared.resS".into()],
    )
    .unwrap();
    assert!(matches!(binding, PayloadBinding::External { .. }));
    assert_eq!(selected.read_to_vec(5).unwrap(), b"RIGHT");
    for (source, offset, size, approved) in [
        ("shared.resS", 0, 1, vec!["shared.resS".into()]),
        ("owned/path/shared.resS", 4, 5, vec![]),
        (
            "owned/path/shared.resS",
            100,
            5,
            vec!["owned/path/shared.resS".into()],
        ),
    ] {
        assert!(
            resolve_payload_region(
                source,
                offset,
                size,
                None,
                "voice.bundle",
                &resources,
                &approved
            )
            .is_err()
        );
    }
    let mut duplicate = resources;
    duplicate.push(resource("owned/path/shared.resS", b"duplicate"));
    assert_eq!(
        resolve_payload_region(
            "owned/path/shared.resS",
            0,
            1,
            None,
            "voice.bundle",
            &duplicate,
            &["owned/path/shared.resS".into()]
        )
        .unwrap_err()
        .kind,
        UnityFailureKind::ApprovedInput
    );
}

#[test]
fn inline_region_starts_after_compression_integer_not_declared_offset() {
    let object = Region::from_bytes(b"metadataFSB5payload".to_vec());
    let (binding, region) =
        resolve_payload_region("", 0, 4, Some((&object, 8)), "voice.bundle", &[], &[]).unwrap();
    assert!(matches!(binding, PayloadBinding::Inline { .. }));
    assert_eq!(region.read_to_vec(4).unwrap(), b"FSB5");
    assert!(
        resolve_payload_region("", 30, 4, Some((&object, 8)), "voice.bundle", &[], &[]).is_err()
    );
    assert!(
        resolve_payload_region("", 0, 20, Some((&object, 8)), "voice.bundle", &[], &[]).is_err()
    );
}

#[test]
fn backend_rejects_unity_data_over_the_approved_bundle_limit_before_parsing() {
    use mgwt_voice_dataset_builder::{
        config::InputEntry,
        hash::sha256_bytes,
        inventory::{VerifiedBundleInputs, VerifiedInputFile},
        paths::ValidatedAppPath,
    };
    let app_dir = tempfile::tempdir_in(".").unwrap();
    std::fs::write(app_dir.path().join("large.bundle"), b"not-unity").unwrap();
    let app = ValidatedAppPath::new(app_dir.path()).unwrap();
    let bundle = VerifiedInputFile::open(
        &app,
        &InputEntry {
            relative_path: "large.bundle".into(),
            size: 9,
            sha256: sha256_bytes(b"not-unity"),
        },
    )
    .unwrap();
    let approved = VerifiedBundleInputs {
        bundle,
        resources: vec![],
    };
    let scratch = tempfile::tempdir_in(".").unwrap();
    let reader = UnityReader::with_bundle_limit(scratch.path(), 8);
    let error = reader.open_bundle(&approved).unwrap_err();
    assert_eq!(
        error.unity_kind(),
        Some(mgwt_voice_dataset_builder::error::UnityErrorKind::ApprovedInput)
    );
}
