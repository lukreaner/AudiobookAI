//! Service-facing provider runtime.
//!
//! This module keeps provider credentials out of serializable profile records, owns every
//! managed child handle it uses, and centralizes retry decisions that can affect billing.
#![allow(clippy::missing_errors_doc, clippy::too_many_lines)]

mod factory;
mod manager;
mod profile;
mod retry;

pub use factory::{ProviderAdapterBundle, ProviderAdapterFactory};
pub use manager::{ManagedProcessView, ProviderRuntime, RuntimeError, ShutdownReport};
pub use profile::{CredentialMaterial, RuntimeAdapterKind, RuntimeModelControl, RuntimeProfile};
pub use retry::{
    AttemptNumber, FailureClass, NoopRetryJournal, RetryEvent, RetryEventOutcome, RetryExecution,
    RetryExecutionError, RetryJournal, RetryJournalError, RetryPolicy, RetryPolicyError,
    classify_provider_error, execute_with_retry, timeout_after_dispatch,
};
