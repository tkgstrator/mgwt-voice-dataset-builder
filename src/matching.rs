//! Pure preliminary exact-ID join. Audio stays undecoded until resolution.
use crate::{
    error::AppError,
    hash::sha256_bytes,
    model::{
        AudioAssetRef, AudioProvenance, CandidateProvenance, CandidateRejection, ScriptProvenance,
        ScriptReference,
    },
    normalize::normalize_voice_id,
};
use std::collections::{BTreeMap, BTreeSet};

const STAGES: &[&str] = &[
    "asset_shape",
    "voice_id",
    "transcription",
    "speaker",
    "script_group_conflict",
    "exact_join",
    "audio_decode",
    "same_id_audio_conflict",
    "cross_id_duplicate_conflict",
];
type Decision = (usize, usize, Vec<u8>, String, &'static str, &'static str);

pub use crate::model::CandidateKind;
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AuditState {
    Pending,
    PreliminaryRejected,
}
#[derive(Debug, Clone)]
pub struct PendingMatchGroup {
    pub voice_id: String,
    pub script_references: Vec<ScriptReference>,
    pub audio_refs: Vec<AudioAssetRef>,
}
#[derive(Debug, Clone)]
pub struct MatchAuditRecord {
    pub candidate_id: String,
    pub candidate_kind: CandidateKind,
    pub group_id: Option<String>,
    pub state: AuditState,
    pub reason: Option<&'static str>,
    pub provenance: CandidateProvenance,
}
#[derive(Debug, Clone)]
pub struct MatchGroupExplanation {
    pub voice_id: String,
    pub terminal_stage: &'static str,
    pub reason: &'static str,
    pub script_candidate_ids: Vec<String>,
    pub audio_candidate_ids: Vec<String>,
}
#[derive(Debug, Clone, Default)]
pub struct PreliminaryAccounting {
    pub source_total: usize,
    pub pending_total: usize,
    pub rejected_total: usize,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SourceTotals {
    pub script_references: usize,
    pub audio_assets: usize,
    pub prior_rejections: usize,
}
#[derive(Debug, Clone)]
pub struct MatchPlan {
    pub pending_groups: Vec<PendingMatchGroup>,
    pub audit_records: Vec<MatchAuditRecord>,
    pub candidate_rejections: Vec<CandidateRejection>,
    pub group_explanations: Vec<MatchGroupExplanation>,
    pub pending_script_candidate_ids: Vec<String>,
    pub pending_audio_candidate_ids: Vec<String>,
    pub accounting: PreliminaryAccounting,
}

fn invalid(message: impl Into<String>) -> AppError {
    AppError::processing(message)
}
fn script_provenance_key(p: &ScriptProvenance) -> Vec<u8> {
    format!(
        "{}\0{}\0{}\0{}\0{}\0{}",
        p.bundle_path, p.file_index, p.object_index, p.path_id, p.container_path, p.command_index
    )
    .into_bytes()
}
fn provenance_key(p: &CandidateProvenance) -> Vec<u8> {
    match p {
        CandidateProvenance::Script(p) => script_provenance_key(p),
        CandidateProvenance::Audio(p) => format!(
            "{}\0{}\0{}\0{}\0{}",
            p.bundle_path, p.file_index, p.object_index, p.path_id, p.container_path
        )
        .into_bytes(),
    }
}
fn audio_provenance(a: &AudioAssetRef) -> CandidateProvenance {
    CandidateProvenance::Audio(AudioProvenance {
        bundle_path: a.bundle_path.clone(),
        file_index: a.file_index,
        object_index: a.object_index,
        path_id: a.path_id,
        container_path: a.container.clone().unwrap_or_default(),
    })
}
/// Matches the former Python identity: bundle, path ID and container.
pub fn audio_candidate_id(a: &AudioAssetRef) -> String {
    sha256_bytes(
        format!(
            "{}\0{}\0{}",
            a.bundle_path,
            a.path_id,
            a.container.as_deref().unwrap_or("")
        )
        .as_bytes(),
    )
}
fn stage_rank(stage: &str) -> Option<usize> {
    STAGES.iter().position(|s| *s == stage)
}
fn reject(
    id: String,
    group_id: Option<String>,
    stage: &'static str,
    reason: &'static str,
    provenance: CandidateProvenance,
) -> CandidateRejection {
    let kind = match &provenance {
        CandidateProvenance::Script(_) => CandidateKind::ScriptReference,
        CandidateProvenance::Audio(_) => CandidateKind::AudioAsset,
    };
    CandidateRejection {
        candidate_id: id,
        candidate_kind: kind,
        blocking_group_ids: group_id.iter().cloned().collect(),
        group_id,
        terminal_stage: stage,
        reason,
        provenance,
    }
}
fn audit(
    id: String,
    kind: CandidateKind,
    group_id: Option<String>,
    decision: Option<(&'static str, &'static str)>,
    provenance: CandidateProvenance,
) -> MatchAuditRecord {
    MatchAuditRecord {
        candidate_id: id,
        candidate_kind: kind,
        group_id,
        state: if decision.is_some() {
            AuditState::PreliminaryRejected
        } else {
            AuditState::Pending
        },
        reason: decision.map(|(_, reason)| reason),
        provenance,
    }
}

/// Every source receives one preliminary audit and either one rejection or one pending identity.
pub fn match_candidates(
    scripts: &[ScriptReference],
    audio: &[AudioAssetRef],
    prior_rejections: &[CandidateRejection],
    expected: SourceTotals,
) -> Result<MatchPlan, AppError> {
    if expected
        != (SourceTotals {
            script_references: scripts.len(),
            audio_assets: audio.len(),
            prior_rejections: prior_rejections.len(),
        })
    {
        return Err(invalid("orchestration source totals mismatch"));
    }
    let mut by_script: BTreeMap<String, Vec<&ScriptReference>> = BTreeMap::new();
    let mut by_audio: BTreeMap<String, Vec<&AudioAssetRef>> = BTreeMap::new();
    let mut by_prior: BTreeMap<String, Vec<&CandidateRejection>> = BTreeMap::new();
    let mut seen = BTreeSet::new();
    let mut audits = Vec::new();
    let mut rejections = Vec::new();
    let mut pending = Vec::new();
    let mut explanations = Vec::new();
    for s in scripts {
        if !seen.insert(s.parsed.candidate_id.clone()) {
            return Err(invalid("duplicate source candidate identity"));
        }
        if normalize_voice_id(&s.parsed.voice_id).ok().as_deref() != Some(&s.parsed.voice_id) {
            return Err(invalid("resolved script has invalid normalized voice ID"));
        }
        by_script
            .entry(s.parsed.voice_id.clone())
            .or_default()
            .push(s);
    }
    for r in prior_rejections {
        if !seen.insert(r.candidate_id.clone()) {
            return Err(invalid("duplicate source candidate identity"));
        }
        if stage_rank(r.terminal_stage).is_none() {
            return Err(invalid("invalid terminal stage"));
        }
        if !matches!(
            (&r.candidate_kind, &r.provenance),
            (
                CandidateKind::ScriptReference,
                CandidateProvenance::Script(_)
            ) | (CandidateKind::AudioAsset, CandidateProvenance::Audio(_))
        ) {
            return Err(invalid("source kind and provenance disagree"));
        }
        let mut blockers = r.blocking_group_ids.clone();
        blockers.sort();
        blockers.dedup();
        if blockers != r.blocking_group_ids
            || (!blockers.is_empty()
                && r.group_id.as_deref()
                    != (if blockers.len() == 1 {
                        Some(blockers[0].as_str())
                    } else {
                        None
                    }))
        {
            return Err(invalid("invalid rejection blocking group IDs"));
        }
        if blockers.is_empty()
            && let Some(group) = &r.group_id
        {
            blockers.push(group.clone());
        }
        for group in blockers {
            if normalize_voice_id(&group).ok().as_deref() != Some(group.as_str()) {
                return Err(invalid("invalid blocking voice ID"));
            }
            by_prior.entry(group).or_default().push(r);
        }
        audits.push(audit(
            r.candidate_id.clone(),
            r.candidate_kind,
            r.group_id.clone(),
            Some((r.terminal_stage, r.reason)),
            r.provenance.clone(),
        ));
        rejections.push(r.clone());
    }
    for a in audio {
        let id = audio_candidate_id(a);
        if !seen.insert(id.clone()) {
            return Err(invalid("duplicate source candidate identity"));
        }
        if let Ok(voice_id) = normalize_voice_id(&a.name) {
            by_audio.entry(voice_id).or_default().push(a);
        } else {
            let p = audio_provenance(a);
            audits.push(audit(
                id.clone(),
                CandidateKind::AudioAsset,
                None,
                Some(("voice_id", "invalid_voice_id")),
                p.clone(),
            ));
            rejections.push(reject(id, None, "voice_id", "invalid_voice_id", p));
        }
    }
    let voices: BTreeSet<String> = by_script
        .keys()
        .chain(by_audio.keys())
        .chain(by_prior.keys())
        .cloned()
        .collect();
    for voice in voices {
        let mut ss = by_script.remove(&voice).unwrap_or_default();
        let mut aa = by_audio.remove(&voice).unwrap_or_default();
        let mut rr = by_prior.remove(&voice).unwrap_or_default();
        ss.sort_by_key(|s| {
            (
                script_provenance_key(&s.parsed.provenance),
                s.parsed.candidate_id.clone(),
            )
        });
        aa.sort_by_key(|a| {
            (
                a.container.clone().unwrap_or_default().into_bytes(),
                provenance_key(&audio_provenance(a)),
                audio_candidate_id(a),
            )
        });
        rr.sort_by_key(|r| (provenance_key(&r.provenance), r.candidate_id.clone()));
        // Fixed stage order; at the same stage the first provenance wins.
        let mut decisions: Vec<Decision> = rr
            .iter()
            .map(|r| {
                (
                    stage_rank(r.terminal_stage).unwrap(),
                    0,
                    provenance_key(&r.provenance),
                    r.candidate_id.clone(),
                    r.terminal_stage,
                    r.reason,
                )
            })
            .collect();
        if ss
            .iter()
            .map(|s| s.parsed.transcription.text.as_str())
            .collect::<BTreeSet<_>>()
            .len()
            > 1
        {
            decisions.push((
                stage_rank("script_group_conflict").unwrap(),
                1,
                vec![],
                String::new(),
                "script_group_conflict",
                "transcription_conflict",
            ));
        }
        if ss.iter().any(|s| s.speaker_resolution.speaker.is_none())
            || ss
                .iter()
                .filter_map(|s| s.speaker_resolution.speaker.as_deref())
                .collect::<BTreeSet<_>>()
                .len()
                > 1
        {
            decisions.push((
                stage_rank("script_group_conflict").unwrap(),
                2,
                vec![],
                String::new(),
                "script_group_conflict",
                "speaker_conflict",
            ));
        }
        if aa.is_empty() && !ss.is_empty() {
            decisions.push((
                stage_rank("exact_join").unwrap(),
                1,
                vec![],
                String::new(),
                "exact_join",
                "missing_audio",
            ));
        } else if ss.is_empty() && !aa.is_empty() {
            decisions.push((
                stage_rank("exact_join").unwrap(),
                1,
                vec![],
                String::new(),
                "exact_join",
                "missing_script",
            ));
        }
        let decision = decisions.into_iter().min();
        let terminal = decision.as_ref().map(|d| (d.4, d.5));
        if let Some((stage, reason)) = terminal {
            let mut script_sources: Vec<(Vec<u8>, String)> = ss
                .iter()
                .map(|s| {
                    (
                        script_provenance_key(&s.parsed.provenance),
                        s.parsed.candidate_id.clone(),
                    )
                })
                .chain(
                    rr.iter()
                        .filter(|r| r.candidate_kind == CandidateKind::ScriptReference)
                        .map(|r| (provenance_key(&r.provenance), r.candidate_id.clone())),
                )
                .collect();
            script_sources.sort();
            let mut audio_sources: Vec<(Vec<u8>, String)> = aa
                .iter()
                .map(|a| (provenance_key(&audio_provenance(a)), audio_candidate_id(a)))
                .chain(
                    rr.iter()
                        .filter(|r| r.candidate_kind == CandidateKind::AudioAsset)
                        .map(|r| (provenance_key(&r.provenance), r.candidate_id.clone())),
                )
                .collect();
            audio_sources.sort();
            explanations.push(MatchGroupExplanation {
                voice_id: voice.clone(),
                terminal_stage: stage,
                reason,
                script_candidate_ids: script_sources.into_iter().map(|(_, id)| id).collect(),
                audio_candidate_ids: audio_sources.into_iter().map(|(_, id)| id).collect(),
            });
        } else {
            pending.push(PendingMatchGroup {
                voice_id: voice.clone(),
                script_references: ss.iter().map(|s| (*s).clone()).collect(),
                audio_refs: aa.iter().map(|a| (*a).clone()).collect(),
            });
        }
        for s in ss {
            let p = CandidateProvenance::Script(s.parsed.provenance.clone());
            let id = s.parsed.candidate_id.clone();
            audits.push(audit(
                id.clone(),
                CandidateKind::ScriptReference,
                Some(voice.clone()),
                terminal,
                p.clone(),
            ));
            if let Some((stage, reason)) = terminal {
                rejections.push(reject(id, Some(voice.clone()), stage, reason, p));
            }
        }
        for a in aa {
            let p = audio_provenance(a);
            let id = audio_candidate_id(a);
            audits.push(audit(
                id.clone(),
                CandidateKind::AudioAsset,
                Some(voice.clone()),
                terminal,
                p.clone(),
            ));
            if let Some((stage, reason)) = terminal {
                rejections.push(reject(id, Some(voice.clone()), stage, reason, p));
            }
        }
    }
    audits.sort_by_key(|a| (provenance_key(&a.provenance), a.candidate_id.clone()));
    rejections.sort_by_key(|r| (provenance_key(&r.provenance), r.candidate_id.clone()));
    let pending_script_candidate_ids = audits
        .iter()
        .filter(|a| {
            a.state == AuditState::Pending && a.candidate_kind == CandidateKind::ScriptReference
        })
        .map(|a| a.candidate_id.clone())
        .collect::<Vec<_>>();
    let pending_audio_candidate_ids = audits
        .iter()
        .filter(|a| a.state == AuditState::Pending && a.candidate_kind == CandidateKind::AudioAsset)
        .map(|a| a.candidate_id.clone())
        .collect::<Vec<_>>();
    let accounting = PreliminaryAccounting {
        source_total: scripts.len() + audio.len() + prior_rejections.len(),
        pending_total: pending_script_candidate_ids.len() + pending_audio_candidate_ids.len(),
        rejected_total: rejections.len(),
    };
    if audits.len() != accounting.source_total
        || accounting.pending_total + accounting.rejected_total != accounting.source_total
        || seen.len() != accounting.source_total
    {
        return Err(invalid("preliminary source accounting mismatch"));
    }
    Ok(MatchPlan {
        pending_groups: pending,
        audit_records: audits,
        candidate_rejections: rejections,
        group_explanations: explanations,
        pending_script_candidate_ids,
        pending_audio_candidate_ids,
        accounting,
    })
}
