//! Structural Naninovel dialogue projection from detached Unity TypeTree values.
use crate::{
    config::ApprovedSpeakers,
    error::AppError,
    hash::sha256_bytes,
    model::{
        CandidateRejection, ParsedScriptReference, RawScriptObject, ScriptParseResult,
        ScriptProvenance, ScriptReference, ScriptResolutionResult, SpeakerEvidenceRecord,
    },
    normalize::{normalize_transcription, normalize_voice_id},
    speaker::{resolve_speaker, safe_speaker},
};
use std::collections::{BTreeMap, BTreeSet};
use unity_rs_core::type_tree::TypeValue as V;

fn invalid(message: &str) -> AppError {
    AppError::processing(format!("unreadable script object: {message}"))
}
fn get<'a>(value: &'a V, key: &str) -> Option<&'a V> {
    match value {
        V::Object(fields) => fields.iter().find(|f| f.name == key).map(|f| &f.value),
        V::Map(entries) => entries
            .iter()
            .find(|e| matches!(&e.key, V::String(s) if s == key))
            .map(|e| &e.value),
        _ => None,
    }
}
fn string(value: &V) -> Option<&str> {
    if let V::String(s) = value {
        Some(s)
    } else {
        None
    }
}
fn rid(value: &V) -> Option<String> {
    match value {
        V::String(s) => Some(s.clone()),
        V::Signed(n) => Some(n.to_string()),
        V::Unsigned(n) => Some(n.to_string()),
        _ => None,
    }
}
fn field_string<'a>(value: &'a V, key: &str) -> Option<&'a str> {
    get(value, key).and_then(string)
}
fn sequence(value: &V) -> Option<&[V]> {
    if let V::Array(items) = value {
        Some(items)
    } else {
        None
    }
}
fn keys(value: &V) -> Option<Vec<&str>> {
    match value {
        V::Object(fields) => Some(fields.iter().map(|f| f.name.as_str()).collect()),
        V::Map(entries) => entries.iter().map(|e| string(&e.key)).collect(),
        _ => None,
    }
}
fn sha(value: &str) -> String {
    sha256_bytes(value.as_bytes())
}
fn provenance(raw: &RawScriptObject, index: usize) -> ScriptProvenance {
    ScriptProvenance {
        bundle_path: raw.bundle_path.clone(),
        file_index: raw.file_index,
        object_index: raw.object_index,
        path_id: raw.path_id,
        container_path: raw.container.clone().unwrap_or_default(),
        command_index: index,
    }
}
fn candidate_id(source: &ScriptProvenance) -> String {
    sha(&format!(
        "{}\0{}\0{}\0{}",
        source.bundle_path, source.path_id, source.container_path, source.command_index
    ))
}
fn logical_path(path: &str) -> bool {
    !path.is_empty()
        && path.len() <= 4096
        && !path.starts_with('/')
        && !path.contains('\\')
        && path.split('/').all(|part| {
            !part.is_empty()
                && part != "."
                && part != ".."
                && part.len() <= 255
                && !part.chars().any(|c| (c as u32) < 32 || c == '\u{7f}')
        })
}
fn evidence(parts: Option<&[V]>, data: &V) -> (Vec<String>, bool) {
    let mut raw = Vec::new();
    if let Some(parts) = parts {
        for part in parts {
            if let Some(id) = get(part, "id") {
                raw.push(id);
            }
        }
    }
    let mut invalid = false;
    if let Some(path) = get(data, "VoicePath") {
        if let Some(value) = get(path, "value") {
            raw.push(value);
        } else {
            invalid = true;
        }
    }
    let mut ids = BTreeSet::new();
    for id in raw {
        match string(id).and_then(|s| normalize_voice_id(s).ok()) {
            Some(id) => {
                ids.insert(id);
            }
            None => invalid = true,
        }
    }
    (ids.into_iter().collect(), invalid)
}
fn reject(
    source: &ScriptProvenance,
    blocking: Vec<String>,
    stage: &'static str,
    reason: &'static str,
) -> CandidateRejection {
    CandidateRejection {
        candidate_id: candidate_id(source),
        group_id: if blocking.len() == 1 {
            Some(blocking[0].clone())
        } else {
            None
        },
        terminal_stage: stage,
        reason,
        provenance: source.clone(),
        blocking_group_ids: blocking,
    }
}
fn fallback_speaker(id: &str) -> Option<&str> {
    let suffix = id.rsplit_once('_')?.1;
    let index = suffix.find(|c: char| c.is_ascii_digit())?;
    let (speaker, number) = suffix.split_at(index);
    if safe_speaker(speaker) && !number.is_empty() && number.bytes().all(|b| b.is_ascii_digit()) {
        Some(speaker)
    } else {
        None
    }
}

pub fn parse_script_object(raw: &RawScriptObject) -> Result<ScriptParseResult, AppError> {
    let payload = &raw.value;
    let path = field_string(payload, "path")
        .filter(|p| logical_path(p))
        .ok_or_else(|| invalid("path"))?;
    let commands = get(payload, "playlist")
        .and_then(|v| get(v, "commands"))
        .and_then(sequence)
        .ok_or_else(|| invalid("playlist commands"))?;
    let refs = get(payload, "references")
        .and_then(|v| get(v, "RefIds"))
        .and_then(sequence)
        .ok_or_else(|| invalid("reference IDs"))?;
    let map = get(payload, "textMap")
        .and_then(|v| get(v, "idToText"))
        .ok_or_else(|| invalid("text map"))?;
    let text_keys = get(map, "keys")
        .and_then(sequence)
        .ok_or_else(|| invalid("text keys"))?;
    let values = get(map, "values")
        .and_then(sequence)
        .ok_or_else(|| invalid("text values"))?;
    if text_keys.len() != values.len() {
        return Err(invalid("text map length"));
    }
    let mut texts = BTreeMap::new();
    for (key, value) in text_keys.iter().zip(values) {
        let key = string(key).ok_or_else(|| invalid("text key"))?;
        if texts.insert(key, value).is_some() {
            return Err(invalid("duplicate text key"));
        }
    }
    let mut by_id = BTreeMap::new();
    for entry in refs {
        let rid = get(entry, "rid")
            .and_then(rid)
            .ok_or_else(|| invalid("reference rid"))?;
        if by_id.insert(rid, entry).is_some() {
            return Err(invalid("duplicate reference rid"));
        }
    }
    let mut kinds = Vec::with_capacity(commands.len());
    for command in commands {
        let rid = get(command, "rid")
            .and_then(rid)
            .ok_or_else(|| invalid("command rid"))?;
        let entry = by_id
            .get(&rid)
            .ok_or_else(|| invalid("missing reference rid"))?;
        let kind = get(entry, "type")
            .and_then(|v| field_string(v, "class"))
            .filter(|s| s.len() <= 128)
            .ok_or_else(|| invalid("command class"))?;
        kinds.push(kind);
    }
    let mut parsed_references = Vec::new();
    let mut candidate_rejections = Vec::new();
    let mut speaker_evidence = Vec::new();
    for (index, command) in commands.iter().enumerate() {
        let rid = get(command, "rid")
            .and_then(rid)
            .ok_or_else(|| invalid("command rid"))?;
        let entry = by_id[&rid];
        let kind = kinds[index];
        let data = get(entry, "data");
        let empty = V::Object(Vec::new());
        let data = data.unwrap_or(&empty);
        if keys(data).is_none() {
            return Err(invalid("command data"));
        }
        let text_value = get(data, "Text").and_then(|v| get(v, "value"));
        let parts = text_value.and_then(|v| get(v, "parts")).and_then(sequence);
        let embedded = parts.is_some_and(|items| items.iter().any(|p| get(p, "id").is_some()));
        let (blocking, invalid_id) = evidence(parts, data);
        let voice_like = kind.starts_with("Print")
            || kind == "PlayVoice"
            || get(data, "VoicePath").is_some()
            || embedded;
        if !voice_like {
            continue;
        }
        let source = provenance(raw, index);
        let raw_voice = parts
            .filter(|p| p.len() == 1)
            .and_then(|p| p.first())
            .and_then(|p| field_string(p, "id"))
            .or_else(|| get(data, "VoicePath").and_then(|v| field_string(v, "value")));
        let raw_author = get(data, "AuthorId").and_then(|v| field_string(v, "value"));
        if let Some(author) = raw_author.filter(|a| safe_speaker(a)) {
            speaker_evidence.push(SpeakerEvidenceRecord {
                speaker: author.to_owned(),
                source: "author_id",
                voice_id_sha256: raw_voice.map(sha),
                provenance: source.clone(),
            });
        } else if raw_author.is_none_or(str::is_empty)
            && let Some(speaker) = raw_voice
                .and_then(|v| normalize_voice_id(v).ok())
                .as_deref()
                .and_then(fallback_speaker)
        {
            speaker_evidence.push(SpeakerEvidenceRecord {
                speaker: speaker.to_owned(),
                source: "voice_id",
                voice_id_sha256: raw_voice.map(sha),
                provenance: source.clone(),
            });
        }
        if kind == "PrintTextModified" && blocking.len() > 1 {
            candidate_rejections.push(reject(
                &source,
                blocking,
                "asset_shape",
                "ambiguous_dialogue",
            ));
            continue;
        }
        if kind != "PrintTextModified" {
            candidate_rejections.push(reject(
                &source,
                blocking,
                "asset_shape",
                "unsupported_asset_shape",
            ));
            continue;
        }
        let Some(parts) = parts else {
            candidate_rejections.push(reject(
                &source,
                blocking,
                "asset_shape",
                "unsupported_asset_shape",
            ));
            continue;
        };
        if parts.len() != 1 {
            candidate_rejections.push(reject(
                &source,
                blocking,
                "asset_shape",
                if parts.is_empty() {
                    "unsupported_asset_shape"
                } else {
                    "ambiguous_dialogue"
                },
            ));
            continue;
        }
        let part = &parts[0];
        let exact = keys(part).is_some_and(|mut names| {
            names.sort_unstable();
            names == ["id", "spot", "text"]
        });
        if !exact || !matches!(get(part, "text"), Some(V::String(s)) if s.is_empty()) {
            candidate_rejections.push(reject(
                &source,
                blocking,
                "asset_shape",
                "unsupported_asset_shape",
            ));
            continue;
        }
        if invalid_id {
            candidate_rejections.push(reject(&source, blocking, "voice_id", "invalid_voice_id"));
            continue;
        }
        let Some(voice_id) = field_string(part, "id").and_then(|id| normalize_voice_id(id).ok())
        else {
            candidate_rejections.push(reject(&source, blocking, "voice_id", "invalid_voice_id"));
            continue;
        };
        let Some(raw_text) = field_string(part, "id")
            .and_then(|id| texts.get(id))
            .and_then(|v| string(v))
        else {
            candidate_rejections.push(reject(
                &source,
                blocking,
                "asset_shape",
                "ambiguous_dialogue",
            ));
            continue;
        };
        let author_value = get(data, "AuthorId").and_then(|v| get(v, "value"));
        if author_value.is_some_and(|v| !matches!(v, V::String(_))) {
            candidate_rejections.push(reject(
                &source,
                blocking,
                "asset_shape",
                "unsupported_asset_shape",
            ));
            continue;
        }
        let author = author_value
            .and_then(string)
            .filter(|s| !s.is_empty())
            .map(str::to_owned);
        let transcription = normalize_transcription(raw_text);
        if !transcription.is_lexical {
            candidate_rejections.push(reject(
                &source,
                blocking,
                "transcription",
                "non_lexical_transcription",
            ));
            continue;
        }
        parsed_references.push(ParsedScriptReference {
            candidate_id: candidate_id(&source),
            voice_id,
            raw_author_id: author,
            raw_display_text: raw_text.to_owned(),
            transcription,
            provenance: source,
            script_path: path.to_owned(),
            script_name: path.rsplit('/').next().unwrap_or(path).to_owned(),
            neighbor_commands: (index.saturating_sub(1)..(index + 2).min(kinds.len()))
                .map(|i| (i, kinds[i].to_owned()))
                .collect(),
        });
    }
    Ok(ScriptParseResult {
        parsed_references,
        candidate_rejections,
        speaker_evidence,
        object_bundle_path: raw.bundle_path.clone(),
        object_file_index: raw.file_index,
        object_index: raw.object_index,
        object_path_id: raw.path_id,
        object_container_path_sha256: sha(raw.container.as_deref().unwrap_or("")),
        script_path_sha256: sha(path),
        command_count: commands.len(),
        reference_count: refs.len(),
        text_entry_count: text_keys.len(),
    })
}

pub fn resolve_script_references(
    results: &[ScriptParseResult],
    approved: &ApprovedSpeakers,
) -> Result<ScriptResolutionResult, AppError> {
    let mut references = Vec::new();
    let mut rejections = Vec::new();
    let mut source_ids = BTreeSet::new();
    let mut candidate_ids = BTreeSet::new();
    for result in results {
        let key = (
            &result.object_bundle_path,
            result.object_file_index,
            result.object_index,
            result.object_path_id,
        );
        if !source_ids.insert(key) {
            return Err(invalid("duplicate script source"));
        }
        for rejected in &result.candidate_rejections {
            if !candidate_ids.insert(&rejected.candidate_id) {
                return Err(invalid("duplicate candidate ID"));
            }
            rejections.push(rejected.clone());
        }
        for parsed in &result.parsed_references {
            if !candidate_ids.insert(&parsed.candidate_id) {
                return Err(invalid("duplicate candidate ID"));
            }
            let resolution =
                resolve_speaker(parsed.raw_author_id.as_deref(), &parsed.voice_id, approved);
            if resolution.speaker.is_some() {
                references.push(ScriptReference {
                    parsed: parsed.clone(),
                    speaker_resolution: resolution,
                });
            } else {
                rejections.push(reject(
                    &parsed.provenance,
                    vec![parsed.voice_id.clone()],
                    "speaker",
                    "speaker_conflict",
                ));
            }
        }
    }
    references.sort_by(|a, b| a.parsed.candidate_id.cmp(&b.parsed.candidate_id));
    rejections.sort_by(|a, b| a.candidate_id.cmp(&b.candidate_id));
    Ok(ScriptResolutionResult {
        references,
        candidate_rejections: rejections,
    })
}
