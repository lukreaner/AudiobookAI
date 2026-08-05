use std::{collections::BTreeMap, sync::OnceLock};

use audiobookai_core::{
    Budget, BudgetAllocation, BudgetId, BudgetMetric, BudgetReservation, BudgetScopeKind, Job,
    JobId, Money, ProvenanceQuality, ProviderProfileId, RateCard, RateCardId, ReservationId,
    ReservationStatus, UsageEvent, UsageQuantities, UsageWorkload,
};
use chrono::Utc;
use sqlx::Row;

use crate::{AppState, ServiceError};

static BUDGET_RESERVATION_LIFECYCLE: OnceLock<tokio::sync::Mutex<()>> = OnceLock::new();

#[derive(Clone, Debug)]
pub(crate) struct RatedUsageEstimate {
    pub provider_profile_id: ProviderProfileId,
    pub workload: UsageWorkload,
    pub quantities: UsageQuantities,
    pub cost: Option<Money>,
    pub rate_card_id: Option<RateCardId>,
}

pub(crate) async fn rate_usage_estimate(
    state: &AppState,
    provider_profile_id: ProviderProfileId,
    workload: UsageWorkload,
    model: Option<String>,
    mut quantities: UsageQuantities,
) -> Result<RatedUsageEstimate, ServiceError> {
    let card = applicable_rate_card(state, provider_profile_id, workload, model.as_deref()).await?;
    if quantities.provider_credits.is_none() {
        quantities.provider_credits = card
            .as_ref()
            .and_then(|card| estimate_provider_credits(card, &quantities));
    }
    let cost = card
        .as_ref()
        .and_then(|card| price_quantities(card, &quantities));
    Ok(RatedUsageEstimate {
        provider_profile_id,
        workload,
        quantities,
        cost,
        rate_card_id: card.map(|card| card.id),
    })
}

fn estimate_provider_credits(card: &RateCard, quantities: &UsageQuantities) -> Option<i64> {
    match card.workload {
        UsageWorkload::Tts => {
            let characters = quantities.characters?;
            let rate = first_rate(
                card,
                &[
                    "provider_credits_per_character_micros",
                    "credits_per_character_micros",
                ],
            )?;
            multiply_u64(characters, rate)
        }
        UsageWorkload::CharacterDetection => None,
    }
}

/// Loads the newest applicable price snapshot, preferring an exact model match over a
/// provider-wide fallback. Rate cards are configuration data; this function never contacts a
/// provider or infers a current price.
pub(crate) async fn applicable_rate_card(
    state: &AppState,
    provider_id: audiobookai_core::ProviderProfileId,
    workload: UsageWorkload,
    model: Option<&str>,
) -> Result<Option<RateCard>, ServiceError> {
    let rows = sqlx::query(
        "SELECT payload FROM rate_cards WHERE provider_id = ? AND workload = ? \
         AND effective_at <= ? AND (expires_at IS NULL OR expires_at > ?) \
         ORDER BY effective_at DESC",
    )
    .bind(provider_id.to_string())
    .bind(workload_name(workload))
    .bind(Utc::now().to_rfc3339())
    .bind(Utc::now().to_rfc3339())
    .fetch_all(state.database.pool())
    .await
    .map_err(|error| ServiceError::Storage(error.to_string()))?;
    let cards = rows
        .into_iter()
        .filter_map(|row| serde_json::from_str::<RateCard>(row.get::<&str, _>("payload")).ok())
        .collect::<Vec<_>>();
    Ok(model
        .and_then(|model| {
            cards
                .iter()
                .find(|card| card.model.as_deref() == Some(model))
                .cloned()
        })
        .or_else(|| cards.into_iter().find(|card| card.model.is_none())))
}

pub(crate) async fn apply_rate_card_snapshot(
    state: &AppState,
    event: &mut UsageEvent,
    preferred: Option<RateCardId>,
) -> Result<(), ServiceError> {
    let card = if let Some(id) = preferred {
        let payload =
            sqlx::query_scalar::<_, String>("SELECT payload FROM rate_cards WHERE id = ?")
                .bind(id.to_string())
                .fetch_optional(state.database.pool())
                .await
                .map_err(storage_error)?
                .ok_or_else(|| {
                    ServiceError::Conflict(
                        "the rate-card snapshot reserved for this request no longer exists"
                            .to_owned(),
                    )
                })?;
        let card = serde_json::from_str::<RateCard>(&payload)
            .map_err(|error| ServiceError::Internal(error.to_string()))?;
        if card.provider_profile_id != event.provider_profile_id
            || card.workload != event.workload
            || card
                .model
                .as_deref()
                .is_some_and(|model| event.model.as_deref() != Some(model))
        {
            return Err(ServiceError::Conflict(
                "the reserved rate-card snapshot does not match this provider request".to_owned(),
            ));
        }
        Some(card)
    } else {
        applicable_rate_card(
            state,
            event.provider_profile_id,
            event.workload,
            event.model.as_deref(),
        )
        .await?
    };
    let Some(card) = card else {
        return Ok(());
    };
    let Some(cost) = price_quantities(&card, &event.quantities) else {
        return Ok(());
    };
    event.cost = Some(cost);
    event.cost_source = ProvenanceQuality::Estimated;
    event.rate_card_id = Some(card.id);
    Ok(())
}

/// Atomically reserves all enabled budgets that apply to the supplied provider estimates.
/// Hard budgets fail closed when their metric cannot be estimated. The returned reservation is
/// durable before the caller is allowed to dispatch a provider request.
pub(crate) async fn reserve_for_estimates(
    state: &AppState,
    job: &Job,
    estimates: &[RatedUsageEstimate],
) -> Result<Option<ReservationId>, ServiceError> {
    let Some(reservation) = prepare_reservation_for_estimates(state, job, estimates).await? else {
        return Ok(None);
    };
    if job.allow_budget_override {
        state
            .database
            .repositories()
            .budgets
            .reserve_with_override(&reservation)
            .await
            .map_err(storage_error)?;
    } else {
        state
            .database
            .repositories()
            .budgets
            .reserve(&reservation)
            .await
            .map_err(|error| ServiceError::Conflict(error.to_string()))?;
    }
    refresh_budget_views(state).await?;
    Ok(Some(reservation.id))
}

/// Builds a fresh admission cycle without making it visible. Retry admission persists this value
/// in the same transaction that makes the job runnable.
pub(crate) async fn prepare_reservation_for_estimates(
    state: &AppState,
    job: &Job,
    estimates: &[RatedUsageEstimate],
) -> Result<Option<BudgetReservation>, ServiceError> {
    let budgets = state
        .database
        .repositories()
        .budgets
        .list_enabled()
        .await
        .map_err(storage_error)?;
    let mut allocations = Vec::new();
    for budget in budgets {
        let applicable = estimates
            .iter()
            .filter(|estimate| budget_applies_to_provider(&budget, estimate.provider_profile_id))
            .collect::<Vec<_>>();
        if applicable.is_empty() {
            continue;
        }
        let amount = match estimate_budget_amount(&budget, &applicable) {
            Some(amount) => amount,
            None if budget.hard && !job.allow_budget_override => {
                return Err(ServiceError::Conflict(format!(
                    "hard budget '{}' cannot be reserved because its {:?} estimate is unknown; configure a compatible rate card before starting this job",
                    budget.name, budget.metric
                )));
            }
            None => 0,
        };
        allocations.push(BudgetAllocation {
            budget_id: budget.id,
            reserved_amount: amount.max(0),
            actual_amount: None,
        });
    }
    if allocations.is_empty() {
        return Ok(None);
    }
    let now = Utc::now();
    Ok(Some(BudgetReservation {
        id: ReservationId::new(),
        job_id: job.id,
        status: ReservationStatus::Active,
        allocations,
        created_at: now,
        expires_at: Some(now + chrono::Duration::days(7)),
        reconciled_at: None,
    }))
}

/// Fails closed immediately before dispatch when a current hard budget is not covered by the
/// job's active reservation or the projected request would consume more than was reserved.
pub(crate) async fn verify_dispatch_is_reserved(
    state: &AppState,
    job_id: JobId,
    estimate: &RatedUsageEstimate,
) -> Result<(), ServiceError> {
    let repositories = state.database.repositories();
    let job = repositories
        .jobs
        .get(job_id)
        .await
        .map_err(storage_error)?
        .ok_or(ServiceError::NotFound)?;
    if job.allow_budget_override {
        return Ok(());
    }
    let budgets = repositories
        .budgets
        .list_enabled()
        .await
        .map_err(storage_error)?
        .into_iter()
        .filter(|budget| {
            budget.hard && budget_applies_to_provider(budget, estimate.provider_profile_id)
        })
        .collect::<Vec<_>>();
    if budgets.is_empty() {
        return Ok(());
    }
    let reservation_id = job.reservation_id.ok_or_else(|| {
        ServiceError::Conflict(
            "a hard budget applies to this request, but the job has no active reservation"
                .to_owned(),
        )
    })?;
    let reservation = repositories
        .budgets
        .get_reservation(reservation_id)
        .await
        .map_err(storage_error)?
        .ok_or(ServiceError::NotFound)?;
    if reservation.status != ReservationStatus::Active {
        return Err(ServiceError::Conflict(
            "the job's budget reservation is no longer active".to_owned(),
        ));
    }
    let sequence_after = repositories
        .budgets
        .usage_sequence_start(reservation_id)
        .await
        .map_err(storage_error)?;
    let events = repositories
        .usage
        .list(&audiobookai_storage::repositories::UsageFilter {
            job_id: Some(job_id),
            sequence_after: Some(sequence_after),
            ..audiobookai_storage::repositories::UsageFilter::default()
        })
        .await
        .map_err(storage_error)?;
    for budget in budgets {
        let allocation = reservation
            .allocations
            .iter()
            .find(|allocation| allocation.budget_id == budget.id)
            .ok_or_else(|| {
                ServiceError::Conflict(format!(
                    "hard budget '{}' is not covered by this job's reservation; restart preflight",
                    budget.name
                ))
            })?;
        let projected = estimate_amount_for_budget(&budget, estimate).ok_or_else(|| {
            ServiceError::Conflict(format!(
                "hard budget '{}' cannot verify this dispatch because its {:?} usage is unknown",
                budget.name, budget.metric
            ))
        })?;
        let mut consumed = 0_i64;
        for event in events
            .iter()
            .filter(|event| budget_applies_to_provider(&budget, event.provider_profile_id))
        {
            let amount = usage_amount_for_budget(&budget, event).ok_or_else(|| {
                ServiceError::Conflict(format!(
                    "hard budget '{}' cannot verify prior dispatch usage because it is unknown",
                    budget.name
                ))
            })?;
            consumed = consumed.saturating_add(amount.max(0));
        }
        if consumed.saturating_add(projected.max(0)) > allocation.reserved_amount {
            return Err(ServiceError::Conflict(format!(
                "hard budget '{}' would exceed this job's active reservation",
                budget.name
            )));
        }
    }
    Ok(())
}

/// Reconciles terminal jobs against the append-only usage ledger. A job that made no billable
/// request releases its reservation instead. Unknown relevant usage consumes the reserved amount
/// rather than being treated as zero.
pub(crate) async fn finalize_job_reservation(
    state: &AppState,
    job_id: JobId,
) -> Result<(), ServiceError> {
    let _guard = lock_budget_reservation_lifecycle().await;
    finalize_job_reservation_locked(state, job_id).await
}

/// Serializes terminal reconciliation with fresh retry admission. Callers that keep this guard
/// through the retry transaction prevent a finishing worker from finalizing the new cycle.
pub(crate) async fn lock_budget_reservation_lifecycle() -> tokio::sync::MutexGuard<'static, ()> {
    BUDGET_RESERVATION_LIFECYCLE
        .get_or_init(|| tokio::sync::Mutex::new(()))
        .lock()
        .await
}

#[allow(clippy::too_many_lines)]
pub(crate) async fn finalize_job_reservation_locked(
    state: &AppState,
    job_id: JobId,
) -> Result<(), ServiceError> {
    let repositories = state.database.repositories();
    let job = repositories
        .jobs
        .get(job_id)
        .await
        .map_err(storage_error)?
        .ok_or(ServiceError::NotFound)?;
    if !job.state.is_terminal() {
        return Ok(());
    }
    let Some(reservation_id) = job.reservation_id else {
        return Ok(());
    };
    let reservation = repositories
        .budgets
        .get_reservation(reservation_id)
        .await
        .map_err(storage_error)?
        .ok_or(ServiceError::NotFound)?;
    if matches!(
        reservation.status,
        ReservationStatus::Reconciled | ReservationStatus::Released
    ) {
        return Ok(());
    }
    let sequence_after = repositories
        .budgets
        .usage_sequence_start(reservation_id)
        .await
        .map_err(storage_error)?;
    let events = repositories
        .usage
        .list(&audiobookai_storage::repositories::UsageFilter {
            job_id: Some(job_id),
            sequence_after: Some(sequence_after),
            ..audiobookai_storage::repositories::UsageFilter::default()
        })
        .await
        .map_err(storage_error)?;
    if events.is_empty() {
        let successful_dispatch_exists = sqlx::query_scalar::<_, i64>(
            "SELECT EXISTS(SELECT 1 FROM job_attempts a \
             JOIN job_units u ON u.id = a.job_unit_id \
             WHERE u.job_id = ? AND a.finished_at IS NOT NULL \
             AND a.finished_at >= ? \
             AND a.failure_class IS NULL AND a.uncertain_charge = 0)",
        )
        .bind(job_id.to_string())
        .bind(reservation.created_at.to_rfc3339())
        .fetch_one(state.database.pool())
        .await
        .map_err(storage_error)?
            != 0;
        if successful_dispatch_exists {
            // A succeeded attempt is durable proof that provider-owned work completed. If its
            // usage row could not be written, retain the hard-budget reservation for recovery or
            // manual reconciliation instead of incorrectly releasing it as an uncharged job.
            tracing::warn!(
                diagnostic_code = "budget.reconciliation.success_usage_missing",
                %job_id,
                %reservation_id,
                "successful provider dispatch has no usage event; reservation retained"
            );
            return Ok(());
        }
        repositories
            .budgets
            .release(reservation_id, Utc::now())
            .await
            .map_err(storage_error)?;
        refresh_budget_views(state).await?;
        return Ok(());
    }
    let mut actuals = BTreeMap::<BudgetId, i64>::new();
    for allocation in &reservation.allocations {
        let Some(budget) = repositories
            .budgets
            .get(allocation.budget_id)
            .await
            .map_err(storage_error)?
        else {
            continue;
        };
        let applicable = events
            .iter()
            .filter(|event| budget_applies_to_provider(&budget, event.provider_profile_id))
            .collect::<Vec<_>>();
        let mut actual = 0_i64;
        let mut unknown = false;
        for event in applicable {
            if let Some(amount) = usage_amount_for_budget(&budget, event) {
                actual = actual.saturating_add(amount.max(0));
            } else {
                unknown = true;
            }
        }
        if unknown {
            actual = actual.max(allocation.reserved_amount);
        }
        actuals.insert(allocation.budget_id, actual);
    }
    repositories
        .budgets
        .reconcile(reservation_id, &actuals, Utc::now())
        .await
        .map_err(storage_error)?;
    refresh_budget_views(state).await
}

/// Refreshes each budget from the repository's period-aware current state. In particular,
/// per-job budgets report no global reserved total and concurrent reservations remain visible.
pub(crate) async fn refresh_budget_views(state: &AppState) -> Result<(), ServiceError> {
    let repository = state.database.repositories().budgets;
    let budgets = repository.list_enabled().await.map_err(storage_error)?;
    let mut refreshed = Vec::with_capacity(budgets.len());
    for budget in budgets {
        let reserved = repository
            .active_reserved(budget.id)
            .await
            .map_err(storage_error)?;
        refreshed.push((budget, reserved));
    }
    let mut catalog = state.catalog.write().await;
    for (budget, reserved) in refreshed {
        if let Some(view) = catalog.budgets.get_mut(&budget.id.as_uuid()) {
            view.used = budget.used;
            view.reserved = reserved;
        }
    }
    Ok(())
}

fn budget_applies_to_provider(budget: &Budget, provider_id: ProviderProfileId) -> bool {
    match budget.scope.kind {
        BudgetScopeKind::Global => true,
        BudgetScopeKind::Provider => budget.scope.provider_profile_id == Some(provider_id),
    }
}

fn estimate_budget_amount(budget: &Budget, estimates: &[&RatedUsageEstimate]) -> Option<i64> {
    estimates.iter().try_fold(0_i64, |total, estimate| {
        estimate_amount_for_budget(budget, estimate)
            .map(|amount| total.saturating_add(amount.max(0)))
    })
}

fn estimate_amount_for_budget(budget: &Budget, estimate: &RatedUsageEstimate) -> Option<i64> {
    if budget.metric == BudgetMetric::MoneyMicros {
        estimate.rate_card_id?;
        let cost = estimate.cost.as_ref()?;
        let currency = budget.currency.as_deref()?;
        return currency
            .eq_ignore_ascii_case(&cost.currency)
            .then_some(cost.micros);
    }
    structurally_known_zero(estimate.workload, budget.metric).or_else(|| {
        estimate
            .quantities
            .amount_for(budget.metric, estimate.cost.as_ref())
    })
}

fn usage_amount_for_budget(budget: &Budget, event: &UsageEvent) -> Option<i64> {
    if budget.metric == BudgetMetric::MoneyMicros {
        let cost = event.cost.as_ref()?;
        let currency = budget.currency.as_deref()?;
        return currency
            .eq_ignore_ascii_case(&cost.currency)
            .then_some(cost.micros);
    }
    structurally_known_zero(event.workload, budget.metric).or_else(|| {
        event
            .quantities
            .amount_for(budget.metric, event.cost.as_ref())
    })
}

const fn structurally_known_zero(workload: UsageWorkload, metric: BudgetMetric) -> Option<i64> {
    match (workload, metric) {
        (UsageWorkload::CharacterDetection, BudgetMetric::AudioMilliseconds) => Some(0),
        _ => None,
    }
}

fn storage_error(error: impl std::fmt::Display) -> ServiceError {
    ServiceError::Storage(error.to_string())
}

pub(crate) fn price_quantities(card: &RateCard, quantities: &UsageQuantities) -> Option<Money> {
    let micros = match card.workload {
        UsageWorkload::Tts => price_tts(card, quantities)?,
        UsageWorkload::CharacterDetection => price_character_detection(card, quantities)?,
    };
    Some(Money {
        micros,
        currency: card.currency.clone(),
    })
}

fn price_tts(card: &RateCard, quantities: &UsageQuantities) -> Option<i64> {
    let characters = quantities.characters?;
    if let Some(rate) = first_rate(card, &["per_character_micros", "character_micros"]) {
        return multiply_u64(characters, rate);
    }
    let rate = first_rate(card, &["per_1000_characters_micros"])?;
    multiply_ratio_ceil(characters, rate, 1_000)
}

fn price_character_detection(card: &RateCard, quantities: &UsageQuantities) -> Option<i64> {
    let input = quantities.input_tokens?;
    let output = quantities.output_tokens?;
    let cached = quantities.cache_read_tokens.unwrap_or_default().min(input);
    let mut total = price_tokens(
        card,
        input.saturating_sub(cached),
        &["per_input_token_micros", "input_token_micros"],
        &["per_1m_input_tokens_micros"],
    )?;
    total = total.saturating_add(price_tokens(
        card,
        output,
        &["per_output_token_micros", "output_token_micros"],
        &["per_1m_output_tokens_micros"],
    )?);
    if cached > 0 {
        total = total.saturating_add(
            price_tokens(
                card,
                cached,
                &["per_cached_input_token_micros", "cached_input_token_micros"],
                &["per_1m_cached_input_tokens_micros"],
            )
            .or_else(|| {
                price_tokens(
                    card,
                    cached,
                    &["per_input_token_micros", "input_token_micros"],
                    &["per_1m_input_tokens_micros"],
                )
            })?,
        );
    }
    if let Some(reasoning) = quantities.reasoning_tokens
        && reasoning > 0
    {
        total = total.saturating_add(
            price_tokens(
                card,
                reasoning,
                &["per_reasoning_token_micros", "reasoning_token_micros"],
                &["per_1m_reasoning_tokens_micros"],
            )
            .or_else(|| {
                price_tokens(
                    card,
                    reasoning,
                    &["per_output_token_micros", "output_token_micros"],
                    &["per_1m_output_tokens_micros"],
                )
            })?,
        );
    }
    Some(total)
}

fn price_tokens(
    card: &RateCard,
    tokens: u64,
    direct_keys: &[&str],
    per_million_keys: &[&str],
) -> Option<i64> {
    if let Some(rate) = first_rate(card, direct_keys) {
        return multiply_u64(tokens, rate);
    }
    let rate = first_rate(card, per_million_keys)?;
    multiply_ratio_ceil(tokens, rate, 1_000_000)
}

fn first_rate(card: &RateCard, keys: &[&str]) -> Option<i64> {
    keys.iter().find_map(|key| card.pricing.get(*key).copied())
}

fn multiply_u64(quantity: u64, rate: i64) -> Option<i64> {
    let quantity = i128::from(quantity);
    let rate = i128::from(rate);
    i64::try_from(quantity.checked_mul(rate)?).ok()
}

fn multiply_ratio_ceil(quantity: u64, rate: i64, divisor: i128) -> Option<i64> {
    let value = i128::from(quantity).checked_mul(i128::from(rate))?;
    let rounded = value.checked_add(divisor.saturating_sub(1))? / divisor;
    i64::try_from(rounded).ok()
}

pub(crate) const fn workload_name(workload: UsageWorkload) -> &'static str {
    match workload {
        UsageWorkload::Tts => "tts",
        UsageWorkload::CharacterDetection => "character_detection",
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use audiobookai_core::{
        AttemptId, BookId, BudgetPeriod, BudgetScope, BudgetScopeKind, JobAttempt, JobKind,
        JobState, JobUnit, JobUnitId, JobUnitKind, JobUnitState, ProjectId, ProviderProfileId,
        RateCardId,
    };

    use super::*;

    fn card(workload: UsageWorkload, pricing: BTreeMap<String, i64>) -> RateCard {
        RateCard {
            id: RateCardId::new(),
            provider_profile_id: ProviderProfileId::new(),
            model: None,
            workload,
            currency: "EUR".to_owned(),
            effective_at: Utc::now(),
            expires_at: None,
            source: "test snapshot".to_owned(),
            source_url: None,
            pricing,
            user_overridden: true,
        }
    }

    fn budget(metric: BudgetMetric, currency: Option<&str>) -> Budget {
        let now = Utc::now();
        Budget {
            id: BudgetId::new(),
            name: "test budget".to_owned(),
            scope: BudgetScope {
                kind: BudgetScopeKind::Global,
                provider_profile_id: None,
            },
            period: BudgetPeriod::Lifetime,
            metric,
            currency: currency.map(str::to_owned),
            limit: i64::MAX,
            used: 0,
            warning_threshold_percent: 80,
            hard: true,
            enabled: true,
            period_started_at: now,
            period_ends_at: None,
            created_at: now,
            updated_at: now,
        }
    }

    fn estimate(
        workload: UsageWorkload,
        quantities: UsageQuantities,
        cost: Option<Money>,
    ) -> RatedUsageEstimate {
        RatedUsageEstimate {
            provider_profile_id: ProviderProfileId::new(),
            workload,
            quantities,
            cost,
            rate_card_id: Some(RateCardId::new()),
        }
    }

    async fn test_state() -> (tempfile::TempDir, AppState) {
        let directory = tempfile::tempdir().expect("temporary directory");
        let database = audiobookai_storage::Database::open_in(directory.path())
            .await
            .expect("database");
        let state = AppState::new(
            crate::ServiceConfig {
                bind: "127.0.0.1:0".parse().expect("address"),
                data_dir: directory.path().to_path_buf(),
                bundled_sidecar_dir: None,
                tls: None,
                lan_hostnames: Vec::new(),
                allow_insecure_lan: false,
                desktop_bootstrap: false,
            },
            database,
        )
        .await
        .expect("application state");
        (directory, state)
    }

    #[allow(clippy::too_many_lines)]
    async fn terminal_job_with_reservation(
        state: &AppState,
        reservation_created_at: chrono::DateTime<Utc>,
        attempt_finished_at: chrono::DateTime<Utc>,
    ) -> (JobId, ReservationId) {
        let project_id = ProjectId::new();
        let book_id = BookId::new();
        let created_at = reservation_created_at - chrono::Duration::minutes(20);
        sqlx::query(
            "INSERT INTO books (id, managed_epub_path, source_hash, imported_at, payload) \
             VALUES (?, ?, ?, ?, '{}')",
        )
        .bind(book_id.to_string())
        .bind(format!("/fixtures/{book_id}.epub"))
        .bind(format!("fixture-{book_id}"))
        .bind(created_at.to_rfc3339())
        .execute(state.database.pool())
        .await
        .expect("book fixture");
        sqlx::query(
            "INSERT INTO projects \
             (id, book_id, name, status, created_at, updated_at, revision, payload) \
             VALUES (?, ?, 'Accounting fixture', 'draft', ?, ?, 0, '{}')",
        )
        .bind(project_id.to_string())
        .bind(book_id.to_string())
        .bind(created_at.to_rfc3339())
        .bind(created_at.to_rfc3339())
        .execute(state.database.pool())
        .await
        .expect("project fixture");

        let job_id = JobId::new();
        let unit_id = JobUnitId::new();
        let mut job = Job {
            id: job_id,
            project_id,
            kind: JobKind::Preview,
            state: JobState::Running,
            export_profile_id: None,
            reservation_id: None,
            progress_completed: 0,
            progress_total: 1,
            status_message: None,
            allow_budget_override: false,
            created_at,
            started_at: Some(created_at),
            finished_at: None,
            updated_at: created_at,
            revision: 0,
        };
        let unit = JobUnit {
            id: unit_id,
            job_id,
            kind: JobUnitKind::SynthesisSegment,
            state: JobUnitState::Running,
            chapter_id: None,
            segment_id: None,
            provider_profile_id: None,
            dependencies: Vec::new(),
            attempt_count: 1,
            next_attempt_at: None,
            output_artifact_id: None,
            payload: BTreeMap::new(),
            created_at,
            updated_at: created_at,
        };
        state
            .database
            .repositories()
            .proofing
            .insert_job_graph(&job, std::slice::from_ref(&unit), None)
            .await
            .expect("job fixture");

        let budget = budget(BudgetMetric::Characters, None);
        state
            .database
            .repositories()
            .budgets
            .upsert(&budget)
            .await
            .expect("budget fixture");
        let reservation = BudgetReservation {
            id: ReservationId::new(),
            job_id,
            status: ReservationStatus::Active,
            allocations: vec![BudgetAllocation {
                budget_id: budget.id,
                reserved_amount: 100,
                actual_amount: None,
            }],
            created_at: reservation_created_at,
            expires_at: Some(reservation_created_at + chrono::Duration::days(1)),
            reconciled_at: None,
        };
        state
            .database
            .repositories()
            .budgets
            .reserve(&reservation)
            .await
            .expect("reservation fixture");
        let expected_revision = job.revision;
        job.reservation_id = Some(reservation.id);
        job.updated_at = reservation_created_at;
        job = state
            .database
            .repositories()
            .jobs
            .update(&job, expected_revision)
            .await
            .expect("attach reservation");

        let attempt = JobAttempt {
            id: AttemptId::new(),
            job_unit_id: unit_id,
            ordinal: 1,
            started_at: attempt_finished_at - chrono::Duration::seconds(1),
            finished_at: Some(attempt_finished_at),
            failure_class: None,
            error_code: None,
            redacted_error: None,
            provider_request_id: Some("provider-request".to_owned()),
            uncertain_charge: false,
        };
        state
            .database
            .repositories()
            .jobs
            .insert_attempt(&attempt)
            .await
            .expect("attempt fixture");
        job.transition(
            JobState::Failed,
            reservation_created_at + chrono::Duration::minutes(1),
        )
        .expect("terminal job");
        state
            .database
            .repositories()
            .jobs
            .update(&job, job.revision)
            .await
            .expect("persist terminal job");

        (job_id, reservation.id)
    }

    #[tokio::test]
    async fn prior_cycle_success_does_not_retain_a_fresh_uncharged_reservation() {
        let (_directory, state) = test_state().await;
        let reservation_created_at = Utc::now();
        let (job_id, reservation_id) = terminal_job_with_reservation(
            &state,
            reservation_created_at,
            reservation_created_at - chrono::Duration::minutes(5),
        )
        .await;

        finalize_job_reservation(&state, job_id)
            .await
            .expect("finalize reservation");

        let reservation = state
            .database
            .repositories()
            .budgets
            .get_reservation(reservation_id)
            .await
            .expect("load reservation")
            .expect("reservation exists");
        assert_eq!(reservation.status, ReservationStatus::Released);
    }

    #[tokio::test]
    async fn current_cycle_success_without_usage_retains_the_reservation() {
        let (_directory, state) = test_state().await;
        let reservation_created_at = Utc::now();
        let (job_id, reservation_id) = terminal_job_with_reservation(
            &state,
            reservation_created_at,
            reservation_created_at + chrono::Duration::seconds(5),
        )
        .await;

        finalize_job_reservation(&state, job_id)
            .await
            .expect("finalize reservation");

        let reservation = state
            .database
            .repositories()
            .budgets
            .get_reservation(reservation_id)
            .await
            .expect("load reservation")
            .expect("reservation exists");
        assert_eq!(reservation.status, ReservationStatus::Active);
    }

    #[test]
    fn prices_tts_proportionally_from_per_thousand_snapshot() {
        let card = card(
            UsageWorkload::Tts,
            BTreeMap::from([("per_1000_characters_micros".to_owned(), 125_000)]),
        );
        let quantities = UsageQuantities {
            characters: Some(1_001),
            ..UsageQuantities::default()
        };
        assert_eq!(
            price_quantities(&card, &quantities).unwrap().micros,
            125_125
        );
    }

    #[test]
    fn prices_ai_input_output_and_reasoning_from_snapshot() {
        let card = card(
            UsageWorkload::CharacterDetection,
            BTreeMap::from([
                ("per_1m_input_tokens_micros".to_owned(), 1_000_000),
                ("per_1m_output_tokens_micros".to_owned(), 2_000_000),
            ]),
        );
        let quantities = UsageQuantities {
            input_tokens: Some(500_000),
            output_tokens: Some(100_000),
            reasoning_tokens: Some(50_000),
            ..UsageQuantities::default()
        };
        assert_eq!(
            price_quantities(&card, &quantities).unwrap().micros,
            800_000
        );
    }

    #[test]
    fn refuses_partial_ai_cost_when_a_required_rate_is_missing() {
        let card = card(
            UsageWorkload::CharacterDetection,
            BTreeMap::from([("per_1m_input_tokens_micros".to_owned(), 1_000_000)]),
        );
        let quantities = UsageQuantities {
            input_tokens: Some(100),
            output_tokens: Some(100),
            ..UsageQuantities::default()
        };
        assert!(price_quantities(&card, &quantities).is_none());
    }

    #[test]
    fn ai_budget_estimates_keep_tokens_distinct_and_audio_structurally_zero() {
        let estimate = estimate(
            UsageWorkload::CharacterDetection,
            UsageQuantities {
                input_tokens: Some(1_000),
                output_tokens: Some(200),
                reasoning_tokens: Some(300),
                ..UsageQuantities::default()
            },
            None,
        );
        assert_eq!(
            estimate_amount_for_budget(&budget(BudgetMetric::InputTokens, None), &estimate),
            Some(1_000)
        );
        assert_eq!(
            estimate_amount_for_budget(&budget(BudgetMetric::TotalTokens, None), &estimate),
            Some(1_500)
        );
        assert_eq!(
            estimate_amount_for_budget(&budget(BudgetMetric::AudioMilliseconds, None), &estimate),
            Some(0)
        );
    }

    #[test]
    fn monetary_budget_requires_matching_currency_and_rate_card_provenance() {
        let mut estimate = estimate(
            UsageWorkload::Tts,
            UsageQuantities {
                characters: Some(10),
                ..UsageQuantities::default()
            },
            Some(Money {
                micros: 123,
                currency: "EUR".to_owned(),
            }),
        );
        assert_eq!(
            estimate_amount_for_budget(&budget(BudgetMetric::MoneyMicros, Some("EUR")), &estimate),
            Some(123)
        );
        assert_eq!(
            estimate_amount_for_budget(&budget(BudgetMetric::MoneyMicros, Some("USD")), &estimate),
            None
        );
        estimate.rate_card_id = None;
        assert_eq!(
            estimate_amount_for_budget(&budget(BudgetMetric::MoneyMicros, Some("EUR")), &estimate),
            None
        );
    }

    #[test]
    fn reservation_sums_each_dispatch_price_without_losing_rounding() {
        let first = estimate(
            UsageWorkload::Tts,
            UsageQuantities {
                characters: Some(1),
                ..UsageQuantities::default()
            },
            Some(Money {
                micros: 1,
                currency: "EUR".to_owned(),
            }),
        );
        let second = first.clone();
        assert_eq!(
            estimate_budget_amount(
                &budget(BudgetMetric::MoneyMicros, Some("EUR")),
                &[&first, &second]
            ),
            Some(2)
        );
    }
}
