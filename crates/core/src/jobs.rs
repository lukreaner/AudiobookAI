use std::collections::BTreeMap;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::{
    ArtifactId, AttemptId, ChapterId, DomainError, ExportProfileId, FileFingerprint, JobId,
    JobUnitId, Money, ProjectId, ProviderProfileId, ReservationId, SegmentId, Validate,
    ValidationIssue,
};

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum PreflightOperation {
    Preview,
    Estimate,
    DryRun,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum PreflightSeverity {
    Information,
    Warning,
    Error,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct PreflightFinding {
    pub code: String,
    pub severity: PreflightSeverity,
    pub subject: Option<String>,
    pub message: String,
    pub remediation: Option<String>,
}

#[derive(Clone, Debug, Default, Deserialize, PartialEq, Serialize)]
pub struct ConversionEstimate {
    pub selected_chapters: u32,
    pub characters: u64,
    pub estimated_input_tokens: Option<u64>,
    pub estimated_output_tokens: Option<u64>,
    pub estimated_audio_seconds: Option<f64>,
    pub estimated_provider_credits: Option<i64>,
    pub estimated_costs: Vec<Money>,
    pub estimated_disk_bytes: Option<u64>,
    pub earliest_completion_seconds: Option<u64>,
    pub latest_completion_seconds: Option<u64>,
    #[serde(default)]
    pub rate_card_provenance: BTreeMap<String, String>,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct PreflightReport {
    pub operation: PreflightOperation,
    pub project_id: ProjectId,
    pub generated_at: DateTime<Utc>,
    pub estimate: Option<ConversionEstimate>,
    pub findings: Vec<PreflightFinding>,
    pub preview_artifact_id: Option<ArtifactId>,
    pub potentially_billable: bool,
}

impl PreflightReport {
    #[must_use]
    pub fn can_start(&self) -> bool {
        !self
            .findings
            .iter()
            .any(|finding| finding.severity == PreflightSeverity::Error)
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum JobKind {
    CharacterDetection,
    Preview,
    Conversion,
    Export,
    CacheCleanup,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum JobState {
    Queued,
    Running,
    Pausing,
    Paused,
    Cancelling,
    Cancelled,
    Failed,
    Completed,
}

impl JobState {
    #[must_use]
    pub const fn is_terminal(self) -> bool {
        matches!(self, Self::Cancelled | Self::Failed | Self::Completed)
    }

    #[must_use]
    pub const fn can_transition_to(self, next: Self) -> bool {
        matches!(
            (self, next),
            (
                Self::Queued,
                Self::Running | Self::Cancelling | Self::Cancelled | Self::Failed
            ) | (
                Self::Running,
                Self::Pausing | Self::Cancelling | Self::Failed | Self::Completed
            ) | (
                Self::Pausing,
                Self::Paused | Self::Cancelling | Self::Failed
            ) | (
                Self::Paused,
                Self::Queued | Self::Running | Self::Cancelling | Self::Cancelled
            ) | (Self::Cancelling, Self::Cancelled | Self::Failed)
                | (Self::Failed, Self::Queued)
        )
    }
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct Job {
    pub id: JobId,
    pub project_id: ProjectId,
    pub kind: JobKind,
    pub state: JobState,
    pub export_profile_id: Option<ExportProfileId>,
    pub reservation_id: Option<ReservationId>,
    pub progress_completed: u64,
    pub progress_total: u64,
    pub status_message: Option<String>,
    pub allow_budget_override: bool,
    pub created_at: DateTime<Utc>,
    pub started_at: Option<DateTime<Utc>>,
    pub finished_at: Option<DateTime<Utc>>,
    pub updated_at: DateTime<Utc>,
    pub revision: u64,
}

impl Job {
    /// Moves this job through its explicit lifecycle state machine.
    ///
    /// # Errors
    ///
    /// Returns [`DomainError::InvalidTransition`] when `next` cannot follow the
    /// current state.
    pub fn transition(&mut self, next: JobState, now: DateTime<Utc>) -> Result<(), DomainError> {
        if !self.state.can_transition_to(next) {
            return Err(DomainError::InvalidTransition {
                entity: "job".to_owned(),
                from: format!("{:?}", self.state).to_lowercase(),
                to: format!("{next:?}").to_lowercase(),
            });
        }
        if next == JobState::Running && self.started_at.is_none() {
            self.started_at = Some(now);
        }
        if next.is_terminal() {
            self.finished_at = Some(now);
        }
        self.state = next;
        self.updated_at = now;
        Ok(())
    }
}

impl Validate for Job {
    fn validation_issues(&self) -> Vec<ValidationIssue> {
        let mut issues = Vec::new();
        if self.progress_completed > self.progress_total {
            issues.push(ValidationIssue::new(
                "progress_completed",
                "out_of_range",
                "completed progress cannot exceed total progress",
            ));
        }
        if self.state.is_terminal() && self.finished_at.is_none() {
            issues.push(ValidationIssue::new(
                "finished_at",
                "required",
                "terminal jobs require a finish time",
            ));
        }
        issues
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum JobUnitKind {
    DetectionBatch,
    SynthesisSegment,
    ChapterAssembly,
    MusicMix,
    Normalization,
    FinalExport,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum JobUnitState {
    Blocked,
    Ready,
    Running,
    Retrying,
    Paused,
    Cancelled,
    Failed,
    Completed,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct JobUnit {
    pub id: JobUnitId,
    pub job_id: JobId,
    pub kind: JobUnitKind,
    pub state: JobUnitState,
    pub chapter_id: Option<ChapterId>,
    pub segment_id: Option<SegmentId>,
    pub provider_profile_id: Option<ProviderProfileId>,
    pub dependencies: Vec<JobUnitId>,
    pub attempt_count: u16,
    pub next_attempt_at: Option<DateTime<Utc>>,
    pub output_artifact_id: Option<ArtifactId>,
    #[serde(default)]
    pub payload: BTreeMap<String, serde_json::Value>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum FailureClass {
    Transport,
    RateLimit,
    ProviderServer,
    Authentication,
    Validation,
    CapabilityDrift,
    Cancelled,
    TimeoutBeforeDispatch,
    TimeoutAfterDispatch,
    MediaProcessing,
    Internal,
}

impl FailureClass {
    #[must_use]
    pub const fn is_transient(self) -> bool {
        matches!(
            self,
            Self::Transport | Self::RateLimit | Self::ProviderServer | Self::TimeoutBeforeDispatch
        )
    }

    #[must_use]
    pub const fn may_have_charged(self) -> bool {
        matches!(self, Self::TimeoutAfterDispatch)
    }
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct JobAttempt {
    pub id: AttemptId,
    pub job_unit_id: JobUnitId,
    pub ordinal: u16,
    pub started_at: DateTime<Utc>,
    pub finished_at: Option<DateTime<Utc>>,
    pub failure_class: Option<FailureClass>,
    pub error_code: Option<String>,
    pub redacted_error: Option<String>,
    pub provider_request_id: Option<String>,
    pub uncertain_charge: bool,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ArtifactKind {
    ImportedEpub,
    Cover,
    ReferenceAudio,
    Preview,
    SegmentAudio,
    ChapterMaster,
    MixedMaster,
    Export,
    ExportManifest,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct Artifact {
    pub id: ArtifactId,
    pub kind: ArtifactKind,
    pub path: String,
    pub fingerprint: FileFingerprint,
    pub media_type: Option<String>,
    pub duration_ms: Option<u64>,
    pub cache_key: Option<String>,
    pub pinned_by_job_id: Option<JobId>,
    pub created_at: DateTime<Utc>,
    pub last_accessed_at: DateTime<Utc>,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct CacheManifest {
    pub schema_version: u32,
    pub cache_key: String,
    pub artifact_id: ArtifactId,
    pub text_hash: String,
    pub context_hash: String,
    pub provider_profile_id: ProviderProfileId,
    pub provider_endpoint_fingerprint: String,
    pub provider_version: Option<String>,
    pub model: Option<String>,
    pub voice_fingerprint: String,
    pub settings_hash: String,
    pub dictionary_revision_hash: String,
    pub normalization_version: String,
    pub raw_request_provenance: serde_json::Value,
    pub created_at: DateTime<Utc>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use uuid::Uuid;

    fn job() -> Job {
        let now = Utc::now();
        Job {
            id: JobId::from_uuid(Uuid::new_v4()),
            project_id: ProjectId::from_uuid(Uuid::new_v4()),
            kind: JobKind::Conversion,
            state: JobState::Queued,
            export_profile_id: None,
            reservation_id: None,
            progress_completed: 0,
            progress_total: 1,
            status_message: None,
            allow_budget_override: false,
            created_at: now,
            started_at: None,
            finished_at: None,
            updated_at: now,
            revision: 0,
        }
    }

    #[test]
    fn job_state_machine_rejects_skipping_to_completed() {
        let mut job = job();
        assert!(job.transition(JobState::Completed, Utc::now()).is_err());
    }

    #[test]
    fn job_state_machine_tracks_start_and_finish() {
        let mut job = job();
        job.transition(JobState::Running, Utc::now())
            .expect("start transition");
        job.transition(JobState::Completed, Utc::now())
            .expect("finish transition");
        assert!(job.started_at.is_some());
        assert!(job.finished_at.is_some());
    }
}
