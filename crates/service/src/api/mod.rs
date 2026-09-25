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
        ProviderProfileView, ProviderRoleView, ProviderStatusView, ReviewStatus, StartJobInput,
        UsageSummaryView, VoiceAssignmentView, VoiceView,
    },
    state::ImportRecord,
};

mod accounting;
mod characters;
mod jobs;
mod managed_runtimes;
mod preflight;
mod projects;
mod pronunciation;
mod provider_config;
mod providers;
mod settings;
#[cfg(test)]
mod tests;
mod voices;

// Handlers and helpers are grouped by domain; the router and the shared request helpers stay here.
// Items used elsewhere in the crate are re-exported so `crate::api::…` paths remain stable.
use self::{
    accounting::{
        create_budget, create_rate_card, delete_budget, delete_rate_card, list_budgets,
        list_rate_cards, usage_summary,
    },
    characters::{
        advance_character_revision_tx, approve_character_review, character_detection_status,
        create_character, delete_character, delete_speaker_override, list_characters,
        merge_character, start_character_detection, sync_character_review_catalog,
        update_character, upsert_speaker_override,
    },
    jobs::{
        artifact_download, get_job, job_action, job_events, job_playback, list_exports, list_jobs,
        start_job,
    },
    managed_runtimes::{
        cancel_mlx_operation, cancel_piper_operation, download_mlx_model, download_piper_voice,
        install_mlx_audio, install_piper, mlx_management_status, piper_management_status,
        remove_mlx_model, remove_piper_voice, uninstall_mlx_audio, uninstall_piper,
    },
    preflight::{
        dry_run_project, extend_dry_run_environment, preflight_dry_run, preflight_estimate,
        preflight_preview, voice_auditions,
    },
    settings::{
        complete_first_run, create_lan_token, get_settings, imported_project_settings,
        list_lan_tokens, lock_secret_store, revoke_lan_sessions, revoke_lan_token, secret_status,
        set_lan_password, unlock_secret_store, update_settings,
    },
};
pub(crate) use self::{projects::*, pronunciation::*, provider_config::*, providers::*, voices::*};

/// Sending book text to a cloud provider needs the project's explicit consent. The stable code
/// lets the dashboard offer that consent directly instead of only showing the message.
pub(crate) fn cloud_text_consent_required(provider_name: &str) -> ServiceError {
    ServiceError::ConflictDetails {
        code: "cloud_text_consent_required",
        detail: format!(
            "grant this project permission to send book text to the cloud provider {provider_name}"
        ),
        meta: serde_json::json!({ "providerName": provider_name }),
    }
}

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
            "/api/v1/providers/native/availability",
            get(native_provider_availability),
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
            "/api/v1/providers/piper/management",
            get(piper_management_status),
        )
        .route("/api/v1/providers/piper/install", post(install_piper))
        .route("/api/v1/providers/piper/uninstall", post(uninstall_piper))
        .route(
            "/api/v1/providers/piper/operations/{id}/cancel",
            post(cancel_piper_operation),
        )
        .route("/api/v1/providers/piper/voices", post(download_piper_voice))
        .route(
            "/api/v1/providers/piper/voices/{voice_id}",
            delete(remove_piper_voice),
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

async fn native_provider_availability(State(state): State<Arc<AppState>>) -> Response {
    let mut response = Json(crate::state::native_tts_availability(&state.config)).into_response();
    response.headers_mut().insert(
        header::CACHE_CONTROL,
        http::HeaderValue::from_static("no-store"),
    );
    response
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
