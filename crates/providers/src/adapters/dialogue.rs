//! Model-facing character-detection wire format.
//!
//! Language models cannot count UTF-8 bytes reliably, particularly in text with multi-byte
//! characters such as German umlauts or typographic quotation marks. The adapter therefore asks
//! for verbatim quote anchors (the opening and closing words of each spoken passage) and resolves
//! them to byte ranges deterministically. Paragraph IDs are replaced with short request-local
//! aliases so the model spends its output budget on attribution instead of echoing UUIDs.

use std::collections::BTreeMap;

use serde::Deserialize;

use crate::{
    CharacterDetectionRequest, CharacterDetectionResult, DetectedCharacter, DetectedDialogue,
    DetectionParagraph, ProviderError, Result,
};

pub(super) const DETECTION_SCHEMA: &str = r#"{"type":"object","additionalProperties":false,"properties":{"characters":{"type":"array","items":{"type":"object","additionalProperties":false,"properties":{"canonical_name":{"type":"string"},"aliases":{"type":"array","items":{"type":"string"}},"confidence":{"type":"number","minimum":0,"maximum":1}},"required":["canonical_name","aliases","confidence"]}},"dialogue":{"type":"array","items":{"type":"object","additionalProperties":false,"properties":{"paragraph_id":{"type":"string"},"quote_start":{"type":"string"},"quote_end":{"type":"string"},"character":{"type":"string"},"confidence":{"type":"number","minimum":0,"maximum":1}},"required":["paragraph_id","quote_start","quote_end","character","confidence"]}}},"required":["characters","dialogue"]}"#;

/// Paragraph payload sent to the model. `id` is a short request-local alias.
#[derive(serde::Serialize)]
struct WireParagraph<'a> {
    id: String,
    text: &'a str,
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    context_only: bool,
}

/// Serializes the request paragraphs with short aliases (`p1`, `p2`, ...).
pub(super) fn wire_input(request: &CharacterDetectionRequest) -> Result<String> {
    let paragraphs = request
        .paragraphs
        .iter()
        .enumerate()
        .map(|(index, paragraph)| WireParagraph {
            id: wire_id(index),
            text: &paragraph.text,
            context_only: paragraph.context_only,
        })
        .collect::<Vec<_>>();
    serde_json::to_string(&paragraphs)
        .map_err(|error| ProviderError::Configuration(error.to_string()))
}

fn wire_id(index: usize) -> String {
    format!("p{}", index + 1)
}

#[derive(Debug, Deserialize)]
struct WireResult {
    #[serde(default)]
    characters: Vec<WireCharacter>,
    #[serde(default)]
    dialogue: Vec<WireDialogue>,
}

#[derive(Debug, Deserialize)]
struct WireCharacter {
    canonical_name: String,
    #[serde(default)]
    aliases: Vec<String>,
    #[serde(default)]
    confidence: Option<f32>,
}

#[derive(Debug, Deserialize)]
struct WireDialogue {
    paragraph_id: String,
    character: String,
    #[serde(default)]
    quote_start: Option<String>,
    #[serde(default)]
    quote_end: Option<String>,
    /// Some models return the complete passage instead of separate anchors.
    #[serde(default)]
    text: Option<String>,
    /// Legacy byte offsets remain a last-resort fallback for models that ignore the schema.
    #[serde(default)]
    start: Option<u32>,
    #[serde(default)]
    end: Option<u32>,
    #[serde(default)]
    confidence: Option<f32>,
}

/// Parses model output and resolves every dialogue passage against the request paragraphs.
///
/// Individual passages that cannot be located are dropped instead of failing the whole batch:
/// an unattributed passage falls back to the narrator voice and stays editable in review, while a
/// failed batch would discard every correct attribution the model produced.
pub(super) fn parse_and_resolve(
    content: &str,
    request: &CharacterDetectionRequest,
) -> Result<CharacterDetectionResult> {
    let json = extract_json_object(content);
    let wire: WireResult = serde_json::from_str(json).map_err(|error| {
        if error.is_eof() {
            ProviderError::OutputTruncated
        } else {
            ProviderError::InvalidResponse(format!(
                "character detection output is not schema-valid JSON (line {}, column {})",
                error.line(),
                error.column()
            ))
        }
    })?;
    Ok(resolve(wire, request))
}

fn resolve(wire: WireResult, request: &CharacterDetectionRequest) -> CharacterDetectionResult {
    let characters = wire
        .characters
        .into_iter()
        .filter_map(|character| {
            let canonical_name = character.canonical_name.trim().to_owned();
            (!canonical_name.is_empty()).then(|| DetectedCharacter {
                canonical_name,
                aliases: character
                    .aliases
                    .into_iter()
                    .map(|alias| alias.trim().to_owned())
                    .filter(|alias| !alias.is_empty())
                    .collect(),
                confidence: clamp_confidence(character.confidence),
            })
        })
        .collect();

    let by_alias = request
        .paragraphs
        .iter()
        .enumerate()
        .map(|(index, paragraph)| (wire_id(index), paragraph))
        .collect::<BTreeMap<_, _>>();
    let mut cursors = BTreeMap::<&str, usize>::new();
    let mut dialogue = Vec::with_capacity(wire.dialogue.len());
    let mut unresolved = 0_usize;
    for item in wire.dialogue {
        let character = item.character.trim();
        let paragraph = by_alias
            .get(item.paragraph_id.trim())
            .copied()
            // Tolerate models that echo the original identifier.
            .or_else(|| {
                request
                    .paragraphs
                    .iter()
                    .find(|paragraph| paragraph.id == item.paragraph_id.trim())
            });
        let (Some(paragraph), false) = (paragraph, character.is_empty()) else {
            unresolved += 1;
            continue;
        };
        let cursor = cursors.get(paragraph.id.as_str()).copied().unwrap_or(0);
        let Some((start, end)) = locate_item(&item, paragraph, cursor) else {
            unresolved += 1;
            continue;
        };
        let (Ok(start_u32), Ok(end_u32)) = (u32::try_from(start), u32::try_from(end)) else {
            unresolved += 1;
            continue;
        };
        cursors.insert(paragraph.id.as_str(), end);
        dialogue.push(DetectedDialogue {
            paragraph_id: paragraph.id.clone(),
            character: character.to_owned(),
            start: start_u32,
            end: end_u32,
            confidence: clamp_confidence(item.confidence),
        });
    }
    if unresolved > 0 {
        tracing::debug!(
            diagnostic_code = "detection.dialogue.unresolved",
            unresolved,
            resolved = dialogue.len(),
            "dropped dialogue passages that could not be located in their paragraph"
        );
    }
    CharacterDetectionResult {
        characters,
        dialogue,
        usage: crate::ProviderUsage::default(),
    }
}

fn clamp_confidence(value: Option<f32>) -> f32 {
    value
        .filter(|value| value.is_finite())
        .map_or(0.5, |value| value.clamp(0.0, 1.0))
}

fn locate_item(
    item: &WireDialogue,
    paragraph: &DetectionParagraph,
    cursor: usize,
) -> Option<(usize, usize)> {
    let text = paragraph.text.as_str();
    let full = non_blank(item.text.as_deref());
    let opening = non_blank(item.quote_start.as_deref())
        .or(full)
        .or_else(|| non_blank(item.quote_end.as_deref()));
    if let Some(opening) = opening {
        let closing = non_blank(item.quote_end.as_deref())
            .or(full)
            .unwrap_or(opening);
        // Prefer the next unmatched occurrence so repeated lines map to successive passages.
        let range = locate_passage(text, opening, closing, cursor).or_else(|| {
            (cursor > 0)
                .then(|| locate_passage(text, opening, closing, 0))
                .flatten()
        })?;
        return Some(expand_to_quotation_marks(text, range));
    }
    let (start, end) = (
        usize::try_from(item.start?).ok()?,
        usize::try_from(item.end?).ok()?,
    );
    (start < end && end <= text.len() && text.is_char_boundary(start) && text.is_char_boundary(end))
        .then_some((start, end))
}

fn non_blank(value: Option<&str>) -> Option<&str> {
    value.filter(|value| !value.trim().is_empty())
}

/// Finds the passage that begins with `opening` and ends with `closing`, searching from `from`.
pub(crate) fn locate_passage(
    text: &str,
    opening: &str,
    closing: &str,
    from: usize,
) -> Option<(usize, usize)> {
    let haystack = NormalizedText::new(text);
    let opening = normalize_anchor(opening);
    let closing = normalize_anchor(closing);
    if opening.is_empty() || closing.is_empty() {
        return None;
    }
    let from = haystack.position_at_or_after(from);
    let start = haystack.find(&opening, from)?;
    let minimum_end = start + opening.len();
    // Choose the first closing anchor that ends at or after the opening anchor. This also covers
    // short passages where the model repeats the same words in both anchors.
    let mut search = start;
    loop {
        let candidate = haystack.find(&closing, search)?;
        let candidate_end = candidate + closing.len();
        if candidate_end >= minimum_end {
            return Some((
                haystack.byte_start(start),
                haystack.byte_end(candidate_end - 1),
            ));
        }
        search = candidate + 1;
    }
}

/// Includes directly adjacent quotation marks in the passage. Otherwise the narrator would be
/// asked to speak segments consisting only of punctuation.
fn expand_to_quotation_marks(text: &str, (mut start, mut end): (usize, usize)) -> (usize, usize) {
    while let Some(previous) = text[..start].chars().next_back() {
        if !is_quotation_mark(previous) {
            break;
        }
        start -= previous.len_utf8();
    }
    // Trailing punctuation directly inside the closing mark belongs to the utterance.
    let mut probe = end;
    while let Some(next) = text[probe..].chars().next() {
        if is_quotation_mark(next) {
            probe += next.len_utf8();
            end = probe;
        } else if matches!(next, ',' | '.' | '!' | '?' | '…' | ';' | ':') && probe == end {
            probe += next.len_utf8();
        } else {
            break;
        }
    }
    (start, end)
}

const fn is_quotation_mark(character: char) -> bool {
    matches!(
        character,
        '"' | '\u{201C}'
            | '\u{201D}'
            | '\u{201E}'
            | '\u{201F}'
            | '\u{00AB}'
            | '\u{00BB}'
            | '\u{2039}'
            | '\u{203A}'
            | '\u{300C}'
            | '\u{300D}'
            | '\u{300E}'
            | '\u{300F}'
            | '\u{2018}'
            | '\u{201A}'
            | '\u{201B}'
    )
}

/// Characters that are folded before matching, with the original byte range they came from.
struct NormalizedText {
    characters: Vec<char>,
    origins: Vec<(usize, usize)>,
}

impl NormalizedText {
    fn new(text: &str) -> Self {
        let mut characters = Vec::with_capacity(text.len());
        let mut origins = Vec::with_capacity(text.len());
        for (offset, character) in text.char_indices() {
            let origin = (offset, offset + character.len_utf8());
            push_folded(character, origin, &mut characters, &mut origins);
        }
        Self {
            characters,
            origins,
        }
    }

    fn position_at_or_after(&self, byte: usize) -> usize {
        self.origins.partition_point(|origin| origin.0 < byte)
    }

    fn find(&self, needle: &[char], from: usize) -> Option<usize> {
        if needle.is_empty() || needle.len() > self.characters.len() {
            return None;
        }
        (from..=self.characters.len() - needle.len())
            .find(|&start| self.characters[start..start + needle.len()] == *needle)
    }

    fn byte_start(&self, position: usize) -> usize {
        self.origins[position].0
    }

    fn byte_end(&self, position: usize) -> usize {
        self.origins[position].1
    }
}

fn normalize_anchor(value: &str) -> Vec<char> {
    let trimmed = value
        .trim()
        .trim_matches(|character: char| is_quotation_mark(character) || character.is_whitespace());
    let mut characters = Vec::new();
    let mut origins = Vec::new();
    for character in trimmed.chars() {
        push_folded(character, (0, 0), &mut characters, &mut origins);
    }
    while characters.last() == Some(&' ') {
        characters.pop();
    }
    while characters.first() == Some(&' ') {
        characters.remove(0);
    }
    characters
}

fn push_folded(
    character: char,
    origin: (usize, usize),
    characters: &mut Vec<char>,
    origins: &mut Vec<(usize, usize)>,
) {
    let folded = match character {
        // Invisible formatting characters never appear in model output.
        '\u{00AD}' | '\u{200B}' | '\u{200C}' | '\u{200D}' | '\u{2060}' | '\u{FEFF}' => return,
        '\u{2018}' | '\u{2019}' | '\u{201A}' | '\u{201B}' | '\u{2032}' | '`' | '\u{00B4}' => '\'',
        '\u{2010}' | '\u{2011}' | '\u{2012}' | '\u{2013}' | '\u{2014}' | '\u{2015}'
        | '\u{2212}' => '-',
        '\u{2026}' => {
            for _ in 0..3 {
                characters.push('.');
                origins.push(origin);
            }
            return;
        }
        character if is_quotation_mark(character) => '"',
        character if character.is_whitespace() => {
            if characters.last() == Some(&' ') {
                return;
            }
            ' '
        }
        character => character.to_lowercase().next().unwrap_or(character),
    };
    characters.push(folded);
    origins.push(origin);
}

/// Accepts fenced JSON and leading or trailing prose around the top-level object.
pub(super) fn extract_json_object(content: &str) -> &str {
    let trimmed = content.trim();
    let unfenced = trimmed
        .strip_prefix("```json")
        .or_else(|| trimmed.strip_prefix("```"))
        .and_then(|value| value.strip_suffix("```"))
        .map_or(trimmed, str::trim);
    if unfenced.starts_with('{') {
        return unfenced;
    }
    match (unfenced.find('{'), unfenced.rfind('}')) {
        (Some(start), Some(end)) if start < end => &unfenced[start..=end],
        // Leave truncated or non-JSON output intact so the parser can classify it.
        (Some(start), _) => &unfenced[start..],
        _ => unfenced,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn request(paragraphs: &[(&str, &str, bool)]) -> CharacterDetectionRequest {
        CharacterDetectionRequest {
            request_id: uuid::Uuid::new_v4(),
            model: "model".to_owned(),
            system_prompt: String::new(),
            paragraphs: paragraphs
                .iter()
                .map(|(id, text, context_only)| DetectionParagraph {
                    id: (*id).to_owned(),
                    text: (*text).to_owned(),
                    context_only: *context_only,
                })
                .collect(),
            temperature: crate::Temperature::Default,
            reasoning: crate::ReasoningControl::Inherit,
            max_output_tokens: 1_024,
        }
    }

    fn slice<'a>(request: &'a CharacterDetectionRequest, span: &DetectedDialogue) -> &'a str {
        let paragraph = request
            .paragraphs
            .iter()
            .find(|paragraph| paragraph.id == span.paragraph_id)
            .unwrap();
        &paragraph.text[span.start as usize..span.end as usize]
    }

    #[test]
    fn wire_input_uses_short_aliases_and_omits_false_context_flags() {
        let request = request(&[
            ("0190c7b2-aaaa-7000-8000-000000000001", "Erster.", false),
            ("0190c7b2-aaaa-7000-8000-000000000002", "Zweiter.", true),
        ]);
        assert_eq!(
            wire_input(&request).unwrap(),
            r#"[{"id":"p1","text":"Erster."},{"id":"p2","text":"Zweiter.","context_only":true}]"#
        );
    }

    #[test]
    fn german_quotes_resolve_to_exact_byte_ranges_including_quotation_marks() {
        let text = "„Grüß dich, Jürgen“, sagte Käthe. „Wie geht’s dir heute?“";
        let request = request(&[("paragraph", text, false)]);
        let result = parse_and_resolve(
            r#"{"characters":[{"canonical_name":"Käthe","aliases":[],"confidence":0.9}],
               "dialogue":[
                 {"paragraph_id":"p1","quote_start":"Grüß dich","quote_end":"Jürgen","character":"Käthe","confidence":0.9},
                 {"paragraph_id":"p1","quote_start":"\"Wie geht's","quote_end":"dir heute?\"","character":"Käthe","confidence":0.8}
               ]}"#,
            &request,
        )
        .unwrap();

        assert_eq!(result.dialogue.len(), 2);
        assert_eq!(slice(&request, &result.dialogue[0]), "„Grüß dich, Jürgen“");
        assert_eq!(
            slice(&request, &result.dialogue[1]),
            "„Wie geht’s dir heute?“"
        );
    }

    #[test]
    fn repeated_quotes_map_to_successive_occurrences() {
        let text = "\"Yes,\" said Ann. \"Yes,\" said Bob.";
        let request = request(&[("paragraph", text, false)]);
        let result = parse_and_resolve(
            r#"{"characters":[],"dialogue":[
                 {"paragraph_id":"p1","quote_start":"Yes,","quote_end":"Yes,","character":"Ann","confidence":1},
                 {"paragraph_id":"p1","quote_start":"Yes,","quote_end":"Yes,","character":"Bob","confidence":1}
               ]}"#,
            &request,
        )
        .unwrap();

        assert_eq!(result.dialogue[0].start, 0);
        assert!(result.dialogue[1].start > result.dialogue[0].end);
        assert_eq!(slice(&request, &result.dialogue[1]), "\"Yes,\"");
    }

    #[test]
    fn whitespace_case_and_ellipsis_differences_are_tolerated() {
        let text = "Er rief:  »Warte…\u{00AD}  WARTE doch!«";
        let request = request(&[("paragraph", text, false)]);
        let result = parse_and_resolve(
            r#"{"characters":[],"dialogue":[{"paragraph_id":"p1","quote_start":"warte... warte","quote_end":"doch!","character":"Er","confidence":0.7}]}"#,
            &request,
        )
        .unwrap();

        assert_eq!(
            slice(&request, &result.dialogue[0]),
            "»Warte…\u{00AD}  WARTE doch!«"
        );
    }

    #[test]
    fn unlocatable_passages_and_unknown_paragraphs_are_dropped_not_fatal() {
        let request = request(&[("paragraph", "\"Hello,\" she said.", false)]);
        let result = parse_and_resolve(
            r#"{"characters":[{"canonical_name":"  ","aliases":[],"confidence":2}],"dialogue":[
                 {"paragraph_id":"p1","quote_start":"Goodbye","quote_end":"Goodbye","character":"Ann","confidence":1},
                 {"paragraph_id":"p9","quote_start":"Hello","quote_end":"Hello","character":"Ann","confidence":1},
                 {"paragraph_id":"p1","quote_start":"Hello","quote_end":"Hello","character":"Ann","confidence":7}
               ]}"#,
            &request,
        )
        .unwrap();

        assert!(result.characters.is_empty());
        assert_eq!(result.dialogue.len(), 1);
        assert!((result.dialogue[0].confidence - 1.0).abs() < f32::EPSILON);
        assert_eq!(slice(&request, &result.dialogue[0]), "\"Hello,\"");
    }

    #[test]
    fn full_text_and_legacy_offsets_remain_supported() {
        let request = request(&[("paragraph", "\"Hi there,\" said Tom.", false)]);
        let result = parse_and_resolve(
            r#"{"characters":[],"dialogue":[
                 {"paragraph_id":"p1","text":"Hi there,","character":"Tom","confidence":1},
                 {"paragraph_id":"paragraph","start":1,"end":9,"character":"Tom","confidence":1}
               ]}"#,
            &request,
        )
        .unwrap();

        assert_eq!(slice(&request, &result.dialogue[0]), "\"Hi there,\"");
        assert_eq!(slice(&request, &result.dialogue[1]), "Hi there");
    }

    #[test]
    fn json_is_extracted_from_fences_and_surrounding_prose() {
        assert_eq!(extract_json_object("```json\n{\"a\":1}\n```"), "{\"a\":1}");
        assert_eq!(
            extract_json_object("Here is the result:\n{\"a\":{\"b\":1}}\nDone."),
            "{\"a\":{\"b\":1}}"
        );
        assert_eq!(extract_json_object("Sure: {\"a\":"), "{\"a\":");
    }

    #[test]
    fn truncated_output_is_classified_without_retaining_content() {
        let request = request(&[("paragraph", "text", false)]);
        let error = parse_and_resolve(
            r#"{"characters":[{"canonical_name":"private source text"#,
            &request,
        )
        .unwrap_err();

        assert!(matches!(error, ProviderError::OutputTruncated));
        assert!(!error.to_string().contains("private source text"));
    }

    #[test]
    fn closing_anchor_before_the_opening_anchor_is_not_selected() {
        let text = "end. \"Begin here and end.\"";
        assert_eq!(locate_passage(text, "Begin here", "end.", 0), Some((6, 25)));
    }
}
