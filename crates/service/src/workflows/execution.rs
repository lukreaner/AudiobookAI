use super::{
    AppState, Arc, AttemptId, BTreeMap, BoxFuture, CharacterDetectionResult, ChronoDuration,
    CoreFailureClass, DETECTION_CONTEXT_FALLBACK_LIMIT, DETECTION_OUTPUT_FALLBACK_LIMIT,
    DetectionBatch, DetectionJobConfig, FailureClass, JobAttempt, JobId, JobState, JobUnit,
    JobUnitId, JobUnitState, ProviderError, ProviderModeView, ProviderProfileId,
    ProviderProfileView, ProviderStatusView, ProviderUsage, RateCardId, RetryEvent,
    RetryEventOutcome, RetryJournal, RetryJournalError, RetryPolicy, ServiceError, UsageEventId,
    UsageQuantities, UsageSource, UsageWorkload, Utc, Uuid, VecDeque, append_detection_usage,
    detection_provider_is_non_billable_local, detection_request, detection_request_estimate,
    detection_unit_rate_card, execute_with_retry, load_detection_run, mark_detection_unit,
    storage_error, update_detection_run,
};

#[allow(clippy::too_many_arguments, clippy::too_many_lines)]
pub(super) async fn execute_detection_batch(
    state: &Arc<AppState>,
    provider: &Arc<dyn audiobookai_providers::CharacterProvider>,
    policy: &RetryPolicy,
    config: &DetectionJobConfig,
    batch: &DetectionBatch,
    unit: &mut JobUnit,
    profile: &ProviderProfileView,
    project_id: Uuid,
) -> Result<Option<CharacterDetectionResult>, ServiceError> {
    let initial_repair = unit
        .payload
        .get("needsRepair")
        .and_then(serde_json::Value::as_bool)
        .unwrap_or(false);
    let mut pending = VecDeque::from([(batch.clone(), initial_repair)]);
    let mut context_fallbacks = unit
        .payload
        .get("contextFallbackCount")
        .and_then(serde_json::Value::as_u64)
        .and_then(|value| usize::try_from(value).ok())
        .unwrap_or_default();
    let mut output_fallbacks = unit
        .payload
        .get("outputFallbackCount")
        .and_then(serde_json::Value::as_u64)
        .and_then(|value| usize::try_from(value).ok())
        .unwrap_or_default();
    let mut combined = CharacterDetectionResult {
        characters: Vec::new(),
        dialogue: Vec::new(),
        usage: ProviderUsage::default(),
    };
    let mut completed_requests = 0_usize;
    while let Some((current_batch, mut repair)) = pending.pop_front() {
        loop {
            if repair {
                let mut run = load_detection_run(state, config.detection_run_id).await?;
                if !run.repair_attempted {
                    run.repair_attempted = true;
                    update_detection_run(state, &run).await?;
                }
            }
            let request_paragraphs = current_batch.request_paragraphs();
            let request = detection_request(
                &config.model,
                &request_paragraphs,
                current_batch.max_output_tokens,
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
            let request_estimate = detection_request_estimate(&current_batch, &config.reasoning);
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
                Ok(execution) => {
                    let result = rebase_detection_result(execution.value, &current_batch)?;
                    append_detection_result(&mut combined, result, completed_requests > 0);
                    completed_requests = completed_requests.saturating_add(1);
                    break;
                }
                Err(crate::runtime::RetryExecutionError::Provider {
                    source: ProviderError::ContextWindowExceeded,
                    ..
                }) => {
                    context_fallbacks = context_fallbacks.saturating_add(1);
                    if context_fallbacks > DETECTION_CONTEXT_FALLBACK_LIMIT {
                        return Err(ServiceError::Conflict(
                            "the provider context window remains too small after adaptive batching"
                                .to_owned(),
                        ));
                    }
                    let children = current_batch.split_for_context_retry()?;
                    unit.payload.insert(
                        "contextFallbackCount".to_owned(),
                        serde_json::json!(context_fallbacks),
                    );
                    unit.payload.remove("requestId");
                    unit.payload.insert(
                        "dispatchState".to_owned(),
                        serde_json::json!("context_rebatch"),
                    );
                    mark_detection_unit(state, unit, JobUnitState::Running, None).await?;
                    for child in children.into_iter().rev() {
                        pending.push_front((child, repair));
                    }
                    break;
                }
                Err(crate::runtime::RetryExecutionError::Provider {
                    source: ProviderError::OutputTruncated,
                    ..
                }) => {
                    output_fallbacks = output_fallbacks.saturating_add(1);
                    if output_fallbacks > DETECTION_OUTPUT_FALLBACK_LIMIT {
                        return Err(ServiceError::Conflict(
                            "the provider output remains incomplete after adaptive batching"
                                .to_owned(),
                        ));
                    }
                    let children = current_batch.split_for_output_retry()?;
                    unit.payload.insert(
                        "outputFallbackCount".to_owned(),
                        serde_json::json!(output_fallbacks),
                    );
                    unit.payload.remove("requestId");
                    unit.payload.insert(
                        "dispatchState".to_owned(),
                        serde_json::json!("output_rebatch"),
                    );
                    mark_detection_unit(state, unit, JobUnitState::Running, None).await?;
                    for child in children.into_iter().rev() {
                        pending.push_front((child, repair));
                    }
                    break;
                }
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
    Ok(Some(combined))
}

pub(super) fn rebase_detection_result(
    mut result: CharacterDetectionResult,
    batch: &DetectionBatch,
) -> Result<CharacterDetectionResult, ServiceError> {
    let paragraphs = batch
        .paragraphs
        .iter()
        .map(|paragraph| (paragraph.fragment.request_id.as_str(), paragraph))
        .collect::<BTreeMap<_, _>>();
    let mut dialogue = Vec::with_capacity(result.dialogue.len());
    for mut span in result.dialogue {
        let paragraph = paragraphs.get(span.paragraph_id.as_str()).ok_or_else(|| {
            ServiceError::Conflict(
                "provider returned dialogue for an unknown paragraph fragment".to_owned(),
            )
        })?;
        if paragraph.context_only {
            continue;
        }
        let source_start = u32::try_from(paragraph.fragment.source_byte_start).map_err(|_| {
            ServiceError::Conflict("paragraph byte offset exceeds the supported range".to_owned())
        })?;
        span.start = span.start.checked_add(source_start).ok_or_else(|| {
            ServiceError::Conflict("dialogue byte offset exceeds the supported range".to_owned())
        })?;
        span.end = span.end.checked_add(source_start).ok_or_else(|| {
            ServiceError::Conflict("dialogue byte offset exceeds the supported range".to_owned())
        })?;
        span.paragraph_id.clone_from(&paragraph.fragment.source_id);
        dialogue.push(span);
    }
    result.dialogue = dialogue;
    Ok(result)
}

pub(super) fn append_detection_result(
    combined: &mut CharacterDetectionResult,
    mut result: CharacterDetectionResult,
    has_prior_usage: bool,
) {
    combined.characters.append(&mut result.characters);
    combined.dialogue.append(&mut result.dialogue);
    append_provider_usage(&mut combined.usage, result.usage, has_prior_usage);
}

pub(super) fn append_provider_usage(
    combined: &mut ProviderUsage,
    next: ProviderUsage,
    has_prior_usage: bool,
) {
    if !has_prior_usage {
        *combined = next;
        return;
    }
    let sum = |left: Option<u64>, right: Option<u64>| {
        left.zip(right)
            .map(|(left, right)| left.saturating_add(right))
    };
    combined.characters = sum(combined.characters, next.characters);
    combined.audio_milliseconds = sum(combined.audio_milliseconds, next.audio_milliseconds);
    combined.input_tokens = sum(combined.input_tokens, next.input_tokens);
    combined.output_tokens = sum(combined.output_tokens, next.output_tokens);
    combined.cached_tokens = sum(combined.cached_tokens, next.cached_tokens);
    combined.reasoning_tokens = sum(combined.reasoning_tokens, next.reasoning_tokens);
    combined.credits_micros = combined
        .credits_micros
        .zip(next.credits_micros)
        .map(|(left, right)| left.saturating_add(right));
    combined.source = match (combined.source, next.source) {
        (UsageSource::Reported, UsageSource::Reported) => UsageSource::Reported,
        (UsageSource::Estimated, UsageSource::Estimated) => UsageSource::Estimated,
        _ => UsageSource::Unknown,
    };
    combined.request_id = None;
    combined.raw_redacted = None;
}

pub(super) async fn detection_dispatch_guard(
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

pub(super) const fn detection_state_allows_dispatch(state: JobState) -> bool {
    matches!(state, JobState::Running)
}

pub(super) async fn durable_attempt_offset(
    state: &AppState,
    unit_id: JobUnitId,
) -> Result<u16, ServiceError> {
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
pub(super) struct DetectionUsageContext {
    pub(super) project_id: Uuid,
    pub(super) job_id: JobId,
    pub(super) profile: ProviderProfileView,
    pub(super) request_estimate: UsageQuantities,
    pub(super) provider_request_id: Uuid,
    pub(super) rate_card_id: Option<RateCardId>,
}

#[derive(Clone)]
pub(super) struct SqliteRetryJournal {
    pub(super) state: Arc<AppState>,
    pub(super) unit_id: JobUnitId,
    pub(super) attempt_offset: u16,
    pub(super) usage_context: DetectionUsageContext,
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
    pub(super) fn ordinal(
        &self,
        attempt: crate::runtime::AttemptNumber,
    ) -> Result<u16, RetryJournalError> {
        self.attempt_offset
            .checked_add(attempt.get())
            .ok_or_else(|| RetryJournalError::new("detection attempt count overflow"))
    }

    pub(super) async fn record_dispatch_started(
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
                // Paid/unknown providers remain uncertain until a response is durably
                // classified. Built-in local LLM runtimes cannot create a provider charge.
                uncertain_charge: !detection_provider_is_non_billable_local(
                    &self.usage_context.profile,
                ),
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

pub(super) const fn core_failure_class(class: FailureClass) -> CoreFailureClass {
    match class {
        FailureClass::Transient => CoreFailureClass::Transport,
        FailureClass::RateLimited => CoreFailureClass::RateLimit,
        FailureClass::Authentication => CoreFailureClass::Authentication,
        FailureClass::Validation | FailureClass::Permanent => CoreFailureClass::Validation,
        FailureClass::UncertainCharge => CoreFailureClass::TimeoutAfterDispatch,
        FailureClass::Cancelled => CoreFailureClass::Cancelled,
    }
}

pub(super) const fn core_failure_class_name(class: CoreFailureClass) -> &'static str {
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
