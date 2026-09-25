use super::{
    AppState, Arc, Deserialize, Duration, FilePath, Json, Path, PathBuf, ProviderKindView,
    ProviderModeView, ProviderProfileView, ProviderRoleView, ProviderStatusView, ServiceError,
    State, StatusCode, Utc, Uuid, default_capabilities, persist_provider,
    validate_provider_location,
};

pub(super) async fn mlx_management_status(
    State(state): State<Arc<AppState>>,
) -> Json<crate::mlx_management::MlxManagementView> {
    Json(state.mlx.view().await)
}

pub(super) async fn install_mlx_audio(
    State(state): State<Arc<AppState>>,
) -> Result<(StatusCode, Json<crate::mlx_management::MlxOperationView>), ServiceError> {
    let operation = state.mlx.start_install().await?;
    watch_mlx_install_for_profile(Arc::clone(&state), operation.id);
    Ok((StatusCode::ACCEPTED, Json(operation)))
}

pub(super) async fn uninstall_mlx_audio(
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

pub(super) async fn cancel_mlx_operation(
    State(state): State<Arc<AppState>>,
    Path(id): Path<Uuid>,
) -> Result<Json<crate::mlx_management::MlxOperationView>, ServiceError> {
    state.mlx.cancel(id).await.map(Json)
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct DownloadMlxModelInput {
    pub(super) repository: String,
    #[serde(default = "default_model_revision")]
    pub(super) revision: String,
}

pub(super) fn default_model_revision() -> String {
    "main".to_owned()
}

pub(super) async fn download_mlx_model(
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
pub(super) struct ConfirmMlxAction {
    #[serde(default)]
    pub(super) confirmed: bool,
}

pub(super) async fn remove_mlx_model(
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

pub(super) fn require_mlx_uninstall_confirmation(confirmed: bool) -> Result<(), ServiceError> {
    if confirmed {
        Ok(())
    } else {
        Err(ServiceError::InvalidRequest(
            "an explicit confirmed=true body is required before uninstalling the app-owned MLX-audio runtime"
                .to_owned(),
        ))
    }
}

pub(super) async fn mlx_model_is_in_use(
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
pub(super) struct CanonicalMlxModelPath {
    pub(super) managed_root: PathBuf,
    pub(super) model: PathBuf,
}

pub(super) async fn canonical_mlx_model_path(
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

pub(super) async fn canonicalize_path_with_optional_missing_leaf(
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

pub(super) async fn mlx_model_path_matches(
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

pub(super) async fn json_payloads_reference_model_path(
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

pub(super) async fn payload_references_model_path(
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

pub(super) fn collect_model_path_candidates<'a>(
    value: &'a serde_json::Value,
    output: &mut Vec<&'a str>,
) {
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

pub(super) fn watch_mlx_install_for_profile(state: Arc<AppState>, operation_id: Uuid) {
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

pub(super) async fn auto_configure_mlx_profile(
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
        role: ProviderRoleView::Tts,
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
        context_window_tokens: None,
        credential_configured: false,
        capabilities: Some(default_capabilities(
            &ProviderKindView::MlxAudio,
            ProviderRoleView::Tts,
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

pub(super) async fn piper_management_status(
    State(state): State<Arc<AppState>>,
) -> Json<crate::piper_management::PiperManagementView> {
    let mut view = state.piper.view().await;
    let has_connection = state
        .catalog
        .read()
        .await
        .providers
        .values()
        .any(|profile| matches!(profile.kind, ProviderKindView::Piper));
    let action_required = view.installed && !view.installed_voices.is_empty() && !has_connection;
    state
        .piper
        .set_profile_action_required(action_required)
        .await;
    view.profile_action_required = action_required;
    Json(view)
}

pub(super) async fn install_piper(
    State(state): State<Arc<AppState>>,
) -> Result<
    (
        StatusCode,
        Json<crate::piper_management::PiperOperationView>,
    ),
    ServiceError,
> {
    let operation = state.piper.start_install().await?;
    Ok((StatusCode::ACCEPTED, Json(operation)))
}

pub(super) async fn uninstall_piper(
    State(state): State<Arc<AppState>>,
    Json(input): Json<ConfirmMlxAction>,
) -> Result<
    (
        StatusCode,
        Json<crate::piper_management::PiperOperationView>,
    ),
    ServiceError,
> {
    if !input.confirmed {
        return Err(ServiceError::InvalidRequest(
            "an explicit confirmed=true body is required before uninstalling Piper".to_owned(),
        ));
    }
    // Provider create/update/delete also hold this guard. Keeping it until the manager records the
    // uninstall operation makes the admission decision atomic with respect to Piper connections:
    // either an existing connection blocks uninstall, or a later mutation observes active uninstall.
    let _model_lifecycle_guard = state.model_lifecycle.lock().await;
    let profiles = state
        .catalog
        .read()
        .await
        .providers
        .values()
        .filter(|profile| matches!(profile.kind, ProviderKindView::Piper))
        .count();
    if profiles > 0 {
        return Err(ServiceError::Conflict(
            "delete every Piper provider connection before uninstalling its runtime; downloaded voices are retained"
                .to_owned(),
        ));
    }
    let operation = state.piper.start_uninstall().await?;
    Ok((StatusCode::ACCEPTED, Json(operation)))
}

pub(super) async fn cancel_piper_operation(
    State(state): State<Arc<AppState>>,
    Path(id): Path<Uuid>,
) -> Result<Json<crate::piper_management::PiperOperationView>, ServiceError> {
    state.piper.cancel(id).await.map(Json)
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct DownloadPiperVoiceInput {
    pub(super) voice_id: String,
    #[serde(default)]
    pub(super) license_confirmed: bool,
}

pub(super) async fn download_piper_voice(
    State(state): State<Arc<AppState>>,
    Json(input): Json<DownloadPiperVoiceInput>,
) -> Result<
    (
        StatusCode,
        Json<crate::piper_management::PiperOperationView>,
    ),
    ServiceError,
> {
    let _model_lifecycle_guard = state.model_lifecycle.lock().await;
    let operation = state
        .piper
        .start_voice_download(&input.voice_id, input.license_confirmed)
        .await?;
    Ok((StatusCode::ACCEPTED, Json(operation)))
}

pub(super) async fn remove_piper_voice(
    State(state): State<Arc<AppState>>,
    Path(voice_id): Path<String>,
    Json(input): Json<ConfirmMlxAction>,
) -> Result<StatusCode, ServiceError> {
    let _model_lifecycle_guard = state.model_lifecycle.lock().await;
    let in_use = piper_voice_is_in_use(&state, &voice_id).await?;
    state
        .piper
        .remove_voice(&voice_id, input.confirmed, in_use)
        .await?;
    Ok(StatusCode::NO_CONTENT)
}

pub(super) async fn piper_voice_is_in_use(
    state: &AppState,
    voice_id: &str,
) -> Result<bool, ServiceError> {
    let catalog_references = {
        let catalog = state.catalog.read().await;
        catalog.providers.values().any(|profile| {
            matches!(profile.kind, ProviderKindView::Piper)
                && profile.model.as_deref() == Some(voice_id)
        }) || catalog.characters.values().flatten().any(|character| {
            character
                .voice_assignment
                .as_ref()
                .is_some_and(|assignment| assignment.model.as_deref() == Some(voice_id))
        })
    };
    if catalog_references {
        return Ok(true);
    }
    let assignment_payloads =
        sqlx::query_scalar::<_, String>("SELECT payload FROM voice_assignments")
            .fetch_all(state.database.pool())
            .await
            .map_err(|error| ServiceError::Storage(error.to_string()))?;
    let active_job_payloads = sqlx::query_scalar::<_, String>(
        "SELECT ju.payload FROM job_units ju \
         JOIN jobs j ON j.id = ju.job_id \
         WHERE j.state NOT IN ('cancelled', 'failed', 'completed')",
    )
    .fetch_all(state.database.pool())
    .await
    .map_err(|error| ServiceError::Storage(error.to_string()))?;
    for payload in assignment_payloads.into_iter().chain(active_job_payloads) {
        let value: serde_json::Value = serde_json::from_str(&payload).map_err(|_| {
            ServiceError::Conflict(
                "stored assignment or active-job metadata could not be verified; Piper voice removal is blocked"
                    .to_owned(),
            )
        })?;
        let mut candidates = Vec::new();
        collect_model_id_candidates(&value, &mut candidates);
        if candidates
            .into_iter()
            .any(|candidate| candidate == voice_id)
        {
            return Ok(true);
        }
    }
    Ok(false)
}

pub(super) fn collect_model_id_candidates<'a>(
    value: &'a serde_json::Value,
    output: &mut Vec<&'a str>,
) {
    match value {
        serde_json::Value::Object(fields) => {
            for (key, value) in fields {
                if key.eq_ignore_ascii_case("model")
                    && let Some(candidate) = value.as_str()
                {
                    output.push(candidate);
                }
                collect_model_id_candidates(value, output);
            }
        }
        serde_json::Value::Array(values) => {
            for value in values {
                collect_model_id_candidates(value, output);
            }
        }
        _ => {}
    }
}
