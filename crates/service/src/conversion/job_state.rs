use super::{
    AppState, Duration, HashSet, Job, JobId, JobKind, JobState, JobStatusView, JobUnit,
    JobUnitKind, JobUnitState, OUTPUT_ADMISSION_LOCK, OutputDestinationReservation, Path,
    ProviderProfileId, ReservationId, SegmentPlan, ServiceError, StorageError, UsageQuantities,
    UsageWorkload, Utc, artifact_path, ensure_output_directory_not_reserved, export_destination,
    internal_error, job_status_view, load_conversion_plan, load_export_profile, load_unit_plan,
    output_reservation_admission_error, prepare_output_reservation, progress_ratio,
    require_output_reservation, retry_policy, retry_reservation_multiplier, storage_error,
    unit_view, validate_regeneration_retry_provider_snapshot,
};

pub(super) async fn transition_job(
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
pub(super) async fn transition_export_job_with_reservation(
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
pub(super) async fn retry_billable_estimates(
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

pub(super) async fn reset_non_detection_retry_units(
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

pub(super) async fn prepare_retry_output_claim(
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

pub(super) fn retry_admission_error(error: StorageError) -> ServiceError {
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

pub(super) async fn admit_failed_job_retry(
    state: &AppState,
    job_id: JobId,
) -> Result<Job, ServiceError> {
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

pub(super) async fn set_job_message(
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

pub(super) async fn update_unit_state(
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

pub(super) async fn increment_job_progress(
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

pub(super) async fn wait_until_runnable(
    state: &AppState,
    job_id: JobId,
) -> Result<(), ServiceError> {
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

pub(super) async fn update_staged_job_failure(state: &AppState, job_id: JobId, message: &str) {
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

pub(super) async fn release_unattached_reservation(
    state: &AppState,
    reservation_id: ReservationId,
) {
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

pub(super) async fn release_completed_output_reservation(state: &AppState, job_id: JobId) {
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

pub(super) async fn fail_interrupted_paid_job(
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

pub(super) async fn mark_domain_job_failed(state: &AppState, job_id: JobId, message: &str) {
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

pub(super) async fn mark_job_failed(state: &AppState, job_id: JobId, message: &str) {
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

pub(super) async fn mark_job_units_failed(state: &AppState, job_id: JobId, message: &str) {
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

pub(super) async fn reconcile_job_budgets(
    state: &AppState,
    job_id: JobId,
) -> Result<(), ServiceError> {
    crate::accounting::finalize_job_reservation(state, job_id).await
}
