use std::{
    collections::BTreeMap,
    sync::{Arc, OnceLock},
};

use audiobookai_core::{
    CharacterRole, JobState, PerformanceSettings, ProductionSegment, ProjectId, ProofingPlan,
    SegmentId, SegmentReviewState, SegmentSelection, SegmentTake, SegmentTakeId, Speaker,
    TimingSettings,
};
use axum::{
    Json, Router,
    extract::{Path, Query, State},
    routing::{get, patch, post, put},
};
use chrono::{DateTime, Duration as ChronoDuration, Utc};
use rand::RngCore;
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::{AppState, ServiceError, models::PronunciationRuleView};

static ESTIMATE_SIGNING_KEY: OnceLock<[u8; 32]> = OnceLock::new();

pub(crate) fn routes() -> Router<Arc<AppState>> {
    Router::new()
        .route("/api/v1/projects/{id}/proofing", get(get_summary))
        .route(
            "/api/v1/projects/{id}/proofing/segments",
            get(list_segments),
        )
        .route(
            "/api/v1/projects/{project_id}/proofing/segments/{segment_id}",
            patch(update_segment),
        )
        .route(
            "/api/v1/projects/{project_id}/proofing/segments/{segment_id}/review",
            put(update_review),
        )
        .route(
            "/api/v1/projects/{project_id}/proofing/segments/{segment_id}/takes",
            get(list_takes),
        )
        .route(
            "/api/v1/projects/{project_id}/proofing/segments/{segment_id}/selection",
            put(select_take),
        )
        .route(
            "/api/v1/projects/{project_id}/proofing/segments/{segment_id}/regeneration-estimate",
            post(regeneration_estimate),
        )
        .route(
            "/api/v1/projects/{project_id}/proofing/segments/{segment_id}/regenerations",
            post(start_regeneration),
        )
        .route(
            "/api/v1/projects/{project_id}/proofing/exports",
            post(start_proof_export),
        )
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct ProofingCountsView {
    total: usize,
    unreviewed: usize,
    flagged: usize,
    approved: usize,
    locked: usize,
    stale: usize,
    missing: usize,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct ProofingChapterView {
    id: Uuid,
    title: String,
    total: usize,
    issue_count: usize,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
#[allow(clippy::struct_excessive_bools)]
pub(crate) struct ProofingSummaryView {
    available: bool,
    requires_new_conversion: bool,
    plan: Option<ProofingPlan>,
    counts: ProofingCountsView,
    chapters: Vec<ProofingChapterView>,
    retailer_export_ready: bool,
    generic_export_ready: bool,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct ProofingSegmentView {
    segment: ProductionSegment,
    selection: Option<SegmentSelection>,
    selected_take: Option<SegmentTake>,
    take_count: usize,
    selected_take_current: bool,
    audio_url: Option<String>,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct ProofingSegmentPage {
    items: Vec<ProofingSegmentView>,
    total: usize,
    next_cursor: Option<String>,
}

#[derive(Clone, Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
struct SegmentQuery {
    chapter_id: Option<Uuid>,
    state: Option<SegmentReviewState>,
    issues_only: Option<bool>,
    stale_only: Option<bool>,
    search: Option<String>,
    cursor: Option<usize>,
    limit: Option<usize>,
}

async fn get_summary(
    State(state): State<Arc<AppState>>,
    Path(project_id): Path<Uuid>,
) -> Result<Json<ProofingSummaryView>, ServiceError> {
    ensure_project_exists(&state, project_id).await?;
    let repository = state.database.repositories().proofing;
    let plan = repository
        .get_plan(ProjectId::from_uuid(project_id))
        .await
        .map_err(storage_error)?;
    let Some(plan) = plan else {
        return Ok(Json(ProofingSummaryView {
            available: false,
            requires_new_conversion: true,
            plan: None,
            counts: empty_counts(),
            chapters: Vec::new(),
            retailer_export_ready: false,
            generic_export_ready: false,
        }));
    };
    let segments = repository
        .list_active_segments(ProjectId::from_uuid(project_id), None)
        .await
        .map_err(storage_error)?;
    let (counts, views) = analyze_segments(&state, &repository, &segments).await?;
    let project = state
        .catalog
        .read()
        .await
        .projects
        .get(&project_id)
        .cloned()
        .ok_or(ServiceError::NotFound)?;
    let mut chapter_counts = BTreeMap::<Uuid, (usize, usize)>::new();
    for view in &views {
        if let Some(chapter_id) = view.segment.chapter_id {
            let entry = chapter_counts.entry(chapter_id.as_uuid()).or_default();
            entry.0 = entry.0.saturating_add(1);
            if !view.selected_take_current
                || view.selected_take.is_none()
                || !view
                    .selected_take
                    .as_ref()
                    .is_none_or(|take| take.findings.is_empty())
            {
                entry.1 = entry.1.saturating_add(1);
            }
        }
    }
    let chapters = project
        .chapters
        .into_iter()
        .filter_map(|chapter| {
            chapter_counts
                .remove(&chapter.id)
                .map(|(total, issue_count)| ProofingChapterView {
                    id: chapter.id,
                    title: chapter.title,
                    total,
                    issue_count,
                })
        })
        .collect::<Vec<_>>();
    let no_hard_blockers = counts.flagged == 0 && counts.stale == 0 && counts.missing == 0;
    let retailer_export_ready = no_hard_blockers
        && counts.unreviewed == 0
        && segments
            .iter()
            .all(|segment| segment.review_state.is_accepted())
        && matches!(plan.status, audiobookai_core::ProofingPlanStatus::Ready);
    let generic_export_ready =
        no_hard_blockers && matches!(plan.status, audiobookai_core::ProofingPlanStatus::Ready);
    Ok(Json(ProofingSummaryView {
        available: true,
        requires_new_conversion: false,
        plan: Some(plan),
        counts,
        chapters,
        retailer_export_ready,
        generic_export_ready,
    }))
}

async fn list_segments(
    State(state): State<Arc<AppState>>,
    Path(project_id): Path<Uuid>,
    Query(query): Query<SegmentQuery>,
) -> Result<Json<ProofingSegmentPage>, ServiceError> {
    ensure_project_exists(&state, project_id).await?;
    let repository = state.database.repositories().proofing;
    repository
        .get_plan(ProjectId::from_uuid(project_id))
        .await
        .map_err(storage_error)?
        .ok_or_else(|| {
            conflict(
                "proofing_unavailable",
                "start a new conversion to create a proofing plan",
            )
        })?;
    let chapter_id = query.chapter_id.map(audiobookai_core::ChapterId::from_uuid);
    let segments = repository
        .list_active_segments(ProjectId::from_uuid(project_id), chapter_id)
        .await
        .map_err(storage_error)?;
    let (_, mut items) = analyze_segments(&state, &repository, &segments).await?;
    if let Some(state) = query.state {
        items.retain(|view| view.segment.review_state == state);
    }
    if query.issues_only.unwrap_or(false) {
        items.retain(|view| {
            view.selected_take
                .as_ref()
                .is_some_and(|take| !take.findings.is_empty())
        });
    }
    if query.stale_only.unwrap_or(false) {
        items.retain(|view| !view.selected_take_current);
    }
    if let Some(search) = query
        .search
        .as_deref()
        .map(str::trim)
        .filter(|v| !v.is_empty())
    {
        let search = search.to_lowercase();
        items.retain(|view| {
            view.segment.original_text.to_lowercase().contains(&search)
                || view.segment.effective_text.to_lowercase().contains(&search)
        });
    }
    let total = items.len();
    let start = query.cursor.unwrap_or_default().min(total);
    let limit = query.limit.unwrap_or(100).clamp(1, 250);
    let end = start.saturating_add(limit).min(total);
    let next_cursor = (end < total).then(|| end.to_string());
    Ok(Json(ProofingSegmentPage {
        items: items[start..end].to_vec(),
        total,
        next_cursor,
    }))
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct SegmentUpdateInput {
    expected_revision: u64,
    text_override: Option<String>,
    #[serde(default)]
    clear_text_override: bool,
    performance_override: Option<PerformanceSettings>,
    timing_override: Option<TimingSettings>,
}

#[allow(clippy::too_many_lines)]
async fn update_segment(
    State(state): State<Arc<AppState>>,
    Path((project_id, segment_id)): Path<(Uuid, Uuid)>,
    Json(input): Json<SegmentUpdateInput>,
) -> Result<Json<ProofingSegmentView>, ServiceError> {
    let _model_lifecycle_guard = state.model_lifecycle.lock().await;
    let project_lock = state.character_lifecycle_lock(project_id).await;
    let _project_guard = project_lock.lock().await;
    let mut plan = ensure_proof_mutable(&state, project_id).await?;
    ensure_no_active_proof_export(&state, project_id).await?;
    ensure_no_active_regeneration(&state, SegmentId::from_uuid(segment_id)).await?;
    let repository = state.database.repositories().proofing;
    let mut segment = owned_segment(&repository, project_id, segment_id).await?;
    if segment.revision != input.expected_revision {
        return Err(stale_segment(segment.id, segment.revision));
    }
    if segment.review_state == SegmentReviewState::Locked {
        return Err(conflict(
            "segment_locked",
            "unlock this segment before editing it",
        ));
    }
    if !input.clear_text_override
        && input.text_override.is_none()
        && input.performance_override.is_none()
        && input.timing_override.is_none()
    {
        return Err(ServiceError::InvalidRequest(
            "provide at least one segment override to update".to_owned(),
        ));
    }
    if input.clear_text_override && input.text_override.is_some() {
        return Err(ServiceError::InvalidRequest(
            "textOverride and clearTextOverride cannot be used together".to_owned(),
        ));
    }
    if input.clear_text_override {
        segment.narration_text_override = None;
    } else if let Some(value) = input.text_override {
        let value = value.trim().to_owned();
        if value.is_empty() {
            return Err(ServiceError::InvalidRequest(
                "use clearTextOverride to remove a narration override".to_owned(),
            ));
        }
        if value.chars().count() > 20_000 {
            return Err(ServiceError::InvalidRequest(
                "segment narration text exceeds 20,000 characters".to_owned(),
            ));
        }
        segment.narration_text_override = Some(value);
    }
    if let Some(performance) = input.performance_override {
        segment.performance_override = performance;
    }
    if let Some(timing) = input.timing_override {
        segment.timing_override = timing;
    }
    let base_text = segment
        .narration_text_override
        .as_deref()
        .unwrap_or(&segment.original_text);
    let (character_id, assignment, rules, language) =
        synthesis_context(&state, project_id, &segment).await?;
    let effective_performance = assignment
        .performance
        .overlay(&segment.performance_override);
    let provider = state
        .catalog
        .read()
        .await
        .providers
        .get(&assignment.provider_profile_id)
        .cloned()
        .ok_or_else(|| conflict("provider_missing", "the assigned provider is unavailable"))?;
    let effective_model = assignment.model.as_deref().or(provider.model.as_deref());
    let effective_timing = TimingSettings {
        pause_before_ms: segment
            .timing_override
            .pause_before_ms
            .or(assignment.timing.pause_before_ms),
        pause_after_ms: segment
            .timing_override
            .pause_after_ms
            .or(assignment.timing.pause_after_ms),
    };
    crate::api::validate_voice_direction(
        &effective_performance,
        &effective_timing,
        effective_model,
        provider.capabilities.as_ref(),
    )?;
    let (effective_text, _, dictionary_revision) = crate::conversion::apply_pronunciation_rules(
        base_text,
        &rules,
        project_id,
        character_id,
        language.as_deref(),
    )?;
    segment.effective_text = effective_text;
    segment.expected_input_hash = crate::conversion::semantic_input_hash(
        &segment.effective_text,
        segment.context_before.as_deref(),
        assignment.provider_profile_id,
        effective_model,
        assignment.voice_id,
        &dictionary_revision,
        &effective_performance,
    )?;
    segment.review_state = SegmentReviewState::Unreviewed;
    segment.updated_at = Utc::now();
    let mut all = repository
        .list_active_segments(ProjectId::from_uuid(project_id), None)
        .await
        .map_err(storage_error)?;
    if let Some(value) = all.iter_mut().find(|value| value.id == segment.id) {
        *value = segment.clone();
    }
    let expected_plan_revision = plan.plan_revision;
    plan.plan_revision = plan.plan_revision.saturating_add(1);
    plan.plan_hash = plan_hash(&all);
    plan.updated_at = Utc::now();
    let stored = repository
        .update_segment_and_plan(
            &segment,
            input.expected_revision,
            &plan,
            expected_plan_revision,
        )
        .await
        .map_err(map_storage_conflict)?;
    state.events.publish(
        "proofing.segment.updated",
        serde_json::json!({"projectId": project_id, "segmentId": segment_id}),
    );
    segment_view(&state, &repository, stored).await.map(Json)
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ReviewInput {
    state: SegmentReviewState,
    expected_revision: u64,
}

async fn update_review(
    State(state): State<Arc<AppState>>,
    Path((project_id, segment_id)): Path<(Uuid, Uuid)>,
    Json(input): Json<ReviewInput>,
) -> Result<Json<ProofingSegmentView>, ServiceError> {
    let project_lock = state.character_lifecycle_lock(project_id).await;
    let _project_guard = project_lock.lock().await;
    ensure_proof_mutable(&state, project_id).await?;
    ensure_no_active_proof_export(&state, project_id).await?;
    ensure_no_active_regeneration(&state, SegmentId::from_uuid(segment_id)).await?;
    let repository = state.database.repositories().proofing;
    let mut segment = owned_segment(&repository, project_id, segment_id).await?;
    if segment.revision != input.expected_revision {
        return Err(stale_segment(segment.id, segment.revision));
    }
    if segment.review_state == SegmentReviewState::Locked
        && input.state != SegmentReviewState::Approved
        && input.state != SegmentReviewState::Locked
    {
        return Err(conflict(
            "segment_locked",
            "unlock this segment before changing review state",
        ));
    }
    if input.state.is_accepted() {
        let selection = repository
            .get_selection(segment.id)
            .await
            .map_err(storage_error)?
            .ok_or_else(|| conflict("take_missing", "select a usable take before approving"))?;
        let take = repository
            .get_take(selection.take_id)
            .await
            .map_err(storage_error)?
            .ok_or_else(|| conflict("take_missing", "the selected take is unavailable"))?;
        if take.semantic_input_hash != segment.expected_input_hash {
            return Err(conflict(
                "take_stale",
                "regenerate or select a take matching the current narration inputs",
            ));
        }
    }
    segment.review_state = input.state;
    segment.updated_at = Utc::now();
    let stored = repository
        .update_segment(&segment, input.expected_revision)
        .await
        .map_err(map_storage_conflict)?;
    state.events.publish(
        "proofing.segment.reviewed",
        serde_json::json!({"projectId": project_id, "segmentId": segment_id, "state": input.state}),
    );
    segment_view(&state, &repository, stored).await.map(Json)
}

async fn list_takes(
    State(state): State<Arc<AppState>>,
    Path((project_id, segment_id)): Path<(Uuid, Uuid)>,
) -> Result<Json<Vec<SegmentTake>>, ServiceError> {
    let repository = state.database.repositories().proofing;
    owned_segment(&repository, project_id, segment_id).await?;
    Ok(Json(
        repository
            .list_takes(SegmentId::from_uuid(segment_id))
            .await
            .map_err(storage_error)?,
    ))
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct SelectionInput {
    take_id: Uuid,
    expected_revision: u64,
    expected_segment_revision: u64,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct RegenerationEstimateView {
    segment_id: Uuid,
    segment_revision: u64,
    provider_profile_id: Uuid,
    provider_name: String,
    model: Option<String>,
    characters: u64,
    monetary_cost_micros: Option<i64>,
    currency: Option<String>,
    credits: Option<i64>,
    unknown_pricing: bool,
    estimate_token: String,
    expires_at: DateTime<Utc>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct RegenerationEstimateInput {
    expected_segment_revision: u64,
}

async fn regeneration_estimate(
    State(state): State<Arc<AppState>>,
    Path((project_id, segment_id)): Path<(Uuid, Uuid)>,
    Json(input): Json<RegenerationEstimateInput>,
) -> Result<Json<RegenerationEstimateView>, ServiceError> {
    let _model_lifecycle_guard = state.model_lifecycle.lock().await;
    let project_lock = state.character_lifecycle_lock(project_id).await;
    let _project_guard = project_lock.lock().await;
    ensure_proof_mutable(&state, project_id).await?;
    ensure_no_active_proof_export(&state, project_id).await?;
    ensure_no_active_regeneration(&state, SegmentId::from_uuid(segment_id)).await?;
    let repository = state.database.repositories().proofing;
    let segment = owned_segment(&repository, project_id, segment_id).await?;
    if segment.revision != input.expected_segment_revision {
        return Err(stale_segment(segment.id, segment.revision));
    }
    if segment.review_state == SegmentReviewState::Locked {
        return Err(conflict(
            "segment_locked",
            "unlock this segment before regenerating it",
        ));
    }
    let quote = crate::conversion::quote_segment_regeneration(
        &state,
        project_id,
        SegmentId::from_uuid(segment_id),
    )
    .await?;
    let expires_at = Utc::now() + ChronoDuration::minutes(10);
    Ok(Json(regeneration_estimate_view(quote, expires_at)?))
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct StartRegenerationInput {
    expected_segment_revision: u64,
    estimate_token: String,
    #[serde(default)]
    allow_budget_override: bool,
}

async fn start_regeneration(
    State(state): State<Arc<AppState>>,
    Path((project_id, segment_id)): Path<(Uuid, Uuid)>,
    Json(input): Json<StartRegenerationInput>,
) -> Result<Json<crate::models::JobView>, ServiceError> {
    let _model_lifecycle_guard = state.model_lifecycle.lock().await;
    let project_lock = state.character_lifecycle_lock(project_id).await;
    let _project_guard = project_lock.lock().await;
    if let Some(job) = crate::api::blocking_project_job(&state, project_id, None).await {
        return Err(crate::api::active_job_conflict(&job));
    }
    ensure_proof_mutable(&state, project_id).await?;
    ensure_no_active_proof_export(&state, project_id).await?;
    ensure_no_active_regeneration(&state, SegmentId::from_uuid(segment_id)).await?;
    let expires_seconds = input
        .estimate_token
        .split_once('.')
        .and_then(|(value, _)| value.parse::<i64>().ok())
        .ok_or_else(|| {
            ServiceError::InvalidRequest("invalid regeneration estimate token".to_owned())
        })?;
    let expires_at = DateTime::from_timestamp(expires_seconds, 0).ok_or_else(|| {
        ServiceError::InvalidRequest("invalid regeneration estimate expiry".to_owned())
    })?;
    if expires_at <= Utc::now() {
        return Err(ServiceError::ConflictDetails {
            code: "estimate_expired",
            detail: "the regeneration estimate expired; request a new estimate".to_owned(),
            meta: serde_json::json!({"segmentId": segment_id}),
        });
    }
    let quote = crate::conversion::quote_segment_regeneration(
        &state,
        project_id,
        SegmentId::from_uuid(segment_id),
    )
    .await?;
    if quote.segment_revision != input.expected_segment_revision {
        return Err(stale_segment(quote.segment_id, quote.segment_revision));
    }
    let current = regeneration_estimate_view(quote, expires_at)?;
    if current.estimate_token != input.estimate_token {
        return Err(ServiceError::ConflictDetails {
            code: "estimate_changed",
            detail: "the segment inputs or pricing changed; review a fresh estimate".to_owned(),
            meta: serde_json::json!({"segmentId": segment_id}),
        });
    }
    let job = crate::conversion::start_segment_regeneration(
        Arc::clone(&state),
        project_id,
        SegmentId::from_uuid(segment_id),
        input.expected_segment_revision,
        input.allow_budget_override,
    )
    .await?;
    Ok(Json(job))
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct StartProofExportInput {
    #[serde(default)]
    strict_retailer: bool,
    #[serde(default)]
    export: crate::models::ExportOptionsInput,
}

async fn start_proof_export(
    State(state): State<Arc<AppState>>,
    Path(project_id): Path<Uuid>,
    Json(input): Json<StartProofExportInput>,
) -> Result<Json<crate::models::JobView>, ServiceError> {
    let _model_lifecycle_guard = state.model_lifecycle.lock().await;
    let project_lock = state.character_lifecycle_lock(project_id).await;
    let _project_guard = project_lock.lock().await;
    if let Some(job) = crate::api::blocking_project_job(&state, project_id, None).await {
        return Err(crate::api::active_job_conflict(&job));
    }
    ensure_proof_mutable(&state, project_id).await?;
    ensure_no_active_proof_export(&state, project_id).await?;
    crate::conversion::start_proof_export(
        Arc::clone(&state),
        project_id,
        input.export,
        input.strict_retailer,
    )
    .await
    .map(Json)
}

fn regeneration_estimate_view(
    quote: crate::conversion::RegenerationQuote,
    expires_at: DateTime<Utc>,
) -> Result<RegenerationEstimateView, ServiceError> {
    let payload = serde_json::json!({
        "segmentId": quote.segment_id,
        "segmentRevision": quote.segment_revision,
        "semanticInputHash": quote.semantic_input_hash,
        "providerProfileId": quote.provider_profile_id,
        "model": quote.model,
        "characters": quote.characters,
        "monetaryCostMicros": quote.monetary_cost_micros,
        "currency": quote.currency,
        "credits": quote.credits,
        "rateCardId": quote.rate_card_id,
        "expiresAt": expires_at.timestamp(),
    });
    let bytes =
        serde_json::to_vec(&payload).map_err(|error| ServiceError::Internal(error.to_string()))?;
    let estimate_token = format!(
        "{}.{}",
        expires_at.timestamp(),
        signed_estimate_hash(&bytes).to_hex()
    );
    Ok(RegenerationEstimateView {
        segment_id: quote.segment_id.as_uuid(),
        segment_revision: quote.segment_revision,
        provider_profile_id: quote.provider_profile_id.as_uuid(),
        provider_name: quote.provider_name,
        model: quote.model,
        characters: quote.characters,
        monetary_cost_micros: quote.monetary_cost_micros,
        currency: quote.currency,
        credits: quote.credits,
        unknown_pricing: quote.monetary_cost_micros.is_none() && quote.credits.is_none(),
        estimate_token,
        expires_at,
    })
}

fn signed_estimate_hash(payload: &[u8]) -> blake3::Hash {
    let key = ESTIMATE_SIGNING_KEY.get_or_init(|| {
        let mut key = [0_u8; 32];
        rand::rng().fill_bytes(&mut key);
        key
    });
    blake3::keyed_hash(key, payload)
}

async fn select_take(
    State(state): State<Arc<AppState>>,
    Path((project_id, segment_id)): Path<(Uuid, Uuid)>,
    Json(input): Json<SelectionInput>,
) -> Result<Json<ProofingSegmentView>, ServiceError> {
    let project_lock = state.character_lifecycle_lock(project_id).await;
    let _project_guard = project_lock.lock().await;
    ensure_proof_mutable(&state, project_id).await?;
    ensure_no_active_proof_export(&state, project_id).await?;
    ensure_no_active_regeneration(&state, SegmentId::from_uuid(segment_id)).await?;
    let repository = state.database.repositories().proofing;
    let mut segment = owned_segment(&repository, project_id, segment_id).await?;
    if segment.revision != input.expected_segment_revision {
        return Err(stale_segment(segment.id, segment.revision));
    }
    if segment.review_state == SegmentReviewState::Locked {
        return Err(conflict(
            "segment_locked",
            "unlock this segment before selecting another take",
        ));
    }
    let selection = SegmentSelection {
        segment_id: segment.id,
        take_id: SegmentTakeId::from_uuid(input.take_id),
        selected_at: Utc::now(),
        revision: input.expected_revision,
    };
    segment.review_state = SegmentReviewState::Unreviewed;
    segment.updated_at = Utc::now();
    let (_, stored) = repository
        .select_take_and_update_segment(
            &selection,
            input.expected_revision,
            &segment,
            input.expected_segment_revision,
        )
        .await
        .map_err(map_storage_conflict)?;
    state.events.publish(
        "proofing.segment.selection.updated",
        serde_json::json!({"projectId": project_id, "segmentId": segment_id, "takeId": input.take_id}),
    );
    segment_view(&state, &repository, stored).await.map(Json)
}

async fn analyze_segments(
    state: &AppState,
    repository: &audiobookai_storage::ProofingRepository,
    segments: &[ProductionSegment],
) -> Result<(ProofingCountsView, Vec<ProofingSegmentView>), ServiceError> {
    let mut counts = empty_counts();
    let mut views = Vec::with_capacity(segments.len());
    for segment in segments {
        let view = segment_view(state, repository, segment.clone()).await?;
        counts.total = counts.total.saturating_add(1);
        match segment.review_state {
            SegmentReviewState::Unreviewed => {
                counts.unreviewed = counts.unreviewed.saturating_add(1);
            }
            SegmentReviewState::Flagged => counts.flagged = counts.flagged.saturating_add(1),
            SegmentReviewState::Approved => counts.approved = counts.approved.saturating_add(1),
            SegmentReviewState::Locked => counts.locked = counts.locked.saturating_add(1),
        }
        if view.selected_take.is_none() {
            counts.missing = counts.missing.saturating_add(1);
        } else if !view.selected_take_current {
            counts.stale = counts.stale.saturating_add(1);
        }
        views.push(view);
    }
    Ok((counts, views))
}

async fn segment_view(
    state: &AppState,
    repository: &audiobookai_storage::ProofingRepository,
    segment: ProductionSegment,
) -> Result<ProofingSegmentView, ServiceError> {
    let selection = repository
        .get_selection(segment.id)
        .await
        .map_err(storage_error)?;
    let selected_take = match &selection {
        Some(selection) => repository
            .get_take(selection.take_id)
            .await
            .map_err(storage_error)?,
        None => None,
    };
    let take_count = repository
        .list_takes(segment.id)
        .await
        .map_err(storage_error)?
        .len();
    let segment_inputs_current = crate::conversion::load_proofing_segment_plan(
        state,
        segment.project_id.as_uuid(),
        &segment,
    )
    .await
    .and_then(|plan| crate::conversion::segment_semantic_input_hash(&plan))
    .is_ok_and(|hash| hash == segment.expected_input_hash);
    let selected_take_current = segment_inputs_current
        && selected_take
            .as_ref()
            .is_some_and(|take| take.semantic_input_hash == segment.expected_input_hash);
    let audio_url = selected_take
        .as_ref()
        .map(|take| format!("/api/v1/artifacts/{}", take.artifact_id));
    Ok(ProofingSegmentView {
        segment,
        selection,
        selected_take,
        take_count,
        selected_take_current,
        audio_url,
    })
}

async fn ensure_proof_mutable(
    state: &AppState,
    project_id: Uuid,
) -> Result<ProofingPlan, ServiceError> {
    let plan = state
        .database
        .repositories()
        .proofing
        .get_plan(ProjectId::from_uuid(project_id))
        .await
        .map_err(storage_error)?
        .ok_or_else(|| {
            conflict(
                "proofing_unavailable",
                "start a new conversion to create proofing data",
            )
        })?;
    let job = state
        .database
        .repositories()
        .jobs
        .get(plan.source_conversion_job_id)
        .await
        .map_err(storage_error)?
        .ok_or_else(|| {
            conflict(
                "conversion_missing",
                "the source conversion job is unavailable",
            )
        })?;
    if job.state != JobState::Paused && !job.state.is_terminal() {
        return Err(conflict(
            "conversion_active",
            "pause or finish the conversion before changing proofing data",
        ));
    }
    Ok(plan)
}

async fn ensure_no_active_regeneration(
    state: &AppState,
    segment_id: SegmentId,
) -> Result<(), ServiceError> {
    let active_job = sqlx::query_scalar::<_, String>(
        "SELECT j.id FROM jobs j JOIN job_units u ON u.job_id = j.id \
         WHERE j.kind = 'segment_regeneration' AND u.proof_segment_id = ? \
         AND j.state NOT IN ('cancelled', 'failed', 'completed') LIMIT 1",
    )
    .bind(segment_id.to_string())
    .fetch_optional(state.database.pool())
    .await
    .map_err(storage_error)?;
    if let Some(active_job_id) = active_job {
        return Err(ServiceError::ConflictDetails {
            code: "active_segment_regeneration",
            detail: "wait for the active regeneration before changing this segment".to_owned(),
            meta: serde_json::json!({"activeJobId": active_job_id, "segmentId": segment_id}),
        });
    }
    Ok(())
}

async fn ensure_no_active_proof_export(
    state: &AppState,
    project_id: Uuid,
) -> Result<(), ServiceError> {
    let active_job = sqlx::query_scalar::<_, String>(
        "SELECT id FROM jobs WHERE project_id = ? AND kind = 'export' \
         AND state NOT IN ('cancelled', 'failed', 'completed') LIMIT 1",
    )
    .bind(project_id.to_string())
    .fetch_optional(state.database.pool())
    .await
    .map_err(storage_error)?;
    if let Some(active_job_id) = active_job {
        return Err(ServiceError::ConflictDetails {
            code: "active_proof_export",
            detail: "wait for the active proofing export before changing proofing data".to_owned(),
            meta: serde_json::json!({"activeJobId": active_job_id, "projectId": project_id}),
        });
    }
    Ok(())
}

async fn synthesis_context(
    state: &AppState,
    project_id: Uuid,
    segment: &ProductionSegment,
) -> Result<
    (
        Uuid,
        crate::models::VoiceAssignmentView,
        Vec<PronunciationRuleView>,
        Option<String>,
    ),
    ServiceError,
> {
    let catalog = state.catalog.read().await;
    let characters = catalog
        .characters
        .get(&project_id)
        .ok_or(ServiceError::NotFound)?;
    let character = match segment.speaker {
        Speaker::Character(id) => characters
            .iter()
            .find(|character| character.id == id.as_uuid()),
        Speaker::Narrator => characters
            .iter()
            .find(|character| character.role == CharacterRole::Narrator),
        Speaker::Named(ref name) => characters
            .iter()
            .find(|character| character.canonical_name.eq_ignore_ascii_case(name)),
    }
    .ok_or_else(|| {
        conflict(
            "speaker_missing",
            "the segment speaker is no longer available",
        )
    })?;
    let assignment = character.voice_assignment.clone().ok_or_else(|| {
        conflict(
            "voice_missing",
            "assign a voice before editing narration text",
        )
    })?;
    let language = catalog
        .projects
        .get(&project_id)
        .and_then(|project| project.summary.language.clone());
    Ok((
        character.id,
        assignment,
        catalog.pronunciation_rules.clone(),
        language,
    ))
}

async fn owned_segment(
    repository: &audiobookai_storage::ProofingRepository,
    project_id: Uuid,
    segment_id: Uuid,
) -> Result<ProductionSegment, ServiceError> {
    let segment = repository
        .get_segment(SegmentId::from_uuid(segment_id))
        .await
        .map_err(storage_error)?
        .ok_or(ServiceError::NotFound)?;
    if segment.project_id.as_uuid() != project_id || !segment.active {
        return Err(ServiceError::NotFound);
    }
    Ok(segment)
}

async fn ensure_project_exists(state: &AppState, project_id: Uuid) -> Result<(), ServiceError> {
    if state
        .catalog
        .read()
        .await
        .projects
        .contains_key(&project_id)
    {
        Ok(())
    } else {
        Err(ServiceError::NotFound)
    }
}

fn plan_hash(segments: &[ProductionSegment]) -> String {
    let mut values = segments.iter().collect::<Vec<_>>();
    values.sort_by_key(|segment| (segment.chapter_id, segment.ordinal, segment.id));
    let mut hasher = blake3::Hasher::new();
    for segment in values {
        hasher.update(segment.stable_key.as_bytes());
        hasher.update(&[0]);
        hasher.update(segment.expected_input_hash.as_bytes());
        hasher.update(&[0]);
    }
    hasher.finalize().to_hex().to_string()
}

const fn empty_counts() -> ProofingCountsView {
    ProofingCountsView {
        total: 0,
        unreviewed: 0,
        flagged: 0,
        approved: 0,
        locked: 0,
        stale: 0,
        missing: 0,
    }
}

fn stale_segment(segment_id: SegmentId, current_revision: u64) -> ServiceError {
    ServiceError::ConflictDetails {
        code: "stale_segment_revision",
        detail: "the segment changed since it was loaded".to_owned(),
        meta: serde_json::json!({
            "segmentId": segment_id,
            "currentRevision": current_revision,
        }),
    }
}

fn conflict(code: &'static str, detail: &'static str) -> ServiceError {
    ServiceError::ConflictDetails {
        code,
        detail: detail.to_owned(),
        meta: serde_json::json!({}),
    }
}

fn storage_error(error: impl std::fmt::Display) -> ServiceError {
    ServiceError::Storage(error.to_string())
}

fn map_storage_conflict(error: audiobookai_storage::StorageError) -> ServiceError {
    match error {
        audiobookai_storage::StorageError::StaleRevision { entity, id } => {
            ServiceError::ConflictDetails {
                code: "stale_revision",
                detail: format!("stale {entity} revision for {id}"),
                meta: serde_json::json!({"entity": entity, "id": id}),
            }
        }
        other => storage_error(other),
    }
}
