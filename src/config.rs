//! Portable, strict approval records. No application location is trusted by a config.
use crate::{
    error::AppError,
    hash::{sha256_bytes, valid_sha256},
};
use serde::{Deserialize, Serialize};
use std::path::{Component, Path};

const MAX_CONFIG_BYTES: usize = 1024 * 1024;
const MAX_TEXT_BYTES: usize = 512;
const EXPECTED_SPEAKER_EVIDENCE_SHA256: &str =
    "b1d37d048c911c2564188725101c01d84ef95b7220f7fbb650028fd5d1452427";
const FORMER_MANIFEST_SHA256: &str =
    "b6fc6867ae2d32c59783c83a7fcd34dbdd7b660d855b4bbc432c5e1729a17e61";
const FORMER_CANDIDATE_SHA256: &str =
    "e8112ff95dd504fa9819316d4814716291d7fc14c7b63d53d7a351b98e7aec2f";
const FORMER_FULL_INPUT_FINGERPRINT: &str =
    "fe209715f41b88d7b1c54c76b22aaa46819d5f1110effb02fa2677fb48b5b1c6";

fn invalid(message: impl Into<String>) -> AppError {
    AppError::validation(message)
}
fn text_ok(value: &str) -> bool {
    !value.is_empty() && value.len() <= MAX_TEXT_BYTES && !value.chars().any(char::is_control)
}
fn path_ok(path: &str) -> bool {
    path.len() <= MAX_TEXT_BYTES
        && !path.contains('\\')
        && !path
            .split('/')
            .any(|p| p.is_empty() || p == "." || p == "..")
        && !Path::new(path).is_absolute()
        && Path::new(path)
            .components()
            .all(|c| matches!(c, Component::Normal(_)))
}
fn parse_json<T: for<'de> Deserialize<'de>>(bytes: &[u8]) -> Result<T, AppError> {
    if bytes.len() > MAX_CONFIG_BYTES {
        return Err(invalid("configuration exceeds size limit"));
    }
    serde_json::from_slice(bytes).map_err(|e| invalid(format!("configuration JSON: {e}")))
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AppIdentity {
    pub app_info_company: String,
    pub app_info_product: String,
    pub bundle_identifier: String,
    pub bundle_version: String,
    pub short_version: String,
    pub unity_build_number: String,
}
impl AppIdentity {
    fn validate(&self) -> Result<(), AppError> {
        for field in [
            &self.app_info_company,
            &self.app_info_product,
            &self.bundle_identifier,
            &self.bundle_version,
            &self.short_version,
            &self.unity_build_number,
        ] {
            if !text_ok(field) {
                return Err(invalid("invalid app identity text"));
            }
        }
        if self.app_info_company != "Re,AER"
            || self.app_info_product != "manosaba"
            || self.bundle_identifier != "com.manosaba.game"
            || self.bundle_version != "1"
            || self.short_version != "1.1.2"
            || self.unity_build_number != "170d2541580d"
        {
            return Err(invalid("reviewed app identity mismatch"));
        }
        Ok(())
    }
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "lowercase")]
pub enum BundleKind {
    Script,
    Voice,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct InputEntry {
    pub relative_path: String,
    pub size: u64,
    pub sha256: String,
}
impl InputEntry {
    fn validate(&self) -> Result<(), AppError> {
        if !path_ok(&self.relative_path) || !valid_sha256(&self.sha256) || self.size == 0 {
            return Err(invalid("invalid input entry"));
        }
        Ok(())
    }
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BundleEntry {
    pub kind: BundleKind,
    pub relative_path: String,
    pub size: u64,
    pub sha256: String,
}
impl BundleEntry {
    fn validate(&self) -> Result<(), AppError> {
        InputEntry {
            relative_path: self.relative_path.clone(),
            size: self.size,
            sha256: self.sha256.clone(),
        }
        .validate()
    }
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ApprovedManifest {
    pub schema_version: String,
    pub approval_state: String,
    pub app_identity: AppIdentity,
    pub unity_version: String,
    pub bundles: Vec<BundleEntry>,
    pub identity_files: Vec<InputEntry>,
    pub resources: Vec<InputEntry>,
    pub script_bundle_count: usize,
    pub voice_bundle_count: usize,
    pub total_consumed_bytes: u64,
}
impl ApprovedManifest {
    pub fn parse(bytes: &[u8]) -> Result<Self, AppError> {
        let manifest: Self = parse_json(bytes)?;
        if manifest.schema_version != "manosaba-inputs-v1-portable"
            || manifest.approval_state != "approved"
            || manifest.unity_version != "6000.0.48f1"
        {
            return Err(invalid("manifest schema or approval mismatch"));
        }
        manifest.app_identity.validate()?;
        if manifest.bundles.len() != 59
            || manifest.identity_files.len() != 3
            || manifest.script_bundle_count != 24
            || manifest.voice_bundle_count != 35
        {
            return Err(invalid("manifest entry count mismatch"));
        }
        let mut all = Vec::new();
        let mut scripts = 0;
        let mut voices = 0;
        let mut size = 0_u64;
        for entry in &manifest.bundles {
            entry.validate()?;
            all.push(entry.relative_path.as_str());
            size = size
                .checked_add(entry.size)
                .ok_or_else(|| invalid("size overflow"))?;
            match entry.kind {
                BundleKind::Script => scripts += 1,
                BundleKind::Voice => voices += 1,
            }
        }
        for entry in manifest
            .identity_files
            .iter()
            .chain(manifest.resources.iter())
        {
            entry.validate()?;
            all.push(&entry.relative_path);
            size = size
                .checked_add(entry.size)
                .ok_or_else(|| invalid("size overflow"))?;
        }
        if scripts != 24 || voices != 35 || size != manifest.total_consumed_bytes {
            return Err(invalid("manifest totals mismatch"));
        }
        all.sort_unstable();
        if all.windows(2).any(|pair| pair[0] == pair[1]) {
            return Err(invalid("duplicate input path"));
        }
        if !manifest
            .bundles
            .windows(2)
            .all(|p| p[0].relative_path < p[1].relative_path)
            || !manifest
                .identity_files
                .windows(2)
                .all(|p| p[0].relative_path < p[1].relative_path)
            || !manifest
                .resources
                .windows(2)
                .all(|p| p[0].relative_path < p[1].relative_path)
        {
            return Err(invalid("input paths not sorted"));
        }
        Ok(manifest)
    }
    pub fn entries(&self) -> Vec<InputEntry> {
        let mut out: Vec<_> = self
            .bundles
            .iter()
            .map(|e| InputEntry {
                relative_path: e.relative_path.clone(),
                size: e.size,
                sha256: e.sha256.clone(),
            })
            .collect();
        out.extend(self.identity_files.clone());
        out.extend(self.resources.clone());
        out.sort_by(|a, b| a.relative_path.cmp(&b.relative_path));
        out
    }
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ManifestIdentity {
    pub app_identity: AppIdentity,
    pub unity_version: String,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SpeakerEvidence {
    pub id: String,
    pub evidence_count: u64,
    pub evidence_sha256: String,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SpeakerCounts {
    pub bundle_count: u64,
    pub candidate_count: u64,
    pub candidate_rejection_count: u64,
    pub command_count: u64,
    pub object_count: u64,
    pub parsed_reference_count: u64,
    pub reference_count: u64,
    pub speaker_evidence_count: u64,
    pub text_entry_count: u64,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RejectionReasons {
    pub invalid_voice_id: u64,
    pub non_lexical_transcription: u64,
    pub unsupported_asset_shape: u64,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ApprovedSpeakers {
    pub schema_version: String,
    pub approval_state: String,
    pub candidate_schema_version: u32,
    pub counts: SpeakerCounts,
    pub evidence_canonicalization: String,
    pub grammar_revision: String,
    pub input_manifest_identity: ManifestIdentity,
    pub input_manifest_sha256: String,
    pub rejection_reasons: RejectionReasons,
    pub speaker_ids: Vec<String>,
    pub speakers: Vec<SpeakerEvidence>,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ApprovalMigration {
    pub schema_version: String,
    pub former_candidate_sha256: String,
    pub former_manifest_sha256: String,
    pub former_full_input_fingerprint: String,
    pub former_speakers_sha256: String,
    pub portable_manifest_sha256: String,
    pub portable_speakers_sha256: String,
    pub speaker_evidence_sha256: String,
}
#[derive(Debug, Clone)]
pub struct ApprovalUnit {
    pub manifest: ApprovedManifest,
    pub speakers: ApprovedSpeakers,
    pub migration: ApprovalMigration,
}

pub fn load_approval(
    manifest_bytes: &[u8],
    speaker_bytes: &[u8],
    migration_bytes: &[u8],
) -> Result<ApprovalUnit, AppError> {
    let manifest = ApprovedManifest::parse(manifest_bytes)?;
    let speakers: ApprovedSpeakers = parse_json(speaker_bytes)?;
    let migration: ApprovalMigration = parse_json(migration_bytes)?;
    if speakers.schema_version != "manosaba-speakers-v1-portable"
        || speakers.approval_state != "approved"
        || speakers.candidate_schema_version != 1
        || speakers.evidence_canonicalization != "sorted-structural-records-json-ascii-compact-v1"
        || speakers.grammar_revision != "manosaba-speaker-grammar-v1"
    {
        return Err(invalid("speaker schema mismatch"));
    }
    if speakers.counts
        != (SpeakerCounts {
            bundle_count: 24,
            candidate_count: 34070,
            candidate_rejection_count: 3308,
            command_count: 112486,
            object_count: 440,
            parsed_reference_count: 30762,
            reference_count: 304789,
            speaker_evidence_count: 33557,
            text_entry_count: 34838,
        })
        || speakers.rejection_reasons
            != (RejectionReasons {
                invalid_voice_id: 403,
                non_lexical_transcription: 458,
                unsupported_asset_shape: 2447,
            })
    {
        return Err(invalid("reviewed structural evidence mismatch"));
    }
    if migration.schema_version != "manosaba-config-migration-v1"
        || migration.former_candidate_sha256 != FORMER_CANDIDATE_SHA256
        || migration.former_manifest_sha256 != FORMER_MANIFEST_SHA256
        || migration.former_full_input_fingerprint != FORMER_FULL_INPUT_FINGERPRINT
        || migration.former_speakers_sha256
            != "3279b5c7a207edc55e95a8911b34b3a25ed5d54ee6789c5ef4e008dd926c16d1"
    {
        return Err(invalid("migration provenance mismatch"));
    }
    if speakers.input_manifest_sha256 != sha256_bytes(manifest_bytes)
        || migration.portable_manifest_sha256 != sha256_bytes(manifest_bytes)
        || migration.portable_speakers_sha256 != sha256_bytes(speaker_bytes)
    {
        return Err(invalid("approval hash binding mismatch"));
    }
    if speakers.input_manifest_identity
        != (ManifestIdentity {
            app_identity: manifest.app_identity.clone(),
            unity_version: manifest.unity_version.clone(),
        })
    {
        return Err(invalid("approval identity mismatch"));
    }
    if speakers.speaker_ids.len() != 26
        || speakers.speakers.len() != 26
        || !speakers.speaker_ids.windows(2).all(|p| p[0] < p[1])
    {
        return Err(invalid("speaker ID set mismatch"));
    }
    let mut count = 0_u64;
    for (id, evidence) in speakers.speaker_ids.iter().zip(&speakers.speakers) {
        if id != &evidence.id
            || !text_ok(id)
            || !id.bytes().all(|b| b.is_ascii_alphanumeric())
            || evidence.evidence_count == 0
            || !valid_sha256(&evidence.evidence_sha256)
        {
            return Err(invalid("invalid speaker evidence"));
        }
        count = count
            .checked_add(evidence.evidence_count)
            .ok_or_else(|| invalid("speaker count overflow"))?;
    }
    if speakers.counts.speaker_evidence_count != count {
        return Err(invalid("speaker evidence total mismatch"));
    }
    let evidence_value = serde_json::to_value(&speakers.speakers)
        .map_err(|e| invalid(format!("speaker evidence: {e}")))?;
    let evidence_bytes = serde_json::to_vec(&evidence_value)
        .map_err(|e| invalid(format!("speaker evidence: {e}")))?;
    if sha256_bytes(&evidence_bytes) != migration.speaker_evidence_sha256
        || migration.speaker_evidence_sha256 != EXPECTED_SPEAKER_EVIDENCE_SHA256
    {
        return Err(invalid("speaker evidence digest mismatch"));
    }
    Ok(ApprovalUnit {
        manifest,
        speakers,
        migration,
    })
}

pub fn checked_in_approval() -> Result<ApprovalUnit, AppError> {
    load_approval(
        include_bytes!("../config/manosaba-inputs-v1.json"),
        include_bytes!("../config/manosaba-speakers-v1.json"),
        include_bytes!("../config/manosaba-config-migration-v1.json"),
    )
}
