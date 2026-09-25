use super::{
    AppState, Arc, Artifact, ArtifactId, ArtifactKind, AttemptJournal, BTreeMap, Chapter, Job,
    JobId, JobKind, JobState, JobUnit, JobUnitId, JobUnitKind, JobUnitState,
    MAX_PREVIEW_CHARACTERS, NORMALIZATION_VERSION, Paragraph, Path, PreviewView, Project,
    ProjectId, ProviderError, ProviderId, ProviderProfileId, SegmentId, SegmentPlan, ServiceError,
    SynthesisRequest, TtsUsageContext, UsageQuantities, UsageWorkload, Utc, Uuid,
    VoiceAssignmentView, append_tts_usage, apply_pronunciation_rules, artifact_for_file_with_id,
    attempt_id_for_ordinal, build_assignments, cache, enforce_cache_limit, execute_with_retry,
    internal_error, mark_domain_job_failed, media_error, normalize_provider_audio,
    persist_artifact, probe_duration_ms, provider_semaphore, redacted_endpoint,
    release_job_cache_pins, release_unattached_reservation, requested_audio_format,
    resolve_sidecars, retry_policy, retry_reservation_multiplier, retry_service_error,
    segment_cache_fingerprint, segment_key, storage_error, transition_job, update_unit_state,
    validate_segment_dispatch_boundary,
};

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
pub(super) async fn preview_with_assignment(
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

pub(super) async fn first_selected_paragraph(
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
