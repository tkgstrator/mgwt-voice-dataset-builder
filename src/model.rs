use crate::unity::PayloadBinding;
use crate::{normalize::NormalizationResult, speaker::SpeakerResolution};
use unity_rs_core::type_tree::TypeValue;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScriptProvenance {
    pub bundle_path: String,
    pub file_index: usize,
    pub object_index: usize,
    pub path_id: i64,
    pub container_path: String,
    pub command_index: usize,
}

#[derive(Debug, Clone)]
pub struct ParsedScriptReference {
    pub candidate_id: String,
    pub voice_id: String,
    pub raw_author_id: Option<String>,
    pub raw_display_text: String,
    pub transcription: NormalizationResult,
    pub provenance: ScriptProvenance,
    pub script_path: String,
    pub script_name: String,
    pub neighbor_commands: Vec<(usize, String)>,
}

#[derive(Debug, Clone)]
pub struct ScriptReference {
    pub parsed: ParsedScriptReference,
    pub speaker_resolution: SpeakerResolution,
}

#[derive(Debug, Clone)]
pub struct CandidateRejection {
    pub candidate_id: String,
    pub group_id: Option<String>,
    pub terminal_stage: &'static str,
    pub reason: &'static str,
    pub provenance: ScriptProvenance,
    pub blocking_group_ids: Vec<String>,
}

#[derive(Debug, Clone)]
pub struct SpeakerEvidenceRecord {
    pub speaker: String,
    pub source: &'static str,
    pub voice_id_sha256: Option<String>,
    pub provenance: ScriptProvenance,
}

#[derive(Debug, Clone)]
pub struct ScriptParseResult {
    pub parsed_references: Vec<ParsedScriptReference>,
    pub candidate_rejections: Vec<CandidateRejection>,
    pub speaker_evidence: Vec<SpeakerEvidenceRecord>,
    pub object_bundle_path: String,
    pub object_file_index: usize,
    pub object_index: usize,
    pub object_path_id: i64,
    pub object_container_path_sha256: String,
    pub script_path_sha256: String,
    pub command_count: usize,
    pub reference_count: usize,
    pub text_entry_count: usize,
}

#[derive(Debug, Clone)]
pub struct ScriptResolutionResult {
    pub references: Vec<ScriptReference>,
    pub candidate_rejections: Vec<CandidateRejection>,
}

#[derive(Debug, Clone)]
pub struct RawScriptObject {
    pub bundle_path: String,
    pub file_index: usize,
    pub object_index: usize,
    pub path_id: i64,
    pub name: Option<String>,
    pub container: Option<String>,
    pub value: TypeValue,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PredecodeFrameCount {
    Unavailable,
    ValidatedPayloadHeader(u64),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FrameCountSource {
    ValidatedPayloadHeaderAndWav,
    ValidatedDecodedWav,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MetadataCompleteness {
    RequiresMeasurement,
    CompleteFromValidatedHeader,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct AudioMetadata {
    pub channels: u16,
    pub sample_rate: u32,
    pub bits_per_sample: u16,
    pub duration_seconds: f32,
    pub compression_format: i32,
    pub subsound_index: i32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum CodecKind {
    ExistingWave,
    LegacyPcm16,
    Fsb5Pcm,
    Fsb5Ima,
    Fsb5Dsp,
    Fsb5Vag,
    Fsb5Hevag,
    Fsb5Fadpcm,
    Fsb5Mpeg,
    Fsb5Opus,
    Fsb5Vorbis,
    Unsupported,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum PayloadLayout {
    Inline,
    BundleResource,
    External,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct CodecLayoutSignature {
    pub codec: CodecKind,
    pub layout: PayloadLayout,
    pub compression_format: i32,
    pub channels: u16,
    pub sample_rate: u32,
    pub bits_per_sample: u16,
}
#[derive(Debug, Clone, PartialEq)]
pub struct AudioAssetRef {
    pub bundle_path: String,
    pub file_index: usize,
    pub object_index: usize,
    pub path_id: i64,
    pub name: String,
    pub container: Option<String>,
    pub binding: PayloadBinding,
    pub metadata: AudioMetadata,
    pub metadata_completeness: MetadataCompleteness,
    pub predecode_frames: PredecodeFrameCount,
    pub signature: CodecLayoutSignature,
}
#[derive(Debug, Clone, Copy)]
pub struct AudioLimits {
    pub maximum_wav_bytes: u64,
    pub maximum_expansion_ratio: u64,
}
impl Default for AudioLimits {
    fn default() -> Self {
        Self {
            maximum_wav_bytes: 256 * 1024 * 1024,
            maximum_expansion_ratio: 4096,
        }
    }
}
#[derive(Debug, Clone)]
pub struct DecodedAudioReceipt {
    pub wav_bytes: u64,
    pub wav_sha256: String,
    pub channels: u16,
    pub sample_rate: u32,
    pub bits_per_sample: u16,
    pub frame_count: u64,
    pub frame_count_source: FrameCountSource,
    pub peak_live_buffer_bytes: usize,
}
