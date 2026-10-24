//! Bundle-scoped access to the pinned pure-Rust Unity reader.
use crate::{
    error::{AppError, UnityErrorKind},
    inventory::{VerifiedBundleInputs, VerifiedSnapshotSet},
    model::{
        AudioAssetRef, AudioLimits, AudioMetadata, CodecKind, CodecLayoutSignature,
        DecodedAudioReceipt, FrameCountSource, MetadataCompleteness, PayloadLayout,
        PredecodeFrameCount, RawScriptObject,
    },
};
use sha2::{Digest, Sha256};
use std::{
    io::Write,
    path::{Path, PathBuf},
};
use unity_rs_core::{
    audio::{DirectWavKind, direct_wav_output_size, write_direct_wav},
    bundle::{BundleParseLimits, UnityFsBundle},
    endian::{Endian, EndianReader},
    file_type::{FileType, HEADER_SCAN_LENGTH, detect_file_type},
    loader::{AssetCollection, AssetLoadLimits, LoadedResource, LoadedSerializedFile},
    serialized::{SerializedFile, SerializedOpenOptions, SerializedParseLimits},
    simple_assets::{AudioClipAsset, SimpleAssetReadLimits, read_audio_clip_asset_by_file_index},
    source::Region,
    type_tree::{TypeField, TypeTreeReadLimits, TypeValue},
};

pub trait UnityBackend {
    type Session: BundleSession;
    fn open_bundle(&self, approved: &VerifiedBundleInputs) -> Result<Self::Session, AppError>;
}

fn approved_input(message: impl Into<String>) -> AppError {
    AppError::unity(UnityErrorKind::ApprovedInput, message)
}
fn unsupported(message: impl Into<String>) -> AppError {
    AppError::unity(UnityErrorKind::UnsupportedLayoutOrCodec, message)
}
fn candidate(message: impl Into<String>) -> AppError {
    AppError::unity(UnityErrorKind::CandidateDecode, message)
}
fn unexpected(message: impl Into<String>) -> AppError {
    AppError::unity(UnityErrorKind::Unexpected, message)
}

fn validate_subsound_index(index: i32) -> Result<(), AppError> {
    if index == 0 {
        Ok(())
    } else {
        Err(unsupported(
            "AudioClip subsound index is unsupported by pinned first-subsound decoder",
        ))
    }
}

fn validate_frame_count(
    predecode: PredecodeFrameCount,
    wav_frames: u64,
) -> Result<FrameCountSource, AppError> {
    match predecode {
        PredecodeFrameCount::Unavailable => Ok(FrameCountSource::ValidatedDecodedWav),
        PredecodeFrameCount::ValidatedPayloadHeader(frames) if frames == wav_frames => {
            Ok(FrameCountSource::ValidatedPayloadHeaderAndWav)
        }
        PredecodeFrameCount::ValidatedPayloadHeader(_) => {
            Err(unsupported("payload and WAV frame counts disagree"))
        }
    }
}

pub trait BundleSession {
    fn raw_script_objects(&mut self) -> Result<Vec<crate::model::RawScriptObject>, AppError>;
    fn audio_refs(&mut self) -> Result<Vec<crate::model::AudioAssetRef>, AppError>;
    fn stream_audio(
        &mut self,
        reference: &crate::model::AudioAssetRef,
        sink: &mut dyn Write,
        limits: &crate::model::AudioLimits,
    ) -> Result<crate::model::DecodedAudioReceipt, AppError>;
}

#[derive(Debug, Clone)]
pub struct UnityReader {
    scratch: PathBuf,
    maximum_bundle_bytes: u64,
}

impl UnityReader {
    pub fn new(scratch: impl AsRef<Path>) -> Self {
        Self::with_bundle_limit(scratch, 512 * 1024 * 1024)
    }
    pub fn with_bundle_limit(scratch: impl AsRef<Path>, maximum_bundle_bytes: u64) -> Self {
        Self {
            scratch: scratch.as_ref().to_path_buf(),
            maximum_bundle_bytes,
        }
    }
}

#[derive(Debug)]
pub struct UnitySession {
    collection: AssetCollection,
    snapshots: VerifiedSnapshotSet,
    bundle_path: String,
    approved_external: Vec<String>,
    cached_audio_refs: Option<Vec<AudioAssetRef>>,
    peak_live_buffer_bytes: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SessionMetrics {
    pub bundle_opens: u64,
    pub peak_live_buffer_bytes: usize,
}

impl UnitySession {
    pub fn metrics(&self) -> SessionMetrics {
        SessionMetrics {
            bundle_opens: 1,
            peak_live_buffer_bytes: self.peak_live_buffer_bytes,
        }
    }
}

impl BundleSession for UnitySession {
    fn raw_script_objects(&mut self) -> Result<Vec<RawScriptObject>, AppError> {
        self.snapshots.checkpoint()?;
        let mut scripts = Vec::new();
        for (file_index, loaded) in self.collection.serialized_files().iter().enumerate() {
            for (object_index, object) in loaded.file.objects.iter().enumerate() {
                if object.class_id != unity_rs_core::monobehaviour::MONO_BEHAVIOUR_CLASS_ID {
                    continue;
                }
                let value = loaded
                    .file
                    .read_type_tree_value_with_limits(object_index, type_tree_limits())
                    .map_err(core_error)?;
                let metadata = self.collection.object_metadata(file_index, object.path_id);
                scripts.push(RawScriptObject {
                    bundle_path: self.bundle_path.clone(),
                    file_index,
                    object_index,
                    path_id: object.path_id,
                    name: metadata.and_then(|m| m.name.clone()),
                    container: metadata.and_then(|m| m.container.as_ref().map(|s| s.to_string())),
                    value,
                });
            }
        }
        scripts.sort_by(|a, b| (&a.bundle_path, a.path_id).cmp(&(&b.bundle_path, b.path_id)));
        self.snapshots.checkpoint()?;
        Ok(scripts)
    }
    fn audio_refs(&mut self) -> Result<Vec<AudioAssetRef>, AppError> {
        self.snapshots.checkpoint()?;
        if let Some(refs) = &self.cached_audio_refs {
            return Ok(refs.clone());
        }
        let mut refs = Vec::new();
        for (file_index, loaded) in self.collection.serialized_files().iter().enumerate() {
            for (object_index, object) in loaded.file.objects.iter().enumerate() {
                if object.class_id == unity_rs_core::simple_assets::AUDIO_CLIP_CLASS_ID {
                    refs.push(project_audio_clip(
                        &self.collection,
                        &self.bundle_path,
                        &self.approved_external,
                        file_index,
                        object_index,
                    )?);
                }
            }
        }
        refs.sort_by(|a, b| (&a.bundle_path, a.path_id).cmp(&(&b.bundle_path, b.path_id)));
        self.snapshots.checkpoint()?;
        self.cached_audio_refs = Some(refs.clone());
        Ok(refs)
    }
    fn stream_audio(
        &mut self,
        reference: &AudioAssetRef,
        sink: &mut dyn Write,
        limits: &AudioLimits,
    ) -> Result<DecodedAudioReceipt, AppError> {
        self.snapshots.checkpoint()?;
        let result = (|| {
            if reference.bundle_path != self.bundle_path {
                return Err(approved_input(
                    "AudioClip locator belongs to another bundle",
                ));
            }
            if !self
                .audio_refs()?
                .iter()
                .any(|current| current == reference)
            {
                return Err(approved_input(
                    "AudioClip locator does not match indexed metadata",
                ));
            }
            let asset = load_exact_asset(&self.collection, reference)?;
            let kind = asset
                .direct_wav
                .ok_or_else(|| unsupported("unsupported AudioClip codec"))?;
            if codec_kind(Some(kind)) != reference.signature.codec {
                return Err(unsupported("AudioClip codec signature drift"));
            }
            let predicted = direct_wav_output_size(&asset.payload, kind).map_err(core_error)?;
            if predicted > limits.maximum_wav_bytes || predicted > 256 * 1024 * 1024 {
                return Err(unsupported("decoded WAV exceeds hard limit"));
            }
            let expansion_cap = asset
                .payload
                .len()
                .checked_mul(limits.maximum_expansion_ratio)
                .ok_or_else(|| unsupported("audio expansion limit overflow"))?;
            if predicted > expansion_cap {
                return Err(unsupported("audio expansion ratio exceeds limit"));
            }
            let mut bounded =
                BoundedWavSink::new(sink, limits.maximum_wav_bytes.min(256 * 1024 * 1024));
            let written = write_direct_wav(&asset.payload, kind, bounded.maximum, &mut bounded)
                .map_err(decode_error)?;
            if written != predicted || written != bounded.written {
                return Err(unexpected("decoded WAV byte count disagrees"));
            }
            let wav = bounded.finish(reference)?;
            let frame_count_source =
                validate_frame_count(reference.predecode_frames, wav.frame_count)?;
            self.peak_live_buffer_bytes =
                self.peak_live_buffer_bytes.max(wav.peak_live_buffer_bytes);
            Ok(DecodedAudioReceipt {
                frame_count_source,
                ..wav
            })
        })();
        self.snapshots.checkpoint()?;
        result
    }
}

impl UnityBackend for UnityReader {
    type Session = UnitySession;
    fn open_bundle(&self, approved: &VerifiedBundleInputs) -> Result<Self::Session, AppError> {
        if approved.bundle.size > self.maximum_bundle_bytes {
            return Err(approved_input(
                "approved bundle exceeds configured hard limit",
            ));
        }
        let scratch = self
            .scratch
            .canonicalize()
            .map_err(|e| AppError::unity(UnityErrorKind::Io, format!("scratch path: {e}")))?;
        let snapshots = approved.snapshot_set_shared(&scratch)?;
        let bundle_path = approved.bundle.relative_path.clone();
        let collection = snapshots.with_parser_paths(|bundle_snapshot, external| {
            open_pinned_bundle(bundle_snapshot, &bundle_path, external)
        })?;
        Ok(UnitySession {
            collection,
            snapshots,
            bundle_path,
            approved_external: approved
                .resources
                .iter()
                .map(|r| r.relative_path.clone())
                .collect(),
            cached_audio_refs: None,
            peak_live_buffer_bytes: 0,
        })
    }
}

fn core_error(error: unity_rs_core::Error) -> AppError {
    match error {
        unity_rs_core::Error::Io(error) => {
            AppError::unity(UnityErrorKind::Io, format!("Unity input I/O: {error}"))
        }
        unity_rs_core::Error::Unsupported(message) => {
            unsupported(format!("unsupported Unity layout/codec: {message}"))
        }
        unity_rs_core::Error::InvalidData(message) => {
            unsupported(format!("invalid Unity layout: {message}"))
        }
    }
}

fn decode_error(error: unity_rs_core::Error) -> AppError {
    match error {
        unity_rs_core::Error::Io(error) => {
            AppError::unity(UnityErrorKind::Io, format!("Unity decode I/O: {error}"))
        }
        unity_rs_core::Error::Unsupported(message) => unsupported(message),
        unity_rs_core::Error::InvalidData(message) => candidate(message),
    }
}

fn open_pinned_bundle(
    path: &Path,
    bundle_path: &str,
    external: &[(String, PathBuf)],
) -> Result<AssetCollection, AppError> {
    let region = Region::from_file(path).map_err(core_error)?;
    let limits = BundleParseLimits {
        max_entries: 200_000,
        max_blocks: 200_000,
        max_path_length: 4096,
        max_entry_read_size: 512 * 1024 * 1024,
        ..BundleParseLimits::default()
    };
    let bundle = UnityFsBundle::open_with_limits(&region, limits).map_err(core_error)?;
    let expanded = bundle
        .entries
        .iter()
        .try_fold(0_u64, |total, entry| total.checked_add(entry.size))
        .ok_or_else(|| approved_input("expanded bundle size overflow"))?;
    if expanded > 2 * 1024 * 1024 * 1024 {
        return Err(approved_input("expanded bundle exceeds hard limit"));
    }
    let mut files = Vec::new();
    let mut resources = Vec::new();
    for (index, entry) in bundle.entries.iter().enumerate() {
        let entry_region = bundle.entry_region(index).map_err(core_error)?;
        let mut header = vec![0; (entry.size as usize).min(HEADER_SCAN_LENGTH)];
        entry_region
            .read_exact_at(0, &mut header)
            .map_err(core_error)?;
        let label = format!("{bundle_path}::{}", entry.path);
        match detect_file_type(&header, entry.size).file_type {
            FileType::AssetsFile => {
                let parse_limits = SerializedParseLimits {
                    maximum_metadata_bytes: 256 * 1024 * 1024,
                    maximum_objects: 200_000,
                    maximum_string_bytes: 1024 * 1024,
                    ..SerializedParseLimits::default()
                };
                let file = SerializedFile::open_with_options(
                    entry_region,
                    SerializedOpenOptions {
                        limits: parse_limits,
                        bundle_version_hint: Some(bundle.header.common.unity_revision.clone()),
                        strict_unity_versions: true,
                        ..SerializedOpenOptions::default()
                    },
                )
                .map_err(core_error)?;
                if file.unity_version.to_string() != "6000.0.48f1" {
                    return Err(unsupported("Unity version is outside the approved layout"));
                }
                files.push(LoadedSerializedFile { path: label, file });
            }
            FileType::ResourceFile => resources.push(LoadedResource {
                path: label,
                region: entry_region,
            }),
            _ => {
                return Err(unsupported("nested bundle entry layout is unsupported"));
            }
        }
    }
    for (label, path) in external {
        resources.push(LoadedResource {
            path: label.clone(),
            region: Region::from_file(path).map_err(core_error)?,
        });
    }
    let mut collection = AssetCollection::from_loaded_parts(files, resources);
    let metadata_limits = AssetLoadLimits {
        maximum_object_metadata_entries: 200_000,
        maximum_object_metadata_index_bytes: 256 * 1024 * 1024,
        maximum_path_bytes: 4096,
        ..AssetLoadLimits::default()
    };
    collection
        .resolve_object_metadata(metadata_limits)
        .map_err(core_error)?;
    Ok(collection)
}

fn type_tree_limits() -> TypeTreeReadLimits {
    TypeTreeReadLimits {
        maximum_string_bytes: 1024 * 1024,
        maximum_byte_array_bytes: 1024 * 1024,
        maximum_typeless_bytes: 1024 * 1024,
        maximum_materialized_bytes: 16 * 1024 * 1024,
        ..TypeTreeReadLimits::default()
    }
}

fn fields(value: &TypeValue) -> Result<&[TypeField], AppError> {
    match value {
        TypeValue::Object(fields) => Ok(fields),
        _ => Err(unsupported("unsupported AudioClip TypeTree object layout")),
    }
}
fn field<'a>(fields: &'a [TypeField], name: &str) -> Result<&'a TypeValue, AppError> {
    fields
        .iter()
        .find(|field| field.name == name)
        .map(|field| &field.value)
        .ok_or_else(|| unsupported(format!("AudioClip TypeTree lacks {name}")))
}
fn integer(value: &TypeValue) -> Result<i64, AppError> {
    match value {
        TypeValue::Signed(n) => Ok(*n),
        TypeValue::Unsigned(n) => {
            i64::try_from(*n).map_err(|_| unsupported("AudioClip integer exceeds range"))
        }
        _ => Err(unsupported("AudioClip TypeTree integer has wrong type")),
    }
}
fn positive_u16(value: &TypeValue) -> Result<u16, AppError> {
    u16::try_from(integer(value)?)
        .ok()
        .filter(|n| *n > 0)
        .ok_or_else(|| unsupported("AudioClip positive u16 is invalid"))
}
fn positive_u32(value: &TypeValue) -> Result<u32, AppError> {
    u32::try_from(integer(value)?)
        .ok()
        .filter(|n| *n > 0)
        .ok_or_else(|| unsupported("AudioClip positive u32 is invalid"))
}
fn nonnegative_u64(value: &TypeValue) -> Result<u64, AppError> {
    u64::try_from(integer(value)?).map_err(|_| unsupported("AudioClip resource range is negative"))
}
fn string(value: &TypeValue) -> Result<&str, AppError> {
    match value {
        TypeValue::String(s) => Ok(s),
        _ => Err(unsupported("AudioClip TypeTree string has wrong type")),
    }
}

fn reader_advance(
    reader: &mut EndianReader<unity_rs_core::source::RegionCursor>,
    bytes: u64,
    max: u64,
) -> Result<(), AppError> {
    let end = reader
        .position()
        .map_err(core_error)?
        .checked_add(bytes)
        .ok_or_else(|| unsupported("AudioClip parser position overflow"))?;
    if end > max {
        return Err(unsupported("AudioClip parser exceeded object range"));
    }
    reader.set_position(end).map_err(core_error)
}
fn reader_align(
    reader: &mut EndianReader<unity_rs_core::source::RegionCursor>,
    absolute_start: u64,
    max: u64,
) -> Result<(), AppError> {
    let position = reader.position().map_err(core_error)?;
    let remainder = absolute_start
        .checked_add(position)
        .ok_or_else(|| unsupported("AudioClip alignment overflow"))?
        % 4;
    if remainder == 0 {
        Ok(())
    } else {
        reader_advance(reader, 4 - remainder, max)
    }
}
fn read_bounded_string(
    reader: &mut EndianReader<unity_rs_core::source::RegionCursor>,
    absolute_start: u64,
    max: u64,
) -> Result<String, AppError> {
    let length = usize::try_from(reader.read_i32().map_err(core_error)?)
        .map_err(|_| unsupported("AudioClip string length is negative"))?;
    if length > 1024 * 1024 {
        return Err(unsupported("AudioClip string exceeds hard limit"));
    }
    if reader
        .position()
        .map_err(core_error)?
        .checked_add(length as u64)
        .is_none_or(|end| end > max)
    {
        return Err(unsupported("AudioClip string exceeds object range"));
    }
    let text = reader.read_utf8(length).map_err(core_error)?;
    if length > 0 {
        reader_align(reader, absolute_start, max)?;
    }
    Ok(text)
}

fn post_compression_position(
    file: &SerializedFile,
    object_index: usize,
) -> Result<(u64, String, u64, u64, i32), AppError> {
    let object = file
        .objects
        .get(object_index)
        .ok_or_else(|| unsupported("AudioClip object index is invalid"))?;
    let region = file.object_region(object_index).map_err(core_error)?;
    let endian = if file.header.endianness == 0 {
        Endian::Little
    } else {
        Endian::Big
    };
    let mut reader = EndianReader::new(region.cursor(), endian);
    if file.target_platform == -2 {
        reader_advance(
            &mut reader,
            4 + if file.header.version.0 < 14 { 16 } else { 24 },
            region.len(),
        )?;
    }
    let _name = read_bounded_string(&mut reader, object.byte_start, region.len())?;
    reader_advance(&mut reader, 12, region.len())?;
    let _bits = reader.read_i32().map_err(core_error)?;
    reader_advance(&mut reader, 5, region.len())?;
    reader_align(&mut reader, object.byte_start, region.len())?;
    reader_advance(&mut reader, 7, region.len())?;
    reader_align(&mut reader, object.byte_start, region.len())?;
    let source = read_bounded_string(&mut reader, object.byte_start, region.len())?;
    let offset = u64::try_from(reader.read_i64().map_err(core_error)?)
        .map_err(|_| unsupported("negative AudioClip resource offset"))?;
    let size = u64::try_from(reader.read_i64().map_err(core_error)?)
        .map_err(|_| unsupported("negative AudioClip resource size"))?;
    let compression = reader.read_i32().map_err(core_error)?;
    Ok((
        reader.position().map_err(core_error)?,
        source,
        offset,
        size,
        compression,
    ))
}

fn codec_kind(kind: Option<DirectWavKind>) -> CodecKind {
    match kind {
        Some(DirectWavKind::ExistingWave) => CodecKind::ExistingWave,
        Some(DirectWavKind::LegacyPcm16 { .. }) => CodecKind::LegacyPcm16,
        Some(DirectWavKind::Fsb5Pcm(_)) => CodecKind::Fsb5Pcm,
        Some(DirectWavKind::Fsb5Ima(_)) => CodecKind::Fsb5Ima,
        Some(DirectWavKind::Fsb5Dsp(_)) => CodecKind::Fsb5Dsp,
        Some(DirectWavKind::Fsb5Vag(_)) => CodecKind::Fsb5Vag,
        Some(DirectWavKind::Fsb5Hevag(_)) => CodecKind::Fsb5Hevag,
        Some(DirectWavKind::Fsb5Fadpcm(_)) => CodecKind::Fsb5Fadpcm,
        Some(DirectWavKind::Fsb5Mpeg(_)) => CodecKind::Fsb5Mpeg,
        Some(DirectWavKind::Fsb5Opus(_)) => CodecKind::Fsb5Opus,
        Some(DirectWavKind::Fsb5Vorbis(_)) => CodecKind::Fsb5Vorbis,
        None => CodecKind::Unsupported,
    }
}

fn predecode_frames(
    payload: &Region,
    kind: Option<DirectWavKind>,
) -> Result<PredecodeFrameCount, AppError> {
    let Some(kind) = kind else {
        return Ok(PredecodeFrameCount::Unavailable);
    };
    let mut header = [0_u8; 12];
    if payload.len() < header.len() as u64 {
        return Ok(PredecodeFrameCount::Unavailable);
    }
    payload.read_exact_at(0, &mut header).map_err(core_error)?;
    if &header[..4] != b"FSB5" {
        return Ok(PredecodeFrameCount::Unavailable);
    }
    let bank_subsounds = u32::from_le_bytes(header[8..12].try_into().expect("four bytes"));
    if bank_subsounds != 1 {
        return Ok(PredecodeFrameCount::Unavailable);
    }
    let frames = match kind {
        DirectWavKind::Fsb5Ima(stream) => Some(stream.frame_count),
        DirectWavKind::Fsb5Dsp(stream) => Some(stream.frame_count),
        DirectWavKind::Fsb5Vag(stream) => Some(stream.frame_count),
        DirectWavKind::Fsb5Hevag(stream) => Some(stream.frame_count),
        DirectWavKind::Fsb5Fadpcm(stream) => Some(stream.frame_count),
        DirectWavKind::Fsb5Mpeg(stream) => Some(stream.frame_count),
        DirectWavKind::Fsb5Opus(stream) => Some(stream.frame_count),
        DirectWavKind::Fsb5Vorbis(stream) => Some(stream.frame_count),
        _ => None,
    };
    Ok(frames.filter(|n| *n > 0).map_or(
        PredecodeFrameCount::Unavailable,
        PredecodeFrameCount::ValidatedPayloadHeader,
    ))
}

fn project_audio_clip(
    collection: &AssetCollection,
    bundle_path: &str,
    approved_external: &[String],
    file_index: usize,
    object_index: usize,
) -> Result<AudioAssetRef, AppError> {
    let loaded = collection
        .serialized_files()
        .get(file_index)
        .ok_or_else(|| unsupported("AudioClip file index is invalid"))?;
    let file = &loaded.file;
    let object = file
        .objects
        .get(object_index)
        .ok_or_else(|| unsupported("AudioClip object index is invalid"))?;
    let projection = file
        .read_type_tree_value_with_limits(object_index, type_tree_limits())
        .map_err(core_error)?;
    let root = fields(&projection)?;
    let name = string(field(root, "m_Name")?)?.to_owned();
    let channels = positive_u16(field(root, "m_Channels")?)?;
    let sample_rate = positive_u32(field(root, "m_Frequency")?)?;
    let bits_per_sample = positive_u16(field(root, "m_BitsPerSample")?)?;
    let duration_seconds = match field(root, "m_Length")? {
        TypeValue::Float32(value) => *value,
        _ => {
            return Err(unsupported("AudioClip duration layout is unsupported"));
        }
    };
    if !duration_seconds.is_finite() || duration_seconds < 0.0 {
        return Err(unsupported("AudioClip duration is invalid"));
    }
    let subsound_index = i32::try_from(integer(field(root, "m_SubsoundIndex")?)?)
        .map_err(|_| unsupported("AudioClip subsound index is invalid"))?;
    validate_subsound_index(subsound_index)?;
    let compression_format = i32::try_from(integer(field(root, "m_CompressionFormat")?)?)
        .map_err(|_| unsupported("AudioClip compression format is invalid"))?;
    let resource = fields(field(root, "m_Resource")?)?;
    let source = string(field(resource, "m_Source")?)?;
    let offset = nonnegative_u64(field(resource, "m_Offset")?)?;
    let size = nonnegative_u64(field(resource, "m_Size")?)?;
    if size > 256 * 1024 * 1024 {
        return Err(unsupported("AudioClip payload exceeds hard limit"));
    }
    let inline = if source.is_empty() {
        let (start, raw_source, raw_offset, raw_size, raw_compression) =
            post_compression_position(file, object_index)?;
        if raw_source != source
            || raw_offset != offset
            || raw_size != size
            || raw_compression != compression_format
        {
            return Err(unsupported(
                "AudioClip TypeTree and parser resource fields disagree",
            ));
        }
        Some((file.object_region(object_index).map_err(core_error)?, start))
    } else {
        None
    };
    let (binding, expected_region) = resolve_payload_region(
        source,
        offset,
        size,
        inline.as_ref().map(|(region, start)| (region, *start)),
        bundle_path,
        collection.resources(),
        approved_external,
    )
    .map_err(|error| match error.kind {
        UnityFailureKind::ApprovedInput => approved_input(error.message),
        UnityFailureKind::UnsupportedLayout => unsupported(error.message),
        UnityFailureKind::CandidateDecode => candidate(error.message),
        UnityFailureKind::Io => AppError::unity(UnityErrorKind::Io, error.message),
        UnityFailureKind::Unexpected => unexpected(error.message),
    })?;
    let limits = SimpleAssetReadLimits {
        maximum_string_bytes: 1024 * 1024,
        maximum_payload_bytes: 256 * 1024 * 1024,
        ..SimpleAssetReadLimits::default()
    };
    let read = |collection: &AssetCollection, file_index| {
        read_audio_clip_asset_by_file_index(collection, file_index, object_index, limits)
            .map_err(core_error)
    };
    let mut asset = read(collection, file_index)?;
    if asset.payload != expected_region {
        let selected_key = match &binding {
            PayloadBinding::BundleResource { key, .. } => Some(key.as_str()),
            PayloadBinding::External { source, .. } => Some(source.as_str()),
            PayloadBinding::Inline { .. } => None,
        };
        let selected = selected_key
            .and_then(|key| {
                collection
                    .resources()
                    .iter()
                    .find(|resource| resource.path == key)
            })
            .ok_or_else(|| {
                unsupported("AudioClip high-level payload did not match exact source")
            })?;
        let scoped =
            AssetCollection::from_loaded_parts(vec![loaded.clone()], vec![selected.clone()]);
        asset = read(&scoped, 0)?;
    }
    if asset.path_id != object.path_id || asset.name != name || asset.payload != expected_region {
        return Err(unsupported(
            "AudioClip TypeTree, core asset, and exact source disagree",
        ));
    }
    let metadata = collection.object_metadata(file_index, object.path_id);
    if let Some(metadata_name) = metadata.and_then(|entry| entry.name.as_deref())
        && metadata_name != name
    {
        return Err(unsupported("AudioClip container index name disagrees"));
    }
    let predecode_frames = predecode_frames(&asset.payload, asset.direct_wav)?;
    let metadata_completeness = match predecode_frames {
        PredecodeFrameCount::Unavailable => MetadataCompleteness::RequiresMeasurement,
        _ => MetadataCompleteness::CompleteFromValidatedHeader,
    };
    let layout = match binding {
        PayloadBinding::Inline { .. } => PayloadLayout::Inline,
        PayloadBinding::BundleResource { .. } => PayloadLayout::BundleResource,
        PayloadBinding::External { .. } => PayloadLayout::External,
    };
    let signature = CodecLayoutSignature {
        codec: codec_kind(asset.direct_wav),
        layout,
        compression_format,
        channels,
        sample_rate,
        bits_per_sample,
    };
    Ok(AudioAssetRef {
        bundle_path: bundle_path.into(),
        file_index,
        object_index,
        path_id: object.path_id,
        name,
        container: metadata
            .and_then(|entry| entry.container.as_ref().map(|value| value.to_string())),
        binding,
        metadata: AudioMetadata {
            channels,
            sample_rate,
            bits_per_sample,
            duration_seconds,
            compression_format,
            subsound_index,
        },
        metadata_completeness,
        predecode_frames,
        signature,
    })
}

fn load_exact_asset(
    collection: &AssetCollection,
    reference: &AudioAssetRef,
) -> Result<AudioClipAsset, AppError> {
    let loaded = collection
        .serialized_files()
        .get(reference.file_index)
        .ok_or_else(|| approved_input("AudioClip locator file is absent"))?;
    let expected = match &reference.binding {
        PayloadBinding::Inline { start, size, .. } => loaded
            .file
            .object_region(reference.object_index)
            .map_err(core_error)?
            .subregion(*start, *size)
            .map_err(core_error)?,
        PayloadBinding::BundleResource {
            key, offset, size, ..
        } => {
            let exact: Vec<_> = collection
                .resources()
                .iter()
                .filter(|resource| resource.path == *key)
                .collect();
            if exact.len() != 1 {
                return Err(approved_input("AudioClip bundle resource is not unique"));
            }
            exact[0]
                .region
                .subregion(*offset, *size)
                .map_err(core_error)?
        }
        PayloadBinding::External {
            source,
            offset,
            size,
        } => {
            let exact: Vec<_> = collection
                .resources()
                .iter()
                .filter(|resource| resource.path == *source)
                .collect();
            if exact.len() != 1 {
                return Err(approved_input("AudioClip external resource is not unique"));
            }
            exact[0]
                .region
                .subregion(*offset, *size)
                .map_err(core_error)?
        }
    };
    let limits = SimpleAssetReadLimits {
        maximum_string_bytes: 1024 * 1024,
        maximum_payload_bytes: 256 * 1024 * 1024,
        ..SimpleAssetReadLimits::default()
    };
    let mut asset = read_audio_clip_asset_by_file_index(
        collection,
        reference.file_index,
        reference.object_index,
        limits,
    )
    .map_err(core_error)?;
    if asset.payload != expected {
        let key = match &reference.binding {
            PayloadBinding::Inline { .. } => {
                return Err(unsupported("inline AudioClip payload changed"));
            }
            PayloadBinding::BundleResource { key, .. } => key.as_str(),
            PayloadBinding::External { source, .. } => source.as_str(),
        };
        let selected = collection
            .resources()
            .iter()
            .find(|resource| resource.path == key)
            .ok_or_else(|| approved_input("AudioClip selected resource disappeared"))?;
        let scoped =
            AssetCollection::from_loaded_parts(vec![loaded.clone()], vec![selected.clone()]);
        asset = read_audio_clip_asset_by_file_index(&scoped, 0, reference.object_index, limits)
            .map_err(core_error)?;
    }
    if asset.payload != expected
        || asset.path_id != reference.path_id
        || asset.name != reference.name
    {
        return Err(unsupported("AudioClip exact region or identity changed"));
    }
    Ok(asset)
}

struct BoundedWavSink<'a> {
    output: &'a mut dyn Write,
    maximum: u64,
    written: u64,
    prefix: [u8; 44],
    prefix_len: usize,
    peak_live_buffer_bytes: usize,
    hasher: Sha256,
}

impl<'a> BoundedWavSink<'a> {
    fn new(output: &'a mut dyn Write, maximum: u64) -> Self {
        Self {
            output,
            maximum,
            written: 0,
            prefix: [0; 44],
            prefix_len: 0,
            peak_live_buffer_bytes: 44,
            hasher: Sha256::new(),
        }
    }
    fn finish(self, reference: &AudioAssetRef) -> Result<DecodedAudioReceipt, AppError> {
        if self.prefix_len < 44 {
            return Err(candidate("decoded WAV header is truncated"));
        }
        let bytes = &self.prefix;
        if &bytes[0..4] != b"RIFF"
            || &bytes[8..12] != b"WAVE"
            || &bytes[12..16] != b"fmt "
            || &bytes[36..40] != b"data"
        {
            return Err(candidate("decoded WAV is not canonical PCM RIFF"));
        }
        let u16_at =
            |start| u16::from_le_bytes(bytes[start..start + 2].try_into().expect("two bytes"));
        let u32_at =
            |start| u32::from_le_bytes(bytes[start..start + 4].try_into().expect("four bytes"));
        if u32_at(16) != 16 || u16_at(20) != 1 {
            return Err(candidate("decoded WAV format is unsupported"));
        }
        let channels = u16_at(22);
        let sample_rate = u32_at(24);
        let byte_rate = u32_at(28);
        let block_align = u16_at(32);
        let bits_per_sample = u16_at(34);
        let data_bytes = u64::from(u32_at(40));
        if channels == 0
            || sample_rate == 0
            || !matches!(bits_per_sample, 8 | 16 | 24 | 32)
            || block_align == 0
            || u64::from(block_align) != u64::from(channels) * u64::from(bits_per_sample / 8)
            || u64::from(byte_rate) != u64::from(sample_rate) * u64::from(block_align)
            || data_bytes == 0
            || data_bytes % u64::from(block_align) != 0
            || self.written != 44 + data_bytes
            || self.written != 8 + u64::from(u32_at(4))
        {
            return Err(candidate("decoded WAV lengths or PCM layout are invalid"));
        }
        if channels != reference.metadata.channels
            || sample_rate != reference.metadata.sample_rate
            || bits_per_sample != reference.metadata.bits_per_sample
        {
            return Err(candidate("decoded WAV properties disagree with TypeTree"));
        }
        Ok(DecodedAudioReceipt {
            wav_bytes: self.written,
            wav_sha256: hex::encode(self.hasher.finalize()),
            channels,
            sample_rate,
            bits_per_sample,
            frame_count: data_bytes / u64::from(block_align),
            frame_count_source: FrameCountSource::ValidatedDecodedWav,
            peak_live_buffer_bytes: self.peak_live_buffer_bytes,
        })
    }
}

impl Write for BoundedWavSink<'_> {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        let new_total = self
            .written
            .checked_add(bytes.len() as u64)
            .ok_or_else(|| std::io::Error::other("WAV byte count overflow"))?;
        if new_total > self.maximum {
            return Err(std::io::Error::other("WAV hard limit exceeded"));
        }
        self.peak_live_buffer_bytes = self.peak_live_buffer_bytes.max(44 + bytes.len());
        self.output.write_all(bytes)?;
        let prefix_copy = (44 - self.prefix_len).min(bytes.len());
        self.prefix[self.prefix_len..self.prefix_len + prefix_copy]
            .copy_from_slice(&bytes[..prefix_copy]);
        self.prefix_len += prefix_copy;
        self.hasher.update(bytes);
        self.written = new_total;
        Ok(bytes.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        self.output.flush()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UnityFailureKind {
    CandidateDecode,
    ApprovedInput,
    UnsupportedLayout,
    Io,
    Unexpected,
}

#[derive(Debug)]
pub struct UnityFailure {
    pub kind: UnityFailureKind,
    pub message: String,
}

impl UnityFailure {
    fn new(kind: UnityFailureKind, message: impl Into<String>) -> Self {
        Self {
            kind,
            message: message.into(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PayloadBinding {
    Inline {
        declared_offset: u64,
        start: u64,
        size: u64,
    },
    BundleResource {
        source: String,
        key: String,
        offset: u64,
        size: u64,
    },
    External {
        source: String,
        offset: u64,
        size: u64,
    },
}

fn archive_entry(source: &str) -> Result<&str, UnityFailure> {
    let rest = source.strip_prefix("archive:/CAB-").ok_or_else(|| {
        UnityFailure::new(
            UnityFailureKind::UnsupportedLayout,
            "invalid archive source prefix",
        )
    })?;
    let (directory_id, filename) = rest.split_once('/').ok_or_else(|| {
        UnityFailure::new(
            UnityFailureKind::UnsupportedLayout,
            "archive source has no entry",
        )
    })?;
    let file_id = filename
        .strip_prefix("CAB-")
        .and_then(|s| s.strip_suffix(".resource"))
        .ok_or_else(|| {
            UnityFailure::new(
                UnityFailureKind::UnsupportedLayout,
                "archive entry shape is unsupported",
            )
        })?;
    if directory_id.len() != 32
        || !directory_id.bytes().all(|b| b.is_ascii_hexdigit())
        || file_id != directory_id
    {
        return Err(UnityFailure::new(
            UnityFailureKind::UnsupportedLayout,
            "archive CAB IDs do not match exactly",
        ));
    }
    Ok(filename)
}

/// Bind only the declared source range. The returned `Region` carries source identity.
pub fn resolve_payload_region(
    source: &str,
    declared_offset: u64,
    size: u64,
    inline: Option<(&Region, u64)>,
    owning_bundle: &str,
    resources: &[LoadedResource],
    approved_external: &[String],
) -> Result<(PayloadBinding, Region), UnityFailure> {
    if source.len() > 4096 {
        return Err(UnityFailure::new(
            UnityFailureKind::UnsupportedLayout,
            "resource path exceeds limit",
        ));
    }
    if source.is_empty() {
        let (object, start) = inline.ok_or_else(|| {
            UnityFailure::new(
                UnityFailureKind::UnsupportedLayout,
                "missing inline object source",
            )
        })?;
        if declared_offset > object.len() {
            return Err(UnityFailure::new(
                UnityFailureKind::UnsupportedLayout,
                "inline declared offset is out of range",
            ));
        }
        let region = object.subregion(start, size).map_err(|_| {
            UnityFailure::new(
                UnityFailureKind::UnsupportedLayout,
                "inline payload range is out of bounds",
            )
        })?;
        return Ok((
            PayloadBinding::Inline {
                declared_offset,
                start,
                size,
            },
            region,
        ));
    }
    if source.starts_with("archive:") {
        let filename = archive_entry(source)?;
        let key = format!("{owning_bundle}::{filename}");
        let mut exact = resources.iter().filter(|entry| entry.path == key);
        let selected = exact.next().ok_or_else(|| {
            UnityFailure::new(
                UnityFailureKind::ApprovedInput,
                "owned bundle resource is absent",
            )
        })?;
        if exact.next().is_some() {
            return Err(UnityFailure::new(
                UnityFailureKind::ApprovedInput,
                "duplicate exact bundle resource",
            ));
        }
        let region = selected
            .region
            .subregion(declared_offset, size)
            .map_err(|_| {
                UnityFailure::new(
                    UnityFailureKind::UnsupportedLayout,
                    "bundle resource range is out of bounds",
                )
            })?;
        return Ok((
            PayloadBinding::BundleResource {
                source: source.into(),
                key,
                offset: declared_offset,
                size,
            },
            region,
        ));
    }
    if !approved_external.iter().any(|path| path == source) {
        return Err(UnityFailure::new(
            UnityFailureKind::ApprovedInput,
            "undeclared external resource",
        ));
    }
    let mut exact = resources.iter().filter(|entry| entry.path == source);
    let selected = exact.next().ok_or_else(|| {
        UnityFailure::new(
            UnityFailureKind::ApprovedInput,
            "external resource is absent",
        )
    })?;
    if exact.next().is_some() {
        return Err(UnityFailure::new(
            UnityFailureKind::ApprovedInput,
            "duplicate exact external resource",
        ));
    }
    let region = selected
        .region
        .subregion(declared_offset, size)
        .map_err(|_| {
            UnityFailure::new(
                UnityFailureKind::UnsupportedLayout,
                "external resource range is out of bounds",
            )
        })?;
    Ok((
        PayloadBinding::External {
            source: source.into(),
            offset: declared_offset,
            size,
        },
        region,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use unity_rs_core::audio::Fsb5VorbisStream;

    fn vorbis(frames: u64) -> DirectWavKind {
        DirectWavKind::Fsb5Vorbis(Fsb5VorbisStream {
            data_offset: 64,
            data_length: 10,
            compressed_length: 10,
            frame_count: frames,
            channels: 2,
            sample_rate: 48_000,
            setup_crc: 0,
        })
    }
    fn bank(subsounds: u32) -> Region {
        let mut bytes = vec![0; 64];
        bytes[..4].copy_from_slice(b"FSB5");
        bytes[8..12].copy_from_slice(&subsounds.to_le_bytes());
        Region::from_bytes(bytes)
    }

    #[test]
    fn only_single_subsound_supported_header_yields_exact_frames() {
        assert_eq!(
            predecode_frames(&bank(1), Some(vorbis(123))).unwrap(),
            PredecodeFrameCount::ValidatedPayloadHeader(123)
        );
        assert_eq!(
            predecode_frames(&bank(2), Some(vorbis(123))).unwrap(),
            PredecodeFrameCount::Unavailable
        );
        assert_eq!(
            predecode_frames(&bank(1), Some(vorbis(0))).unwrap(),
            PredecodeFrameCount::Unavailable
        );
        assert_eq!(
            predecode_frames(&Region::from_bytes(vec![0; 64]), Some(vorbis(123))).unwrap(),
            PredecodeFrameCount::Unavailable
        );
    }

    #[test]
    fn selected_subsound_and_wav_frame_evidence_must_agree() {
        assert!(validate_subsound_index(0).is_ok());
        assert!(validate_subsound_index(-1).is_err());
        assert!(validate_subsound_index(1).is_err());
        assert_eq!(
            validate_frame_count(PredecodeFrameCount::ValidatedPayloadHeader(123), 123).unwrap(),
            FrameCountSource::ValidatedPayloadHeaderAndWav
        );
        assert_eq!(
            validate_frame_count(PredecodeFrameCount::Unavailable, 123).unwrap(),
            FrameCountSource::ValidatedDecodedWav
        );
        assert!(
            validate_frame_count(PredecodeFrameCount::ValidatedPayloadHeader(124), 123).is_err()
        );
    }

    #[test]
    fn bounded_sink_accounts_for_the_live_decoder_chunk_and_header() {
        let mut output = Vec::new();
        let mut sink = BoundedWavSink::new(&mut output, 2000);
        sink.write_all(&vec![0; 1000]).unwrap();
        assert_eq!(sink.peak_live_buffer_bytes, 1044);
    }
}
