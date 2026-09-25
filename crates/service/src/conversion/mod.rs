//! Durable TTS preview, conversion, media export, and artifact delivery.

use std::{
    collections::{BTreeMap, BTreeSet, HashMap, HashSet},
    path::{Path, PathBuf},
    process::Stdio,
    str::FromStr,
    sync::{
        Arc, Mutex as StdMutex, OnceLock,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};

use async_trait::async_trait;
use audiobookai_core::{
    Artifact, ArtifactId, ArtifactKind, AttemptId, BackgroundMusicSettings, Book, Chapter,
    ChapterId, CharacterId, DialogueSpan, DuckingSettings, ExportFormat, ExportLayout,
    ExportProfile, ExportProfileId, FileFingerprint, Job, JobAttempt, JobId, JobKind, JobState,
    JobUnit, JobUnitId, JobUnitKind, JobUnitState, Paragraph, PerformanceSettings,
    ProductionSegment, ProductionSegmentSource, Project, ProjectId, ProofExportSelection,
    ProofExportSnapshot, ProofExportSnapshotId, ProofingPlan, ProofingPlanStatus,
    ProvenanceQuality, ProviderProfileId, RateCardId, ReservationId, SegmentId, SegmentReviewState,
    SegmentSelection, SegmentTake, SegmentTakeId, Speaker, SpeakerOverride, TimingSettings,
    UsageEvent, UsageEventId, UsageQuantities, UsageWorkload, Validate, VoiceProfileId,
};
use audiobookai_media::{
    BackgroundMusic, BookMetadata as MediaBookMetadata, CacheFingerprint, ChapterAudio,
    ContentAddressedCache, ExportFormat as MediaExportFormat, ExportPlanner, ExportRequest,
    LoudnessMeasurement, LoudnessSettings, SidecarPair, SidecarResolver,
    parse_loudness_measurement,
};
use audiobookai_providers::{
    AudioChunk, AudioChunkSink, AudioFormat, CancellationFlag, ProviderError, ProviderId,
    ProviderUsage, StreamingSynthesisResponse, SynthesisRequest, SynthesisResponse, TtsProvider,
    UsageSource,
};
use audiobookai_storage::{OutputDestinationReservation, OutputReservationState, StorageError};
use axum::{
    body::Body,
    http::{HeaderMap, HeaderValue, StatusCode, header},
    response::{IntoResponse, Response},
};
use bytes::{Bytes, BytesMut};
use chrono::Utc;
use futures::{StreamExt, TryStreamExt, future::BoxFuture, stream};
use serde::{Deserialize, Serialize};
use sqlx::Row;
use tokio::{
    io::{AsyncReadExt, AsyncSeekExt, AsyncWriteExt},
    process::Command,
    sync::{Semaphore, broadcast, mpsc},
};
use uuid::Uuid;

use crate::{
    AppState, ServiceError,
    models::{
        ExportArtifactView, ExportFormatView, ExportOptionsInput, JobStageView, JobStatusView,
        JobUnitStatusView, JobUnitView, JobView, PreviewView, ProjectDisplayStatus,
        PronunciationKindView, PronunciationRuleView, PronunciationScopeView, ProviderKindView,
        ProviderModeView, ProviderProfileView, StartJobInput, UsageRowView, VoiceAssignmentView,
    },
    runtime::{
        FailureClass as RetryFailureClass, RetryEvent, RetryEventOutcome, RetryJournal,
        RetryJournalError, RetryPolicy, execute_with_retry,
    },
};

mod actions;
mod admission;
mod export;
mod export_profile;
mod job_state;
mod media_files;
mod planning;
mod playback;
mod preview;
mod proofing_plan;
mod provider_io;
mod recovery;
mod synthesis;
#[cfg(test)]
mod tests;
mod units;
mod worker;

// The production pipeline is grouped by stage: admission, planning, durable units, the worker,
// provider synthesis, assembly/export, and playback. Shared plan types stay here, and items used
// elsewhere in the crate are re-exported so `crate::conversion::…` paths remain stable.
pub(crate) use self::{
    actions::*, admission::*, planning::*, playback::*, preview::*, proofing_plan::*,
    provider_io::*, recovery::*,
};
use self::{
    export::{assemble_chapter, export_book, update_export_catalog},
    export_profile::{
        create_export_profile, format_name, layout_name, media_export_format, validate_export_input,
    },
    job_state::{
        admit_failed_job_retry, fail_interrupted_paid_job, increment_job_progress,
        mark_domain_job_failed, mark_job_failed, reconcile_job_budgets,
        release_unattached_reservation, set_job_message, transition_export_job_with_reservation,
        transition_job, update_staged_job_failure, update_unit_state, wait_until_runnable,
    },
    media_files::{
        artifact_for_file, artifact_for_file_with_id, artifact_kind_name, artifact_path,
        atomic_promote, copy_file_atomically, create_directory_no_clobber,
        ensure_existing_job_output_reservation, ensure_existing_real_directory,
        ensure_export_root_identity, ensure_output_directory_not_reserved,
        ensure_private_directory, ensure_private_staging_file_path, export_destination,
        export_manifest_path, ffmpeg_build_description, fingerprint_file, load_artifact,
        media_error, media_type_for_path, output_reservation_admission_error, persist_artifact,
        prepare_output_reservation, prepare_private_export_staging, probe_duration_ms,
        require_output_reservation, resolve_sidecars, run_process, run_process_capture, sync_file,
        validate_flac, verify_selected_artifact_integrity, verify_selected_artifacts_before_use,
        write_file_atomically, write_job_staging_file_atomically,
    },
    synthesis::synthesize_segment,
    units::{
        build_job_units, job_status_view, job_view, load_unit_plan, ordered_units, progress_ratio,
        unit_count, unit_view,
    },
    worker::{
        load_export_profile, load_required_proof_export_snapshot, reserve_job_budgets,
        schedule_conversion_job, schedule_conversion_retry, schedule_segment_regeneration_job,
        schedule_segment_regeneration_retry,
    },
};

const NORMALIZATION_VERSION: &str = "48k-flac-segment-v1";
const MAX_PREVIEW_CHARACTERS: usize = 500;
const RANGE_CHUNK_BYTES: usize = 64 * 1024;
const RECOVERED_PRODUCTION_CONFLICT: &str = "legacy state contained multiple active production jobs for this project; this conflicting job was failed without redispatch";

type ProviderSemaphoreRegistry = HashMap<Uuid, (u16, Arc<Semaphore>)>;

static PROVIDER_SEMAPHORES: OnceLock<StdMutex<ProviderSemaphoreRegistry>> = OnceLock::new();
static PLAYBACK_HUBS: OnceLock<StdMutex<HashMap<Uuid, Arc<PlaybackHub>>>> = OnceLock::new();
// A value of `true` records a start request that arrived while the current owner was still
// publishing its terminal state or reconciling accounting. The owner consumes that request and
// runs another iteration; if removal wins the mutex race, the requester becomes the new owner.
// Either ordering therefore leaves exactly one worker responsible for the durable job state.
static ACTIVE_WORKERS: OnceLock<StdMutex<HashMap<Uuid, bool>>> = OnceLock::new();
static OUTPUT_ADMISSION_LOCK: OnceLock<tokio::sync::Mutex<()>> = OnceLock::new();

#[derive(Clone, Debug, Deserialize, Serialize)]
struct SpeakerAssignment {
    character_id: Uuid,
    character_name: String,
    provider_id: Uuid,
    provider_name: String,
    provider_kind: ProviderKindView,
    #[serde(default)]
    provider_role: Option<crate::models::ProviderRoleView>,
    #[serde(default)]
    provider_mode: Option<ProviderModeView>,
    provider_endpoint: Option<String>,
    #[serde(default)]
    provider_snapshot_id: Option<Uuid>,
    provider_version: Option<String>,
    provider_concurrency: u16,
    voice_id: Uuid,
    voice_source: String,
    voice_name: String,
    model: Option<String>,
    #[serde(default)]
    performance: PerformanceSettings,
    #[serde(default)]
    timing: TimingSettings,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub(crate) struct SegmentPlan {
    id: SegmentId,
    proofing: bool,
    key: String,
    chapter_id: Uuid,
    paragraph_id: Uuid,
    source_content_hash: String,
    byte_start: u64,
    byte_end: u64,
    chapter_title: String,
    segment_ordinal: u32,
    playback_ordinal: usize,
    original_text: String,
    text: String,
    context: Option<String>,
    assignment: SpeakerAssignment,
    applied_rule_ids: Vec<Uuid>,
    dictionary_revision: String,
}

#[derive(Clone)]
struct TtsUsageContext {
    job_id: JobId,
    segment: SegmentPlan,
    provider_request_id: Uuid,
    rate_card_id: Option<RateCardId>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
struct ChapterPlan {
    chapter: Chapter,
    segments: Vec<SegmentPlan>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
struct ConversionPlan {
    project: Project,
    book: Book,
    chapters: Vec<ChapterPlan>,
    rules: Vec<PronunciationRuleView>,
    export: ExportProfile,
    music_path: Option<PathBuf>,
}

#[derive(Clone, Debug)]
struct SegmentArtifact {
    plan: SegmentPlan,
    artifact: Artifact,
}

#[derive(Clone, Debug)]
struct ChapterArtifact {
    chapter: Chapter,
    artifact: Artifact,
}

#[derive(Clone, Debug)]
struct PersistedUnitPlan {
    synthesis: HashMap<String, JobUnit>,
    assembly: HashMap<Uuid, JobUnit>,
    mix: Option<JobUnit>,
    normalize: JobUnit,
    export: JobUnit,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
struct ExportPromotionMarker {
    schema_version: u32,
    job_id: JobId,
    final_output: String,
    #[serde(default)]
    split_directory_created: bool,
    files: Vec<ExportPromotionFile>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
struct ExportPromotionFile {
    file_name: String,
    duration_ms: u64,
    fingerprint: FileFingerprint,
}

fn storage_error(error: impl std::fmt::Display) -> ServiceError {
    ServiceError::Storage(error.to_string())
}

fn internal_error(error: impl std::fmt::Display) -> ServiceError {
    ServiceError::Internal(error.to_string())
}
