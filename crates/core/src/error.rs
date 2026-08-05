use serde::{Deserialize, Serialize};
use thiserror::Error;

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct ValidationIssue {
    pub path: String,
    pub code: String,
    pub message: String,
}

impl ValidationIssue {
    #[must_use]
    pub fn new(
        path: impl Into<String>,
        code: impl Into<String>,
        message: impl Into<String>,
    ) -> Self {
        Self {
            path: path.into(),
            code: code.into(),
            message: message.into(),
        }
    }
}

#[derive(Clone, Debug, Deserialize, Error, PartialEq, Serialize)]
#[serde(tag = "code", rename_all = "snake_case")]
pub enum DomainError {
    #[error("domain validation failed")]
    Validation { issues: Vec<ValidationIssue> },
    #[error("invalid {entity} state transition from {from} to {to}")]
    InvalidTransition {
        entity: String,
        from: String,
        to: String,
    },
    #[error("{entity} was not found: {id}")]
    NotFound { entity: String, id: String },
    #[error("{entity} already exists: {id}")]
    Conflict { entity: String, id: String },
    #[error("operation is not supported: {message}")]
    Unsupported { message: String },
}

pub trait Validate {
    fn validation_issues(&self) -> Vec<ValidationIssue>;

    /// Validates the complete record.
    ///
    /// # Errors
    ///
    /// Returns [`DomainError::Validation`] with every discovered issue rather
    /// than stopping at the first invalid field.
    fn validate(&self) -> Result<(), DomainError> {
        let issues = self.validation_issues();
        if issues.is_empty() {
            Ok(())
        } else {
            Err(DomainError::Validation { issues })
        }
    }
}

pub(crate) fn require_non_empty(issues: &mut Vec<ValidationIssue>, path: &str, value: &str) {
    if value.trim().is_empty() {
        issues.push(ValidationIssue::new(
            path,
            "required",
            "value must not be empty",
        ));
    }
}

pub(crate) fn require_currency(issues: &mut Vec<ValidationIssue>, path: &str, value: &str) {
    if value.len() != 3 || !value.bytes().all(|byte| byte.is_ascii_uppercase()) {
        issues.push(ValidationIssue::new(
            path,
            "invalid_currency",
            "currency must be a three-letter uppercase ISO 4217 code",
        ));
    }
}
