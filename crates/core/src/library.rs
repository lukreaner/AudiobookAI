use std::collections::BTreeMap;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::{
    ArtifactId, BookId, ChapterId, CharacterId, DetectionRunId, FileFingerprint, ParagraphId,
    ProjectId, SegmentId, SpeakerOverrideId, Validate, ValidationIssue, error::require_non_empty,
};

#[derive(Clone, Debug, Default, Deserialize, PartialEq, Serialize)]
pub struct BookMetadata {
    pub title: String,
    #[serde(default)]
    pub authors: Vec<String>,
    pub narrator: Option<String>,
    pub publisher: Option<String>,
    pub description: Option<String>,
    pub language: Option<String>,
    pub identifier: Option<String>,
    pub series: Option<SeriesMetadata>,
    pub cover_artifact_id: Option<ArtifactId>,
    #[serde(default)]
    pub extra: BTreeMap<String, String>,
}

impl Validate for BookMetadata {
    fn validation_issues(&self) -> Vec<ValidationIssue> {
        let mut issues = Vec::new();
        require_non_empty(&mut issues, "title", &self.title);
        if let Some(series) = &self.series {
            for issue in series.validation_issues() {
                issues.push(ValidationIssue::new(
                    format!("series.{}", issue.path),
                    issue.code,
                    issue.message,
                ));
            }
        }
        issues
    }
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct SeriesMetadata {
    pub name: String,
    pub position: Option<f32>,
    pub source: SeriesMetadataSource,
}

impl Validate for SeriesMetadata {
    fn validation_issues(&self) -> Vec<ValidationIssue> {
        let mut issues = Vec::new();
        require_non_empty(&mut issues, "name", &self.name);
        if self
            .position
            .is_some_and(|position| !position.is_finite() || position <= 0.0)
        {
            issues.push(ValidationIssue::new(
                "position",
                "out_of_range",
                "series position must be finite and greater than zero",
            ));
        }
        issues
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum SeriesMetadataSource {
    Epub3,
    Calibre,
    Epub2Fallback,
    User,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct CloudConsent {
    pub book_text: bool,
    pub reference_audio: bool,
    pub granted_at: Option<DateTime<Utc>>,
    pub policy_revision: String,
}

impl Default for CloudConsent {
    fn default() -> Self {
        Self {
            book_text: false,
            reference_audio: false,
            granted_at: None,
            policy_revision: "1".to_owned(),
        }
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct ReliabilityPolicy {
    pub max_transient_retries: u16,
    pub base_backoff_ms: u64,
    pub max_backoff_ms: u64,
    pub retry_possible_duplicate_charge: bool,
}

impl Default for ReliabilityPolicy {
    fn default() -> Self {
        Self {
            max_transient_retries: 3,
            base_backoff_ms: 500,
            max_backoff_ms: 30_000,
            retry_possible_duplicate_charge: false,
        }
    }
}

impl Validate for ReliabilityPolicy {
    fn validation_issues(&self) -> Vec<ValidationIssue> {
        let mut issues = Vec::new();
        if self.base_backoff_ms == 0 {
            issues.push(ValidationIssue::new(
                "base_backoff_ms",
                "out_of_range",
                "base backoff must be greater than zero",
            ));
        }
        if self.max_backoff_ms < self.base_backoff_ms {
            issues.push(ValidationIssue::new(
                "max_backoff_ms",
                "out_of_range",
                "maximum backoff must be at least the base backoff",
            ));
        }
        issues
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct ProjectSettings {
    pub global_chapter_concurrency: u16,
    pub reliability: ReliabilityPolicy,
    pub output_name_template: String,
}

impl Default for ProjectSettings {
    fn default() -> Self {
        Self {
            global_chapter_concurrency: 4,
            reliability: ReliabilityPolicy::default(),
            output_name_template: "{title}".to_owned(),
        }
    }
}

impl Validate for ProjectSettings {
    fn validation_issues(&self) -> Vec<ValidationIssue> {
        let mut issues = self.reliability.validation_issues();
        if self.global_chapter_concurrency == 0 {
            issues.push(ValidationIssue::new(
                "global_chapter_concurrency",
                "out_of_range",
                "chapter concurrency must be greater than zero",
            ));
        }
        require_non_empty(
            &mut issues,
            "output_name_template",
            &self.output_name_template,
        );
        issues
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ProjectStatus {
    Importing,
    Draft,
    NeedsCharacterReview,
    Ready,
    Archived,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct Book {
    pub id: BookId,
    pub managed_epub_path: String,
    pub original_filename: String,
    pub source_fingerprint: FileFingerprint,
    pub epub_version: Option<String>,
    pub metadata: BookMetadata,
    pub imported_at: DateTime<Utc>,
}

impl Validate for Book {
    fn validation_issues(&self) -> Vec<ValidationIssue> {
        let mut issues = self.metadata.validation_issues();
        require_non_empty(&mut issues, "managed_epub_path", &self.managed_epub_path);
        require_non_empty(&mut issues, "original_filename", &self.original_filename);
        issues
    }
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct Project {
    pub id: ProjectId,
    pub book_id: BookId,
    pub name: String,
    pub status: ProjectStatus,
    pub metadata: BookMetadata,
    pub cloud_consent: CloudConsent,
    pub settings: ProjectSettings,
    pub character_reviewed_at: Option<DateTime<Utc>>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

impl Validate for Project {
    fn validation_issues(&self) -> Vec<ValidationIssue> {
        let mut issues = self.metadata.validation_issues();
        require_non_empty(&mut issues, "name", &self.name);
        for issue in self.settings.validation_issues() {
            issues.push(ValidationIssue::new(
                format!("settings.{}", issue.path),
                issue.code,
                issue.message,
            ));
        }
        if self.updated_at < self.created_at {
            issues.push(ValidationIssue::new(
                "updated_at",
                "invalid_time_order",
                "updated_at must not precede created_at",
            ));
        }
        issues
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ParagraphKind {
    Heading,
    Prose,
    ListItem,
    Quote,
    Verse,
    ImageDescription,
    Other,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct Chapter {
    pub id: ChapterId,
    pub book_id: BookId,
    pub ordinal: u32,
    pub title: String,
    pub source_href: String,
    pub selected: bool,
    pub text_hash: String,
    pub character_count: u64,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct Paragraph {
    pub id: ParagraphId,
    pub chapter_id: ChapterId,
    pub ordinal: u32,
    pub kind: ParagraphKind,
    pub text: String,
    pub source_start: u64,
    pub source_end: u64,
    pub content_hash: String,
}

impl Validate for Paragraph {
    fn validation_issues(&self) -> Vec<ValidationIssue> {
        let mut issues = Vec::new();
        require_non_empty(&mut issues, "text", &self.text);
        if self.source_end < self.source_start {
            issues.push(ValidationIssue::new(
                "source_end",
                "invalid_range",
                "source_end must be at or after source_start",
            ));
        }
        issues
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(tag = "kind", content = "id", rename_all = "snake_case")]
pub enum Speaker {
    Narrator,
    Character(CharacterId),
    Named(String),
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum SegmentState {
    Pending,
    Synthesizing,
    Cached,
    Failed,
    Invalidated,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct Segment {
    pub id: SegmentId,
    pub chapter_id: ChapterId,
    pub paragraph_id: ParagraphId,
    pub ordinal: u32,
    pub text: String,
    pub text_hash: String,
    pub speaker: Speaker,
    pub context_before: Option<String>,
    pub context_after: Option<String>,
    pub state: SegmentState,
    pub cached_artifact_id: Option<ArtifactId>,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct Character {
    pub id: CharacterId,
    pub project_id: ProjectId,
    pub canonical_name: String,
    #[serde(default)]
    pub aliases: Vec<String>,
    pub description: Option<String>,
    pub confidence: Option<f32>,
    pub detection_run_id: Option<DetectionRunId>,
    pub manually_created: bool,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

impl Validate for Character {
    fn validation_issues(&self) -> Vec<ValidationIssue> {
        let mut issues = Vec::new();
        require_non_empty(&mut issues, "canonical_name", &self.canonical_name);
        if self
            .confidence
            .is_some_and(|value| !value.is_finite() || !(0.0..=1.0).contains(&value))
        {
            issues.push(ValidationIssue::new(
                "confidence",
                "out_of_range",
                "confidence must be between zero and one",
            ));
        }
        issues
    }
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct DialogueSpan {
    pub paragraph_id: ParagraphId,
    pub character_id: CharacterId,
    pub byte_start: u64,
    pub byte_end: u64,
    pub confidence: f32,
    pub evidence: Option<String>,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum DetectionRunStatus {
    Pending,
    Running,
    NeedsRepair,
    Completed,
    Failed,
    Cancelled,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct CharacterDetectionRun {
    pub id: DetectionRunId,
    pub project_id: ProjectId,
    pub provider_profile_id: crate::ProviderProfileId,
    pub model: String,
    pub status: DetectionRunStatus,
    pub paragraph_hashes: Vec<String>,
    pub repair_attempted: bool,
    pub created_at: DateTime<Utc>,
    pub completed_at: Option<DateTime<Utc>>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct SpeakerOverride {
    pub id: SpeakerOverrideId,
    pub project_id: ProjectId,
    pub paragraph_id: ParagraphId,
    pub source_content_hash: String,
    pub byte_start: u64,
    pub byte_end: u64,
    pub speaker: Speaker,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}
