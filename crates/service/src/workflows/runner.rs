use super::{
    ActiveDetectionWorker, AppState, Arc, BTreeMap, CharacterDetectionRun,
    DETECTION_JOB_SCHEMA_VERSION, DetectionBatching, DetectionContextBudget, DetectionJobConfig,
    DetectionPermission, DetectionRunId, DetectionRunStatus, Duration, Job, JobId, JobKind,
    JobState, JobUnitState, JobView, ProjectId, ProviderProfileId, ReasoningControl, RetryPolicy,
    ReviewStatus, ServiceError, Temperature, UsageWorkload, Utc, Uuid, apply_persisted_overrides,
    canonicalize_detection_result, character_views, combined_detection_results,
    complete_detection_job, completed_detection_units, consistent_detection_config,
    detection_batch_index, detection_batches_for_config, detection_request_estimate,
    detection_unit, detection_unit_view, detection_units, effective_detection_context_window,
    execute_detection_batch, fail_job, finalize_detection_unit, insert_detection_run,
    load_detection_run, load_previous_characters, mark_detection_unit, merge_characters,
    paragraph_batches, persist_detection_results, persist_detection_unit_result,
    persisted_detection_result, selected_paragraphs, storage_error, update_detection_run,
    update_job_progress, validate_detection_profile, wait_until_detection_runnable,
};

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
    let profile = state
        .catalog
        .read()
        .await
        .providers
        .get(&provider_id)
        .cloned()
        .ok_or_else(|| ServiceError::Conflict("detection provider was removed".to_owned()))?;
    let provider_mode = profile.mode;
    let context_window_tokens = effective_detection_context_window(state, &profile, &model).await?;
    let context_budget = DetectionContextBudget::new(context_window_tokens)?;
    let batches = paragraph_batches(&paragraphs, context_budget, DetectionBatching::CURRENT)?;
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
        context_window_tokens: Some(context_window_tokens),
        max_output_tokens: Some(context_budget.max_output),
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
pub(super) async fn run_character_detection_inner(
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
    let batches = detection_batches_for_config(&paragraphs, &config)?;
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

    let combined =
        canonicalize_detection_result(combined_detection_results(state, stored_job.id).await?);

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
