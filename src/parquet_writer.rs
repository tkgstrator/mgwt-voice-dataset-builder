//! Deterministic Hugging Face compatible Parquet shards.

use std::collections::{HashMap, HashSet};
use std::fs::{self, File};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use arrow_array::builder::{BinaryBuilder, StringBuilder};
use arrow_array::{ArrayRef, RecordBatch, StructArray};
use arrow_schema::{DataType, Field, Schema, SchemaRef};
use parquet::arrow::ArrowWriter;
use parquet::basic::{Compression, ZstdLevel};
use parquet::file::properties::{WriterProperties, WriterVersion};

pub const TARGET_AUDIO_BYTES: u64 = 500 * 1024 * 1024;
pub const MAX_AUDIO_BYTES: u64 = 256 * 1024 * 1024;
pub const MAX_BATCH_BYTES: usize = 1024 * 1024;
pub const MAX_BATCH_ROWS: usize = 100;
const HF_METADATA: &str = "{\"info\": {\"features\": {\"audio\": {\"decode\": false, \"_type\": \"Audio\"}, \"transcription\": {\"dtype\": \"string\", \"_type\": \"Value\"}, \"speaker\": {\"dtype\": \"string\", \"_type\": \"Value\"}, \"emotion\": {\"dtype\": \"string\", \"_type\": \"Value\"}, \"language\": {\"dtype\": \"string\", \"_type\": \"Value\"}, \"source_path\": {\"dtype\": \"string\", \"_type\": \"Value\"}}}}";

#[derive(Clone, Debug)]
pub struct InputRow {
    pub audio_path: PathBuf,
    pub audio_name: String,
    pub transcription: String,
    pub speaker: String,
    pub source_path: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Shard {
    pub relative_path: String,
    pub rows: usize,
    pub embedded_audio_bytes: u64,
}

pub fn schema() -> SchemaRef {
    let children = vec![
        Arc::new(Field::new("bytes", DataType::Binary, true)),
        Arc::new(Field::new("path", DataType::Utf8, true)),
    ];
    let fields = vec![
        Field::new("audio", DataType::Struct(children.into()), true),
        Field::new("transcription", DataType::Utf8, true),
        Field::new("speaker", DataType::Utf8, true),
        Field::new("emotion", DataType::Utf8, true),
        Field::new("language", DataType::Utf8, true),
        Field::new("source_path", DataType::Utf8, true),
    ];
    Arc::new(Schema::new_with_metadata(
        fields,
        HashMap::from([("huggingface".to_string(), HF_METADATA.to_string())]),
    ))
}

pub(crate) fn safe_speaker(s: &str) -> bool {
    let mut bytes = s.bytes();
    matches!(bytes.next(), Some(b'a'..=b'z' | b'A'..=b'Z'))
        && bytes.all(|b| b.is_ascii_alphanumeric())
}

pub(crate) fn safe_source_path(s: &str) -> bool {
    if s.is_empty()
        || s.len() > 4096
        || s.starts_with('/')
        || s.contains('\\')
        || s.contains('\0')
        || s.as_bytes().get(1) == Some(&b':')
    {
        return false;
    }
    s.split('/').all(|p| {
        !p.is_empty()
            && p != "."
            && p != ".."
            && p.len() <= 255
            && !p.bytes().any(|b| b < 32 || b == 127)
    })
}

pub(crate) fn safe_audio_name(s: &str) -> bool {
    s.ends_with(".wav")
        && s.len() <= 255
        && s.bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_' || b == b'.')
        && s != ".wav"
}

fn invalid(message: &str) -> std::io::Error {
    std::io::Error::new(std::io::ErrorKind::InvalidInput, message)
}

fn parquet_error(e: impl std::fmt::Display) -> std::io::Error {
    std::io::Error::other(e.to_string())
}

/// Caller owns an empty staging root and validates WAV bytes/properties before this layer.
/// Rows are planned from file lengths, then read one bounded batch at a time.
pub fn write_shards(
    rows: &[InputRow],
    root: &Path,
    target_audio_bytes: u64,
) -> std::io::Result<Vec<Shard>> {
    write_shards_checked(rows, root, target_audio_bytes, |_| Ok(()))
}

/// Recheck available space before each Arrow batch is flushed to its shard.
pub fn write_shards_checked(
    rows: &[InputRow],
    root: &Path,
    target_audio_bytes: u64,
    mut before_flush: impl FnMut(u64) -> std::io::Result<()>,
) -> std::io::Result<Vec<Shard>> {
    if target_audio_bytes == 0 {
        return Err(invalid("zero shard target"));
    }
    let mut sorted: Vec<_> = rows.iter().collect();
    sorted.sort_by(|a, b| {
        (
            a.speaker.as_bytes(),
            a.source_path.as_bytes(),
            a.audio_name.as_bytes(),
        )
            .cmp(&(
                b.speaker.as_bytes(),
                b.source_path.as_bytes(),
                b.audio_name.as_bytes(),
            ))
    });
    let mut names = HashSet::new();
    let mut lengths = Vec::with_capacity(sorted.len());
    for row in &sorted {
        if !safe_speaker(&row.speaker)
            || !safe_source_path(&row.source_path)
            || !safe_audio_name(&row.audio_name)
            || row.transcription.trim().is_empty()
            || row.transcription.len() > 1024 * 1024
        {
            return Err(invalid("invalid row field"));
        }
        if !names.insert(row.audio_name.to_ascii_lowercase()) {
            return Err(invalid("audio name collision"));
        }
        let size = fs::metadata(&row.audio_path)?.len();
        if size > MAX_AUDIO_BYTES {
            return Err(invalid("audio exceeds 256 MiB"));
        }
        lengths.push(size);
    }
    let mut planned: Vec<(String, Vec<Vec<usize>>)> = Vec::new();
    for (idx, row) in sorted.iter().enumerate() {
        if planned.last().is_none_or(|g| g.0 != row.speaker) {
            planned.push((row.speaker.clone(), vec![vec![]]));
        }
        let shards = &mut planned.last_mut().expect("just pushed").1;
        let current_bytes: u64 = shards
            .last()
            .expect("initial shard")
            .iter()
            .map(|i| lengths[*i])
            .sum();
        if !shards.last().expect("initial shard").is_empty()
            && current_bytes.saturating_add(lengths[idx]) > target_audio_bytes
        {
            shards.push(vec![]);
        }
        shards.last_mut().expect("shard").push(idx);
    }
    let schema = schema();
    let props = WriterProperties::builder()
        .set_writer_version(WriterVersion::PARQUET_2_0)
        .set_compression(Compression::ZSTD(
            ZstdLevel::try_new(9).map_err(parquet_error)?,
        ))
        .set_dictionary_enabled(false)
        .set_max_row_group_row_count(Some(MAX_BATCH_ROWS))
        .build();
    let mut produced = Vec::new();
    let mut completed_audio_bytes = 0_u64;
    for (speaker, shards) in planned {
        if shards.len() > 99999 {
            return Err(invalid("too many shards"));
        }
        for (number, indices) in shards.iter().enumerate() {
            let relative = format!(
                "data/ja/{speaker}/train-{number:05}-of-{:05}.parquet",
                shards.len()
            );
            let path = root.join(&relative);
            fs::create_dir_all(path.parent().expect("parquet parent"))?;
            let file = File::create(&path)?;
            let mut writer = ArrowWriter::try_new(file, schema.clone(), Some(props.clone()))
                .map_err(parquet_error)?;
            let mut buffered: Vec<(usize, Vec<u8>)> = Vec::new();
            let mut estimated: usize = 0;
            let mut embedded_audio_bytes = 0;
            for &idx in indices {
                let row = sorted[idx];
                let data = fs::read(&row.audio_path)?;
                if data.len() as u64 != lengths[idx] {
                    return Err(invalid("audio size changed"));
                }
                let cost = data
                    .len()
                    .saturating_add(128)
                    .saturating_add(row.audio_name.len())
                    .saturating_add(row.transcription.len())
                    .saturating_add(row.speaker.len())
                    .saturating_add(row.source_path.len())
                    .saturating_add(2);
                if !buffered.is_empty()
                    && (buffered.len() >= MAX_BATCH_ROWS
                        || estimated.saturating_add(cost) > MAX_BATCH_BYTES)
                {
                    before_flush(completed_audio_bytes.saturating_add(embedded_audio_bytes))?;
                    flush_batch(&mut writer, &schema, &sorted, &mut buffered)?;
                    estimated = 0;
                }
                embedded_audio_bytes += data.len() as u64;
                estimated += cost;
                buffered.push((idx, data));
            }
            before_flush(completed_audio_bytes.saturating_add(embedded_audio_bytes))?;
            flush_batch(&mut writer, &schema, &sorted, &mut buffered)?;
            writer.close().map_err(parquet_error)?;
            completed_audio_bytes = completed_audio_bytes
                .checked_add(embedded_audio_bytes)
                .ok_or_else(|| invalid("total embedded audio overflow"))?;
            produced.push(Shard {
                relative_path: relative,
                rows: indices.len(),
                embedded_audio_bytes,
            });
        }
    }
    Ok(produced)
}

fn flush_batch(
    writer: &mut ArrowWriter<File>,
    schema: &SchemaRef,
    rows: &[&InputRow],
    batch: &mut Vec<(usize, Vec<u8>)>,
) -> std::io::Result<()> {
    if batch.is_empty() {
        return Ok(());
    }
    let mut binary = BinaryBuilder::new();
    let mut path = StringBuilder::new();
    let mut transcription = StringBuilder::new();
    let mut speaker = StringBuilder::new();
    let mut emotion = StringBuilder::new();
    let mut language = StringBuilder::new();
    let mut source_path = StringBuilder::new();
    for (idx, data) in batch.drain(..) {
        let row = rows[idx];
        binary.append_value(data);
        path.append_value(&row.audio_name);
        transcription.append_value(&row.transcription);
        speaker.append_value(&row.speaker);
        emotion.append_null();
        language.append_value("ja");
        source_path.append_value(&row.source_path);
    }
    let audio = StructArray::from(vec![
        (
            Arc::new(Field::new("bytes", DataType::Binary, true)),
            Arc::new(binary.finish()) as ArrayRef,
        ),
        (
            Arc::new(Field::new("path", DataType::Utf8, true)),
            Arc::new(path.finish()) as ArrayRef,
        ),
    ]);
    let arrays: Vec<ArrayRef> = vec![
        Arc::new(audio),
        Arc::new(transcription.finish()),
        Arc::new(speaker.finish()),
        Arc::new(emotion.finish()),
        Arc::new(language.finish()),
        Arc::new(source_path.finish()),
    ];
    let record = RecordBatch::try_new(schema.clone(), arrays).map_err(parquet_error)?;
    writer.write(&record).map_err(parquet_error)?;
    writer.flush().map_err(parquet_error)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use arrow_array::{Array, BinaryArray, StringArray};
    use parquet::arrow::arrow_reader::ParquetRecordBatchReaderBuilder;

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

    #[test]
    fn synthetic_shards_are_sorted_bounded_and_deterministic() {
        let tmp = tempfile::tempdir().unwrap();
        let data = wav();
        let mut rows = vec![];
        for (speaker, source, name) in [
            ("Ema", "asset/z", "z.wav"),
            ("Ema", "asset/a", "a.wav"),
            ("Noa", "asset/o", "o.wav"),
        ] {
            let audio_path = tmp.path().join(name);
            fs::write(&audio_path, &data).unwrap();
            rows.push(InputRow {
                audio_path,
                audio_name: name.into(),
                transcription: "synthetic".into(),
                speaker: speaker.into(),
                source_path: source.into(),
            });
        }
        let a = tmp.path().join("a");
        let b = tmp.path().join("b");
        let shards_a = write_shards(&rows, &a, 60).unwrap();
        let shards_b = write_shards(&rows, &b, 60).unwrap();
        assert_eq!(shards_a, shards_b);
        assert_eq!(shards_a.len(), 3);
        assert_eq!(
            shards_a[0].relative_path,
            "data/ja/Ema/train-00000-of-00002.parquet"
        );
        assert_eq!(
            shards_a[1].relative_path,
            "data/ja/Ema/train-00001-of-00002.parquet"
        );
        for shard in &shards_a {
            assert_eq!(
                fs::read(a.join(&shard.relative_path)).unwrap(),
                fs::read(b.join(&shard.relative_path)).unwrap()
            );
        }
        let file = File::open(a.join(&shards_a[0].relative_path)).unwrap();
        let builder = ParquetRecordBatchReaderBuilder::try_new(file).unwrap();
        assert_eq!(builder.schema().as_ref(), schema().as_ref());
        let mut reader = builder.build().unwrap();
        let batch = reader.next().unwrap().unwrap();
        assert_eq!(batch.num_rows(), 1);
        let audio = batch
            .column(0)
            .as_any()
            .downcast_ref::<StructArray>()
            .unwrap();
        assert_eq!(
            audio
                .column(0)
                .as_any()
                .downcast_ref::<BinaryArray>()
                .unwrap()
                .value(0),
            data
        );
        assert_eq!(
            audio
                .column(1)
                .as_any()
                .downcast_ref::<StringArray>()
                .unwrap()
                .value(0),
            "a.wav"
        );
        assert!(batch.column(3).is_null(0));
    }

    #[test]
    fn rejects_unsafe_fields() {
        let tmp = tempfile::tempdir().unwrap();
        let audio_path = tmp.path().join("x.wav");
        fs::write(&audio_path, wav()).unwrap();
        let row = InputRow {
            audio_path,
            audio_name: "x.wav".into(),
            transcription: "text".into(),
            speaker: "../bad".into(),
            source_path: "asset/a".into(),
        };
        assert!(write_shards(&[row], tmp.path(), TARGET_AUDIO_BYTES).is_err());
    }

    #[test]
    fn rejects_sparse_oversized_audio_before_arrow_allocation() {
        let tmp = tempfile::tempdir().unwrap();
        let audio_path = tmp.path().join("large.wav");
        File::create(&audio_path)
            .unwrap()
            .set_len(MAX_AUDIO_BYTES + 1)
            .unwrap();
        let row = InputRow {
            audio_path,
            audio_name: "large.wav".into(),
            transcription: "text".into(),
            speaker: "Ema".into(),
            source_path: "asset/a".into(),
        };
        assert_eq!(
            write_shards(&[row], tmp.path(), TARGET_AUDIO_BYTES)
                .unwrap_err()
                .kind(),
            std::io::ErrorKind::InvalidInput
        );
        assert!(!tmp.path().join("data").exists());
    }

    #[test]
    fn record_batches_flush_at_one_hundred_rows() {
        let tmp = tempfile::tempdir().unwrap();
        let bytes = wav();
        let mut rows = Vec::new();
        for i in 0..102 {
            let name = format!("{i:03}.wav");
            let audio_path = tmp.path().join(&name);
            fs::write(&audio_path, &bytes).unwrap();
            rows.push(InputRow {
                audio_path,
                audio_name: name,
                transcription: "text".into(),
                speaker: "Ema".into(),
                source_path: format!("asset/{i:03}"),
            });
        }
        let shards = write_shards(&rows, &tmp.path().join("out"), TARGET_AUDIO_BYTES).unwrap();
        assert_eq!(shards.len(), 1);
        let file = File::open(tmp.path().join("out").join(&shards[0].relative_path)).unwrap();
        let builder = ParquetRecordBatchReaderBuilder::try_new(file).unwrap();
        assert_eq!(builder.metadata().num_row_groups(), 2);
        assert_eq!(builder.metadata().row_group(0).num_rows(), 100);
        assert_eq!(builder.metadata().row_group(1).num_rows(), 2);
    }

    #[test]
    fn record_batches_flush_at_payload_budget() {
        let tmp = tempfile::tempdir().unwrap();
        let mut bytes = wav();
        let extra = 600_000 - bytes.len();
        bytes.extend(std::iter::repeat_n(0, extra));
        let bytes_len = bytes.len() as u32;
        bytes[4..8].copy_from_slice(&(bytes_len - 8).to_le_bytes());
        bytes[40..44].copy_from_slice(&(bytes_len - 44).to_le_bytes());
        let mut rows = Vec::new();
        for i in 0..3 {
            let name = format!("{i}.wav");
            let audio_path = tmp.path().join(&name);
            fs::write(&audio_path, &bytes).unwrap();
            rows.push(InputRow {
                audio_path,
                audio_name: name,
                transcription: "text".into(),
                speaker: "Ema".into(),
                source_path: format!("asset/{i}"),
            });
        }
        let shards = write_shards(&rows, &tmp.path().join("out"), TARGET_AUDIO_BYTES).unwrap();
        let file = File::open(tmp.path().join("out").join(&shards[0].relative_path)).unwrap();
        let builder = ParquetRecordBatchReaderBuilder::try_new(file).unwrap();
        assert_eq!(builder.metadata().num_row_groups(), 3);
        for i in 0..3 {
            assert_eq!(builder.metadata().row_group(i).num_rows(), 1);
        }
    }
}
