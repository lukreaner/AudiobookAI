use super::{
    AppState, AttemptId, BTreeMap, Character, CharacterDetectionResult, CharacterDetectionRun,
    DetectionRunStatus, DetectionSourceParagraph, FromStr, JobId, JobState, JobStatusView,
    JobUnitState, ParagraphId, ProjectId, ProjectStatus, ProvenanceQuality, ProviderProfileId,
    ProviderProfileView, ProviderUsage, RateCardId, ServiceError, UsageEvent, UsageEventId,
    UsageQuantities, UsageRowView, UsageSource, UsageWorkload, Utc, Uuid, cancel_detection_job,
    consistent_detection_config, detection_units, load_detection_run, mark_detection_unit,
    storage_error, sync_detection_job_view, transition_detection_if,
};

pub(super) async fn insert_detection_run(
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

pub(super) async fn update_detection_run(
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

pub(super) const fn detection_run_status_name(status: DetectionRunStatus) -> &'static str {
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
pub(super) async fn persist_detection_results(
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
pub(super) async fn append_detection_usage(
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

pub(super) fn safe_endpoint_family(endpoint: Option<&str>) -> String {
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

pub(super) fn merge_detection_usage(
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

pub(super) async fn ensure_detection_usage_row(
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

pub(super) async fn update_job_progress(
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

pub(super) async fn fail_job(state: &AppState, job_id: Uuid, detail: &str) {
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
