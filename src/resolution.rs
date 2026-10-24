//! Bundle-scheduled resolution of preliminary exact-ID matches.
use crate::{
    audio::{AudioErrorKind, CanonicalWavReceipt, CanonicalWavSink},
    error::{AppError, UnityErrorKind},
    inventory::VerifiedBundleInputs,
    matching::{MatchPlan, audio_candidate_id},
    model::{
        AudioAssetRef, AudioLimits, AudioProvenance, CandidateKind, CandidateProvenance,
        CandidateRejection, CodecKind, DecodedAudioReceipt, MetadataCompleteness,
        PredecodeFrameCount, ScriptReference,
    },
    unity::{BundleSession, UnityBackend},
};
use std::{
    collections::{BTreeMap, BTreeSet},
    fs::{self, OpenOptions},
    io,
    path::{Path, PathBuf},
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TerminalKind {
    AcceptedScript,
    CanonicalAudio,
    DuplicateAlias,
    Rejected,
}

#[derive(Debug, Clone)]
pub struct TerminalSource {
    pub candidate_id: String,
    pub group_id: Option<String>,
    pub kind: TerminalKind,
    pub canonical_audio_id: Option<String>,
}

#[derive(Debug, Clone)]
pub struct AudioProperties {
    pub wav_bytes: u64,
    pub wav_sha256: String,
    pub channels: u16,
    pub sample_rate: u32,
    pub bits_per_sample: u16,
    pub frame_count: u64,
}

#[derive(Debug, Clone)]
pub struct DuplicateAlias {
    pub candidate_id: String,
    pub voice_id: String,
    pub canonical_audio_id: String,
    pub provenance: AudioProvenance,
}

#[derive(Debug, Clone)]
pub struct ResolvedRow {
    pub voice_ids: Vec<String>,
    pub transcription: String,
    pub speaker: String,
    pub source_path: String,
    pub canonical_audio_id: String,
    pub staged_wav: PathBuf,
    pub audio: AudioProperties,
    pub script_references: Vec<ScriptReference>,
}

#[derive(Debug, Clone)]
pub struct BundleHealth {
    pub bundle_path: String,
    pub eligible_attempts: usize,
    pub successful_decodes: usize,
    pub candidate_failures: usize,
}

#[derive(Debug, Clone)]
pub struct ResolvedDataset {
    pub rows: Vec<ResolvedRow>,
    pub duplicate_aliases: Vec<DuplicateAlias>,
    pub candidate_rejections: Vec<CandidateRejection>,
    pub terminal_sources: Vec<TerminalSource>,
    pub bundle_health: Vec<BundleHealth>,
    pub exact_staged_wav_bytes: u64,
    pub peak_live_buffer_bytes: usize,
}

struct Work {
    voice_id: String,
    id: String,
    reference: AudioAssetRef,
    expected_bytes: Option<u64>,
    measurement: Option<AudioProperties>,
    staged: Option<Staged>,
    failed: bool,
}
struct Staged {
    path: PathBuf,
    audio: AudioProperties,
}
struct OwnedFiles {
    paths: BTreeSet<PathBuf>,
}
impl OwnedFiles {
    fn new() -> Self {
        Self {
            paths: BTreeSet::new(),
        }
    }
    fn track(&mut self, path: PathBuf) {
        self.paths.insert(path);
    }
    fn remove(&mut self, path: &Path) -> Result<(), AppError> {
        fs::remove_file(path).map_err(|e| fatal(format!("remove owned staged WAV: {e}")))?;
        self.paths.remove(path);
        Ok(())
    }
    fn retain(&mut self, kept: &BTreeSet<PathBuf>) -> Result<(), AppError> {
        for path in self.paths.clone().difference(kept) {
            self.remove(path)?;
        }
        self.paths.clear(); // ownership of successful paths passes to the caller
        Ok(())
    }
    fn cleanup(&mut self) -> Result<(), AppError> {
        for path in self.paths.clone() {
            self.remove(&path)?;
        }
        Ok(())
    }
}
impl Drop for OwnedFiles {
    fn drop(&mut self) {
        for path in &self.paths {
            let _ = fs::remove_file(path);
        }
    }
}

fn fatal(message: impl Into<String>) -> AppError {
    AppError::processing(message)
}
fn local(message: impl Into<String>) -> AppError {
    AppError::unity(UnityErrorKind::CandidateDecode, message)
}
fn is_local(error: &AppError) -> bool {
    matches!(
        error.unity_kind(),
        Some(UnityErrorKind::CandidateDecode | UnityErrorKind::UnsupportedLayoutOrCodec)
    )
}
fn provenance(a: &AudioAssetRef) -> AudioProvenance {
    AudioProvenance {
        bundle_path: a.bundle_path.clone(),
        file_index: a.file_index,
        object_index: a.object_index,
        path_id: a.path_id,
        container_path: a.container.clone().unwrap_or_default(),
    }
}
fn provenance_key(a: &AudioAssetRef) -> Vec<u8> {
    format!(
        "{}\0{}\0{}\0{}\0{}",
        a.bundle_path,
        a.file_index,
        a.object_index,
        a.path_id,
        a.container.as_deref().unwrap_or("")
    )
    .into_bytes()
}
fn properties(wav: &CanonicalWavReceipt) -> AudioProperties {
    AudioProperties {
        wav_bytes: wav.wav_bytes,
        wav_sha256: wav.wav_sha256.clone(),
        channels: wav.channels,
        sample_rate: wav.sample_rate,
        bits_per_sample: wav.bits_per_sample,
        frame_count: wav.frame_count,
    }
}
fn checked_wav_size(a: &AudioAssetRef, limits: &AudioLimits) -> Result<Option<u64>, AppError> {
    if a.metadata_completeness != MetadataCompleteness::CompleteFromValidatedHeader
        || a.signature.codec != CodecKind::Fsb5Vorbis
    {
        return Ok(None);
    }
    let PredecodeFrameCount::ValidatedPayloadHeader(frames) = a.predecode_frames else {
        return Ok(None);
    };
    // The pinned single-subsound FSB5 Vorbis path emits signed PCM16.
    if a.metadata.bits_per_sample != 16 || a.metadata.channels == 0 {
        return Ok(None);
    }
    let data = frames
        .checked_mul(u64::from(a.metadata.channels))
        .and_then(|n| n.checked_mul(2))
        .ok_or_else(|| fatal("WAV size overflow"))?;
    let total = 44_u64
        .checked_add(data)
        .and_then(|n| n.checked_add(data & 1))
        .ok_or_else(|| fatal("WAV size overflow"))?;
    if total > limits.maximum_wav_bytes || total > 256 * 1024 * 1024 {
        return Err(fatal("canonical WAV exceeds configured or hard limit"));
    }
    Ok(Some(total))
}
fn decode<S: BundleSession>(
    session: &mut S,
    reference: &AudioAssetRef,
    output: &mut dyn io::Write,
    limits: &AudioLimits,
) -> Result<(AudioProperties, usize), AppError> {
    let mut sink = CanonicalWavSink::new(output, limits.maximum_wav_bytes);
    let decoded = session.stream_audio(reference, &mut sink, limits);
    let failed_sink = sink.failure_kind();
    if let Some(kind) = failed_sink {
        return Err(match kind {
            AudioErrorKind::Malformed => local("decoder emitted malformed WAV"),
            AudioErrorKind::Limit => fatal("canonical WAV exceeded limit"),
            AudioErrorKind::Io => fatal("staged WAV write failed"),
        });
    }
    let decoded: DecodedAudioReceipt = decoded?;
    let wav = sink.finish().map_err(|error| match error.kind() {
        AudioErrorKind::Malformed => local(error.to_string()),
        AudioErrorKind::Limit | AudioErrorKind::Io => fatal(error.to_string()),
    })?;
    if wav.wav_bytes != decoded.wav_bytes
        || wav.wav_sha256 != decoded.wav_sha256
        || wav.channels != decoded.channels
        || wav.sample_rate != decoded.sample_rate
        || wav.bits_per_sample != decoded.bits_per_sample
        || wav.frame_count != decoded.frame_count
    {
        return Err(fatal(
            "decoder receipt and canonical WAV properties disagree",
        ));
    }
    if reference.predecode_frames != PredecodeFrameCount::Unavailable
        && !matches!(reference.predecode_frames, PredecodeFrameCount::ValidatedPayloadHeader(n) if n == wav.frame_count)
    {
        return Err(local("payload and canonical WAV frame counts disagree"));
    }
    Ok((
        properties(&wav),
        decoded
            .peak_live_buffer_bytes
            .max(wav.peak_live_buffer_bytes),
    ))
}
fn reject(
    id: &str,
    voice: &str,
    a: &AudioAssetRef,
    stage: &'static str,
    reason: &'static str,
) -> CandidateRejection {
    CandidateRejection {
        candidate_id: id.into(),
        candidate_kind: CandidateKind::AudioAsset,
        group_id: Some(voice.into()),
        terminal_stage: stage,
        reason,
        provenance: CandidateProvenance::Audio(provenance(a)),
        blocking_group_ids: vec![voice.into()],
    }
}
fn reject_script(
    s: &ScriptReference,
    voice: &str,
    stage: &'static str,
    reason: &'static str,
) -> CandidateRejection {
    CandidateRejection {
        candidate_id: s.parsed.candidate_id.clone(),
        candidate_kind: CandidateKind::ScriptReference,
        group_id: Some(voice.into()),
        terminal_stage: stage,
        reason,
        provenance: CandidateProvenance::Script(s.parsed.provenance.clone()),
        blocking_group_ids: vec![voice.into()],
    }
}

/// Caller supplies an already-approved map and an existing private staging directory.
/// The callback receives bytes already staged and bytes about to be written.
/// All files created here are removed on error.
pub fn resolve<B, D>(
    plan: &MatchPlan,
    backend: &B,
    approved: &BTreeMap<String, VerifiedBundleInputs>,
    staging: &Path,
    limits: &AudioLimits,
    mut check_disk: D,
) -> Result<ResolvedDataset, AppError>
where
    B: UnityBackend,
    D: FnMut(u64, u64) -> Result<(), AppError>,
{
    if !staging.is_dir() {
        return Err(fatal("staging directory is missing"));
    }
    let mut owned = OwnedFiles::new();
    let result = (|| {
        let mut work = Vec::new();
        for group in &plan.pending_groups {
            for a in &group.audio_refs {
                if !approved.contains_key(&a.bundle_path) {
                    return Err(fatal(format!(
                        "approved voice bundle missing: {}",
                        a.bundle_path
                    )));
                }
                work.push(Work {
                    voice_id: group.voice_id.clone(),
                    id: audio_candidate_id(a),
                    reference: a.clone(),
                    expected_bytes: checked_wav_size(a, limits)?,
                    measurement: None,
                    staged: None,
                    failed: false,
                });
            }
        }
        work.sort_by(|x, y| {
            x.reference
                .bundle_path
                .as_bytes()
                .cmp(y.reference.bundle_path.as_bytes())
                .then_with(|| x.voice_id.as_bytes().cmp(y.voice_id.as_bytes()))
                .then_with(|| provenance_key(&x.reference).cmp(&provenance_key(&y.reference)))
        });
        let mut health: BTreeMap<String, BundleHealth> = approved
            .keys()
            .map(|name| {
                (
                    name.clone(),
                    BundleHealth {
                        bundle_path: name.clone(),
                        eligible_attempts: 0,
                        successful_decodes: 0,
                        candidate_failures: 0,
                    },
                )
            })
            .collect();
        for w in &work {
            health
                .get_mut(&w.reference.bundle_path)
                .unwrap()
                .eligible_attempts += 1;
        }
        let mut peak = 0usize;
        // One measurement session per bundle, only where exact canonical size is unknown.
        let mut i = 0;
        while i < work.len() {
            let bundle = work[i].reference.bundle_path.clone();
            let end = i + work[i..]
                .iter()
                .take_while(|w| w.reference.bundle_path == bundle)
                .count();
            if work[i..end].iter().any(|w| w.expected_bytes.is_none()) {
                check_disk(0, 0)?;
                let mut session = backend.open_bundle(&approved[&bundle])?;
                for w in &mut work[i..end] {
                    if w.expected_bytes.is_some() {
                        continue;
                    }
                    match decode(&mut session, &w.reference, &mut io::sink(), limits) {
                        Ok((props, live)) => {
                            w.expected_bytes = Some(props.wav_bytes);
                            w.measurement = Some(props);
                            peak = peak.max(live);
                        }
                        Err(error) if is_local(&error) => {
                            w.failed = true;
                        }
                        Err(error) => return Err(error),
                    }
                }
            }
            i = end;
        }
        let mut cumulative = 0_u64;
        i = 0;
        while i < work.len() {
            let bundle = work[i].reference.bundle_path.clone();
            let end = i + work[i..]
                .iter()
                .take_while(|w| w.reference.bundle_path == bundle)
                .count();
            if work[i..end].iter().any(|w| !w.failed) {
                check_disk(cumulative, 0)?;
                let mut session = backend.open_bundle(&approved[&bundle])?;
                for w in &mut work[i..end] {
                    if w.failed {
                        continue;
                    }
                    let expected = w.expected_bytes.expect("measured or header sized");
                    let reserved = cumulative
                        .checked_add(expected)
                        .ok_or_else(|| fatal("staged WAV total overflow"))?;
                    check_disk(cumulative, expected)?;
                    let path = staging.join(format!("{}.wav", w.id));
                    let mut file = OpenOptions::new()
                        .write(true)
                        .create_new(true)
                        .open(&path)
                        .map_err(|e| fatal(format!("create staged WAV: {e}")))?;
                    owned.track(path.clone());
                    let result = decode(&mut session, &w.reference, &mut file, limits);
                    match result {
                        Ok((props, live)) => {
                            file.sync_all()
                                .map_err(|e| fatal(format!("sync staged WAV: {e}")))?;
                            if props.wav_bytes != expected
                                || w.measurement.as_ref().is_some_and(|m| {
                                    m.wav_sha256 != props.wav_sha256
                                        || m.frame_count != props.frame_count
                                })
                            {
                                return Err(fatal("measurement and staging WAV disagree"));
                            }
                            cumulative = reserved;
                            peak = peak.max(live);
                            w.staged = Some(Staged { path, audio: props });
                            health.get_mut(&bundle).unwrap().successful_decodes += 1;
                        }
                        Err(error) if is_local(&error) => {
                            drop(file);
                            owned.remove(&path)?;
                            w.failed = true;
                        }
                        Err(error) => return Err(error),
                    }
                }
            }
            i = end;
        }
        for b in health.values_mut() {
            b.candidate_failures = b.eligible_attempts - b.successful_decodes;
            if b.eligible_attempts > 0 && b.successful_decodes == 0 {
                return Err(fatal(format!(
                    "all eligible audio decodes failed in bundle {}",
                    b.bundle_path
                )));
            }
        }

        let mut by_id: BTreeMap<String, Vec<&Work>> = BTreeMap::new();
        for w in &work {
            by_id.entry(w.voice_id.clone()).or_default().push(w);
        }
        let mut rejected_groups: BTreeMap<String, (&'static str, &'static str)> = BTreeMap::new();
        for group in &plan.pending_groups {
            let valid = &by_id[&group.voice_id];
            let hashes: BTreeSet<_> = valid
                .iter()
                .filter_map(|w| w.staged.as_ref().map(|s| &s.audio.wav_sha256))
                .collect();
            if hashes.is_empty() {
                rejected_groups.insert(
                    group.voice_id.clone(),
                    ("audio_decode", "all_audio_candidates_failed"),
                );
            } else if hashes.len() > 1 {
                rejected_groups.insert(
                    group.voice_id.clone(),
                    ("same_id_audio_conflict", "different_audio_hashes"),
                );
            }
        }
        let mut by_hash: BTreeMap<String, Vec<&Work>> = BTreeMap::new();
        for w in &work {
            if !rejected_groups.contains_key(&w.voice_id)
                && let Some(s) = &w.staged
            {
                by_hash
                    .entry(s.audio.wav_sha256.clone())
                    .or_default()
                    .push(w);
            }
        }
        for candidates in by_hash.values() {
            let voices: BTreeSet<_> = candidates.iter().map(|w| &w.voice_id).collect();
            if voices.len() < 2 {
                continue;
            }
            let texts: BTreeSet<_> = plan
                .pending_groups
                .iter()
                .filter(|g| voices.contains(&g.voice_id))
                .map(|g| g.script_references[0].parsed.transcription.text.as_str())
                .collect();
            let speakers: BTreeSet<_> = plan
                .pending_groups
                .iter()
                .filter(|g| voices.contains(&g.voice_id))
                .map(|g| {
                    g.script_references[0]
                        .speaker_resolution
                        .speaker
                        .as_deref()
                        .unwrap()
                })
                .collect();
            if texts.len() != 1 || speakers.len() != 1 {
                for voice in voices {
                    rejected_groups.insert(
                        voice.clone(),
                        (
                            "cross_id_duplicate_conflict",
                            "identical_audio_text_or_speaker_conflict",
                        ),
                    );
                }
            }
        }
        // Rebuild because a cross-ID conflict can remove a complete hash cohort.
        by_hash.clear();
        for w in &work {
            if !rejected_groups.contains_key(&w.voice_id)
                && let Some(s) = &w.staged
            {
                by_hash
                    .entry(s.audio.wav_sha256.clone())
                    .or_default()
                    .push(w);
            }
        }
        let mut rejections = plan.candidate_rejections.clone();
        let mut terminal: BTreeMap<String, TerminalSource> = BTreeMap::new();
        for r in &rejections {
            terminal.insert(
                r.candidate_id.clone(),
                TerminalSource {
                    candidate_id: r.candidate_id.clone(),
                    group_id: r.group_id.clone(),
                    kind: TerminalKind::Rejected,
                    canonical_audio_id: None,
                },
            );
        }
        for w in &work {
            if w.failed {
                rejections.push(reject(
                    &w.id,
                    &w.voice_id,
                    &w.reference,
                    "audio_decode",
                    "candidate_decode_failed",
                ));
                terminal.insert(
                    w.id.clone(),
                    TerminalSource {
                        candidate_id: w.id.clone(),
                        group_id: Some(w.voice_id.clone()),
                        kind: TerminalKind::Rejected,
                        canonical_audio_id: None,
                    },
                );
            } else if let Some(&(stage, reason)) = rejected_groups.get(&w.voice_id) {
                rejections.push(reject(&w.id, &w.voice_id, &w.reference, stage, reason));
                terminal.insert(
                    w.id.clone(),
                    TerminalSource {
                        candidate_id: w.id.clone(),
                        group_id: Some(w.voice_id.clone()),
                        kind: TerminalKind::Rejected,
                        canonical_audio_id: None,
                    },
                );
            }
        }
        for group in &plan.pending_groups {
            if let Some(&(stage, reason)) = rejected_groups.get(&group.voice_id) {
                for s in &group.script_references {
                    rejections.push(reject_script(s, &group.voice_id, stage, reason));
                    terminal.insert(
                        s.parsed.candidate_id.clone(),
                        TerminalSource {
                            candidate_id: s.parsed.candidate_id.clone(),
                            group_id: Some(group.voice_id.clone()),
                            kind: TerminalKind::Rejected,
                            canonical_audio_id: None,
                        },
                    );
                }
            } else {
                for s in &group.script_references {
                    terminal.insert(
                        s.parsed.candidate_id.clone(),
                        TerminalSource {
                            candidate_id: s.parsed.candidate_id.clone(),
                            group_id: Some(group.voice_id.clone()),
                            kind: TerminalKind::AcceptedScript,
                            canonical_audio_id: None,
                        },
                    );
                }
            }
        }
        let mut rows = Vec::new();
        let mut aliases = Vec::new();
        let mut kept = BTreeSet::new();
        for candidates in by_hash.values_mut() {
            candidates.sort_by_key(|w| (provenance_key(&w.reference), w.id.clone()));
            let canonical = candidates[0];
            let staged = canonical.staged.as_ref().unwrap();
            kept.insert(staged.path.clone());
            let voices: BTreeSet<_> = candidates.iter().map(|w| w.voice_id.clone()).collect();
            let mut scripts = Vec::new();
            for g in &plan.pending_groups {
                if voices.contains(&g.voice_id) {
                    scripts.extend(g.script_references.clone());
                }
            }
            for (index, w) in candidates.iter().enumerate() {
                let kind = if index == 0 {
                    TerminalKind::CanonicalAudio
                } else {
                    TerminalKind::DuplicateAlias
                };
                terminal.insert(
                    w.id.clone(),
                    TerminalSource {
                        candidate_id: w.id.clone(),
                        group_id: Some(w.voice_id.clone()),
                        kind,
                        canonical_audio_id: Some(canonical.id.clone()),
                    },
                );
                if index != 0 {
                    aliases.push(DuplicateAlias {
                        candidate_id: w.id.clone(),
                        voice_id: w.voice_id.clone(),
                        canonical_audio_id: canonical.id.clone(),
                        provenance: provenance(&w.reference),
                    });
                }
            }
            rows.push(ResolvedRow {
                voice_ids: voices.into_iter().collect(),
                transcription: scripts[0].parsed.transcription.text.clone(),
                speaker: scripts[0].speaker_resolution.speaker.clone().unwrap(),
                source_path: canonical.reference.bundle_path.clone(),
                canonical_audio_id: canonical.id.clone(),
                staged_wav: staged.path.clone(),
                audio: staged.audio.clone(),
                script_references: scripts,
            });
        }
        rows.sort_by(|a, b| {
            a.voice_ids[0]
                .as_bytes()
                .cmp(b.voice_ids[0].as_bytes())
                .then_with(|| a.canonical_audio_id.cmp(&b.canonical_audio_id))
        });
        aliases.sort_by(|a, b| a.candidate_id.cmp(&b.candidate_id));
        rejections.sort_by(|a, b| a.candidate_id.cmp(&b.candidate_id));
        if terminal.len() != plan.accounting.source_total
            || terminal
                .values()
                .filter(|s| s.kind == TerminalKind::Rejected)
                .count()
                != rejections.len()
            || terminal
                .values()
                .filter(|s| s.kind != TerminalKind::Rejected)
                .count()
                + rejections.len()
                != plan.accounting.source_total
        {
            return Err(fatal("terminal source accounting mismatch"));
        }
        let exact_staged_wav_bytes = rows
            .iter()
            .try_fold(0_u64, |sum, row| sum.checked_add(row.audio.wav_bytes))
            .ok_or_else(|| fatal("retained WAV size overflow"))?;
        owned.retain(&kept)?;
        Ok(ResolvedDataset {
            rows,
            duplicate_aliases: aliases,
            candidate_rejections: rejections,
            terminal_sources: terminal.into_values().collect(),
            bundle_health: health.into_values().collect(),
            exact_staged_wav_bytes,
            peak_live_buffer_bytes: peak,
        })
    })();
    match result {
        Ok(dataset) => Ok(dataset),
        Err(error) => {
            if let Err(cleanup) = owned.cleanup() {
                Err(fatal(format!("{error}; cleanup failed: {cleanup}")))
            } else {
                Err(error)
            }
        }
    }
}
