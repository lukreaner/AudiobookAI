use std::time::Duration;

/// Failures returned by provider adapters. The variants intentionally carry no credentials.
#[derive(Debug, thiserror::Error)]
pub enum ProviderError {
    #[error("provider configuration is invalid: {0}")]
    Configuration(String),
    #[error("provider does not support {feature}")]
    Unsupported { feature: &'static str },
    #[error("provider authentication failed")]
    Authentication,
    #[error("provider rate limit reached")]
    RateLimited { retry_after: Option<Duration> },
    #[error("provider request timed out after dispatch; billing status is uncertain")]
    UncertainCharge,
    #[error("provider returned HTTP {status}: {message}")]
    Http { status: u16, message: String },
    #[error("provider transport failed: {0}")]
    Transport(String),
    #[error("provider returned malformed data: {0}")]
    InvalidResponse(String),
    #[error("managed process is not owned by this controller")]
    NotOwned,
    #[error("managed process was not found")]
    ProcessNotFound,
    #[error("managed process failed: {0}")]
    Process(String),
    #[error("operation was cancelled")]
    Cancelled,
}

impl ProviderError {
    pub fn from_status(status: u16, body: &[u8]) -> Self {
        match status {
            401 | 403 => Self::Authentication,
            429 => Self::RateLimited { retry_after: None },
            _ => Self::Http {
                status,
                message: sanitized_message(body),
            },
        }
    }
}

fn sanitized_message(body: &[u8]) -> String {
    // Provider-controlled messages can echo source text, request bodies, or credentials. Keep
    // only a short symbolic error code with a deliberately narrow alphabet; never retain prose.
    let code = serde_json::from_slice::<serde_json::Value>(body)
        .ok()
        .and_then(|value| {
            value
                .pointer("/error/code")
                .or_else(|| value.get("code"))
                .and_then(serde_json::Value::as_str)
                .map(ToOwned::to_owned)
        })
        .filter(|value| {
            !value.is_empty()
                && value.len() <= 64
                && value
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-' | b'.'))
        });
    code.map_or_else(
        || "provider returned an error response".to_owned(),
        |code| format!("provider error code {code}"),
    )
}

pub type Result<T> = std::result::Result<T, ProviderError>;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn provider_error_body_never_survives_as_diagnostic_text() {
        let body = br#"{"error":{"message":"echoed request content","code":"bad_request"}}"#;
        assert_eq!(sanitized_message(body), "provider error code bad_request");
        assert!(!sanitized_message(body).contains("echoed"));
    }

    #[test]
    fn rejects_unstructured_and_secret_shaped_error_codes() {
        assert_eq!(
            sanitized_message(br#"{"code":"contains spaces and request content"}"#),
            "provider returned an error response"
        );
        assert_eq!(
            sanitized_message(b"arbitrary provider prose"),
            "provider returned an error response"
        );
    }
}
