use super::{
    AppState, ArtifactKind, BackgroundMusicSettings, DuckingSettings, ExportFormat,
    ExportFormatView, ExportLayout, ExportOptionsInput, ExportProfile, ExportProfileId, JobId,
    MediaExportFormat, PathBuf, ProjectId, ServiceError, Utc, Uuid, Validate, artifact_for_file,
    copy_file_atomically, ensure_output_directory_not_reserved, internal_error,
    media_type_for_path, persist_artifact, storage_error,
};

pub(super) fn validate_export_input(input: &ExportOptionsInput) -> Result<(), ServiceError> {
    if !(32..=512).contains(&input.bitrate_kbps) {
        return Err(ServiceError::InvalidRequest(
            "audio bitrate must be between 32 and 512 kbps".to_owned(),
        ));
    }
    if !input.music_gain_db.is_finite() || !(-60.0..=0.0).contains(&input.music_gain_db) {
        return Err(ServiceError::InvalidRequest(
            "background music gain must be between -60 and 0 dB".to_owned(),
        ));
    }
    if input.background_music_path.is_some() && !input.confirm_background_music_owned {
        return Err(ServiceError::Conflict(
            "confirm that you own or are licensed to use the selected background audio".to_owned(),
        ));
    }
    Ok(())
}

// File ownership checks, managed input promotion, and durable profile creation
// stay in one ordered flow to avoid partially configured exports.
#[allow(clippy::too_many_lines)]
pub(super) async fn create_export_profile(
    state: &AppState,
    job_id: JobId,
    project_id: Uuid,
    input: &ExportOptionsInput,
) -> Result<(ExportProfile, Option<PathBuf>), ServiceError> {
    let (project, settings) = {
        let catalog = state.catalog.read().await;
        (
            catalog
                .projects
                .get(&project_id)
                .cloned()
                .ok_or(ServiceError::NotFound)?,
            catalog.settings.clone(),
        )
    };
    let output_directory = input
        .output_directory
        .as_ref()
        .map_or_else(|| state.config.data_dir.join("exports"), PathBuf::from);
    if !output_directory.is_absolute() {
        return Err(ServiceError::InvalidRequest(
            "the export directory must be an absolute path".to_owned(),
        ));
    }
    ensure_output_directory_not_reserved(state, &output_directory).await?;
    tokio::fs::create_dir_all(&output_directory).await?;
    let output_directory = tokio::fs::canonicalize(&output_directory).await?;
    let file_name = safe_file_component(
        input
            .file_name
            .as_deref()
            .or(project.output_name.as_deref())
            .unwrap_or(&project.summary.title),
    );
    let format = core_export_format(input.format);
    let layout = if input.split_per_chapter {
        ExportLayout::PerChapter
    } else {
        ExportLayout::SingleFile
    };

    let (music_settings, music_path) = if let Some(source) = &input.background_music_path {
        let source = PathBuf::from(source);
        if !source.is_absolute() {
            return Err(ServiceError::InvalidRequest(
                "background music must use an absolute path".to_owned(),
            ));
        }
        let source = tokio::fs::canonicalize(source).await.map_err(|error| {
            ServiceError::InvalidRequest(format!("background music is unavailable: {error}"))
        })?;
        if !tokio::fs::metadata(&source).await?.is_file() {
            return Err(ServiceError::InvalidRequest(
                "background music is not a regular file".to_owned(),
            ));
        }
        let managed_directory = state
            .config
            .data_dir
            .join("jobs")
            .join(job_id.to_string())
            .join("inputs");
        tokio::fs::create_dir_all(&managed_directory).await?;
        let extension = source
            .extension()
            .and_then(std::ffi::OsStr::to_str)
            .unwrap_or("audio");
        let managed = managed_directory.join(format!("background.{extension}"));
        copy_file_atomically(&source, &managed).await?;
        let artifact = artifact_for_file(
            ArtifactKind::ReferenceAudio,
            &managed,
            Some(media_type_for_path(&managed)),
            None,
            None,
            Some(job_id),
        )
        .await?;
        persist_artifact(state, project_id, &artifact).await?;
        (
            Some(BackgroundMusicSettings {
                artifact_id: artifact.id,
                user_owned_confirmed: true,
                gain_db: input.music_gain_db,
                loop_audio: true,
                trim_start_ms: 0,
                trim_end_ms: None,
                fade_in_ms: 2_000,
                fade_out_ms: 3_000,
                ducking: input.ducking.then_some(DuckingSettings {
                    attenuation_db: -12.0,
                    attack_ms: 20,
                    release_ms: 500,
                    threshold_db: -30.0,
                }),
            }),
            Some(managed),
        )
    } else {
        (None, None)
    };

    let now = Utc::now();
    let profile = ExportProfile {
        id: ExportProfileId::new(),
        project_id: ProjectId::from_uuid(project_id),
        name: format!("{} {}", format_name(format), layout_name(layout)),
        format,
        layout,
        output_directory: output_directory.to_string_lossy().into_owned(),
        filename_template: file_name,
        audio: audiobookai_core::AudioEncodingSettings {
            sample_rate_hz: 48_000,
            channels: 1,
            bitrate_kbps: Some(u32::from(input.bitrate_kbps)),
            target_lufs: settings.default_lufs,
            true_peak_db: settings.default_true_peak_db,
        },
        background_music: music_settings,
        embed_cover: true,
        embed_chapters: !matches!(format, ExportFormat::Wav),
        write_sidecar_manifest: true,
        created_at: now,
        updated_at: now,
    };
    let issues = profile.validation_issues();
    if let Some(issue) = issues.first() {
        return Err(ServiceError::InvalidRequest(issue.message.clone()));
    }
    sqlx::query(
        "INSERT INTO export_profiles (id, project_id, name, format, layout, updated_at, payload) \
         VALUES (?, ?, ?, ?, ?, ?, ?)",
    )
    .bind(profile.id.to_string())
    .bind(profile.project_id.to_string())
    .bind(&profile.name)
    .bind(format_name(profile.format))
    .bind(layout_name(profile.layout))
    .bind(profile.updated_at.to_rfc3339())
    .bind(serde_json::to_string(&profile).map_err(internal_error)?)
    .execute(state.database.pool())
    .await
    .map_err(storage_error)?;
    Ok((profile, music_path))
}

pub(super) fn core_export_format(format: ExportFormatView) -> ExportFormat {
    match format {
        ExportFormatView::Mp3 => ExportFormat::Mp3,
        ExportFormatView::Wav => ExportFormat::Wav,
        ExportFormatView::M4a => ExportFormat::M4a,
        ExportFormatView::M4b => ExportFormat::M4b,
    }
}

pub(super) const fn media_export_format(format: ExportFormat) -> MediaExportFormat {
    match format {
        ExportFormat::Mp3 => MediaExportFormat::Mp3,
        ExportFormat::Wav => MediaExportFormat::Wav,
        ExportFormat::M4a => MediaExportFormat::M4a,
        ExportFormat::M4b => MediaExportFormat::M4b,
    }
}

pub(super) const fn format_name(format: ExportFormat) -> &'static str {
    match format {
        ExportFormat::Mp3 => "mp3",
        ExportFormat::Wav => "wav",
        ExportFormat::M4a => "m4a",
        ExportFormat::M4b => "m4b",
    }
}

pub(super) const fn layout_name(layout: ExportLayout) -> &'static str {
    match layout {
        ExportLayout::SingleFile => "single_file",
        ExportLayout::PerChapter => "per_chapter",
    }
}

pub(super) fn safe_file_component(value: &str) -> String {
    let value = value
        .chars()
        .map(|character| {
            if character.is_control()
                || matches!(
                    character,
                    '/' | '\\' | ':' | '*' | '?' | '"' | '<' | '>' | '|'
                )
            {
                '_'
            } else {
                character
            }
        })
        .collect::<String>();
    let value = value.trim().trim_end_matches(['.', ' ']);
    if value.is_empty() {
        "Audiobook".to_owned()
    } else {
        value.chars().take(120).collect()
    }
}
