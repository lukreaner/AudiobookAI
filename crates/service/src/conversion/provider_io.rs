use super::{
    AppState, Arc, Artifact, ArtifactId, ArtifactKind, AttemptId, AudioFormat, BTreeMap, BoxFuture,
    Bytes, CacheFingerprint, ChapterId, Command, ContentAddressedCache, Duration, HashMap, HashSet,
    Job, JobAttempt, JobId, JobUnit, JobUnitId, NORMALIZATION_VERSION, PROVIDER_SEMAPHORES, Path,
    PathBuf, PerformanceSettings, ProjectId, ProvenanceQuality, ProviderId, ProviderKindView,
    ProviderModeView, ProviderProfileId, ProviderUsage, RateCardId, RetryEvent, RetryEventOutcome,
    RetryFailureClass, RetryJournal, RetryJournalError, RetryPolicy, SegmentPlan, Semaphore,
    ServiceError, SidecarPair, SpeakerAssignment, StdMutex, SynthesisResponse, TtsUsageContext,
    UsageEvent, UsageEventId, UsageQuantities, UsageRowView, UsageSource, UsageWorkload, Utc, Uuid,
    VoiceProfileId, artifact_for_file, internal_error, media_error, persist_artifact,
    probe_duration_ms, resolve_sidecars, run_process, storage_error,
};

pub(super) fn provider_semaphore(provider_id: Uuid, requested: u16) -> Arc<Semaphore> {
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

pub(super) fn cache(state: &AppState) -> ContentAddressedCache {
    ContentAddressedCache::new(state.catalog.try_read().map_or_else(
        |_| state.database.paths().cache.clone(),
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

pub(super) async fn active_job_cache_keys(
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

pub(super) async fn cache_keys_for_artifacts(
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

pub(super) async fn release_job_cache_pins(
    state: &AppState,
    job_id: JobId,
) -> Result<(), ServiceError> {
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

pub(super) fn segment_cache_fingerprint(
    segment: &SegmentPlan,
    operation: &str,
) -> CacheFingerprint {
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
        performance: segment.assignment.performance.clone(),
        settings,
        dictionary_revision: segment.dictionary_revision.clone(),
        normalization_version: NORMALIZATION_VERSION.to_owned(),
    }
}

pub(crate) fn segment_semantic_input_hash(segment: &SegmentPlan) -> Result<String, ServiceError> {
    semantic_input_hash(
        &segment.text,
        segment.context.as_deref(),
        segment.assignment.provider_id,
        segment.assignment.model.as_deref(),
        segment.assignment.voice_id,
        &segment.dictionary_revision,
        &segment.assignment.performance,
    )
}

pub(crate) fn semantic_input_hash(
    text: &str,
    context: Option<&str>,
    provider_id: Uuid,
    model: Option<&str>,
    voice_id: Uuid,
    dictionary_revision: &str,
    performance: &PerformanceSettings,
) -> Result<String, ServiceError> {
    let value = serde_json::json!({
        "schemaVersion": 1,
        "text": text,
        "context": context,
        "providerProfileId": provider_id,
        "model": model,
        "voiceProfileId": voice_id,
        "dictionaryRevision": dictionary_revision,
        "normalizationVersion": NORMALIZATION_VERSION,
        "performance": performance,
    });
    let bytes = serde_json::to_vec(&value).map_err(internal_error)?;
    Ok(blake3::hash(&bytes).to_hex().to_string())
}

pub(super) fn provider_endpoint_family(assignment: &SpeakerAssignment) -> &'static str {
    match (&assignment.provider_kind, assignment.provider_role) {
        (ProviderKindView::Elevenlabs, _) => "elevenlabs-v1",
        (ProviderKindView::MlxAudio, _) => "openai-audio-mlx",
        (ProviderKindView::Localai, _) => "openai-audio-localai",
        (ProviderKindView::AlltalkV2, _) => "alltalk-v2",
        (ProviderKindView::Piper, _) => "piper-cli-v1",
        (ProviderKindView::NativeOs, _) => "native-os",
        (ProviderKindView::OpenaiTts, _)
        | (ProviderKindView::Openai, Some(crate::models::ProviderRoleView::Tts)) => {
            "openai-speech-v1"
        }
        (ProviderKindView::Gemini, Some(crate::models::ProviderRoleView::Tts)) => {
            "gemini-interactions-tts-v1"
        }
        (ProviderKindView::Openai, _) => "openai",
        (ProviderKindView::OpenaiCompatible, _) => "openai-compatible",
        (ProviderKindView::Anthropic, _) => "anthropic",
        (ProviderKindView::Gemini, _) => "gemini",
        (ProviderKindView::Qwen, _) => "qwen",
        (ProviderKindView::Kimi, _) => "kimi",
        (ProviderKindView::Moonshot, _) => "moonshot",
        (ProviderKindView::LmStudio, _) => "lm-studio",
        (ProviderKindView::Ollama, _) => "ollama",
    }
}

pub(super) fn requested_audio_format(assignment: &SpeakerAssignment) -> AudioFormat {
    match (&assignment.provider_kind, assignment.provider_role) {
        (ProviderKindView::Elevenlabs | ProviderKindView::OpenaiTts, _)
        | (ProviderKindView::Openai, Some(crate::models::ProviderRoleView::Tts)) => {
            AudioFormat::Mp3
        }
        _ => AudioFormat::Wav,
    }
}

pub(super) async fn retry_policy(
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

pub(super) fn retry_reservation_multiplier(policy: &RetryPolicy) -> usize {
    if policy.retries_uncertain_charge() {
        usize::from(policy.max_attempts())
    } else {
        1
    }
}

pub(super) fn provider_mode_matches_runtime(
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

pub(super) fn persisted_provider_mode_matches(
    expected: Option<ProviderModeView>,
    current: ProviderModeView,
) -> bool {
    expected == Some(current)
}

pub(super) fn persisted_provider_snapshot_matches(
    expected: Option<Uuid>,
    current: Option<Uuid>,
) -> bool {
    expected.is_some() && expected == current
}

#[allow(clippy::too_many_lines)]
pub(super) async fn validate_regeneration_retry_provider_snapshot(
    state: &AppState,
    job: &Job,
    segment: &SegmentPlan,
) -> Result<(), ServiceError> {
    let (profile, voice_matches) = {
        let catalog = state.catalog.read().await;
        let profile = catalog
            .providers
            .get(&segment.assignment.provider_id)
            .cloned()
            .ok_or_else(|| {
                ServiceError::Conflict(
                    "the regeneration provider was removed; start a new segment regeneration"
                        .to_owned(),
                )
            })?;
        let voice_matches = catalog.voices.iter().any(|voice| {
            voice.id == segment.assignment.voice_id
                && voice.provider_profile_id == segment.assignment.provider_id
        }) && catalog
            .voice_sources
            .get(&segment.assignment.voice_id)
            .is_some_and(|source| source == &segment.assignment.voice_source);
        (profile, voice_matches)
    };
    crate::api::validate_billable_tts_provider_readiness(&profile).map_err(|_| {
        ServiceError::Conflict(
            "the regeneration provider is no longer ready; start a new segment regeneration"
                .to_owned(),
        )
    })?;
    if profile.kind != segment.assignment.provider_kind
        || !matches!(profile.role, crate::models::ProviderRoleView::Tts)
        || segment
            .assignment
            .provider_role
            .is_some_and(|role| role != profile.role)
        || profile.endpoint != segment.assignment.provider_endpoint
        || !persisted_provider_mode_matches(segment.assignment.provider_mode, profile.mode)
        || !voice_matches
    {
        return Err(ServiceError::Conflict(
            "the regeneration provider or voice changed; start a new segment regeneration"
                .to_owned(),
        ));
    }
    if segment.assignment.model.is_none() && profile.model.is_some() {
        return Err(ServiceError::Conflict(
            "the regeneration provider's default model changed; start a new segment regeneration"
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
    if matches!(profile.mode, ProviderModeView::CloudRemote) && !project.cloud_consent.book_text {
        return Err(ServiceError::Conflict(
            "cloud-text consent was revoked; start a new segment regeneration after granting consent"
                .to_owned(),
        ));
    }
    let domain_provider = state
        .database
        .repositories()
        .providers
        .get(ProviderProfileId::from_uuid(segment.assignment.provider_id))
        .await
        .map_err(storage_error)?
        .ok_or_else(|| {
            ServiceError::Conflict(
                "the regeneration provider profile no longer exists; start a new segment regeneration"
                    .to_owned(),
            )
        })?;
    let current_snapshot = domain_provider.capability_snapshot.as_ref();
    if !persisted_provider_snapshot_matches(
        segment.assignment.provider_snapshot_id,
        current_snapshot.map(|snapshot| snapshot.id.as_uuid()),
    ) || segment.assignment.provider_version
        != current_snapshot.and_then(|snapshot| snapshot.provider_version.clone())
    {
        return Err(ServiceError::Conflict(
            "the regeneration provider capability snapshot changed; start a new segment regeneration"
                .to_owned(),
        ));
    }
    let runtime_id =
        ProviderId::new(segment.assignment.provider_id.to_string()).map_err(internal_error)?;
    let runtime = state
        .providers
        .tts(&runtime_id)
        .await
        .map_err(|_| {
            ServiceError::Conflict(
                "the durable regeneration provider runtime is unavailable; start a new segment regeneration"
                    .to_owned(),
            )
        })?;
    if runtime.descriptor().id != runtime_id
        || runtime.descriptor().endpoint_family != provider_endpoint_family(&segment.assignment)
        || segment
            .assignment
            .provider_mode
            .is_none_or(|mode| !provider_mode_matches_runtime(mode, runtime.descriptor().kind))
    {
        return Err(ServiceError::Conflict(
            "the regeneration provider runtime identity changed; start a new segment regeneration"
                .to_owned(),
        ));
    }
    crate::api::validate_voice_direction(
        &segment.assignment.performance,
        &segment.assignment.timing,
        segment.assignment.model.as_deref(),
        profile.capabilities.as_ref(),
    )
    .map_err(|_| {
        ServiceError::Conflict(
            "the durable voice direction is no longer supported; start a new segment regeneration"
                .to_owned(),
        )
    })?;
    Ok(())
}

pub(super) async fn segment_project_id(
    state: &AppState,
    chapter_id: Uuid,
) -> Result<Uuid, ServiceError> {
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

pub(super) fn retry_service_error(
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
pub(super) struct AttemptJournal {
    pub(super) state: Arc<AppState>,
    pub(super) unit_id: JobUnitId,
    pub(super) usage_context: TtsUsageContext,
}

impl AttemptJournal {
    pub(super) fn new(
        state: Arc<AppState>,
        unit_id: JobUnitId,
        usage_context: TtsUsageContext,
    ) -> Self {
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

pub(super) async fn attempt_id_for_ordinal(
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

pub(super) const fn core_failure_class(class: RetryFailureClass) -> audiobookai_core::FailureClass {
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

pub(super) async fn normalize_provider_audio(
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
        .map_err(ServiceError::Io)?
        .into_temp_path();
    tokio::fs::write(&input, &response.audio).await?;
    let output = tempfile::Builder::new()
        .prefix("audiobookai-normalized-")
        .suffix(".flac")
        .tempfile()
        .map_err(ServiceError::Io)?
        .into_temp_path();
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
        input.to_string_lossy().into_owned(),
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
        output.to_string_lossy().into_owned(),
    ];
    run_process(&sidecars.ffmpeg, &arguments, "normalize provider audio").await?;
    let bytes = tokio::fs::read(&output).await?;
    if bytes.len() < 4 || &bytes[..4] != b"fLaC" {
        return Err(ServiceError::Internal(
            "FFmpeg did not produce a valid canonical FLAC segment".to_owned(),
        ));
    }
    Ok(bytes)
}

pub(super) async fn decode_flac_pcm(
    sidecars: &SidecarPair,
    path: &Path,
) -> Result<Bytes, ServiceError> {
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

pub(super) async fn ensure_cached_artifact(
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

pub(super) async fn append_tts_usage(
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
        segment_id: segment.proofing.then_some(segment.id),
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

pub(super) async fn ensure_tts_usage_row(
    state: &AppState,
    segment: &SegmentPlan,
    event: &UsageEvent,
) {
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

pub(super) fn redacted_endpoint(endpoint: Option<&str>) -> Option<String> {
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
