use super::{
    AppState, Arc, BTreeMap, Deserialize, Duration, HashMap, Json, Page, Path, ProviderKindView,
    ProviderModeView, ProviderProfileInput, ProviderProfileView, ProviderRoleView,
    ProviderStatusView, ServiceError, State, StatusCode, Utc, Uuid, VoiceView,
    canonical_provider_kind, default_capabilities, is_native_provider, persist_provider,
    persist_voice_profile, provider_capabilities_are_fresh, provider_model_catalog_is_strict,
    provider_model_is_compatible, reject_empty, validate_provider_context_window,
    validate_provider_location, validate_provider_model_compatibility,
    validate_provider_sensitive_fields,
};

pub(super) const LM_STUDIO_STATUS_RECOVERY_BUDGET: Duration = Duration::from_secs(4);

/// Rechecks only stale external LM Studio profiles while serving a provider list. This lets a
/// server that starts after `AudiobookAI` recover from its startup-time Offline observation without
/// turning every provider-list read into cloud traffic or taking ownership of a managed child.
pub(super) async fn recover_stale_external_lm_studio_statuses(state: &Arc<AppState>) {
    let candidates = state
        .catalog
        .read()
        .await
        .providers
        .values()
        .filter(|profile| {
            matches!(profile.kind, ProviderKindView::LmStudio)
                && matches!(profile.mode, ProviderModeView::ExternalEndpoint)
                && matches!(
                    profile.status,
                    ProviderStatusView::Offline | ProviderStatusView::Error
                )
        })
        .map(|profile| profile.id)
        .collect::<Vec<_>>();

    for id in candidates {
        let Ok(_lifecycle_guard) = state.model_lifecycle.try_lock() else {
            return;
        };
        let still_recoverable =
            state
                .catalog
                .read()
                .await
                .providers
                .get(&id)
                .is_some_and(|profile| {
                    matches!(profile.kind, ProviderKindView::LmStudio)
                        && matches!(profile.mode, ProviderModeView::ExternalEndpoint)
                        && matches!(
                            profile.status,
                            ProviderStatusView::Offline | ProviderStatusView::Error
                        )
                });
        if !still_recoverable {
            continue;
        }
        match tokio::time::timeout(
            LM_STUDIO_STATUS_RECOVERY_BUDGET,
            refresh_provider(state, id),
        )
        .await
        {
            Ok(Ok(_)) => {}
            Ok(Err(error)) => {
                tracing::debug!(diagnostic_code = "provider.status.recovery.failed", provider_id = %id, %error, "LM Studio status recovery failed");
            }
            Err(_) => {
                tracing::debug!(diagnostic_code = "provider.status.recovery.timeout", provider_id = %id, "LM Studio status recovery timed out");
            }
        }
    }
}

pub(super) async fn list_providers(
    State(state): State<Arc<AppState>>,
) -> Json<Page<ProviderProfileView>> {
    recover_stale_external_lm_studio_statuses(&state).await;
    let catalog = state.catalog.read().await;
    let mut providers = catalog.providers.values().cloned().collect::<Vec<_>>();
    providers.sort_by(|left, right| left.name.cmp(&right.name));
    Json(Page::all(providers))
}

#[derive(Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct ProviderModelDiscoveryInput {
    pub(super) provider_id: Option<Uuid>,
    #[serde(flatten)]
    pub(super) profile: ProviderProfileInput,
}

#[derive(Debug, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct AvailableProviderModelView {
    pub(super) id: String,
    pub(super) name: String,
}

#[derive(Debug, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct AvailableProviderModelsView {
    pub(super) items: Vec<AvailableProviderModelView>,
    pub(super) strict: bool,
}

pub(super) async fn provider_model_discovery_profile(
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
        let kind =
            canonical_provider_kind(input.kind.clone().ok_or_else(|| {
                ServiceError::InvalidRequest("provider kind is required".to_owned())
            })?);
        let role = input
            .role
            .ok_or_else(|| ServiceError::InvalidRequest("provider role is required".to_owned()))?;
        ProviderProfileView {
            id: Uuid::new_v4(),
            name: input.name.clone().unwrap_or_else(|| format!("{kind:?}")),
            kind,
            role,
            mode: input.mode.unwrap_or(ProviderModeView::CloudRemote),
            endpoint: None,
            executable_path: None,
            working_directory: None,
            arguments: Vec::new(),
            status: ProviderStatusView::Unconfigured,
            model: None,
            context_window_tokens: input.context_window_tokens.flatten(),
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
        profile.kind = canonical_provider_kind(kind.clone());
    }
    profile.kind = canonical_provider_kind(profile.kind);
    if let Some(role) = input.role {
        profile.role = role;
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
    if let Some(context_window_tokens) = input.context_window_tokens {
        profile.context_window_tokens = context_window_tokens;
    }
    Ok(profile)
}

pub(super) async fn provider_model_discovery_credential(
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

pub(super) fn piper_uninstall_in_progress(
    active_operation: Option<&crate::piper_management::PiperOperationView>,
) -> bool {
    active_operation.is_some_and(|operation| {
        matches!(
            operation.kind,
            crate::piper_management::PiperOperationKind::Uninstall
        )
    })
}

/// Builds the selected adapter in memory and performs only its model-list request. The supplied
/// credential is zeroized after the request and is never persisted by this preview endpoint.
pub(super) async fn discover_provider_models(
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
        profile.role,
        profile.mode,
        None,
        credential.is_some(),
    )?;
    // Piper models are app-owned voice bundles. Use the manager's verified inventory instead of
    // treating arbitrary directories below the shared voice root as selectable models. This path
    // also supports choosing a model before a connection-scoped Piper adapter can be built.
    if matches!(profile.kind, ProviderKindView::Piper) {
        let _model_lifecycle_guard = state.model_lifecycle.lock().await;
        let management = state.piper.view().await;
        if piper_uninstall_in_progress(management.active_operation.as_ref()) {
            return Err(ServiceError::Conflict(
                "Piper is being uninstalled; wait for the operation to finish before discovering models"
                    .to_owned(),
            ));
        }
        let items = if management.installed {
            management
                .installed_voices
                .into_iter()
                .map(|voice| AvailableProviderModelView {
                    id: voice.id,
                    name: voice.name,
                })
                .collect()
        } else {
            Vec::new()
        };
        return Ok(Json(AvailableProviderModelsView {
            items,
            strict: true,
        }));
    }
    // A stale model from the other role must not prevent discovery of a compatible replacement.
    // Model selection is not used to build or query the temporary adapter.
    profile.model = None;
    profile.capabilities = Some(default_capabilities(
        &profile.kind,
        profile.role,
        profile.mode,
    ));

    let runtime_profile = crate::state::runtime_profile_from_view(&profile, &state.config)?;
    let models = state
        .providers
        .preview_models(&runtime_profile, credential.as_ref())
        .await
        .map_err(|error| ServiceError::Conflict(error.to_string()))?;
    let mut unique = BTreeMap::<String, String>::new();
    let strict = provider_model_catalog_is_strict(&profile.kind, profile.role);
    for model in models {
        let id = model.id.trim();
        if id.is_empty()
            || (strict && !provider_model_is_compatible(&profile.kind, profile.role, id))
        {
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
        strict,
    }))
}

pub(super) async fn get_provider(
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
pub(super) enum ProviderModelCapability {
    List,
    Download,
    Delete,
}

pub(super) fn require_provider_model_capability(
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

pub(super) async fn provider_for_model_operation(
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

pub(super) async fn provider_model_library(
    State(state): State<Arc<AppState>>,
    Path(id): Path<Uuid>,
) -> Result<Json<crate::provider_models::ProviderModelLibraryView>, ServiceError> {
    provider_for_model_operation(&state, id, ProviderModelCapability::List).await?;
    state.provider_models.library(id).await.map(Json)
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct DownloadProviderModelInput {
    pub(super) model: String,
    pub(super) quantization: Option<String>,
}

pub(super) async fn download_provider_model(
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

pub(super) async fn cancel_provider_model_download(
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
pub(super) struct DeleteProviderModelInput {
    pub(super) model: String,
    #[serde(default)]
    pub(super) confirmed: bool,
}

pub(super) async fn delete_provider_model(
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

pub(super) async fn provider_model_is_in_use(
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

pub(super) fn character_assignments_reference_provider_model(
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

pub(super) fn payload_references_provider_model(
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

pub(super) fn provider_models_equal(kind: &ProviderKindView, left: &str, right: &str) -> bool {
    provider_models_equal_checked(kind, left, right).unwrap_or(false)
}

pub(super) fn provider_models_equal_checked(
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

/// Rejects a runtime replacement/removal while durable work can still dispatch through it.
///
/// Job admission and provider mutation both hold `model_lifecycle`, so the query and the
/// subsequent mutation form one closed window: either the job is admitted first and blocks the
/// mutation, or the mutation completes first and the job validates the replacement profile.
pub(super) async fn ensure_provider_runtime_mutation_allowed(
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

#[allow(clippy::too_many_lines)]
pub(super) async fn delete_provider(
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
    let durable_in_use = sqlx::query_scalar::<_, i64>(
        "SELECT 1 FROM voice_assignments WHERE provider_id = ? LIMIT 1",
    )
    .bind(id.to_string())
    .fetch_optional(state.database.pool())
    .await
    .map_err(|error| ServiceError::Storage(error.to_string()))?
    .is_some();
    if in_use || durable_in_use {
        return Err(ServiceError::Conflict(
            "remove this provider from all character voice assignments before deleting it"
                .to_owned(),
        ));
    }
    let retained_voice = sqlx::query_scalar::<_, i64>(
        "SELECT 1 FROM voice_profiles WHERE provider_id = ? \
         AND NOT (ownership = 'provider' AND origin IN ('provider_catalog', 'native_system')) \
         LIMIT 1",
    )
    .bind(id.to_string())
    .fetch_optional(state.database.pool())
    .await
    .map_err(|error| ServiceError::Storage(error.to_string()))?
    .is_some();
    if retained_voice {
        return Err(ServiceError::Conflict(
            "remove every cloned or user-owned voice through its dedicated voice lifecycle before deleting this provider"
                .to_owned(),
        ));
    }
    let mut tombstone = state
        .database
        .repositories()
        .providers
        .get(audiobookai_core::ProviderProfileId::from_uuid(id))
        .await
        .map_err(|error| ServiceError::Storage(error.to_string()))?
        .ok_or(ServiceError::NotFound)?;
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
    let mut secret_ids = tombstone
        .environment_secret_ids
        .values()
        .copied()
        .collect::<Vec<_>>();
    for candidate in [tombstone.credential_secret_id, secret_id]
        .into_iter()
        .flatten()
    {
        if !secret_ids.contains(&candidate) {
            secret_ids.push(candidate);
        }
    }
    tombstone.enabled = false;
    tombstone.credential_secret_id = None;
    tombstone.environment_secret_ids.clear();
    tombstone.capability_snapshot = None;
    tombstone.updated_at = Utc::now();
    let tombstone_payload = serde_json::to_string(&tombstone)
        .map_err(|error| ServiceError::Internal(error.to_string()))?;
    let database_delete = async {
        let mut transaction = state
            .database
            .pool()
            .begin()
            .await
            .map_err(|error| ServiceError::Storage(error.to_string()))?;
        // Discovered catalog voices are children owned by the connection. Assignments were
        // checked above, so remove these rows before the provider's RESTRICT foreign key.
        sqlx::query(
            "DELETE FROM voice_profiles WHERE provider_id = ? \
             AND ownership = 'provider' AND origin IN ('provider_catalog', 'native_system')",
        )
        .bind(id.to_string())
        .execute(&mut *transaction)
        .await
        .map_err(|error| ServiceError::Storage(error.to_string()))?;
        // Provider IDs are durable accounting/provenance identities. Keep a credential-free,
        // disabled tombstone so completed jobs, detections, and usage remain auditable while the
        // connection disappears from runtime/catalog selection and no longer blocks uninstall.
        let deleted = sqlx::query(
            "UPDATE providers SET enabled = 0, updated_at = ?, payload = ? WHERE id = ?",
        )
        .bind(tombstone.updated_at.to_rfc3339())
        .bind(&tombstone_payload)
        .bind(id.to_string())
        .execute(&mut *transaction)
        .await
        .map_err(|error| ServiceError::Storage(error.to_string()))?;
        if deleted.rows_affected() == 0 {
            return Err(ServiceError::NotFound);
        }
        sqlx::query("UPDATE budgets SET enabled = 0 WHERE provider_id = ?")
            .bind(id.to_string())
            .execute(&mut *transaction)
            .await
            .map_err(|error| ServiceError::Storage(error.to_string()))?;
        transaction
            .commit()
            .await
            .map_err(|error| ServiceError::Storage(error.to_string()))
    }
    .await;
    if let Err(error) = database_delete {
        // The catalog still contains the unchanged profile. Restore its runtime registration so
        // a historical FK or storage failure cannot strand a visible connection as unusable.
        if let Err(restore_error) = state.sync_provider_runtime(id).await {
            tracing::warn!(
                diagnostic_code = "provider.delete.runtime_restore.failed",
                provider_id = %id,
                %restore_error,
                "provider deletion failed and its runtime registration could not be restored"
            );
        }
        return Err(error);
    }
    for secret_id in secret_ids {
        if let Err(error) = state.secrets.delete(secret_id).await {
            tracing::warn!(diagnostic_code = "provider.secret.cleanup.failed", %secret_id, %error, "provider was deleted but its orphaned secret reference could not be removed");
        }
    }
    let mut catalog = state.catalog.write().await;
    catalog.providers.remove(&id);
    catalog.provider_secret_ids.remove(&id);
    catalog
        .budgets
        .retain(|_, budget| budget.provider_profile_id != Some(id));
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

#[allow(clippy::too_many_lines)]
pub(super) async fn create_provider(
    State(state): State<Arc<AppState>>,
    Json(input): Json<ProviderProfileInput>,
) -> Result<(StatusCode, Json<ProviderProfileView>), ServiceError> {
    let credential = input.credential;
    let kind = canonical_provider_kind(
        input
            .kind
            .ok_or_else(|| ServiceError::InvalidRequest("provider kind is required".to_owned()))?,
    );
    let role = input
        .role
        .ok_or_else(|| ServiceError::InvalidRequest("provider role is required".to_owned()))?;
    let mode = input.mode.unwrap_or(ProviderModeView::CloudRemote);
    let endpoint = input.endpoint.flatten();
    let executable_path = input.executable_path.flatten();
    let working_directory = input.working_directory.flatten();
    let arguments = input.arguments.unwrap_or_default();
    let model = input.model.flatten();
    let context_window_tokens = input.context_window_tokens.flatten();
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
    validate_provider_sensitive_fields(&kind, role, mode, model.as_deref(), credential.is_some())?;
    validate_provider_context_window(role, context_window_tokens)?;
    if matches!(&kind, ProviderKindView::NativeOs) {
        let availability = crate::state::native_tts_availability(&state.config);
        if !availability.available {
            return Err(ServiceError::InvalidRequest(
                availability.detail.unwrap_or_else(|| {
                    "native system voices are unavailable on this computer".to_owned()
                }),
            ));
        }
    }
    let name = input.name.unwrap_or_else(|| format!("{kind:?}"));
    reject_empty("name", &name)?;
    let capabilities = default_capabilities(&kind, role, mode);
    let mut profile = ProviderProfileView {
        id: Uuid::new_v4(),
        name,
        kind,
        role,
        mode,
        endpoint,
        executable_path,
        working_directory,
        arguments,
        status: ProviderStatusView::Unconfigured,
        model,
        context_window_tokens,
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
        profile.last_error = Some(crate::state::provider_setup_error_detail(
            &profile,
            &state.config,
            &error,
        ));
        state
            .catalog
            .write()
            .await
            .providers
            .insert(profile.id, profile.clone());
    } else {
        profile = refresh_provider(&state, profile.id).await?;
    }
    Ok((StatusCode::CREATED, Json(profile)))
}

// Persist edits before reconstructing the runtime so a temporarily unavailable or invalid
// connection remains visible as Unconfigured and can be repaired without losing its settings.
#[allow(clippy::too_many_lines)]
pub(super) async fn update_provider(
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
    let previous_role = updated.role;
    let previous_model = updated.model.clone();
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
    updated.kind = canonical_provider_kind(updated.kind);
    if let Some(kind) = input.kind.map(canonical_provider_kind) {
        if kind != updated.kind {
            return Err(ServiceError::InvalidRequest(
                "a provider connection's type cannot be changed; create a separate connection instead"
                    .to_owned(),
            ));
        }
        updated.kind = kind;
    }
    if let Some(role) = input.role {
        updated.role = role;
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
    if let Some(context_window_tokens) = input.context_window_tokens {
        updated.context_window_tokens = context_window_tokens;
    }
    let piper_model_changed =
        matches!(updated.kind, ProviderKindView::Piper) && previous_model != updated.model;
    if previous_role != updated.role || piper_model_changed {
        let catalog_in_use = state
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
        let durable_in_use = sqlx::query_scalar::<_, i64>(
            "SELECT 1 FROM voice_assignments WHERE provider_id = ? LIMIT 1",
        )
        .bind(id.to_string())
        .fetch_optional(state.database.pool())
        .await
        .map_err(|error| ServiceError::Storage(error.to_string()))?
        .is_some();
        if catalog_in_use || durable_in_use {
            let detail = if piper_model_changed {
                "remove this provider from all character voice assignments before changing its Piper voice"
            } else {
                "remove this provider from all character voice assignments before changing its TTS/LLM role"
            };
            return Err(ServiceError::Conflict(detail.to_owned()));
        }
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
        updated.role,
        updated.mode,
        updated.model.as_deref(),
        credential.is_some()
            || (!was_native
                && is_native_provider(&updated.kind, updated.mode)
                && secret_id.is_some()),
    )?;
    validate_provider_context_window(updated.role, updated.context_window_tokens)?;
    updated.capabilities = Some(default_capabilities(
        &updated.kind,
        updated.role,
        updated.mode,
    ));
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
    if previous_role != updated.role && matches!(updated.role, ProviderRoleView::Llm) {
        sqlx::query("DELETE FROM voice_profiles WHERE provider_id = ?")
            .bind(id.to_string())
            .execute(state.database.pool())
            .await
            .map_err(|error| ServiceError::Storage(error.to_string()))?;
    }
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
        updated.last_error = Some(crate::state::provider_setup_error_detail(
            &updated,
            &state.config,
            &error,
        ));
    } else {
        updated = refresh_provider(&state, id).await?;
    }
    if previous_role != updated.role && matches!(updated.role, ProviderRoleView::Llm) {
        let mut catalog = state.catalog.write().await;
        let removed = catalog
            .voices
            .iter()
            .filter(|voice| voice.provider_profile_id == id)
            .map(|voice| voice.id)
            .collect::<Vec<_>>();
        catalog
            .voices
            .retain(|voice| voice.provider_profile_id != id);
        for voice_id in removed {
            catalog.voice_sources.remove(&voice_id);
        }
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
pub(super) async fn provider_action(
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
    let sync_error = if !runtime_registered
        || (action == "refresh" && matches!(profile.kind, ProviderKindView::NativeOs))
    {
        state.sync_provider_runtime(id).await.err()
    } else {
        None
    };
    if let Some(error) = sync_error {
        if action == "refresh" && matches!(profile.kind, ProviderKindView::NativeOs) {
            let mut updated = profile;
            updated.status = ProviderStatusView::Unconfigured;
            updated.last_error = Some(crate::state::provider_setup_error_detail(
                &updated,
                &state.config,
                &error,
            ));
            state
                .catalog
                .write()
                .await
                .providers
                .insert(id, updated.clone());
            return Ok(Json(
                serde_json::to_value(updated)
                    .map_err(|error| ServiceError::Internal(error.to_string()))?,
            ));
        }
        return Err(error);
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
            validate_provider_model_compatibility(&profile.kind, profile.role, Some(model))?;
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
pub(super) struct ProviderActionInput {
    pub(super) model: Option<String>,
}

// Health probing and voice-catalog replacement are one refresh transaction;
// the linear flow avoids publishing a partially refreshed provider.
#[allow(clippy::too_many_lines)]
pub(crate) async fn refresh_provider(
    state: &AppState,
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
    if matches!(profile.kind, ProviderKindView::NativeOs) {
        let availability =
            crate::state::native_tts_availability_for_profile(&profile, &state.config);
        if !availability.available {
            let mut updated = profile;
            updated.status = ProviderStatusView::Unconfigured;
            updated.last_error = availability.detail;
            state
                .catalog
                .write()
                .await
                .providers
                .insert(id, updated.clone());
            return Ok(updated);
        }
    }
    if let Err(error) =
        validate_provider_model_compatibility(&profile.kind, profile.role, profile.model.as_deref())
    {
        let mut updated = profile;
        updated.status = ProviderStatusView::Unconfigured;
        updated.last_error = Some(error.to_string());
        state
            .catalog
            .write()
            .await
            .providers
            .insert(id, updated.clone());
        return Ok(updated);
    }
    let runtime_id = audiobookai_providers::ProviderId::new(id.to_string())
        .map_err(|error| ServiceError::Internal(error.to_string()))?;
    let health = match profile.role {
        ProviderRoleView::Tts => {
            state
                .providers
                .tts(&runtime_id)
                .await
                .map_err(|error| ServiceError::Conflict(error.to_string()))?
                .health()
                .await
        }
        ProviderRoleView::Llm => {
            state
                .providers
                .character(&runtime_id)
                .await
                .map_err(|error| ServiceError::Conflict(error.to_string()))?
                .health()
                .await
        }
    };
    let previous_capabilities = profile.capabilities.clone();
    let mut updated = profile;
    updated.capabilities = Some(default_capabilities(
        &updated.kind,
        updated.role,
        updated.mode,
    ));
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

    if matches!(updated.role, ProviderRoleView::Llm) {
        refresh_generation_controls(state, &runtime_id, previous_capabilities, &mut updated).await;
    }

    if matches!(updated.role, ProviderRoleView::Tts)
        && matches!(updated.status, ProviderStatusView::Online)
        && let Ok(voices) = state.providers.discover_voices(&runtime_id).await
    {
        let selected_piper_model = matches!(updated.kind, ProviderKindView::Piper)
            .then(|| updated.model.clone())
            .flatten();
        let mapped = voices
            .into_iter()
            .filter(|voice| {
                selected_piper_model
                    .as_deref()
                    .is_none_or(|selected| voice.id == selected)
            })
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

const GENERATION_CONTROLS_TIMEOUT: Duration = Duration::from_secs(8);

/// Restricts an LLM connection's temperature and reasoning options to what its exact model
/// accepts, as reported by the provider.
///
/// When the provider cannot be asked, the options last determined for the same model are kept;
/// otherwise only provider defaults remain, because those are always accepted.
async fn refresh_generation_controls(
    state: &AppState,
    runtime_id: &audiobookai_providers::ProviderId,
    previous: Option<crate::models::ProviderCapabilitiesView>,
    updated: &mut ProviderProfileView,
) {
    let Some(model) = updated.model.clone() else {
        apply_generation_controls(
            updated,
            &audiobookai_providers::ModelGenerationControls::provider_default_only(),
            None,
        );
        return;
    };
    let discovered = if matches!(updated.status, ProviderStatusView::Online) {
        match state.providers.character(runtime_id).await {
            Ok(provider) => tokio::time::timeout(
                GENERATION_CONTROLS_TIMEOUT,
                provider.model_generation_controls(&model),
            )
            .await
            .map_err(|_| "timed out".to_owned())
            .and_then(|result| result.map_err(|error| error.to_string())),
            Err(error) => Err(error.to_string()),
        }
    } else {
        Err("provider offline".to_owned())
    };
    match discovered {
        Ok(controls) => apply_generation_controls(updated, &controls, Some(model)),
        Err(error) => {
            tracing::debug!(diagnostic_code = "provider.generation_controls.unavailable", provider_id = %updated.id, %error, "model generation controls could not be determined");
            let reusable = previous.filter(|capabilities| {
                capabilities.generation_controls_model.as_deref() == Some(model.as_str())
            });
            if let (Some(previous), Some(current)) = (reusable, updated.capabilities.as_mut()) {
                current.temperature = previous.temperature;
                current.reasoning = previous.reasoning;
                current.reasoning_efforts = previous.reasoning_efforts;
                current.min_reasoning_budget = previous.min_reasoning_budget;
                current.max_reasoning_budget = previous.max_reasoning_budget;
                current.max_temperature = previous.max_temperature;
                current.generation_controls_model = previous.generation_controls_model;
                current.generation_controls_source = previous.generation_controls_source;
            } else {
                apply_generation_controls(
                    updated,
                    &audiobookai_providers::ModelGenerationControls::provider_default_only(),
                    Some(model),
                );
            }
        }
    }
}

/// The options determined for `model` on this connection; only provider defaults when none were
/// determined for exactly this model.
pub(crate) fn model_generation_controls_for(
    profile: &ProviderProfileView,
    model: &str,
) -> audiobookai_providers::ModelGenerationControls {
    use audiobookai_providers::{
        GenerationControlsSource, ModelGenerationControls, ParameterSupport, ReasoningEffort,
        ReasoningMode,
    };

    let Some(capabilities) = profile
        .capabilities
        .as_ref()
        .filter(|capabilities| capabilities.generation_controls_model.as_deref() == Some(model))
    else {
        return ModelGenerationControls::provider_default_only();
    };
    ModelGenerationControls {
        temperature: match capabilities.temperature.as_str() {
            "number" => ParameterSupport::Value,
            "nullable" => ParameterSupport::NullableValue,
            _ => ParameterSupport::Unsupported,
        },
        max_temperature: capabilities.max_temperature,
        reasoning: capabilities
            .reasoning
            .iter()
            .filter_map(|mode| match mode.as_str() {
                "disabled" => Some(ReasoningMode::Disabled),
                "effort" => Some(ReasoningMode::Effort),
                "adaptive" => Some(ReasoningMode::Adaptive),
                "token_budget" => Some(ReasoningMode::TokenBudget),
                _ => None,
            })
            .collect(),
        efforts: capabilities
            .reasoning_efforts
            .iter()
            .filter_map(|level| ReasoningEffort::new(level.as_str()).ok())
            .collect(),
        min_token_budget: capabilities.min_reasoning_budget,
        max_token_budget: capabilities.max_reasoning_budget,
        source: GenerationControlsSource::AdapterContract,
    }
}

pub(crate) fn apply_generation_controls(
    profile: &mut ProviderProfileView,
    controls: &audiobookai_providers::ModelGenerationControls,
    model: Option<String>,
) {
    use audiobookai_providers::{GenerationControlsSource, ParameterSupport, ReasoningMode};

    let Some(capabilities) = profile.capabilities.as_mut() else {
        return;
    };
    match controls.temperature {
        ParameterSupport::Value => "number",
        ParameterSupport::NullableValue => "nullable",
        ParameterSupport::Unsupported | ParameterSupport::OmitOnly => "unsupported",
    }
    .clone_into(&mut capabilities.temperature);
    capabilities.max_temperature = controls.max_temperature;
    capabilities.reasoning = [
        (ReasoningMode::Disabled, "disabled"),
        (ReasoningMode::Effort, "effort"),
        (ReasoningMode::Adaptive, "adaptive"),
        (ReasoningMode::TokenBudget, "token_budget"),
    ]
    .into_iter()
    .filter(|(mode, _)| controls.reasoning.contains(mode))
    .map(|(_, name)| name.to_owned())
    .collect();
    capabilities.reasoning_efforts = controls
        .efforts
        .iter()
        .map(|level| level.as_str().to_owned())
        .collect();
    capabilities.min_reasoning_budget = controls.min_token_budget;
    capabilities.max_reasoning_budget = controls.max_token_budget;
    capabilities.generation_controls_model = model;
    let source = match controls.source {
        GenerationControlsSource::ModelApi => "model_api",
        GenerationControlsSource::ValidationProbe => "validation_probe",
        GenerationControlsSource::AdapterContract => "adapter_contract",
        GenerationControlsSource::ProviderDefaultOnly => "provider_default_only",
    };
    capabilities.generation_controls_source = Some(source.to_owned());
    if let Some(capability_source) = profile.capability_source.as_mut() {
        capability_source.push_str("+generation_controls:");
        capability_source.push_str(source);
    }
}

pub(super) async fn persist_discovered_voice(
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

pub(super) async fn wait_for_provider_readiness(
    state: &Arc<AppState>,
    id: Uuid,
) -> Result<(), ServiceError> {
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

pub(super) async fn set_provider_status(
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

pub(super) fn stable_voice_id(provider_id: Uuid, provider_voice_id: &str) -> Uuid {
    let mut hasher = blake3::Hasher::new();
    hasher.update(provider_id.as_bytes());
    hasher.update(provider_voice_id.as_bytes());
    let hash = hasher.finalize();
    let mut bytes = [0_u8; 16];
    bytes.copy_from_slice(&hash.as_bytes()[..16]);
    Uuid::from_bytes(bytes)
}
