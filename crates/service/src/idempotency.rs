use std::sync::Arc;

use audiobookai_storage::repositories::{IdempotencyClaim, IdempotentResponse};
use axum::{
    body::{Body, to_bytes},
    extract::{Request, State},
    http::{Method, StatusCode, header},
    middleware::Next,
    response::{IntoResponse, Response},
};
use chrono::{Duration, Utc};

use crate::{AppState, ServiceError};

const MAX_JSON_BODY_BYTES: usize = 2 * 1024 * 1024;
const MAX_REPLAY_BODY_BYTES: usize = 16 * 1024 * 1024;

pub async fn enforce(
    State(state): State<Arc<AppState>>,
    request: Request,
    next: Next,
) -> Result<Response, ServiceError> {
    // These routes accept credentials, return a one-time credential, or accept model identifiers
    // into which a credential can be pasted accidentally. They must reach their handlers as opaque
    // streams: generic idempotency must never buffer, fingerprint, or replay their bodies. The
    // handlers retain their operation-specific validation and authorization boundaries.
    if is_secret_bearing_route(request.method(), request.uri().path()) {
        return Ok(next.run(request).await);
    }
    if matches!(
        *request.method(),
        Method::GET | Method::HEAD | Method::OPTIONS
    ) || request.uri().path() == "/api/v1/auth/bootstrap"
    {
        return Ok(next.run(request).await);
    }
    let content_type = request
        .headers()
        .get(header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .unwrap_or_default();
    // Multipart EPUBs remain streaming and bounded by the hardened importer. Buffering a
    // potentially 1 GiB upload solely to hash it would create a denial-of-service primitive.
    if !content_type.starts_with("application/json") {
        return Ok(next.run(request).await);
    }
    let key = request
        .headers()
        .get("idempotency-key")
        .and_then(|value| value.to_str().ok())
        .map(str::to_owned)
        .ok_or_else(|| {
            ServiceError::InvalidRequest(
                "Idempotency-Key is required for state-changing JSON requests".to_owned(),
            )
        })?;
    if key.len() > 200 || key.chars().any(char::is_whitespace) {
        return Err(ServiceError::InvalidRequest(
            "Idempotency-Key must be at most 200 non-whitespace characters".to_owned(),
        ));
    }

    let (parts, body) = request.into_parts();
    let body = to_bytes(body, MAX_JSON_BODY_BYTES)
        .await
        .map_err(|_| ServiceError::InvalidRequest("JSON request body exceeds 2 MiB".to_owned()))?;
    let scope = format!("{} {}", parts.method, parts.uri.path());
    let mut request_fingerprint = blake3::Hasher::new();
    request_fingerprint.update(parts.method.as_str().as_bytes());
    request_fingerprint.update(parts.uri.to_string().as_bytes());
    request_fingerprint.update(&body);
    let request_hash = request_fingerprint.finalize().to_hex().to_string();
    let repository = state.database.repositories().idempotency;
    let now = Utc::now();
    let claim = repository
        .claim(&scope, &key, &request_hash, now, now + Duration::hours(24))
        .await
        .map_err(|error| ServiceError::Conflict(error.to_string()))?;
    match claim {
        IdempotencyClaim::Replay(stored) => return replay_response(stored),
        IdempotencyClaim::InProgress => {
            return Err(ServiceError::Conflict(
                "a request with this idempotency key is still in progress".to_owned(),
            ));
        }
        IdempotencyClaim::Acquired => {}
    }

    let response = next.run(Request::from_parts(parts, Body::from(body))).await;
    let (parts, body) = response.into_parts();
    let status = parts.status;
    let Ok(response_body) = to_bytes(body, MAX_REPLAY_BODY_BYTES).await else {
        let _ = repository.forget(&scope, &key).await;
        return Err(ServiceError::Internal(
            "response exceeded the idempotency replay limit".to_owned(),
        ));
    };
    if status.is_server_error() {
        let _ = repository.forget(&scope, &key).await;
    } else {
        let replay = IdempotentResponse {
            status: status.as_u16(),
            body: response_body.to_vec(),
            content_type: parts
                .headers
                .get(header::CONTENT_TYPE)
                .and_then(|value| value.to_str().ok())
                .unwrap_or("application/octet-stream")
                .to_owned(),
        };
        repository
            .complete(&scope, &key, &request_hash, &replay)
            .await
            .map_err(|error| ServiceError::Storage(error.to_string()))?;
    }
    Ok(Response::from_parts(parts, Body::from(response_body)))
}

fn is_secret_bearing_route(method: &Method, path: &str) -> bool {
    match (method, path) {
        (
            &Method::POST,
            "/api/v1/providers"
            | "/api/v1/provider-models/discover"
            | "/api/v1/providers/mlx-audio/models"
            | "/api/v1/secrets/unlock"
            | "/api/v1/settings/lan/tokens",
        )
        | (&Method::PUT, "/api/v1/settings/lan/password") => true,
        (&Method::PATCH, path) => path
            .strip_prefix("/api/v1/providers/")
            .filter(|id| !id.is_empty() && !id.contains('/'))
            .is_some_and(|id| uuid::Uuid::parse_str(id).is_ok()),
        _ => is_sensitive_provider_model_route(method, path),
    }
}

fn is_sensitive_provider_model_route(method: &Method, path: &str) -> bool {
    let Some(remainder) = path.strip_prefix("/api/v1/providers/") else {
        return false;
    };
    let mut segments = remainder.split('/');
    let (Some(provider_id), Some(resource)) = (segments.next(), segments.next()) else {
        return false;
    };
    if uuid::Uuid::parse_str(provider_id).is_err() {
        return false;
    }

    matches!(
        (method, resource, segments.next(), segments.next()),
        (&Method::POST | &Method::DELETE, "models", None, None)
            | (
                &Method::POST,
                "actions",
                Some("load-model" | "unload-model" | "switch-model"),
                None,
            )
    )
}

fn replay_response(stored: IdempotentResponse) -> Result<Response, ServiceError> {
    let status = StatusCode::from_u16(stored.status)
        .map_err(|_| ServiceError::Internal("stored response status is invalid".to_owned()))?;
    let mut response = (status, stored.body).into_response();
    response.headers_mut().insert(
        header::CONTENT_TYPE,
        stored
            .content_type
            .parse()
            .map_err(|_| ServiceError::Internal("stored content type is invalid".to_owned()))?,
    );
    response.headers_mut().insert(
        "idempotency-replayed",
        http::HeaderValue::from_static("true"),
    );
    Ok(response)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn secret_routes_bypass_body_fingerprinting_and_replay() {
        let provider_id = uuid::Uuid::new_v4();
        for (method, path) in [
            (Method::POST, "/api/v1/providers".to_owned()),
            (Method::POST, "/api/v1/provider-models/discover".to_owned()),
            (Method::PATCH, format!("/api/v1/providers/{provider_id}")),
            (Method::POST, "/api/v1/secrets/unlock".to_owned()),
            (Method::PUT, "/api/v1/settings/lan/password".to_owned()),
            (Method::POST, "/api/v1/settings/lan/tokens".to_owned()),
            (
                Method::POST,
                format!("/api/v1/providers/{provider_id}/models"),
            ),
            (
                Method::DELETE,
                format!("/api/v1/providers/{provider_id}/models"),
            ),
            (
                Method::POST,
                format!("/api/v1/providers/{provider_id}/actions/load-model"),
            ),
            (
                Method::POST,
                format!("/api/v1/providers/{provider_id}/actions/unload-model"),
            ),
            (
                Method::POST,
                format!("/api/v1/providers/{provider_id}/actions/switch-model"),
            ),
            (
                Method::POST,
                "/api/v1/providers/mlx-audio/models".to_owned(),
            ),
        ] {
            assert!(
                is_secret_bearing_route(&method, &path),
                "secret-bearing route was not excluded: {method} {path}"
            );
        }
    }

    #[test]
    fn provider_route_matching_requires_one_exact_uuid_segment() {
        let provider_id = uuid::Uuid::new_v4();
        for path in [
            "/api/v1/providers/not-a-uuid".to_owned(),
            format!("/api/v1/providers/{provider_id}/actions/start"),
            format!("/api/v1/providers/{provider_id}/"),
            format!("/api/v1/providers/{provider_id}suffix"),
            "/api/v1/providers/".to_owned(),
        ] {
            assert!(
                !is_secret_bearing_route(&Method::PATCH, &path),
                "non-profile route must not bypass idempotency: {path}"
            );
        }
        assert!(!is_secret_bearing_route(
            &Method::POST,
            &format!("/api/v1/providers/{provider_id}")
        ));
        assert!(!is_secret_bearing_route(
            &Method::GET,
            "/api/v1/settings/lan/tokens"
        ));
    }

    #[test]
    fn provider_model_route_matching_requires_exact_methods_and_segments() {
        let provider_id = uuid::Uuid::new_v4();
        for (method, path) in [
            (
                Method::POST,
                "/api/v1/providers/not-a-uuid/models".to_owned(),
            ),
            (
                Method::POST,
                format!("/api/v1/providers/{provider_id}/models/extra"),
            ),
            (
                Method::PUT,
                format!("/api/v1/providers/{provider_id}/models"),
            ),
            (
                Method::GET,
                format!("/api/v1/providers/{provider_id}/models"),
            ),
            (
                Method::POST,
                format!("/api/v1/providers/{provider_id}/actions/start"),
            ),
            (
                Method::POST,
                format!("/api/v1/providers/{provider_id}/actions/load-model/extra"),
            ),
            (
                Method::GET,
                format!("/api/v1/providers/{provider_id}/actions/load-model"),
            ),
            (
                Method::POST,
                "/api/v1/providers/not-a-uuid/actions/switch-model".to_owned(),
            ),
            (Method::GET, "/api/v1/providers/mlx-audio/models".to_owned()),
            (
                Method::POST,
                "/api/v1/providers/mlx-audio/models/extra".to_owned(),
            ),
        ] {
            assert!(
                !is_secret_bearing_route(&method, &path),
                "unrelated or malformed route must not bypass idempotency: {method} {path}"
            );
        }
    }
}
