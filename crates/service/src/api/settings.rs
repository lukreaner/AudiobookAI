use super::{
    AppSettingsView, AppState, Arc, Deserialize, Json, Path, ServiceError, State, StatusCode, Utc,
    assign_bool, assign_f32, assign_string, assign_u16_in_range, assign_u64,
};

pub(super) async fn get_settings(
    State(state): State<Arc<AppState>>,
) -> Result<Json<AppSettingsView>, ServiceError> {
    Ok(Json(settings_with_lan_status(&state).await?))
}

pub(super) async fn update_settings(
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
pub(super) fn apply_owner_settings_patch(
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

pub(super) fn imported_project_settings(
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

pub(super) async fn complete_first_run(
    State(state): State<Arc<AppState>>,
) -> Result<Json<AppSettingsView>, ServiceError> {
    let mut settings = state.catalog.read().await.settings.clone();
    settings.first_run_complete = true;
    persist_settings(&state, &settings).await?;
    state.catalog.write().await.settings = settings.clone();
    Ok(Json(settings))
}

pub(super) async fn revoke_lan_sessions(
    State(state): State<Arc<AppState>>,
) -> Result<StatusCode, ServiceError> {
    state.auth.revoke_lan_sessions().await?;
    Ok(StatusCode::NO_CONTENT)
}

#[derive(Debug, Deserialize)]
pub(super) struct CreateLanTokenInput {
    pub(super) name: String,
}

pub(super) async fn list_lan_tokens(
    State(state): State<Arc<AppState>>,
) -> Result<Json<Vec<crate::auth::ApiTokenSummary>>, ServiceError> {
    Ok(Json(state.auth.list_api_tokens().await?))
}

pub(super) async fn create_lan_token(
    State(state): State<Arc<AppState>>,
    Json(input): Json<CreateLanTokenInput>,
) -> Result<(StatusCode, Json<crate::auth::IssuedApiToken>), ServiceError> {
    let token = state.auth.issue_api_token(input.name).await?;
    Ok((StatusCode::CREATED, Json(token)))
}

pub(super) async fn revoke_lan_token(
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
pub(super) struct SetLanPasswordInput {
    pub(super) password: zeroize::Zeroizing<String>,
}

pub(super) async fn set_lan_password(
    State(state): State<Arc<AppState>>,
    Json(input): Json<SetLanPasswordInput>,
) -> Result<StatusCode, ServiceError> {
    state
        .auth
        .configure_lan_password(&state.secrets, input.password.as_str())
        .await?;
    Ok(StatusCode::NO_CONTENT)
}

pub(super) async fn settings_with_lan_status(
    state: &AppState,
) -> Result<AppSettingsView, ServiceError> {
    let mut settings = state.catalog.read().await.settings.clone();
    let status = state.auth.lan_status().await?;
    settings.lan.password_configured = status.password_configured;
    settings.lan.api_token_count = status.api_token_count;
    settings.lan.active_sessions = status.active_sessions;
    settings.lan.restart_required =
        crate::config::lan_restart_required(&settings.lan, &state.config);
    Ok(settings)
}

pub(super) async fn persist_settings(
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
pub(super) struct SecretStatusResponse {
    pub(super) unlocked: bool,
    pub(super) backend: &'static str,
}

pub(super) async fn secret_status(
    State(state): State<Arc<AppState>>,
) -> Json<SecretStatusResponse> {
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
pub(super) struct UnlockSecretStoreInput {
    pub(super) passphrase: zeroize::Zeroizing<String>,
}

pub(super) async fn unlock_secret_store(
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

pub(super) async fn lock_secret_store(State(state): State<Arc<AppState>>) -> StatusCode {
    state.secrets.lock().await;
    state.catalog.write().await.settings.secret_store = crate::models::SecretStoreView::Locked;
    StatusCode::NO_CONTENT
}
