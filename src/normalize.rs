//! Narrow normalization of proven Naninovel text and voice IDs.
use regex::{Captures, Regex};
use std::sync::LazyLock;
use unicode_general_category::get_general_category;
use unicode_normalization::UnicodeNormalization;

static RUBY: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r#"<ruby="([^"<>]+)">([^<>]*)</ruby>"#).unwrap());
static CONTROL: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)</?(?:link|color|b|size|sprite)(?:\s[^<>]*|=[^<>]*)?>|<br\s*/?>").unwrap()
});
static BREAKS: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"[^\S\n]*\n(?:[^\S\n]*\n)*[^\S\n]*").unwrap());

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NormalizationResult {
    pub text: String,
    pub operations: Vec<&'static str>,
    pub is_lexical: bool,
}

pub fn normalize_voice_id(raw: &str) -> Result<String, &'static str> {
    let id = if raw.len() >= 4 && raw.as_bytes()[raw.len() - 4..].eq_ignore_ascii_case(b".wav") {
        &raw[..raw.len() - 4]
    } else {
        raw
    };
    if id.is_empty()
        || id.len() > 512
        || id == "."
        || id == ".."
        || id.contains("..")
        || !id.is_ascii()
        || !id.bytes().enumerate().all(|(i, b)| {
            b.is_ascii_alphanumeric() || b == b'_' || (i > 0 && (b == b'.' || b == b'-'))
        })
    {
        return Err("invalid_voice_id");
    }
    Ok(id.to_owned())
}

pub fn normalize_transcription(raw: &str) -> NormalizationResult {
    let mut operations = Vec::new();
    if raw.len() > 4096
        || raw
            .chars()
            .any(|c| matches!(c as u32, 0..=8 | 11..=12 | 14..=31 | 127..=159))
    {
        return NormalizationResult {
            text: String::new(),
            operations,
            is_lexical: false,
        };
    }
    let mut text = raw.replace("\r\n", "\n").replace('\r', "\n");
    if text != raw {
        operations.push("line_endings");
    }
    let replaced = RUBY
        .replace_all(&text, |caps: &Captures<'_>| caps[1].to_owned())
        .into_owned();
    if replaced != text {
        operations.push("ruby_reading");
        text = replaced;
    }
    let replaced = CONTROL
        .replace_all(&text, |caps: &Captures<'_>| {
            if caps[0].to_ascii_lowercase().starts_with("<br") {
                "\n"
            } else {
                ""
            }
        })
        .into_owned();
    if replaced != text {
        operations.push("non_spoken_tag");
        text = replaced;
    }
    if text.contains(['<', '>']) {
        operations.push("unsupported_markup");
        return NormalizationResult {
            text: String::new(),
            operations,
            is_lexical: false,
        };
    }
    let starts_break = text
        .trim_start_matches(|c: char| c != '\n' && c.is_whitespace())
        .starts_with('\n');
    let ends_break = text
        .trim_end_matches(|c: char| c != '\n' && c.is_whitespace())
        .ends_with('\n');
    let mut collapsed = BREAKS.replace_all(&text, " ").into_owned();
    if starts_break && collapsed.starts_with(' ') {
        collapsed.remove(0);
    }
    if ends_break && collapsed.ends_with(' ') {
        collapsed.pop();
    }
    if collapsed != text {
        operations.push("spoken_line_breaks");
    }
    text = collapsed.nfc().collect();
    if text != collapsed {
        operations.push("unicode_nfc");
    }
    let lexical = text.len() <= 4096
        && text.chars().any(|c| {
            matches!(
                get_general_category(c).abbreviation().as_bytes()[0],
                b'L' | b'N'
            )
        });
    NormalizationResult {
        text,
        operations,
        is_lexical: lexical,
    }
}
