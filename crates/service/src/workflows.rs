use std::{
    collections::{BTreeMap, BTreeSet},
    str::FromStr,
    sync::{Arc, Mutex as StdMutex, OnceLock},
    time::Duration,
};

use audiobookai_core::{
    AttemptId, Character, CharacterDetectionRun, CharacterId, DetectionRunId, DetectionRunStatus,
    FailureClass as CoreFailureClass, Job, JobAttempt, JobId, JobKind, JobState, JobUnit,
    JobUnitId, JobUnitKind, JobUnitState, ParagraphId, ProjectId, ProjectStatus, ProvenanceQuality,
    ProviderProfileId, RateCardId, UsageEvent, UsageEventId, UsageQuantities, UsageWorkload,
};
use audiobookai_providers::{
    CharacterDetectionRequest, CharacterDetectionResult, DetectedCharacter, DetectedDialogue,
    DetectionParagraph, ProviderError, ProviderUsage, ReasoningControl, Temperature, UsageSource,
};
use chrono::{Duration as ChronoDuration, Utc};
use futures::future::BoxFuture;
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::{
    AppState, ServiceError,
    models::{
        CharacterView, DialogueEvidenceView, JobStageView, JobStatusView, JobUnitStatusView,
        JobUnitView, JobView, ProviderModeView, ProviderProfileView, ProviderStatusView,
        ReviewStatus, UsageRowView,
    },
    runtime::{
        FailureClass, RetryEvent, RetryEventOutcome, RetryJournal, RetryJournalError, RetryPolicy,
        execute_with_retry,
    },
};

const DETECTION_BATCH_PARAGRAPHS: usize = 24;
const DETECTION_CONTEXT_OVERLAP: usize = 2;
const DETECTION_JOB_SCHEMA_VERSION: u32 = 4;

static ACTIVE_DETECTION_WORKERS: OnceLock<StdMutex<BTreeSet<Uuid>>> = OnceLock::new();

#[derive(Debug)]
struct ActiveDetectionWorker {
    job_id: Uuid,
}

impl ActiveDetectionWorker {
    fn acquire(job_id: Uuid) -> Option<Self> {
        let workers = ACTIVE_DETECTION_WORKERS.get_or_init(|| StdMutex::new(BTreeSet::new()));
        let mut active = workers
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        active.insert(job_id).then_some(Self { job_id })
    }
}

impl Drop for ActiveDetectionWorker {
    fn drop(&mut self) {
        ACTIVE_DETECTION_WORKERS
            .get_or_init(|| StdMutex::new(BTreeSet::new()))
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .remove(&self.job_id);
    }
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
struct DetectionJobConfig {
    schema_version: u32,
    provider_profile_id: Uuid,
    model: String,
    provider_endpoint: Option<String>,
    #[serde(default)]
    provider_mode: Option<ProviderModeView>,
    #[serde(default)]
    provider_snapshot_id: Option<Uuid>,
    temperature: Temperature,
    reasoning: ReasoningControl,
    detection_run_id: DetectionRunId,
    #[serde(default)]
    base_character_revision: u64,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
struct PersistedDetectionResult {
    characters: Vec<DetectedCharacter>,
    dialogue: Vec<DetectedDialogue>,
    usage: ProviderUsage,
}

impl From<CharacterDetectionResult> for PersistedDetectionResult {
    fn from(result: CharacterDetectionResult) -> Self {
        Self {
            characters: result.characters,
            dialogue: result.dialogue,
            usage: result.usage,
        }
    }
}

impl From<PersistedDetectionResult> for CharacterDetectionResult {
    fn from(result: PersistedDetectionResult) -> Self {
        Self {
            characters: result.characters,
            dialogue: result.dialogue,
            usage: result.usage,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum RecoveryDecision {
    Keep,
    FinalizePersistedResult,
    RedispatchSafe,
    FailUncertain,
    FailTerminal,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum DetectionPermission {
    Run,
    Cancelled,
    Terminal,
}

/// Creates the complete durable detection graph before the first provider request is dispatched.
#[allow(clippy::too_many_arguments, clippy::too_many_lines)]
pub async fn persist_detection_job(
    state: &AppState,
    view: &JobView,
    provider_id: Uuid,
    model: String,
    provider_endpoint: Option<String>,
    temperature: Temperature,
    reasoning: ReasoningControl,
    base_character_revision: u64,
) -> Result<JobView, ServiceError> {
    let project = state
        .database
        .repositories()
        .projects
        .get_project(ProjectId::from_uuid(view.project_id))
        .await
        .map_err(storage_error)?
        .ok_or(ServiceError::NotFound)?;
    let paragraphs = selected_paragraphs(state, &project).await?;
    if paragraphs.is_empty() {
        return Err(ServiceError::Conflict(
            "the selected chapters contain no speakable paragraphs".to_owned(),
        ));
    }
    let batches = paragraph_batches(&paragraphs);
    let provider_mode = state
        .catalog
        .read()
        .await
        .providers
        .get(&provider_id)
        .map(|provider| provider.mode)
        .ok_or_else(|| ServiceError::Conflict("detection provider was removed".to_owned()))?;
    let provider_snapshot_id = state
        .database
        .repositories()
        .providers
        .get(ProviderProfileId::from_uuid(provider_id))
        .await
        .map_err(storage_error)?
        .and_then(|provider| provider.capability_snapshot)
        .map(|snapshot| snapshot.id.as_uuid())
        .ok_or_else(|| {
            ServiceError::Conflict(
                "detection provider has no durable capability snapshot".to_owned(),
            )
        })?;
    let detection_run_id = DetectionRunId::new();
    let config = DetectionJobConfig {
        schema_version: DETECTION_JOB_SCHEMA_VERSION,
        provider_profile_id: provider_id,
        model: model.clone(),
        provider_endpoint,
        provider_mode: Some(provider_mode),
        provider_snapshot_id: Some(provider_snapshot_id),
        temperature,
        reasoning,
        detection_run_id,
        base_character_revision,
    };
    let now = view.updated_at;
    let mut job = Job {
        id: JobId::from_uuid(view.id),
        project_id: ProjectId::from_uuid(view.project_id),
        kind: JobKind::CharacterDetection,
        state: JobState::Queued,
        export_profile_id: None,
        reservation_id: None,
        progress_completed: 0,
        progress_total: u64::try_from(batches.len()).unwrap_or(u64::MAX),
        status_message: Some("Queued for character detection".to_owned()),
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

    let detection_run = CharacterDetectionRun {
        id: detection_run_id,
        project_id: ProjectId::from_uuid(view.project_id),
        provider_profile_id: ProviderProfileId::from_uuid(provider_id),
        model,
        status: DetectionRunStatus::Pending,
        paragraph_hashes: paragraphs
            .iter()
            .map(|paragraph| paragraph.hash.clone())
            .collect(),
        repair_attempted: false,
        created_at: now,
        completed_at: None,
    };
    insert_detection_run(state, &detection_run).await?;

    let mut units = Vec::with_capacity(batches.len());
    let mut reservation_estimates = Vec::new();
    let maximum_attempts = project
        .settings
        .reliability
        .max_transient_retries
        .saturating_add(1);
    for (batch_index, batch) in batches.iter().enumerate() {
        let request_estimate = detection_request_estimate(batch, &config.reasoning);
        let rated_estimate = crate::accounting::rate_usage_estimate(
            state,
            ProviderProfileId::from_uuid(provider_id),
            UsageWorkload::CharacterDetection,
            Some(config.model.clone()),
            request_estimate.clone(),
        )
        .await?;
        // One normal request plus the single schema-repair request, each with the configured
        // attempt ceiling. Unused capacity is released at terminal reconciliation.
        for _ in 0..usize::from(maximum_attempts).saturating_mul(2) {
            reservation_estimates.push(rated_estimate.clone());
        }
        let unit = detection_unit(
            view.id,
            provider_id,
            batch_index,
            &config,
            &request_estimate,
            rated_estimate.rate_card_id,
        )?;
        state
            .database
            .repositories()
            .jobs
            .upsert_unit(&unit)
            .await
            .map_err(storage_error)?;
        units.push(detection_unit_view(&unit));
    }
    match crate::accounting::reserve_for_estimates(state, &job, &reservation_estimates).await {
        Ok(Some(reservation_id)) => {
            let expected_revision = job.revision;
            job.reservation_id = Some(reservation_id);
            job.updated_at = Utc::now();
            job = state
                .database
                .repositories()
                .jobs
                .update(&job, expected_revision)
                .await
                .map_err(storage_error)?;
        }
        Ok(None) => {}
        Err(error) => {
            let expected_revision = job.revision;
            job.transition(JobState::Failed, Utc::now())
                .map_err(|transition| ServiceError::Conflict(transition.to_string()))?;
            job.status_message = Some(error.to_string());
            let _ = state
                .database
                .repositories()
                .jobs
                .update(&job, expected_revision)
                .await;
            let mut failed_run = detection_run;
            failed_run.status = DetectionRunStatus::Failed;
            failed_run.completed_at = Some(Utc::now());
            let _ = update_detection_run(state, &failed_run).await;
            return Err(error);
        }
    }
    let mut persisted_view = view.clone();
    persisted_view.current_stage = job.status_message;
    persisted_view.units = units;
    persisted_view.updated_at = now;
    Ok(persisted_view)
}

/// Starts a detection worker. A process-local guard prevents duplicate workers for one job.
pub fn spawn_character_detection(state: Arc<AppState>, job_id: Uuid) {
    tokio::spawn(run_character_detection(state, job_id));
}

pub async fn run_character_detection(state: Arc<AppState>, job_id: Uuid) {
    let mut acquired = None;
    for _ in 0..80 {
        if let Some(worker) = ActiveDetectionWorker::acquire(job_id) {
            acquired = Some(worker);
            break;
        }
        let state_now = state
            .database
            .repositories()
            .jobs
            .get(JobId::from_uuid(job_id))
            .await
            .ok()
            .flatten()
            .map(|job| job.state);
        if !matches!(state_now, Some(JobState::Queued | JobState::Running)) {
            return;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    let Some(_worker) = acquired else {
        return;
    };
    if let Err(error) = run_character_detection_inner(&state, job_id).await {
        let project_id = state
            .database
            .repositories()
            .jobs
            .get(JobId::from_uuid(job_id))
            .await
            .ok()
            .flatten()
            .map(|job| job.project_id.as_uuid());
        tracing::warn!(diagnostic_code = "detection.failed", %job_id, ?project_id, %error, "character detection failed");
        fail_job(&state, job_id, &error.to_string()).await;
    }
}

#[allow(clippy::too_many_lines)]
async fn run_character_detection_inner(
    state: &Arc<AppState>,
    job_id: Uuid,
) -> Result<(), ServiceError> {
    if !matches!(
        wait_until_detection_runnable(state, job_id).await?,
        DetectionPermission::Run
    ) {
        return Ok(());
    }

    let stored_job = state
        .database
        .repositories()
        .jobs
        .get(JobId::from_uuid(job_id))
        .await
        .map_err(storage_error)?
        .ok_or(ServiceError::NotFound)?;
    let project_id = stored_job.project_id.as_uuid();
    let mut units = detection_units(state, stored_job.id).await?;
    let config = consistent_detection_config(&units)?;
    let provider_id = config.provider_profile_id;

    let profile = {
        let catalog = state.catalog.read().await;
        catalog
            .providers
            .get(&provider_id)
            .cloned()
            .ok_or(ServiceError::NotFound)?
    };
    validate_detection_profile(&profile, &config)?;
    let domain_project = state
        .database
        .repositories()
        .projects
        .get_project(ProjectId::from_uuid(project_id))
        .await
        .map_err(|error| ServiceError::Storage(error.to_string()))?
        .ok_or(ServiceError::NotFound)?;
    let paragraphs = selected_paragraphs(state, &domain_project).await?;
    if paragraphs.is_empty() {
        return Err(ServiceError::Conflict(
            "the selected chapters contain no speakable paragraphs".to_owned(),
        ));
    }
    let detection_run = load_detection_run(state, config.detection_run_id).await?;
    let paragraph_hashes = paragraphs
        .iter()
        .map(|paragraph| paragraph.hash.clone())
        .collect::<Vec<_>>();
    if detection_run.project_id.as_uuid() != project_id
        || detection_run.provider_profile_id.as_uuid() != provider_id
        || detection_run.model != config.model
        || detection_run.paragraph_hashes != paragraph_hashes
    {
        return Err(ServiceError::Conflict(
            "the selected text or detection provider changed; start a new character-detection run"
                .to_owned(),
        ));
    }
    let batches = paragraph_batches(&paragraphs);
    if batches.len() != units.len() {
        return Err(ServiceError::Conflict(
            "the durable detection graph no longer matches the selected text".to_owned(),
        ));
    }
    let runtime_id = audiobookai_providers::ProviderId::new(provider_id.to_string())
        .map_err(|error| ServiceError::Internal(error.to_string()))?;
    let provider = state
        .providers
        .character(&runtime_id)
        .await
        .map_err(|error| ServiceError::Conflict(error.to_string()))?;
    config
        .temperature
        .validate(provider.capabilities().temperature)
        .map_err(|error| ServiceError::Conflict(error.to_string()))?;
    config
        .reasoning
        .validate(provider.capabilities())
        .map_err(|error| ServiceError::Conflict(error.to_string()))?;
    let mut detection_run = detection_run;
    detection_run.status = DetectionRunStatus::Running;
    detection_run.completed_at = None;
    update_detection_run(state, &detection_run).await?;

    let policy = RetryPolicy::new(
        domain_project
            .settings
            .reliability
            .max_transient_retries
            .saturating_add(1),
        Duration::from_millis(domain_project.settings.reliability.base_backoff_ms),
        Duration::from_millis(domain_project.settings.reliability.max_backoff_ms),
    )
    .map_err(|error| ServiceError::Internal(error.to_string()))?
    .with_uncertain_charge_retries(
        domain_project
            .settings
            .reliability
            .retry_possible_duplicate_charge,
    );

    for batch_index in 0..batches.len() {
        loop {
            match wait_until_detection_runnable(state, job_id).await? {
                DetectionPermission::Run => {}
                DetectionPermission::Cancelled | DetectionPermission::Terminal => return Ok(()),
            }
            units = detection_units(state, stored_job.id).await?;
            let mut unit = units
                .iter()
                .find(|unit| detection_batch_index(unit) == Some(batch_index))
                .cloned()
                .ok_or_else(|| {
                    ServiceError::Conflict(format!(
                        "durable detection batch {batch_index} is missing"
                    ))
                })?;
            if unit.state == JobUnitState::Completed {
                break;
            }
            if persisted_detection_result(&unit)?.is_some() {
                finalize_detection_unit(state, &mut unit, &profile, project_id).await?;
                update_job_progress(
                    state,
                    job_id,
                    completed_detection_units(state, stored_job.id).await?,
                    batches.len(),
                    &format!("Detection batch {} of {}", batch_index + 1, batches.len()),
                )
                .await?;
                break;
            }
            mark_detection_unit(state, &mut unit, JobUnitState::Running, None).await?;
            update_job_progress(
                state,
                job_id,
                completed_detection_units(state, stored_job.id).await?,
                batches.len(),
                &format!("Detection batch {} of {}", batch_index + 1, batches.len()),
            )
            .await?;
            let Some(result) = execute_detection_batch(
                state,
                &provider,
                &policy,
                &config,
                &batches[batch_index],
                &mut unit,
                &profile,
                project_id,
            )
            .await?
            else {
                mark_detection_unit(state, &mut unit, JobUnitState::Paused, None).await?;
                continue;
            };
            persist_detection_unit_result(state, &mut unit, result).await?;
            finalize_detection_unit(state, &mut unit, &profile, project_id).await?;
            update_job_progress(
                state,
                job_id,
                completed_detection_units(state, stored_job.id).await?,
                batches.len(),
                &format!("Detection batch {} of {}", batch_index + 1, batches.len()),
            )
            .await?;
            break;
        }
    }

    if !matches!(
        wait_until_detection_runnable(state, job_id).await?,
        DetectionPermission::Run
    ) {
        return Ok(());
    }

    let combined = combined_detection_results(state, stored_job.id).await?;

    let previous_characters = load_previous_characters(state, project_id).await?;
    let characters = merge_characters(
        &combined,
        &paragraphs,
        project_id,
        config.detection_run_id,
        &previous_characters,
    );
    let character_revision = persist_detection_results(
        state,
        &detection_run,
        &characters,
        &combined,
        &paragraphs,
        config.base_character_revision,
    )
    .await?;
    let previous_assignments = state
        .catalog
        .read()
        .await
        .characters
        .get(&project_id)
        .cloned()
        .unwrap_or_default()
        .into_iter()
        .map(|character| {
            (
                character.canonical_name.to_lowercase(),
                character.voice_assignment,
            )
        })
        .collect::<BTreeMap<_, _>>();
    let mut views = character_views(&characters, &combined, &paragraphs, &previous_assignments);
    apply_persisted_overrides(state, project_id, &paragraphs, &mut views).await?;
    {
        let mut catalog = state.catalog.write().await;
        catalog.characters.insert(project_id, views);
        if let Some(project) = catalog.projects.get_mut(&project_id) {
            project.character_review_status = ReviewStatus::NeedsReview;
            project.character_revision = character_revision;
            project.summary.updated_at = Utc::now();
        }
    }
    detection_run.status = DetectionRunStatus::Completed;
    detection_run.completed_at = Some(Utc::now());
    update_detection_run(state, &detection_run).await?;
    if !complete_detection_job(state, JobId::from_uuid(job_id)).await? {
        return Ok(());
    }
    state.events.publish(
        "character-detection.completed",
        serde_json::json!({ "jobId": job_id, "projectId": project_id }),
    );
    Ok(())
}

#[derive(Clone)]
struct DetectionSourceParagraph {
    id: ParagraphId,
    text: String,
    hash: String,
    chapter_title: String,
    chapter_id: audiobookai_core::ChapterId,
}

async fn selected_paragraphs(
    state: &AppState,
    project: &audiobookai_core::Project,
) -> Result<Vec<DetectionSourceParagraph>, ServiceError> {
    let repository = state.database.repositories().projects;
    let chapters = repository
        .list_chapters(project.book_id)
        .await
        .map_err(|error| ServiceError::Storage(error.to_string()))?;
    let mut output = Vec::new();
    for chapter in chapters.into_iter().filter(|chapter| chapter.selected) {
        let paragraphs = repository
            .list_paragraphs(chapter.id)
            .await
            .map_err(|error| ServiceError::Storage(error.to_string()))?;
        output.extend(
            paragraphs
                .into_iter()
                .map(|paragraph| DetectionSourceParagraph {
                    id: paragraph.id,
                    text: paragraph.text,
                    hash: paragraph.content_hash,
                    chapter_title: chapter.title.clone(),
                    chapter_id: chapter.id,
                }),
        );
    }
    Ok(output)
}

fn paragraph_batches(paragraphs: &[DetectionSourceParagraph]) -> Vec<Vec<DetectionParagraph>> {
    (0..paragraphs.len())
        .step_by(DETECTION_BATCH_PARAGRAPHS)
        .map(|start| {
            let end = (start + DETECTION_BATCH_PARAGRAPHS).min(paragraphs.len());
            let context_start = start.saturating_sub(DETECTION_CONTEXT_OVERLAP);
            let context_end = (end + DETECTION_CONTEXT_OVERLAP).min(paragraphs.len());
            paragraphs[context_start..context_end]
                .iter()
                .enumerate()
                .map(|(offset, paragraph)| {
                    let absolute = context_start + offset;
                    DetectionParagraph {
                        id: paragraph.id.to_string(),
                        text: paragraph.text.clone(),
                        context_only: absolute < start || absolute >= end,
                    }
                })
                .collect()
        })
        .collect()
}

fn detection_request_estimate(
    paragraphs: &[DetectionParagraph],
    reasoning: &ReasoningControl,
) -> UsageQuantities {
    let characters = paragraphs.iter().fold(0_u64, |total, paragraph| {
        total.saturating_add(u64::try_from(paragraph.text.chars().count()).unwrap_or(u64::MAX))
    });
    // A byte-per-token upper estimate plus stable schema/prompt and paragraph-ID overhead is
    // deliberately conservative across tokenizers without persisting the source text.
    let input_tokens = paragraphs.iter().fold(2_048_u64, |total, paragraph| {
        total
            .saturating_add(u64::try_from(paragraph.text.len()).unwrap_or(u64::MAX))
            .saturating_add(u64::try_from(paragraph.id.len()).unwrap_or(u64::MAX))
            .saturating_add(128)
    });
    let reasoning_tokens = match reasoning {
        ReasoningControl::Disabled => 0,
        ReasoningControl::TokenBudget { tokens } => u64::from(*tokens),
        ReasoningControl::Effort { effort } => match effort {
            audiobookai_providers::ReasoningEffort::Minimal => 2_048,
            audiobookai_providers::ReasoningEffort::Low => 4_096,
            audiobookai_providers::ReasoningEffort::Medium => 8_192,
            audiobookai_providers::ReasoningEffort::High => 16_384,
        },
        ReasoningControl::Inherit | ReasoningControl::Adaptive => 16_384,
    };
    UsageQuantities {
        characters: Some(characters),
        input_tokens: Some(input_tokens),
        output_tokens: Some(4_096),
        reasoning_tokens: Some(reasoning_tokens),
        ..UsageQuantities::default()
    }
}

fn storage_error(error: impl std::fmt::Display) -> ServiceError {
    ServiceError::Storage(error.to_string())
}

/// Recovers active detection jobs after process restart without replaying ambiguous requests.
pub async fn resume_durable_detections(state: Arc<AppState>) -> Result<(), ServiceError> {
    let active = state
        .database
        .repositories()
        .jobs
        .list_active()
        .await
        .map_err(storage_error)?;
    let mut claimed_projects = BTreeSet::new();
    for job in active
        .into_iter()
        .filter(|job| job.kind == JobKind::CharacterDetection)
    {
        if !claimed_projects.insert(job.project_id) {
            fail_job(
                &state,
                job.id.as_uuid(),
                "duplicate character-detection job recovered; it was not redispatched",
            )
            .await;
            continue;
        }
        match job.state {
            JobState::Cancelling => {
                cancel_detection_job(&state, job.id).await?;
            }
            JobState::Paused | JobState::Cancelled | JobState::Failed | JobState::Completed => {}
            JobState::Queued | JobState::Running | JobState::Pausing => {
                if !recover_detection_job(&state, &job).await? {
                    continue;
                }
                if job.state == JobState::Pausing {
                    if transition_detection_if(
                        &state,
                        job.id,
                        JobState::Pausing,
                        JobState::Paused,
                        "Paused after restart",
                    )
                    .await?
                    {
                        pause_detection_units(&state, job.id).await?;
                    }
                } else {
                    spawn_character_detection(Arc::clone(&state), job.id.as_uuid());
                }
            }
        }
    }
    Ok(())
}

/// Applies character-detection crash accounting before terminalizing a legacy job that conflicts
/// with another active production job. This is called by conversion recovery before any project
/// worker is spawned, so an in-flight paid detection is never silently redispatched or released as
/// zero usage.
pub(crate) async fn fail_recovered_production_conflict(
    state: &AppState,
    job: &Job,
    detail: &str,
) -> Result<(), ServiceError> {
    if job.kind != JobKind::CharacterDetection {
        return Err(ServiceError::Internal(
            "character-detection recovery received another job kind".to_owned(),
        ));
    }
    if recover_detection_job(state, job).await? {
        fail_job(state, job.id.as_uuid(), detail).await;
    }
    let recovered = state
        .database
        .repositories()
        .jobs
        .get(job.id)
        .await
        .map_err(storage_error)?
        .ok_or(ServiceError::NotFound)?;
    if !recovered.state.is_terminal() {
        return Err(ServiceError::Conflict(format!(
            "recovered character-detection job {} could not be terminalized safely",
            job.id
        )));
    }
    Ok(())
}

/// Makes paused or failed detection units runnable after an explicit user action.
pub async fn reset_detection_units_for_restart(
    state: &AppState,
    job_id: JobId,
    explicit_retry: bool,
) -> Result<(), ServiceError> {
    for mut unit in detection_units(state, job_id).await? {
        let should_reset = if explicit_retry {
            !matches!(
                unit.state,
                JobUnitState::Completed | JobUnitState::Cancelled
            )
        } else {
            unit.state == JobUnitState::Paused
        };
        if !should_reset {
            continue;
        }
        if explicit_retry {
            unit.payload.remove("requestId");
            unit.payload.insert(
                "dispatchState".to_owned(),
                serde_json::json!("explicit_retry"),
            );
        }
        mark_detection_unit(state, &mut unit, JobUnitState::Ready, None).await?;
    }
    Ok(())
}

/// Resets only detection batches that can redispatch and returns the exact worst-case estimates
/// for their fresh manual-retry budget cycle.
pub(crate) async fn prepare_detection_retry_units(
    state: &AppState,
    job_id: JobId,
) -> Result<Vec<crate::accounting::RatedUsageEstimate>, ServiceError> {
    let job = state
        .database
        .repositories()
        .jobs
        .get(job_id)
        .await
        .map_err(storage_error)?
        .ok_or(ServiceError::NotFound)?;
    let project = state
        .database
        .repositories()
        .projects
        .get_project(job.project_id)
        .await
        .map_err(storage_error)?
        .ok_or(ServiceError::NotFound)?;
    let maximum_attempts = project
        .settings
        .reliability
        .max_transient_retries
        .saturating_add(1);
    let multiplier = usize::from(maximum_attempts).saturating_mul(2);
    let mut estimates = Vec::new();
    for mut unit in detection_units(state, job_id).await? {
        if matches!(
            unit.state,
            JobUnitState::Completed | JobUnitState::Cancelled
        ) {
            continue;
        }
        if unit
            .payload
            .get("uncertainUsageUnresolved")
            .and_then(serde_json::Value::as_bool)
            .unwrap_or(false)
        {
            return Err(ServiceError::ConflictDetails {
                code: "retry_usage_unresolved",
                detail:
                    "this detection job has unresolved provider usage and cannot be retried safely"
                        .to_owned(),
                meta: serde_json::json!({"jobId": job_id}),
            });
        }
        let config = detection_config(&unit)?;
        let estimate = crate::accounting::rate_usage_estimate(
            state,
            ProviderProfileId::from_uuid(config.provider_profile_id),
            UsageWorkload::CharacterDetection,
            Some(config.model),
            detection_unit_estimate(&unit)?,
        )
        .await?;
        unit.payload.remove("requestId");
        unit.payload.insert(
            "dispatchState".to_owned(),
            serde_json::json!("explicit_retry"),
        );
        unit.payload.insert(
            "rateCardId".to_owned(),
            serde_json::to_value(estimate.rate_card_id)
                .map_err(|error| ServiceError::Internal(error.to_string()))?,
        );
        mark_detection_unit(state, &mut unit, JobUnitState::Ready, None).await?;
        for _ in 0..multiplier {
            estimates.push(estimate.clone());
        }
    }
    Ok(estimates)
}

/// Prevents an explicit retry from applying results to a character set or source text that was
/// edited after the failed attempt.
#[allow(clippy::too_many_lines)]
pub async fn validate_detection_retry(state: &AppState, job_id: JobId) -> Result<(), ServiceError> {
    let job = state
        .database
        .repositories()
        .jobs
        .get(job_id)
        .await
        .map_err(storage_error)?
        .ok_or(ServiceError::NotFound)?;
    let config = consistent_detection_config(&detection_units(state, job_id).await?)?;
    let revision =
        sqlx::query_scalar::<_, i64>("SELECT character_revision FROM projects WHERE id = ?")
            .bind(job.project_id.to_string())
            .fetch_one(state.database.pool())
            .await
            .map_err(storage_error)?;
    if u64::try_from(revision).ok() != Some(config.base_character_revision) {
        return Err(ServiceError::Conflict(
            "character review changed after this detection failed; start a new detection job"
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
    let profile = {
        let catalog = state.catalog.read().await;
        catalog
            .providers
            .get(&config.provider_profile_id)
            .cloned()
            .ok_or_else(|| {
                ServiceError::Conflict(
                    "the detection provider was removed; start a new detection job".to_owned(),
                )
            })?
    };
    validate_detection_profile(&profile, &config)?;
    if !crate::api::provider_capabilities_are_fresh(&profile)
        || !profile
            .capabilities
            .as_ref()
            .is_some_and(|capabilities| capabilities.character_detection)
    {
        return Err(ServiceError::Conflict(
            "the detection provider is no longer ready; refresh it and start a new detection job"
                .to_owned(),
        ));
    }
    if matches!(profile.mode, ProviderModeView::CloudRemote) && !project.cloud_consent.book_text {
        return Err(ServiceError::Conflict(
            "cloud-text consent was revoked; grant consent and start a new detection job"
                .to_owned(),
        ));
    }
    let current_snapshot_id = state
        .database
        .repositories()
        .providers
        .get(ProviderProfileId::from_uuid(config.provider_profile_id))
        .await
        .map_err(storage_error)?
        .and_then(|provider| provider.capability_snapshot)
        .map(|snapshot| snapshot.id.as_uuid());
    if config.provider_snapshot_id.is_none() || current_snapshot_id != config.provider_snapshot_id {
        return Err(ServiceError::Conflict(
            "the detection provider capability or credential snapshot changed; start a new detection job"
                .to_owned(),
        ));
    }
    let runtime_id = audiobookai_providers::ProviderId::new(config.provider_profile_id.to_string())
        .map_err(|error| ServiceError::Internal(error.to_string()))?;
    let runtime = state.providers.character(&runtime_id).await.map_err(|_| {
        ServiceError::Conflict(
            "the durable detection provider runtime is unavailable; start a new detection job"
                .to_owned(),
        )
    })?;
    config
        .temperature
        .validate(runtime.capabilities().temperature)
        .map_err(|_| {
            ServiceError::Conflict(
                "the detection provider capabilities changed; start a new detection job".to_owned(),
            )
        })?;
    if runtime.descriptor().id != runtime_id
        || config
            .provider_mode
            .is_none_or(|mode| !detection_runtime_mode_matches(mode, runtime.descriptor().kind))
    {
        return Err(ServiceError::Conflict(
            "the detection provider runtime identity changed; start a new detection job".to_owned(),
        ));
    }
    config
        .reasoning
        .validate(runtime.capabilities())
        .map_err(|_| {
            ServiceError::Conflict(
                "the detection provider capabilities changed; start a new detection job".to_owned(),
            )
        })?;
    let current_hashes = selected_paragraphs(state, &project)
        .await?
        .into_iter()
        .map(|paragraph| paragraph.hash)
        .collect::<Vec<_>>();
    let run = load_detection_run(state, config.detection_run_id).await?;
    if run.project_id != job.project_id
        || run.provider_profile_id.as_uuid() != config.provider_profile_id
        || run.model != config.model
        || current_hashes != run.paragraph_hashes
    {
        return Err(ServiceError::Conflict(
            "selected source text or detection routing changed after this job failed; start a new detection job"
                .to_owned(),
        ));
    }
    Ok(())
}

async fn recover_detection_job(state: &AppState, job: &Job) -> Result<bool, ServiceError> {
    let units = detection_units(state, job.id).await?;
    if let Err(error) = consistent_detection_config(&units) {
        fail_job(state, job.id.as_uuid(), &error.to_string()).await;
        return Ok(false);
    }
    for mut unit in units {
        let latest = latest_detection_attempt(state, unit.id).await?;
        match recovery_decision(&unit, latest.as_ref())? {
            RecoveryDecision::Keep | RecoveryDecision::FinalizePersistedResult => {}
            RecoveryDecision::RedispatchSafe => {
                unit.payload.remove("requestId");
                unit.payload.insert(
                    "dispatchState".to_owned(),
                    serde_json::json!("recovered_safe"),
                );
                mark_detection_unit(state, &mut unit, JobUnitState::Ready, None).await?;
            }
            RecoveryDecision::FailUncertain => {
                if let Some(attempt) = latest.as_ref() {
                    append_recovered_uncertain_detection_usage(state, job, &unit, attempt).await?;
                } else {
                    // Legacy rows can prove that dispatch might have started without retaining an
                    // attempt/request snapshot. Preserve that unknown as durable state so failure
                    // handling does not release the reservation as if usage were zero.
                    unit.payload.insert(
                        "uncertainUsageUnresolved".to_owned(),
                        serde_json::json!(true),
                    );
                }
                let detail = "A character-detection request was in flight when the app stopped and may have been charged. Retry this batch explicitly.";
                mark_detection_unit(state, &mut unit, JobUnitState::Failed, Some(detail)).await?;
                if let Some(view) = state.catalog.write().await.jobs.get_mut(&job.id.as_uuid()) {
                    view.uncertain_charge = true;
                }
                fail_job(state, job.id.as_uuid(), detail).await;
                return Ok(false);
            }
            RecoveryDecision::FailTerminal => {
                let detail = "Character detection stopped after a non-retryable provider response. Retry explicitly after correcting the provider configuration.";
                mark_detection_unit(state, &mut unit, JobUnitState::Failed, Some(detail)).await?;
                fail_job(state, job.id.as_uuid(), detail).await;
                return Ok(false);
            }
        }
    }
    Ok(true)
}

async fn append_recovered_uncertain_detection_usage(
    state: &AppState,
    job: &Job,
    unit: &JobUnit,
    attempt: &JobAttempt,
) -> Result<(), ServiceError> {
    let config = detection_config(unit)?;
    let profile = state
        .catalog
        .read()
        .await
        .providers
        .get(&config.provider_profile_id)
        .cloned()
        .ok_or(ServiceError::NotFound)?;
    let usage = ProviderUsage {
        source: UsageSource::Estimated,
        request_id: attempt.provider_request_id.clone(),
        ..ProviderUsage::default()
    };
    append_detection_usage(
        state,
        UsageEventId::new(),
        Some(attempt.id),
        &usage,
        &detection_unit_estimate(unit)?,
        &profile,
        job.project_id.as_uuid(),
        job.id.as_uuid(),
        profile.id,
        true,
        detection_unit_rate_card(unit)?,
    )
    .await
}

fn recovery_decision(
    unit: &JobUnit,
    latest_attempt: Option<&JobAttempt>,
) -> Result<RecoveryDecision, ServiceError> {
    if persisted_detection_result(unit)?.is_some() {
        return Ok(if unit.state == JobUnitState::Completed {
            RecoveryDecision::Keep
        } else {
            RecoveryDecision::FinalizePersistedResult
        });
    }
    if matches!(
        unit.state,
        JobUnitState::Blocked
            | JobUnitState::Ready
            | JobUnitState::Paused
            | JobUnitState::Completed
    ) {
        return Ok(RecoveryDecision::Keep);
    }
    if matches!(unit.state, JobUnitState::Cancelled | JobUnitState::Failed) {
        return Ok(RecoveryDecision::FailTerminal);
    }
    let dispatch_state = unit
        .payload
        .get("dispatchState")
        .and_then(serde_json::Value::as_str)
        .unwrap_or("unknown");
    let Some(attempt) = latest_attempt else {
        return Ok(if matches!(dispatch_state, "prepared" | "recovered_safe") {
            RecoveryDecision::RedispatchSafe
        } else {
            RecoveryDecision::FailUncertain
        });
    };
    if attempt.finished_at.is_none()
        || attempt.uncertain_charge
        || attempt.failure_class.is_none()
        || attempt
            .failure_class
            .is_some_and(CoreFailureClass::may_have_charged)
    {
        return Ok(RecoveryDecision::FailUncertain);
    }
    if attempt.failure_class == Some(CoreFailureClass::Cancelled)
        && dispatch_state == "cancelled_before_dispatch"
    {
        return Ok(RecoveryDecision::RedispatchSafe);
    }
    Ok(
        if attempt
            .failure_class
            .is_some_and(CoreFailureClass::is_transient)
        {
            RecoveryDecision::RedispatchSafe
        } else {
            RecoveryDecision::FailTerminal
        },
    )
}

async fn latest_detection_attempt(
    state: &AppState,
    unit_id: JobUnitId,
) -> Result<Option<JobAttempt>, ServiceError> {
    let payload = sqlx::query_scalar::<_, String>(
        "SELECT payload FROM job_attempts WHERE job_unit_id = ? ORDER BY ordinal DESC LIMIT 1",
    )
    .bind(unit_id.to_string())
    .fetch_optional(state.database.pool())
    .await
    .map_err(storage_error)?;
    payload
        .map(|payload| {
            serde_json::from_str(&payload)
                .map_err(|error| ServiceError::Internal(error.to_string()))
        })
        .transpose()
}

async fn wait_until_detection_runnable(
    state: &AppState,
    job_id: Uuid,
) -> Result<DetectionPermission, ServiceError> {
    let job_id = JobId::from_uuid(job_id);
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
                let _ = transition_detection_if(
                    state,
                    job_id,
                    JobState::Queued,
                    JobState::Running,
                    "Detecting characters",
                )
                .await?;
            }
            JobState::Running => {
                for mut unit in detection_units(state, job_id).await? {
                    if unit.state == JobUnitState::Paused {
                        mark_detection_unit(state, &mut unit, JobUnitState::Ready, None).await?;
                    }
                }
                return Ok(DetectionPermission::Run);
            }
            JobState::Pausing => {
                if transition_detection_if(
                    state,
                    job_id,
                    JobState::Pausing,
                    JobState::Paused,
                    "Paused at a character-detection batch boundary",
                )
                .await?
                {
                    pause_detection_units(state, job_id).await?;
                }
            }
            JobState::Paused => tokio::time::sleep(Duration::from_millis(250)).await,
            JobState::Cancelling => {
                cancel_detection_job(state, job_id).await?;
                return Ok(DetectionPermission::Cancelled);
            }
            JobState::Cancelled => return Ok(DetectionPermission::Cancelled),
            JobState::Failed | JobState::Completed => return Ok(DetectionPermission::Terminal),
        }
    }
}

async fn pause_detection_units(state: &AppState, job_id: JobId) -> Result<(), ServiceError> {
    for mut unit in detection_units(state, job_id).await? {
        if matches!(
            unit.state,
            JobUnitState::Blocked
                | JobUnitState::Ready
                | JobUnitState::Running
                | JobUnitState::Retrying
        ) {
            mark_detection_unit(state, &mut unit, JobUnitState::Paused, None).await?;
        }
    }
    Ok(())
}

async fn cancel_detection_job(state: &AppState, job_id: JobId) -> Result<(), ServiceError> {
    let units = detection_units(state, job_id).await?;
    let config = consistent_detection_config(&units).ok();
    for mut unit in units {
        if !matches!(
            unit.state,
            JobUnitState::Completed | JobUnitState::Cancelled
        ) {
            mark_detection_unit(state, &mut unit, JobUnitState::Cancelled, None).await?;
        }
    }
    let _ = transition_detection_if(
        state,
        job_id,
        JobState::Cancelling,
        JobState::Cancelled,
        "Character detection cancelled",
    )
    .await?;
    if let Some(config) = config
        && let Ok(mut run) = load_detection_run(state, config.detection_run_id).await
    {
        run.status = DetectionRunStatus::Cancelled;
        run.completed_at = Some(Utc::now());
        update_detection_run(state, &run).await?;
    }
    crate::accounting::finalize_job_reservation(state, job_id).await?;
    state
        .events
        .publish("job.cancelled", serde_json::json!({ "jobId": job_id }));
    Ok(())
}

async fn transition_detection_if(
    state: &AppState,
    job_id: JobId,
    from: JobState,
    to: JobState,
    message: &str,
) -> Result<bool, ServiceError> {
    let repository = state.database.repositories().jobs;
    let mut job = repository
        .get(job_id)
        .await
        .map_err(storage_error)?
        .ok_or(ServiceError::NotFound)?;
    if job.state != from {
        return Ok(false);
    }
    let expected_revision = job.revision;
    job.transition(to, Utc::now())
        .map_err(|error| ServiceError::Conflict(error.to_string()))?;
    job.status_message = Some(message.to_owned());
    let updated = match repository.update(&job, expected_revision).await {
        Ok(job) => job,
        Err(error) => {
            let current = repository
                .get(job_id)
                .await
                .map_err(storage_error)?
                .ok_or(ServiceError::NotFound)?;
            if current.revision != expected_revision || current.state != from {
                return Ok(false);
            }
            return Err(storage_error(error));
        }
    };
    sync_detection_job_view(state, &updated).await;
    state.events.publish(
        "job.updated",
        serde_json::json!({
            "jobId": job_id,
            "status": updated.state,
            "message": message,
        }),
    );
    Ok(true)
}

async fn complete_detection_job(state: &AppState, job_id: JobId) -> Result<bool, ServiceError> {
    loop {
        match wait_until_detection_runnable(state, job_id.as_uuid()).await? {
            DetectionPermission::Run => {
                if transition_detection_if(
                    state,
                    job_id,
                    JobState::Running,
                    JobState::Completed,
                    "Character review required",
                )
                .await?
                {
                    crate::accounting::finalize_job_reservation(state, job_id).await?;
                    return Ok(true);
                }
            }
            DetectionPermission::Cancelled | DetectionPermission::Terminal => return Ok(false),
        }
    }
}

async fn sync_detection_job_view(state: &AppState, job: &Job) {
    if let Some(view) = state.catalog.write().await.jobs.get_mut(&job.id.as_uuid()) {
        view.status = match job.state {
            JobState::Queued => JobStatusView::Queued,
            JobState::Running => JobStatusView::Running,
            JobState::Pausing => JobStatusView::Pausing,
            JobState::Cancelling => JobStatusView::Cancelling,
            JobState::Paused => JobStatusView::Paused,
            JobState::Cancelled => JobStatusView::Cancelled,
            JobState::Failed => JobStatusView::Failed,
            JobState::Completed => JobStatusView::Complete,
        };
        view.current_stage.clone_from(&job.status_message);
        view.started_at = job.started_at;
        view.updated_at = job.updated_at;
        view.progress = progress_fraction(job.progress_completed, job.progress_total);
    }
}

fn progress_fraction(completed: u64, total: u64) -> f32 {
    const SCALE: u128 = 10_000;
    if total == 0 {
        return 0.0;
    }
    let scaled = u128::from(completed)
        .saturating_mul(SCALE)
        .checked_div(u128::from(total))
        .unwrap_or_default()
        .min(SCALE);
    let scaled = u16::try_from(scaled).unwrap_or(10_000);
    f32::from(scaled) / 100.0
}

fn detection_request(
    model: &str,
    paragraphs: &[DetectionParagraph],
    repair: bool,
    temperature: Temperature,
    reasoning: ReasoningControl,
) -> CharacterDetectionRequest {
    CharacterDetectionRequest {
        request_id: Uuid::new_v4(),
        model: model.to_owned(),
        system_prompt: if repair {
            "Repair the prior malformed result. Return only schema-valid character and dialogue JSON. Preserve paragraph IDs and use UTF-8 byte offsets.".to_owned()
        } else {
            "Identify the narrator and named speaking characters. Return canonical names, aliases, confidence, and dialogue spans using the supplied paragraph IDs and UTF-8 byte offsets. Do not invent dialogue.".to_owned()
        },
        paragraphs: paragraphs.to_vec(),
        temperature,
        reasoning,
        max_output_tokens: 4_096,
    }
}

fn detection_unit(
    job_id: Uuid,
    provider_id: Uuid,
    batch_index: usize,
    config: &DetectionJobConfig,
    request_estimate: &UsageQuantities,
    rate_card_id: Option<RateCardId>,
) -> Result<JobUnit, ServiceError> {
    Ok(JobUnit {
        id: JobUnitId::new(),
        job_id: JobId::from_uuid(job_id),
        kind: JobUnitKind::DetectionBatch,
        state: JobUnitState::Ready,
        chapter_id: None,
        segment_id: None,
        provider_profile_id: Some(ProviderProfileId::from_uuid(provider_id)),
        dependencies: Vec::new(),
        attempt_count: 0,
        next_attempt_at: None,
        output_artifact_id: None,
        payload: BTreeMap::from([
            ("batchIndex".to_owned(), serde_json::json!(batch_index)),
            (
                "title".to_owned(),
                serde_json::json!(format!("Detection batch {}", batch_index + 1)),
            ),
            ("progress".to_owned(), serde_json::json!(0.0)),
            (
                "detectionConfig".to_owned(),
                serde_json::to_value(config)
                    .map_err(|error| ServiceError::Internal(error.to_string()))?,
            ),
            (
                "usageEventId".to_owned(),
                serde_json::json!(UsageEventId::new().to_string()),
            ),
            ("dispatchState".to_owned(), serde_json::json!("prepared")),
            ("needsRepair".to_owned(), serde_json::json!(false)),
            (
                "requestUsageEstimate".to_owned(),
                serde_json::to_value(request_estimate)
                    .map_err(|error| ServiceError::Internal(error.to_string()))?,
            ),
            (
                "rateCardId".to_owned(),
                serde_json::to_value(rate_card_id)
                    .map_err(|error| ServiceError::Internal(error.to_string()))?,
            ),
        ]),
        created_at: Utc::now(),
        updated_at: Utc::now(),
    })
}

fn detection_unit_rate_card(unit: &JobUnit) -> Result<Option<RateCardId>, ServiceError> {
    unit.payload
        .get("rateCardId")
        .cloned()
        .map_or(Ok(None), |value| {
            serde_json::from_value(value).map_err(|error| {
                ServiceError::Conflict(format!(
                    "detection batch has an invalid rate-card reference: {error}"
                ))
            })
        })
}

fn detection_unit_estimate(unit: &JobUnit) -> Result<UsageQuantities, ServiceError> {
    unit.payload
        .get("requestUsageEstimate")
        .cloned()
        .ok_or_else(|| {
            ServiceError::Conflict(
                "detection batch is missing its durable usage estimate".to_owned(),
            )
        })
        .and_then(|value| {
            serde_json::from_value(value).map_err(|error| {
                ServiceError::Conflict(format!(
                    "detection batch has an invalid usage estimate: {error}"
                ))
            })
        })
}

fn detection_unit_view(unit: &JobUnit) -> JobUnitView {
    JobUnitView {
        id: unit.id.as_uuid(),
        title: unit
            .payload
            .get("title")
            .and_then(serde_json::Value::as_str)
            .unwrap_or("Detect characters")
            .to_owned(),
        stage: JobStageView::Detect,
        status: match unit.state {
            JobUnitState::Blocked | JobUnitState::Ready | JobUnitState::Retrying => {
                JobUnitStatusView::Queued
            }
            JobUnitState::Running => JobUnitStatusView::Running,
            JobUnitState::Paused => JobUnitStatusView::Paused,
            JobUnitState::Cancelled => JobUnitStatusView::Cancelled,
            JobUnitState::Failed => JobUnitStatusView::Failed,
            JobUnitState::Completed => JobUnitStatusView::Complete,
        },
        progress: if unit.state == JobUnitState::Completed {
            100.0
        } else {
            unit.payload
                .get("progress")
                .cloned()
                .and_then(|value| serde_json::from_value::<f32>(value).ok())
                .unwrap_or_default()
                .clamp(0.0, 1.0)
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

fn detection_batch_index(unit: &JobUnit) -> Option<usize> {
    unit.payload
        .get("batchIndex")
        .and_then(serde_json::Value::as_u64)
        .and_then(|value| usize::try_from(value).ok())
}

fn detection_config(unit: &JobUnit) -> Result<DetectionJobConfig, ServiceError> {
    let config = unit
        .payload
        .get("detectionConfig")
        .cloned()
        .ok_or_else(|| {
            ServiceError::Conflict(
                "detection job is missing its durable provider configuration".to_owned(),
            )
        })?;
    let config: DetectionJobConfig = serde_json::from_value(config).map_err(|error| {
        ServiceError::Conflict(format!("invalid detection job config: {error}"))
    })?;
    if !matches!(config.schema_version, 2 | 3 | DETECTION_JOB_SCHEMA_VERSION) {
        return Err(ServiceError::Conflict(format!(
            "unsupported detection job schema version {}",
            config.schema_version
        )));
    }
    Ok(config)
}

fn consistent_detection_config(units: &[JobUnit]) -> Result<DetectionJobConfig, ServiceError> {
    let first = units.first().ok_or_else(|| {
        ServiceError::Conflict(
            "detection job has no durable batches; start a new detection run".to_owned(),
        )
    })?;
    let expected = detection_config(first)?;
    let mut indexes = BTreeSet::new();
    for unit in units {
        if unit.kind != JobUnitKind::DetectionBatch
            || unit.provider_profile_id.map(ProviderProfileId::as_uuid)
                != Some(expected.provider_profile_id)
            || detection_config(unit)? != expected
        {
            return Err(ServiceError::Conflict(
                "durable character-detection batches have inconsistent provider settings"
                    .to_owned(),
            ));
        }
        let index = detection_batch_index(unit).ok_or_else(|| {
            ServiceError::Conflict("detection batch is missing its durable index".to_owned())
        })?;
        if !indexes.insert(index) {
            return Err(ServiceError::Conflict(
                "detection job contains a duplicate batch index".to_owned(),
            ));
        }
    }
    if indexes.iter().copied().ne(0..indexes.len()) {
        return Err(ServiceError::Conflict(
            "detection job contains a non-contiguous batch graph".to_owned(),
        ));
    }
    Ok(expected)
}

fn validate_detection_profile(
    profile: &ProviderProfileView,
    config: &DetectionJobConfig,
) -> Result<(), ServiceError> {
    if profile.id != config.provider_profile_id
        || !matches!(profile.role, crate::models::ProviderRoleView::Llm)
        || profile.model.as_deref() != Some(config.model.as_str())
        || profile.endpoint != config.provider_endpoint
        || Some(profile.mode) != config.provider_mode
    {
        return Err(ServiceError::Conflict(
            "the detection provider endpoint or model changed; start a new detection run"
                .to_owned(),
        ));
    }
    if !matches!(profile.status, ProviderStatusView::Online) {
        return Err(ServiceError::Conflict(
            "the detection provider is not online".to_owned(),
        ));
    }
    if matches!(profile.mode, ProviderModeView::CloudRemote) && !profile.credential_configured {
        return Err(ServiceError::Conflict(
            "the cloud detection provider credential is not configured".to_owned(),
        ));
    }
    Ok(())
}

fn detection_runtime_mode_matches(
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

async fn detection_units(state: &AppState, job_id: JobId) -> Result<Vec<JobUnit>, ServiceError> {
    let mut units = state
        .database
        .repositories()
        .jobs
        .list_units(job_id)
        .await
        .map_err(storage_error)?
        .into_iter()
        .filter(|unit| unit.kind == JobUnitKind::DetectionBatch)
        .collect::<Vec<_>>();
    units.sort_by_key(detection_batch_index);
    Ok(units)
}

fn persisted_detection_result(
    unit: &JobUnit,
) -> Result<Option<PersistedDetectionResult>, ServiceError> {
    unit.payload
        .get("result")
        .cloned()
        .map(|value| {
            serde_json::from_value(value).map_err(|error| {
                ServiceError::Conflict(format!(
                    "durable detection batch result is invalid: {error}"
                ))
            })
        })
        .transpose()
}

async fn completed_detection_units(state: &AppState, job_id: JobId) -> Result<usize, ServiceError> {
    Ok(detection_units(state, job_id)
        .await?
        .into_iter()
        .filter(|unit| unit.state == JobUnitState::Completed)
        .count())
}

async fn combined_detection_results(
    state: &AppState,
    job_id: JobId,
) -> Result<CharacterDetectionResult, ServiceError> {
    let units = detection_units(state, job_id).await?;
    let mut combined = CharacterDetectionResult {
        characters: Vec::new(),
        dialogue: Vec::new(),
        usage: ProviderUsage::default(),
    };
    for unit in units {
        if unit.state != JobUnitState::Completed {
            return Err(ServiceError::Conflict(
                "character detection cannot finish while a batch is incomplete".to_owned(),
            ));
        }
        let result = persisted_detection_result(&unit)?.ok_or_else(|| {
            ServiceError::Conflict(
                "completed detection batch is missing its durable result".to_owned(),
            )
        })?;
        combined.characters.extend(result.characters);
        combined.dialogue.extend(result.dialogue);
    }
    Ok(combined)
}

async fn mark_detection_unit(
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
        unit.payload.insert(
            "lastError".to_owned(),
            serde_json::json!(error.chars().take(512).collect::<String>()),
        );
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
        *view = detection_unit_view(unit);
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

async fn persist_detection_unit_result(
    state: &AppState,
    unit: &mut JobUnit,
    result: CharacterDetectionResult,
) -> Result<(), ServiceError> {
    unit.payload.insert(
        "result".to_owned(),
        serde_json::to_value(PersistedDetectionResult::from(result))
            .map_err(|error| ServiceError::Internal(error.to_string()))?,
    );
    unit.payload.insert(
        "dispatchState".to_owned(),
        serde_json::json!("result_persisted"),
    );
    mark_detection_unit(state, unit, JobUnitState::Running, None).await
}

async fn finalize_detection_unit(
    state: &AppState,
    unit: &mut JobUnit,
    profile: &ProviderProfileView,
    project_id: Uuid,
) -> Result<(), ServiceError> {
    let result = persisted_detection_result(unit)?.ok_or_else(|| {
        ServiceError::Conflict("detection batch result is not durable".to_owned())
    })?;
    let usage_event_id = unit
        .payload
        .get("usageEventId")
        .and_then(serde_json::Value::as_str)
        .ok_or_else(|| {
            ServiceError::Conflict("detection batch is missing its usage event id".to_owned())
        })?
        .parse::<UsageEventId>()
        .map_err(|error| ServiceError::Conflict(error.to_string()))?;
    let latest_attempt = latest_detection_attempt(state, unit.id).await?;
    let attempt_id = latest_attempt.as_ref().map(|attempt| attempt.id);
    let mut usage = result.usage.clone();
    if usage.request_id.is_none() {
        usage.request_id = latest_attempt.and_then(|attempt| attempt.provider_request_id);
    }
    let request_estimate = detection_unit_estimate(unit)?;
    append_detection_usage(
        state,
        usage_event_id,
        attempt_id,
        &usage,
        &request_estimate,
        profile,
        project_id,
        unit.job_id.as_uuid(),
        profile.id,
        false,
        detection_unit_rate_card(unit)?,
    )
    .await?;
    unit.payload
        .insert("dispatchState".to_owned(), serde_json::json!("completed"));
    mark_detection_unit(state, unit, JobUnitState::Completed, None).await
}

async fn load_detection_run(
    state: &AppState,
    run_id: DetectionRunId,
) -> Result<CharacterDetectionRun, ServiceError> {
    let payload =
        sqlx::query_scalar::<_, String>("SELECT payload FROM detection_runs WHERE id = ?")
            .bind(run_id.to_string())
            .fetch_optional(state.database.pool())
            .await
            .map_err(storage_error)?
            .ok_or(ServiceError::NotFound)?;
    serde_json::from_str(&payload).map_err(|error| ServiceError::Internal(error.to_string()))
}

#[allow(clippy::too_many_arguments, clippy::too_many_lines)]
async fn execute_detection_batch(
    state: &Arc<AppState>,
    provider: &Arc<dyn audiobookai_providers::CharacterProvider>,
    policy: &RetryPolicy,
    config: &DetectionJobConfig,
    batch: &[DetectionParagraph],
    unit: &mut JobUnit,
    profile: &ProviderProfileView,
    project_id: Uuid,
) -> Result<Option<CharacterDetectionResult>, ServiceError> {
    let mut repair = unit
        .payload
        .get("needsRepair")
        .and_then(serde_json::Value::as_bool)
        .unwrap_or(false);
    loop {
        if repair {
            let mut run = load_detection_run(state, config.detection_run_id).await?;
            if !run.repair_attempted {
                run.repair_attempted = true;
                update_detection_run(state, &run).await?;
            }
        }
        let request = detection_request(
            &config.model,
            batch,
            repair,
            config.temperature,
            config.reasoning.clone(),
        );
        unit.payload.insert(
            "requestId".to_owned(),
            serde_json::json!(request.request_id),
        );
        unit.payload
            .insert("dispatchState".to_owned(), serde_json::json!("prepared"));
        mark_detection_unit(state, unit, JobUnitState::Running, None).await?;
        let attempt_offset = durable_attempt_offset(state, unit.id).await?;
        let request_estimate = detection_unit_estimate(unit)?;
        let dispatch_estimate = crate::accounting::rate_usage_estimate(
            state,
            ProviderProfileId::from_uuid(config.provider_profile_id),
            UsageWorkload::CharacterDetection,
            Some(config.model.clone()),
            request_estimate.clone(),
        )
        .await?;
        let journal = SqliteRetryJournal {
            state: Arc::clone(state),
            unit_id: unit.id,
            attempt_offset,
            usage_context: DetectionUsageContext {
                project_id,
                job_id: unit.job_id,
                profile: profile.clone(),
                request_estimate,
                provider_request_id: request.request_id,
                rate_card_id: detection_unit_rate_card(unit)?,
            },
        };
        let durable_job_id = unit.job_id;
        let dispatch_consent_lock = state.dispatch_consent_lifecycle_lock(project_id).await;
        let execution = execute_with_retry(policy, &journal, |attempt| {
            let state = Arc::clone(state);
            let provider = Arc::clone(provider);
            let request = request.clone();
            let config = config.clone();
            let journal = journal.clone();
            let dispatch_estimate = dispatch_estimate.clone();
            let dispatch_consent_lock = Arc::clone(&dispatch_consent_lock);
            async move {
                let _dispatch_consent_guard = dispatch_consent_lock.read().await;
                detection_dispatch_guard(&state, &config, durable_job_id, &dispatch_estimate)
                    .await?;
                journal
                    .record_dispatch_started(attempt, request.request_id)
                    .await
                    .map_err(|error| ProviderError::Process(error.to_string()))?;
                provider.detect_characters(request).await
            }
        })
        .await;
        unit.attempt_count = durable_attempt_offset(state, unit.id).await?;
        match execution {
            Ok(execution) => return Ok(Some(execution.value)),
            Err(crate::runtime::RetryExecutionError::Provider {
                source: ProviderError::InvalidResponse(_),
                ..
            }) if !repair => {
                repair = true;
                unit.payload
                    .insert("needsRepair".to_owned(), serde_json::json!(true));
                unit.payload.remove("requestId");
                unit.payload
                    .insert("dispatchState".to_owned(), serde_json::json!("prepared"));
                mark_detection_unit(state, unit, JobUnitState::Running, None).await?;
            }
            Err(crate::runtime::RetryExecutionError::Provider {
                source: ProviderError::Cancelled,
                ..
            }) => {
                let job = state
                    .database
                    .repositories()
                    .jobs
                    .get(unit.job_id)
                    .await
                    .map_err(storage_error)?
                    .ok_or(ServiceError::NotFound)?;
                if matches!(
                    job.state,
                    JobState::Pausing
                        | JobState::Paused
                        | JobState::Cancelling
                        | JobState::Cancelled
                ) {
                    return Ok(None);
                }
                return Err(ServiceError::Conflict(
                    "character-detection provider cancelled the request".to_owned(),
                ));
            }
            Err(error) => return Err(ServiceError::Conflict(error.to_string())),
        }
    }
}

async fn detection_dispatch_guard(
    state: &AppState,
    config: &DetectionJobConfig,
    job_id: JobId,
    dispatch_estimate: &crate::accounting::RatedUsageEstimate,
) -> Result<(), ProviderError> {
    let job = state
        .database
        .repositories()
        .jobs
        .get(job_id)
        .await
        .map_err(|error| ProviderError::Process(error.to_string()))?
        .ok_or_else(|| ProviderError::Configuration("detection job no longer exists".to_owned()))?;
    if !detection_state_allows_dispatch(job.state) {
        return Err(ProviderError::Cancelled);
    }
    let catalog = state.catalog.read().await;
    let profile = catalog
        .providers
        .get(&config.provider_profile_id)
        .ok_or_else(|| ProviderError::Configuration("detection provider was removed".to_owned()))?;
    if profile.model.as_deref() != Some(config.model.as_str())
        || !matches!(profile.role, crate::models::ProviderRoleView::Llm)
        || profile.endpoint != config.provider_endpoint
        || Some(profile.mode) != config.provider_mode
        || !matches!(profile.status, ProviderStatusView::Online)
        || !crate::api::provider_capabilities_are_fresh(profile)
        || !profile
            .capabilities
            .as_ref()
            .is_some_and(|capabilities| capabilities.character_detection)
    {
        return Err(ProviderError::Configuration(
            "detection provider configuration changed while the job was active".to_owned(),
        ));
    }
    if matches!(profile.mode, ProviderModeView::CloudRemote) {
        if !profile.credential_configured {
            return Err(ProviderError::Authentication);
        }
        if !catalog
            .projects
            .get(&job.project_id.as_uuid())
            .is_some_and(|project| project.consent_cloud_text)
        {
            return Err(ProviderError::Configuration(
                "cloud-text consent was revoked while detection was active".to_owned(),
            ));
        }
    }
    drop(catalog);
    let current_snapshot_id = state
        .database
        .repositories()
        .providers
        .get(ProviderProfileId::from_uuid(config.provider_profile_id))
        .await
        .map_err(|error| ProviderError::Process(error.to_string()))?
        .and_then(|provider| provider.capability_snapshot)
        .map(|snapshot| snapshot.id.as_uuid());
    if config.provider_snapshot_id.is_none() || current_snapshot_id != config.provider_snapshot_id {
        return Err(ProviderError::Configuration(
            "detection provider capability or credential snapshot changed while the job was active"
                .to_owned(),
        ));
    }
    crate::accounting::verify_dispatch_is_reserved(state, job_id, dispatch_estimate)
        .await
        .map_err(|_| {
            ProviderError::Configuration(
                "the active hard-budget reservation does not permit this detection request"
                    .to_owned(),
            )
        })?;
    Ok(())
}

const fn detection_state_allows_dispatch(state: JobState) -> bool {
    matches!(state, JobState::Running)
}

async fn durable_attempt_offset(state: &AppState, unit_id: JobUnitId) -> Result<u16, ServiceError> {
    let ordinal = sqlx::query_scalar::<_, Option<i64>>(
        "SELECT MAX(ordinal) FROM job_attempts WHERE job_unit_id = ?",
    )
    .bind(unit_id.to_string())
    .fetch_one(state.database.pool())
    .await
    .map_err(storage_error)?
    .unwrap_or_default();
    u16::try_from(ordinal)
        .map_err(|_| ServiceError::Conflict("detection attempt count is out of range".to_owned()))
}

#[derive(Clone)]
struct DetectionUsageContext {
    project_id: Uuid,
    job_id: JobId,
    profile: ProviderProfileView,
    request_estimate: UsageQuantities,
    provider_request_id: Uuid,
    rate_card_id: Option<RateCardId>,
}

#[derive(Clone)]
struct SqliteRetryJournal {
    state: Arc<AppState>,
    unit_id: JobUnitId,
    attempt_offset: u16,
    usage_context: DetectionUsageContext,
}

impl std::fmt::Debug for SqliteRetryJournal {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("SqliteRetryJournal")
            .field("unit_id", &self.unit_id)
            .field("job_id", &self.usage_context.job_id)
            .finish_non_exhaustive()
    }
}

impl SqliteRetryJournal {
    fn ordinal(&self, attempt: crate::runtime::AttemptNumber) -> Result<u16, RetryJournalError> {
        self.attempt_offset
            .checked_add(attempt.get())
            .ok_or_else(|| RetryJournalError::new("detection attempt count overflow"))
    }

    async fn record_dispatch_started(
        &self,
        attempt: crate::runtime::AttemptNumber,
        request_id: Uuid,
    ) -> Result<(), RetryJournalError> {
        let ordinal = self.ordinal(attempt)?;
        let repository = self.state.database.repositories().jobs;
        let mut unit = repository
            .get_unit(self.unit_id)
            .await
            .map_err(|error| RetryJournalError::new(error.to_string()))?
            .ok_or_else(|| RetryJournalError::new("detection unit no longer exists"))?;
        unit.attempt_count = unit.attempt_count.max(ordinal);
        unit.next_attempt_at = None;
        unit.state = JobUnitState::Running;
        unit.updated_at = Utc::now();
        unit.payload
            .insert("dispatchState".to_owned(), serde_json::json!("dispatched"));
        unit.payload
            .insert("requestId".to_owned(), serde_json::json!(request_id));
        repository
            .upsert_unit(&unit)
            .await
            .map_err(|error| RetryJournalError::new(error.to_string()))?;
        repository
            .insert_attempt(&JobAttempt {
                id: AttemptId::new(),
                job_unit_id: self.unit_id,
                ordinal,
                started_at: Utc::now(),
                finished_at: None,
                failure_class: None,
                error_code: None,
                redacted_error: None,
                provider_request_id: Some(request_id.to_string()),
                // Conservatively true until a response is durably classified.
                uncertain_charge: true,
            })
            .await
            .map_err(|error| RetryJournalError::new(error.to_string()))?;
        Ok(())
    }
}

impl RetryJournal for SqliteRetryJournal {
    #[allow(clippy::too_many_lines)]
    fn record(&self, event: RetryEvent) -> BoxFuture<'_, Result<(), RetryJournalError>> {
        let state = Arc::clone(&self.state);
        let unit_id = self.unit_id;
        let ordinal = self.ordinal(event.attempt);
        let usage_context = self.usage_context.clone();
        Box::pin(async move {
            let ordinal = ordinal?;
            let payload = sqlx::query_scalar::<_, String>(
                "SELECT payload FROM job_attempts WHERE job_unit_id = ? AND ordinal = ?",
            )
            .bind(unit_id.to_string())
            .bind(i64::from(ordinal))
            .fetch_optional(state.database.pool())
            .await
            .map_err(|error| RetryJournalError::new(error.to_string()))?;
            let dispatch_was_recorded = payload.is_some();
            let mut attempt = if let Some(payload) = payload {
                serde_json::from_str::<JobAttempt>(&payload)
                    .map_err(|error| RetryJournalError::new(error.to_string()))?
            } else {
                JobAttempt {
                    id: AttemptId::new(),
                    job_unit_id: unit_id,
                    ordinal,
                    started_at: event.recorded_at,
                    finished_at: None,
                    failure_class: None,
                    error_code: None,
                    redacted_error: None,
                    provider_request_id: None,
                    uncertain_charge: false,
                }
            };
            let (
                failure_class,
                uncertain_charge,
                error_code,
                redacted_error,
                dispatch_state,
                retry_after,
            ) = match event.outcome {
                RetryEventOutcome::Succeeded => {
                    (None, false, None, None, "response_received", None)
                }
                RetryEventOutcome::Failed {
                    class,
                    will_retry,
                    retry_after,
                } => (
                    Some(core_failure_class(class)),
                    class == FailureClass::UncertainCharge,
                    Some(format!("{class:?}").to_lowercase()),
                    Some("provider request failed; sensitive details were redacted".to_owned()),
                    if !dispatch_was_recorded && class == FailureClass::Cancelled {
                        "cancelled_before_dispatch"
                    } else if will_retry {
                        "retry_wait"
                    } else {
                        "failed_response"
                    },
                    retry_after,
                ),
            };
            attempt.finished_at = Some(event.recorded_at);
            attempt.failure_class = failure_class;
            attempt.error_code = error_code;
            attempt.redacted_error = redacted_error;
            attempt.uncertain_charge = uncertain_charge;
            let repository = state.database.repositories().jobs;
            if dispatch_was_recorded {
                sqlx::query(
                    "UPDATE job_attempts SET finished_at = ?, failure_class = ?, uncertain_charge = ?, payload = ? \
                     WHERE job_unit_id = ? AND ordinal = ?",
                )
                .bind(event.recorded_at.to_rfc3339())
                .bind(attempt.failure_class.map(core_failure_class_name))
                .bind(attempt.uncertain_charge)
                .bind(
                    serde_json::to_string(&attempt)
                        .map_err(|error| RetryJournalError::new(error.to_string()))?,
                )
                .bind(unit_id.to_string())
                .bind(i64::from(ordinal))
                .execute(state.database.pool())
                .await
                .map_err(|error| RetryJournalError::new(error.to_string()))?;
            } else {
                repository
                    .insert_attempt(&attempt)
                    .await
                    .map_err(|error| RetryJournalError::new(error.to_string()))?;
            }
            let mut unit = repository
                .get_unit(unit_id)
                .await
                .map_err(|error| RetryJournalError::new(error.to_string()))?
                .ok_or_else(|| RetryJournalError::new("detection unit no longer exists"))?;
            unit.attempt_count = unit.attempt_count.max(ordinal);
            unit.next_attempt_at = retry_after
                .and_then(|delay| ChronoDuration::from_std(delay).ok())
                .map(|delay| event.recorded_at + delay);
            unit.payload.insert(
                "dispatchState".to_owned(),
                serde_json::json!(dispatch_state),
            );
            unit.updated_at = event.recorded_at;
            repository
                .upsert_unit(&unit)
                .await
                .map_err(|error| RetryJournalError::new(error.to_string()))?;
            if uncertain_charge {
                let usage = ProviderUsage {
                    source: UsageSource::Estimated,
                    request_id: Some(usage_context.provider_request_id.to_string()),
                    ..ProviderUsage::default()
                };
                append_detection_usage(
                    &state,
                    UsageEventId::new(),
                    Some(attempt.id),
                    &usage,
                    &usage_context.request_estimate,
                    &usage_context.profile,
                    usage_context.project_id,
                    usage_context.job_id.as_uuid(),
                    usage_context.profile.id,
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

const fn core_failure_class(class: FailureClass) -> CoreFailureClass {
    match class {
        FailureClass::Transient => CoreFailureClass::Transport,
        FailureClass::RateLimited => CoreFailureClass::RateLimit,
        FailureClass::Authentication => CoreFailureClass::Authentication,
        FailureClass::Validation | FailureClass::Permanent => CoreFailureClass::Validation,
        FailureClass::UncertainCharge => CoreFailureClass::TimeoutAfterDispatch,
        FailureClass::Cancelled => CoreFailureClass::Cancelled,
    }
}

const fn core_failure_class_name(class: CoreFailureClass) -> &'static str {
    match class {
        CoreFailureClass::Transport => "transport",
        CoreFailureClass::RateLimit => "rate_limit",
        CoreFailureClass::ProviderServer => "provider_server",
        CoreFailureClass::Authentication => "authentication",
        CoreFailureClass::Validation => "validation",
        CoreFailureClass::CapabilityDrift => "capability_drift",
        CoreFailureClass::Cancelled => "cancelled",
        CoreFailureClass::TimeoutBeforeDispatch => "timeout_before_dispatch",
        CoreFailureClass::TimeoutAfterDispatch => "timeout_after_dispatch",
        CoreFailureClass::MediaProcessing => "media_processing",
        CoreFailureClass::Internal => "internal",
    }
}

fn merge_characters(
    result: &CharacterDetectionResult,
    _paragraphs: &[DetectionSourceParagraph],
    project_id: Uuid,
    run_id: DetectionRunId,
    previous_characters: &BTreeMap<String, Character>,
) -> Vec<Character> {
    let mut merged = BTreeMap::<String, (String, Vec<String>, f32)>::new();
    for character in &result.characters {
        let key = character.canonical_name.trim().to_lowercase();
        if key.is_empty() {
            continue;
        }
        let entry = merged.entry(key).or_insert_with(|| {
            (
                character.canonical_name.trim().to_owned(),
                Vec::new(),
                character.confidence,
            )
        });
        entry.2 = entry.2.max(character.confidence);
        for alias in &character.aliases {
            if !entry
                .1
                .iter()
                .any(|existing| existing.eq_ignore_ascii_case(alias))
            {
                entry.1.push(alias.clone());
            }
        }
    }
    merged
        .entry("narrator".to_owned())
        .or_insert(("Narrator".to_owned(), Vec::new(), 1.0));
    let now = Utc::now();
    let mut output = merged
        .into_values()
        .map(|(detected_name, detected_aliases, confidence)| {
            let previous = previous_characters.get(&detected_name.to_lowercase());
            let preserve_identity = previous.is_some_and(|character| character.manually_created);
            Character {
                id: previous.map_or_else(CharacterId::new, |character| character.id),
                project_id: ProjectId::from_uuid(project_id),
                role: if detected_name.eq_ignore_ascii_case("narrator") {
                    audiobookai_core::CharacterRole::Narrator
                } else {
                    audiobookai_core::CharacterRole::Character
                },
                canonical_name: previous
                    .filter(|_| preserve_identity)
                    .map_or(detected_name, |character| character.canonical_name.clone()),
                aliases: previous
                    .filter(|_| preserve_identity)
                    .map_or(detected_aliases, |character| character.aliases.clone()),
                description: previous.and_then(|character| character.description.clone()),
                confidence: Some(confidence),
                detection_run_id: Some(run_id),
                manually_created: preserve_identity,
                created_at: previous.map_or(now, |character| character.created_at),
                updated_at: now,
            }
        })
        .collect::<Vec<_>>();
    let mut retained_ids = output
        .iter()
        .map(|character| character.id)
        .collect::<BTreeSet<_>>();
    for previous in previous_characters.values() {
        if previous.manually_created && retained_ids.insert(previous.id) {
            output.push(previous.clone());
        }
    }
    output
}

async fn load_previous_characters(
    state: &AppState,
    project_id: Uuid,
) -> Result<BTreeMap<String, Character>, ServiceError> {
    let payloads = sqlx::query_as::<_, (String, String)>(
        "SELECT role, payload FROM characters WHERE project_id = ? ORDER BY updated_at DESC",
    )
    .bind(project_id.to_string())
    .fetch_all(state.database.pool())
    .await
    .map_err(|error| ServiceError::Storage(error.to_string()))?;
    let mut mapped = BTreeMap::new();
    for (role, payload) in payloads {
        let mut character: Character = serde_json::from_str(&payload)
            .map_err(|error| ServiceError::Internal(error.to_string()))?;
        character.role = if role == "narrator" {
            audiobookai_core::CharacterRole::Narrator
        } else {
            audiobookai_core::CharacterRole::Character
        };
        for name in std::iter::once(&character.canonical_name).chain(character.aliases.iter()) {
            mapped
                .entry(name.to_lowercase())
                .or_insert_with(|| character.clone());
        }
    }
    Ok(mapped)
}

fn character_views(
    characters: &[Character],
    result: &CharacterDetectionResult,
    paragraphs: &[DetectionSourceParagraph],
    previous_assignments: &BTreeMap<String, Option<crate::models::VoiceAssignmentView>>,
) -> Vec<CharacterView> {
    characters
        .iter()
        .map(|character| {
            let names = std::iter::once(character.canonical_name.as_str())
                .chain(character.aliases.iter().map(String::as_str))
                .collect::<Vec<_>>();
            let evidence = result
                .dialogue
                .iter()
                .filter(|dialogue| {
                    names
                        .iter()
                        .any(|name| name.eq_ignore_ascii_case(&dialogue.character))
                })
                .filter_map(|dialogue| {
                    let paragraph_id = ParagraphId::from_str(&dialogue.paragraph_id).ok()?;
                    let paragraph = paragraphs
                        .iter()
                        .find(|paragraph| paragraph.id == paragraph_id)?;
                    let start = usize::try_from(dialogue.start).ok()?;
                    let end = usize::try_from(dialogue.end).ok()?;
                    Some(DialogueEvidenceView {
                        id: Uuid::new_v4(),
                        paragraph_id: paragraph.id.as_uuid(),
                        chapter_id: paragraph.chapter_id.as_uuid(),
                        chapter_title: paragraph.chapter_title.clone(),
                        excerpt: paragraph
                            .text
                            .get(start..end)
                            .unwrap_or(paragraph.text.as_str())
                            .chars()
                            .take(240)
                            .collect(),
                        confidence: dialogue.confidence,
                        start_offset: start,
                        end_offset: end,
                        speaker_override: None,
                    })
                })
                .collect::<Vec<_>>();
            CharacterView {
                id: character.id.as_uuid(),
                role: character.role,
                canonical_name: character.canonical_name.clone(),
                aliases: character.aliases.clone(),
                confidence: character.confidence.unwrap_or_default(),
                dialogue_count: evidence.len(),
                voice_assignment: previous_assignments
                    .get(&character.canonical_name.to_lowercase())
                    .cloned()
                    .flatten(),
                evidence,
            }
        })
        .collect()
}

async fn apply_persisted_overrides(
    state: &AppState,
    project_id: Uuid,
    paragraphs: &[DetectionSourceParagraph],
    views: &mut [CharacterView],
) -> Result<(), ServiceError> {
    use audiobookai_core::{Speaker, SpeakerOverride};

    let payloads = sqlx::query_scalar::<_, String>(
        "SELECT payload FROM speaker_overrides WHERE project_id = ? ORDER BY updated_at",
    )
    .bind(project_id.to_string())
    .fetch_all(state.database.pool())
    .await
    .map_err(|error| ServiceError::Storage(error.to_string()))?;
    for payload in payloads {
        let record: SpeakerOverride = serde_json::from_str(&payload)
            .map_err(|error| ServiceError::Internal(error.to_string()))?;
        let Some(paragraph) = paragraphs
            .iter()
            .find(|paragraph| paragraph.id == record.paragraph_id)
        else {
            continue;
        };
        if paragraph.hash != record.source_content_hash {
            continue;
        }
        let speaker_name = match record.speaker {
            Speaker::Narrator => "Narrator".to_owned(),
            Speaker::Character(character_id) => views
                .iter()
                .find(|character| character.id == character_id.as_uuid())
                .map_or_else(
                    || character_id.to_string(),
                    |character| character.canonical_name.clone(),
                ),
            Speaker::Named(name) => name,
        };
        for evidence in views
            .iter_mut()
            .flat_map(|character| &mut character.evidence)
        {
            if evidence.paragraph_id == record.paragraph_id.as_uuid()
                && evidence.start_offset == usize::try_from(record.byte_start).unwrap_or(usize::MAX)
                && evidence.end_offset == usize::try_from(record.byte_end).unwrap_or(usize::MAX)
            {
                evidence.speaker_override = Some(speaker_name.clone());
            }
        }
    }
    Ok(())
}

async fn insert_detection_run(
    state: &AppState,
    run: &CharacterDetectionRun,
) -> Result<(), ServiceError> {
    sqlx::query(
        "INSERT INTO detection_runs (id, project_id, provider_id, status, created_at, completed_at, payload) \
         VALUES (?, ?, ?, ?, ?, NULL, ?)",
    )
    .bind(run.id.to_string())
    .bind(run.project_id.to_string())
    .bind(run.provider_profile_id.to_string())
    .bind(detection_run_status_name(run.status))
    .bind(run.created_at.to_rfc3339())
    .bind(serde_json::to_string(run).map_err(|error| ServiceError::Internal(error.to_string()))?)
    .execute(state.database.pool())
    .await
    .map_err(|error| ServiceError::Storage(error.to_string()))?;
    Ok(())
}

async fn update_detection_run(
    state: &AppState,
    run: &CharacterDetectionRun,
) -> Result<(), ServiceError> {
    sqlx::query("UPDATE detection_runs SET status = ?, completed_at = ?, payload = ? WHERE id = ?")
        .bind(detection_run_status_name(run.status))
        .bind(run.completed_at.map(|time| time.to_rfc3339()))
        .bind(
            serde_json::to_string(run)
                .map_err(|error| ServiceError::Internal(error.to_string()))?,
        )
        .bind(run.id.to_string())
        .execute(state.database.pool())
        .await
        .map_err(|error| ServiceError::Storage(error.to_string()))?;
    Ok(())
}

const fn detection_run_status_name(status: DetectionRunStatus) -> &'static str {
    match status {
        DetectionRunStatus::Completed => "completed",
        DetectionRunStatus::Failed => "failed",
        DetectionRunStatus::Cancelled => "cancelled",
        DetectionRunStatus::NeedsRepair => "needs_repair",
        DetectionRunStatus::Pending => "pending",
        DetectionRunStatus::Running => "running",
    }
}

#[allow(clippy::too_many_lines)]
async fn persist_detection_results(
    state: &AppState,
    run: &CharacterDetectionRun,
    characters: &[Character],
    result: &CharacterDetectionResult,
    paragraphs: &[DetectionSourceParagraph],
    base_character_revision: u64,
) -> Result<u64, ServiceError> {
    let mut transaction = state
        .database
        .pool()
        .begin()
        .await
        .map_err(|error| ServiceError::Storage(error.to_string()))?;
    for character in characters {
        sqlx::query(
            "INSERT INTO characters (id, project_id, role, canonical_name, updated_at, payload) \
             VALUES (?, ?, ?, ?, ?, ?) \
             ON CONFLICT(id) DO UPDATE SET role = excluded.role, \
             canonical_name = excluded.canonical_name, updated_at = excluded.updated_at, \
             payload = excluded.payload",
        )
        .bind(character.id.to_string())
        .bind(character.project_id.to_string())
        .bind(match character.role {
            audiobookai_core::CharacterRole::Narrator => "narrator",
            audiobookai_core::CharacterRole::Character => "character",
        })
        .bind(&character.canonical_name)
        .bind(character.updated_at.to_rfc3339())
        .bind(
            serde_json::to_string(character)
                .map_err(|error| ServiceError::Internal(error.to_string()))?,
        )
        .execute(&mut *transaction)
        .await
        .map_err(|error| ServiceError::Storage(error.to_string()))?;
        sqlx::query("DELETE FROM character_aliases WHERE character_id = ?")
            .bind(character.id.to_string())
            .execute(&mut *transaction)
            .await
            .map_err(|error| ServiceError::Storage(error.to_string()))?;
        for alias in &character.aliases {
            sqlx::query(
                "INSERT INTO character_aliases (character_id, alias, normalized_alias) VALUES (?, ?, ?)",
            )
            .bind(character.id.to_string())
            .bind(alias)
            .bind(alias.trim().to_lowercase())
            .execute(&mut *transaction)
            .await
            .map_err(|error| ServiceError::Storage(error.to_string()))?;
        }
    }
    for dialogue in &result.dialogue {
        let Some(character) = characters.iter().find(|character| {
            character
                .canonical_name
                .eq_ignore_ascii_case(&dialogue.character)
                || character
                    .aliases
                    .iter()
                    .any(|alias| alias.eq_ignore_ascii_case(&dialogue.character))
        }) else {
            continue;
        };
        let Ok(paragraph_id) = ParagraphId::from_str(&dialogue.paragraph_id) else {
            continue;
        };
        let paragraph = paragraphs
            .iter()
            .find(|paragraph| paragraph.id == paragraph_id);
        let start = u64::from(dialogue.start);
        let end = u64::from(dialogue.end);
        let span = audiobookai_core::DialogueSpan {
            paragraph_id,
            character_id: character.id,
            byte_start: start,
            byte_end: end,
            confidence: dialogue.confidence,
            evidence: paragraph.and_then(|paragraph| {
                paragraph
                    .text
                    .get(usize::try_from(start).ok()?..usize::try_from(end).ok()?)
                    .map(|text| text.chars().take(240).collect())
            }),
        };
        sqlx::query(
            "INSERT INTO dialogue_spans \
             (detection_run_id, paragraph_id, character_id, byte_start, byte_end, payload) \
             VALUES (?, ?, ?, ?, ?, ?) \
             ON CONFLICT(detection_run_id, paragraph_id, byte_start, byte_end) DO UPDATE SET \
             character_id = excluded.character_id, payload = excluded.payload",
        )
        .bind(run.id.to_string())
        .bind(paragraph_id.to_string())
        .bind(character.id.to_string())
        .bind(i64::try_from(start).map_err(|_| {
            ServiceError::Internal("dialogue offset exceeds SQLite range".to_owned())
        })?)
        .bind(i64::try_from(end).map_err(|_| {
            ServiceError::Internal("dialogue offset exceeds SQLite range".to_owned())
        })?)
        .bind(
            serde_json::to_string(&span)
                .map_err(|error| ServiceError::Internal(error.to_string()))?,
        )
        .execute(&mut *transaction)
        .await
        .map_err(|error| ServiceError::Storage(error.to_string()))?;
    }
    let (project_revision, stored_character_revision, project_payload) =
        sqlx::query_as::<_, (i64, i64, String)>(
            "SELECT revision, character_revision, payload FROM projects WHERE id = ?",
        )
        .bind(run.project_id.to_string())
        .fetch_optional(&mut *transaction)
        .await
        .map_err(storage_error)?
        .ok_or(ServiceError::NotFound)?;
    if u64::try_from(stored_character_revision).ok() != Some(base_character_revision) {
        return Err(ServiceError::Conflict(
            "character review changed while detection was running; start a new detection job"
                .to_owned(),
        ));
    }
    let mut project: audiobookai_core::Project = serde_json::from_str(&project_payload)
        .map_err(|error| ServiceError::Internal(error.to_string()))?;
    project.status = ProjectStatus::NeedsCharacterReview;
    project.character_reviewed_at = None;
    project.updated_at = Utc::now();
    let next_character_revision = base_character_revision.saturating_add(1);
    let next_project_revision = project_revision.saturating_add(1);
    let updated = sqlx::query(
        "UPDATE projects SET status = 'needs_character_review', updated_at = ?, revision = ?, \
         character_revision = ?, payload = ? WHERE id = ? AND revision = ? \
         AND character_revision = ?",
    )
    .bind(project.updated_at.to_rfc3339())
    .bind(next_project_revision)
    .bind(i64::try_from(next_character_revision).unwrap_or(i64::MAX))
    .bind(
        serde_json::to_string(&project)
            .map_err(|error| ServiceError::Internal(error.to_string()))?,
    )
    .bind(run.project_id.to_string())
    .bind(project_revision)
    .bind(i64::try_from(base_character_revision).unwrap_or(i64::MAX))
    .execute(&mut *transaction)
    .await
    .map_err(|error| ServiceError::Storage(error.to_string()))?;
    if updated.rows_affected() != 1 {
        return Err(ServiceError::Conflict(
            "character review changed while detection was running; start a new detection job"
                .to_owned(),
        ));
    }
    transaction
        .commit()
        .await
        .map_err(|error| ServiceError::Storage(error.to_string()))?;
    Ok(next_character_revision)
}

#[allow(clippy::too_many_arguments)]
async fn append_detection_usage(
    state: &AppState,
    usage_event_id: UsageEventId,
    attempt_id: Option<AttemptId>,
    usage: &ProviderUsage,
    request_estimate: &UsageQuantities,
    profile: &ProviderProfileView,
    project_id: Uuid,
    job_id: Uuid,
    provider_id: Uuid,
    uncertain_charge: bool,
    rate_card_id: Option<RateCardId>,
) -> Result<(), ServiceError> {
    if uncertain_charge
        && let Some(attempt_id) = attempt_id
        && let Some(payload) = sqlx::query_scalar::<_, String>(
            "SELECT payload FROM usage_ledger WHERE attempt_id = ? AND uncertain_charge = 1 LIMIT 1",
        )
        .bind(attempt_id.to_string())
        .fetch_optional(state.database.pool())
        .await
        .map_err(storage_error)?
    {
        let event = serde_json::from_str::<UsageEvent>(&payload)
            .map_err(|error| ServiceError::Internal(error.to_string()))?;
        ensure_detection_usage_row(state, profile, &event).await;
        return Ok(());
    }
    let (quantities, quantity_source) = merge_detection_usage(usage, request_estimate);
    let mut event = UsageEvent {
        id: usage_event_id,
        occurred_at: Utc::now(),
        workload: UsageWorkload::CharacterDetection,
        project_id: ProjectId::from_uuid(project_id),
        job_id: Some(JobId::from_uuid(job_id)),
        attempt_id,
        chapter_id: None,
        segment_id: None,
        provider_profile_id: ProviderProfileId::from_uuid(provider_id),
        provider_family: format!("{:?}", profile.kind).to_lowercase(),
        endpoint_family: safe_endpoint_family(profile.endpoint.as_deref()),
        model: profile.model.clone(),
        voice_profile_id: None,
        provider_request_id: usage.request_id.clone(),
        quantities,
        quantity_source,
        cost: None,
        cost_source: ProvenanceQuality::Unknown,
        rate_card_id: None,
        uncertain_charge,
        redacted_raw_usage: if uncertain_charge {
            BTreeMap::new()
        } else {
            usage
                .raw_redacted
                .as_ref()
                .and_then(serde_json::Value::as_object)
                .map(|object| object.clone().into_iter().collect())
                .unwrap_or_default()
        },
    };
    let usage_repository = state.database.repositories().usage;
    if let Some(stored) = usage_repository
        .get(usage_event_id)
        .await
        .map_err(storage_error)?
    {
        event = stored;
    } else {
        crate::accounting::apply_rate_card_snapshot(state, &mut event, rate_card_id).await?;
        usage_repository
            .append(&event)
            .await
            .map_err(storage_error)?;
    }
    ensure_detection_usage_row(state, profile, &event).await;
    Ok(())
}

fn safe_endpoint_family(endpoint: Option<&str>) -> String {
    endpoint
        .and_then(|endpoint| url::Url::parse(endpoint).ok())
        .and_then(|endpoint| {
            let host = endpoint.host_str()?;
            let port = endpoint
                .port()
                .map(|port| format!(":{port}"))
                .unwrap_or_default();
            Some(format!("{}://{host}{port}", endpoint.scheme()))
        })
        .unwrap_or_else(|| "official".to_owned())
}

fn merge_detection_usage(
    usage: &ProviderUsage,
    estimate: &UsageQuantities,
) -> (UsageQuantities, ProvenanceQuality) {
    let estimated = usage.characters.is_none()
        || usage.input_tokens.is_none()
        || usage.output_tokens.is_none()
        || usage.reasoning_tokens.is_none();
    let quantities = UsageQuantities {
        characters: usage.characters.or(estimate.characters),
        audio_milliseconds: usage.audio_milliseconds,
        input_tokens: usage.input_tokens.or(estimate.input_tokens),
        output_tokens: usage.output_tokens.or(estimate.output_tokens),
        cache_read_tokens: usage.cached_tokens,
        cache_write_tokens: None,
        reasoning_tokens: usage.reasoning_tokens.or(estimate.reasoning_tokens),
        provider_credits: usage.credits_micros,
    };
    let source = if estimated || usage.source == UsageSource::Estimated {
        ProvenanceQuality::Estimated
    } else {
        match usage.source {
            UsageSource::Reported => ProvenanceQuality::Reported,
            UsageSource::Estimated => ProvenanceQuality::Estimated,
            UsageSource::Unknown => ProvenanceQuality::Unknown,
        }
    };
    (quantities, source)
}

async fn ensure_detection_usage_row(
    state: &AppState,
    profile: &ProviderProfileView,
    event: &UsageEvent,
) {
    let project_title = state
        .catalog
        .read()
        .await
        .projects
        .get(&event.project_id.as_uuid())
        .map(|project| project.summary.title.clone());
    let mut catalog = state.catalog.write().await;
    if catalog
        .usage_rows
        .iter()
        .any(|row| row.id == event.id.as_uuid())
    {
        return;
    }
    catalog.usage_rows.push(UsageRowView {
        id: event.id.as_uuid(),
        occurred_at: event.occurred_at,
        project_title,
        provider_name: profile.name.clone(),
        operation: if event.uncertain_charge {
            "character_detection_uncertain_charge".to_owned()
        } else {
            "character_detection".to_owned()
        },
        model: event.model.clone(),
        voice: None,
        characters: event.quantities.characters,
        input_tokens: event.quantities.input_tokens,
        output_tokens: event.quantities.output_tokens,
        cost_micros: event.cost.as_ref().map(|cost| cost.micros),
        currency: event.cost.as_ref().map(|cost| cost.currency.clone()),
        provenance: format!("{:?}", event.quantity_source).to_lowercase(),
        request_id: event.provider_request_id.clone(),
    });
}

async fn update_job_progress(
    state: &AppState,
    job_id: Uuid,
    completed: usize,
    total: usize,
    progress_stage: &str,
) -> Result<(), ServiceError> {
    let repository = state.database.repositories().jobs;
    for _ in 0..4 {
        let mut job = repository
            .get(JobId::from_uuid(job_id))
            .await
            .map_err(storage_error)?
            .ok_or(ServiceError::NotFound)?;
        if job.state.is_terminal() {
            return Ok(());
        }
        let expected_revision = job.revision;
        job.progress_completed = u64::try_from(completed).unwrap_or(u64::MAX);
        job.progress_total = u64::try_from(total).unwrap_or(u64::MAX);
        job.status_message = Some(progress_stage.to_owned());
        job.updated_at = Utc::now();
        if let Ok(updated) = repository.update(&job, expected_revision).await {
            sync_detection_job_view(state, &updated).await;
            state.events.publish(
                "job.progress",
                serde_json::json!({
                    "jobId": job_id,
                    "completed": completed,
                    "total": total,
                }),
            );
            return Ok(());
        }
    }
    Err(ServiceError::Conflict(
        "character-detection progress changed concurrently; retrying is safe".to_owned(),
    ))
}

async fn fail_job(state: &AppState, job_id: Uuid, detail: &str) {
    let id = JobId::from_uuid(job_id);
    let Ok(Some(mut job)) = state.database.repositories().jobs.get(id).await else {
        return;
    };
    if matches!(job.state, JobState::Cancelling) {
        let _ = cancel_detection_job(state, id).await;
        return;
    }
    if matches!(job.state, JobState::Cancelled | JobState::Completed) {
        return;
    }
    if job.state == JobState::Paused {
        let _ = transition_detection_if(
            state,
            id,
            JobState::Paused,
            JobState::Running,
            "Preparing failed detection job for retry",
        )
        .await;
        if let Ok(Some(updated)) = state.database.repositories().jobs.get(id).await {
            job = updated;
        }
    }
    for mut unit in detection_units(state, id).await.unwrap_or_default() {
        if !matches!(
            unit.state,
            JobUnitState::Completed | JobUnitState::Cancelled
        ) {
            let _ = mark_detection_unit(state, &mut unit, JobUnitState::Failed, Some(detail)).await;
        }
    }
    if let Ok(units) = detection_units(state, id).await
        && let Ok(config) = consistent_detection_config(&units)
        && let Ok(mut run) = load_detection_run(state, config.detection_run_id).await
        && !matches!(
            run.status,
            DetectionRunStatus::Completed | DetectionRunStatus::Cancelled
        )
    {
        run.status = DetectionRunStatus::Failed;
        run.completed_at = Some(Utc::now());
        let _ = update_detection_run(state, &run).await;
    }
    if job.state == JobState::Failed {
        if let Some(view) = state.catalog.write().await.jobs.get_mut(&job_id) {
            view.status = JobStatusView::Failed;
            view.current_stage = Some(detail.to_owned());
            view.updated_at = Utc::now();
        }
    } else {
        let _ = transition_detection_if(state, id, job.state, JobState::Failed, detail).await;
    }
    if (detail.contains("may have been charged") || detail.contains("uncertain"))
        && let Some(view) = state.catalog.write().await.jobs.get_mut(&job_id)
    {
        view.uncertain_charge = true;
    }
    state.events.publish(
        "job.failed",
        serde_json::json!({ "jobId": job_id, "detail": detail }),
    );
    let retain_unknown_reservation = match detection_units(state, id).await {
        Ok(units) => units.iter().any(|unit| {
            unit.payload
                .get("uncertainUsageUnresolved")
                .and_then(serde_json::Value::as_bool)
                .unwrap_or(false)
        }),
        Err(error) => {
            tracing::warn!(%job_id, %error, "could not verify character-detection usage during failure; reservation retained");
            true
        }
    };
    if retain_unknown_reservation {
        tracing::warn!(diagnostic_code = "detection.recovery.usage_unresolved", %job_id, "character-detection usage is unresolved; reservation retained");
    } else if let Err(error) = crate::accounting::finalize_job_reservation(state, id).await {
        tracing::warn!(%job_id, %error, "could not finalize character-detection budget reservation");
    }
}

#[cfg(test)]
mod tests {
    use audiobookai_providers::ReasoningEffort;

    use super::*;

    fn config() -> DetectionJobConfig {
        DetectionJobConfig {
            schema_version: DETECTION_JOB_SCHEMA_VERSION,
            provider_profile_id: Uuid::new_v4(),
            model: "model".to_owned(),
            provider_endpoint: Some("http://127.0.0.1:1234".to_owned()),
            provider_mode: Some(ProviderModeView::ExternalEndpoint),
            provider_snapshot_id: Some(Uuid::new_v4()),
            temperature: Temperature::Default,
            reasoning: ReasoningControl::Inherit,
            detection_run_id: DetectionRunId::new(),
            base_character_revision: 0,
        }
    }

    fn unit(state: JobUnitState) -> JobUnit {
        let config = config();
        let mut unit = detection_unit(
            Uuid::new_v4(),
            config.provider_profile_id,
            0,
            &config,
            &UsageQuantities::default(),
            None,
        )
        .expect("detection unit");
        unit.state = state;
        unit
    }

    fn attempt(
        unit_id: JobUnitId,
        finished: bool,
        failure_class: Option<CoreFailureClass>,
        uncertain_charge: bool,
    ) -> JobAttempt {
        let now = Utc::now();
        JobAttempt {
            id: AttemptId::new(),
            job_unit_id: unit_id,
            ordinal: 1,
            started_at: now,
            finished_at: finished.then_some(now),
            failure_class,
            error_code: None,
            redacted_error: None,
            provider_request_id: None,
            uncertain_charge,
        }
    }

    #[test]
    fn detection_request_preserves_explicit_provider_controls() {
        let request = detection_request(
            "model",
            &[],
            false,
            Temperature::Null,
            ReasoningControl::Effort {
                effort: ReasoningEffort::High,
            },
        );

        assert_eq!(request.temperature, Temperature::Null);
        assert_eq!(
            request.reasoning,
            ReasoningControl::Effort {
                effort: ReasoningEffort::High,
            }
        );
    }

    #[test]
    fn displayed_detection_progress_is_bounded_without_lossy_integer_casts() {
        assert!(progress_fraction(0, 0).abs() < f32::EPSILON);
        assert!((progress_fraction(1, 2) - 50.0).abs() < f32::EPSILON);
        assert!((progress_fraction(2, 1) - 100.0).abs() < f32::EPSILON);
    }

    #[test]
    fn a_new_detection_run_retains_manual_characters_absent_from_the_model_result() {
        let project_id = Uuid::new_v4();
        let manual_id = CharacterId::new();
        let now = Utc::now();
        let manual = Character {
            id: manual_id,
            project_id: ProjectId::from_uuid(project_id),
            role: audiobookai_core::CharacterRole::Character,
            canonical_name: "Archivist".to_owned(),
            aliases: vec!["The Keeper".to_owned()],
            description: Some("Manually curated".to_owned()),
            confidence: Some(1.0),
            detection_run_id: None,
            manually_created: true,
            created_at: now,
            updated_at: now,
        };
        let previous = BTreeMap::from([("archivist".to_owned(), manual)]);
        let result = CharacterDetectionResult {
            characters: vec![DetectedCharacter {
                canonical_name: "Visitor".to_owned(),
                aliases: Vec::new(),
                confidence: 0.8,
            }],
            dialogue: Vec::new(),
            usage: ProviderUsage::default(),
        };

        let merged = merge_characters(&result, &[], project_id, DetectionRunId::new(), &previous);

        let retained = merged
            .iter()
            .find(|character| character.id == manual_id)
            .expect("manual character is retained");
        assert_eq!(retained.canonical_name, "Archivist");
        assert!(retained.manually_created);
        assert!(merged.iter().any(|character| {
            character.role == audiobookai_core::CharacterRole::Narrator
                && character.canonical_name == "Narrator"
        }));
    }

    #[test]
    fn detection_reservation_estimate_is_conservative_and_keeps_reasoning_separate() {
        let paragraphs = vec![DetectionParagraph {
            id: "paragraph-1".to_owned(),
            text: "Grüße from the narrator".to_owned(),
            context_only: false,
        }];
        let estimate = detection_request_estimate(
            &paragraphs,
            &ReasoningControl::Effort {
                effort: ReasoningEffort::High,
            },
        );
        assert_eq!(
            estimate.characters,
            Some(u64::try_from(paragraphs[0].text.chars().count()).unwrap())
        );
        assert!(estimate.input_tokens.unwrap() >= paragraphs[0].text.len() as u64);
        assert_eq!(estimate.output_tokens, Some(4_096));
        assert_eq!(estimate.reasoning_tokens, Some(16_384));
    }

    #[test]
    fn missing_detection_usage_uses_nonzero_estimates_instead_of_zero() {
        let estimate = UsageQuantities {
            characters: Some(900),
            input_tokens: Some(1_500),
            output_tokens: Some(4_096),
            reasoning_tokens: Some(8_192),
            ..UsageQuantities::default()
        };
        let (quantities, source) = merge_detection_usage(&ProviderUsage::default(), &estimate);
        assert_eq!(quantities.input_tokens, Some(1_500));
        assert_eq!(quantities.output_tokens, Some(4_096));
        assert_eq!(quantities.reasoning_tokens, Some(8_192));
        assert_eq!(source, ProvenanceQuality::Estimated);
        assert_eq!(quantities.provider_credits, None);
    }

    #[test]
    fn detection_usage_endpoint_provenance_never_keeps_url_credentials_or_queries() {
        assert_eq!(
            safe_endpoint_family(Some(
                "https://user:credential-placeholder@example.test:8443/v1?debug=removed"
            )),
            "https://example.test:8443"
        );
    }

    #[test]
    fn detection_unit_persists_complete_dispatch_configuration() {
        let config = config();
        let estimate = UsageQuantities {
            input_tokens: Some(1_000),
            output_tokens: Some(4_096),
            ..UsageQuantities::default()
        };
        let unit = detection_unit(
            Uuid::new_v4(),
            config.provider_profile_id,
            3,
            &config,
            &estimate,
            None,
        )
        .expect("detection unit");

        assert_eq!(detection_batch_index(&unit), Some(3));
        assert_eq!(detection_config(&unit).expect("config"), config);
        assert!(
            unit.payload
                .get("usageEventId")
                .and_then(serde_json::Value::as_str)
                .and_then(|value| value.parse::<UsageEventId>().ok())
                .is_some()
        );
        assert_eq!(detection_unit_estimate(&unit).expect("estimate"), estimate);
    }

    #[test]
    fn detection_retry_profile_rejects_model_endpoint_or_mode_drift() {
        let config = config();
        let profile = ProviderProfileView {
            id: config.provider_profile_id,
            name: "Detection provider".to_owned(),
            kind: crate::models::ProviderKindView::OpenaiCompatible,
            role: crate::models::ProviderRoleView::Llm,
            mode: ProviderModeView::ExternalEndpoint,
            endpoint: config.provider_endpoint.clone(),
            executable_path: None,
            working_directory: None,
            arguments: Vec::new(),
            status: ProviderStatusView::Online,
            model: Some(config.model.clone()),
            credential_configured: true,
            capabilities: None,
            capability_source: None,
            capability_updated_at: Some(Utc::now()),
            last_error: None,
        };
        validate_detection_profile(&profile, &config).expect("matching durable profile");

        let mut changed_model = profile.clone();
        changed_model.model = Some("different-model".to_owned());
        assert!(validate_detection_profile(&changed_model, &config).is_err());

        let mut changed_endpoint = profile.clone();
        changed_endpoint.endpoint = Some("http://127.0.0.1:9999".to_owned());
        assert!(validate_detection_profile(&changed_endpoint, &config).is_err());

        let mut changed_mode = profile;
        changed_mode.mode = ProviderModeView::CloudRemote;
        assert!(validate_detection_profile(&changed_mode, &config).is_err());
    }

    #[test]
    fn restart_never_redispatches_an_unfinished_or_lost_successful_request() {
        let mut unit = unit(JobUnitState::Running);
        unit.payload
            .insert("dispatchState".to_owned(), serde_json::json!("dispatched"));
        let unfinished = attempt(unit.id, false, None, true);
        assert_eq!(
            recovery_decision(&unit, Some(&unfinished)).expect("decision"),
            RecoveryDecision::FailUncertain
        );

        let returned_but_not_persisted = attempt(unit.id, true, None, false);
        unit.payload.insert(
            "dispatchState".to_owned(),
            serde_json::json!("response_received"),
        );
        assert_eq!(
            recovery_decision(&unit, Some(&returned_but_not_persisted)).expect("decision"),
            RecoveryDecision::FailUncertain
        );
    }

    #[test]
    fn restart_resumes_only_safe_or_durable_batch_work() {
        let mut transient_unit = unit(JobUnitState::Running);
        transient_unit
            .payload
            .insert("dispatchState".to_owned(), serde_json::json!("retry_wait"));
        let transient = attempt(
            transient_unit.id,
            true,
            Some(CoreFailureClass::Transport),
            false,
        );
        assert_eq!(
            recovery_decision(&transient_unit, Some(&transient)).expect("decision"),
            RecoveryDecision::RedispatchSafe
        );

        let mut paused_before_dispatch = unit(JobUnitState::Running);
        paused_before_dispatch.payload.insert(
            "dispatchState".to_owned(),
            serde_json::json!("cancelled_before_dispatch"),
        );
        let cancelled = attempt(
            paused_before_dispatch.id,
            true,
            Some(CoreFailureClass::Cancelled),
            false,
        );
        assert_eq!(
            recovery_decision(&paused_before_dispatch, Some(&cancelled)).expect("decision"),
            RecoveryDecision::RedispatchSafe
        );

        let mut durable = unit(JobUnitState::Running);
        durable.payload.insert(
            "result".to_owned(),
            serde_json::to_value(PersistedDetectionResult {
                characters: Vec::new(),
                dialogue: Vec::new(),
                usage: ProviderUsage::default(),
            })
            .expect("result"),
        );
        assert_eq!(
            recovery_decision(&durable, None).expect("decision"),
            RecoveryDecision::FinalizePersistedResult
        );
    }

    #[test]
    fn pause_and_cancel_states_gate_every_new_provider_dispatch() {
        assert!(detection_state_allows_dispatch(JobState::Running));
        for state in [
            JobState::Queued,
            JobState::Pausing,
            JobState::Paused,
            JobState::Cancelling,
            JobState::Cancelled,
            JobState::Failed,
            JobState::Completed,
        ] {
            assert!(!detection_state_allows_dispatch(state), "state {state:?}");
        }
    }
}
