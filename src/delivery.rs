//! Local dataset delivery, full readback verification, and safe promotion.

use std::collections::{BTreeMap, BTreeSet};
use std::fs::{self, File};
use std::io::{self, BufRead, BufReader, Read, Write};
use std::path::Path;

use arrow_array::{Array, BinaryArray, StringArray, StructArray};
use parquet::arrow::arrow_reader::ParquetRecordBatchReaderBuilder;
use rustix::fs::{RenameFlags, renameat_with};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};

use crate::parquet_writer::{
    InputRow, Shard, safe_audio_name, safe_source_path, safe_speaker, schema,
};

#[derive(Debug)]
pub struct PreparedDataset {
    pub rows: Vec<InputRow>,
    pub audit_records: Vec<Value>,
    pub rejected_records: Vec<Value>,
    pub producer_revision: String,
    pub approved_input_sha256: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct FileRecord {
    pub path: String,
    pub bytes: u64,
    pub sha256: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ShardRecord {
    pub path: String,
    pub rows: usize,
    pub embedded_audio_bytes: u64,
    pub parquet_bytes: u64,
    pub sha256: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Report {
    pub format_version: u32,
    pub producer_revision: String,
    pub approved_input_sha256: String,
    pub row_count: usize,
    pub embedded_audio_bytes: u64,
    pub audit_record_count: usize,
    pub rejected_record_count: usize,
    pub shards: Vec<ShardRecord>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Manifest {
    pub format_version: u32,
    /// Every publishable file except this manifest, to avoid self-hashing.
    pub files: Vec<FileRecord>,
}

fn invalid(message: impl Into<String>) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message.into())
}
fn other(e: impl std::fmt::Display) -> io::Error {
    io::Error::other(e.to_string())
}

fn sha256_file(path: &Path) -> io::Result<String> {
    let mut file = File::open(path)?;
    let mut hasher = Sha256::new();
    let mut buf = [0u8; 1024 * 1024];
    loop {
        let n = file.read(&mut buf)?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
    }
    Ok(hex::encode(hasher.finalize()))
}

fn file_record(root: &Path, relative: &str) -> io::Result<FileRecord> {
    if !safe_source_path(relative) {
        return Err(invalid("unsafe file record path"));
    }
    let path = root.join(relative);
    let metadata = fs::symlink_metadata(&path)?;
    if !metadata.is_file() || metadata.file_type().is_symlink() {
        return Err(invalid("non-regular output file"));
    }
    Ok(FileRecord {
        path: relative.to_string(),
        bytes: metadata.len(),
        sha256: sha256_file(&path)?,
    })
}

fn write_json<T: Serialize>(path: &Path, value: &T) -> io::Result<()> {
    let mut file = File::create(path)?;
    serde_json::to_writer(&mut file, value).map_err(other)?;
    file.write_all(b"\n")?;
    file.sync_all()
}

fn write_jsonl(path: &Path, records: &[Value]) -> io::Result<()> {
    let mut file = File::create(path)?;
    let mut lines = records
        .iter()
        .map(|value| serde_json::to_vec(value).map_err(other))
        .collect::<io::Result<Vec<_>>>()?;
    lines.sort();
    for line in lines {
        file.write_all(&line)?;
        file.write_all(b"\n")?;
    }
    file.sync_all()
}

pub fn write_sidecars(
    root: &Path,
    prepared: &PreparedDataset,
    shards: &[Shard],
) -> io::Result<Report> {
    let metadata_dir = root.join("metadata");
    fs::create_dir_all(&metadata_dir)?;
    write_jsonl(&metadata_dir.join("audit.jsonl"), &prepared.audit_records)?;
    write_jsonl(
        &metadata_dir.join("rejected.jsonl"),
        &prepared.rejected_records,
    )?;
    fs::write(
        root.join("README.md"),
        "# Manosaba Japanese Voice Dataset\n\nColumns: audio, transcription, speaker, emotion, language, source_path. Audio is embedded PCM WAV.\n",
    )?;
    let mut summary = Vec::with_capacity(shards.len());
    for shard in shards {
        let record = file_record(root, &shard.relative_path)?;
        summary.push(ShardRecord {
            path: record.path,
            rows: shard.rows,
            embedded_audio_bytes: shard.embedded_audio_bytes,
            parquet_bytes: record.bytes,
            sha256: record.sha256,
        });
    }
    let row_count = summary.iter().try_fold(0usize, |n, s| {
        n.checked_add(s.rows)
            .ok_or_else(|| invalid("row count overflow"))
    })?;
    let embedded_audio_bytes = summary.iter().try_fold(0u64, |n, s| {
        n.checked_add(s.embedded_audio_bytes)
            .ok_or_else(|| invalid("audio count overflow"))
    })?;
    let report = Report {
        format_version: 1,
        producer_revision: prepared.producer_revision.clone(),
        approved_input_sha256: prepared.approved_input_sha256.clone(),
        row_count,
        embedded_audio_bytes,
        audit_record_count: prepared.audit_records.len(),
        rejected_record_count: prepared.rejected_records.len(),
        shards: summary,
    };
    write_json(&metadata_dir.join("report.json"), &report)?;
    let mut paths = vec![
        "README.md".to_string(),
        "metadata/audit.jsonl".into(),
        "metadata/rejected.jsonl".into(),
        "metadata/report.json".into(),
    ];
    paths.extend(shards.iter().map(|s| s.relative_path.clone()));
    paths.sort();
    let files = paths
        .iter()
        .map(|p| file_record(root, p))
        .collect::<io::Result<Vec<_>>>()?;
    write_json(
        &metadata_dir.join("manifest.json"),
        &Manifest {
            format_version: 1,
            files,
        },
    )?;
    Ok(report)
}

fn list_files(root: &Path, at: &Path, out: &mut BTreeSet<String>) -> io::Result<()> {
    for entry in fs::read_dir(at)? {
        let entry = entry?;
        let path = entry.path();
        let metadata = fs::symlink_metadata(&path)?;
        if metadata.file_type().is_symlink() {
            return Err(invalid("symlink in dataset"));
        }
        if metadata.is_dir() {
            list_files(root, &path, out)?;
        } else if metadata.is_file() {
            let relative = path
                .strip_prefix(root)
                .map_err(other)?
                .to_str()
                .ok_or_else(|| invalid("non-utf8 output path"))?
                .replace('\\', "/");
            out.insert(relative);
        } else {
            return Err(invalid("unexpected output type"));
        }
    }
    Ok(())
}

fn count_jsonl(path: &Path) -> io::Result<usize> {
    let mut count = 0;
    let mut prior: Option<String> = None;
    for line in BufReader::new(File::open(path)?).lines() {
        let line = line?;
        if line.is_empty() {
            return Err(invalid("empty JSONL record"));
        }
        if prior.as_ref().is_some_and(|p| p >= &line) {
            return Err(invalid("unsorted/duplicate JSONL record"));
        }
        let _: Value = serde_json::from_str(&line).map_err(other)?;
        prior = Some(line);
        count += 1;
    }
    Ok(count)
}

/// Validates the strict canonical PCM WAV layout specified by the design.
fn canonical_wav(bytes: &[u8]) -> io::Result<()> {
    if bytes.len() < 46
        || &bytes[..4] != b"RIFF"
        || &bytes[8..16] != b"WAVEfmt "
        || &bytes[36..40] != b"data"
    {
        return Err(invalid("non-canonical WAV header"));
    }
    let u16at = |i| u16::from_le_bytes([bytes[i], bytes[i + 1]]);
    let u32at = |i| u32::from_le_bytes(bytes[i..i + 4].try_into().expect("4-byte slice"));
    if u32at(4) as usize != bytes.len() - 8 || u32at(16) != 16 || u16at(20) != 1 {
        return Err(invalid("non-canonical WAV RIFF/fmt"));
    }
    let channels = u16at(22) as u32;
    let rate = u32at(24);
    let byte_rate = u32at(28);
    let block = u16at(32) as u32;
    let bits = u16at(34) as u32;
    let data = u32at(40) as usize;
    if channels == 0
        || rate == 0
        || ![8, 16, 24, 32].contains(&bits)
        || data == 0
        || block
            != channels
                .checked_mul(bits / 8)
                .ok_or_else(|| invalid("WAV block overflow"))?
        || byte_rate
            != rate
                .checked_mul(block)
                .ok_or_else(|| invalid("WAV rate overflow"))?
        || !data.is_multiple_of(block as usize)
        || bytes.len() != 44 + data + (data & 1)
        || (data & 1 == 1 && bytes[bytes.len() - 1] != 0)
    {
        return Err(invalid("non-canonical WAV data"));
    }
    Ok(())
}

pub fn verify_dataset(root: &Path) -> io::Result<Report> {
    let manifest: Manifest =
        serde_json::from_reader(File::open(root.join("metadata/manifest.json"))?).map_err(other)?;
    let report: Report =
        serde_json::from_reader(File::open(root.join("metadata/report.json"))?).map_err(other)?;
    if manifest.format_version != 1 || report.format_version != 1 {
        return Err(invalid("unknown format version"));
    }
    if report.producer_revision.is_empty()
        || report.approved_input_sha256.len() != 64
        || !report
            .approved_input_sha256
            .bytes()
            .all(|b| b.is_ascii_hexdigit())
    {
        return Err(invalid("missing producer/input identity"));
    }
    let mut expected = BTreeSet::from(["metadata/manifest.json".to_string()]);
    let mut previous = None;
    for record in &manifest.files {
        if !safe_source_path(&record.path)
            || previous
                .as_ref()
                .is_some_and(|p: &String| p >= &record.path)
        {
            return Err(invalid("unsorted/unsafe manifest"));
        }
        previous = Some(record.path.clone());
        expected.insert(record.path.clone());
        if &file_record(root, &record.path)? != record {
            return Err(invalid("file digest/size mismatch"));
        }
    }
    let mut actual = BTreeSet::new();
    list_files(root, root, &mut actual)?;
    if actual != expected {
        return Err(invalid("unexpected/missing dataset file"));
    }
    let required = [
        "README.md",
        "metadata/audit.jsonl",
        "metadata/rejected.jsonl",
        "metadata/report.json",
    ];
    if required.iter().any(|p| !expected.contains(*p)) {
        return Err(invalid("missing sidecar"));
    }
    if count_jsonl(&root.join("metadata/audit.jsonl"))? != report.audit_record_count
        || count_jsonl(&root.join("metadata/rejected.jsonl"))? != report.rejected_record_count
    {
        return Err(invalid("sidecar record count mismatch"));
    }
    let expected_shards: BTreeSet<String> = report.shards.iter().map(|s| s.path.clone()).collect();
    if expected_shards.len() != report.shards.len()
        || expected
            .difference(&BTreeSet::from([
                "metadata/manifest.json".to_string(),
                "README.md".into(),
                "metadata/audit.jsonl".into(),
                "metadata/rejected.jsonl".into(),
                "metadata/report.json".into(),
            ]))
            .cloned()
            .collect::<BTreeSet<_>>()
            != expected_shards
    {
        return Err(invalid("shard file set mismatch"));
    }
    let mut total_rows = 0;
    let mut total_audio = 0u64;
    let mut prior_key: Option<(String, String, String)> = None;
    let mut observed_names = BTreeSet::new();
    let mut speaker_shard_counts: BTreeMap<String, usize> = BTreeMap::new();
    for shard in &report.shards {
        let parts: Vec<_> = shard.path.split('/').collect();
        if parts.len() != 4 || parts[0] != "data" || parts[1] != "ja" || !safe_speaker(parts[2]) {
            return Err(invalid("unsafe shard path"));
        }
        *speaker_shard_counts
            .entry(parts[2].to_string())
            .or_default() += 1;
    }
    let mut speaker_seen: BTreeMap<String, usize> = BTreeMap::new();
    for shard in &report.shards {
        let parts: Vec<_> = shard.path.split('/').collect();
        let speaker = parts[2];
        let number = speaker_seen.entry(speaker.to_string()).or_default();
        let expected_name = format!(
            "train-{number:05}-of-{:05}.parquet",
            speaker_shard_counts[speaker]
        );
        *number += 1;
        if parts[3] != expected_name {
            return Err(invalid("shard numbering/order mismatch"));
        }
        let record = file_record(root, &shard.path)?;
        if shard.parquet_bytes != record.bytes || shard.sha256 != record.sha256 {
            return Err(invalid("shard report mismatch"));
        }
        let file = File::open(root.join(&shard.path))?;
        let builder = ParquetRecordBatchReaderBuilder::try_new(file)
            .map_err(other)?
            .with_batch_size(1);
        if builder.schema().as_ref() != schema().as_ref() {
            return Err(invalid("schema/metadata mismatch"));
        }
        let mut row_count = 0;
        let mut audio_bytes = 0u64;
        for batch in builder.build().map_err(other)? {
            let batch = batch.map_err(other)?;
            let audio = batch
                .column(0)
                .as_any()
                .downcast_ref::<StructArray>()
                .ok_or_else(|| invalid("audio not struct"))?;
            let wav = audio
                .column(0)
                .as_any()
                .downcast_ref::<BinaryArray>()
                .ok_or_else(|| invalid("audio bytes not binary"))?;
            let path = audio
                .column(1)
                .as_any()
                .downcast_ref::<StringArray>()
                .ok_or_else(|| invalid("audio path not string"))?;
            let texts: Vec<&StringArray> = [1, 2, 4, 5]
                .into_iter()
                .map(|i| {
                    batch
                        .column(i)
                        .as_any()
                        .downcast_ref::<StringArray>()
                        .ok_or_else(|| invalid("text column not string"))
                })
                .collect::<io::Result<_>>()?;
            for i in 0..batch.num_rows() {
                if audio.is_null(i)
                    || wav.is_null(i)
                    || path.is_null(i)
                    || !batch.column(3).is_null(i)
                    || texts.iter().any(|t| t.is_null(i))
                {
                    return Err(invalid("invalid column null"));
                }
                let name = path.value(i);
                let source = texts[3].value(i);
                if !safe_audio_name(name)
                    || !safe_source_path(source)
                    || texts[0].value(i).trim().is_empty()
                    || texts[1].value(i) != speaker
                    || texts[2].value(i) != "ja"
                {
                    return Err(invalid("invalid row value"));
                }
                let key = (speaker.to_string(), source.to_string(), name.to_string());
                if prior_key.as_ref().is_some_and(|p| p >= &key) {
                    return Err(invalid("row ordering/duplicate mismatch"));
                }
                prior_key = Some(key);
                if !observed_names.insert(name.to_ascii_lowercase()) {
                    return Err(invalid("duplicate audio path"));
                }
                canonical_wav(wav.value(i))?;
                audio_bytes = audio_bytes
                    .checked_add(wav.value_length(i) as u64)
                    .ok_or_else(|| invalid("audio bytes overflow"))?;
                row_count += 1;
            }
        }
        if row_count != shard.rows || audio_bytes != shard.embedded_audio_bytes {
            return Err(invalid("shard row/audio totals mismatch"));
        }
        total_rows += row_count;
        total_audio += audio_bytes;
    }
    if total_rows != report.row_count || total_audio != report.embedded_audio_bytes {
        return Err(invalid("report totals mismatch"));
    }
    Ok(report)
}

/// Write into a private sibling directory, read back the complete dataset,
/// recheck approved input, then publish through the bound parent without clobbering.
pub fn build_from_validated<R, C, D>(
    output: &crate::paths::ValidatedOutputPath,
    target_audio_bytes: u64,
    resolve: R,
    before_promote: C,
    mut check_shard_disk: D,
) -> Result<Report, crate::error::AppError>
where
    R: FnOnce(&Path) -> Result<PreparedDataset, crate::error::AppError>,
    C: FnOnce() -> Result<(), crate::error::AppError>,
    D: FnMut(u64) -> Result<(), crate::error::AppError>,
{
    use crate::{
        error::AppError, inventory::requested_parent_matches, parquet_writer::write_shards_checked,
    };
    let processing = |error: io::Error| AppError::processing(format!("dataset delivery: {error}"));
    requested_parent_matches(output)?;
    let stage = tempfile::Builder::new()
        .prefix(".mgwt-stage-")
        .tempdir_in(output.requested_parent())
        .map_err(processing)?;
    let stage_leaf = stage
        .path()
        .file_name()
        .ok_or_else(|| AppError::processing("stage leaf missing"))?;
    let work = stage.path().join("work");
    fs::create_dir(&work).map_err(processing)?;
    let prepared = resolve(&work)?;
    if prepared.rows.is_empty() {
        return Err(AppError::processing("no accepted dataset rows"));
    }
    let shards = write_shards_checked(&prepared.rows, stage.path(), target_audio_bytes, |bytes| {
        check_shard_disk(bytes).map_err(other)
    })
    .map_err(processing)?;
    fs::remove_dir_all(&work).map_err(processing)?;
    let report = write_sidecars(stage.path(), &prepared, &shards).map_err(processing)?;
    let verified = verify_dataset(stage.path()).map_err(processing)?;
    if verified != report {
        return Err(AppError::processing("dataset readback report mismatch"));
    }
    before_promote()?;
    requested_parent_matches(output)?;
    renameat_with(
        output.parent(),
        stage_leaf,
        output.parent(),
        output.leaf(),
        RenameFlags::NOREPLACE,
    )
    .map_err(|e| AppError::validation(format!("publish dataset without clobber: {e}")))?;
    Ok(report)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::paths::{ValidatedAppPath, ValidatedOutputPath};
    use serde_json::json;

    fn wav() -> Vec<u8> {
        let mut b = b"RIFF".to_vec();
        b.extend_from_slice(&38_u32.to_le_bytes());
        b.extend_from_slice(b"WAVEfmt ");
        b.extend_from_slice(&16_u32.to_le_bytes());
        b.extend_from_slice(&1_u16.to_le_bytes());
        b.extend_from_slice(&1_u16.to_le_bytes());
        b.extend_from_slice(&44_100_u32.to_le_bytes());
        b.extend_from_slice(&88_200_u32.to_le_bytes());
        b.extend_from_slice(&2_u16.to_le_bytes());
        b.extend_from_slice(&16_u16.to_le_bytes());
        b.extend_from_slice(b"data");
        b.extend_from_slice(&2_u32.to_le_bytes());
        b.extend_from_slice(&1_i16.to_le_bytes());
        b
    }
    fn prepare(work: &Path) -> Result<PreparedDataset, crate::error::AppError> {
        let audio_path = work.join("synthetic.wav");
        fs::write(&audio_path, wav()).unwrap();
        Ok(PreparedDataset {
            rows: vec![InputRow {
                audio_path,
                audio_name: "synthetic.wav".into(),
                transcription: "Synthetic line".into(),
                speaker: "Ema".into(),
                source_path: "asset/synthetic".into(),
            }],
            audit_records: vec![json!({"candidate_id": "a", "state": "accepted"})],
            rejected_records: vec![json!({"candidate_id": "b", "reason": "unmatched"})],
            producer_revision: "synthetic-revision".into(),
            approved_input_sha256: "0123456789abcdef".repeat(4),
        })
    }
    #[test]
    fn synthetic_build_promotes_and_verifies_without_clobber() {
        let tmp = tempfile::tempdir_in(".").unwrap();
        let app_path = tmp.path().join("app");
        fs::create_dir(&app_path).unwrap();
        let app = ValidatedAppPath::new(&app_path).unwrap();
        let output = tmp.path().join("dataset");
        let bound = ValidatedOutputPath::new(&output, &app).unwrap();
        let mut checked = 0;
        let report = build_from_validated(
            &bound,
            500 * 1024 * 1024,
            prepare,
            || {
                assert!(!output.exists());
                Ok(())
            },
            |_| {
                checked += 1;
                Ok(())
            },
        )
        .unwrap();
        assert!(checked > 0);
        assert_eq!(report.row_count, 1);
        assert_eq!(
            report.shards[0].path,
            "data/ja/Ema/train-00000-of-00001.parquet"
        );
        assert_eq!(verify_dataset(&output).unwrap(), report);
        assert!(ValidatedOutputPath::new(&output, &app).is_err());
    }
    #[test]
    fn failed_final_check_removes_stage_and_leaves_final_absent() {
        let tmp = tempfile::tempdir_in(".").unwrap();
        let app_path = tmp.path().join("app");
        fs::create_dir(&app_path).unwrap();
        let app = ValidatedAppPath::new(&app_path).unwrap();
        let output = tmp.path().join("dataset");
        let bound = ValidatedOutputPath::new(&output, &app).unwrap();
        let error = build_from_validated(
            &bound,
            500 * 1024 * 1024,
            prepare,
            || Err(crate::error::AppError::processing("checkpoint failed")),
            |_| Ok(()),
        )
        .unwrap_err();
        assert!(error.to_string().contains("checkpoint failed"));
        assert!(!output.exists());
        assert_eq!(fs::read_dir(tmp.path()).unwrap().count(), 1);
    }
}
