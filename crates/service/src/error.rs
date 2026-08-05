use std::net::SocketAddr;

use axum::{
    Json,
    http::StatusCode,
    response::{IntoResponse, Response},
};
use serde::Serialize;
use thiserror::Error;

#[derive(Debug, Error)]
pub enum ServiceError {
    #[error("application data directory is unavailable")]
    DataDirectoryUnavailable,
    #[error("TLS configuration is required before binding to {0}")]
    TlsRequiredForLan(SocketAddr),
    #[error("invalid TLS configuration: {0}")]
    TlsConfiguration(String),
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),
    #[error("service task failed: {0}")]
    Join(#[from] tokio::task::JoinError),
    #[error("invalid request: {0}")]
    InvalidRequest(String),
    #[error("resource not found")]
    NotFound,
    #[error("resource conflict: {0}")]
    Conflict(String),
    #[error("authentication required: {0}")]
    Unauthorized(String),
    #[error("request forbidden: {0}")]
    Forbidden(String),
    #[error("rate limited: {0}")]
    RateLimited(String),
    #[error("storage error: {0}")]
    Storage(String),
    #[error("internal service error: {0}")]
    Internal(String),
}

#[derive(Debug, Serialize)]
pub struct ProblemDetails {
    #[serde(rename = "type")]
    pub kind: &'static str,
    pub title: &'static str,
    pub status: u16,
    pub detail: String,
}

impl IntoResponse for ServiceError {
    fn into_response(self) -> Response {
        let (status, kind, title, detail) = match self {
            Self::InvalidRequest(detail) => (
                StatusCode::BAD_REQUEST,
                "urn:audiobookai:problem:invalid-request",
                "Invalid request",
                detail,
            ),
            Self::NotFound => (
                StatusCode::NOT_FOUND,
                "urn:audiobookai:problem:not-found",
                "Not found",
                self.to_string(),
            ),
            Self::Conflict(detail) => (
                StatusCode::CONFLICT,
                "urn:audiobookai:problem:conflict",
                "Conflict",
                detail,
            ),
            Self::Unauthorized(detail) => (
                StatusCode::UNAUTHORIZED,
                "urn:audiobookai:problem:unauthorized",
                "Authentication required",
                detail,
            ),
            Self::Forbidden(detail) => (
                StatusCode::FORBIDDEN,
                "urn:audiobookai:problem:forbidden",
                "Forbidden",
                detail,
            ),
            Self::RateLimited(detail) => (
                StatusCode::TOO_MANY_REQUESTS,
                "urn:audiobookai:problem:rate-limited",
                "Too many requests",
                detail,
            ),
            Self::TlsRequiredForLan(address) => (
                StatusCode::PRECONDITION_FAILED,
                "urn:audiobookai:problem:tls-required",
                "TLS required",
                format!("configure TLS before binding to {address}"),
            ),
            other => {
                tracing::error!(diagnostic_code = "service.request.failed", error = %other, "service request failed");
                (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    "urn:audiobookai:problem:internal",
                    "Internal error",
                    "the operation could not be completed".to_owned(),
                )
            }
        };
        (
            status,
            Json(ProblemDetails {
                kind,
                title,
                status: status.as_u16(),
                detail,
            }),
        )
            .into_response()
    }
}
