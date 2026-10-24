//! Opt-in comparison with the approved real pair and ignored Python oracle.
use mgwt_voice_dataset_builder::{
    config::{InputEntry, checked_in_approval},
    inventory::{VerifiedBundleInputs, VerifiedInputFile, inventory_app},
    model::{AudioLimits, CodecKind, CodecLayoutSignature, FrameCountSource, PredecodeFrameCount},
    paths::ValidatedAppPath,
    unity::{BundleSession, PayloadBinding, UnityBackend, UnityReader},
};
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::{
    collections::{BTreeMap, BTreeSet},
    ffi::OsString,
    path::{Path, PathBuf},
    process::Command,
};

fn test_wav(samples: &[i16]) -> Vec<u8> {
    assert_eq!(samples.len() % 2, 0);
    let size = u32::try_from(samples.len() * 2).unwrap();
    let mut wav = Vec::new();
    wav.extend_from_slice(b"RIFF");
    wav.extend_from_slice(&(size + 36).to_le_bytes());
    wav.extend_from_slice(b"WAVEfmt ");
    wav.extend_from_slice(&16_u32.to_le_bytes());
    wav.extend_from_slice(&1_u16.to_le_bytes());
    wav.extend_from_slice(&2_u16.to_le_bytes());
    wav.extend_from_slice(&48_000_u32.to_le_bytes());
    wav.extend_from_slice(&192_000_u32.to_le_bytes());
    wav.extend_from_slice(&4_u16.to_le_bytes());
    wav.extend_from_slice(&16_u16.to_le_bytes());
    wav.extend_from_slice(b"data");
    wav.extend_from_slice(&size.to_le_bytes());
    for sample in samples {
        wav.extend_from_slice(&sample.to_le_bytes());
    }
    wav
}

#[derive(Debug)]
struct PcmMetrics {
    samples: usize,
    differing: usize,
    max_absolute_delta: i32,
    mean_absolute_delta: f64,
    rms_delta: f64,
}

fn parse_pcm16(wav: &[u8]) -> Result<(u16, u32, Vec<i16>), String> {
    if wav.len() < 44
        || &wav[0..4] != b"RIFF"
        || &wav[8..16] != b"WAVEfmt "
        || &wav[36..40] != b"data"
    {
        return Err("noncanonical RIFF layout".into());
    }
    let u16_at = |at: usize| u16::from_le_bytes(wav[at..at + 2].try_into().unwrap());
    let u32_at = |at: usize| u32::from_le_bytes(wav[at..at + 4].try_into().unwrap());
    let channels = u16_at(22);
    let rate = u32_at(24);
    let align = u16_at(32);
    if u32_at(16) != 16
        || u16_at(20) != 1
        || u16_at(34) != 16
        || channels == 0
        || rate == 0
        || align != channels * 2
        || u32_at(28) != rate * u32::from(align)
        || usize::try_from(u32_at(4)).unwrap() + 8 != wav.len()
        || usize::try_from(u32_at(40)).unwrap() + 44 != wav.len()
        || !(wav.len() - 44).is_multiple_of(usize::from(align))
    {
        return Err("invalid PCM16 WAV fields".into());
    }
    let samples = wav[44..]
        .as_chunks::<2>()
        .0
        .iter()
        .map(|pair| i16::from_le_bytes(*pair))
        .collect();
    Ok((channels, rate, samples))
}

fn compare_pcm(oracle: &[u8], rust: &[u8]) -> Result<PcmMetrics, String> {
    let (channels_a, rate_a, a) = parse_pcm16(oracle)?;
    let (channels_b, rate_b, b) = parse_pcm16(rust)?;
    if oracle[..44] != rust[..44]
        || channels_a != channels_b
        || rate_a != rate_b
        || channels_a != 2
        || rate_a != 48_000
        || a.len() != b.len()
        || a.is_empty()
    {
        return Err("PCM headers, ordering, or length differ".into());
    }
    let n = a.len();
    let mut differing = 0_usize;
    let mut maximum = 0_i32;
    let mut absolute_sum = 0_u64;
    let mut square_sum = 0_u64;
    for (&left, &right) in a.iter().zip(&b) {
        let delta = (i32::from(left) - i32::from(right)).abs();
        if delta != 0 {
            differing += 1;
        }
        maximum = maximum.max(delta);
        absolute_sum += u64::try_from(delta).unwrap();
        square_sum += u64::try_from(delta * delta).unwrap();
    }
    let metrics = PcmMetrics {
        samples: n,
        differing,
        max_absolute_delta: maximum,
        mean_absolute_delta: absolute_sum as f64 / n as f64,
        rms_delta: (square_sum as f64 / n as f64).sqrt(),
    };
    if maximum > 1
        || differing * 20 > n
        || absolute_sum * 20 > n as u64
        || square_sum * 16 > n as u64
    {
        return Err("PCM sample thresholds exceeded".into());
    }
    // Whole-frame, no-wrap/no-pad, full-common-overlap diagnostic. An early exit
    // is valid once a lag can no longer beat zero's exact mean-square score.
    let channels = usize::from(channels_a);
    let frames = n / channels;
    for lag in -1024_i32..=1024 {
        if lag == 0 {
            continue;
        }
        let distance = lag.unsigned_abs() as usize;
        if distance >= frames {
            continue;
        }
        let overlap_samples = (frames - distance) * channels;
        let (a_start, b_start) = if lag > 0 {
            (distance * channels, 0)
        } else {
            (0, distance * channels)
        };
        let threshold = u128::from(square_sum) * overlap_samples as u128;
        let mut lag_square = 0_u128;
        for offset in 0..overlap_samples {
            let delta = i32::from(a[a_start + offset]) - i32::from(b[b_start + offset]);
            lag_square += (delta as i64 * delta as i64) as u128;
            if lag_square * n as u128 >= threshold {
                break;
            }
        }
        if lag_square * (n as u128) < threshold {
            return Err(format!(
                "nonzero whole-frame lag {lag} scores lower than zero"
            ));
        }
    }
    Ok(metrics)
}

#[test]
fn pcm_equivalence_rejects_shift_swap_wrong_clip_and_two_lsb() {
    let original: Vec<i16> = (0..100)
        .flat_map(|frame| [frame * 3 + 100, -frame * 7 - 200])
        .collect();
    let oracle = test_wav(&original);
    let mut close_samples = original.clone();
    close_samples[10] += 1;
    let close = test_wav(&close_samples);
    assert!(compare_pcm(&oracle, &close).is_ok());
    let mut shifted = original[2..].to_vec();
    shifted.extend_from_slice(&[0, 0]);
    let mut swapped = original.clone();
    for frame in swapped.as_chunks_mut::<2>().0 {
        frame.swap(0, 1);
    }
    let wrong: Vec<i16> = original.iter().map(|sample| -*sample).collect();
    let mut two_lsb = original.clone();
    two_lsb[10] += 2;
    for bad in [
        test_wav(&shifted),
        test_wav(&swapped),
        test_wav(&wrong),
        test_wav(&two_lsb),
    ] {
        assert!(compare_pcm(&oracle, &bad).is_err());
    }
}

fn approved_pair(
    approval: &mgwt_voice_dataset_builder::config::ApprovalUnit,
) -> (
    &mgwt_voice_dataset_builder::config::BundleEntry,
    &mgwt_voice_dataset_builder::config::BundleEntry,
) {
    let script = approval
        .manifest
        .bundles
        .iter()
        .find(|b| {
            b.relative_path
                .contains("naninovel-scripts_assets_naninovelscripts-act01_chapter01_trial")
        })
        .unwrap();
    let voice = approval
        .manifest
        .bundles
        .iter()
        .find(|b| {
            b.relative_path
                .contains("general-voice-act01_chapter01_trial_assets_all")
        })
        .unwrap();
    (script, voice)
}

fn verified_bundle(
    app: &ValidatedAppPath,
    bundle: &mgwt_voice_dataset_builder::config::BundleEntry,
) -> VerifiedBundleInputs {
    VerifiedBundleInputs {
        bundle: VerifiedInputFile::open(
            app,
            &InputEntry {
                relative_path: bundle.relative_path.clone(),
                size: bundle.size,
                sha256: bundle.sha256.clone(),
            },
        )
        .unwrap(),
        resources: vec![],
    }
}

const PYTHON_REPRODUCE: &str = r#"
import hashlib, json, sys
from pathlib import Path
project, app, oracle_path, bundle_rel, expected_hash, output = map(Path, sys.argv[1:])
sys.path.insert(0, str(project / 'src'))
from manosaba_dataset.audio import canonicalize_wav, canonical_audio_sha256, validate_wav
from manosaba_dataset.inventory import inventory_app, verify_expected_input_manifest
from manosaba_dataset.unity_assets import iter_audio_refs, UnityAudioDecoder
inventory = inventory_app(app)
verify_expected_input_manifest(inventory, project / 'config/manosaba-inputs-v1.json')
bundle = next(b for b in inventory.bundles if b.relative_path == str(bundle_rel))
assert bundle.sha256 == str(expected_hash)
oracle = json.loads(oracle_path.read_bytes())
shapes = {int(item['path_id']): item for item in oracle['decoded_shapes']}
assert len(shapes) == 3
refs = {r.path_id: r for r in iter_audio_refs(app / bundle_rel) if r.path_id in shapes}
assert refs.keys() == shapes.keys()
decoder = UnityAudioDecoder(app, inventory)
for path_id, baseline in shapes.items():
    canonical, props = canonicalize_wav(decoder.decode(refs[path_id]).wav_bytes)
    assert validate_wav(canonical) == props
    assert canonical_audio_sha256(canonical) == baseline['canonical_sha256']
    assert len(canonical) == baseline['canonical_wav_bytes']
    assert props.channels == baseline['channels']
    assert props.sample_rate == baseline['sample_rate']
    assert props.frame_count == baseline['frame_count']
    (output / f'{path_id}.wav').write_bytes(canonical)
print(json.dumps({'reproduced': len(shapes), 'bundle_sha256': bundle.sha256}))
"#;

#[derive(Debug)]
struct PythonOraclePaths {
    project: PathBuf,
    app: PathBuf,
    oracle: PathBuf,
    bin: PathBuf,
}

fn required_python_paths(
    lookup: impl Fn(&str) -> Option<OsString>,
) -> Result<PythonOraclePaths, &'static str> {
    Ok(PythonOraclePaths {
        project: lookup("MGWT_PYTHON_PROJECT_PATH")
            .ok_or("MGWT_PYTHON_PROJECT_PATH")?
            .into(),
        app: lookup("MGWT_PYTHON_APP_PATH")
            .ok_or("MGWT_PYTHON_APP_PATH")?
            .into(),
        oracle: lookup("MGWT_PYTHON_ORACLE_PATH")
            .ok_or("MGWT_PYTHON_ORACLE_PATH")?
            .into(),
        bin: lookup("MGWT_PYTHON_BIN").ok_or("MGWT_PYTHON_BIN")?.into(),
    })
}

#[test]
fn python_oracle_paths_require_each_explicit_environment_value() {
    let names = [
        "MGWT_PYTHON_PROJECT_PATH",
        "MGWT_PYTHON_APP_PATH",
        "MGWT_PYTHON_ORACLE_PATH",
        "MGWT_PYTHON_BIN",
    ];
    for missing in names {
        let error = required_python_paths(|name| {
            (name != missing).then(|| std::ffi::OsString::from("present"))
        })
        .unwrap_err();
        assert_eq!(error, missing);
    }
    let paths = required_python_paths(|_| Some(std::ffi::OsString::from("present"))).unwrap();
    assert_eq!(paths.project, Path::new("present"));
    assert_eq!(paths.app, Path::new("present"));
    assert_eq!(paths.oracle, Path::new("present"));
    assert_eq!(paths.bin, Path::new("present"));
}

#[test]
#[ignore = "requires MGWT_APP_PATH and four explicit MGWT_PYTHON_* paths"]
fn approved_pair_and_pcm_compatibility_gate() {
    let python_paths = required_python_paths(|name| std::env::var_os(name))
        .unwrap_or_else(|name| panic!("{name} required"));
    let app_os = std::env::var_os("MGWT_APP_PATH").expect("MGWT_APP_PATH required");
    let app = ValidatedAppPath::new(Path::new(&app_os)).unwrap();
    let approval = checked_in_approval().unwrap();
    let pre_inventory = inventory_app(&app, &approval.manifest).unwrap();
    assert_eq!(pre_inventory.entries.len(), 62);
    let (script_entry, voice_entry) = approved_pair(&approval);
    let scratch = tempfile::tempdir().unwrap();
    let reader = UnityReader::new(scratch.path());
    let mut scripts = reader
        .open_bundle(&verified_bundle(&app, script_entry))
        .unwrap();
    let script_objects = scripts.raw_script_objects().unwrap();
    assert_eq!(script_objects.len(), 17);
    assert_eq!(scripts.metrics().bundle_opens, 1);
    scripts.finish().unwrap();

    let oracle: Value =
        serde_json::from_slice(&std::fs::read(&python_paths.oracle).unwrap()).unwrap();
    assert_eq!(oracle["probe"]["unity_version"], "6000.0.48f1");
    assert_eq!(oracle["probe"]["observed_counts"]["script_objects"], 17);
    assert_eq!(oracle["probe"]["observed_counts"]["audio_assets"], 1_753);
    let temporary_wavs = tempfile::tempdir().unwrap();
    let output = Command::new(&python_paths.bin)
        .arg("-c")
        .arg(PYTHON_REPRODUCE)
        .arg(&python_paths.project)
        .arg(&python_paths.app)
        .arg(&python_paths.oracle)
        .arg(&voice_entry.relative_path)
        .arg(&voice_entry.sha256)
        .arg(temporary_wavs.path())
        .output()
        .unwrap();
    assert!(output.status.success(), "Python oracle reproduction failed");
    let reproduction: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(reproduction["reproduced"], 3);
    assert_eq!(reproduction["bundle_sha256"], voice_entry.sha256);

    let mut session = reader
        .open_bundle(&verified_bundle(&app, voice_entry))
        .unwrap();
    let refs = session.audio_refs().unwrap();
    assert_eq!(refs.len(), 1_753);
    assert!(
        refs.windows(2)
            .all(|pair| pair[0].path_id < pair[1].path_id)
    );
    assert!(
        refs.iter()
            .all(|r| matches!(r.binding, PayloadBinding::BundleResource { .. }))
    );
    assert!(refs.iter().all(|r| r.metadata.subsound_index == 0));
    assert!(refs.iter().all(|r| matches!(
        r.predecode_frames,
        PredecodeFrameCount::ValidatedPayloadHeader(_)
    )));
    assert_eq!(session.metrics().bundle_opens, 1);
    struct FailingSink;
    impl std::io::Write for FailingSink {
        fn write(&mut self, _: &[u8]) -> std::io::Result<usize> {
            Err(std::io::Error::other("controlled sink interruption"))
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }
    use mgwt_voice_dataset_builder::error::UnityErrorKind;
    let mut wrong_bundle = refs[0].clone();
    wrong_bundle.bundle_path = "other.bundle".into();
    assert_eq!(
        session
            .stream_audio(&wrong_bundle, &mut std::io::sink(), &AudioLimits::default())
            .unwrap_err()
            .unity_kind(),
        Some(UnityErrorKind::ApprovedInput)
    );
    let mut drift = refs[0].clone();
    drift.name.push('!');
    assert_eq!(
        session
            .stream_audio(&drift, &mut std::io::sink(), &AudioLimits::default())
            .unwrap_err()
            .unity_kind(),
        Some(UnityErrorKind::ApprovedInput)
    );
    assert_eq!(
        session
            .stream_audio(
                &refs[0],
                &mut std::io::sink(),
                &AudioLimits {
                    maximum_wav_bytes: 43,
                    maximum_expansion_ratio: 4096,
                }
            )
            .unwrap_err()
            .unity_kind(),
        Some(UnityErrorKind::UnsupportedLayoutOrCodec)
    );
    assert_eq!(
        session
            .stream_audio(
                &refs[0],
                &mut std::io::sink(),
                &AudioLimits {
                    maximum_wav_bytes: 256 * 1024 * 1024,
                    maximum_expansion_ratio: 0,
                }
            )
            .unwrap_err()
            .unity_kind(),
        Some(UnityErrorKind::UnsupportedLayoutOrCodec)
    );
    assert_eq!(
        session
            .stream_audio(&refs[0], &mut FailingSink, &AudioLimits::default())
            .unwrap_err()
            .unity_kind(),
        Some(UnityErrorKind::Io)
    );
    session
        .stream_audio(&refs[1], &mut std::io::sink(), &AudioLimits::default())
        .unwrap();
    let mut signatures = BTreeMap::<CodecLayoutSignature, i64>::new();
    for reference in &refs {
        signatures
            .entry(reference.signature)
            .or_insert(reference.path_id);
    }
    assert_eq!(
        signatures.len(),
        1,
        "new codec/layout signature needs independent reviewed evidence"
    );
    let observed_signature = *signatures.keys().next().unwrap();
    assert_eq!(observed_signature.codec, CodecKind::Fsb5Vorbis);
    assert_eq!(
        observed_signature.layout,
        mgwt_voice_dataset_builder::model::PayloadLayout::BundleResource
    );
    assert_eq!(observed_signature.bits_per_sample, 16);
    assert_eq!(observed_signature.sample_rate, 48_000);
    let oracle_ids: BTreeSet<_> = oracle["decoded_shapes"]
        .as_array()
        .unwrap()
        .iter()
        .map(|shape| shape["path_id"].as_i64().unwrap())
        .collect();
    let mut first_hashes = BTreeMap::new();
    let mut evidence = Vec::new();
    let limits = AudioLimits::default();
    for reference in refs.iter().filter(|r| {
        oracle_ids.contains(&r.path_id)
            || signatures
                .get(&r.signature)
                .is_some_and(|id| *id == r.path_id)
    }) {
        let mut rust_wav = Vec::new();
        let receipt = session
            .stream_audio(reference, &mut rust_wav, &limits)
            .unwrap();
        assert_eq!(receipt.wav_bytes as usize, rust_wav.len());
        assert!(receipt.peak_live_buffer_bytes > 44);
        assert!(receipt.peak_live_buffer_bytes <= 256 * 1024);
        assert!((receipt.peak_live_buffer_bytes as u64) < receipt.wav_bytes);
        assert!(receipt.wav_bytes <= limits.maximum_wav_bytes);
        assert!(matches!(
            receipt.frame_count_source,
            FrameCountSource::ValidatedPayloadHeaderAndWav
        ));
        assert_eq!(
            Sha256::digest(&rust_wav).as_slice(),
            hex::decode(&receipt.wav_sha256).unwrap()
        );
        first_hashes.insert(reference.path_id, receipt.wav_sha256);
        if oracle_ids.contains(&reference.path_id) {
            assert_eq!(reference.signature.codec, CodecKind::Fsb5Vorbis);
            let baseline = oracle["decoded_shapes"]
                .as_array()
                .unwrap()
                .iter()
                .find(|shape| shape["path_id"].as_i64() == Some(reference.path_id))
                .unwrap();
            let python_wav = std::fs::read(
                temporary_wavs
                    .path()
                    .join(format!("{}.wav", reference.path_id)),
            )
            .unwrap();
            assert_eq!(
                hex::encode(Sha256::digest(&python_wav)),
                baseline["canonical_sha256"]
            );
            assert_eq!(
                receipt.wav_bytes,
                baseline["canonical_wav_bytes"].as_u64().unwrap()
            );
            assert_eq!(
                receipt.channels as u64,
                baseline["channels"].as_u64().unwrap()
            );
            assert_eq!(receipt.sample_rate, 48_000);
            assert_eq!(
                receipt.sample_rate as u64,
                baseline["sample_rate"].as_u64().unwrap()
            );
            assert_eq!(
                receipt.frame_count,
                baseline["frame_count"].as_u64().unwrap()
            );
            let metrics = compare_pcm(&python_wav, &rust_wav).unwrap();
            evidence.push((
                metrics.samples,
                metrics.differing,
                metrics.max_absolute_delta,
                metrics.mean_absolute_delta,
                metrics.rms_delta,
            ));
        }
    }
    assert_eq!(evidence.len(), 3);
    let expected_ids: BTreeSet<_> = oracle_ids
        .union(&signatures.values().copied().collect())
        .copied()
        .collect();
    assert_eq!(first_hashes.len(), expected_ids.len());
    assert_eq!(session.metrics().bundle_opens, 1);
    assert!(session.metrics().peak_live_buffer_bytes <= 256 * 1024);
    session.finish().unwrap();
    // A fresh verified snapshot and UnityFS open must reproduce each WAV exactly.
    let mut second = reader
        .open_bundle(&verified_bundle(&app, voice_entry))
        .unwrap();
    for (path_id, first_hash) in &first_hashes {
        let reference = second
            .audio_refs()
            .unwrap()
            .into_iter()
            .find(|r| r.path_id == *path_id)
            .unwrap();
        let receipt = second
            .stream_audio(&reference, &mut std::io::sink(), &limits)
            .unwrap();
        assert_eq!(&receipt.wav_sha256, first_hash);
    }
    assert_eq!(second.metrics().bundle_opens, 1);
    let second_peak = second.metrics().peak_live_buffer_bytes;
    second.finish().unwrap();
    let post_inventory = inventory_app(&app, &approval.manifest).unwrap();
    assert_eq!(pre_inventory, post_inventory);
    eprintln!(
        "script_objects=17 audio_refs=1753 signatures={} oracle_reproduced=3 rust_repeat_hashes={} pcm_metrics={evidence:?} first_pass_open=1 second_pass_open=1 peak_sink_bytes={}",
        signatures.len(),
        first_hashes.len(),
        second_peak
    );
}
