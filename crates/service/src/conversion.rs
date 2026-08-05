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
    ChapterId, DialogueSpan, DuckingSettings, ExportFormat, ExportLayout, ExportProfile,
    ExportProfileId, FileFingerprint, Job, JobAttempt, JobId, JobKind, JobState, JobUnit,
    JobUnitId, JobUnitKind, JobUnitState, Paragraph, Project, ProjectId, ProvenanceQuality,
    ProviderProfileId, RateCardId, ReservationId, Speaker, SpeakerOverride, UsageEvent,
    UsageEventId, UsageQuantities, UsageWorkload, Validate, VoiceProfileId,
};
use audiobookai_media::{
    BackgroundMusic, BookMetadata as MediaBookMetadata, CacheFingerprint, ChapterAudio,
    ContentAddressedCache, ExportFormat as MediaExportFormat, ExportPlanner, ExportRequest,
    LoudnessMeasurement, LoudnessSettings, SidecarPair, SidecarResolver,
    parse_loudness_measurement,
};
use audiobookai_providers::{
    AudioChunk, AudioChunkSink, AudioFormat, CancellationFlag, ProviderError, ProviderId,
    ProviderUsage, SynthesisRequest, SynthesisResponse, TtsProvider, UsageSource,
};
use axum::{
    body::Body,
    http::{HeaderMap, HeaderValue, StatusCode, header},
    response::{IntoResponse, Response},
};
use bytes::{Bytes, BytesMut};
use chrono::Utc;
use futures::{StreamExt, TryStreamExt, future::BoxFuture, stream};
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
        ProviderModeView, ProviderProfileView, StartJobInput, UsageRowView,
    },
    runtime::{
        FailureClass as RetryFailureClass, RetryEvent, RetryEventOutcome, RetryJournal,
        RetryJournalError, RetryPolicy, execute_with_retry,
    },
};

const NORMALIZATION_VERSION: &str = "48k-flac-segment-v1";
const MAX_PREVIEW_CHARACTERS: usize = 500;
const RANGE_CHUNK_BYTES: usize = 64 * 1024;

type ProviderSemaphoreRegistry = HashMap<Uuid, (u16, Arc<Semaphore>)>;

static PROVIDER_SEMAPHORES: OnceLock<StdMutex<ProviderSemaphoreRegistry>> = OnceLock::new();
static PLAYBACK_HUBS: OnceLock<StdMutex<HashMap<Uuid, Arc<PlaybackHub>>>> = OnceLock::new();
static ACTIVE_WORKERS: OnceLock<StdMutex<BTreeSet<Uuid>>> = OnceLock::new();

#[derive(Clone, Debug)]
struct SpeakerAssignment {
    character_id: Uuid,
    character_name: String,
    provider_id: Uuid,
    provider_name: String,
    provider_kind: ProviderKindView,
    provider_endpoint: Option<String>,
    provider_version: Option<String>,
    provider_concurrency: u16,
    voice_id: Uuid,
    voice_source: String,
    voice_name: String,
    model: Option<String>,
}

#[derive(Clone, Debug)]
struct SegmentPlan {
    key: String,
    chapter_id: Uuid,
    chapter_title: String,
    segment_ordinal: u32,
    playback_ordinal: usize,
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

#[derive(Clone, Debug)]
struct ChapterPlan {
    chapter: Chapter,
    segments: Vec<SegmentPlan>,
}

#[derive(Clone, Debug)]
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

/// Creates a durable conversion and starts its in-process worker.
pub async fn start_conversion(
    state: Arc<AppState>,
    input: StartJobInput,
) -> Result<JobView, ServiceError> {
    validate_export_input(&input.export)?;
    let _shutdown_admission = state.admit_shutdown_sensitive_work().await?;
    let job_id = JobId::new();
    let (export_profile, music_path) =
        create_export_profile(&state, job_id, input.project_id, &input.export).await?;
    let plan = load_conversion_plan(&state, input.project_id, export_profile, music_path).await?;
    ensure_output_is_available(&plan.export).await?;
    let units = build_job_units(job_id, &plan);
    let now = Utc::now();
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
        .insert(&job)
        .await
        .map_err(storage_error)?;
    persist_units(&state, &units).await?;

    match reserve_job_budgets(&state, &job, &plan).await {
        Ok(reservation_id) => {
            if let Some(reservation_id) = reservation_id {
                let expected = job.revision;
                job.reservation_id = Some(reservation_id);
                job.updated_at = Utc::now();
                job = state
                    .database
                    .repositories()
                    .jobs
                    .update(&job, expected)
                    .await
                    .map_err(storage_error)?;
            }
        }
        Err(error) => {
            mark_domain_job_failed(&state, job_id, &error.to_string()).await;
            return Err(error);
        }
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
    tokio::spawn(run_conversion_job(Arc::clone(&state), job_id));
    Ok(view)
}

/// Restarts conversion workers after an application restart without duplicating completed units.
pub async fn resume_durable_conversions(state: Arc<AppState>) -> Result<(), ServiceError> {
    let active = state
        .database
        .repositories()
        .jobs
        .list_active()
        .await
        .map_err(storage_error)?;
    for job in active {
        if job.kind != JobKind::Conversion {
            continue;
        }
        match job.state {
            JobState::Queued | JobState::Running => {
                tokio::spawn(run_conversion_job(Arc::clone(&state), job.id));
            }
            JobState::Pausing => {
                let _ = transition_job(&state, job.id, JobState::Paused, "Paused").await;
            }
            JobState::Cancelling => {
                let _ = transition_job(&state, job.id, JobState::Cancelled, "Cancelled").await;
            }
            JobState::Paused | JobState::Cancelled | JobState::Failed | JobState::Completed => {}
        }
    }
    Ok(())
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
        if !matches!(job.kind, JobKind::Conversion | JobKind::CharacterDetection) {
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
        .find(|character| character.canonical_name.eq_ignore_ascii_case("narrator"))
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

async fn build_assignments(
    project: &Project,
    characters: &[crate::models::CharacterView],
    voice_sources: &HashMap<Uuid, String>,
    providers: &HashMap<Uuid, ProviderProfileView>,
    state: &AppState,
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
        if !provider
            .capabilities
            .as_ref()
            .is_some_and(|capabilities| capabilities.tts)
        {
            return Err(ServiceError::Conflict(format!(
                "the provider assigned to '{}' has no verified TTS capability",
                character.canonical_name
            )));
        }
        if matches!(provider.mode, ProviderModeView::CloudRemote)
            && !project.cloud_consent.book_text
        {
            return Err(ServiceError::Conflict(format!(
                "grant project consent before sending book text to {}",
                provider.name
            )));
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
        result.insert(
            character.id,
            SpeakerAssignment {
                character_id: character.id,
                character_name: character.canonical_name.clone(),
                provider_id: assignment.provider_profile_id,
                provider_name: provider.name.clone(),
                provider_kind: provider.kind.clone(),
                provider_endpoint: provider.endpoint.clone(),
                provider_version,
                provider_concurrency: concurrency,
                voice_id: assignment.voice_id,
                voice_source,
                voice_name: assignment.voice_name.clone(),
                model: assignment.model.clone().or_else(|| provider.model.clone()),
            },
        );
    }
    Ok(result)
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
            let text = text.trim();
            if text.is_empty() {
                continue;
            }
            let assignment = assignments.get(&character_id).cloned().ok_or_else(|| {
                ServiceError::Conflict(format!(
                    "a speaker in '{}' has no valid voice assignment",
                    chapter.title
                ))
            })?;
            let (text, applied_rule_ids, dictionary_revision) = apply_pronunciation_rules(
                text,
                rules,
                project.id.as_uuid(),
                character_id,
                project.metadata.language.as_deref(),
            )?;
            let segment_ordinal = u32::try_from(output.len()).unwrap_or(u32::MAX);
            let key = segment_key(chapter.id.as_uuid(), paragraph_id, start, end, character_id);
            output.push(SegmentPlan {
                key,
                chapter_id: chapter.id.as_uuid(),
                chapter_title: chapter.title.clone(),
                segment_ordinal,
                playback_ordinal: 0,
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

fn apply_pronunciation_rules(
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
            let unit = JobUnit {
                id: JobUnitId::new(),
                job_id,
                kind: JobUnitKind::SynthesisSegment,
                state: JobUnitState::Ready,
                chapter_id: Some(ChapterId::from_uuid(segment.chapter_id)),
                segment_id: None,
                provider_profile_id: Some(ProviderProfileId::from_uuid(
                    segment.assignment.provider_id,
                )),
                dependencies: Vec::new(),
                attempt_count: 0,
                next_attempt_at: None,
                output_artifact_id: None,
                payload: BTreeMap::from([
                    ("segmentKey".to_owned(), serde_json::json!(segment.key)),
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

async fn persist_units(state: &AppState, units: &PersistedUnitPlan) -> Result<(), ServiceError> {
    for unit in units.synthesis.values() {
        state
            .database
            .repositories()
            .jobs
            .upsert_unit(unit)
            .await
            .map_err(storage_error)?;
    }
    for unit in units.assembly.values() {
        state
            .database
            .repositories()
            .jobs
            .upsert_unit(unit)
            .await
            .map_err(storage_error)?;
    }
    if let Some(unit) = &units.mix {
        state
            .database
            .repositories()
            .jobs
            .upsert_unit(unit)
            .await
            .map_err(storage_error)?;
    }
    state
        .database
        .repositories()
        .jobs
        .upsert_unit(&units.normalize)
        .await
        .map_err(storage_error)?;
    state
        .database
        .repositories()
        .jobs
        .upsert_unit(&units.export)
        .await
        .map_err(storage_error)
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
        let units = build_job_units(job_id, plan);
        persist_units(state, &units).await?;
        return Ok(units);
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
            JobUnitKind::DetectionBatch => {}
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
    Ok(PersistedUnitPlan {
        synthesis,
        assembly,
        mix,
        normalize: normalize.expect("checked above"),
        export: export.expect("checked above"),
    })
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
    });
    JobView {
        id: job.id.as_uuid(),
        project_id: job.project_id.as_uuid(),
        project_title: title.to_owned(),
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
        },
        status: unit_status_view(unit.state),
        progress: unit
            .payload
            .get("progress")
            .and_then(serde_json::Value::as_f64)
            .map_or(0.0, unit_interval_f32),
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
        JobState::Pausing | JobState::Cancelling => JobStatusView::Pausing,
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
        f32::from(basis_points) / 10_000.0
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

async fn run_conversion_job(state: Arc<AppState>, job_id: JobId) {
    let workers = ACTIVE_WORKERS.get_or_init(|| StdMutex::new(BTreeSet::new()));
    {
        let mut workers = workers
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if !workers.insert(job_id.as_uuid()) {
            return;
        }
    }
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
    workers
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .remove(&job_id.as_uuid());
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
    let plan = load_conversion_plan(state, job.project_id.as_uuid(), export, music_path).await?;
    let mut units = load_unit_plan(state, job_id, &plan).await?;
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
        async move {
            assemble_chapter(
                &state,
                job_id,
                chapter,
                unit,
                artifacts,
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
    let completed =
        transition_job(state, job_id, JobState::Completed, "Conversion complete").await?;
    if let Err(error) = release_job_cache_pins(state, job_id).await {
        tracing::warn!(diagnostic_code = "conversion.cache.unpin.failed", %job_id, %error, "could not release completed job cache pins");
    }
    let cache_limit = state.catalog.read().await.settings.cache_limit_bytes;
    if let Err(error) = enforce_cache_limit(state, cache_limit).await {
        tracing::warn!(diagnostic_code = "conversion.cache.prune.failed", %job_id, %error, "could not enforce the cache limit after conversion");
    }
    update_export_catalog(state, &plan, &export_artifacts, manifest_artifact.id).await?;
    {
        let mut catalog = state.catalog.write().await;
        if let Some(project) = catalog.projects.get_mut(&plan.project.id.as_uuid()) {
            project.summary.status = ProjectDisplayStatus::Completed;
            project.summary.progress = 1.0;
        }
        if let Some(view) = catalog.jobs.get_mut(&job_id.as_uuid()) {
            view.status = JobStatusView::Complete;
            view.progress = 1.0;
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

async fn synthesize_provider_audio(
    provider: Arc<dyn TtsProvider>,
    request: SynthesisRequest,
    sidecars: &SidecarPair,
    job_id: JobId,
    playback_ordinal: usize,
) -> Result<StreamedSynthesis, ProviderError> {
    if !provider.capabilities().streaming {
        return provider
            .synthesize(request)
            .await
            .map(|response| StreamedSynthesis {
                response,
                progressive_decode_complete: false,
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
    let fingerprint = segment_cache_fingerprint(&segment, "conversion");
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
            async move {
                crate::accounting::verify_dispatch_is_reserved(&state, job_id, &dispatch_estimate)
                    .await
                    .map_err(|_| {
                        ProviderError::Configuration(
                            "the active hard-budget reservation does not permit this dispatch"
                                .to_owned(),
                        )
                    })?;
                synthesize_provider_audio(provider, request, &sidecars, job_id, playback_ordinal)
                    .await
            }
        })
        .await
        .map_err(|error| retry_service_error(state, job_id, &segment, &error))?;
        unit.attempt_count = execution.attempts.get();
        let successful_attempt_id =
            attempt_id_for_ordinal(state, unit.id, execution.attempts.get()).await?;
        let streamed = execution.value;
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
        let mut usage = response.usage.clone();
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
        (artifact, streamed.progressive_decode_complete)
    };
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
        |_| state.config.data_dir.join("cache"),
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
        settings,
        dictionary_revision: segment.dictionary_revision.clone(),
        normalization_version: NORMALIZATION_VERSION.to_owned(),
    }
}

fn provider_endpoint_family(assignment: &SpeakerAssignment) -> &'static str {
    match assignment.provider_kind {
        ProviderKindView::Elevenlabs => "elevenlabs-v1",
        ProviderKindView::MlxAudio => "openai-audio-mlx",
        ProviderKindView::Localai => "openai-audio-localai",
        ProviderKindView::AlltalkV2 => "alltalk-v2",
        ProviderKindView::NativeOs => "native-os",
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
        ProviderKindView::Elevenlabs => AudioFormat::Mp3,
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
        .map_err(ServiceError::Io)?;
    tokio::fs::write(input.path(), &response.audio).await?;
    let output = tempfile::Builder::new()
        .prefix("audiobookai-normalized-")
        .suffix(".flac")
        .tempfile()
        .map_err(ServiceError::Io)?;
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
        input.path().to_string_lossy().into_owned(),
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
        output.path().to_string_lossy().into_owned(),
    ];
    run_process(&sidecars.ffmpeg, &arguments, "normalize provider audio").await?;
    let bytes = tokio::fs::read(output.path()).await?;
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
        segment_id: None,
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

async fn assemble_chapter(
    state: &Arc<AppState>,
    job_id: JobId,
    chapter: ChapterPlan,
    mut unit: JobUnit,
    mut segments: Vec<SegmentArtifact>,
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
        .map_err(ServiceError::Io)?;
    let mut arguments = vec![
        "-hide_banner".to_owned(),
        "-nostdin".to_owned(),
        "-y".to_owned(),
    ];
    for segment in &segments {
        arguments.extend(["-i".to_owned(), segment.artifact.path.clone()]);
    }
    let mut filter = String::new();
    for index in 0..segments.len() {
        write!(
            filter,
            "[{index}:a]aresample=48000:async=1:first_pts=0[a{index}];"
        )
        .expect("writing to String cannot fail");
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
        temporary.path().to_string_lossy().into_owned(),
    ]);
    run_process(&sidecars.ffmpeg, &arguments, "assemble chapter").await?;
    validate_flac(temporary.path()).await?;
    atomic_promote(temporary.path(), &destination).await?;
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
    tokio::fs::create_dir_all(&output_directory).await?;
    let extension = media_export_format(plan.export.format).extension();
    let final_output = if plan.export.layout == ExportLayout::PerChapter {
        output_directory.join(&plan.export.filename_template)
    } else {
        output_directory.join(format!("{}.{}", plan.export.filename_template, extension))
    };
    if final_output.exists() {
        return Err(ServiceError::Conflict(format!(
            "export destination already exists: {}",
            final_output.display()
        )));
    }
    let temporary_output = if plan.export.layout == ExportLayout::PerChapter {
        output_directory.join(format!(
            ".{}-{}.partial",
            plan.export.filename_template, job_id
        ))
    } else {
        output_directory.join(format!(
            ".{}-{}.partial.{}",
            plan.export.filename_template, job_id, extension
        ))
    };
    if plan.export.layout == ExportLayout::PerChapter {
        tokio::fs::create_dir_all(&temporary_output).await?;
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
        write_file_atomically(&auxiliary.path, auxiliary.contents.as_bytes()).await?;
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

    let final_paths = if plan.export.layout == ExportLayout::PerChapter {
        tokio::fs::create_dir(&final_output).await?;
        let mut final_paths = Vec::new();
        for (path, duration) in rendered {
            let file_name = path.file_name().ok_or_else(|| {
                ServiceError::Internal("split export has no file name".to_owned())
            })?;
            let destination = final_output.join(file_name);
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
        atomic_promote(&temporary_output, &final_output).await?;
        vec![(final_output.clone(), duration)]
    };

    let ffmpeg_build = ffmpeg_build_description(sidecars).await?;
    let manifest_value =
        export_manifest_value(state, job_id, plan, chapters, &final_paths, &ffmpeg_build).await?;
    let manifest_path = if plan.export.layout == ExportLayout::PerChapter {
        final_output.join("audiobookai-export-manifest.json")
    } else {
        final_output.with_file_name(format!(
            "{}.manifest.json",
            final_output
                .file_name()
                .and_then(std::ffi::OsStr::to_str)
                .unwrap_or("audiobook")
        ))
    };
    write_file_atomically(
        &manifest_path,
        &serde_json::to_vec_pretty(&manifest_value).map_err(internal_error)?,
    )
    .await?;
    let manifest_artifact = artifact_for_file(
        ArtifactKind::ExportManifest,
        &manifest_path,
        Some("application/json".to_owned()),
        None,
        None,
        Some(job_id),
    )
    .await?;
    persist_artifact(state, plan.project.id.as_uuid(), &manifest_artifact).await?;
    let mut artifacts = Vec::new();
    for (path, duration) in final_paths {
        let artifact = artifact_for_file(
            ArtifactKind::Export,
            &path,
            Some(media_type_for_path(&path)),
            Some(duration),
            None,
            Some(job_id),
        )
        .await?;
        persist_artifact(state, plan.project.id.as_uuid(), &artifact).await?;
        artifacts.push(artifact);
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
    Ok(serde_json::json!({
        "schemaVersion": 1,
        "projectId": plan.project.id,
        "jobId": job_id,
        "createdAt": Utc::now(),
        "source": plan.book.source_fingerprint,
        "metadata": plan.project.metadata,
        "outputFormat": format_name(plan.export.format),
        "layout": layout_name(plan.export.layout),
        "outputFiles": final_paths.iter().map(|(path, _)| path.to_string_lossy().into_owned()).collect::<Vec<_>>(),
        "chapterMarkers": chapter_markers,
        "voiceProvenance": voices.into_values().collect::<Vec<_>>(),
        "dictionaryRevisions": dictionary_revisions,
        "audio": plan.export.audio,
        "ffmpegBuild": ffmpeg_build,
        "usageTotals": usage,
    }))
}

async fn update_export_catalog(
    state: &AppState,
    plan: &ConversionPlan,
    artifacts: &[Artifact],
    manifest_id: ArtifactId,
) -> Result<(), ServiceError> {
    let mut views = Vec::new();
    for artifact in artifacts {
        let path = Path::new(&artifact.path);
        let size = tokio::fs::metadata(path).await?.len();
        views.push(ExportArtifactView {
            id: artifact.id.as_uuid(),
            project_id: plan.project.id.as_uuid(),
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
        .map_err(ServiceError::Io)?;
    tokio::fs::copy(source, temporary.path()).await?;
    sync_file(temporary.path()).await?;
    atomic_promote(temporary.path(), destination).await
}

async fn write_file_atomically(destination: &Path, bytes: &[u8]) -> Result<(), ServiceError> {
    let parent = destination.parent().ok_or_else(|| {
        ServiceError::InvalidRequest("destination has no parent directory".to_owned())
    })?;
    tokio::fs::create_dir_all(parent).await?;
    let temporary = tempfile::Builder::new()
        .prefix(".audiobookai-write-")
        .tempfile_in(parent)
        .map_err(ServiceError::Io)?;
    tokio::fs::write(temporary.path(), bytes).await?;
    sync_file(temporary.path()).await?;
    atomic_promote(temporary.path(), destination).await
}

async fn sync_file(path: &Path) -> Result<(), ServiceError> {
    tokio::fs::OpenOptions::new()
        .read(true)
        .open(path)
        .await?
        .sync_all()
        .await?;
    Ok(())
}

async fn atomic_promote(source: &Path, destination: &Path) -> Result<(), ServiceError> {
    if destination.exists() {
        return Err(ServiceError::Conflict(format!(
            "refusing to overwrite {}",
            destination.display()
        )));
    }
    tokio::fs::rename(source, destination).await?;
    #[cfg(unix)]
    if let Some(parent) = destination.parent() {
        tokio::fs::File::open(parent).await?.sync_all().await?;
    }
    Ok(())
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

async fn ensure_output_is_available(profile: &ExportProfile) -> Result<(), ServiceError> {
    let root = PathBuf::from(&profile.output_directory);
    tokio::fs::create_dir_all(&root).await?;
    let destination = if profile.layout == ExportLayout::PerChapter {
        root.join(&profile.filename_template)
    } else {
        root.join(format!(
            "{}.{}",
            profile.filename_template,
            media_export_format(profile.format).extension()
        ))
    };
    if destination.exists() {
        return Err(ServiceError::Conflict(format!(
            "export destination already exists: {}",
            destination.display()
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
    Ok(())
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
    let job = repository
        .update(&job, expected)
        .await
        .map_err(storage_error)?;
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
        let _ = repository.update(&job, expected).await;
    }
}

async fn mark_job_failed(state: &AppState, job_id: JobId, message: &str) {
    mark_domain_job_failed(state, job_id, message).await;
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
    let sidecars = resolve_sidecars(&state)?;
    let project = state
        .database
        .repositories()
        .projects
        .get_project(ProjectId::from_uuid(project_id))
        .await
        .map_err(storage_error)?
        .ok_or(ServiceError::NotFound)?;
    let (narrator, voices, providers, rules) = {
        let catalog = state.catalog.read().await;
        let narrator = catalog
            .characters
            .get(&project_id)
            .and_then(|characters| {
                characters
                    .iter()
                    .find(|character| character.canonical_name.eq_ignore_ascii_case("narrator"))
            })
            .cloned()
            .ok_or_else(|| {
                ServiceError::Conflict(
                    "detect characters and assign the narrator voice before previewing".to_owned(),
                )
            })?;
        (
            narrator,
            catalog.voice_sources.clone(),
            catalog.providers.clone(),
            catalog.pronunciation_rules.clone(),
        )
    };
    let assignments = build_assignments(
        &project,
        std::slice::from_ref(&narrator),
        &voices,
        &providers,
        &state,
    )
    .await?;
    let assignment = assignments
        .get(&narrator.id)
        .cloned()
        .ok_or_else(|| ServiceError::Conflict("the narrator has no voice".to_owned()))?;
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
        narrator.id,
        project.metadata.language.as_deref(),
    )?;
    let segment = SegmentPlan {
        key: segment_key(
            chapter.id.as_uuid(),
            paragraph.id.as_uuid(),
            0,
            original.len(),
            narrator.id,
        ),
        chapter_id: chapter.id.as_uuid(),
        chapter_title: chapter.title,
        segment_ordinal: 0,
        playback_ordinal: 0,
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
    state
        .database
        .repositories()
        .jobs
        .insert(&job)
        .await
        .map_err(storage_error)?;
    let mut unit = JobUnit {
        id: JobUnitId::new(),
        job_id: job.id,
        kind: JobUnitKind::SynthesisSegment,
        state: JobUnitState::Running,
        chapter_id: Some(chapter.id),
        segment_id: None,
        provider_profile_id: Some(ProviderProfileId::from_uuid(segment.assignment.provider_id)),
        dependencies: Vec::new(),
        attempt_count: 0,
        next_attempt_at: None,
        output_artifact_id: None,
        payload: BTreeMap::from([(
            "title".to_owned(),
            serde_json::json!("Billable narrator preview"),
        )]),
        created_at: now,
        updated_at: now,
    };
    state
        .database
        .repositories()
        .jobs
        .upsert_unit(&unit)
        .await
        .map_err(storage_error)?;
    let policy = retry_policy(&state, &segment).await?;
    let request_character_count = u64::try_from(segment.text.chars().count()).unwrap_or(u64::MAX);
    let reservation_multiplier = if policy.retries_uncertain_charge() {
        usize::from(policy.max_attempts())
    } else {
        1
    };
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
    match crate::accounting::reserve_for_estimates(&state, &job, &reservation_estimates).await {
        Ok(Some(reservation_id)) => {
            let expected = job.revision;
            job.reservation_id = Some(reservation_id);
            job.updated_at = Utc::now();
            job = state
                .database
                .repositories()
                .jobs
                .update(&job, expected)
                .await
                .map_err(storage_error)?;
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
        let execution = execute_with_retry(&policy, &journal, |_| {
            let state = Arc::clone(&state);
            let provider = Arc::clone(&provider);
            let request = request.clone();
            let dispatch_estimate = dispatch_estimate.clone();
            async move {
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
    let job = state
        .database
        .repositories()
        .jobs
        .get(id)
        .await
        .map_err(storage_error)?
        .ok_or(ServiceError::NotFound)?;
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
            transition_job(&state, id, JobState::Running, "Resuming job").await?;
            match job.kind {
                JobKind::Conversion => {
                    tokio::spawn(run_conversion_job(Arc::clone(&state), id));
                }
                JobKind::CharacterDetection => {
                    crate::workflows::reset_detection_units_for_restart(&state, id, false).await?;
                    crate::workflows::spawn_character_detection(Arc::clone(&state), id.as_uuid());
                }
                JobKind::Preview | JobKind::Export | JobKind::CacheCleanup => {}
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
                JobKind::Conversion => {
                    tokio::spawn(run_conversion_job(Arc::clone(&state), id));
                }
                JobKind::CharacterDetection => {
                    crate::workflows::spawn_character_detection(Arc::clone(&state), id.as_uuid());
                }
                JobKind::Preview | JobKind::Export | JobKind::CacheCleanup => {}
            }
        }
        ("retry", JobState::Failed) => {
            transition_job(&state, id, JobState::Queued, "Queued for retry").await?;
            if job.kind == JobKind::CharacterDetection {
                crate::workflows::reset_detection_units_for_restart(&state, id, true).await?;
            } else {
                for mut unit in state
                    .database
                    .repositories()
                    .jobs
                    .list_units(id)
                    .await
                    .map_err(storage_error)?
                {
                    if unit.state == JobUnitState::Failed {
                        let next = if unit.dependencies.is_empty() {
                            JobUnitState::Ready
                        } else {
                            JobUnitState::Blocked
                        };
                        update_unit_state(&state, &mut unit, next, None).await?;
                    }
                }
            }
            match job.kind {
                JobKind::Conversion => {
                    tokio::spawn(run_conversion_job(Arc::clone(&state), id));
                }
                JobKind::CharacterDetection => {
                    crate::workflows::spawn_character_detection(Arc::clone(&state), id.as_uuid());
                }
                JobKind::Preview | JobKind::Export | JobKind::CacheCleanup => {}
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
        let Some(profile_id) = row.get::<Option<String>, _>("export_profile_id") else {
            continue;
        };
        let Ok(profile_id) = ExportProfileId::from_str(&profile_id) else {
            continue;
        };
        let profile = load_export_profile(state, profile_id).await?;
        let manifest_id = if let Some(job_id) = row.get::<Option<String>, _>("pinned_by_job_id") {
            sqlx::query_scalar::<_, String>(
                "SELECT id FROM artifacts WHERE pinned_by_job_id = ? AND kind = 'export_manifest' LIMIT 1",
            )
            .bind(job_id)
            .fetch_optional(state.database.pool())
            .await
            .map_err(storage_error)?
            .and_then(|value| Uuid::parse_str(&value).ok())
        } else {
            None
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
        let path = Path::new(&artifact.path);
        views.push(ExportArtifactView {
            id: artifact.id.as_uuid(),
            project_id,
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
            manifest_url: manifest_id
                .map_or_else(String::new, |id| format!("/api/v1/artifacts/{id}")),
            chapter_markers: profile.layout == ExportLayout::SingleFile
                && !matches!(profile.format, ExportFormat::Wav),
        });
    }
    Ok(views)
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
}
