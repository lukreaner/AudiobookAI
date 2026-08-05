use std::collections::BTreeMap;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::{
    ArtifactId, CharacterId, DictionaryId, DictionaryRuleId, ProjectId, ProviderProfileId, Speaker,
    Validate, ValidationIssue, VoiceAssignmentId, VoiceProfileId, error::require_non_empty,
};

/// Provider-neutral controls that can affect the generated performance of one speech segment.
///
/// Every field is optional so omission continues to mean "use the provider/model default". A
/// provider adapter must validate non-empty settings against a model-bound capability descriptor
/// before serializing them; it must never forward this structure as an open-ended options object.
#[derive(Clone, Debug, Default, Deserialize, PartialEq, Serialize)]
pub struct PerformanceSettings {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub speed: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pitch: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stability: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub similarity: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub style: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub speaker_boost: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub delivery_cue: Option<DeliveryCue>,
}

impl PerformanceSettings {
    /// Broad provider-neutral bounds. Model descriptors may narrow these ranges further.
    pub const MIN_SPEED: f64 = 0.25;
    pub const MAX_SPEED: f64 = 4.0;
    pub const MIN_PITCH: f64 = 0.25;
    pub const MAX_PITCH: f64 = 4.0;

    #[must_use]
    pub const fn is_empty(&self) -> bool {
        self.speed.is_none()
            && self.pitch.is_none()
            && self.stability.is_none()
            && self.similarity.is_none()
            && self.style.is_none()
            && self.speaker_boost.is_none()
            && self.delivery_cue.is_none()
    }

    /// Applies the explicitly set values from `overrides` on top of these defaults.
    #[must_use]
    pub fn overlay(&self, overrides: &Self) -> Self {
        Self {
            speed: overrides.speed.or(self.speed),
            pitch: overrides.pitch.or(self.pitch),
            stability: overrides.stability.or(self.stability),
            similarity: overrides.similarity.or(self.similarity),
            style: overrides.style.or(self.style),
            speaker_boost: overrides.speaker_boost.or(self.speaker_boost),
            delivery_cue: overrides.delivery_cue.or(self.delivery_cue),
        }
    }
}

impl Validate for PerformanceSettings {
    fn validation_issues(&self) -> Vec<ValidationIssue> {
        let mut issues = Vec::new();
        require_optional_number(
            &mut issues,
            "speed",
            self.speed,
            Self::MIN_SPEED,
            Self::MAX_SPEED,
        );
        require_optional_number(
            &mut issues,
            "pitch",
            self.pitch,
            Self::MIN_PITCH,
            Self::MAX_PITCH,
        );
        for (path, value) in [
            ("stability", self.stability),
            ("similarity", self.similarity),
            ("style", self.style),
        ] {
            require_optional_number(&mut issues, path, value, 0.0, 1.0);
        }
        issues
    }
}

/// A closed, provider-neutral delivery direction.
///
/// The allowlist prevents arbitrary prompt material from crossing a provider boundary. Adapters
/// may map a cue only to a documented instruction field and must not add it to spoken book text.
#[derive(Clone, Copy, Debug, Deserialize, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum DeliveryCue {
    Whisper,
    Shout,
    Sarcastic,
    Curious,
    Excited,
    Crying,
    Mischievous,
}

impl DeliveryCue {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Whisper => "whisper",
            Self::Shout => "shout",
            Self::Sarcastic => "sarcastic",
            Self::Curious => "curious",
            Self::Excited => "excited",
            Self::Crying => "crying",
            Self::Mischievous => "mischievous",
        }
    }
}

/// Pauses owned by the local assembly pipeline rather than a TTS provider.
///
/// These values intentionally do not appear in provider synthesis requests or segment-audio
/// cache fingerprints. Changing timing must reuse the synthesized audio and only rebuild the
/// affected assembly output.
#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
pub struct TimingSettings {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pause_before_ms: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pause_after_ms: Option<u32>,
}

impl TimingSettings {
    pub const MAX_PAUSE_MS: u32 = 5_000;

    #[must_use]
    pub const fn is_empty(&self) -> bool {
        self.pause_before_ms.is_none() && self.pause_after_ms.is_none()
    }
}

impl Validate for TimingSettings {
    fn validation_issues(&self) -> Vec<ValidationIssue> {
        let mut issues = Vec::new();
        for (path, value) in [
            ("pause_before_ms", self.pause_before_ms),
            ("pause_after_ms", self.pause_after_ms),
        ] {
            if value.is_some_and(|value| value > Self::MAX_PAUSE_MS) {
                issues.push(ValidationIssue::new(
                    path,
                    "out_of_range",
                    "segment pause must not exceed 5000 milliseconds",
                ));
            }
        }
        issues
    }
}

fn require_optional_number(
    issues: &mut Vec<ValidationIssue>,
    path: &str,
    value: Option<f64>,
    minimum: f64,
    maximum: f64,
) {
    if value.is_some_and(|value| !value.is_finite() || !(minimum..=maximum).contains(&value)) {
        issues.push(ValidationIssue::new(
            path,
            "out_of_range",
            format!("{path} must be finite and between {minimum} and {maximum}"),
        ));
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum VoiceOrigin {
    ProviderCatalog,
    LocalReference,
    ProviderClone,
    NativeSystem,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum VoiceOwnership {
    Provider,
    User,
    AudiobookAi,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct VoiceProfile {
    pub id: VoiceProfileId,
    pub provider_profile_id: ProviderProfileId,
    pub provider_voice_id: Option<String>,
    pub name: String,
    pub origin: VoiceOrigin,
    pub ownership: VoiceOwnership,
    pub reference_audio_artifact_ids: Vec<ArtifactId>,
    pub language: Option<String>,
    pub model: Option<String>,
    #[serde(default)]
    pub settings: BTreeMap<String, serde_json::Value>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

impl VoiceProfile {
    #[must_use]
    pub fn requires_delete_confirmation(&self) -> bool {
        self.origin == VoiceOrigin::ProviderClone && self.ownership == VoiceOwnership::AudiobookAi
    }

    #[must_use]
    pub fn remote_deletion_allowed(&self) -> bool {
        self.requires_delete_confirmation()
    }
}

impl Validate for VoiceProfile {
    fn validation_issues(&self) -> Vec<ValidationIssue> {
        let mut issues = Vec::new();
        require_non_empty(&mut issues, "name", &self.name);
        if matches!(
            self.origin,
            VoiceOrigin::ProviderCatalog | VoiceOrigin::ProviderClone
        ) && self.provider_voice_id.as_deref().is_none_or(str::is_empty)
        {
            issues.push(ValidationIssue::new(
                "provider_voice_id",
                "required",
                "provider catalog and clone voices require a provider voice id",
            ));
        }
        if self.origin == VoiceOrigin::LocalReference
            && self.reference_audio_artifact_ids.is_empty()
        {
            issues.push(ValidationIssue::new(
                "reference_audio_artifact_ids",
                "required",
                "a local reference voice requires at least one audio artifact",
            ));
        }
        issues
    }
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct VoiceAssignment {
    pub id: VoiceAssignmentId,
    pub project_id: ProjectId,
    pub speaker: Speaker,
    pub voice_profile_id: VoiceProfileId,
    pub provider_profile_id: ProviderProfileId,
    pub model: Option<String>,
    #[serde(default)]
    pub performance: PerformanceSettings,
    #[serde(default)]
    pub timing: TimingSettings,
    #[serde(default)]
    pub settings: BTreeMap<String, serde_json::Value>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum DictionaryScope {
    Global,
    Project,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct PronunciationDictionary {
    pub id: DictionaryId,
    pub name: String,
    pub scope: DictionaryScope,
    pub project_id: Option<ProjectId>,
    pub enabled: bool,
    pub revision: u64,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

impl Validate for PronunciationDictionary {
    fn validation_issues(&self) -> Vec<ValidationIssue> {
        let mut issues = Vec::new();
        require_non_empty(&mut issues, "name", &self.name);
        match (self.scope, self.project_id) {
            (DictionaryScope::Global, Some(_)) => issues.push(ValidationIssue::new(
                "project_id",
                "must_be_omitted",
                "global dictionaries cannot be tied to a project",
            )),
            (DictionaryScope::Project, None) => issues.push(ValidationIssue::new(
                "project_id",
                "required",
                "project dictionaries require a project id",
            )),
            _ => {}
        }
        issues
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum DictionaryRuleKind {
    Literal,
    WholeWord,
    Regex,
    Alias,
    Phoneme,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum PhonemeAlphabet {
    Ipa,
    Xsampa,
    ProviderNative,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct DictionaryRule {
    pub id: DictionaryRuleId,
    pub dictionary_id: DictionaryId,
    pub ordinal: u32,
    pub kind: DictionaryRuleKind,
    pub pattern: String,
    pub replacement: String,
    pub case_sensitive: bool,
    pub language: Option<String>,
    pub character_id: Option<CharacterId>,
    pub phoneme_alphabet: Option<PhonemeAlphabet>,
    pub enabled: bool,
}

impl Validate for DictionaryRule {
    fn validation_issues(&self) -> Vec<ValidationIssue> {
        let mut issues = Vec::new();
        require_non_empty(&mut issues, "pattern", &self.pattern);
        require_non_empty(&mut issues, "replacement", &self.replacement);
        if self.kind == DictionaryRuleKind::Phoneme && self.phoneme_alphabet.is_none() {
            issues.push(ValidationIssue::new(
                "phoneme_alphabet",
                "required",
                "phoneme rules require an alphabet",
            ));
        }
        if self.kind != DictionaryRuleKind::Phoneme && self.phoneme_alphabet.is_some() {
            issues.push(ValidationIssue::new(
                "phoneme_alphabet",
                "must_be_omitted",
                "phoneme alphabet is only valid for phoneme rules",
            ));
        }
        issues
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct DictionaryConflict {
    pub first_rule_id: DictionaryRuleId,
    pub second_rule_id: DictionaryRuleId,
    pub sample: String,
    pub explanation: String,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct ProviderPronunciationPayload {
    pub transformed_text: String,
    pub ssml: Option<String>,
    pub provider_dictionary_ids: Vec<String>,
    pub applied_rule_ids: Vec<DictionaryRuleId>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn performance_settings_validate_provider_neutral_bounds() {
        let valid = PerformanceSettings {
            speed: Some(0.25),
            pitch: Some(4.0),
            stability: Some(0.0),
            similarity: Some(1.0),
            ..PerformanceSettings::default()
        };
        assert!(valid.validation_issues().is_empty());

        let invalid = PerformanceSettings {
            speed: Some(0.0),
            pitch: Some(f64::NAN),
            style: Some(1.01),
            ..PerformanceSettings::default()
        };
        let paths = invalid
            .validation_issues()
            .into_iter()
            .map(|issue| issue.path)
            .collect::<Vec<_>>();
        assert_eq!(paths, ["speed", "pitch", "style"]);
    }

    #[test]
    fn delivery_cue_is_a_closed_serializable_allowlist() {
        let cue = DeliveryCue::Mischievous;
        assert_eq!(cue.as_str(), "mischievous");
        let serialized = serde_json::to_value(cue).expect("cue serializes");
        assert_eq!(serialized, serde_json::json!("mischievous"));
        let round_trip: DeliveryCue = serde_json::from_value(serialized).expect("cue deserializes");
        assert_eq!(round_trip, cue);
        assert!(
            serde_json::from_value::<DeliveryCue>(serde_json::json!("arbitrary prompt")).is_err()
        );
    }

    #[test]
    fn timing_settings_are_local_bounded_metadata() {
        let settings = TimingSettings {
            pause_before_ms: Some(250),
            pause_after_ms: Some(TimingSettings::MAX_PAUSE_MS),
        };
        assert!(settings.validation_issues().is_empty());
        assert_eq!(
            serde_json::to_value(settings).expect("timing serializes"),
            serde_json::json!({
                "pause_before_ms": 250,
                "pause_after_ms": 5_000
            })
        );
        assert!(
            !TimingSettings {
                pause_before_ms: None,
                pause_after_ms: Some(TimingSettings::MAX_PAUSE_MS + 1),
            }
            .validation_issues()
            .is_empty()
        );
    }
}
