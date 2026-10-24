//! Bundle-scoped access to the pinned pure-Rust Unity reader.
use crate::{
    audio::{AudioError, AudioErrorKind, CanonicalWavSink},
    error::{AppError, UnityErrorKind},
    inventory::{VerifiedBundleInputs, VerifiedSnapshotSet},
    model::{
        AudioAssetRef, AudioLimits, AudioMetadata, CodecKind, CodecLayoutSignature,
        DecodedAudioReceipt, FrameCountSource, MetadataCompleteness, PayloadLayout,
        PredecodeFrameCount, RawScriptObject,
    },
};
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
    serialized_file::SerializedFileHeader,
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
    /// Stream canonical PCM into a caller-owned staged sink. A late validation
    /// failure can leave partial bytes, so the caller must discard that stage.
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

const MAXIMUM_BUNDLE_BYTES: u64 = 512 * 1024 * 1024;

fn snapshot_result<T>(result: Result<T, AppError>) -> Result<T, AppError> {
    result.map_err(|error| match error {
        AppError::Validation(message) => approved_input(message),
        other => other,
    })
}

impl UnityReader {
    pub fn new(scratch: impl AsRef<Path>) -> Self {
        Self::with_bundle_limit(scratch, MAXIMUM_BUNDLE_BYTES)
    }
    pub fn with_bundle_limit(scratch: impl AsRef<Path>, maximum_bundle_bytes: u64) -> Self {
        Self {
            scratch: scratch.as_ref().to_path_buf(),
            maximum_bundle_bytes: maximum_bundle_bytes.min(MAXIMUM_BUNDLE_BYTES),
        }
    }
    pub fn maximum_bundle_bytes(&self) -> u64 {
        self.maximum_bundle_bytes
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
    bundle_opens: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SessionMetrics {
    pub bundle_opens: u64,
    pub peak_live_buffer_bytes: usize,
}

impl UnitySession {
    pub fn metrics(&self) -> SessionMetrics {
        SessionMetrics {
            bundle_opens: self.bundle_opens,
            peak_live_buffer_bytes: self.peak_live_buffer_bytes,
        }
    }
}

impl BundleSession for UnitySession {
    fn raw_script_objects(&mut self) -> Result<Vec<RawScriptObject>, AppError> {
        snapshot_result(self.snapshots.checkpoint())?;
        let mut scripts = Vec::new();
        let mut remaining = 256 * 1024 * 1024_usize;
        for (file_index, loaded) in self.collection.serialized_files().iter().enumerate() {
            for (object_index, object) in loaded.file.objects.iter().enumerate() {
                if object.class_id != unity_rs_core::monobehaviour::MONO_BEHAVIOUR_CLASS_ID {
                    continue;
                }
                let mut limits = type_tree_limits();
                limits.maximum_materialized_bytes =
                    limits.maximum_materialized_bytes.min(remaining);
                let value = loaded
                    .file
                    .read_type_tree_value_with_limits(object_index, limits)
                    .map_err(core_error)?;
                remaining = remaining
                    .checked_sub(retained_script_value_bytes(&value)?)
                    .ok_or_else(|| {
                        approved_input("aggregate retained script values exceed 256 MiB")
                    })?;
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
        scripts.sort_by(|a, b| {
            (&a.bundle_path, a.file_index, a.object_index, a.path_id).cmp(&(
                &b.bundle_path,
                b.file_index,
                b.object_index,
                b.path_id,
            ))
        });
        snapshot_result(self.snapshots.checkpoint())?;
        Ok(scripts)
    }
    fn audio_refs(&mut self) -> Result<Vec<AudioAssetRef>, AppError> {
        snapshot_result(self.snapshots.checkpoint())?;
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
        refs.sort_by(|a, b| {
            (&a.bundle_path, a.file_index, a.object_index, a.path_id).cmp(&(
                &b.bundle_path,
                b.file_index,
                b.object_index,
                b.path_id,
            ))
        });
        snapshot_result(self.snapshots.checkpoint())?;
        self.cached_audio_refs = Some(refs.clone());
        Ok(refs)
    }
    fn stream_audio(
        &mut self,
        reference: &AudioAssetRef,
        sink: &mut dyn Write,
        limits: &AudioLimits,
    ) -> Result<DecodedAudioReceipt, AppError> {
        snapshot_result(self.snapshots.checkpoint())?;
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
            let mut canonical = CanonicalWavSink::new(sink, limits.maximum_wav_bytes);
            let written = write_direct_wav(&asset.payload, kind, predicted, &mut canonical)
                .map_err(|error| match canonical.failure_kind() {
                    Some(AudioErrorKind::Malformed) => candidate("decoded WAV is malformed"),
                    Some(AudioErrorKind::Limit) => unsupported("canonical WAV exceeds hard limit"),
                    Some(AudioErrorKind::Io) => {
                        AppError::unity(UnityErrorKind::Io, "WAV destination write failed")
                    }
                    None => decode_error(error),
                })?;
            if written != predicted || written != canonical.source_bytes() {
                return Err(unexpected("decoded WAV byte count disagrees"));
            }
            let wav = canonical.finish().map_err(canonical_error)?;
            if wav.channels != reference.metadata.channels
                || wav.sample_rate != reference.metadata.sample_rate
                || wav.bits_per_sample != reference.metadata.bits_per_sample
            {
                return Err(candidate("decoded WAV properties disagree with TypeTree"));
            }
            let frame_count_source =
                validate_frame_count(reference.predecode_frames, wav.frame_count)?;
            self.peak_live_buffer_bytes =
                self.peak_live_buffer_bytes.max(wav.peak_live_buffer_bytes);
            Ok(DecodedAudioReceipt {
                wav_bytes: wav.wav_bytes,
                wav_sha256: wav.wav_sha256,
                channels: wav.channels,
                sample_rate: wav.sample_rate,
                bits_per_sample: wav.bits_per_sample,
                frame_count: wav.frame_count,
                frame_count_source,
                peak_live_buffer_bytes: wav.peak_live_buffer_bytes,
            })
        })();
        snapshot_result(self.snapshots.checkpoint())?;
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
        let snapshots = snapshot_result(approved.snapshot_set_shared(&scratch))?;
        let bundle_path = approved.bundle.relative_path.clone();
        let (collection, bundle_opens) =
            snapshot_result(snapshots.with_parser_paths(|bundle_snapshot, external| {
                open_pinned_bundle(bundle_snapshot, &bundle_path, external)
            }))?;
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
            bundle_opens,
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
fn canonical_error(error: AudioError) -> AppError {
    match error {
        AudioError::Malformed(message) => candidate(message),
        AudioError::Limit(message) => unsupported(message),
        AudioError::Io(error) => {
            AppError::unity(UnityErrorKind::Io, format!("WAV destination: {error}"))
        }
    }
}

#[derive(Clone, Copy)]
struct ParseBudgets {
    expanded_bytes: u64,
    metadata_bytes: u64,
    objects: usize,
}

impl Default for ParseBudgets {
    fn default() -> Self {
        Self {
            expanded_bytes: 2 * 1024 * 1024 * 1024,
            metadata_bytes: 256 * 1024 * 1024,
            objects: 200_000,
        }
    }
}

fn counted_unityfs_open(
    region: &Region,
    limits: BundleParseLimits,
    opens: &mut u64,
) -> Result<UnityFsBundle, AppError> {
    *opens = opens
        .checked_add(1)
        .ok_or_else(|| unexpected("bundle open counter overflow"))?;
    UnityFsBundle::open_with_limits(region, limits).map_err(core_error)
}

fn open_pinned_bundle(
    path: &Path,
    bundle_path: &str,
    external: &[(String, PathBuf)],
) -> Result<(AssetCollection, u64), AppError> {
    open_pinned_bundle_with_budgets(path, bundle_path, external, ParseBudgets::default())
}

fn open_pinned_bundle_with_budgets(
    path: &Path,
    bundle_path: &str,
    external: &[(String, PathBuf)],
    budgets: ParseBudgets,
) -> Result<(AssetCollection, u64), AppError> {
    let region = Region::from_file(path).map_err(core_error)?;
    let limits = BundleParseLimits {
        max_entries: 200_000,
        max_blocks: 200_000,
        max_path_length: 4096,
        max_entry_read_size: 512 * 1024 * 1024,
        ..BundleParseLimits::default()
    };
    let mut bundle_opens = 0;
    let bundle = counted_unityfs_open(&region, limits, &mut bundle_opens)?;
    let block_expanded = bundle
        .blocks
        .iter()
        .try_fold(0_u64, |total, block| {
            total.checked_add(u64::from(block.uncompressed_size))
        })
        .ok_or_else(|| approved_input("expanded storage-block sum overflow"))?;
    if block_expanded > budgets.expanded_bytes {
        return Err(approved_input("expanded storage blocks exceed hard limit"));
    }
    let expanded = bundle
        .entries
        .iter()
        .try_fold(0_u64, |total, entry| total.checked_add(entry.size))
        .ok_or_else(|| approved_input("expanded bundle size overflow"))?;
    if expanded > budgets.expanded_bytes {
        return Err(approved_input("expanded bundle exceeds hard limit"));
    }
    let mut files = Vec::new();
    let mut resources = Vec::new();
    let mut metadata_remaining = budgets.metadata_bytes;
    let mut objects_remaining = budgets.objects;
    for (index, entry) in bundle.entries.iter().enumerate() {
        let entry_region = bundle.entry_region(index).map_err(core_error)?;
        let mut header = vec![0; (entry.size as usize).min(HEADER_SCAN_LENGTH)];
        entry_region
            .read_exact_at(0, &mut header)
            .map_err(core_error)?;
        let label = format!("{bundle_path}::{}", entry.path);
        match detect_file_type(&header, entry.size).file_type {
            FileType::AssetsFile => {
                let mut header_reader = EndianReader::new(entry_region.cursor(), Endian::Big);
                let serialized_header =
                    SerializedFileHeader::read(&mut header_reader).map_err(core_error)?;
                if u64::from(serialized_header.metadata_size) > metadata_remaining {
                    return Err(approved_input(
                        "aggregate serialized metadata exceeds limit",
                    ));
                }
                let parse_limits = SerializedParseLimits {
                    maximum_metadata_bytes: metadata_remaining,
                    maximum_objects: objects_remaining,
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
                .map_err(|error| {
                    if objects_remaining == 0
                        && matches!(error, unity_rs_core::Error::InvalidData(_))
                    {
                        approved_input("aggregate serialized objects exceed limit")
                    } else {
                        core_error(error)
                    }
                })?;
                metadata_remaining = metadata_remaining
                    .checked_sub(u64::from(file.header.metadata_size))
                    .ok_or_else(|| approved_input("aggregate serialized metadata exceeds limit"))?;
                objects_remaining = objects_remaining
                    .checked_sub(file.objects.len())
                    .ok_or_else(|| approved_input("aggregate serialized objects exceed limit"))?;
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
        maximum_object_metadata_entries: budgets.objects,
        maximum_object_metadata_index_bytes: 256 * 1024 * 1024,
        maximum_reference_index_entries: budgets
            .objects
            .saturating_add(collection.serialized_files().len()),
        maximum_resource_index_entries: 200_000,
        maximum_container_assignments: budgets.objects,
        maximum_object_name_assignments: budgets.objects,
        maximum_total_object_name_bytes: 256 * 1024 * 1024,
        maximum_path_bytes: 4096,
        ..AssetLoadLimits::default()
    };
    collection
        .resolve_object_metadata(metadata_limits)
        .map_err(core_error)?;
    collection
        .rebuild_reference_index(metadata_limits)
        .map_err(core_error)?;
    collection
        .rebuild_resource_index(metadata_limits)
        .map_err(core_error)?;
    Ok((collection, bundle_opens))
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

/// Count the retained heap owned by one decoded value before appending it to
/// the bundle collection. The reader's remaining materialization budget also
/// bounds the temporary allocation while this value is being decoded.
fn retained_script_value_bytes(value: &TypeValue) -> Result<usize, AppError> {
    fn add(total: usize, more: usize) -> Result<usize, AppError> {
        total
            .checked_add(more)
            .ok_or_else(|| approved_input("script value size overflow"))
    }
    fn slots<T>(count: usize) -> Result<usize, AppError> {
        count
            .checked_mul(std::mem::size_of::<T>())
            .ok_or_else(|| approved_input("script value capacity overflow"))
    }
    fn heap(value: &TypeValue) -> Result<usize, AppError> {
        match value {
            TypeValue::String(text) => Ok(text.capacity()),
            TypeValue::ByteArray { bytes, .. } => Ok(bytes.capacity()),
            TypeValue::Array(items) => items
                .iter()
                .try_fold(slots::<TypeValue>(items.capacity())?, |total, item| {
                    add(total, heap(item)?)
                }),
            TypeValue::Object(fields) => fields
                .iter()
                .try_fold(slots::<TypeField>(fields.capacity())?, |total, field| {
                    add(add(total, field.name.capacity())?, heap(&field.value)?)
                }),
            TypeValue::Map(entries) => entries.iter().try_fold(
                slots::<unity_rs_core::type_tree::TypeMapEntry>(entries.capacity())?,
                |total, entry| add(add(total, heap(&entry.key)?)?, heap(&entry.value)?),
            ),
            _ => Ok(0),
        }
    }
    add(std::mem::size_of::<TypeValue>(), heap(value)?)
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
    let scoped = exact_scoped_collection(collection, file_index, &binding)?;
    let asset = read_audio_clip_asset_by_file_index(&scoped, 0, object_index, limits)
        .map_err(core_error)?;
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

/// Build a collection containing only the verified source before invoking the
/// high-level reader. Its portable-name lookup is never allowed to inspect a
/// competing resource, even transiently.
fn exact_scoped_collection(
    collection: &AssetCollection,
    file_index: usize,
    binding: &PayloadBinding,
) -> Result<AssetCollection, AppError> {
    let file = collection
        .serialized_files()
        .get(file_index)
        .ok_or_else(|| approved_input("AudioClip locator file is absent"))?
        .clone();
    let selected = match binding {
        PayloadBinding::Inline { .. } => None,
        PayloadBinding::BundleResource { key, .. } => Some(key.as_str()),
        PayloadBinding::External { source, .. } => Some(source.as_str()),
    };
    let resources = if let Some(key) = selected {
        let mut exact = collection
            .resources()
            .iter()
            .filter(|resource| resource.path == key);
        let resource = exact
            .next()
            .ok_or_else(|| approved_input("selected AudioClip resource is absent"))?;
        if exact.next().is_some() {
            return Err(approved_input("selected AudioClip resource is duplicated"));
        }
        vec![resource.clone()]
    } else {
        Vec::new()
    };
    Ok(AssetCollection::from_loaded_parts(vec![file], resources))
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
    let scoped = exact_scoped_collection(collection, reference.file_index, &reference.binding)?;
    let asset = read_audio_clip_asset_by_file_index(&scoped, 0, reference.object_index, limits)
        .map_err(core_error)?;
    if asset.payload != expected
        || asset.path_id != reference.path_id
        || asset.name != reference.name
    {
        return Err(unsupported("AudioClip exact region or identity changed"));
    }
    Ok(asset)
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
    use std::sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    };
    use unity_rs_core::audio::Fsb5VorbisStream;
    use unity_rs_core::source::Source;

    #[derive(Debug)]
    struct CountedSource {
        bytes: Vec<u8>,
        reads: Arc<AtomicUsize>,
    }
    impl Source for CountedSource {
        fn len(&self) -> u64 {
            self.bytes.len() as u64
        }
        fn read_exact_at(&self, offset: u64, output: &mut [u8]) -> std::io::Result<()> {
            self.reads.fetch_add(1, Ordering::SeqCst);
            let start = offset as usize;
            output.copy_from_slice(&self.bytes[start..start + output.len()]);
            Ok(())
        }
    }

    fn fixture_wav() -> Vec<u8> {
        let mut wav = b"RIFF".to_vec();
        wav.extend_from_slice(&40_u32.to_le_bytes());
        wav.extend_from_slice(b"WAVEfmt ");
        wav.extend_from_slice(&16_u32.to_le_bytes());
        wav.extend_from_slice(&1_u16.to_le_bytes());
        wav.extend_from_slice(&2_u16.to_le_bytes());
        wav.extend_from_slice(&48_000_u32.to_le_bytes());
        wav.extend_from_slice(&192_000_u32.to_le_bytes());
        wav.extend_from_slice(&4_u16.to_le_bytes());
        wav.extend_from_slice(&16_u16.to_le_bytes());
        wav.extend_from_slice(b"data");
        wav.extend_from_slice(&4_u32.to_le_bytes());
        wav.extend_from_slice(&[1, 2, 3, 4]);
        wav
    }

    fn fixture_audio_object(source: &str, payload: &[u8]) -> Vec<u8> {
        let mut object = Vec::new();
        object.extend_from_slice(&5_i32.to_le_bytes());
        object.extend_from_slice(b"theme");
        while !object.len().is_multiple_of(4) {
            object.push(0);
        }
        object.extend_from_slice(&0_i32.to_le_bytes());
        object.extend_from_slice(&2_i32.to_le_bytes());
        object.extend_from_slice(&48_000_i32.to_le_bytes());
        object.extend_from_slice(&16_i32.to_le_bytes());
        object.extend_from_slice(&0_f32.to_le_bytes());
        object.push(0);
        while !object.len().is_multiple_of(4) {
            object.push(0);
        }
        object.extend_from_slice(&0_i32.to_le_bytes());
        object.extend_from_slice(&[0_u8; 3]);
        while !object.len().is_multiple_of(4) {
            object.push(0);
        }
        object.extend_from_slice(&i32::try_from(source.len()).unwrap().to_le_bytes());
        object.extend_from_slice(source.as_bytes());
        if !source.is_empty() {
            while !object.len().is_multiple_of(4) {
                object.push(0);
            }
        }
        object.extend_from_slice(&0_i64.to_le_bytes());
        object.extend_from_slice(&(payload.len() as i64).to_le_bytes());
        object.extend_from_slice(&1_i32.to_le_bytes());
        if source.is_empty() {
            object.extend_from_slice(payload);
        }
        object
    }

    #[test]
    fn exact_scoped_adapter_read_never_touches_an_earlier_basename_collision() {
        const CAB: &str = "0123456789abcdef0123456789abcdef";
        let name = format!("CAB-{CAB}.resource");
        let uri = format!("archive:/CAB-{CAB}/{name}");
        let key = format!("owning.bundle::{name}");
        let wav = fixture_wav();
        let object = fixture_audio_object(&uri, &wav);
        let file = SerializedFile::open(Region::from_bytes(fixture_asset(83, &object))).unwrap();
        let reads = Arc::new(AtomicUsize::new(0));
        let wrong = Region::new(
            Arc::new(CountedSource {
                bytes: wav.clone(),
                reads: reads.clone(),
            }),
            0,
            wav.len() as u64,
        )
        .unwrap();
        let collection = AssetCollection::from_loaded_parts(
            vec![LoadedSerializedFile {
                path: "owning.bundle::audio.assets".into(),
                file,
            }],
            vec![
                LoadedResource {
                    path: format!("other.bundle::{name}"),
                    region: wrong,
                },
                LoadedResource {
                    path: key.clone(),
                    region: Region::from_bytes(wav.clone()),
                },
            ],
        );
        let reference = AudioAssetRef {
            bundle_path: "owning.bundle".into(),
            file_index: 0,
            object_index: 0,
            path_id: 7,
            name: "theme".into(),
            container: None,
            binding: PayloadBinding::BundleResource {
                source: uri,
                key,
                offset: 0,
                size: wav.len() as u64,
            },
            metadata: AudioMetadata {
                channels: 2,
                sample_rate: 48_000,
                bits_per_sample: 16,
                duration_seconds: 0.0,
                compression_format: 1,
                subsound_index: 0,
            },
            metadata_completeness: MetadataCompleteness::RequiresMeasurement,
            predecode_frames: PredecodeFrameCount::Unavailable,
            signature: CodecLayoutSignature {
                codec: CodecKind::ExistingWave,
                layout: PayloadLayout::BundleResource,
                compression_format: 1,
                channels: 2,
                sample_rate: 48_000,
                bits_per_sample: 16,
            },
        };
        let _ = read_audio_clip_asset_by_file_index(
            &collection,
            0,
            0,
            SimpleAssetReadLimits::default(),
        )
        .unwrap();
        assert!(
            reads.load(Ordering::SeqCst) > 0,
            "fixture must expose pinned basename fallback"
        );
        reads.store(0, Ordering::SeqCst);
        let asset = load_exact_asset(&collection, &reference).unwrap();
        assert_eq!(asset.payload.read_to_vec(64).unwrap(), wav);
        assert_eq!(reads.load(Ordering::SeqCst), 0);
    }

    fn fixture_session(
        layout: PayloadLayout,
    ) -> (UnitySession, tempfile::TempDir, tempfile::TempDir) {
        fixture_session_with_wav(layout, fixture_wav())
    }

    fn fixture_session_with_wav(
        layout: PayloadLayout,
        wav: Vec<u8>,
    ) -> (UnitySession, tempfile::TempDir, tempfile::TempDir) {
        use crate::{
            config::InputEntry, hash::sha256_bytes, inventory::VerifiedInputFile,
            paths::ValidatedAppPath,
        };
        let app_dir = tempfile::tempdir_in(".").unwrap();
        let scratch = tempfile::tempdir_in(".").unwrap();
        std::fs::write(app_dir.path().join("fixture.bundle"), b"snapshot").unwrap();
        let app = ValidatedAppPath::new(app_dir.path()).unwrap();
        let approved = VerifiedBundleInputs {
            bundle: VerifiedInputFile::open(
                &app,
                &InputEntry {
                    relative_path: "fixture.bundle".into(),
                    size: 8,
                    sha256: sha256_bytes(b"snapshot"),
                },
            )
            .unwrap(),
            resources: vec![],
        };
        let snapshots = approved
            .snapshot_set_shared(&scratch.path().canonicalize().unwrap())
            .unwrap();
        const CAB: &str = "0123456789abcdef0123456789abcdef";
        let source = match layout {
            PayloadLayout::Inline => "".to_owned(),
            PayloadLayout::BundleResource => format!("archive:/CAB-{CAB}/CAB-{CAB}.resource"),
            PayloadLayout::External => "owned/path/shared.resS".into(),
        };
        let object = fixture_audio_object(&source, &wav);
        let file = fixture_audio_file(&object, if source.is_empty() { wav.len() } else { 0 });
        let files = vec![
            LoadedSerializedFile {
                path: "fixture.bundle::one.assets".into(),
                file: file.clone(),
            },
            LoadedSerializedFile {
                path: "fixture.bundle::two.assets".into(),
                file,
            },
        ];
        let resources = match layout {
            PayloadLayout::Inline => vec![],
            _ => {
                let key = if layout == PayloadLayout::BundleResource {
                    format!("fixture.bundle::CAB-{CAB}.resource")
                } else {
                    source.clone()
                };
                vec![
                    LoadedResource {
                        path: if layout == PayloadLayout::BundleResource {
                            format!("other.bundle::CAB-{CAB}.resource")
                        } else {
                            "other/path/shared.resS".into()
                        },
                        region: Region::from_bytes(vec![9; wav.len()]),
                    },
                    LoadedResource {
                        path: key,
                        region: Region::from_bytes(wav.clone()),
                    },
                ]
            }
        };
        let session = UnitySession {
            collection: AssetCollection::from_loaded_parts(files, resources),
            snapshots,
            bundle_path: "fixture.bundle".into(),
            approved_external: if layout == PayloadLayout::External {
                vec![source]
            } else {
                vec![]
            },
            cached_audio_refs: None,
            peak_live_buffer_bytes: 0,
            bundle_opens: 1,
        };
        (session, app_dir, scratch)
    }

    #[test]
    fn actual_bundle_session_streams_inline_external_and_bundle_resource_then_recovers() {
        struct Interrupt;
        impl Write for Interrupt {
            fn write(&mut self, _: &[u8]) -> std::io::Result<usize> {
                Err(std::io::Error::other("controlled interruption"))
            }
            fn flush(&mut self) -> std::io::Result<()> {
                Ok(())
            }
        }
        for layout in [
            PayloadLayout::Inline,
            PayloadLayout::External,
            PayloadLayout::BundleResource,
        ] {
            let (mut session, _app, _scratch) = fixture_session(layout);
            let refs = session.audio_refs().unwrap();
            assert_eq!(refs.len(), 2);
            assert!(
                refs.iter()
                    .all(|reference| reference.signature.layout == layout)
            );
            let first = &refs[0];
            let second = &refs[1];
            let mut wav = Vec::new();
            let receipt = session
                .stream_audio(first, &mut wav, &AudioLimits::default())
                .unwrap();
            assert_eq!(receipt.frame_count, 1);
            assert_eq!(
                receipt.frame_count_source,
                FrameCountSource::ValidatedDecodedWav
            );
            assert_eq!(wav, fixture_wav());
            assert_eq!(
                session
                    .stream_audio(first, &mut Interrupt, &AudioLimits::default())
                    .unwrap_err()
                    .unity_kind(),
                Some(UnityErrorKind::Io)
            );
            let mut later = Vec::new();
            session
                .stream_audio(second, &mut later, &AudioLimits::default())
                .unwrap();
            assert_eq!(later, wav);
            assert_eq!(session.metrics().bundle_opens, 1);
        }
    }

    #[test]
    fn existing_wave_with_ancillary_chunk_streams_as_canonical_pcm() {
        let mut source = fixture_wav();
        source.splice(36..36, [b'J', b'U', b'N', b'K', 4, 0, 0, 0, 9, 8, 7, 6]);
        source[4..8].copy_from_slice(&52_u32.to_le_bytes());
        let (mut session, _app, _scratch) =
            fixture_session_with_wav(PayloadLayout::External, source);
        let reference = session.audio_refs().unwrap().remove(0);
        let mut output = Vec::new();
        let receipt = session
            .stream_audio(&reference, &mut output, &AudioLimits::default())
            .unwrap();
        assert_eq!(output, fixture_wav());
        assert_eq!(receipt.wav_bytes, output.len() as u64);
        assert_eq!(receipt.frame_count, 1);
    }

    fn fixture_bundle(blocks: &[(u32, u16, Vec<u8>)], entries: &[(u64, u64, &str)]) -> Vec<u8> {
        let mut info = vec![0_u8; 16];
        info.extend_from_slice(&i32::try_from(blocks.len()).unwrap().to_be_bytes());
        for (uncompressed, compression, data) in blocks {
            info.extend_from_slice(&uncompressed.to_be_bytes());
            info.extend_from_slice(&u32::try_from(data.len()).unwrap().to_be_bytes());
            info.extend_from_slice(&compression.to_be_bytes());
        }
        info.extend_from_slice(&i32::try_from(entries.len()).unwrap().to_be_bytes());
        for (offset, size, name) in entries {
            info.extend_from_slice(&i64::try_from(*offset).unwrap().to_be_bytes());
            info.extend_from_slice(&i64::try_from(*size).unwrap().to_be_bytes());
            info.extend_from_slice(&0_u32.to_be_bytes());
            info.extend_from_slice(name.as_bytes());
            info.push(0);
        }
        let mut bytes = b"UnityFS\0".to_vec();
        bytes.extend_from_slice(&6_u32.to_be_bytes());
        bytes.extend_from_slice(b"5.x.x\0");
        bytes.extend_from_slice(b"6000.0.48f1\0");
        let size_at = bytes.len();
        bytes.extend_from_slice(&0_i64.to_be_bytes());
        bytes.extend_from_slice(&u32::try_from(info.len()).unwrap().to_be_bytes());
        bytes.extend_from_slice(&u32::try_from(info.len()).unwrap().to_be_bytes());
        bytes.extend_from_slice(&0x40_u32.to_be_bytes());
        bytes.extend_from_slice(&info);
        for (_, _, data) in blocks {
            bytes.extend_from_slice(data);
        }
        let size = i64::try_from(bytes.len()).unwrap();
        bytes[size_at..size_at + 8].copy_from_slice(&size.to_be_bytes());
        bytes
    }

    fn fixture_asset(class_id: i32, object: &[u8]) -> Vec<u8> {
        fixture_asset_with_count(class_id, object, 1)
    }

    fn fixture_asset_with_count(class_id: i32, object: &[u8], count: usize) -> Vec<u8> {
        let mut metadata = b"6000.0.48f1\0".to_vec();
        metadata.extend_from_slice(&13_i32.to_le_bytes());
        metadata.push(0);
        metadata.extend_from_slice(&1_i32.to_le_bytes());
        metadata.extend_from_slice(&class_id.to_le_bytes());
        metadata.push(0);
        metadata.extend_from_slice(&(-1_i16).to_le_bytes());
        metadata.extend_from_slice(&[0_u8; 16]);
        metadata.extend_from_slice(&i32::try_from(count).unwrap().to_le_bytes());
        while !(48 + metadata.len()).is_multiple_of(4) {
            metadata.push(0);
        }
        for index in 0..count {
            metadata.extend_from_slice(&i64::try_from(index + 7).unwrap().to_le_bytes());
            metadata.extend_from_slice(&u64::try_from(index * object.len()).unwrap().to_le_bytes());
            metadata.extend_from_slice(&u32::try_from(object.len()).unwrap().to_le_bytes());
            metadata.extend_from_slice(&0_i32.to_le_bytes());
        }
        for _ in 0..3 {
            metadata.extend_from_slice(&0_i32.to_le_bytes());
        }
        metadata.push(0);
        let metadata_size = u32::try_from(metadata.len()).unwrap();
        let data_offset = (48 + u64::from(metadata_size)).div_ceil(16) * 16;
        let file_size = data_offset + (object.len() * count) as u64;
        let mut bytes = vec![0_u8; 48];
        bytes[8..12].copy_from_slice(&22_u32.to_be_bytes());
        bytes[20..24].copy_from_slice(&metadata_size.to_be_bytes());
        bytes[24..32].copy_from_slice(&i64::try_from(file_size).unwrap().to_be_bytes());
        bytes[32..40].copy_from_slice(&i64::try_from(data_offset).unwrap().to_be_bytes());
        bytes.extend_from_slice(&metadata);
        bytes.resize(data_offset as usize, 0);
        for _ in 0..count {
            bytes.extend_from_slice(object);
        }
        bytes
    }

    fn fixture_type_node(
        type_name: &str,
        field_name: &str,
        level: u32,
        align: bool,
    ) -> unity_rs_core::serialized::TypeTreeNode {
        unity_rs_core::serialized::TypeTreeNode {
            type_name: type_name.into(),
            field_name: field_name.into(),
            byte_size: -1,
            index: 0,
            type_flags: 0,
            version: 1,
            meta_flags: if align { 0x4000 } else { 0 },
            level,
            type_string_offset: None,
            name_string_offset: None,
            reference_type_hash: 0,
        }
    }

    fn fixture_audio_file(object: &[u8], inline_bytes: usize) -> SerializedFile {
        use unity_rs_core::serialized::TypeTree;
        let mut file = SerializedFile::open(Region::from_bytes(fixture_asset(83, object))).unwrap();
        let node = fixture_type_node;
        let mut nodes = vec![
            node("AudioClip", "Base", 0, false),
            node("string", "m_Name", 1, true),
            node("Array", "Array", 2, true),
            node("int", "size", 3, false),
            unity_rs_core::serialized::TypeTreeNode {
                byte_size: 1,
                ..node("char", "data", 3, false)
            },
            node("int", "m_LoadType", 1, false),
            node("int", "m_Channels", 1, false),
            node("int", "m_Frequency", 1, false),
            node("int", "m_BitsPerSample", 1, false),
            node("float", "m_Length", 1, false),
            node("bool", "m_IsTrackerFormat", 1, false),
            node("bool", "m_Ambisonic", 1, true),
            node("int", "m_SubsoundIndex", 1, false),
            node("bool", "m_PreloadAudioData", 1, false),
            node("bool", "m_LoadInBackground", 1, false),
            node("bool", "m_Legacy3D", 1, true),
            node("StreamingInfo", "m_Resource", 1, false),
            node("string", "m_Source", 2, true),
            node("Array", "Array", 3, true),
            node("int", "size", 4, false),
            unity_rs_core::serialized::TypeTreeNode {
                byte_size: 1,
                ..node("char", "data", 4, false)
            },
            node("SInt64", "m_Offset", 2, false),
            node("SInt64", "m_Size", 2, false),
            node("int", "m_CompressionFormat", 1, false),
        ];
        for index in 0..inline_bytes {
            nodes.push(node("UInt8", &format!("m_Inline{index}"), 1, false));
        }
        file.types[0].type_tree = Some(TypeTree {
            nodes,
            string_buffer: Vec::new(),
        });
        file.type_tree_enabled = true;
        file
    }

    fn fixture_file(bytes: &[u8]) -> tempfile::NamedTempFile {
        let file = tempfile::NamedTempFile::new().unwrap();
        std::fs::write(file.path(), bytes).unwrap();
        file
    }

    #[test]
    fn unreferenced_storage_blocks_count_toward_the_expanded_ceiling() {
        let blocks: Vec<_> = (0..5)
            .map(|_| (512 * 1024 * 1024, 2_u16, vec![0_u8]))
            .collect();
        let file = fixture_file(&fixture_bundle(&blocks, &[(0, 0, "empty")]));
        let error = open_pinned_bundle(file.path(), "fixture.bundle", &[]).unwrap_err();
        assert_eq!(error.unity_kind(), Some(UnityErrorKind::ApprovedInput));
    }

    #[test]
    fn metadata_and_object_budgets_are_aggregate_across_serialized_entries() {
        let asset = fixture_asset(9999, b"x");
        let mut data = asset.clone();
        data.extend_from_slice(&asset);
        let bundle = fixture_bundle(
            &[(data.len() as u32, 0, data)],
            &[
                (0, asset.len() as u64, "one.assets"),
                (asset.len() as u64, asset.len() as u64, "two.assets"),
            ],
        );
        let file = fixture_file(&bundle);
        let metadata_bytes = u64::from(u32::from_be_bytes(asset[20..24].try_into().unwrap()));
        let object_error = open_pinned_bundle_with_budgets(
            file.path(),
            "fixture.bundle",
            &[],
            ParseBudgets {
                expanded_bytes: 2 * 1024 * 1024 * 1024,
                metadata_bytes: 256 * 1024 * 1024,
                objects: 1,
            },
        )
        .unwrap_err();
        assert_eq!(
            object_error.unity_kind(),
            Some(UnityErrorKind::ApprovedInput)
        );
        let metadata_error = open_pinned_bundle_with_budgets(
            file.path(),
            "fixture.bundle",
            &[],
            ParseBudgets {
                expanded_bytes: 2 * 1024 * 1024 * 1024,
                metadata_bytes: metadata_bytes + 1,
                objects: 200_000,
            },
        )
        .unwrap_err();
        assert_eq!(
            metadata_error.unity_kind(),
            Some(UnityErrorKind::ApprovedInput)
        );
    }

    #[test]
    fn exact_object_cap_allows_later_empty_serialized_file_but_rejects_added_object() {
        let at_cap = fixture_asset_with_count(9999, b"", 200_000);
        let empty = fixture_asset_with_count(9999, b"", 0);
        let nonempty = fixture_asset(9999, b"");
        let mut accepted_data = at_cap.clone();
        accepted_data.extend_from_slice(&empty);
        let bundle = fixture_bundle(
            &[(accepted_data.len() as u32, 0, accepted_data)],
            &[
                (0, at_cap.len() as u64, "at-cap.assets"),
                (at_cap.len() as u64, empty.len() as u64, "empty.assets"),
            ],
        );
        let accepted = fixture_file(&bundle);
        let (collection, _) = open_pinned_bundle(accepted.path(), "fixture.bundle", &[]).unwrap();
        assert_eq!(collection.serialized_files().len(), 2);
        assert_eq!(collection.serialized_files()[0].file.objects.len(), 200_000);
        assert!(collection.serialized_files()[1].file.objects.is_empty());

        let mut rejected_data = at_cap.clone();
        rejected_data.extend_from_slice(&nonempty);
        let rejected_bundle = fixture_bundle(
            &[(rejected_data.len() as u32, 0, rejected_data)],
            &[
                (0, at_cap.len() as u64, "at-cap.assets"),
                (at_cap.len() as u64, nonempty.len() as u64, "extra.assets"),
            ],
        );
        let rejected = fixture_file(&rejected_bundle);
        let error = open_pinned_bundle(rejected.path(), "fixture.bundle", &[]).unwrap_err();
        assert_eq!(error.unity_kind(), Some(UnityErrorKind::ApprovedInput));
    }

    #[test]
    fn malformed_serialized_entry_is_unsupported_not_approved_input() {
        let mut asset = fixture_asset(9999, b"x");
        asset[20..24].copy_from_slice(&1_u32.to_be_bytes());
        let bundle = fixture_bundle(
            &[(asset.len() as u32, 0, asset.clone())],
            &[(0, asset.len() as u64, "malformed.assets")],
        );
        let file = fixture_file(&bundle);
        let error = open_pinned_bundle(file.path(), "fixture.bundle", &[]).unwrap_err();
        assert_eq!(
            error.unity_kind(),
            Some(UnityErrorKind::UnsupportedLayoutOrCodec)
        );
    }

    #[test]
    fn counted_unityfs_open_records_each_real_invocation() {
        let data = b"data".to_vec();
        let region = Region::from_bytes(fixture_bundle(&[(4, 0, data)], &[(0, 4, "a.resource")]));
        let mut opens = 0;
        for _ in 0..2 {
            counted_unityfs_open(&region, BundleParseLimits::default(), &mut opens).unwrap();
        }
        assert_eq!(opens, 2);
    }

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
    fn retained_script_value_accounts_for_nested_string_and_vector_capacity() {
        let value = TypeValue::Array(vec![TypeValue::String("sample".to_owned())]);
        assert!(
            retained_script_value_bytes(&value).unwrap() >= std::mem::size_of::<TypeValue>() + 6
        );
    }
}
