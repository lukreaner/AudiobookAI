use super::{
    AppState, BTreeMap, BTreeSet, CharacterDetectionRequest, CharacterDetectionResult,
    CharacterDetectionRun, DETECTION_JOB_SCHEMA_VERSION, DETECTION_REPAIR_SUFFIX,
    DETECTION_SYSTEM_PROMPT, DETECTION_TOKEN_AWARE_SCHEMA_VERSION, DetectionJobConfig,
    DetectionParagraph, DetectionRunId, JobId, JobStageView, JobUnit, JobUnitId, JobUnitKind,
    JobUnitState, JobUnitStatusView, JobUnitView, PersistedDetectionResult, ProviderModeView,
    ProviderProfileId, ProviderProfileView, ProviderStatusView, ProviderUsage, RateCardId,
    ReasoningControl, ServiceError, Temperature, UsageEventId, UsageQuantities, Utc, Uuid,
    append_detection_usage, latest_detection_attempt, storage_error,
};

pub(super) fn detection_request(
    model: &str,
    paragraphs: &[DetectionParagraph],
    max_output_tokens: u32,
    repair: bool,
    temperature: Temperature,
    reasoning: ReasoningControl,
) -> CharacterDetectionRequest {
    CharacterDetectionRequest {
        request_id: Uuid::new_v4(),
        model: model.to_owned(),
        system_prompt: if repair {
            format!("{DETECTION_SYSTEM_PROMPT}\n\n{DETECTION_REPAIR_SUFFIX}")
        } else {
            DETECTION_SYSTEM_PROMPT.to_owned()
        },
        paragraphs: paragraphs.to_vec(),
        temperature,
        reasoning,
        max_output_tokens,
    }
}

pub(super) fn detection_unit(
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

pub(super) fn detection_unit_rate_card(unit: &JobUnit) -> Result<Option<RateCardId>, ServiceError> {
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

pub(super) fn detection_unit_estimate(unit: &JobUnit) -> Result<UsageQuantities, ServiceError> {
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

pub(super) fn detection_unit_view(unit: &JobUnit) -> JobUnitView {
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

pub(super) fn detection_batch_index(unit: &JobUnit) -> Option<usize> {
    unit.payload
        .get("batchIndex")
        .and_then(serde_json::Value::as_u64)
        .and_then(|value| usize::try_from(value).ok())
}

pub(super) fn detection_config(unit: &JobUnit) -> Result<DetectionJobConfig, ServiceError> {
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
    if !matches!(
        config.schema_version,
        2 | 3 | 4 | DETECTION_TOKEN_AWARE_SCHEMA_VERSION | DETECTION_JOB_SCHEMA_VERSION
    ) {
        return Err(ServiceError::Conflict(format!(
            "unsupported detection job schema version {}",
            config.schema_version
        )));
    }
    Ok(config)
}

pub(super) fn consistent_detection_config(
    units: &[JobUnit],
) -> Result<DetectionJobConfig, ServiceError> {
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

pub(super) fn validate_detection_profile(
    profile: &ProviderProfileView,
    config: &DetectionJobConfig,
) -> Result<(), ServiceError> {
    if !detection_profile_matches_dispatch_contract(profile, config) {
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

pub(super) fn detection_profile_matches_dispatch_contract(
    profile: &ProviderProfileView,
    config: &DetectionJobConfig,
) -> bool {
    profile.id == config.provider_profile_id
        && matches!(profile.role, crate::models::ProviderRoleView::Llm)
        && profile.model.as_deref() == Some(config.model.as_str())
        && profile.endpoint == config.provider_endpoint
        && Some(profile.mode) == config.provider_mode
}

pub(super) fn detection_provider_is_non_billable_local(profile: &ProviderProfileView) -> bool {
    matches!(
        &profile.kind,
        crate::models::ProviderKindView::LmStudio | crate::models::ProviderKindView::Ollama
    ) && matches!(
        profile.mode,
        ProviderModeView::ExternalEndpoint | ProviderModeView::ManagedChild
    )
}

pub(super) fn detection_runtime_mode_matches(
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

pub(super) fn detection_runtime_profile_matches(
    expected_id: &audiobookai_providers::ProviderId,
    expected_mode: Option<ProviderModeView>,
    runtime: &crate::runtime::RuntimeProfile,
) -> bool {
    runtime.id == *expected_id
        && expected_mode.is_some_and(|mode| detection_runtime_mode_matches(mode, runtime.mode))
}

pub(super) async fn detection_units(
    state: &AppState,
    job_id: JobId,
) -> Result<Vec<JobUnit>, ServiceError> {
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

pub(super) fn persisted_detection_result(
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

pub(super) async fn completed_detection_units(
    state: &AppState,
    job_id: JobId,
) -> Result<usize, ServiceError> {
    Ok(detection_units(state, job_id)
        .await?
        .into_iter()
        .filter(|unit| unit.state == JobUnitState::Completed)
        .count())
}

pub(super) async fn combined_detection_results(
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

pub(super) async fn mark_detection_unit(
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

pub(super) async fn persist_detection_unit_result(
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

pub(super) async fn finalize_detection_unit(
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

pub(super) async fn load_detection_run(
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
