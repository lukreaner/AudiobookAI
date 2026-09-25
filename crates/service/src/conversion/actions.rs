use super::{
    AppState, Arc, Artifact, ArtifactId, ArtifactKind, AsyncReadExt, AsyncSeekExt, BTreeMap, Body,
    Bytes, ExportArtifactView, ExportFormat, ExportLayout, ExportProfileId, FromStr, HeaderMap,
    HeaderValue, IntoResponse, JobId, JobKind, JobState, JobView, Path, PathBuf, RANGE_CHUNK_BYTES,
    Response, Row, ServiceError, StatusCode, Utc, Uuid, admit_failed_job_retry, format_name,
    header, internal_error, layout_name, load_artifact, load_export_profile,
    schedule_conversion_job, schedule_conversion_retry, schedule_segment_regeneration_job,
    schedule_segment_regeneration_retry, storage_error, stream,
    transition_export_job_with_reservation, transition_job,
};

/// Applies a user lifecycle action to both the domain record and desktop view.
// The lifecycle state machine stays together so each job kind and transition
// is checked against the same durable-state rules.
#[allow(clippy::too_many_lines)]
pub async fn job_action(
    state: Arc<AppState>,
    job_id: Uuid,
    action: &str,
) -> Result<JobView, ServiceError> {
    let id = JobId::from_uuid(job_id);
    let initial_job = state
        .database
        .repositories()
        .jobs
        .get(id)
        .await
        .map_err(storage_error)?
        .ok_or(ServiceError::NotFound)?;
    let _model_lifecycle_guard = state.model_lifecycle.lock().await;
    let project_lock = state
        .character_lifecycle_lock(initial_job.project_id.as_uuid())
        .await;
    let _project_guard = project_lock.lock().await;
    let job = state
        .database
        .repositories()
        .jobs
        .get(id)
        .await
        .map_err(storage_error)?
        .ok_or(ServiceError::NotFound)?;
    if matches!(action, "pause" | "resume" | "cancel" | "retry")
        && matches!(
            job.kind,
            JobKind::Preview | JobKind::QualityControl | JobKind::CacheCleanup
        )
    {
        return Err(ServiceError::ConflictDetails {
            code: "job_action_unsupported",
            detail: "this job runs synchronously and does not support lifecycle actions".to_owned(),
            meta: serde_json::json!({"jobId": job_id, "action": action, "kind": job.kind}),
        });
    }
    if matches!(action, "resume" | "retry")
        && let Some(active) =
            crate::api::blocking_project_job(&state, job.project_id.as_uuid(), Some(job_id)).await
    {
        return Err(crate::api::active_job_conflict(&active));
    }
    match (action, job.state) {
        ("pause", JobState::Queued) => {
            transition_job(&state, id, JobState::Running, "Preparing to pause").await?;
            transition_job(
                &state,
                id,
                JobState::Pausing,
                "Pausing after the active request",
            )
            .await?;
            if job.kind == JobKind::CharacterDetection {
                crate::workflows::spawn_character_detection(Arc::clone(&state), id.as_uuid());
            }
        }
        ("pause", JobState::Running) => {
            transition_job(
                &state,
                id,
                JobState::Pausing,
                "Pausing after the active request",
            )
            .await?;
            if job.kind == JobKind::CharacterDetection {
                crate::workflows::spawn_character_detection(Arc::clone(&state), id.as_uuid());
            }
        }
        ("resume", JobState::Paused) => {
            if matches!(job.kind, JobKind::Conversion | JobKind::Export) {
                transition_export_job_with_reservation(
                    &state,
                    id,
                    JobState::Running,
                    "Resuming job",
                )
                .await?;
            } else {
                transition_job(&state, id, JobState::Running, "Resuming job").await?;
            }
            match job.kind {
                JobKind::Conversion | JobKind::Export => {
                    schedule_conversion_job(Arc::clone(&state), id);
                }
                JobKind::SegmentRegeneration => {
                    schedule_segment_regeneration_job(Arc::clone(&state), id);
                }
                JobKind::CharacterDetection => {
                    crate::workflows::reset_detection_units_for_restart(&state, id, false).await?;
                    crate::workflows::spawn_character_detection(Arc::clone(&state), id.as_uuid());
                }
                JobKind::Preview | JobKind::QualityControl | JobKind::CacheCleanup => {}
            }
        }
        ("cancel", JobState::Queued | JobState::Running | JobState::Paused) => {
            transition_job(
                &state,
                id,
                JobState::Cancelling,
                "Cancelling after the active request",
            )
            .await?;
            match job.kind {
                JobKind::Conversion | JobKind::Export => {
                    schedule_conversion_job(Arc::clone(&state), id);
                }
                JobKind::SegmentRegeneration => {
                    schedule_segment_regeneration_job(Arc::clone(&state), id);
                }
                JobKind::CharacterDetection => {
                    crate::workflows::spawn_character_detection(Arc::clone(&state), id.as_uuid());
                }
                JobKind::Preview | JobKind::QualityControl | JobKind::CacheCleanup => {}
            }
        }
        ("retry", JobState::Failed) => {
            if job.kind == JobKind::CharacterDetection {
                crate::workflows::validate_detection_retry(&state, id).await?;
            }
            admit_failed_job_retry(&state, id).await?;
            match job.kind {
                JobKind::Conversion | JobKind::Export => {
                    schedule_conversion_retry(Arc::clone(&state), id);
                }
                JobKind::SegmentRegeneration => {
                    schedule_segment_regeneration_retry(Arc::clone(&state), id);
                }
                JobKind::CharacterDetection => {
                    crate::workflows::spawn_character_detection(Arc::clone(&state), id.as_uuid());
                }
                JobKind::Preview | JobKind::QualityControl | JobKind::CacheCleanup => {}
            }
        }
        (known, _) if matches!(known, "pause" | "resume" | "cancel" | "retry") => {
            return Err(ServiceError::Conflict(format!(
                "cannot {known} a job in state {:?}",
                job.state
            )));
        }
        _ => return Err(ServiceError::NotFound),
    }
    state
        .catalog
        .read()
        .await
        .jobs
        .get(&job_id)
        .cloned()
        .ok_or(ServiceError::NotFound)
}

/// Rebuilds export views from durable artifacts, including after restart.
pub async fn list_exports(state: &AppState) -> Result<Vec<ExportArtifactView>, ServiceError> {
    let rows = sqlx::query(
        "SELECT a.payload, a.project_id, a.pinned_by_job_id, j.export_profile_id \
         FROM artifacts a LEFT JOIN jobs j ON j.id = a.pinned_by_job_id \
         WHERE a.kind = 'export' ORDER BY a.created_at DESC",
    )
    .fetch_all(state.database.pool())
    .await
    .map_err(storage_error)?;
    let mut views = Vec::new();
    let mut manifest_orders = BTreeMap::<JobId, ExportManifestOrder>::new();
    for row in rows {
        let Ok(artifact) = serde_json::from_str::<Artifact>(row.get::<&str, _>("payload")) else {
            continue;
        };
        if !Path::new(&artifact.path).is_file() {
            continue;
        }
        let Ok(project_id) = Uuid::parse_str(row.get::<&str, _>("project_id")) else {
            continue;
        };
        let Some(job_id) = row.get::<Option<String>, _>("pinned_by_job_id") else {
            continue;
        };
        let Ok(job_id) = JobId::from_str(&job_id) else {
            continue;
        };
        let Some(profile_id) = row.get::<Option<String>, _>("export_profile_id") else {
            continue;
        };
        let Ok(profile_id) = ExportProfileId::from_str(&profile_id) else {
            continue;
        };
        let profile = load_export_profile(state, profile_id).await?;
        if let std::collections::btree_map::Entry::Vacant(entry) = manifest_orders.entry(job_id) {
            entry.insert(load_export_manifest_order(state, job_id).await?);
        }
        let manifest_order = manifest_orders
            .get(&job_id)
            .expect("manifest order was inserted immediately above");
        let project_title = state
            .catalog
            .read()
            .await
            .projects
            .get(&project_id)
            .map_or_else(
                || "Audiobook".to_owned(),
                |project| project.summary.title.clone(),
            );
        let path = Path::new(&artifact.path);
        views.push(ExportArtifactView {
            id: artifact.id.as_uuid(),
            project_id,
            job_id: job_id.as_uuid(),
            part_index: manifest_order
                .part_indexes
                .get(&artifact.id)
                .copied()
                .unwrap_or(0),
            part_count: manifest_order.part_count,
            project_title,
            format: format_name(profile.format).to_owned(),
            split_mode: layout_name(profile.layout).to_owned(),
            file_name: path
                .file_name()
                .and_then(std::ffi::OsStr::to_str)
                .unwrap_or("audiobook")
                .to_owned(),
            size_bytes: artifact.fingerprint.size_bytes,
            duration_seconds: artifact.duration_ms.unwrap_or_default().div_ceil(1_000),
            created_at: artifact.created_at,
            download_url: format!("/api/v1/artifacts/{}", artifact.id),
            manifest_url: manifest_order
                .manifest_id
                .map_or_else(String::new, |id| format!("/api/v1/artifacts/{id}")),
            chapter_markers: profile.layout == ExportLayout::SingleFile
                && !matches!(profile.format, ExportFormat::Wav),
        });
    }
    Ok(views)
}

#[derive(Debug, Default)]
pub(super) struct ExportManifestOrder {
    pub(super) manifest_id: Option<Uuid>,
    pub(super) part_indexes: BTreeMap<ArtifactId, u32>,
    pub(super) part_count: u32,
}

pub(super) async fn load_export_manifest_order(
    state: &AppState,
    job_id: JobId,
) -> Result<ExportManifestOrder, ServiceError> {
    let export_rows = sqlx::query_scalar::<_, String>(
        "SELECT payload FROM artifacts WHERE pinned_by_job_id = ? AND kind = 'export' \
         ORDER BY created_at ASC, id ASC",
    )
    .bind(job_id.to_string())
    .fetch_all(state.database.pool())
    .await
    .map_err(storage_error)?;
    let exports = export_rows
        .into_iter()
        .filter_map(|payload| serde_json::from_str::<Artifact>(&payload).ok())
        .collect::<Vec<_>>();
    let manifest = sqlx::query_scalar::<_, String>(
        "SELECT payload FROM artifacts WHERE pinned_by_job_id = ? AND kind = 'export_manifest' \
         ORDER BY created_at DESC LIMIT 1",
    )
    .bind(job_id.to_string())
    .fetch_optional(state.database.pool())
    .await
    .map_err(storage_error)?
    .and_then(|payload| serde_json::from_str::<Artifact>(&payload).ok());
    let manifest_id = manifest.as_ref().map(|artifact| artifact.id.as_uuid());
    let mut manifest_paths = Vec::new();
    if let Some(manifest) = &manifest
        && let Ok(contents) = tokio::fs::read_to_string(&manifest.path).await
        && let Ok(value) = serde_json::from_str::<serde_json::Value>(&contents)
        && let Some(paths) = value
            .get("outputFiles")
            .and_then(serde_json::Value::as_array)
    {
        manifest_paths.extend(
            paths
                .iter()
                .filter_map(serde_json::Value::as_str)
                .map(str::to_owned),
        );
    }
    let ordered_ids = canonical_export_ids(&exports, &manifest_paths);
    let part_count = u32::try_from(ordered_ids.len()).unwrap_or(u32::MAX);
    let part_indexes = ordered_ids
        .into_iter()
        .enumerate()
        .map(|(index, id)| (id, u32::try_from(index).unwrap_or(u32::MAX)))
        .collect();
    Ok(ExportManifestOrder {
        manifest_id,
        part_indexes,
        part_count,
    })
}

pub(crate) async fn canonical_export_artifact_ids(
    state: &AppState,
    job_id: JobId,
) -> Result<Vec<ArtifactId>, ServiceError> {
    let order = load_export_manifest_order(state, job_id).await?;
    let mut parts = order.part_indexes.into_iter().collect::<Vec<_>>();
    parts.sort_by_key(|(_, index)| *index);
    Ok(parts.into_iter().map(|(id, _)| id).collect())
}

pub(super) fn canonical_export_ids(
    exports: &[Artifact],
    manifest_paths: &[String],
) -> Vec<ArtifactId> {
    let mut ordered_ids = Vec::with_capacity(exports.len());
    for path in manifest_paths {
        if let Some(artifact) = exports.iter().find(|artifact| artifact.path == *path)
            && !ordered_ids.contains(&artifact.id)
        {
            ordered_ids.push(artifact.id);
        }
    }
    let remaining_ids = exports
        .iter()
        .map(|artifact| artifact.id)
        .filter(|id| !ordered_ids.contains(id))
        .collect::<Vec<_>>();
    ordered_ids.extend(remaining_ids);
    ordered_ids
}

/// Streams an authenticated artifact and honors a single RFC 7233 byte range.
pub async fn artifact_response(
    state: &AppState,
    id: Uuid,
    request_headers: &HeaderMap,
) -> Result<Response, ServiceError> {
    let artifact = load_artifact(state, ArtifactId::from_uuid(id)).await?;
    let path = PathBuf::from(&artifact.path);
    let length = tokio::fs::metadata(&path).await?.len();
    let range = if let Some(value) = request_headers.get(header::RANGE) {
        if let Ok(range) = parse_byte_range(value, length) {
            Some(range)
        } else {
            let mut response = StatusCode::RANGE_NOT_SATISFIABLE.into_response();
            response.headers_mut().insert(
                header::CONTENT_RANGE,
                HeaderValue::from_str(&format!("bytes */{length}")).map_err(internal_error)?,
            );
            return Ok(response);
        }
    } else {
        None
    };
    let (start, end, status) = range.map_or(
        (0, length.saturating_sub(1), StatusCode::OK),
        |(start, end)| (start, end, StatusCode::PARTIAL_CONTENT),
    );
    let response_length = if length == 0 {
        0
    } else {
        end.saturating_sub(start).saturating_add(1)
    };
    let mut file = tokio::fs::File::open(&path).await?;
    file.seek(std::io::SeekFrom::Start(start)).await?;
    let stream = stream::try_unfold(
        (file, response_length),
        |(mut file, remaining)| async move {
            if remaining == 0 {
                return Ok::<_, std::io::Error>(None);
            }
            let maximum = usize::try_from(remaining.min(RANGE_CHUNK_BYTES as u64))
                .unwrap_or(RANGE_CHUNK_BYTES);
            let mut buffer = vec![0_u8; maximum];
            let read = file.read(&mut buffer).await?;
            if read == 0 {
                return Ok(None);
            }
            buffer.truncate(read);
            let remaining = remaining.saturating_sub(u64::try_from(read).unwrap_or(u64::MAX));
            Ok(Some((Bytes::from(buffer), (file, remaining))))
        },
    );
    let mut response = Response::new(Body::from_stream(stream));
    *response.status_mut() = status;
    let headers = response.headers_mut();
    headers.insert(header::ACCEPT_RANGES, HeaderValue::from_static("bytes"));
    headers.insert(
        header::CONTENT_LENGTH,
        HeaderValue::from_str(&response_length.to_string()).map_err(internal_error)?,
    );
    headers.insert(
        header::CONTENT_TYPE,
        HeaderValue::from_str(
            artifact
                .media_type
                .as_deref()
                .unwrap_or("application/octet-stream"),
        )
        .map_err(internal_error)?,
    );
    if status == StatusCode::PARTIAL_CONTENT {
        headers.insert(
            header::CONTENT_RANGE,
            HeaderValue::from_str(&format!("bytes {start}-{end}/{length}"))
                .map_err(internal_error)?,
        );
    }
    let disposition = if artifact.kind == ArtifactKind::Export {
        "attachment"
    } else {
        "inline"
    };
    let filename = path
        .file_name()
        .and_then(std::ffi::OsStr::to_str)
        .unwrap_or("artifact")
        .replace(['"', '\r', '\n'], "_");
    headers.insert(
        header::CONTENT_DISPOSITION,
        HeaderValue::from_str(&format!("{disposition}; filename=\"{filename}\""))
            .map_err(internal_error)?,
    );
    sqlx::query("UPDATE artifacts SET last_accessed_at = ? WHERE id = ?")
        .bind(Utc::now().to_rfc3339())
        .bind(id.to_string())
        .execute(state.database.pool())
        .await
        .map_err(storage_error)?;
    Ok(response)
}

pub(super) fn parse_byte_range(value: &HeaderValue, length: u64) -> Result<(u64, u64), ()> {
    if length == 0 {
        return Err(());
    }
    let value = value.to_str().map_err(|_| ())?;
    let value = value.strip_prefix("bytes=").ok_or(())?;
    if value.contains(',') {
        return Err(());
    }
    let (start, end) = value.split_once('-').ok_or(())?;
    if start.is_empty() {
        let suffix = end.parse::<u64>().map_err(|_| ())?;
        if suffix == 0 {
            return Err(());
        }
        return Ok((length.saturating_sub(suffix.min(length)), length - 1));
    }
    let start = start.parse::<u64>().map_err(|_| ())?;
    if start >= length {
        return Err(());
    }
    let end = if end.is_empty() {
        length - 1
    } else {
        end.parse::<u64>().map_err(|_| ())?.min(length - 1)
    };
    (start <= end).then_some((start, end)).ok_or(())
}
