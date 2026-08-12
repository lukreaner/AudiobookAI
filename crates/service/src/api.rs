use std::{
    collections::{BTreeMap, BTreeSet, HashMap, HashSet},
    convert::Infallible,
    path::{Path as FilePath, PathBuf},
    sync::Arc,
    time::Duration,
};

use axum::{
    Json, Router,
    body::{Body, Bytes},
    extract::{DefaultBodyLimit, Multipart, Path, Query, State, WebSocketUpgrade, ws::Message},
    http::{HeaderMap, StatusCode, header},
    response::{
        IntoResponse, Response,
        sse::{Event, KeepAlive, Sse},
    },
    routing::{delete, get, post, put},
};
use chrono::{Duration as ChronoDuration, Utc};
use futures::{SinkExt, Stream, StreamExt};
use serde::Deserialize;
use tokio::io::AsyncWriteExt;
use tokio_stream::wrappers::BroadcastStream;
use uuid::Uuid;

use crate::{
    AppState, ServiceError, ServiceEvent,
    models::{
        AppSettingsView, BookSummary, BudgetView, ChapterDisplayStatus, ChapterView, CheckStatus,
        CommitImport, CreateBudgetInput, DryRunCheckView, DryRunView, EstimateView,
        ExportArtifactView, ExportOptionsInput, ImportDraft, JobStatusView, JobUnitView, JobView,
        Page, PreviewView, ProjectDetail, ProjectDisplayStatus, PronunciationRuleView,
        ProviderCapabilitiesView, ProviderKindView, ProviderModeView, ProviderProfileInput,
        ProviderProfileView, ProviderStatusView, ReviewStatus, StartJobInput, UsageSummaryView,
        VoiceAssignmentView, VoiceView,
    },
    state::ImportRecord,
};

#[derive(Debug, serde::Serialize)]
#[serde(rename_all = "camelCase")]
struct HealthResponse {
    status: &'static str,
    version: &'static str,
    database: &'static str,
}

// Keeping the declarative route table together makes route precedence and
// body-limit placement auditable; splitting it would obscure those guarantees.
#[allow(clippy::too_many_lines)]
pub fn router(state: Arc<AppState>) -> Router {
    Router::new()
        .route("/api/v1/health", get(health))
        .route("/api/v1/events", get(events))
        .route("/api/v1/projects", get(list_projects))
        .route(
            "/api/v1/projects/{id}",
            get(get_project)
                .patch(update_project)
                .delete(delete_project),
        )
        .route("/api/v1/projects/{id}/cover", get(project_cover))
        .route(
            "/api/v1/imports",
            post(create_import_draft).layer(DefaultBodyLimit::max(1024 * 1024 * 1024)),
        )
        .route(
            "/api/v1/imports/from-path",
            post(create_import_draft_from_path),
        )
        .route("/api/v1/imports/{id}/cover", get(import_cover))
        .route("/api/v1/imports/{id}/commit", post(commit_import))
        .route(
            "/api/v1/projects/{id}/characters",
            get(list_characters).post(create_character),
        )
        .route(
            "/api/v1/projects/{project_id}/characters/{character_id}",
            axum::routing::patch(update_character),
        )
        .route(
            "/api/v1/projects/{id}/character-detection",
            get(character_detection_status).post(start_character_detection),
        )
        .route(
            "/api/v1/projects/{project_id}/characters/{character_id}/actions/merge",
            post(merge_character),
        )
        .route(
            "/api/v1/projects/{project_id}/characters/{character_id}/actions/delete",
            post(delete_character),
        )
        .route(
            "/api/v1/projects/{id}/character-review",
            put(approve_character_review),
        )
        .route(
            "/api/v1/projects/{project_id}/characters/{character_id}/voice",
            put(assign_voice),
        )
        .route(
            "/api/v1/projects/{project_id}/speaker-overrides/{paragraph_id}",
            put(upsert_speaker_override).delete(delete_speaker_override),
        )
        .route("/api/v1/voices", get(list_voices))
        .route(
            "/api/v1/providers/{id}/voice-clones",
            post(create_voice_clone).layer(DefaultBodyLimit::max(64 * 1024 * 1024)),
        )
        .route(
            "/api/v1/voices/{id}",
            axum::routing::patch(update_voice_clone).delete(delete_voice_clone),
        )
        .route(
            "/api/v1/pronunciation-rules",
            get(list_pronunciation_rules).post(create_pronunciation_rule),
        )
        .route(
            "/api/v1/pronunciation-rules/preview",
            post(preview_pronunciation_rules),
        )
        .route(
            "/api/v1/pronunciation-rules/{id}",
            delete(delete_pronunciation_rule),
        )
        .route(
            "/api/v1/providers",
            get(list_providers).post(create_provider),
        )
        .route(
            "/api/v1/provider-models/discover",
            post(discover_provider_models),
        )
        .route(
            "/api/v1/providers/{id}",
            get(get_provider)
                .patch(update_provider)
                .delete(delete_provider),
        )
        .route(
            "/api/v1/providers/{id}/actions/{action}",
            post(provider_action),
        )
        .route(
            "/api/v1/providers/{id}/models",
            get(provider_model_library)
                .post(download_provider_model)
                .delete(delete_provider_model),
        )
        .route(
            "/api/v1/providers/{id}/model-downloads/{operation_id}/cancel",
            post(cancel_provider_model_download),
        )
        .route(
            "/api/v1/providers/mlx-audio/management",
            get(mlx_management_status),
        )
        .route(
            "/api/v1/providers/mlx-audio/install",
            post(install_mlx_audio),
        )
        .route(
            "/api/v1/providers/mlx-audio/uninstall",
            post(uninstall_mlx_audio),
        )
        .route(
            "/api/v1/providers/mlx-audio/operations/{id}/cancel",
            post(cancel_mlx_operation),
        )
        .route(
            "/api/v1/providers/mlx-audio/models",
            post(download_mlx_model),
        )
        .route(
            "/api/v1/providers/mlx-audio/models/{id}",
            delete(remove_mlx_model),
        )
        .route(
            "/api/v1/projects/{id}/preflight/estimate",
            post(preflight_estimate),
        )
        .route(
            "/api/v1/projects/{id}/preflight/dry-run",
            post(preflight_dry_run),
        )
        .route(
            "/api/v1/projects/{id}/preflight/preview",
            post(preflight_preview),
        )
        .route(
            "/api/v1/projects/{id}/voice-auditions",
            post(voice_auditions),
        )
        .route("/api/v1/jobs", get(list_jobs).post(start_job))
        .route("/api/v1/jobs/{id}", get(get_job))
        .route("/api/v1/jobs/{id}/actions/{action}", post(job_action))
        .route("/api/v1/jobs/{id}/events", get(job_events))
        .route("/api/v1/jobs/{id}/playback", get(job_playback))
        .route("/api/v1/artifacts/{id}", get(artifact_download))
        .route("/api/v1/exports", get(list_exports))
        .route("/api/v1/usage/summary", get(usage_summary))
        .route("/api/v1/budgets", get(list_budgets).post(create_budget))
        .route("/api/v1/budgets/{id}", delete(delete_budget))
        .route(
            "/api/v1/rate-cards",
            get(list_rate_cards).post(create_rate_card),
        )
        .route("/api/v1/rate-cards/{id}", delete(delete_rate_card))
        .route("/api/v1/diagnostics", get(list_diagnostics))
        .route("/api/v1/diagnostics/export", get(export_diagnostics))
        .route("/api/v1/settings", get(get_settings).patch(update_settings))
        .route("/api/v1/settings/first-run", post(complete_first_run))
        .route("/api/v1/settings/lan/sessions", delete(revoke_lan_sessions))
        .route(
            "/api/v1/settings/lan/tokens",
            get(list_lan_tokens).post(create_lan_token),
        )
        .route("/api/v1/settings/lan/tokens/{id}", delete(revoke_lan_token))
        .route("/api/v1/settings/lan/password", put(set_lan_password))
        .route("/api/v1/secrets/status", get(secret_status))
        .route("/api/v1/secrets/unlock", post(unlock_secret_store))
        .route("/api/v1/secrets/lock", post(lock_secret_store))
        .merge(crate::proofing::routes())
        .merge(crate::distribution::routes())
        .with_state(state)
}

async fn health(State(_state): State<Arc<AppState>>) -> Json<HealthResponse> {
    Json(HealthResponse {
        status: "ready",
        version: env!("CARGO_PKG_VERSION"),
        database: "ready",
    })
}

async fn list_diagnostics(Query(query): Query<crate::diagnostics::DiagnosticQuery>) -> Response {
    let mut response = Json(crate::diagnostics::global().query(&query)).into_response();
    response.headers_mut().insert(
        header::CACHE_CONTROL,
        http::HeaderValue::from_static("no-store"),
    );
    response
}

async fn export_diagnostics(
    Query(query): Query<crate::diagnostics::DiagnosticQuery>,
) -> Result<Response, ServiceError> {
    let payload = crate::diagnostics::global().export_jsonl(&query);
    let filename = format!(
        "audiobookai-diagnostics-{}.jsonl",
        Utc::now().format("%Y%m%d-%H%M%S")
    );
    Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, "application/x-ndjson; charset=utf-8")
        .header(
            header::CONTENT_DISPOSITION,
            format!("attachment; filename=\"{filename}\""),
        )
        .header(header::CACHE_CONTROL, "no-store")
        .header("x-content-type-options", "nosniff")
        .body(Body::from(payload))
        .map_err(|error| ServiceError::Internal(error.to_string()))
}

async fn events(
    State(state): State<Arc<AppState>>,
) -> Sse<impl Stream<Item = Result<Event, Infallible>>> {
    event_stream(state, None)
}

// Owning the Arc keeps the opaque SSE stream independent of the handler's
// extractor lifetime even though subscription creation only borrows it briefly.
#[allow(clippy::needless_pass_by_value)]
fn event_stream(
    state: Arc<AppState>,
    job_id: Option<Uuid>,
) -> Sse<impl Stream<Item = Result<Event, Infallible>>> {
    let stream = BroadcastStream::new(state.events.subscribe()).filter_map(move |result| {
        let include = result.ok().filter(|event| {
            job_id.is_none_or(|id| {
                event
                    .payload
                    .get("jobId")
                    .and_then(serde_json::Value::as_str)
                    == Some(id.to_string().as_str())
            })
        });
        async move { include.map(|event| Ok(to_sse(event))) }
    });
    Sse::new(stream).keep_alive(
        KeepAlive::new()
            .interval(Duration::from_secs(15))
            .text("keep-alive"),
    )
}

fn to_sse(event: ServiceEvent) -> Event {
    Event::default()
        .id(event.sequence.to_string())
        .event(event.event_type)
        .json_data(event.payload)
        .expect("serializable event payload")
}

async fn list_projects(State(state): State<Arc<AppState>>) -> Json<Page<BookSummary>> {
    let catalog = state.catalog.read().await;
    let mut projects = catalog
        .projects
        .values()
        .map(|project| project.summary.clone())
        .collect::<Vec<_>>();
    projects.sort_by_key(|project| std::cmp::Reverse(project.updated_at));
    Json(Page::all(projects))
}

async fn get_project(
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
async fn update_project(
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

async fn delete_project(
    State(state): State<Arc<AppState>>,
    Path(id): Path<Uuid>,
) -> Result<StatusCode, ServiceError> {
    let _model_lifecycle_guard = state.model_lifecycle.lock().await;
    let dispatch_consent_lock = state.dispatch_consent_lifecycle_lock(id).await;
    let _dispatch_consent_guard = dispatch_consent_lock.write().await;
    let project_lock = state.character_lifecycle_lock(id).await;
    let _project_guard = project_lock.lock().await;
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

async fn persist_project_view(state: &AppState, view: &ProjectDetail) -> Result<(), ServiceError> {
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

async fn archive_project(state: &AppState, id: Uuid) -> Result<(), ServiceError> {
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

async fn create_import_draft(
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
struct ImportFromPathInput {
    source_path: std::path::PathBuf,
}

async fn copy_file_durably(source: &FilePath, destination: &FilePath) -> Result<(), ServiceError> {
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

async fn create_import_draft_from_path(
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

async fn finish_import_draft(
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

async fn import_cover(
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

async fn project_cover(
    State(state): State<Arc<AppState>>,
    Path(id): Path<Uuid>,
) -> Result<Response, ServiceError> {
    if !state.catalog.read().await.projects.contains_key(&id) {
        return Err(ServiceError::NotFound);
    }
    let directory = state.config.data_dir.join("library").join(id.to_string());
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
async fn commit_import(
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
    let library_dir = state
        .config
        .data_dir
        .join("library")
        .join(project_id.to_string());
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

async fn hash_file(
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

fn active_job_status(status: JobStatusView) -> bool {
    matches!(
        status,
        JobStatusView::Queued
            | JobStatusView::Running
            | JobStatusView::Pausing
            | JobStatusView::Paused
            | JobStatusView::Cancelling
    )
}

fn blocks_project_mutation(kind: crate::models::JobKindView) -> bool {
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

async fn blocking_character_job(state: &AppState, project_id: Uuid) -> Option<JobView> {
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

async fn advance_character_revision_tx(
    transaction: &mut sqlx::Transaction<'_, sqlx::Sqlite>,
    project_id: Uuid,
    expected_character_revision: u64,
    approved: bool,
) -> Result<u64, ServiceError> {
    use audiobookai_core::{Project, ProjectStatus};

    let row = sqlx::query_as::<_, (i64, i64, String)>(
        "SELECT revision, character_revision, payload FROM projects WHERE id = ?",
    )
    .bind(project_id.to_string())
    .fetch_optional(&mut **transaction)
    .await
    .map_err(|error| ServiceError::Storage(error.to_string()))?
    .ok_or(ServiceError::NotFound)?;
    let stored_character_revision = u64::try_from(row.1)
        .map_err(|_| ServiceError::Internal("stored character revision is invalid".to_owned()))?;
    if stored_character_revision != expected_character_revision {
        return Err(ServiceError::ConflictDetails {
            code: "stale_character_revision",
            detail: "character review changed; refresh before saving".to_owned(),
            meta: serde_json::json!({
                "currentCharacterRevision": stored_character_revision,
            }),
        });
    }
    let mut project: Project =
        serde_json::from_str(&row.2).map_err(|error| ServiceError::Internal(error.to_string()))?;
    project.status = if approved {
        ProjectStatus::Ready
    } else {
        ProjectStatus::NeedsCharacterReview
    };
    project.character_reviewed_at = approved.then(Utc::now);
    project.updated_at = Utc::now();
    let next_character_revision = stored_character_revision.saturating_add(1);
    let next_revision = u64::try_from(row.0)
        .map_err(|_| ServiceError::Internal("stored project revision is invalid".to_owned()))?
        .saturating_add(1);
    let result = sqlx::query(
        "UPDATE projects SET status = ?, updated_at = ?, revision = ?, character_revision = ?, \
         payload = ? WHERE id = ? AND revision = ? AND character_revision = ?",
    )
    .bind(if approved {
        "ready"
    } else {
        "needs_character_review"
    })
    .bind(project.updated_at.to_rfc3339())
    .bind(i64::try_from(next_revision).unwrap_or(i64::MAX))
    .bind(i64::try_from(next_character_revision).unwrap_or(i64::MAX))
    .bind(
        serde_json::to_string(&project)
            .map_err(|error| ServiceError::Internal(error.to_string()))?,
    )
    .bind(project_id.to_string())
    .bind(row.0)
    .bind(row.1)
    .execute(&mut **transaction)
    .await
    .map_err(|error| ServiceError::Storage(error.to_string()))?;
    if result.rows_affected() != 1 {
        return Err(ServiceError::Conflict(
            "project review changed concurrently".to_owned(),
        ));
    }
    Ok(next_character_revision)
}

async fn sync_character_review_catalog(
    state: &AppState,
    project_id: Uuid,
    approved: bool,
    character_revision: u64,
) -> Result<(), ServiceError> {
    let mut catalog = state.catalog.write().await;
    let project = catalog
        .projects
        .get_mut(&project_id)
        .ok_or(ServiceError::NotFound)?;
    project.character_review_status = if approved {
        ReviewStatus::Approved
    } else {
        ReviewStatus::NeedsReview
    };
    project.character_revision = character_revision;
    project.summary.status = if approved {
        ProjectDisplayStatus::Ready
    } else {
        ProjectDisplayStatus::Draft
    };
    project.summary.updated_at = Utc::now();
    Ok(())
}

async fn list_characters(
    State(state): State<Arc<AppState>>,
    Path(id): Path<Uuid>,
) -> Result<Json<crate::models::CharacterPageView>, ServiceError> {
    let catalog = state.catalog.read().await;
    let project = catalog.projects.get(&id).ok_or(ServiceError::NotFound)?;
    let items = catalog.characters.get(&id).cloned().unwrap_or_default();
    Ok(Json(crate::models::CharacterPageView {
        total: items.len(),
        items,
        character_revision: project.character_revision,
    }))
}

async fn character_detection_status(
    State(state): State<Arc<AppState>>,
    Path(project_id): Path<Uuid>,
) -> Result<Json<crate::models::CharacterDetectionStatusView>, ServiceError> {
    let catalog = state.catalog.read().await;
    if !catalog.projects.contains_key(&project_id) {
        return Err(ServiceError::NotFound);
    }
    let mut jobs = catalog
        .jobs
        .values()
        .filter(|job| {
            job.project_id == project_id
                && matches!(job.kind, crate::models::JobKindView::CharacterDetection)
        })
        .cloned()
        .collect::<Vec<_>>();
    jobs.sort_by_key(|job| std::cmp::Reverse(job.updated_at));
    let latest_job = jobs.first().cloned();
    let active_job = jobs.into_iter().find(|job| active_job_status(job.status));
    Ok(Json(crate::models::CharacterDetectionStatusView {
        active_job,
        latest_job,
    }))
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct DetectionInput {
    provider_profile_id: Uuid,
    expected_character_revision: u64,
    #[serde(default)]
    temperature: audiobookai_providers::Temperature,
    #[serde(default)]
    reasoning: audiobookai_providers::ReasoningControl,
}

#[allow(clippy::too_many_lines)]
async fn start_character_detection(
    State(state): State<Arc<AppState>>,
    Path(project_id): Path<Uuid>,
    Json(input): Json<DetectionInput>,
) -> Result<(StatusCode, Json<JobView>), ServiceError> {
    let _shutdown_admission = state.admit_shutdown_sensitive_work().await?;
    let _model_lifecycle_guard = state.model_lifecycle.lock().await;
    let project_lock = state.character_lifecycle_lock(project_id).await;
    let _project_guard = project_lock.lock().await;
    if let Some(job) = blocking_character_job(&state, project_id).await {
        return Err(active_job_conflict(&job));
    }
    let mut catalog = state.catalog.write().await;
    let provider = catalog
        .providers
        .get(&input.provider_profile_id)
        .ok_or_else(|| ServiceError::InvalidRequest("unknown provider profile".to_owned()))?;
    let provider_is_cloud = matches!(provider.mode, ProviderModeView::CloudRemote);
    let supports_detection = provider
        .capabilities
        .as_ref()
        .is_some_and(|caps| caps.character_detection);
    if !matches!(provider.status, ProviderStatusView::Online) {
        return Err(ServiceError::InvalidRequest(
            "refresh the selected provider and confirm it is online before character detection"
                .to_owned(),
        ));
    }
    if provider_is_cloud && !provider.credential_configured {
        return Err(ServiceError::InvalidRequest(
            "configure the selected cloud provider credential before character detection"
                .to_owned(),
        ));
    }
    let model = provider
        .model
        .as_deref()
        .filter(|model| !model.trim().is_empty())
        .map(str::to_owned)
        .ok_or_else(|| {
            ServiceError::InvalidRequest(
                "select a model on the character-detection provider before starting detection"
                    .to_owned(),
            )
        })?;
    let provider_endpoint = provider.endpoint.clone();
    let project = catalog
        .projects
        .get_mut(&project_id)
        .ok_or(ServiceError::NotFound)?;
    if project.character_revision != input.expected_character_revision {
        return Err(ServiceError::ConflictDetails {
            code: "stale_character_revision",
            detail: "character review changed; refresh before starting detection".to_owned(),
            meta: serde_json::json!({
                "currentCharacterRevision": project.character_revision,
            }),
        });
    }
    if provider_is_cloud && !project.consent_cloud_text {
        return Err(ServiceError::InvalidRequest(
            "grant this project permission to send book text to the selected cloud provider"
                .to_owned(),
        ));
    }
    if !supports_detection {
        return Err(ServiceError::InvalidRequest(
            "selected provider does not support character detection".to_owned(),
        ));
    }
    let project_title = project.summary.title.clone();
    drop(catalog);
    let runtime_id = audiobookai_providers::ProviderId::new(input.provider_profile_id.to_string())
        .map_err(|error| ServiceError::Internal(error.to_string()))?;
    let runtime_provider = state
        .providers
        .character(&runtime_id)
        .await
        .map_err(|error| ServiceError::Conflict(error.to_string()))?;
    input
        .temperature
        .validate(runtime_provider.capabilities().temperature)
        .map_err(|error| ServiceError::InvalidRequest(error.to_string()))?;
    input
        .reasoning
        .validate(runtime_provider.capabilities())
        .map_err(|error| ServiceError::InvalidRequest(error.to_string()))?;
    let job = new_job(
        project_id,
        project_title,
        crate::models::JobKindView::CharacterDetection,
        Vec::new(),
    );
    let job = crate::workflows::persist_detection_job(
        &state,
        &job,
        input.provider_profile_id,
        model,
        provider_endpoint,
        input.temperature,
        input.reasoning,
        input.expected_character_revision.saturating_add(1),
    )
    .await?;
    let mut transaction = state
        .database
        .pool()
        .begin()
        .await
        .map_err(|error| ServiceError::Storage(error.to_string()))?;
    let character_revision = advance_character_revision_tx(
        &mut transaction,
        project_id,
        input.expected_character_revision,
        false,
    )
    .await?;
    transaction
        .commit()
        .await
        .map_err(|error| ServiceError::Storage(error.to_string()))?;
    state.catalog.write().await.jobs.insert(job.id, job.clone());
    sync_character_review_catalog(&state, project_id, false, character_revision).await?;
    state.events.publish(
        "job.queued",
        serde_json::json!({ "jobId": job.id, "projectId": project_id }),
    );
    crate::workflows::spawn_character_detection(Arc::clone(&state), job.id);
    Ok((StatusCode::ACCEPTED, Json(job)))
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ReviewInput {
    approved: bool,
    expected_character_revision: u64,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct CharacterPatchInput {
    #[serde(alias = "name")]
    canonical_name: String,
    #[serde(default)]
    aliases: Vec<String>,
    expected_character_revision: u64,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct MergeCharacterInput {
    target_character_id: Uuid,
    expected_character_revision: u64,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct CharacterRevisionInput {
    expected_character_revision: u64,
}

fn normalized_character_aliases(canonical_name: &str, aliases: Vec<String>) -> Vec<String> {
    let mut normalized = HashSet::new();
    aliases
        .into_iter()
        .filter_map(|alias| {
            let alias = alias.trim();
            if alias.is_empty() || alias.eq_ignore_ascii_case(canonical_name) {
                return None;
            }
            normalized
                .insert(alias.to_lowercase())
                .then(|| alias.to_owned())
        })
        .collect()
}

fn ensure_character_name_available(
    characters: &[crate::models::CharacterView],
    canonical_name: &str,
    aliases: &[String],
    except_id: Option<Uuid>,
) -> Result<(), ServiceError> {
    let requested_names = std::iter::once(canonical_name)
        .chain(aliases.iter().map(String::as_str))
        .collect::<Vec<_>>();
    let conflict = characters
        .iter()
        .filter(|character| Some(character.id) != except_id)
        .find_map(|character| {
            std::iter::once(character.canonical_name.as_str())
                .chain(character.aliases.iter().map(String::as_str))
                .find(|existing| {
                    requested_names
                        .iter()
                        .any(|requested| existing.eq_ignore_ascii_case(requested))
                })
                .map(str::to_owned)
        });
    if let Some(conflicting_name) = conflict {
        return Err(ServiceError::ConflictDetails {
            code: "identity_conflict",
            detail: "another project character already uses that name or alias".to_owned(),
            meta: serde_json::json!({
                "canonicalName": canonical_name,
                "conflictingName": conflicting_name,
            }),
        });
    }
    Ok(())
}

#[allow(clippy::too_many_lines)]
async fn create_character(
    State(state): State<Arc<AppState>>,
    Path(project_id): Path<Uuid>,
    Json(input): Json<CharacterPatchInput>,
) -> Result<(StatusCode, Json<crate::models::CharacterMutationView>), ServiceError> {
    use audiobookai_core::{Character, CharacterId, CharacterRole, ProjectId, Validate};

    reject_empty("canonicalName", &input.canonical_name)?;
    let canonical_name = input.canonical_name.trim().to_owned();
    let aliases = normalized_character_aliases(&canonical_name, input.aliases);
    let project_lock = state.character_lifecycle_lock(project_id).await;
    let _project_guard = project_lock.lock().await;
    if let Some(job) = blocking_character_job(&state, project_id).await {
        return Err(active_job_conflict(&job));
    }
    {
        let catalog = state.catalog.read().await;
        let project = catalog
            .projects
            .get(&project_id)
            .ok_or(ServiceError::NotFound)?;
        if project.character_revision != input.expected_character_revision {
            return Err(ServiceError::ConflictDetails {
                code: "stale_character_revision",
                detail: "character review changed; refresh before saving".to_owned(),
                meta: serde_json::json!({
                    "currentCharacterRevision": project.character_revision,
                }),
            });
        }
        ensure_character_name_available(
            catalog
                .characters
                .get(&project_id)
                .map_or(&[], Vec::as_slice),
            &canonical_name,
            &aliases,
            None,
        )?;
    }
    let now = Utc::now();
    let character = Character {
        id: CharacterId::new(),
        project_id: ProjectId::from_uuid(project_id),
        role: CharacterRole::Character,
        canonical_name: canonical_name.clone(),
        aliases: aliases.clone(),
        description: None,
        confidence: Some(1.0),
        detection_run_id: None,
        manually_created: true,
        created_at: now,
        updated_at: now,
    };
    character
        .validate()
        .map_err(|error| ServiceError::InvalidRequest(error.to_string()))?;
    let mut transaction = state
        .database
        .pool()
        .begin()
        .await
        .map_err(|error| ServiceError::Storage(error.to_string()))?;
    sqlx::query(
        "INSERT INTO characters (id, project_id, role, canonical_name, updated_at, payload) \
         VALUES (?, ?, 'character', ?, ?, ?)",
    )
    .bind(character.id.to_string())
    .bind(project_id.to_string())
    .bind(&canonical_name)
    .bind(now.to_rfc3339())
    .bind(
        serde_json::to_string(&character)
            .map_err(|error| ServiceError::Internal(error.to_string()))?,
    )
    .execute(&mut *transaction)
    .await
    .map_err(|error| ServiceError::Storage(error.to_string()))?;
    for alias in &aliases {
        sqlx::query(
            "INSERT INTO character_aliases (character_id, alias, normalized_alias) VALUES (?, ?, ?)",
        )
        .bind(character.id.to_string())
        .bind(alias)
        .bind(alias.to_lowercase())
        .execute(&mut *transaction)
        .await
        .map_err(|error| ServiceError::Storage(error.to_string()))?;
    }
    let character_revision = advance_character_revision_tx(
        &mut transaction,
        project_id,
        input.expected_character_revision,
        false,
    )
    .await?;
    transaction
        .commit()
        .await
        .map_err(|error| ServiceError::Storage(error.to_string()))?;
    let view = crate::models::CharacterView {
        id: character.id.as_uuid(),
        role: CharacterRole::Character,
        canonical_name,
        aliases,
        confidence: 1.0,
        dialogue_count: 0,
        voice_assignment: None,
        evidence: Vec::new(),
    };
    {
        let mut catalog = state.catalog.write().await;
        let characters = catalog.characters.entry(project_id).or_default();
        characters.push(view.clone());
        characters.sort_by(|left, right| {
            left.canonical_name
                .to_lowercase()
                .cmp(&right.canonical_name.to_lowercase())
        });
    }
    sync_character_review_catalog(&state, project_id, false, character_revision).await?;
    state.events.publish(
        "character.updated",
        serde_json::json!({
            "projectId": project_id,
            "characterId": character.id.as_uuid(),
            "characterRevision": character_revision,
            "operation": "created",
        }),
    );
    Ok((
        StatusCode::CREATED,
        Json(crate::models::CharacterMutationView {
            character: Some(view),
            removed_character_id: None,
            inherited_voice: None,
            character_revision,
        }),
    ))
}

// Character identity, aliases, evidence, and review invalidation form one
// consistency boundary; extracting fragments would make ordering less clear.
#[allow(clippy::too_many_lines)]
async fn update_character(
    State(state): State<Arc<AppState>>,
    Path((project_id, character_id)): Path<(Uuid, Uuid)>,
    Json(input): Json<CharacterPatchInput>,
) -> Result<Json<crate::models::CharacterMutationView>, ServiceError> {
    use audiobookai_core::{Character, CharacterId, Validate};

    reject_empty("canonicalName", &input.canonical_name)?;
    let canonical_name = input.canonical_name.trim().to_owned();
    let mut aliases = normalized_character_aliases(&canonical_name, input.aliases);
    let mut normalized = aliases
        .iter()
        .map(|alias| alias.to_lowercase())
        .collect::<HashSet<_>>();
    let project_lock = state.character_lifecycle_lock(project_id).await;
    let _project_guard = project_lock.lock().await;
    if let Some(job) = blocking_character_job(&state, project_id).await {
        return Err(active_job_conflict(&job));
    }
    {
        let catalog = state.catalog.read().await;
        let project = catalog
            .projects
            .get(&project_id)
            .ok_or(ServiceError::NotFound)?;
        if project.character_revision != input.expected_character_revision {
            return Err(ServiceError::ConflictDetails {
                code: "stale_character_revision",
                detail: "character review changed; refresh before saving".to_owned(),
                meta: serde_json::json!({
                    "currentCharacterRevision": project.character_revision,
                }),
            });
        }
        ensure_character_name_available(
            catalog
                .characters
                .get(&project_id)
                .map_or(&[], Vec::as_slice),
            &canonical_name,
            &aliases,
            Some(character_id),
        )?;
    }

    let (role, payload) = sqlx::query_as::<_, (String, String)>(
        "SELECT role, payload FROM characters WHERE id = ? AND project_id = ?",
    )
    .bind(character_id.to_string())
    .bind(project_id.to_string())
    .fetch_optional(state.database.pool())
    .await
    .map_err(|error| ServiceError::Storage(error.to_string()))?
    .ok_or(ServiceError::NotFound)?;
    let mut character: Character = serde_json::from_str(&payload)
        .map_err(|error| ServiceError::Internal(error.to_string()))?;
    character.role = if role == "narrator" {
        audiobookai_core::CharacterRole::Narrator
    } else {
        audiobookai_core::CharacterRole::Character
    };
    if !character
        .canonical_name
        .eq_ignore_ascii_case(&canonical_name)
    {
        let previous_name = character.canonical_name.trim();
        if !previous_name.is_empty() && normalized.insert(previous_name.to_lowercase()) {
            aliases.push(previous_name.to_owned());
        }
    }
    character.canonical_name = canonical_name.clone();
    character.aliases.clone_from(&aliases);
    character.manually_created = true;
    character.updated_at = Utc::now();
    character
        .validate()
        .map_err(|error| ServiceError::InvalidRequest(error.to_string()))?;

    let mut transaction = state
        .database
        .pool()
        .begin()
        .await
        .map_err(|error| ServiceError::Storage(error.to_string()))?;
    sqlx::query(
        "UPDATE characters SET canonical_name = ?, updated_at = ?, payload = ? WHERE id = ?",
    )
    .bind(&character.canonical_name)
    .bind(character.updated_at.to_rfc3339())
    .bind(
        serde_json::to_string(&character)
            .map_err(|error| ServiceError::Internal(error.to_string()))?,
    )
    .bind(CharacterId::from_uuid(character_id).to_string())
    .execute(&mut *transaction)
    .await
    .map_err(|error| ServiceError::Storage(error.to_string()))?;
    sqlx::query("DELETE FROM character_aliases WHERE character_id = ?")
        .bind(character_id.to_string())
        .execute(&mut *transaction)
        .await
        .map_err(|error| ServiceError::Storage(error.to_string()))?;
    for alias in &aliases {
        sqlx::query(
            "INSERT INTO character_aliases (character_id, alias, normalized_alias) VALUES (?, ?, ?)",
        )
        .bind(character_id.to_string())
        .bind(alias)
        .bind(alias.to_lowercase())
        .execute(&mut *transaction)
        .await
        .map_err(|error| ServiceError::Storage(error.to_string()))?;
    }
    let character_revision = advance_character_revision_tx(
        &mut transaction,
        project_id,
        input.expected_character_revision,
        false,
    )
    .await?;
    transaction
        .commit()
        .await
        .map_err(|error| ServiceError::Storage(error.to_string()))?;

    let updated = {
        let mut catalog = state.catalog.write().await;
        let characters = catalog
            .characters
            .get_mut(&project_id)
            .ok_or(ServiceError::NotFound)?;
        let view = characters
            .iter_mut()
            .find(|candidate| candidate.id == character_id)
            .ok_or(ServiceError::NotFound)?;
        view.canonical_name = canonical_name;
        view.aliases = aliases;
        view.clone()
    };
    sync_character_review_catalog(&state, project_id, false, character_revision).await?;
    state.events.publish(
        "character.updated",
        serde_json::json!({
            "projectId": project_id,
            "characterId": character_id,
            "characterRevision": character_revision,
            "operation": "updated",
        }),
    );
    Ok(Json(crate::models::CharacterMutationView {
        character: Some(updated),
        removed_character_id: None,
        inherited_voice: None,
        character_revision,
    }))
}

#[allow(clippy::too_many_lines)]
async fn merge_character(
    State(state): State<Arc<AppState>>,
    Path((project_id, source_id)): Path<(Uuid, Uuid)>,
    Json(input): Json<MergeCharacterInput>,
) -> Result<Json<crate::models::CharacterMutationView>, ServiceError> {
    use audiobookai_core::{
        Character, CharacterId, CharacterRole, DictionaryRule, Speaker, SpeakerOverride,
        VoiceAssignment,
    };

    if source_id == input.target_character_id {
        return Err(ServiceError::InvalidRequest(
            "merge source and target must be different characters".to_owned(),
        ));
    }
    let project_lock = state.character_lifecycle_lock(project_id).await;
    let _project_guard = project_lock.lock().await;
    if let Some(job) = blocking_character_job(&state, project_id).await {
        return Err(active_job_conflict(&job));
    }
    let (source_role, source_payload, target_role, target_payload) = {
        let rows = sqlx::query_as::<_, (String, String, String)>(
            "SELECT id, role, payload FROM characters WHERE project_id = ? AND id IN (?, ?)",
        )
        .bind(project_id.to_string())
        .bind(source_id.to_string())
        .bind(input.target_character_id.to_string())
        .fetch_all(state.database.pool())
        .await
        .map_err(|error| ServiceError::Storage(error.to_string()))?;
        let source = rows
            .iter()
            .find(|row| row.0 == source_id.to_string())
            .map(|row| (row.1.clone(), row.2.clone()))
            .ok_or(ServiceError::NotFound)?;
        let target = rows
            .iter()
            .find(|row| row.0 == input.target_character_id.to_string())
            .map(|row| (row.1.clone(), row.2.clone()))
            .ok_or(ServiceError::NotFound)?;
        (source.0, source.1, target.0, target.1)
    };
    let mut source: Character = serde_json::from_str(&source_payload)
        .map_err(|error| ServiceError::Internal(error.to_string()))?;
    let mut target: Character = serde_json::from_str(&target_payload)
        .map_err(|error| ServiceError::Internal(error.to_string()))?;
    source.role = if source_role == "narrator" {
        CharacterRole::Narrator
    } else {
        CharacterRole::Character
    };
    target.role = if target_role == "narrator" {
        CharacterRole::Narrator
    } else {
        CharacterRole::Character
    };
    if source.role == CharacterRole::Narrator {
        return Err(ServiceError::ConflictDetails {
            code: "protected_narrator",
            detail: "the narrator cannot be merged into another character".to_owned(),
            meta: serde_json::json!({ "characterId": source_id }),
        });
    }
    let mut merged_aliases = target.aliases.clone();
    merged_aliases.push(source.canonical_name.clone());
    merged_aliases.extend(source.aliases.clone());
    target.aliases = normalized_character_aliases(&target.canonical_name, merged_aliases);
    target.manually_created = true;
    target.updated_at = Utc::now();

    let mut transaction = state
        .database
        .pool()
        .begin()
        .await
        .map_err(|error| ServiceError::Storage(error.to_string()))?;
    let current_revision =
        sqlx::query_scalar::<_, i64>("SELECT character_revision FROM projects WHERE id = ?")
            .bind(project_id.to_string())
            .fetch_one(&mut *transaction)
            .await
            .map_err(|error| ServiceError::Storage(error.to_string()))?;
    if u64::try_from(current_revision).ok() != Some(input.expected_character_revision) {
        return Err(ServiceError::ConflictDetails {
            code: "stale_character_revision",
            detail: "character review changed; refresh before saving".to_owned(),
            meta: serde_json::json!({ "currentCharacterRevision": current_revision }),
        });
    }

    sqlx::query(
        "UPDATE dialogue_spans SET character_id = ?, \
         payload = json_set(payload, '$.character_id', ?) WHERE character_id = ?",
    )
    .bind(input.target_character_id.to_string())
    .bind(input.target_character_id.to_string())
    .bind(source_id.to_string())
    .execute(&mut *transaction)
    .await
    .map_err(|error| ServiceError::Storage(error.to_string()))?;

    let override_rows = sqlx::query_as::<_, (String, String)>(
        "SELECT id, payload FROM speaker_overrides WHERE speaker_character_id = ?",
    )
    .bind(source_id.to_string())
    .fetch_all(&mut *transaction)
    .await
    .map_err(|error| ServiceError::Storage(error.to_string()))?;
    for (override_id, payload) in override_rows {
        let mut record: SpeakerOverride = serde_json::from_str(&payload)
            .map_err(|error| ServiceError::Internal(error.to_string()))?;
        record.speaker = if target.role == CharacterRole::Narrator {
            Speaker::Narrator
        } else {
            Speaker::Character(CharacterId::from_uuid(input.target_character_id))
        };
        record.updated_at = Utc::now();
        sqlx::query(
            "UPDATE speaker_overrides SET speaker_character_id = ?, updated_at = ?, payload = ? \
             WHERE id = ?",
        )
        .bind(
            (target.role != CharacterRole::Narrator).then(|| input.target_character_id.to_string()),
        )
        .bind(record.updated_at.to_rfc3339())
        .bind(
            serde_json::to_string(&record)
                .map_err(|error| ServiceError::Internal(error.to_string()))?,
        )
        .bind(override_id)
        .execute(&mut *transaction)
        .await
        .map_err(|error| ServiceError::Storage(error.to_string()))?;
    }

    let rule_rows = sqlx::query_as::<_, (String, String)>(
        "SELECT id, payload FROM dictionary_rules WHERE character_id = ?",
    )
    .bind(source_id.to_string())
    .fetch_all(&mut *transaction)
    .await
    .map_err(|error| ServiceError::Storage(error.to_string()))?;
    for (rule_id, payload) in rule_rows {
        let mut rule: DictionaryRule = serde_json::from_str(&payload)
            .map_err(|error| ServiceError::Internal(error.to_string()))?;
        rule.character_id = Some(CharacterId::from_uuid(input.target_character_id));
        sqlx::query("UPDATE dictionary_rules SET character_id = ?, payload = ? WHERE id = ?")
            .bind(input.target_character_id.to_string())
            .bind(
                serde_json::to_string(&rule)
                    .map_err(|error| ServiceError::Internal(error.to_string()))?,
            )
            .bind(rule_id)
            .execute(&mut *transaction)
            .await
            .map_err(|error| ServiceError::Storage(error.to_string()))?;
    }

    let target_assignment = sqlx::query_scalar::<_, String>(
        "SELECT id FROM voice_assignments WHERE project_id = ? AND character_id = ?",
    )
    .bind(project_id.to_string())
    .bind(input.target_character_id.to_string())
    .fetch_optional(&mut *transaction)
    .await
    .map_err(|error| ServiceError::Storage(error.to_string()))?;
    let source_assignment = sqlx::query_as::<_, (String, String)>(
        "SELECT id, payload FROM voice_assignments WHERE project_id = ? AND character_id = ?",
    )
    .bind(project_id.to_string())
    .bind(source_id.to_string())
    .fetch_optional(&mut *transaction)
    .await
    .map_err(|error| ServiceError::Storage(error.to_string()))?;
    let inherited_voice = target_assignment.is_none() && source_assignment.is_some();
    if let Some((assignment_id, payload)) = source_assignment {
        if target_assignment.is_some() {
            sqlx::query("DELETE FROM voice_assignments WHERE id = ?")
                .bind(assignment_id)
                .execute(&mut *transaction)
                .await
                .map_err(|error| ServiceError::Storage(error.to_string()))?;
        } else {
            let mut assignment: VoiceAssignment = serde_json::from_str(&payload)
                .map_err(|error| ServiceError::Internal(error.to_string()))?;
            assignment.speaker = if target.role == CharacterRole::Narrator {
                Speaker::Narrator
            } else {
                Speaker::Character(CharacterId::from_uuid(input.target_character_id))
            };
            assignment.updated_at = Utc::now();
            let speaker_key = if target.role == CharacterRole::Narrator {
                "narrator".to_owned()
            } else {
                format!("character:{}", input.target_character_id)
            };
            sqlx::query(
                "UPDATE voice_assignments SET character_id = ?, speaker_key = ?, updated_at = ?, \
                 payload = ? WHERE id = ?",
            )
            .bind(input.target_character_id.to_string())
            .bind(speaker_key)
            .bind(assignment.updated_at.to_rfc3339())
            .bind(
                serde_json::to_string(&assignment)
                    .map_err(|error| ServiceError::Internal(error.to_string()))?,
            )
            .bind(assignment_id)
            .execute(&mut *transaction)
            .await
            .map_err(|error| ServiceError::Storage(error.to_string()))?;
        }
    }

    sqlx::query(
        "UPDATE characters SET role = ?, canonical_name = ?, updated_at = ?, payload = ? WHERE id = ?",
    )
    .bind(if target.role == CharacterRole::Narrator {
        "narrator"
    } else {
        "character"
    })
    .bind(&target.canonical_name)
    .bind(target.updated_at.to_rfc3339())
    .bind(
        serde_json::to_string(&target)
            .map_err(|error| ServiceError::Internal(error.to_string()))?,
    )
    .bind(input.target_character_id.to_string())
    .execute(&mut *transaction)
    .await
    .map_err(|error| ServiceError::Storage(error.to_string()))?;
    sqlx::query("DELETE FROM character_aliases WHERE character_id = ?")
        .bind(input.target_character_id.to_string())
        .execute(&mut *transaction)
        .await
        .map_err(|error| ServiceError::Storage(error.to_string()))?;
    for alias in &target.aliases {
        sqlx::query(
            "INSERT INTO character_aliases (character_id, alias, normalized_alias) VALUES (?, ?, ?)",
        )
        .bind(input.target_character_id.to_string())
        .bind(alias)
        .bind(alias.to_lowercase())
        .execute(&mut *transaction)
        .await
        .map_err(|error| ServiceError::Storage(error.to_string()))?;
    }
    sqlx::query("DELETE FROM characters WHERE id = ? AND project_id = ?")
        .bind(source_id.to_string())
        .bind(project_id.to_string())
        .execute(&mut *transaction)
        .await
        .map_err(|error| ServiceError::Storage(error.to_string()))?;
    let character_revision = advance_character_revision_tx(
        &mut transaction,
        project_id,
        input.expected_character_revision,
        false,
    )
    .await?;
    transaction
        .commit()
        .await
        .map_err(|error| ServiceError::Storage(error.to_string()))?;

    let updated = {
        let mut catalog = state.catalog.write().await;
        for rule in &mut catalog.pronunciation_rules {
            if rule.character_id == Some(source_id) {
                rule.character_id = Some(input.target_character_id);
            }
        }
        let characters = catalog
            .characters
            .get_mut(&project_id)
            .ok_or(ServiceError::NotFound)?;
        let source_view = characters
            .iter()
            .find(|character| character.id == source_id)
            .cloned()
            .ok_or(ServiceError::NotFound)?;
        characters.retain(|character| character.id != source_id);
        let target_view = characters
            .iter_mut()
            .find(|character| character.id == input.target_character_id)
            .ok_or(ServiceError::NotFound)?;
        target_view.aliases = target.aliases;
        target_view.evidence.extend(source_view.evidence);
        target_view.dialogue_count = target_view.evidence.len();
        if target_view.voice_assignment.is_none() {
            target_view.voice_assignment = source_view.voice_assignment;
        }
        target_view.clone()
    };
    sync_character_review_catalog(&state, project_id, false, character_revision).await?;
    state.events.publish(
        "character.updated",
        serde_json::json!({
            "projectId": project_id,
            "characterId": input.target_character_id,
            "removedCharacterId": source_id,
            "characterRevision": character_revision,
            "operation": "merged",
        }),
    );
    Ok(Json(crate::models::CharacterMutationView {
        character: Some(updated),
        removed_character_id: Some(source_id),
        inherited_voice: Some(inherited_voice),
        character_revision,
    }))
}

#[allow(clippy::too_many_lines)]
async fn delete_character(
    State(state): State<Arc<AppState>>,
    Path((project_id, character_id)): Path<(Uuid, Uuid)>,
    Json(input): Json<CharacterRevisionInput>,
) -> Result<Json<crate::models::CharacterMutationView>, ServiceError> {
    use audiobookai_core::{Character, CharacterRole};

    let project_lock = state.character_lifecycle_lock(project_id).await;
    let _project_guard = project_lock.lock().await;
    if let Some(job) = blocking_character_job(&state, project_id).await {
        return Err(active_job_conflict(&job));
    }
    let (role, payload) = sqlx::query_as::<_, (String, String)>(
        "SELECT role, payload FROM characters WHERE id = ? AND project_id = ?",
    )
    .bind(character_id.to_string())
    .bind(project_id.to_string())
    .fetch_optional(state.database.pool())
    .await
    .map_err(|error| ServiceError::Storage(error.to_string()))?
    .ok_or(ServiceError::NotFound)?;
    let mut character: Character = serde_json::from_str(&payload)
        .map_err(|error| ServiceError::Internal(error.to_string()))?;
    character.role = if role == "narrator" {
        CharacterRole::Narrator
    } else {
        CharacterRole::Character
    };
    if character.role == CharacterRole::Narrator {
        return Err(ServiceError::ConflictDetails {
            code: "protected_narrator",
            detail: "the narrator cannot be deleted".to_owned(),
            meta: serde_json::json!({ "characterId": character_id }),
        });
    }
    let dialogue_count =
        sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM dialogue_spans WHERE character_id = ?")
            .bind(character_id.to_string())
            .fetch_one(state.database.pool())
            .await
            .map_err(|error| ServiceError::Storage(error.to_string()))?;
    let override_count = sqlx::query_scalar::<_, i64>(
        "SELECT COUNT(*) FROM speaker_overrides WHERE speaker_character_id = ?",
    )
    .bind(character_id.to_string())
    .fetch_one(state.database.pool())
    .await
    .map_err(|error| ServiceError::Storage(error.to_string()))?;
    let rule_count = sqlx::query_scalar::<_, i64>(
        "SELECT COUNT(*) FROM dictionary_rules WHERE character_id = ?",
    )
    .bind(character_id.to_string())
    .fetch_one(state.database.pool())
    .await
    .map_err(|error| ServiceError::Storage(error.to_string()))?;
    if dialogue_count + override_count + rule_count > 0 {
        return Err(ServiceError::ConflictDetails {
            code: "character_in_use",
            detail: "merge this referenced character into another identity before deleting it"
                .to_owned(),
            meta: serde_json::json!({
                "dialogueSpans": dialogue_count,
                "speakerOverrides": override_count,
                "pronunciationRules": rule_count,
                "mergeRequired": true,
            }),
        });
    }
    let mut transaction = state
        .database
        .pool()
        .begin()
        .await
        .map_err(|error| ServiceError::Storage(error.to_string()))?;
    sqlx::query("DELETE FROM voice_assignments WHERE project_id = ? AND character_id = ?")
        .bind(project_id.to_string())
        .bind(character_id.to_string())
        .execute(&mut *transaction)
        .await
        .map_err(|error| ServiceError::Storage(error.to_string()))?;
    sqlx::query("DELETE FROM characters WHERE id = ? AND project_id = ?")
        .bind(character_id.to_string())
        .bind(project_id.to_string())
        .execute(&mut *transaction)
        .await
        .map_err(|error| ServiceError::Storage(error.to_string()))?;
    let character_revision = advance_character_revision_tx(
        &mut transaction,
        project_id,
        input.expected_character_revision,
        false,
    )
    .await?;
    transaction
        .commit()
        .await
        .map_err(|error| ServiceError::Storage(error.to_string()))?;
    {
        let mut catalog = state.catalog.write().await;
        let characters = catalog
            .characters
            .get_mut(&project_id)
            .ok_or(ServiceError::NotFound)?;
        characters.retain(|character| character.id != character_id);
    }
    sync_character_review_catalog(&state, project_id, false, character_revision).await?;
    state.events.publish(
        "character.updated",
        serde_json::json!({
            "projectId": project_id,
            "removedCharacterId": character_id,
            "characterRevision": character_revision,
            "operation": "deleted",
        }),
    );
    Ok(Json(crate::models::CharacterMutationView {
        character: None,
        removed_character_id: Some(character_id),
        inherited_voice: None,
        character_revision,
    }))
}

async fn approve_character_review(
    State(state): State<Arc<AppState>>,
    Path(project_id): Path<Uuid>,
    Json(input): Json<ReviewInput>,
) -> Result<Json<serde_json::Value>, ServiceError> {
    let project_lock = state.character_lifecycle_lock(project_id).await;
    let _project_guard = project_lock.lock().await;
    if let Some(job) = blocking_character_job(&state, project_id).await {
        return Err(active_job_conflict(&job));
    }
    if input.approved {
        let catalog = state.catalog.read().await;
        let characters = catalog.characters.get(&project_id).ok_or_else(|| {
            ServiceError::Conflict("run and review character detection first".to_owned())
        })?;
        if characters.is_empty()
            || characters
                .iter()
                .any(|character| character.voice_assignment.is_none())
        {
            return Err(ServiceError::Conflict(
                "assign a voice to the narrator and every detected character before approval"
                    .to_owned(),
            ));
        }
        if characters
            .iter()
            .filter(|character| matches!(character.role, audiobookai_core::CharacterRole::Narrator))
            .count()
            != 1
        {
            return Err(ServiceError::Conflict(
                "character review must contain exactly one narrator".to_owned(),
            ));
        }
    }
    let mut transaction = state
        .database
        .pool()
        .begin()
        .await
        .map_err(|error| ServiceError::Storage(error.to_string()))?;
    let character_revision = advance_character_revision_tx(
        &mut transaction,
        project_id,
        input.expected_character_revision,
        input.approved,
    )
    .await?;
    transaction
        .commit()
        .await
        .map_err(|error| ServiceError::Storage(error.to_string()))?;
    sync_character_review_catalog(&state, project_id, input.approved, character_revision).await?;
    state.events.publish(
        "character-review.updated",
        serde_json::json!({
            "projectId": project_id,
            "approved": input.approved,
            "characterRevision": character_revision,
        }),
    );
    Ok(Json(serde_json::json!({
        "reviewStatus": if input.approved { "approved" } else { "needs_review" },
        "characterRevision": character_revision,
    })))
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct VoiceAssignmentInput {
    provider_profile_id: Uuid,
    provider_name: String,
    voice_id: Uuid,
    voice_name: String,
    model: Option<String>,
    #[serde(default)]
    performance: audiobookai_core::PerformanceSettings,
    #[serde(default)]
    timing: audiobookai_core::TimingSettings,
    expected_character_revision: u64,
}

#[allow(clippy::too_many_lines)]
async fn assign_voice(
    State(state): State<Arc<AppState>>,
    Path((project_id, character_id)): Path<(Uuid, Uuid)>,
    Json(input): Json<VoiceAssignmentInput>,
) -> Result<Json<crate::models::CharacterMutationView>, ServiceError> {
    let _model_lifecycle_guard = state.model_lifecycle.lock().await;
    let project_lock = state.character_lifecycle_lock(project_id).await;
    let _project_guard = project_lock.lock().await;
    if let Some(job) = blocking_character_job(&state, project_id).await {
        return Err(active_job_conflict(&job));
    }
    let mut assignment = VoiceAssignmentView {
        provider_profile_id: input.provider_profile_id,
        provider_name: input.provider_name,
        voice_id: input.voice_id,
        voice_name: input.voice_name,
        model: input.model,
        performance: input.performance,
        timing: input.timing,
    };
    let (voice, source_id, provider) = {
        let catalog = state.catalog.read().await;
        let provider = catalog
            .providers
            .get(&assignment.provider_profile_id)
            .cloned()
            .ok_or_else(|| ServiceError::InvalidRequest("unknown provider profile".to_owned()))?;
        let voice = catalog
            .voices
            .iter()
            .find(|voice| {
                voice.id == assignment.voice_id
                    && voice.provider_profile_id == assignment.provider_profile_id
            })
            .cloned()
            .ok_or_else(|| {
                ServiceError::InvalidRequest(
                    "the selected voice does not belong to the selected provider".to_owned(),
                )
            })?;
        let source_id = catalog
            .voice_sources
            .get(&voice.id)
            .cloned()
            .ok_or_else(|| {
                ServiceError::Conflict("refresh the provider voice catalog first".to_owned())
            })?;
        (voice, source_id, provider)
    };
    assignment.provider_name.clone_from(&provider.name);
    assignment.voice_name.clone_from(&voice.name);
    validate_voice_direction(
        &assignment.performance,
        &assignment.timing,
        assignment.model.as_deref().or(provider.model.as_deref()),
        provider.capabilities.as_ref(),
    )?;
    let mut transaction = state
        .database
        .pool()
        .begin()
        .await
        .map_err(|error| ServiceError::Storage(error.to_string()))?;
    persist_voice_assignment(
        &state,
        &mut transaction,
        project_id,
        character_id,
        &voice,
        &source_id,
        &assignment,
    )
    .await?;
    let character_revision = advance_character_revision_tx(
        &mut transaction,
        project_id,
        input.expected_character_revision,
        false,
    )
    .await?;
    transaction
        .commit()
        .await
        .map_err(|error| ServiceError::Storage(error.to_string()))?;
    let updated = {
        let mut catalog = state.catalog.write().await;
        let characters = catalog
            .characters
            .get_mut(&project_id)
            .ok_or(ServiceError::NotFound)?;
        let character = characters
            .iter_mut()
            .find(|character| character.id == character_id)
            .ok_or(ServiceError::NotFound)?;
        character.voice_assignment = Some(assignment);
        character.clone()
    };
    sync_character_review_catalog(&state, project_id, false, character_revision).await?;
    Ok(Json(crate::models::CharacterMutationView {
        character: Some(updated),
        removed_character_id: None,
        inherited_voice: None,
        character_revision,
    }))
}

pub(crate) fn validate_voice_direction(
    performance: &audiobookai_core::PerformanceSettings,
    timing: &audiobookai_core::TimingSettings,
    model: Option<&str>,
    capabilities: Option<&ProviderCapabilitiesView>,
) -> Result<(), ServiceError> {
    use audiobookai_core::Validate;

    if let Some(issue) = performance.validation_issues().into_iter().next() {
        return Err(ServiceError::InvalidRequest(issue.message));
    }
    if let Some(issue) = timing.validation_issues().into_iter().next() {
        return Err(ServiceError::InvalidRequest(issue.message));
    }
    if performance.is_empty() {
        return Ok(());
    }
    let model = model.ok_or_else(|| {
        ServiceError::InvalidRequest(
            "select an exact TTS model before setting performance controls".to_owned(),
        )
    })?;
    let descriptor = capabilities
        .and_then(|values| {
            values
                .model_performance
                .iter()
                .find(|descriptor| descriptor.model == model)
        })
        .ok_or_else(|| {
            ServiceError::InvalidRequest(
                "the selected provider model has no verified performance controls".to_owned(),
            )
        })?;
    validate_performance_value("speed", performance.speed, descriptor.performance.speed)?;
    validate_performance_value("pitch", performance.pitch, descriptor.performance.pitch)?;
    validate_performance_value(
        "stability",
        performance.stability,
        descriptor.performance.stability,
    )?;
    validate_performance_value(
        "similarity",
        performance.similarity,
        descriptor.performance.similarity,
    )?;
    validate_performance_value("style", performance.style, descriptor.performance.style)?;
    if performance.speaker_boost.is_some() && !descriptor.performance.speaker_boost {
        return Err(ServiceError::InvalidRequest(
            "speaker boost is not supported by the selected model".to_owned(),
        ));
    }
    if let Some(cue) = performance.delivery_cue
        && !descriptor.performance.delivery_cues.contains(&cue)
    {
        return Err(ServiceError::InvalidRequest(
            "the selected delivery cue is not supported by the selected model".to_owned(),
        ));
    }
    Ok(())
}

fn validate_performance_value(
    name: &str,
    value: Option<f64>,
    range: Option<audiobookai_core::PerformanceRange>,
) -> Result<(), ServiceError> {
    let Some(value) = value else {
        return Ok(());
    };
    if !range.is_some_and(|range| range.contains(value)) {
        return Err(ServiceError::InvalidRequest(format!(
            "{name} is not supported at this value by the selected model"
        )));
    }
    Ok(())
}

// Voice-profile and speaker-assignment upserts must remain visibly ordered so
// their relational identities cannot drift during future changes.
#[allow(clippy::too_many_lines)]
async fn persist_voice_assignment(
    state: &AppState,
    transaction: &mut sqlx::Transaction<'_, sqlx::Sqlite>,
    project_id: Uuid,
    character_id: Uuid,
    voice: &VoiceView,
    provider_voice_id: &str,
    assignment: &VoiceAssignmentView,
) -> Result<(), ServiceError> {
    use audiobookai_core::{
        CharacterId, ProjectId, ProviderProfileId, Speaker, VoiceAssignment, VoiceAssignmentId,
        VoiceOrigin, VoiceOwnership, VoiceProfile, VoiceProfileId,
    };

    let now = Utc::now();
    let existing_profile =
        sqlx::query_scalar::<_, String>("SELECT payload FROM voice_profiles WHERE id = ?")
            .bind(voice.id.to_string())
            .fetch_optional(&mut **transaction)
            .await
            .map_err(|error| ServiceError::Storage(error.to_string()))?
            .and_then(|payload| serde_json::from_str::<VoiceProfile>(&payload).ok());
    let profile = VoiceProfile {
        id: VoiceProfileId::from_uuid(voice.id),
        provider_profile_id: ProviderProfileId::from_uuid(voice.provider_profile_id),
        provider_voice_id: Some(provider_voice_id.to_owned()),
        name: voice.name.clone(),
        origin: match voice.kind {
            crate::models::VoiceKindView::Catalog => VoiceOrigin::ProviderCatalog,
            crate::models::VoiceKindView::LocalReference => VoiceOrigin::LocalReference,
            crate::models::VoiceKindView::RemoteClone => VoiceOrigin::ProviderClone,
            crate::models::VoiceKindView::Native => VoiceOrigin::NativeSystem,
        },
        ownership: if voice.owned {
            VoiceOwnership::AudiobookAi
        } else if matches!(voice.kind, crate::models::VoiceKindView::LocalReference) {
            VoiceOwnership::User
        } else {
            VoiceOwnership::Provider
        },
        reference_audio_artifact_ids: existing_profile.as_ref().map_or_else(Vec::new, |profile| {
            profile.reference_audio_artifact_ids.clone()
        }),
        language: voice.locale.clone(),
        model: assignment.model.clone(),
        settings: existing_profile
            .as_ref()
            .map_or_else(std::collections::BTreeMap::new, |profile| {
                profile.settings.clone()
            }),
        created_at: existing_profile
            .as_ref()
            .map_or(now, |profile| profile.created_at),
        updated_at: now,
    };
    sqlx::query(
        "INSERT INTO voice_profiles \
         (id, provider_id, name, origin, ownership, provider_voice_id, updated_at, payload) \
         VALUES (?, ?, ?, ?, ?, ?, ?, ?) \
         ON CONFLICT(id) DO UPDATE SET provider_id = excluded.provider_id, name = excluded.name, \
         origin = excluded.origin, ownership = excluded.ownership, \
         provider_voice_id = excluded.provider_voice_id, updated_at = excluded.updated_at, \
         payload = excluded.payload",
    )
    .bind(profile.id.to_string())
    .bind(profile.provider_profile_id.to_string())
    .bind(&profile.name)
    .bind(match profile.origin {
        VoiceOrigin::ProviderCatalog => "provider_catalog",
        VoiceOrigin::LocalReference => "local_reference",
        VoiceOrigin::ProviderClone => "provider_clone",
        VoiceOrigin::NativeSystem => "native_system",
    })
    .bind(match profile.ownership {
        VoiceOwnership::Provider => "provider",
        VoiceOwnership::User => "user",
        VoiceOwnership::AudiobookAi => "audiobook_ai",
    })
    .bind(&profile.provider_voice_id)
    .bind(profile.updated_at.to_rfc3339())
    .bind(
        serde_json::to_string(&profile)
            .map_err(|error| ServiceError::Internal(error.to_string()))?,
    )
    .execute(&mut **transaction)
    .await
    .map_err(|error| ServiceError::Storage(error.to_string()))?;

    let speaker = if state
        .catalog
        .read()
        .await
        .characters
        .get(&project_id)
        .and_then(|characters| {
            characters
                .iter()
                .find(|character| character.id == character_id)
        })
        .is_some_and(|character| {
            matches!(character.role, audiobookai_core::CharacterRole::Narrator)
        }) {
        Speaker::Narrator
    } else {
        Speaker::Character(CharacterId::from_uuid(character_id))
    };
    let speaker_key = match &speaker {
        Speaker::Narrator => "narrator".to_owned(),
        Speaker::Character(id) => format!("character:{id}"),
        Speaker::Named(name) => format!("named:{}", name.to_lowercase()),
    };
    let existing = sqlx::query_scalar::<_, String>(
        "SELECT payload FROM voice_assignments WHERE project_id = ? AND speaker_key = ?",
    )
    .bind(project_id.to_string())
    .bind(&speaker_key)
    .fetch_optional(&mut **transaction)
    .await
    .map_err(|error| ServiceError::Storage(error.to_string()))?
    .and_then(|payload| serde_json::from_str::<VoiceAssignment>(&payload).ok());
    let domain_assignment = VoiceAssignment {
        id: existing
            .as_ref()
            .map_or_else(VoiceAssignmentId::new, |stored| stored.id),
        project_id: ProjectId::from_uuid(project_id),
        speaker,
        voice_profile_id: profile.id,
        provider_profile_id: profile.provider_profile_id,
        model: assignment.model.clone(),
        performance: assignment.performance.clone(),
        timing: assignment.timing.clone(),
        settings: std::collections::BTreeMap::new(),
        created_at: existing.as_ref().map_or(now, |stored| stored.created_at),
        updated_at: now,
    };
    sqlx::query(
        "INSERT INTO voice_assignments \
         (id, project_id, provider_id, voice_profile_id, speaker_key, updated_at, payload, character_id) \
         VALUES (?, ?, ?, ?, ?, ?, ?, ?) \
         ON CONFLICT(project_id, speaker_key) DO UPDATE SET provider_id = excluded.provider_id, \
         voice_profile_id = excluded.voice_profile_id, updated_at = excluded.updated_at, \
         payload = excluded.payload, character_id = excluded.character_id",
    )
    .bind(domain_assignment.id.to_string())
    .bind(domain_assignment.project_id.to_string())
    .bind(domain_assignment.provider_profile_id.to_string())
    .bind(domain_assignment.voice_profile_id.to_string())
    .bind(speaker_key)
    .bind(domain_assignment.updated_at.to_rfc3339())
    .bind(
        serde_json::to_string(&domain_assignment)
            .map_err(|error| ServiceError::Internal(error.to_string()))?,
    )
    .bind(character_id.to_string())
    .execute(&mut **transaction)
    .await
    .map_err(|error| ServiceError::Storage(error.to_string()))?;
    Ok(())
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct SpeakerOverrideInput {
    character_id: Option<Uuid>,
    start_offset: usize,
    end_offset: usize,
    expected_character_revision: u64,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct DeleteSpeakerOverrideInput {
    start_offset: usize,
    end_offset: usize,
    expected_character_revision: u64,
}

// Override validation, durable storage, and catalog projection are one atomic
// application operation and are kept linear for auditability.
#[allow(clippy::too_many_lines)]
async fn upsert_speaker_override(
    State(state): State<Arc<AppState>>,
    Path((project_id, paragraph_id)): Path<(Uuid, Uuid)>,
    Json(input): Json<SpeakerOverrideInput>,
) -> Result<Json<serde_json::Value>, ServiceError> {
    use audiobookai_core::{
        CharacterId, Paragraph, ParagraphId, ProjectId, Speaker, SpeakerOverride, SpeakerOverrideId,
    };

    let project_lock = state.character_lifecycle_lock(project_id).await;
    let _project_guard = project_lock.lock().await;
    if let Some(job) = blocking_character_job(&state, project_id).await {
        return Err(active_job_conflict(&job));
    }
    let paragraph_payload = sqlx::query_scalar::<_, String>(
        "SELECT p.payload FROM paragraphs p \
         JOIN chapters c ON c.id = p.chapter_id \
         JOIN projects pr ON pr.book_id = c.book_id \
         WHERE p.id = ? AND pr.id = ?",
    )
    .bind(paragraph_id.to_string())
    .bind(project_id.to_string())
    .fetch_optional(state.database.pool())
    .await
    .map_err(|error| ServiceError::Storage(error.to_string()))?
    .ok_or(ServiceError::NotFound)?;
    let paragraph: Paragraph = serde_json::from_str(&paragraph_payload)
        .map_err(|error| ServiceError::Internal(error.to_string()))?;
    if input.start_offset >= input.end_offset
        || input.end_offset > paragraph.text.len()
        || !paragraph.text.is_char_boundary(input.start_offset)
        || !paragraph.text.is_char_boundary(input.end_offset)
    {
        return Err(ServiceError::InvalidRequest(
            "speaker override offsets must be valid UTF-8 byte boundaries inside the paragraph"
                .to_owned(),
        ));
    }
    let (speaker, speaker_name, speaker_character_id) = if let Some(character_id) =
        input.character_id
    {
        let character = state
            .catalog
            .read()
            .await
            .characters
            .get(&project_id)
            .and_then(|characters| characters.iter().find(|item| item.id == character_id))
            .cloned()
            .ok_or_else(|| ServiceError::InvalidRequest("unknown project character".to_owned()))?;
        if matches!(character.role, audiobookai_core::CharacterRole::Narrator) {
            (Speaker::Narrator, character.canonical_name, None)
        } else {
            (
                Speaker::Character(CharacterId::from_uuid(character_id)),
                character.canonical_name,
                Some(character_id),
            )
        }
    } else {
        (Speaker::Narrator, "Narrator".to_owned(), None)
    };
    let existing_id = sqlx::query_scalar::<_, String>(
        "SELECT id FROM speaker_overrides \
         WHERE project_id = ? AND paragraph_id = ? AND source_content_hash = ? \
         AND byte_start = ? AND byte_end = ? \
         ORDER BY updated_at DESC LIMIT 1",
    )
    .bind(project_id.to_string())
    .bind(paragraph_id.to_string())
    .bind(&paragraph.content_hash)
    .bind(
        i64::try_from(input.start_offset)
            .map_err(|_| ServiceError::InvalidRequest("offset is too large".to_owned()))?,
    )
    .bind(
        i64::try_from(input.end_offset)
            .map_err(|_| ServiceError::InvalidRequest("offset is too large".to_owned()))?,
    )
    .fetch_optional(state.database.pool())
    .await
    .map_err(|error| ServiceError::Storage(error.to_string()))?;
    let now = Utc::now();
    let record = SpeakerOverride {
        id: existing_id
            .as_deref()
            .and_then(|id| Uuid::parse_str(id).ok())
            .map_or_else(SpeakerOverrideId::new, SpeakerOverrideId::from_uuid),
        project_id: ProjectId::from_uuid(project_id),
        paragraph_id: ParagraphId::from_uuid(paragraph_id),
        source_content_hash: paragraph.content_hash,
        byte_start: input.start_offset as u64,
        byte_end: input.end_offset as u64,
        speaker,
        created_at: now,
        updated_at: now,
    };
    let mut transaction = state
        .database
        .pool()
        .begin()
        .await
        .map_err(|error| ServiceError::Storage(error.to_string()))?;
    sqlx::query(
        "INSERT INTO speaker_overrides \
         (id, project_id, paragraph_id, source_content_hash, byte_start, byte_end, updated_at, payload, speaker_character_id) \
         VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?) \
         ON CONFLICT(project_id, paragraph_id, source_content_hash, byte_start, byte_end) \
         DO UPDATE SET updated_at = excluded.updated_at, payload = excluded.payload, \
         speaker_character_id = excluded.speaker_character_id",
    )
    .bind(record.id.to_string())
    .bind(record.project_id.to_string())
    .bind(record.paragraph_id.to_string())
    .bind(&record.source_content_hash)
    .bind(i64::try_from(record.byte_start).unwrap_or(i64::MAX))
    .bind(i64::try_from(record.byte_end).unwrap_or(i64::MAX))
    .bind(record.updated_at.to_rfc3339())
    .bind(serde_json::to_string(&record).map_err(|error| ServiceError::Internal(error.to_string()))?)
    .bind(speaker_character_id.map(|id| id.to_string()))
    .execute(&mut *transaction)
    .await
    .map_err(|error| ServiceError::Storage(error.to_string()))?;
    let character_revision = advance_character_revision_tx(
        &mut transaction,
        project_id,
        input.expected_character_revision,
        false,
    )
    .await?;
    transaction
        .commit()
        .await
        .map_err(|error| ServiceError::Storage(error.to_string()))?;
    apply_speaker_override_to_catalog(
        &state,
        project_id,
        paragraph_id,
        input.start_offset,
        input.end_offset,
        Some(speaker_name),
    )
    .await;
    sync_character_review_catalog(&state, project_id, false, character_revision).await?;
    Ok(Json(serde_json::json!({
        "id": record.id.as_uuid(),
        "projectId": project_id,
        "paragraphId": paragraph_id,
        "startOffset": input.start_offset,
        "endOffset": input.end_offset,
        "characterId": speaker_character_id,
        "characterRevision": character_revision,
    })))
}

async fn delete_speaker_override(
    State(state): State<Arc<AppState>>,
    Path((project_id, paragraph_id)): Path<(Uuid, Uuid)>,
    Json(input): Json<DeleteSpeakerOverrideInput>,
) -> Result<Json<serde_json::Value>, ServiceError> {
    let project_lock = state.character_lifecycle_lock(project_id).await;
    let _project_guard = project_lock.lock().await;
    if let Some(job) = blocking_character_job(&state, project_id).await {
        return Err(active_job_conflict(&job));
    }
    let content_hash = sqlx::query_scalar::<_, String>(
        "SELECT p.content_hash FROM paragraphs p \
         JOIN chapters c ON c.id = p.chapter_id \
         JOIN projects pr ON pr.book_id = c.book_id \
         WHERE p.id = ? AND pr.id = ?",
    )
    .bind(paragraph_id.to_string())
    .bind(project_id.to_string())
    .fetch_optional(state.database.pool())
    .await
    .map_err(|error| ServiceError::Storage(error.to_string()))?
    .ok_or(ServiceError::NotFound)?;
    let mut transaction = state
        .database
        .pool()
        .begin()
        .await
        .map_err(|error| ServiceError::Storage(error.to_string()))?;
    let result = sqlx::query(
        "DELETE FROM speaker_overrides WHERE project_id = ? AND paragraph_id = ? \
         AND source_content_hash = ? AND byte_start = ? AND byte_end = ?",
    )
    .bind(project_id.to_string())
    .bind(paragraph_id.to_string())
    .bind(content_hash)
    .bind(
        i64::try_from(input.start_offset)
            .map_err(|_| ServiceError::InvalidRequest("offset is too large".to_owned()))?,
    )
    .bind(
        i64::try_from(input.end_offset)
            .map_err(|_| ServiceError::InvalidRequest("offset is too large".to_owned()))?,
    )
    .execute(&mut *transaction)
    .await
    .map_err(|error| ServiceError::Storage(error.to_string()))?;
    if result.rows_affected() == 0 {
        return Err(ServiceError::NotFound);
    }
    let character_revision = advance_character_revision_tx(
        &mut transaction,
        project_id,
        input.expected_character_revision,
        false,
    )
    .await?;
    transaction
        .commit()
        .await
        .map_err(|error| ServiceError::Storage(error.to_string()))?;
    apply_speaker_override_to_catalog(
        &state,
        project_id,
        paragraph_id,
        input.start_offset,
        input.end_offset,
        None,
    )
    .await;
    sync_character_review_catalog(&state, project_id, false, character_revision).await?;
    Ok(Json(serde_json::json!({
        "paragraphId": paragraph_id,
        "startOffset": input.start_offset,
        "endOffset": input.end_offset,
        "characterRevision": character_revision,
    })))
}

async fn apply_speaker_override_to_catalog(
    state: &AppState,
    project_id: Uuid,
    paragraph_id: Uuid,
    start_offset: usize,
    end_offset: usize,
    speaker: Option<String>,
) {
    let mut catalog = state.catalog.write().await;
    let Some(characters) = catalog.characters.get_mut(&project_id) else {
        return;
    };
    for evidence in characters
        .iter_mut()
        .flat_map(|character| &mut character.evidence)
    {
        if evidence.paragraph_id == paragraph_id
            && (end_offset == usize::MAX
                || (evidence.start_offset == start_offset && evidence.end_offset == end_offset))
        {
            evidence.speaker_override.clone_from(&speaker);
        }
    }
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct VoiceQuery {
    provider_profile_id: Option<Uuid>,
}

async fn list_voices(
    State(state): State<Arc<AppState>>,
    Query(query): Query<VoiceQuery>,
) -> Json<Page<VoiceView>> {
    let catalog = state.catalog.read().await;
    Json(Page::all(
        catalog
            .voices
            .iter()
            .filter(|voice| {
                query
                    .provider_profile_id
                    .is_none_or(|provider_id| voice.provider_profile_id == provider_id)
            })
            .cloned()
            .collect(),
    ))
}

// Multipart validation, consent enforcement, provider dispatch, and ownership
// persistence are intentionally colocated to preserve the security sequence.
#[allow(clippy::too_many_lines)]
async fn create_voice_clone(
    State(state): State<Arc<AppState>>,
    Path(provider_id): Path<Uuid>,
    mut multipart: Multipart,
) -> Result<(StatusCode, Json<VoiceView>), ServiceError> {
    use audiobookai_core::{
        ProviderProfileId, VoiceOrigin, VoiceOwnership, VoiceProfile, VoiceProfileId,
    };
    use audiobookai_providers::{VoiceCloneRequest, VoiceSample};

    let mut name = None;
    let mut description = None;
    let mut project_id = None;
    let mut samples = Vec::new();
    let mut sample_hashes = Vec::new();
    while let Some(field) = multipart
        .next_field()
        .await
        .map_err(|error| ServiceError::InvalidRequest(error.to_string()))?
    {
        match field.name() {
            Some("name") => {
                name = Some(
                    field
                        .text()
                        .await
                        .map_err(|error| ServiceError::InvalidRequest(error.to_string()))?,
                );
            }
            Some("description") => {
                description = Some(
                    field
                        .text()
                        .await
                        .map_err(|error| ServiceError::InvalidRequest(error.to_string()))?,
                );
            }
            Some("projectId") => {
                let value = field
                    .text()
                    .await
                    .map_err(|error| ServiceError::InvalidRequest(error.to_string()))?;
                project_id = Some(Uuid::parse_str(value.trim()).map_err(|_| {
                    ServiceError::InvalidRequest("projectId must be a UUID".to_owned())
                })?);
            }
            Some("referenceAudio") => {
                let file_name = field.file_name().unwrap_or("reference-audio").to_owned();
                let content_type = field
                    .content_type()
                    .unwrap_or("application/octet-stream")
                    .to_owned();
                let bytes = field
                    .bytes()
                    .await
                    .map_err(|error| ServiceError::InvalidRequest(error.to_string()))?;
                if bytes.is_empty() {
                    return Err(ServiceError::InvalidRequest(
                        "reference audio must not be empty".to_owned(),
                    ));
                }
                sample_hashes.push(blake3::hash(&bytes).to_hex().to_string());
                samples.push(VoiceSample {
                    file_name,
                    content_type,
                    bytes,
                });
            }
            _ => {}
        }
    }
    let name = name.ok_or_else(|| ServiceError::InvalidRequest("name is required".to_owned()))?;
    reject_empty("name", &name)?;
    if samples.is_empty() {
        return Err(ServiceError::InvalidRequest(
            "at least one referenceAudio field is required".to_owned(),
        ));
    }
    // Multipart parsing is local and bounded. Acquire lifecycle guards only for the final
    // validation-and-dispatch window so provider routing and project consent cannot change after
    // the checks below but before reference audio leaves the device.
    let _model_lifecycle_guard = state.model_lifecycle.lock().await;
    let provider = state
        .catalog
        .read()
        .await
        .providers
        .get(&provider_id)
        .cloned()
        .ok_or(ServiceError::NotFound)?;
    if !provider_capabilities_are_fresh(&provider)
        || !provider
            .capabilities
            .as_ref()
            .is_some_and(|capabilities| capabilities.voice_cloning)
        || !matches!(provider.status, ProviderStatusView::Online)
    {
        return Err(ServiceError::Conflict(
            "refresh an online provider that supports voice cloning before uploading reference audio"
                .to_owned(),
        ));
    }
    if matches!(provider.mode, ProviderModeView::CloudRemote) && !provider.credential_configured {
        return Err(ServiceError::Conflict(
            "configure the cloud provider credential before uploading reference audio".to_owned(),
        ));
    }
    let dispatch_consent_lock = if matches!(provider.mode, ProviderModeView::CloudRemote) {
        let project_id = project_id.ok_or_else(|| {
            ServiceError::InvalidRequest(
                "projectId is required before reference audio is sent to a cloud provider"
                    .to_owned(),
            )
        })?;
        Some(state.dispatch_consent_lifecycle_lock(project_id).await)
    } else {
        None
    };
    let _dispatch_consent_guard = if let Some(lock) = dispatch_consent_lock.as_ref() {
        Some(lock.read().await)
    } else {
        None
    };
    if matches!(provider.mode, ProviderModeView::CloudRemote) {
        let project_id = project_id.expect("cloud provider requires project id above");
        let consented = state
            .catalog
            .read()
            .await
            .projects
            .get(&project_id)
            .is_some_and(|project| project.consent_cloud_audio);
        if !consented {
            return Err(ServiceError::Forbidden(
                "grant this project permission to send reference audio to cloud providers"
                    .to_owned(),
            ));
        }
    }

    let runtime_id = audiobookai_providers::ProviderId::new(provider_id.to_string())
        .map_err(|error| ServiceError::Internal(error.to_string()))?;
    let clone = state
        .providers
        .voice_cloner(&runtime_id)
        .await
        .map_err(|error| ServiceError::Conflict(error.to_string()))?
        .create_clone(VoiceCloneRequest {
            name: name.trim().to_owned(),
            description,
            samples,
        })
        .await
        .map_err(|error| ServiceError::Conflict(error.to_string()))?;
    if !clone.owned_by_audiobookai {
        return Err(ServiceError::Conflict(
            "the provider did not confirm ownership of the newly created clone".to_owned(),
        ));
    }
    let voice_id = stable_voice_id(provider_id, &clone.provider_voice_id);
    let now = Utc::now();
    let profile = VoiceProfile {
        id: VoiceProfileId::from_uuid(voice_id),
        provider_profile_id: ProviderProfileId::from_uuid(provider_id),
        provider_voice_id: Some(clone.provider_voice_id.clone()),
        name: clone.name.clone(),
        origin: VoiceOrigin::ProviderClone,
        ownership: VoiceOwnership::AudiobookAi,
        reference_audio_artifact_ids: Vec::new(),
        language: None,
        model: provider.model,
        settings: std::collections::BTreeMap::from([
            ("projectId".to_owned(), serde_json::json!(project_id)),
            (
                "referenceAudioHashes".to_owned(),
                serde_json::json!(sample_hashes),
            ),
        ]),
        created_at: clone.created_at,
        updated_at: now,
    };
    persist_voice_profile(&state, &profile).await?;
    let view = VoiceView {
        id: voice_id,
        provider_profile_id: provider_id,
        name: clone.name,
        locale: None,
        gender: None,
        kind: crate::models::VoiceKindView::RemoteClone,
        owned: true,
        preview_url: None,
    };
    let mut catalog = state.catalog.write().await;
    catalog
        .voice_sources
        .insert(voice_id, clone.provider_voice_id);
    catalog.voices.retain(|voice| voice.id != voice_id);
    catalog.voices.push(view.clone());
    Ok((StatusCode::CREATED, Json(view)))
}

#[derive(Debug, Deserialize)]
struct VoiceCloneUpdateInput {
    name: String,
}

async fn update_voice_clone(
    State(state): State<Arc<AppState>>,
    Path(id): Path<Uuid>,
    Json(input): Json<VoiceCloneUpdateInput>,
) -> Result<Json<VoiceView>, ServiceError> {
    use audiobookai_core::{VoiceOrigin, VoiceOwnership, VoiceProfile};
    use audiobookai_providers::VoiceClone;

    reject_empty("name", &input.name)?;
    let payload =
        sqlx::query_scalar::<_, String>("SELECT payload FROM voice_profiles WHERE id = ?")
            .bind(id.to_string())
            .fetch_optional(state.database.pool())
            .await
            .map_err(|error| ServiceError::Storage(error.to_string()))?
            .ok_or(ServiceError::NotFound)?;
    let mut profile: VoiceProfile = serde_json::from_str(&payload)
        .map_err(|error| ServiceError::Internal(error.to_string()))?;
    if profile.origin != VoiceOrigin::ProviderClone
        || profile.ownership != VoiceOwnership::AudiobookAi
    {
        return Err(ServiceError::Forbidden(
            "only app-owned remote clones can be edited".to_owned(),
        ));
    }
    let provider_voice_id = profile.provider_voice_id.clone().ok_or_else(|| {
        ServiceError::Internal("clone is missing its provider voice id".to_owned())
    })?;
    let runtime_id =
        audiobookai_providers::ProviderId::new(profile.provider_profile_id.to_string())
            .map_err(|error| ServiceError::Internal(error.to_string()))?;
    let updated = state
        .providers
        .voice_cloner(&runtime_id)
        .await
        .map_err(|error| ServiceError::Conflict(error.to_string()))?
        .update_clone(
            &VoiceClone {
                provider_voice_id,
                name: profile.name.clone(),
                owned_by_audiobookai: true,
                created_at: profile.created_at,
            },
            input.name.trim().to_owned(),
        )
        .await
        .map_err(|error| ServiceError::Conflict(error.to_string()))?;
    profile.name.clone_from(&updated.name);
    profile.updated_at = Utc::now();
    persist_voice_profile(&state, &profile).await?;
    let mut catalog = state.catalog.write().await;
    let voice = catalog
        .voices
        .iter_mut()
        .find(|voice| voice.id == id)
        .ok_or(ServiceError::NotFound)?;
    voice.name = updated.name;
    Ok(Json(voice.clone()))
}

#[derive(Debug, Deserialize)]
struct DeleteCloneQuery {
    #[serde(default)]
    confirmed: bool,
}

async fn delete_voice_clone(
    State(state): State<Arc<AppState>>,
    Path(id): Path<Uuid>,
    Query(query): Query<DeleteCloneQuery>,
) -> Result<StatusCode, ServiceError> {
    use audiobookai_core::{VoiceOrigin, VoiceOwnership, VoiceProfile};
    use audiobookai_providers::VoiceClone;

    if !query.confirmed {
        return Err(ServiceError::InvalidRequest(
            "deleting an app-owned remote clone requires confirmed=true".to_owned(),
        ));
    }
    let payload =
        sqlx::query_scalar::<_, String>("SELECT payload FROM voice_profiles WHERE id = ?")
            .bind(id.to_string())
            .fetch_optional(state.database.pool())
            .await
            .map_err(|error| ServiceError::Storage(error.to_string()))?
            .ok_or(ServiceError::NotFound)?;
    let profile: VoiceProfile = serde_json::from_str(&payload)
        .map_err(|error| ServiceError::Internal(error.to_string()))?;
    if profile.origin != VoiceOrigin::ProviderClone
        || profile.ownership != VoiceOwnership::AudiobookAi
    {
        return Err(ServiceError::Forbidden(
            "catalog, native, user-owned, and unowned voices are never remotely deleted".to_owned(),
        ));
    }
    let in_use = sqlx::query_scalar::<_, i64>(
        "SELECT COUNT(*) FROM voice_assignments WHERE voice_profile_id = ?",
    )
    .bind(id.to_string())
    .fetch_one(state.database.pool())
    .await
    .map_err(|error| ServiceError::Storage(error.to_string()))?;
    if in_use > 0 {
        return Err(ServiceError::Conflict(
            "remove this clone from every character assignment before deleting it".to_owned(),
        ));
    }
    let provider_voice_id = profile.provider_voice_id.clone().ok_or_else(|| {
        ServiceError::Internal("clone is missing its provider voice id".to_owned())
    })?;
    let runtime_id =
        audiobookai_providers::ProviderId::new(profile.provider_profile_id.to_string())
            .map_err(|error| ServiceError::Internal(error.to_string()))?;
    state
        .providers
        .voice_cloner(&runtime_id)
        .await
        .map_err(|error| ServiceError::Conflict(error.to_string()))?
        .delete_owned_clone(
            &VoiceClone {
                provider_voice_id,
                name: profile.name,
                owned_by_audiobookai: true,
                created_at: profile.created_at,
            },
            true,
        )
        .await
        .map_err(|error| ServiceError::Conflict(error.to_string()))?;
    sqlx::query("DELETE FROM voice_profiles WHERE id = ?")
        .bind(id.to_string())
        .execute(state.database.pool())
        .await
        .map_err(|error| ServiceError::Storage(error.to_string()))?;
    let mut catalog = state.catalog.write().await;
    catalog.voices.retain(|voice| voice.id != id);
    catalog.voice_sources.remove(&id);
    Ok(StatusCode::NO_CONTENT)
}

async fn persist_voice_profile(
    state: &AppState,
    profile: &audiobookai_core::VoiceProfile,
) -> Result<(), ServiceError> {
    use audiobookai_core::{VoiceOrigin, VoiceOwnership};

    sqlx::query(
        "INSERT INTO voice_profiles \
         (id, provider_id, name, origin, ownership, provider_voice_id, updated_at, payload) \
         VALUES (?, ?, ?, ?, ?, ?, ?, ?) \
         ON CONFLICT(id) DO UPDATE SET provider_id = excluded.provider_id, name = excluded.name, \
         origin = excluded.origin, ownership = excluded.ownership, \
         provider_voice_id = excluded.provider_voice_id, updated_at = excluded.updated_at, \
         payload = excluded.payload",
    )
    .bind(profile.id.to_string())
    .bind(profile.provider_profile_id.to_string())
    .bind(&profile.name)
    .bind(match profile.origin {
        VoiceOrigin::ProviderCatalog => "provider_catalog",
        VoiceOrigin::LocalReference => "local_reference",
        VoiceOrigin::ProviderClone => "provider_clone",
        VoiceOrigin::NativeSystem => "native_system",
    })
    .bind(match profile.ownership {
        VoiceOwnership::Provider => "provider",
        VoiceOwnership::User => "user",
        VoiceOwnership::AudiobookAi => "audiobook_ai",
    })
    .bind(&profile.provider_voice_id)
    .bind(profile.updated_at.to_rfc3339())
    .bind(
        serde_json::to_string(profile)
            .map_err(|error| ServiceError::Internal(error.to_string()))?,
    )
    .execute(state.database.pool())
    .await
    .map_err(|error| ServiceError::Storage(error.to_string()))?;
    Ok(())
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ProjectQuery {
    project_id: Option<Uuid>,
}

async fn list_pronunciation_rules(
    State(state): State<Arc<AppState>>,
    Query(query): Query<ProjectQuery>,
) -> Json<Page<PronunciationRuleView>> {
    let catalog = state.catalog.read().await;
    Json(Page::all(
        catalog
            .pronunciation_rules
            .iter()
            .filter(|rule| {
                query
                    .project_id
                    .is_none_or(|id| rule.project_id.is_none() || rule.project_id == Some(id))
            })
            .cloned()
            .collect(),
    ))
}

// Rule normalization, conflict validation, and durable ordering are a single
// consistency flow; splitting it risks changing precedence behavior.
#[allow(clippy::too_many_lines)]
async fn create_pronunciation_rule(
    State(state): State<Arc<AppState>>,
    Json(mut rule): Json<PronunciationRuleView>,
) -> Result<(StatusCode, Json<PronunciationRuleView>), ServiceError> {
    use audiobookai_core::{
        CharacterId, DictionaryId, DictionaryRule, DictionaryRuleId, DictionaryRuleKind,
        DictionaryScope, PhonemeAlphabet, ProjectId, PronunciationDictionary,
    };

    let _model_lifecycle_guard = state.model_lifecycle.lock().await;

    reject_empty("source", &rule.source)?;
    reject_empty("replacement", &rule.replacement)?;
    if matches!(rule.kind, crate::models::PronunciationKindView::Regex) {
        regex::RegexBuilder::new(&rule.source)
            .case_insensitive(!rule.case_sensitive)
            .build()
            .map_err(|error| {
                ServiceError::InvalidRequest(format!("invalid pronunciation regex: {error}"))
            })?;
    }
    rule.id = Uuid::new_v4();
    let catalog = state.catalog.read().await;
    if rule
        .project_id
        .is_some_and(|id| !catalog.projects.contains_key(&id))
    {
        return Err(ServiceError::InvalidRequest("unknown project".to_owned()));
    }
    if matches!(rule.scope, crate::models::PronunciationScopeView::Project)
        && rule.project_id.is_none()
    {
        return Err(ServiceError::InvalidRequest(
            "project-scoped pronunciation rules require projectId".to_owned(),
        ));
    }
    if matches!(rule.scope, crate::models::PronunciationScopeView::Global)
        && rule.project_id.is_some()
    {
        return Err(ServiceError::InvalidRequest(
            "global pronunciation rules must omit projectId".to_owned(),
        ));
    }
    if rule.character_id.is_some_and(|character_id| {
        !catalog
            .characters
            .values()
            .flatten()
            .any(|character| character.id == character_id)
    }) {
        return Err(ServiceError::InvalidRequest("unknown character".to_owned()));
    }
    rule.conflict = catalog
        .pronunciation_rules
        .iter()
        .find(|existing| {
            existing.enabled
                && existing.project_id == rule.project_id
                && existing.language == rule.language
                && existing.character_id == rule.character_id
                && existing.source.eq_ignore_ascii_case(&rule.source)
        })
        .map(|existing| format!("overlaps rule {}", existing.id));
    drop(catalog);

    let scope_name = match rule.scope {
        crate::models::PronunciationScopeView::Global => "global",
        crate::models::PronunciationScopeView::Project => "project",
    };
    let project_key = rule.project_id.map(|id| id.to_string());
    let dictionary_payload = if let Some(payload) = sqlx::query_scalar::<_, String>(
        "SELECT payload FROM dictionaries WHERE scope = ? AND project_id IS ? ORDER BY updated_at LIMIT 1",
    )
    .bind(scope_name)
    .bind(project_key.as_deref())
    .fetch_optional(state.database.pool())
    .await
    .map_err(|error| ServiceError::Storage(error.to_string()))?
    {
        serde_json::from_str::<PronunciationDictionary>(&payload)
            .map_err(|error| ServiceError::Internal(error.to_string()))?
    } else {
        let now = Utc::now();
        PronunciationDictionary {
            id: DictionaryId::new(),
            name: rule.project_id.map_or_else(
                || "Global pronunciation dictionary".to_owned(),
                |_| "Project pronunciation dictionary".to_owned(),
            ),
            scope: match rule.scope {
                crate::models::PronunciationScopeView::Global => DictionaryScope::Global,
                crate::models::PronunciationScopeView::Project => DictionaryScope::Project,
            },
            project_id: rule.project_id.map(ProjectId::from_uuid),
            enabled: true,
            revision: 0,
            created_at: now,
            updated_at: now,
        }
    };
    let mut dictionary = dictionary_payload;
    dictionary.revision = dictionary.revision.saturating_add(1);
    dictionary.updated_at = Utc::now();
    let domain_rule = DictionaryRule {
        id: DictionaryRuleId::from_uuid(rule.id),
        dictionary_id: dictionary.id,
        ordinal: rule.order,
        kind: match rule.kind {
            crate::models::PronunciationKindView::Literal => DictionaryRuleKind::Literal,
            crate::models::PronunciationKindView::WholeWord => DictionaryRuleKind::WholeWord,
            crate::models::PronunciationKindView::Regex => DictionaryRuleKind::Regex,
            crate::models::PronunciationKindView::Alias => DictionaryRuleKind::Alias,
            crate::models::PronunciationKindView::Phoneme => DictionaryRuleKind::Phoneme,
        },
        pattern: rule.source.clone(),
        replacement: rule.replacement.clone(),
        case_sensitive: rule.case_sensitive,
        language: rule.language.clone(),
        character_id: rule.character_id.map(CharacterId::from_uuid),
        phoneme_alphabet: matches!(rule.kind, crate::models::PronunciationKindView::Phoneme)
            .then_some(PhonemeAlphabet::Ipa),
        enabled: rule.enabled,
    };
    let mut transaction = state
        .database
        .pool()
        .begin()
        .await
        .map_err(|error| ServiceError::Storage(error.to_string()))?;
    sqlx::query(
        "INSERT INTO dictionaries \
         (id, project_id, scope, name, revision, enabled, updated_at, payload) \
         VALUES (?, ?, ?, ?, ?, ?, ?, ?) \
         ON CONFLICT(id) DO UPDATE SET name = excluded.name, revision = excluded.revision, \
         enabled = excluded.enabled, updated_at = excluded.updated_at, payload = excluded.payload",
    )
    .bind(dictionary.id.to_string())
    .bind(dictionary.project_id.map(|id| id.to_string()))
    .bind(scope_name)
    .bind(&dictionary.name)
    .bind(i64::try_from(dictionary.revision).unwrap_or(i64::MAX))
    .bind(dictionary.enabled)
    .bind(dictionary.updated_at.to_rfc3339())
    .bind(
        serde_json::to_string(&dictionary)
            .map_err(|error| ServiceError::Internal(error.to_string()))?,
    )
    .execute(&mut *transaction)
    .await
    .map_err(|error| ServiceError::Storage(error.to_string()))?;
    let storage_ordinal = sqlx::query_scalar::<_, i64>(
        "SELECT COALESCE(MAX(ordinal), -1) + 1 FROM dictionary_rules WHERE dictionary_id = ?",
    )
    .bind(dictionary.id.to_string())
    .fetch_one(&mut *transaction)
    .await
    .map_err(|error| ServiceError::Storage(error.to_string()))?;
    sqlx::query(
        "INSERT INTO dictionary_rules \
         (id, dictionary_id, ordinal, kind, enabled, payload, character_id) \
         VALUES (?, ?, ?, ?, ?, ?, ?)",
    )
    .bind(domain_rule.id.to_string())
    .bind(DictionaryId::from_uuid(dictionary.id.as_uuid()).to_string())
    .bind(storage_ordinal)
    .bind(match domain_rule.kind {
        DictionaryRuleKind::Literal => "literal",
        DictionaryRuleKind::WholeWord => "whole_word",
        DictionaryRuleKind::Regex => "regex",
        DictionaryRuleKind::Alias => "alias",
        DictionaryRuleKind::Phoneme => "phoneme",
    })
    .bind(domain_rule.enabled)
    .bind(
        serde_json::to_string(&domain_rule)
            .map_err(|error| ServiceError::Internal(error.to_string()))?,
    )
    .bind(domain_rule.character_id.map(|id| id.to_string()))
    .execute(&mut *transaction)
    .await
    .map_err(|error| ServiceError::Storage(error.to_string()))?;
    transaction
        .commit()
        .await
        .map_err(|error| ServiceError::Storage(error.to_string()))?;
    state
        .catalog
        .write()
        .await
        .pronunciation_rules
        .push(rule.clone());
    Ok((StatusCode::CREATED, Json(rule)))
}

async fn delete_pronunciation_rule(
    State(state): State<Arc<AppState>>,
    Path(id): Path<Uuid>,
) -> Result<StatusCode, ServiceError> {
    let _model_lifecycle_guard = state.model_lifecycle.lock().await;
    let dictionary_id =
        sqlx::query_scalar::<_, String>("SELECT dictionary_id FROM dictionary_rules WHERE id = ?")
            .bind(id.to_string())
            .fetch_optional(state.database.pool())
            .await
            .map_err(|error| ServiceError::Storage(error.to_string()))?
            .ok_or(ServiceError::NotFound)?;
    let mut transaction = state
        .database
        .pool()
        .begin()
        .await
        .map_err(|error| ServiceError::Storage(error.to_string()))?;
    let result = sqlx::query("DELETE FROM dictionary_rules WHERE id = ?")
        .bind(id.to_string())
        .execute(&mut *transaction)
        .await
        .map_err(|error| ServiceError::Storage(error.to_string()))?;
    if result.rows_affected() == 0 {
        return Err(ServiceError::NotFound);
    }
    let payload = sqlx::query_scalar::<_, String>("SELECT payload FROM dictionaries WHERE id = ?")
        .bind(&dictionary_id)
        .fetch_one(&mut *transaction)
        .await
        .map_err(|error| ServiceError::Storage(error.to_string()))?;
    let mut dictionary: audiobookai_core::PronunciationDictionary = serde_json::from_str(&payload)
        .map_err(|error| ServiceError::Internal(error.to_string()))?;
    dictionary.revision = dictionary.revision.saturating_add(1);
    dictionary.updated_at = Utc::now();
    sqlx::query("UPDATE dictionaries SET revision = ?, updated_at = ?, payload = ? WHERE id = ?")
        .bind(i64::try_from(dictionary.revision).unwrap_or(i64::MAX))
        .bind(dictionary.updated_at.to_rfc3339())
        .bind(
            serde_json::to_string(&dictionary)
                .map_err(|error| ServiceError::Internal(error.to_string()))?,
        )
        .bind(dictionary_id)
        .execute(&mut *transaction)
        .await
        .map_err(|error| ServiceError::Storage(error.to_string()))?;
    transaction
        .commit()
        .await
        .map_err(|error| ServiceError::Storage(error.to_string()))?;
    state
        .catalog
        .write()
        .await
        .pronunciation_rules
        .retain(|rule| rule.id != id);
    Ok(StatusCode::NO_CONTENT)
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct PronunciationPreviewInput {
    text: String,
    project_id: Option<Uuid>,
    character_id: Option<Uuid>,
    language: Option<String>,
}

async fn preview_pronunciation_rules(
    State(state): State<Arc<AppState>>,
    Json(input): Json<PronunciationPreviewInput>,
) -> Result<Json<serde_json::Value>, ServiceError> {
    if input.text.len() > 256 * 1024 {
        return Err(ServiceError::InvalidRequest(
            "pronunciation preview text exceeds 256 KiB".to_owned(),
        ));
    }
    let mut rules = state
        .catalog
        .read()
        .await
        .pronunciation_rules
        .iter()
        .filter(|rule| {
            rule.enabled
                && match rule.scope {
                    crate::models::PronunciationScopeView::Global => true,
                    crate::models::PronunciationScopeView::Project => {
                        rule.project_id == input.project_id
                    }
                }
                && rule
                    .character_id
                    .is_none_or(|id| Some(id) == input.character_id)
                && rule.language.as_ref().is_none_or(|language| {
                    input
                        .language
                        .as_ref()
                        .is_some_and(|requested| requested.eq_ignore_ascii_case(language))
                })
        })
        .cloned()
        .collect::<Vec<_>>();
    rules.sort_by_key(|rule| {
        (
            matches!(rule.scope, crate::models::PronunciationScopeView::Project),
            rule.order,
            rule.id,
        )
    });
    let original = input.text;
    let mut transformed = original.clone();
    let mut applied = Vec::new();
    let mut conflicts = Vec::new();
    for rule in rules {
        if let Some(conflict) = &rule.conflict {
            conflicts.push(serde_json::json!({ "ruleId": rule.id, "detail": conflict }));
        }
        let before = transformed.clone();
        transformed = apply_pronunciation_rule(&transformed, &rule)?;
        if transformed != before {
            applied.push(rule.id);
        }
    }
    Ok(Json(serde_json::json!({
        "originalText": original,
        "transformedText": transformed,
        "appliedRuleIds": applied,
        "conflicts": conflicts,
    })))
}

pub(crate) fn apply_pronunciation_rule(
    text: &str,
    rule: &PronunciationRuleView,
) -> Result<String, ServiceError> {
    use crate::models::PronunciationKindView;

    let pattern = match rule.kind {
        PronunciationKindView::Literal | PronunciationKindView::Phoneme => {
            regex::escape(&rule.source)
        }
        PronunciationKindView::WholeWord | PronunciationKindView::Alias => {
            format!(r"\b{}\b", regex::escape(&rule.source))
        }
        PronunciationKindView::Regex => rule.source.clone(),
    };
    let expression = regex::RegexBuilder::new(&pattern)
        .case_insensitive(!rule.case_sensitive)
        .unicode(true)
        .build()
        .map_err(|error| {
            ServiceError::InvalidRequest(format!("invalid pronunciation regex: {error}"))
        })?;
    if matches!(rule.kind, PronunciationKindView::Regex) {
        Ok(expression
            .replace_all(text, rule.replacement.as_str())
            .into_owned())
    } else {
        Ok(expression
            .replace_all(text, |_captures: &regex::Captures<'_>| {
                rule.replacement.as_str()
            })
            .into_owned())
    }
}

async fn list_providers(State(state): State<Arc<AppState>>) -> Json<Page<ProviderProfileView>> {
    let catalog = state.catalog.read().await;
    let mut providers = catalog.providers.values().cloned().collect::<Vec<_>>();
    providers.sort_by(|left, right| left.name.cmp(&right.name));
    Json(Page::all(providers))
}

#[derive(Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ProviderModelDiscoveryInput {
    provider_id: Option<Uuid>,
    #[serde(flatten)]
    profile: ProviderProfileInput,
}

#[derive(Debug, serde::Serialize)]
#[serde(rename_all = "camelCase")]
struct AvailableProviderModelView {
    id: String,
    name: String,
}

#[derive(Debug, serde::Serialize)]
#[serde(rename_all = "camelCase")]
struct AvailableProviderModelsView {
    items: Vec<AvailableProviderModelView>,
}

async fn provider_model_discovery_profile(
    state: &AppState,
    provider_id: Option<Uuid>,
    input: &ProviderProfileInput,
) -> Result<ProviderProfileView, ServiceError> {
    let mut profile = if let Some(provider_id) = provider_id {
        state
            .catalog
            .read()
            .await
            .providers
            .get(&provider_id)
            .cloned()
            .ok_or(ServiceError::NotFound)?
    } else {
        let kind = input
            .kind
            .clone()
            .ok_or_else(|| ServiceError::InvalidRequest("provider kind is required".to_owned()))?;
        ProviderProfileView {
            id: Uuid::new_v4(),
            name: input.name.clone().unwrap_or_else(|| format!("{kind:?}")),
            kind,
            mode: input.mode.unwrap_or(ProviderModeView::CloudRemote),
            endpoint: None,
            executable_path: None,
            working_directory: None,
            arguments: Vec::new(),
            status: ProviderStatusView::Unconfigured,
            model: None,
            credential_configured: false,
            capabilities: None,
            capability_source: None,
            capability_updated_at: None,
            last_error: None,
        }
    };

    if let Some(name) = &input.name {
        reject_empty("name", name)?;
        profile.name.clone_from(name);
    }
    if let Some(kind) = &input.kind {
        profile.kind.clone_from(kind);
    }
    if let Some(mode) = input.mode {
        profile.mode = mode;
    }
    if let Some(endpoint) = &input.endpoint {
        profile.endpoint.clone_from(endpoint);
    }
    if let Some(executable_path) = &input.executable_path {
        profile.executable_path.clone_from(executable_path);
    }
    if let Some(working_directory) = &input.working_directory {
        profile.working_directory.clone_from(working_directory);
    }
    if let Some(arguments) = &input.arguments {
        profile.arguments.clone_from(arguments);
    }
    if let Some(model) = &input.model {
        profile.model.clone_from(model);
    }
    Ok(profile)
}

async fn provider_model_discovery_credential(
    state: &AppState,
    provider_id: Option<Uuid>,
    supplied: Option<&str>,
) -> Result<Option<crate::runtime::CredentialMaterial>, ServiceError> {
    if let Some(supplied) = supplied {
        return Ok(Some(crate::runtime::CredentialMaterial::new(
            supplied.as_bytes().to_vec(),
        )));
    }
    let Some(provider_id) = provider_id else {
        return Ok(None);
    };
    let secret_id = state
        .catalog
        .read()
        .await
        .provider_secret_ids
        .get(&provider_id)
        .copied();
    let Some(secret_id) = secret_id else {
        return Ok(None);
    };
    Ok(Some(
        crate::runtime::CredentialMaterial::from_zeroizing_bytes(
            &state.secrets.expose(secret_id).await?,
        ),
    ))
}

/// Builds the selected adapter in memory and performs only its model-list request. The supplied
/// credential is zeroized after the request and is never persisted by this preview endpoint.
async fn discover_provider_models(
    State(state): State<Arc<AppState>>,
    Json(input): Json<ProviderModelDiscoveryInput>,
) -> Result<Json<AvailableProviderModelsView>, ServiceError> {
    let mut profile =
        provider_model_discovery_profile(&state, input.provider_id, &input.profile).await?;
    let credential = provider_model_discovery_credential(
        &state,
        input.provider_id,
        input
            .profile
            .credential
            .as_ref()
            .map(|value| value.as_str()),
    )
    .await?;

    validate_provider_location(
        profile.mode,
        profile.endpoint.as_deref(),
        profile.executable_path.as_deref(),
        profile.working_directory.as_deref(),
        &profile.arguments,
    )?;
    validate_provider_sensitive_fields(
        &profile.kind,
        profile.mode,
        profile.model.as_deref(),
        credential.is_some(),
    )?;
    profile.capabilities = Some(default_capabilities(&profile.kind, profile.mode));

    let runtime_profile = crate::state::runtime_profile_from_view(&profile, &state.config)?;
    let models = state
        .providers
        .preview_models(&runtime_profile, credential.as_ref())
        .await
        .map_err(|error| ServiceError::Conflict(error.to_string()))?;
    let mut unique = BTreeMap::<String, String>::new();
    for model in models {
        let id = model.id.trim();
        if id.is_empty() {
            continue;
        }
        let name = model.name.trim();
        unique.insert(
            id.to_owned(),
            if name.is_empty() {
                id.to_owned()
            } else {
                name.to_owned()
            },
        );
    }
    Ok(Json(AvailableProviderModelsView {
        items: unique
            .into_iter()
            .map(|(id, name)| AvailableProviderModelView { id, name })
            .collect(),
    }))
}

async fn get_provider(
    State(state): State<Arc<AppState>>,
    Path(id): Path<Uuid>,
) -> Result<Json<ProviderProfileView>, ServiceError> {
    state
        .catalog
        .read()
        .await
        .providers
        .get(&id)
        .cloned()
        .map(Json)
        .ok_or(ServiceError::NotFound)
}

#[derive(Clone, Copy)]
enum ProviderModelCapability {
    List,
    Download,
    Delete,
}

fn require_provider_model_capability(
    profile: &ProviderProfileView,
    capability: ProviderModelCapability,
) -> Result<(), ServiceError> {
    let supported = provider_capabilities_are_fresh(profile)
        && profile
            .capabilities
            .as_ref()
            .is_some_and(|capabilities| match capability {
                ProviderModelCapability::List => capabilities.model_list,
                ProviderModelCapability::Download => capabilities.model_download,
                ProviderModelCapability::Delete => capabilities.model_delete,
            });
    if supported {
        Ok(())
    } else {
        Err(ServiceError::Conflict(
            "the provider capability snapshot does not permit this model operation".to_owned(),
        ))
    }
}

async fn provider_for_model_operation(
    state: &AppState,
    id: Uuid,
    capability: ProviderModelCapability,
) -> Result<ProviderProfileView, ServiceError> {
    let profile = state
        .catalog
        .read()
        .await
        .providers
        .get(&id)
        .cloned()
        .ok_or(ServiceError::NotFound)?;
    require_provider_model_capability(&profile, capability)?;
    Ok(profile)
}

async fn provider_model_library(
    State(state): State<Arc<AppState>>,
    Path(id): Path<Uuid>,
) -> Result<Json<crate::provider_models::ProviderModelLibraryView>, ServiceError> {
    provider_for_model_operation(&state, id, ProviderModelCapability::List).await?;
    state.provider_models.library(id).await.map(Json)
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct DownloadProviderModelInput {
    model: String,
    quantization: Option<String>,
}

async fn download_provider_model(
    State(state): State<Arc<AppState>>,
    Path(id): Path<Uuid>,
    Json(input): Json<DownloadProviderModelInput>,
) -> Result<
    (
        StatusCode,
        Json<crate::provider_models::ProviderModelOperationView>,
    ),
    ServiceError,
> {
    let _model_lifecycle_guard = state.model_lifecycle.lock().await;
    provider_for_model_operation(&state, id, ProviderModelCapability::Download).await?;
    let operation = state
        .provider_models
        .start_download(
            id,
            audiobookai_providers::ModelDownloadRequest {
                model: input.model,
                quantization: input.quantization,
            },
        )
        .await?;
    Ok((StatusCode::ACCEPTED, Json(operation)))
}

async fn cancel_provider_model_download(
    State(state): State<Arc<AppState>>,
    Path((id, operation_id)): Path<(Uuid, Uuid)>,
) -> Result<Json<crate::provider_models::ProviderModelOperationView>, ServiceError> {
    if !state.catalog.read().await.providers.contains_key(&id) {
        return Err(ServiceError::NotFound);
    }
    state
        .provider_models
        .cancel(id, operation_id)
        .await
        .map(Json)
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct DeleteProviderModelInput {
    model: String,
    #[serde(default)]
    confirmed: bool,
}

async fn delete_provider_model(
    State(state): State<Arc<AppState>>,
    Path(id): Path<Uuid>,
    Json(input): Json<DeleteProviderModelInput>,
) -> Result<StatusCode, ServiceError> {
    if !input.confirmed {
        return Err(ServiceError::InvalidRequest(
            "an explicit confirmed=true body is required before deleting a provider model"
                .to_owned(),
        ));
    }
    let _model_lifecycle_guard = state.model_lifecycle.lock().await;
    provider_for_model_operation(&state, id, ProviderModelCapability::Delete).await?;
    let in_use = provider_model_is_in_use(&state, id, &input.model).await?;
    state
        .provider_models
        .delete_model(id, &input.model, true, in_use)
        .await?;
    Ok(StatusCode::NO_CONTENT)
}

async fn provider_model_is_in_use(
    state: &AppState,
    provider_id: Uuid,
    model: &str,
) -> Result<bool, ServiceError> {
    let provider_kind = {
        let catalog = state.catalog.read().await;
        let profile = catalog
            .providers
            .get(&provider_id)
            .ok_or(ServiceError::NotFound)?;
        if profile
            .model
            .as_deref()
            .is_some_and(|assigned| provider_models_equal(&profile.kind, assigned, model))
            || character_assignments_reference_provider_model(
                &catalog.characters,
                provider_id,
                &profile.kind,
                model,
            )
        {
            return Ok(true);
        }
        profile.kind.clone()
    };
    let assignment_payloads = sqlx::query_scalar::<_, String>(
        "SELECT payload FROM voice_assignments WHERE provider_id = ?",
    )
    .bind(provider_id.to_string())
    .fetch_all(state.database.pool())
    .await
    .map_err(|error| ServiceError::Storage(error.to_string()))?;
    for payload in assignment_payloads {
        let assignment: audiobookai_core::VoiceAssignment =
            serde_json::from_str(&payload).map_err(|_| {
                ServiceError::Conflict(
                    "stored voice-assignment metadata could not be verified; model deletion is blocked"
                        .to_owned(),
                )
            })?;
        if assignment.provider_profile_id
            != audiobookai_core::ProviderProfileId::from_uuid(provider_id)
        {
            return Err(ServiceError::Conflict(
                "stored voice-assignment ownership could not be verified; model deletion is blocked"
                    .to_owned(),
            ));
        }
        if let Some(assigned) = assignment.model.as_deref()
            && provider_models_equal_checked(&provider_kind, assigned, model)?
        {
            return Ok(true);
        }
    }
    if state
        .provider_models
        .operations(provider_id)
        .await
        .iter()
        .any(|operation| {
            provider_models_equal(&provider_kind, &operation.model, model)
                && !operation.state.is_terminal()
        })
    {
        return Ok(true);
    }
    let payloads = sqlx::query_scalar::<_, String>(
        "SELECT ju.payload FROM job_units ju \
         JOIN jobs j ON j.id = ju.job_id \
         WHERE ju.provider_id = ? AND j.state NOT IN ('cancelled', 'failed', 'completed')",
    )
    .bind(provider_id.to_string())
    .fetch_all(state.database.pool())
    .await
    .map_err(|error| ServiceError::Storage(error.to_string()))?;
    for payload in payloads {
        let value: serde_json::Value = serde_json::from_str(&payload).map_err(|_| {
            ServiceError::Conflict(
                "active job metadata could not be verified; model deletion is blocked".to_owned(),
            )
        })?;
        if payload_references_provider_model(&value, &provider_kind, model) {
            return Ok(true);
        }
    }
    Ok(false)
}

fn character_assignments_reference_provider_model(
    characters: &HashMap<Uuid, Vec<crate::models::CharacterView>>,
    provider_id: Uuid,
    provider_kind: &ProviderKindView,
    model: &str,
) -> bool {
    characters.values().flatten().any(|character| {
        character
            .voice_assignment
            .as_ref()
            .is_some_and(|assignment| {
                assignment.provider_profile_id == provider_id
                    && assignment.model.as_deref().is_some_and(|assigned| {
                        provider_models_equal(provider_kind, assigned, model)
                    })
            })
    })
}

fn payload_references_provider_model(
    value: &serde_json::Value,
    provider_kind: &ProviderKindView,
    model: &str,
) -> bool {
    match value {
        serde_json::Value::Object(fields) => fields.iter().any(|(key, value)| {
            (key.eq_ignore_ascii_case("model")
                && value.as_str().is_some_and(|candidate| {
                    provider_models_equal(provider_kind, candidate, model)
                }))
                || payload_references_provider_model(value, provider_kind, model)
        }),
        serde_json::Value::Array(values) => values
            .iter()
            .any(|value| payload_references_provider_model(value, provider_kind, model)),
        _ => false,
    }
}

fn provider_models_equal(kind: &ProviderKindView, left: &str, right: &str) -> bool {
    provider_models_equal_checked(kind, left, right).unwrap_or(false)
}

fn provider_models_equal_checked(
    kind: &ProviderKindView,
    left: &str,
    right: &str,
) -> Result<bool, ServiceError> {
    match kind {
        ProviderKindView::Ollama => {
            audiobookai_providers::ollama_model_identifiers_equal(left, right).map_err(|_| {
                ServiceError::Conflict(
                    "stored Ollama model metadata could not be verified; model deletion is blocked"
                        .to_owned(),
                )
            })
        }
        ProviderKindView::Localai => {
            audiobookai_providers::local_ai_model_identifiers_equal(left, right).map_err(|_| {
                ServiceError::Conflict(
                    "stored LocalAI model metadata could not be verified; model deletion is blocked"
                        .to_owned(),
                )
            })
        }
        _ => Ok(left == right),
    }
}

async fn mlx_management_status(
    State(state): State<Arc<AppState>>,
) -> Json<crate::mlx_management::MlxManagementView> {
    Json(state.mlx.view().await)
}

async fn install_mlx_audio(
    State(state): State<Arc<AppState>>,
) -> Result<(StatusCode, Json<crate::mlx_management::MlxOperationView>), ServiceError> {
    let operation = state.mlx.start_install().await?;
    watch_mlx_install_for_profile(Arc::clone(&state), operation.id);
    Ok((StatusCode::ACCEPTED, Json(operation)))
}

async fn uninstall_mlx_audio(
    State(state): State<Arc<AppState>>,
    Json(input): Json<ConfirmMlxAction>,
) -> Result<(StatusCode, Json<crate::mlx_management::MlxOperationView>), ServiceError> {
    require_mlx_uninstall_confirmation(input.confirmed)?;
    let profiles = state
        .catalog
        .read()
        .await
        .providers
        .values()
        .filter(|profile| {
            matches!(profile.kind, ProviderKindView::MlxAudio)
                && matches!(profile.mode, ProviderModeView::ManagedChild)
        })
        .count();
    if profiles > 0 {
        return Err(ServiceError::Conflict(
            "stop and delete the app-managed MLX-audio provider profile before uninstalling its runtime; downloaded models are retained"
                .to_owned(),
        ));
    }
    let operation = state.mlx.start_uninstall().await?;
    Ok((StatusCode::ACCEPTED, Json(operation)))
}

async fn cancel_mlx_operation(
    State(state): State<Arc<AppState>>,
    Path(id): Path<Uuid>,
) -> Result<Json<crate::mlx_management::MlxOperationView>, ServiceError> {
    state.mlx.cancel(id).await.map(Json)
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct DownloadMlxModelInput {
    repository: String,
    #[serde(default = "default_model_revision")]
    revision: String,
}

fn default_model_revision() -> String {
    "main".to_owned()
}

async fn download_mlx_model(
    State(state): State<Arc<AppState>>,
    Json(input): Json<DownloadMlxModelInput>,
) -> Result<(StatusCode, Json<crate::mlx_management::MlxOperationView>), ServiceError> {
    let _model_lifecycle_guard = state.model_lifecycle.lock().await;
    let operation = state
        .mlx
        .start_model_download(input.repository, input.revision)
        .await?;
    Ok((StatusCode::ACCEPTED, Json(operation)))
}

#[derive(Debug, Deserialize)]
struct ConfirmMlxAction {
    #[serde(default)]
    confirmed: bool,
}

async fn remove_mlx_model(
    State(state): State<Arc<AppState>>,
    Path(id): Path<Uuid>,
    Json(input): Json<ConfirmMlxAction>,
) -> Result<StatusCode, ServiceError> {
    if !input.confirmed {
        return Err(ServiceError::InvalidRequest(
            "an explicit confirmed=true body is required before removing an app-owned model"
                .to_owned(),
        ));
    }
    let _model_lifecycle_guard = state.model_lifecycle.lock().await;
    let management = state.mlx.view().await;
    let model = management
        .models
        .iter()
        .find(|model| model.id == id)
        .ok_or(ServiceError::NotFound)?;
    if mlx_model_is_in_use(&state, &model.local_path).await? {
        return Err(ServiceError::Conflict(
            "select another model in every provider and character assignment, and finish or cancel active jobs before removing this app-owned model"
                .to_owned(),
        ));
    }
    state.mlx.remove_model(id).await?;
    Ok(StatusCode::NO_CONTENT)
}

fn require_mlx_uninstall_confirmation(confirmed: bool) -> Result<(), ServiceError> {
    if confirmed {
        Ok(())
    } else {
        Err(ServiceError::InvalidRequest(
            "an explicit confirmed=true body is required before uninstalling the app-owned MLX-audio runtime"
                .to_owned(),
        ))
    }
}

async fn mlx_model_is_in_use(
    state: &AppState,
    model_path: &FilePath,
) -> Result<bool, ServiceError> {
    let canonical_model = canonical_mlx_model_path(state, model_path).await?;
    let in_memory_models = {
        let catalog = state.catalog.read().await;
        let provider_models = catalog
            .providers
            .values()
            .filter_map(|profile| profile.model.clone());
        let assignment_models = catalog
            .characters
            .values()
            .flatten()
            .filter_map(|character| {
                character
                    .voice_assignment
                    .as_ref()
                    .and_then(|assignment| assignment.model.clone())
            });
        provider_models.chain(assignment_models).collect::<Vec<_>>()
    };

    for selected in in_memory_models {
        if mlx_model_path_matches(&selected, &canonical_model).await? {
            return Ok(true);
        }
    }

    let assignment_payloads =
        sqlx::query_scalar::<_, String>("SELECT payload FROM voice_assignments")
            .fetch_all(state.database.pool())
            .await
            .map_err(|error| ServiceError::Storage(error.to_string()))?;
    if json_payloads_reference_model_path(assignment_payloads, &canonical_model).await? {
        return Ok(true);
    }

    let active_job_payloads = sqlx::query_scalar::<_, String>(
        "SELECT ju.payload FROM job_units ju \
         JOIN jobs j ON j.id = ju.job_id \
         WHERE j.state NOT IN ('cancelled', 'failed', 'completed')",
    )
    .fetch_all(state.database.pool())
    .await
    .map_err(|error| ServiceError::Storage(error.to_string()))?;
    if json_payloads_reference_model_path(active_job_payloads, &canonical_model).await? {
        return Ok(true);
    }
    Ok(false)
}

#[derive(Debug)]
struct CanonicalMlxModelPath {
    managed_root: PathBuf,
    model: PathBuf,
}

async fn canonical_mlx_model_path(
    state: &AppState,
    model_path: &FilePath,
) -> Result<CanonicalMlxModelPath, ServiceError> {
    let managed_root = tokio::fs::canonicalize(
        state
            .config
            .data_dir
            .join("managed-providers")
            .join("mlx-audio")
            .join("models"),
    )
    .await
    .map_err(|_| {
        ServiceError::Conflict(
            "the managed MLX model directory could not be verified; model removal is blocked"
                .to_owned(),
        )
    })?;
    let model = canonicalize_path_with_optional_missing_leaf(model_path).await?;
    if model == managed_root || !model.starts_with(&managed_root) {
        return Err(ServiceError::Conflict(
            "the selected MLX model path is outside the managed model directory; removal is blocked"
                .to_owned(),
        ));
    }
    Ok(CanonicalMlxModelPath {
        managed_root,
        model,
    })
}

async fn canonicalize_path_with_optional_missing_leaf(
    path: &FilePath,
) -> Result<PathBuf, ServiceError> {
    match tokio::fs::canonicalize(path).await {
        Ok(path) => Ok(path),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            let name = path.file_name().ok_or_else(|| {
                ServiceError::Conflict(
                    "a model path could not be verified; model removal is blocked".to_owned(),
                )
            })?;
            let parent = path.parent().ok_or_else(|| {
                ServiceError::Conflict(
                    "a model path could not be verified; model removal is blocked".to_owned(),
                )
            })?;
            tokio::fs::canonicalize(parent)
                .await
                .map(|parent| parent.join(name))
                .map_err(|_| {
                    ServiceError::Conflict(
                        "a model path could not be verified; model removal is blocked".to_owned(),
                    )
                })
        }
        Err(_) => Err(ServiceError::Conflict(
            "a model path could not be verified; model removal is blocked".to_owned(),
        )),
    }
}

async fn mlx_model_path_matches(
    candidate: &str,
    expected: &CanonicalMlxModelPath,
) -> Result<bool, ServiceError> {
    let candidate = FilePath::new(candidate);
    if !candidate.is_absolute() {
        return Ok(false);
    }
    let Ok(canonical) = canonicalize_path_with_optional_missing_leaf(candidate).await else {
        return Ok(false);
    };
    Ok(canonical.starts_with(&expected.managed_root) && canonical == expected.model)
}

async fn json_payloads_reference_model_path(
    payloads: Vec<String>,
    model_path: &CanonicalMlxModelPath,
) -> Result<bool, ServiceError> {
    for payload in payloads {
        let value: serde_json::Value = serde_json::from_str(&payload).map_err(|_| {
            ServiceError::Conflict(
                "stored assignment or active-job metadata could not be verified; model removal is blocked"
                    .to_owned(),
            )
        })?;
        if payload_references_model_path(&value, model_path).await? {
            return Ok(true);
        }
    }
    Ok(false)
}

async fn payload_references_model_path(
    value: &serde_json::Value,
    model_path: &CanonicalMlxModelPath,
) -> Result<bool, ServiceError> {
    let mut candidates = Vec::new();
    collect_model_path_candidates(value, &mut candidates);
    for candidate in candidates {
        if mlx_model_path_matches(candidate, model_path).await? {
            return Ok(true);
        }
    }
    Ok(false)
}

fn collect_model_path_candidates<'a>(value: &'a serde_json::Value, output: &mut Vec<&'a str>) {
    match value {
        serde_json::Value::Object(fields) => {
            for (key, value) in fields {
                if key.eq_ignore_ascii_case("model")
                    && let Some(candidate) = value.as_str()
                {
                    output.push(candidate);
                }
                collect_model_path_candidates(value, output);
            }
        }
        serde_json::Value::Array(values) => {
            for value in values {
                collect_model_path_candidates(value, output);
            }
        }
        _ => {}
    }
}

fn watch_mlx_install_for_profile(state: Arc<AppState>, operation_id: Uuid) {
    tokio::spawn(async move {
        loop {
            let view = state.mlx.view().await;
            if view
                .last_operation
                .as_ref()
                .is_some_and(|operation| operation.id == operation_id)
            {
                let succeeded = view.last_operation.as_ref().is_some_and(|operation| {
                    matches!(
                        operation.state,
                        crate::mlx_management::MlxOperationState::Succeeded
                    )
                });
                if succeeded {
                    let configured = auto_configure_mlx_profile(&state, &view).await.is_ok();
                    state.mlx.set_profile_action_required(!configured).await;
                    if !configured {
                        tracing::warn!(
                            diagnostic_code = "mlx.profile.action_required",
                            "MLX-audio installation completed but its managed profile needs review"
                        );
                    }
                }
                return;
            }
            tokio::time::sleep(Duration::from_millis(250)).await;
        }
    });
}

async fn auto_configure_mlx_profile(
    state: &Arc<AppState>,
    management: &crate::mlx_management::MlxManagementView,
) -> Result<(), ServiceError> {
    let server = management.server_executable.as_ref().ok_or_else(|| {
        ServiceError::Conflict(
            "the installed MLX-audio server executable is unavailable".to_owned(),
        )
    })?;
    let existing = state
        .catalog
        .read()
        .await
        .providers
        .values()
        .filter(|profile| matches!(profile.kind, ProviderKindView::MlxAudio))
        .count();
    if existing > 0 {
        return Err(ServiceError::Conflict(
            "an MLX-audio profile already exists and was not changed automatically".to_owned(),
        ));
    }
    let working_directory = server
        .parent()
        .and_then(FilePath::parent)
        .ok_or_else(|| ServiceError::Internal("invalid managed MLX path".to_owned()))?;
    let profile = ProviderProfileView {
        id: Uuid::new_v4(),
        name: "MLX-audio (managed)".to_owned(),
        kind: ProviderKindView::MlxAudio,
        mode: ProviderModeView::ManagedChild,
        endpoint: Some("http://127.0.0.1:8000/".to_owned()),
        executable_path: Some(server.to_string_lossy().into_owned()),
        working_directory: Some(working_directory.to_string_lossy().into_owned()),
        arguments: vec![
            "--host".to_owned(),
            "127.0.0.1".to_owned(),
            "--port".to_owned(),
            "8000".to_owned(),
        ],
        status: ProviderStatusView::Offline,
        model: None,
        credential_configured: false,
        capabilities: Some(default_capabilities(
            &ProviderKindView::MlxAudio,
            ProviderModeView::ManagedChild,
        )),
        capability_source: Some("app_managed_mlx_audio_0.4.6".to_owned()),
        capability_updated_at: Some(Utc::now()),
        last_error: None,
    };
    validate_provider_location(
        profile.mode,
        profile.endpoint.as_deref(),
        profile.executable_path.as_deref(),
        profile.working_directory.as_deref(),
        &profile.arguments,
    )?;
    persist_provider(state, &profile, None).await?;
    state
        .catalog
        .write()
        .await
        .providers
        .insert(profile.id, profile.clone());
    state.sync_provider_runtime(profile.id).await?;
    state.events.publish(
        "provider.created",
        serde_json::json!({ "providerId": profile.id, "source": "mlx_install" }),
    );
    Ok(())
}

/// Rejects a runtime replacement/removal while durable work can still dispatch through it.
///
/// Job admission and provider mutation both hold `model_lifecycle`, so the query and the
/// subsequent mutation form one closed window: either the job is admitted first and blocks the
/// mutation, or the mutation completes first and the job validates the replacement profile.
async fn ensure_provider_runtime_mutation_allowed(
    state: &AppState,
    provider_id: Uuid,
) -> Result<(), ServiceError> {
    let active_job_id = sqlx::query_scalar::<_, String>(
        "SELECT j.id FROM jobs j JOIN job_units u ON u.job_id = j.id \
         WHERE u.provider_id = ? \
         AND j.state NOT IN ('cancelled', 'failed', 'completed') \
         AND u.state NOT IN ('cancelled', 'failed', 'completed') \
         ORDER BY j.created_at, j.id LIMIT 1",
    )
    .bind(provider_id.to_string())
    .fetch_optional(state.database.pool())
    .await
    .map_err(|error| ServiceError::Storage(error.to_string()))?;
    if let Some(job_id) = active_job_id {
        return Err(ServiceError::Conflict(format!(
            "provider runtime cannot change while job {job_id} can still dispatch through it; pause is not sufficient, cancel or finish the job first"
        )));
    }
    Ok(())
}

async fn delete_provider(
    State(state): State<Arc<AppState>>,
    Path(id): Path<Uuid>,
) -> Result<StatusCode, ServiceError> {
    let _model_lifecycle_guard = state.model_lifecycle.lock().await;
    ensure_provider_runtime_mutation_allowed(&state, id).await?;
    let profile = state
        .catalog
        .read()
        .await
        .providers
        .get(&id)
        .cloned()
        .ok_or(ServiceError::NotFound)?;
    if matches!(profile.mode, ProviderModeView::Native) {
        return Err(ServiceError::Conflict(
            "the platform-native provider profile cannot be deleted".to_owned(),
        ));
    }
    let in_use = state
        .catalog
        .read()
        .await
        .characters
        .values()
        .flatten()
        .any(|character| {
            character
                .voice_assignment
                .as_ref()
                .is_some_and(|assignment| assignment.provider_profile_id == id)
        });
    if in_use {
        return Err(ServiceError::Conflict(
            "remove this provider from all character voice assignments before deleting it"
                .to_owned(),
        ));
    }
    let runtime_id = audiobookai_providers::ProviderId::new(id.to_string())
        .map_err(|error| ServiceError::Internal(error.to_string()))?;
    if state.providers.profile_ids().await.contains(&runtime_id) {
        if matches!(profile.mode, ProviderModeView::ManagedChild)
            && state
                .providers
                .status(&runtime_id)
                .await
                .map_err(|error| ServiceError::Conflict(error.to_string()))?
                .handle
                .is_some()
        {
            return Err(ServiceError::Conflict(
                "stop the app-owned provider process before deleting its profile".to_owned(),
            ));
        }
        state
            .providers
            .unregister(&runtime_id)
            .await
            .map_err(|error| ServiceError::Conflict(error.to_string()))?;
    }
    let secret_id = state
        .catalog
        .read()
        .await
        .provider_secret_ids
        .get(&id)
        .copied();
    state
        .database
        .repositories()
        .providers
        .delete(audiobookai_core::ProviderProfileId::from_uuid(id))
        .await
        .map_err(|error| ServiceError::Storage(error.to_string()))?;
    if let Some(secret_id) = secret_id
        && let Err(error) = state.secrets.delete(secret_id).await
    {
        tracing::warn!(diagnostic_code = "provider.secret.cleanup.failed", %secret_id, %error, "provider was deleted but its orphaned secret reference could not be removed");
    }
    let mut catalog = state.catalog.write().await;
    catalog.providers.remove(&id);
    catalog.provider_secret_ids.remove(&id);
    let removed_voice_ids = catalog
        .voices
        .iter()
        .filter(|voice| voice.provider_profile_id == id)
        .map(|voice| voice.id)
        .collect::<Vec<_>>();
    catalog
        .voices
        .retain(|voice| voice.provider_profile_id != id);
    for voice_id in removed_voice_ids {
        catalog.voice_sources.remove(&voice_id);
    }
    Ok(StatusCode::NO_CONTENT)
}

async fn create_provider(
    State(state): State<Arc<AppState>>,
    Json(input): Json<ProviderProfileInput>,
) -> Result<(StatusCode, Json<ProviderProfileView>), ServiceError> {
    let credential = input.credential;
    let kind = input
        .kind
        .ok_or_else(|| ServiceError::InvalidRequest("provider kind is required".to_owned()))?;
    let mode = input.mode.unwrap_or(ProviderModeView::CloudRemote);
    let endpoint = input.endpoint.flatten();
    let executable_path = input.executable_path.flatten();
    let working_directory = input.working_directory.flatten();
    let arguments = input.arguments.unwrap_or_default();
    let model = input.model.flatten();
    let _model_lifecycle_guard = if model.is_some() {
        Some(state.model_lifecycle.lock().await)
    } else {
        None
    };
    validate_provider_location(
        mode,
        endpoint.as_deref(),
        executable_path.as_deref(),
        working_directory.as_deref(),
        &arguments,
    )?;
    validate_provider_sensitive_fields(&kind, mode, model.as_deref(), credential.is_some())?;
    let name = input.name.unwrap_or_else(|| format!("{kind:?}"));
    reject_empty("name", &name)?;
    let capabilities = default_capabilities(&kind, mode);
    let mut profile = ProviderProfileView {
        id: Uuid::new_v4(),
        name,
        kind,
        mode,
        endpoint,
        executable_path,
        working_directory,
        arguments,
        status: ProviderStatusView::Unconfigured,
        model,
        credential_configured: false,
        capabilities: Some(capabilities),
        capability_source: Some("built_in_adapter_contract".to_owned()),
        capability_updated_at: Some(Utc::now()),
        last_error: None,
    };
    let secret_id = if let Some(credential) = credential {
        let reference = state
            .secrets
            .store(
                audiobookai_core::SecretKind::ProviderCredential,
                format!("{} credential", profile.name),
                credential.as_bytes(),
            )
            .await?;
        profile.credential_configured = true;
        Some(reference.id)
    } else {
        None
    };
    persist_provider(&state, &profile, secret_id).await?;
    let mut catalog = state.catalog.write().await;
    catalog.providers.insert(profile.id, profile.clone());
    if let Some(secret_id) = secret_id {
        catalog.provider_secret_ids.insert(profile.id, secret_id);
    }
    drop(catalog);
    if let Err(error) = state.sync_provider_runtime(profile.id).await {
        profile.status = ProviderStatusView::Unconfigured;
        profile.last_error = Some(error.to_string());
        state
            .catalog
            .write()
            .await
            .providers
            .insert(profile.id, profile.clone());
    } else if !matches!(profile.mode, ProviderModeView::Native) {
        profile.status = ProviderStatusView::Offline;
        state
            .catalog
            .write()
            .await
            .providers
            .insert(profile.id, profile.clone());
    }
    Ok((StatusCode::CREATED, Json(profile)))
}

// Provider validation, secret rotation, runtime replacement, and persistence
// must execute in this explicit order to retain rollback behavior.
#[allow(clippy::too_many_lines)]
async fn update_provider(
    State(state): State<Arc<AppState>>,
    Path(id): Path<Uuid>,
    Json(input): Json<ProviderProfileInput>,
) -> Result<Json<ProviderProfileView>, ServiceError> {
    // Every PATCH currently rebuilds the capability snapshot and replaces the registered
    // runtime, even when the visible edit appears innocuous. Serialize the complete persistence
    // and replacement window with validation/dispatch so an admitted paid request can never be
    // redirected to a different endpoint, consent class, adapter, or credential.
    let _model_lifecycle_guard = state.model_lifecycle.lock().await;
    ensure_provider_runtime_mutation_allowed(&state, id).await?;
    let credential = input.credential;
    let catalog = state.catalog.read().await;
    let old_secret_id = catalog.provider_secret_ids.get(&id).copied();
    let mut secret_id = old_secret_id;
    let mut updated = catalog
        .providers
        .get(&id)
        .cloned()
        .ok_or(ServiceError::NotFound)?;
    let was_native = is_native_provider(&updated.kind, updated.mode);
    drop(catalog);
    let runtime_id = audiobookai_providers::ProviderId::new(id.to_string())
        .map_err(|error| ServiceError::Internal(error.to_string()))?;
    if matches!(updated.mode, ProviderModeView::ManagedChild)
        && state.providers.profile_ids().await.contains(&runtime_id)
        && state
            .providers
            .status(&runtime_id)
            .await
            .map_err(|error| ServiceError::Conflict(error.to_string()))?
            .handle
            .is_some()
    {
        return Err(ServiceError::Conflict(
            "stop the app-owned provider process before changing its launch configuration"
                .to_owned(),
        ));
    }
    if let Some(name) = input.name {
        reject_empty("name", &name)?;
        updated.name = name;
    }
    if let Some(kind) = input.kind {
        updated.kind = kind;
    }
    if let Some(mode) = input.mode {
        updated.mode = mode;
    }
    if let Some(endpoint) = input.endpoint {
        updated.endpoint = endpoint;
    }
    if let Some(executable_path) = input.executable_path {
        updated.executable_path = executable_path;
    }
    if let Some(working_directory) = input.working_directory {
        updated.working_directory = working_directory;
    }
    if let Some(arguments) = input.arguments {
        updated.arguments = arguments;
    }
    if let Some(model) = input.model {
        updated.model = model;
    }
    validate_provider_location(
        updated.mode,
        updated.endpoint.as_deref(),
        updated.executable_path.as_deref(),
        updated.working_directory.as_deref(),
        &updated.arguments,
    )?;
    validate_provider_sensitive_fields(
        &updated.kind,
        updated.mode,
        updated.model.as_deref(),
        credential.is_some()
            || (!was_native
                && is_native_provider(&updated.kind, updated.mode)
                && secret_id.is_some()),
    )?;
    updated.capabilities = Some(default_capabilities(&updated.kind, updated.mode));
    updated.capability_source = Some("built_in_adapter_contract".to_owned());
    updated.capability_updated_at = Some(Utc::now());
    if let Some(credential) = credential {
        let reference = state
            .secrets
            .store(
                audiobookai_core::SecretKind::ProviderCredential,
                format!("{} credential", updated.name),
                credential.as_bytes(),
            )
            .await?;
        secret_id = Some(reference.id);
    }
    updated.credential_configured = secret_id.is_some();
    persist_provider(&state, &updated, secret_id).await?;
    if let (Some(old_secret_id), Some(new_secret_id)) = (old_secret_id, secret_id)
        && old_secret_id != new_secret_id
        && let Err(error) = state.secrets.delete(old_secret_id).await
    {
        tracing::warn!(diagnostic_code = "provider.secret.rotation_cleanup.failed", %old_secret_id, %error, "rotated provider credential but could not remove the old encrypted secret");
    }
    let mut catalog = state.catalog.write().await;
    catalog.providers.insert(id, updated.clone());
    if let Some(secret_id) = secret_id {
        catalog.provider_secret_ids.insert(id, secret_id);
    }
    drop(catalog);
    if let Err(error) = state.sync_provider_runtime(id).await {
        updated.status = ProviderStatusView::Unconfigured;
        updated.last_error = Some(error.to_string());
    } else if !matches!(updated.mode, ProviderModeView::Native) {
        updated.status = ProviderStatusView::Offline;
        updated.last_error = None;
    }
    state
        .catalog
        .write()
        .await
        .providers
        .insert(id, updated.clone());
    Ok(Json(updated))
}

// This handler is the capability-gated provider action state machine; keeping
// its branches together makes ownership and control restrictions reviewable.
#[allow(clippy::too_many_lines)]
async fn provider_action(
    State(state): State<Arc<AppState>>,
    Path((id, action)): Path<(Uuid, String)>,
    input: Option<Json<ProviderActionInput>>,
) -> Result<Json<serde_json::Value>, ServiceError> {
    let mutates_runtime = action != "logs";
    let _model_lifecycle_guard = if mutates_runtime {
        Some(state.model_lifecycle.lock().await)
    } else {
        None
    };
    if mutates_runtime {
        ensure_provider_runtime_mutation_allowed(&state, id).await?;
    }
    let profile = state
        .catalog
        .read()
        .await
        .providers
        .get(&id)
        .cloned()
        .ok_or(ServiceError::NotFound)?;
    let runtime_id = audiobookai_providers::ProviderId::new(id.to_string())
        .map_err(|error| ServiceError::Internal(error.to_string()))?;
    let runtime_registered = state.providers.profile_ids().await.contains(&runtime_id);
    if !runtime_registered && !mutates_runtime {
        return Err(ServiceError::Conflict(
            "the provider runtime is unavailable; refresh it before requesting logs".to_owned(),
        ));
    }
    if !runtime_registered {
        state.sync_provider_runtime(id).await?;
    }
    match action.as_str() {
        "refresh" => {
            let refreshed = refresh_provider(&state, id).await?;
            return Ok(Json(
                serde_json::to_value(refreshed)
                    .map_err(|error| ServiceError::Internal(error.to_string()))?,
            ));
        }
        "start" if matches!(profile.mode, ProviderModeView::ManagedChild) => {
            set_provider_status(&state, id, ProviderStatusView::Starting, None).await?;
            state
                .providers
                .start(&runtime_id)
                .await
                .map_err(|error| ServiceError::Conflict(error.to_string()))?;
            wait_for_provider_readiness(&state, id).await?;
        }
        "stop" if matches!(profile.mode, ProviderModeView::ManagedChild) => {
            set_provider_status(&state, id, ProviderStatusView::Stopping, None).await?;
            state
                .providers
                .stop(&runtime_id)
                .await
                .map_err(|error| ServiceError::Conflict(error.to_string()))?;
            set_provider_status(&state, id, ProviderStatusView::Offline, None).await?;
        }
        "restart" if matches!(profile.mode, ProviderModeView::ManagedChild) => {
            set_provider_status(&state, id, ProviderStatusView::Starting, None).await?;
            state
                .providers
                .restart(&runtime_id)
                .await
                .map_err(|error| ServiceError::Conflict(error.to_string()))?;
            wait_for_provider_readiness(&state, id).await?;
        }
        "logs" if matches!(profile.mode, ProviderModeView::ManagedChild) => {
            let logs = state
                .providers
                .logs(&runtime_id, 500)
                .await
                .map_err(|error| ServiceError::Conflict(error.to_string()))?;
            return Ok(Json(serde_json::json!({ "providerId": id, "logs": logs })));
        }
        "load-model" | "unload-model" | "switch-model" => {
            let capability = provider_capabilities_are_fresh(&profile)
                && profile.capabilities.as_ref().is_some_and(|capabilities| {
                    match action.as_str() {
                        "load-model" => capabilities.model_load,
                        "unload-model" => capabilities.model_unload,
                        "switch-model" => capabilities.model_switch,
                        _ => false,
                    }
                });
            if !capability {
                return Err(ServiceError::Conflict(
                    "the provider capability snapshot does not permit this model action".to_owned(),
                ));
            }
            let model = input
                .as_ref()
                .and_then(|Json(input)| input.model.as_deref())
                .ok_or_else(|| ServiceError::InvalidRequest("model is required".to_owned()))?;
            validate_provider_model(Some(model))?;
            match action.as_str() {
                "load-model" => state.providers.load_model(&runtime_id, model).await,
                "unload-model" => state.providers.unload_model(&runtime_id, model).await,
                "switch-model" => state.providers.switch_model(&runtime_id, model).await,
                _ => unreachable!(),
            }
            .map_err(|error| ServiceError::Conflict(error.to_string()))?;
            let mut catalog = state.catalog.write().await;
            if let Some(profile) = catalog.providers.get_mut(&id) {
                if action == "unload-model" {
                    if profile.model.as_deref().is_some_and(|selected| {
                        provider_models_equal(&profile.kind, selected, model)
                    }) {
                        profile.model = None;
                    }
                } else {
                    profile.model = Some(model.to_owned());
                }
            }
        }
        "start" | "stop" | "restart" | "logs" => {
            return Err(ServiceError::InvalidRequest(
                "process actions are available only for app-managed providers".to_owned(),
            ));
        }
        _ => return Err(ServiceError::NotFound),
    }
    let updated = state
        .catalog
        .read()
        .await
        .providers
        .get(&id)
        .cloned()
        .ok_or(ServiceError::NotFound)?;
    let secret_id = state
        .catalog
        .read()
        .await
        .provider_secret_ids
        .get(&id)
        .copied();
    persist_provider(&state, &updated, secret_id).await?;
    state.events.publish(
        "provider.updated",
        serde_json::json!({ "providerId": id, "action": action }),
    );
    Ok(Json(serde_json::to_value(updated).map_err(|error| {
        ServiceError::Internal(error.to_string())
    })?))
}

#[derive(Debug, Default, Deserialize)]
struct ProviderActionInput {
    model: Option<String>,
}

// Health probing and voice-catalog replacement are one refresh transaction;
// the linear flow avoids publishing a partially refreshed provider.
#[allow(clippy::too_many_lines)]
async fn refresh_provider(
    state: &Arc<AppState>,
    id: Uuid,
) -> Result<ProviderProfileView, ServiceError> {
    let profile = state
        .catalog
        .read()
        .await
        .providers
        .get(&id)
        .cloned()
        .ok_or(ServiceError::NotFound)?;
    let runtime_id = audiobookai_providers::ProviderId::new(id.to_string())
        .map_err(|error| ServiceError::Internal(error.to_string()))?;
    let health = if profile
        .capabilities
        .as_ref()
        .is_some_and(|capabilities| capabilities.tts)
        || matches!(
            profile.kind,
            ProviderKindView::Elevenlabs
                | ProviderKindView::MlxAudio
                | ProviderKindView::Localai
                | ProviderKindView::AlltalkV2
                | ProviderKindView::NativeOs
                | ProviderKindView::OpenaiTts
        ) {
        state
            .providers
            .tts(&runtime_id)
            .await
            .map_err(|error| ServiceError::Conflict(error.to_string()))?
            .health()
            .await
    } else {
        state
            .providers
            .character(&runtime_id)
            .await
            .map_err(|error| ServiceError::Conflict(error.to_string()))?
            .health()
            .await
    };
    let mut updated = profile;
    updated.capabilities = Some(default_capabilities(&updated.kind, updated.mode));
    updated.capability_source = Some(
        if health.is_ok() {
            "built_in_adapter_contract+health_probe"
        } else {
            "built_in_adapter_contract"
        }
        .to_owned(),
    );
    updated.capability_updated_at = Some(Utc::now());
    match health {
        Ok(health) if health.available => {
            updated.status = ProviderStatusView::Online;
            updated.last_error = health.message;
        }
        Ok(health) => {
            updated.status = ProviderStatusView::Offline;
            updated.last_error = health.message;
        }
        Err(error) => {
            updated.status = ProviderStatusView::Error;
            updated.last_error = Some(error.to_string());
        }
    }

    if updated
        .capabilities
        .as_ref()
        .is_some_and(|capabilities| capabilities.tts)
        && matches!(updated.status, ProviderStatusView::Online)
        && let Ok(voices) = state.providers.discover_voices(&runtime_id).await
    {
        let mapped = voices
            .into_iter()
            .map(|voice| {
                let voice_id = stable_voice_id(id, &voice.id);
                (
                    voice_id,
                    voice.id,
                    VoiceView {
                        id: voice_id,
                        provider_profile_id: id,
                        name: voice.name,
                        locale: voice.language,
                        gender: voice.metadata.get("gender").cloned(),
                        kind: if matches!(updated.mode, ProviderModeView::Native) {
                            crate::models::VoiceKindView::Native
                        } else if voice.owned_clone {
                            crate::models::VoiceKindView::RemoteClone
                        } else {
                            crate::models::VoiceKindView::Catalog
                        },
                        owned: voice.owned_clone,
                        preview_url: voice.metadata.get("preview_url").cloned(),
                    },
                )
            })
            .collect::<Vec<_>>();
        for (voice_id, source_id, voice) in &mapped {
            persist_discovered_voice(state, &updated, *voice_id, source_id, voice).await?;
        }
        let mut catalog = state.catalog.write().await;
        let old_ids = catalog
            .voices
            .iter()
            .filter(|voice| voice.provider_profile_id == id)
            .map(|voice| voice.id)
            .collect::<Vec<_>>();
        catalog
            .voices
            .retain(|voice| voice.provider_profile_id != id);
        for old_id in old_ids {
            catalog.voice_sources.remove(&old_id);
        }
        for (voice_id, source_id, voice) in mapped {
            catalog.voice_sources.insert(voice_id, source_id);
            catalog.voices.push(voice);
        }
    }
    let secret_id = state
        .catalog
        .read()
        .await
        .provider_secret_ids
        .get(&id)
        .copied();
    persist_provider(state, &updated, secret_id).await?;
    state
        .catalog
        .write()
        .await
        .providers
        .insert(id, updated.clone());
    Ok(updated)
}

async fn persist_discovered_voice(
    state: &AppState,
    provider: &ProviderProfileView,
    voice_id: Uuid,
    source_id: &str,
    voice: &VoiceView,
) -> Result<(), ServiceError> {
    use audiobookai_core::{
        ProviderProfileId, VoiceOrigin, VoiceOwnership, VoiceProfile, VoiceProfileId,
    };

    let existing =
        sqlx::query_scalar::<_, String>("SELECT payload FROM voice_profiles WHERE id = ?")
            .bind(voice_id.to_string())
            .fetch_optional(state.database.pool())
            .await
            .map_err(|error| ServiceError::Storage(error.to_string()))?
            .and_then(|payload| serde_json::from_str::<VoiceProfile>(&payload).ok());
    let now = Utc::now();
    let profile = VoiceProfile {
        id: VoiceProfileId::from_uuid(voice_id),
        provider_profile_id: ProviderProfileId::from_uuid(provider.id),
        provider_voice_id: Some(source_id.to_owned()),
        name: voice.name.clone(),
        origin: match voice.kind {
            crate::models::VoiceKindView::Catalog => VoiceOrigin::ProviderCatalog,
            crate::models::VoiceKindView::LocalReference => VoiceOrigin::LocalReference,
            crate::models::VoiceKindView::RemoteClone => VoiceOrigin::ProviderClone,
            crate::models::VoiceKindView::Native => VoiceOrigin::NativeSystem,
        },
        ownership: if voice.owned {
            VoiceOwnership::AudiobookAi
        } else {
            VoiceOwnership::Provider
        },
        reference_audio_artifact_ids: existing.as_ref().map_or_else(Vec::new, |profile| {
            profile.reference_audio_artifact_ids.clone()
        }),
        language: voice.locale.clone(),
        model: provider.model.clone(),
        settings: existing
            .as_ref()
            .map_or_else(std::collections::BTreeMap::new, |profile| {
                profile.settings.clone()
            }),
        created_at: existing.as_ref().map_or(now, |profile| profile.created_at),
        updated_at: now,
    };
    persist_voice_profile(state, &profile).await
}

async fn wait_for_provider_readiness(state: &Arc<AppState>, id: Uuid) -> Result<(), ServiceError> {
    for _ in 0..20 {
        let refreshed = refresh_provider(state, id).await?;
        if matches!(refreshed.status, ProviderStatusView::Online) {
            return Ok(());
        }
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
    set_provider_status(
        state,
        id,
        ProviderStatusView::Error,
        Some("provider readiness probe timed out".to_owned()),
    )
    .await?;
    Err(ServiceError::Conflict(
        "managed provider did not become ready within 10 seconds".to_owned(),
    ))
}

async fn set_provider_status(
    state: &Arc<AppState>,
    id: Uuid,
    status: ProviderStatusView,
    error: Option<String>,
) -> Result<(), ServiceError> {
    let mut catalog = state.catalog.write().await;
    let profile = catalog
        .providers
        .get_mut(&id)
        .ok_or(ServiceError::NotFound)?;
    profile.status = status;
    profile.last_error = error;
    Ok(())
}

fn stable_voice_id(provider_id: Uuid, provider_voice_id: &str) -> Uuid {
    let mut hasher = blake3::Hasher::new();
    hasher.update(provider_id.as_bytes());
    hasher.update(provider_voice_id.as_bytes());
    let hash = hasher.finalize();
    let mut bytes = [0_u8; 16];
    bytes.copy_from_slice(&hash.as_bytes()[..16]);
    Uuid::from_bytes(bytes)
}

async fn preflight_estimate(
    State(state): State<Arc<AppState>>,
    Path(id): Path<Uuid>,
) -> Result<Json<EstimateView>, ServiceError> {
    let (project, characters, providers) = {
        let catalog = state.catalog.read().await;
        (
            catalog
                .projects
                .get(&id)
                .cloned()
                .ok_or(ServiceError::NotFound)?,
            catalog.characters.get(&id).cloned().unwrap_or_default(),
            catalog.providers.clone(),
        )
    };
    Ok(Json(
        estimate_project(&state, &project, &characters, &providers).await?,
    ))
}

async fn preflight_dry_run(
    State(state): State<Arc<AppState>>,
    Path(id): Path<Uuid>,
    Json(input): Json<DryRunInput>,
) -> Result<Json<DryRunView>, ServiceError> {
    let (project, characters, providers) = {
        let catalog = state.catalog.read().await;
        (
            catalog
                .projects
                .get(&id)
                .cloned()
                .ok_or(ServiceError::NotFound)?,
            catalog.characters.get(&id).cloned().unwrap_or_default(),
            catalog.providers.clone(),
        )
    };
    let dry_run = dry_run_project(&project, &characters, &providers);
    Ok(Json(
        extend_dry_run_environment(
            &state,
            &project,
            &characters,
            &providers,
            &input.export,
            dry_run,
        )
        .await?,
    ))
}

#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
struct DryRunInput {
    #[serde(default)]
    export: ExportOptionsInput,
}

#[derive(Debug, Deserialize)]
struct PreviewInput {
    text: Option<String>,
}

async fn preflight_preview(
    State(state): State<Arc<AppState>>,
    Path(id): Path<Uuid>,
    Json(input): Json<PreviewInput>,
) -> Result<Json<PreviewView>, ServiceError> {
    let _model_lifecycle_guard = state.model_lifecycle.lock().await;
    crate::conversion::preview(Arc::clone(&state), id, input.text)
        .await
        .map(Json)
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct VoiceAuditionCandidateInput {
    candidate_id: String,
    provider_profile_id: Uuid,
    voice_id: Uuid,
    model: Option<String>,
    #[serde(default)]
    performance: audiobookai_core::PerformanceSettings,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct VoiceAuditionInput {
    text: Option<String>,
    character_id: Option<Uuid>,
    #[serde(default)]
    confirm_billable: bool,
    candidates: Vec<VoiceAuditionCandidateInput>,
}

#[derive(Debug, serde::Serialize)]
#[serde(rename_all = "camelCase")]
struct VoiceAuditionResult {
    candidate_id: String,
    provider_profile_id: Uuid,
    voice_id: Uuid,
    preview: Option<PreviewView>,
    error: Option<String>,
}

#[derive(Debug, serde::Serialize)]
#[serde(rename_all = "camelCase")]
struct VoiceAuditionResponse {
    results: Vec<VoiceAuditionResult>,
    potentially_billable: bool,
}

#[allow(clippy::too_many_lines)]
async fn voice_auditions(
    State(state): State<Arc<AppState>>,
    Path(project_id): Path<Uuid>,
    Json(input): Json<VoiceAuditionInput>,
) -> Result<Json<VoiceAuditionResponse>, ServiceError> {
    if !input.confirm_billable {
        return Err(ServiceError::Conflict(
            "confirm that voice auditions may consume provider credits or incur cost".to_owned(),
        ));
    }
    if input.candidates.is_empty() || input.candidates.len() > 6 {
        return Err(ServiceError::InvalidRequest(
            "voice auditions require between one and six candidates".to_owned(),
        ));
    }
    let mut ids = HashSet::new();
    if input.candidates.iter().any(|candidate| {
        candidate.candidate_id.trim().is_empty() || !ids.insert(&candidate.candidate_id)
    }) {
        return Err(ServiceError::InvalidRequest(
            "voice audition candidate ids must be non-empty and unique".to_owned(),
        ));
    }

    // Lock ordering is global model lifecycle, then project character lifecycle. Keep both
    // guards through validation and dispatch so no candidate can be billed against state that
    // changed after the batch-wide preflight.
    let _model_lifecycle_guard = state.model_lifecycle.lock().await;
    let project_lock = state.character_lifecycle_lock(project_id).await;
    let _project_guard = project_lock.lock().await;
    let project = state
        .database
        .repositories()
        .projects
        .get_project(audiobookai_core::ProjectId::from_uuid(project_id))
        .await
        .map_err(|error| ServiceError::Storage(error.to_string()))?
        .ok_or(ServiceError::NotFound)?;

    // Resolve and validate every candidate before the first potentially billable dispatch.
    let assignments = {
        let catalog = state.catalog.read().await;
        input
            .candidates
            .iter()
            .map(|candidate| {
                let provider = catalog
                    .providers
                    .get(&candidate.provider_profile_id)
                    .ok_or_else(|| {
                        ServiceError::InvalidRequest("unknown audition provider".to_owned())
                    })?;
                validate_billable_tts_provider_readiness(provider)?;
                if matches!(provider.mode, ProviderModeView::CloudRemote)
                    && !project.cloud_consent.book_text
                {
                    return Err(ServiceError::Conflict(format!(
                        "grant project consent before sending audition text to {}",
                        provider.name
                    )));
                }
                let voice = catalog
                    .voices
                    .iter()
                    .find(|voice| {
                        voice.id == candidate.voice_id
                            && voice.provider_profile_id == candidate.provider_profile_id
                    })
                    .ok_or_else(|| {
                        ServiceError::InvalidRequest(
                            "an audition voice does not belong to its provider".to_owned(),
                        )
                    })?;
                if catalog
                    .voice_sources
                    .get(&candidate.voice_id)
                    .is_none_or(|source| source.trim().is_empty())
                {
                    return Err(ServiceError::Conflict(
                        "an audition voice has no usable provider source".to_owned(),
                    ));
                }
                validate_voice_direction(
                    &candidate.performance,
                    &audiobookai_core::TimingSettings::default(),
                    candidate.model.as_deref().or(provider.model.as_deref()),
                    provider.capabilities.as_ref(),
                )?;
                Ok(VoiceAssignmentView {
                    provider_profile_id: candidate.provider_profile_id,
                    provider_name: provider.name.clone(),
                    voice_id: candidate.voice_id,
                    voice_name: voice.name.clone(),
                    model: candidate.model.clone(),
                    performance: candidate.performance.clone(),
                    timing: audiobookai_core::TimingSettings::default(),
                })
            })
            .collect::<Result<Vec<_>, ServiceError>>()?
    };

    let mut results = Vec::with_capacity(input.candidates.len());
    let text = input.text.clone();
    let character_id = input.character_id;
    for (candidate, assignment) in input.candidates.into_iter().zip(assignments) {
        let preview = crate::conversion::audition(
            Arc::clone(&state),
            project_id,
            text.clone(),
            character_id,
            assignment,
        )
        .await;
        let (preview, error) = match preview {
            Ok(preview) => (Some(preview), None),
            Err(error) => (None, Some(public_audition_error(&error))),
        };
        results.push(VoiceAuditionResult {
            candidate_id: candidate.candidate_id,
            provider_profile_id: candidate.provider_profile_id,
            voice_id: candidate.voice_id,
            preview,
            error,
        });
    }
    Ok(Json(VoiceAuditionResponse {
        results,
        potentially_billable: true,
    }))
}

fn public_audition_error(error: &ServiceError) -> String {
    match error {
        ServiceError::InvalidRequest(detail)
        | ServiceError::Conflict(detail)
        | ServiceError::Unauthorized(detail)
        | ServiceError::Forbidden(detail)
        | ServiceError::RateLimited(detail)
        | ServiceError::ConflictDetails { detail, .. } => detail.clone(),
        ServiceError::NotFound => "an audition resource is unavailable".to_owned(),
        ServiceError::DataDirectoryUnavailable
        | ServiceError::TlsRequiredForLan(_)
        | ServiceError::TlsConfiguration(_)
        | ServiceError::Io(_)
        | ServiceError::Join(_)
        | ServiceError::Storage(_)
        | ServiceError::Internal(_) => "the audition could not be completed".to_owned(),
    }
}

async fn list_jobs(State(state): State<Arc<AppState>>) -> Json<Page<JobView>> {
    let catalog = state.catalog.read().await;
    let mut jobs = catalog.jobs.values().cloned().collect::<Vec<_>>();
    jobs.sort_by_key(|job| std::cmp::Reverse(job.updated_at));
    Json(Page::all(jobs))
}

async fn get_job(
    State(state): State<Arc<AppState>>,
    Path(id): Path<Uuid>,
) -> Result<Json<JobView>, ServiceError> {
    state
        .catalog
        .read()
        .await
        .jobs
        .get(&id)
        .cloned()
        .map(Json)
        .ok_or(ServiceError::NotFound)
}

async fn start_job(
    State(state): State<Arc<AppState>>,
    Json(input): Json<StartJobInput>,
) -> Result<(StatusCode, Json<JobView>), ServiceError> {
    let _model_lifecycle_guard = state.model_lifecycle.lock().await;
    let project_lock = state.character_lifecycle_lock(input.project_id).await;
    let _project_guard = project_lock.lock().await;
    if let Some(job) = blocking_character_job(&state, input.project_id).await {
        return Err(active_job_conflict(&job));
    }
    let (project, characters, providers) = {
        let catalog = state.catalog.read().await;
        (
            catalog
                .projects
                .get(&input.project_id)
                .cloned()
                .ok_or(ServiceError::NotFound)?,
            catalog
                .characters
                .get(&input.project_id)
                .cloned()
                .unwrap_or_default(),
            catalog.providers.clone(),
        )
    };
    let dry_run = extend_dry_run_environment(
        &state,
        &project,
        &characters,
        &providers,
        &input.export,
        dry_run_project(&project, &characters, &providers),
    )
    .await?;
    if !dry_run.ready {
        return Err(ServiceError::Conflict(
            "dry-run checks for the current export settings must pass before conversion".to_owned(),
        ));
    }
    let job = crate::conversion::start_conversion(Arc::clone(&state), input).await?;
    Ok((StatusCode::ACCEPTED, Json(job)))
}

async fn job_action(
    State(state): State<Arc<AppState>>,
    Path((id, action)): Path<(Uuid, String)>,
) -> Result<Json<JobView>, ServiceError> {
    crate::conversion::job_action(state, id, &action)
        .await
        .map(Json)
}

async fn job_events(
    State(state): State<Arc<AppState>>,
    Path(id): Path<Uuid>,
) -> Result<Sse<impl Stream<Item = Result<Event, Infallible>>>, ServiceError> {
    if !state.catalog.read().await.jobs.contains_key(&id) {
        return Err(ServiceError::NotFound);
    }
    Ok(event_stream(state, Some(id)))
}

async fn job_playback(
    ws: WebSocketUpgrade,
    State(state): State<Arc<AppState>>,
    Path(id): Path<Uuid>,
) -> Result<Response, ServiceError> {
    if !state.catalog.read().await.jobs.contains_key(&id) {
        return Err(ServiceError::NotFound);
    }
    Ok(ws
        .on_upgrade(move |mut socket| async move {
            let mut receiver = crate::conversion::subscribe_playback(id);
            loop {
                tokio::select! {
                    playback_event = receiver.recv() => match playback_event {
                        Ok(crate::conversion::PlaybackPacket::Audio(chunk)) => {
                            if socket.send(Message::Binary(chunk)).await.is_err() {
                                break;
                            }
                        }
                        Ok(crate::conversion::PlaybackPacket::Reset) => {
                            if socket.send(Message::Text(r#"{"type":"reset"}"#.into())).await.is_err() {
                                break;
                            }
                        }
                        Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {},
                        Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
                    },
                    () = tokio::time::sleep(Duration::from_millis(500)) => {
                        if crate::conversion::job_is_terminal(&state, id).await {
                            break;
                        }
                    }
                }
            }
            let _ = socket.close().await;
        })
        .into_response())
}

async fn artifact_download(
    State(state): State<Arc<AppState>>,
    Path(id): Path<Uuid>,
    headers: HeaderMap,
) -> Result<Response, ServiceError> {
    crate::conversion::artifact_response(&state, id, &headers).await
}

async fn list_exports(
    State(state): State<Arc<AppState>>,
) -> Result<Json<Page<ExportArtifactView>>, ServiceError> {
    crate::conversion::list_exports(&state)
        .await
        .map(Page::all)
        .map(Json)
}

async fn usage_summary(State(state): State<Arc<AppState>>) -> Json<UsageSummaryView> {
    let catalog = state.catalog.read().await;
    let period_end = Utc::now();
    let period_start = period_end - ChronoDuration::days(30);
    let rows = catalog.usage_rows.clone();
    let unknown_cost_requests = rows.iter().filter(|row| row.cost_micros.is_none()).count() as u64;
    let cost = rows.iter().filter_map(|row| row.cost_micros).sum::<i64>();
    let characters = rows.iter().filter_map(|row| row.characters).sum::<u64>();
    let input_tokens = rows.iter().filter_map(|row| row.input_tokens).sum::<u64>();
    let output_tokens = rows.iter().filter_map(|row| row.output_tokens).sum::<u64>();
    Json(UsageSummaryView {
        period_start,
        period_end,
        currency: None,
        monetary_cost_micros: (!rows.is_empty()).then_some(cost),
        characters: (!rows.is_empty()).then_some(characters),
        input_tokens: (!rows.is_empty()).then_some(input_tokens),
        output_tokens: (!rows.is_empty()).then_some(output_tokens),
        credits: None,
        unknown_cost_requests,
        rows,
    })
}

async fn list_budgets(State(state): State<Arc<AppState>>) -> Json<Page<BudgetView>> {
    let catalog = state.catalog.read().await;
    Json(Page::all(catalog.budgets.values().cloned().collect()))
}

async fn create_budget(
    State(state): State<Arc<AppState>>,
    Json(input): Json<CreateBudgetInput>,
) -> Result<(StatusCode, Json<BudgetView>), ServiceError> {
    reject_empty("name", &input.name)?;
    if input.limit < 0 || input.warning_percent > 100 {
        return Err(ServiceError::InvalidRequest(
            "budget limit must be non-negative and warningPercent must be 0..100".to_owned(),
        ));
    }
    if matches!(input.metric, crate::models::BudgetMetricView::Money)
        && input.currency.as_ref().is_none_or(|currency| {
            currency.len() != 3 || !currency.bytes().all(|byte| byte.is_ascii_uppercase())
        })
    {
        return Err(ServiceError::InvalidRequest(
            "monetary budgets require a three-letter uppercase currency".to_owned(),
        ));
    }
    if !matches!(input.metric, crate::models::BudgetMetricView::Money) && input.currency.is_some() {
        return Err(ServiceError::InvalidRequest(
            "currency is valid only for monetary budgets".to_owned(),
        ));
    }
    if let Some(provider_id) = input.provider_profile_id
        && !state
            .catalog
            .read()
            .await
            .providers
            .contains_key(&provider_id)
    {
        return Err(ServiceError::InvalidRequest(
            "providerProfileId does not identify a configured provider".to_owned(),
        ));
    }
    let budget = BudgetView {
        id: Uuid::new_v4(),
        name: input.name,
        provider_profile_id: input.provider_profile_id,
        period: input.period,
        metric: input.metric,
        limit: input.limit,
        used: 0,
        reserved: 0,
        hard: input.hard,
        currency: input.currency,
        warning_percent: input.warning_percent,
    };
    persist_budget(&state, &budget).await?;
    state
        .catalog
        .write()
        .await
        .budgets
        .insert(budget.id, budget.clone());
    Ok((StatusCode::CREATED, Json(budget)))
}

async fn persist_budget(state: &AppState, view: &BudgetView) -> Result<(), ServiceError> {
    use audiobookai_core::{
        Budget, BudgetId, BudgetMetric, BudgetPeriod, BudgetScope, BudgetScopeKind,
        ProviderProfileId,
    };
    let now = Utc::now();
    let period = match view.period {
        crate::models::BudgetPeriodView::Job => BudgetPeriod::Job,
        crate::models::BudgetPeriodView::Daily => BudgetPeriod::Daily,
        crate::models::BudgetPeriodView::Monthly => BudgetPeriod::Monthly,
        crate::models::BudgetPeriodView::Lifetime => BudgetPeriod::Lifetime,
    };
    let period_ends_at = match period {
        BudgetPeriod::Job | BudgetPeriod::Lifetime => None,
        BudgetPeriod::Daily => Some(now + ChronoDuration::days(1)),
        BudgetPeriod::Monthly => Some(now + ChronoDuration::days(31)),
    };
    let budget = Budget {
        id: BudgetId::from_uuid(view.id),
        name: view.name.clone(),
        scope: BudgetScope {
            kind: if view.provider_profile_id.is_some() {
                BudgetScopeKind::Provider
            } else {
                BudgetScopeKind::Global
            },
            provider_profile_id: view.provider_profile_id.map(ProviderProfileId::from_uuid),
        },
        period,
        metric: match view.metric {
            crate::models::BudgetMetricView::Money => BudgetMetric::MoneyMicros,
            crate::models::BudgetMetricView::Tokens => BudgetMetric::TotalTokens,
            crate::models::BudgetMetricView::Characters => BudgetMetric::Characters,
            crate::models::BudgetMetricView::Credits => BudgetMetric::ProviderCredits,
        },
        currency: view.currency.clone(),
        limit: view.limit,
        used: view.used,
        warning_threshold_percent: view.warning_percent,
        hard: view.hard,
        enabled: true,
        period_started_at: now,
        period_ends_at,
        created_at: now,
        updated_at: now,
    };
    state
        .database
        .repositories()
        .budgets
        .upsert(&budget)
        .await
        .map_err(|error| ServiceError::Storage(error.to_string()))
}

async fn delete_budget(
    State(state): State<Arc<AppState>>,
    Path(id): Path<Uuid>,
) -> Result<StatusCode, ServiceError> {
    let active = sqlx::query_scalar::<_, i64>(
        "SELECT COUNT(*) FROM budget_allocations a \
         JOIN budget_reservations r ON r.id = a.reservation_id \
         WHERE a.budget_id = ? AND r.status = 'active'",
    )
    .bind(id.to_string())
    .fetch_one(state.database.pool())
    .await
    .map_err(|error| ServiceError::Storage(error.to_string()))?;
    if active > 0 {
        return Err(ServiceError::Conflict(
            "an active job still holds a reservation against this budget".to_owned(),
        ));
    }
    let result = sqlx::query("DELETE FROM budgets WHERE id = ?")
        .bind(id.to_string())
        .execute(state.database.pool())
        .await
        .map_err(|error| ServiceError::Storage(error.to_string()))?;
    if result.rows_affected() == 0 {
        return Err(ServiceError::NotFound);
    }
    state.catalog.write().await.budgets.remove(&id);
    Ok(StatusCode::NO_CONTENT)
}

#[derive(Clone, Debug, serde::Serialize)]
#[serde(rename_all = "camelCase")]
struct RateCardView {
    id: Uuid,
    provider_profile_id: Uuid,
    model: Option<String>,
    workload: audiobookai_core::UsageWorkload,
    currency: String,
    effective_at: chrono::DateTime<Utc>,
    expires_at: Option<chrono::DateTime<Utc>>,
    source: String,
    source_url: Option<String>,
    pricing: BTreeMap<String, i64>,
    user_overridden: bool,
}

impl From<audiobookai_core::RateCard> for RateCardView {
    fn from(card: audiobookai_core::RateCard) -> Self {
        Self {
            id: card.id.as_uuid(),
            provider_profile_id: card.provider_profile_id.as_uuid(),
            model: card.model,
            workload: card.workload,
            currency: card.currency,
            effective_at: card.effective_at,
            expires_at: card.expires_at,
            source: card.source,
            source_url: card.source_url,
            pricing: card.pricing,
            user_overridden: card.user_overridden,
        }
    }
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct RateCardInput {
    provider_profile_id: Uuid,
    model: Option<String>,
    workload: audiobookai_core::UsageWorkload,
    currency: String,
    effective_at: Option<chrono::DateTime<Utc>>,
    expires_at: Option<chrono::DateTime<Utc>>,
    source: String,
    source_url: Option<String>,
    pricing: BTreeMap<String, i64>,
}

async fn list_rate_cards(
    State(state): State<Arc<AppState>>,
) -> Result<Json<Page<RateCardView>>, ServiceError> {
    use sqlx::Row;

    let rows = sqlx::query("SELECT payload FROM rate_cards ORDER BY effective_at DESC")
        .fetch_all(state.database.pool())
        .await
        .map_err(|error| ServiceError::Storage(error.to_string()))?;
    let cards = rows
        .into_iter()
        .map(|row| {
            serde_json::from_str::<audiobookai_core::RateCard>(row.get::<&str, _>("payload"))
                .map(RateCardView::from)
                .map_err(|error| ServiceError::Internal(error.to_string()))
        })
        .collect::<Result<Vec<_>, _>>()?;
    Ok(Json(Page::all(cards)))
}

async fn create_rate_card(
    State(state): State<Arc<AppState>>,
    Json(input): Json<RateCardInput>,
) -> Result<(StatusCode, Json<RateCardView>), ServiceError> {
    use audiobookai_core::{RateCard, RateCardId, UsageWorkload};

    let provider = state
        .catalog
        .read()
        .await
        .providers
        .get(&input.provider_profile_id)
        .cloned()
        .ok_or_else(|| {
            ServiceError::InvalidRequest(
                "providerProfileId does not identify a configured provider".to_owned(),
            )
        })?;
    let capability_matches = provider_capabilities_are_fresh(&provider)
        && provider
            .capabilities
            .as_ref()
            .is_some_and(|capabilities| match input.workload {
                UsageWorkload::Tts => capabilities.tts,
                UsageWorkload::CharacterDetection => capabilities.character_detection,
            });
    if !capability_matches {
        return Err(ServiceError::InvalidRequest(
            "the selected provider does not support this rate-card workload".to_owned(),
        ));
    }
    let currency = input.currency.trim().to_ascii_uppercase();
    if currency.len() != 3 || !currency.bytes().all(|value| value.is_ascii_uppercase()) {
        return Err(ServiceError::InvalidRequest(
            "currency must be a three-letter ISO code".to_owned(),
        ));
    }
    let source = input.source.trim();
    if source.is_empty() || source.chars().count() > 160 {
        return Err(ServiceError::InvalidRequest(
            "rate-card source must contain 1 to 160 characters".to_owned(),
        ));
    }
    validate_rate_pricing(input.workload, &input.pricing)?;
    let effective_at = input.effective_at.unwrap_or_else(Utc::now);
    if input
        .expires_at
        .is_some_and(|expires| expires <= effective_at)
    {
        return Err(ServiceError::InvalidRequest(
            "rate-card expiry must be later than its effective time".to_owned(),
        ));
    }
    let card = RateCard {
        id: RateCardId::new(),
        provider_profile_id: audiobookai_core::ProviderProfileId::from_uuid(
            input.provider_profile_id,
        ),
        model: input
            .model
            .map(|model| model.trim().to_owned())
            .filter(|model| !model.is_empty()),
        workload: input.workload,
        currency,
        effective_at,
        expires_at: input.expires_at,
        source: source.to_owned(),
        source_url: input
            .source_url
            .map(|value| value.trim().to_owned())
            .filter(|value| !value.is_empty()),
        pricing: input.pricing,
        user_overridden: true,
    };
    persist_rate_card(&state, &card).await?;
    Ok((StatusCode::CREATED, Json(RateCardView::from(card))))
}

fn validate_rate_pricing(
    workload: audiobookai_core::UsageWorkload,
    pricing: &BTreeMap<String, i64>,
) -> Result<(), ServiceError> {
    const TTS_KEYS: &[&str] = &[
        "per_character_micros",
        "character_micros",
        "per_1000_characters_micros",
        "provider_credits_per_character_micros",
        "credits_per_character_micros",
    ];
    const AI_KEYS: &[&str] = &[
        "per_input_token_micros",
        "input_token_micros",
        "per_1m_input_tokens_micros",
        "per_output_token_micros",
        "output_token_micros",
        "per_1m_output_tokens_micros",
        "per_cached_input_token_micros",
        "cached_input_token_micros",
        "per_1m_cached_input_tokens_micros",
        "per_reasoning_token_micros",
        "reasoning_token_micros",
        "per_1m_reasoning_tokens_micros",
    ];
    let allowed = match workload {
        audiobookai_core::UsageWorkload::Tts => TTS_KEYS,
        audiobookai_core::UsageWorkload::CharacterDetection => AI_KEYS,
    };
    if pricing.is_empty() {
        return Err(ServiceError::InvalidRequest(
            "a rate card requires at least one price".to_owned(),
        ));
    }
    if let Some((key, _)) = pricing
        .iter()
        .find(|(key, value)| !allowed.contains(&key.as_str()) || **value < 0)
    {
        return Err(ServiceError::InvalidRequest(format!(
            "unsupported or negative rate-card price: {key}"
        )));
    }
    Ok(())
}

async fn persist_rate_card(
    state: &AppState,
    card: &audiobookai_core::RateCard,
) -> Result<(), ServiceError> {
    sqlx::query(
        "INSERT INTO rate_cards \
         (id, provider_id, model, workload, currency, effective_at, expires_at, payload) \
         VALUES (?, ?, ?, ?, ?, ?, ?, ?)",
    )
    .bind(card.id.to_string())
    .bind(card.provider_profile_id.to_string())
    .bind(&card.model)
    .bind(crate::accounting::workload_name(card.workload))
    .bind(&card.currency)
    .bind(card.effective_at.to_rfc3339())
    .bind(card.expires_at.map(|value| value.to_rfc3339()))
    .bind(serde_json::to_string(card).map_err(|error| ServiceError::Internal(error.to_string()))?)
    .execute(state.database.pool())
    .await
    .map_err(|error| ServiceError::Storage(error.to_string()))?;
    Ok(())
}

async fn delete_rate_card(
    State(state): State<Arc<AppState>>,
    Path(id): Path<Uuid>,
) -> Result<StatusCode, ServiceError> {
    let result = sqlx::query("DELETE FROM rate_cards WHERE id = ?")
        .bind(id.to_string())
        .execute(state.database.pool())
        .await
        .map_err(|error| ServiceError::Storage(error.to_string()))?;
    if result.rows_affected() == 0 {
        return Err(ServiceError::NotFound);
    }
    Ok(StatusCode::NO_CONTENT)
}

async fn get_settings(
    State(state): State<Arc<AppState>>,
) -> Result<Json<AppSettingsView>, ServiceError> {
    Ok(Json(settings_with_lan_status(&state).await?))
}

async fn update_settings(
    State(state): State<Arc<AppState>>,
    Json(patch): Json<serde_json::Value>,
) -> Result<Json<AppSettingsView>, ServiceError> {
    let mut settings = state.catalog.read().await.settings.clone();
    apply_owner_settings_patch(&mut settings, &patch)?;
    if patch.get("lan").is_some() && settings.lan.enabled {
        let status = state.auth.lan_status().await?;
        let mut candidate = state.config.clone();
        crate::config::apply_lan_settings(
            &mut candidate,
            &settings.lan,
            status.password_configured || status.api_token_count > 0,
        )?;
    }
    if patch.get("cacheLimitBytes").is_some() {
        crate::conversion::enforce_cache_limit(&state, settings.cache_limit_bytes).await?;
    }
    persist_settings(&state, &settings).await?;
    state.catalog.write().await.settings = settings.clone();
    Ok(Json(settings_with_lan_status(&state).await?))
}

// Owner settings validation is intentionally centralized so cross-field LAN,
// cache, and audio invariants are checked against one candidate snapshot.
#[allow(clippy::too_many_lines)]
fn apply_owner_settings_patch(
    settings: &mut AppSettingsView,
    patch: &serde_json::Value,
) -> Result<(), ServiceError> {
    if patch.get("libraryPath").is_some() || patch.get("cachePath").is_some() {
        return Err(ServiceError::InvalidRequest(
            "libraryPath and cachePath are managed by the desktop installation and are read-only"
                .to_owned(),
        ));
    }
    if let Some(language) = patch.get("language").and_then(serde_json::Value::as_str) {
        if !matches!(language, "en" | "de") {
            return Err(ServiceError::InvalidRequest(
                "language must be en or de".to_owned(),
            ));
        }
        language.clone_into(&mut settings.language);
    }
    if let Some(theme) = patch.get("theme").and_then(serde_json::Value::as_str) {
        if !matches!(theme, "system" | "light" | "dark") {
            return Err(ServiceError::InvalidRequest(
                "theme must be system, light, or dark".to_owned(),
            ));
        }
        theme.clone_into(&mut settings.theme);
    }
    assign_bool(patch, "closeToTray", &mut settings.close_to_tray);
    assign_bool(patch, "checkForUpdates", &mut settings.check_for_updates);
    assign_u64(patch, "cacheLimitBytes", &mut settings.cache_limit_bytes)?;
    assign_u16_in_range(
        patch,
        "defaultConcurrency",
        &mut settings.default_concurrency,
        1,
        32,
    )?;
    assign_u16_in_range(
        patch,
        "defaultRetryCount",
        &mut settings.default_retry_count,
        0,
        10,
    )?;
    assign_f32(patch, "defaultLufs", &mut settings.default_lufs)?;
    assign_f32(
        patch,
        "defaultTruePeakDb",
        &mut settings.default_true_peak_db,
    )?;
    if !(1_000_000_000..=9_007_199_254_740_991).contains(&settings.cache_limit_bytes) {
        return Err(ServiceError::InvalidRequest(
            "cacheLimitBytes must be between 1000000000 and 9007199254740991".to_owned(),
        ));
    }
    if !(-30.0..=-10.0).contains(&settings.default_lufs) {
        return Err(ServiceError::InvalidRequest(
            "defaultLufs must be between -30 and -10".to_owned(),
        ));
    }
    if !(-10.0..=0.0).contains(&settings.default_true_peak_db) {
        return Err(ServiceError::InvalidRequest(
            "defaultTruePeakDb must be between -10 and 0".to_owned(),
        ));
    }
    if let Some(lan) = patch.get("lan") {
        assign_bool(lan, "enabled", &mut settings.lan.enabled);
        assign_bool(lan, "tls", &mut settings.lan.tls);
        assign_bool(
            lan,
            "insecureHttpConfirmed",
            &mut settings.lan.insecure_http_confirmed,
        );
        if let Some(address) = lan.get("bindAddress").and_then(serde_json::Value::as_str) {
            address.clone_into(&mut settings.lan.bind_address);
        }
        assign_u16_in_range(lan, "port", &mut settings.lan.port, 1, u16::MAX)?;
        assign_string(
            lan,
            "certificateChainPath",
            &mut settings.lan.certificate_chain_path,
        )?;
        assign_string(lan, "privateKeyPath", &mut settings.lan.private_key_path)?;
        if let Some(hosts) = lan.get("advertisedHosts") {
            let hosts = hosts.as_array().ok_or_else(|| {
                ServiceError::InvalidRequest("advertisedHosts must be an array".to_owned())
            })?;
            if hosts.len() > 32 {
                return Err(ServiceError::InvalidRequest(
                    "at most 32 advertised LAN hosts may be configured".to_owned(),
                ));
            }
            let mut normalized = Vec::with_capacity(hosts.len());
            let mut unique = std::collections::HashSet::with_capacity(hosts.len());
            for host in hosts {
                let host = host.as_str().ok_or_else(|| {
                    ServiceError::InvalidRequest(
                        "advertisedHosts entries must be strings".to_owned(),
                    )
                })?;
                if host.len() > 253 || !unique.insert(host.to_ascii_lowercase()) {
                    return Err(ServiceError::InvalidRequest(
                        "advertised LAN hosts must be unique and no longer than 253 characters"
                            .to_owned(),
                    ));
                }
                normalized.push(host.to_owned());
            }
            settings.lan.advertised_hosts = normalized;
        }
    }
    Ok(())
}

fn imported_project_settings(
    settings: &AppSettingsView,
    output_name_template: String,
) -> audiobookai_core::ProjectSettings {
    audiobookai_core::ProjectSettings {
        global_chapter_concurrency: settings.default_concurrency,
        reliability: audiobookai_core::ReliabilityPolicy {
            max_transient_retries: settings.default_retry_count,
            ..audiobookai_core::ReliabilityPolicy::default()
        },
        output_name_template,
    }
}

async fn complete_first_run(
    State(state): State<Arc<AppState>>,
) -> Result<Json<AppSettingsView>, ServiceError> {
    let mut settings = state.catalog.read().await.settings.clone();
    settings.first_run_complete = true;
    persist_settings(&state, &settings).await?;
    state.catalog.write().await.settings = settings.clone();
    Ok(Json(settings))
}

async fn revoke_lan_sessions(
    State(state): State<Arc<AppState>>,
) -> Result<StatusCode, ServiceError> {
    state.auth.revoke_lan_sessions().await?;
    Ok(StatusCode::NO_CONTENT)
}

#[derive(Debug, Deserialize)]
struct CreateLanTokenInput {
    name: String,
}

async fn list_lan_tokens(
    State(state): State<Arc<AppState>>,
) -> Result<Json<Vec<crate::auth::ApiTokenSummary>>, ServiceError> {
    Ok(Json(state.auth.list_api_tokens().await?))
}

async fn create_lan_token(
    State(state): State<Arc<AppState>>,
    Json(input): Json<CreateLanTokenInput>,
) -> Result<(StatusCode, Json<crate::auth::IssuedApiToken>), ServiceError> {
    let token = state.auth.issue_api_token(input.name).await?;
    Ok((StatusCode::CREATED, Json(token)))
}

async fn revoke_lan_token(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
) -> Result<StatusCode, ServiceError> {
    let tokens = state.auth.list_api_tokens().await?;
    if !tokens.iter().any(|token| token.id == id) {
        return Err(ServiceError::NotFound);
    }
    let status = state.auth.lan_status().await?;
    if state.catalog.read().await.settings.lan.enabled
        && !status.password_configured
        && status.api_token_count <= 1
    {
        return Err(ServiceError::Conflict(
            "disable LAN mode or configure a password before revoking its final API token"
                .to_owned(),
        ));
    }
    state.auth.revoke_api_token(&id).await?;
    Ok(StatusCode::NO_CONTENT)
}

#[derive(Deserialize)]
struct SetLanPasswordInput {
    password: zeroize::Zeroizing<String>,
}

async fn set_lan_password(
    State(state): State<Arc<AppState>>,
    Json(input): Json<SetLanPasswordInput>,
) -> Result<StatusCode, ServiceError> {
    state
        .auth
        .configure_lan_password(&state.secrets, input.password.as_str())
        .await?;
    Ok(StatusCode::NO_CONTENT)
}

async fn settings_with_lan_status(state: &AppState) -> Result<AppSettingsView, ServiceError> {
    let mut settings = state.catalog.read().await.settings.clone();
    let status = state.auth.lan_status().await?;
    settings.lan.password_configured = status.password_configured;
    settings.lan.api_token_count = status.api_token_count;
    settings.lan.active_sessions = status.active_sessions;
    settings.lan.restart_required =
        crate::config::lan_restart_required(&settings.lan, &state.config);
    Ok(settings)
}

async fn persist_settings(
    state: &AppState,
    settings: &AppSettingsView,
) -> Result<(), ServiceError> {
    let payload = serde_json::to_string(settings)
        .map_err(|error| ServiceError::Internal(error.to_string()))?;
    sqlx::query(
        "INSERT INTO application_settings (key, updated_at, payload) VALUES ('owner', ?, ?) \
         ON CONFLICT(key) DO UPDATE SET updated_at = excluded.updated_at, payload = excluded.payload",
    )
    .bind(Utc::now().to_rfc3339())
    .bind(payload)
    .execute(state.database.pool())
    .await
    .map_err(|error| ServiceError::Storage(error.to_string()))?;
    Ok(())
}

#[derive(Debug, serde::Serialize)]
#[serde(rename_all = "camelCase")]
struct SecretStatusResponse {
    unlocked: bool,
    backend: &'static str,
}

async fn secret_status(State(state): State<Arc<AppState>>) -> Json<SecretStatusResponse> {
    let source = state.secrets.key_source().await;
    Json(SecretStatusResponse {
        unlocked: source.is_some(),
        backend: match source {
            Some(audiobookai_core::MasterKeySource::OsKeychain) => "keychain",
            Some(audiobookai_core::MasterKeySource::Argon2idPassphrase) => "passphrase",
            None => "locked",
        },
    })
}

#[derive(Deserialize)]
struct UnlockSecretStoreInput {
    passphrase: zeroize::Zeroizing<String>,
}

async fn unlock_secret_store(
    State(state): State<Arc<AppState>>,
    Json(input): Json<UnlockSecretStoreInput>,
) -> Result<Json<SecretStatusResponse>, ServiceError> {
    state
        .secrets
        .unlock_with_passphrase(input.passphrase.as_str())
        .await?;
    state.catalog.write().await.settings.secret_store = crate::models::SecretStoreView::Passphrase;
    Ok(Json(SecretStatusResponse {
        unlocked: true,
        backend: "passphrase",
    }))
}

async fn lock_secret_store(State(state): State<Arc<AppState>>) -> StatusCode {
    state.secrets.lock().await;
    state.catalog.write().await.settings.secret_store = crate::models::SecretStoreView::Locked;
    StatusCode::NO_CONTENT
}

// This is the explicit view-to-domain mapping boundary; keeping the mapping in
// one function prevents capability and provenance fields from drifting apart.
#[allow(clippy::too_many_lines)]
pub(crate) async fn persist_provider(
    state: &AppState,
    profile: &ProviderProfileView,
    secret_id: Option<audiobookai_core::SecretId>,
) -> Result<(), ServiceError> {
    use audiobookai_core::{
        CapabilitySnapshot, CapabilitySnapshotId, CharacterDetectionCapabilities,
        ControlCapabilities, PronunciationCapabilities, ProviderAudioFormat, ProviderCapabilities,
        ProviderDeployment, ProviderFamily, ProviderProfile, ProviderProfileId, ProviderRole,
        ReasoningCapability, SettingsMap, SourceProvenance, TemperatureCapability, TtsCapabilities,
        VoiceCloneCapabilities,
    };
    validate_provider_model(profile.model.as_deref())?;
    let provider_id = ProviderProfileId::from_uuid(profile.id);
    let existing = state
        .database
        .repositories()
        .providers
        .get(provider_id)
        .await
        .map_err(|error| ServiceError::Storage(error.to_string()))?;
    let family = match profile.kind {
        ProviderKindView::Elevenlabs => ProviderFamily::ElevenLabs,
        ProviderKindView::MlxAudio => ProviderFamily::MlxAudio,
        ProviderKindView::Localai => ProviderFamily::LocalAi,
        ProviderKindView::AlltalkV2 => ProviderFamily::AllTalkV2,
        ProviderKindView::NativeOs => match std::env::consts::OS {
            "windows" => ProviderFamily::NativeWindows,
            "macos" => ProviderFamily::NativeMacos,
            _ => ProviderFamily::EspeakNg,
        },
        ProviderKindView::OpenaiTts | ProviderKindView::Openai => ProviderFamily::OpenAi,
        ProviderKindView::OpenaiCompatible => ProviderFamily::OpenAiCompatible,
        ProviderKindView::Anthropic => ProviderFamily::Anthropic,
        ProviderKindView::Gemini => ProviderFamily::Gemini,
        ProviderKindView::Qwen => ProviderFamily::Qwen,
        ProviderKindView::Kimi => ProviderFamily::Kimi,
        ProviderKindView::Moonshot => ProviderFamily::Moonshot,
        ProviderKindView::LmStudio => ProviderFamily::LmStudio,
        ProviderKindView::Ollama => ProviderFamily::Ollama,
    };
    let role = if profile
        .capabilities
        .as_ref()
        .is_some_and(|caps| caps.tts && caps.character_detection)
    {
        ProviderRole::Both
    } else if profile.capabilities.as_ref().is_some_and(|caps| caps.tts)
        || matches!(
            profile.kind,
            ProviderKindView::Elevenlabs
                | ProviderKindView::MlxAudio
                | ProviderKindView::AlltalkV2
                | ProviderKindView::NativeOs
                | ProviderKindView::OpenaiTts
        )
    {
        ProviderRole::Tts
    } else {
        ProviderRole::CharacterDetection
    };
    let deployment = match profile.mode {
        ProviderModeView::CloudRemote => ProviderDeployment::CloudRemote,
        ProviderModeView::ExternalEndpoint => ProviderDeployment::ExternalEndpoint,
        ProviderModeView::ManagedChild => ProviderDeployment::ManagedChild,
        ProviderModeView::Native => ProviderDeployment::NativeInProcess,
    };
    let now = Utc::now();
    let mut settings = std::collections::BTreeMap::new();
    if let Some(model) = &profile.model {
        settings.insert("model".to_owned(), serde_json::Value::String(model.clone()));
    }
    let capability_snapshot = profile.capabilities.as_ref().map(|capabilities| {
        let observed_at = profile.capability_updated_at.unwrap_or(now);
        let reasoning = ReasoningCapability {
            disable: capabilities
                .reasoning
                .iter()
                .any(|value| value == "disabled"),
            effort: capabilities.reasoning.iter().any(|value| value == "effort"),
            adaptive: capabilities
                .reasoning
                .iter()
                .any(|value| value == "adaptive"),
            token_budget: capabilities
                .reasoning
                .iter()
                .any(|value| value == "token_budget"),
            min_token_budget: None,
            max_token_budget: None,
        };
        let mut fingerprint = blake3::Hasher::new();
        fingerprint.update(profile.endpoint.as_deref().unwrap_or("native").as_bytes());
        fingerprint.update(profile.model.as_deref().unwrap_or("default").as_bytes());
        CapabilitySnapshot {
            id: CapabilitySnapshotId::new(),
            provider_profile_id: provider_id,
            model: profile.model.clone(),
            provider_version: None,
            endpoint_fingerprint: fingerprint.finalize().to_hex().to_string(),
            capabilities: ProviderCapabilities {
                tts: capabilities.tts.then(|| TtsCapabilities {
                    streaming: capabilities.streaming,
                    // Streaming support does not imply a provider-side cancellation endpoint.
                    // None of the current TTS adapters overrides the fail-closed cancel contract.
                    cancellation: false,
                    voice_discovery: true,
                    voice_cloning: VoiceCloneCapabilities {
                        create: capabilities.voice_cloning,
                        update: capabilities.voice_cloning,
                        delete: capabilities.voice_cloning,
                        local_reference_audio: capabilities.voice_cloning,
                    },
                    pronunciation: PronunciationCapabilities {
                        provider_dictionary: capabilities.pronunciation,
                        ssml: capabilities.pronunciation,
                        ipa: capabilities.pronunciation,
                        alias: capabilities.pronunciation,
                    },
                    output_formats: match profile.kind {
                        ProviderKindView::Elevenlabs => {
                            vec![ProviderAudioFormat::PcmS16Le, ProviderAudioFormat::Mp3]
                        }
                        ProviderKindView::MlxAudio
                        | ProviderKindView::Localai
                        | ProviderKindView::OpenaiTts => vec![
                            ProviderAudioFormat::PcmS16Le,
                            ProviderAudioFormat::Wav,
                            ProviderAudioFormat::Mp3,
                            ProviderAudioFormat::Flac,
                            ProviderAudioFormat::Aac,
                        ],
                        ProviderKindView::AlltalkV2 | ProviderKindView::NativeOs => {
                            vec![ProviderAudioFormat::Wav]
                        }
                        _ => Vec::new(),
                    },
                    reports_character_usage: matches!(profile.kind, ProviderKindView::Elevenlabs),
                    reports_audio_seconds: false,
                    reports_cost: false,
                    max_input_characters: matches!(profile.kind, ProviderKindView::OpenaiTts)
                        .then_some(4096),
                    model_performance: capabilities.model_performance.clone(),
                }),
                character_detection: capabilities.character_detection.then_some({
                    CharacterDetectionCapabilities {
                        streaming: capabilities.streaming,
                        structured_output: true,
                        model_discovery: true,
                        reports_token_usage: matches!(
                            profile.kind,
                            ProviderKindView::Openai
                                | ProviderKindView::Anthropic
                                | ProviderKindView::Gemini
                                | ProviderKindView::Ollama
                        ),
                        reports_cost: false,
                        temperature: match capabilities.temperature.as_str() {
                            "number" => TemperatureCapability::Numeric,
                            "nullable" => TemperatureCapability::NumericOrNull,
                            _ => TemperatureCapability::Unsupported,
                        },
                        reasoning,
                        context_window_tokens: None,
                    }
                }),
                control: (capabilities.process_control || capabilities.model_control).then_some({
                    ControlCapabilities {
                        start: capabilities.process_control,
                        stop: capabilities.process_control,
                        restart: capabilities.process_control,
                        logs: capabilities.process_control,
                        list_installed_models: capabilities.model_list,
                        download_model: capabilities.model_download,
                        delete_model: capabilities.model_delete,
                        load_model: capabilities.model_load,
                        unload_model: capabilities.model_unload,
                        switch_model: capabilities.model_switch,
                    }
                }),
                recommended_concurrency: capabilities.max_concurrency,
            },
            provenance: SourceProvenance {
                source: profile
                    .capability_source
                    .clone()
                    .unwrap_or_else(|| "adapter_probe".to_owned()),
                source_version: Some(env!("CARGO_PKG_VERSION").to_owned()),
                request_id: None,
                observed_at: Some(observed_at),
                attributes: std::collections::BTreeMap::new(),
            },
            observed_at,
            expires_at: Some(observed_at + ChronoDuration::hours(24)),
        }
    });
    let domain = ProviderProfile {
        id: provider_id,
        name: profile.name.clone(),
        family,
        role,
        deployment,
        endpoint: profile.endpoint.clone(),
        executable_path: profile.executable_path.clone(),
        working_directory: profile.working_directory.clone(),
        arguments: profile.arguments.clone(),
        environment_secret_ids: existing
            .as_ref()
            .map(|profile| profile.environment_secret_ids.clone())
            .unwrap_or_default(),
        credential_secret_id: secret_id,
        enabled: true,
        concurrency_override: existing
            .as_ref()
            .and_then(|profile| profile.concurrency_override),
        settings: SettingsMap(settings),
        capability_snapshot,
        created_at: existing.as_ref().map_or(now, |profile| profile.created_at),
        updated_at: now,
    };
    state
        .database
        .repositories()
        .providers
        .upsert(&domain)
        .await
        .map_err(|error| ServiceError::Storage(error.to_string()))
}

#[derive(Clone, Debug)]
struct ProviderCharacterEstimate {
    provider_id: Uuid,
    provider_name: String,
    model: Option<String>,
    characters: u64,
    duration_seconds: u64,
    card: Option<audiobookai_core::RateCard>,
    cost: Option<audiobookai_core::Money>,
    credits: Option<i64>,
}

#[allow(clippy::too_many_lines)]
async fn estimate_project(
    state: &AppState,
    project: &ProjectDetail,
    characters: &[crate::models::CharacterView],
    providers: &HashMap<Uuid, ProviderProfileView>,
) -> Result<EstimateView, ServiceError> {
    let selected = project.chapters.iter().filter(|chapter| chapter.selected);
    let selected_chapters = selected.clone().count();
    let total_characters = selected
        .clone()
        .map(|chapter| u64::try_from(chapter.character_count).unwrap_or(u64::MAX))
        .sum();
    let estimated_duration_seconds = estimated_seconds(total_characters);
    let lines = priced_assignment_estimates(state, project, characters, providers).await?;
    let all_costs_known = !lines.is_empty() && lines.iter().all(|line| line.cost.is_some());
    let currencies = lines
        .iter()
        .filter_map(|line| line.cost.as_ref().map(|cost| cost.currency.clone()))
        .collect::<BTreeSet<_>>();
    let currency = (all_costs_known && currencies.len() == 1)
        .then(|| currencies.iter().next().cloned())
        .flatten();
    let monetary_cost_micros = currency.as_ref().map(|_| {
        lines
            .iter()
            .filter_map(|line| line.cost.as_ref())
            .fold(0_i64, |total, cost| total.saturating_add(cost.micros))
    });
    let credits =
        (!lines.is_empty() && lines.iter().all(|line| line.credits.is_some())).then(|| {
            lines
                .iter()
                .filter_map(|line| line.credits)
                .fold(0_i64, i64::saturating_add)
        });
    let priced_cards = lines
        .iter()
        .filter(|line| line.cost.is_some())
        .filter_map(|line| line.card.as_ref())
        .collect::<Vec<_>>();
    let price_source = (priced_cards.len() == lines.len() && !priced_cards.is_empty()).then(|| {
        priced_cards
            .iter()
            .map(|card| card.source.clone())
            .collect::<BTreeSet<_>>()
            .into_iter()
            .collect::<Vec<_>>()
            .join("; ")
    });
    let price_effective_at = (priced_cards.len() == lines.len() && !priced_cards.is_empty())
        .then(|| priced_cards.iter().map(|card| card.effective_at).min())
        .flatten();
    let mut unknown_fields = vec![
        "TTS token count (no provider tokenizer selected)".to_owned(),
        "provider throughput".to_owned(),
    ];
    if lines.is_empty() {
        unknown_fields.push("provider pricing (voice assignments incomplete)".to_owned());
        unknown_fields.push("provider credits (voice assignments incomplete)".to_owned());
    } else {
        for line in &lines {
            let model = line.model.as_deref().unwrap_or("provider default");
            if line.cost.is_none() {
                unknown_fields.push(format!(
                    "provider pricing ({} / {model})",
                    line.provider_name
                ));
            }
            if line.credits.is_none() {
                unknown_fields.push(format!(
                    "provider credits ({} / {model})",
                    line.provider_name
                ));
            }
        }
        if all_costs_known && currencies.len() > 1 {
            unknown_fields.push("aggregate cost (multiple currencies)".to_owned());
        }
    }
    unknown_fields.sort();
    unknown_fields.dedup();
    let provider_estimates = lines
        .iter()
        .map(|line| crate::models::ProviderEstimateView {
            provider_profile_id: line.provider_id,
            provider_name: line.provider_name.clone(),
            model: line.model.clone(),
            characters: line.characters,
            estimated_duration_seconds: line.duration_seconds,
            monetary_cost_micros: line.cost.as_ref().map(|cost| cost.micros),
            currency: line.cost.as_ref().map(|cost| cost.currency.clone()),
            credits: line.credits,
            rate_card_id: line.card.as_ref().map(|card| card.id.as_uuid()),
            price_source: line.card.as_ref().map(|card| card.source.clone()),
            price_effective_at: line.card.as_ref().map(|card| card.effective_at),
        })
        .collect();
    Ok(EstimateView {
        selected_chapters,
        characters: total_characters,
        estimated_tokens: None,
        estimated_duration_seconds,
        estimated_disk_bytes: estimated_duration_seconds.saturating_mul(96_000),
        estimated_completion_seconds_low: None,
        estimated_completion_seconds_high: None,
        monetary_cost_micros,
        currency,
        credits,
        price_source,
        price_effective_at,
        provider_estimates,
        unknown_fields,
    })
}

#[allow(clippy::too_many_lines)]
async fn priced_assignment_estimates(
    state: &AppState,
    project: &ProjectDetail,
    characters: &[crate::models::CharacterView],
    providers: &HashMap<Uuid, ProviderProfileView>,
) -> Result<Vec<ProviderCharacterEstimate>, ServiceError> {
    use audiobookai_core::{ProviderProfileId, UsageQuantities, UsageWorkload};

    let selected_chapters = project
        .chapters
        .iter()
        .filter(|chapter| chapter.selected)
        .map(|chapter| chapter.id)
        .collect::<HashSet<_>>();
    let total_characters = project
        .chapters
        .iter()
        .filter(|chapter| chapter.selected)
        .map(|chapter| u64::try_from(chapter.character_count).unwrap_or(u64::MAX))
        .fold(0_u64, u64::saturating_add);
    let mut by_character = HashMap::<Uuid, u64>::new();
    let mut remaining_characters = total_characters;
    for character in characters
        .iter()
        .filter(|character| !matches!(character.role, audiobookai_core::CharacterRole::Narrator))
    {
        let count = character
            .evidence
            .iter()
            .filter(|evidence| selected_chapters.contains(&evidence.chapter_id))
            .map(|evidence| {
                u64::try_from(evidence.end_offset.saturating_sub(evidence.start_offset))
                    .unwrap_or(u64::MAX)
            })
            .fold(0_u64, u64::saturating_add);
        let count = count.min(remaining_characters);
        remaining_characters = remaining_characters.saturating_sub(count);
        by_character.insert(character.id, count);
    }
    if let Some(narrator) = characters
        .iter()
        .find(|character| matches!(character.role, audiobookai_core::CharacterRole::Narrator))
    {
        by_character.insert(narrator.id, remaining_characters);
    }
    let mut grouped = BTreeMap::<(Uuid, Option<String>), (String, u64)>::new();
    for character in characters {
        let Some(assignment) = &character.voice_assignment else {
            continue;
        };
        let count = by_character.get(&character.id).copied().unwrap_or_default();
        if count == 0 {
            continue;
        }
        let provider = providers.get(&assignment.provider_profile_id);
        let model = assignment
            .model
            .clone()
            .or_else(|| provider.and_then(|provider| provider.model.clone()));
        let name = provider.map_or_else(
            || assignment.provider_name.clone(),
            |provider| provider.name.clone(),
        );
        let entry = grouped
            .entry((assignment.provider_profile_id, model))
            .or_insert((name, 0));
        entry.1 = entry.1.saturating_add(count);
    }
    let mut lines = Vec::with_capacity(grouped.len());
    for ((provider_id, model), (provider_name, characters)) in grouped {
        let card = crate::accounting::applicable_rate_card(
            state,
            ProviderProfileId::from_uuid(provider_id),
            UsageWorkload::Tts,
            model.as_deref(),
        )
        .await?;
        let duration_seconds = estimated_seconds(characters);
        let quantities = UsageQuantities {
            characters: Some(characters),
            audio_milliseconds: Some(duration_seconds.saturating_mul(1_000)),
            ..UsageQuantities::default()
        };
        let cost = card
            .as_ref()
            .and_then(|card| crate::accounting::price_quantities(card, &quantities));
        let credits = card.as_ref().and_then(|card| {
            let rate = [
                "provider_credits_per_character_micros",
                "credits_per_character_micros",
            ]
            .iter()
            .find_map(|key| card.pricing.get(*key).copied())?;
            i64::try_from(characters)
                .ok()
                .map(|count| count.saturating_mul(rate))
        });
        lines.push(ProviderCharacterEstimate {
            provider_id,
            provider_name,
            model,
            characters,
            duration_seconds,
            card,
            cost,
            credits,
        });
    }
    Ok(lines)
}

fn dry_run_project(
    project: &ProjectDetail,
    characters: &[crate::models::CharacterView],
    providers: &std::collections::HashMap<Uuid, ProviderProfileView>,
) -> DryRunView {
    let selected = project
        .chapters
        .iter()
        .filter(|chapter| chapter.selected)
        .count();
    let assignments_complete = !characters.is_empty()
        && characters
            .iter()
            .all(|character| character.voice_assignment.is_some());
    let mut provider_issues = BTreeSet::new();
    for character in characters {
        let Some(assignment) = &character.voice_assignment else {
            continue;
        };
        let Some(provider) = providers.get(&assignment.provider_profile_id) else {
            provider_issues.insert(format!(
                "{} uses a provider that no longer exists",
                character.canonical_name
            ));
            continue;
        };
        if !provider_capabilities_are_fresh(provider)
            || !provider
                .capabilities
                .as_ref()
                .is_some_and(|capabilities| capabilities.tts)
        {
            provider_issues.insert(format!("{} has no verified TTS capability", provider.name));
        }
        if !matches!(provider.status, ProviderStatusView::Online) {
            provider_issues.insert(format!("{} is not online", provider.name));
        }
        if matches!(provider.mode, ProviderModeView::CloudRemote) && !provider.credential_configured
        {
            provider_issues.insert(format!("{} has no credential", provider.name));
        }
        if matches!(provider.mode, ProviderModeView::CloudRemote) && !project.consent_cloud_text {
            provider_issues.insert(format!("{} has no project text consent", provider.name));
        }
    }
    let providers_ready = assignments_complete && provider_issues.is_empty();
    let provider_detail = if provider_issues.is_empty() {
        "Every assigned TTS provider is online and permitted".to_owned()
    } else {
        provider_issues.into_iter().collect::<Vec<_>>().join("; ")
    };
    let checks = vec![
        check(
            "chapters",
            "Chapter selection",
            selected > 0,
            format!("{selected} chapter(s) selected"),
            "Select at least one chapter",
        ),
        check(
            "character_review",
            "Character review",
            matches!(project.character_review_status, ReviewStatus::Approved),
            "Character dialogue has been reviewed".to_owned(),
            "Review and approve detected speakers",
        ),
        check(
            "voice_assignments",
            "Voice assignments",
            assignments_complete,
            "Every detected character has a voice".to_owned(),
            "Assign narrator and character voices",
        ),
        check(
            "providers",
            "TTS providers",
            providers_ready,
            provider_detail.clone(),
            &provider_detail,
        ),
    ];
    DryRunView {
        ready: checks
            .iter()
            .all(|check| matches!(check.status, CheckStatus::Pass)),
        checked_at: Utc::now(),
        checks,
    }
}

#[allow(clippy::too_many_lines)]
async fn extend_dry_run_environment(
    state: &AppState,
    project: &ProjectDetail,
    characters: &[crate::models::CharacterView],
    providers: &HashMap<Uuid, ProviderProfileView>,
    export: &ExportOptionsInput,
    mut dry_run: DryRunView,
) -> Result<DryRunView, ServiceError> {
    let (export_valid, export_detail) = validate_dry_run_export(export).await;
    dry_run.checks.push(check(
        "export_settings",
        "Export settings",
        export_valid,
        export_detail.clone(),
        &export_detail,
    ));

    let output = export
        .output_directory
        .as_deref()
        .map_or_else(|| state.config.data_dir.join("exports"), PathBuf::from);
    let (output_valid, output_detail, existing_output_ancestor) =
        inspect_output_location(&output).await;
    dry_run.checks.push(check(
        "output_location",
        "Output location",
        output_valid,
        output_detail.clone(),
        &output_detail,
    ));

    let (codecs_ready, codec_detail) = inspect_media_tools(state).await;
    dry_run.checks.push(check(
        "media_codecs",
        "Media codecs",
        codecs_ready,
        codec_detail.clone(),
        &codec_detail,
    ));

    let estimate = estimate_project(state, project, characters, providers).await?;
    let estimated_disk_bytes = estimate.estimated_disk_bytes;
    let (disk_ready, disk_detail) = if let Some(directory) = existing_output_ancestor {
        let available = tokio::task::spawn_blocking(move || fs2::available_space(directory))
            .await
            .ok()
            .and_then(Result::ok);
        available.map_or_else(
            || {
                (
                    false,
                    "Available output disk space could not be measured".to_owned(),
                )
            },
            |available| {
                (
                    available >= estimated_disk_bytes,
                    format!(
                        "{available} bytes available; {estimated_disk_bytes} bytes estimated for this conversion"
                    ),
                )
            },
        )
    } else {
        (
            false,
            "Output disk space cannot be checked until the path is valid".to_owned(),
        )
    };
    dry_run.checks.push(check(
        "disk_space",
        "Disk space",
        disk_ready,
        disk_detail.clone(),
        &disk_detail,
    ));

    let (dictionary_status, dictionary_detail, dictionary_action) =
        inspect_pronunciation_rules(state, project, characters).await;
    dry_run.checks.push(status_check(
        "pronunciation_dictionaries",
        "Pronunciation dictionaries",
        dictionary_status,
        dictionary_detail,
        dictionary_action,
    ));

    let (concurrency_status, concurrency_detail, concurrency_action) =
        inspect_concurrency(state, characters, providers).await?;
    dry_run.checks.push(status_check(
        "concurrency",
        "Provider concurrency",
        concurrency_status,
        concurrency_detail,
        concurrency_action,
    ));

    let (budget_status, budget_detail, budget_action) =
        inspect_budget_capacity(state, project, characters, providers).await?;
    dry_run.checks.push(status_check(
        "budgets",
        "Budgets and reservations",
        budget_status,
        budget_detail,
        budget_action,
    ));

    let cache_path = state.catalog.read().await.settings.cache_path.clone();
    let cache_limit = state.catalog.read().await.settings.cache_limit_bytes;
    let (cache_status, cache_detail, cache_action) =
        inspect_cache_readiness(&cache_path, cache_limit, estimated_disk_bytes).await;
    dry_run.checks.push(status_check(
        "cache",
        "Audio cache",
        cache_status,
        cache_detail,
        cache_action,
    ));
    dry_run.ready = dry_run
        .checks
        .iter()
        .all(|item| !matches!(item.status, CheckStatus::Fail | CheckStatus::Pending));
    Ok(dry_run)
}

async fn inspect_pronunciation_rules(
    state: &AppState,
    project: &ProjectDetail,
    characters: &[crate::models::CharacterView],
) -> (CheckStatus, String, Option<String>) {
    let character_ids = characters
        .iter()
        .map(|character| character.id)
        .collect::<HashSet<_>>();
    let rules = state.catalog.read().await.pronunciation_rules.clone();
    let relevant = rules
        .iter()
        .filter(|rule| {
            rule.enabled
                && match rule.scope {
                    crate::models::PronunciationScopeView::Global => true,
                    crate::models::PronunciationScopeView::Project => {
                        rule.project_id == Some(project.summary.id)
                    }
                }
        })
        .collect::<Vec<_>>();
    let mut invalid = Vec::new();
    let mut conflicts = Vec::new();
    for rule in &relevant {
        if rule.source.trim().is_empty() || rule.replacement.trim().is_empty() {
            invalid.push(rule.id.to_string());
        }
        if rule
            .character_id
            .is_some_and(|character_id| !character_ids.contains(&character_id))
        {
            invalid.push(rule.id.to_string());
        }
        if matches!(rule.kind, crate::models::PronunciationKindView::Regex)
            && regex::RegexBuilder::new(&rule.source)
                .case_insensitive(!rule.case_sensitive)
                .build()
                .is_err()
        {
            invalid.push(rule.id.to_string());
        }
    }
    for (index, rule) in relevant.iter().enumerate() {
        if relevant.iter().skip(index.saturating_add(1)).any(|other| {
            rule.project_id == other.project_id
                && rule.language == other.language
                && rule.character_id == other.character_id
                && rule.source.eq_ignore_ascii_case(&other.source)
        }) {
            conflicts.push(rule.id.to_string());
        }
    }
    invalid.sort();
    invalid.dedup();
    if !invalid.is_empty() {
        return (
            CheckStatus::Fail,
            format!(
                "{} enabled pronunciation rule(s) are invalid",
                invalid.len()
            ),
            Some("Repair or disable the invalid pronunciation rules".to_owned()),
        );
    }
    if !conflicts.is_empty() {
        return (
            CheckStatus::Warning,
            format!(
                "{} enabled rule(s) overlap; deterministic precedence will be used",
                conflicts.len()
            ),
            Some("Review the pronunciation conflict preview".to_owned()),
        );
    }
    (
        CheckStatus::Pass,
        format!(
            "{} applicable enabled pronunciation rule(s) validated",
            relevant.len()
        ),
        None,
    )
}

async fn inspect_concurrency(
    state: &AppState,
    characters: &[crate::models::CharacterView],
    providers: &HashMap<Uuid, ProviderProfileView>,
) -> Result<(CheckStatus, String, Option<String>), ServiceError> {
    let global = state.catalog.read().await.settings.default_concurrency;
    if !(1..=32).contains(&global) {
        return Ok((
            CheckStatus::Fail,
            format!("Global chapter concurrency {global} is outside the supported range"),
            Some("Set global chapter concurrency between 1 and 32".to_owned()),
        ));
    }
    let assigned = characters
        .iter()
        .filter_map(|character| {
            character
                .voice_assignment
                .as_ref()
                .map(|assignment| assignment.provider_profile_id)
        })
        .collect::<BTreeSet<_>>();
    let mut serialized = Vec::new();
    let mut verified = Vec::new();
    for provider_id in assigned {
        let Some(provider) = providers.get(&provider_id) else {
            continue;
        };
        match provider_capabilities_are_fresh(provider)
            .then_some(provider.capabilities.as_ref())
            .flatten()
            .and_then(|capabilities| capabilities.max_concurrency)
        {
            Some(limit) if limit > 0 => verified.push(format!("{}={limit}", provider.name)),
            Some(_) => {
                return Ok((
                    CheckStatus::Fail,
                    format!("{} reports an invalid zero concurrency", provider.name),
                    Some("Refresh or correct the provider capability snapshot".to_owned()),
                ));
            }
            None => serialized.push(provider.name.clone()),
        }
    }
    if !serialized.is_empty() {
        return Ok((
            CheckStatus::Warning,
            format!(
                "Global chapter concurrency is {global}; unknown provider concurrency defaults to one for {}",
                serialized.join(", ")
            ),
            Some("Optionally configure a verified provider concurrency override".to_owned()),
        ));
    }
    Ok((
        CheckStatus::Pass,
        if verified.is_empty() {
            format!("Global chapter concurrency is {global}")
        } else {
            format!(
                "Global chapter concurrency is {global}; provider limits: {}",
                verified.join(", ")
            )
        },
        None,
    ))
}

#[allow(clippy::too_many_lines)]
async fn inspect_budget_capacity(
    state: &AppState,
    project: &ProjectDetail,
    characters: &[crate::models::CharacterView],
    providers: &HashMap<Uuid, ProviderProfileView>,
) -> Result<(CheckStatus, String, Option<String>), ServiceError> {
    use audiobookai_core::{BudgetPeriod, BudgetScopeKind};

    let budgets = state
        .database
        .repositories()
        .budgets
        .list_enabled()
        .await
        .map_err(|error| ServiceError::Storage(error.to_string()))?;
    if budgets.is_empty() {
        return Ok((
            CheckStatus::Pass,
            "No enabled budget applies to this conversion".to_owned(),
            None,
        ));
    }
    let lines = priced_assignment_estimates(state, project, characters, providers).await?;
    let mut failures = Vec::new();
    let mut warnings = Vec::new();
    let mut checked = 0_usize;
    for budget in budgets {
        let applicable = lines
            .iter()
            .filter(|line| match budget.scope.kind {
                BudgetScopeKind::Global => true,
                BudgetScopeKind::Provider => budget
                    .scope
                    .provider_profile_id
                    .is_some_and(|id| id.as_uuid() == line.provider_id),
            })
            .collect::<Vec<_>>();
        if applicable.is_empty() {
            continue;
        }
        checked = checked.saturating_add(1);
        let projected = applicable.iter().try_fold(0_i64, |total, line| {
            budget_line_amount(&budget, line).map(|amount| total.saturating_add(amount.max(0)))
        });
        let reserved = state
            .database
            .repositories()
            .budgets
            .active_reserved(budget.id)
            .await
            .map_err(|error| ServiceError::Storage(error.to_string()))?;
        let remaining = if matches!(budget.period, BudgetPeriod::Job) {
            budget.limit
        } else {
            budget.remaining(reserved)
        };
        let issue = match projected {
            None => Some(format!(
                "{} has an unknown {:?} estimate",
                budget.name, budget.metric
            )),
            Some(amount) if amount > remaining => Some(format!(
                "{} needs {amount}, but only {remaining} remains",
                budget.name
            )),
            Some(amount) => {
                let projected_total = if matches!(budget.period, BudgetPeriod::Job) {
                    amount
                } else {
                    budget.used.saturating_add(reserved).saturating_add(amount)
                };
                let threshold = budget
                    .limit
                    .saturating_mul(i64::from(budget.warning_threshold_percent))
                    / 100;
                (projected_total >= threshold && budget.warning_threshold_percent > 0)
                    .then(|| format!("{} will cross its warning threshold", budget.name))
            }
        };
        if let Some(issue) = issue {
            if budget.hard && projected.is_none_or(|amount| amount > remaining) {
                failures.push(issue);
            } else {
                warnings.push(issue);
            }
        }
    }
    if !failures.is_empty() {
        return Ok((
            CheckStatus::Fail,
            failures.join("; "),
            Some(
                "Configure compatible rate cards, free budget capacity, or use an explicit per-job override"
                    .to_owned(),
            ),
        ));
    }
    if !warnings.is_empty() {
        return Ok((
            CheckStatus::Warning,
            warnings.join("; "),
            Some("Review soft-budget warnings before conversion".to_owned()),
        ));
    }
    Ok((
        CheckStatus::Pass,
        format!(
            "{checked} applicable budget(s) have sufficient unreserved capacity; no reservation was created"
        ),
        None,
    ))
}

fn budget_line_amount(
    budget: &audiobookai_core::Budget,
    line: &ProviderCharacterEstimate,
) -> Option<i64> {
    use audiobookai_core::BudgetMetric;

    match budget.metric {
        BudgetMetric::MoneyMicros => {
            let cost = line.cost.as_ref()?;
            budget
                .currency
                .as_deref()?
                .eq_ignore_ascii_case(&cost.currency)
                .then_some(cost.micros)
        }
        BudgetMetric::Characters => i64::try_from(line.characters).ok(),
        BudgetMetric::AudioMilliseconds => {
            i64::try_from(line.duration_seconds.saturating_mul(1_000)).ok()
        }
        BudgetMetric::ProviderCredits => line.credits,
        BudgetMetric::InputTokens | BudgetMetric::OutputTokens | BudgetMetric::TotalTokens => None,
    }
}

async fn inspect_cache_readiness(
    raw_path: &str,
    cache_limit: u64,
    estimated_bytes: u64,
) -> (CheckStatus, String, Option<String>) {
    let path = PathBuf::from(raw_path);
    let (valid, detail, ancestor) = inspect_output_location(&path).await;
    if !valid {
        return (
            CheckStatus::Fail,
            detail,
            Some("Repair the managed cache directory permissions".to_owned()),
        );
    }
    if cache_limit < estimated_bytes {
        return (
            CheckStatus::Fail,
            format!(
                "Cache limit is {cache_limit} bytes, below the {estimated_bytes}-byte conversion estimate"
            ),
            Some("Increase the cache limit before conversion".to_owned()),
        );
    }
    let Some(ancestor) = ancestor else {
        return (
            CheckStatus::Fail,
            "Cache disk space could not be inspected".to_owned(),
            Some("Repair the managed cache directory".to_owned()),
        );
    };
    let available = tokio::task::spawn_blocking(move || fs2::available_space(ancestor))
        .await
        .ok()
        .and_then(Result::ok);
    match available {
        Some(available) if available >= estimated_bytes => (
            CheckStatus::Pass,
            format!(
                "Cache is ready with {available} bytes available and a {cache_limit}-byte limit"
            ),
            None,
        ),
        Some(available) => (
            CheckStatus::Fail,
            format!(
                "Cache has {available} bytes available, below the {estimated_bytes}-byte estimate"
            ),
            Some("Free cache disk space before conversion".to_owned()),
        ),
        None => (
            CheckStatus::Fail,
            "Available cache disk space could not be measured".to_owned(),
            Some("Repair the managed cache directory".to_owned()),
        ),
    }
}

async fn validate_dry_run_export(export: &ExportOptionsInput) -> (bool, String) {
    if !(32..=512).contains(&export.bitrate_kbps) {
        return (
            false,
            "Audio bitrate must be between 32 and 512 kbps".to_owned(),
        );
    }
    if !export.music_gain_db.is_finite() || !(-60.0..=0.0).contains(&export.music_gain_db) {
        return (
            false,
            "Background music gain must be between -60 and 0 dB".to_owned(),
        );
    }
    let Some(music) = export.background_music_path.as_deref() else {
        return (
            true,
            "Export format and audio settings are valid".to_owned(),
        );
    };
    if !export.confirm_background_music_owned {
        return (
            false,
            "Confirm that you own or may use the selected background audio".to_owned(),
        );
    }
    let path = FilePath::new(music);
    if !path.is_absolute() {
        return (
            false,
            "Background music must use an absolute path".to_owned(),
        );
    }
    match tokio::fs::metadata(path).await {
        Ok(metadata) if metadata.is_file() => (
            true,
            "Export settings and background audio are valid".to_owned(),
        ),
        Ok(_) => (false, "Background music is not a regular file".to_owned()),
        Err(error) => (false, format!("Background music is unavailable: {error}")),
    }
}

async fn inspect_output_location(output: &FilePath) -> (bool, String, Option<PathBuf>) {
    if !output.is_absolute() {
        return (
            false,
            "The output directory must be an absolute path".to_owned(),
            None,
        );
    }
    let mut candidate = output.to_path_buf();
    loop {
        match tokio::fs::metadata(&candidate).await {
            Ok(metadata) if metadata.is_dir() => {
                if metadata.permissions().readonly() {
                    return (false, format!("{} is read-only", candidate.display()), None);
                }
                return (
                    true,
                    format!("Output will be written under {}", output.display()),
                    Some(candidate),
                );
            }
            Ok(_) => {
                return (
                    false,
                    format!("{} is not a directory", candidate.display()),
                    None,
                );
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                if !candidate.pop() {
                    return (
                        false,
                        "No existing output-directory ancestor was found".to_owned(),
                        None,
                    );
                }
            }
            Err(error) => {
                return (
                    false,
                    format!("Output location is unavailable: {error}"),
                    None,
                );
            }
        }
    }
}

async fn inspect_media_tools(state: &AppState) -> (bool, String) {
    let executable = |name: &str| {
        if cfg!(windows) {
            format!("{name}.exe")
        } else {
            name.to_owned()
        }
    };
    if let Some(directory) = &state.config.bundled_sidecar_dir {
        let ffmpeg = directory.join(executable("ffmpeg"));
        let ffprobe = directory.join(executable("ffprobe"));
        let valid = ffmpeg.is_file() && ffprobe.is_file();
        return (
            valid,
            if valid {
                format!(
                    "Bundled FFmpeg and ffprobe found in {}",
                    directory.display()
                )
            } else {
                format!(
                    "Bundled FFmpeg or ffprobe is missing from {}",
                    directory.display()
                )
            },
        );
    }
    let ffmpeg = tokio::process::Command::new(executable("ffmpeg"))
        .args(["-hide_banner", "-version"])
        .kill_on_drop(true)
        .output()
        .await;
    let ffprobe = tokio::process::Command::new(executable("ffprobe"))
        .args(["-hide_banner", "-version"])
        .kill_on_drop(true)
        .output()
        .await;
    let valid = ffmpeg.as_ref().is_ok_and(|output| output.status.success())
        && ffprobe.as_ref().is_ok_and(|output| output.status.success());
    (
        valid,
        if valid {
            "Developer FFmpeg and ffprobe are available on PATH".to_owned()
        } else {
            "FFmpeg and ffprobe are unavailable; packaged releases require bundled sidecars"
                .to_owned()
        },
    )
}

fn check(
    id: &str,
    label: &str,
    passes: bool,
    pass_detail: String,
    action: &str,
) -> DryRunCheckView {
    DryRunCheckView {
        id: id.to_owned(),
        label: label.to_owned(),
        status: if passes {
            CheckStatus::Pass
        } else {
            CheckStatus::Fail
        },
        detail: if passes {
            pass_detail
        } else {
            action.to_owned()
        },
        action: (!passes).then(|| action.to_owned()),
    }
}

fn status_check(
    id: &str,
    label: &str,
    status: CheckStatus,
    detail: String,
    action: Option<String>,
) -> DryRunCheckView {
    DryRunCheckView {
        id: id.to_owned(),
        label: label.to_owned(),
        status,
        detail,
        action,
    }
}

fn new_job(
    project_id: Uuid,
    project_title: String,
    kind: crate::models::JobKindView,
    units: Vec<JobUnitView>,
) -> JobView {
    JobView {
        id: Uuid::new_v4(),
        project_id,
        project_title,
        kind,
        status: JobStatusView::Queued,
        progress: 0.0,
        current_stage: None,
        started_at: None,
        updated_at: Utc::now(),
        estimated_remaining_seconds: None,
        units,
        progressive_playback_url: None,
        uncertain_charge: false,
    }
}

fn refresh_project_summary(project: &mut ProjectDetail) {
    project.summary.chapter_count = project.chapters.len();
    project.summary.selected_chapter_count = project
        .chapters
        .iter()
        .filter(|chapter| chapter.selected)
        .count();
    project.summary.duration_seconds = Some(
        project
            .chapters
            .iter()
            .filter(|chapter| chapter.selected)
            .filter_map(|chapter| chapter.estimated_seconds)
            .sum(),
    );
    project.summary.updated_at = Utc::now();
}

fn estimated_seconds(characters: u64) -> u64 {
    characters.div_ceil(14).max(1)
}

fn binary_response(media_type: &str, bytes: Vec<u8>) -> Result<Response, ServiceError> {
    let content_type = media_type
        .parse::<http::HeaderValue>()
        .map_err(|_| ServiceError::Internal("invalid stored media type".to_owned()))?;
    let mut headers = HeaderMap::new();
    headers.insert(header::CONTENT_TYPE, content_type);
    headers.insert(
        header::CACHE_CONTROL,
        http::HeaderValue::from_static("private, max-age=3600"),
    );
    Ok((headers, Bytes::from(bytes)).into_response())
}

// The outer option means "field omitted" and the inner option preserves an
// explicit JSON null, which is required by PATCH semantics.
#[allow(clippy::option_option)]
fn optional_string(value: &serde_json::Value, key: &str) -> Option<Option<String>> {
    value.get(key).map(|item| {
        if item.is_null() {
            None
        } else {
            item.as_str().map(str::to_owned)
        }
    })
}

fn reject_empty(field: &str, value: &str) -> Result<(), ServiceError> {
    if value.trim().is_empty() {
        Err(ServiceError::InvalidRequest(format!(
            "{field} must not be empty"
        )))
    } else {
        Ok(())
    }
}

fn validate_provider_location(
    mode: ProviderModeView,
    endpoint: Option<&str>,
    executable: Option<&str>,
    working_directory: Option<&str>,
    arguments: &[String],
) -> Result<(), ServiceError> {
    if let Some(endpoint) = endpoint {
        let url = url::Url::parse(endpoint).map_err(|_| {
            ServiceError::InvalidRequest("endpoint must be an absolute URL".to_owned())
        })?;
        if !matches!(url.scheme(), "http" | "https") || url.host_str().is_none() {
            return Err(ServiceError::InvalidRequest(
                "endpoint must be an absolute HTTP(S) URL".to_owned(),
            ));
        }
        if !url.username().is_empty() || url.password().is_some() {
            return Err(ServiceError::InvalidRequest(
                "credentials must not be embedded in provider URLs".to_owned(),
            ));
        }
        if url.query().is_some() || url.fragment().is_some() {
            return Err(ServiceError::InvalidRequest(
                "provider endpoints must not contain query parameters or fragments".to_owned(),
            ));
        }
    }
    if matches!(mode, ProviderModeView::ManagedChild) {
        let executable = executable.ok_or_else(|| {
            ServiceError::InvalidRequest(
                "managed providers require an absolute executable path".to_owned(),
            )
        })?;
        let executable_path = std::path::Path::new(executable);
        if executable.contains('\0') || !executable_path.is_absolute() {
            return Err(ServiceError::InvalidRequest(
                "managed providers require an absolute executable path".to_owned(),
            ));
        }
        let metadata = executable_path.metadata().map_err(|_| {
            ServiceError::InvalidRequest(
                "managed provider executable must be an existing file".to_owned(),
            )
        })?;
        if !metadata.is_file() {
            return Err(ServiceError::InvalidRequest(
                "managed provider executable must be an existing file".to_owned(),
            ));
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            if metadata.permissions().mode() & 0o111 == 0 {
                return Err(ServiceError::InvalidRequest(
                    "managed provider executable is not marked executable".to_owned(),
                ));
            }
        }
        if let Some(directory) = working_directory {
            let path = std::path::Path::new(directory);
            if directory.contains('\0') || !path.is_absolute() || !path.is_dir() {
                return Err(ServiceError::InvalidRequest(
                    "managed provider working directory must be an existing absolute directory"
                        .to_owned(),
                ));
            }
        }
        audiobookai_providers::validate_managed_process_arguments(arguments)
            .map_err(|error| ServiceError::InvalidRequest(error.to_string()))?;
    } else if executable.is_some() || working_directory.is_some() || !arguments.is_empty() {
        return Err(ServiceError::InvalidRequest(
            "only managed providers may define an executable, working directory, or arguments"
                .to_owned(),
        ));
    }
    Ok(())
}

fn validate_optional_provider_value(field: &str, value: Option<&str>) -> Result<(), ServiceError> {
    if value.is_some_and(|value| value.trim().is_empty()) {
        return Err(ServiceError::InvalidRequest(format!(
            "{field} must be omitted or null instead of empty"
        )));
    }
    Ok(())
}

fn validate_provider_sensitive_fields(
    kind: &ProviderKindView,
    mode: ProviderModeView,
    model: Option<&str>,
    has_credential: bool,
) -> Result<(), ServiceError> {
    validate_provider_model(model)?;
    if has_credential && is_native_provider(kind, mode) {
        return Err(ServiceError::InvalidRequest(
            "native provider profiles must not be configured with credentials".to_owned(),
        ));
    }
    Ok(())
}

fn validate_provider_model(model: Option<&str>) -> Result<(), ServiceError> {
    validate_optional_provider_value("model", model)?;
    if model.is_some_and(audiobookai_providers::contains_secret_shaped_value) {
        return Err(ServiceError::InvalidRequest(
            "provider model resembles sensitive credential material and cannot be stored"
                .to_owned(),
        ));
    }
    Ok(())
}

fn is_native_provider(kind: &ProviderKindView, mode: ProviderModeView) -> bool {
    matches!(kind, ProviderKindView::NativeOs) || matches!(mode, ProviderModeView::Native)
}

fn default_capabilities(
    kind: &ProviderKindView,
    mode: ProviderModeView,
) -> ProviderCapabilitiesView {
    let tts = matches!(
        kind,
        ProviderKindView::Elevenlabs
            | ProviderKindView::MlxAudio
            | ProviderKindView::Localai
            | ProviderKindView::AlltalkV2
            | ProviderKindView::NativeOs
            | ProviderKindView::OpenaiTts
    );
    let character_detection = matches!(
        kind,
        ProviderKindView::Openai
            | ProviderKindView::OpenaiCompatible
            | ProviderKindView::Anthropic
            | ProviderKindView::Gemini
            | ProviderKindView::Qwen
            | ProviderKindView::Kimi
            | ProviderKindView::Moonshot
            | ProviderKindView::LmStudio
            | ProviderKindView::Ollama
    );
    let provider_model_library = matches!(
        kind,
        ProviderKindView::LmStudio | ProviderKindView::Ollama | ProviderKindView::Localai
    );
    let provider_model_runtime = matches!(
        kind,
        ProviderKindView::LmStudio | ProviderKindView::Ollama | ProviderKindView::Localai
    );
    ProviderCapabilitiesView {
        tts,
        character_detection,
        streaming: matches!(
            kind,
            ProviderKindView::Elevenlabs
                | ProviderKindView::MlxAudio
                | ProviderKindView::Localai
                | ProviderKindView::OpenaiTts
        ),
        voice_cloning: matches!(kind, ProviderKindView::Elevenlabs),
        pronunciation: matches!(kind, ProviderKindView::Elevenlabs),
        process_control: matches!(mode, ProviderModeView::ManagedChild),
        model_control: provider_model_library,
        model_list: provider_model_library,
        model_download: provider_model_library,
        // LocalAI's adapter checks its authenticated /system view and fails closed unless the
        // target's loaded state can be proven before the provider deletion endpoint is called.
        model_delete: matches!(kind, ProviderKindView::Ollama | ProviderKindView::Localai),
        model_load: provider_model_runtime,
        model_unload: provider_model_runtime,
        // The current provider trait's default switch operation is only an alias for load. Do
        // not advertise a true atomic switch until an adapter implements that contract.
        model_switch: false,
        temperature: match kind {
            ProviderKindView::Openai => "nullable",
            ProviderKindView::OpenaiCompatible
            | ProviderKindView::Anthropic
            | ProviderKindView::Gemini
            | ProviderKindView::Qwen
            | ProviderKindView::Kimi
            | ProviderKindView::Moonshot
            | ProviderKindView::LmStudio
            | ProviderKindView::Ollama => "number",
            _ => "unsupported",
        }
        .to_owned(),
        reasoning: match kind {
            ProviderKindView::Openai | ProviderKindView::Ollama => {
                vec!["disabled".to_owned(), "effort".to_owned()]
            }
            ProviderKindView::Anthropic => vec![
                "disabled".to_owned(),
                "adaptive".to_owned(),
                "token_budget".to_owned(),
            ],
            ProviderKindView::Gemini => {
                vec!["disabled".to_owned(), "token_budget".to_owned()]
            }
            ProviderKindView::Qwen | ProviderKindView::Kimi => {
                vec!["disabled".to_owned()]
            }
            _ => Vec::new(),
        },
        max_concurrency: Some(1),
        model_performance: default_model_performance(kind),
    }
}

fn default_model_performance(
    kind: &ProviderKindView,
) -> Vec<audiobookai_core::ModelPerformanceCapabilities> {
    use audiobookai_core::{
        ModelPerformanceCapabilities, PerformanceCapabilities, PerformanceRange,
    };

    if matches!(kind, ProviderKindView::OpenaiTts) {
        return audiobookai_providers::adapters::openai_tts_model_performance_capabilities();
    }
    if !matches!(kind, ProviderKindView::Elevenlabs) {
        return Vec::new();
    }
    let performance = PerformanceCapabilities {
        speed: Some(PerformanceRange::new(0.7, 1.2)),
        pitch: None,
        stability: Some(PerformanceRange::new(0.0, 1.0)),
        similarity: Some(PerformanceRange::new(0.0, 1.0)),
        style: Some(PerformanceRange::new(0.0, 1.0)),
        speaker_boost: true,
        delivery_cues: Vec::new(),
    };
    [
        "eleven_multilingual_v2",
        "eleven_flash_v2_5",
        "eleven_turbo_v2_5",
    ]
    .into_iter()
    .map(|model| ModelPerformanceCapabilities {
        model: model.to_owned(),
        performance: performance.clone(),
    })
    .collect()
}

pub(crate) fn provider_capabilities_are_fresh(profile: &ProviderProfileView) -> bool {
    profile
        .capability_updated_at
        .is_some_and(|observed_at| observed_at + ChronoDuration::hours(24) > Utc::now())
}

pub(crate) fn validate_billable_tts_provider_readiness(
    profile: &ProviderProfileView,
) -> Result<(), ServiceError> {
    if !provider_capabilities_are_fresh(profile)
        || !profile
            .capabilities
            .as_ref()
            .is_some_and(|capabilities| capabilities.tts)
    {
        return Err(ServiceError::Conflict(format!(
            "refresh provider '{}' to verify its current TTS capability",
            profile.name
        )));
    }
    if !matches!(profile.status, ProviderStatusView::Online) {
        return Err(ServiceError::Conflict(format!(
            "provider '{}' must be online before paid synthesis",
            profile.name
        )));
    }
    if matches!(profile.mode, ProviderModeView::CloudRemote) && !profile.credential_configured {
        return Err(ServiceError::Conflict(format!(
            "configure a credential for cloud provider '{}' before paid synthesis",
            profile.name
        )));
    }
    Ok(())
}

fn assign_bool(value: &serde_json::Value, key: &str, target: &mut bool) {
    if let Some(next) = value.get(key).and_then(serde_json::Value::as_bool) {
        *target = next;
    }
}

fn assign_string(
    value: &serde_json::Value,
    key: &str,
    target: &mut String,
) -> Result<(), ServiceError> {
    if let Some(raw) = value.get(key) {
        raw.as_str()
            .ok_or_else(|| ServiceError::InvalidRequest(format!("{key} must be a string")))?
            .clone_into(target);
    }
    Ok(())
}

fn assign_u64(value: &serde_json::Value, key: &str, target: &mut u64) -> Result<(), ServiceError> {
    if let Some(raw) = value.get(key) {
        *target = raw
            .as_u64()
            .ok_or_else(|| ServiceError::InvalidRequest(format!("{key} must be non-negative")))?;
    }
    Ok(())
}

fn assign_u16_in_range(
    value: &serde_json::Value,
    key: &str,
    target: &mut u16,
    minimum: u16,
    maximum: u16,
) -> Result<(), ServiceError> {
    if let Some(raw) = value.get(key) {
        let parsed = raw
            .as_u64()
            .and_then(|number| u16::try_from(number).ok())
            .filter(|number| (minimum..=maximum).contains(number))
            .ok_or_else(|| {
                ServiceError::InvalidRequest(format!(
                    "{key} must be between {minimum} and {maximum}"
                ))
            })?;
        *target = parsed;
    }
    Ok(())
}

fn assign_f32(value: &serde_json::Value, key: &str, target: &mut f32) -> Result<(), ServiceError> {
    if let Some(raw) = value.get(key) {
        *target = json_f32(raw, key)?;
    }
    Ok(())
}

// JSON numbers are f64; domain settings deliberately use f32 and reject
// non-finite or out-of-range values before the narrowing conversion.
#[allow(clippy::cast_possible_truncation)]
fn json_f32(value: &serde_json::Value, key: &str) -> Result<f32, ServiceError> {
    let number = value
        .as_f64()
        .filter(|number| number.is_finite())
        .ok_or_else(|| ServiceError::InvalidRequest(format!("{key} must be a number")))?;
    if !(f64::from(f32::MIN)..=f64::from(f32::MAX)).contains(&number) {
        return Err(ServiceError::InvalidRequest(format!(
            "{key} is outside the supported numeric range"
        )));
    }
    Ok(number as f32)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn durable_copy_does_not_inherit_read_only_source_permissions() {
        let directory = tempfile::tempdir().expect("temporary directory");
        let source = directory.path().join("source.epub");
        let destination = directory.path().join("import.epub");
        tokio::fs::write(&source, b"epub bytes").await.unwrap();
        let original_permissions = tokio::fs::metadata(&source).await.unwrap().permissions();
        let mut read_only_permissions = original_permissions.clone();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            read_only_permissions.set_mode(read_only_permissions.mode() & !0o222);
        }
        #[cfg(windows)]
        read_only_permissions.set_readonly(true);
        tokio::fs::set_permissions(&source, read_only_permissions)
            .await
            .unwrap();

        copy_file_durably(&source, &destination)
            .await
            .expect("durable copy");

        assert_eq!(tokio::fs::read(destination).await.unwrap(), b"epub bytes");
        tokio::fs::set_permissions(source, original_permissions)
            .await
            .unwrap();
    }

    fn billable_tts_provider_fixture() -> ProviderProfileView {
        ProviderProfileView {
            id: Uuid::new_v4(),
            name: "TTS fixture".to_owned(),
            kind: ProviderKindView::Localai,
            mode: ProviderModeView::ExternalEndpoint,
            endpoint: Some("http://127.0.0.1:8080".to_owned()),
            executable_path: None,
            working_directory: None,
            arguments: Vec::new(),
            status: ProviderStatusView::Online,
            model: Some("tts-model".to_owned()),
            credential_configured: false,
            capabilities: Some(default_capabilities(
                &ProviderKindView::Localai,
                ProviderModeView::ExternalEndpoint,
            )),
            capability_source: Some("test".to_owned()),
            capability_updated_at: Some(Utc::now()),
            last_error: None,
        }
    }

    #[test]
    fn billable_tts_readiness_requires_fresh_online_and_configured_cloud_provider() {
        let ready = billable_tts_provider_fixture();
        validate_billable_tts_provider_readiness(&ready).expect("fresh local provider");

        let mut stale = ready.clone();
        stale.capability_updated_at = Some(Utc::now() - ChronoDuration::hours(25));
        assert!(validate_billable_tts_provider_readiness(&stale).is_err());

        let mut offline = ready.clone();
        offline.status = ProviderStatusView::Offline;
        assert!(validate_billable_tts_provider_readiness(&offline).is_err());

        let mut cloud = ready;
        cloud.mode = ProviderModeView::CloudRemote;
        assert!(validate_billable_tts_provider_readiness(&cloud).is_err());
        cloud.credential_configured = true;
        validate_billable_tts_provider_readiness(&cloud).expect("configured cloud provider");
    }

    #[test]
    fn project_mutation_admission_includes_regeneration_and_proof_export() {
        for kind in [
            crate::models::JobKindView::CharacterDetection,
            crate::models::JobKindView::Conversion,
            crate::models::JobKindView::SegmentRegeneration,
            crate::models::JobKindView::Export,
        ] {
            assert!(
                blocks_project_mutation(kind),
                "{kind:?} must block mutation"
            );
        }
        for kind in [
            crate::models::JobKindView::Preview,
            crate::models::JobKindView::QualityControl,
            crate::models::JobKindView::CacheCleanup,
        ] {
            assert!(
                !blocks_project_mutation(kind),
                "{kind:?} must not block mutation"
            );
        }
    }

    #[test]
    fn character_review_revision_uses_the_public_camel_case_contract() {
        let input: ReviewInput = serde_json::from_value(serde_json::json!({
            "approved": true,
            "expectedCharacterRevision": 7,
        }))
        .expect("review input");

        assert!(input.approved);
        assert_eq!(input.expected_character_revision, 7);
    }

    fn project_fixture() -> ProjectDetail {
        let now = Utc::now();
        ProjectDetail {
            summary: BookSummary {
                id: Uuid::new_v4(),
                title: "Book".to_owned(),
                author: None,
                cover_url: None,
                chapter_count: 1,
                selected_chapter_count: 1,
                duration_seconds: None,
                progress: 0.0,
                status: ProjectDisplayStatus::Draft,
                updated_at: now,
                language: None,
                series: None,
                series_position: None,
            },
            narrator: None,
            publisher: None,
            description: None,
            consent_cloud_text: false,
            consent_cloud_audio: false,
            chapters: vec![ChapterView {
                id: Uuid::new_v4(),
                index: 0,
                title: "One".to_owned(),
                selected: true,
                word_count: 20,
                character_count: 140,
                estimated_seconds: None,
                status: ChapterDisplayStatus::Pending,
            }],
            character_review_status: ReviewStatus::NotStarted,
            character_revision: 0,
            output_name: None,
        }
    }

    #[tokio::test]
    async fn estimates_without_making_provider_requests() {
        let directory = tempfile::tempdir().expect("tempdir");
        let database = audiobookai_storage::Database::open_in(directory.path())
            .await
            .expect("database");
        let state = AppState::new(
            crate::ServiceConfig {
                bind: "127.0.0.1:0".parse().expect("address"),
                data_dir: directory.path().to_path_buf(),
                bundled_sidecar_dir: None,
                tls: None,
                lan_hostnames: Vec::new(),
                allow_insecure_lan: false,
                desktop_bootstrap: false,
            },
            database,
        )
        .await
        .expect("state");
        let project = project_fixture();
        let estimate = estimate_project(&state, &project, &[], &HashMap::new())
            .await
            .expect("estimate");
        assert_eq!(estimate.characters, 140);
        assert_eq!(estimate.estimated_duration_seconds, 10);
        assert!(estimate.estimated_tokens.is_none());
        assert!(estimate.monetary_cost_micros.is_none());
    }

    #[tokio::test]
    async fn estimate_uses_stored_assignment_and_rate_card_provenance() {
        use audiobookai_core::{ProviderProfileId, RateCard, RateCardId, UsageWorkload};

        let directory = tempfile::tempdir().expect("tempdir");
        let database = audiobookai_storage::Database::open_in(directory.path())
            .await
            .expect("database");
        let state = AppState::new(
            crate::ServiceConfig {
                bind: "127.0.0.1:0".parse().expect("address"),
                data_dir: directory.path().to_path_buf(),
                bundled_sidecar_dir: None,
                tls: None,
                lan_hostnames: Vec::new(),
                allow_insecure_lan: false,
                desktop_bootstrap: false,
            },
            database,
        )
        .await
        .expect("state");
        let provider_id = Uuid::new_v4();
        let provider = ProviderProfileView {
            id: provider_id,
            name: "Local fixture".to_owned(),
            kind: ProviderKindView::Localai,
            mode: ProviderModeView::ExternalEndpoint,
            endpoint: Some("http://127.0.0.1:8080".to_owned()),
            executable_path: None,
            working_directory: None,
            arguments: Vec::new(),
            status: ProviderStatusView::Online,
            model: Some("fixture-model".to_owned()),
            credential_configured: false,
            capabilities: Some(default_capabilities(
                &ProviderKindView::Localai,
                ProviderModeView::ExternalEndpoint,
            )),
            capability_source: Some("fixture".to_owned()),
            capability_updated_at: Some(Utc::now()),
            last_error: None,
        };
        persist_provider(&state, &provider, None)
            .await
            .expect("provider");
        let effective_at = Utc::now() - ChronoDuration::minutes(1);
        let card = RateCard {
            id: RateCardId::new(),
            provider_profile_id: ProviderProfileId::from_uuid(provider_id),
            model: Some("fixture-model".to_owned()),
            workload: UsageWorkload::Tts,
            currency: "EUR".to_owned(),
            effective_at,
            expires_at: None,
            source: "Local fixture rate".to_owned(),
            source_url: None,
            pricing: BTreeMap::from([
                ("per_1000_characters_micros".to_owned(), 1_000),
                ("credits_per_character_micros".to_owned(), 2),
            ]),
            user_overridden: true,
        };
        persist_rate_card(&state, &card).await.expect("rate card");
        let project = project_fixture();
        let characters = vec![crate::models::CharacterView {
            id: Uuid::new_v4(),
            role: audiobookai_core::CharacterRole::Narrator,
            canonical_name: "Narrator".to_owned(),
            aliases: Vec::new(),
            confidence: 1.0,
            dialogue_count: 0,
            voice_assignment: Some(VoiceAssignmentView {
                provider_profile_id: provider_id,
                provider_name: provider.name.clone(),
                voice_id: Uuid::new_v4(),
                voice_name: "Fixture voice".to_owned(),
                model: None,
                performance: audiobookai_core::PerformanceSettings::default(),
                timing: audiobookai_core::TimingSettings::default(),
            }),
            evidence: Vec::new(),
        }];
        let providers = HashMap::from([(provider_id, provider)]);
        let estimate = estimate_project(&state, &project, &characters, &providers)
            .await
            .expect("estimate");

        assert_eq!(estimate.monetary_cost_micros, Some(140));
        assert_eq!(estimate.currency.as_deref(), Some("EUR"));
        assert_eq!(estimate.credits, Some(280));
        assert_eq!(estimate.price_source.as_deref(), Some("Local fixture rate"));
        assert_eq!(estimate.price_effective_at, Some(effective_at));
        assert_eq!(estimate.provider_estimates.len(), 1);
        assert_eq!(estimate.provider_estimates[0].characters, 140);
        assert_eq!(
            estimate.provider_estimates[0].model.as_deref(),
            Some("fixture-model")
        );
    }

    #[test]
    fn dry_run_accepts_an_online_local_tts_provider_without_a_credential() {
        let provider_id = Uuid::new_v4();
        let voice_id = Uuid::new_v4();
        let mut project = project_fixture();
        project.character_review_status = ReviewStatus::Approved;
        let characters = vec![crate::models::CharacterView {
            id: Uuid::new_v4(),
            role: audiobookai_core::CharacterRole::Narrator,
            canonical_name: "Narrator".to_owned(),
            aliases: Vec::new(),
            confidence: 1.0,
            dialogue_count: 1,
            voice_assignment: Some(VoiceAssignmentView {
                provider_profile_id: provider_id,
                provider_name: "LocalAI".to_owned(),
                voice_id,
                voice_name: "Local voice".to_owned(),
                model: None,
                performance: audiobookai_core::PerformanceSettings::default(),
                timing: audiobookai_core::TimingSettings::default(),
            }),
            evidence: Vec::new(),
        }];
        let provider = ProviderProfileView {
            id: provider_id,
            name: "LocalAI".to_owned(),
            kind: ProviderKindView::Localai,
            mode: ProviderModeView::ExternalEndpoint,
            endpoint: Some("http://127.0.0.1:8080".to_owned()),
            executable_path: None,
            working_directory: None,
            arguments: Vec::new(),
            status: ProviderStatusView::Online,
            model: None,
            credential_configured: false,
            capabilities: Some(default_capabilities(
                &ProviderKindView::Localai,
                ProviderModeView::ExternalEndpoint,
            )),
            capability_source: Some("test".to_owned()),
            capability_updated_at: Some(Utc::now()),
            last_error: None,
        };
        let result = dry_run_project(
            &project,
            &characters,
            &[(provider_id, provider)].into_iter().collect(),
        );
        assert!(result.ready, "{:#?}", result.checks);
    }

    #[test]
    fn managed_provider_requires_an_absolute_executable() {
        assert!(
            validate_provider_location(
                ProviderModeView::ManagedChild,
                None,
                Some("bin/server"),
                None,
                &[],
            )
            .is_err()
        );
    }

    #[test]
    fn provider_patch_distinguishes_omitted_values_from_explicit_clears() {
        let omitted: ProviderProfileInput =
            serde_json::from_value(serde_json::json!({})).expect("omitted patch");
        let cleared: ProviderProfileInput = serde_json::from_value(serde_json::json!({
            "endpoint": null,
            "executablePath": null,
            "workingDirectory": null,
            "model": null,
            "arguments": []
        }))
        .expect("clear patch");

        assert!(omitted.endpoint.is_none());
        assert!(matches!(cleared.endpoint, Some(None)));
        assert!(matches!(cleared.executable_path, Some(None)));
        assert!(matches!(cleared.working_directory, Some(None)));
        assert!(matches!(cleared.model, Some(None)));
        assert_eq!(cleared.arguments, Some(Vec::new()));
    }

    #[test]
    fn provider_model_discovery_uses_the_flat_provider_contract() {
        let provider_id = Uuid::new_v4();
        let input: ProviderModelDiscoveryInput = serde_json::from_value(serde_json::json!({
            "providerId": provider_id,
            "name": "Local model preview",
            "kind": "ollama",
            "mode": "external_endpoint",
            "endpoint": "http://127.0.0.1:11434/",
            "model": null
        }))
        .expect("provider model discovery input");

        assert_eq!(input.provider_id, Some(provider_id));
        assert!(matches!(input.profile.kind, Some(ProviderKindView::Ollama)));
        assert!(matches!(
            input.profile.endpoint,
            Some(Some(ref endpoint)) if endpoint == "http://127.0.0.1:11434/"
        ));
        assert!(matches!(input.profile.model, Some(None)));
    }

    #[test]
    fn provider_input_debug_never_contains_the_credential() {
        let credential = ["runtime", "credential", "value"].join("-");
        let input: ProviderProfileInput = serde_json::from_value(serde_json::json!({
            "credential": credential
        }))
        .expect("provider input");
        let debug = format!("{input:?}");
        assert!(!debug.contains(&credential));
        assert!(debug.contains("[REDACTED]"));
    }

    #[test]
    fn managed_arguments_are_bounded_and_never_shell_parsed() {
        let executable = std::env::current_exe().expect("test executable");
        let working_directory = std::env::current_dir().expect("test working directory");
        assert!(
            validate_provider_location(
                ProviderModeView::ManagedChild,
                Some("http://127.0.0.1:8080"),
                executable.to_str(),
                working_directory.to_str(),
                &["--listen".to_owned(), "127.0.0.1; echo ignored".to_owned()],
            )
            .is_ok()
        );
        assert!(
            validate_provider_location(
                ProviderModeView::ManagedChild,
                None,
                executable.to_str(),
                None,
                &["bad\0argument".to_owned()],
            )
            .is_err()
        );
        assert!(
            validate_provider_location(
                ProviderModeView::ManagedChild,
                None,
                working_directory.to_str(),
                None,
                &[],
            )
            .is_err()
        );
    }

    #[test]
    fn managed_arguments_reject_credential_flags() {
        let executable = std::env::current_exe().expect("test executable");
        let working_directory = std::env::current_dir().expect("test working directory");
        let arguments = [
            "--api-key".to_owned(),
            format!("--{}={}", "TOKEN", "fixture-value"),
            "--auth_token".to_owned(),
            format!("--{}={}", "password", "fixture-value"),
            "--authorization".to_owned(),
        ];
        for argument in arguments {
            let result = validate_provider_location(
                ProviderModeView::ManagedChild,
                None,
                executable.to_str(),
                working_directory.to_str(),
                &[argument],
            );
            assert!(result.is_err(), "credential flag should be rejected");
            assert!(
                result
                    .expect_err("credential flag rejection")
                    .to_string()
                    .contains("encrypted credential storage")
            );
        }
    }

    #[test]
    fn managed_arguments_reject_secret_shaped_whole_values_without_echoing_them() {
        let executable = std::env::current_exe().expect("test executable");
        let working_directory = std::env::current_dir().expect("test working directory");
        let prefixed = [
            ["h", "f"].concat(),
            "syntheticcredential0123456789".to_owned(),
        ]
        .join("_");
        let jwt = ["headerpart0123", "payloadpart4567", "signaturepart89"].join(".");
        for argument in [prefixed.clone(), jwt.clone(), format!("--cache={prefixed}")] {
            let error = validate_provider_location(
                ProviderModeView::ManagedChild,
                None,
                executable.to_str(),
                working_directory.to_str(),
                std::slice::from_ref(&argument),
            )
            .expect_err("secret-shaped process argument must be rejected");
            let message = error.to_string();
            assert!(message.contains("encrypted credential storage"));
            assert!(!message.contains(&argument));
        }
    }

    #[test]
    fn provider_models_reject_secret_shapes_and_native_profiles_reject_credentials() {
        let prefixed = [
            ["s", "k"].concat(),
            "syntheticcredential0123456789".to_owned(),
        ]
        .join("-");
        let jwt = ["headerpart0123", "payloadpart4567", "signaturepart89"].join(".");
        for model in [prefixed.clone(), jwt, format!("owner/{prefixed}")] {
            let error = validate_provider_sensitive_fields(
                &ProviderKindView::Ollama,
                ProviderModeView::ExternalEndpoint,
                Some(&model),
                false,
            )
            .expect_err("secret-shaped model must be rejected before persistence");
            let message = error.to_string();
            assert!(message.contains("sensitive credential material"));
            assert!(!message.contains(&model));
        }
        assert!(
            validate_provider_sensitive_fields(
                &ProviderKindView::Ollama,
                ProviderModeView::ExternalEndpoint,
                Some("gemma3:latest"),
                false,
            )
            .is_ok()
        );

        for (kind, mode) in [
            (ProviderKindView::NativeOs, ProviderModeView::Native),
            (
                ProviderKindView::NativeOs,
                ProviderModeView::ExternalEndpoint,
            ),
            (ProviderKindView::Elevenlabs, ProviderModeView::Native),
        ] {
            let error = validate_provider_sensitive_fields(&kind, mode, None, true)
                .expect_err("native credentials must be rejected");
            assert!(error.to_string().contains("must not be configured"));
        }
        assert!(
            validate_provider_sensitive_fields(
                &ProviderKindView::NativeOs,
                ProviderModeView::Native,
                None,
                false,
            )
            .is_ok()
        );
    }

    #[test]
    fn built_in_capabilities_only_advertise_reachable_controls() {
        let local_ai =
            default_capabilities(&ProviderKindView::Localai, ProviderModeView::ManagedChild);
        assert!(local_ai.tts);
        assert!(!local_ai.character_detection);
        assert!(!local_ai.voice_cloning);
        assert!(local_ai.process_control);
        assert!(local_ai.model_control);
        assert!(local_ai.model_list);
        assert!(local_ai.model_download);
        assert!(local_ai.model_delete);
        assert!(local_ai.model_load);
        assert!(local_ai.model_unload);
        assert!(!local_ai.model_switch);

        let ollama = default_capabilities(
            &ProviderKindView::Ollama,
            ProviderModeView::ExternalEndpoint,
        );
        assert!(!ollama.tts);
        assert!(ollama.character_detection);
        assert!(ollama.model_control);
        assert_eq!(ollama.temperature, "number");
        assert_eq!(ollama.reasoning, ["disabled", "effort"]);

        let openai_speech =
            default_capabilities(&ProviderKindView::OpenaiTts, ProviderModeView::CloudRemote);
        assert!(openai_speech.tts);
        assert!(openai_speech.streaming);
        assert!(!openai_speech.character_detection);
        assert_eq!(openai_speech.temperature, "unsupported");
        assert!(
            openai_speech
                .model_performance
                .iter()
                .any(|capability| capability.model == "gpt-4o-mini-tts")
        );
    }

    #[test]
    fn ollama_deletion_guards_canonicalize_latest_and_include_character_assignments() {
        let project_id = Uuid::new_v4();
        let provider_id = Uuid::new_v4();
        let other_provider_id = Uuid::new_v4();
        let character = crate::models::CharacterView {
            id: Uuid::new_v4(),
            role: audiobookai_core::CharacterRole::Character,
            canonical_name: "Character".to_owned(),
            aliases: Vec::new(),
            confidence: 1.0,
            dialogue_count: 1,
            voice_assignment: Some(VoiceAssignmentView {
                provider_profile_id: provider_id,
                provider_name: "Ollama".to_owned(),
                voice_id: Uuid::new_v4(),
                voice_name: "Voice".to_owned(),
                model: Some("gemma3:latest".to_owned()),
                performance: audiobookai_core::PerformanceSettings::default(),
                timing: audiobookai_core::TimingSettings::default(),
            }),
            evidence: Vec::new(),
        };
        let characters = HashMap::from([(project_id, vec![character])]);

        assert!(character_assignments_reference_provider_model(
            &characters,
            provider_id,
            &ProviderKindView::Ollama,
            "gemma3"
        ));
        assert!(!character_assignments_reference_provider_model(
            &characters,
            other_provider_id,
            &ProviderKindView::Ollama,
            "gemma3"
        ));
        assert!(payload_references_provider_model(
            &serde_json::json!({"settings": {"model": "gemma3:latest"}}),
            &ProviderKindView::Ollama,
            "gemma3"
        ));
        assert!(!payload_references_provider_model(
            &serde_json::json!({"settings": {"model": "gemma3:v2"}}),
            &ProviderKindView::Ollama,
            "gemma3"
        ));
        assert!(provider_models_equal(
            &ProviderKindView::Localai,
            "localai@voice-model",
            "voice-model"
        ));
    }

    #[tokio::test]
    #[allow(clippy::too_many_lines)]
    async fn provider_model_removal_detects_durable_assignment_missing_from_catalog() {
        let directory = tempfile::tempdir().expect("tempdir");
        let database = audiobookai_storage::Database::open_in(directory.path())
            .await
            .expect("database");
        let state = AppState::new(
            crate::ServiceConfig {
                bind: "127.0.0.1:0".parse().expect("address"),
                data_dir: directory.path().to_path_buf(),
                bundled_sidecar_dir: None,
                tls: None,
                lan_hostnames: Vec::new(),
                allow_insecure_lan: false,
                desktop_bootstrap: false,
            },
            database,
        )
        .await
        .expect("state");
        let provider_id = Uuid::new_v4();
        let provider = ProviderProfileView {
            id: provider_id,
            name: "Ollama fixture".to_owned(),
            kind: ProviderKindView::Ollama,
            mode: ProviderModeView::ExternalEndpoint,
            endpoint: Some("http://127.0.0.1:11434/".to_owned()),
            executable_path: None,
            working_directory: None,
            arguments: Vec::new(),
            status: ProviderStatusView::Offline,
            model: None,
            credential_configured: false,
            capabilities: Some(default_capabilities(
                &ProviderKindView::Ollama,
                ProviderModeView::ExternalEndpoint,
            )),
            capability_source: Some("test".to_owned()),
            capability_updated_at: Some(Utc::now()),
            last_error: None,
        };
        persist_provider(&state, &provider, None)
            .await
            .expect("persist provider");
        state
            .catalog
            .write()
            .await
            .providers
            .insert(provider_id, provider);

        let book_id = Uuid::new_v4();
        let project_id = Uuid::new_v4();
        let voice_id = Uuid::new_v4();
        let assignment_id = Uuid::new_v4();
        let now_at = Utc::now();
        let now = now_at.to_rfc3339();
        sqlx::query(
            "INSERT INTO books (id, managed_epub_path, source_hash, imported_at, payload) \
             VALUES (?, ?, ?, ?, ?)",
        )
        .bind(book_id.to_string())
        .bind(
            directory
                .path()
                .join("fixture.epub")
                .to_string_lossy()
                .into_owned(),
        )
        .bind("fixture-hash")
        .bind(&now)
        .bind("{}")
        .execute(state.database.pool())
        .await
        .expect("book row");
        sqlx::query(
            "INSERT INTO projects \
             (id, book_id, name, status, created_at, updated_at, revision, payload) \
             VALUES (?, ?, ?, ?, ?, ?, 0, ?)",
        )
        .bind(project_id.to_string())
        .bind(book_id.to_string())
        .bind("Fixture")
        .bind("ready")
        .bind(&now)
        .bind(&now)
        .bind("{}")
        .execute(state.database.pool())
        .await
        .expect("project row");
        sqlx::query(
            "INSERT INTO voice_profiles \
             (id, provider_id, name, origin, ownership, provider_voice_id, updated_at, payload) \
             VALUES (?, ?, ?, ?, ?, ?, ?, ?)",
        )
        .bind(voice_id.to_string())
        .bind(provider_id.to_string())
        .bind("Fixture voice")
        .bind("provider_catalog")
        .bind("provider")
        .bind("fixture-voice")
        .bind(&now)
        .bind("{}")
        .execute(state.database.pool())
        .await
        .expect("voice row");
        let durable_assignment = audiobookai_core::VoiceAssignment {
            id: audiobookai_core::VoiceAssignmentId::from_uuid(assignment_id),
            project_id: audiobookai_core::ProjectId::from_uuid(project_id),
            speaker: audiobookai_core::Speaker::Narrator,
            voice_profile_id: audiobookai_core::VoiceProfileId::from_uuid(voice_id),
            provider_profile_id: audiobookai_core::ProviderProfileId::from_uuid(provider_id),
            model: Some("gemma3:latest".to_owned()),
            performance: audiobookai_core::PerformanceSettings::default(),
            timing: audiobookai_core::TimingSettings::default(),
            settings: BTreeMap::new(),
            created_at: now_at,
            updated_at: now_at,
        };
        sqlx::query(
            "INSERT INTO voice_assignments \
             (id, project_id, provider_id, voice_profile_id, speaker_key, updated_at, payload) \
             VALUES (?, ?, ?, ?, ?, ?, ?)",
        )
        .bind(assignment_id.to_string())
        .bind(project_id.to_string())
        .bind(provider_id.to_string())
        .bind(voice_id.to_string())
        .bind("narrator")
        .bind(&now)
        .bind(serde_json::to_string(&durable_assignment).expect("assignment payload"))
        .execute(state.database.pool())
        .await
        .expect("assignment row");

        assert!(
            provider_model_is_in_use(&state, provider_id, "gemma3")
                .await
                .expect("in-use check")
        );
        sqlx::query("UPDATE voice_assignments SET payload = ? WHERE id = ?")
            .bind("{}")
            .bind(assignment_id.to_string())
            .execute(state.database.pool())
            .await
            .expect("corrupt assignment payload");
        assert!(
            provider_model_is_in_use(&state, provider_id, "gemma3")
                .await
                .is_err(),
            "unverifiable durable assignment metadata must block deletion"
        );
    }

    #[tokio::test]
    async fn model_lifecycle_lock_serializes_delete_and_reference_creation_windows() {
        let directory = tempfile::tempdir().expect("tempdir");
        let database = audiobookai_storage::Database::open_in(directory.path())
            .await
            .expect("database");
        let state = AppState::new(
            crate::ServiceConfig {
                bind: "127.0.0.1:0".parse().expect("address"),
                data_dir: directory.path().to_path_buf(),
                bundled_sidecar_dir: None,
                tls: None,
                lan_hostnames: Vec::new(),
                allow_insecure_lan: false,
                desktop_bootstrap: false,
            },
            database,
        )
        .await
        .expect("state");
        let deletion_guard = state.model_lifecycle.lock().await;
        let contender = Arc::clone(&state.model_lifecycle);
        let (acquired_tx, mut acquired_rx) = tokio::sync::oneshot::channel();
        let waiter = tokio::spawn(async move {
            let _reference_guard = contender.lock().await;
            let _ = acquired_tx.send(());
        });

        assert!(
            tokio::time::timeout(Duration::from_millis(25), &mut acquired_rx)
                .await
                .is_err(),
            "reference creation must wait for the delete window"
        );
        drop(deletion_guard);
        tokio::time::timeout(Duration::from_secs(1), acquired_rx)
            .await
            .expect("reference lock acquisition timed out")
            .expect("reference waiter stopped");
        waiter.await.expect("reference waiter");
    }

    #[tokio::test]
    async fn runtime_affecting_provider_patch_waits_for_dispatch_lifecycle() {
        let directory = tempfile::tempdir().expect("tempdir");
        let database = audiobookai_storage::Database::open_in(directory.path())
            .await
            .expect("database");
        let state = Arc::new(
            AppState::new(
                crate::ServiceConfig {
                    bind: "127.0.0.1:0".parse().expect("address"),
                    data_dir: directory.path().to_path_buf(),
                    bundled_sidecar_dir: None,
                    tls: None,
                    lan_hostnames: Vec::new(),
                    allow_insecure_lan: false,
                    desktop_bootstrap: false,
                },
                database,
            )
            .await
            .expect("state"),
        );
        let lifecycle_guard = state.model_lifecycle.lock().await;
        let contender = Arc::clone(&state);
        let missing_provider_id = Uuid::new_v4();
        let mut patch = tokio::spawn(async move {
            update_provider(
                State(contender),
                Path(missing_provider_id),
                Json(ProviderProfileInput {
                    name: None,
                    kind: None,
                    mode: None,
                    endpoint: Some(Some("http://127.0.0.1:9999/".to_owned())),
                    executable_path: None,
                    working_directory: None,
                    arguments: None,
                    model: None,
                    credential: None,
                }),
            )
            .await
        });

        assert!(
            tokio::time::timeout(Duration::from_millis(25), &mut patch)
                .await
                .is_err(),
            "provider routing changes must wait for the dispatch validation window"
        );
        drop(lifecycle_guard);
        assert!(matches!(
            tokio::time::timeout(Duration::from_secs(1), patch)
                .await
                .expect("provider patch timed out")
                .expect("provider patch task"),
            Err(ServiceError::NotFound)
        ));
    }

    #[tokio::test]
    async fn consent_revocation_waits_for_an_in_flight_dispatch_boundary() {
        let directory = tempfile::tempdir().expect("tempdir");
        let database = audiobookai_storage::Database::open_in(directory.path())
            .await
            .expect("database");
        let state = Arc::new(
            AppState::new(
                crate::ServiceConfig {
                    bind: "127.0.0.1:0".parse().expect("address"),
                    data_dir: directory.path().to_path_buf(),
                    bundled_sidecar_dir: None,
                    tls: None,
                    lan_hostnames: Vec::new(),
                    allow_insecure_lan: false,
                    desktop_bootstrap: false,
                },
                database,
            )
            .await
            .expect("state"),
        );
        let project_id = Uuid::new_v4();
        let lifecycle = state.dispatch_consent_lifecycle_lock(project_id).await;
        let dispatch_guard = lifecycle.read().await;
        let contender = Arc::clone(&state);
        let mut revocation = tokio::spawn(async move {
            update_project(
                State(contender),
                Path(project_id),
                Json(serde_json::json!({ "consentCloudText": false })),
            )
            .await
        });

        assert!(
            tokio::time::timeout(Duration::from_millis(25), &mut revocation)
                .await
                .is_err(),
            "consent revocation must wait until the provider dispatch returns"
        );
        drop(dispatch_guard);
        assert!(matches!(
            tokio::time::timeout(Duration::from_secs(1), revocation)
                .await
                .expect("consent revocation timed out")
                .expect("consent revocation task"),
            Err(ServiceError::NotFound)
        ));
    }

    #[test]
    fn owner_audio_and_job_defaults_are_validated_and_applied() {
        let mut settings = AppSettingsView::defaults(std::path::Path::new("/managed"));
        apply_owner_settings_patch(
            &mut settings,
            &serde_json::json!({
                "cacheLimitBytes": 12_000_000_000_u64,
                "defaultConcurrency": 7,
                "defaultRetryCount": 0,
                "defaultLufs": -18.5,
                "defaultTruePeakDb": -2.5,
            }),
        )
        .expect("valid settings patch");

        assert_eq!(settings.cache_limit_bytes, 12_000_000_000);
        assert_eq!(settings.default_concurrency, 7);
        assert_eq!(settings.default_retry_count, 0);
        assert!((settings.default_lufs - -18.5).abs() < f32::EPSILON);
        assert!((settings.default_true_peak_db - -2.5).abs() < f32::EPSILON);

        let project_settings = imported_project_settings(&settings, "{title}".to_owned());
        assert_eq!(project_settings.global_chapter_concurrency, 7);
        assert_eq!(project_settings.reliability.max_transient_retries, 0);
    }

    #[test]
    fn owner_settings_reject_unsafe_ranges_and_managed_path_changes() {
        for invalid in [
            serde_json::json!({"cacheLimitBytes": 999_999_999_u64}),
            serde_json::json!({"defaultConcurrency": 0}),
            serde_json::json!({"defaultConcurrency": 33}),
            serde_json::json!({"defaultRetryCount": 11}),
            serde_json::json!({"defaultLufs": -30.5}),
            serde_json::json!({"defaultLufs": -9.5}),
            serde_json::json!({"defaultTruePeakDb": -10.5}),
            serde_json::json!({"defaultTruePeakDb": 0.5}),
            serde_json::json!({"libraryPath": "/tmp/moved-library"}),
            serde_json::json!({"cachePath": "/tmp/moved-cache"}),
        ] {
            let mut settings = AppSettingsView::defaults(std::path::Path::new("/managed"));
            assert!(
                apply_owner_settings_patch(&mut settings, &invalid).is_err(),
                "patch should be rejected: {invalid}"
            );
        }
    }

    #[test]
    fn mlx_runtime_uninstall_requires_explicit_confirmation() {
        assert!(require_mlx_uninstall_confirmation(false).is_err());
        assert!(require_mlx_uninstall_confirmation(true).is_ok());
    }

    #[tokio::test]
    async fn mlx_model_removal_detects_character_specific_assignment() {
        let directory = tempfile::tempdir().expect("tempdir");
        let database = audiobookai_storage::Database::open_in(directory.path())
            .await
            .expect("database");
        let state = AppState::new(
            crate::ServiceConfig {
                bind: "127.0.0.1:0".parse().expect("address"),
                data_dir: directory.path().to_path_buf(),
                bundled_sidecar_dir: None,
                tls: None,
                lan_hostnames: Vec::new(),
                allow_insecure_lan: false,
                desktop_bootstrap: false,
            },
            database,
        )
        .await
        .expect("state");
        let provider_id = Uuid::new_v4();
        let model_path = directory
            .path()
            .join("managed-providers/mlx-audio/models")
            .join(Uuid::new_v4().to_string())
            .join("payload");
        tokio::fs::create_dir_all(&model_path)
            .await
            .expect("model payload");
        let model_alias = model_path.join("..").join("payload");
        let provider = ProviderProfileView {
            id: provider_id,
            name: "MLX fixture".to_owned(),
            kind: ProviderKindView::MlxAudio,
            mode: ProviderModeView::ManagedChild,
            endpoint: Some("http://127.0.0.1:8000/".to_owned()),
            executable_path: Some("/app-owned/mlx_audio.server".to_owned()),
            working_directory: Some("/app-owned".to_owned()),
            arguments: Vec::new(),
            status: ProviderStatusView::Offline,
            model: None,
            credential_configured: false,
            capabilities: None,
            capability_source: None,
            capability_updated_at: None,
            last_error: None,
        };
        let project_id = Uuid::new_v4();
        {
            let mut catalog = state.catalog.write().await;
            catalog.providers.insert(provider_id, provider);
            catalog.characters.insert(
                project_id,
                vec![crate::models::CharacterView {
                    id: Uuid::new_v4(),
                    role: audiobookai_core::CharacterRole::Character,
                    canonical_name: "Character".to_owned(),
                    aliases: Vec::new(),
                    confidence: 1.0,
                    dialogue_count: 1,
                    voice_assignment: Some(VoiceAssignmentView {
                        provider_profile_id: provider_id,
                        provider_name: "MLX fixture".to_owned(),
                        voice_id: Uuid::new_v4(),
                        voice_name: "Voice".to_owned(),
                        model: Some(model_alias.to_string_lossy().into_owned()),
                        performance: audiobookai_core::PerformanceSettings::default(),
                        timing: audiobookai_core::TimingSettings::default(),
                    }),
                    evidence: Vec::new(),
                }],
            );
        }

        assert!(
            mlx_model_is_in_use(&state, &model_path)
                .await
                .expect("in-use check")
        );
    }

    #[tokio::test]
    async fn mlx_model_removal_payload_checks_are_canonical_recursive_and_fail_closed() {
        let directory = tempfile::tempdir().expect("tempdir");
        let database = audiobookai_storage::Database::open_in(directory.path())
            .await
            .expect("database");
        let state = AppState::new(
            crate::ServiceConfig {
                bind: "127.0.0.1:0".parse().expect("address"),
                data_dir: directory.path().to_path_buf(),
                bundled_sidecar_dir: None,
                tls: None,
                lan_hostnames: Vec::new(),
                allow_insecure_lan: false,
                desktop_bootstrap: false,
            },
            database,
        )
        .await
        .expect("state");
        let model_path = directory
            .path()
            .join("managed-providers/mlx-audio/models")
            .join(Uuid::new_v4().to_string())
            .join("payload");
        tokio::fs::create_dir_all(&model_path)
            .await
            .expect("model payload");
        let canonical = canonical_mlx_model_path(&state, &model_path)
            .await
            .expect("canonical model");
        let alias = model_path.join("..").join("payload");
        let nested = serde_json::json!({"segment": {"settings": {"model": alias}}});
        assert!(
            payload_references_model_path(&nested, &canonical)
                .await
                .expect("payload check")
        );
        assert!(
            json_payloads_reference_model_path(vec!["not valid JSON".to_owned()], &canonical)
                .await
                .is_err()
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn mlx_model_path_comparison_resolves_symlink_aliases_but_contains_targets() {
        use std::os::unix::fs::symlink;

        let directory = tempfile::tempdir().expect("tempdir");
        let database = audiobookai_storage::Database::open_in(directory.path())
            .await
            .expect("database");
        let state = AppState::new(
            crate::ServiceConfig {
                bind: "127.0.0.1:0".parse().expect("address"),
                data_dir: directory.path().to_path_buf(),
                bundled_sidecar_dir: None,
                tls: None,
                lan_hostnames: Vec::new(),
                allow_insecure_lan: false,
                desktop_bootstrap: false,
            },
            database,
        )
        .await
        .expect("state");
        let model_path = directory
            .path()
            .join("managed-providers/mlx-audio/models")
            .join(Uuid::new_v4().to_string())
            .join("payload");
        tokio::fs::create_dir_all(&model_path)
            .await
            .expect("model payload");
        let alias = directory.path().join("model-alias");
        symlink(&model_path, &alias).expect("symlink alias");
        let canonical = canonical_mlx_model_path(&state, &model_path)
            .await
            .expect("canonical model");

        assert!(
            mlx_model_path_matches(alias.to_str().expect("UTF-8 alias"), &canonical)
                .await
                .expect("alias check")
        );
        assert!(
            canonical_mlx_model_path(&state, directory.path())
                .await
                .is_err(),
            "a deletion target outside managed storage must fail closed"
        );
    }
}
