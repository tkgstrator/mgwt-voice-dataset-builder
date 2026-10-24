//! Finite, exact speaker resolution.
use crate::{config::ApprovedSpeakers, normalize::normalize_voice_id};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SpeakerResolution {
    pub speaker: Option<String>,
    pub evidence: Vec<&'static str>,
    pub reason: Option<&'static str>,
}

pub fn resolve_speaker(
    author: Option<&str>,
    voice_id: &str,
    approved: &ApprovedSpeakers,
) -> SpeakerResolution {
    let reject = |reason| SpeakerResolution {
        speaker: None,
        evidence: Vec::new(),
        reason: Some(reason),
    };
    if author.is_some_and(|s| !safe_speaker(s)) {
        return reject("unsafe_author_id");
    }
    let Ok(id) = normalize_voice_id(voice_id) else {
        return reject("ambiguous_or_missing_voice_evidence");
    };
    let Some((_, suffix)) = id.rsplit_once('_') else {
        return reject("ambiguous_or_missing_voice_evidence");
    };
    let matches: Vec<_> = approved
        .speaker_ids
        .iter()
        .filter(|speaker| {
            suffix.strip_prefix(speaker.as_str()).is_some_and(|digits| {
                !digits.is_empty() && digits.bytes().all(|b| b.is_ascii_digit())
            })
        })
        .collect();
    if matches.len() != 1 {
        return reject("ambiguous_or_missing_voice_evidence");
    }
    let speaker = matches[0];
    if author.is_some_and(|a| a != speaker) {
        return reject("speaker_conflict");
    }
    SpeakerResolution {
        speaker: Some(speaker.clone()),
        evidence: if author.is_some() {
            vec!["author_id", "voice_id"]
        } else {
            vec!["voice_id"]
        },
        reason: None,
    }
}

pub fn safe_speaker(value: &str) -> bool {
    !value.is_empty()
        && value
            .bytes()
            .enumerate()
            .all(|(i, b)| b.is_ascii_alphabetic() || (i > 0 && b.is_ascii_digit()))
}
