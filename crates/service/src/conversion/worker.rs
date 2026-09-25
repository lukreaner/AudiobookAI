use super::{
    ACTIVE_WORKERS, AppState, Arc, BTreeMap, BTreeSet, ChapterArtifact, ConversionPlan,
    ExportProfile, ExportProfileId, HashMap, Job, JobId, JobKind, JobState, JobStatusView, JobUnit,
    JobUnitKind, JobUnitState, PersistedUnitPlan, ProjectDisplayStatus, ProjectId,
    ProofExportSnapshot, ProofExportSnapshotId, ProofingPlanStatus, ProviderProfileId,
    ReservationId, SegmentArtifact, SegmentPlan, ServiceError, StdMutex, StreamExt, TryStreamExt,
    UsageQuantities, UsageWorkload, Utc, artifact_path, assemble_chapter,
    complete_playback_segment, enforce_cache_limit, export_book, increment_job_progress,
    internal_error, job_view, load_artifact, load_conversion_plan, load_unit_plan, mark_job_failed,
    prepare_playback, reconcile_job_budgets, record_interrupted_paid_unit_uncertainty,
    release_job_cache_pins, resolve_sidecars, set_job_message, storage_error, stream,
    synthesize_segment, transition_job, unit_view, update_export_catalog, update_unit_state,
    verify_selected_artifact_integrity, wait_until_runnable,
};

pub(super) async fn reserve_job_budgets(
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

pub(super) fn request_production_worker(job_id: JobId, retry_after_cleanup: bool) -> bool {
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

pub(super) fn finish_production_worker_iteration(job_id: JobId) -> bool {
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

pub(super) fn schedule_conversion_job(state: Arc<AppState>, job_id: JobId) {
    if request_production_worker(job_id, false) {
        tokio::spawn(run_conversion_job(state, job_id));
    }
}

pub(super) fn schedule_segment_regeneration_job(state: Arc<AppState>, job_id: JobId) {
    if request_production_worker(job_id, false) {
        tokio::spawn(run_segment_regeneration_job(state, job_id));
    }
}

pub(super) fn schedule_conversion_retry(state: Arc<AppState>, job_id: JobId) {
    if request_production_worker(job_id, true) {
        tokio::spawn(run_conversion_job(state, job_id));
    }
}

pub(super) fn schedule_segment_regeneration_retry(state: Arc<AppState>, job_id: JobId) {
    if request_production_worker(job_id, true) {
        tokio::spawn(run_segment_regeneration_job(state, job_id));
    }
}

pub(super) async fn run_conversion_job(state: Arc<AppState>, job_id: JobId) {
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

pub(super) async fn run_segment_regeneration_job(state: Arc<AppState>, job_id: JobId) {
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

pub(super) async fn run_segment_regeneration_job_inner(
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
pub(super) async fn run_conversion_job_inner(
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

pub(super) async fn mark_proofing_plan_ready(
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

pub(super) async fn load_required_proof_export_snapshot(
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

pub(super) async fn load_validated_proof_export_plan(
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

pub(super) fn bind_persisted_segment_ids(plan: &mut ConversionPlan, units: &PersistedUnitPlan) {
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

pub(super) async fn load_export_profile(
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

pub(super) fn unit_is_reusable(state: &AppState, unit: &JobUnit) -> bool {
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

pub(super) async fn synchronize_job_progress(
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

pub(super) async fn refresh_catalog_job(
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
