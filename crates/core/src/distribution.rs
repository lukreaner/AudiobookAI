use std::collections::BTreeMap;

use chrono::{DateTime, NaiveDate, Utc};
use serde::{Deserialize, Serialize};

use crate::{
    ArtifactId, ExportPackageId, JobId, ProductionSegmentSource, ProjectId, QualityReportId,
    SegmentId, SegmentTakeId,
};

#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum DistributionTarget {
    #[default]
    GenericM4b,
    Acx,
    SpotifyForAuthors,
    GooglePlay,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct DistributionPolicyRef {
    pub target: DistributionTarget,
    pub policy_version: String,
    pub effective_date: NaiveDate,
    pub source_urls: Vec<String>,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum QualityFindingStatus {
    Pass,
    Warning,
    Fail,
    Manual,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct QualityFinding {
    pub code: String,
    pub status: QualityFindingStatus,
    pub scope: String,
    pub message: String,
    pub actual: Option<serde_json::Value>,
    pub expected: Option<serde_json::Value>,
    pub start_ms: Option<u64>,
    pub end_ms: Option<u64>,
    pub remediation: Option<String>,
    pub acknowledged: bool,
}

#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
pub struct ManualAttestations {
    pub acx_external_authorization: Option<DateTime<Utc>>,
    pub acx_authorization_reference: Option<String>,
    pub spotify_digital_voice_disclosure: Option<DateTime<Utc>>,
    pub rights_and_eligibility_confirmed: Option<DateTime<Utc>>,
}

#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
pub struct DistributionMetadata {
    pub subtitle: Option<String>,
    pub authors: Vec<String>,
    pub narrators: Vec<String>,
    pub publisher: Option<String>,
    pub imprint: Option<String>,
    pub description: Option<String>,
    pub language: Option<String>,
    pub abridged: Option<bool>,
    pub identifier: Option<String>,
    pub identifier_kind: Option<String>,
    pub source_rights: Option<String>,
    pub audio_rights: Option<String>,
    pub release_date: Option<NaiveDate>,
    pub cover_artifact_id: Option<ArtifactId>,
    pub opening_credit_segment_ids: Vec<crate::SegmentId>,
    pub closing_credit_segment_ids: Vec<crate::SegmentId>,
    pub sample_segment_ids: Vec<crate::SegmentId>,
    pub attestations: ManualAttestations,
}

/// Frozen proofing identity used by distribution metadata during one quality-control run.
///
/// Keeping both hashes makes a stored report independently explain whether its selected take was
/// current when the report ran, even if the project's active proofing plan changes later.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct DistributionSegmentEvidence {
    pub segment_id: SegmentId,
    pub source: Option<ProductionSegmentSource>,
    pub active: bool,
    /// Revision of the proofing segment evaluated by this run.
    #[serde(default)]
    pub segment_revision: Option<u64>,
    /// Identity and status of the active proofing plan evaluated by this run. Global plan
    /// revisions are intentionally excluded: an unrelated segment edit must not stale this
    /// segment's distribution evidence.
    #[serde(default)]
    pub plan_source_conversion_job_id: Option<JobId>,
    #[serde(default)]
    pub plan_status: Option<crate::ProofingPlanStatus>,
    pub selection_revision: Option<u64>,
    pub take_id: Option<SegmentTakeId>,
    pub take_artifact_id: Option<ArtifactId>,
    pub expected_input_hash: Option<String>,
    pub selected_take_input_hash: Option<String>,
    pub current_input_hash: Option<String>,
    pub current: bool,
    pub problem: Option<String>,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct QualityReport {
    pub id: QualityReportId,
    pub package_id: ExportPackageId,
    pub policy: DistributionPolicyRef,
    /// BLAKE3 digest of the exact machine-readable policy snapshot evaluated by this run.
    #[serde(default)]
    pub policy_digest: String,
    /// Exact policy rules shown to the user and evaluated by this run.
    #[serde(default)]
    pub policy_snapshot: Option<serde_json::Value>,
    /// Revision of the distribution metadata used by this run. Older stored reports default to
    /// zero because metadata provenance was added after the initial report format.
    #[serde(default)]
    pub metadata_revision: u64,
    /// BLAKE3 digest of the project title plus the complete distribution-metadata snapshot.
    #[serde(default)]
    pub metadata_digest: String,
    /// Exact distribution metadata evaluated by this run.
    #[serde(default)]
    pub metadata_snapshot: Option<DistributionMetadata>,
    /// Project title evaluated alongside the distribution metadata.
    #[serde(default)]
    pub project_title: Option<String>,
    /// BLAKE3 digest of the immutable package inputs, excluding its latest-report pointer.
    #[serde(default)]
    pub package_digest: String,
    /// Exact package inputs evaluated by this run.
    #[serde(default)]
    pub package_snapshot: Option<ExportPackage>,
    /// Unique export manifest derived from the package's completed export job.
    #[serde(default)]
    pub export_manifest_artifact_id: Option<ArtifactId>,
    /// Active proofing segments and selected takes referenced by credits or sample metadata.
    #[serde(default)]
    pub segment_evidence: Vec<DistributionSegmentEvidence>,
    pub technical_ready: bool,
    pub submission_ready: bool,
    pub findings: Vec<QualityFinding>,
    pub analyzer_version: String,
    pub ffmpeg_version: String,
    pub ffmpeg_build_fingerprint: String,
    /// BLAKE3 hashes for every upload, review, derived manifest, cover, and referenced selected
    /// take artifact that influenced this report.
    #[serde(default)]
    pub file_hashes: BTreeMap<String, String>,
    /// BLAKE3 digest of the proofing evidence, artifact identities, package job state, and exact
    /// file hashes that influenced this report. Reports created before this provenance field was
    /// introduced are deliberately treated as stale.
    #[serde(default)]
    pub input_digest: String,
    pub generated_at: DateTime<Utc>,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct ExportPackage {
    pub id: ExportPackageId,
    pub project_id: ProjectId,
    pub job_id: JobId,
    pub target: DistributionTarget,
    pub output_directory: String,
    pub upload_artifact_ids: Vec<ArtifactId>,
    pub review_artifact_ids: Vec<ArtifactId>,
    pub quality_report_id: Option<QualityReportId>,
    pub created_at: DateTime<Utc>,
}
