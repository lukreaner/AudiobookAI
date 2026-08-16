//! Durable TTS preview, conversion, media export, and artifact delivery.

use std::{
    collections::{BTreeMap, BTreeSet, HashMap, HashSet},
    fmt::Write as _,
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

/// Creates a durable conversion and starts its in-process worker.
#[allow(clippy::too_many_lines)]
pub async fn start_conversion(
    state: Arc<AppState>,
    input: StartJobInput,
) -> Result<JobView, ServiceError> {
    validate_export_input(&input.export)?;
    let _shutdown_admission = state.admit_shutdown_sensitive_work().await?;
    let output_admission = OUTPUT_ADMISSION_LOCK
        .get_or_init(|| tokio::sync::Mutex::new(()))
        .lock()
        .await;
    let job_id = JobId::new();
    let (export_profile, music_path) =
        create_export_profile(&state, job_id, input.project_id, &input.export).await?;
    let plan = load_conversion_plan(&state, input.project_id, export_profile, music_path).await?;
    let units = build_job_units(job_id, &plan);
    let now = Utc::now();
    let output_reservation =
        prepare_output_reservation(job_id, plan.project.id, &plan.export, now).await?;
    let total = u64::try_from(unit_count(&units)).unwrap_or(u64::MAX);
    let mut job = Job {
        id: job_id,
        project_id: ProjectId::from_uuid(input.project_id),
        kind: JobKind::Conversion,
        state: JobState::Queued,
        export_profile_id: Some(plan.export.id),
        reservation_id: None,
        progress_completed: 0,
        progress_total: total,
        status_message: Some("Queued for conversion".to_owned()),
        allow_budget_override: input.allow_budget_override,
        created_at: now,
        started_at: None,
        finished_at: None,
        updated_at: now,
        revision: 0,
    };
    state
        .database
        .repositories()
        .jobs
        .insert_with_output_reservation(&job, &output_reservation)
        .await
        .map_err(output_reservation_admission_error)?;
    drop(output_admission);
    let reservation_id = match reserve_job_budgets(&state, &job, &plan).await {
        Ok(reservation_id) => reservation_id,
        Err(error) => {
            mark_domain_job_failed(&state, job_id, &error.to_string()).await;
            return Err(error);
        }
    };
    if let Some(reservation_id) = reservation_id {
        let expected = job.revision;
        job.reservation_id = Some(reservation_id);
        job.updated_at = Utc::now();
        job = match state
            .database
            .repositories()
            .jobs
            .update(&job, expected)
            .await
        {
            Ok(job) => job,
            Err(error) => {
                release_unattached_reservation(&state, reservation_id).await;
                mark_domain_job_failed(&state, job_id, &error.to_string()).await;
                return Err(storage_error(error));
            }
        }
    }

    let (proofing_plan, proofing_segments) = match build_proofing_plan(&state, &job, &plan).await {
        Ok(value) => value,
        Err(error) => {
            mark_domain_job_failed(&state, job_id, &error.to_string()).await;
            let _ = reconcile_job_budgets(&state, job_id).await;
            return Err(error);
        }
    };
    let durable_units = ordered_units(&units);
    if let Err(error) = state
        .database
        .repositories()
        .proofing
        .replace_plan_with_units(&proofing_plan, &proofing_segments, &durable_units)
        .await
    {
        let error = storage_error(error);
        mark_domain_job_failed(&state, job_id, &error.to_string()).await;
        let _ = reconcile_job_budgets(&state, job_id).await;
        return Err(error);
    }

    let view = job_view(&job, &plan.project.metadata.title, &units);
    {
        let mut catalog = state.catalog.write().await;
        catalog.jobs.insert(job_id.as_uuid(), view.clone());
        if let Some(project) = catalog.projects.get_mut(&input.project_id) {
            project.summary.status = ProjectDisplayStatus::Processing;
            project.summary.progress = 0.0;
        }
    }
    state.events.publish(
        "job.queued",
        serde_json::json!({"jobId": job_id, "projectId": input.project_id}),
    );
    schedule_conversion_job(Arc::clone(&state), job_id);
    Ok(view)
}

/// Starts a provider-free export from the exact takes selected in the proofing workbench.
/// The serialized plan and selection snapshot make the job crash-resumable without consulting
/// mutable narration or assignment state again.
#[allow(clippy::too_many_lines)]
pub(crate) async fn start_proof_export(
    state: Arc<AppState>,
    project_id: Uuid,
    input: ExportOptionsInput,
    strict_retailer: bool,
) -> Result<JobView, ServiceError> {
    validate_export_input(&input)?;
    let _shutdown_admission = state.admit_shutdown_sensitive_work().await?;
    let output_admission = OUTPUT_ADMISSION_LOCK
        .get_or_init(|| tokio::sync::Mutex::new(()))
        .lock()
        .await;
    let repositories = state.database.repositories();
    let proof = repositories
        .proofing
        .get_plan(ProjectId::from_uuid(project_id))
        .await
        .map_err(storage_error)?
        .ok_or_else(|| ServiceError::Conflict("proofing data is unavailable".to_owned()))?;
    if proof.status != ProofingPlanStatus::Ready {
        return Err(ServiceError::Conflict(
            "finish the active proofing plan before exporting".to_owned(),
        ));
    }
    let segments = repositories
        .proofing
        .list_active_segments(ProjectId::from_uuid(project_id), None)
        .await
        .map_err(storage_error)?;
    if segments.is_empty() {
        return Err(ServiceError::Conflict(
            "the proofing plan contains no production segments".to_owned(),
        ));
    }
    let project = repositories
        .projects
        .get_project(ProjectId::from_uuid(project_id))
        .await
        .map_err(storage_error)?
        .ok_or(ServiceError::NotFound)?;
    let book = repositories
        .projects
        .get_book(project.book_id)
        .await
        .map_err(storage_error)?
        .ok_or(ServiceError::NotFound)?;
    let source_chapters = repositories
        .projects
        .list_chapters(project.book_id)
        .await
        .map_err(storage_error)?;

    let mut snapshot_selections = Vec::with_capacity(segments.len());
    let mut selected_artifacts = HashMap::new();
    let mut planned_segments = HashMap::<Uuid, Vec<SegmentPlan>>::new();
    for segment in &segments {
        if strict_retailer && !segment.review_state.is_accepted() {
            return Err(ServiceError::Conflict(format!(
                "segment {} must be approved or locked for a retailer export",
                segment.id
            )));
        }
        if segment.review_state == SegmentReviewState::Flagged {
            return Err(ServiceError::Conflict(format!(
                "resolve the flag on segment {} before exporting",
                segment.id
            )));
        }
        let selection = repositories
            .proofing
            .get_selection(segment.id)
            .await
            .map_err(storage_error)?
            .ok_or_else(|| {
                ServiceError::Conflict(format!("select a take for segment {}", segment.id))
            })?;
        let take = repositories
            .proofing
            .get_take(selection.take_id)
            .await
            .map_err(storage_error)?
            .ok_or_else(|| ServiceError::Conflict("a selected take is unavailable".to_owned()))?;
        if take.semantic_input_hash != segment.expected_input_hash {
            return Err(ServiceError::Conflict(format!(
                "the selected take for segment {} is stale",
                segment.id
            )));
        }
        let artifact = load_artifact(&state, take.artifact_id).await?;
        verify_selected_artifact_integrity(&artifact).await?;
        let segment_plan = load_proofing_segment_plan(&state, project_id, segment).await?;
        if segment_semantic_input_hash(&segment_plan)? != segment.expected_input_hash {
            return Err(ServiceError::Conflict(format!(
                "the narration inputs for segment {} changed; review or regenerate it first",
                segment.id
            )));
        }
        let chapter_id = segment_plan.chapter_id;
        selected_artifacts.insert(segment_plan.key.clone(), take.artifact_id);
        planned_segments
            .entry(chapter_id)
            .or_default()
            .push(segment_plan);
        snapshot_selections.push(ProofExportSelection {
            segment_id: segment.id,
            take_id: take.id,
            artifact_id: take.artifact_id,
        });
    }
    let mut chapters = Vec::new();
    for chapter in source_chapters {
        let Some(mut chapter_segments) = planned_segments.remove(&chapter.id.as_uuid()) else {
            continue;
        };
        chapter_segments.sort_by_key(|segment| segment.segment_ordinal);
        chapters.push(ChapterPlan {
            chapter,
            segments: chapter_segments,
        });
    }
    if !planned_segments.is_empty() {
        return Err(ServiceError::Conflict(
            "a proofing segment no longer belongs to an available chapter".to_owned(),
        ));
    }

    let job_id = JobId::new();
    let (export, music_path) = create_export_profile(&state, job_id, project_id, &input).await?;
    let rules = state.catalog.read().await.pronunciation_rules.clone();
    let plan = ConversionPlan {
        project,
        book,
        chapters,
        rules,
        export,
        music_path,
    };
    let mut units = build_job_units(job_id, &plan);
    let now = Utc::now();
    let output_reservation =
        prepare_output_reservation(job_id, plan.project.id, &plan.export, now).await?;
    for (key, unit) in &mut units.synthesis {
        unit.state = JobUnitState::Completed;
        unit.output_artifact_id = selected_artifacts.get(key).copied();
        unit.updated_at = now;
    }
    let snapshot = ProofExportSnapshot {
        id: ProofExportSnapshotId::new(),
        project_id: ProjectId::from_uuid(project_id),
        job_id,
        export_profile_id: plan.export.id,
        plan_revision: proof.plan_revision,
        plan_hash: proof.plan_hash.clone(),
        selections: snapshot_selections,
        created_at: now,
    };
    units.export.payload.insert(
        "proofExportPlan".to_owned(),
        serde_json::to_value(&plan).map_err(internal_error)?,
    );
    units.export.payload.insert(
        "proofExportSnapshotId".to_owned(),
        serde_json::json!(snapshot.id),
    );
    let completed = u64::try_from(units.synthesis.len()).unwrap_or(u64::MAX);
    let job = Job {
        id: job_id,
        project_id: ProjectId::from_uuid(project_id),
        kind: JobKind::Export,
        state: JobState::Queued,
        export_profile_id: Some(plan.export.id),
        reservation_id: None,
        progress_completed: completed,
        progress_total: u64::try_from(unit_count(&units)).unwrap_or(u64::MAX),
        status_message: Some("Queued proofing export".to_owned()),
        allow_budget_override: false,
        created_at: now,
        started_at: None,
        finished_at: None,
        updated_at: now,
        revision: 0,
    };
    repositories
        .proofing
        .insert_export_job_graph_with_output_reservation(
            &job,
            &ordered_units(&units),
            &snapshot,
            &output_reservation,
        )
        .await
        .map_err(output_reservation_admission_error)?;
    drop(output_admission);

    let view = job_view(&job, &plan.project.metadata.title, &units);
    state
        .catalog
        .write()
        .await
        .jobs
        .insert(job_id.as_uuid(), view.clone());
    state.events.publish(
        "job.created",
        serde_json::json!({"jobId": job_id, "projectId": project_id, "kind": "proof_export"}),
    );
    schedule_conversion_job(Arc::clone(&state), job_id);
    Ok(view)
}

#[derive(Clone, Debug)]
pub(crate) struct RegenerationQuote {
    pub segment_id: SegmentId,
    pub segment_revision: u64,
    pub semantic_input_hash: String,
    pub provider_profile_id: ProviderProfileId,
    pub provider_name: String,
    pub model: Option<String>,
    pub characters: u64,
    pub monetary_cost_micros: Option<i64>,
    pub currency: Option<String>,
    pub credits: Option<i64>,
    pub rate_card_id: Option<RateCardId>,
}

pub(crate) async fn quote_segment_regeneration(
    state: &AppState,
    project_id: Uuid,
    segment_id: SegmentId,
) -> Result<RegenerationQuote, ServiceError> {
    let segment = state
        .database
        .repositories()
        .proofing
        .get_segment(segment_id)
        .await
        .map_err(storage_error)?
        .ok_or(ServiceError::NotFound)?;
    if segment.project_id.as_uuid() != project_id || !segment.active {
        return Err(ServiceError::NotFound);
    }
    let plan = load_proofing_segment_plan(state, project_id, &segment).await?;
    let semantic_input_hash = segment_semantic_input_hash(&plan)?;
    if semantic_input_hash != segment.expected_input_hash {
        return Err(ServiceError::ConflictDetails {
            code: "proofing_plan_dirty",
            detail: "update the proofing plan before regenerating this segment".to_owned(),
            meta: serde_json::json!({"segmentId": segment_id}),
        });
    }
    let characters = u64::try_from(plan.text.chars().count()).unwrap_or(u64::MAX);
    let estimate = crate::accounting::rate_usage_estimate(
        state,
        ProviderProfileId::from_uuid(plan.assignment.provider_id),
        UsageWorkload::Tts,
        plan.assignment.model.clone(),
        UsageQuantities {
            characters: Some(characters),
            ..UsageQuantities::default()
        },
    )
    .await?;
    Ok(RegenerationQuote {
        segment_id,
        segment_revision: segment.revision,
        semantic_input_hash,
        provider_profile_id: estimate.provider_profile_id,
        provider_name: plan.assignment.provider_name,
        model: plan.assignment.model,
        characters,
        monetary_cost_micros: estimate.cost.as_ref().map(|cost| cost.micros),
        currency: estimate.cost.as_ref().map(|cost| cost.currency.clone()),
        credits: estimate.quantities.provider_credits,
        rate_card_id: estimate.rate_card_id,
    })
}

#[allow(clippy::too_many_lines)]
pub(crate) async fn start_segment_regeneration(
    state: Arc<AppState>,
    project_id: Uuid,
    segment_id: SegmentId,
    expected_segment_revision: u64,
    allow_budget_override: bool,
) -> Result<JobView, ServiceError> {
    let _shutdown_admission = state.admit_shutdown_sensitive_work().await?;
    let segment = state
        .database
        .repositories()
        .proofing
        .get_segment(segment_id)
        .await
        .map_err(storage_error)?
        .ok_or(ServiceError::NotFound)?;
    if segment.project_id.as_uuid() != project_id || !segment.active {
        return Err(ServiceError::NotFound);
    }
    if segment.revision != expected_segment_revision {
        return Err(ServiceError::ConflictDetails {
            code: "stale_segment_revision",
            detail: "the segment changed after it was estimated".to_owned(),
            meta: serde_json::json!({
                "segmentId": segment_id,
                "currentRevision": segment.revision,
            }),
        });
    }
    if segment.review_state == SegmentReviewState::Locked {
        return Err(ServiceError::ConflictDetails {
            code: "segment_locked",
            detail: "unlock this segment before regenerating it".to_owned(),
            meta: serde_json::json!({"segmentId": segment_id}),
        });
    }
    let segment_plan =
        load_dispatchable_proofing_segment_plan(&state, project_id, &segment).await?;
    if segment_semantic_input_hash(&segment_plan)? != segment.expected_input_hash {
        return Err(ServiceError::ConflictDetails {
            code: "estimate_changed",
            detail: "the segment synthesis inputs changed after estimation".to_owned(),
            meta: serde_json::json!({"segmentId": segment_id}),
        });
    }
    let active = state
        .database
        .repositories()
        .jobs
        .list_active()
        .await
        .map_err(storage_error)?;
    if let Some(existing) = active.into_iter().find(|job| {
        job.kind == JobKind::SegmentRegeneration && job.project_id.as_uuid() == project_id
    }) {
        return Err(ServiceError::ConflictDetails {
            code: "active_segment_regeneration",
            detail: "another segment regeneration is already active for this project".to_owned(),
            meta: serde_json::json!({"activeJobId": existing.id}),
        });
    }
    let estimate = crate::accounting::rate_usage_estimate(
        &state,
        ProviderProfileId::from_uuid(segment_plan.assignment.provider_id),
        UsageWorkload::Tts,
        segment_plan.assignment.model.clone(),
        UsageQuantities {
            characters: u64::try_from(segment_plan.text.chars().count()).ok(),
            ..UsageQuantities::default()
        },
    )
    .await?;
    let now = Utc::now();
    let take_id = SegmentTakeId::new();
    let mut job = Job {
        id: JobId::new(),
        project_id: ProjectId::from_uuid(project_id),
        kind: JobKind::SegmentRegeneration,
        // A terminal staging state makes a crash before budget admission fail closed. The job is
        // moved to Queued only after its graph and reservation association are both durable.
        state: JobState::Failed,
        export_profile_id: None,
        reservation_id: None,
        progress_completed: 0,
        progress_total: 1,
        status_message: Some("Segment regeneration was interrupted before admission".to_owned()),
        allow_budget_override,
        created_at: now,
        started_at: None,
        finished_at: Some(now),
        updated_at: now,
        revision: 0,
    };
    let unit = JobUnit {
        id: JobUnitId::new(),
        job_id: job.id,
        kind: JobUnitKind::SynthesisSegment,
        state: JobUnitState::Ready,
        chapter_id: Some(ChapterId::from_uuid(segment_plan.chapter_id)),
        segment_id: Some(segment_id),
        provider_profile_id: Some(ProviderProfileId::from_uuid(
            segment_plan.assignment.provider_id,
        )),
        dependencies: Vec::new(),
        attempt_count: 0,
        next_attempt_at: None,
        output_artifact_id: None,
        payload: BTreeMap::from([
            (
                "title".to_owned(),
                serde_json::json!(format!("Regenerate {}", segment_plan.chapter_title)),
            ),
            ("progress".to_owned(), serde_json::json!(0.0)),
            ("segmentKey".to_owned(), serde_json::json!(segment_plan.key)),
            ("takeId".to_owned(), serde_json::json!(take_id)),
            (
                "takeArtifactId".to_owned(),
                serde_json::json!(ArtifactId::new()),
            ),
            (
                "cacheOperation".to_owned(),
                serde_json::json!(format!("regeneration:{take_id}")),
            ),
            ("autoSelect".to_owned(), serde_json::json!(false)),
            (
                "segmentPlan".to_owned(),
                serde_json::to_value(&segment_plan).map_err(internal_error)?,
            ),
        ]),
        created_at: now,
        updated_at: now,
    };
    let repositories = state.database.repositories();
    repositories
        .proofing
        .insert_job_graph(&job, std::slice::from_ref(&unit), None)
        .await
        .map_err(storage_error)?;
    let policy = retry_policy(&state, &segment_plan).await?;
    let reservation_multiplier = retry_reservation_multiplier(&policy);
    let reservation_estimates = vec![estimate; reservation_multiplier];
    let reservation_id = match crate::accounting::reserve_for_estimates(
        &state,
        &job,
        &reservation_estimates,
    )
    .await
    {
        Ok(reservation_id) => reservation_id,
        Err(error) => {
            update_staged_job_failure(&state, job.id, &error.to_string()).await;
            return Err(error);
        }
    };
    let expected = job.revision;
    job.reservation_id = reservation_id;
    job.transition(JobState::Queued, Utc::now())
        .map_err(|error| ServiceError::Internal(error.to_string()))?;
    job.finished_at = None;
    job.status_message = Some("Queued segment regeneration".to_owned());
    job = match repositories.jobs.update(&job, expected).await {
        Ok(job) => job,
        Err(error) => {
            if let Some(reservation_id) = reservation_id {
                release_unattached_reservation(&state, reservation_id).await;
            }
            update_staged_job_failure(&state, job.id, &error.to_string()).await;
            return Err(storage_error(error));
        }
    };
    let project_title = state
        .catalog
        .read()
        .await
        .projects
        .get(&project_id)
        .map_or_else(
            || "Audiobook".to_owned(),
            |project| project.summary.title.clone(),
        );
    let view = single_unit_job_view(&job, &project_title, &unit);
    state
        .catalog
        .write()
        .await
        .jobs
        .insert(job.id.as_uuid(), view.clone());
    state.events.publish(
        "job.queued",
        serde_json::json!({"jobId": job.id, "projectId": project_id, "segmentId": segment_id}),
    );
    schedule_segment_regeneration_job(Arc::clone(&state), job.id);
    Ok(view)
}

fn single_unit_job_view(job: &Job, title: &str, unit: &JobUnit) -> JobView {
    JobView {
        id: job.id.as_uuid(),
        project_id: job.project_id.as_uuid(),
        project_title: title.to_owned(),
        kind: crate::models::JobKindView::SegmentRegeneration,
        status: job_status_view(job.state),
        progress: progress_ratio(job.progress_completed, job.progress_total),
        current_stage: job.status_message.clone(),
        started_at: job.started_at,
        updated_at: job.updated_at,
        estimated_remaining_seconds: None,
        units: vec![unit_view(unit)],
        progressive_playback_url: None,
        uncertain_charge: false,
    }
}

#[allow(clippy::too_many_lines)]
pub(crate) async fn load_proofing_segment_plan(
    state: &AppState,
    project_id: Uuid,
    segment: &ProductionSegment,
) -> Result<SegmentPlan, ServiceError> {
    load_proofing_segment_plan_for(state, project_id, segment, AssignmentPurpose::Semantic).await
}

async fn load_dispatchable_proofing_segment_plan(
    state: &AppState,
    project_id: Uuid,
    segment: &ProductionSegment,
) -> Result<SegmentPlan, ServiceError> {
    load_proofing_segment_plan_for(state, project_id, segment, AssignmentPurpose::Dispatch).await
}

#[allow(clippy::too_many_lines)]
async fn load_proofing_segment_plan_for(
    state: &AppState,
    project_id: Uuid,
    segment: &ProductionSegment,
    purpose: AssignmentPurpose,
) -> Result<SegmentPlan, ServiceError> {
    let project = state
        .database
        .repositories()
        .projects
        .get_project(ProjectId::from_uuid(project_id))
        .await
        .map_err(storage_error)?
        .ok_or(ServiceError::NotFound)?;
    let (characters, voices, providers, rules, chapter_title) = {
        let catalog = state.catalog.read().await;
        let characters = catalog
            .characters
            .get(&project_id)
            .cloned()
            .unwrap_or_default();
        let chapter_title = segment
            .chapter_id
            .and_then(|id| {
                catalog
                    .projects
                    .get(&project_id)
                    .and_then(|project| {
                        project
                            .chapters
                            .iter()
                            .find(|chapter| chapter.id == id.as_uuid())
                    })
                    .map(|chapter| chapter.title.clone())
            })
            .unwrap_or_else(|| "Production credit".to_owned());
        (
            characters,
            catalog.voice_sources.clone(),
            catalog.providers.clone(),
            catalog.pronunciation_rules.clone(),
            chapter_title,
        )
    };
    let character_id = match &segment.speaker {
        Speaker::Character(id) => id.as_uuid(),
        Speaker::Narrator => characters
            .iter()
            .find(|character| character.role == audiobookai_core::CharacterRole::Narrator)
            .map(|character| character.id)
            .ok_or_else(|| ServiceError::Conflict("the narrator is unavailable".to_owned()))?,
        Speaker::Named(name) => characters
            .iter()
            .find(|character| character.canonical_name.eq_ignore_ascii_case(name))
            .map(|character| character.id)
            .ok_or_else(|| {
                ServiceError::Conflict("the segment speaker is unavailable".to_owned())
            })?,
    };
    let character = characters
        .iter()
        .find(|character| character.id == character_id)
        .cloned()
        .ok_or_else(|| ServiceError::Conflict("the segment speaker is unavailable".to_owned()))?;
    let assignments = build_assignments_for(
        &project,
        std::slice::from_ref(&character),
        &voices,
        &providers,
        state,
        purpose,
    )
    .await?;
    let mut assignment = assignments
        .get(&character_id)
        .cloned()
        .ok_or_else(|| ServiceError::Conflict("the segment speaker has no voice".to_owned()))?;
    assignment.performance = assignment
        .performance
        .overlay(&segment.performance_override);
    assignment.timing = TimingSettings {
        pause_before_ms: segment
            .timing_override
            .pause_before_ms
            .or(assignment.timing.pause_before_ms),
        pause_after_ms: segment
            .timing_override
            .pause_after_ms
            .or(assignment.timing.pause_after_ms),
    };
    let base_text = segment
        .narration_text_override
        .as_deref()
        .unwrap_or(&segment.original_text);
    let (text, applied_rule_ids, dictionary_revision) = apply_pronunciation_rules(
        base_text,
        &rules,
        project_id,
        character_id,
        project.metadata.language.as_deref(),
    )?;
    let context = match (&segment.context_before, &segment.context_after) {
        (Some(before), Some(after)) => Some(format!("{before}\n---\n{after}")),
        (Some(value), None) | (None, Some(value)) => Some(value.clone()),
        (None, None) => None,
    };
    Ok(SegmentPlan {
        id: segment.id,
        proofing: true,
        key: segment.stable_key.clone(),
        chapter_id: segment
            .chapter_id
            .ok_or_else(|| {
                ServiceError::Conflict("credit regeneration is not available yet".to_owned())
            })?
            .as_uuid(),
        paragraph_id: segment
            .paragraph_id
            .ok_or_else(|| ServiceError::Conflict("segment source is unavailable".to_owned()))?
            .as_uuid(),
        source_content_hash: segment.source_content_hash.clone(),
        byte_start: segment.byte_start.unwrap_or_default(),
        byte_end: segment.byte_end.unwrap_or_default(),
        chapter_title,
        segment_ordinal: segment.ordinal,
        playback_ordinal: usize::try_from(segment.ordinal).unwrap_or(usize::MAX),
        original_text: segment.original_text.clone(),
        text,
        context,
        assignment,
        applied_rule_ids,
        dictionary_revision,
    })
}

async fn build_proofing_plan(
    state: &AppState,
    job: &Job,
    conversion: &ConversionPlan,
) -> Result<(ProofingPlan, Vec<ProductionSegment>), ServiceError> {
    let repositories = state.database.repositories();
    let previous = repositories
        .proofing
        .get_plan(conversion.project.id)
        .await
        .map_err(storage_error)?;
    let now = Utc::now();
    let narrator_id = state
        .catalog
        .read()
        .await
        .characters
        .get(&conversion.project.id.as_uuid())
        .and_then(|characters| {
            characters
                .iter()
                .find(|character| character.role == audiobookai_core::CharacterRole::Narrator)
        })
        .map(|character| character.id);
    let mut segments = Vec::new();
    let mut plan_hasher = blake3::Hasher::new();
    for chapter in &conversion.chapters {
        for segment in &chapter.segments {
            let expected_input_hash = segment_semantic_input_hash(segment)?;
            plan_hasher.update(segment.key.as_bytes());
            plan_hasher.update(&[0]);
            plan_hasher.update(expected_input_hash.as_bytes());
            plan_hasher.update(&[0]);
            let speaker = if narrator_id == Some(segment.assignment.character_id) {
                Speaker::Narrator
            } else {
                Speaker::Character(CharacterId::from_uuid(segment.assignment.character_id))
            };
            segments.push(ProductionSegment {
                id: segment.id,
                project_id: conversion.project.id,
                chapter_id: Some(ChapterId::from_uuid(segment.chapter_id)),
                paragraph_id: Some(audiobookai_core::ParagraphId::from_uuid(
                    segment.paragraph_id,
                )),
                source: ProductionSegmentSource::EpubRange,
                stable_key: segment.key.clone(),
                ordinal: segment.segment_ordinal,
                source_content_hash: segment.source_content_hash.clone(),
                byte_start: Some(segment.byte_start),
                byte_end: Some(segment.byte_end),
                speaker,
                original_text: segment.original_text.clone(),
                narration_text_override: None,
                effective_text: segment.text.clone(),
                context_before: segment.context.clone(),
                context_after: None,
                performance_override: PerformanceSettings::default(),
                timing_override: TimingSettings::default(),
                expected_input_hash,
                review_state: SegmentReviewState::Unreviewed,
                active: true,
                revision: 0,
                created_at: now,
                updated_at: now,
            });
        }
    }
    let plan = ProofingPlan {
        project_id: conversion.project.id,
        source_conversion_job_id: job.id,
        plan_revision: previous
            .as_ref()
            .map_or(1, |value| value.plan_revision.saturating_add(1)),
        plan_hash: plan_hasher.finalize().to_hex().to_string(),
        status: ProofingPlanStatus::Incomplete,
        dirty_reasons: Vec::new(),
        created_at: previous.map_or(now, |value| value.created_at),
        updated_at: now,
    };
    Ok((plan, segments))
}

/// Restarts conversion workers after an application restart without duplicating completed units.
pub async fn resume_durable_conversions(state: Arc<AppState>) -> Result<(), ServiceError> {
    release_orphaned_admission_reservations(&state).await?;
    recover_terminal_paid_reservations(&state).await?;
    state
        .database
        .repositories()
        .jobs
        .release_terminal_output_reservations()
        .await
        .map_err(storage_error)?;
    let active = state
        .database
        .repositories()
        .jobs
        .list_active()
        .await
        .map_err(storage_error)?;

    // Older databases could contain more than one active project-production job because the
    // invariant was previously enforced only at API admission time. Resolve every conflicting
    // record durably before any worker is spawned. Retaining the oldest record matches the job
    // that would already have blocked later admissions; the UUID tie-break makes legacy rows with
    // identical timestamps deterministic.
    let recovered_conflicts = recovered_production_conflicts(&active);
    for job in active
        .iter()
        .filter(|job| recovered_conflicts.contains(&job.id))
    {
        fail_recovered_production_conflict(&state, job).await?;
    }

    for job in active {
        if recovered_conflicts.contains(&job.id) {
            continue;
        }
        if job.kind == JobKind::Preview {
            recover_interrupted_preview(&state, &job).await?;
            continue;
        }
        if !matches!(
            job.kind,
            JobKind::Conversion | JobKind::SegmentRegeneration | JobKind::Export
        ) {
            continue;
        }
        if fail_recovered_interrupted_paid_job(&state, &job).await? {
            continue;
        }
        if matches!(job.kind, JobKind::Conversion | JobKind::Export) {
            let recovery = {
                let _output_admission = OUTPUT_ADMISSION_LOCK
                    .get_or_init(|| tokio::sync::Mutex::new(()))
                    .lock()
                    .await;
                ensure_existing_job_output_reservation(&state, &job).await
            };
            if let Err(error) = recovery {
                let message = format!(
                    "legacy export job could not acquire its output destination before restart: {error}"
                );
                fail_interrupted_paid_job(&state, job.id, &message).await?;
                continue;
            }
        }
        match job.state {
            JobState::Queued | JobState::Running => {
                if job.kind == JobKind::SegmentRegeneration {
                    schedule_segment_regeneration_job(Arc::clone(&state), job.id);
                } else {
                    schedule_conversion_job(Arc::clone(&state), job.id);
                }
            }
            JobState::Pausing => {
                transition_job(&state, job.id, JobState::Paused, "Paused").await?;
            }
            JobState::Cancelling => {
                transition_job(&state, job.id, JobState::Cancelled, "Cancelled").await?;
            }
            JobState::Paused | JobState::Cancelled | JobState::Failed | JobState::Completed => {}
        }
    }
    Ok(())
}

fn recovered_production_conflicts(active: &[Job]) -> BTreeSet<JobId> {
    let mut projects = BTreeMap::<ProjectId, Vec<&Job>>::new();
    for job in active.iter().filter(|job| {
        matches!(
            job.kind,
            JobKind::CharacterDetection
                | JobKind::Conversion
                | JobKind::SegmentRegeneration
                | JobKind::Export
        )
    }) {
        projects.entry(job.project_id).or_default().push(job);
    }

    let mut conflicts = BTreeSet::new();
    for jobs in projects.values_mut() {
        jobs.sort_by(|left, right| {
            left.created_at
                .cmp(&right.created_at)
                .then_with(|| left.id.to_string().cmp(&right.id.to_string()))
        });
        conflicts.extend(jobs.iter().skip(1).map(|job| job.id));
    }
    conflicts
}

async fn fail_recovered_production_conflict(
    state: &AppState,
    job: &Job,
) -> Result<(), ServiceError> {
    if job.kind == JobKind::CharacterDetection {
        return crate::workflows::fail_recovered_production_conflict(
            state,
            job,
            RECOVERED_PRODUCTION_CONFLICT,
        )
        .await;
    }

    let interrupted = interrupted_paid_dispatches(state, job.id).await?;
    let uncertainty_recorded = record_recovered_paid_dispatches(state, &interrupted).await?;
    let message = if interrupted.is_empty() {
        RECOVERED_PRODUCTION_CONFLICT.to_owned()
    } else {
        format!(
            "{RECOVERED_PRODUCTION_CONFLICT}; one or more provider requests may have been charged and were not retried automatically"
        )
    };
    fail_interrupted_paid_job(state, job.id, &message).await?;
    if uncertainty_recorded {
        reconcile_job_budgets(state, job.id).await?;
    }
    Ok(())
}

async fn fail_recovered_interrupted_paid_job(
    state: &AppState,
    job: &Job,
) -> Result<bool, ServiceError> {
    let interrupted = interrupted_paid_dispatches(state, job.id).await?;
    if interrupted.is_empty() {
        return Ok(false);
    }
    let uncertainty_recorded = record_recovered_paid_dispatches(state, &interrupted).await?;
    let message = match job.kind {
        JobKind::SegmentRegeneration => {
            "the application stopped while a paid regeneration dispatch was in progress; the provider may have charged it, so it was not retried automatically"
        }
        JobKind::Conversion | JobKind::Export => {
            "the application stopped while one or more paid synthesis dispatches were in progress; the provider may have charged them, so they were not retried automatically"
        }
        JobKind::CharacterDetection
        | JobKind::Preview
        | JobKind::QualityControl
        | JobKind::CacheCleanup => {
            return Err(ServiceError::Internal(
                "paid synthesis recovery was requested for an unsupported job kind".to_owned(),
            ));
        }
    };
    fail_interrupted_paid_job(state, job.id, message).await?;
    if uncertainty_recorded {
        reconcile_job_budgets(state, job.id).await?;
    }
    Ok(true)
}

async fn record_recovered_paid_dispatches(
    state: &AppState,
    units: &[JobUnit],
) -> Result<bool, ServiceError> {
    let mut uncertainty_recorded = true;
    for unit in units {
        if let Err(error) = record_interrupted_paid_unit_uncertainty(state, unit).await {
            // Legacy rows may not contain a complete segment snapshot. Keep their reservation
            // active for manual reconciliation instead of treating an unknown paid request as
            // zero usage.
            uncertainty_recorded = false;
            let mut unresolved = unit.clone();
            unresolved.payload.insert(
                "uncertainUsageUnresolved".to_owned(),
                serde_json::json!(true),
            );
            state
                .database
                .repositories()
                .jobs
                .upsert_unit(&unresolved)
                .await
                .map_err(storage_error)?;
            tracing::warn!(diagnostic_code = "production.recovery.usage_unresolved", job_id = %unit.job_id, unit_id = %unit.id, %error, "interrupted production usage could not be reconstructed; reservation retained");
        }
    }
    Ok(uncertainty_recorded)
}

async fn recover_interrupted_preview(state: &AppState, job: &Job) -> Result<(), ServiceError> {
    let units = state
        .database
        .repositories()
        .jobs
        .list_units(job.id)
        .await
        .map_err(storage_error)?;
    let mut uncertainty_recorded = true;
    for unit in units.iter().filter(|unit| {
        unit.kind == JobUnitKind::SynthesisSegment
            && matches!(unit.state, JobUnitState::Running | JobUnitState::Retrying)
    }) {
        if let Err(error) = record_interrupted_paid_unit_uncertainty(state, unit).await {
            // Legacy preview units did not persist their segment snapshot. Fail the job without
            // redispatch, but retain its reservation instead of incorrectly treating usage as zero.
            uncertainty_recorded = false;
            let mut unresolved = unit.clone();
            unresolved.payload.insert(
                "uncertainUsageUnresolved".to_owned(),
                serde_json::json!(true),
            );
            state
                .database
                .repositories()
                .jobs
                .upsert_unit(&unresolved)
                .await
                .map_err(storage_error)?;
            tracing::warn!(diagnostic_code = "preview.recovery.usage_unresolved", job_id = %job.id, %error, "interrupted preview usage could not be reconstructed; reservation retained");
        }
    }
    let message =
        "the application stopped during a billable preview; it was not redispatched automatically";
    fail_interrupted_paid_job(state, job.id, message).await?;
    if uncertainty_recorded {
        crate::accounting::finalize_job_reservation(state, job.id).await?;
    }
    Ok(())
}

async fn recover_terminal_paid_reservations(state: &AppState) -> Result<(), ServiceError> {
    let rows = sqlx::query(
        "SELECT j.id, j.state FROM jobs j \
         JOIN budget_reservations r ON r.id = j.reservation_id \
         WHERE j.kind IN \
         ('conversion', 'segment_regeneration', 'export', 'preview', 'character_detection') \
         AND j.state IN ('failed', 'cancelled', 'completed') \
         AND r.status IN ('active', 'expired')",
    )
    .fetch_all(state.database.pool())
    .await
    .map_err(storage_error)?;
    for row in rows {
        let job_id = JobId::from_str(row.get::<&str, _>("id")).map_err(internal_error)?;
        let recover_interrupted_dispatch = row.get::<&str, _>("state") == "failed";
        let mut uncertainty_recorded = true;
        for mut unit in state
            .database
            .repositories()
            .jobs
            .list_units(job_id)
            .await
            .map_err(storage_error)?
        {
            if unit
                .payload
                .get("uncertainUsageUnresolved")
                .and_then(serde_json::Value::as_bool)
                .unwrap_or(false)
            {
                uncertainty_recorded = false;
            }
            if recover_interrupted_dispatch
                && unit.kind == JobUnitKind::SynthesisSegment
                && matches!(unit.state, JobUnitState::Running | JobUnitState::Retrying)
            {
                if let Err(error) = record_interrupted_paid_unit_uncertainty(state, &unit).await {
                    uncertainty_recorded = false;
                    unit.payload.insert(
                        "uncertainUsageUnresolved".to_owned(),
                        serde_json::json!(true),
                    );
                    tracing::warn!(diagnostic_code = "paid_job.recovery.usage_unresolved", %job_id, %error, "interrupted paid usage could not be reconstructed; reservation retained");
                }
                update_unit_state(
                    state,
                    &mut unit,
                    JobUnitState::Failed,
                    Some("interrupted provider dispatch may have been charged"),
                )
                .await?;
            }
        }
        if uncertainty_recorded {
            crate::accounting::finalize_job_reservation(state, job_id).await?;
        }
    }
    Ok(())
}

async fn release_orphaned_admission_reservations(state: &AppState) -> Result<(), ServiceError> {
    let rows = sqlx::query(
        "SELECT r.id, r.job_id, r.usage_sequence_start FROM budget_reservations r \
         JOIN jobs j ON j.id = r.job_id \
         WHERE r.status IN ('active', 'expired') \
         AND (j.reservation_id IS NULL OR j.reservation_id != r.id)",
    )
    .fetch_all(state.database.pool())
    .await
    .map_err(storage_error)?;
    let mut released_any = false;
    for row in rows {
        let reservation_id =
            ReservationId::from_str(row.get::<&str, _>("id")).map_err(internal_error)?;
        let job_id = JobId::from_str(row.get::<&str, _>("job_id")).map_err(internal_error)?;
        let usage_sequence_start = row.get::<i64, _>("usage_sequence_start");
        let usage_count = sqlx::query_scalar::<_, i64>(
            "SELECT COUNT(*) FROM usage_ledger WHERE job_id = ? AND sequence > ?",
        )
        .bind(job_id.to_string())
        .bind(usage_sequence_start)
        .fetch_one(state.database.pool())
        .await
        .map_err(storage_error)?;
        if usage_count > 0 {
            tracing::warn!(diagnostic_code = "budget.admission.orphaned_with_usage", %job_id, %reservation_id, "an unattached reservation has usage and was retained for manual reconciliation");
            continue;
        }
        state
            .database
            .repositories()
            .budgets
            .release(reservation_id, Utc::now())
            .await
            .map_err(storage_error)?;
        released_any = true;
    }
    if released_any {
        crate::accounting::refresh_budget_views(state).await?;
    }
    Ok(())
}

async fn interrupted_paid_dispatches(
    state: &AppState,
    job_id: JobId,
) -> Result<Vec<JobUnit>, ServiceError> {
    Ok(state
        .database
        .repositories()
        .jobs
        .list_units(job_id)
        .await
        .map_err(storage_error)?
        .into_iter()
        .filter(|unit| {
            unit.kind == JobUnitKind::SynthesisSegment
                && matches!(unit.state, JobUnitState::Running | JobUnitState::Retrying)
        })
        .collect())
}

async fn record_interrupted_paid_unit_uncertainty(
    state: &AppState,
    unit: &JobUnit,
) -> Result<(), ServiceError> {
    let segment = unit
        .payload
        .get("segmentPlan")
        .cloned()
        .ok_or_else(|| {
            ServiceError::Conflict(
                "interrupted paid synthesis has no durable input snapshot".to_owned(),
            )
        })
        .and_then(|value| serde_json::from_value::<SegmentPlan>(value).map_err(internal_error))?;
    let existing = sqlx::query_scalar::<_, String>(
        "SELECT payload FROM job_attempts WHERE job_unit_id = ? AND uncertain_charge = 1 \
         ORDER BY ordinal DESC LIMIT 1",
    )
    .bind(unit.id.to_string())
    .fetch_optional(state.database.pool())
    .await
    .map_err(storage_error)?;
    let attempt = if let Some(payload) = existing {
        serde_json::from_str::<JobAttempt>(&payload).map_err(internal_error)?
    } else {
        let maximum = sqlx::query_scalar::<_, Option<i64>>(
            "SELECT MAX(ordinal) FROM job_attempts WHERE job_unit_id = ?",
        )
        .bind(unit.id.to_string())
        .fetch_one(state.database.pool())
        .await
        .map_err(storage_error)?
        .unwrap_or(0);
        let ordinal = u16::try_from(maximum.saturating_add(1)).map_err(|_| {
            ServiceError::Conflict(
                "interrupted regeneration exhausted its durable attempt counter".to_owned(),
            )
        })?;
        let now = Utc::now();
        let attempt = JobAttempt {
            id: AttemptId::new(),
            job_unit_id: unit.id,
            ordinal,
            started_at: unit.updated_at,
            finished_at: Some(now),
            failure_class: Some(audiobookai_core::FailureClass::TimeoutAfterDispatch),
            error_code: Some("application_interrupted_after_dispatch".to_owned()),
            redacted_error: Some(
                "application stopped before the paid provider response was durably recorded"
                    .to_owned(),
            ),
            provider_request_id: None,
            uncertain_charge: true,
        };
        state
            .database
            .repositories()
            .jobs
            .insert_attempt(&attempt)
            .await
            .map_err(storage_error)?;
        attempt
    };
    append_tts_usage(
        state,
        unit.job_id,
        &segment,
        Some(attempt.id),
        &ProviderUsage {
            source: UsageSource::Estimated,
            characters: u64::try_from(segment.text.chars().count()).ok(),
            ..ProviderUsage::default()
        },
        true,
        None,
    )
    .await
}

/// Persists a pause boundary for every active durable job before desktop shutdown.
pub async fn checkpoint_jobs_for_shutdown(state: Arc<AppState>) -> Result<usize, ServiceError> {
    let active = state
        .database
        .repositories()
        .jobs
        .list_active()
        .await
        .map_err(storage_error)?;
    let mut checkpointed = 0_usize;
    for job in active {
        if !matches!(
            job.kind,
            JobKind::Conversion
                | JobKind::CharacterDetection
                | JobKind::SegmentRegeneration
                | JobKind::Export
        ) {
            continue;
        }
        match job.state {
            JobState::Queued => {
                transition_job(
                    &state,
                    job.id,
                    JobState::Running,
                    "Preparing shutdown checkpoint",
                )
                .await?;
                transition_job(
                    &state,
                    job.id,
                    JobState::Pausing,
                    "Checkpointed for application shutdown",
                )
                .await?;
                if job.kind == JobKind::CharacterDetection {
                    crate::workflows::spawn_character_detection(
                        Arc::clone(&state),
                        job.id.as_uuid(),
                    );
                }
                checkpointed = checkpointed.saturating_add(1);
            }
            JobState::Running => {
                transition_job(
                    &state,
                    job.id,
                    JobState::Pausing,
                    "Checkpointed for application shutdown",
                )
                .await?;
                if job.kind == JobKind::CharacterDetection {
                    crate::workflows::spawn_character_detection(
                        Arc::clone(&state),
                        job.id.as_uuid(),
                    );
                }
                checkpointed = checkpointed.saturating_add(1);
            }
            JobState::Pausing => {
                checkpointed = checkpointed.saturating_add(1);
            }
            JobState::Paused
            | JobState::Cancelling
            | JobState::Cancelled
            | JobState::Failed
            | JobState::Completed => {}
        }
    }
    Ok(checkpointed)
}

fn storage_error(error: impl std::fmt::Display) -> ServiceError {
    ServiceError::Storage(error.to_string())
}

fn validate_export_input(input: &ExportOptionsInput) -> Result<(), ServiceError> {
    if !(32..=512).contains(&input.bitrate_kbps) {
        return Err(ServiceError::InvalidRequest(
            "audio bitrate must be between 32 and 512 kbps".to_owned(),
        ));
    }
    if !input.music_gain_db.is_finite() || !(-60.0..=0.0).contains(&input.music_gain_db) {
        return Err(ServiceError::InvalidRequest(
            "background music gain must be between -60 and 0 dB".to_owned(),
        ));
    }
    if input.background_music_path.is_some() && !input.confirm_background_music_owned {
        return Err(ServiceError::Conflict(
            "confirm that you own or are licensed to use the selected background audio".to_owned(),
        ));
    }
    Ok(())
}

// File ownership checks, managed input promotion, and durable profile creation
// stay in one ordered flow to avoid partially configured exports.
#[allow(clippy::too_many_lines)]
async fn create_export_profile(
    state: &AppState,
    job_id: JobId,
    project_id: Uuid,
    input: &ExportOptionsInput,
) -> Result<(ExportProfile, Option<PathBuf>), ServiceError> {
    let (project, settings) = {
        let catalog = state.catalog.read().await;
        (
            catalog
                .projects
                .get(&project_id)
                .cloned()
                .ok_or(ServiceError::NotFound)?,
            catalog.settings.clone(),
        )
    };
    let output_directory = input
        .output_directory
        .as_ref()
        .map_or_else(|| state.config.data_dir.join("exports"), PathBuf::from);
    if !output_directory.is_absolute() {
        return Err(ServiceError::InvalidRequest(
            "the export directory must be an absolute path".to_owned(),
        ));
    }
    ensure_output_directory_not_reserved(state, &output_directory).await?;
    tokio::fs::create_dir_all(&output_directory).await?;
    let output_directory = tokio::fs::canonicalize(&output_directory).await?;
    let file_name = safe_file_component(
        input
            .file_name
            .as_deref()
            .or(project.output_name.as_deref())
            .unwrap_or(&project.summary.title),
    );
    let format = core_export_format(input.format);
    let layout = if input.split_per_chapter {
        ExportLayout::PerChapter
    } else {
        ExportLayout::SingleFile
    };

    let (music_settings, music_path) = if let Some(source) = &input.background_music_path {
        let source = PathBuf::from(source);
        if !source.is_absolute() {
            return Err(ServiceError::InvalidRequest(
                "background music must use an absolute path".to_owned(),
            ));
        }
        let source = tokio::fs::canonicalize(source).await.map_err(|error| {
            ServiceError::InvalidRequest(format!("background music is unavailable: {error}"))
        })?;
        if !tokio::fs::metadata(&source).await?.is_file() {
            return Err(ServiceError::InvalidRequest(
                "background music is not a regular file".to_owned(),
            ));
        }
        let managed_directory = state
            .config
            .data_dir
            .join("jobs")
            .join(job_id.to_string())
            .join("inputs");
        tokio::fs::create_dir_all(&managed_directory).await?;
        let extension = source
            .extension()
            .and_then(std::ffi::OsStr::to_str)
            .unwrap_or("audio");
        let managed = managed_directory.join(format!("background.{extension}"));
        copy_file_atomically(&source, &managed).await?;
        let artifact = artifact_for_file(
            ArtifactKind::ReferenceAudio,
            &managed,
            Some(media_type_for_path(&managed)),
            None,
            None,
            Some(job_id),
        )
        .await?;
        persist_artifact(state, project_id, &artifact).await?;
        (
            Some(BackgroundMusicSettings {
                artifact_id: artifact.id,
                user_owned_confirmed: true,
                gain_db: input.music_gain_db,
                loop_audio: true,
                trim_start_ms: 0,
                trim_end_ms: None,
                fade_in_ms: 2_000,
                fade_out_ms: 3_000,
                ducking: input.ducking.then_some(DuckingSettings {
                    attenuation_db: -12.0,
                    attack_ms: 20,
                    release_ms: 500,
                    threshold_db: -30.0,
                }),
            }),
            Some(managed),
        )
    } else {
        (None, None)
    };

    let now = Utc::now();
    let profile = ExportProfile {
        id: ExportProfileId::new(),
        project_id: ProjectId::from_uuid(project_id),
        name: format!("{} {}", format_name(format), layout_name(layout)),
        format,
        layout,
        output_directory: output_directory.to_string_lossy().into_owned(),
        filename_template: file_name,
        audio: audiobookai_core::AudioEncodingSettings {
            sample_rate_hz: 48_000,
            channels: 1,
            bitrate_kbps: Some(u32::from(input.bitrate_kbps)),
            target_lufs: settings.default_lufs,
            true_peak_db: settings.default_true_peak_db,
        },
        background_music: music_settings,
        embed_cover: true,
        embed_chapters: !matches!(format, ExportFormat::Wav),
        write_sidecar_manifest: true,
        created_at: now,
        updated_at: now,
    };
    let issues = profile.validation_issues();
    if let Some(issue) = issues.first() {
        return Err(ServiceError::InvalidRequest(issue.message.clone()));
    }
    sqlx::query(
        "INSERT INTO export_profiles (id, project_id, name, format, layout, updated_at, payload) \
         VALUES (?, ?, ?, ?, ?, ?, ?)",
    )
    .bind(profile.id.to_string())
    .bind(profile.project_id.to_string())
    .bind(&profile.name)
    .bind(format_name(profile.format))
    .bind(layout_name(profile.layout))
    .bind(profile.updated_at.to_rfc3339())
    .bind(serde_json::to_string(&profile).map_err(internal_error)?)
    .execute(state.database.pool())
    .await
    .map_err(storage_error)?;
    Ok((profile, music_path))
}

fn internal_error(error: impl std::fmt::Display) -> ServiceError {
    ServiceError::Internal(error.to_string())
}

fn core_export_format(format: ExportFormatView) -> ExportFormat {
    match format {
        ExportFormatView::Mp3 => ExportFormat::Mp3,
        ExportFormatView::Wav => ExportFormat::Wav,
        ExportFormatView::M4a => ExportFormat::M4a,
        ExportFormatView::M4b => ExportFormat::M4b,
    }
}

const fn media_export_format(format: ExportFormat) -> MediaExportFormat {
    match format {
        ExportFormat::Mp3 => MediaExportFormat::Mp3,
        ExportFormat::Wav => MediaExportFormat::Wav,
        ExportFormat::M4a => MediaExportFormat::M4a,
        ExportFormat::M4b => MediaExportFormat::M4b,
    }
}

const fn format_name(format: ExportFormat) -> &'static str {
    match format {
        ExportFormat::Mp3 => "mp3",
        ExportFormat::Wav => "wav",
        ExportFormat::M4a => "m4a",
        ExportFormat::M4b => "m4b",
    }
}

const fn layout_name(layout: ExportLayout) -> &'static str {
    match layout {
        ExportLayout::SingleFile => "single_file",
        ExportLayout::PerChapter => "per_chapter",
    }
}

fn safe_file_component(value: &str) -> String {
    let value = value
        .chars()
        .map(|character| {
            if character.is_control()
                || matches!(
                    character,
                    '/' | '\\' | ':' | '*' | '?' | '"' | '<' | '>' | '|'
                )
            {
                '_'
            } else {
                character
            }
        })
        .collect::<String>();
    let value = value.trim().trim_end_matches(['.', ' ']);
    if value.is_empty() {
        "Audiobook".to_owned()
    } else {
        value.chars().take(120).collect()
    }
}

async fn load_conversion_plan(
    state: &AppState,
    project_id: Uuid,
    export: ExportProfile,
    music_path: Option<PathBuf>,
) -> Result<ConversionPlan, ServiceError> {
    let repositories = state.database.repositories();
    let project = repositories
        .projects
        .get_project(ProjectId::from_uuid(project_id))
        .await
        .map_err(storage_error)?
        .ok_or(ServiceError::NotFound)?;
    let book = repositories
        .projects
        .get_book(project.book_id)
        .await
        .map_err(storage_error)?
        .ok_or(ServiceError::NotFound)?;
    let selected = repositories
        .projects
        .list_chapters(book.id)
        .await
        .map_err(storage_error)?
        .into_iter()
        .filter(|chapter| chapter.selected)
        .collect::<Vec<_>>();
    if selected.is_empty() {
        return Err(ServiceError::Conflict(
            "select at least one chapter before conversion".to_owned(),
        ));
    }

    let (characters, voices, providers, rules) = {
        let catalog = state.catalog.read().await;
        (
            catalog
                .characters
                .get(&project_id)
                .cloned()
                .unwrap_or_default(),
            catalog.voice_sources.clone(),
            catalog.providers.clone(),
            catalog.pronunciation_rules.clone(),
        )
    };
    if !matches!(project.status, audiobookai_core::ProjectStatus::Ready)
        || project.character_reviewed_at.is_none()
    {
        return Err(ServiceError::Conflict(
            "approve the character review before paid synthesis".to_owned(),
        ));
    }
    let assignments = build_assignments(&project, &characters, &voices, &providers, state).await?;
    let narrator_id = characters
        .iter()
        .find(|character| matches!(character.role, audiobookai_core::CharacterRole::Narrator))
        .map(|character| character.id)
        .ok_or_else(|| {
            ServiceError::Conflict("the reviewed character set has no narrator".to_owned())
        })?;
    let detection_spans = load_detection_spans(state, project_id).await?;
    let overrides = load_speaker_overrides(state, project_id).await?;
    let mut chapter_plans = Vec::with_capacity(selected.len());
    let mut playback_ordinal = 0_usize;
    for chapter in selected {
        let paragraphs = repositories
            .projects
            .list_paragraphs(chapter.id)
            .await
            .map_err(storage_error)?;
        let mut segments = segment_chapter(
            &project,
            &chapter,
            &paragraphs,
            narrator_id,
            &assignments,
            &detection_spans,
            &overrides,
            &rules,
        )?;
        if segments.is_empty() {
            return Err(ServiceError::Conflict(format!(
                "chapter '{}' contains no speakable text",
                chapter.title
            )));
        }
        for segment in &mut segments {
            segment.playback_ordinal = playback_ordinal;
            playback_ordinal = playback_ordinal.saturating_add(1);
        }
        chapter_plans.push(ChapterPlan { chapter, segments });
    }
    Ok(ConversionPlan {
        project,
        book,
        chapters: chapter_plans,
        rules,
        export,
        music_path,
    })
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum AssignmentPurpose {
    Semantic,
    Dispatch,
}

async fn build_assignments(
    project: &Project,
    characters: &[crate::models::CharacterView],
    voice_sources: &HashMap<Uuid, String>,
    providers: &HashMap<Uuid, ProviderProfileView>,
    state: &AppState,
) -> Result<HashMap<Uuid, SpeakerAssignment>, ServiceError> {
    build_assignments_for(
        project,
        characters,
        voice_sources,
        providers,
        state,
        AssignmentPurpose::Dispatch,
    )
    .await
}

async fn build_assignments_for(
    project: &Project,
    characters: &[crate::models::CharacterView],
    voice_sources: &HashMap<Uuid, String>,
    providers: &HashMap<Uuid, ProviderProfileView>,
    state: &AppState,
    purpose: AssignmentPurpose,
) -> Result<HashMap<Uuid, SpeakerAssignment>, ServiceError> {
    let mut result = HashMap::new();
    for character in characters {
        let assignment = character.voice_assignment.as_ref().ok_or_else(|| {
            ServiceError::Conflict(format!(
                "assign a voice to '{}' before conversion",
                character.canonical_name
            ))
        })?;
        let provider = providers
            .get(&assignment.provider_profile_id)
            .ok_or_else(|| {
                ServiceError::Conflict(format!(
                    "the provider assigned to '{}' no longer exists",
                    character.canonical_name
                ))
            })?;
        if purpose == AssignmentPurpose::Dispatch {
            crate::api::validate_billable_tts_provider_readiness(provider)?;
            if matches!(provider.mode, ProviderModeView::CloudRemote)
                && !project.cloud_consent.book_text
            {
                return Err(ServiceError::Conflict(format!(
                    "grant project consent before sending book text to {}",
                    provider.name
                )));
            }
        }
        let voice_source = voice_sources
            .get(&assignment.voice_id)
            .cloned()
            .ok_or_else(|| {
                ServiceError::Conflict(format!(
                    "the voice assigned to '{}' is no longer available",
                    character.canonical_name
                ))
            })?;
        let domain_provider = state
            .database
            .repositories()
            .providers
            .get(ProviderProfileId::from_uuid(assignment.provider_profile_id))
            .await
            .map_err(storage_error)?;
        let concurrency = domain_provider
            .as_ref()
            .map_or(1, audiobookai_core::ProviderProfile::effective_concurrency);
        let provider_version = domain_provider
            .as_ref()
            .and_then(|profile| profile.capability_snapshot.as_ref())
            .and_then(|snapshot| snapshot.provider_version.clone());
        let provider_snapshot_id = domain_provider
            .as_ref()
            .and_then(|profile| profile.capability_snapshot.as_ref())
            .map(|snapshot| snapshot.id.as_uuid());
        let model = assignment.model.clone().or_else(|| provider.model.clone());
        if purpose == AssignmentPurpose::Dispatch {
            crate::api::validate_voice_direction(
                &assignment.performance,
                &assignment.timing,
                model.as_deref(),
                provider.capabilities.as_ref(),
            )?;
        } else {
            if let Some(issue) = assignment
                .performance
                .validation_issues()
                .into_iter()
                .next()
            {
                return Err(ServiceError::InvalidRequest(issue.message));
            }
            if let Some(issue) = assignment.timing.validation_issues().into_iter().next() {
                return Err(ServiceError::InvalidRequest(issue.message));
            }
        }
        result.insert(
            character.id,
            SpeakerAssignment {
                character_id: character.id,
                character_name: character.canonical_name.clone(),
                provider_id: assignment.provider_profile_id,
                provider_name: provider.name.clone(),
                provider_kind: provider.kind.clone(),
                provider_mode: Some(provider.mode),
                provider_endpoint: provider.endpoint.clone(),
                provider_snapshot_id,
                provider_version,
                provider_concurrency: concurrency,
                voice_id: assignment.voice_id,
                voice_source,
                voice_name: assignment.voice_name.clone(),
                model,
                performance: assignment.performance.clone(),
                timing: assignment.timing.clone(),
            },
        );
    }
    Ok(result)
}

async fn validate_segment_dispatch_boundary(
    state: &AppState,
    project_id: Uuid,
    segment: &SegmentPlan,
) -> Result<(), ProviderError> {
    let (profile, voice_source, voice_belongs_to_provider, cloud_text_consent) = {
        let catalog = state.catalog.read().await;
        let profile = catalog
            .providers
            .get(&segment.assignment.provider_id)
            .cloned()
            .ok_or_else(|| ProviderError::Configuration("TTS provider was removed".to_owned()))?;
        let voice_source = catalog
            .voice_sources
            .get(&segment.assignment.voice_id)
            .cloned();
        let voice_belongs_to_provider = catalog.voices.iter().any(|voice| {
            voice.id == segment.assignment.voice_id
                && voice.provider_profile_id == segment.assignment.provider_id
        });
        let cloud_text_consent = catalog
            .projects
            .get(&project_id)
            .is_some_and(|project| project.consent_cloud_text);
        (
            profile,
            voice_source,
            voice_belongs_to_provider,
            cloud_text_consent,
        )
    };
    crate::api::validate_billable_tts_provider_readiness(&profile)
        .map_err(|error| ProviderError::Configuration(error.to_string()))?;
    if profile.kind != segment.assignment.provider_kind
        || Some(profile.mode) != segment.assignment.provider_mode
        || profile.endpoint != segment.assignment.provider_endpoint
    {
        return Err(ProviderError::Configuration(
            "TTS provider routing changed after this job was admitted".to_owned(),
        ));
    }
    if matches!(profile.mode, ProviderModeView::CloudRemote) && !cloud_text_consent {
        return Err(ProviderError::Configuration(
            "cloud-text consent is not active for this dispatch".to_owned(),
        ));
    }
    if !voice_belongs_to_provider
        || voice_source.as_deref() != Some(segment.assignment.voice_source.as_str())
    {
        return Err(ProviderError::Configuration(
            "the selected provider voice changed after this job was admitted".to_owned(),
        ));
    }
    crate::api::validate_voice_direction(
        &segment.assignment.performance,
        &segment.assignment.timing,
        segment.assignment.model.as_deref(),
        profile.capabilities.as_ref(),
    )
    .map_err(|error| ProviderError::Configuration(error.to_string()))?;

    let current_snapshot_id = state
        .database
        .repositories()
        .providers
        .get(ProviderProfileId::from_uuid(segment.assignment.provider_id))
        .await
        .map_err(|error| ProviderError::Process(error.to_string()))?
        .and_then(|provider| provider.capability_snapshot)
        .map(|snapshot| snapshot.id.as_uuid());
    if segment.assignment.provider_snapshot_id.is_none()
        || current_snapshot_id != segment.assignment.provider_snapshot_id
    {
        return Err(ProviderError::Configuration(
            "TTS provider capability or credential snapshot changed after this job was admitted"
                .to_owned(),
        ));
    }
    Ok(())
}

async fn load_detection_spans(
    state: &AppState,
    project_id: Uuid,
) -> Result<HashMap<Uuid, Vec<DialogueSpan>>, ServiceError> {
    let run_id = sqlx::query_scalar::<_, String>(
        "SELECT id FROM detection_runs WHERE project_id = ? AND status = 'completed' \
         ORDER BY completed_at DESC, created_at DESC LIMIT 1",
    )
    .bind(project_id.to_string())
    .fetch_optional(state.database.pool())
    .await
    .map_err(storage_error)?;
    let Some(run_id) = run_id else {
        return Ok(HashMap::new());
    };
    let rows = sqlx::query(
        "SELECT paragraph_id, payload FROM dialogue_spans WHERE detection_run_id = ? \
         ORDER BY paragraph_id, byte_start, byte_end",
    )
    .bind(run_id)
    .fetch_all(state.database.pool())
    .await
    .map_err(storage_error)?;
    let mut spans = HashMap::<Uuid, Vec<DialogueSpan>>::new();
    for row in rows {
        let paragraph_id = row.get::<String, _>("paragraph_id");
        let Ok(paragraph_id) = Uuid::parse_str(&paragraph_id) else {
            continue;
        };
        let payload = row.get::<String, _>("payload");
        if let Ok(span) = serde_json::from_str::<DialogueSpan>(&payload) {
            spans.entry(paragraph_id).or_default().push(span);
        }
    }
    Ok(spans)
}

async fn load_speaker_overrides(
    state: &AppState,
    project_id: Uuid,
) -> Result<HashMap<Uuid, Vec<SpeakerOverride>>, ServiceError> {
    let rows = sqlx::query(
        "SELECT paragraph_id, payload FROM speaker_overrides WHERE project_id = ? \
         ORDER BY paragraph_id, byte_start, byte_end, updated_at",
    )
    .bind(project_id.to_string())
    .fetch_all(state.database.pool())
    .await
    .map_err(storage_error)?;
    let mut overrides = HashMap::<Uuid, Vec<SpeakerOverride>>::new();
    for row in rows {
        let paragraph_id = row.get::<String, _>("paragraph_id");
        let Ok(paragraph_id) = Uuid::parse_str(&paragraph_id) else {
            continue;
        };
        let payload = row.get::<String, _>("payload");
        if let Ok(value) = serde_json::from_str::<SpeakerOverride>(&payload) {
            overrides.entry(paragraph_id).or_default().push(value);
        }
    }
    Ok(overrides)
}

#[allow(clippy::too_many_arguments)]
fn segment_chapter(
    project: &Project,
    chapter: &Chapter,
    paragraphs: &[Paragraph],
    narrator_id: Uuid,
    assignments: &HashMap<Uuid, SpeakerAssignment>,
    detection_spans: &HashMap<Uuid, Vec<DialogueSpan>>,
    overrides: &HashMap<Uuid, Vec<SpeakerOverride>>,
    rules: &[PronunciationRuleView],
) -> Result<Vec<SegmentPlan>, ServiceError> {
    let mut output = Vec::new();
    for paragraph in paragraphs {
        if paragraph.text.trim().is_empty() {
            continue;
        }
        let paragraph_id = paragraph.id.as_uuid();
        let valid_overrides = overrides
            .get(&paragraph_id)
            .into_iter()
            .flatten()
            .filter(|value| value.source_content_hash == paragraph.content_hash)
            .collect::<Vec<_>>();
        let detected = detection_spans
            .get(&paragraph_id)
            .map(Vec::as_slice)
            .unwrap_or_default();
        let ranges = speaker_ranges(
            &paragraph.text,
            narrator_id,
            detected,
            &valid_overrides,
            assignments,
        );
        for (start, end, character_id) in ranges {
            let Some(text) = paragraph.text.get(start..end) else {
                continue;
            };
            let original_text = text.trim();
            if original_text.is_empty() {
                continue;
            }
            let assignment = assignments.get(&character_id).cloned().ok_or_else(|| {
                ServiceError::Conflict(format!(
                    "a speaker in '{}' has no valid voice assignment",
                    chapter.title
                ))
            })?;
            let (text, applied_rule_ids, dictionary_revision) = apply_pronunciation_rules(
                original_text,
                rules,
                project.id.as_uuid(),
                character_id,
                project.metadata.language.as_deref(),
            )?;
            let segment_ordinal = u32::try_from(output.len()).unwrap_or(u32::MAX);
            let key = segment_key(chapter.id.as_uuid(), paragraph_id, start, end, character_id);
            output.push(SegmentPlan {
                id: SegmentId::new(),
                proofing: true,
                key,
                chapter_id: chapter.id.as_uuid(),
                paragraph_id,
                source_content_hash: paragraph.content_hash.clone(),
                byte_start: u64::try_from(start).unwrap_or(u64::MAX),
                byte_end: u64::try_from(end).unwrap_or(u64::MAX),
                chapter_title: chapter.title.clone(),
                segment_ordinal,
                playback_ordinal: 0,
                original_text: original_text.to_owned(),
                text,
                context: None,
                assignment,
                applied_rule_ids,
                dictionary_revision,
            });
        }
    }
    let contexts = output
        .iter()
        .map(|segment| segment.text.clone())
        .collect::<Vec<_>>();
    for (index, segment) in output.iter_mut().enumerate() {
        let before = index
            .checked_sub(1)
            .and_then(|previous| contexts.get(previous))
            .map(|value| trailing_characters(value, 160));
        let after = contexts
            .get(index.saturating_add(1))
            .map(|value| leading_characters(value, 160));
        segment.context = match (before, after) {
            (Some(before), Some(after)) => Some(format!("{before}\n---\n{after}")),
            (Some(value), None) | (None, Some(value)) => Some(value),
            (None, None) => None,
        };
    }
    Ok(output)
}

fn speaker_ranges(
    text: &str,
    narrator_id: Uuid,
    detected: &[DialogueSpan],
    overrides: &[&SpeakerOverride],
    assignments: &HashMap<Uuid, SpeakerAssignment>,
) -> Vec<(usize, usize, Uuid)> {
    #[derive(Clone, Copy)]
    struct Span {
        start: usize,
        end: usize,
        speaker: Uuid,
        manual: bool,
    }
    let mut spans = Vec::new();
    for detected in detected {
        let speaker = detected.character_id.as_uuid();
        if !assignments.contains_key(&speaker) {
            continue;
        }
        if let Some((start, end)) = valid_text_range(text, detected.byte_start, detected.byte_end) {
            spans.push(Span {
                start,
                end,
                speaker,
                manual: false,
            });
        }
    }
    for value in overrides {
        let Some(speaker) = speaker_character_id(&value.speaker, narrator_id, assignments) else {
            continue;
        };
        if let Some((start, end)) = valid_text_range(text, value.byte_start, value.byte_end) {
            spans.push(Span {
                start,
                end,
                speaker,
                manual: true,
            });
        }
    }
    let mut boundaries = BTreeSet::from([0, text.len()]);
    for span in &spans {
        boundaries.insert(span.start);
        boundaries.insert(span.end);
    }
    let boundaries = boundaries.into_iter().collect::<Vec<_>>();
    let mut ranges = Vec::<(usize, usize, Uuid)>::new();
    for pair in boundaries.windows(2) {
        let start = pair[0];
        let end = pair[1];
        if start == end {
            continue;
        }
        let speaker = spans
            .iter()
            .filter(|span| span.start <= start && span.end >= end)
            .max_by_key(|span| (span.manual, span.start, std::cmp::Reverse(span.end)))
            .map_or(narrator_id, |span| span.speaker);
        if let Some(last) = ranges.last_mut()
            && last.2 == speaker
            && last.1 == start
        {
            last.1 = end;
        } else {
            ranges.push((start, end, speaker));
        }
    }
    ranges
}

fn speaker_character_id(
    speaker: &Speaker,
    narrator_id: Uuid,
    assignments: &HashMap<Uuid, SpeakerAssignment>,
) -> Option<Uuid> {
    match speaker {
        Speaker::Narrator => Some(narrator_id),
        Speaker::Character(id) => assignments
            .contains_key(&id.as_uuid())
            .then(|| id.as_uuid()),
        Speaker::Named(name) => assignments
            .values()
            .find(|assignment| assignment.character_name.eq_ignore_ascii_case(name))
            .map(|assignment| assignment.character_id),
    }
}

fn valid_text_range(text: &str, start: u64, end: u64) -> Option<(usize, usize)> {
    let mut start = usize::try_from(start).ok()?.min(text.len());
    let mut end = usize::try_from(end).ok()?.min(text.len());
    while start < text.len() && !text.is_char_boundary(start) {
        start = start.saturating_add(1);
    }
    while end > start && !text.is_char_boundary(end) {
        end = end.saturating_sub(1);
    }
    (start < end).then_some((start, end))
}

fn segment_key(
    chapter_id: Uuid,
    paragraph_id: Uuid,
    start: usize,
    end: usize,
    speaker_id: Uuid,
) -> String {
    let mut hasher = blake3::Hasher::new();
    for value in [
        chapter_id.to_string(),
        paragraph_id.to_string(),
        start.to_string(),
        end.to_string(),
        speaker_id.to_string(),
    ] {
        hasher.update(value.as_bytes());
        hasher.update(&[0]);
    }
    hasher.finalize().to_hex().to_string()
}

fn leading_characters(value: &str, count: usize) -> String {
    value.chars().take(count).collect()
}

fn trailing_characters(value: &str, count: usize) -> String {
    let mut characters = value.chars().rev().take(count).collect::<Vec<_>>();
    characters.reverse();
    characters.into_iter().collect()
}

pub(crate) fn apply_pronunciation_rules(
    text: &str,
    rules: &[PronunciationRuleView],
    project_id: Uuid,
    character_id: Uuid,
    language: Option<&str>,
) -> Result<(String, Vec<Uuid>, String), ServiceError> {
    let mut applicable = rules
        .iter()
        .filter(|rule| {
            rule.enabled
                && match rule.scope {
                    PronunciationScopeView::Global => true,
                    PronunciationScopeView::Project => rule.project_id == Some(project_id),
                }
                && rule.character_id.is_none_or(|id| id == character_id)
                && rule.language.as_deref().is_none_or(|rule_language| {
                    language.is_some_and(|value| value.eq_ignore_ascii_case(rule_language))
                })
        })
        .collect::<Vec<_>>();
    applicable.sort_by_key(|rule| {
        (
            matches!(rule.scope, PronunciationScopeView::Project),
            rule.order,
            rule.id,
        )
    });
    let mut transformed = text.to_owned();
    let mut applied = Vec::new();
    let mut revision = blake3::Hasher::new();
    for rule in applicable {
        revision.update(rule.id.as_bytes());
        revision.update(rule.source.as_bytes());
        revision.update(rule.replacement.as_bytes());
        let pattern = match rule.kind {
            PronunciationKindView::Literal | PronunciationKindView::Phoneme => {
                regex::escape(&rule.source)
            }
            PronunciationKindView::WholeWord | PronunciationKindView::Alias => {
                format!(r"\b{}\b", regex::escape(&rule.source))
            }
            PronunciationKindView::Regex => rule.source.clone(),
        };
        let expression = regex::RegexBuilder::new(&pattern)
            .case_insensitive(!rule.case_sensitive)
            .unicode(true)
            .build()
            .map_err(|error| {
                ServiceError::InvalidRequest(format!("invalid pronunciation regex: {error}"))
            })?;
        let before = transformed.clone();
        transformed = if matches!(rule.kind, PronunciationKindView::Regex) {
            expression
                .replace_all(&transformed, rule.replacement.as_str())
                .into_owned()
        } else {
            expression
                .replace_all(&transformed, |_captures: &regex::Captures<'_>| {
                    rule.replacement.as_str()
                })
                .into_owned()
        };
        if transformed != before {
            applied.push(rule.id);
        }
    }
    Ok((
        transformed,
        applied,
        revision.finalize().to_hex().to_string(),
    ))
}

// The dependency graph is built as one cohesive value so every edge remains
// visible and cannot drift across partially shared helper state.
#[allow(clippy::too_many_lines)]
fn build_job_units(job_id: JobId, plan: &ConversionPlan) -> PersistedUnitPlan {
    let now = Utc::now();
    let mut synthesis = HashMap::new();
    let mut synthesis_by_chapter = HashMap::<Uuid, Vec<JobUnitId>>::new();
    for chapter in &plan.chapters {
        for segment in &chapter.segments {
            let take_id = SegmentTakeId::new();
            let take_artifact_id = ArtifactId::new();
            let unit = JobUnit {
                id: JobUnitId::new(),
                job_id,
                kind: JobUnitKind::SynthesisSegment,
                state: JobUnitState::Ready,
                chapter_id: Some(ChapterId::from_uuid(segment.chapter_id)),
                segment_id: segment.proofing.then_some(segment.id),
                provider_profile_id: Some(ProviderProfileId::from_uuid(
                    segment.assignment.provider_id,
                )),
                dependencies: Vec::new(),
                attempt_count: 0,
                next_attempt_at: None,
                output_artifact_id: None,
                payload: BTreeMap::from([
                    ("segmentKey".to_owned(), serde_json::json!(segment.key)),
                    ("takeId".to_owned(), serde_json::json!(take_id)),
                    (
                        "takeArtifactId".to_owned(),
                        serde_json::json!(take_artifact_id),
                    ),
                    ("cacheOperation".to_owned(), serde_json::json!("conversion")),
                    ("autoSelect".to_owned(), serde_json::json!(true)),
                    (
                        "segmentPlan".to_owned(),
                        serde_json::to_value(segment)
                            .expect("a validated conversion segment plan is serializable"),
                    ),
                    (
                        "title".to_owned(),
                        serde_json::json!(format!(
                            "{} — {}",
                            segment.chapter_title, segment.assignment.character_name
                        )),
                    ),
                    ("progress".to_owned(), serde_json::json!(0.0)),
                ]),
                created_at: now,
                updated_at: now,
            };
            synthesis_by_chapter
                .entry(segment.chapter_id)
                .or_default()
                .push(unit.id);
            synthesis.insert(segment.key.clone(), unit);
        }
    }
    let mut assembly = HashMap::new();
    for chapter in &plan.chapters {
        let unit = JobUnit {
            id: JobUnitId::new(),
            job_id,
            kind: JobUnitKind::ChapterAssembly,
            state: JobUnitState::Blocked,
            chapter_id: Some(chapter.chapter.id),
            segment_id: None,
            provider_profile_id: None,
            dependencies: synthesis_by_chapter
                .remove(&chapter.chapter.id.as_uuid())
                .unwrap_or_default(),
            attempt_count: 0,
            next_attempt_at: None,
            output_artifact_id: None,
            payload: BTreeMap::from([
                (
                    "title".to_owned(),
                    serde_json::json!(format!("Assemble {}", chapter.chapter.title)),
                ),
                ("progress".to_owned(), serde_json::json!(0.0)),
            ]),
            created_at: now,
            updated_at: now,
        };
        assembly.insert(chapter.chapter.id.as_uuid(), unit);
    }
    let assembly_ids = assembly.values().map(|unit| unit.id).collect::<Vec<_>>();
    let mix = plan.export.background_music.as_ref().map(|_| JobUnit {
        id: JobUnitId::new(),
        job_id,
        kind: JobUnitKind::MusicMix,
        state: JobUnitState::Blocked,
        chapter_id: None,
        segment_id: None,
        provider_profile_id: None,
        dependencies: assembly_ids.clone(),
        attempt_count: 0,
        next_attempt_at: None,
        output_artifact_id: None,
        payload: BTreeMap::from([
            (
                "title".to_owned(),
                serde_json::json!("Mix background music"),
            ),
            ("progress".to_owned(), serde_json::json!(0.0)),
        ]),
        created_at: now,
        updated_at: now,
    });
    let normalize_dependencies = mix
        .as_ref()
        .map_or_else(|| assembly_ids.clone(), |unit| vec![unit.id]);
    let normalize = JobUnit {
        id: JobUnitId::new(),
        job_id,
        kind: JobUnitKind::Normalization,
        state: JobUnitState::Blocked,
        chapter_id: None,
        segment_id: None,
        provider_profile_id: None,
        dependencies: normalize_dependencies,
        attempt_count: 0,
        next_attempt_at: None,
        output_artifact_id: None,
        payload: BTreeMap::from([
            (
                "title".to_owned(),
                serde_json::json!("Measure final loudness"),
            ),
            ("progress".to_owned(), serde_json::json!(0.0)),
        ]),
        created_at: now,
        updated_at: now,
    };
    let export = JobUnit {
        id: JobUnitId::new(),
        job_id,
        kind: JobUnitKind::FinalExport,
        state: JobUnitState::Blocked,
        chapter_id: None,
        segment_id: None,
        provider_profile_id: None,
        dependencies: vec![normalize.id],
        attempt_count: 0,
        next_attempt_at: None,
        output_artifact_id: None,
        payload: BTreeMap::from([
            ("title".to_owned(), serde_json::json!("Export audiobook")),
            ("progress".to_owned(), serde_json::json!(0.0)),
        ]),
        created_at: now,
        updated_at: now,
    };
    PersistedUnitPlan {
        synthesis,
        assembly,
        mix,
        normalize,
        export,
    }
}

fn unit_count(units: &PersistedUnitPlan) -> usize {
    units.synthesis.len() + units.assembly.len() + 2 + usize::from(units.mix.is_some())
}

fn ordered_units(units: &PersistedUnitPlan) -> Vec<JobUnit> {
    let mut ordered = Vec::with_capacity(unit_count(units));
    let mut synthesis = units.synthesis.values().cloned().collect::<Vec<_>>();
    synthesis.sort_by_key(|unit| unit.id);
    ordered.extend(synthesis);
    let mut assembly = units.assembly.values().cloned().collect::<Vec<_>>();
    assembly.sort_by_key(|unit| unit.id);
    ordered.extend(assembly);
    if let Some(unit) = &units.mix {
        ordered.push(unit.clone());
    }
    ordered.push(units.normalize.clone());
    ordered.push(units.export.clone());
    ordered
}

async fn load_unit_plan(
    state: &AppState,
    job_id: JobId,
    plan: &ConversionPlan,
) -> Result<PersistedUnitPlan, ServiceError> {
    let existing = state
        .database
        .repositories()
        .jobs
        .list_units(job_id)
        .await
        .map_err(storage_error)?;
    if existing.is_empty() {
        return Err(ServiceError::Conflict(
            "the durable job graph is missing; the job was not admitted and cannot be resumed"
                .to_owned(),
        ));
    }
    let mut synthesis = HashMap::new();
    let mut assembly = HashMap::new();
    let mut mix = None;
    let mut normalize = None;
    let mut export = None;
    for unit in existing {
        match unit.kind {
            JobUnitKind::SynthesisSegment => {
                if let Some(key) = unit
                    .payload
                    .get("segmentKey")
                    .and_then(serde_json::Value::as_str)
                {
                    synthesis.insert(key.to_owned(), unit);
                }
            }
            JobUnitKind::ChapterAssembly => {
                if let Some(chapter_id) = unit.chapter_id {
                    assembly.insert(chapter_id.as_uuid(), unit);
                }
            }
            JobUnitKind::MusicMix => mix = Some(unit),
            JobUnitKind::Normalization => normalize = Some(unit),
            JobUnitKind::FinalExport => export = Some(unit),
            JobUnitKind::DetectionBatch | JobUnitKind::QualityControl => {}
        }
    }
    let expected_keys = plan
        .chapters
        .iter()
        .flat_map(|chapter| chapter.segments.iter().map(|segment| segment.key.clone()))
        .collect::<BTreeSet<_>>();
    if synthesis.keys().cloned().collect::<BTreeSet<_>>() != expected_keys
        || assembly.len() != plan.chapters.len()
        || normalize.is_none()
        || export.is_none()
        || mix.is_some() != plan.export.background_music.is_some()
    {
        return Err(ServiceError::Conflict(
            "the durable job graph no longer matches the reviewed project; start a new conversion"
                .to_owned(),
        ));
    }
    for segment in plan
        .chapters
        .iter()
        .flat_map(|chapter| chapter.segments.iter())
    {
        let unit = synthesis
            .get(&segment.key)
            .expect("the durable synthesis key set was checked above");
        validate_durable_segment_snapshot(unit, segment)?;
    }
    Ok(PersistedUnitPlan {
        synthesis,
        assembly,
        mix,
        normalize: normalize.expect("checked above"),
        export: export.expect("checked above"),
    })
}

fn validate_durable_segment_snapshot(
    unit: &JobUnit,
    current: &SegmentPlan,
) -> Result<(), ServiceError> {
    let snapshot = unit
        .payload
        .get("segmentPlan")
        .cloned()
        .ok_or_else(|| {
            ServiceError::Conflict(
                "the conversion predates durable narration snapshots; start a new conversion"
                    .to_owned(),
            )
        })
        .and_then(|value| serde_json::from_value::<SegmentPlan>(value).map_err(internal_error))?;
    let operation = unit
        .payload
        .get("cacheOperation")
        .and_then(serde_json::Value::as_str)
        .unwrap_or("conversion");
    let semantic_matches =
        segment_semantic_input_hash(&snapshot)? == segment_semantic_input_hash(current)?;
    let provider_identity_matches = snapshot.assignment.provider_kind
        == current.assignment.provider_kind
        && snapshot.assignment.provider_mode == current.assignment.provider_mode
        && snapshot.assignment.provider_endpoint == current.assignment.provider_endpoint
        && snapshot.assignment.provider_snapshot_id == current.assignment.provider_snapshot_id;
    let snapshot_cache_key = segment_cache_fingerprint(&snapshot, operation)
        .key()
        .map_err(media_error)?;
    let current_cache_key = segment_cache_fingerprint(current, operation)
        .key()
        .map_err(media_error)?;
    if !semantic_matches || !provider_identity_matches || snapshot_cache_key != current_cache_key {
        return Err(ServiceError::Conflict(
            "the narration inputs changed after this job was admitted; start a new conversion"
                .to_owned(),
        ));
    }
    Ok(())
}

fn job_view(job: &Job, title: &str, units: &PersistedUnitPlan) -> JobView {
    let mut views = units
        .synthesis
        .values()
        .chain(units.assembly.values())
        .chain(units.mix.iter())
        .chain(std::iter::once(&units.normalize))
        .chain(std::iter::once(&units.export))
        .map(unit_view)
        .collect::<Vec<_>>();
    views.sort_by_key(|unit| match unit.stage {
        JobStageView::Detect => 0,
        JobStageView::Synthesize => 1,
        JobStageView::Assemble => 2,
        JobStageView::Mix => 3,
        JobStageView::Normalize => 4,
        JobStageView::Export => 5,
        JobStageView::QualityControl => 6,
    });
    JobView {
        id: job.id.as_uuid(),
        project_id: job.project_id.as_uuid(),
        project_title: title.to_owned(),
        kind: match job.kind {
            JobKind::CharacterDetection => crate::models::JobKindView::CharacterDetection,
            JobKind::Preview => crate::models::JobKindView::Preview,
            JobKind::Conversion => crate::models::JobKindView::Conversion,
            JobKind::SegmentRegeneration => crate::models::JobKindView::SegmentRegeneration,
            JobKind::Export => crate::models::JobKindView::Export,
            JobKind::QualityControl => crate::models::JobKindView::QualityControl,
            JobKind::CacheCleanup => crate::models::JobKindView::CacheCleanup,
        },
        status: job_status_view(job.state),
        progress: progress_ratio(job.progress_completed, job.progress_total),
        current_stage: job.status_message.clone(),
        started_at: job.started_at,
        updated_at: job.updated_at,
        estimated_remaining_seconds: None,
        units: views,
        progressive_playback_url: Some(format!("/api/v1/jobs/{}/playback", job.id)),
        uncertain_charge: false,
    }
}

fn unit_view(unit: &JobUnit) -> JobUnitView {
    JobUnitView {
        id: unit.id.as_uuid(),
        title: unit
            .payload
            .get("title")
            .and_then(serde_json::Value::as_str)
            .unwrap_or("Conversion step")
            .to_owned(),
        stage: match unit.kind {
            JobUnitKind::DetectionBatch => JobStageView::Detect,
            JobUnitKind::SynthesisSegment => JobStageView::Synthesize,
            JobUnitKind::ChapterAssembly => JobStageView::Assemble,
            JobUnitKind::MusicMix => JobStageView::Mix,
            JobUnitKind::Normalization => JobStageView::Normalize,
            JobUnitKind::FinalExport => JobStageView::Export,
            JobUnitKind::QualityControl => JobStageView::QualityControl,
        },
        status: unit_status_view(unit.state),
        progress: if unit.state == JobUnitState::Completed {
            100.0
        } else {
            unit.payload
                .get("progress")
                .and_then(serde_json::Value::as_f64)
                .map_or(0.0, unit_interval_f32)
                * 100.0
        },
        attempt: u32::from(unit.attempt_count),
        last_error: unit
            .payload
            .get("lastError")
            .and_then(serde_json::Value::as_str)
            .map(str::to_owned),
    }
}

const fn job_status_view(state: JobState) -> JobStatusView {
    match state {
        JobState::Queued => JobStatusView::Queued,
        JobState::Running => JobStatusView::Running,
        JobState::Pausing => JobStatusView::Pausing,
        JobState::Cancelling => JobStatusView::Cancelling,
        JobState::Paused => JobStatusView::Paused,
        JobState::Cancelled => JobStatusView::Cancelled,
        JobState::Failed => JobStatusView::Failed,
        JobState::Completed => JobStatusView::Complete,
    }
}

const fn unit_status_view(state: JobUnitState) -> JobUnitStatusView {
    match state {
        JobUnitState::Blocked | JobUnitState::Ready | JobUnitState::Retrying => {
            JobUnitStatusView::Queued
        }
        JobUnitState::Running => JobUnitStatusView::Running,
        JobUnitState::Paused => JobUnitStatusView::Paused,
        JobUnitState::Cancelled => JobUnitStatusView::Cancelled,
        JobUnitState::Failed => JobUnitStatusView::Failed,
        JobUnitState::Completed => JobUnitStatusView::Complete,
    }
}

fn progress_ratio(completed: u64, total: u64) -> f32 {
    if total == 0 {
        0.0
    } else {
        let scaled = u128::from(completed.min(total)).saturating_mul(10_000) / u128::from(total);
        let basis_points = u16::try_from(scaled).unwrap_or(10_000);
        f32::from(basis_points) / 100.0
    }
}

// Persisted progress is constrained to [0, 1], where every finite f64 value
// has a safe, bounded f32 representation for presentation purposes.
#[allow(clippy::cast_possible_truncation)]
fn unit_interval_f32(value: f64) -> f32 {
    if value.is_finite() {
        value.clamp(0.0, 1.0) as f32
    } else {
        0.0
    }
}

async fn reserve_job_budgets(
    state: &AppState,
    job: &Job,
    plan: &ConversionPlan,
) -> Result<Option<ReservationId>, ServiceError> {
    let duplicate_charge_multiplier = if plan
        .project
        .settings
        .reliability
        .retry_possible_duplicate_charge
    {
        u64::from(
            plan.project
                .settings
                .reliability
                .max_transient_retries
                .saturating_add(1),
        )
    } else {
        1
    };
    let dispatch_capacity = plan
        .chapters
        .iter()
        .map(|chapter| chapter.segments.len())
        .sum::<usize>()
        .saturating_mul(usize::try_from(duplicate_charge_multiplier).unwrap_or(usize::MAX));
    let mut estimates = Vec::with_capacity(dispatch_capacity);
    for segment in plan.chapters.iter().flat_map(|chapter| &chapter.segments) {
        let characters = u64::try_from(segment.text.chars().count()).unwrap_or(u64::MAX);
        let estimate = crate::accounting::rate_usage_estimate(
            state,
            ProviderProfileId::from_uuid(segment.assignment.provider_id),
            UsageWorkload::Tts,
            segment.assignment.model.clone(),
            UsageQuantities {
                characters: Some(characters),
                ..UsageQuantities::default()
            },
        )
        .await?;
        for _ in 0..duplicate_charge_multiplier {
            estimates.push(estimate.clone());
        }
    }
    crate::accounting::reserve_for_estimates(state, job, &estimates).await
}

fn request_production_worker(job_id: JobId, retry_after_cleanup: bool) -> bool {
    let workers = ACTIVE_WORKERS.get_or_init(|| StdMutex::new(HashMap::new()));
    let mut workers = workers
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    if let Some(restart_requested) = workers.get_mut(&job_id.as_uuid()) {
        *restart_requested |= retry_after_cleanup;
        false
    } else {
        workers.insert(job_id.as_uuid(), false);
        true
    }
}

fn finish_production_worker_iteration(job_id: JobId) -> bool {
    let workers = ACTIVE_WORKERS.get_or_init(|| StdMutex::new(HashMap::new()));
    let mut workers = workers
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    if workers.get(&job_id.as_uuid()).copied().unwrap_or(false) {
        workers.insert(job_id.as_uuid(), false);
        true
    } else {
        workers.remove(&job_id.as_uuid());
        false
    }
}

fn schedule_conversion_job(state: Arc<AppState>, job_id: JobId) {
    if request_production_worker(job_id, false) {
        tokio::spawn(run_conversion_job(state, job_id));
    }
}

fn schedule_segment_regeneration_job(state: Arc<AppState>, job_id: JobId) {
    if request_production_worker(job_id, false) {
        tokio::spawn(run_segment_regeneration_job(state, job_id));
    }
}

fn schedule_conversion_retry(state: Arc<AppState>, job_id: JobId) {
    if request_production_worker(job_id, true) {
        tokio::spawn(run_conversion_job(state, job_id));
    }
}

fn schedule_segment_regeneration_retry(state: Arc<AppState>, job_id: JobId) {
    if request_production_worker(job_id, true) {
        tokio::spawn(run_segment_regeneration_job(state, job_id));
    }
}

async fn run_conversion_job(state: Arc<AppState>, job_id: JobId) {
    loop {
        let result = run_conversion_job_inner(&state, job_id).await;
        if let Err(error) = result {
            tracing::warn!(diagnostic_code = "conversion.failed", %job_id, %error, "conversion job failed");
            if !matches!(error, ServiceError::Conflict(ref message) if message == "job cancelled") {
                mark_job_failed(&state, job_id, &error.to_string()).await;
            }
        }
        if let Err(error) = reconcile_job_budgets(&state, job_id).await {
            tracing::warn!(diagnostic_code = "conversion.budget.reconcile.failed", %job_id, %error, "could not reconcile conversion budget reservation");
        }
        if !finish_production_worker_iteration(job_id) {
            break;
        }
    }
}

async fn run_segment_regeneration_job(state: Arc<AppState>, job_id: JobId) {
    loop {
        let result = run_segment_regeneration_job_inner(&state, job_id).await;
        if let Err(error) = &result {
            tracing::warn!(diagnostic_code = "proofing.regeneration.failed", %job_id, %error, "segment regeneration failed");
            if !matches!(error, ServiceError::Conflict(message) if message == "job cancelled") {
                mark_job_failed(&state, job_id, &error.to_string()).await;
            }
        }
        if let Err(error) = reconcile_job_budgets(&state, job_id).await {
            tracing::warn!(diagnostic_code = "proofing.regeneration.budget.reconcile.failed", %job_id, %error, "could not reconcile regeneration budget");
        }
        if !finish_production_worker_iteration(job_id) {
            break;
        }
    }
}

async fn run_segment_regeneration_job_inner(
    state: &Arc<AppState>,
    job_id: JobId,
) -> Result<(), ServiceError> {
    let repository = state.database.repositories().jobs;
    let mut job = repository
        .get(job_id)
        .await
        .map_err(storage_error)?
        .ok_or(ServiceError::NotFound)?;
    if job.kind != JobKind::SegmentRegeneration {
        return Err(ServiceError::Conflict(
            "job is not a segment regeneration".to_owned(),
        ));
    }
    if job.state == JobState::Queued {
        job = transition_job(
            state,
            job_id,
            JobState::Running,
            "Regenerating proofing segment",
        )
        .await?;
    }
    wait_until_runnable(state, job_id).await?;
    let mut units = repository.list_units(job_id).await.map_err(storage_error)?;
    let mut unit = units
        .drain(..)
        .find(|unit| unit.kind == JobUnitKind::SynthesisSegment)
        .ok_or_else(|| {
            ServiceError::Internal("regeneration job has no synthesis unit".to_owned())
        })?;
    if unit.state == JobUnitState::Completed {
        if !job.state.is_terminal() {
            transition_job(
                state,
                job_id,
                JobState::Completed,
                "Segment regeneration complete",
            )
            .await?;
        }
        return Ok(());
    }
    if matches!(unit.state, JobUnitState::Running | JobUnitState::Retrying) {
        record_interrupted_paid_unit_uncertainty(state, &unit).await?;
        return Err(ServiceError::Conflict(
            "a previous paid regeneration dispatch was interrupted and may have been charged; automatic redispatch is disabled"
                .to_owned(),
        ));
    }
    let segment = unit
        .payload
        .get("segmentPlan")
        .cloned()
        .ok_or_else(|| ServiceError::Internal("regeneration unit has no input snapshot".to_owned()))
        .and_then(|value| serde_json::from_value::<SegmentPlan>(value).map_err(internal_error))?;
    let sidecars = resolve_sidecars(state)?;
    let progress_guard = tokio::sync::Mutex::new(());
    synthesize_segment(
        state,
        job_id,
        segment,
        unit.clone(),
        &sidecars,
        &progress_guard,
    )
    .await?;
    let completed = transition_job(
        state,
        job_id,
        JobState::Completed,
        "Segment regeneration complete",
    )
    .await?;
    if let Err(error) = release_job_cache_pins(state, job_id).await {
        tracing::warn!(diagnostic_code = "proofing.regeneration.cache.unpin.failed", %job_id, %error, "could not release regeneration cache pin");
    }
    if let Some(view) = state.catalog.write().await.jobs.get_mut(&job_id.as_uuid()) {
        view.status = JobStatusView::Complete;
        view.progress = 100.0;
        view.current_stage = completed.status_message;
        view.updated_at = completed.updated_at;
        if let Some(current) = repository.get_unit(unit.id).await.map_err(storage_error)? {
            unit = current;
            view.units = vec![unit_view(&unit)];
        }
    }
    state.events.publish(
        "job.completed",
        serde_json::json!({"jobId": job_id, "projectId": job.project_id, "segmentId": unit.segment_id}),
    );
    Ok(())
}

// Durable synthesis, assembly, export, and catalog transitions are deliberately
// linear here so crash-resume ordering remains auditable.
#[allow(clippy::too_many_lines)]
async fn run_conversion_job_inner(
    state: &Arc<AppState>,
    job_id: JobId,
) -> Result<(), ServiceError> {
    let mut job = state
        .database
        .repositories()
        .jobs
        .get(job_id)
        .await
        .map_err(storage_error)?
        .ok_or(ServiceError::NotFound)?;
    if job.state == JobState::Queued {
        job = transition_job(state, job_id, JobState::Running, "Starting conversion").await?;
    }
    wait_until_runnable(state, job_id).await?;
    let mut plan = if job.kind == JobKind::Export {
        load_validated_proof_export_plan(state, &job).await?
    } else {
        let export = load_export_profile(
            state,
            job.export_profile_id.ok_or_else(|| {
                ServiceError::Internal("conversion job has no export profile".to_owned())
            })?,
        )
        .await?;
        let music_path = if let Some(music) = &export.background_music {
            Some(artifact_path(state, music.artifact_id).await?)
        } else {
            None
        };
        load_conversion_plan(state, job.project_id.as_uuid(), export, music_path).await?
    };
    let mut units = load_unit_plan(state, job_id, &plan).await?;
    bind_persisted_segment_ids(&mut plan, &units);
    synchronize_job_progress(state, job_id, &units).await?;
    refresh_catalog_job(state, job_id, &plan, &units).await?;
    let sidecars = resolve_sidecars(state)?;
    let global_limit = usize::from(plan.project.settings.global_chapter_concurrency.max(1));
    let progress_guard = Arc::new(tokio::sync::Mutex::new(()));

    let pending = plan
        .chapters
        .iter()
        .flat_map(|chapter| chapter.segments.iter())
        .filter_map(|segment| {
            let unit = units.synthesis.get(&segment.key)?;
            (!unit_is_reusable(state, unit)).then(|| (segment.clone(), unit.clone()))
        })
        .collect::<Vec<_>>();
    let pending_playback_ordinals = pending
        .iter()
        .map(|(segment, _)| segment.playback_ordinal)
        .collect::<BTreeSet<_>>();
    let next_playback_ordinal = pending.first().map_or_else(
        || {
            plan.chapters
                .iter()
                .map(|chapter| chapter.segments.len())
                .sum()
        },
        |(segment, _)| segment.playback_ordinal,
    );
    prepare_playback(job_id, next_playback_ordinal);
    for skipped in plan
        .chapters
        .iter()
        .flat_map(|chapter| &chapter.segments)
        .map(|segment| segment.playback_ordinal)
        .filter(|ordinal| {
            *ordinal >= next_playback_ordinal && !pending_playback_ordinals.contains(ordinal)
        })
    {
        complete_playback_segment(job_id, skipped);
    }
    stream::iter(pending.into_iter().map(|(segment, unit)| {
        let state = Arc::clone(state);
        let sidecars = sidecars.clone();
        let progress_guard = Arc::clone(&progress_guard);
        async move {
            synthesize_segment(&state, job_id, segment, unit, &sidecars, &progress_guard).await
        }
    }))
    .buffer_unordered(global_limit)
    .try_collect::<Vec<_>>()
    .await?;

    units = load_unit_plan(state, job_id, &plan).await?;
    let mut segment_artifacts = Vec::new();
    for chapter in &plan.chapters {
        for segment in &chapter.segments {
            let unit = units
                .synthesis
                .get(&segment.key)
                .ok_or_else(|| ServiceError::Internal("synthesis unit disappeared".to_owned()))?;
            let artifact_id = unit.output_artifact_id.ok_or_else(|| {
                ServiceError::Internal("completed synthesis unit has no artifact".to_owned())
            })?;
            let artifact = load_artifact(state, artifact_id).await?;
            segment_artifacts.push(SegmentArtifact {
                plan: segment.clone(),
                artifact,
            });
        }
    }

    let pending_chapters = plan
        .chapters
        .iter()
        .filter_map(|chapter| {
            let unit = units.assembly.get(&chapter.chapter.id.as_uuid())?;
            (!unit_is_reusable(state, unit)).then(|| (chapter.clone(), unit.clone()))
        })
        .collect::<Vec<_>>();
    stream::iter(pending_chapters.into_iter().map(|(chapter, unit)| {
        let state = Arc::clone(state);
        let sidecars = sidecars.clone();
        let progress_guard = Arc::clone(&progress_guard);
        let artifacts = segment_artifacts
            .iter()
            .filter(|artifact| artifact.plan.chapter_id == chapter.chapter.id.as_uuid())
            .cloned()
            .collect::<Vec<_>>();
        let verify_selected_artifacts = job.kind == JobKind::Export;
        async move {
            assemble_chapter(
                &state,
                job_id,
                chapter,
                unit,
                artifacts,
                verify_selected_artifacts,
                &sidecars,
                &progress_guard,
            )
            .await
        }
    }))
    .buffer_unordered(global_limit)
    .try_collect::<Vec<_>>()
    .await?;

    units = load_unit_plan(state, job_id, &plan).await?;
    let mut chapter_artifacts = Vec::new();
    for chapter in &plan.chapters {
        let unit = units
            .assembly
            .get(&chapter.chapter.id.as_uuid())
            .ok_or_else(|| ServiceError::Internal("assembly unit disappeared".to_owned()))?;
        let artifact = load_artifact(
            state,
            unit.output_artifact_id.ok_or_else(|| {
                ServiceError::Internal("chapter assembly has no artifact".to_owned())
            })?,
        )
        .await?;
        chapter_artifacts.push(ChapterArtifact {
            chapter: chapter.chapter.clone(),
            artifact,
        });
    }

    wait_until_runnable(state, job_id).await?;
    if let Some(mut mix) = units.mix.clone()
        && mix.state != JobUnitState::Completed
    {
        update_unit_state(state, &mut mix, JobUnitState::Running, None).await?;
        set_job_message(state, job_id, "Preparing background music mix").await?;
        update_unit_state(state, &mut mix, JobUnitState::Completed, None).await?;
        increment_job_progress(state, job_id, &progress_guard).await?;
        units.mix = Some(mix);
    }

    let (export_artifacts, manifest_artifact) = export_book(
        state,
        job_id,
        &plan,
        &chapter_artifacts,
        &sidecars,
        &mut units,
        &progress_guard,
    )
    .await?;
    let completion_message = if job.kind == JobKind::Export {
        "Proofing export complete"
    } else {
        "Conversion complete"
    };
    let completed = transition_job(state, job_id, JobState::Completed, completion_message).await?;
    if job.kind == JobKind::Conversion {
        mark_proofing_plan_ready(state, plan.project.id, job_id).await?;
    }
    if let Err(error) = release_job_cache_pins(state, job_id).await {
        tracing::warn!(diagnostic_code = "conversion.cache.unpin.failed", %job_id, %error, "could not release completed job cache pins");
    }
    let cache_limit = state.catalog.read().await.settings.cache_limit_bytes;
    if let Err(error) = enforce_cache_limit(state, cache_limit).await {
        tracing::warn!(diagnostic_code = "conversion.cache.prune.failed", %job_id, %error, "could not enforce the cache limit after conversion");
    }
    update_export_catalog(
        state,
        job_id,
        &plan,
        &export_artifacts,
        manifest_artifact.id,
    )
    .await?;
    {
        let mut catalog = state.catalog.write().await;
        if let Some(project) = catalog.projects.get_mut(&plan.project.id.as_uuid()) {
            project.summary.status = ProjectDisplayStatus::Completed;
            project.summary.progress = 100.0;
        }
        if let Some(view) = catalog.jobs.get_mut(&job_id.as_uuid()) {
            view.status = JobStatusView::Complete;
            view.progress = 100.0;
            view.current_stage = completed.status_message;
            view.updated_at = completed.updated_at;
        }
    }
    state.events.publish(
        "job.completed",
        serde_json::json!({"jobId": job_id, "projectId": plan.project.id}),
    );
    Ok(())
}

async fn mark_proofing_plan_ready(
    state: &AppState,
    project_id: ProjectId,
    job_id: JobId,
) -> Result<(), ServiceError> {
    let project_lock = state.character_lifecycle_lock(project_id.as_uuid()).await;
    let _project_guard = project_lock.lock().await;
    let repository = state.database.repositories().proofing;
    let Some(mut plan) = repository
        .get_plan(project_id)
        .await
        .map_err(storage_error)?
    else {
        return Ok(());
    };
    if plan.source_conversion_job_id != job_id {
        return Ok(());
    }
    let expected_revision = plan.plan_revision;
    plan.status = ProofingPlanStatus::Ready;
    plan.dirty_reasons.clear();
    plan.updated_at = Utc::now();
    repository
        .update_plan(&plan, expected_revision)
        .await
        .map_err(storage_error)
}

async fn load_required_proof_export_snapshot(
    state: &AppState,
    job: &Job,
) -> Result<ProofExportSnapshot, ServiceError> {
    let payload = sqlx::query_scalar::<_, String>(
        "SELECT payload FROM proof_export_snapshots WHERE job_id = ?",
    )
    .bind(job.id.to_string())
    .fetch_optional(state.database.pool())
    .await
    .map_err(storage_error)?
    .ok_or_else(|| {
        ServiceError::Conflict(
            "proof export audit snapshot is missing; refusing an unaudited export".to_owned(),
        )
    })?;
    let snapshot: ProofExportSnapshot = serde_json::from_str(&payload).map_err(internal_error)?;
    if snapshot.job_id != job.id
        || snapshot.project_id != job.project_id
        || Some(snapshot.export_profile_id) != job.export_profile_id
    {
        return Err(ServiceError::Conflict(
            "proof export audit snapshot does not match its durable job".to_owned(),
        ));
    }
    Ok(snapshot)
}

async fn load_validated_proof_export_plan(
    state: &AppState,
    job: &Job,
) -> Result<ConversionPlan, ServiceError> {
    let units = state
        .database
        .repositories()
        .jobs
        .list_units(job.id)
        .await
        .map_err(storage_error)?;
    let export = units
        .iter()
        .find(|unit| unit.kind == JobUnitKind::FinalExport)
        .ok_or_else(|| {
            ServiceError::Conflict("proof export has no durable export unit".to_owned())
        })?;
    let plan = export
        .payload
        .get("proofExportPlan")
        .cloned()
        .ok_or_else(|| ServiceError::Conflict("proof export has no durable plan".to_owned()))
        .and_then(|value| {
            serde_json::from_value::<ConversionPlan>(value).map_err(internal_error)
        })?;
    let expected_snapshot_id = export
        .payload
        .get("proofExportSnapshotId")
        .cloned()
        .ok_or_else(|| {
            ServiceError::Conflict("proof export unit has no audit snapshot id".to_owned())
        })
        .and_then(|value| {
            serde_json::from_value::<ProofExportSnapshotId>(value).map_err(internal_error)
        })?;
    let snapshot = load_required_proof_export_snapshot(state, job).await?;
    if snapshot.id != expected_snapshot_id
        || plan.project.id != job.project_id
        || Some(plan.export.id) != job.export_profile_id
    {
        return Err(ServiceError::Conflict(
            "proof export plan and audit snapshot do not match the durable job".to_owned(),
        ));
    }
    let expected = snapshot
        .selections
        .iter()
        .map(|selection| (selection.segment_id, selection.artifact_id))
        .collect::<BTreeMap<_, _>>();
    if expected.len() != snapshot.selections.len() {
        return Err(ServiceError::Conflict(
            "proof export snapshot contains duplicate segment selections".to_owned(),
        ));
    }
    let actual = units
        .iter()
        .filter(|unit| unit.kind == JobUnitKind::SynthesisSegment)
        .map(|unit| {
            unit.segment_id.zip(unit.output_artifact_id).ok_or_else(|| {
                ServiceError::Conflict(
                    "proof export synthesis unit is missing its selected artifact".to_owned(),
                )
            })
        })
        .collect::<Result<BTreeMap<_, _>, _>>()?;
    let synthesis_count = units
        .iter()
        .filter(|unit| unit.kind == JobUnitKind::SynthesisSegment)
        .count();
    if actual.len() != synthesis_count {
        return Err(ServiceError::Conflict(
            "proof export job graph contains duplicate segment units".to_owned(),
        ));
    }
    if actual != expected {
        return Err(ServiceError::Conflict(
            "proof export job graph differs from its reviewed take snapshot".to_owned(),
        ));
    }
    for selection in &snapshot.selections {
        let artifact = load_artifact(state, selection.artifact_id).await?;
        verify_selected_artifact_integrity(&artifact).await?;
    }
    Ok(plan)
}

fn bind_persisted_segment_ids(plan: &mut ConversionPlan, units: &PersistedUnitPlan) {
    for segment in plan
        .chapters
        .iter_mut()
        .flat_map(|chapter| chapter.segments.iter_mut())
    {
        if let Some(id) = units
            .synthesis
            .get(&segment.key)
            .and_then(|unit| unit.segment_id)
        {
            segment.id = id;
        }
    }
}

async fn load_export_profile(
    state: &AppState,
    id: ExportProfileId,
) -> Result<ExportProfile, ServiceError> {
    let payload =
        sqlx::query_scalar::<_, String>("SELECT payload FROM export_profiles WHERE id = ?")
            .bind(id.to_string())
            .fetch_optional(state.database.pool())
            .await
            .map_err(storage_error)?
            .ok_or(ServiceError::NotFound)?;
    serde_json::from_str(&payload).map_err(internal_error)
}

fn unit_is_reusable(state: &AppState, unit: &JobUnit) -> bool {
    if unit.state != JobUnitState::Completed {
        return false;
    }
    let Some(artifact_id) = unit.output_artifact_id else {
        return false;
    };
    let row = sqlx::query_scalar::<_, String>("SELECT path FROM artifacts WHERE id = ?")
        .bind(artifact_id.to_string())
        .fetch_optional(state.database.pool());
    // This synchronous predicate is used only for planning. A missing path is detected when the
    // artifact is loaded below, so a completed unit remains a cache candidate here.
    drop(row);
    true
}

async fn synchronize_job_progress(
    state: &AppState,
    job_id: JobId,
    units: &PersistedUnitPlan,
) -> Result<(), ServiceError> {
    let completed = units
        .synthesis
        .values()
        .chain(units.assembly.values())
        .chain(units.mix.iter())
        .chain(std::iter::once(&units.normalize))
        .chain(std::iter::once(&units.export))
        .filter(|unit| unit.state == JobUnitState::Completed)
        .count();
    let repository = state.database.repositories().jobs;
    let mut job = repository
        .get(job_id)
        .await
        .map_err(storage_error)?
        .ok_or(ServiceError::NotFound)?;
    let expected = job.revision;
    job.progress_completed = u64::try_from(completed).unwrap_or(u64::MAX);
    job.updated_at = Utc::now();
    repository
        .update(&job, expected)
        .await
        .map_err(storage_error)?;
    Ok(())
}

async fn refresh_catalog_job(
    state: &AppState,
    job_id: JobId,
    plan: &ConversionPlan,
    units: &PersistedUnitPlan,
) -> Result<(), ServiceError> {
    let job = state
        .database
        .repositories()
        .jobs
        .get(job_id)
        .await
        .map_err(storage_error)?
        .ok_or(ServiceError::NotFound)?;
    state.catalog.write().await.jobs.insert(
        job_id.as_uuid(),
        job_view(&job, &plan.project.metadata.title, units),
    );
    Ok(())
}

#[derive(Debug)]
struct ProviderStreamSink {
    request_id: Uuid,
    format: AudioFormat,
    next_sequence: tokio::sync::Mutex<u64>,
    audio: tokio::sync::Mutex<BytesMut>,
    decoder: tokio::sync::Mutex<Option<mpsc::Sender<Bytes>>>,
    final_seen: AtomicBool,
}

impl ProviderStreamSink {
    fn new(request_id: Uuid, format: AudioFormat, decoder: Option<mpsc::Sender<Bytes>>) -> Self {
        Self {
            request_id,
            format,
            next_sequence: tokio::sync::Mutex::new(0),
            audio: tokio::sync::Mutex::new(BytesMut::new()),
            decoder: tokio::sync::Mutex::new(decoder),
            final_seen: AtomicBool::new(false),
        }
    }

    async fn finish(&self) -> Result<Bytes, ProviderError> {
        self.decoder.lock().await.take();
        if !self.final_seen.load(Ordering::Acquire) {
            return Err(ProviderError::InvalidResponse(
                "streaming TTS ended without a final chunk".to_owned(),
            ));
        }
        let audio = self.audio.lock().await;
        if audio.is_empty() {
            return Err(ProviderError::InvalidResponse(
                "streaming TTS returned no audio".to_owned(),
            ));
        }
        Ok(Bytes::copy_from_slice(&audio))
    }

    async fn abort(&self) {
        self.decoder.lock().await.take();
    }
}

#[async_trait]
impl AudioChunkSink for ProviderStreamSink {
    async fn send(&self, chunk: AudioChunk) -> Result<(), ProviderError> {
        if chunk.request_id != self.request_id || chunk.format != self.format {
            return Err(ProviderError::InvalidResponse(
                "streaming TTS changed request identity or audio format".to_owned(),
            ));
        }
        if self.final_seen.load(Ordering::Acquire) {
            return Err(ProviderError::InvalidResponse(
                "streaming TTS sent data after its final chunk".to_owned(),
            ));
        }
        let mut next_sequence = self.next_sequence.lock().await;
        if chunk.sequence != *next_sequence {
            return Err(ProviderError::InvalidResponse(format!(
                "streaming TTS chunk sequence {} arrived while {} was expected",
                chunk.sequence, *next_sequence
            )));
        }
        *next_sequence = next_sequence.saturating_add(1);
        drop(next_sequence);

        if !chunk.data.is_empty() {
            self.audio.lock().await.extend_from_slice(&chunk.data);
            let decoder = self.decoder.lock().await.clone();
            if let Some(decoder) = decoder {
                // Playback decoding is best effort. A failed decoder must not discard a paid,
                // otherwise valid provider response; canonical normalization still validates it.
                let _ = decoder.send(chunk.data).await;
            }
        }
        if chunk.final_chunk {
            self.final_seen.store(true, Ordering::Release);
            self.decoder.lock().await.take();
        }
        Ok(())
    }
}

#[derive(Debug)]
struct StreamedSynthesis {
    response: SynthesisResponse,
    progressive_decode_complete: bool,
}

#[derive(Debug)]
enum ProviderSynthesisDispatch {
    Complete(StreamedSynthesis),
    Streaming {
        metadata: StreamingSynthesisResponse,
        sink: Arc<ProviderStreamSink>,
        decoder_task: Option<tokio::task::JoinHandle<bool>>,
        job_id: JobId,
        playback_ordinal: usize,
    },
}

impl ProviderSynthesisDispatch {
    fn usage(&self) -> &ProviderUsage {
        match self {
            Self::Complete(streamed) => &streamed.response.usage,
            Self::Streaming { metadata, .. } => &metadata.usage,
        }
    }
}

/// Performs only the provider-owned portion of synthesis.
///
/// A successful return is the billing boundary consumed by the retry journal. Streaming sink
/// validation and local decoder completion deliberately happen later so they cannot turn a paid
/// provider success into a retryable provider failure.
async fn dispatch_provider_audio(
    provider: Arc<dyn TtsProvider>,
    request: SynthesisRequest,
    sidecars: &SidecarPair,
    job_id: JobId,
    playback_ordinal: usize,
) -> Result<ProviderSynthesisDispatch, ProviderError> {
    if !provider.capabilities().streaming {
        return provider.synthesize(request).await.map(|response| {
            ProviderSynthesisDispatch::Complete(StreamedSynthesis {
                response,
                progressive_decode_complete: false,
            })
        });
    }

    // Start decoding with the provider request rather than waiting for a subscriber. A listener
    // that connects halfway through synthesis can then receive the remaining live chunks.
    let (decoder_sender, decoder_task) = match spawn_stream_playback_decoder(
        sidecars,
        job_id,
        playback_ordinal,
    ) {
        Ok((sender, task)) => (Some(sender), Some(task)),
        Err(error) => {
            tracing::warn!(diagnostic_code = "playback.decoder.start.failed", %job_id, %error, "could not start progressive playback decoder");
            (None, None)
        }
    };
    let sink = Arc::new(ProviderStreamSink::new(
        request.request_id,
        request.format,
        decoder_sender,
    ));
    let metadata = match provider
        .synthesize_stream(
            request,
            CancellationFlag::default(),
            Arc::clone(&sink) as Arc<dyn AudioChunkSink>,
        )
        .await
    {
        Ok(metadata) => metadata,
        Err(error) => {
            sink.abort().await;
            if let Some(decoder) = decoder_task {
                let _ = decoder.await;
            }
            reset_playback_segment(job_id, playback_ordinal);
            return Err(error);
        }
    };
    Ok(ProviderSynthesisDispatch::Streaming {
        metadata,
        sink,
        decoder_task,
        job_id,
        playback_ordinal,
    })
}

async fn finish_provider_audio(
    dispatch: ProviderSynthesisDispatch,
) -> Result<StreamedSynthesis, ProviderError> {
    match dispatch {
        ProviderSynthesisDispatch::Complete(streamed) => Ok(streamed),
        ProviderSynthesisDispatch::Streaming {
            metadata,
            sink,
            decoder_task,
            job_id,
            playback_ordinal,
        } => {
            let audio = match sink.finish().await {
                Ok(audio) => audio,
                Err(error) => {
                    if let Some(decoder) = decoder_task {
                        let _ = decoder.await;
                    }
                    reset_playback_segment(job_id, playback_ordinal);
                    return Err(error);
                }
            };
            let progressive_decode_complete = if let Some(decoder) = decoder_task {
                decoder.await.unwrap_or(false)
            } else {
                false
            };
            Ok(StreamedSynthesis {
                response: SynthesisResponse {
                    audio,
                    content_type: metadata.content_type,
                    usage: metadata.usage,
                },
                progressive_decode_complete,
            })
        }
    }
}

fn spawn_stream_playback_decoder(
    sidecars: &SidecarPair,
    job_id: JobId,
    playback_ordinal: usize,
) -> Result<(mpsc::Sender<Bytes>, tokio::task::JoinHandle<bool>), ProviderError> {
    let mut child = Command::new(&sidecars.ffmpeg)
        .args([
            "-hide_banner",
            "-loglevel",
            "error",
            "-nostdin",
            "-i",
            "pipe:0",
            "-vn",
            "-ar",
            "48000",
            "-ac",
            "1",
            "-f",
            "f32le",
            "pipe:1",
        ])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .kill_on_drop(true)
        .spawn()
        .map_err(|error| ProviderError::Transport(error.to_string()))?;
    let mut stdin = child.stdin.take().ok_or_else(|| {
        ProviderError::Transport("FFmpeg playback stdin is unavailable".to_owned())
    })?;
    let mut stdout = child.stdout.take().ok_or_else(|| {
        ProviderError::Transport("FFmpeg playback stdout is unavailable".to_owned())
    })?;
    let (sender, mut receiver) = mpsc::channel::<Bytes>(8);
    let task = tokio::spawn(async move {
        let result = async {
            let writer = async {
                while let Some(chunk) = receiver.recv().await {
                    stdin.write_all(&chunk).await?;
                }
                stdin.shutdown().await
            };
            let reader = async {
                let mut buffer = vec![0_u8; RANGE_CHUNK_BYTES];
                let mut pending = BytesMut::new();
                loop {
                    let read = stdout.read(&mut buffer).await?;
                    if read == 0 {
                        break;
                    }
                    pending.extend_from_slice(&buffer[..read]);
                    let aligned = pending.len() - (pending.len() % std::mem::size_of::<f32>());
                    if aligned > 0 {
                        publish_playback_chunk(
                            job_id,
                            playback_ordinal,
                            pending.split_to(aligned).freeze(),
                        );
                    }
                }
                if pending.is_empty() {
                    Ok(())
                } else {
                    Err(std::io::Error::new(
                        std::io::ErrorKind::InvalidData,
                        "FFmpeg produced a partial float sample",
                    ))
                }
            };
            tokio::try_join!(writer, reader)?;
            let status = child.wait().await?;
            if status.success() {
                Ok(())
            } else {
                Err(std::io::Error::other(
                    "FFmpeg could not decode provider stream",
                ))
            }
        }
        .await;
        if let Err(error) = &result {
            tracing::warn!(diagnostic_code = "playback.decoder.stopped", %job_id, %error, "progressive provider audio decoder stopped");
        }
        result.is_ok()
    });
    Ok((sender, task))
}

// Reservation verification, retry accounting, cache promotion, and playback
// completion form one dispatch transaction and must retain this order.
#[allow(clippy::too_many_lines)]
async fn synthesize_segment(
    state: &Arc<AppState>,
    job_id: JobId,
    segment: SegmentPlan,
    mut unit: JobUnit,
    sidecars: &SidecarPair,
    progress_guard: &tokio::sync::Mutex<()>,
) -> Result<SegmentArtifact, ServiceError> {
    wait_until_runnable(state, job_id).await?;
    update_unit_state(state, &mut unit, JobUnitState::Running, None).await?;
    set_job_message(
        state,
        job_id,
        &format!(
            "Synthesizing {} with {}",
            segment.chapter_title, segment.assignment.provider_name
        ),
    )
    .await?;
    let cache = cache(state);
    let cache_operation = unit
        .payload
        .get("cacheOperation")
        .and_then(serde_json::Value::as_str)
        .unwrap_or("conversion");
    let fingerprint = segment_cache_fingerprint(&segment, cache_operation);
    let cache_key = fingerprint
        .key()
        .map_err(|error| ServiceError::Internal(error.to_string()))?;
    let (artifact, progressive_decode_complete) = if cache.contains(&cache_key) {
        cache.pin(&cache_key).map_err(media_error)?;
        (
            ensure_cached_artifact(
                state,
                job_id,
                segment.chapter_id,
                &cache_key,
                ArtifactKind::SegmentAudio,
            )
            .await?,
            false,
        )
    } else {
        let runtime_id =
            ProviderId::new(segment.assignment.provider_id.to_string()).map_err(internal_error)?;
        let provider = state
            .providers
            .tts(&runtime_id)
            .await
            .map_err(|error| ServiceError::Conflict(error.to_string()))?;
        let semaphore = provider_semaphore(
            segment.assignment.provider_id,
            segment.assignment.provider_concurrency,
        );
        let _permit = semaphore
            .acquire_owned()
            .await
            .map_err(|_| ServiceError::Internal("provider semaphore closed".to_owned()))?;
        wait_until_runnable(state, job_id).await?;
        let request = SynthesisRequest {
            request_id: Uuid::new_v4(),
            text: segment.text.clone(),
            model: segment.assignment.model.clone(),
            voice: segment.assignment.voice_source.clone(),
            format: requested_audio_format(&segment.assignment),
            performance: segment.assignment.performance.clone(),
            options: BTreeMap::new(),
            pronunciation_dictionary_ids: Vec::new(),
        };
        let playback_ordinal = segment.playback_ordinal;
        let dispatch_estimate = crate::accounting::rate_usage_estimate(
            state,
            ProviderProfileId::from_uuid(segment.assignment.provider_id),
            UsageWorkload::Tts,
            segment.assignment.model.clone(),
            UsageQuantities {
                characters: u64::try_from(segment.text.chars().count()).ok(),
                ..UsageQuantities::default()
            },
        )
        .await?;
        let policy = retry_policy(state, &segment).await?;
        let dispatch_project_id = segment_project_id(state, segment.chapter_id).await?;
        let dispatch_consent_lock = state
            .dispatch_consent_lifecycle_lock(dispatch_project_id)
            .await;
        let journal = AttemptJournal::new(
            Arc::clone(state),
            unit.id,
            TtsUsageContext {
                job_id,
                segment: segment.clone(),
                provider_request_id: request.request_id,
                rate_card_id: dispatch_estimate.rate_card_id,
            },
        );
        let execution = execute_with_retry(&policy, &journal, |_| {
            let state = Arc::clone(state);
            let provider = Arc::clone(&provider);
            let request = request.clone();
            let sidecars = sidecars.clone();
            let dispatch_estimate = dispatch_estimate.clone();
            let dispatch_consent_lock = Arc::clone(&dispatch_consent_lock);
            let dispatch_segment = segment.clone();
            async move {
                let _dispatch_consent_guard = dispatch_consent_lock.read().await;
                validate_segment_dispatch_boundary(&state, dispatch_project_id, &dispatch_segment)
                    .await?;
                crate::accounting::verify_dispatch_is_reserved(&state, job_id, &dispatch_estimate)
                    .await
                    .map_err(|_| {
                        ProviderError::Configuration(
                            "the active hard-budget reservation does not permit this dispatch"
                                .to_owned(),
                        )
                    })?;
                dispatch_provider_audio(provider, request, &sidecars, job_id, playback_ordinal)
                    .await
            }
        })
        .await
        .map_err(|error| retry_service_error(state, job_id, &segment, &error))?;
        unit.attempt_count = execution.attempts.get();
        let successful_attempt_id =
            attempt_id_for_ordinal(state, unit.id, execution.attempts.get()).await?;
        let dispatch = execution.value;
        // The provider has already accepted and completed a potentially billable request. Record
        // its usage before any local decoding, cache, probe, or artifact operation can fail.
        let mut usage = dispatch.usage().clone();
        if usage.request_id.is_none() {
            usage.request_id = Some(request.request_id.to_string());
        }
        append_tts_usage(
            state,
            job_id,
            &segment,
            successful_attempt_id,
            &usage,
            false,
            dispatch_estimate.rate_card_id,
        )
        .await?;
        let streamed = finish_provider_audio(dispatch)
            .await
            .map_err(|error| ServiceError::Conflict(error.to_string()))?;
        let response = streamed.response;
        let flac = normalize_provider_audio(sidecars, &response, segment.text.chars().count() < 50)
            .await?;
        let artifact_id = ArtifactId::new();
        let manifest = serde_json::json!({
            "schemaVersion": 1,
            "artifactId": artifact_id,
            "cacheKey": cache_key.as_str(),
            "textHash": blake3::hash(segment.text.as_bytes()).to_hex().to_string(),
            "contextHash": segment.context.as_deref().map_or_else(String::new, |value| blake3::hash(value.as_bytes()).to_hex().to_string()),
            "providerProfileId": segment.assignment.provider_id,
            "providerEndpoint": redacted_endpoint(segment.assignment.provider_endpoint.as_deref()),
            "providerVersion": segment.assignment.provider_version,
            "model": segment.assignment.model,
            "voiceProfileId": segment.assignment.voice_id,
            "dictionaryRevision": segment.dictionary_revision,
            "appliedRuleIds": segment.applied_rule_ids,
            "normalizationVersion": NORMALIZATION_VERSION,
            "providerRequestId": response.usage.request_id,
            "createdAt": Utc::now(),
        });
        let path = cache
            .put(&cache_key, &flac, &manifest)
            .map_err(media_error)?;
        cache.pin(&cache_key).map_err(media_error)?;
        let duration_ms = probe_duration_ms(sidecars, &path).await?;
        let artifact = artifact_for_file_with_id(
            artifact_id,
            ArtifactKind::SegmentAudio,
            &path,
            Some("audio/flac".to_owned()),
            Some(duration_ms),
            Some(cache_key.as_str().to_owned()),
            Some(job_id),
        )
        .await?;
        persist_artifact(
            state,
            segment_project_id(state, segment.chapter_id).await?,
            &artifact,
        )
        .await?;
        (artifact, streamed.progressive_decode_complete)
    };
    let artifact = persist_proof_take(state, job_id, &segment, &unit, &artifact).await?;
    unit.output_artifact_id = Some(artifact.id);
    update_unit_state(state, &mut unit, JobUnitState::Completed, None).await?;
    increment_job_progress(state, job_id, progress_guard).await?;
    if progressive_decode_complete {
        complete_playback_segment(job_id, segment.playback_ordinal);
    } else if playback_listener_count(job_id) > 0 {
        reset_playback_segment(job_id, segment.playback_ordinal);
        let pcm = decode_flac_pcm(sidecars, Path::new(&artifact.path)).await?;
        complete_playback_with_pcm(job_id, segment.playback_ordinal, &pcm);
    } else {
        complete_playback_segment(job_id, segment.playback_ordinal);
    }
    Ok(SegmentArtifact {
        plan: segment,
        artifact,
    })
}

#[allow(clippy::too_many_lines)]
async fn persist_proof_take(
    state: &AppState,
    job_id: JobId,
    segment: &SegmentPlan,
    unit: &JobUnit,
    source: &Artifact,
) -> Result<Artifact, ServiceError> {
    let Some(segment_id) = unit.segment_id else {
        // Jobs created by versions before the proofing migration remain resumable,
        // but their transient artifacts cannot be promoted into trustworthy takes.
        return Ok(source.clone());
    };
    let take_id = unit
        .payload
        .get("takeId")
        .cloned()
        .and_then(|value| serde_json::from_value::<SegmentTakeId>(value).ok())
        .ok_or_else(|| ServiceError::Internal("proofing unit has no take id".to_owned()))?;
    if let Some(existing) = state
        .database
        .repositories()
        .proofing
        .get_take(take_id)
        .await
        .map_err(storage_error)?
    {
        let artifact = load_artifact(state, existing.artifact_id).await?;
        verify_selected_artifact_integrity(&artifact).await?;
        return Ok(artifact);
    }
    let artifact_id = unit
        .payload
        .get("takeArtifactId")
        .cloned()
        .and_then(|value| serde_json::from_value::<ArtifactId>(value).ok())
        .ok_or_else(|| ServiceError::Internal("proofing unit has no artifact id".to_owned()))?;
    let project_id = segment_project_id(state, segment.chapter_id).await?;
    let take_directory = state
        .config
        .data_dir
        .join("library")
        .join(project_id.to_string())
        .join("proofing")
        .join("takes");
    tokio::fs::create_dir_all(&take_directory).await?;
    let destination = take_directory.join(format!("{take_id}.flac"));
    let expected_fingerprint = materialize_proof_take_file(source, &destination).await?;
    let artifact = if sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM artifacts WHERE id = ?")
        .bind(artifact_id.to_string())
        .fetch_one(state.database.pool())
        .await
        .map_err(storage_error)?
        > 0
    {
        let existing = load_artifact(state, artifact_id).await?;
        if Path::new(&existing.path) != destination || existing.fingerprint != expected_fingerprint
        {
            return Err(ServiceError::Conflict(
                "existing proof-take artifact does not match its durable source".to_owned(),
            ));
        }
        existing
    } else {
        let artifact = artifact_for_file_with_id(
            artifact_id,
            ArtifactKind::SegmentAudio,
            &destination,
            Some("audio/flac".to_owned()),
            source.duration_ms,
            None,
            None,
        )
        .await?;
        persist_artifact(state, project_id, &artifact).await?;
        artifact
    };
    // Quality-control builds hold the same project lock through their final provenance
    // revalidation and persistence. Serialize the durable take/selection mutation with that
    // window so a report can never commit a half-old proofing identity.
    let project_lock = state.character_lifecycle_lock(project_id).await;
    let _project_guard = project_lock.lock().await;
    if let Some(existing) = state
        .database
        .repositories()
        .proofing
        .get_take(take_id)
        .await
        .map_err(storage_error)?
    {
        let artifact = load_artifact(state, existing.artifact_id).await?;
        verify_selected_artifact_integrity(&artifact).await?;
        return Ok(artifact);
    }
    let ordinal = sqlx::query_scalar::<_, i64>(
        "SELECT COALESCE(MAX(ordinal), 0) + 1 FROM segment_takes WHERE segment_id = ?",
    )
    .bind(segment_id.to_string())
    .fetch_one(state.database.pool())
    .await
    .map_err(storage_error)?;
    let semantic_input_hash = segment_semantic_input_hash(segment)?;
    let now = Utc::now();
    let take = SegmentTake {
        id: take_id,
        segment_id,
        artifact_id: artifact.id,
        ordinal: u32::try_from(ordinal).unwrap_or(u32::MAX),
        source_job_id: job_id,
        source_job_unit_id: unit.id,
        semantic_input_hash,
        duration_ms: artifact.duration_ms.unwrap_or_default(),
        provider_profile_id: Some(ProviderProfileId::from_uuid(segment.assignment.provider_id)),
        model: segment.assignment.model.clone(),
        voice_profile_id: Some(VoiceProfileId::from_uuid(segment.assignment.voice_id)),
        dictionary_revision_hash: segment.dictionary_revision.clone(),
        normalization_version: NORMALIZATION_VERSION.to_owned(),
        synthesis_provenance: BTreeMap::from([
            (
                "providerName".to_owned(),
                serde_json::json!(segment.assignment.provider_name),
            ),
            (
                "voiceName".to_owned(),
                serde_json::json!(segment.assignment.voice_name),
            ),
            (
                "performance".to_owned(),
                serde_json::json!(segment.assignment.performance),
            ),
            (
                "timing".to_owned(),
                serde_json::json!(segment.assignment.timing),
            ),
        ]),
        findings: Vec::new(),
        created_at: now,
    };
    let auto_select = unit
        .payload
        .get("autoSelect")
        .and_then(serde_json::Value::as_bool)
        .unwrap_or(false);
    let repository = state.database.repositories().proofing;
    if auto_select {
        let selection = SegmentSelection {
            segment_id,
            take_id,
            selected_at: now,
            revision: 0,
        };
        repository
            .insert_take_and_select(&take, &selection)
            .await
            .map_err(storage_error)?;
    } else {
        repository.insert_take(&take).await.map_err(storage_error)?;
    }
    Ok(artifact)
}

async fn materialize_proof_take_file(
    source: &Artifact,
    destination: &Path,
) -> Result<FileFingerprint, ServiceError> {
    let source_path = Path::new(&source.path);
    let source_metadata = tokio::fs::symlink_metadata(source_path).await?;
    if source_metadata.file_type().is_symlink() || !source_metadata.is_file() {
        return Err(ServiceError::Conflict(format!(
            "proof-take source is not a regular managed file: {}",
            source_path.display()
        )));
    }
    let expected = fingerprint_file(source_path).await?;
    if expected != source.fingerprint {
        return Err(ServiceError::Conflict(format!(
            "proof-take source no longer matches its durable fingerprint: {}",
            source_path.display()
        )));
    }

    match tokio::fs::symlink_metadata(destination).await {
        Ok(metadata) if metadata.is_file() && !metadata.file_type().is_symlink() => {}
        Ok(_) => {
            return Err(ServiceError::Conflict(format!(
                "proof-take destination is not a regular managed file: {}",
                destination.display()
            )));
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            match tokio::fs::hard_link(source_path, destination).await {
                Ok(()) => {}
                Err(_) => copy_file_atomically(source_path, destination).await?,
            }
        }
        Err(error) => return Err(ServiceError::Io(error)),
    }

    let destination_metadata = tokio::fs::symlink_metadata(destination).await?;
    if destination_metadata.file_type().is_symlink() || !destination_metadata.is_file() {
        return Err(ServiceError::Conflict(format!(
            "proof-take destination was replaced during materialization: {}",
            destination.display()
        )));
    }
    if fingerprint_file(destination).await? != expected {
        return Err(ServiceError::Conflict(format!(
            "existing proof-take file does not match its durable source: {}",
            destination.display()
        )));
    }
    audiobookai_storage::harden_private_file(destination)
        .await
        .map_err(storage_error)?;
    Ok(expected)
}

fn provider_semaphore(provider_id: Uuid, requested: u16) -> Arc<Semaphore> {
    let capacity = requested.max(1);
    let registry = PROVIDER_SEMAPHORES.get_or_init(|| StdMutex::new(HashMap::new()));
    let mut registry = registry
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    registry
        .entry(provider_id)
        .or_insert_with(|| (capacity, Arc::new(Semaphore::new(usize::from(capacity)))))
        .1
        .clone()
}

fn cache(state: &AppState) -> ContentAddressedCache {
    ContentAddressedCache::new(state.catalog.try_read().map_or_else(
        |_| state.database.paths().cache.clone(),
        |catalog| PathBuf::from(&catalog.settings.cache_path),
    ))
}

pub(crate) async fn enforce_cache_limit(
    state: &AppState,
    maximum_bytes: u64,
) -> Result<(), ServiceError> {
    let protected = active_job_cache_keys(state).await?;
    let object_cache = cache(state);
    let report = tokio::task::spawn_blocking(move || object_cache.prune(maximum_bytes, &protected))
        .await
        .map_err(internal_error)?
        .map_err(media_error)?;
    for key in report.removed_keys {
        sqlx::query("DELETE FROM artifacts WHERE cache_key = ? AND pinned_by_job_id IS NULL")
            .bind(key.as_str())
            .execute(state.database.pool())
            .await
            .map_err(storage_error)?;
    }
    Ok(())
}

async fn active_job_cache_keys(
    state: &AppState,
) -> Result<HashSet<audiobookai_media::CacheKey>, ServiceError> {
    let payloads = sqlx::query_scalar::<_, String>(
        "SELECT ju.payload FROM job_units ju JOIN jobs j ON j.id = ju.job_id \
         WHERE j.state NOT IN ('cancelled', 'failed', 'completed')",
    )
    .fetch_all(state.database.pool())
    .await
    .map_err(storage_error)?;
    let mut artifact_ids = HashSet::new();
    for payload in payloads {
        let unit: JobUnit = serde_json::from_str(&payload).map_err(internal_error)?;
        if let Some(artifact_id) = unit.output_artifact_id {
            artifact_ids.insert(artifact_id);
        }
    }
    cache_keys_for_artifacts(state, artifact_ids).await
}

async fn cache_keys_for_artifacts(
    state: &AppState,
    artifact_ids: HashSet<ArtifactId>,
) -> Result<HashSet<audiobookai_media::CacheKey>, ServiceError> {
    let mut keys = HashSet::new();
    for artifact_id in artifact_ids {
        let payload = sqlx::query_scalar::<_, String>("SELECT payload FROM artifacts WHERE id = ?")
            .bind(artifact_id.to_string())
            .fetch_optional(state.database.pool())
            .await
            .map_err(storage_error)?;
        let Some(payload) = payload else {
            continue;
        };
        let artifact: Artifact = serde_json::from_str(&payload).map_err(internal_error)?;
        if let Some(key) = artifact
            .cache_key
            .and_then(|value| audiobookai_media::CacheKey::parse(value).ok())
        {
            keys.insert(key);
        }
    }
    Ok(keys)
}

async fn release_job_cache_pins(state: &AppState, job_id: JobId) -> Result<(), ServiceError> {
    let units = state
        .database
        .repositories()
        .jobs
        .list_units(job_id)
        .await
        .map_err(storage_error)?;
    let artifact_ids = units
        .into_iter()
        .filter_map(|unit| unit.output_artifact_id)
        .collect::<HashSet<_>>();
    let keys = cache_keys_for_artifacts(state, artifact_ids).await?;
    let still_active = active_job_cache_keys(state).await?;
    let object_cache = cache(state);
    for key in keys.difference(&still_active) {
        object_cache.unpin(key).map_err(media_error)?;
    }

    let payloads = sqlx::query_scalar::<_, String>(
        "SELECT payload FROM artifacts WHERE pinned_by_job_id = ? AND cache_key IS NOT NULL",
    )
    .bind(job_id.to_string())
    .fetch_all(state.database.pool())
    .await
    .map_err(storage_error)?;
    for payload in payloads {
        let mut artifact: Artifact = serde_json::from_str(&payload).map_err(internal_error)?;
        artifact.pinned_by_job_id = None;
        sqlx::query("UPDATE artifacts SET pinned_by_job_id = NULL, payload = ? WHERE id = ?")
            .bind(serde_json::to_string(&artifact).map_err(internal_error)?)
            .bind(artifact.id.to_string())
            .execute(state.database.pool())
            .await
            .map_err(storage_error)?;
    }
    Ok(())
}

fn segment_cache_fingerprint(segment: &SegmentPlan, operation: &str) -> CacheFingerprint {
    let mut settings = BTreeMap::new();
    settings.insert("operation".to_owned(), serde_json::json!(operation));
    settings.insert(
        "appliedRuleIds".to_owned(),
        serde_json::json!(segment.applied_rule_ids),
    );
    CacheFingerprint {
        schema_version: 1,
        text: segment.text.clone(),
        context: segment.context.clone(),
        provider_id: segment.assignment.provider_id.to_string(),
        provider_endpoint_family: provider_endpoint_family(&segment.assignment).to_owned(),
        provider_version: segment.assignment.provider_version.clone(),
        model: segment.assignment.model.clone(),
        voice: segment.assignment.voice_source.clone(),
        reference_audio_hashes: Vec::new(),
        performance: segment.assignment.performance.clone(),
        settings,
        dictionary_revision: segment.dictionary_revision.clone(),
        normalization_version: NORMALIZATION_VERSION.to_owned(),
    }
}

pub(crate) fn segment_semantic_input_hash(segment: &SegmentPlan) -> Result<String, ServiceError> {
    semantic_input_hash(
        &segment.text,
        segment.context.as_deref(),
        segment.assignment.provider_id,
        segment.assignment.model.as_deref(),
        segment.assignment.voice_id,
        &segment.dictionary_revision,
        &segment.assignment.performance,
    )
}

pub(crate) fn semantic_input_hash(
    text: &str,
    context: Option<&str>,
    provider_id: Uuid,
    model: Option<&str>,
    voice_id: Uuid,
    dictionary_revision: &str,
    performance: &PerformanceSettings,
) -> Result<String, ServiceError> {
    let value = serde_json::json!({
        "schemaVersion": 1,
        "text": text,
        "context": context,
        "providerProfileId": provider_id,
        "model": model,
        "voiceProfileId": voice_id,
        "dictionaryRevision": dictionary_revision,
        "normalizationVersion": NORMALIZATION_VERSION,
        "performance": performance,
    });
    let bytes = serde_json::to_vec(&value).map_err(internal_error)?;
    Ok(blake3::hash(&bytes).to_hex().to_string())
}

fn provider_endpoint_family(assignment: &SpeakerAssignment) -> &'static str {
    match assignment.provider_kind {
        ProviderKindView::Elevenlabs => "elevenlabs-v1",
        ProviderKindView::MlxAudio => "openai-audio-mlx",
        ProviderKindView::Localai => "openai-audio-localai",
        ProviderKindView::AlltalkV2 => "alltalk-v2",
        ProviderKindView::NativeOs => "native-os",
        ProviderKindView::OpenaiTts => "openai-speech-v1",
        ProviderKindView::Openai => "openai",
        ProviderKindView::OpenaiCompatible => "openai-compatible",
        ProviderKindView::Anthropic => "anthropic",
        ProviderKindView::Gemini => "gemini",
        ProviderKindView::Qwen => "qwen",
        ProviderKindView::Kimi => "kimi",
        ProviderKindView::Moonshot => "moonshot",
        ProviderKindView::LmStudio => "lm-studio",
        ProviderKindView::Ollama => "ollama",
    }
}

fn requested_audio_format(assignment: &SpeakerAssignment) -> AudioFormat {
    match assignment.provider_kind {
        ProviderKindView::Elevenlabs | ProviderKindView::OpenaiTts => AudioFormat::Mp3,
        _ => AudioFormat::Wav,
    }
}

async fn retry_policy(
    state: &AppState,
    segment: &SegmentPlan,
) -> Result<RetryPolicy, ServiceError> {
    let project_id = segment_project_id(state, segment.chapter_id).await?;
    let project = state
        .database
        .repositories()
        .projects
        .get_project(ProjectId::from_uuid(project_id))
        .await
        .map_err(storage_error)?
        .ok_or(ServiceError::NotFound)?;
    RetryPolicy::new(
        project
            .settings
            .reliability
            .max_transient_retries
            .saturating_add(1),
        Duration::from_millis(project.settings.reliability.base_backoff_ms),
        Duration::from_millis(project.settings.reliability.max_backoff_ms),
    )
    .map(|policy| {
        policy.with_uncertain_charge_retries(
            project.settings.reliability.retry_possible_duplicate_charge,
        )
    })
    .map_err(internal_error)
}

fn retry_reservation_multiplier(policy: &RetryPolicy) -> usize {
    if policy.retries_uncertain_charge() {
        usize::from(policy.max_attempts())
    } else {
        1
    }
}

fn provider_mode_matches_runtime(
    mode: ProviderModeView,
    runtime: audiobookai_providers::ProviderKind,
) -> bool {
    matches!(
        (mode, runtime),
        (
            ProviderModeView::CloudRemote,
            audiobookai_providers::ProviderKind::CloudRemote
        ) | (
            ProviderModeView::ExternalEndpoint,
            audiobookai_providers::ProviderKind::ExternalEndpoint
        ) | (
            ProviderModeView::ManagedChild,
            audiobookai_providers::ProviderKind::ManagedChild
        ) | (
            ProviderModeView::Native,
            audiobookai_providers::ProviderKind::Native
        )
    )
}

fn persisted_provider_mode_matches(
    expected: Option<ProviderModeView>,
    current: ProviderModeView,
) -> bool {
    expected == Some(current)
}

fn persisted_provider_snapshot_matches(expected: Option<Uuid>, current: Option<Uuid>) -> bool {
    expected.is_some() && expected == current
}

#[allow(clippy::too_many_lines)]
async fn validate_regeneration_retry_provider_snapshot(
    state: &AppState,
    job: &Job,
    segment: &SegmentPlan,
) -> Result<(), ServiceError> {
    let (profile, voice_matches) = {
        let catalog = state.catalog.read().await;
        let profile = catalog
            .providers
            .get(&segment.assignment.provider_id)
            .cloned()
            .ok_or_else(|| {
                ServiceError::Conflict(
                    "the regeneration provider was removed; start a new segment regeneration"
                        .to_owned(),
                )
            })?;
        let voice_matches = catalog.voices.iter().any(|voice| {
            voice.id == segment.assignment.voice_id
                && voice.provider_profile_id == segment.assignment.provider_id
        }) && catalog
            .voice_sources
            .get(&segment.assignment.voice_id)
            .is_some_and(|source| source == &segment.assignment.voice_source);
        (profile, voice_matches)
    };
    crate::api::validate_billable_tts_provider_readiness(&profile).map_err(|_| {
        ServiceError::Conflict(
            "the regeneration provider is no longer ready; start a new segment regeneration"
                .to_owned(),
        )
    })?;
    if profile.kind != segment.assignment.provider_kind
        || profile.endpoint != segment.assignment.provider_endpoint
        || !persisted_provider_mode_matches(segment.assignment.provider_mode, profile.mode)
        || !voice_matches
    {
        return Err(ServiceError::Conflict(
            "the regeneration provider or voice changed; start a new segment regeneration"
                .to_owned(),
        ));
    }
    if segment.assignment.model.is_none() && profile.model.is_some() {
        return Err(ServiceError::Conflict(
            "the regeneration provider's default model changed; start a new segment regeneration"
                .to_owned(),
        ));
    }
    let project = state
        .database
        .repositories()
        .projects
        .get_project(job.project_id)
        .await
        .map_err(storage_error)?
        .ok_or(ServiceError::NotFound)?;
    if matches!(profile.mode, ProviderModeView::CloudRemote) && !project.cloud_consent.book_text {
        return Err(ServiceError::Conflict(
            "cloud-text consent was revoked; start a new segment regeneration after granting consent"
                .to_owned(),
        ));
    }
    let domain_provider = state
        .database
        .repositories()
        .providers
        .get(ProviderProfileId::from_uuid(segment.assignment.provider_id))
        .await
        .map_err(storage_error)?
        .ok_or_else(|| {
            ServiceError::Conflict(
                "the regeneration provider profile no longer exists; start a new segment regeneration"
                    .to_owned(),
            )
        })?;
    let current_snapshot = domain_provider.capability_snapshot.as_ref();
    if !persisted_provider_snapshot_matches(
        segment.assignment.provider_snapshot_id,
        current_snapshot.map(|snapshot| snapshot.id.as_uuid()),
    ) || segment.assignment.provider_version
        != current_snapshot.and_then(|snapshot| snapshot.provider_version.clone())
    {
        return Err(ServiceError::Conflict(
            "the regeneration provider capability snapshot changed; start a new segment regeneration"
                .to_owned(),
        ));
    }
    let runtime_id =
        ProviderId::new(segment.assignment.provider_id.to_string()).map_err(internal_error)?;
    let runtime = state
        .providers
        .tts(&runtime_id)
        .await
        .map_err(|_| {
            ServiceError::Conflict(
                "the durable regeneration provider runtime is unavailable; start a new segment regeneration"
                    .to_owned(),
            )
        })?;
    if runtime.descriptor().id != runtime_id
        || runtime.descriptor().endpoint_family != provider_endpoint_family(&segment.assignment)
        || segment
            .assignment
            .provider_mode
            .is_none_or(|mode| !provider_mode_matches_runtime(mode, runtime.descriptor().kind))
    {
        return Err(ServiceError::Conflict(
            "the regeneration provider runtime identity changed; start a new segment regeneration"
                .to_owned(),
        ));
    }
    crate::api::validate_voice_direction(
        &segment.assignment.performance,
        &segment.assignment.timing,
        segment.assignment.model.as_deref(),
        profile.capabilities.as_ref(),
    )
    .map_err(|_| {
        ServiceError::Conflict(
            "the durable voice direction is no longer supported; start a new segment regeneration"
                .to_owned(),
        )
    })?;
    Ok(())
}

async fn segment_project_id(state: &AppState, chapter_id: Uuid) -> Result<Uuid, ServiceError> {
    let value = sqlx::query_scalar::<_, String>(
        "SELECT p.id FROM projects p JOIN chapters c ON c.book_id = p.book_id WHERE c.id = ?",
    )
    .bind(chapter_id.to_string())
    .fetch_optional(state.database.pool())
    .await
    .map_err(storage_error)?
    .ok_or(ServiceError::NotFound)?;
    Uuid::parse_str(&value).map_err(internal_error)
}

fn retry_service_error(
    _state: &AppState,
    _job_id: JobId,
    _segment: &SegmentPlan,
    error: &crate::runtime::RetryExecutionError,
) -> ServiceError {
    match error.failure_class() {
        Some(RetryFailureClass::UncertainCharge) => ServiceError::Conflict(format!(
            "a provider request timed out after dispatch and may have been charged; it was not retried: {error}"
        )),
        _ => ServiceError::Conflict(error.to_string()),
    }
}

#[derive(Clone)]
struct AttemptJournal {
    state: Arc<AppState>,
    unit_id: JobUnitId,
    usage_context: TtsUsageContext,
}

impl AttemptJournal {
    fn new(state: Arc<AppState>, unit_id: JobUnitId, usage_context: TtsUsageContext) -> Self {
        Self {
            state,
            unit_id,
            usage_context,
        }
    }
}

impl std::fmt::Debug for AttemptJournal {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("AttemptJournal")
            .field("unit_id", &self.unit_id)
            .field("job_id", &self.usage_context.job_id)
            .finish_non_exhaustive()
    }
}

impl RetryJournal for AttemptJournal {
    fn record(&self, event: RetryEvent) -> BoxFuture<'_, Result<(), RetryJournalError>> {
        let state = Arc::clone(&self.state);
        let unit_id = self.unit_id;
        let usage_context = self.usage_context.clone();
        Box::pin(async move {
            let (failure_class, uncertain_charge) = match event.outcome {
                RetryEventOutcome::Succeeded => (None, false),
                RetryEventOutcome::Failed { class, .. } => (
                    Some(core_failure_class(class)),
                    class == RetryFailureClass::UncertainCharge,
                ),
            };
            let attempt = JobAttempt {
                id: AttemptId::new(),
                job_unit_id: unit_id,
                ordinal: event.attempt.get(),
                started_at: event.recorded_at,
                finished_at: Some(event.recorded_at),
                failure_class,
                error_code: None,
                redacted_error: None,
                provider_request_id: Some(usage_context.provider_request_id.to_string()),
                uncertain_charge,
            };
            state
                .database
                .repositories()
                .jobs
                .insert_attempt(&attempt)
                .await
                .map_err(|error| RetryJournalError::new(error.to_string()))?;
            if uncertain_charge {
                let usage = ProviderUsage {
                    source: UsageSource::Estimated,
                    characters: u64::try_from(usage_context.segment.text.chars().count()).ok(),
                    request_id: Some(usage_context.provider_request_id.to_string()),
                    ..ProviderUsage::default()
                };
                append_tts_usage(
                    &state,
                    usage_context.job_id,
                    &usage_context.segment,
                    Some(attempt.id),
                    &usage,
                    true,
                    usage_context.rate_card_id,
                )
                .await
                .map_err(|error| RetryJournalError::new(error.to_string()))?;
            }
            Ok(())
        })
    }
}

async fn attempt_id_for_ordinal(
    state: &AppState,
    unit_id: JobUnitId,
    ordinal: u16,
) -> Result<Option<AttemptId>, ServiceError> {
    let payload = sqlx::query_scalar::<_, String>(
        "SELECT payload FROM job_attempts WHERE job_unit_id = ? AND ordinal = ? LIMIT 1",
    )
    .bind(unit_id.to_string())
    .bind(i64::from(ordinal))
    .fetch_optional(state.database.pool())
    .await
    .map_err(storage_error)?;
    payload
        .map(|payload| {
            serde_json::from_str::<JobAttempt>(&payload)
                .map(|attempt| attempt.id)
                .map_err(internal_error)
        })
        .transpose()
}

const fn core_failure_class(class: RetryFailureClass) -> audiobookai_core::FailureClass {
    match class {
        RetryFailureClass::Transient => audiobookai_core::FailureClass::Transport,
        RetryFailureClass::RateLimited => audiobookai_core::FailureClass::RateLimit,
        RetryFailureClass::Authentication => audiobookai_core::FailureClass::Authentication,
        RetryFailureClass::Validation | RetryFailureClass::Permanent => {
            audiobookai_core::FailureClass::Validation
        }
        RetryFailureClass::UncertainCharge => audiobookai_core::FailureClass::TimeoutAfterDispatch,
        RetryFailureClass::Cancelled => audiobookai_core::FailureClass::Cancelled,
    }
}

async fn normalize_provider_audio(
    sidecars: &SidecarPair,
    response: &SynthesisResponse,
    short_segment: bool,
) -> Result<Vec<u8>, ServiceError> {
    let suffix = match response
        .content_type
        .split(';')
        .next()
        .unwrap_or_default()
        .trim()
    {
        "audio/mpeg" | "audio/mp3" => ".mp3",
        "audio/flac" => ".flac",
        "audio/aac" | "audio/mp4" => ".aac",
        _ => ".wav",
    };
    let input = tempfile::Builder::new()
        .prefix("audiobookai-provider-")
        .suffix(suffix)
        .tempfile()
        .map_err(ServiceError::Io)?
        .into_temp_path();
    tokio::fs::write(&input, &response.audio).await?;
    let output = tempfile::Builder::new()
        .prefix("audiobookai-normalized-")
        .suffix(".flac")
        .tempfile()
        .map_err(ServiceError::Io)?
        .into_temp_path();
    let filter = if short_segment {
        "dynaudnorm=f=75:g=9:p=0.85:m=5,alimiter=limit=0.7079"
    } else {
        "loudnorm=I=-19:TP=-3:LRA=7"
    };
    let arguments = vec![
        "-hide_banner".to_owned(),
        "-nostdin".to_owned(),
        "-y".to_owned(),
        "-i".to_owned(),
        input.to_string_lossy().into_owned(),
        "-vn".to_owned(),
        "-af".to_owned(),
        filter.to_owned(),
        "-ar".to_owned(),
        "48000".to_owned(),
        "-ac".to_owned(),
        "1".to_owned(),
        "-c:a".to_owned(),
        "flac".to_owned(),
        "-compression_level".to_owned(),
        "8".to_owned(),
        "-f".to_owned(),
        "flac".to_owned(),
        output.to_string_lossy().into_owned(),
    ];
    run_process(&sidecars.ffmpeg, &arguments, "normalize provider audio").await?;
    let bytes = tokio::fs::read(&output).await?;
    if bytes.len() < 4 || &bytes[..4] != b"fLaC" {
        return Err(ServiceError::Internal(
            "FFmpeg did not produce a valid canonical FLAC segment".to_owned(),
        ));
    }
    Ok(bytes)
}

async fn decode_flac_pcm(sidecars: &SidecarPair, path: &Path) -> Result<Bytes, ServiceError> {
    let output = Command::new(&sidecars.ffmpeg)
        .args(["-hide_banner", "-loglevel", "error", "-nostdin", "-i"])
        .arg(path)
        .args(["-vn", "-ar", "48000", "-ac", "1", "-f", "f32le", "pipe:1"])
        .kill_on_drop(true)
        .output()
        .await?;
    if !output.status.success() {
        return Err(ServiceError::Internal(
            "could not decode progressive playback audio".to_owned(),
        ));
    }
    Ok(Bytes::from(output.stdout))
}

async fn ensure_cached_artifact(
    state: &AppState,
    job_id: JobId,
    chapter_id: Uuid,
    key: &audiobookai_media::CacheKey,
    kind: ArtifactKind,
) -> Result<Artifact, ServiceError> {
    if let Some(payload) =
        sqlx::query_scalar::<_, String>("SELECT payload FROM artifacts WHERE cache_key = ?")
            .bind(key.as_str())
            .fetch_optional(state.database.pool())
            .await
            .map_err(storage_error)?
    {
        let artifact: Artifact = serde_json::from_str(&payload).map_err(internal_error)?;
        if Path::new(&artifact.path).is_file() {
            return Ok(artifact);
        }
    }
    let path = cache(state).path(key);
    let duration = probe_duration_ms(&resolve_sidecars(state)?, &path).await?;
    let artifact = artifact_for_file(
        kind,
        &path,
        Some("audio/flac".to_owned()),
        Some(duration),
        Some(key.as_str().to_owned()),
        Some(job_id),
    )
    .await?;
    let project_id = segment_project_id(state, chapter_id).await?;
    persist_artifact(state, project_id, &artifact).await?;
    Ok(artifact)
}

async fn append_tts_usage(
    state: &AppState,
    job_id: JobId,
    segment: &SegmentPlan,
    attempt_id: Option<AttemptId>,
    usage: &ProviderUsage,
    uncertain_charge: bool,
    rate_card_id: Option<RateCardId>,
) -> Result<(), ServiceError> {
    if let Some(attempt_id) = attempt_id
        && let Some(payload) = sqlx::query_scalar::<_, String>(
            "SELECT payload FROM usage_ledger WHERE attempt_id = ? AND uncertain_charge = ? LIMIT 1",
        )
        .bind(attempt_id.to_string())
        .bind(uncertain_charge)
        .fetch_optional(state.database.pool())
        .await
        .map_err(storage_error)?
    {
        let stored = serde_json::from_str::<UsageEvent>(&payload).map_err(internal_error)?;
        ensure_tts_usage_row(state, segment, &stored).await;
        return Ok(());
    }
    let project_id = segment_project_id(state, segment.chapter_id).await?;
    let used_character_estimate = usage.characters.is_none();
    let quantities = UsageQuantities {
        characters: usage
            .characters
            .or_else(|| u64::try_from(segment.text.chars().count()).ok()),
        audio_milliseconds: usage.audio_milliseconds,
        input_tokens: usage.input_tokens,
        output_tokens: usage.output_tokens,
        cache_read_tokens: usage.cached_tokens,
        cache_write_tokens: None,
        reasoning_tokens: usage.reasoning_tokens,
        provider_credits: usage.credits_micros,
    };
    let mut event = UsageEvent {
        id: UsageEventId::new(),
        occurred_at: Utc::now(),
        workload: UsageWorkload::Tts,
        project_id: ProjectId::from_uuid(project_id),
        job_id: Some(job_id),
        attempt_id,
        chapter_id: Some(ChapterId::from_uuid(segment.chapter_id)),
        segment_id: segment.proofing.then_some(segment.id),
        provider_profile_id: ProviderProfileId::from_uuid(segment.assignment.provider_id),
        provider_family: provider_endpoint_family(&segment.assignment).to_owned(),
        endpoint_family: provider_endpoint_family(&segment.assignment).to_owned(),
        model: segment.assignment.model.clone(),
        voice_profile_id: Some(VoiceProfileId::from_uuid(segment.assignment.voice_id)),
        provider_request_id: usage.request_id.clone(),
        quantities: quantities.clone(),
        quantity_source: match (usage.source, used_character_estimate) {
            (UsageSource::Reported, false) => ProvenanceQuality::Reported,
            (UsageSource::Reported | UsageSource::Estimated | UsageSource::Unknown, true)
            | (UsageSource::Estimated, false) => ProvenanceQuality::Estimated,
            (UsageSource::Unknown, false) => ProvenanceQuality::Unknown,
        },
        cost: None,
        cost_source: ProvenanceQuality::Unknown,
        rate_card_id: None,
        uncertain_charge,
        redacted_raw_usage: usage
            .raw_redacted
            .as_ref()
            .and_then(serde_json::Value::as_object)
            .map(|map| {
                map.iter()
                    .map(|(key, value)| (key.clone(), value.clone()))
                    .collect()
            })
            .unwrap_or_default(),
    };
    crate::accounting::apply_rate_card_snapshot(state, &mut event, rate_card_id).await?;
    state
        .database
        .repositories()
        .usage
        .append(&event)
        .await
        .map_err(storage_error)?;
    ensure_tts_usage_row(state, segment, &event).await;
    Ok(())
}

async fn ensure_tts_usage_row(state: &AppState, segment: &SegmentPlan, event: &UsageEvent) {
    let project_title = state
        .catalog
        .read()
        .await
        .projects
        .get(&event.project_id.as_uuid())
        .map(|value| value.summary.title.clone());
    let mut catalog = state.catalog.write().await;
    if catalog
        .usage_rows
        .iter()
        .any(|row| row.id == event.id.as_uuid())
    {
        return;
    }
    catalog.usage_rows.insert(
        0,
        UsageRowView {
            id: event.id.as_uuid(),
            occurred_at: event.occurred_at,
            project_title,
            provider_name: segment.assignment.provider_name.clone(),
            operation: if event.uncertain_charge {
                "tts_uncertain_charge".to_owned()
            } else {
                "tts".to_owned()
            },
            model: event.model.clone(),
            voice: Some(segment.assignment.voice_name.clone()),
            characters: event.quantities.characters,
            input_tokens: event.quantities.input_tokens,
            output_tokens: event.quantities.output_tokens,
            cost_micros: event.cost.as_ref().map(|cost| cost.micros),
            currency: event.cost.as_ref().map(|cost| cost.currency.clone()),
            provenance: format!("{:?}", event.quantity_source).to_lowercase(),
            request_id: event.provider_request_id.clone(),
        },
    );
}

fn redacted_endpoint(endpoint: Option<&str>) -> Option<String> {
    endpoint.and_then(|value| {
        let parsed = url::Url::parse(value).ok()?;
        Some(format!(
            "{}://{}{}",
            parsed.scheme(),
            parsed.host_str()?,
            parsed
                .port()
                .map_or_else(String::new, |port| format!(":{port}"))
        ))
    })
}

#[allow(clippy::too_many_arguments, clippy::too_many_lines)]
async fn assemble_chapter(
    state: &Arc<AppState>,
    job_id: JobId,
    chapter: ChapterPlan,
    mut unit: JobUnit,
    mut segments: Vec<SegmentArtifact>,
    verify_selected_artifacts: bool,
    sidecars: &SidecarPair,
    progress_guard: &tokio::sync::Mutex<()>,
) -> Result<ChapterArtifact, ServiceError> {
    wait_until_runnable(state, job_id).await?;
    update_unit_state(state, &mut unit, JobUnitState::Running, None).await?;
    set_job_message(
        state,
        job_id,
        &format!("Assembling {}", chapter.chapter.title),
    )
    .await?;
    segments.sort_by_key(|segment| segment.plan.segment_ordinal);
    let output_directory = state
        .config
        .data_dir
        .join("jobs")
        .join(job_id.to_string())
        .join("chapters");
    tokio::fs::create_dir_all(&output_directory).await?;
    let destination = output_directory.join(format!(
        "{:04}-{}.flac",
        chapter.chapter.ordinal, chapter.chapter.id
    ));
    let temporary = tempfile::Builder::new()
        .prefix(".chapter-")
        .suffix(".flac")
        .tempfile_in(&output_directory)
        .map_err(ServiceError::Io)?
        .into_temp_path();
    let mut arguments = vec![
        "-hide_banner".to_owned(),
        "-nostdin".to_owned(),
        "-y".to_owned(),
    ];
    for segment in &segments {
        arguments.extend(["-i".to_owned(), segment.artifact.path.clone()]);
    }
    let mut filter = String::new();
    for (index, segment) in segments.iter().enumerate() {
        write!(filter, "[{index}:a]aresample=48000:async=1:first_pts=0")
            .expect("writing to String cannot fail");
        if let Some(milliseconds) = segment.plan.assignment.timing.pause_before_ms {
            write!(filter, ",adelay={milliseconds}:all=1").expect("writing to String cannot fail");
        }
        if let Some(milliseconds) = segment.plan.assignment.timing.pause_after_ms {
            let seconds = f64::from(milliseconds) / 1_000.0;
            write!(filter, ",apad=pad_dur={seconds:.3}").expect("writing to String cannot fail");
        }
        write!(filter, "[a{index}];").expect("writing to String cannot fail");
    }
    for index in 0..segments.len() {
        write!(filter, "[a{index}]").expect("writing to String cannot fail");
    }
    write!(filter, "concat=n={}:v=0:a=1[outa]", segments.len())
        .expect("writing to String cannot fail");
    arguments.extend([
        "-filter_complex".to_owned(),
        filter,
        "-map".to_owned(),
        "[outa]".to_owned(),
        "-ar".to_owned(),
        "48000".to_owned(),
        "-ac".to_owned(),
        "1".to_owned(),
        "-c:a".to_owned(),
        "flac".to_owned(),
        "-compression_level".to_owned(),
        "8".to_owned(),
        "-f".to_owned(),
        "flac".to_owned(),
        temporary.to_string_lossy().into_owned(),
    ]);
    // A pause can begin after the worker's initial proof-export validation. Check the job state
    // again at the actual input-consumption boundary, then re-hash every selected take before
    // FFmpeg is allowed to open it.
    wait_until_runnable(state, job_id).await?;
    if verify_selected_artifacts {
        let artifacts = segments
            .iter()
            .map(|segment| &segment.artifact)
            .collect::<Vec<_>>();
        verify_selected_artifacts_before_use(&artifacts).await?;
    }
    run_process(&sidecars.ffmpeg, &arguments, "assemble chapter").await?;
    validate_flac(&temporary).await?;
    atomic_promote(&temporary, &destination).await?;
    let duration_ms = probe_duration_ms(sidecars, &destination).await?;
    let artifact = artifact_for_file(
        ArtifactKind::ChapterMaster,
        &destination,
        Some("audio/flac".to_owned()),
        Some(duration_ms),
        None,
        Some(job_id),
    )
    .await?;
    persist_artifact(
        state,
        chapter_project_id(state, chapter.chapter.id).await?,
        &artifact,
    )
    .await?;
    unit.output_artifact_id = Some(artifact.id);
    update_unit_state(state, &mut unit, JobUnitState::Completed, None).await?;
    increment_job_progress(state, job_id, progress_guard).await?;
    Ok(ChapterArtifact {
        chapter: chapter.chapter,
        artifact,
    })
}

async fn chapter_project_id(state: &AppState, chapter_id: ChapterId) -> Result<Uuid, ServiceError> {
    segment_project_id(state, chapter_id.as_uuid()).await
}

// Export assembly coordinates media metadata, atomic outputs, job units, and
// manifests; splitting the sequence would increase partial-output risk.
#[allow(clippy::too_many_arguments, clippy::too_many_lines)]
async fn export_book(
    state: &Arc<AppState>,
    job_id: JobId,
    plan: &ConversionPlan,
    chapters: &[ChapterArtifact],
    sidecars: &SidecarPair,
    units: &mut PersistedUnitPlan,
    progress_guard: &tokio::sync::Mutex<()>,
) -> Result<(Vec<Artifact>, Artifact), ServiceError> {
    let media_chapters = chapters
        .iter()
        .map(|chapter| ChapterAudio {
            title: chapter.chapter.title.clone(),
            path: PathBuf::from(&chapter.artifact.path),
            duration_milliseconds: chapter.artifact.duration_ms.unwrap_or(1).max(1),
        })
        .collect::<Vec<_>>();
    let output_directory = PathBuf::from(&plan.export.output_directory);
    ensure_export_root_identity(&plan.export).await?;
    let extension = media_export_format(plan.export.format).extension();
    let final_output = if plan.export.layout == ExportLayout::PerChapter {
        output_directory.join(&plan.export.filename_template)
    } else {
        output_directory.join(format!("{}.{}", plan.export.filename_template, extension))
    };
    require_output_reservation(state, job_id, plan.project.id, &plan.export, &final_output).await?;
    if units.export.state == JobUnitState::Completed {
        return load_completed_export_result(state, job_id).await;
    }
    let staging_directory = prepare_private_export_staging(state, job_id).await?;
    let temporary_output =
        export_staging_output_path(&staging_directory, plan.export.layout, extension);
    if final_output.exists() {
        ensure_export_root_identity(&plan.export).await?;
        let recovered = recover_promoted_export(
            state,
            job_id,
            plan.export.layout,
            &temporary_output,
            &final_output,
        )
        .await?;
        state
            .database
            .repositories()
            .jobs
            .mark_output_promoted(job_id, Utc::now())
            .await
            .map_err(storage_error)?;
        return finalize_export_outputs(
            state,
            job_id,
            plan,
            chapters,
            sidecars,
            units,
            progress_guard,
            recovered,
            &final_output,
        )
        .await;
    }
    if plan.export.layout == ExportLayout::PerChapter {
        ensure_private_directory(&temporary_output).await?;
    } else {
        ensure_private_staging_file_path(&staging_directory, &temporary_output).await?;
    }
    let metadata = MediaBookMetadata {
        title: plan.project.metadata.title.clone(),
        authors: plan.project.metadata.authors.clone(),
        narrator: plan.project.metadata.narrator.clone(),
        series: plan
            .project
            .metadata
            .series
            .as_ref()
            .map(|series| series.name.clone()),
        series_position: plan
            .project
            .metadata
            .series
            .as_ref()
            .and_then(|series| series.position)
            .map(f64::from),
        language: plan.project.metadata.language.clone(),
        date: None,
        description: plan.project.metadata.description.clone(),
        isbn: plan.project.metadata.identifier.clone(),
        additional: BTreeMap::from([(
            "publisher".to_owned(),
            plan.project.metadata.publisher.clone().unwrap_or_default(),
        )]),
    };
    let background_music = plan.music_path.as_ref().map(|path| {
        let settings = plan
            .export
            .background_music
            .as_ref()
            .expect("music path requires settings");
        BackgroundMusic {
            path: path.clone(),
            trim_start_seconds: Duration::from_millis(settings.trim_start_ms).as_secs_f64(),
            trim_end_seconds: settings
                .trim_end_ms
                .map(|value| Duration::from_millis(value).as_secs_f64()),
            gain_db: f64::from(settings.gain_db),
            fade_in_seconds: Duration::from_millis(settings.fade_in_ms).as_secs_f64(),
            fade_out_seconds: Duration::from_millis(settings.fade_out_ms).as_secs_f64(),
            duck_threshold: settings.ducking.as_ref().map_or(0.03, |ducking| {
                10_f64.powf(f64::from(ducking.threshold_db) / 20.0)
            }),
            duck_ratio: settings.ducking.as_ref().map_or(1.0, |_| 8.0),
        }
    });
    let cover = state
        .config
        .data_dir
        .join("library")
        .join(plan.project.id.to_string())
        .join("cover.bin");
    let mut request = ExportRequest::audiobook_defaults(
        media_chapters,
        temporary_output.clone(),
        media_export_format(plan.export.format),
        metadata,
    );
    request.split_per_chapter = plan.export.layout == ExportLayout::PerChapter;
    request.cover_art = cover.is_file().then_some(cover);
    request.background_music = background_music;
    request.loudness = Some(LoudnessSettings {
        target_lufs: f64::from(plan.export.audio.target_lufs),
        true_peak_db: f64::from(plan.export.audio.true_peak_db),
        loudness_range: 7.0,
    });
    request.preview = false;
    request.overwrite = true;
    request.bitrate_kbps = plan
        .export
        .audio
        .bitrate_kbps
        .and_then(|value| u16::try_from(value).ok())
        .unwrap_or(128);
    request.sample_rate = 48_000;
    request.channels = plan.export.audio.channels;

    wait_until_runnable(state, job_id).await?;
    if units.normalize.state != JobUnitState::Completed {
        update_unit_state(state, &mut units.normalize, JobUnitState::Running, None).await?;
        set_job_message(state, job_id, "Measuring final loudness").await?;
    }
    let planner = ExportPlanner::new(sidecars.clone());
    let analyses = planner.loudness_analysis(&request).map_err(media_error)?;
    let mut measurements = Vec::<LoudnessMeasurement>::new();
    for invocation in analyses {
        let stderr = run_process_capture(
            &invocation.executable,
            &invocation.arguments,
            &invocation.purpose,
        )
        .await?;
        measurements.push(parse_loudness_measurement(&stderr).map_err(media_error)?);
    }
    if units.normalize.state != JobUnitState::Completed {
        update_unit_state(state, &mut units.normalize, JobUnitState::Completed, None).await?;
        increment_job_progress(state, job_id, progress_guard).await?;
    }

    wait_until_runnable(state, job_id).await?;
    if units.export.state != JobUnitState::Completed {
        update_unit_state(state, &mut units.export, JobUnitState::Running, None).await?;
    }
    set_job_message(state, job_id, "Writing final audiobook").await?;
    let render = planner
        .render(&request, &measurements)
        .map_err(media_error)?;
    for auxiliary in &render.auxiliary_files {
        write_job_staging_file_atomically(
            &staging_directory,
            &auxiliary.path,
            auxiliary.contents.as_bytes(),
        )
        .await?;
    }
    for path in &render.outputs {
        ensure_private_staging_file_path(&staging_directory, path).await?;
    }
    for invocation in &render.invocations {
        run_process(
            &invocation.executable,
            &invocation.arguments,
            &invocation.purpose,
        )
        .await?;
    }
    let mut rendered = Vec::new();
    for path in &render.outputs {
        let duration = probe_duration_ms(sidecars, path).await?;
        if duration == 0 {
            return Err(ServiceError::Internal(format!(
                "FFmpeg produced an empty export: {}",
                path.display()
            )));
        }
        sync_file(path).await?;
        rendered.push((path.clone(), duration));
    }

    persist_export_promotion_marker(state, job_id, &final_output, &rendered).await?;
    state
        .database
        .repositories()
        .jobs
        .mark_output_promoting(job_id, Utc::now())
        .await
        .map_err(storage_error)?;

    ensure_export_root_identity(&plan.export).await?;
    let final_paths = if plan.export.layout == ExportLayout::PerChapter {
        create_directory_no_clobber(&final_output).await?;
        ensure_existing_real_directory(&final_output, "split export destination").await?;
        mark_split_export_directory_created(state, job_id, &final_output).await?;
        let mut final_paths = Vec::new();
        for (path, duration) in rendered {
            let file_name = path.file_name().ok_or_else(|| {
                ServiceError::Internal("split export has no file name".to_owned())
            })?;
            let destination = final_output.join(file_name);
            ensure_export_root_identity(&plan.export).await?;
            ensure_existing_real_directory(&final_output, "split export destination").await?;
            atomic_promote(&path, &destination).await?;
            final_paths.push((destination, duration));
        }
        let _ = tokio::fs::remove_dir(&temporary_output).await;
        final_paths
    } else {
        let duration = rendered
            .first()
            .map(|(_, duration)| *duration)
            .ok_or_else(|| ServiceError::Internal("export produced no files".to_owned()))?;
        ensure_export_root_identity(&plan.export).await?;
        atomic_promote(&temporary_output, &final_output).await?;
        vec![(final_output.clone(), duration)]
    };
    state
        .database
        .repositories()
        .jobs
        .mark_output_promoted(job_id, Utc::now())
        .await
        .map_err(storage_error)?;

    finalize_export_outputs(
        state,
        job_id,
        plan,
        chapters,
        sidecars,
        units,
        progress_guard,
        final_paths,
        &final_output,
    )
    .await
}

fn export_staging_output_path(
    staging_directory: &Path,
    layout: ExportLayout,
    extension: &str,
) -> PathBuf {
    if layout == ExportLayout::PerChapter {
        staging_directory.join("chapters")
    } else {
        staging_directory.join(format!("audiobook.{extension}"))
    }
}

fn export_promotion_marker_path(staging_directory: &Path) -> PathBuf {
    staging_directory.join("export-promotion.json")
}

async fn persist_export_promotion_marker(
    state: &AppState,
    job_id: JobId,
    final_output: &Path,
    rendered: &[(PathBuf, u64)],
) -> Result<(), ServiceError> {
    let staging_directory = prepare_private_export_staging(state, job_id).await?;
    let mut files = Vec::with_capacity(rendered.len());
    for (path, duration_ms) in rendered {
        ensure_private_staging_file_path(&staging_directory, path).await?;
        let file_name = path
            .file_name()
            .and_then(std::ffi::OsStr::to_str)
            .ok_or_else(|| ServiceError::Internal("rendered export has no file name".to_owned()))?;
        files.push(ExportPromotionFile {
            file_name: file_name.to_owned(),
            duration_ms: *duration_ms,
            fingerprint: fingerprint_file(path).await?,
        });
    }
    if files.is_empty() {
        return Err(ServiceError::Internal(
            "cannot promote an export without rendered files".to_owned(),
        ));
    }
    let marker = ExportPromotionMarker {
        schema_version: 1,
        job_id,
        final_output: final_output.to_string_lossy().into_owned(),
        split_directory_created: false,
        files,
    };
    let path = export_promotion_marker_path(&staging_directory);
    write_job_staging_file_atomically(
        &staging_directory,
        &path,
        &serde_json::to_vec_pretty(&marker).map_err(internal_error)?,
    )
    .await
}

async fn mark_split_export_directory_created(
    state: &AppState,
    job_id: JobId,
    final_output: &Path,
) -> Result<(), ServiceError> {
    ensure_existing_real_directory(final_output, "split export destination").await?;
    let staging_directory = prepare_private_export_staging(state, job_id).await?;
    let marker_path = export_promotion_marker_path(&staging_directory);
    ensure_private_staging_file_path(&staging_directory, &marker_path).await?;
    let mut marker: ExportPromotionMarker =
        serde_json::from_slice(&tokio::fs::read(&marker_path).await.map_err(|error| {
            ServiceError::Conflict(format!(
                "split export directory exists without a readable job-owned promotion marker: {error}"
            ))
        })?)
        .map_err(internal_error)?;
    if marker.schema_version != 1
        || marker.job_id != job_id
        || Path::new(&marker.final_output) != final_output
        || marker.files.is_empty()
    {
        return Err(ServiceError::Conflict(
            "split export promotion marker does not match this durable job".to_owned(),
        ));
    }
    marker.split_directory_created = true;
    write_job_staging_file_atomically(
        &staging_directory,
        &marker_path,
        &serde_json::to_vec_pretty(&marker).map_err(internal_error)?,
    )
    .await
}

async fn verify_promoted_file(path: &Path, expected: &FileFingerprint) -> Result<(), ServiceError> {
    let metadata = tokio::fs::symlink_metadata(path).await.map_err(|error| {
        ServiceError::Conflict(format!(
            "promoted export file is unavailable ({}): {error}",
            path.display()
        ))
    })?;
    if metadata.file_type().is_symlink() || !metadata.is_file() {
        return Err(ServiceError::Conflict(format!(
            "promoted export path is not a regular job-owned file: {}",
            path.display()
        )));
    }
    let actual = fingerprint_file(path).await?;
    if &actual != expected {
        return Err(ServiceError::Conflict(format!(
            "promoted export file no longer matches its durable marker: {}",
            path.display()
        )));
    }
    Ok(())
}

async fn recover_promoted_export(
    state: &AppState,
    job_id: JobId,
    layout: ExportLayout,
    temporary_output: &Path,
    final_output: &Path,
) -> Result<Vec<(PathBuf, u64)>, ServiceError> {
    let staging_directory = prepare_private_export_staging(state, job_id).await?;
    let marker_path = export_promotion_marker_path(&staging_directory);
    ensure_private_staging_file_path(&staging_directory, &marker_path).await?;
    if layout == ExportLayout::PerChapter {
        ensure_existing_real_directory(final_output, "split export destination").await?;
    }
    let marker: ExportPromotionMarker =
        serde_json::from_slice(&tokio::fs::read(&marker_path).await.map_err(|error| {
            ServiceError::Conflict(format!(
                "export destination exists without a readable job-owned promotion marker: {error}"
            ))
        })?)
        .map_err(internal_error)?;
    if marker.schema_version != 1
        || marker.job_id != job_id
        || Path::new(&marker.final_output) != final_output
        || marker.files.is_empty()
        || (layout == ExportLayout::SingleFile && marker.files.len() != 1)
    {
        return Err(ServiceError::Conflict(
            "export promotion marker does not match this durable job".to_owned(),
        ));
    }
    if layout == ExportLayout::PerChapter && !marker.split_directory_created {
        return Err(ServiceError::Conflict(
            "split export directory was not durably created by this job".to_owned(),
        ));
    }
    let mut names = BTreeSet::new();
    let mut recovered = Vec::with_capacity(marker.files.len());
    for file in marker.files {
        let component = Path::new(&file.file_name);
        if component.file_name().is_none()
            || component
                .parent()
                .is_some_and(|parent| !parent.as_os_str().is_empty())
            || !names.insert(file.file_name.clone())
            || file.duration_ms == 0
        {
            return Err(ServiceError::Conflict(
                "export promotion marker contains an invalid output entry".to_owned(),
            ));
        }
        let destination = if layout == ExportLayout::PerChapter {
            final_output.join(&file.file_name)
        } else {
            final_output.to_path_buf()
        };
        if destination.exists() {
            verify_promoted_file(&destination, &file.fingerprint).await?;
        } else {
            let source = if layout == ExportLayout::PerChapter {
                temporary_output.join(&file.file_name)
            } else {
                temporary_output.to_path_buf()
            };
            verify_promoted_file(&source, &file.fingerprint).await?;
            atomic_promote(&source, &destination).await?;
        }
        recovered.push((destination, file.duration_ms));
    }
    if layout == ExportLayout::PerChapter {
        let _ = tokio::fs::remove_dir(temporary_output).await;
    }
    Ok(recovered)
}

async fn ensure_export_manifest_file(
    path: &Path,
    job_id: JobId,
    value: &serde_json::Value,
) -> Result<(), ServiceError> {
    if path.exists() {
        let existing: serde_json::Value =
            serde_json::from_slice(&tokio::fs::read(path).await?).map_err(internal_error)?;
        if existing == *value {
            return Ok(());
        }
        if existing.get("jobId") == Some(&serde_json::json!(job_id)) {
            return Err(ServiceError::Conflict(format!(
                "existing job-owned export manifest does not match the immutable export snapshot: {}",
                path.display()
            )));
        }
        return Err(ServiceError::Conflict(format!(
            "export manifest belongs to another job: {}",
            path.display()
        )));
    }
    write_file_atomically(
        path,
        &serde_json::to_vec_pretty(value).map_err(internal_error)?,
    )
    .await
}

async fn ensure_job_artifact(
    state: &AppState,
    project_id: Uuid,
    job_id: JobId,
    kind: ArtifactKind,
    path: &Path,
    media_type: Option<String>,
    duration_ms: Option<u64>,
) -> Result<Artifact, ServiceError> {
    let candidate =
        artifact_for_file(kind, path, media_type, duration_ms, None, Some(job_id)).await?;
    let existing = sqlx::query_scalar::<_, String>(
        "SELECT payload FROM artifacts WHERE pinned_by_job_id = ? AND kind = ? AND path = ? \
         ORDER BY created_at LIMIT 1",
    )
    .bind(job_id.to_string())
    .bind(artifact_kind_name(kind))
    .bind(&candidate.path)
    .fetch_optional(state.database.pool())
    .await
    .map_err(storage_error)?;
    if let Some(existing) = existing {
        let existing: Artifact = serde_json::from_str(&existing).map_err(internal_error)?;
        if existing.fingerprint != candidate.fingerprint {
            return Err(ServiceError::Conflict(format!(
                "persisted export artifact changed during recovery: {}",
                candidate.path
            )));
        }
        return Ok(existing);
    }
    persist_artifact(state, project_id, &candidate).await?;
    Ok(candidate)
}

async fn load_completed_export_result(
    state: &AppState,
    job_id: JobId,
) -> Result<(Vec<Artifact>, Artifact), ServiceError> {
    let payloads = sqlx::query_scalar::<_, String>(
        "SELECT payload FROM artifacts WHERE pinned_by_job_id = ? \
         AND kind IN ('export', 'export_manifest') ORDER BY created_at, id",
    )
    .bind(job_id.to_string())
    .fetch_all(state.database.pool())
    .await
    .map_err(storage_error)?;
    let mut exports = Vec::new();
    let mut manifest = None;
    for payload in payloads {
        let artifact: Artifact = serde_json::from_str(&payload).map_err(internal_error)?;
        if fingerprint_file(Path::new(&artifact.path)).await? != artifact.fingerprint {
            return Err(ServiceError::Conflict(format!(
                "completed export artifact no longer matches its fingerprint: {}",
                artifact.path
            )));
        }
        match artifact.kind {
            ArtifactKind::Export => exports.push(artifact),
            ArtifactKind::ExportManifest => {
                manifest.get_or_insert(artifact);
            }
            _ => unreachable!("query restricts export artifact kinds"),
        }
    }
    exports.sort_by(|left, right| left.path.cmp(&right.path));
    if exports.is_empty() {
        return Err(ServiceError::Conflict(
            "completed export unit has no durable output artifacts".to_owned(),
        ));
    }
    let manifest = manifest.ok_or_else(|| {
        ServiceError::Conflict("completed export unit has no durable manifest".to_owned())
    })?;
    Ok((exports, manifest))
}

#[allow(clippy::too_many_arguments)]
async fn finalize_export_outputs(
    state: &Arc<AppState>,
    job_id: JobId,
    plan: &ConversionPlan,
    chapters: &[ChapterArtifact],
    sidecars: &SidecarPair,
    units: &mut PersistedUnitPlan,
    progress_guard: &tokio::sync::Mutex<()>,
    final_paths: Vec<(PathBuf, u64)>,
    final_output: &Path,
) -> Result<(Vec<Artifact>, Artifact), ServiceError> {
    let ffmpeg_build = ffmpeg_build_description(sidecars).await?;
    let manifest_value =
        export_manifest_value(state, job_id, plan, chapters, &final_paths, &ffmpeg_build).await?;
    ensure_export_root_identity(&plan.export).await?;
    if plan.export.layout == ExportLayout::PerChapter {
        ensure_existing_real_directory(final_output, "split export destination").await?;
    }
    let manifest_path = export_manifest_path(plan.export.layout, final_output);
    ensure_export_manifest_file(&manifest_path, job_id, &manifest_value).await?;
    let manifest_artifact = ensure_job_artifact(
        state,
        plan.project.id.as_uuid(),
        job_id,
        ArtifactKind::ExportManifest,
        &manifest_path,
        Some("application/json".to_owned()),
        None,
    )
    .await?;
    let mut artifacts = Vec::new();
    for (path, duration) in final_paths {
        artifacts.push(
            ensure_job_artifact(
                state,
                plan.project.id.as_uuid(),
                job_id,
                ArtifactKind::Export,
                &path,
                Some(media_type_for_path(&path)),
                Some(duration),
            )
            .await?,
        );
    }
    units.export.output_artifact_id = artifacts.first().map(|artifact| artifact.id);
    if units.export.state != JobUnitState::Completed {
        update_unit_state(state, &mut units.export, JobUnitState::Completed, None).await?;
        increment_job_progress(state, job_id, progress_guard).await?;
    }
    Ok((artifacts, manifest_artifact))
}

async fn export_manifest_value(
    state: &AppState,
    job_id: JobId,
    plan: &ConversionPlan,
    chapters: &[ChapterArtifact],
    final_paths: &[(PathBuf, u64)],
    ffmpeg_build: &str,
) -> Result<serde_json::Value, ServiceError> {
    let usage = state
        .database
        .repositories()
        .usage
        .totals(&audiobookai_storage::repositories::UsageFilter {
            job_id: Some(job_id),
            ..audiobookai_storage::repositories::UsageFilter::default()
        })
        .await
        .map_err(storage_error)?;
    let mut chapter_start = 0_u64;
    let chapter_markers = chapters
        .iter()
        .map(|chapter| {
            let duration = chapter.artifact.duration_ms.unwrap_or_default();
            let start = chapter_start;
            chapter_start = chapter_start.saturating_add(duration);
            serde_json::json!({
                "chapterId": chapter.chapter.id,
                "title": chapter.chapter.title,
                "startMilliseconds": start,
                "endMilliseconds": chapter_start,
            })
        })
        .collect::<Vec<_>>();
    let mut voices = BTreeMap::<Uuid, serde_json::Value>::new();
    for segment in plan.chapters.iter().flat_map(|chapter| &chapter.segments) {
        voices
            .entry(segment.assignment.voice_id)
            .or_insert_with(|| {
                serde_json::json!({
                    "character": segment.assignment.character_name,
                    "providerProfileId": segment.assignment.provider_id,
                    "providerFamily": provider_endpoint_family(&segment.assignment),
                    "providerVersion": segment.assignment.provider_version,
                    "model": segment.assignment.model,
                    "voiceProfileId": segment.assignment.voice_id,
                    "voiceName": segment.assignment.voice_name,
                })
            });
    }
    let mut dictionary_revisions = BTreeMap::new();
    for rule in &plan.rules {
        dictionary_revisions.insert(rule.id.to_string(), u64::from(rule.order));
    }
    let job = state
        .database
        .repositories()
        .jobs
        .get(job_id)
        .await
        .map_err(storage_error)?
        .ok_or(ServiceError::NotFound)?;
    let proofing_snapshot = if job.kind == JobKind::Export {
        Some(
            serde_json::to_value(load_required_proof_export_snapshot(state, &job).await?)
                .map_err(internal_error)?,
        )
    } else {
        None
    };
    Ok(serde_json::json!({
        "schemaVersion": 1,
        "projectId": plan.project.id,
        "jobId": job_id,
        "createdAt": job.created_at,
        "source": plan.book.source_fingerprint,
        "metadata": plan.project.metadata,
        "outputFormat": format_name(plan.export.format),
        "layout": layout_name(plan.export.layout),
        "outputFiles": final_paths.iter().map(|(path, _)| path.to_string_lossy().into_owned()).collect::<Vec<_>>(),
        "chapterMarkers": chapter_markers,
        "voiceProvenance": voices.into_values().collect::<Vec<_>>(),
        "dictionaryRevisions": dictionary_revisions,
        "proofingSnapshot": proofing_snapshot,
        "audio": plan.export.audio,
        "ffmpegBuild": ffmpeg_build,
        "usageTotals": usage,
    }))
}

async fn update_export_catalog(
    state: &AppState,
    job_id: JobId,
    plan: &ConversionPlan,
    artifacts: &[Artifact],
    manifest_id: ArtifactId,
) -> Result<(), ServiceError> {
    let mut views = Vec::new();
    let part_count = u32::try_from(artifacts.len()).unwrap_or(u32::MAX);
    for (part_index, artifact) in artifacts.iter().enumerate() {
        let path = Path::new(&artifact.path);
        let size = tokio::fs::metadata(path).await?.len();
        views.push(ExportArtifactView {
            id: artifact.id.as_uuid(),
            project_id: plan.project.id.as_uuid(),
            job_id: job_id.as_uuid(),
            part_index: u32::try_from(part_index).unwrap_or(u32::MAX),
            part_count,
            project_title: plan.project.metadata.title.clone(),
            format: format_name(plan.export.format).to_owned(),
            split_mode: layout_name(plan.export.layout).to_owned(),
            file_name: path
                .file_name()
                .and_then(std::ffi::OsStr::to_str)
                .unwrap_or("audiobook")
                .to_owned(),
            size_bytes: size,
            duration_seconds: artifact.duration_ms.unwrap_or_default().div_ceil(1_000),
            created_at: artifact.created_at,
            download_url: format!("/api/v1/artifacts/{}", artifact.id),
            manifest_url: format!("/api/v1/artifacts/{manifest_id}"),
            chapter_markers: plan.export.layout == ExportLayout::SingleFile
                && !matches!(plan.export.format, ExportFormat::Wav),
        });
    }
    state.catalog.write().await.exports.extend(views);
    Ok(())
}

fn resolve_sidecars(state: &AppState) -> Result<SidecarPair, ServiceError> {
    if let Some(packaged_root) = &state.config.bundled_sidecar_dir {
        return SidecarResolver::bundled(packaged_root)
            .resolve()
            .map_err(|error| {
                ServiceError::Conflict(format!(
                    "packaged FFmpeg sidecars failed validation: {error}"
                ))
            });
    }
    let mut directories = vec![state.config.data_dir.join("sidecars").join("bin")];
    if let Ok(executable) = std::env::current_exe()
        && let Some(parent) = executable.parent()
    {
        directories.push(parent.join("sidecars").join("bin"));
        directories.push(parent.join("../Resources/sidecars/bin"));
    }
    let explicit = match (
        std::env::var_os("AUDIOBOOKAI_FFMPEG"),
        std::env::var_os("AUDIOBOOKAI_FFPROBE"),
    ) {
        (Some(ffmpeg), Some(ffprobe)) => Some((PathBuf::from(ffmpeg), PathBuf::from(ffprobe))),
        (None, None) => None,
        _ => {
            return Err(ServiceError::InvalidRequest(
                "set both AUDIOBOOKAI_FFMPEG and AUDIOBOOKAI_FFPROBE".to_owned(),
            ));
        }
    };
    let allow_system = cfg!(debug_assertions)
        || std::env::var("AUDIOBOOKAI_ALLOW_SYSTEM_FFMPEG").is_ok_and(|value| value == "1");
    let mut last_error = None;
    for directory in directories {
        let mut resolver = SidecarResolver::bundled(directory).allow_system_path(allow_system);
        if let Some((ffmpeg, ffprobe)) = &explicit {
            resolver = resolver.explicit(ffmpeg, ffprobe);
        }
        match resolver.resolve() {
            Ok(pair) => return Ok(pair),
            Err(error) => last_error = Some(error),
        }
    }
    Err(ServiceError::Conflict(format!(
        "FFmpeg and ffprobe are unavailable: {}. Packaged releases include verified sidecars; source builds may explicitly enable system FFmpeg.",
        last_error.map_or_else(
            || "no candidate paths were found".to_owned(),
            |error| error.to_string()
        )
    )))
}

fn media_error(error: impl std::fmt::Display) -> ServiceError {
    ServiceError::Internal(error.to_string())
}

async fn run_process(
    executable: &Path,
    arguments: &[String],
    purpose: &str,
) -> Result<(), ServiceError> {
    run_process_capture(executable, arguments, purpose)
        .await
        .map(|_| ())
}

async fn run_process_capture(
    executable: &Path,
    arguments: &[String],
    purpose: &str,
) -> Result<String, ServiceError> {
    let output = Command::new(executable)
        .args(arguments)
        .kill_on_drop(true)
        .output()
        .await?;
    let stderr = String::from_utf8_lossy(&output.stderr).into_owned();
    if output.status.success() {
        Ok(stderr)
    } else {
        let mut redacted = stderr.replace(['\r', '\n'], " ");
        redacted.truncate(1_024);
        Err(ServiceError::Internal(format!(
            "media step '{purpose}' failed{}",
            if redacted.is_empty() {
                String::new()
            } else {
                format!(": {redacted}")
            }
        )))
    }
}

async fn ffmpeg_build_description(sidecars: &SidecarPair) -> Result<String, ServiceError> {
    let output = Command::new(&sidecars.ffmpeg)
        .args(["-hide_banner", "-version"])
        .kill_on_drop(true)
        .output()
        .await?;
    if !output.status.success() {
        return Err(ServiceError::Internal(
            "could not identify the FFmpeg sidecar".to_owned(),
        ));
    }
    Ok(String::from_utf8_lossy(&output.stdout)
        .lines()
        .take(2)
        .collect::<Vec<_>>()
        .join("\n"))
}

async fn probe_duration_ms(sidecars: &SidecarPair, path: &Path) -> Result<u64, ServiceError> {
    let output = Command::new(&sidecars.ffprobe)
        .args([
            "-v",
            "error",
            "-show_entries",
            "format=duration",
            "-of",
            "default=noprint_wrappers=1:nokey=1",
        ])
        .arg(path)
        .kill_on_drop(true)
        .output()
        .await?;
    if !output.status.success() {
        return Err(ServiceError::Internal(format!(
            "ffprobe could not validate {}",
            path.display()
        )));
    }
    let seconds = String::from_utf8_lossy(&output.stdout)
        .trim()
        .parse::<f64>()
        .map_err(|_| ServiceError::Internal("ffprobe returned an invalid duration".to_owned()))?;
    if !seconds.is_finite() || seconds <= 0.0 {
        return Err(ServiceError::Internal(
            "audio duration is not positive".to_owned(),
        ));
    }
    let duration = Duration::try_from_secs_f64(seconds)
        .map_err(|_| ServiceError::Internal("audio duration is out of range".to_owned()))?;
    Ok(u64::try_from(duration.as_millis())
        .unwrap_or(u64::MAX)
        .max(1))
}

async fn validate_flac(path: &Path) -> Result<(), ServiceError> {
    let mut file = tokio::fs::File::open(path).await?;
    let mut signature = [0_u8; 4];
    file.read_exact(&mut signature).await?;
    if signature != *b"fLaC" {
        return Err(ServiceError::Internal(
            "chapter assembly did not produce FLAC".to_owned(),
        ));
    }
    Ok(())
}

async fn copy_file_atomically(source: &Path, destination: &Path) -> Result<(), ServiceError> {
    let parent = destination.parent().ok_or_else(|| {
        ServiceError::InvalidRequest("destination has no parent directory".to_owned())
    })?;
    tokio::fs::create_dir_all(parent).await?;
    let temporary = tempfile::Builder::new()
        .prefix(".audiobookai-copy-")
        .tempfile_in(parent)
        .map_err(ServiceError::Io)?
        .into_temp_path();
    tokio::fs::copy(source, &temporary).await?;
    sync_file(&temporary).await?;
    atomic_promote(&temporary, destination).await
}

async fn write_file_atomically(destination: &Path, bytes: &[u8]) -> Result<(), ServiceError> {
    let parent = destination.parent().ok_or_else(|| {
        ServiceError::InvalidRequest("destination has no parent directory".to_owned())
    })?;
    tokio::fs::create_dir_all(parent).await?;
    let temporary = tempfile::Builder::new()
        .prefix(".audiobookai-write-")
        .tempfile_in(parent)
        .map_err(ServiceError::Io)?
        .into_temp_path();
    tokio::fs::write(&temporary, bytes).await?;
    sync_file(&temporary).await?;
    atomic_promote(&temporary, destination).await
}

async fn write_job_staging_file_atomically(
    private_root: &Path,
    destination: &Path,
    bytes: &[u8],
) -> Result<(), ServiceError> {
    ensure_private_staging_file_path(private_root, destination).await?;
    let parent = destination.parent().ok_or_else(|| {
        ServiceError::InvalidRequest("staging destination has no parent directory".to_owned())
    })?;
    let temporary = tempfile::Builder::new()
        .prefix(".audiobookai-staging-write-")
        .tempfile_in(parent)
        .map_err(ServiceError::Io)?
        .into_temp_path();
    tokio::fs::write(&temporary, bytes).await?;
    sync_file(&temporary).await?;
    ensure_private_staging_file_path(private_root, destination).await?;
    match tokio::fs::symlink_metadata(destination).await {
        Ok(metadata) if metadata.file_type().is_file() && !metadata.file_type().is_symlink() => {
            tokio::fs::remove_file(destination).await?;
        }
        Ok(_) => {
            return Err(ServiceError::Conflict(format!(
                "job staging destination is not a regular file: {}",
                destination.display()
            )));
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(ServiceError::Io(error)),
    }
    atomic_promote(&temporary, destination).await
}

async fn prepare_private_export_staging(
    state: &AppState,
    job_id: JobId,
) -> Result<PathBuf, ServiceError> {
    let managed_root = tokio::fs::canonicalize(&state.config.data_dir)
        .await
        .map_err(|error| {
            ServiceError::Conflict(format!(
                "managed application data directory is unavailable ({}): {error}",
                state.config.data_dir.display()
            ))
        })?;
    let jobs = managed_root.join("jobs");
    ensure_private_directory(&jobs).await?;
    let job = jobs.join(job_id.to_string());
    ensure_private_directory(&job).await?;
    let staging = job.join("export-staging");
    ensure_private_directory(&staging).await?;
    Ok(staging)
}

async fn ensure_private_directory(path: &Path) -> Result<(), ServiceError> {
    match tokio::fs::symlink_metadata(path).await {
        Ok(metadata) => {
            if metadata.file_type().is_symlink() || !metadata.is_dir() {
                return Err(ServiceError::Conflict(format!(
                    "managed export staging component is not a private directory: {}",
                    path.display()
                )));
            }
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            match tokio::fs::create_dir(path).await {
                Ok(()) => {}
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
                    let metadata = tokio::fs::symlink_metadata(path).await?;
                    if metadata.file_type().is_symlink() || !metadata.is_dir() {
                        return Err(ServiceError::Conflict(format!(
                            "managed export staging component was replaced during creation: {}",
                            path.display()
                        )));
                    }
                }
                Err(error) => return Err(ServiceError::Io(error)),
            }
        }
        Err(error) => return Err(ServiceError::Io(error)),
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        tokio::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700)).await?;
    }
    Ok(())
}

async fn ensure_existing_real_directory(
    path: &Path,
    description: &str,
) -> Result<(), ServiceError> {
    let metadata = tokio::fs::symlink_metadata(path).await.map_err(|error| {
        ServiceError::Conflict(format!(
            "{description} is unavailable ({}): {error}",
            path.display()
        ))
    })?;
    if metadata.file_type().is_symlink() || !metadata.is_dir() {
        return Err(ServiceError::Conflict(format!(
            "{description} is not a real directory: {}",
            path.display()
        )));
    }
    Ok(())
}

async fn ensure_private_staging_file_path(
    private_root: &Path,
    path: &Path,
) -> Result<(), ServiceError> {
    ensure_existing_real_directory(private_root, "managed export staging root").await?;
    if path == private_root || !path.starts_with(private_root) {
        return Err(ServiceError::Conflict(format!(
            "export staging file escapes its job-private directory: {}",
            path.display()
        )));
    }
    let parent = path.parent().ok_or_else(|| {
        ServiceError::Conflict("export staging file has no parent directory".to_owned())
    })?;
    ensure_existing_real_directory(parent, "managed export staging parent").await?;
    let canonical_root = tokio::fs::canonicalize(private_root).await?;
    let canonical_parent = tokio::fs::canonicalize(parent).await?;
    if !canonical_parent.starts_with(&canonical_root) {
        return Err(ServiceError::Conflict(format!(
            "export staging parent resolves outside its job-private directory: {}",
            path.display()
        )));
    }
    match tokio::fs::symlink_metadata(path).await {
        Ok(metadata) if metadata.is_file() && !metadata.file_type().is_symlink() => Ok(()),
        Ok(_) => Err(ServiceError::Conflict(format!(
            "export staging destination is not a regular private file: {}",
            path.display()
        ))),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(ServiceError::Io(error)),
    }
}

async fn sync_file(path: &Path) -> Result<(), ServiceError> {
    let mut options = tokio::fs::OpenOptions::new();
    #[cfg(windows)]
    options.write(true);
    #[cfg(not(windows))]
    options.read(true);
    options.open(path).await?.sync_all().await?;
    Ok(())
}

fn no_clobber_conflict(destination: &Path) -> ServiceError {
    ServiceError::Conflict(format!("refusing to overwrite {}", destination.display()))
}

async fn failed_no_clobber_operation_is_conflict(
    error: &std::io::Error,
    destination: &Path,
) -> bool {
    error.kind() == std::io::ErrorKind::AlreadyExists
        || tokio::fs::symlink_metadata(destination).await.is_ok()
}

async fn atomic_promote(source: &Path, destination: &Path) -> Result<(), ServiceError> {
    sync_file(source).await?;
    match tokio::fs::hard_link(source, destination).await {
        Ok(()) => {}
        Err(error) => {
            if failed_no_clobber_operation_is_conflict(&error, destination).await {
                return Err(no_clobber_conflict(destination));
            }
            tracing::debug!(
                diagnostic_code = "storage.promotion.hard_link.unavailable",
                %error,
                source = %source.display(),
                destination = %destination.display(),
                "falling back to exclusive-copy promotion"
            );
            copy_file_no_clobber(source, destination).await?;
        }
    }
    // The destination link already names the complete source inode. Source cleanup is
    // best-effort so a staging unlink failure cannot turn a successful no-clobber promotion into
    // an ambiguous retry that sees an existing final path.
    let _ = tokio::fs::remove_file(source).await;
    #[cfg(unix)]
    if let Some(parent) = destination.parent() {
        tokio::fs::File::open(parent).await?.sync_all().await?;
    }
    Ok(())
}

async fn copy_file_no_clobber(source: &Path, destination: &Path) -> Result<(), ServiceError> {
    let mut input = tokio::fs::File::open(source).await?;
    copy_reader_no_clobber(&mut input, destination).await
}

async fn copy_reader_no_clobber<R>(input: &mut R, destination: &Path) -> Result<(), ServiceError>
where
    R: tokio::io::AsyncRead + Unpin,
{
    let mut output = match tokio::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(destination)
        .await
    {
        Ok(output) => output,
        Err(error) => {
            if failed_no_clobber_operation_is_conflict(&error, destination).await {
                return Err(no_clobber_conflict(destination));
            }
            return Err(ServiceError::Io(error));
        }
    };
    if let Err(error) = tokio::io::copy(input, &mut output).await {
        drop(output);
        // The destination was created exclusively, but pathname ownership can change after this
        // handle is opened. Removing by path here could therefore delete a foreign replacement.
        // Retain the partial file fail-closed instead; public export callers already hold their
        // durable promotion reservation before this fallback can publish a destination name.
        return Err(ServiceError::Io(error));
    }
    if let Err(error) = output.sync_all().await {
        drop(output);
        // As above, never perform path-based cleanup after publishing the destination name.
        return Err(ServiceError::Io(error));
    }
    Ok(())
}

async fn create_directory_no_clobber(destination: &Path) -> Result<(), ServiceError> {
    match tokio::fs::create_dir(destination).await {
        Ok(()) => {
            #[cfg(unix)]
            if let Some(parent) = destination.parent() {
                tokio::fs::File::open(parent).await?.sync_all().await?;
            }
            Ok(())
        }
        Err(error) => {
            if failed_no_clobber_operation_is_conflict(&error, destination).await {
                Err(no_clobber_conflict(destination))
            } else {
                Err(ServiceError::Io(error))
            }
        }
    }
}

async fn artifact_for_file(
    kind: ArtifactKind,
    path: &Path,
    media_type: Option<String>,
    duration_ms: Option<u64>,
    cache_key: Option<String>,
    pinned_by_job_id: Option<JobId>,
) -> Result<Artifact, ServiceError> {
    artifact_for_file_with_id(
        ArtifactId::new(),
        kind,
        path,
        media_type,
        duration_ms,
        cache_key,
        pinned_by_job_id,
    )
    .await
}

#[allow(clippy::too_many_arguments)]
async fn artifact_for_file_with_id(
    id: ArtifactId,
    kind: ArtifactKind,
    path: &Path,
    media_type: Option<String>,
    duration_ms: Option<u64>,
    cache_key: Option<String>,
    pinned_by_job_id: Option<JobId>,
) -> Result<Artifact, ServiceError> {
    let path = tokio::fs::canonicalize(path).await?;
    let fingerprint = fingerprint_file(&path).await?;
    let now = Utc::now();
    Ok(Artifact {
        id,
        kind,
        path: path.to_string_lossy().into_owned(),
        fingerprint,
        media_type,
        duration_ms,
        cache_key,
        pinned_by_job_id,
        created_at: now,
        last_accessed_at: now,
    })
}

async fn fingerprint_file(path: &Path) -> Result<FileFingerprint, ServiceError> {
    let mut file = tokio::fs::File::open(path).await?;
    let mut hasher = blake3::Hasher::new();
    let mut size = 0_u64;
    let mut buffer = vec![0_u8; RANGE_CHUNK_BYTES];
    loop {
        let read = file.read(&mut buffer).await?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
        size = size.saturating_add(u64::try_from(read).unwrap_or(u64::MAX));
    }
    Ok(FileFingerprint {
        algorithm: "blake3".to_owned(),
        digest: hasher.finalize().to_hex().to_string(),
        size_bytes: size,
    })
}

async fn verify_selected_artifact_integrity(artifact: &Artifact) -> Result<(), ServiceError> {
    if artifact.fingerprint.algorithm != "blake3"
        || artifact.fingerprint.digest.len() != 64
        || !artifact
            .fingerprint
            .digest
            .bytes()
            .all(|byte| byte.is_ascii_hexdigit())
    {
        return Err(ServiceError::Conflict(format!(
            "selected artifact {} has no known BLAKE3 fingerprint",
            artifact.id
        )));
    }
    let path = Path::new(&artifact.path);
    let metadata = tokio::fs::symlink_metadata(path).await.map_err(|error| {
        ServiceError::Conflict(format!(
            "selected artifact {} is unavailable: {error}",
            artifact.id
        ))
    })?;
    if metadata.file_type().is_symlink() || !metadata.is_file() {
        return Err(ServiceError::Conflict(format!(
            "selected artifact {} is not a regular managed file",
            artifact.id
        )));
    }
    if fingerprint_file(path).await? != artifact.fingerprint {
        return Err(ServiceError::Conflict(format!(
            "selected artifact {} no longer matches its stored BLAKE3 fingerprint",
            artifact.id
        )));
    }
    Ok(())
}

async fn verify_selected_artifacts_before_use(artifacts: &[&Artifact]) -> Result<(), ServiceError> {
    for artifact in artifacts {
        verify_selected_artifact_integrity(artifact).await?;
    }
    Ok(())
}

async fn persist_artifact(
    state: &AppState,
    project_id: Uuid,
    artifact: &Artifact,
) -> Result<(), ServiceError> {
    let result = sqlx::query(
        "INSERT INTO artifacts \
         (id, project_id, kind, path, cache_key, pinned_by_job_id, created_at, last_accessed_at, payload) \
         VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?)",
    )
    .bind(artifact.id.to_string())
    .bind(project_id.to_string())
    .bind(artifact_kind_name(artifact.kind))
    .bind(&artifact.path)
    .bind(&artifact.cache_key)
    .bind(artifact.pinned_by_job_id.map(|id| id.to_string()))
    .bind(artifact.created_at.to_rfc3339())
    .bind(artifact.last_accessed_at.to_rfc3339())
    .bind(serde_json::to_string(artifact).map_err(internal_error)?)
    .execute(state.database.pool())
    .await;
    match result {
        Ok(_) => Ok(()),
        Err(error)
            if error
                .to_string()
                .contains("UNIQUE constraint failed: artifacts.cache_key") =>
        {
            Ok(())
        }
        Err(error) => Err(storage_error(error)),
    }
}

async fn load_artifact(state: &AppState, id: ArtifactId) -> Result<Artifact, ServiceError> {
    let payload = sqlx::query_scalar::<_, String>("SELECT payload FROM artifacts WHERE id = ?")
        .bind(id.to_string())
        .fetch_optional(state.database.pool())
        .await
        .map_err(storage_error)?
        .ok_or(ServiceError::NotFound)?;
    let artifact: Artifact = serde_json::from_str(&payload).map_err(internal_error)?;
    if !Path::new(&artifact.path).is_file() {
        return Err(ServiceError::Conflict(format!(
            "artifact file is missing: {}",
            artifact.path
        )));
    }
    Ok(artifact)
}

async fn artifact_path(state: &AppState, id: ArtifactId) -> Result<PathBuf, ServiceError> {
    load_artifact(state, id)
        .await
        .map(|artifact| PathBuf::from(artifact.path))
}

const fn artifact_kind_name(kind: ArtifactKind) -> &'static str {
    match kind {
        ArtifactKind::ImportedEpub => "imported_epub",
        ArtifactKind::Cover => "cover",
        ArtifactKind::ReferenceAudio => "reference_audio",
        ArtifactKind::Preview => "preview",
        ArtifactKind::SegmentAudio => "segment_audio",
        ArtifactKind::ChapterMaster => "chapter_master",
        ArtifactKind::MixedMaster => "mixed_master",
        ArtifactKind::Export => "export",
        ArtifactKind::ExportManifest => "export_manifest",
    }
}

fn media_type_for_path(path: &Path) -> String {
    let value = match path
        .extension()
        .and_then(std::ffi::OsStr::to_str)
        .unwrap_or_default()
        .to_ascii_lowercase()
        .as_str()
    {
        "mp3" => "audio/mpeg",
        "wav" => "audio/wav",
        "m4a" | "m4b" => "audio/mp4",
        "flac" => "audio/flac",
        "json" => "application/json",
        _ => "application/octet-stream",
    };
    value.to_owned()
}

fn export_destination(profile: &ExportProfile) -> PathBuf {
    let root = PathBuf::from(&profile.output_directory);
    if profile.layout == ExportLayout::PerChapter {
        root.join(&profile.filename_template)
    } else {
        root.join(format!(
            "{}.{}",
            profile.filename_template,
            media_export_format(profile.format).extension()
        ))
    }
}

fn export_manifest_path(layout: ExportLayout, final_output: &Path) -> PathBuf {
    if layout == ExportLayout::PerChapter {
        final_output.join("audiobookai-export-manifest.json")
    } else {
        final_output.with_file_name(format!(
            "{}.manifest.json",
            final_output
                .file_name()
                .and_then(std::ffi::OsStr::to_str)
                .unwrap_or("audiobook")
        ))
    }
}

fn output_destination_key(destination: &Path) -> String {
    // Export folders are canonicalized before the profile is stored. Case-folding the complete
    // child path and Unicode-normalizing it is deliberately conservative: it prevents a database
    // moved between filesystems, or running on normalization-insensitive APFS, from admitting two
    // paid jobs whose destinations alias there.
    audiobookai_storage::normalize_output_destination_key(&destination.to_string_lossy())
}

async fn ensure_export_root_identity(profile: &ExportProfile) -> Result<(), ServiceError> {
    let expected = Path::new(&profile.output_directory);
    let metadata = tokio::fs::symlink_metadata(expected)
        .await
        .map_err(|error| {
            ServiceError::Conflict(format!(
                "reserved export directory is unavailable ({}): {error}",
                expected.display()
            ))
        })?;
    if metadata.file_type().is_symlink() || !metadata.is_dir() {
        return Err(ServiceError::Conflict(format!(
            "reserved export directory was replaced by a non-directory or symlink: {}",
            expected.display()
        )));
    }
    let actual = tokio::fs::canonicalize(expected).await?;
    if actual != expected {
        return Err(ServiceError::Conflict(format!(
            "reserved export directory now resolves to a different location: {} -> {}",
            expected.display(),
            actual.display()
        )));
    }
    Ok(())
}

async fn prospective_canonical_path(path: &Path) -> Result<PathBuf, ServiceError> {
    if !path.is_absolute() {
        return Err(ServiceError::InvalidRequest(
            "the export path must be absolute".to_owned(),
        ));
    }
    let mut cursor = path.to_path_buf();
    let mut suffix = Vec::new();
    if let Some(component) = cursor.file_name() {
        suffix.push(component.to_os_string());
        cursor = cursor.parent().unwrap_or(Path::new("/")).to_path_buf();
    }
    let mut base = loop {
        match tokio::fs::canonicalize(&cursor).await {
            Ok(base) => break base,
            Err(error)
                if matches!(
                    error.kind(),
                    std::io::ErrorKind::NotFound | std::io::ErrorKind::NotADirectory
                ) =>
            {
                let component = cursor.file_name().ok_or_else(|| {
                    ServiceError::Conflict(format!(
                        "cannot resolve the export path {}",
                        path.display()
                    ))
                })?;
                suffix.push(component.to_os_string());
                cursor = cursor
                    .parent()
                    .ok_or_else(|| {
                        ServiceError::Conflict(format!(
                            "cannot resolve the export path {}",
                            path.display()
                        ))
                    })?
                    .to_path_buf();
            }
            Err(error) => return Err(ServiceError::Io(error)),
        }
    };
    for component in suffix.into_iter().rev() {
        base.push(component);
    }
    Ok(lexically_normalized_path(&base))
}

fn lexically_normalized_path(path: &Path) -> PathBuf {
    let mut prefix = None;
    let mut rooted = false;
    let mut components = Vec::new();
    for component in path.components() {
        match component {
            std::path::Component::Prefix(value) => {
                prefix = Some(value.as_os_str().to_os_string());
            }
            std::path::Component::RootDir => {
                rooted = true;
                components.clear();
            }
            std::path::Component::CurDir => {}
            std::path::Component::ParentDir => {
                components.pop();
            }
            std::path::Component::Normal(value) => components.push(value.to_os_string()),
        }
    }
    let mut normalized = PathBuf::new();
    if let Some(prefix) = prefix {
        normalized.push(prefix);
    }
    if rooted {
        normalized.push(std::path::MAIN_SEPARATOR_STR);
    }
    for component in components {
        normalized.push(component);
    }
    normalized
}

async fn ensure_output_directory_not_reserved(
    state: &AppState,
    output_directory: &Path,
) -> Result<(), ServiceError> {
    let normalized = prospective_canonical_path(output_directory).await?;
    let key = output_destination_key(&normalized);
    if let Some(existing) = state
        .database
        .repositories()
        .jobs
        .find_output_reservation_containing_path(&key)
        .await
        .map_err(storage_error)?
    {
        return Err(ServiceError::ConflictDetails {
            code: "output_directory_reserved",
            detail: format!(
                "the export directory is inside another job's reserved destination: {}",
                existing.destination_path
            ),
            meta: serde_json::json!({
                "destination": existing.destination_path,
                "ownerJobId": existing.job_id,
                "ownerProjectId": existing.project_id,
            }),
        });
    }
    Ok(())
}

async fn prepare_output_reservation(
    job_id: JobId,
    project_id: ProjectId,
    profile: &ExportProfile,
    now: chrono::DateTime<Utc>,
) -> Result<OutputDestinationReservation, ServiceError> {
    let root = PathBuf::from(&profile.output_directory);
    ensure_export_root_identity(profile).await?;
    let destination = export_destination(profile);
    if destination.exists() {
        return Err(ServiceError::Conflict(format!(
            "export destination already exists: {}",
            destination.display()
        )));
    }
    let manifest = export_manifest_path(profile.layout, &destination);
    if manifest.exists() {
        return Err(ServiceError::Conflict(format!(
            "export manifest destination already exists: {}",
            manifest.display()
        )));
    }
    let probe = tempfile::Builder::new()
        .prefix(".audiobookai-write-test-")
        .tempfile_in(&root)
        .map_err(|error| {
            ServiceError::Conflict(format!(
                "export directory is not writable ({}): {error}",
                root.display()
            ))
        })?;
    drop(probe);
    ensure_export_root_identity(profile).await?;
    Ok(OutputDestinationReservation {
        job_id,
        project_id,
        destination_key: output_destination_key(&destination),
        destination_path: destination.to_string_lossy().into_owned(),
        layout: profile.layout,
        state: OutputReservationState::Reserved,
        created_at: now,
        updated_at: now,
        promoted_at: None,
    })
}

fn output_reservation_admission_error(error: StorageError) -> ServiceError {
    match error {
        StorageError::Conflict {
            entity: "output destination",
            id,
        } => ServiceError::ConflictDetails {
            code: "output_destination_reserved",
            detail: format!("another job already owns the export destination: {id}"),
            meta: serde_json::json!({"destination": id}),
        },
        other => storage_error(other),
    }
}

async fn require_output_reservation(
    state: &AppState,
    job_id: JobId,
    project_id: ProjectId,
    profile: &ExportProfile,
    destination: &Path,
) -> Result<OutputDestinationReservation, ServiceError> {
    let reservation = state
        .database
        .repositories()
        .jobs
        .get_output_reservation(job_id)
        .await
        .map_err(storage_error)?
        .ok_or_else(|| {
            ServiceError::Conflict(
                "export job has no durable output destination reservation".to_owned(),
            )
        })?;
    if reservation.project_id != project_id
        || reservation.destination_key != output_destination_key(destination)
        || Path::new(&reservation.destination_path) != destination
        || reservation.layout != profile.layout
    {
        return Err(ServiceError::Conflict(
            "export job does not own its planned output destination".to_owned(),
        ));
    }
    ensure_export_root_identity(profile).await?;
    if reservation.state == OutputReservationState::Promoted && !destination.exists() {
        return Err(ServiceError::Conflict(
            "the job-owned promoted export destination is missing".to_owned(),
        ));
    }
    Ok(reservation)
}

async fn ensure_existing_job_output_reservation(
    state: &AppState,
    job: &Job,
) -> Result<(), ServiceError> {
    let profile = load_export_profile(
        state,
        job.export_profile_id.ok_or_else(|| {
            ServiceError::Conflict("export job has no durable export profile".to_owned())
        })?,
    )
    .await?;
    let destination = export_destination(&profile);
    let repository = state.database.repositories().jobs;
    if repository
        .get_output_reservation(job.id)
        .await
        .map_err(storage_error)?
        .is_some()
    {
        require_output_reservation(state, job.id, job.project_id, &profile, &destination).await?;
        return Ok(());
    }
    ensure_output_directory_not_reserved(state, Path::new(&profile.output_directory)).await?;
    let reservation =
        prepare_output_reservation(job.id, job.project_id, &profile, Utc::now()).await?;
    repository
        .acquire_output_reservation_for_existing_job(job, &reservation)
        .await
        .map_err(output_reservation_admission_error)
}

async fn transition_job(
    state: &AppState,
    job_id: JobId,
    next: JobState,
    message: &str,
) -> Result<Job, ServiceError> {
    let repository = state.database.repositories().jobs;
    let mut job = repository
        .get(job_id)
        .await
        .map_err(storage_error)?
        .ok_or(ServiceError::NotFound)?;
    if job.state == next {
        return Ok(job);
    }
    let expected = job.revision;
    job.transition(next, Utc::now())
        .map_err(|error| ServiceError::Conflict(error.to_string()))?;
    job.status_message = Some(message.to_owned());
    let job = if matches!(job.state, JobState::Failed | JobState::Cancelled) {
        repository
            .update_terminal_with_output_release(&job, expected)
            .await
    } else {
        repository.update(&job, expected).await
    }
    .map_err(storage_error)?;
    if job.state == JobState::Completed {
        release_completed_output_reservation(state, job_id).await;
    }
    if let Some(view) = state.catalog.write().await.jobs.get_mut(&job_id.as_uuid()) {
        view.status = job_status_view(job.state);
        view.current_stage.clone_from(&job.status_message);
        view.started_at = job.started_at;
        view.updated_at = job.updated_at;
    }
    state.events.publish(
        "job.updated",
        serde_json::json!({"jobId": job_id, "status": job.state, "message": message}),
    );
    Ok(job)
}

/// Retry/resume must not make an export runnable before it owns the exact destination again.
async fn transition_export_job_with_reservation(
    state: &AppState,
    job_id: JobId,
    next: JobState,
    message: &str,
) -> Result<Job, ServiceError> {
    let _output_admission = OUTPUT_ADMISSION_LOCK
        .get_or_init(|| tokio::sync::Mutex::new(()))
        .lock()
        .await;
    let repository = state.database.repositories().jobs;
    let mut job = repository
        .get(job_id)
        .await
        .map_err(storage_error)?
        .ok_or(ServiceError::NotFound)?;
    if !matches!(job.kind, JobKind::Conversion | JobKind::Export) {
        return transition_job(state, job_id, next, message).await;
    }
    let profile = load_export_profile(
        state,
        job.export_profile_id.ok_or_else(|| {
            ServiceError::Conflict("export job has no durable export profile".to_owned())
        })?,
    )
    .await?;
    let destination = export_destination(&profile);
    if repository
        .get_output_reservation(job_id)
        .await
        .map_err(storage_error)?
        .is_some()
    {
        require_output_reservation(state, job_id, job.project_id, &profile, &destination).await?;
        return transition_job(state, job_id, next, message).await;
    }

    let now = Utc::now();
    ensure_output_directory_not_reserved(state, Path::new(&profile.output_directory)).await?;
    let reservation = prepare_output_reservation(job_id, job.project_id, &profile, now).await?;
    let expected = job.revision;
    job.transition(next, now)
        .map_err(|error| ServiceError::Conflict(error.to_string()))?;
    job.status_message = Some(message.to_owned());
    let job = repository
        .update_with_output_reservation(&job, expected, &reservation)
        .await
        .map_err(output_reservation_admission_error)?;
    if let Some(view) = state.catalog.write().await.jobs.get_mut(&job_id.as_uuid()) {
        view.status = job_status_view(job.state);
        view.current_stage.clone_from(&job.status_message);
        view.started_at = job.started_at;
        view.updated_at = job.updated_at;
    }
    state.events.publish(
        "job.updated",
        serde_json::json!({"jobId": job_id, "status": job.state, "message": message}),
    );
    Ok(job)
}

#[allow(clippy::too_many_lines)]
async fn retry_billable_estimates(
    state: &AppState,
    job: &Job,
) -> Result<Vec<crate::accounting::RatedUsageEstimate>, ServiceError> {
    let persisted_units = state
        .database
        .repositories()
        .jobs
        .list_units(job.id)
        .await
        .map_err(storage_error)?;
    if persisted_units.iter().any(|unit| {
        unit.payload
            .get("uncertainUsageUnresolved")
            .and_then(serde_json::Value::as_bool)
            .unwrap_or(false)
    }) {
        return Err(ServiceError::ConflictDetails {
            code: "retry_usage_unresolved",
            detail: "this job has unresolved provider usage and cannot be retried safely"
                .to_owned(),
            meta: serde_json::json!({"jobId": job.id}),
        });
    }
    if job.kind == JobKind::CharacterDetection {
        return crate::workflows::prepare_detection_retry_units(state, job.id).await;
    }
    if matches!(
        job.kind,
        JobKind::Preview | JobKind::QualityControl | JobKind::CacheCleanup
    ) {
        return Err(ServiceError::Conflict(
            "this job kind does not support manual retry".to_owned(),
        ));
    }

    let retryable_synthesis = persisted_units
        .iter()
        .filter(|unit| {
            unit.kind == JobUnitKind::SynthesisSegment
                && !matches!(
                    unit.state,
                    JobUnitState::Completed | JobUnitState::Cancelled
                )
        })
        .map(|unit| unit.id)
        .collect::<HashSet<_>>();
    if retryable_synthesis.is_empty() {
        return Ok(Vec::new());
    }

    match job.kind {
        JobKind::Conversion => {
            let export = load_export_profile(
                state,
                job.export_profile_id.ok_or_else(|| {
                    ServiceError::Conflict("conversion job has no export profile".to_owned())
                })?,
            )
            .await?;
            let music_path = if let Some(music) = &export.background_music {
                Some(artifact_path(state, music.artifact_id).await?)
            } else {
                None
            };
            let plan =
                load_conversion_plan(state, job.project_id.as_uuid(), export, music_path).await?;
            let units = load_unit_plan(state, job.id, &plan).await?;
            let multiplier = if plan
                .project
                .settings
                .reliability
                .retry_possible_duplicate_charge
            {
                usize::from(
                    plan.project
                        .settings
                        .reliability
                        .max_transient_retries
                        .saturating_add(1),
                )
            } else {
                1
            };
            let mut estimates = Vec::new();
            let mut matched_units = 0_usize;
            for segment in plan.chapters.iter().flat_map(|chapter| &chapter.segments) {
                let unit = units.synthesis.get(&segment.key).ok_or_else(|| {
                    ServiceError::Conflict(
                        "conversion retry graph no longer matches its narration plan".to_owned(),
                    )
                })?;
                if !retryable_synthesis.contains(&unit.id) {
                    continue;
                }
                matched_units = matched_units.saturating_add(1);
                let estimate = crate::accounting::rate_usage_estimate(
                    state,
                    ProviderProfileId::from_uuid(segment.assignment.provider_id),
                    UsageWorkload::Tts,
                    segment.assignment.model.clone(),
                    UsageQuantities {
                        characters: u64::try_from(segment.text.chars().count()).ok(),
                        ..UsageQuantities::default()
                    },
                )
                .await?;
                for _ in 0..multiplier {
                    estimates.push(estimate.clone());
                }
            }
            if matched_units != retryable_synthesis.len() {
                return Err(ServiceError::Conflict(
                    "conversion retry contains unmatched billable synthesis units".to_owned(),
                ));
            }
            Ok(estimates)
        }
        JobKind::SegmentRegeneration => {
            let mut estimates = Vec::with_capacity(retryable_synthesis.len());
            for unit in persisted_units
                .iter()
                .filter(|unit| retryable_synthesis.contains(&unit.id))
            {
                let segment = unit
                    .payload
                    .get("segmentPlan")
                    .cloned()
                    .ok_or_else(|| {
                        ServiceError::Conflict(
                            "regeneration retry is missing its durable segment plan".to_owned(),
                        )
                    })
                    .and_then(|value| {
                        serde_json::from_value::<SegmentPlan>(value).map_err(internal_error)
                    })?;
                validate_regeneration_retry_provider_snapshot(state, job, &segment).await?;
                let policy = retry_policy(state, &segment).await?;
                let multiplier = retry_reservation_multiplier(&policy);
                let estimate = crate::accounting::rate_usage_estimate(
                    state,
                    ProviderProfileId::from_uuid(segment.assignment.provider_id),
                    UsageWorkload::Tts,
                    segment.assignment.model.clone(),
                    UsageQuantities {
                        characters: u64::try_from(segment.text.chars().count()).ok(),
                        ..UsageQuantities::default()
                    },
                )
                .await?;
                for _ in 0..multiplier {
                    estimates.push(estimate.clone());
                }
            }
            Ok(estimates)
        }
        JobKind::Export => Err(ServiceError::Conflict(
            "a provider-free proof export cannot retry synthesis units".to_owned(),
        )),
        JobKind::CharacterDetection => unreachable!("handled before synthesis planning"),
        JobKind::Preview | JobKind::QualityControl | JobKind::CacheCleanup => Err(
            ServiceError::Conflict("this job kind does not support manual retry".to_owned()),
        ),
    }
}

async fn reset_non_detection_retry_units(
    state: &AppState,
    job_id: JobId,
) -> Result<(), ServiceError> {
    for mut unit in state
        .database
        .repositories()
        .jobs
        .list_units(job_id)
        .await
        .map_err(storage_error)?
    {
        if unit.state == JobUnitState::Failed {
            let next = if unit.dependencies.is_empty() {
                JobUnitState::Ready
            } else {
                JobUnitState::Blocked
            };
            update_unit_state(state, &mut unit, next, None).await?;
        }
    }
    Ok(())
}

async fn prepare_retry_output_claim(
    state: &AppState,
    job: &Job,
) -> Result<Option<OutputDestinationReservation>, ServiceError> {
    if !matches!(job.kind, JobKind::Conversion | JobKind::Export) {
        return Ok(None);
    }
    let profile = load_export_profile(
        state,
        job.export_profile_id.ok_or_else(|| {
            ServiceError::Conflict("export job has no durable export profile".to_owned())
        })?,
    )
    .await?;
    let destination = export_destination(&profile);
    let repository = state.database.repositories().jobs;
    if repository
        .get_output_reservation(job.id)
        .await
        .map_err(storage_error)?
        .is_some()
    {
        require_output_reservation(state, job.id, job.project_id, &profile, &destination).await?;
        return Ok(None);
    }
    ensure_output_directory_not_reserved(state, Path::new(&profile.output_directory)).await?;
    prepare_output_reservation(job.id, job.project_id, &profile, Utc::now())
        .await
        .map(Some)
}

fn retry_admission_error(error: StorageError) -> ServiceError {
    match error {
        StorageError::Conflict {
            entity: "output destination",
            id,
        } => output_reservation_admission_error(StorageError::Conflict {
            entity: "output destination",
            id,
        }),
        error @ (StorageError::BudgetExceeded { .. }
        | StorageError::Conflict {
            entity: "active budget reservation",
            ..
        }
        | StorageError::Conflict {
            entity: "retry budget predecessor",
            ..
        }) => ServiceError::Conflict(error.to_string()),
        other => storage_error(other),
    }
}

async fn admit_failed_job_retry(state: &AppState, job_id: JobId) -> Result<Job, ServiceError> {
    let repository = state.database.repositories().jobs;
    let job = repository
        .get(job_id)
        .await
        .map_err(storage_error)?
        .ok_or(ServiceError::NotFound)?;
    if job.state != JobState::Failed {
        return Err(ServiceError::Conflict(
            "only a failed job can be admitted for retry".to_owned(),
        ));
    }
    let _output_admission = if matches!(job.kind, JobKind::Conversion | JobKind::Export) {
        Some(
            OUTPUT_ADMISSION_LOCK
                .get_or_init(|| tokio::sync::Mutex::new(()))
                .lock()
                .await,
        )
    } else {
        None
    };

    let estimates = retry_billable_estimates(state, &job).await?;
    if job.kind != JobKind::CharacterDetection {
        reset_non_detection_retry_units(state, job_id).await?;
    }
    let output_reservation = prepare_retry_output_claim(state, &job).await?;

    let _budget_lifecycle = crate::accounting::lock_budget_reservation_lifecycle().await;
    crate::accounting::finalize_job_reservation_locked(state, job_id).await?;
    let mut current = repository
        .get(job_id)
        .await
        .map_err(storage_error)?
        .ok_or(ServiceError::NotFound)?;
    if current.state != JobState::Failed {
        return Err(ServiceError::Conflict(
            "job state changed while retry admission was being prepared".to_owned(),
        ));
    }
    let budget_reservation =
        crate::accounting::prepare_reservation_for_estimates(state, &current, &estimates).await?;
    let expected = current.revision;
    current.reservation_id = budget_reservation.as_ref().map(|value| value.id);
    current
        .transition(JobState::Queued, Utc::now())
        .map_err(|error| ServiceError::Conflict(error.to_string()))?;
    current.finished_at = None;
    current.status_message = Some("Queued for retry".to_owned());
    let current = repository
        .update_with_retry_admission(
            &current,
            expected,
            budget_reservation.as_ref(),
            output_reservation.as_ref(),
        )
        .await
        .map_err(retry_admission_error)?;
    if budget_reservation.is_some() {
        crate::accounting::refresh_budget_views(state).await?;
    }
    if let Some(view) = state.catalog.write().await.jobs.get_mut(&job_id.as_uuid()) {
        view.status = JobStatusView::Queued;
        view.current_stage.clone_from(&current.status_message);
        view.started_at = current.started_at;
        view.updated_at = current.updated_at;
    }
    state.events.publish(
        "job.updated",
        serde_json::json!({"jobId": job_id, "status": current.state, "message": "Queued for retry"}),
    );
    Ok(current)
}

async fn set_job_message(
    state: &AppState,
    job_id: JobId,
    message: &str,
) -> Result<(), ServiceError> {
    let repository = state.database.repositories().jobs;
    let mut job = repository
        .get(job_id)
        .await
        .map_err(storage_error)?
        .ok_or(ServiceError::NotFound)?;
    let expected = job.revision;
    job.status_message = Some(message.to_owned());
    job.updated_at = Utc::now();
    let updated = repository
        .update(&job, expected)
        .await
        .map_err(storage_error)?;
    if let Some(view) = state.catalog.write().await.jobs.get_mut(&job_id.as_uuid()) {
        view.current_stage = Some(message.to_owned());
        view.updated_at = updated.updated_at;
    }
    Ok(())
}

async fn update_unit_state(
    state: &AppState,
    unit: &mut JobUnit,
    next: JobUnitState,
    error: Option<&str>,
) -> Result<(), ServiceError> {
    unit.state = next;
    unit.updated_at = Utc::now();
    unit.payload.insert(
        "progress".to_owned(),
        serde_json::json!(if next == JobUnitState::Completed {
            1.0
        } else {
            0.0
        }),
    );
    if let Some(error) = error {
        unit.payload
            .insert("lastError".to_owned(), serde_json::json!(error));
    } else if next != JobUnitState::Failed {
        unit.payload.remove("lastError");
    }
    state
        .database
        .repositories()
        .jobs
        .upsert_unit(unit)
        .await
        .map_err(storage_error)?;
    if let Some(view) = state
        .catalog
        .write()
        .await
        .jobs
        .get_mut(&unit.job_id.as_uuid())
        .and_then(|job| {
            job.units
                .iter_mut()
                .find(|view| view.id == unit.id.as_uuid())
        })
    {
        *view = unit_view(unit);
    }
    state.events.publish(
        "job.unit.updated",
        serde_json::json!({
            "jobId": unit.job_id,
            "unitId": unit.id,
            "status": unit.state,
        }),
    );
    Ok(())
}

async fn increment_job_progress(
    state: &AppState,
    job_id: JobId,
    guard: &tokio::sync::Mutex<()>,
) -> Result<(), ServiceError> {
    let _guard = guard.lock().await;
    let repository = state.database.repositories().jobs;
    let mut job = repository
        .get(job_id)
        .await
        .map_err(storage_error)?
        .ok_or(ServiceError::NotFound)?;
    let expected = job.revision;
    job.progress_completed = job
        .progress_completed
        .saturating_add(1)
        .min(job.progress_total);
    job.updated_at = Utc::now();
    let job = repository
        .update(&job, expected)
        .await
        .map_err(storage_error)?;
    if let Some(view) = state.catalog.write().await.jobs.get_mut(&job_id.as_uuid()) {
        view.progress = progress_ratio(job.progress_completed, job.progress_total);
        view.updated_at = job.updated_at;
    }
    state.events.publish(
        "job.progress",
        serde_json::json!({
            "jobId": job_id,
            "completed": job.progress_completed,
            "total": job.progress_total,
        }),
    );
    Ok(())
}

async fn wait_until_runnable(state: &AppState, job_id: JobId) -> Result<(), ServiceError> {
    loop {
        let job = state
            .database
            .repositories()
            .jobs
            .get(job_id)
            .await
            .map_err(storage_error)?
            .ok_or(ServiceError::NotFound)?;
        match job.state {
            JobState::Queued => {
                transition_job(state, job_id, JobState::Running, "Resuming conversion").await?;
                return Ok(());
            }
            JobState::Running => return Ok(()),
            JobState::Pausing => {
                transition_job(state, job_id, JobState::Paused, "Paused").await?;
            }
            JobState::Paused => tokio::time::sleep(Duration::from_millis(250)).await,
            JobState::Cancelling => {
                transition_job(state, job_id, JobState::Cancelled, "Cancelled").await?;
                return Err(ServiceError::Conflict("job cancelled".to_owned()));
            }
            JobState::Cancelled => {
                return Err(ServiceError::Conflict("job cancelled".to_owned()));
            }
            JobState::Failed => {
                return Err(ServiceError::Conflict("job has failed".to_owned()));
            }
            JobState::Completed => {
                return Err(ServiceError::Conflict("job is already complete".to_owned()));
            }
        }
    }
}

async fn update_staged_job_failure(state: &AppState, job_id: JobId, message: &str) {
    let repository = state.database.repositories().jobs;
    let Ok(Some(mut job)) = repository.get(job_id).await else {
        return;
    };
    if job.state != JobState::Failed {
        return;
    }
    let expected = job.revision;
    job.status_message = Some(message.to_owned());
    job.updated_at = Utc::now();
    let _ = repository.update(&job, expected).await;
}

async fn release_unattached_reservation(state: &AppState, reservation_id: ReservationId) {
    if let Err(error) = state
        .database
        .repositories()
        .budgets
        .release(reservation_id, Utc::now())
        .await
    {
        tracing::warn!(diagnostic_code = "budget.admission.release.failed", %reservation_id, %error, "could not release an unattached admission reservation");
    }
    let _ = crate::accounting::refresh_budget_views(state).await;
}

async fn release_completed_output_reservation(state: &AppState, job_id: JobId) {
    if let Err(error) = state
        .database
        .repositories()
        .jobs
        .release_completed_output_reservation(job_id)
        .await
    {
        tracing::warn!(
            diagnostic_code = "conversion.output_reservation.completed_release.failed",
            %job_id,
            %error,
            "could not release a completed output reservation"
        );
    }
}

async fn fail_interrupted_paid_job(
    state: &AppState,
    job_id: JobId,
    message: &str,
) -> Result<(), ServiceError> {
    let repository = state.database.repositories().jobs;
    mark_job_units_failed(state, job_id, message).await;
    if let Some(mut job) = repository.get(job_id).await.map_err(storage_error)?
        && !job.state.is_terminal()
    {
        let expected = job.revision;
        let now = Utc::now();
        // Paused -> Failed is intentionally not a public lifecycle transition. Crash recovery is
        // a fail-closed repair, so persist the terminal state directly with complete timestamps.
        job.state = JobState::Failed;
        job.status_message = Some(message.to_owned());
        job.finished_at = Some(now);
        job.updated_at = now;
        repository
            .update_terminal_with_output_release(&job, expected)
            .await
            .map_err(storage_error)?;
    }
    mark_job_failed(state, job_id, message).await;
    let recovered = repository
        .get(job_id)
        .await
        .map_err(storage_error)?
        .ok_or(ServiceError::NotFound)?;
    if !recovered.state.is_terminal() {
        return Err(ServiceError::Conflict(format!(
            "recovered job {job_id} could not be terminalized safely"
        )));
    }
    Ok(())
}

async fn mark_domain_job_failed(state: &AppState, job_id: JobId, message: &str) {
    let repository = state.database.repositories().jobs;
    let Ok(Some(mut job)) = repository.get(job_id).await else {
        return;
    };
    if job.state.is_terminal() {
        return;
    }
    let expected = job.revision;
    if job.transition(JobState::Failed, Utc::now()).is_ok() {
        job.status_message = Some(message.to_owned());
        let _ = repository
            .update_terminal_with_output_release(&job, expected)
            .await;
    }
}

async fn mark_job_failed(state: &AppState, job_id: JobId, message: &str) {
    mark_job_units_failed(state, job_id, message).await;
    // Failed becomes externally visible only after every retryable unit is durable. The same
    // transaction also releases a still-Reserved output claim, closing both retry races.
    mark_domain_job_failed(state, job_id, message).await;
    let uncertain = message.contains("may have been charged") || message.contains("uncertain");
    if let Some(view) = state.catalog.write().await.jobs.get_mut(&job_id.as_uuid()) {
        view.status = JobStatusView::Failed;
        view.current_stage = Some(message.to_owned());
        view.uncertain_charge |= uncertain;
        view.updated_at = Utc::now();
    }
    state.events.publish(
        "job.failed",
        serde_json::json!({"jobId": job_id, "detail": message, "uncertainCharge": uncertain}),
    );
}

async fn mark_job_units_failed(state: &AppState, job_id: JobId, message: &str) {
    let units = state
        .database
        .repositories()
        .jobs
        .list_units(job_id)
        .await
        .unwrap_or_default();
    for mut unit in units {
        if matches!(
            unit.state,
            JobUnitState::Running | JobUnitState::Ready | JobUnitState::Retrying
        ) {
            let _ = update_unit_state(state, &mut unit, JobUnitState::Failed, Some(message)).await;
        }
    }
}

async fn reconcile_job_budgets(state: &AppState, job_id: JobId) -> Result<(), ServiceError> {
    crate::accounting::finalize_job_reservation(state, job_id).await
}

#[derive(Clone, Debug)]
pub enum PlaybackPacket {
    Audio(Bytes),
    Reset,
}

#[derive(Debug, Default)]
struct PlaybackSegmentBuffer {
    chunks: Vec<Bytes>,
    complete: bool,
    published: bool,
}

#[derive(Debug, Default)]
struct PlaybackOrderState {
    next_ordinal: usize,
    segments: BTreeMap<usize, PlaybackSegmentBuffer>,
}

#[derive(Debug)]
struct PlaybackHub {
    sender: broadcast::Sender<PlaybackPacket>,
    order: StdMutex<PlaybackOrderState>,
}

fn playback_hub(job_id: JobId) -> Arc<PlaybackHub> {
    let registry = PLAYBACK_HUBS.get_or_init(|| StdMutex::new(HashMap::new()));
    let mut registry = registry
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    registry
        .entry(job_id.as_uuid())
        .or_insert_with(|| {
            Arc::new(PlaybackHub {
                sender: broadcast::channel(64).0,
                order: StdMutex::new(PlaybackOrderState::default()),
            })
        })
        .clone()
}

pub fn subscribe_playback(job_id: Uuid) -> broadcast::Receiver<PlaybackPacket> {
    playback_hub(JobId::from_uuid(job_id)).sender.subscribe()
}

fn playback_listener_count(job_id: JobId) -> usize {
    playback_hub(job_id).sender.receiver_count()
}

fn prepare_playback(job_id: JobId, next_ordinal: usize) {
    let hub = playback_hub(job_id);
    let mut order = hub
        .order
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    *order = PlaybackOrderState {
        next_ordinal,
        segments: BTreeMap::new(),
    };
}

fn publish_playback_chunk(job_id: JobId, ordinal: usize, pcm: Bytes) {
    if pcm.is_empty() {
        return;
    }
    let hub = playback_hub(job_id);
    if hub.sender.receiver_count() == 0 {
        return;
    }
    let mut order = hub
        .order
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    if ordinal < order.next_ordinal {
        return;
    }
    order.segments.entry(ordinal).or_default().chunks.push(pcm);
    drain_playback_order(&hub, &mut order);
}

fn complete_playback_with_pcm(job_id: JobId, ordinal: usize, pcm: &[u8]) {
    for chunk in pcm.chunks(RANGE_CHUNK_BYTES - (RANGE_CHUNK_BYTES % 4)) {
        publish_playback_chunk(job_id, ordinal, Bytes::copy_from_slice(chunk));
    }
    complete_playback_segment(job_id, ordinal);
}

fn complete_playback_segment(job_id: JobId, ordinal: usize) {
    let hub = playback_hub(job_id);
    let mut order = hub
        .order
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    if ordinal < order.next_ordinal {
        return;
    }
    order.segments.entry(ordinal).or_default().complete = true;
    drain_playback_order(&hub, &mut order);
}

fn reset_playback_segment(job_id: JobId, ordinal: usize) {
    let hub = playback_hub(job_id);
    let mut order = hub
        .order
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    if ordinal < order.next_ordinal {
        return;
    }
    let segment = order.segments.entry(ordinal).or_default();
    segment.chunks.clear();
    segment.complete = false;
    if segment.published {
        segment.published = false;
        let _ = hub.sender.send(PlaybackPacket::Reset);
    }
}

fn drain_playback_order(hub: &PlaybackHub, order: &mut PlaybackOrderState) {
    loop {
        let next = order.next_ordinal;
        let Some(segment) = order.segments.get_mut(&next) else {
            break;
        };
        for chunk in segment.chunks.drain(..) {
            if hub.sender.send(PlaybackPacket::Audio(chunk)).is_ok() {
                segment.published = true;
            }
        }
        if !segment.complete {
            break;
        }
        order.segments.remove(&next);
        order.next_ordinal = order.next_ordinal.saturating_add(1);
    }
}

pub async fn job_is_terminal(state: &AppState, job_id: Uuid) -> bool {
    state
        .database
        .repositories()
        .jobs
        .get(JobId::from_uuid(job_id))
        .await
        .ok()
        .flatten()
        .is_none_or(|job| job.state.is_terminal())
}

/// Performs a short, clearly billable preview with the reviewed narrator assignment.
// Preview is a miniature billable job: consent, reservation, retry, usage,
// cache, artifact, and reconciliation steps intentionally share one flow.
#[allow(clippy::too_many_lines)]
pub async fn preview(
    state: Arc<AppState>,
    project_id: Uuid,
    requested_text: Option<String>,
) -> Result<PreviewView, ServiceError> {
    preview_with_assignment(state, project_id, requested_text, None, None).await
}

pub(crate) async fn audition(
    state: Arc<AppState>,
    project_id: Uuid,
    requested_text: Option<String>,
    character_id: Option<Uuid>,
    assignment: VoiceAssignmentView,
) -> Result<PreviewView, ServiceError> {
    preview_with_assignment(
        state,
        project_id,
        requested_text,
        character_id,
        Some(assignment),
    )
    .await
}

#[allow(clippy::too_many_lines)]
async fn preview_with_assignment(
    state: Arc<AppState>,
    project_id: Uuid,
    requested_text: Option<String>,
    character_id: Option<Uuid>,
    assignment_override: Option<VoiceAssignmentView>,
) -> Result<PreviewView, ServiceError> {
    let sidecars = resolve_sidecars(&state)?;
    let project = state
        .database
        .repositories()
        .projects
        .get_project(ProjectId::from_uuid(project_id))
        .await
        .map_err(storage_error)?
        .ok_or(ServiceError::NotFound)?;
    let (mut target, voices, providers, rules) = {
        let catalog = state.catalog.read().await;
        let target = catalog
            .characters
            .get(&project_id)
            .and_then(|characters| {
                character_id
                    .and_then(|id| characters.iter().find(|character| character.id == id))
                    .or_else(|| {
                        characters.iter().find(|character| {
                            matches!(character.role, audiobookai_core::CharacterRole::Narrator)
                        })
                    })
            })
            .cloned()
            .ok_or_else(|| {
                ServiceError::Conflict("detect characters before previewing a voice".to_owned())
            })?;
        (
            target,
            catalog.voice_sources.clone(),
            catalog.providers.clone(),
            catalog.pronunciation_rules.clone(),
        )
    };
    if let Some(assignment) = assignment_override {
        target.voice_assignment = Some(assignment);
    }
    let assignments = build_assignments(
        &project,
        std::slice::from_ref(&target),
        &voices,
        &providers,
        &state,
    )
    .await?;
    let assignment = assignments
        .get(&target.id)
        .cloned()
        .ok_or_else(|| ServiceError::Conflict("the preview character has no voice".to_owned()))?;
    let (chapter, paragraph) = first_selected_paragraph(&state, &project).await?;
    let original = requested_text.unwrap_or(paragraph.text);
    let original = original.trim();
    if original.is_empty() {
        return Err(ServiceError::InvalidRequest(
            "preview text must not be empty".to_owned(),
        ));
    }
    let original = original
        .chars()
        .take(MAX_PREVIEW_CHARACTERS)
        .collect::<String>();
    let (text, applied_rule_ids, dictionary_revision) = apply_pronunciation_rules(
        &original,
        &rules,
        project_id,
        target.id,
        project.metadata.language.as_deref(),
    )?;
    let segment = SegmentPlan {
        id: SegmentId::new(),
        proofing: false,
        key: segment_key(
            chapter.id.as_uuid(),
            paragraph.id.as_uuid(),
            0,
            original.len(),
            target.id,
        ),
        chapter_id: chapter.id.as_uuid(),
        paragraph_id: paragraph.id.as_uuid(),
        source_content_hash: paragraph.content_hash,
        byte_start: 0,
        byte_end: u64::try_from(original.len()).unwrap_or(u64::MAX),
        chapter_title: chapter.title,
        segment_ordinal: 0,
        playback_ordinal: 0,
        original_text: original.clone(),
        text: text.clone(),
        context: None,
        assignment,
        applied_rule_ids,
        dictionary_revision,
    };
    let cache = cache(&state);
    let key = segment_cache_fingerprint(&segment, "preview")
        .key()
        .map_err(media_error)?;
    if let Some(payload) = sqlx::query_scalar::<_, String>(
        "SELECT payload FROM artifacts WHERE cache_key = ? AND kind = 'preview'",
    )
    .bind(key.as_str())
    .fetch_optional(state.database.pool())
    .await
    .map_err(storage_error)?
    {
        let artifact: Artifact = serde_json::from_str(&payload).map_err(internal_error)?;
        if Path::new(&artifact.path).is_file() {
            return Ok(PreviewView {
                artifact_id: artifact.id.as_uuid(),
                audio_url: format!("/api/v1/artifacts/{}", artifact.id),
                text,
                duration_seconds: artifact.duration_ms.unwrap_or_default().div_ceil(1_000),
                billable: true,
                cached: true,
            });
        }
    }

    let policy = retry_policy(&state, &segment).await?;
    let request_character_count = u64::try_from(segment.text.chars().count()).unwrap_or(u64::MAX);
    let reservation_multiplier = retry_reservation_multiplier(&policy);
    let reservation_estimate = crate::accounting::rate_usage_estimate(
        &state,
        ProviderProfileId::from_uuid(segment.assignment.provider_id),
        UsageWorkload::Tts,
        segment.assignment.model.clone(),
        UsageQuantities {
            characters: Some(request_character_count),
            ..UsageQuantities::default()
        },
    )
    .await?;
    let reservation_estimates = vec![reservation_estimate; reservation_multiplier];

    let now = Utc::now();
    let mut job = Job {
        id: JobId::new(),
        project_id: ProjectId::from_uuid(project_id),
        kind: JobKind::Preview,
        state: JobState::Queued,
        export_profile_id: None,
        reservation_id: None,
        progress_completed: 0,
        progress_total: 1,
        status_message: Some("Billable preview".to_owned()),
        allow_budget_override: false,
        created_at: now,
        started_at: None,
        finished_at: None,
        updated_at: now,
        revision: 0,
    };
    let mut unit = JobUnit {
        id: JobUnitId::new(),
        job_id: job.id,
        kind: JobUnitKind::SynthesisSegment,
        state: JobUnitState::Ready,
        chapter_id: Some(chapter.id),
        segment_id: None,
        provider_profile_id: Some(ProviderProfileId::from_uuid(segment.assignment.provider_id)),
        dependencies: Vec::new(),
        attempt_count: 0,
        next_attempt_at: None,
        output_artifact_id: None,
        payload: BTreeMap::from([
            (
                "title".to_owned(),
                serde_json::json!("Billable narrator preview"),
            ),
            (
                "segmentPlan".to_owned(),
                serde_json::to_value(&segment).map_err(internal_error)?,
            ),
        ]),
        created_at: now,
        updated_at: now,
    };
    state
        .database
        .repositories()
        .proofing
        .insert_job_graph(&job, std::slice::from_ref(&unit), None)
        .await
        .map_err(storage_error)?;
    match crate::accounting::reserve_for_estimates(&state, &job, &reservation_estimates).await {
        Ok(Some(reservation_id)) => {
            let expected = job.revision;
            job.reservation_id = Some(reservation_id);
            job.updated_at = Utc::now();
            job = match state
                .database
                .repositories()
                .jobs
                .update(&job, expected)
                .await
            {
                Ok(job) => job,
                Err(error) => {
                    release_unattached_reservation(&state, reservation_id).await;
                    let detail = error.to_string();
                    let _ =
                        update_unit_state(&state, &mut unit, JobUnitState::Failed, Some(&detail))
                            .await;
                    mark_domain_job_failed(&state, job.id, &detail).await;
                    return Err(storage_error(error));
                }
            };
        }
        Ok(None) => {}
        Err(error) => {
            let detail = error.to_string();
            let _ = update_unit_state(&state, &mut unit, JobUnitState::Failed, Some(&detail)).await;
            mark_domain_job_failed(&state, job.id, &detail).await;
            return Err(error);
        }
    }

    let result: Result<PreviewView, ServiceError> = async {
        job = transition_job(
            &state,
            job.id,
            JobState::Running,
            "Synthesizing billable preview",
        )
        .await?;
        let runtime_id =
            ProviderId::new(segment.assignment.provider_id.to_string()).map_err(internal_error)?;
        let provider = state
            .providers
            .tts(&runtime_id)
            .await
            .map_err(|error| ServiceError::Conflict(error.to_string()))?;
        let _permit = provider_semaphore(
            segment.assignment.provider_id,
            segment.assignment.provider_concurrency,
        )
        .acquire_owned()
        .await
        .map_err(|_| ServiceError::Internal("provider semaphore closed".to_owned()))?;
        let request = SynthesisRequest {
            request_id: Uuid::new_v4(),
            text: segment.text.clone(),
            model: segment.assignment.model.clone(),
            voice: segment.assignment.voice_source.clone(),
            format: requested_audio_format(&segment.assignment),
            performance: segment.assignment.performance.clone(),
            options: BTreeMap::new(),
            pronunciation_dictionary_ids: Vec::new(),
        };
        let dispatch_estimate = crate::accounting::rate_usage_estimate(
            &state,
            ProviderProfileId::from_uuid(segment.assignment.provider_id),
            UsageWorkload::Tts,
            segment.assignment.model.clone(),
            UsageQuantities {
                characters: Some(request_character_count),
                ..UsageQuantities::default()
            },
        )
        .await?;
        let journal = AttemptJournal::new(
            Arc::clone(&state),
            unit.id,
            TtsUsageContext {
                job_id: job.id,
                segment: segment.clone(),
                provider_request_id: request.request_id,
                rate_card_id: dispatch_estimate.rate_card_id,
            },
        );
        let dispatch_consent_lock = state.dispatch_consent_lifecycle_lock(project_id).await;
        update_unit_state(&state, &mut unit, JobUnitState::Running, None).await?;
        let execution = execute_with_retry(&policy, &journal, |_| {
            let state = Arc::clone(&state);
            let provider = Arc::clone(&provider);
            let request = request.clone();
            let dispatch_estimate = dispatch_estimate.clone();
            let dispatch_consent_lock = Arc::clone(&dispatch_consent_lock);
            let dispatch_segment = segment.clone();
            async move {
                let _dispatch_consent_guard = dispatch_consent_lock.read().await;
                validate_segment_dispatch_boundary(&state, project_id, &dispatch_segment).await?;
                crate::accounting::verify_dispatch_is_reserved(
                    &state,
                    job.id,
                    &dispatch_estimate,
                )
                .await
                .map_err(|_| {
                    ProviderError::Configuration(
                        "the active hard-budget reservation does not permit this preview"
                            .to_owned(),
                    )
                })?;
                provider.preview(request).await
            }
        })
        .await
        .map_err(|error| retry_service_error(&state, job.id, &segment, &error))?;
        unit.attempt_count = execution.attempts.get();
        let successful_attempt_id =
            attempt_id_for_ordinal(&state, unit.id, execution.attempts.get()).await?;
        let response = execution.value;
        // Persist billable usage at the provider-success boundary. Local media failures after
        // this point must fail the preview without making accounting look uncharged.
        let mut usage = response.usage.clone();
        if usage.request_id.is_none() {
            usage.request_id = Some(request.request_id.to_string());
        }
        append_tts_usage(
            &state,
            job.id,
            &segment,
            successful_attempt_id,
            &usage,
            false,
            dispatch_estimate.rate_card_id,
        )
        .await?;
        let flac = normalize_provider_audio(&sidecars, &response, true).await?;
        let artifact_id = ArtifactId::new();
        let path = cache
            .put(
                &key,
                &flac,
                &serde_json::json!({
                    "schemaVersion": 1,
                    "artifactId": artifact_id,
                    "cacheKey": key.as_str(),
                    "operation": "preview",
                    "potentiallyBillable": true,
                    "providerProfileId": segment.assignment.provider_id,
                    "providerEndpoint": redacted_endpoint(segment.assignment.provider_endpoint.as_deref()),
                    "model": segment.assignment.model,
                    "voiceProfileId": segment.assignment.voice_id,
                    "dictionaryRevision": segment.dictionary_revision,
                    "normalizationVersion": NORMALIZATION_VERSION,
                    "createdAt": Utc::now(),
                }),
            )
            .map_err(media_error)?;
        cache.pin(&key).map_err(media_error)?;
        let duration = probe_duration_ms(&sidecars, &path).await?;
        let artifact = artifact_for_file_with_id(
            artifact_id,
            ArtifactKind::Preview,
            &path,
            Some("audio/flac".to_owned()),
            Some(duration),
            Some(key.as_str().to_owned()),
            Some(job.id),
        )
        .await?;
        persist_artifact(&state, project_id, &artifact).await?;
        unit.output_artifact_id = Some(artifact.id);
        update_unit_state(&state, &mut unit, JobUnitState::Completed, None).await?;
        job.progress_completed = 1;
        job.updated_at = Utc::now();
        let expected = job.revision;
        job = state
            .database
            .repositories()
            .jobs
            .update(&job, expected)
            .await
            .map_err(storage_error)?;
        let _ = transition_job(&state, job.id, JobState::Completed, "Preview complete").await?;
        if let Err(error) = release_job_cache_pins(&state, job.id).await {
            tracing::warn!(diagnostic_code = "preview.cache.unpin.failed", job_id = %job.id, %error, "could not release preview cache pins");
        }
        let cache_limit = state.catalog.read().await.settings.cache_limit_bytes;
        if let Err(error) = enforce_cache_limit(&state, cache_limit).await {
            tracing::warn!(diagnostic_code = "preview.cache.prune.failed", job_id = %job.id, %error, "could not enforce the cache limit after preview");
        }
        Ok(PreviewView {
            artifact_id: artifact.id.as_uuid(),
            audio_url: format!("/api/v1/artifacts/{}", artifact.id),
            text: text.clone(),
            duration_seconds: duration.div_ceil(1_000),
            billable: true,
            cached: false,
        })
    }
    .await;
    if let Err(error) = &result {
        let detail = error.to_string();
        let _ = update_unit_state(&state, &mut unit, JobUnitState::Failed, Some(&detail)).await;
        mark_domain_job_failed(&state, job.id, &detail).await;
    }
    if let Err(error) = crate::accounting::finalize_job_reservation(&state, job.id).await {
        if result.is_ok() {
            return Err(error);
        }
        tracing::warn!(diagnostic_code = "preview.budget.finalize.failed", job_id = %job.id, %error, "could not finalize preview budget reservation");
    }
    result
}

async fn first_selected_paragraph(
    state: &AppState,
    project: &Project,
) -> Result<(Chapter, Paragraph), ServiceError> {
    let chapters = state
        .database
        .repositories()
        .projects
        .list_chapters(project.book_id)
        .await
        .map_err(storage_error)?;
    for chapter in chapters.into_iter().filter(|chapter| chapter.selected) {
        let paragraphs = state
            .database
            .repositories()
            .projects
            .list_paragraphs(chapter.id)
            .await
            .map_err(storage_error)?;
        if let Some(paragraph) = paragraphs
            .into_iter()
            .find(|paragraph| !paragraph.text.trim().is_empty())
        {
            return Ok((chapter, paragraph));
        }
    }
    Err(ServiceError::Conflict(
        "the selected chapters contain no preview text".to_owned(),
    ))
}

/// Applies a user lifecycle action to both the domain record and desktop view.
// The lifecycle state machine stays together so each job kind and transition
// is checked against the same durable-state rules.
#[allow(clippy::too_many_lines)]
pub async fn job_action(
    state: Arc<AppState>,
    job_id: Uuid,
    action: &str,
) -> Result<JobView, ServiceError> {
    let id = JobId::from_uuid(job_id);
    let initial_job = state
        .database
        .repositories()
        .jobs
        .get(id)
        .await
        .map_err(storage_error)?
        .ok_or(ServiceError::NotFound)?;
    let _model_lifecycle_guard = state.model_lifecycle.lock().await;
    let project_lock = state
        .character_lifecycle_lock(initial_job.project_id.as_uuid())
        .await;
    let _project_guard = project_lock.lock().await;
    let job = state
        .database
        .repositories()
        .jobs
        .get(id)
        .await
        .map_err(storage_error)?
        .ok_or(ServiceError::NotFound)?;
    if matches!(action, "pause" | "resume" | "cancel" | "retry")
        && matches!(
            job.kind,
            JobKind::Preview | JobKind::QualityControl | JobKind::CacheCleanup
        )
    {
        return Err(ServiceError::ConflictDetails {
            code: "job_action_unsupported",
            detail: "this job runs synchronously and does not support lifecycle actions".to_owned(),
            meta: serde_json::json!({"jobId": job_id, "action": action, "kind": job.kind}),
        });
    }
    if matches!(action, "resume" | "retry")
        && let Some(active) =
            crate::api::blocking_project_job(&state, job.project_id.as_uuid(), Some(job_id)).await
    {
        return Err(crate::api::active_job_conflict(&active));
    }
    match (action, job.state) {
        ("pause", JobState::Queued) => {
            transition_job(&state, id, JobState::Running, "Preparing to pause").await?;
            transition_job(
                &state,
                id,
                JobState::Pausing,
                "Pausing after the active request",
            )
            .await?;
            if job.kind == JobKind::CharacterDetection {
                crate::workflows::spawn_character_detection(Arc::clone(&state), id.as_uuid());
            }
        }
        ("pause", JobState::Running) => {
            transition_job(
                &state,
                id,
                JobState::Pausing,
                "Pausing after the active request",
            )
            .await?;
            if job.kind == JobKind::CharacterDetection {
                crate::workflows::spawn_character_detection(Arc::clone(&state), id.as_uuid());
            }
        }
        ("resume", JobState::Paused) => {
            if matches!(job.kind, JobKind::Conversion | JobKind::Export) {
                transition_export_job_with_reservation(
                    &state,
                    id,
                    JobState::Running,
                    "Resuming job",
                )
                .await?;
            } else {
                transition_job(&state, id, JobState::Running, "Resuming job").await?;
            }
            match job.kind {
                JobKind::Conversion | JobKind::Export => {
                    schedule_conversion_job(Arc::clone(&state), id);
                }
                JobKind::SegmentRegeneration => {
                    schedule_segment_regeneration_job(Arc::clone(&state), id);
                }
                JobKind::CharacterDetection => {
                    crate::workflows::reset_detection_units_for_restart(&state, id, false).await?;
                    crate::workflows::spawn_character_detection(Arc::clone(&state), id.as_uuid());
                }
                JobKind::Preview | JobKind::QualityControl | JobKind::CacheCleanup => {}
            }
        }
        ("cancel", JobState::Queued | JobState::Running | JobState::Paused) => {
            transition_job(
                &state,
                id,
                JobState::Cancelling,
                "Cancelling after the active request",
            )
            .await?;
            match job.kind {
                JobKind::Conversion | JobKind::Export => {
                    schedule_conversion_job(Arc::clone(&state), id);
                }
                JobKind::SegmentRegeneration => {
                    schedule_segment_regeneration_job(Arc::clone(&state), id);
                }
                JobKind::CharacterDetection => {
                    crate::workflows::spawn_character_detection(Arc::clone(&state), id.as_uuid());
                }
                JobKind::Preview | JobKind::QualityControl | JobKind::CacheCleanup => {}
            }
        }
        ("retry", JobState::Failed) => {
            if job.kind == JobKind::CharacterDetection {
                crate::workflows::validate_detection_retry(&state, id).await?;
            }
            admit_failed_job_retry(&state, id).await?;
            match job.kind {
                JobKind::Conversion | JobKind::Export => {
                    schedule_conversion_retry(Arc::clone(&state), id);
                }
                JobKind::SegmentRegeneration => {
                    schedule_segment_regeneration_retry(Arc::clone(&state), id);
                }
                JobKind::CharacterDetection => {
                    crate::workflows::spawn_character_detection(Arc::clone(&state), id.as_uuid());
                }
                JobKind::Preview | JobKind::QualityControl | JobKind::CacheCleanup => {}
            }
        }
        (known, _) if matches!(known, "pause" | "resume" | "cancel" | "retry") => {
            return Err(ServiceError::Conflict(format!(
                "cannot {known} a job in state {:?}",
                job.state
            )));
        }
        _ => return Err(ServiceError::NotFound),
    }
    state
        .catalog
        .read()
        .await
        .jobs
        .get(&job_id)
        .cloned()
        .ok_or(ServiceError::NotFound)
}

/// Rebuilds export views from durable artifacts, including after restart.
pub async fn list_exports(state: &AppState) -> Result<Vec<ExportArtifactView>, ServiceError> {
    let rows = sqlx::query(
        "SELECT a.payload, a.project_id, a.pinned_by_job_id, j.export_profile_id \
         FROM artifacts a LEFT JOIN jobs j ON j.id = a.pinned_by_job_id \
         WHERE a.kind = 'export' ORDER BY a.created_at DESC",
    )
    .fetch_all(state.database.pool())
    .await
    .map_err(storage_error)?;
    let mut views = Vec::new();
    let mut manifest_orders = BTreeMap::<JobId, ExportManifestOrder>::new();
    for row in rows {
        let Ok(artifact) = serde_json::from_str::<Artifact>(row.get::<&str, _>("payload")) else {
            continue;
        };
        if !Path::new(&artifact.path).is_file() {
            continue;
        }
        let Ok(project_id) = Uuid::parse_str(row.get::<&str, _>("project_id")) else {
            continue;
        };
        let Some(job_id) = row.get::<Option<String>, _>("pinned_by_job_id") else {
            continue;
        };
        let Ok(job_id) = JobId::from_str(&job_id) else {
            continue;
        };
        let Some(profile_id) = row.get::<Option<String>, _>("export_profile_id") else {
            continue;
        };
        let Ok(profile_id) = ExportProfileId::from_str(&profile_id) else {
            continue;
        };
        let profile = load_export_profile(state, profile_id).await?;
        if let std::collections::btree_map::Entry::Vacant(entry) = manifest_orders.entry(job_id) {
            entry.insert(load_export_manifest_order(state, job_id).await?);
        }
        let manifest_order = manifest_orders
            .get(&job_id)
            .expect("manifest order was inserted immediately above");
        let project_title = state
            .catalog
            .read()
            .await
            .projects
            .get(&project_id)
            .map_or_else(
                || "Audiobook".to_owned(),
                |project| project.summary.title.clone(),
            );
        let path = Path::new(&artifact.path);
        views.push(ExportArtifactView {
            id: artifact.id.as_uuid(),
            project_id,
            job_id: job_id.as_uuid(),
            part_index: manifest_order
                .part_indexes
                .get(&artifact.id)
                .copied()
                .unwrap_or(0),
            part_count: manifest_order.part_count,
            project_title,
            format: format_name(profile.format).to_owned(),
            split_mode: layout_name(profile.layout).to_owned(),
            file_name: path
                .file_name()
                .and_then(std::ffi::OsStr::to_str)
                .unwrap_or("audiobook")
                .to_owned(),
            size_bytes: artifact.fingerprint.size_bytes,
            duration_seconds: artifact.duration_ms.unwrap_or_default().div_ceil(1_000),
            created_at: artifact.created_at,
            download_url: format!("/api/v1/artifacts/{}", artifact.id),
            manifest_url: manifest_order
                .manifest_id
                .map_or_else(String::new, |id| format!("/api/v1/artifacts/{id}")),
            chapter_markers: profile.layout == ExportLayout::SingleFile
                && !matches!(profile.format, ExportFormat::Wav),
        });
    }
    Ok(views)
}

#[derive(Debug, Default)]
struct ExportManifestOrder {
    manifest_id: Option<Uuid>,
    part_indexes: BTreeMap<ArtifactId, u32>,
    part_count: u32,
}

async fn load_export_manifest_order(
    state: &AppState,
    job_id: JobId,
) -> Result<ExportManifestOrder, ServiceError> {
    let export_rows = sqlx::query_scalar::<_, String>(
        "SELECT payload FROM artifacts WHERE pinned_by_job_id = ? AND kind = 'export' \
         ORDER BY created_at ASC, id ASC",
    )
    .bind(job_id.to_string())
    .fetch_all(state.database.pool())
    .await
    .map_err(storage_error)?;
    let exports = export_rows
        .into_iter()
        .filter_map(|payload| serde_json::from_str::<Artifact>(&payload).ok())
        .collect::<Vec<_>>();
    let manifest = sqlx::query_scalar::<_, String>(
        "SELECT payload FROM artifacts WHERE pinned_by_job_id = ? AND kind = 'export_manifest' \
         ORDER BY created_at DESC LIMIT 1",
    )
    .bind(job_id.to_string())
    .fetch_optional(state.database.pool())
    .await
    .map_err(storage_error)?
    .and_then(|payload| serde_json::from_str::<Artifact>(&payload).ok());
    let manifest_id = manifest.as_ref().map(|artifact| artifact.id.as_uuid());
    let mut manifest_paths = Vec::new();
    if let Some(manifest) = &manifest
        && let Ok(contents) = tokio::fs::read_to_string(&manifest.path).await
        && let Ok(value) = serde_json::from_str::<serde_json::Value>(&contents)
        && let Some(paths) = value
            .get("outputFiles")
            .and_then(serde_json::Value::as_array)
    {
        manifest_paths.extend(
            paths
                .iter()
                .filter_map(serde_json::Value::as_str)
                .map(str::to_owned),
        );
    }
    let ordered_ids = canonical_export_ids(&exports, &manifest_paths);
    let part_count = u32::try_from(ordered_ids.len()).unwrap_or(u32::MAX);
    let part_indexes = ordered_ids
        .into_iter()
        .enumerate()
        .map(|(index, id)| (id, u32::try_from(index).unwrap_or(u32::MAX)))
        .collect();
    Ok(ExportManifestOrder {
        manifest_id,
        part_indexes,
        part_count,
    })
}

pub(crate) async fn canonical_export_artifact_ids(
    state: &AppState,
    job_id: JobId,
) -> Result<Vec<ArtifactId>, ServiceError> {
    let order = load_export_manifest_order(state, job_id).await?;
    let mut parts = order.part_indexes.into_iter().collect::<Vec<_>>();
    parts.sort_by_key(|(_, index)| *index);
    Ok(parts.into_iter().map(|(id, _)| id).collect())
}

fn canonical_export_ids(exports: &[Artifact], manifest_paths: &[String]) -> Vec<ArtifactId> {
    let mut ordered_ids = Vec::with_capacity(exports.len());
    for path in manifest_paths {
        if let Some(artifact) = exports.iter().find(|artifact| artifact.path == *path)
            && !ordered_ids.contains(&artifact.id)
        {
            ordered_ids.push(artifact.id);
        }
    }
    let remaining_ids = exports
        .iter()
        .map(|artifact| artifact.id)
        .filter(|id| !ordered_ids.contains(id))
        .collect::<Vec<_>>();
    ordered_ids.extend(remaining_ids);
    ordered_ids
}

/// Streams an authenticated artifact and honors a single RFC 7233 byte range.
pub async fn artifact_response(
    state: &AppState,
    id: Uuid,
    request_headers: &HeaderMap,
) -> Result<Response, ServiceError> {
    let artifact = load_artifact(state, ArtifactId::from_uuid(id)).await?;
    let path = PathBuf::from(&artifact.path);
    let length = tokio::fs::metadata(&path).await?.len();
    let range = if let Some(value) = request_headers.get(header::RANGE) {
        if let Ok(range) = parse_byte_range(value, length) {
            Some(range)
        } else {
            let mut response = StatusCode::RANGE_NOT_SATISFIABLE.into_response();
            response.headers_mut().insert(
                header::CONTENT_RANGE,
                HeaderValue::from_str(&format!("bytes */{length}")).map_err(internal_error)?,
            );
            return Ok(response);
        }
    } else {
        None
    };
    let (start, end, status) = range.map_or(
        (0, length.saturating_sub(1), StatusCode::OK),
        |(start, end)| (start, end, StatusCode::PARTIAL_CONTENT),
    );
    let response_length = if length == 0 {
        0
    } else {
        end.saturating_sub(start).saturating_add(1)
    };
    let mut file = tokio::fs::File::open(&path).await?;
    file.seek(std::io::SeekFrom::Start(start)).await?;
    let stream = stream::try_unfold(
        (file, response_length),
        |(mut file, remaining)| async move {
            if remaining == 0 {
                return Ok::<_, std::io::Error>(None);
            }
            let maximum = usize::try_from(remaining.min(RANGE_CHUNK_BYTES as u64))
                .unwrap_or(RANGE_CHUNK_BYTES);
            let mut buffer = vec![0_u8; maximum];
            let read = file.read(&mut buffer).await?;
            if read == 0 {
                return Ok(None);
            }
            buffer.truncate(read);
            let remaining = remaining.saturating_sub(u64::try_from(read).unwrap_or(u64::MAX));
            Ok(Some((Bytes::from(buffer), (file, remaining))))
        },
    );
    let mut response = Response::new(Body::from_stream(stream));
    *response.status_mut() = status;
    let headers = response.headers_mut();
    headers.insert(header::ACCEPT_RANGES, HeaderValue::from_static("bytes"));
    headers.insert(
        header::CONTENT_LENGTH,
        HeaderValue::from_str(&response_length.to_string()).map_err(internal_error)?,
    );
    headers.insert(
        header::CONTENT_TYPE,
        HeaderValue::from_str(
            artifact
                .media_type
                .as_deref()
                .unwrap_or("application/octet-stream"),
        )
        .map_err(internal_error)?,
    );
    if status == StatusCode::PARTIAL_CONTENT {
        headers.insert(
            header::CONTENT_RANGE,
            HeaderValue::from_str(&format!("bytes {start}-{end}/{length}"))
                .map_err(internal_error)?,
        );
    }
    let disposition = if artifact.kind == ArtifactKind::Export {
        "attachment"
    } else {
        "inline"
    };
    let filename = path
        .file_name()
        .and_then(std::ffi::OsStr::to_str)
        .unwrap_or("artifact")
        .replace(['"', '\r', '\n'], "_");
    headers.insert(
        header::CONTENT_DISPOSITION,
        HeaderValue::from_str(&format!("{disposition}; filename=\"{filename}\""))
            .map_err(internal_error)?,
    );
    sqlx::query("UPDATE artifacts SET last_accessed_at = ? WHERE id = ?")
        .bind(Utc::now().to_rfc3339())
        .bind(id.to_string())
        .execute(state.database.pool())
        .await
        .map_err(storage_error)?;
    Ok(response)
}

fn parse_byte_range(value: &HeaderValue, length: u64) -> Result<(u64, u64), ()> {
    if length == 0 {
        return Err(());
    }
    let value = value.to_str().map_err(|_| ())?;
    let value = value.strip_prefix("bytes=").ok_or(())?;
    if value.contains(',') {
        return Err(());
    }
    let (start, end) = value.split_once('-').ok_or(())?;
    if start.is_empty() {
        let suffix = end.parse::<u64>().map_err(|_| ())?;
        if suffix == 0 {
            return Err(());
        }
        return Ok((length.saturating_sub(suffix.min(length)), length - 1));
    }
    let start = start.parse::<u64>().map_err(|_| ())?;
    if start >= length {
        return Err(());
    }
    let end = if end.is_empty() {
        length - 1
    } else {
        end.parse::<u64>().map_err(|_| ())?.min(length - 1)
    };
    (start <= end).then_some((start, end)).ok_or(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_segment_plan() -> SegmentPlan {
        SegmentPlan {
            id: SegmentId::new(),
            proofing: true,
            key: "stable-segment".to_owned(),
            chapter_id: Uuid::from_u128(1),
            paragraph_id: Uuid::from_u128(2),
            source_content_hash: "source".to_owned(),
            byte_start: 0,
            byte_end: 5,
            chapter_title: "Chapter".to_owned(),
            segment_ordinal: 0,
            playback_ordinal: 0,
            original_text: "Hello".to_owned(),
            text: "Hello".to_owned(),
            context: None,
            assignment: SpeakerAssignment {
                character_id: Uuid::from_u128(3),
                character_name: "Narrator".to_owned(),
                provider_id: Uuid::from_u128(4),
                provider_name: "Provider".to_owned(),
                provider_kind: ProviderKindView::Elevenlabs,
                provider_mode: Some(ProviderModeView::CloudRemote),
                provider_endpoint: None,
                provider_snapshot_id: Some(Uuid::from_u128(6)),
                provider_version: Some("1".to_owned()),
                provider_concurrency: 1,
                voice_id: Uuid::from_u128(5),
                voice_source: "voice".to_owned(),
                voice_name: "Voice".to_owned(),
                model: Some("eleven_multilingual_v2".to_owned()),
                performance: PerformanceSettings {
                    speed: Some(1.0),
                    ..PerformanceSettings::default()
                },
                timing: TimingSettings::default(),
            },
            applied_rule_ids: Vec::new(),
            dictionary_revision: "dictionary".to_owned(),
        }
    }

    async fn filesystem_test_state() -> (tempfile::TempDir, Arc<AppState>) {
        let directory = tempfile::tempdir().expect("temporary directory");
        let database = audiobookai_storage::Database::open_in(directory.path())
            .await
            .expect("database");
        let state = Arc::new(
            AppState::new(
                crate::ServiceConfig {
                    bind: "127.0.0.1:0".parse().expect("address"),
                    data_dir: directory.path().to_path_buf(),
                    bundled_sidecar_dir: None,
                    tls: None,
                    lan_hostnames: Vec::new(),
                    allow_insecure_lan: false,
                    desktop_bootstrap: false,
                },
                database,
            )
            .await
            .expect("application state"),
        );
        (directory, state)
    }

    async fn insert_recovery_project(state: &AppState, name: &str) -> ProjectId {
        let now = Utc::now();
        let book_id = audiobookai_core::BookId::new();
        let project_id = ProjectId::new();
        sqlx::query(
            "INSERT INTO books (id, managed_epub_path, source_hash, imported_at, payload) \
             VALUES (?, ?, ?, ?, '{}')",
        )
        .bind(book_id.to_string())
        .bind(format!("/fixtures/{book_id}.epub"))
        .bind(format!("fixture-{book_id}"))
        .bind(now.to_rfc3339())
        .execute(state.database.pool())
        .await
        .expect("recovery book fixture");
        sqlx::query(
            "INSERT INTO projects \
             (id, book_id, name, status, created_at, updated_at, revision, payload) \
             VALUES (?, ?, ?, 'draft', ?, ?, 0, '{}')",
        )
        .bind(project_id.to_string())
        .bind(book_id.to_string())
        .bind(name)
        .bind(now.to_rfc3339())
        .bind(now.to_rfc3339())
        .execute(state.database.pool())
        .await
        .expect("recovery project fixture");
        project_id
    }

    fn recovered_job(
        id: u128,
        project_id: ProjectId,
        kind: JobKind,
        state: JobState,
        created_at: chrono::DateTime<Utc>,
    ) -> Job {
        Job {
            id: JobId::from_uuid(Uuid::from_u128(id)),
            project_id,
            kind,
            state,
            export_profile_id: None,
            reservation_id: None,
            progress_completed: 0,
            progress_total: 1,
            status_message: Some("legacy active job".to_owned()),
            allow_budget_override: false,
            created_at,
            started_at: (state != JobState::Queued).then_some(created_at),
            finished_at: None,
            updated_at: created_at,
            revision: 0,
        }
    }

    async fn insert_recovery_export_profile(
        state: &AppState,
        project_id: ProjectId,
        output_directory: &Path,
    ) -> ExportProfile {
        let now = Utc::now();
        let output_directory = tokio::fs::canonicalize(output_directory)
            .await
            .expect("canonical recovery output directory");
        let profile = ExportProfile {
            id: ExportProfileId::new(),
            project_id,
            name: "Recovery M4B".to_owned(),
            format: ExportFormat::M4b,
            layout: ExportLayout::SingleFile,
            output_directory: output_directory.to_string_lossy().into_owned(),
            filename_template: "legacy-book".to_owned(),
            audio: audiobookai_core::AudioEncodingSettings::default(),
            background_music: None,
            embed_cover: true,
            embed_chapters: true,
            write_sidecar_manifest: true,
            created_at: now,
            updated_at: now,
        };
        sqlx::query(
            "INSERT INTO export_profiles (id, project_id, name, format, layout, updated_at, payload) \
             VALUES (?, ?, ?, 'm4b', 'single_file', ?, ?)",
        )
        .bind(profile.id.to_string())
        .bind(project_id.to_string())
        .bind(&profile.name)
        .bind(now.to_rfc3339())
        .bind(serde_json::to_string(&profile).unwrap())
        .execute(state.database.pool())
        .await
        .expect("recovery export profile");
        profile
    }

    #[test]
    fn recovered_project_conflicts_choose_one_deterministic_production_survivor() {
        let project_id = ProjectId::new();
        let other_project_id = ProjectId::new();
        let now = Utc::now();
        let earlier = now - chrono::Duration::seconds(10);
        let preview = recovered_job(1, project_id, JobKind::Preview, JobState::Running, earlier);
        let deterministic_survivor = recovered_job(
            2,
            project_id,
            JobKind::CharacterDetection,
            JobState::Paused,
            now,
        );
        let same_timestamp =
            recovered_job(3, project_id, JobKind::Conversion, JobState::Running, now);
        let newer = recovered_job(
            4,
            project_id,
            JobKind::Export,
            JobState::Queued,
            now + chrono::Duration::seconds(1),
        );
        let unrelated = recovered_job(
            5,
            other_project_id,
            JobKind::SegmentRegeneration,
            JobState::Running,
            now,
        );

        let conflicts = recovered_production_conflicts(&[
            preview,
            deterministic_survivor.clone(),
            same_timestamp.clone(),
            newer.clone(),
            unrelated,
        ]);

        assert_eq!(conflicts, BTreeSet::from([same_timestamp.id, newer.id]));
        assert!(!conflicts.contains(&deterministic_survivor.id));
    }

    #[test]
    fn worker_handoff_never_drops_an_accepted_retry_at_release_boundary() {
        // Conversion and proof export share the conversion worker; regeneration uses the same
        // ownership registry with its own runner. Exercise each scheduling class independently.
        for job_id in [JobId::new(), JobId::new(), JobId::new()] {
            assert!(
                request_production_worker(job_id, false),
                "the original worker owns the first start"
            );
            assert!(
                !request_production_worker(job_id, true),
                "a retry while cleanup is active is handed to the owner"
            );
            assert!(
                finish_production_worker_iteration(job_id),
                "the owner must consume the handed-off retry"
            );
            assert!(
                !finish_production_worker_iteration(job_id),
                "the owner releases itself after the retry iteration"
            );

            // If release wins the mutex race, the retry request becomes a fresh owner instead.
            assert!(
                request_production_worker(job_id, true),
                "a retry after release must create a new owner"
            );
            assert!(!finish_production_worker_iteration(job_id));
        }

        let paused_job_id = JobId::new();
        assert!(request_production_worker(paused_job_id, false));
        assert!(
            !request_production_worker(paused_job_id, false),
            "resume wakes an existing paused owner without requesting a second iteration"
        );
        assert!(
            !finish_production_worker_iteration(paused_job_id),
            "a successful resumed owner must be released without rerunning a completed job"
        );
    }

    #[tokio::test]
    async fn startup_terminalizes_legacy_project_conflicts_before_resuming_survivor() {
        let (directory, state) = filesystem_test_state().await;
        let project_id = insert_recovery_project(&state, "Conflicting recovery").await;
        let output_directory = directory.path().join("survivor-exports");
        tokio::fs::create_dir_all(&output_directory).await.unwrap();
        let profile = insert_recovery_export_profile(&state, project_id, &output_directory).await;
        let now = Utc::now();
        let mut survivor = recovered_job(
            10,
            project_id,
            JobKind::Conversion,
            JobState::Paused,
            now - chrono::Duration::seconds(10),
        );
        survivor.export_profile_id = Some(profile.id);
        let conflicting_export =
            recovered_job(11, project_id, JobKind::Export, JobState::Queued, now);
        let conflicting_detection = recovered_job(
            12,
            project_id,
            JobKind::CharacterDetection,
            JobState::Queued,
            now + chrono::Duration::seconds(1),
        );
        let jobs = state.database.repositories().jobs;
        jobs.insert(&survivor).await.expect("survivor fixture");
        jobs.insert(&conflicting_export)
            .await
            .expect("export conflict fixture");
        jobs.insert(&conflicting_detection)
            .await
            .expect("detection conflict fixture");

        resume_durable_conversions(Arc::clone(&state))
            .await
            .expect("conflicts recovered");

        assert_eq!(
            jobs.get(survivor.id).await.unwrap().unwrap().state,
            JobState::Paused
        );
        let failed_export = jobs.get(conflicting_export.id).await.unwrap().unwrap();
        assert_eq!(failed_export.state, JobState::Failed);
        assert_eq!(
            failed_export.status_message.as_deref(),
            Some(RECOVERED_PRODUCTION_CONFLICT)
        );
        assert!(
            jobs.get(conflicting_detection.id)
                .await
                .unwrap()
                .unwrap()
                .state
                .is_terminal()
        );
        assert_eq!(
            jobs.list_active()
                .await
                .unwrap()
                .into_iter()
                .map(|job| job.id)
                .collect::<Vec<_>>(),
            vec![survivor.id]
        );
    }

    #[tokio::test]
    async fn startup_acquires_legacy_export_destination_before_resume() {
        let (directory, state) = filesystem_test_state().await;
        let project_id = insert_recovery_project(&state, "Legacy output recovery").await;
        let output_directory = directory.path().join("exports");
        tokio::fs::create_dir_all(&output_directory).await.unwrap();
        let profile = insert_recovery_export_profile(&state, project_id, &output_directory).await;
        let mut legacy = recovered_job(
            13,
            project_id,
            JobKind::Conversion,
            JobState::Paused,
            Utc::now(),
        );
        legacy.export_profile_id = Some(profile.id);
        let jobs = state.database.repositories().jobs;
        jobs.insert(&legacy)
            .await
            .expect("legacy job without claim");

        resume_durable_conversions(Arc::clone(&state))
            .await
            .expect("legacy output claim recovered");

        let reservation = jobs
            .get_output_reservation(legacy.id)
            .await
            .unwrap()
            .expect("recovered output claim");
        assert_eq!(reservation.state, OutputReservationState::Reserved);
        assert_eq!(
            Path::new(&reservation.destination_path),
            Path::new(&profile.output_directory).join("legacy-book.m4b")
        );
        assert_eq!(
            jobs.get(legacy.id).await.unwrap().unwrap().state,
            JobState::Paused
        );
    }

    #[tokio::test]
    async fn startup_fails_legacy_export_instead_of_adopting_existing_output() {
        let (directory, state) = filesystem_test_state().await;
        let project_id = insert_recovery_project(&state, "Legacy output conflict").await;
        let output_directory = directory.path().join("exports");
        tokio::fs::create_dir_all(&output_directory).await.unwrap();
        let profile = insert_recovery_export_profile(&state, project_id, &output_directory).await;
        let destination = output_directory.join("legacy-book.m4b");
        tokio::fs::write(&destination, b"foreign output")
            .await
            .unwrap();
        let mut legacy = recovered_job(
            15,
            project_id,
            JobKind::Export,
            JobState::Paused,
            Utc::now(),
        );
        legacy.export_profile_id = Some(profile.id);
        let jobs = state.database.repositories().jobs;
        jobs.insert(&legacy)
            .await
            .expect("legacy job without claim");

        resume_durable_conversions(Arc::clone(&state))
            .await
            .expect("legacy conflict terminalized");

        let recovered = jobs.get(legacy.id).await.unwrap().unwrap();
        assert_eq!(recovered.state, JobState::Failed);
        assert!(
            recovered
                .status_message
                .as_deref()
                .is_some_and(|message| message.contains("already exists"))
        );
        assert_eq!(jobs.get_output_reservation(legacy.id).await.unwrap(), None);
        assert_eq!(
            tokio::fs::read(destination).await.unwrap(),
            b"foreign output"
        );
    }

    #[tokio::test]
    async fn reserved_destination_is_rejected_before_becoming_an_output_root() {
        let (directory, state) = filesystem_test_state().await;
        let project_id = insert_recovery_project(&state, "Hierarchy ownership").await;
        let export_root = directory.path().join("exports");
        tokio::fs::create_dir_all(&export_root).await.unwrap();
        let reserved_destination = tokio::fs::canonicalize(&export_root)
            .await
            .unwrap()
            .join("book.m4b");
        let owner = recovered_job(
            14,
            project_id,
            JobKind::Conversion,
            JobState::Paused,
            Utc::now(),
        );
        let now = Utc::now();
        let reservation = OutputDestinationReservation {
            job_id: owner.id,
            project_id,
            destination_key: output_destination_key(&reserved_destination),
            destination_path: reserved_destination.to_string_lossy().into_owned(),
            layout: ExportLayout::SingleFile,
            state: OutputReservationState::Reserved,
            created_at: now,
            updated_at: now,
            promoted_at: None,
        };
        state
            .database
            .repositories()
            .jobs
            .insert_with_output_reservation(&owner, &reservation)
            .await
            .unwrap();

        assert!(
            ensure_output_directory_not_reserved(&state, &reserved_destination)
                .await
                .is_err()
        );
        assert!(!reserved_destination.exists());
        let reserved_manifest = PathBuf::from(format!(
            "{}.manifest.json",
            reserved_destination.to_string_lossy()
        ));
        assert!(
            ensure_output_directory_not_reserved(&state, &reserved_manifest)
                .await
                .is_err()
        );
        assert!(!reserved_manifest.exists());
    }

    #[tokio::test]
    async fn unresolved_paid_conflict_is_failed_without_releasing_its_reservation() {
        let (_directory, state) = filesystem_test_state().await;
        let project_id = insert_recovery_project(&state, "Uncertain recovery").await;
        let now = Utc::now();
        let survivor = recovered_job(
            20,
            project_id,
            JobKind::Conversion,
            JobState::Paused,
            now - chrono::Duration::seconds(10),
        );
        let reservation_id = ReservationId::new();
        let mut conflicting = recovered_job(
            21,
            project_id,
            JobKind::SegmentRegeneration,
            JobState::Running,
            now,
        );
        conflicting.reservation_id = Some(reservation_id);
        let unit = JobUnit {
            id: JobUnitId::new(),
            job_id: conflicting.id,
            kind: JobUnitKind::SynthesisSegment,
            state: JobUnitState::Running,
            chapter_id: None,
            segment_id: None,
            provider_profile_id: None,
            dependencies: Vec::new(),
            attempt_count: 0,
            next_attempt_at: None,
            output_artifact_id: None,
            payload: BTreeMap::new(),
            created_at: now,
            updated_at: now,
        };
        let jobs = state.database.repositories().jobs;
        jobs.insert(&survivor).await.expect("survivor fixture");
        jobs.insert(&conflicting)
            .await
            .expect("paid conflict fixture");
        jobs.upsert_unit(&unit).await.expect("paid unit fixture");
        sqlx::query(
            "INSERT INTO budget_reservations \
             (id, job_id, status, created_at, expires_at, reconciled_at, payload) \
             VALUES (?, ?, 'active', ?, NULL, NULL, '{}')",
        )
        .bind(reservation_id.to_string())
        .bind(conflicting.id.to_string())
        .bind(now.to_rfc3339())
        .execute(state.database.pool())
        .await
        .expect("legacy reservation fixture");

        resume_durable_conversions(Arc::clone(&state))
            .await
            .expect("uncertain conflict recovered");

        let recovered = jobs.get(conflicting.id).await.unwrap().unwrap();
        assert_eq!(recovered.state, JobState::Failed);
        assert!(
            recovered
                .status_message
                .as_deref()
                .is_some_and(|message| message.contains("may have been charged"))
        );
        let recovered_unit = jobs.get_unit(unit.id).await.unwrap().unwrap();
        assert_eq!(recovered_unit.state, JobUnitState::Failed);
        assert_eq!(
            recovered_unit
                .payload
                .get("uncertainUsageUnresolved")
                .and_then(serde_json::Value::as_bool),
            Some(true)
        );
        assert_eq!(
            sqlx::query_scalar::<_, String>("SELECT status FROM budget_reservations WHERE id = ?")
                .bind(reservation_id.to_string())
                .fetch_one(state.database.pool())
                .await
                .unwrap(),
            "active"
        );

        // A second startup keeps the unknown charge and its reservation durable instead of
        // retroactively releasing it as a zero-usage job.
        resume_durable_conversions(Arc::clone(&state))
            .await
            .expect("idempotent uncertain recovery");
        assert_eq!(
            sqlx::query_scalar::<_, String>("SELECT status FROM budget_reservations WHERE id = ?")
                .bind(reservation_id.to_string())
                .fetch_one(state.database.pool())
                .await
                .unwrap(),
            "active"
        );
    }

    #[allow(clippy::too_many_lines)]
    #[tokio::test]
    async fn startup_reconciles_expired_terminal_detection_cycle_usage() {
        let (_directory, state) = filesystem_test_state().await;
        let project_id = insert_recovery_project(&state, "Expired detection accounting").await;
        let now = Utc::now();
        let cycle_started_at = now - chrono::Duration::hours(2);
        let reservation_id = ReservationId::new();
        let mut job = recovered_job(
            22,
            project_id,
            JobKind::CharacterDetection,
            JobState::Failed,
            cycle_started_at - chrono::Duration::minutes(5),
        );
        job.reservation_id = Some(reservation_id);
        job.finished_at = Some(now - chrono::Duration::hours(1));
        state
            .database
            .repositories()
            .jobs
            .insert(&job)
            .await
            .expect("terminal detection fixture");

        let provider = audiobookai_core::ProviderProfile {
            id: ProviderProfileId::new(),
            name: "Detection provider".to_owned(),
            family: audiobookai_core::ProviderFamily::OpenAiCompatible,
            role: audiobookai_core::ProviderRole::CharacterDetection,
            deployment: audiobookai_core::ProviderDeployment::ExternalEndpoint,
            endpoint: Some("http://127.0.0.1:1234".to_owned()),
            executable_path: None,
            working_directory: None,
            arguments: Vec::new(),
            environment_secret_ids: BTreeMap::new(),
            credential_secret_id: None,
            enabled: true,
            concurrency_override: None,
            settings: audiobookai_core::SettingsMap::default(),
            capability_snapshot: None,
            created_at: cycle_started_at,
            updated_at: cycle_started_at,
        };
        state
            .database
            .repositories()
            .providers
            .upsert(&provider)
            .await
            .expect("provider fixture");
        let budget = audiobookai_core::Budget {
            id: audiobookai_core::BudgetId::new(),
            name: "Detection characters".to_owned(),
            scope: audiobookai_core::BudgetScope {
                kind: audiobookai_core::BudgetScopeKind::Global,
                provider_profile_id: None,
            },
            period: audiobookai_core::BudgetPeriod::Lifetime,
            metric: audiobookai_core::BudgetMetric::Characters,
            currency: None,
            limit: 100,
            used: 0,
            warning_threshold_percent: 80,
            hard: true,
            enabled: true,
            period_started_at: cycle_started_at,
            period_ends_at: None,
            created_at: cycle_started_at,
            updated_at: cycle_started_at,
        };
        state
            .database
            .repositories()
            .budgets
            .upsert(&budget)
            .await
            .expect("budget fixture");
        let reservation = audiobookai_core::BudgetReservation {
            id: reservation_id,
            job_id: job.id,
            status: audiobookai_core::ReservationStatus::Active,
            allocations: vec![audiobookai_core::BudgetAllocation {
                budget_id: budget.id,
                reserved_amount: 50,
                actual_amount: None,
            }],
            created_at: cycle_started_at,
            expires_at: Some(now - chrono::Duration::hours(1)),
            reconciled_at: None,
        };
        state
            .database
            .repositories()
            .budgets
            .reserve(&reservation)
            .await
            .expect("reservation fixture");
        state
            .database
            .repositories()
            .usage
            .append(&UsageEvent {
                id: UsageEventId::new(),
                occurred_at: cycle_started_at + chrono::Duration::minutes(1),
                workload: UsageWorkload::CharacterDetection,
                project_id,
                job_id: Some(job.id),
                attempt_id: None,
                chapter_id: None,
                segment_id: None,
                provider_profile_id: provider.id,
                provider_family: "openai_compatible".to_owned(),
                endpoint_family: "http://127.0.0.1:1234".to_owned(),
                model: Some("detection-model".to_owned()),
                voice_profile_id: None,
                provider_request_id: None,
                quantities: UsageQuantities {
                    characters: Some(12),
                    ..UsageQuantities::default()
                },
                quantity_source: ProvenanceQuality::Reported,
                cost: None,
                cost_source: ProvenanceQuality::Unknown,
                rate_card_id: None,
                uncertain_charge: false,
                redacted_raw_usage: BTreeMap::new(),
            })
            .await
            .expect("usage fixture");
        state
            .database
            .repositories()
            .budgets
            .get_at(budget.id, now)
            .await
            .expect("expire reservation");

        recover_terminal_paid_reservations(&state)
            .await
            .expect("startup accounting recovery");

        assert_eq!(
            state
                .database
                .repositories()
                .budgets
                .get_reservation(reservation_id)
                .await
                .unwrap()
                .unwrap()
                .status,
            audiobookai_core::ReservationStatus::Reconciled
        );
        assert_eq!(
            state
                .database
                .repositories()
                .budgets
                .get(budget.id)
                .await
                .unwrap()
                .unwrap()
                .used,
            12
        );
    }

    #[allow(clippy::too_many_lines)]
    #[tokio::test]
    async fn semantic_assignment_resolution_does_not_require_dispatch_readiness_or_consent() {
        let (_directory, state) = filesystem_test_state().await;
        let now = Utc::now();
        let project = Project {
            id: ProjectId::new(),
            book_id: audiobookai_core::BookId::new(),
            name: "Semantic fixture".to_owned(),
            status: audiobookai_core::ProjectStatus::Ready,
            metadata: audiobookai_core::BookMetadata::default(),
            cloud_consent: audiobookai_core::CloudConsent::default(),
            settings: audiobookai_core::ProjectSettings::default(),
            character_reviewed_at: Some(now),
            created_at: now,
            updated_at: now,
        };
        let provider_id = Uuid::new_v4();
        let voice_id = Uuid::new_v4();
        let character = crate::models::CharacterView {
            id: Uuid::new_v4(),
            role: audiobookai_core::CharacterRole::Narrator,
            canonical_name: "Narrator".to_owned(),
            aliases: Vec::new(),
            confidence: 1.0,
            dialogue_count: 1,
            voice_assignment: Some(VoiceAssignmentView {
                provider_profile_id: provider_id,
                provider_name: "Offline cloud".to_owned(),
                voice_id,
                voice_name: "Stored voice".to_owned(),
                model: Some("stored-model".to_owned()),
                performance: PerformanceSettings::default(),
                timing: TimingSettings::default(),
            }),
            evidence: Vec::new(),
        };
        let provider = ProviderProfileView {
            id: provider_id,
            name: "Offline cloud".to_owned(),
            kind: ProviderKindView::Openai,
            mode: ProviderModeView::CloudRemote,
            endpoint: Some("https://example.invalid".to_owned()),
            executable_path: None,
            working_directory: None,
            arguments: Vec::new(),
            status: crate::models::ProviderStatusView::Offline,
            model: Some("stored-model".to_owned()),
            credential_configured: false,
            capabilities: None,
            capability_source: None,
            capability_updated_at: None,
            last_error: None,
        };
        let voices = HashMap::from([(voice_id, "stored-provider-voice".to_owned())]);
        let providers = HashMap::from([(provider_id, provider)]);

        let semantic = build_assignments_for(
            &project,
            std::slice::from_ref(&character),
            &voices,
            &providers,
            &state,
            AssignmentPurpose::Semantic,
        )
        .await
        .expect("provider-free semantic identity");
        assert_eq!(semantic[&character.id].voice_id, voice_id);
        assert!(
            build_assignments(
                &project,
                std::slice::from_ref(&character),
                &voices,
                &providers,
                &state,
            )
            .await
            .is_err(),
            "dispatch still requires a ready provider and cloud consent"
        );

        let mut consent_only = providers[&provider_id].clone();
        consent_only.status = crate::models::ProviderStatusView::Online;
        consent_only.credential_configured = true;
        consent_only.capability_updated_at = Some(Utc::now());
        consent_only.capabilities = Some(crate::models::ProviderCapabilitiesView {
            tts: true,
            character_detection: false,
            streaming: false,
            voice_cloning: false,
            pronunciation: false,
            process_control: false,
            model_control: false,
            model_list: false,
            model_download: false,
            model_delete: false,
            model_load: false,
            model_unload: false,
            model_switch: false,
            temperature: "unsupported".to_owned(),
            reasoning: Vec::new(),
            max_concurrency: Some(1),
            model_performance: Vec::new(),
        });
        let consent_only = HashMap::from([(provider_id, consent_only)]);
        assert!(matches!(
            build_assignments(
                &project,
                std::slice::from_ref(&character),
                &voices,
                &consent_only,
                &state,
            )
            .await,
            Err(ServiceError::Conflict(detail)) if detail.contains("consent")
        ));
    }

    #[tokio::test]
    async fn assembly_boundary_rechecks_selected_artifact_integrity() {
        let directory = tempfile::tempdir().expect("temporary directory");
        let path = directory.path().join("selected.flac");
        tokio::fs::write(&path, b"selected take")
            .await
            .expect("selected artifact");
        let now = Utc::now();
        let artifact = Artifact {
            id: ArtifactId::new(),
            kind: ArtifactKind::SegmentAudio,
            path: path.to_string_lossy().into_owned(),
            fingerprint: fingerprint_file(&path).await.expect("fingerprint"),
            media_type: Some("audio/flac".to_owned()),
            duration_ms: Some(1_000),
            cache_key: None,
            pinned_by_job_id: None,
            created_at: now,
            last_accessed_at: now,
        };
        verify_selected_artifact_integrity(&artifact)
            .await
            .expect("initial proof-export validation");

        let mut unknown = artifact.clone();
        unknown.fingerprint.algorithm = "sha256".to_owned();
        assert!(
            verify_selected_artifacts_before_use(&[&unknown])
                .await
                .is_err()
        );

        tokio::fs::write(&path, b"tampered take")
            .await
            .expect("tampered artifact");
        assert!(
            verify_selected_artifacts_before_use(&[&artifact])
                .await
                .is_err(),
            "the assembly boundary must reject a take changed after initial validation"
        );
    }

    #[tokio::test]
    async fn proof_take_materialization_rejects_a_retained_partial_file() {
        let directory = tempfile::tempdir().expect("temporary directory");
        let source_path = directory.path().join("source.flac");
        let destination = directory.path().join("take.flac");
        tokio::fs::write(&source_path, b"complete normalized take")
            .await
            .unwrap();
        tokio::fs::write(&destination, b"retained partial")
            .await
            .unwrap();
        let source = artifact_for_file(
            ArtifactKind::SegmentAudio,
            &source_path,
            Some("audio/flac".to_owned()),
            Some(1_000),
            None,
            None,
        )
        .await
        .expect("source artifact");

        assert!(matches!(
            materialize_proof_take_file(&source, &destination).await,
            Err(ServiceError::Conflict(_))
        ));
        assert_eq!(
            tokio::fs::read(destination).await.unwrap(),
            b"retained partial",
            "mismatched partial data must remain fail-closed and never become a take"
        );
    }

    #[tokio::test]
    async fn split_export_promotion_resumes_after_only_some_files_were_moved() {
        let (directory, state) = filesystem_test_state().await;
        let job_id = JobId::new();
        let output_directory = directory.path().join("exports");
        let staging = prepare_private_export_staging(&state, job_id)
            .await
            .expect("private export staging");
        let temporary = export_staging_output_path(&staging, ExportLayout::PerChapter, "m4b");
        let final_output = output_directory.join("book");
        tokio::fs::create_dir_all(&output_directory)
            .await
            .expect("public export directory");
        tokio::fs::create_dir_all(&temporary)
            .await
            .expect("temporary export directory");
        let first = temporary.join("01-first.m4b");
        let second = temporary.join("02-second.m4b");
        tokio::fs::write(&first, b"first chapter")
            .await
            .expect("first export file");
        tokio::fs::write(&second, b"second chapter")
            .await
            .expect("second export file");
        persist_export_promotion_marker(
            &state,
            job_id,
            &final_output,
            &[(first.clone(), 1_000), (second.clone(), 2_000)],
        )
        .await
        .expect("promotion marker");

        tokio::fs::create_dir(&final_output)
            .await
            .expect("final export directory");
        mark_split_export_directory_created(&state, job_id, &final_output)
            .await
            .expect("durable split-directory ownership marker");
        atomic_promote(&first, &final_output.join("01-first.m4b"))
            .await
            .expect("first promotion");

        let recovered = recover_promoted_export(
            &state,
            job_id,
            ExportLayout::PerChapter,
            &temporary,
            &final_output,
        )
        .await
        .expect("resume promotion");
        assert_eq!(recovered.len(), 2);
        assert_eq!(
            tokio::fs::read(final_output.join("01-first.m4b"))
                .await
                .unwrap(),
            b"first chapter"
        );
        assert_eq!(
            tokio::fs::read(final_output.join("02-second.m4b"))
                .await
                .unwrap(),
            b"second chapter"
        );
        assert!(!temporary.exists());
    }

    #[tokio::test]
    async fn split_export_recovery_rejects_a_foreign_existing_directory() {
        let (directory, state) = filesystem_test_state().await;
        let job_id = JobId::new();
        let output_directory = directory.path().join("exports");
        let staging = prepare_private_export_staging(&state, job_id)
            .await
            .expect("private export staging");
        let temporary = export_staging_output_path(&staging, ExportLayout::PerChapter, "m4b");
        let final_output = output_directory.join("book");
        tokio::fs::create_dir_all(&output_directory)
            .await
            .expect("public export directory");
        tokio::fs::create_dir_all(&temporary)
            .await
            .expect("temporary export directory");
        let staged = temporary.join("01-first.m4b");
        tokio::fs::write(&staged, b"job-owned chapter")
            .await
            .expect("staged export file");
        persist_export_promotion_marker(&state, job_id, &final_output, &[(staged.clone(), 1_000)])
            .await
            .expect("promotion marker");

        tokio::fs::create_dir(&final_output)
            .await
            .expect("foreign final export directory");
        let foreign = final_output.join("foreign.txt");
        tokio::fs::write(&foreign, b"foreign data")
            .await
            .expect("foreign directory contents");

        assert!(matches!(
            recover_promoted_export(
                &state,
                job_id,
                ExportLayout::PerChapter,
                &temporary,
                &final_output,
            )
            .await,
            Err(ServiceError::Conflict(_))
        ));
        assert_eq!(
            tokio::fs::read(&staged).await.unwrap(),
            b"job-owned chapter"
        );
        assert_eq!(tokio::fs::read(&foreign).await.unwrap(), b"foreign data");
        assert!(!final_output.join("01-first.m4b").exists());
    }

    #[tokio::test]
    async fn durable_file_sync_uses_platform_compatible_access() {
        let directory = tempfile::tempdir().expect("temporary directory");
        let path = directory.path().join("completed-output.m4b");
        tokio::fs::write(&path, b"complete output")
            .await
            .expect("completed output");

        sync_file(&path).await.expect("durable file sync");

        assert_eq!(tokio::fs::read(path).await.unwrap(), b"complete output");
    }

    #[tokio::test]
    async fn promotion_never_replaces_an_existing_destination() {
        let directory = tempfile::tempdir().expect("temporary directory");
        let source = directory.path().join("staged.m4b");
        let destination = directory.path().join("book.m4b");
        tokio::fs::write(&source, b"job-owned output")
            .await
            .unwrap();
        tokio::fs::write(&destination, b"foreign output")
            .await
            .unwrap();

        assert!(matches!(
            atomic_promote(&source, &destination).await,
            Err(ServiceError::Conflict(_))
        ));
        assert_eq!(
            tokio::fs::read(&destination).await.unwrap(),
            b"foreign output"
        );
        assert_eq!(tokio::fs::read(&source).await.unwrap(), b"job-owned output");
    }

    #[tokio::test]
    async fn permission_denied_is_a_conflict_when_the_no_clobber_destination_exists() {
        let directory = tempfile::tempdir().expect("temporary directory");
        let existing = directory.path().join("book.m4b");
        tokio::fs::write(&existing, b"foreign output")
            .await
            .unwrap();
        let permission_denied = std::io::Error::from(std::io::ErrorKind::PermissionDenied);

        assert!(
            failed_no_clobber_operation_is_conflict(&permission_denied, &existing).await,
            "Windows can report an existing no-clobber target as access denied"
        );
        assert!(
            !failed_no_clobber_operation_is_conflict(
                &permission_denied,
                &directory.path().join("available.m4b"),
            )
            .await,
            "an unrelated permission failure must remain an I/O error"
        );
        assert_eq!(tokio::fs::read(existing).await.unwrap(), b"foreign output");
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn managed_export_staging_ignores_a_hostile_legacy_public_symlink() {
        use std::os::unix::fs::symlink;

        let (directory, state) = filesystem_test_state().await;
        let output_directory = directory.path().join("public-exports");
        tokio::fs::create_dir_all(&output_directory).await.unwrap();
        let victim = directory.path().join("victim.txt");
        tokio::fs::write(&victim, b"must remain untouched")
            .await
            .unwrap();
        let job_id = JobId::new();
        let legacy_public_staging = output_directory.join(format!(".book-{job_id}.partial.m4b"));
        symlink(&victim, &legacy_public_staging).expect("hostile public staging symlink");

        let private_staging = prepare_private_export_staging(&state, job_id)
            .await
            .expect("managed staging directory");
        let managed_output =
            export_staging_output_path(&private_staging, ExportLayout::SingleFile, "m4b");
        write_job_staging_file_atomically(&private_staging, &managed_output, b"new export bytes")
            .await
            .expect("managed staging write");

        assert_eq!(
            tokio::fs::read(&victim).await.unwrap(),
            b"must remain untouched"
        );
        assert!(
            tokio::fs::symlink_metadata(&legacy_public_staging)
                .await
                .unwrap()
                .file_type()
                .is_symlink()
        );
        assert_eq!(
            tokio::fs::read(managed_output).await.unwrap(),
            b"new export bytes"
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn swapped_export_root_symlink_is_rejected_before_promotion() {
        use std::os::unix::fs::symlink;

        let (directory, state) = filesystem_test_state().await;
        let project_id = insert_recovery_project(&state, "Swapped export root").await;
        let output_directory = directory.path().join("reserved-output");
        let replacement = directory.path().join("foreign-output");
        tokio::fs::create_dir_all(&output_directory).await.unwrap();
        tokio::fs::create_dir_all(&replacement).await.unwrap();
        let output_directory = tokio::fs::canonicalize(&output_directory).await.unwrap();
        let profile = insert_recovery_export_profile(&state, project_id, &output_directory).await;
        let displaced = directory.path().join("displaced-output");
        tokio::fs::rename(&output_directory, &displaced)
            .await
            .unwrap();
        symlink(&replacement, &output_directory).expect("replacement output symlink");

        assert!(matches!(
            ensure_export_root_identity(&profile).await,
            Err(ServiceError::Conflict(_))
        ));
        assert!(!replacement.join("legacy-book.m4b").exists());
    }

    #[tokio::test]
    async fn failed_exclusive_copy_retains_its_partial_destination_fail_closed() {
        use std::{
            pin::Pin,
            task::{Context, Poll},
        };

        struct FailsAfterPrefix(bool);

        impl tokio::io::AsyncRead for FailsAfterPrefix {
            fn poll_read(
                mut self: Pin<&mut Self>,
                _context: &mut Context<'_>,
                buffer: &mut tokio::io::ReadBuf<'_>,
            ) -> Poll<std::io::Result<()>> {
                if self.0 {
                    Poll::Ready(Err(std::io::Error::other("injected copy failure")))
                } else {
                    buffer.put_slice(b"partial job output");
                    self.0 = true;
                    Poll::Ready(Ok(()))
                }
            }
        }

        let directory = tempfile::tempdir().expect("temporary directory");
        let destination = directory.path().join("book.m4b");
        let mut source = FailsAfterPrefix(false);

        assert!(matches!(
            copy_reader_no_clobber(&mut source, &destination).await,
            Err(ServiceError::Io(_))
        ));
        assert_eq!(
            tokio::fs::read(destination).await.unwrap(),
            b"partial job output",
            "a failed fallback must retain the exclusively-created path instead of deleting by name"
        );
    }

    #[tokio::test]
    async fn retry_replaces_only_job_owned_staging_auxiliary_files() {
        let directory = tempfile::tempdir().expect("temporary directory");
        let auxiliary = directory.path().join("..book-job.partial.m4b.ffmetadata");
        write_job_staging_file_atomically(directory.path(), &auxiliary, b"first metadata")
            .await
            .unwrap();
        write_job_staging_file_atomically(directory.path(), &auxiliary, b"retry metadata")
            .await
            .unwrap();
        assert_eq!(
            tokio::fs::read(&auxiliary).await.unwrap(),
            b"retry metadata"
        );

        let public = directory.path().join("book.m4b.manifest.json");
        write_file_atomically(&public, b"first public sidecar")
            .await
            .unwrap();
        assert!(
            write_file_atomically(&public, b"different public sidecar")
                .await
                .is_err()
        );
        assert_eq!(
            tokio::fs::read(public).await.unwrap(),
            b"first public sidecar"
        );
    }

    #[tokio::test]
    async fn provider_stream_sink_requires_order_and_a_final_chunk() {
        let request_id = Uuid::new_v4();
        let sink = ProviderStreamSink::new(request_id, AudioFormat::Wav, None);
        let out_of_order = sink
            .send(AudioChunk {
                request_id,
                sequence: 1,
                format: AudioFormat::Wav,
                sample_rate: None,
                channels: None,
                data: Bytes::from_static(b"late"),
                final_chunk: false,
            })
            .await;
        assert!(out_of_order.is_err());

        sink.send(AudioChunk {
            request_id,
            sequence: 0,
            format: AudioFormat::Wav,
            sample_rate: None,
            channels: None,
            data: Bytes::from_static(b"first"),
            final_chunk: false,
        })
        .await
        .expect("first chunk");
        sink.send(AudioChunk {
            request_id,
            sequence: 1,
            format: AudioFormat::Wav,
            sample_rate: None,
            channels: None,
            data: Bytes::from_static(b"second"),
            final_chunk: true,
        })
        .await
        .expect("final chunk");

        assert_eq!(
            sink.finish().await.expect("collected audio"),
            b"firstsecond"[..]
        );
    }

    #[tokio::test]
    async fn streaming_local_finalization_happens_after_billable_dispatch_success() {
        let request_id = Uuid::new_v4();
        let dispatch = ProviderSynthesisDispatch::Streaming {
            metadata: StreamingSynthesisResponse {
                content_type: "audio/wav".to_owned(),
                usage: ProviderUsage {
                    source: UsageSource::Reported,
                    characters: Some(17),
                    request_id: Some(request_id.to_string()),
                    ..ProviderUsage::default()
                },
            },
            // Deliberately omit the provider's final chunk. This is a local response-validation
            // failure after the provider returned successful billing metadata.
            sink: Arc::new(ProviderStreamSink::new(request_id, AudioFormat::Wav, None)),
            decoder_task: None,
            job_id: JobId::new(),
            playback_ordinal: 0,
        };

        assert_eq!(dispatch.usage().characters, Some(17));
        assert!(matches!(
            finish_provider_audio(dispatch).await,
            Err(ProviderError::InvalidResponse(_))
        ));
    }

    #[test]
    fn playback_coordinator_preserves_segment_order_and_resets_retries() {
        let job_id = JobId::new();
        prepare_playback(job_id, 0);
        let mut receiver = subscribe_playback(job_id.as_uuid());

        publish_playback_chunk(job_id, 1, Bytes::from_static(b"later"));
        complete_playback_segment(job_id, 1);
        assert!(matches!(
            receiver.try_recv(),
            Err(broadcast::error::TryRecvError::Empty)
        ));

        publish_playback_chunk(job_id, 0, Bytes::from_static(b"first"));
        assert!(matches!(
            receiver.try_recv(),
            Ok(PlaybackPacket::Audio(value)) if value == b"first"[..]
        ));
        reset_playback_segment(job_id, 0);
        assert!(matches!(receiver.try_recv(), Ok(PlaybackPacket::Reset)));

        publish_playback_chunk(job_id, 0, Bytes::from_static(b"retry"));
        complete_playback_segment(job_id, 0);
        assert!(matches!(
            receiver.try_recv(),
            Ok(PlaybackPacket::Audio(value)) if value == b"retry"[..]
        ));
        assert!(matches!(
            receiver.try_recv(),
            Ok(PlaybackPacket::Audio(value)) if value == b"later"[..]
        ));
    }

    #[test]
    fn parses_bounded_open_and_suffix_byte_ranges() {
        assert_eq!(
            parse_byte_range(&HeaderValue::from_static("bytes=0-9"), 100),
            Ok((0, 9))
        );
        assert_eq!(
            parse_byte_range(&HeaderValue::from_static("bytes=90-"), 100),
            Ok((90, 99))
        );
        assert_eq!(
            parse_byte_range(&HeaderValue::from_static("bytes=-10"), 100),
            Ok((90, 99))
        );
        assert_eq!(
            parse_byte_range(&HeaderValue::from_static("bytes=-200"), 100),
            Ok((0, 99))
        );
        assert_eq!(
            parse_byte_range(&HeaderValue::from_static("bytes=90-200"), 100),
            Ok((90, 99))
        );
    }

    #[test]
    fn rejects_unsatisfiable_or_multiple_byte_ranges() {
        for value in [
            "items=0-9",
            "bytes=",
            "bytes=-0",
            "bytes=100-",
            "bytes=20-10",
            "bytes=0-1,4-5",
        ] {
            assert!(
                parse_byte_range(&HeaderValue::from_str(value).expect("valid header"), 100)
                    .is_err(),
                "{value} should be rejected"
            );
        }
        assert!(
            parse_byte_range(&HeaderValue::from_static("bytes=0-0"), 0).is_err(),
            "an empty artifact cannot satisfy a range"
        );
    }

    #[test]
    fn sanitizes_export_file_components() {
        assert_eq!(safe_file_component("  A/B: C?.  "), "A_B_ C_");
        assert_eq!(safe_file_component(" . "), "Audiobook");
        assert_eq!(safe_file_component("\u{0000}\n"), "__");
        assert_eq!(safe_file_component(&"a".repeat(121)).chars().count(), 120);
    }

    #[test]
    fn export_parts_follow_manifest_order_and_retain_unlisted_fallbacks() {
        let now = Utc::now();
        let artifact = |id: u128, path: &str| Artifact {
            id: ArtifactId::from_uuid(Uuid::from_u128(id)),
            kind: ArtifactKind::Export,
            path: path.to_owned(),
            fingerprint: FileFingerprint {
                algorithm: "blake3".to_owned(),
                digest: "00".repeat(32),
                size_bytes: 1,
            },
            media_type: Some("audio/mp4".to_owned()),
            duration_ms: Some(1_000),
            cache_key: None,
            pinned_by_job_id: Some(JobId::from_uuid(Uuid::from_u128(99))),
            created_at: now,
            last_accessed_at: now,
        };
        let first = artifact(1, "/exports/part-1.m4b");
        let second = artifact(2, "/exports/part-2.m4b");
        let fallback = artifact(3, "/exports/recovered.m4b");
        let ordered = canonical_export_ids(
            &[first.clone(), second.clone(), fallback.clone()],
            &[second.path.clone(), first.path.clone()],
        );
        assert_eq!(ordered, [second.id, first.id, fallback.id]);
    }

    #[test]
    fn pronunciation_rules_are_scoped_and_deterministic() {
        let project_id = Uuid::from_u128(1);
        let character_id = Uuid::from_u128(2);
        let other_character = Uuid::from_u128(3);
        let global_id = Uuid::from_u128(10);
        let project_rule_id = Uuid::from_u128(11);
        let rules = vec![
            PronunciationRuleView {
                id: project_rule_id,
                scope: PronunciationScopeView::Project,
                kind: PronunciationKindView::WholeWord,
                source: "Mister".to_owned(),
                replacement: "Doctor".to_owned(),
                language: Some("en".to_owned()),
                character_id: Some(character_id),
                case_sensitive: false,
                enabled: true,
                order: 1,
                conflict: None,
                project_id: Some(project_id),
            },
            PronunciationRuleView {
                id: global_id,
                scope: PronunciationScopeView::Global,
                kind: PronunciationKindView::Literal,
                source: "Mr.".to_owned(),
                replacement: "Mister".to_owned(),
                language: None,
                character_id: None,
                case_sensitive: true,
                enabled: true,
                order: 99,
                conflict: None,
                project_id: None,
            },
            PronunciationRuleView {
                id: Uuid::from_u128(12),
                scope: PronunciationScopeView::Global,
                kind: PronunciationKindView::Literal,
                source: "ignored".to_owned(),
                replacement: "applied".to_owned(),
                language: None,
                character_id: Some(other_character),
                case_sensitive: true,
                enabled: true,
                order: 0,
                conflict: None,
                project_id: None,
            },
        ];

        let (text, applied, revision) = apply_pronunciation_rules(
            "Mr. Smith ignored this.",
            &rules,
            project_id,
            character_id,
            Some("EN"),
        )
        .expect("valid rules");

        assert_eq!(text, "Doctor Smith ignored this.");
        assert_eq!(applied, vec![global_id, project_rule_id]);
        assert_eq!(revision.len(), 64);
    }

    #[test]
    fn synthesis_identity_tracks_performance_but_not_local_timing() {
        let mut segment = test_segment_plan();
        let semantic = segment_semantic_input_hash(&segment).expect("semantic hash");
        let cache_key = segment_cache_fingerprint(&segment, "conversion")
            .key()
            .expect("cache key");

        segment.assignment.timing.pause_after_ms = Some(750);
        assert_eq!(
            segment_semantic_input_hash(&segment).expect("timing semantic hash"),
            semantic
        );
        assert_eq!(
            segment_cache_fingerprint(&segment, "conversion")
                .key()
                .expect("timing cache key"),
            cache_key
        );

        segment.assignment.performance.speed = Some(1.1);
        assert_ne!(
            segment_semantic_input_hash(&segment).expect("performance semantic hash"),
            semantic
        );
        assert_ne!(
            segment_cache_fingerprint(&segment, "conversion")
                .key()
                .expect("performance cache key"),
            cache_key
        );
    }

    #[test]
    fn durable_conversion_snapshot_rejects_same_key_narration_or_model_drift() {
        let snapshot = test_segment_plan();
        let now = Utc::now();
        let unit = JobUnit {
            id: JobUnitId::new(),
            job_id: JobId::new(),
            kind: JobUnitKind::SynthesisSegment,
            state: JobUnitState::Failed,
            chapter_id: Some(ChapterId::from_uuid(snapshot.chapter_id)),
            segment_id: None,
            provider_profile_id: Some(ProviderProfileId::from_uuid(
                snapshot.assignment.provider_id,
            )),
            dependencies: Vec::new(),
            attempt_count: 1,
            next_attempt_at: None,
            output_artifact_id: None,
            payload: BTreeMap::from([
                (
                    "segmentKey".to_owned(),
                    serde_json::json!(snapshot.key.clone()),
                ),
                ("cacheOperation".to_owned(), serde_json::json!("conversion")),
                (
                    "segmentPlan".to_owned(),
                    serde_json::to_value(&snapshot).unwrap(),
                ),
            ]),
            created_at: now,
            updated_at: now,
        };
        validate_durable_segment_snapshot(&unit, &snapshot).expect("unchanged snapshot");

        let mut changed_text = snapshot.clone();
        changed_text.text = "Changed narration".to_owned();
        assert!(matches!(
            validate_durable_segment_snapshot(&unit, &changed_text),
            Err(ServiceError::Conflict(detail)) if detail.contains("start a new conversion")
        ));

        let mut changed_model = snapshot;
        changed_model.assignment.model = Some("changed-model".to_owned());
        assert!(matches!(
            validate_durable_segment_snapshot(&unit, &changed_model),
            Err(ServiceError::Conflict(detail)) if detail.contains("start a new conversion")
        ));

        let mut changed_routing = test_segment_plan();
        changed_routing.assignment.provider_endpoint =
            Some("https://different-provider.example/v1".to_owned());
        assert!(matches!(
            validate_durable_segment_snapshot(&unit, &changed_routing),
            Err(ServiceError::Conflict(detail)) if detail.contains("start a new conversion")
        ));
    }

    #[test]
    fn regeneration_reservation_multiplier_follows_uncertain_charge_policy() {
        let base =
            RetryPolicy::new(4, Duration::from_millis(1), Duration::from_millis(10)).unwrap();
        assert_eq!(
            retry_reservation_multiplier(&base.clone().with_uncertain_charge_retries(false)),
            1
        );
        assert_eq!(
            retry_reservation_multiplier(&base.with_uncertain_charge_retries(true)),
            4
        );
    }

    #[test]
    fn regeneration_runtime_mode_must_match_the_durable_snapshot() {
        assert!(provider_mode_matches_runtime(
            ProviderModeView::CloudRemote,
            audiobookai_providers::ProviderKind::CloudRemote,
        ));
        assert!(provider_mode_matches_runtime(
            ProviderModeView::ExternalEndpoint,
            audiobookai_providers::ProviderKind::ExternalEndpoint,
        ));
        assert!(provider_mode_matches_runtime(
            ProviderModeView::ManagedChild,
            audiobookai_providers::ProviderKind::ManagedChild,
        ));
        assert!(provider_mode_matches_runtime(
            ProviderModeView::Native,
            audiobookai_providers::ProviderKind::Native,
        ));
        assert!(!provider_mode_matches_runtime(
            ProviderModeView::CloudRemote,
            audiobookai_providers::ProviderKind::ExternalEndpoint,
        ));
    }

    #[test]
    fn regeneration_retry_requires_an_exact_persisted_provider_identity() {
        assert!(persisted_provider_mode_matches(
            Some(ProviderModeView::CloudRemote),
            ProviderModeView::CloudRemote,
        ));
        assert!(!persisted_provider_mode_matches(
            None,
            ProviderModeView::CloudRemote,
        ));
        assert!(!persisted_provider_mode_matches(
            Some(ProviderModeView::ExternalEndpoint),
            ProviderModeView::CloudRemote,
        ));

        let snapshot = Uuid::new_v4();
        assert!(persisted_provider_snapshot_matches(
            Some(snapshot),
            Some(snapshot)
        ));
        assert!(!persisted_provider_snapshot_matches(None, Some(snapshot)));
        assert!(!persisted_provider_snapshot_matches(Some(snapshot), None));
        assert!(!persisted_provider_snapshot_matches(
            Some(snapshot),
            Some(Uuid::new_v4())
        ));
    }
}
