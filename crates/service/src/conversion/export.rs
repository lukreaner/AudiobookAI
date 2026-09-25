use super::{
    AppState, Arc, Artifact, ArtifactId, ArtifactKind, BTreeMap, BTreeSet, BackgroundMusic,
    ChapterArtifact, ChapterAudio, ChapterId, ChapterPlan, ConversionPlan, Duration,
    ExportArtifactView, ExportFormat, ExportLayout, ExportPlanner, ExportPromotionFile,
    ExportPromotionMarker, ExportRequest, FileFingerprint, JobId, JobKind, JobUnit, JobUnitState,
    LoudnessMeasurement, LoudnessSettings, MediaBookMetadata, Path, PathBuf, PersistedUnitPlan,
    SegmentArtifact, ServiceError, SidecarPair, Utc, Uuid, artifact_for_file, artifact_kind_name,
    atomic_promote, create_directory_no_clobber, ensure_existing_real_directory,
    ensure_export_root_identity, ensure_private_directory, ensure_private_staging_file_path,
    export_manifest_path, ffmpeg_build_description, fingerprint_file, format_name,
    increment_job_progress, internal_error, layout_name, load_required_proof_export_snapshot,
    media_error, media_export_format, media_type_for_path, parse_loudness_measurement,
    persist_artifact, prepare_private_export_staging, probe_duration_ms, provider_endpoint_family,
    require_output_reservation, run_process, run_process_capture, segment_project_id,
    set_job_message, storage_error, sync_file, update_unit_state, validate_flac,
    verify_selected_artifacts_before_use, wait_until_runnable, write_file_atomically,
    write_job_staging_file_atomically,
};
use std::fmt::Write as _;

#[allow(clippy::too_many_arguments, clippy::too_many_lines)]
pub(super) async fn assemble_chapter(
    state: &Arc<AppState>,
    job_id: JobId,
    chapter: ChapterPlan,
    mut unit: JobUnit,
    mut segments: Vec<SegmentArtifact>,
    verify_selected_artifacts: bool,
    sidecars: &SidecarPair,
    progress_guard: &tokio::sync::Mutex<()>,
) -> Result<ChapterArtifact, ServiceError> {
    wait_until_runnable(state, job_id).await?;
    update_unit_state(state, &mut unit, JobUnitState::Running, None).await?;
    set_job_message(
        state,
        job_id,
        &format!("Assembling {}", chapter.chapter.title),
    )
    .await?;
    segments.sort_by_key(|segment| segment.plan.segment_ordinal);
    let output_directory = state
        .config
        .data_dir
        .join("jobs")
        .join(job_id.to_string())
        .join("chapters");
    tokio::fs::create_dir_all(&output_directory).await?;
    let destination = output_directory.join(format!(
        "{:04}-{}.flac",
        chapter.chapter.ordinal, chapter.chapter.id
    ));
    let temporary = tempfile::Builder::new()
        .prefix(".chapter-")
        .suffix(".flac")
        .tempfile_in(&output_directory)
        .map_err(ServiceError::Io)?
        .into_temp_path();
    let mut arguments = vec![
        "-hide_banner".to_owned(),
        "-nostdin".to_owned(),
        "-y".to_owned(),
    ];
    for segment in &segments {
        arguments.extend(["-i".to_owned(), segment.artifact.path.clone()]);
    }
    let mut filter = String::new();
    for (index, segment) in segments.iter().enumerate() {
        write!(filter, "[{index}:a]aresample=48000:async=1:first_pts=0")
            .expect("writing to String cannot fail");
        if let Some(milliseconds) = segment.plan.assignment.timing.pause_before_ms {
            write!(filter, ",adelay={milliseconds}:all=1").expect("writing to String cannot fail");
        }
        if let Some(milliseconds) = segment.plan.assignment.timing.pause_after_ms {
            let seconds = f64::from(milliseconds) / 1_000.0;
            write!(filter, ",apad=pad_dur={seconds:.3}").expect("writing to String cannot fail");
        }
        write!(filter, "[a{index}];").expect("writing to String cannot fail");
    }
    for index in 0..segments.len() {
        write!(filter, "[a{index}]").expect("writing to String cannot fail");
    }
    write!(filter, "concat=n={}:v=0:a=1[outa]", segments.len())
        .expect("writing to String cannot fail");
    arguments.extend([
        "-filter_complex".to_owned(),
        filter,
        "-map".to_owned(),
        "[outa]".to_owned(),
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
        temporary.to_string_lossy().into_owned(),
    ]);
    // A pause can begin after the worker's initial proof-export validation. Check the job state
    // again at the actual input-consumption boundary, then re-hash every selected take before
    // FFmpeg is allowed to open it.
    wait_until_runnable(state, job_id).await?;
    if verify_selected_artifacts {
        let artifacts = segments
            .iter()
            .map(|segment| &segment.artifact)
            .collect::<Vec<_>>();
        verify_selected_artifacts_before_use(&artifacts).await?;
    }
    run_process(&sidecars.ffmpeg, &arguments, "assemble chapter").await?;
    validate_flac(&temporary).await?;
    atomic_promote(&temporary, &destination).await?;
    let duration_ms = probe_duration_ms(sidecars, &destination).await?;
    let artifact = artifact_for_file(
        ArtifactKind::ChapterMaster,
        &destination,
        Some("audio/flac".to_owned()),
        Some(duration_ms),
        None,
        Some(job_id),
    )
    .await?;
    persist_artifact(
        state,
        chapter_project_id(state, chapter.chapter.id).await?,
        &artifact,
    )
    .await?;
    unit.output_artifact_id = Some(artifact.id);
    update_unit_state(state, &mut unit, JobUnitState::Completed, None).await?;
    increment_job_progress(state, job_id, progress_guard).await?;
    Ok(ChapterArtifact {
        chapter: chapter.chapter,
        artifact,
    })
}

pub(super) async fn chapter_project_id(
    state: &AppState,
    chapter_id: ChapterId,
) -> Result<Uuid, ServiceError> {
    segment_project_id(state, chapter_id.as_uuid()).await
}

// Export assembly coordinates media metadata, atomic outputs, job units, and
// manifests; splitting the sequence would increase partial-output risk.
#[allow(clippy::too_many_arguments, clippy::too_many_lines)]
pub(super) async fn export_book(
    state: &Arc<AppState>,
    job_id: JobId,
    plan: &ConversionPlan,
    chapters: &[ChapterArtifact],
    sidecars: &SidecarPair,
    units: &mut PersistedUnitPlan,
    progress_guard: &tokio::sync::Mutex<()>,
) -> Result<(Vec<Artifact>, Artifact), ServiceError> {
    let media_chapters = chapters
        .iter()
        .map(|chapter| ChapterAudio {
            title: chapter.chapter.title.clone(),
            path: PathBuf::from(&chapter.artifact.path),
            duration_milliseconds: chapter.artifact.duration_ms.unwrap_or(1).max(1),
        })
        .collect::<Vec<_>>();
    let output_directory = PathBuf::from(&plan.export.output_directory);
    ensure_export_root_identity(&plan.export).await?;
    let extension = media_export_format(plan.export.format).extension();
    let final_output = if plan.export.layout == ExportLayout::PerChapter {
        output_directory.join(&plan.export.filename_template)
    } else {
        output_directory.join(format!("{}.{}", plan.export.filename_template, extension))
    };
    require_output_reservation(state, job_id, plan.project.id, &plan.export, &final_output).await?;
    if units.export.state == JobUnitState::Completed {
        return load_completed_export_result(state, job_id).await;
    }
    let staging_directory = prepare_private_export_staging(state, job_id).await?;
    let temporary_output =
        export_staging_output_path(&staging_directory, plan.export.layout, extension);
    if final_output.exists() {
        ensure_export_root_identity(&plan.export).await?;
        let recovered = recover_promoted_export(
            state,
            job_id,
            plan.export.layout,
            &temporary_output,
            &final_output,
        )
        .await?;
        state
            .database
            .repositories()
            .jobs
            .mark_output_promoted(job_id, Utc::now())
            .await
            .map_err(storage_error)?;
        return finalize_export_outputs(
            state,
            job_id,
            plan,
            chapters,
            sidecars,
            units,
            progress_guard,
            recovered,
            &final_output,
        )
        .await;
    }
    if plan.export.layout == ExportLayout::PerChapter {
        ensure_private_directory(&temporary_output).await?;
    } else {
        ensure_private_staging_file_path(&staging_directory, &temporary_output).await?;
    }
    let metadata = MediaBookMetadata {
        title: plan.project.metadata.title.clone(),
        authors: plan.project.metadata.authors.clone(),
        narrator: plan.project.metadata.narrator.clone(),
        series: plan
            .project
            .metadata
            .series
            .as_ref()
            .map(|series| series.name.clone()),
        series_position: plan
            .project
            .metadata
            .series
            .as_ref()
            .and_then(|series| series.position)
            .map(f64::from),
        language: plan.project.metadata.language.clone(),
        date: None,
        description: plan.project.metadata.description.clone(),
        isbn: plan.project.metadata.identifier.clone(),
        additional: BTreeMap::from([(
            "publisher".to_owned(),
            plan.project.metadata.publisher.clone().unwrap_or_default(),
        )]),
    };
    let background_music = plan.music_path.as_ref().map(|path| {
        let settings = plan
            .export
            .background_music
            .as_ref()
            .expect("music path requires settings");
        BackgroundMusic {
            path: path.clone(),
            trim_start_seconds: Duration::from_millis(settings.trim_start_ms).as_secs_f64(),
            trim_end_seconds: settings
                .trim_end_ms
                .map(|value| Duration::from_millis(value).as_secs_f64()),
            gain_db: f64::from(settings.gain_db),
            fade_in_seconds: Duration::from_millis(settings.fade_in_ms).as_secs_f64(),
            fade_out_seconds: Duration::from_millis(settings.fade_out_ms).as_secs_f64(),
            duck_threshold: settings.ducking.as_ref().map_or(0.03, |ducking| {
                10_f64.powf(f64::from(ducking.threshold_db) / 20.0)
            }),
            duck_ratio: settings.ducking.as_ref().map_or(1.0, |_| 8.0),
        }
    });
    let cover = state
        .config
        .data_dir
        .join("library")
        .join(plan.project.id.to_string())
        .join("cover.bin");
    let mut request = ExportRequest::audiobook_defaults(
        media_chapters,
        temporary_output.clone(),
        media_export_format(plan.export.format),
        metadata,
    );
    request.split_per_chapter = plan.export.layout == ExportLayout::PerChapter;
    request.cover_art = cover.is_file().then_some(cover);
    request.background_music = background_music;
    request.loudness = Some(LoudnessSettings {
        target_lufs: f64::from(plan.export.audio.target_lufs),
        true_peak_db: f64::from(plan.export.audio.true_peak_db),
        loudness_range: 7.0,
    });
    request.preview = false;
    request.overwrite = true;
    request.bitrate_kbps = plan
        .export
        .audio
        .bitrate_kbps
        .and_then(|value| u16::try_from(value).ok())
        .unwrap_or(128);
    request.sample_rate = 48_000;
    request.channels = plan.export.audio.channels;

    wait_until_runnable(state, job_id).await?;
    if units.normalize.state != JobUnitState::Completed {
        update_unit_state(state, &mut units.normalize, JobUnitState::Running, None).await?;
        set_job_message(state, job_id, "Measuring final loudness").await?;
    }
    let planner = ExportPlanner::new(sidecars.clone());
    let analyses = planner.loudness_analysis(&request).map_err(media_error)?;
    let mut measurements = Vec::<LoudnessMeasurement>::new();
    for invocation in analyses {
        let stderr = run_process_capture(
            &invocation.executable,
            &invocation.arguments,
            &invocation.purpose,
        )
        .await?;
        measurements.push(parse_loudness_measurement(&stderr).map_err(media_error)?);
    }
    if units.normalize.state != JobUnitState::Completed {
        update_unit_state(state, &mut units.normalize, JobUnitState::Completed, None).await?;
        increment_job_progress(state, job_id, progress_guard).await?;
    }

    wait_until_runnable(state, job_id).await?;
    if units.export.state != JobUnitState::Completed {
        update_unit_state(state, &mut units.export, JobUnitState::Running, None).await?;
    }
    set_job_message(state, job_id, "Writing final audiobook").await?;
    let render = planner
        .render(&request, &measurements)
        .map_err(media_error)?;
    for auxiliary in &render.auxiliary_files {
        write_job_staging_file_atomically(
            &staging_directory,
            &auxiliary.path,
            auxiliary.contents.as_bytes(),
        )
        .await?;
    }
    for path in &render.outputs {
        ensure_private_staging_file_path(&staging_directory, path).await?;
    }
    for invocation in &render.invocations {
        run_process(
            &invocation.executable,
            &invocation.arguments,
            &invocation.purpose,
        )
        .await?;
    }
    let mut rendered = Vec::new();
    for path in &render.outputs {
        let duration = probe_duration_ms(sidecars, path).await?;
        if duration == 0 {
            return Err(ServiceError::Internal(format!(
                "FFmpeg produced an empty export: {}",
                path.display()
            )));
        }
        sync_file(path).await?;
        rendered.push((path.clone(), duration));
    }

    persist_export_promotion_marker(state, job_id, &final_output, &rendered).await?;
    state
        .database
        .repositories()
        .jobs
        .mark_output_promoting(job_id, Utc::now())
        .await
        .map_err(storage_error)?;

    ensure_export_root_identity(&plan.export).await?;
    let final_paths = if plan.export.layout == ExportLayout::PerChapter {
        create_directory_no_clobber(&final_output).await?;
        ensure_existing_real_directory(&final_output, "split export destination").await?;
        mark_split_export_directory_created(state, job_id, &final_output).await?;
        let mut final_paths = Vec::new();
        for (path, duration) in rendered {
            let file_name = path.file_name().ok_or_else(|| {
                ServiceError::Internal("split export has no file name".to_owned())
            })?;
            let destination = final_output.join(file_name);
            ensure_export_root_identity(&plan.export).await?;
            ensure_existing_real_directory(&final_output, "split export destination").await?;
            atomic_promote(&path, &destination).await?;
            final_paths.push((destination, duration));
        }
        let _ = tokio::fs::remove_dir(&temporary_output).await;
        final_paths
    } else {
        let duration = rendered
            .first()
            .map(|(_, duration)| *duration)
            .ok_or_else(|| ServiceError::Internal("export produced no files".to_owned()))?;
        ensure_export_root_identity(&plan.export).await?;
        atomic_promote(&temporary_output, &final_output).await?;
        vec![(final_output.clone(), duration)]
    };
    state
        .database
        .repositories()
        .jobs
        .mark_output_promoted(job_id, Utc::now())
        .await
        .map_err(storage_error)?;

    finalize_export_outputs(
        state,
        job_id,
        plan,
        chapters,
        sidecars,
        units,
        progress_guard,
        final_paths,
        &final_output,
    )
    .await
}

pub(super) fn export_staging_output_path(
    staging_directory: &Path,
    layout: ExportLayout,
    extension: &str,
) -> PathBuf {
    if layout == ExportLayout::PerChapter {
        staging_directory.join("chapters")
    } else {
        staging_directory.join(format!("audiobook.{extension}"))
    }
}

pub(super) fn export_promotion_marker_path(staging_directory: &Path) -> PathBuf {
    staging_directory.join("export-promotion.json")
}

pub(super) async fn persist_export_promotion_marker(
    state: &AppState,
    job_id: JobId,
    final_output: &Path,
    rendered: &[(PathBuf, u64)],
) -> Result<(), ServiceError> {
    let staging_directory = prepare_private_export_staging(state, job_id).await?;
    let mut files = Vec::with_capacity(rendered.len());
    for (path, duration_ms) in rendered {
        ensure_private_staging_file_path(&staging_directory, path).await?;
        let file_name = path
            .file_name()
            .and_then(std::ffi::OsStr::to_str)
            .ok_or_else(|| ServiceError::Internal("rendered export has no file name".to_owned()))?;
        files.push(ExportPromotionFile {
            file_name: file_name.to_owned(),
            duration_ms: *duration_ms,
            fingerprint: fingerprint_file(path).await?,
        });
    }
    if files.is_empty() {
        return Err(ServiceError::Internal(
            "cannot promote an export without rendered files".to_owned(),
        ));
    }
    let marker = ExportPromotionMarker {
        schema_version: 1,
        job_id,
        final_output: final_output.to_string_lossy().into_owned(),
        split_directory_created: false,
        files,
    };
    let path = export_promotion_marker_path(&staging_directory);
    write_job_staging_file_atomically(
        &staging_directory,
        &path,
        &serde_json::to_vec_pretty(&marker).map_err(internal_error)?,
    )
    .await
}

pub(super) async fn mark_split_export_directory_created(
    state: &AppState,
    job_id: JobId,
    final_output: &Path,
) -> Result<(), ServiceError> {
    ensure_existing_real_directory(final_output, "split export destination").await?;
    let staging_directory = prepare_private_export_staging(state, job_id).await?;
    let marker_path = export_promotion_marker_path(&staging_directory);
    ensure_private_staging_file_path(&staging_directory, &marker_path).await?;
    let mut marker: ExportPromotionMarker =
        serde_json::from_slice(&tokio::fs::read(&marker_path).await.map_err(|error| {
            ServiceError::Conflict(format!(
                "split export directory exists without a readable job-owned promotion marker: {error}"
            ))
        })?)
        .map_err(internal_error)?;
    if marker.schema_version != 1
        || marker.job_id != job_id
        || Path::new(&marker.final_output) != final_output
        || marker.files.is_empty()
    {
        return Err(ServiceError::Conflict(
            "split export promotion marker does not match this durable job".to_owned(),
        ));
    }
    marker.split_directory_created = true;
    write_job_staging_file_atomically(
        &staging_directory,
        &marker_path,
        &serde_json::to_vec_pretty(&marker).map_err(internal_error)?,
    )
    .await
}

pub(super) async fn verify_promoted_file(
    path: &Path,
    expected: &FileFingerprint,
) -> Result<(), ServiceError> {
    let metadata = tokio::fs::symlink_metadata(path).await.map_err(|error| {
        ServiceError::Conflict(format!(
            "promoted export file is unavailable ({}): {error}",
            path.display()
        ))
    })?;
    if metadata.file_type().is_symlink() || !metadata.is_file() {
        return Err(ServiceError::Conflict(format!(
            "promoted export path is not a regular job-owned file: {}",
            path.display()
        )));
    }
    let actual = fingerprint_file(path).await?;
    if &actual != expected {
        return Err(ServiceError::Conflict(format!(
            "promoted export file no longer matches its durable marker: {}",
            path.display()
        )));
    }
    Ok(())
}

pub(super) async fn recover_promoted_export(
    state: &AppState,
    job_id: JobId,
    layout: ExportLayout,
    temporary_output: &Path,
    final_output: &Path,
) -> Result<Vec<(PathBuf, u64)>, ServiceError> {
    let staging_directory = prepare_private_export_staging(state, job_id).await?;
    let marker_path = export_promotion_marker_path(&staging_directory);
    ensure_private_staging_file_path(&staging_directory, &marker_path).await?;
    if layout == ExportLayout::PerChapter {
        ensure_existing_real_directory(final_output, "split export destination").await?;
    }
    let marker: ExportPromotionMarker =
        serde_json::from_slice(&tokio::fs::read(&marker_path).await.map_err(|error| {
            ServiceError::Conflict(format!(
                "export destination exists without a readable job-owned promotion marker: {error}"
            ))
        })?)
        .map_err(internal_error)?;
    if marker.schema_version != 1
        || marker.job_id != job_id
        || Path::new(&marker.final_output) != final_output
        || marker.files.is_empty()
        || (layout == ExportLayout::SingleFile && marker.files.len() != 1)
    {
        return Err(ServiceError::Conflict(
            "export promotion marker does not match this durable job".to_owned(),
        ));
    }
    if layout == ExportLayout::PerChapter && !marker.split_directory_created {
        return Err(ServiceError::Conflict(
            "split export directory was not durably created by this job".to_owned(),
        ));
    }
    let mut names = BTreeSet::new();
    let mut recovered = Vec::with_capacity(marker.files.len());
    for file in marker.files {
        let component = Path::new(&file.file_name);
        if component.file_name().is_none()
            || component
                .parent()
                .is_some_and(|parent| !parent.as_os_str().is_empty())
            || !names.insert(file.file_name.clone())
            || file.duration_ms == 0
        {
            return Err(ServiceError::Conflict(
                "export promotion marker contains an invalid output entry".to_owned(),
            ));
        }
        let destination = if layout == ExportLayout::PerChapter {
            final_output.join(&file.file_name)
        } else {
            final_output.to_path_buf()
        };
        if destination.exists() {
            verify_promoted_file(&destination, &file.fingerprint).await?;
        } else {
            let source = if layout == ExportLayout::PerChapter {
                temporary_output.join(&file.file_name)
            } else {
                temporary_output.to_path_buf()
            };
            verify_promoted_file(&source, &file.fingerprint).await?;
            atomic_promote(&source, &destination).await?;
        }
        recovered.push((destination, file.duration_ms));
    }
    if layout == ExportLayout::PerChapter {
        let _ = tokio::fs::remove_dir(temporary_output).await;
    }
    Ok(recovered)
}

pub(super) async fn ensure_export_manifest_file(
    path: &Path,
    job_id: JobId,
    value: &serde_json::Value,
) -> Result<(), ServiceError> {
    if path.exists() {
        let existing: serde_json::Value =
            serde_json::from_slice(&tokio::fs::read(path).await?).map_err(internal_error)?;
        if existing == *value {
            return Ok(());
        }
        if existing.get("jobId") == Some(&serde_json::json!(job_id)) {
            return Err(ServiceError::Conflict(format!(
                "existing job-owned export manifest does not match the immutable export snapshot: {}",
                path.display()
            )));
        }
        return Err(ServiceError::Conflict(format!(
            "export manifest belongs to another job: {}",
            path.display()
        )));
    }
    write_file_atomically(
        path,
        &serde_json::to_vec_pretty(value).map_err(internal_error)?,
    )
    .await
}

pub(super) async fn ensure_job_artifact(
    state: &AppState,
    project_id: Uuid,
    job_id: JobId,
    kind: ArtifactKind,
    path: &Path,
    media_type: Option<String>,
    duration_ms: Option<u64>,
) -> Result<Artifact, ServiceError> {
    let candidate =
        artifact_for_file(kind, path, media_type, duration_ms, None, Some(job_id)).await?;
    let existing = sqlx::query_scalar::<_, String>(
        "SELECT payload FROM artifacts WHERE pinned_by_job_id = ? AND kind = ? AND path = ? \
         ORDER BY created_at LIMIT 1",
    )
    .bind(job_id.to_string())
    .bind(artifact_kind_name(kind))
    .bind(&candidate.path)
    .fetch_optional(state.database.pool())
    .await
    .map_err(storage_error)?;
    if let Some(existing) = existing {
        let existing: Artifact = serde_json::from_str(&existing).map_err(internal_error)?;
        if existing.fingerprint != candidate.fingerprint {
            return Err(ServiceError::Conflict(format!(
                "persisted export artifact changed during recovery: {}",
                candidate.path
            )));
        }
        return Ok(existing);
    }
    persist_artifact(state, project_id, &candidate).await?;
    Ok(candidate)
}

pub(super) async fn load_completed_export_result(
    state: &AppState,
    job_id: JobId,
) -> Result<(Vec<Artifact>, Artifact), ServiceError> {
    let payloads = sqlx::query_scalar::<_, String>(
        "SELECT payload FROM artifacts WHERE pinned_by_job_id = ? \
         AND kind IN ('export', 'export_manifest') ORDER BY created_at, id",
    )
    .bind(job_id.to_string())
    .fetch_all(state.database.pool())
    .await
    .map_err(storage_error)?;
    let mut exports = Vec::new();
    let mut manifest = None;
    for payload in payloads {
        let artifact: Artifact = serde_json::from_str(&payload).map_err(internal_error)?;
        if fingerprint_file(Path::new(&artifact.path)).await? != artifact.fingerprint {
            return Err(ServiceError::Conflict(format!(
                "completed export artifact no longer matches its fingerprint: {}",
                artifact.path
            )));
        }
        match artifact.kind {
            ArtifactKind::Export => exports.push(artifact),
            ArtifactKind::ExportManifest => {
                manifest.get_or_insert(artifact);
            }
            _ => unreachable!("query restricts export artifact kinds"),
        }
    }
    exports.sort_by(|left, right| left.path.cmp(&right.path));
    if exports.is_empty() {
        return Err(ServiceError::Conflict(
            "completed export unit has no durable output artifacts".to_owned(),
        ));
    }
    let manifest = manifest.ok_or_else(|| {
        ServiceError::Conflict("completed export unit has no durable manifest".to_owned())
    })?;
    Ok((exports, manifest))
}

#[allow(clippy::too_many_arguments)]
pub(super) async fn finalize_export_outputs(
    state: &Arc<AppState>,
    job_id: JobId,
    plan: &ConversionPlan,
    chapters: &[ChapterArtifact],
    sidecars: &SidecarPair,
    units: &mut PersistedUnitPlan,
    progress_guard: &tokio::sync::Mutex<()>,
    final_paths: Vec<(PathBuf, u64)>,
    final_output: &Path,
) -> Result<(Vec<Artifact>, Artifact), ServiceError> {
    let ffmpeg_build = ffmpeg_build_description(sidecars).await?;
    let manifest_value =
        export_manifest_value(state, job_id, plan, chapters, &final_paths, &ffmpeg_build).await?;
    ensure_export_root_identity(&plan.export).await?;
    if plan.export.layout == ExportLayout::PerChapter {
        ensure_existing_real_directory(final_output, "split export destination").await?;
    }
    let manifest_path = export_manifest_path(plan.export.layout, final_output);
    ensure_export_manifest_file(&manifest_path, job_id, &manifest_value).await?;
    let manifest_artifact = ensure_job_artifact(
        state,
        plan.project.id.as_uuid(),
        job_id,
        ArtifactKind::ExportManifest,
        &manifest_path,
        Some("application/json".to_owned()),
        None,
    )
    .await?;
    let mut artifacts = Vec::new();
    for (path, duration) in final_paths {
        artifacts.push(
            ensure_job_artifact(
                state,
                plan.project.id.as_uuid(),
                job_id,
                ArtifactKind::Export,
                &path,
                Some(media_type_for_path(&path)),
                Some(duration),
            )
            .await?,
        );
    }
    units.export.output_artifact_id = artifacts.first().map(|artifact| artifact.id);
    if units.export.state != JobUnitState::Completed {
        update_unit_state(state, &mut units.export, JobUnitState::Completed, None).await?;
        increment_job_progress(state, job_id, progress_guard).await?;
    }
    Ok((artifacts, manifest_artifact))
}

pub(super) async fn export_manifest_value(
    state: &AppState,
    job_id: JobId,
    plan: &ConversionPlan,
    chapters: &[ChapterArtifact],
    final_paths: &[(PathBuf, u64)],
    ffmpeg_build: &str,
) -> Result<serde_json::Value, ServiceError> {
    let usage = state
        .database
        .repositories()
        .usage
        .totals(&audiobookai_storage::repositories::UsageFilter {
            job_id: Some(job_id),
            ..audiobookai_storage::repositories::UsageFilter::default()
        })
        .await
        .map_err(storage_error)?;
    let mut chapter_start = 0_u64;
    let chapter_markers = chapters
        .iter()
        .map(|chapter| {
            let duration = chapter.artifact.duration_ms.unwrap_or_default();
            let start = chapter_start;
            chapter_start = chapter_start.saturating_add(duration);
            serde_json::json!({
                "chapterId": chapter.chapter.id,
                "title": chapter.chapter.title,
                "startMilliseconds": start,
                "endMilliseconds": chapter_start,
            })
        })
        .collect::<Vec<_>>();
    let mut voices = BTreeMap::<Uuid, serde_json::Value>::new();
    for segment in plan.chapters.iter().flat_map(|chapter| &chapter.segments) {
        voices
            .entry(segment.assignment.voice_id)
            .or_insert_with(|| {
                serde_json::json!({
                    "character": segment.assignment.character_name,
                    "providerProfileId": segment.assignment.provider_id,
                    "providerFamily": provider_endpoint_family(&segment.assignment),
                    "providerVersion": segment.assignment.provider_version,
                    "model": segment.assignment.model,
                    "voiceProfileId": segment.assignment.voice_id,
                    "voiceName": segment.assignment.voice_name,
                })
            });
    }
    let mut dictionary_revisions = BTreeMap::new();
    for rule in &plan.rules {
        dictionary_revisions.insert(rule.id.to_string(), u64::from(rule.order));
    }
    let job = state
        .database
        .repositories()
        .jobs
        .get(job_id)
        .await
        .map_err(storage_error)?
        .ok_or(ServiceError::NotFound)?;
    let proofing_snapshot = if job.kind == JobKind::Export {
        Some(
            serde_json::to_value(load_required_proof_export_snapshot(state, &job).await?)
                .map_err(internal_error)?,
        )
    } else {
        None
    };
    Ok(serde_json::json!({
        "schemaVersion": 1,
        "projectId": plan.project.id,
        "jobId": job_id,
        "createdAt": job.created_at,
        "source": plan.book.source_fingerprint,
        "metadata": plan.project.metadata,
        "outputFormat": format_name(plan.export.format),
        "layout": layout_name(plan.export.layout),
        "outputFiles": final_paths.iter().map(|(path, _)| path.to_string_lossy().into_owned()).collect::<Vec<_>>(),
        "chapterMarkers": chapter_markers,
        "voiceProvenance": voices.into_values().collect::<Vec<_>>(),
        "dictionaryRevisions": dictionary_revisions,
        "proofingSnapshot": proofing_snapshot,
        "audio": plan.export.audio,
        "ffmpegBuild": ffmpeg_build,
        "usageTotals": usage,
    }))
}

pub(super) async fn update_export_catalog(
    state: &AppState,
    job_id: JobId,
    plan: &ConversionPlan,
    artifacts: &[Artifact],
    manifest_id: ArtifactId,
) -> Result<(), ServiceError> {
    let mut views = Vec::new();
    let part_count = u32::try_from(artifacts.len()).unwrap_or(u32::MAX);
    for (part_index, artifact) in artifacts.iter().enumerate() {
        let path = Path::new(&artifact.path);
        let size = tokio::fs::metadata(path).await?.len();
        views.push(ExportArtifactView {
            id: artifact.id.as_uuid(),
            project_id: plan.project.id.as_uuid(),
            job_id: job_id.as_uuid(),
            part_index: u32::try_from(part_index).unwrap_or(u32::MAX),
            part_count,
            project_title: plan.project.metadata.title.clone(),
            format: format_name(plan.export.format).to_owned(),
            split_mode: layout_name(plan.export.layout).to_owned(),
            file_name: path
                .file_name()
                .and_then(std::ffi::OsStr::to_str)
                .unwrap_or("audiobook")
                .to_owned(),
            size_bytes: size,
            duration_seconds: artifact.duration_ms.unwrap_or_default().div_ceil(1_000),
            created_at: artifact.created_at,
            download_url: format!("/api/v1/artifacts/{}", artifact.id),
            manifest_url: format!("/api/v1/artifacts/{manifest_id}"),
            chapter_markers: plan.export.layout == ExportLayout::SingleFile
                && !matches!(plan.export.format, ExportFormat::Wav),
        });
    }
    state.catalog.write().await.exports.extend(views);
    Ok(())
}
