use super::{
    AppState, Artifact, ArtifactId, ArtifactKind, AsyncReadExt, Command, Duration, ExportLayout,
    ExportProfile, FileFingerprint, Job, JobId, OutputDestinationReservation,
    OutputReservationState, Path, PathBuf, ProjectId, RANGE_CHUNK_BYTES, ServiceError, SidecarPair,
    SidecarResolver, StorageError, Utc, Uuid, internal_error, load_export_profile,
    media_export_format, storage_error,
};

pub(super) fn resolve_sidecars(state: &AppState) -> Result<SidecarPair, ServiceError> {
    if let Some(packaged_root) = &state.config.bundled_sidecar_dir {
        return SidecarResolver::bundled(packaged_root)
            .resolve()
            .map_err(|error| {
                ServiceError::Conflict(format!(
                    "packaged FFmpeg sidecars failed validation: {error}"
                ))
            });
    }
    let mut directories = vec![state.config.data_dir.join("sidecars").join("bin")];
    if let Ok(executable) = std::env::current_exe()
        && let Some(parent) = executable.parent()
    {
        directories.push(parent.join("sidecars").join("bin"));
        directories.push(parent.join("../Resources/sidecars/bin"));
    }
    let explicit = match (
        std::env::var_os("AUDIOBOOKAI_FFMPEG"),
        std::env::var_os("AUDIOBOOKAI_FFPROBE"),
    ) {
        (Some(ffmpeg), Some(ffprobe)) => Some((PathBuf::from(ffmpeg), PathBuf::from(ffprobe))),
        (None, None) => None,
        _ => {
            return Err(ServiceError::InvalidRequest(
                "set both AUDIOBOOKAI_FFMPEG and AUDIOBOOKAI_FFPROBE".to_owned(),
            ));
        }
    };
    let allow_system = cfg!(debug_assertions)
        || std::env::var("AUDIOBOOKAI_ALLOW_SYSTEM_FFMPEG").is_ok_and(|value| value == "1");
    let mut last_error = None;
    for directory in directories {
        let mut resolver = SidecarResolver::bundled(directory).allow_system_path(allow_system);
        if let Some((ffmpeg, ffprobe)) = &explicit {
            resolver = resolver.explicit(ffmpeg, ffprobe);
        }
        match resolver.resolve() {
            Ok(pair) => return Ok(pair),
            Err(error) => last_error = Some(error),
        }
    }
    Err(ServiceError::Conflict(format!(
        "FFmpeg and ffprobe are unavailable: {}. Packaged releases include verified sidecars; source builds may explicitly enable system FFmpeg.",
        last_error.map_or_else(
            || "no candidate paths were found".to_owned(),
            |error| error.to_string()
        )
    )))
}

pub(super) fn media_error(error: impl std::fmt::Display) -> ServiceError {
    ServiceError::Internal(error.to_string())
}

pub(super) async fn run_process(
    executable: &Path,
    arguments: &[String],
    purpose: &str,
) -> Result<(), ServiceError> {
    run_process_capture(executable, arguments, purpose)
        .await
        .map(|_| ())
}

pub(super) async fn run_process_capture(
    executable: &Path,
    arguments: &[String],
    purpose: &str,
) -> Result<String, ServiceError> {
    let output = Command::new(executable)
        .args(arguments)
        .kill_on_drop(true)
        .output()
        .await?;
    let stderr = String::from_utf8_lossy(&output.stderr).into_owned();
    if output.status.success() {
        Ok(stderr)
    } else {
        let mut redacted = stderr.replace(['\r', '\n'], " ");
        redacted.truncate(1_024);
        Err(ServiceError::Internal(format!(
            "media step '{purpose}' failed{}",
            if redacted.is_empty() {
                String::new()
            } else {
                format!(": {redacted}")
            }
        )))
    }
}

pub(super) async fn ffmpeg_build_description(
    sidecars: &SidecarPair,
) -> Result<String, ServiceError> {
    let output = Command::new(&sidecars.ffmpeg)
        .args(["-hide_banner", "-version"])
        .kill_on_drop(true)
        .output()
        .await?;
    if !output.status.success() {
        return Err(ServiceError::Internal(
            "could not identify the FFmpeg sidecar".to_owned(),
        ));
    }
    Ok(String::from_utf8_lossy(&output.stdout)
        .lines()
        .take(2)
        .collect::<Vec<_>>()
        .join("\n"))
}

pub(super) async fn probe_duration_ms(
    sidecars: &SidecarPair,
    path: &Path,
) -> Result<u64, ServiceError> {
    let output = Command::new(&sidecars.ffprobe)
        .args([
            "-v",
            "error",
            "-show_entries",
            "format=duration",
            "-of",
            "default=noprint_wrappers=1:nokey=1",
        ])
        .arg(path)
        .kill_on_drop(true)
        .output()
        .await?;
    if !output.status.success() {
        return Err(ServiceError::Internal(format!(
            "ffprobe could not validate {}",
            path.display()
        )));
    }
    let seconds = String::from_utf8_lossy(&output.stdout)
        .trim()
        .parse::<f64>()
        .map_err(|_| ServiceError::Internal("ffprobe returned an invalid duration".to_owned()))?;
    if !seconds.is_finite() || seconds <= 0.0 {
        return Err(ServiceError::Internal(
            "audio duration is not positive".to_owned(),
        ));
    }
    let duration = Duration::try_from_secs_f64(seconds)
        .map_err(|_| ServiceError::Internal("audio duration is out of range".to_owned()))?;
    Ok(u64::try_from(duration.as_millis())
        .unwrap_or(u64::MAX)
        .max(1))
}

pub(super) async fn validate_flac(path: &Path) -> Result<(), ServiceError> {
    let mut file = tokio::fs::File::open(path).await?;
    let mut signature = [0_u8; 4];
    file.read_exact(&mut signature).await?;
    if signature != *b"fLaC" {
        return Err(ServiceError::Internal(
            "chapter assembly did not produce FLAC".to_owned(),
        ));
    }
    Ok(())
}

pub(super) async fn copy_file_atomically(
    source: &Path,
    destination: &Path,
) -> Result<(), ServiceError> {
    let parent = destination.parent().ok_or_else(|| {
        ServiceError::InvalidRequest("destination has no parent directory".to_owned())
    })?;
    tokio::fs::create_dir_all(parent).await?;
    let temporary = tempfile::Builder::new()
        .prefix(".audiobookai-copy-")
        .tempfile_in(parent)
        .map_err(ServiceError::Io)?
        .into_temp_path();
    tokio::fs::copy(source, &temporary).await?;
    sync_file(&temporary).await?;
    atomic_promote(&temporary, destination).await
}

pub(super) async fn write_file_atomically(
    destination: &Path,
    bytes: &[u8],
) -> Result<(), ServiceError> {
    let parent = destination.parent().ok_or_else(|| {
        ServiceError::InvalidRequest("destination has no parent directory".to_owned())
    })?;
    tokio::fs::create_dir_all(parent).await?;
    let temporary = tempfile::Builder::new()
        .prefix(".audiobookai-write-")
        .tempfile_in(parent)
        .map_err(ServiceError::Io)?
        .into_temp_path();
    tokio::fs::write(&temporary, bytes).await?;
    sync_file(&temporary).await?;
    atomic_promote(&temporary, destination).await
}

pub(super) async fn write_job_staging_file_atomically(
    private_root: &Path,
    destination: &Path,
    bytes: &[u8],
) -> Result<(), ServiceError> {
    ensure_private_staging_file_path(private_root, destination).await?;
    let parent = destination.parent().ok_or_else(|| {
        ServiceError::InvalidRequest("staging destination has no parent directory".to_owned())
    })?;
    let temporary = tempfile::Builder::new()
        .prefix(".audiobookai-staging-write-")
        .tempfile_in(parent)
        .map_err(ServiceError::Io)?
        .into_temp_path();
    tokio::fs::write(&temporary, bytes).await?;
    sync_file(&temporary).await?;
    ensure_private_staging_file_path(private_root, destination).await?;
    match tokio::fs::symlink_metadata(destination).await {
        Ok(metadata) if metadata.file_type().is_file() && !metadata.file_type().is_symlink() => {
            tokio::fs::remove_file(destination).await?;
        }
        Ok(_) => {
            return Err(ServiceError::Conflict(format!(
                "job staging destination is not a regular file: {}",
                destination.display()
            )));
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(ServiceError::Io(error)),
    }
    atomic_promote(&temporary, destination).await
}

pub(super) async fn prepare_private_export_staging(
    state: &AppState,
    job_id: JobId,
) -> Result<PathBuf, ServiceError> {
    let managed_root = tokio::fs::canonicalize(&state.config.data_dir)
        .await
        .map_err(|error| {
            ServiceError::Conflict(format!(
                "managed application data directory is unavailable ({}): {error}",
                state.config.data_dir.display()
            ))
        })?;
    let jobs = managed_root.join("jobs");
    ensure_private_directory(&jobs).await?;
    let job = jobs.join(job_id.to_string());
    ensure_private_directory(&job).await?;
    let staging = job.join("export-staging");
    ensure_private_directory(&staging).await?;
    Ok(staging)
}

pub(super) async fn ensure_private_directory(path: &Path) -> Result<(), ServiceError> {
    match tokio::fs::symlink_metadata(path).await {
        Ok(metadata) => {
            if metadata.file_type().is_symlink() || !metadata.is_dir() {
                return Err(ServiceError::Conflict(format!(
                    "managed export staging component is not a private directory: {}",
                    path.display()
                )));
            }
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            match tokio::fs::create_dir(path).await {
                Ok(()) => {}
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
                    let metadata = tokio::fs::symlink_metadata(path).await?;
                    if metadata.file_type().is_symlink() || !metadata.is_dir() {
                        return Err(ServiceError::Conflict(format!(
                            "managed export staging component was replaced during creation: {}",
                            path.display()
                        )));
                    }
                }
                Err(error) => return Err(ServiceError::Io(error)),
            }
        }
        Err(error) => return Err(ServiceError::Io(error)),
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        tokio::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700)).await?;
    }
    Ok(())
}

pub(super) async fn ensure_existing_real_directory(
    path: &Path,
    description: &str,
) -> Result<(), ServiceError> {
    let metadata = tokio::fs::symlink_metadata(path).await.map_err(|error| {
        ServiceError::Conflict(format!(
            "{description} is unavailable ({}): {error}",
            path.display()
        ))
    })?;
    if metadata.file_type().is_symlink() || !metadata.is_dir() {
        return Err(ServiceError::Conflict(format!(
            "{description} is not a real directory: {}",
            path.display()
        )));
    }
    Ok(())
}

pub(super) async fn ensure_private_staging_file_path(
    private_root: &Path,
    path: &Path,
) -> Result<(), ServiceError> {
    ensure_existing_real_directory(private_root, "managed export staging root").await?;
    if path == private_root || !path.starts_with(private_root) {
        return Err(ServiceError::Conflict(format!(
            "export staging file escapes its job-private directory: {}",
            path.display()
        )));
    }
    let parent = path.parent().ok_or_else(|| {
        ServiceError::Conflict("export staging file has no parent directory".to_owned())
    })?;
    ensure_existing_real_directory(parent, "managed export staging parent").await?;
    let canonical_root = tokio::fs::canonicalize(private_root).await?;
    let canonical_parent = tokio::fs::canonicalize(parent).await?;
    if !canonical_parent.starts_with(&canonical_root) {
        return Err(ServiceError::Conflict(format!(
            "export staging parent resolves outside its job-private directory: {}",
            path.display()
        )));
    }
    match tokio::fs::symlink_metadata(path).await {
        Ok(metadata) if metadata.is_file() && !metadata.file_type().is_symlink() => Ok(()),
        Ok(_) => Err(ServiceError::Conflict(format!(
            "export staging destination is not a regular private file: {}",
            path.display()
        ))),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(ServiceError::Io(error)),
    }
}

pub(super) async fn sync_file(path: &Path) -> Result<(), ServiceError> {
    let mut options = tokio::fs::OpenOptions::new();
    #[cfg(windows)]
    options.write(true);
    #[cfg(not(windows))]
    options.read(true);
    options.open(path).await?.sync_all().await?;
    Ok(())
}

pub(super) fn no_clobber_conflict(destination: &Path) -> ServiceError {
    ServiceError::Conflict(format!("refusing to overwrite {}", destination.display()))
}

pub(super) async fn failed_no_clobber_operation_is_conflict(
    error: &std::io::Error,
    destination: &Path,
) -> bool {
    error.kind() == std::io::ErrorKind::AlreadyExists
        || tokio::fs::symlink_metadata(destination).await.is_ok()
}

pub(super) async fn atomic_promote(source: &Path, destination: &Path) -> Result<(), ServiceError> {
    sync_file(source).await?;
    match tokio::fs::hard_link(source, destination).await {
        Ok(()) => {}
        Err(error) => {
            if failed_no_clobber_operation_is_conflict(&error, destination).await {
                return Err(no_clobber_conflict(destination));
            }
            tracing::debug!(
                diagnostic_code = "storage.promotion.hard_link.unavailable",
                %error,
                source = %source.display(),
                destination = %destination.display(),
                "falling back to exclusive-copy promotion"
            );
            copy_file_no_clobber(source, destination).await?;
        }
    }
    // The destination link already names the complete source inode. Source cleanup is
    // best-effort so a staging unlink failure cannot turn a successful no-clobber promotion into
    // an ambiguous retry that sees an existing final path.
    let _ = tokio::fs::remove_file(source).await;
    #[cfg(unix)]
    if let Some(parent) = destination.parent() {
        tokio::fs::File::open(parent).await?.sync_all().await?;
    }
    Ok(())
}

pub(super) async fn copy_file_no_clobber(
    source: &Path,
    destination: &Path,
) -> Result<(), ServiceError> {
    let mut input = tokio::fs::File::open(source).await?;
    copy_reader_no_clobber(&mut input, destination).await
}

pub(super) async fn copy_reader_no_clobber<R>(
    input: &mut R,
    destination: &Path,
) -> Result<(), ServiceError>
where
    R: tokio::io::AsyncRead + Unpin,
{
    let mut output = match tokio::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(destination)
        .await
    {
        Ok(output) => output,
        Err(error) => {
            if failed_no_clobber_operation_is_conflict(&error, destination).await {
                return Err(no_clobber_conflict(destination));
            }
            return Err(ServiceError::Io(error));
        }
    };
    if let Err(error) = tokio::io::copy(input, &mut output).await {
        drop(output);
        // The destination was created exclusively, but pathname ownership can change after this
        // handle is opened. Removing by path here could therefore delete a foreign replacement.
        // Retain the partial file fail-closed instead; public export callers already hold their
        // durable promotion reservation before this fallback can publish a destination name.
        return Err(ServiceError::Io(error));
    }
    if let Err(error) = output.sync_all().await {
        drop(output);
        // As above, never perform path-based cleanup after publishing the destination name.
        return Err(ServiceError::Io(error));
    }
    Ok(())
}

pub(super) async fn create_directory_no_clobber(destination: &Path) -> Result<(), ServiceError> {
    match tokio::fs::create_dir(destination).await {
        Ok(()) => {
            #[cfg(unix)]
            if let Some(parent) = destination.parent() {
                tokio::fs::File::open(parent).await?.sync_all().await?;
            }
            Ok(())
        }
        Err(error) => {
            if failed_no_clobber_operation_is_conflict(&error, destination).await {
                Err(no_clobber_conflict(destination))
            } else {
                Err(ServiceError::Io(error))
            }
        }
    }
}

pub(super) async fn artifact_for_file(
    kind: ArtifactKind,
    path: &Path,
    media_type: Option<String>,
    duration_ms: Option<u64>,
    cache_key: Option<String>,
    pinned_by_job_id: Option<JobId>,
) -> Result<Artifact, ServiceError> {
    artifact_for_file_with_id(
        ArtifactId::new(),
        kind,
        path,
        media_type,
        duration_ms,
        cache_key,
        pinned_by_job_id,
    )
    .await
}

#[allow(clippy::too_many_arguments)]
pub(super) async fn artifact_for_file_with_id(
    id: ArtifactId,
    kind: ArtifactKind,
    path: &Path,
    media_type: Option<String>,
    duration_ms: Option<u64>,
    cache_key: Option<String>,
    pinned_by_job_id: Option<JobId>,
) -> Result<Artifact, ServiceError> {
    let path = tokio::fs::canonicalize(path).await?;
    let fingerprint = fingerprint_file(&path).await?;
    let now = Utc::now();
    Ok(Artifact {
        id,
        kind,
        path: path.to_string_lossy().into_owned(),
        fingerprint,
        media_type,
        duration_ms,
        cache_key,
        pinned_by_job_id,
        created_at: now,
        last_accessed_at: now,
    })
}

pub(super) async fn fingerprint_file(path: &Path) -> Result<FileFingerprint, ServiceError> {
    let mut file = tokio::fs::File::open(path).await?;
    let mut hasher = blake3::Hasher::new();
    let mut size = 0_u64;
    let mut buffer = vec![0_u8; RANGE_CHUNK_BYTES];
    loop {
        let read = file.read(&mut buffer).await?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
        size = size.saturating_add(u64::try_from(read).unwrap_or(u64::MAX));
    }
    Ok(FileFingerprint {
        algorithm: "blake3".to_owned(),
        digest: hasher.finalize().to_hex().to_string(),
        size_bytes: size,
    })
}

pub(super) async fn verify_selected_artifact_integrity(
    artifact: &Artifact,
) -> Result<(), ServiceError> {
    if artifact.fingerprint.algorithm != "blake3"
        || artifact.fingerprint.digest.len() != 64
        || !artifact
            .fingerprint
            .digest
            .bytes()
            .all(|byte| byte.is_ascii_hexdigit())
    {
        return Err(ServiceError::Conflict(format!(
            "selected artifact {} has no known BLAKE3 fingerprint",
            artifact.id
        )));
    }
    let path = Path::new(&artifact.path);
    let metadata = tokio::fs::symlink_metadata(path).await.map_err(|error| {
        ServiceError::Conflict(format!(
            "selected artifact {} is unavailable: {error}",
            artifact.id
        ))
    })?;
    if metadata.file_type().is_symlink() || !metadata.is_file() {
        return Err(ServiceError::Conflict(format!(
            "selected artifact {} is not a regular managed file",
            artifact.id
        )));
    }
    if fingerprint_file(path).await? != artifact.fingerprint {
        return Err(ServiceError::Conflict(format!(
            "selected artifact {} no longer matches its stored BLAKE3 fingerprint",
            artifact.id
        )));
    }
    Ok(())
}

pub(super) async fn verify_selected_artifacts_before_use(
    artifacts: &[&Artifact],
) -> Result<(), ServiceError> {
    for artifact in artifacts {
        verify_selected_artifact_integrity(artifact).await?;
    }
    Ok(())
}

pub(super) async fn persist_artifact(
    state: &AppState,
    project_id: Uuid,
    artifact: &Artifact,
) -> Result<(), ServiceError> {
    let result = sqlx::query(
        "INSERT INTO artifacts \
         (id, project_id, kind, path, cache_key, pinned_by_job_id, created_at, last_accessed_at, payload) \
         VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?)",
    )
    .bind(artifact.id.to_string())
    .bind(project_id.to_string())
    .bind(artifact_kind_name(artifact.kind))
    .bind(&artifact.path)
    .bind(&artifact.cache_key)
    .bind(artifact.pinned_by_job_id.map(|id| id.to_string()))
    .bind(artifact.created_at.to_rfc3339())
    .bind(artifact.last_accessed_at.to_rfc3339())
    .bind(serde_json::to_string(artifact).map_err(internal_error)?)
    .execute(state.database.pool())
    .await;
    match result {
        Ok(_) => Ok(()),
        Err(error)
            if error
                .to_string()
                .contains("UNIQUE constraint failed: artifacts.cache_key") =>
        {
            Ok(())
        }
        Err(error) => Err(storage_error(error)),
    }
}

pub(super) async fn load_artifact(
    state: &AppState,
    id: ArtifactId,
) -> Result<Artifact, ServiceError> {
    let payload = sqlx::query_scalar::<_, String>("SELECT payload FROM artifacts WHERE id = ?")
        .bind(id.to_string())
        .fetch_optional(state.database.pool())
        .await
        .map_err(storage_error)?
        .ok_or(ServiceError::NotFound)?;
    let artifact: Artifact = serde_json::from_str(&payload).map_err(internal_error)?;
    if !Path::new(&artifact.path).is_file() {
        return Err(ServiceError::Conflict(format!(
            "artifact file is missing: {}",
            artifact.path
        )));
    }
    Ok(artifact)
}

pub(super) async fn artifact_path(
    state: &AppState,
    id: ArtifactId,
) -> Result<PathBuf, ServiceError> {
    load_artifact(state, id)
        .await
        .map(|artifact| PathBuf::from(artifact.path))
}

pub(super) const fn artifact_kind_name(kind: ArtifactKind) -> &'static str {
    match kind {
        ArtifactKind::ImportedEpub => "imported_epub",
        ArtifactKind::Cover => "cover",
        ArtifactKind::ReferenceAudio => "reference_audio",
        ArtifactKind::Preview => "preview",
        ArtifactKind::SegmentAudio => "segment_audio",
        ArtifactKind::ChapterMaster => "chapter_master",
        ArtifactKind::MixedMaster => "mixed_master",
        ArtifactKind::Export => "export",
        ArtifactKind::ExportManifest => "export_manifest",
    }
}

pub(super) fn media_type_for_path(path: &Path) -> String {
    let value = match path
        .extension()
        .and_then(std::ffi::OsStr::to_str)
        .unwrap_or_default()
        .to_ascii_lowercase()
        .as_str()
    {
        "mp3" => "audio/mpeg",
        "wav" => "audio/wav",
        "m4a" | "m4b" => "audio/mp4",
        "flac" => "audio/flac",
        "json" => "application/json",
        _ => "application/octet-stream",
    };
    value.to_owned()
}

pub(super) fn export_destination(profile: &ExportProfile) -> PathBuf {
    let root = PathBuf::from(&profile.output_directory);
    if profile.layout == ExportLayout::PerChapter {
        root.join(&profile.filename_template)
    } else {
        root.join(format!(
            "{}.{}",
            profile.filename_template,
            media_export_format(profile.format).extension()
        ))
    }
}

pub(super) fn export_manifest_path(layout: ExportLayout, final_output: &Path) -> PathBuf {
    if layout == ExportLayout::PerChapter {
        final_output.join("audiobookai-export-manifest.json")
    } else {
        final_output.with_file_name(format!(
            "{}.manifest.json",
            final_output
                .file_name()
                .and_then(std::ffi::OsStr::to_str)
                .unwrap_or("audiobook")
        ))
    }
}

pub(super) fn output_destination_key(destination: &Path) -> String {
    // Export folders are canonicalized before the profile is stored. Case-folding the complete
    // child path and Unicode-normalizing it is deliberately conservative: it prevents a database
    // moved between filesystems, or running on normalization-insensitive APFS, from admitting two
    // paid jobs whose destinations alias there.
    audiobookai_storage::normalize_output_destination_key(&destination.to_string_lossy())
}

pub(super) async fn ensure_export_root_identity(
    profile: &ExportProfile,
) -> Result<(), ServiceError> {
    let expected = Path::new(&profile.output_directory);
    let metadata = tokio::fs::symlink_metadata(expected)
        .await
        .map_err(|error| {
            ServiceError::Conflict(format!(
                "reserved export directory is unavailable ({}): {error}",
                expected.display()
            ))
        })?;
    if metadata.file_type().is_symlink() || !metadata.is_dir() {
        return Err(ServiceError::Conflict(format!(
            "reserved export directory was replaced by a non-directory or symlink: {}",
            expected.display()
        )));
    }
    let actual = tokio::fs::canonicalize(expected).await?;
    if actual != expected {
        return Err(ServiceError::Conflict(format!(
            "reserved export directory now resolves to a different location: {} -> {}",
            expected.display(),
            actual.display()
        )));
    }
    Ok(())
}

pub(super) async fn prospective_canonical_path(path: &Path) -> Result<PathBuf, ServiceError> {
    if !path.is_absolute() {
        return Err(ServiceError::InvalidRequest(
            "the export path must be absolute".to_owned(),
        ));
    }
    let mut cursor = path.to_path_buf();
    let mut suffix = Vec::new();
    if let Some(component) = cursor.file_name() {
        suffix.push(component.to_os_string());
        cursor = cursor.parent().unwrap_or(Path::new("/")).to_path_buf();
    }
    let mut base = loop {
        match tokio::fs::canonicalize(&cursor).await {
            Ok(base) => break base,
            Err(error)
                if matches!(
                    error.kind(),
                    std::io::ErrorKind::NotFound | std::io::ErrorKind::NotADirectory
                ) =>
            {
                let component = cursor.file_name().ok_or_else(|| {
                    ServiceError::Conflict(format!(
                        "cannot resolve the export path {}",
                        path.display()
                    ))
                })?;
                suffix.push(component.to_os_string());
                cursor = cursor
                    .parent()
                    .ok_or_else(|| {
                        ServiceError::Conflict(format!(
                            "cannot resolve the export path {}",
                            path.display()
                        ))
                    })?
                    .to_path_buf();
            }
            Err(error) => return Err(ServiceError::Io(error)),
        }
    };
    for component in suffix.into_iter().rev() {
        base.push(component);
    }
    Ok(lexically_normalized_path(&base))
}

pub(super) fn lexically_normalized_path(path: &Path) -> PathBuf {
    let mut prefix = None;
    let mut rooted = false;
    let mut components = Vec::new();
    for component in path.components() {
        match component {
            std::path::Component::Prefix(value) => {
                prefix = Some(value.as_os_str().to_os_string());
            }
            std::path::Component::RootDir => {
                rooted = true;
                components.clear();
            }
            std::path::Component::CurDir => {}
            std::path::Component::ParentDir => {
                components.pop();
            }
            std::path::Component::Normal(value) => components.push(value.to_os_string()),
        }
    }
    let mut normalized = PathBuf::new();
    if let Some(prefix) = prefix {
        normalized.push(prefix);
    }
    if rooted {
        normalized.push(std::path::MAIN_SEPARATOR_STR);
    }
    for component in components {
        normalized.push(component);
    }
    normalized
}

pub(super) async fn ensure_output_directory_not_reserved(
    state: &AppState,
    output_directory: &Path,
) -> Result<(), ServiceError> {
    let normalized = prospective_canonical_path(output_directory).await?;
    let key = output_destination_key(&normalized);
    if let Some(existing) = state
        .database
        .repositories()
        .jobs
        .find_output_reservation_containing_path(&key)
        .await
        .map_err(storage_error)?
    {
        return Err(ServiceError::ConflictDetails {
            code: "output_directory_reserved",
            detail: format!(
                "the export directory is inside another job's reserved destination: {}",
                existing.destination_path
            ),
            meta: serde_json::json!({
                "destination": existing.destination_path,
                "ownerJobId": existing.job_id,
                "ownerProjectId": existing.project_id,
            }),
        });
    }
    Ok(())
}

pub(super) async fn prepare_output_reservation(
    job_id: JobId,
    project_id: ProjectId,
    profile: &ExportProfile,
    now: chrono::DateTime<Utc>,
) -> Result<OutputDestinationReservation, ServiceError> {
    let root = PathBuf::from(&profile.output_directory);
    ensure_export_root_identity(profile).await?;
    let destination = export_destination(profile);
    if destination.exists() {
        return Err(ServiceError::Conflict(format!(
            "export destination already exists: {}",
            destination.display()
        )));
    }
    let manifest = export_manifest_path(profile.layout, &destination);
    if manifest.exists() {
        return Err(ServiceError::Conflict(format!(
            "export manifest destination already exists: {}",
            manifest.display()
        )));
    }
    let probe = tempfile::Builder::new()
        .prefix(".audiobookai-write-test-")
        .tempfile_in(&root)
        .map_err(|error| {
            ServiceError::Conflict(format!(
                "export directory is not writable ({}): {error}",
                root.display()
            ))
        })?;
    drop(probe);
    ensure_export_root_identity(profile).await?;
    Ok(OutputDestinationReservation {
        job_id,
        project_id,
        destination_key: output_destination_key(&destination),
        destination_path: destination.to_string_lossy().into_owned(),
        layout: profile.layout,
        state: OutputReservationState::Reserved,
        created_at: now,
        updated_at: now,
        promoted_at: None,
    })
}

pub(super) fn output_reservation_admission_error(error: StorageError) -> ServiceError {
    match error {
        StorageError::Conflict {
            entity: "output destination",
            id,
        } => ServiceError::ConflictDetails {
            code: "output_destination_reserved",
            detail: format!("another job already owns the export destination: {id}"),
            meta: serde_json::json!({"destination": id}),
        },
        other => storage_error(other),
    }
}

pub(super) async fn require_output_reservation(
    state: &AppState,
    job_id: JobId,
    project_id: ProjectId,
    profile: &ExportProfile,
    destination: &Path,
) -> Result<OutputDestinationReservation, ServiceError> {
    let reservation = state
        .database
        .repositories()
        .jobs
        .get_output_reservation(job_id)
        .await
        .map_err(storage_error)?
        .ok_or_else(|| {
            ServiceError::Conflict(
                "export job has no durable output destination reservation".to_owned(),
            )
        })?;
    if reservation.project_id != project_id
        || reservation.destination_key != output_destination_key(destination)
        || Path::new(&reservation.destination_path) != destination
        || reservation.layout != profile.layout
    {
        return Err(ServiceError::Conflict(
            "export job does not own its planned output destination".to_owned(),
        ));
    }
    ensure_export_root_identity(profile).await?;
    if reservation.state == OutputReservationState::Promoted && !destination.exists() {
        return Err(ServiceError::Conflict(
            "the job-owned promoted export destination is missing".to_owned(),
        ));
    }
    Ok(reservation)
}

pub(super) async fn ensure_existing_job_output_reservation(
    state: &AppState,
    job: &Job,
) -> Result<(), ServiceError> {
    let profile = load_export_profile(
        state,
        job.export_profile_id.ok_or_else(|| {
            ServiceError::Conflict("export job has no durable export profile".to_owned())
        })?,
    )
    .await?;
    let destination = export_destination(&profile);
    let repository = state.database.repositories().jobs;
    if repository
        .get_output_reservation(job.id)
        .await
        .map_err(storage_error)?
        .is_some()
    {
        require_output_reservation(state, job.id, job.project_id, &profile, &destination).await?;
        return Ok(());
    }
    ensure_output_directory_not_reserved(state, Path::new(&profile.output_directory)).await?;
    let reservation =
        prepare_output_reservation(job.id, job.project_id, &profile, Utc::now()).await?;
    repository
        .acquire_output_reservation_for_existing_job(job, &reservation)
        .await
        .map_err(output_reservation_admission_error)
}
