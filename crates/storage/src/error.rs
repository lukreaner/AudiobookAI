use std::path::PathBuf;

use audiobookai_core::{BudgetId, DomainError};
use thiserror::Error;

pub type Result<T, E = StorageError> = std::result::Result<T, E>;

#[derive(Debug, Error)]
pub enum StorageError {
    #[error("database error: {0}")]
    Database(#[from] sqlx::Error),
    #[error("database migration error: {0}")]
    Migration(#[from] sqlx::migrate::MigrateError),
    #[error("file system error: {0}")]
    Io(#[from] std::io::Error),
    #[error("stored JSON is invalid: {0}")]
    Serialization(#[from] serde_json::Error),
    #[error(transparent)]
    Domain(#[from] DomainError),
    #[error("another AudiobookAI writer owns {0}")]
    AlreadyRunning(PathBuf),
    #[error("{entity} was not found: {id}")]
    NotFound { entity: &'static str, id: String },
    #[error("{entity} already exists: {id}")]
    Conflict { entity: &'static str, id: String },
    #[error("stale {entity} revision for {id}")]
    StaleRevision { entity: &'static str, id: String },
    #[error(
        "budget {budget_id} has insufficient capacity: requested {requested}, remaining {remaining}"
    )]
    BudgetExceeded {
        budget_id: BudgetId,
        requested: i64,
        remaining: i64,
    },
    #[error("idempotency key was reused with a different request payload")]
    IdempotencyMismatch,
    #[error("stored data is invalid: {0}")]
    InvalidData(String),
}

impl StorageError {
    pub(crate) fn is_unique_violation(error: &sqlx::Error) -> bool {
        matches!(
            error,
            sqlx::Error::Database(database_error) if database_error.is_unique_violation()
        )
    }
}
