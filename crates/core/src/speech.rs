use std::collections::BTreeMap;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::{
    ArtifactId, CharacterId, DictionaryId, DictionaryRuleId, ProjectId, ProviderProfileId, Speaker,
    Validate, ValidationIssue, VoiceAssignmentId, VoiceProfileId, error::require_non_empty,
};

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
