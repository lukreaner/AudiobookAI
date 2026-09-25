use super::{
    AppState, Arc, Artifact, ArtifactId, ArtifactKind, AsyncReadExt, AsyncWriteExt, AtomicBool,
    AttemptJournal, AudioChunk, AudioChunkSink, AudioFormat, BTreeMap, Bytes, BytesMut,
    CancellationFlag, Command, FileFingerprint, JobId, JobUnit, JobUnitState,
    NORMALIZATION_VERSION, Ordering, Path, ProviderError, ProviderId, ProviderProfileId,
    ProviderUsage, RANGE_CHUNK_BYTES, SegmentArtifact, SegmentPlan, SegmentSelection, SegmentTake,
    SegmentTakeId, ServiceError, SidecarPair, Stdio, StreamingSynthesisResponse, SynthesisRequest,
    SynthesisResponse, TtsProvider, TtsUsageContext, UsageQuantities, UsageWorkload, Utc, Uuid,
    VoiceProfileId, append_tts_usage, artifact_for_file_with_id, async_trait,
    attempt_id_for_ordinal, cache, complete_playback_segment, complete_playback_with_pcm,
    copy_file_atomically, decode_flac_pcm, ensure_cached_artifact, execute_with_retry,
    fingerprint_file, increment_job_progress, internal_error, load_artifact, media_error, mpsc,
    normalize_provider_audio, persist_artifact, playback_listener_count, probe_duration_ms,
    provider_semaphore, publish_playback_chunk, redacted_endpoint, requested_audio_format,
    reset_playback_segment, retry_policy, retry_service_error, segment_cache_fingerprint,
    segment_project_id, segment_semantic_input_hash, set_job_message, storage_error,
    update_unit_state, validate_segment_dispatch_boundary, verify_selected_artifact_integrity,
    wait_until_runnable,
};

#[derive(Debug)]
pub(super) struct ProviderStreamSink {
    pub(super) request_id: Uuid,
    pub(super) format: AudioFormat,
    pub(super) next_sequence: tokio::sync::Mutex<u64>,
    pub(super) audio: tokio::sync::Mutex<BytesMut>,
    pub(super) decoder: tokio::sync::Mutex<Option<mpsc::Sender<Bytes>>>,
    pub(super) final_seen: AtomicBool,
}

impl ProviderStreamSink {
    pub(super) fn new(
        request_id: Uuid,
        format: AudioFormat,
        decoder: Option<mpsc::Sender<Bytes>>,
    ) -> Self {
        Self {
            request_id,
            format,
            next_sequence: tokio::sync::Mutex::new(0),
            audio: tokio::sync::Mutex::new(BytesMut::new()),
            decoder: tokio::sync::Mutex::new(decoder),
            final_seen: AtomicBool::new(false),
        }
    }

    pub(super) async fn finish(&self) -> Result<Bytes, ProviderError> {
        self.decoder.lock().await.take();
        if !self.final_seen.load(Ordering::Acquire) {
            return Err(ProviderError::InvalidResponse(
                "streaming TTS ended without a final chunk".to_owned(),
            ));
        }
        let audio = self.audio.lock().await;
        if audio.is_empty() {
            return Err(ProviderError::InvalidResponse(
                "streaming TTS returned no audio".to_owned(),
            ));
        }
        Ok(Bytes::copy_from_slice(&audio))
    }

    pub(super) async fn abort(&self) {
        self.decoder.lock().await.take();
    }
}

#[async_trait]
impl AudioChunkSink for ProviderStreamSink {
    async fn send(&self, chunk: AudioChunk) -> Result<(), ProviderError> {
        if chunk.request_id != self.request_id || chunk.format != self.format {
            return Err(ProviderError::InvalidResponse(
                "streaming TTS changed request identity or audio format".to_owned(),
            ));
        }
        if self.final_seen.load(Ordering::Acquire) {
            return Err(ProviderError::InvalidResponse(
                "streaming TTS sent data after its final chunk".to_owned(),
            ));
        }
        let mut next_sequence = self.next_sequence.lock().await;
        if chunk.sequence != *next_sequence {
            return Err(ProviderError::InvalidResponse(format!(
                "streaming TTS chunk sequence {} arrived while {} was expected",
                chunk.sequence, *next_sequence
            )));
        }
        *next_sequence = next_sequence.saturating_add(1);
        drop(next_sequence);

        if !chunk.data.is_empty() {
            self.audio.lock().await.extend_from_slice(&chunk.data);
            let decoder = self.decoder.lock().await.clone();
            if let Some(decoder) = decoder {
                // Playback decoding is best effort. A failed decoder must not discard a paid,
                // otherwise valid provider response; canonical normalization still validates it.
                let _ = decoder.send(chunk.data).await;
            }
        }
        if chunk.final_chunk {
            self.final_seen.store(true, Ordering::Release);
            self.decoder.lock().await.take();
        }
        Ok(())
    }
}

#[derive(Debug)]
pub(super) struct StreamedSynthesis {
    pub(super) response: SynthesisResponse,
    pub(super) progressive_decode_complete: bool,
}

#[derive(Debug)]
pub(super) enum ProviderSynthesisDispatch {
    Complete(StreamedSynthesis),
    Streaming {
        metadata: StreamingSynthesisResponse,
        sink: Arc<ProviderStreamSink>,
        decoder_task: Option<tokio::task::JoinHandle<bool>>,
        job_id: JobId,
        playback_ordinal: usize,
    },
}

impl ProviderSynthesisDispatch {
    pub(super) fn usage(&self) -> &ProviderUsage {
        match self {
            Self::Complete(streamed) => &streamed.response.usage,
            Self::Streaming { metadata, .. } => &metadata.usage,
        }
    }
}

/// Performs only the provider-owned portion of synthesis.
///
/// A successful return is the billing boundary consumed by the retry journal. Streaming sink
/// validation and local decoder completion deliberately happen later so they cannot turn a paid
/// provider success into a retryable provider failure.
pub(super) async fn dispatch_provider_audio(
    provider: Arc<dyn TtsProvider>,
    request: SynthesisRequest,
    sidecars: &SidecarPair,
    job_id: JobId,
    playback_ordinal: usize,
) -> Result<ProviderSynthesisDispatch, ProviderError> {
    if !provider.capabilities().streaming {
        return provider.synthesize(request).await.map(|response| {
            ProviderSynthesisDispatch::Complete(StreamedSynthesis {
                response,
                progressive_decode_complete: false,
            })
        });
    }

    // Start decoding with the provider request rather than waiting for a subscriber. A listener
    // that connects halfway through synthesis can then receive the remaining live chunks.
    let (decoder_sender, decoder_task) = match spawn_stream_playback_decoder(
        sidecars,
        job_id,
        playback_ordinal,
    ) {
        Ok((sender, task)) => (Some(sender), Some(task)),
        Err(error) => {
            tracing::warn!(diagnostic_code = "playback.decoder.start.failed", %job_id, %error, "could not start progressive playback decoder");
            (None, None)
        }
    };
    let sink = Arc::new(ProviderStreamSink::new(
        request.request_id,
        request.format,
        decoder_sender,
    ));
    let metadata = match provider
        .synthesize_stream(
            request,
            CancellationFlag::default(),
            Arc::clone(&sink) as Arc<dyn AudioChunkSink>,
        )
        .await
    {
        Ok(metadata) => metadata,
        Err(error) => {
            sink.abort().await;
            if let Some(decoder) = decoder_task {
                let _ = decoder.await;
            }
            reset_playback_segment(job_id, playback_ordinal);
            return Err(error);
        }
    };
    Ok(ProviderSynthesisDispatch::Streaming {
        metadata,
        sink,
        decoder_task,
        job_id,
        playback_ordinal,
    })
}

pub(super) async fn finish_provider_audio(
    dispatch: ProviderSynthesisDispatch,
) -> Result<StreamedSynthesis, ProviderError> {
    match dispatch {
        ProviderSynthesisDispatch::Complete(streamed) => Ok(streamed),
        ProviderSynthesisDispatch::Streaming {
            metadata,
            sink,
            decoder_task,
            job_id,
            playback_ordinal,
        } => {
            let audio = match sink.finish().await {
                Ok(audio) => audio,
                Err(error) => {
                    if let Some(decoder) = decoder_task {
                        let _ = decoder.await;
                    }
                    reset_playback_segment(job_id, playback_ordinal);
                    return Err(error);
                }
            };
            let progressive_decode_complete = if let Some(decoder) = decoder_task {
                decoder.await.unwrap_or(false)
            } else {
                false
            };
            Ok(StreamedSynthesis {
                response: SynthesisResponse {
                    audio,
                    content_type: metadata.content_type,
                    usage: metadata.usage,
                },
                progressive_decode_complete,
            })
        }
    }
}

pub(super) fn spawn_stream_playback_decoder(
    sidecars: &SidecarPair,
    job_id: JobId,
    playback_ordinal: usize,
) -> Result<(mpsc::Sender<Bytes>, tokio::task::JoinHandle<bool>), ProviderError> {
    let mut child = Command::new(&sidecars.ffmpeg)
        .args([
            "-hide_banner",
            "-loglevel",
            "error",
            "-nostdin",
            "-i",
            "pipe:0",
            "-vn",
            "-ar",
            "48000",
            "-ac",
            "1",
            "-f",
            "f32le",
            "pipe:1",
        ])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .kill_on_drop(true)
        .spawn()
        .map_err(|error| ProviderError::Transport(error.to_string()))?;
    let mut stdin = child.stdin.take().ok_or_else(|| {
        ProviderError::Transport("FFmpeg playback stdin is unavailable".to_owned())
    })?;
    let mut stdout = child.stdout.take().ok_or_else(|| {
        ProviderError::Transport("FFmpeg playback stdout is unavailable".to_owned())
    })?;
    let (sender, mut receiver) = mpsc::channel::<Bytes>(8);
    let task = tokio::spawn(async move {
        let result = async {
            let writer = async {
                while let Some(chunk) = receiver.recv().await {
                    stdin.write_all(&chunk).await?;
                }
                stdin.shutdown().await
            };
            let reader = async {
                let mut buffer = vec![0_u8; RANGE_CHUNK_BYTES];
                let mut pending = BytesMut::new();
                loop {
                    let read = stdout.read(&mut buffer).await?;
                    if read == 0 {
                        break;
                    }
                    pending.extend_from_slice(&buffer[..read]);
                    let aligned = pending.len() - (pending.len() % std::mem::size_of::<f32>());
                    if aligned > 0 {
                        publish_playback_chunk(
                            job_id,
                            playback_ordinal,
                            pending.split_to(aligned).freeze(),
                        );
                    }
                }
                if pending.is_empty() {
                    Ok(())
                } else {
                    Err(std::io::Error::new(
                        std::io::ErrorKind::InvalidData,
                        "FFmpeg produced a partial float sample",
                    ))
                }
            };
            tokio::try_join!(writer, reader)?;
            let status = child.wait().await?;
            if status.success() {
                Ok(())
            } else {
                Err(std::io::Error::other(
                    "FFmpeg could not decode provider stream",
                ))
            }
        }
        .await;
        if let Err(error) = &result {
            tracing::warn!(diagnostic_code = "playback.decoder.stopped", %job_id, %error, "progressive provider audio decoder stopped");
        }
        result.is_ok()
    });
    Ok((sender, task))
}

// Reservation verification, retry accounting, cache promotion, and playback
// completion form one dispatch transaction and must retain this order.
#[allow(clippy::too_many_lines)]
pub(super) async fn synthesize_segment(
    state: &Arc<AppState>,
    job_id: JobId,
    segment: SegmentPlan,
    mut unit: JobUnit,
    sidecars: &SidecarPair,
    progress_guard: &tokio::sync::Mutex<()>,
) -> Result<SegmentArtifact, ServiceError> {
    wait_until_runnable(state, job_id).await?;
    update_unit_state(state, &mut unit, JobUnitState::Running, None).await?;
    set_job_message(
        state,
        job_id,
        &format!(
            "Synthesizing {} with {}",
            segment.chapter_title, segment.assignment.provider_name
        ),
    )
    .await?;
    let cache = cache(state);
    let cache_operation = unit
        .payload
        .get("cacheOperation")
        .and_then(serde_json::Value::as_str)
        .unwrap_or("conversion");
    let fingerprint = segment_cache_fingerprint(&segment, cache_operation);
    let cache_key = fingerprint
        .key()
        .map_err(|error| ServiceError::Internal(error.to_string()))?;
    let (artifact, progressive_decode_complete) = if cache.contains(&cache_key) {
        cache.pin(&cache_key).map_err(media_error)?;
        (
            ensure_cached_artifact(
                state,
                job_id,
                segment.chapter_id,
                &cache_key,
                ArtifactKind::SegmentAudio,
            )
            .await?,
            false,
        )
    } else {
        let runtime_id =
            ProviderId::new(segment.assignment.provider_id.to_string()).map_err(internal_error)?;
        let provider = state
            .providers
            .tts(&runtime_id)
            .await
            .map_err(|error| ServiceError::Conflict(error.to_string()))?;
        let semaphore = provider_semaphore(
            segment.assignment.provider_id,
            segment.assignment.provider_concurrency,
        );
        let _permit = semaphore
            .acquire_owned()
            .await
            .map_err(|_| ServiceError::Internal("provider semaphore closed".to_owned()))?;
        wait_until_runnable(state, job_id).await?;
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
        let playback_ordinal = segment.playback_ordinal;
        let dispatch_estimate = crate::accounting::rate_usage_estimate(
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
        let policy = retry_policy(state, &segment).await?;
        let dispatch_project_id = segment_project_id(state, segment.chapter_id).await?;
        let dispatch_consent_lock = state
            .dispatch_consent_lifecycle_lock(dispatch_project_id)
            .await;
        let journal = AttemptJournal::new(
            Arc::clone(state),
            unit.id,
            TtsUsageContext {
                job_id,
                segment: segment.clone(),
                provider_request_id: request.request_id,
                rate_card_id: dispatch_estimate.rate_card_id,
            },
        );
        let execution = execute_with_retry(&policy, &journal, |_| {
            let state = Arc::clone(state);
            let provider = Arc::clone(&provider);
            let request = request.clone();
            let sidecars = sidecars.clone();
            let dispatch_estimate = dispatch_estimate.clone();
            let dispatch_consent_lock = Arc::clone(&dispatch_consent_lock);
            let dispatch_segment = segment.clone();
            async move {
                let _dispatch_consent_guard = dispatch_consent_lock.read().await;
                validate_segment_dispatch_boundary(&state, dispatch_project_id, &dispatch_segment)
                    .await?;
                crate::accounting::verify_dispatch_is_reserved(&state, job_id, &dispatch_estimate)
                    .await
                    .map_err(|_| {
                        ProviderError::Configuration(
                            "the active hard-budget reservation does not permit this dispatch"
                                .to_owned(),
                        )
                    })?;
                dispatch_provider_audio(provider, request, &sidecars, job_id, playback_ordinal)
                    .await
            }
        })
        .await
        .map_err(|error| retry_service_error(state, job_id, &segment, &error))?;
        unit.attempt_count = execution.attempts.get();
        let successful_attempt_id =
            attempt_id_for_ordinal(state, unit.id, execution.attempts.get()).await?;
        let dispatch = execution.value;
        // The provider has already accepted and completed a potentially billable request. Record
        // its usage before any local decoding, cache, probe, or artifact operation can fail.
        let mut usage = dispatch.usage().clone();
        if usage.request_id.is_none() {
            usage.request_id = Some(request.request_id.to_string());
        }
        append_tts_usage(
            state,
            job_id,
            &segment,
            successful_attempt_id,
            &usage,
            false,
            dispatch_estimate.rate_card_id,
        )
        .await?;
        let streamed = finish_provider_audio(dispatch)
            .await
            .map_err(|error| ServiceError::Conflict(error.to_string()))?;
        let response = streamed.response;
        let flac = normalize_provider_audio(sidecars, &response, segment.text.chars().count() < 50)
            .await?;
        let artifact_id = ArtifactId::new();
        let manifest = serde_json::json!({
            "schemaVersion": 1,
            "artifactId": artifact_id,
            "cacheKey": cache_key.as_str(),
            "textHash": blake3::hash(segment.text.as_bytes()).to_hex().to_string(),
            "contextHash": segment.context.as_deref().map_or_else(String::new, |value| blake3::hash(value.as_bytes()).to_hex().to_string()),
            "providerProfileId": segment.assignment.provider_id,
            "providerEndpoint": redacted_endpoint(segment.assignment.provider_endpoint.as_deref()),
            "providerVersion": segment.assignment.provider_version,
            "model": segment.assignment.model,
            "voiceProfileId": segment.assignment.voice_id,
            "dictionaryRevision": segment.dictionary_revision,
            "appliedRuleIds": segment.applied_rule_ids,
            "normalizationVersion": NORMALIZATION_VERSION,
            "providerRequestId": response.usage.request_id,
            "createdAt": Utc::now(),
        });
        let path = cache
            .put(&cache_key, &flac, &manifest)
            .map_err(media_error)?;
        cache.pin(&cache_key).map_err(media_error)?;
        let duration_ms = probe_duration_ms(sidecars, &path).await?;
        let artifact = artifact_for_file_with_id(
            artifact_id,
            ArtifactKind::SegmentAudio,
            &path,
            Some("audio/flac".to_owned()),
            Some(duration_ms),
            Some(cache_key.as_str().to_owned()),
            Some(job_id),
        )
        .await?;
        persist_artifact(
            state,
            segment_project_id(state, segment.chapter_id).await?,
            &artifact,
        )
        .await?;
        (artifact, streamed.progressive_decode_complete)
    };
    let artifact = persist_proof_take(state, job_id, &segment, &unit, &artifact).await?;
    unit.output_artifact_id = Some(artifact.id);
    update_unit_state(state, &mut unit, JobUnitState::Completed, None).await?;
    increment_job_progress(state, job_id, progress_guard).await?;
    if progressive_decode_complete {
        complete_playback_segment(job_id, segment.playback_ordinal);
    } else if playback_listener_count(job_id) > 0 {
        reset_playback_segment(job_id, segment.playback_ordinal);
        let pcm = decode_flac_pcm(sidecars, Path::new(&artifact.path)).await?;
        complete_playback_with_pcm(job_id, segment.playback_ordinal, &pcm);
    } else {
        complete_playback_segment(job_id, segment.playback_ordinal);
    }
    Ok(SegmentArtifact {
        plan: segment,
        artifact,
    })
}

#[allow(clippy::too_many_lines)]
pub(super) async fn persist_proof_take(
    state: &AppState,
    job_id: JobId,
    segment: &SegmentPlan,
    unit: &JobUnit,
    source: &Artifact,
) -> Result<Artifact, ServiceError> {
    let Some(segment_id) = unit.segment_id else {
        // Jobs created by versions before the proofing migration remain resumable,
        // but their transient artifacts cannot be promoted into trustworthy takes.
        return Ok(source.clone());
    };
    let take_id = unit
        .payload
        .get("takeId")
        .cloned()
        .and_then(|value| serde_json::from_value::<SegmentTakeId>(value).ok())
        .ok_or_else(|| ServiceError::Internal("proofing unit has no take id".to_owned()))?;
    if let Some(existing) = state
        .database
        .repositories()
        .proofing
        .get_take(take_id)
        .await
        .map_err(storage_error)?
    {
        let artifact = load_artifact(state, existing.artifact_id).await?;
        verify_selected_artifact_integrity(&artifact).await?;
        return Ok(artifact);
    }
    let artifact_id = unit
        .payload
        .get("takeArtifactId")
        .cloned()
        .and_then(|value| serde_json::from_value::<ArtifactId>(value).ok())
        .ok_or_else(|| ServiceError::Internal("proofing unit has no artifact id".to_owned()))?;
    let project_id = segment_project_id(state, segment.chapter_id).await?;
    let take_directory = state
        .config
        .data_dir
        .join("library")
        .join(project_id.to_string())
        .join("proofing")
        .join("takes");
    tokio::fs::create_dir_all(&take_directory).await?;
    let destination = take_directory.join(format!("{take_id}.flac"));
    let expected_fingerprint = materialize_proof_take_file(source, &destination).await?;
    let artifact = if sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM artifacts WHERE id = ?")
        .bind(artifact_id.to_string())
        .fetch_one(state.database.pool())
        .await
        .map_err(storage_error)?
        > 0
    {
        let existing = load_artifact(state, artifact_id).await?;
        if Path::new(&existing.path) != destination || existing.fingerprint != expected_fingerprint
        {
            return Err(ServiceError::Conflict(
                "existing proof-take artifact does not match its durable source".to_owned(),
            ));
        }
        existing
    } else {
        let artifact = artifact_for_file_with_id(
            artifact_id,
            ArtifactKind::SegmentAudio,
            &destination,
            Some("audio/flac".to_owned()),
            source.duration_ms,
            None,
            None,
        )
        .await?;
        persist_artifact(state, project_id, &artifact).await?;
        artifact
    };
    // Quality-control builds hold the same project lock through their final provenance
    // revalidation and persistence. Serialize the durable take/selection mutation with that
    // window so a report can never commit a half-old proofing identity.
    let project_lock = state.character_lifecycle_lock(project_id).await;
    let _project_guard = project_lock.lock().await;
    if let Some(existing) = state
        .database
        .repositories()
        .proofing
        .get_take(take_id)
        .await
        .map_err(storage_error)?
    {
        let artifact = load_artifact(state, existing.artifact_id).await?;
        verify_selected_artifact_integrity(&artifact).await?;
        return Ok(artifact);
    }
    let ordinal = sqlx::query_scalar::<_, i64>(
        "SELECT COALESCE(MAX(ordinal), 0) + 1 FROM segment_takes WHERE segment_id = ?",
    )
    .bind(segment_id.to_string())
    .fetch_one(state.database.pool())
    .await
    .map_err(storage_error)?;
    let semantic_input_hash = segment_semantic_input_hash(segment)?;
    let now = Utc::now();
    let take = SegmentTake {
        id: take_id,
        segment_id,
        artifact_id: artifact.id,
        ordinal: u32::try_from(ordinal).unwrap_or(u32::MAX),
        source_job_id: job_id,
        source_job_unit_id: unit.id,
        semantic_input_hash,
        duration_ms: artifact.duration_ms.unwrap_or_default(),
        provider_profile_id: Some(ProviderProfileId::from_uuid(segment.assignment.provider_id)),
        model: segment.assignment.model.clone(),
        voice_profile_id: Some(VoiceProfileId::from_uuid(segment.assignment.voice_id)),
        dictionary_revision_hash: segment.dictionary_revision.clone(),
        normalization_version: NORMALIZATION_VERSION.to_owned(),
        synthesis_provenance: BTreeMap::from([
            (
                "providerName".to_owned(),
                serde_json::json!(segment.assignment.provider_name),
            ),
            (
                "voiceName".to_owned(),
                serde_json::json!(segment.assignment.voice_name),
            ),
            (
                "performance".to_owned(),
                serde_json::json!(segment.assignment.performance),
            ),
            (
                "timing".to_owned(),
                serde_json::json!(segment.assignment.timing),
            ),
        ]),
        findings: Vec::new(),
        created_at: now,
    };
    let auto_select = unit
        .payload
        .get("autoSelect")
        .and_then(serde_json::Value::as_bool)
        .unwrap_or(false);
    let repository = state.database.repositories().proofing;
    if auto_select {
        let selection = SegmentSelection {
            segment_id,
            take_id,
            selected_at: now,
            revision: 0,
        };
        repository
            .insert_take_and_select(&take, &selection)
            .await
            .map_err(storage_error)?;
    } else {
        repository.insert_take(&take).await.map_err(storage_error)?;
    }
    Ok(artifact)
}

pub(super) async fn materialize_proof_take_file(
    source: &Artifact,
    destination: &Path,
) -> Result<FileFingerprint, ServiceError> {
    let source_path = Path::new(&source.path);
    let source_metadata = tokio::fs::symlink_metadata(source_path).await?;
    if source_metadata.file_type().is_symlink() || !source_metadata.is_file() {
        return Err(ServiceError::Conflict(format!(
            "proof-take source is not a regular managed file: {}",
            source_path.display()
        )));
    }
    let expected = fingerprint_file(source_path).await?;
    if expected != source.fingerprint {
        return Err(ServiceError::Conflict(format!(
            "proof-take source no longer matches its durable fingerprint: {}",
            source_path.display()
        )));
    }

    match tokio::fs::symlink_metadata(destination).await {
        Ok(metadata) if metadata.is_file() && !metadata.file_type().is_symlink() => {}
        Ok(_) => {
            return Err(ServiceError::Conflict(format!(
                "proof-take destination is not a regular managed file: {}",
                destination.display()
            )));
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            match tokio::fs::hard_link(source_path, destination).await {
                Ok(()) => {}
                Err(_) => copy_file_atomically(source_path, destination).await?,
            }
        }
        Err(error) => return Err(ServiceError::Io(error)),
    }

    let destination_metadata = tokio::fs::symlink_metadata(destination).await?;
    if destination_metadata.file_type().is_symlink() || !destination_metadata.is_file() {
        return Err(ServiceError::Conflict(format!(
            "proof-take destination was replaced during materialization: {}",
            destination.display()
        )));
    }
    if fingerprint_file(destination).await? != expected {
        return Err(ServiceError::Conflict(format!(
            "existing proof-take file does not match its durable source: {}",
            destination.display()
        )));
    }
    audiobookai_storage::harden_private_file(destination)
        .await
        .map_err(storage_error)?;
    Ok(expected)
}
