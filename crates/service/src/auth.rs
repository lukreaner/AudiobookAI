use std::{collections::VecDeque, fmt, net::IpAddr, str::FromStr, sync::Arc};

use axum::{
    Json, Router,
    body::Body,
    extract::{Request, State},
    http::{HeaderMap, Method, StatusCode, header, uri::Authority},
    middleware::Next,
    response::{IntoResponse, Response},
    routing::post,
};
use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use chrono::{Duration, Utc};
use rand::RngCore;
use serde::{Deserialize, Serialize};
use sqlx::Row;
use tokio::sync::Mutex;
use uuid::Uuid;
use zeroize::Zeroizing;

use crate::{AppState, ServiceError};

const SESSION_COOKIE: &str = "audiobookai_session";
const CSRF_COOKIE: &str = "audiobookai_csrf";
const SESSION_HOURS: i64 = 12;

#[derive(Clone)]
pub struct AuthManager {
    database: audiobookai_storage::Database,
    bootstrap: Arc<Mutex<Option<BootstrapSecret>>>,
    authentication_required: bool,
    login_failures: Arc<Mutex<VecDeque<chrono::DateTime<Utc>>>>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct IssuedApiToken {
    pub id: String,
    pub token: Zeroizing<String>,
    pub label: String,
    pub created_at: chrono::DateTime<Utc>,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ApiTokenSummary {
    pub id: String,
    pub label: String,
    pub created_at: chrono::DateTime<Utc>,
    pub last_used_at: Option<chrono::DateTime<Utc>>,
}

#[derive(Clone, Copy, Debug, Default)]
pub struct LanAuthStatus {
    pub password_configured: bool,
    pub api_token_count: usize,
    pub active_sessions: usize,
}

impl fmt::Debug for IssuedApiToken {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("IssuedApiToken")
            .field("id", &self.id)
            .field("token", &"[REDACTED]")
            .field("label", &self.label)
            .field("created_at", &self.created_at)
            .finish()
    }
}

struct BootstrapSecret {
    plaintext: Zeroizing<String>,
    hash: [u8; 32],
}

impl fmt::Debug for AuthManager {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("AuthManager")
            .field("bootstrap", &"[REDACTED]")
            .field("authentication_required", &self.authentication_required)
            .finish_non_exhaustive()
    }
}

impl AuthManager {
    pub fn initialize(
        database: audiobookai_storage::Database,
        desktop_bootstrap: bool,
        listener_is_loopback: bool,
    ) -> Self {
        let bootstrap = desktop_bootstrap.then(|| {
            let plaintext = random_token();
            BootstrapSecret {
                hash: token_hash(plaintext.as_bytes()),
                plaintext: Zeroizing::new(plaintext),
            }
        });
        Self {
            database,
            bootstrap: Arc::new(Mutex::new(bootstrap)),
            authentication_required: desktop_bootstrap || !listener_is_loopback,
            login_failures: Arc::new(Mutex::new(VecDeque::new())),
        }
    }

    pub async fn take_bootstrap_nonce(&self) -> Option<Zeroizing<String>> {
        self.bootstrap
            .lock()
            .await
            .as_ref()
            .map(|secret| Zeroizing::new(secret.plaintext.to_string()))
    }

    async fn exchange_bootstrap(&self, nonce: &str) -> Result<SessionTokens, ServiceError> {
        let supplied_hash = token_hash(nonce.as_bytes());
        {
            let mut bootstrap = self.bootstrap.lock().await;
            let valid = bootstrap
                .as_ref()
                .is_some_and(|secret| constant_time_eq(&secret.hash, &supplied_hash));
            if !valid {
                return Err(ServiceError::Unauthorized(
                    "the desktop bootstrap nonce is invalid or has already been used".to_owned(),
                ));
            }
            bootstrap.take();
        }

        self.create_session("desktop").await
    }

    async fn create_session(&self, kind: &str) -> Result<SessionTokens, ServiceError> {
        let tokens = SessionTokens {
            session: Zeroizing::new(random_token()),
            csrf: Zeroizing::new(random_token()),
        };
        let now = Utc::now();
        let expires_at = now + Duration::hours(SESSION_HOURS);
        sqlx::query(
            "INSERT INTO auth_sessions \
             (id, kind, token_hash, csrf_hash, created_at, expires_at, last_seen_at, revoked_at, peer_address) \
             VALUES (?, ?, ?, ?, ?, ?, ?, NULL, NULL)",
        )
        .bind(Uuid::new_v4().to_string())
        .bind(kind)
        .bind(token_hash(tokens.session.as_bytes()).to_vec())
        .bind(token_hash(tokens.csrf.as_bytes()).to_vec())
        .bind(now.to_rfc3339())
        .bind(expires_at.to_rfc3339())
        .bind(now.to_rfc3339())
        .execute(self.database.pool())
        .await
        .map_err(|error| ServiceError::Storage(error.to_string()))?;
        Ok(tokens)
    }

    pub async fn configure_lan_password(
        &self,
        vault: &crate::secrets::SecretVault,
        password: &str,
    ) -> Result<(), ServiceError> {
        if password.chars().count() < 12 {
            return Err(ServiceError::InvalidRequest(
                "LAN password must contain at least 12 characters".to_owned(),
            ));
        }
        let mut salt = [0_u8; 16];
        rand::rng().fill_bytes(&mut salt);
        let mut hash = Zeroizing::new([0_u8; 32]);
        argon2::Argon2::default()
            .hash_password_into(password.as_bytes(), &salt, hash.as_mut())
            .map_err(|_| ServiceError::Internal("LAN password derivation failed".to_owned()))?;
        let verifier = PasswordVerifier {
            salt: URL_SAFE_NO_PAD.encode(salt),
            hash: URL_SAFE_NO_PAD.encode(hash.as_ref()),
        };
        let payload = Zeroizing::new(
            serde_json::to_vec(&verifier)
                .map_err(|error| ServiceError::Internal(error.to_string()))?,
        );
        let old_id = self.password_secret_id().await.ok();
        let reference = vault
            .store(
                audiobookai_core::SecretKind::LanPasswordVerifier,
                "LAN password verifier".to_owned(),
                payload.as_slice(),
            )
            .await?;
        sqlx::query(
            "INSERT INTO application_settings (key, updated_at, payload) \
             VALUES ('lan_password_secret', ?, ?) \
             ON CONFLICT(key) DO UPDATE SET updated_at = excluded.updated_at, payload = excluded.payload",
        )
        .bind(Utc::now().to_rfc3339())
        .bind(serde_json::json!({ "secretId": reference.id }).to_string())
        .execute(self.database.pool())
        .await
        .map_err(|error| ServiceError::Storage(error.to_string()))?;
        if let Some(old_id) = old_id.filter(|id| *id != reference.id) {
            let _ = vault.delete(old_id).await;
        }
        Ok(())
    }

    async fn login_with_password(
        &self,
        vault: &crate::secrets::SecretVault,
        password: &str,
    ) -> Result<SessionTokens, ServiceError> {
        self.enforce_login_throttle().await?;
        let id = self.password_secret_id().await?;
        let payload = vault.expose(id).await?;
        let verifier: PasswordVerifier = serde_json::from_slice(payload.as_slice())
            .map_err(|_| ServiceError::Internal("LAN password verifier is invalid".to_owned()))?;
        let salt = URL_SAFE_NO_PAD
            .decode(verifier.salt)
            .map_err(|_| ServiceError::Internal("LAN password salt is invalid".to_owned()))?;
        let expected = URL_SAFE_NO_PAD
            .decode(verifier.hash)
            .map_err(|_| ServiceError::Internal("LAN password hash is invalid".to_owned()))?;
        let mut supplied = Zeroizing::new([0_u8; 32]);
        argon2::Argon2::default()
            .hash_password_into(password.as_bytes(), &salt, supplied.as_mut())
            .map_err(|_| ServiceError::Internal("LAN password derivation failed".to_owned()))?;
        if !constant_time_eq(&expected, supplied.as_ref()) {
            self.login_failures.lock().await.push_back(Utc::now());
            return Err(ServiceError::Unauthorized(
                "the LAN password is invalid".to_owned(),
            ));
        }
        self.login_failures.lock().await.clear();
        self.create_session("lan_browser").await
    }

    async fn password_secret_id(&self) -> Result<audiobookai_core::SecretId, ServiceError> {
        let payload = sqlx::query_scalar::<_, String>(
            "SELECT payload FROM application_settings WHERE key = 'lan_password_secret'",
        )
        .fetch_optional(self.database.pool())
        .await
        .map_err(|error| ServiceError::Storage(error.to_string()))?
        .ok_or_else(|| {
            ServiceError::Conflict("LAN password authentication is not configured".to_owned())
        })?;
        let value: serde_json::Value = serde_json::from_str(&payload)
            .map_err(|error| ServiceError::Internal(error.to_string()))?;
        audiobookai_core::SecretId::from_str(
            value
                .get("secretId")
                .and_then(serde_json::Value::as_str)
                .ok_or_else(|| {
                    ServiceError::Internal("LAN password reference is invalid".to_owned())
                })?,
        )
        .map_err(|_| ServiceError::Internal("LAN password reference is invalid".to_owned()))
    }

    async fn enforce_login_throttle(&self) -> Result<(), ServiceError> {
        let cutoff = Utc::now() - Duration::minutes(1);
        let mut failures = self.login_failures.lock().await;
        while failures.front().is_some_and(|time| *time < cutoff) {
            failures.pop_front();
        }
        if failures.len() >= 5 {
            return Err(ServiceError::RateLimited(
                "too many failed LAN login attempts; retry in one minute".to_owned(),
            ));
        }
        Ok(())
    }

    async fn validate_session(
        &self,
        session_token: &str,
        csrf_token: Option<&str>,
        require_csrf: bool,
    ) -> Result<(), ServiceError> {
        let session_hash = token_hash(session_token.as_bytes());
        let rows = sqlx::query(
            "SELECT id, token_hash, csrf_hash, expires_at FROM auth_sessions \
             WHERE revoked_at IS NULL AND expires_at > ?",
        )
        .bind(Utc::now().to_rfc3339())
        .fetch_all(self.database.pool())
        .await
        .map_err(|error| ServiceError::Storage(error.to_string()))?;

        let mut matched_id = None;
        for row in rows {
            let stored_hash = row.get::<Vec<u8>, _>("token_hash");
            if constant_time_eq(&stored_hash, &session_hash) {
                if require_csrf {
                    let supplied = csrf_token.ok_or_else(|| {
                        ServiceError::Forbidden("a CSRF token is required".to_owned())
                    })?;
                    let supplied_hash = token_hash(supplied.as_bytes());
                    let stored_csrf =
                        row.get::<Option<Vec<u8>>, _>("csrf_hash").ok_or_else(|| {
                            ServiceError::Forbidden("the session has no CSRF binding".to_owned())
                        })?;
                    if !constant_time_eq(&stored_csrf, &supplied_hash) {
                        return Err(ServiceError::Forbidden(
                            "the CSRF token does not match this session".to_owned(),
                        ));
                    }
                }
                matched_id = Some(row.get::<String, _>("id"));
                break;
            }
        }
        let id = matched_id.ok_or_else(|| {
            ServiceError::Unauthorized("the session is missing, expired, or revoked".to_owned())
        })?;
        sqlx::query("UPDATE auth_sessions SET last_seen_at = ? WHERE id = ?")
            .bind(Utc::now().to_rfc3339())
            .bind(id)
            .execute(self.database.pool())
            .await
            .map_err(|error| ServiceError::Storage(error.to_string()))?;
        Ok(())
    }

    pub async fn revoke_lan_sessions(&self) -> Result<u64, ServiceError> {
        let result = sqlx::query(
            "UPDATE auth_sessions SET revoked_at = ? \
             WHERE kind = 'lan_browser' AND revoked_at IS NULL",
        )
        .bind(Utc::now().to_rfc3339())
        .execute(self.database.pool())
        .await
        .map_err(|error| ServiceError::Storage(error.to_string()))?;
        Ok(result.rows_affected())
    }

    pub async fn lan_status(&self) -> Result<LanAuthStatus, ServiceError> {
        let password_configured = sqlx::query_scalar::<_, i64>(
            "SELECT EXISTS(SELECT 1 FROM application_settings WHERE key = 'lan_password_secret')",
        )
        .fetch_one(self.database.pool())
        .await
        .map_err(|error| ServiceError::Storage(error.to_string()))?
            != 0;
        let api_token_count = sqlx::query_scalar::<_, i64>(
            "SELECT COUNT(*) FROM api_tokens WHERE revoked_at IS NULL \
             AND (expires_at IS NULL OR expires_at > ?)",
        )
        .bind(Utc::now().to_rfc3339())
        .fetch_one(self.database.pool())
        .await
        .map_err(|error| ServiceError::Storage(error.to_string()))?;
        let active_sessions = sqlx::query_scalar::<_, i64>(
            "SELECT COUNT(*) FROM auth_sessions WHERE kind = 'lan_browser' \
             AND revoked_at IS NULL AND expires_at > ?",
        )
        .bind(Utc::now().to_rfc3339())
        .fetch_one(self.database.pool())
        .await
        .map_err(|error| ServiceError::Storage(error.to_string()))?;
        Ok(LanAuthStatus {
            password_configured,
            api_token_count: usize::try_from(api_token_count).unwrap_or(usize::MAX),
            active_sessions: usize::try_from(active_sessions).unwrap_or(usize::MAX),
        })
    }

    pub async fn list_api_tokens(&self) -> Result<Vec<ApiTokenSummary>, ServiceError> {
        let rows = sqlx::query(
            "SELECT id, label, created_at, last_used_at FROM api_tokens \
             WHERE revoked_at IS NULL AND (expires_at IS NULL OR expires_at > ?) \
             ORDER BY created_at DESC",
        )
        .bind(Utc::now().to_rfc3339())
        .fetch_all(self.database.pool())
        .await
        .map_err(|error| ServiceError::Storage(error.to_string()))?;
        rows.into_iter()
            .map(|row| {
                let created_at = row.get::<String, _>("created_at").parse().map_err(|_| {
                    ServiceError::Internal("API token creation time is invalid".to_owned())
                })?;
                let last_used_at = row
                    .get::<Option<String>, _>("last_used_at")
                    .map(|value| value.parse())
                    .transpose()
                    .map_err(|_| {
                        ServiceError::Internal("API token use time is invalid".to_owned())
                    })?;
                Ok(ApiTokenSummary {
                    id: row.get("id"),
                    label: row.get("label"),
                    created_at,
                    last_used_at,
                })
            })
            .collect()
    }

    pub async fn issue_api_token(&self, label: String) -> Result<IssuedApiToken, ServiceError> {
        let label = label.trim().to_owned();
        if label.is_empty() || label.chars().count() > 80 {
            return Err(ServiceError::InvalidRequest(
                "API token label must contain 1 to 80 characters".to_owned(),
            ));
        }
        let token = Zeroizing::new(random_token());
        let id = Uuid::new_v4().to_string();
        let created_at = Utc::now();
        sqlx::query(
            "INSERT INTO api_tokens \
             (id, label, token_hash, created_at, last_used_at, expires_at, revoked_at) \
             VALUES (?, ?, ?, ?, NULL, NULL, NULL)",
        )
        .bind(&id)
        .bind(&label)
        .bind(token_hash(token.as_bytes()).to_vec())
        .bind(created_at.to_rfc3339())
        .execute(self.database.pool())
        .await
        .map_err(|error| ServiceError::Storage(error.to_string()))?;
        Ok(IssuedApiToken {
            id,
            token,
            label,
            created_at,
        })
    }

    pub async fn revoke_api_token(&self, id: &str) -> Result<(), ServiceError> {
        let result =
            sqlx::query("UPDATE api_tokens SET revoked_at = ? WHERE id = ? AND revoked_at IS NULL")
                .bind(Utc::now().to_rfc3339())
                .bind(id)
                .execute(self.database.pool())
                .await
                .map_err(|error| ServiceError::Storage(error.to_string()))?;
        if result.rows_affected() == 0 {
            return Err(ServiceError::NotFound);
        }
        Ok(())
    }

    async fn validate_api_token(&self, token: &str) -> Result<(), ServiceError> {
        let supplied_hash = token_hash(token.as_bytes());
        let rows = sqlx::query(
            "SELECT id, token_hash FROM api_tokens WHERE revoked_at IS NULL \
             AND (expires_at IS NULL OR expires_at > ?)",
        )
        .bind(Utc::now().to_rfc3339())
        .fetch_all(self.database.pool())
        .await
        .map_err(|error| ServiceError::Storage(error.to_string()))?;
        let id = rows
            .into_iter()
            .find_map(|row| {
                constant_time_eq(&row.get::<Vec<u8>, _>("token_hash"), &supplied_hash)
                    .then(|| row.get::<String, _>("id"))
            })
            .ok_or_else(|| ServiceError::Unauthorized("the API token is invalid".to_owned()))?;
        sqlx::query("UPDATE api_tokens SET last_used_at = ? WHERE id = ?")
            .bind(Utc::now().to_rfc3339())
            .bind(id)
            .execute(self.database.pool())
            .await
            .map_err(|error| ServiceError::Storage(error.to_string()))?;
        Ok(())
    }
}

struct SessionTokens {
    session: Zeroizing<String>,
    csrf: Zeroizing<String>,
}

#[derive(Deserialize, Serialize)]
struct PasswordVerifier {
    salt: String,
    hash: String,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct BootstrapRequest {
    nonce: Zeroizing<String>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct BootstrapResponse {
    authenticated: bool,
    expires_in_seconds: i64,
}

#[derive(Deserialize)]
struct LoginRequest {
    password: Zeroizing<String>,
}

pub fn router(state: Arc<AppState>) -> Router {
    Router::new()
        .route("/api/v1/auth/bootstrap", post(bootstrap))
        .route("/api/v1/auth/login", post(login))
        .with_state(state)
}

async fn bootstrap(
    State(state): State<Arc<AppState>>,
    Json(input): Json<BootstrapRequest>,
) -> Result<Response, ServiceError> {
    let tokens = state.auth.exchange_bootstrap(input.nonce.as_str()).await?;
    session_response(&tokens, state.config.uses_tls())
}

async fn login(
    State(state): State<Arc<AppState>>,
    Json(input): Json<LoginRequest>,
) -> Result<Response, ServiceError> {
    let tokens = state
        .auth
        .login_with_password(&state.secrets, input.password.as_str())
        .await?;
    session_response(&tokens, state.config.uses_tls())
}

fn session_response(tokens: &SessionTokens, secure: bool) -> Result<Response, ServiceError> {
    let mut response = (
        StatusCode::OK,
        Json(BootstrapResponse {
            authenticated: true,
            expires_in_seconds: SESSION_HOURS * 60 * 60,
        }),
    )
        .into_response();
    let secure_attribute = if secure { "; Secure" } else { "" };
    let session_cookie = format!(
        "{SESSION_COOKIE}={}; Path=/; HttpOnly; SameSite=Strict; Max-Age={}{secure_attribute}",
        tokens.session.as_str(),
        SESSION_HOURS * 60 * 60
    );
    let csrf_cookie = format!(
        "{CSRF_COOKIE}={}; Path=/; SameSite=Strict; Max-Age={}{secure_attribute}",
        tokens.csrf.as_str(),
        SESSION_HOURS * 60 * 60
    );
    response.headers_mut().append(
        header::SET_COOKIE,
        session_cookie
            .parse()
            .map_err(|_| ServiceError::Internal("could not create session cookie".to_owned()))?,
    );
    response.headers_mut().append(
        header::SET_COOKIE,
        csrf_cookie
            .parse()
            .map_err(|_| ServiceError::Internal("could not create CSRF cookie".to_owned()))?,
    );
    Ok(response)
}

/// Reject DNS-rebinding and cross-origin requests before serving either the
/// dashboard or an API/authentication endpoint.
pub async fn enforce_authority(
    State(state): State<Arc<AppState>>,
    request: Request<Body>,
    next: Next,
) -> Result<Response, ServiceError> {
    validate_host_and_origin(&state.config, request.headers())?;
    Ok(next.run(request).await)
}

pub async fn require_session(
    State(state): State<Arc<AppState>>,
    request: Request<Body>,
    next: Next,
) -> Result<Response, ServiceError> {
    let path = request.uri().path();
    if path == "/api/v1/health"
        || matches!(path, "/api/v1/auth/bootstrap" | "/api/v1/auth/login")
        || request.method() == Method::OPTIONS
        || !state.auth.authentication_required
    {
        return Ok(next.run(request).await);
    }
    if let Some(token) = request
        .headers()
        .get(header::AUTHORIZATION)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.strip_prefix("Bearer "))
    {
        state.auth.validate_api_token(token).await?;
        return Ok(next.run(request).await);
    }
    let session = cookie_value(request.headers(), SESSION_COOKIE).ok_or_else(|| {
        ServiceError::Unauthorized("an authenticated AudiobookAI session is required".to_owned())
    })?;
    let unsafe_method = !matches!(
        *request.method(),
        Method::GET | Method::HEAD | Method::OPTIONS
    );
    let csrf = request
        .headers()
        .get("x-csrf-token")
        .and_then(|value| value.to_str().ok());
    state
        .auth
        .validate_session(session, csrf, unsafe_method)
        .await?;
    Ok(next.run(request).await)
}

fn validate_host_and_origin(
    config: &crate::ServiceConfig,
    headers: &HeaderMap,
) -> Result<(), ServiceError> {
    let host = headers
        .get(header::HOST)
        .and_then(|value| value.to_str().ok())
        .ok_or_else(|| ServiceError::Forbidden("the Host header is required".to_owned()))?;
    let authority = Authority::from_str(host)
        .map_err(|_| ServiceError::Forbidden("the Host header is invalid".to_owned()))?;
    let host_name = normalize_host(authority.host());
    let expected_port = config.bind.port();
    let authority_port = authority.port_u16().unwrap_or_else(|| default_port(config));
    if authority_port != expected_port || !host_is_allowed(config, &host_name) {
        return Err(ServiceError::Forbidden(
            "the Host header does not identify this service authority".to_owned(),
        ));
    }

    if let Some(origin) = headers
        .get(header::ORIGIN)
        .and_then(|value| value.to_str().ok())
    {
        if trusted_tauri_origin(config, origin) {
            return Ok(());
        }
        let origin = url::Url::parse(origin)
            .map_err(|_| ServiceError::Forbidden("the Origin header is invalid".to_owned()))?;
        let origin_is_bare = origin.username().is_empty()
            && origin.password().is_none()
            && origin.path() == "/"
            && origin.query().is_none()
            && origin.fragment().is_none();
        let same_authority = origin.scheme() == config.scheme()
            && origin.port_or_known_default() == Some(expected_port)
            && origin
                .host_str()
                .map(normalize_host)
                .is_some_and(|origin_host| origin_host == host_name);
        if !origin_is_bare || !same_authority {
            return Err(ServiceError::Forbidden(
                "the Origin header is not trusted by this service".to_owned(),
            ));
        }
    }
    Ok(())
}

fn default_port(config: &crate::ServiceConfig) -> u16 {
    if config.uses_tls() { 443 } else { 80 }
}

fn host_is_allowed(config: &crate::ServiceConfig, candidate: &str) -> bool {
    if config.bind.ip().is_loopback() {
        return candidate.eq_ignore_ascii_case("localhost")
            || normalize_host(&config.bind.ip().to_string()) == candidate;
    }

    (!config.bind.ip().is_unspecified()
        && normalize_host(&config.bind.ip().to_string()) == candidate)
        || config
            .lan_hostnames
            .iter()
            .any(|hostname| normalize_host(hostname) == candidate)
}

fn normalize_host(host: &str) -> String {
    host.trim_start_matches('[')
        .trim_end_matches(']')
        .parse::<IpAddr>()
        .map_or_else(|_| host.to_ascii_lowercase(), |address| address.to_string())
}

fn trusted_tauri_origin(config: &crate::ServiceConfig, origin: &str) -> bool {
    config.desktop_bootstrap
        && config.bind.ip().is_loopback()
        && matches!(
            origin,
            "tauri://localhost" | "http://tauri.localhost" | "https://tauri.localhost"
        )
}

fn cookie_value<'a>(headers: &'a HeaderMap, name: &str) -> Option<&'a str> {
    headers
        .get(header::COOKIE)?
        .to_str()
        .ok()?
        .split(';')
        .map(str::trim)
        .find_map(|cookie| cookie.strip_prefix(name)?.strip_prefix('='))
}

fn random_token() -> String {
    let mut bytes = [0_u8; 32];
    rand::rng().fill_bytes(&mut bytes);
    URL_SAFE_NO_PAD.encode(bytes)
}

fn token_hash(token: &[u8]) -> [u8; 32] {
    *blake3::hash(token).as_bytes()
}

fn constant_time_eq(left: &[u8], right: &[u8]) -> bool {
    if left.len() != right.len() {
        return false;
    }
    left.iter()
        .zip(right)
        .fold(0_u8, |difference, (left, right)| {
            difference | (left ^ right)
        })
        == 0
}

#[cfg(test)]
mod tests {
    use super::*;

    fn service_config(bind: &str, tls: bool) -> crate::ServiceConfig {
        crate::ServiceConfig {
            bind: bind.parse().expect("address"),
            data_dir: std::path::PathBuf::from("test-data"),
            bundled_sidecar_dir: None,
            tls: tls.then(|| crate::TlsConfig {
                certificate_chain_path: std::path::PathBuf::from("certificate.pem"),
                private_key_path: std::path::PathBuf::from("private-key.pem"),
            }),
            lan_hostnames: Vec::new(),
            allow_insecure_lan: false,
            desktop_bootstrap: false,
        }
    }

    #[test]
    fn token_comparison_is_exact() {
        assert!(constant_time_eq(b"same", b"same"));
        assert!(!constant_time_eq(b"same", b"diff"));
        assert!(!constant_time_eq(b"short", b"longer"));
    }

    #[test]
    fn parses_named_cookie() {
        let mut headers = HeaderMap::new();
        headers.insert(
            header::COOKIE,
            "one=1; audiobookai_session=secret; two=2".parse().unwrap(),
        );
        assert_eq!(cookie_value(&headers, SESSION_COOKIE), Some("secret"));
    }

    #[test]
    fn loopback_authority_requires_the_selected_port() {
        let config = service_config("127.0.0.1:7788", false);
        let mut headers = HeaderMap::new();
        headers.insert(header::HOST, "localhost:7788".parse().unwrap());
        headers.insert(header::ORIGIN, "http://localhost:7788".parse().unwrap());
        validate_host_and_origin(&config, &headers).expect("matching authority");

        headers.insert(header::HOST, "localhost:7789".parse().unwrap());
        assert!(matches!(
            validate_host_and_origin(&config, &headers),
            Err(ServiceError::Forbidden(_))
        ));
    }

    #[test]
    fn lan_origin_requires_exact_https_authority() {
        let mut config = service_config("0.0.0.0:8443", true);
        config.lan_hostnames.push("reader.home.arpa".to_owned());
        let mut headers = HeaderMap::new();
        headers.insert(header::HOST, "reader.home.arpa:8443".parse().unwrap());
        headers.insert(
            header::ORIGIN,
            "https://reader.home.arpa:8443".parse().unwrap(),
        );
        validate_host_and_origin(&config, &headers).expect("matching TLS authority");

        for untrusted in [
            "http://reader.home.arpa:8443",
            "https://reader.home.arpa:9443",
            "https://other.home.arpa:8443",
        ] {
            headers.insert(header::ORIGIN, untrusted.parse().unwrap());
            assert!(matches!(
                validate_host_and_origin(&config, &headers),
                Err(ServiceError::Forbidden(_))
            ));
        }
    }

    #[test]
    fn tls_sessions_set_secure_cookies() {
        let response = session_response(
            &SessionTokens {
                session: Zeroizing::new("session".to_owned()),
                csrf: Zeroizing::new("csrf".to_owned()),
            },
            true,
        )
        .expect("response");
        let cookies = response
            .headers()
            .get_all(header::SET_COOKIE)
            .iter()
            .map(|value| value.to_str().expect("cookie"))
            .collect::<Vec<_>>();
        assert_eq!(cookies.len(), 2);
        assert!(cookies.iter().all(|cookie| cookie.contains("; Secure")));
        assert!(cookies[0].contains("HttpOnly"));
        assert!(
            cookies
                .iter()
                .all(|cookie| cookie.contains("SameSite=Strict"))
        );
    }
}
