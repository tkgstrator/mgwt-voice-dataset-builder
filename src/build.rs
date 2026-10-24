//! Approved input to locally verified Parquet dataset orchestration.
use crate::{
    config::{BundleEntry, BundleKind, InputEntry},
    delivery::{self, PreparedDataset, Report},
    error::AppError,
    hash::sha256_bytes,
    inventory::{self, VerifiedBundleInputs, VerifiedInputFile},
    matching::{self, SourceTotals},
    model::{CandidateKind, CandidateProvenance, CandidateRejection},
    naninovel::{parse_script_object, resolve_script_references},
    parquet_writer::InputRow,
    paths::{ValidatedAppPath, ValidatedOutputPath},
    resolution::{self, TerminalKind},
    unity::{BundleSession, UnityBackend, UnityReader},
};
use serde_json::{Value, json};
#[cfg(windows)]
use std::sync::OnceLock;
use std::{
    collections::BTreeMap,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
};

const SIDECAR_ALLOWANCE: u64 = 64 * 1024 * 1024;
const SAFETY_MARGIN: u64 = 512 * 1024 * 1024;
const MAX_SINGLE_WAV: u64 = 256 * 1024 * 1024;

struct BuildInterruption {
    raised: Arc<AtomicBool>,
    #[cfg(unix)]
    registrations: [signal_hook::SigId; 2],
}

impl BuildInterruption {
    fn new() -> Result<Self, AppError> {
        #[cfg(unix)]
        {
            let raised = Arc::new(AtomicBool::new(false));
            let first =
                signal_hook::flag::register(signal_hook::consts::SIGINT, Arc::clone(&raised))
                    .map_err(|e| AppError::processing(format!("install interrupt handler: {e}")))?;
            let second = match signal_hook::flag::register(
                signal_hook::consts::SIGTERM,
                Arc::clone(&raised),
            ) {
                Ok(id) => id,
                Err(e) => {
                    signal_hook::low_level::unregister(first);
                    return Err(AppError::processing(format!(
                        "install termination handler: {e}"
                    )));
                }
            };
            Ok(Self {
                raised,
                registrations: [first, second],
            })
        }
        #[cfg(windows)]
        {
            static HANDLER: OnceLock<Result<Arc<AtomicBool>, String>> = OnceLock::new();
            let raised = HANDLER
                .get_or_init(|| {
                    let raised = Arc::new(AtomicBool::new(false));
                    let flag = Arc::clone(&raised);
                    ctrlc::set_handler(move || flag.store(true, Ordering::SeqCst))
                        .map_err(|e| e.to_string())?;
                    Ok(raised)
                })
                .as_ref()
                .map_err(|e| AppError::processing(format!("install interrupt handler: {e}")))?;
            Ok(Self {
                raised: Arc::clone(raised),
            })
        }
    }

    fn check(&self) -> Result<(), AppError> {
        if self.raised.load(Ordering::SeqCst) {
            Err(AppError::processing("build interrupted"))
        } else {
            Ok(())
        }
    }
}

impl Drop for BuildInterruption {
    fn drop(&mut self) {
        #[cfg(unix)]
        for id in self.registrations {
            signal_hook::low_level::unregister(id);
        }
    }
}

fn bound_bundle(
    app: &ValidatedAppPath,
    bundle: &BundleEntry,
) -> Result<VerifiedBundleInputs, AppError> {
    Ok(VerifiedBundleInputs {
        bundle: VerifiedInputFile::open(
            app,
            &InputEntry {
                relative_path: bundle.relative_path.clone(),
                size: bundle.size,
                sha256: bundle.sha256.clone(),
            },
        )?,
        resources: Vec::new(),
    })
}

fn future_space_need(next_wav: u64, next_parquet: u64, snapshot: u64) -> Result<u64, AppError> {
    next_wav
        .checked_add(next_parquet)
        .and_then(|n| n.checked_add(snapshot))
        .and_then(|n| n.checked_add(SIDECAR_ALLOWANCE))
        .and_then(|n| n.checked_add(SAFETY_MARGIN))
        .ok_or_else(|| AppError::processing("disk reservation arithmetic overflow"))
}

fn ensure_free_space(free: u64, needed: u64) -> Result<(), AppError> {
    if needed > free {
        return Err(AppError::processing(format!(
            "insufficient output disk space: need {needed} bytes, available {free} bytes"
        )));
    }
    Ok(())
}

/// Current free space already reflects every staged WAV and Parquet byte written so far.
fn check_disk(
    output: &ValidatedOutputPath,
    next_wav: u64,
    next_parquet: u64,
    snapshot: u64,
) -> Result<(), AppError> {
    #[cfg(unix)]
    let free = {
        let stat = rustix::fs::fstatvfs(output.parent())
            .map_err(|e| AppError::processing(format!("inspect output free space: {e}")))?;
        stat.f_bavail
            .checked_mul(stat.f_frsize)
            .ok_or_else(|| AppError::processing("free-space arithmetic overflow"))?
    };
    #[cfg(windows)]
    let free = mgwt_windows_native::available_space(output.parent())
        .map_err(|e| AppError::processing(format!("inspect output free space: {e}")))?;
    ensure_free_space(free, future_space_need(next_wav, next_parquet, snapshot)?)
}

fn kind_label(kind: CandidateKind) -> &'static str {
    match kind {
        CandidateKind::ScriptReference => "script_reference",
        CandidateKind::AudioAsset => "audio_asset",
    }
}
fn provenance_json(source: &CandidateProvenance) -> Value {
    match source {
        CandidateProvenance::Script(p) => {
            json!({"bundle_path":p.bundle_path,"file_index":p.file_index,"object_index":p.object_index,"path_id":p.path_id,"container_path":p.container_path,"command_index":p.command_index})
        }
        CandidateProvenance::Audio(p) => {
            json!({"bundle_path":p.bundle_path,"file_index":p.file_index,"object_index":p.object_index,"path_id":p.path_id,"container_path":p.container_path})
        }
    }
}
fn rejection_json(r: &CandidateRejection) -> Value {
    json!({"candidate_id":r.candidate_id,"candidate_kind":kind_label(r.candidate_kind),"group_id":r.group_id,"terminal_stage":r.terminal_stage,"reason":r.reason,"blocking_group_ids":r.blocking_group_ids,"provenance":provenance_json(&r.provenance)})
}
fn terminal_label(kind: TerminalKind) -> &'static str {
    match kind {
        TerminalKind::AcceptedScript => "accepted",
        TerminalKind::CanonicalAudio => "canonical_audio",
        TerminalKind::DuplicateAlias => "duplicate_alias",
        TerminalKind::Rejected => "rejected",
    }
}

/// Build a complete local dataset from the exact checked-in approval. No network publication.
pub fn build_app(
    app: &ValidatedAppPath,
    output: &ValidatedOutputPath,
    shard_audio_mib: u64,
    bundle_workers: usize,
) -> Result<Report, AppError> {
    if bundle_workers == 0 {
        return Err(AppError::validation("bundle worker count must be positive"));
    }
    let interruption = BuildInterruption::new()?;
    let approval = crate::config::checked_in_approval()?;
    let pre = inventory::inventory_app(app, &approval.manifest)?;
    interruption.check()?;
    let shard_target = shard_audio_mib
        .checked_mul(1024 * 1024)
        .filter(|n| *n > 0)
        .ok_or_else(|| AppError::validation("invalid shard audio target"))?;
    let max_snapshot = approval
        .manifest
        .bundles
        .iter()
        .map(|b| b.size)
        .max()
        .unwrap_or(0)
        .checked_add(
            approval
                .manifest
                .resources
                .iter()
                .map(|r| r.size)
                .sum::<u64>(),
        )
        .ok_or_else(|| AppError::processing("snapshot reserve overflow"))?;
    let next_shard = shard_target.max(MAX_SINGLE_WAV);
    check_disk(output, 0, next_shard, max_snapshot)?;
    delivery::build_from_validated(
        output,
        shard_target,
        |work| {
            let reader = UnityReader::new(output.requested_parent());
            let mut parsed = Vec::new();
            let mut script_bundles = 0_u64;
            for bundle in approval
                .manifest
                .bundles
                .iter()
                .filter(|b| b.kind == BundleKind::Script)
            {
                interruption.check()?;
                check_disk(output, 0, next_shard, bundle.size)?;
                let approved = bound_bundle(app, bundle)?;
                let mut session = reader.open_bundle(&approved)?;
                let raw_objects = session.raw_script_objects();
                session.finish()?;
                for raw in raw_objects? {
                    interruption.check()?;
                    parsed.push(parse_script_object(&raw)?);
                }
                script_bundles += 1;
            }
            let counts = &approval.speakers.counts;
            let object_count = parsed.len() as u64;
            let command_count = parsed.iter().map(|p| p.command_count as u64).sum::<u64>();
            let reference_count = parsed.iter().map(|p| p.reference_count as u64).sum::<u64>();
            let text_entry_count = parsed
                .iter()
                .map(|p| p.text_entry_count as u64)
                .sum::<u64>();
            let parsed_count = parsed
                .iter()
                .map(|p| p.parsed_references.len() as u64)
                .sum::<u64>();
            let parser_rejection_count = parsed
                .iter()
                .map(|p| p.candidate_rejections.len() as u64)
                .sum::<u64>();
            let evidence_count = parsed
                .iter()
                .map(|p| p.speaker_evidence.len() as u64)
                .sum::<u64>();
            if (
                script_bundles,
                object_count,
                command_count,
                reference_count,
                text_entry_count,
                parsed_count,
                parser_rejection_count,
                evidence_count,
            ) != (
                counts.bundle_count,
                counts.object_count,
                counts.command_count,
                counts.reference_count,
                counts.text_entry_count,
                counts.parsed_reference_count,
                counts.candidate_rejection_count,
                counts.speaker_evidence_count,
            ) {
                return Err(AppError::validation(
                    "approved script structure counts changed",
                ));
            }
            let scripts = resolve_script_references(&parsed, &approval.speakers)?;
            let speaker_rejections = scripts
                .candidate_rejections
                .len()
                .checked_sub(parser_rejection_count as usize)
                .ok_or_else(|| AppError::processing("speaker rejection accounting underflow"))?;
            let independent_scripts = (parsed_count as usize)
                .checked_sub(speaker_rejections)
                .ok_or_else(|| AppError::processing("script count underflow"))?;
            let independent_rejections = (parser_rejection_count as usize)
                .checked_add(speaker_rejections)
                .ok_or_else(|| AppError::processing("rejection count overflow"))?;
            if scripts.references.len() != independent_scripts
                || scripts.candidate_rejections.len() != independent_rejections
            {
                return Err(AppError::processing("script source accounting mismatch"));
            }
            drop(parsed);
            let mut audio = Vec::new();
            let mut voice_inputs = BTreeMap::new();
            let mut independent_audio = 0_usize;
            for bundle in approval
                .manifest
                .bundles
                .iter()
                .filter(|b| b.kind == BundleKind::Voice)
            {
                interruption.check()?;
                check_disk(output, 0, next_shard, bundle.size)?;
                let approved = bound_bundle(app, bundle)?;
                let mut session = reader.open_bundle(&approved)?;
                let references = session.audio_refs();
                session.finish()?;
                let references = references?;
                independent_audio = independent_audio
                    .checked_add(references.len())
                    .ok_or_else(|| AppError::processing("audio source count overflow"))?;
                audio.extend(references);
                voice_inputs.insert(bundle.relative_path.clone(), approved);
            }
            let plan = matching::match_candidates(
                &scripts.references,
                &audio,
                &scripts.candidate_rejections,
                SourceTotals {
                    script_references: independent_scripts,
                    audio_assets: independent_audio,
                    prior_rejections: independent_rejections,
                },
            )?;
            let effective_workers = resolution::effective_bundle_workers(&plan, bundle_workers);
            let parallel_snapshot_reserve = max_snapshot
                .checked_mul(
                    u64::try_from(effective_workers)
                        .map_err(|_| AppError::processing("parallel snapshot reserve overflow"))?,
                )
                .ok_or_else(|| AppError::processing("parallel snapshot reserve overflow"))?;
            let resolved = resolution::resolve_with_workers(
                &plan,
                &reader,
                &voice_inputs,
                work,
                &crate::model::AudioLimits::default(),
                effective_workers,
                |_already_staged, next_wav| {
                    interruption.check()?;
                    check_disk(output, next_wav, next_shard, parallel_snapshot_reserve)
                },
            )?;
            if resolved.rows.is_empty() {
                return Err(AppError::processing("no accepted dataset rows"));
            }
            let rows = resolved
                .rows
                .iter()
                .map(|row| InputRow {
                    audio_path: row.staged_wav.clone(),
                    audio_name: format!("{}.wav", row.canonical_audio_id),
                    transcription: row.transcription.clone(),
                    speaker: row.speaker.clone(),
                    source_path: row.source_path.clone(),
                })
                .collect();
            let terminals: BTreeMap<_, _> = resolved
                .terminal_sources
                .iter()
                .map(|t| (t.candidate_id.as_str(), t))
                .collect();
            let rejected: BTreeMap<_, _> = resolved
                .candidate_rejections
                .iter()
                .map(|r| (r.candidate_id.as_str(), r))
                .collect();
            let audit_records = plan.audit_records.iter().map(|source| {
            let terminal = terminals.get(source.candidate_id.as_str()).expect("terminal accounting validated");
            json!({"candidate_id":source.candidate_id,"candidate_kind":kind_label(source.candidate_kind),"group_id":source.group_id,"status":terminal_label(terminal.kind),"canonical_audio_id":terminal.canonical_audio_id,"reason":rejected.get(source.candidate_id.as_str()).map(|r| r.reason),"provenance":provenance_json(&source.provenance)})
        }).collect();
            let rejected_records = resolved
                .candidate_rejections
                .iter()
                .map(rejection_json)
                .collect();
            Ok(PreparedDataset {
                rows,
                audit_records,
                rejected_records,
                producer_revision: env!("CARGO_PKG_VERSION").into(),
                approved_input_sha256: sha256_bytes(include_bytes!(
                    "../config/manosaba-inputs-v1.json"
                )),
            })
        },
        || {
            interruption.check()?;
            let post = inventory::inventory_app(app, &approval.manifest)?;
            interruption.check()?;
            if post != pre {
                return Err(AppError::validation("approved inputs changed during build"));
            }
            Ok(())
        },
        |_written_audio| check_disk(output, 0, next_shard, max_snapshot),
        || interruption.check(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn current_free_space_counts_only_future_writes() {
        let next_wav = 2 * 1024 * 1024;
        let next_shard = 500 * 1024 * 1024;
        let snapshot = 70 * 1024 * 1024;
        let needed = future_space_need(next_wav, next_shard, snapshot).unwrap();
        // Earlier staged WAVs and Parquet shards have already reduced the free-space reading.
        let already_written = 10 * 1024 * 1024 * 1024_u64;
        assert!(already_written > needed);
        assert!(ensure_free_space(needed, needed).is_ok());
        assert!(ensure_free_space(needed - 1, needed).is_err());
        assert_eq!(
            needed,
            next_wav + next_shard + snapshot + SIDECAR_ALLOWANCE + SAFETY_MARGIN
        );
    }

    #[cfg(unix)]
    #[test]
    fn injected_signal_flag_requests_cancellation() {
        let interruption = BuildInterruption::new().unwrap();
        assert!(interruption.check().is_ok());
        interruption.raised.store(true, Ordering::SeqCst);
        assert!(
            interruption
                .check()
                .unwrap_err()
                .to_string()
                .contains("interrupted")
        );
    }
}
