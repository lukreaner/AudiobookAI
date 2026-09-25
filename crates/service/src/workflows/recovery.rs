use super::{
    AppState, Arc, BTreeSet, CoreFailureClass, DetectionPermission, DetectionRunStatus, Duration,
    Job, JobAttempt, JobId, JobKind, JobState, JobStatusView, JobUnit, JobUnitId, JobUnitState,
    ProviderModeView, ProviderProfileId, ProviderUsage, RecoveryDecision, ServiceError,
    UsageEventId, UsageSource, UsageWorkload, Utc, Uuid, append_detection_usage,
    consistent_detection_config, core_failure_class_name, detection_config,
    detection_profile_matches_dispatch_contract, detection_provider_is_non_billable_local,
    detection_runtime_profile_matches, detection_unit_estimate, detection_unit_rate_card,
    detection_units, fail_job, load_detection_run, mark_detection_unit, persisted_detection_result,
    selected_paragraphs, spawn_character_detection, storage_error, update_detection_run,
    validate_detection_profile,
};

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
            unit.payload.remove("contextFallbackCount");
            unit.payload.remove("outputFallbackCount");
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
        unit.payload.remove("contextFallbackCount");
        unit.payload.remove("outputFallbackCount");
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
    let registered_runtime = state.providers.profile(&runtime_id).await.map_err(|_| {
        ServiceError::Conflict(
            "the durable detection provider runtime is unavailable; start a new detection job"
                .to_owned(),
        )
    })?;
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
    // Adapter descriptors identify a provider family (for example `lmstudio`), while the
    // registered runtime profile identifies the concrete durable connection. Retry validation
    // must compare the latter with the persisted profile UUID.
    if !detection_runtime_profile_matches(&runtime_id, config.provider_mode, &registered_runtime) {
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

pub(super) async fn recover_detection_job(
    state: &AppState,
    job: &Job,
) -> Result<bool, ServiceError> {
    let units = detection_units(state, job.id).await?;
    let config = match consistent_detection_config(&units) {
        Ok(config) => config,
        Err(error) => {
            fail_job(state, job.id.as_uuid(), &error.to_string()).await;
            return Ok(false);
        }
    };
    let non_billable_local = state
        .catalog
        .read()
        .await
        .providers
        .get(&config.provider_profile_id)
        .is_some_and(|profile| {
            detection_profile_matches_dispatch_contract(profile, &config)
                && detection_provider_is_non_billable_local(profile)
        });
    for mut unit in units {
        let latest = latest_detection_attempt(state, unit.id).await?;
        match recovery_decision(&unit, latest.as_ref(), non_billable_local)? {
            RecoveryDecision::Keep | RecoveryDecision::FinalizePersistedResult => {}
            RecoveryDecision::RedispatchSafe => {
                if non_billable_local && let Some(attempt) = latest.as_ref() {
                    reconcile_interrupted_non_billable_attempt(state, attempt).await?;
                }
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

pub(super) async fn append_recovered_uncertain_detection_usage(
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

pub(super) fn recovery_decision(
    unit: &JobUnit,
    latest_attempt: Option<&JobAttempt>,
    in_flight_redispatch_is_non_billable: bool,
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
        return Ok(
            if matches!(dispatch_state, "prepared" | "recovered_safe")
                || (in_flight_redispatch_is_non_billable && dispatch_state == "dispatched")
            {
                RecoveryDecision::RedispatchSafe
            } else {
                RecoveryDecision::FailUncertain
            },
        );
    };
    let dispatch_outcome_is_ambiguous = attempt.finished_at.is_none()
        || attempt.uncertain_charge
        || attempt.failure_class.is_none()
        || attempt
            .failure_class
            .is_some_and(CoreFailureClass::may_have_charged);
    if dispatch_outcome_is_ambiguous {
        return Ok(if in_flight_redispatch_is_non_billable {
            RecoveryDecision::RedispatchSafe
        } else {
            RecoveryDecision::FailUncertain
        });
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

pub(super) async fn reconcile_interrupted_non_billable_attempt(
    state: &AppState,
    attempt: &JobAttempt,
) -> Result<(), ServiceError> {
    if attempt.finished_at.is_some() && !attempt.uncertain_charge {
        return Ok(());
    }
    let mut repaired = attempt.clone();
    repaired.finished_at = Some(repaired.finished_at.unwrap_or_else(Utc::now));
    repaired.failure_class = Some(CoreFailureClass::Transport);
    repaired.error_code = Some("local_response_interrupted".to_owned());
    repaired.redacted_error =
        Some("local provider response was interrupted before it could be persisted".to_owned());
    repaired.uncertain_charge = false;
    let result = sqlx::query(
        "UPDATE job_attempts SET finished_at = ?, failure_class = ?, uncertain_charge = 0, payload = ? \
         WHERE id = ? AND job_unit_id = ?",
    )
    .bind(repaired.finished_at.map(|value| value.to_rfc3339()))
    .bind(core_failure_class_name(CoreFailureClass::Transport))
    .bind(
        serde_json::to_string(&repaired)
            .map_err(|error| ServiceError::Internal(error.to_string()))?,
    )
    .bind(repaired.id.to_string())
    .bind(repaired.job_unit_id.to_string())
    .execute(state.database.pool())
    .await
    .map_err(storage_error)?;
    if result.rows_affected() != 1 {
        return Err(ServiceError::Conflict(
            "the interrupted local detection attempt changed during recovery".to_owned(),
        ));
    }
    Ok(())
}

pub(super) async fn latest_detection_attempt(
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

pub(super) async fn wait_until_detection_runnable(
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

pub(super) async fn pause_detection_units(
    state: &AppState,
    job_id: JobId,
) -> Result<(), ServiceError> {
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

pub(super) async fn cancel_detection_job(
    state: &AppState,
    job_id: JobId,
) -> Result<(), ServiceError> {
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

pub(super) async fn transition_detection_if(
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

pub(super) async fn complete_detection_job(
    state: &AppState,
    job_id: JobId,
) -> Result<bool, ServiceError> {
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

pub(super) async fn sync_detection_job_view(state: &AppState, job: &Job) {
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

pub(super) fn progress_fraction(completed: u64, total: u64) -> f32 {
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
