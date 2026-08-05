use std::collections::BTreeMap;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::{
    ArtifactId, ChapterId, ExportProfileId, JobId, JobUnitId, ParagraphId, PerformanceSettings,
    ProjectId, ProofExportSnapshotId, SegmentId, SegmentTakeId, Speaker, TimingSettings,
};

#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum SegmentReviewState {
    #[default]
    Unreviewed,
    Flagged,
    Approved,
    Locked,
}

impl SegmentReviewState {
    #[must_use]
    pub const fn is_accepted(self) -> bool {
        matches!(self, Self::Approved | Self::Locked)
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ProductionSegmentSource {
    EpubRange,
    OpeningCredit,
    ClosingCredit,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct ProductionSegment {
    pub id: SegmentId,
    pub project_id: ProjectId,
    pub chapter_id: Option<ChapterId>,
    pub paragraph_id: Option<ParagraphId>,
    pub source: ProductionSegmentSource,
    pub stable_key: String,
    pub ordinal: u32,
    pub source_content_hash: String,
    pub byte_start: Option<u64>,
    pub byte_end: Option<u64>,
    pub speaker: Speaker,
    pub original_text: String,
    pub narration_text_override: Option<String>,
    pub effective_text: String,
    pub context_before: Option<String>,
    pub context_after: Option<String>,
    /// Per-segment values layered on top of the assigned speaker defaults.
    #[serde(default)]
    pub performance_override: PerformanceSettings,
    /// Local assembly pauses. These never enter a provider request or synthesis cache key.
    #[serde(default)]
    pub timing_override: TimingSettings,
    pub expected_input_hash: String,
    pub review_state: SegmentReviewState,
    pub active: bool,
    pub revision: u64,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ProofingPlanStatus {
    #[default]
    Ready,
    Dirty,
    Incomplete,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct ProofingPlan {
    pub project_id: ProjectId,
    pub source_conversion_job_id: JobId,
    pub plan_revision: u64,
    pub plan_hash: String,
    pub status: ProofingPlanStatus,
    #[serde(default)]
    pub dirty_reasons: Vec<String>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum TakeFindingSeverity {
    Warning,
    Error,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct TakeFinding {
    pub code: String,
    pub severity: TakeFindingSeverity,
    pub message: String,
    pub start_ms: Option<u64>,
    pub end_ms: Option<u64>,
    pub actual: Option<f64>,
    pub expected: Option<String>,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct SegmentTake {
    pub id: SegmentTakeId,
    pub segment_id: SegmentId,
    pub artifact_id: ArtifactId,
    pub ordinal: u32,
    pub source_job_id: JobId,
    pub source_job_unit_id: JobUnitId,
    pub semantic_input_hash: String,
    pub duration_ms: u64,
    pub provider_profile_id: Option<crate::ProviderProfileId>,
    pub model: Option<String>,
    pub voice_profile_id: Option<crate::VoiceProfileId>,
    pub dictionary_revision_hash: String,
    pub normalization_version: String,
    #[serde(default)]
    pub synthesis_provenance: BTreeMap<String, serde_json::Value>,
    #[serde(default)]
    pub findings: Vec<TakeFinding>,
    pub created_at: DateTime<Utc>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct SegmentSelection {
    pub segment_id: SegmentId,
    pub take_id: SegmentTakeId,
    pub selected_at: DateTime<Utc>,
    pub revision: u64,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct ProofExportSelection {
    pub segment_id: SegmentId,
    pub take_id: SegmentTakeId,
    pub artifact_id: ArtifactId,
}

/// Immutable audit record of the takes and proofing revision used for a local re-export.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct ProofExportSnapshot {
    pub id: ProofExportSnapshotId,
    pub project_id: ProjectId,
    pub job_id: JobId,
    pub export_profile_id: ExportProfileId,
    pub plan_revision: u64,
    pub plan_hash: String,
    pub selections: Vec<ProofExportSelection>,
    pub created_at: DateTime<Utc>,
}
