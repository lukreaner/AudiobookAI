use std::{fmt, future::Future, time::Duration};

use audiobookai_providers::ProviderError;
use chrono::{DateTime, Utc};
use futures::future::BoxFuture;

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub struct AttemptNumber(u16);

impl AttemptNumber {
    pub const fn get(self) -> u16 {
        self.0
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum FailureClass {
    Transient,
    RateLimited,
    Authentication,
    Validation,
    UncertainCharge,
    Cancelled,
    Permanent,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum RetryEventOutcome {
    Succeeded,
    Failed {
        class: FailureClass,
        will_retry: bool,
        retry_after: Option<Duration>,
    },
}

/// Secret-free event intended for durable attempt and usage ledgers.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RetryEvent {
    pub attempt: AttemptNumber,
    pub recorded_at: DateTime<Utc>,
    pub outcome: RetryEventOutcome,
}

#[derive(Debug, thiserror::Error)]
#[error("retry journal failed: {message}")]
pub struct RetryJournalError {
    message: String,
}

impl RetryJournalError {
    pub fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
        }
    }
}

/// Persists an attempt result before the runtime returns or dispatches another request.
pub trait RetryJournal: fmt::Debug + Send + Sync {
    fn record(&self, event: RetryEvent) -> BoxFuture<'_, Result<(), RetryJournalError>>;
}

#[derive(Clone, Copy, Debug, Default)]
pub struct NoopRetryJournal;

impl RetryJournal for NoopRetryJournal {
    fn record(&self, _event: RetryEvent) -> BoxFuture<'_, Result<(), RetryJournalError>> {
        Box::pin(async { Ok(()) })
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RetryPolicy {
    max_attempts: u16,
    base_delay: Duration,
    max_delay: Duration,
    jitter_basis_points: u16,
    retry_uncertain_charge: bool,
}

impl RetryPolicy {
    pub fn new(
        max_attempts: u16,
        base_delay: Duration,
        max_delay: Duration,
    ) -> Result<Self, RetryPolicyError> {
        if max_attempts == 0 {
            return Err(RetryPolicyError::NoAttempts);
        }
        if base_delay > max_delay {
            return Err(RetryPolicyError::InvalidDelayRange);
        }
        Ok(Self {
            max_attempts,
            base_delay,
            max_delay,
            jitter_basis_points: 2_000,
            retry_uncertain_charge: false,
        })
    }

    pub const fn max_attempts(&self) -> u16 {
        self.max_attempts
    }

    pub const fn retries_uncertain_charge(&self) -> bool {
        self.retry_uncertain_charge
    }

    pub fn with_jitter_basis_points(
        mut self,
        jitter_basis_points: u16,
    ) -> Result<Self, RetryPolicyError> {
        if jitter_basis_points > 10_000 {
            return Err(RetryPolicyError::InvalidJitter);
        }
        self.jitter_basis_points = jitter_basis_points;
        Ok(self)
    }

    /// Enables the reliability policy that may create duplicate provider charges.
    #[must_use]
    pub const fn with_uncertain_charge_retries(mut self, enabled: bool) -> Self {
        self.retry_uncertain_charge = enabled;
        self
    }

    fn delay_for(&self, attempt: AttemptNumber, hint: Option<Duration>) -> Duration {
        if let Some(hint) = hint {
            return hint.min(self.max_delay);
        }
        let exponent = u32::from(attempt.get().saturating_sub(1)).min(31);
        let factor = 1_u32.checked_shl(exponent).unwrap_or(u32::MAX);
        let base = self.base_delay.saturating_mul(factor).min(self.max_delay);
        if self.jitter_basis_points == 0 || base.is_zero() || base == self.max_delay {
            return base;
        }
        let available = self.max_delay.saturating_sub(base);
        let jitter_window = duration_fraction(base, self.jitter_basis_points).min(available);
        if jitter_window.is_zero() {
            return base;
        }
        let upper_nanos = u64::try_from(jitter_window.as_nanos()).unwrap_or(u64::MAX);
        let jitter_nanos = rand::random::<u64>() % upper_nanos.saturating_add(1);
        base.saturating_add(Duration::from_nanos(jitter_nanos))
    }
}

impl Default for RetryPolicy {
    fn default() -> Self {
        Self {
            // One initial dispatch plus the default three transient retries.
            max_attempts: 4,
            base_delay: Duration::from_millis(500),
            max_delay: Duration::from_secs(30),
            jitter_basis_points: 2_000,
            retry_uncertain_charge: false,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, thiserror::Error)]
pub enum RetryPolicyError {
    #[error("retry policy must allow at least one attempt")]
    NoAttempts,
    #[error("retry base delay may not exceed its maximum delay")]
    InvalidDelayRange,
    #[error("retry jitter may not exceed 10000 basis points")]
    InvalidJitter,
}

#[derive(Debug)]
pub struct RetryExecution<T> {
    pub value: T,
    pub attempts: AttemptNumber,
}

#[derive(Debug, thiserror::Error)]
pub enum RetryExecutionError {
    #[error("provider operation failed after {attempts:?}: {source}")]
    Provider {
        source: ProviderError,
        class: FailureClass,
        attempts: AttemptNumber,
    },
    #[error("could not durably record provider attempt {attempts:?}: {source}")]
    Journal {
        source: RetryJournalError,
        attempts: AttemptNumber,
    },
}

impl RetryExecutionError {
    pub const fn attempts(&self) -> AttemptNumber {
        match self {
            Self::Provider { attempts, .. } | Self::Journal { attempts, .. } => *attempts,
        }
    }

    pub const fn failure_class(&self) -> Option<FailureClass> {
        match self {
            Self::Provider { class, .. } => Some(*class),
            Self::Journal { .. } => None,
        }
    }
}

pub async fn execute_with_retry<T, J, F, Fut>(
    policy: &RetryPolicy,
    journal: &J,
    mut operation: F,
) -> Result<RetryExecution<T>, RetryExecutionError>
where
    J: RetryJournal + ?Sized,
    F: FnMut(AttemptNumber) -> Fut,
    Fut: Future<Output = Result<T, ProviderError>>,
{
    for raw_attempt in 1..=policy.max_attempts {
        let attempt = AttemptNumber(raw_attempt);
        match operation(attempt).await {
            Ok(value) => {
                persist_event(
                    journal,
                    RetryEvent {
                        attempt,
                        recorded_at: Utc::now(),
                        outcome: RetryEventOutcome::Succeeded,
                    },
                )
                .await
                .map_err(|source| RetryExecutionError::Journal {
                    source,
                    attempts: attempt,
                })?;
                return Ok(RetryExecution {
                    value,
                    attempts: attempt,
                });
            }
            Err(error) => {
                let class = classify_provider_error(&error);
                let has_attempt_remaining = raw_attempt < policy.max_attempts;
                let permitted = retry_permitted(class, policy);
                let will_retry = has_attempt_remaining && permitted;
                let delay = will_retry.then(|| policy.delay_for(attempt, retry_after_hint(&error)));
                persist_event(
                    journal,
                    RetryEvent {
                        attempt,
                        recorded_at: Utc::now(),
                        outcome: RetryEventOutcome::Failed {
                            class,
                            will_retry,
                            retry_after: delay,
                        },
                    },
                )
                .await
                .map_err(|source| RetryExecutionError::Journal {
                    source,
                    attempts: attempt,
                })?;
                if let Some(delay) = delay {
                    tokio::time::sleep(delay).await;
                } else {
                    return Err(RetryExecutionError::Provider {
                        source: error,
                        class,
                        attempts: attempt,
                    });
                }
            }
        }
    }
    unreachable!("validated retry policies always execute at least one attempt")
}

pub const fn classify_provider_error(error: &ProviderError) -> FailureClass {
    match error {
        ProviderError::Authentication => FailureClass::Authentication,
        ProviderError::Configuration(_)
        | ProviderError::Unsupported { .. }
        | ProviderError::InvalidResponse(_) => FailureClass::Validation,
        ProviderError::RateLimited { .. } | ProviderError::Http { status: 429, .. } => {
            FailureClass::RateLimited
        }
        ProviderError::UncertainCharge => FailureClass::UncertainCharge,
        ProviderError::Transport(_) | ProviderError::Process(_) => FailureClass::Transient,
        ProviderError::Http { status, .. } if *status == 408 || *status == 425 => {
            FailureClass::Transient
        }
        ProviderError::Http { status, .. } if *status >= 500 => FailureClass::Transient,
        ProviderError::Http { status, .. }
            if matches!(*status, 400 | 404 | 405 | 409 | 415 | 422) =>
        {
            FailureClass::Validation
        }
        ProviderError::Cancelled => FailureClass::Cancelled,
        ProviderError::NotOwned | ProviderError::ProcessNotFound | ProviderError::Http { .. } => {
            FailureClass::Permanent
        }
    }
}

/// Converts a timeout into a billing-safe provider error based on dispatch state.
pub fn timeout_after_dispatch(dispatched: bool) -> ProviderError {
    if dispatched {
        ProviderError::UncertainCharge
    } else {
        ProviderError::Transport("provider request timed out before dispatch".to_owned())
    }
}

fn retry_permitted(class: FailureClass, policy: &RetryPolicy) -> bool {
    matches!(class, FailureClass::Transient | FailureClass::RateLimited)
        || (matches!(class, FailureClass::UncertainCharge) && policy.retry_uncertain_charge)
}

fn retry_after_hint(error: &ProviderError) -> Option<Duration> {
    match error {
        ProviderError::RateLimited { retry_after } => *retry_after,
        _ => None,
    }
}

async fn persist_event<J: RetryJournal + ?Sized>(
    journal: &J,
    event: RetryEvent,
) -> Result<(), RetryJournalError> {
    journal.record(event).await
}

fn duration_fraction(duration: Duration, basis_points: u16) -> Duration {
    let nanos = duration.as_nanos().saturating_mul(u128::from(basis_points)) / 10_000;
    Duration::from_nanos(u64::try_from(nanos).unwrap_or(u64::MAX))
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex as StdMutex;

    use futures::future;

    use super::*;

    #[derive(Debug, Default)]
    struct RecordingJournal {
        events: StdMutex<Vec<RetryEvent>>,
    }

    impl RetryJournal for RecordingJournal {
        fn record(&self, event: RetryEvent) -> BoxFuture<'_, Result<(), RetryJournalError>> {
            self.events.lock().unwrap().push(event);
            Box::pin(async { Ok(()) })
        }
    }

    fn immediate_policy(max_attempts: u16) -> RetryPolicy {
        RetryPolicy::new(max_attempts, Duration::ZERO, Duration::ZERO)
            .unwrap()
            .with_jitter_basis_points(0)
            .unwrap()
    }

    #[test]
    fn classifies_non_retryable_and_transient_failures() {
        assert_eq!(
            classify_provider_error(&ProviderError::Authentication),
            FailureClass::Authentication
        );
        assert_eq!(
            classify_provider_error(&ProviderError::Configuration("bad".to_owned())),
            FailureClass::Validation
        );
        assert_eq!(
            classify_provider_error(&ProviderError::Http {
                status: 503,
                message: "unavailable".to_owned(),
            }),
            FailureClass::Transient
        );
        assert_eq!(
            classify_provider_error(&timeout_after_dispatch(true)),
            FailureClass::UncertainCharge
        );
    }

    #[tokio::test]
    async fn retries_transient_failures_with_a_hard_bound() {
        let journal = RecordingJournal::default();
        let mut calls = 0_u16;
        let result = execute_with_retry(&immediate_policy(3), &journal, |_| {
            calls = calls.saturating_add(1);
            future::ready(Err::<(), _>(ProviderError::Transport(
                "temporary".to_owned(),
            )))
        })
        .await
        .unwrap_err();
        assert_eq!(calls, 3);
        assert_eq!(result.attempts().get(), 3);
        assert_eq!(result.failure_class(), Some(FailureClass::Transient));
        let events = journal.events.lock().unwrap();
        assert_eq!(events.len(), 3);
        assert!(matches!(
            events.last().map(|event| &event.outcome),
            Some(RetryEventOutcome::Failed {
                will_retry: false,
                ..
            })
        ));
    }

    #[tokio::test]
    async fn uncertain_charge_requires_explicit_duplicate_billing_permission() {
        let journal = RecordingJournal::default();
        let mut blocked_calls = 0_u16;
        let blocked = execute_with_retry(&immediate_policy(3), &journal, |_| {
            blocked_calls = blocked_calls.saturating_add(1);
            future::ready(Err::<(), _>(ProviderError::UncertainCharge))
        })
        .await
        .unwrap_err();
        assert_eq!(blocked_calls, 1);
        assert_eq!(blocked.failure_class(), Some(FailureClass::UncertainCharge));

        let allowed_policy = immediate_policy(3).with_uncertain_charge_retries(true);
        let mut allowed_calls = 0_u16;
        let completed = execute_with_retry(&allowed_policy, &NoopRetryJournal, |_| {
            allowed_calls = allowed_calls.saturating_add(1);
            future::ready(if allowed_calls == 1 {
                Err(ProviderError::UncertainCharge)
            } else {
                Ok("completed")
            })
        })
        .await
        .unwrap();
        assert_eq!(completed.value, "completed");
        assert_eq!(completed.attempts.get(), 2);
    }

    #[tokio::test]
    async fn authentication_and_validation_are_never_retried() {
        for error in [
            ProviderError::Authentication,
            ProviderError::Configuration("invalid".to_owned()),
        ] {
            let mut calls = 0_u16;
            let mut error = Some(error);
            let result = execute_with_retry(&immediate_policy(4), &NoopRetryJournal, |_| {
                calls = calls.saturating_add(1);
                future::ready(Err::<(), _>(error.take().unwrap()))
            })
            .await;
            assert!(result.is_err());
            assert_eq!(calls, 1);
        }
    }
}
