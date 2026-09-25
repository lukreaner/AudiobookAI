use super::{
    AppState, Arc, AsyncWriteExt, BookSummary, ChapterDisplayStatus, ChapterView, CommitImport,
    Deserialize, FilePath, HashSet, ImportDraft, ImportRecord, JobStatusView, JobView, Json,
    Multipart, Page, Path, ProjectDetail, ProjectDisplayStatus, Response, ReviewStatus,
    ServiceError, State, StatusCode, Utc, Uuid, binary_response, estimated_seconds,
    imported_project_settings, json_f32, optional_string, refresh_project_summary, reject_empty,
};

pub(super) async fn list_projects(State(state): State<Arc<AppState>>) -> Json<Page<BookSummary>> {
    let catalog = state.catalog.read().await;
    let mut projects = catalog
        .projects
        .values()
        .map(|project| project.summary.clone())
        .collect::<Vec<_>>();
    projects.sort_by_key(|project| std::cmp::Reverse(project.updated_at));
    Json(Page::all(projects))
}

pub(super) async fn get_project(
    State(state): State<Arc<AppState>>,
    Path(id): Path<Uuid>,
) -> Result<Json<ProjectDetail>, ServiceError> {
    let catalog = state.catalog.read().await;
    catalog
        .projects
        .get(&id)
        .cloned()
        .map(Json)
        .ok_or(ServiceError::NotFound)
}

#[allow(clippy::too_many_lines)]
pub(super) async fn update_project(
    State(state): State<Arc<AppState>>,
    Path(id): Path<Uuid>,
    Json(patch): Json<serde_json::Value>,
) -> Result<Json<ProjectDetail>, ServiceError> {
    let _model_lifecycle_guard = state.model_lifecycle.lock().await;
    let changes_dispatch_consent =
        patch.get("consentCloudText").is_some() || patch.get("consentCloudAudio").is_some();
    let dispatch_consent_lock = if changes_dispatch_consent {
        Some(state.dispatch_consent_lifecycle_lock(id).await)
    } else {
        None
    };
    let _dispatch_consent_guard = if let Some(lock) = dispatch_consent_lock.as_ref() {
        Some(lock.write().await)
    } else {
        None
    };
    let project_lock = state.character_lifecycle_lock(id).await;
    let _project_guard = project_lock.lock().await;
    let mut project = state
        .catalog
        .read()
        .await
        .projects
        .get(&id)
        .cloned()
        .ok_or(ServiceError::NotFound)?;
    if let Some(title) = patch.get("title").and_then(serde_json::Value::as_str) {
        reject_empty("title", title)?;
        project.summary.title = title.to_owned();
    }
    if let Some(author) = optional_string(&patch, "author") {
        project.summary.author = author;
    }
    if let Some(narrator) = optional_string(&patch, "narrator") {
        project.narrator = narrator;
    }
    if let Some(publisher) = optional_string(&patch, "publisher") {
        project.publisher = publisher;
    }
    if let Some(description) = optional_string(&patch, "description") {
        project.description = description;
    }
    if let Some(language) = optional_string(&patch, "language") {
        project.summary.language = language;
    }
    if let Some(series) = optional_string(&patch, "series") {
        project.summary.series = series;
    }
    if let Some(position) = patch.get("seriesPosition") {
        project.summary.series_position = if position.is_null() {
            None
        } else {
            Some(json_f32(position, "seriesPosition")?)
        };
    }
    if let Some(value) = patch
        .get("consentCloudText")
        .and_then(serde_json::Value::as_bool)
    {
        project.consent_cloud_text = value;
    }
    if let Some(value) = patch
        .get("consentCloudAudio")
        .and_then(serde_json::Value::as_bool)
    {
        project.consent_cloud_audio = value;
    }
    if let Some(output_name) = optional_string(&patch, "outputName") {
        project.output_name = output_name;
    }
    if let Some(chapters) = patch.get("chapters").and_then(serde_json::Value::as_array) {
        for chapter_patch in chapters {
            let Some(chapter_id) = chapter_patch
                .get("id")
                .and_then(serde_json::Value::as_str)
                .and_then(|value| Uuid::parse_str(value).ok())
            else {
                continue;
            };
            if let Some(chapter) = project
                .chapters
                .iter_mut()
                .find(|item| item.id == chapter_id)
            {
                if let Some(selected) = chapter_patch
                    .get("selected")
                    .and_then(serde_json::Value::as_bool)
                {
                    chapter.selected = selected;
                }
                if let Some(title) = chapter_patch
                    .get("title")
                    .and_then(serde_json::Value::as_str)
                {
                    reject_empty("chapter.title", title)?;
                    title.clone_into(&mut chapter.title);
                }
            }
        }
    }
    refresh_project_summary(&mut project);
    persist_project_view(&state, &project).await?;
    state
        .catalog
        .write()
        .await
        .projects
        .insert(id, project.clone());
    state
        .events
        .publish("project.updated", serde_json::json!({ "projectId": id }));
    Ok(Json(project))
}

pub(super) async fn delete_project(
    State(state): State<Arc<AppState>>,
    Path(id): Path<Uuid>,
) -> Result<StatusCode, ServiceError> {
    let _model_lifecycle_guard = state.model_lifecycle.lock().await;
    let dispatch_consent_lock = state.dispatch_consent_lifecycle_lock(id).await;
    let _dispatch_consent_guard = dispatch_consent_lock.write().await;
    let project_lock = state.character_lifecycle_lock(id).await;
    let _project_guard = project_lock.lock().await;
    // Deleting would orphan any job still working on the project, including previews.
    let active_job = state
        .catalog
        .read()
        .await
        .jobs
        .values()
        .find(|job| job.project_id == id && active_job_status(job.status))
        .cloned();
    if let Some(job) = active_job {
        return Err(active_job_conflict(&job));
    }
    archive_project(&state, id).await?;
    let mut catalog = state.catalog.write().await;
    catalog.projects.remove(&id).ok_or(ServiceError::NotFound)?;
    catalog.characters.remove(&id);
    catalog.jobs.retain(|_, job| job.project_id != id);
    drop(catalog);
    state
        .events
        .publish("project.deleted", serde_json::json!({ "projectId": id }));
    Ok(StatusCode::NO_CONTENT)
}

pub(super) async fn persist_project_view(
    state: &AppState,
    view: &ProjectDetail,
) -> Result<(), ServiceError> {
    use audiobookai_core::{ProjectId, SeriesMetadata, SeriesMetadataSource};

    let project_id = ProjectId::from_uuid(view.summary.id);
    let repository = state.database.repositories().projects;
    let mut project = repository
        .get_project(project_id)
        .await
        .map_err(|error| ServiceError::Storage(error.to_string()))?
        .ok_or(ServiceError::NotFound)?;
    project.name = view.summary.title.clone();
    project.metadata.title = view.summary.title.clone();
    project.metadata.authors = view.summary.author.iter().cloned().collect();
    project.metadata.narrator.clone_from(&view.narrator);
    project.metadata.publisher.clone_from(&view.publisher);
    project.metadata.description.clone_from(&view.description);
    project.metadata.language.clone_from(&view.summary.language);
    project.metadata.series = view.summary.series.as_ref().map(|name| SeriesMetadata {
        name: name.clone(),
        position: view.summary.series_position,
        source: SeriesMetadataSource::User,
    });
    let consent_changed = project.cloud_consent.book_text != view.consent_cloud_text
        || project.cloud_consent.reference_audio != view.consent_cloud_audio;
    project.cloud_consent.book_text = view.consent_cloud_text;
    project.cloud_consent.reference_audio = view.consent_cloud_audio;
    if consent_changed {
        project.cloud_consent.granted_at =
            (view.consent_cloud_text || view.consent_cloud_audio).then(Utc::now);
    }
    if let Some(output_name) = &view.output_name {
        project
            .settings
            .output_name_template
            .clone_from(output_name);
    }
    project.updated_at = Utc::now();
    let revision = sqlx::query_scalar::<_, i64>("SELECT revision FROM projects WHERE id = ?")
        .bind(project_id.to_string())
        .fetch_optional(state.database.pool())
        .await
        .map_err(|error| ServiceError::Storage(error.to_string()))?
        .ok_or(ServiceError::NotFound)?;
    repository
        .update_project(
            &project,
            u64::try_from(revision).map_err(|_| {
                ServiceError::Internal("stored project revision is invalid".to_owned())
            })?,
        )
        .await
        .map_err(|error| ServiceError::Storage(error.to_string()))?;

    let book_id = project.book_id;
    let mut chapters = repository
        .list_chapters(book_id)
        .await
        .map_err(|error| ServiceError::Storage(error.to_string()))?;
    for chapter in &mut chapters {
        let Some(view_chapter) = view
            .chapters
            .iter()
            .find(|candidate| candidate.id == chapter.id.as_uuid())
        else {
            continue;
        };
        if chapter.selected == view_chapter.selected && chapter.title == view_chapter.title {
            continue;
        }
        chapter.selected = view_chapter.selected;
        chapter.title.clone_from(&view_chapter.title);
        sqlx::query("UPDATE chapters SET selected = ?, payload = ? WHERE id = ?")
            .bind(chapter.selected)
            .bind(
                serde_json::to_string(chapter)
                    .map_err(|error| ServiceError::Internal(error.to_string()))?,
            )
            .bind(chapter.id.to_string())
            .execute(state.database.pool())
            .await
            .map_err(|error| ServiceError::Storage(error.to_string()))?;
    }
    Ok(())
}

pub(super) async fn archive_project(state: &AppState, id: Uuid) -> Result<(), ServiceError> {
    use audiobookai_core::{ProjectId, ProjectStatus};

    let project_id = ProjectId::from_uuid(id);
    let repository = state.database.repositories().projects;
    let mut project = repository
        .get_project(project_id)
        .await
        .map_err(|error| ServiceError::Storage(error.to_string()))?
        .ok_or(ServiceError::NotFound)?;
    let revision = sqlx::query_scalar::<_, i64>("SELECT revision FROM projects WHERE id = ?")
        .bind(project_id.to_string())
        .fetch_one(state.database.pool())
        .await
        .map_err(|error| ServiceError::Storage(error.to_string()))?;
    project.status = ProjectStatus::Archived;
    project.updated_at = Utc::now();
    repository
        .update_project(
            &project,
            u64::try_from(revision)
                .map_err(|_| ServiceError::Internal("stored revision is invalid".to_owned()))?,
        )
        .await
        .map_err(|error| ServiceError::Storage(error.to_string()))?;
    Ok(())
}

pub(super) async fn create_import_draft(
    State(state): State<Arc<AppState>>,
    mut multipart: Multipart,
) -> Result<(StatusCode, Json<ImportDraft>), ServiceError> {
    let draft_id = Uuid::new_v4();
    let import_dir = state.config.data_dir.join("imports");
    tokio::fs::create_dir_all(&import_dir).await?;
    let path = import_dir.join(format!("{draft_id}.epub"));
    let mut source_name = None;
    let mut received = false;

    while let Some(mut field) = multipart
        .next_field()
        .await
        .map_err(|error| ServiceError::InvalidRequest(error.to_string()))?
    {
        if field.name() != Some("epub") {
            continue;
        }
        source_name = field.file_name().map(str::to_owned);
        let mut file = tokio::fs::File::create(&path).await?;
        let mut total = 0_u64;
        while let Some(chunk) = field
            .chunk()
            .await
            .map_err(|error| ServiceError::InvalidRequest(error.to_string()))?
        {
            total = total.saturating_add(chunk.len() as u64);
            if total > audiobookai_epub::ImportLimits::default().max_archive_bytes {
                let _ = tokio::fs::remove_file(&path).await;
                return Err(ServiceError::InvalidRequest(
                    "EPUB exceeds the 1 GiB import limit".to_owned(),
                ));
            }
            file.write_all(&chunk).await?;
        }
        file.sync_all().await?;
        received = true;
        break;
    }
    if !received {
        return Err(ServiceError::InvalidRequest(
            "multipart field 'epub' is required".to_owned(),
        ));
    }

    finish_import_draft(&state, draft_id, path, source_name).await
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct ImportFromPathInput {
    pub(super) source_path: std::path::PathBuf,
}

pub(super) async fn copy_file_durably(
    source: &FilePath,
    destination: &FilePath,
) -> Result<(), ServiceError> {
    let mut source_file = tokio::fs::File::open(source).await?;
    let mut destination_file = tokio::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(destination)
        .await?;
    tokio::io::copy(&mut source_file, &mut destination_file).await?;
    destination_file.sync_all().await?;
    Ok(())
}

pub(super) async fn create_import_draft_from_path(
    State(state): State<Arc<AppState>>,
    Json(input): Json<ImportFromPathInput>,
) -> Result<(StatusCode, Json<ImportDraft>), ServiceError> {
    if !state.config.desktop_bootstrap || !state.config.bind.ip().is_loopback() {
        return Err(ServiceError::Forbidden(
            "local-path import is available only to the authenticated desktop host".to_owned(),
        ));
    }
    let source = tokio::fs::canonicalize(&input.source_path)
        .await
        .map_err(|error| {
            if error.kind() == std::io::ErrorKind::NotFound {
                ServiceError::InvalidRequest("the selected EPUB no longer exists".to_owned())
            } else {
                ServiceError::Io(error)
            }
        })?;
    if !source
        .extension()
        .and_then(std::ffi::OsStr::to_str)
        .is_some_and(|extension| extension.eq_ignore_ascii_case("epub"))
    {
        return Err(ServiceError::InvalidRequest(
            "the selected file must have an .epub extension".to_owned(),
        ));
    }
    let metadata = tokio::fs::metadata(&source).await?;
    if !metadata.is_file()
        || metadata.len() > audiobookai_epub::ImportLimits::default().max_archive_bytes
    {
        return Err(ServiceError::InvalidRequest(
            "the selected EPUB is not a regular file or exceeds the 1 GiB import limit".to_owned(),
        ));
    }
    let draft_id = Uuid::new_v4();
    let import_dir = state.config.data_dir.join("imports");
    tokio::fs::create_dir_all(&import_dir).await?;
    let destination = import_dir.join(format!("{draft_id}.epub"));
    copy_file_durably(&source, &destination).await?;
    let source_name = source
        .file_name()
        .and_then(std::ffi::OsStr::to_str)
        .map(str::to_owned);
    finish_import_draft(&state, draft_id, destination, source_name).await
}

pub(super) async fn finish_import_draft(
    state: &Arc<AppState>,
    draft_id: Uuid,
    path: std::path::PathBuf,
    source_name: Option<String>,
) -> Result<(StatusCode, Json<ImportDraft>), ServiceError> {
    let parse_path = path.clone();
    let imported = tokio::task::spawn_blocking(move || {
        audiobookai_epub::import(&parse_path, &audiobookai_epub::ImportLimits::default())
    })
    .await
    .map_err(ServiceError::Join)?;
    let imported = match imported {
        Ok(imported) => imported,
        Err(error) => {
            let _ = tokio::fs::remove_file(&path).await;
            return Err(ServiceError::InvalidRequest(error.to_string()));
        }
    };

    let chapters = imported
        .chapters
        .iter()
        .map(|chapter| ChapterView {
            id: Uuid::new_v4(),
            index: chapter.order,
            title: chapter.title.clone(),
            selected: chapter.linear,
            word_count: chapter.text.split_whitespace().count(),
            character_count: chapter.text.chars().count(),
            estimated_seconds: Some(estimated_seconds(chapter.text.chars().count() as u64)),
            status: ChapterDisplayStatus::Pending,
        })
        .collect::<Vec<_>>();
    let view = ImportDraft {
        draft_id,
        source_name: source_name.unwrap_or_else(|| "book.epub".to_owned()),
        title: imported.metadata.title.clone(),
        author: imported.metadata.authors.first().cloned(),
        language: imported.metadata.language.clone(),
        cover_url: imported
            .cover
            .as_ref()
            .map(|_| format!("/api/v1/imports/{draft_id}/cover")),
        chapters,
        warnings: if imported.inspection.has_encryption_manifest {
            vec!["The EPUB contains permitted font obfuscation metadata; readable text is not DRM-protected.".to_owned()]
        } else {
            Vec::new()
        },
    };
    state.catalog.write().await.import_drafts.insert(
        draft_id,
        ImportRecord {
            view: view.clone(),
            managed_path: path,
            imported,
        },
    );
    state
        .events
        .publish("import.ready", serde_json::json!({ "draftId": draft_id }));
    Ok((StatusCode::CREATED, Json(view)))
}

pub(super) async fn import_cover(
    State(state): State<Arc<AppState>>,
    Path(id): Path<Uuid>,
) -> Result<Response, ServiceError> {
    let catalog = state.catalog.read().await;
    let cover = catalog
        .import_drafts
        .get(&id)
        .and_then(|record| record.imported.cover.as_ref())
        .ok_or(ServiceError::NotFound)?;
    binary_response(&cover.media_type, cover.bytes.clone())
}

pub(super) async fn project_cover(
    State(state): State<Arc<AppState>>,
    Path(id): Path<Uuid>,
) -> Result<Response, ServiceError> {
    if !state.catalog.read().await.projects.contains_key(&id) {
        return Err(ServiceError::NotFound);
    }
    let directory = state.database.paths().library.join(id.to_string());
    let bytes = tokio::fs::read(directory.join("cover.bin"))
        .await
        .map_err(|error| {
            if error.kind() == std::io::ErrorKind::NotFound {
                ServiceError::NotFound
            } else {
                ServiceError::Io(error)
            }
        })?;
    let media_type = tokio::fs::read_to_string(directory.join("cover.mime"))
        .await
        .unwrap_or_else(|_| "application/octet-stream".to_owned());
    binary_response(media_type.trim(), bytes)
}

// Import validation, managed-file promotion, and the database transaction are
// intentionally kept in one ordered flow to avoid partial-import regressions.
#[allow(clippy::too_many_lines)]
pub(super) async fn commit_import(
    State(state): State<Arc<AppState>>,
    Path(id): Path<Uuid>,
    Json(input): Json<CommitImport>,
) -> Result<(StatusCode, Json<ProjectDetail>), ServiceError> {
    let selected = input.chapter_ids.into_iter().collect::<HashSet<_>>();
    if selected.is_empty() {
        return Err(ServiceError::InvalidRequest(
            "select at least one chapter".to_owned(),
        ));
    }
    let mut catalog = state.catalog.write().await;
    let record = catalog
        .import_drafts
        .remove(&id)
        .ok_or(ServiceError::NotFound)?;
    let project_settings = imported_project_settings(&catalog.settings, record.view.title.clone());
    if selected.iter().any(|chapter_id| {
        !record
            .view
            .chapters
            .iter()
            .any(|chapter| chapter.id == *chapter_id)
    }) {
        return Err(ServiceError::InvalidRequest(
            "chapter selection contains an unknown chapter".to_owned(),
        ));
    }
    let project_id = Uuid::new_v4();
    let library_dir = state.database.paths().library.join(project_id.to_string());
    tokio::fs::create_dir_all(&library_dir).await?;
    let managed_epub = library_dir.join("source.epub");
    if tokio::fs::rename(&record.managed_path, &managed_epub)
        .await
        .is_err()
    {
        tokio::fs::copy(&record.managed_path, &managed_epub).await?;
        tokio::fs::remove_file(&record.managed_path).await?;
    }
    if let Some(cover) = &record.imported.cover {
        tokio::fs::write(library_dir.join("cover.bin"), &cover.bytes).await?;
        tokio::fs::write(library_dir.join("cover.mime"), cover.media_type.as_bytes()).await?;
    }

    let mut chapters = record.view.chapters;
    for chapter in &mut chapters {
        chapter.selected = selected.contains(&chapter.id);
    }
    let now = Utc::now();
    let book_id = audiobookai_core::BookId::new();
    let domain_project_id = audiobookai_core::ProjectId::from_uuid(project_id);
    let source_fingerprint = hash_file(managed_epub.clone()).await?;
    let series =
        record
            .imported
            .metadata
            .series
            .as_ref()
            .map(|name| audiobookai_core::SeriesMetadata {
                name: name.clone(),
                position: record
                    .imported
                    .metadata
                    .series_position
                    .as_deref()
                    .and_then(|value| value.parse::<f32>().ok()),
                source: audiobookai_core::SeriesMetadataSource::Epub3,
            });
    let metadata = audiobookai_core::BookMetadata {
        title: record.view.title.clone(),
        authors: record
            .view
            .author
            .iter()
            .cloned()
            .chain(record.imported.metadata.authors.iter().skip(1).cloned())
            .collect(),
        narrator: None,
        publisher: None,
        description: record.imported.metadata.description.clone(),
        language: record.view.language.clone(),
        identifier: record.imported.metadata.identifier.clone(),
        series,
        cover_artifact_id: record
            .imported
            .cover
            .as_ref()
            .map(|_| audiobookai_core::ArtifactId::new()),
        extra: std::collections::BTreeMap::new(),
    };
    let domain_book = audiobookai_core::Book {
        id: book_id,
        managed_epub_path: managed_epub.to_string_lossy().into_owned(),
        original_filename: record.view.source_name.clone(),
        source_fingerprint,
        epub_version: None,
        metadata: metadata.clone(),
        imported_at: now,
    };
    let domain_project = audiobookai_core::Project {
        id: domain_project_id,
        book_id,
        name: record.view.title.clone(),
        status: audiobookai_core::ProjectStatus::Draft,
        metadata,
        cloud_consent: audiobookai_core::CloudConsent::default(),
        settings: project_settings,
        character_reviewed_at: None,
        created_at: now,
        updated_at: now,
    };
    let mut domain_chapters = Vec::with_capacity(chapters.len());
    let mut domain_paragraphs = Vec::new();
    for chapter_view in &chapters {
        let imported_chapter = record
            .imported
            .chapters
            .iter()
            .find(|chapter| chapter.order == chapter_view.index)
            .ok_or_else(|| {
                ServiceError::Internal("chapter mapping was lost during import".to_owned())
            })?;
        let chapter_id = audiobookai_core::ChapterId::from_uuid(chapter_view.id);
        domain_chapters.push(audiobookai_core::Chapter {
            id: chapter_id,
            book_id,
            ordinal: u32::try_from(chapter_view.index).unwrap_or(u32::MAX),
            title: chapter_view.title.clone(),
            source_href: imported_chapter.source_href.clone(),
            selected: chapter_view.selected,
            text_hash: imported_chapter.content_hash.clone(),
            character_count: imported_chapter.text.chars().count() as u64,
            word_count: imported_chapter.text.split_whitespace().count() as u64,
        });
        domain_paragraphs.extend(imported_chapter.paragraphs.iter().enumerate().map(
            |(ordinal, paragraph)| audiobookai_core::Paragraph {
                id: audiobookai_core::ParagraphId::new(),
                chapter_id,
                ordinal: u32::try_from(ordinal).unwrap_or(u32::MAX),
                kind: match paragraph.kind {
                    audiobookai_epub::ParagraphKind::Heading => {
                        audiobookai_core::ParagraphKind::Heading
                    }
                    audiobookai_epub::ParagraphKind::Paragraph => {
                        audiobookai_core::ParagraphKind::Prose
                    }
                    audiobookai_epub::ParagraphKind::ListItem => {
                        audiobookai_core::ParagraphKind::ListItem
                    }
                    audiobookai_epub::ParagraphKind::Quote => {
                        audiobookai_core::ParagraphKind::Quote
                    }
                    audiobookai_epub::ParagraphKind::Preformatted => {
                        audiobookai_core::ParagraphKind::Verse
                    }
                    audiobookai_epub::ParagraphKind::ImageDescription => {
                        audiobookai_core::ParagraphKind::ImageDescription
                    }
                },
                text: paragraph.text.clone(),
                source_start: paragraph.start_offset as u64,
                source_end: paragraph.end_offset as u64,
                content_hash: paragraph.content_hash.clone(),
            },
        ));
    }
    state
        .database
        .repositories()
        .projects
        .create_import(
            &domain_book,
            &domain_project,
            &domain_chapters,
            &domain_paragraphs,
        )
        .await
        .map_err(|error| ServiceError::Storage(error.to_string()))?;
    let selected_count = chapters.iter().filter(|chapter| chapter.selected).count();
    let total_seconds = chapters
        .iter()
        .filter(|chapter| chapter.selected)
        .filter_map(|chapter| chapter.estimated_seconds)
        .sum();
    let project = ProjectDetail {
        summary: BookSummary {
            id: project_id,
            title: record.view.title,
            author: record.view.author,
            cover_url: record
                .imported
                .cover
                .as_ref()
                .map(|_| format!("/api/v1/projects/{project_id}/cover")),
            chapter_count: chapters.len(),
            selected_chapter_count: selected_count,
            duration_seconds: Some(total_seconds),
            progress: 0.0,
            status: ProjectDisplayStatus::Draft,
            updated_at: now,
            language: record.view.language,
            series: record.imported.metadata.series,
            series_position: record
                .imported
                .metadata
                .series_position
                .and_then(|value| value.parse::<f32>().ok()),
        },
        narrator: None,
        publisher: None,
        description: record.imported.metadata.description,
        consent_cloud_text: false,
        consent_cloud_audio: false,
        chapters,
        character_review_status: ReviewStatus::NotStarted,
        character_revision: 0,
        output_name: None,
    };
    catalog.projects.insert(project_id, project.clone());
    catalog
        .project_book_ids
        .insert(project_id, book_id.as_uuid());
    drop(catalog);
    state.events.publish(
        "project.created",
        serde_json::json!({ "projectId": project_id }),
    );
    Ok((StatusCode::CREATED, Json(project)))
}

pub(super) async fn hash_file(
    path: std::path::PathBuf,
) -> Result<audiobookai_core::FileFingerprint, ServiceError> {
    tokio::task::spawn_blocking(move || {
        use std::io::Read;
        let mut file = std::fs::File::open(path)?;
        let mut hasher = blake3::Hasher::new();
        let mut buffer = vec![0_u8; 64 * 1024];
        let mut size_bytes = 0_u64;
        loop {
            let read = file.read(&mut buffer)?;
            if read == 0 {
                break;
            }
            hasher.update(&buffer[..read]);
            size_bytes = size_bytes.saturating_add(read as u64);
        }
        Ok::<_, std::io::Error>(audiobookai_core::FileFingerprint {
            algorithm: "blake3".to_owned(),
            digest: hasher.finalize().to_hex().to_string(),
            size_bytes,
        })
    })
    .await
    .map_err(ServiceError::Join)?
    .map_err(ServiceError::Io)
}

pub(super) fn active_job_status(status: JobStatusView) -> bool {
    matches!(
        status,
        JobStatusView::Queued
            | JobStatusView::Running
            | JobStatusView::Pausing
            | JobStatusView::Paused
            | JobStatusView::Cancelling
    )
}

pub(super) fn blocks_project_mutation(kind: crate::models::JobKindView) -> bool {
    matches!(
        kind,
        crate::models::JobKindView::CharacterDetection
            | crate::models::JobKindView::Conversion
            | crate::models::JobKindView::SegmentRegeneration
            | crate::models::JobKindView::Export
    )
}

pub(crate) async fn blocking_project_job(
    state: &AppState,
    project_id: Uuid,
    exclude_job_id: Option<Uuid>,
) -> Option<JobView> {
    let catalog = state.catalog.read().await;
    let mut jobs = catalog
        .jobs
        .values()
        .filter(|job| {
            job.project_id == project_id
                && Some(job.id) != exclude_job_id
                && active_job_status(job.status)
                && blocks_project_mutation(job.kind)
        })
        .cloned()
        .collect::<Vec<_>>();
    jobs.sort_by_key(|job| job.updated_at);
    jobs.into_iter().next()
}

pub(super) async fn blocking_character_job(state: &AppState, project_id: Uuid) -> Option<JobView> {
    blocking_project_job(state, project_id, None).await
}

pub(crate) fn active_job_conflict(job: &JobView) -> ServiceError {
    let code = match job.kind {
        crate::models::JobKindView::CharacterDetection => "active_character_detection",
        crate::models::JobKindView::SegmentRegeneration => "active_segment_regeneration",
        crate::models::JobKindView::Export => "active_proof_export",
        crate::models::JobKindView::Conversion
        | crate::models::JobKindView::Preview
        | crate::models::JobKindView::QualityControl
        | crate::models::JobKindView::CacheCleanup => "active_conversion",
    };
    ServiceError::ConflictDetails {
        code,
        detail: "finish or cancel the active project production job before changing the project or starting conflicting work".to_owned(),
        meta: serde_json::json!({ "activeJobId": job.id }),
    }
}
