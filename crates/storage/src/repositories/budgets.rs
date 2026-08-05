use std::collections::{BTreeMap, BTreeSet};

use audiobookai_core::{
    Budget, BudgetId, BudgetPeriod, BudgetReservation, ReservationId, ReservationStatus, Validate,
};
use chrono::{DateTime, Datelike, Duration, TimeZone, Utc};
use sqlx::{Row, Sqlite, SqlitePool, Transaction};

use crate::{Result, StorageError};

use super::util::{decode, encode, enum_text};

#[derive(Clone, Debug)]
pub struct BudgetRepository {
    pool: SqlitePool,
}

impl BudgetRepository {
    #[must_use]
    pub fn new(pool: SqlitePool) -> Self {
        Self { pool }
    }

    pub async fn upsert(&self, budget: &Budget) -> Result<()> {
        budget.validate()?;
        let mut budget = budget.clone();
        normalize_budget_for_upsert(&mut budget);
        budget.validate()?;
        let mut tx = self.pool.begin().await?;
        let previous_period =
            sqlx::query_scalar::<_, String>("SELECT period FROM budgets WHERE id = ?")
                .bind(budget.id.to_string())
                .fetch_optional(&mut *tx)
                .await?;
        let period = enum_text(&budget.period)?;
        if previous_period
            .as_deref()
            .is_some_and(|value| value != period)
        {
            sqlx::query("DELETE FROM budget_period_usage WHERE budget_id = ?")
                .bind(budget.id.to_string())
                .execute(&mut *tx)
                .await?;
        }
        sqlx::query(
            "INSERT INTO budgets \
             (id, provider_id, name, scope, period, metric, currency, limit_value, used_value, hard, enabled, \
              period_started_at, period_ends_at, updated_at, payload) \
             VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?) \
             ON CONFLICT(id) DO UPDATE SET provider_id = excluded.provider_id, name = excluded.name, \
             scope = excluded.scope, period = excluded.period, metric = excluded.metric, \
             currency = excluded.currency, limit_value = excluded.limit_value, used_value = excluded.used_value, \
             hard = excluded.hard, enabled = excluded.enabled, period_started_at = excluded.period_started_at, \
             period_ends_at = excluded.period_ends_at, updated_at = excluded.updated_at, payload = excluded.payload",
        )
        .bind(budget.id.to_string())
        .bind(budget.scope.provider_profile_id.map(|id| id.to_string()))
        .bind(&budget.name)
        .bind(enum_text(&budget.scope.kind)?)
        .bind(&period)
        .bind(enum_text(&budget.metric)?)
        .bind(&budget.currency)
        .bind(budget.limit)
        .bind(budget.used)
        .bind(budget.hard)
        .bind(budget.enabled)
        .bind(budget.period_started_at.to_rfc3339())
        .bind(budget.period_ends_at.map(|value| value.to_rfc3339()))
        .bind(budget.updated_at.to_rfc3339())
        .bind(encode(&budget)?)
        .execute(&mut *tx)
        .await?;
        if let Some(window) = period_window(budget.period, budget.updated_at) {
            set_period_usage_tx(&mut tx, budget.id, window, budget.used, budget.updated_at).await?;
        }
        tx.commit().await?;
        Ok(())
    }

    pub async fn get(&self, id: BudgetId) -> Result<Option<Budget>> {
        self.get_at(id, Utc::now()).await
    }

    /// Returns a budget with its current UTC calendar period materialized.
    pub async fn get_at(&self, id: BudgetId, now: DateTime<Utc>) -> Result<Option<Budget>> {
        let mut tx = self.pool.begin().await?;
        expire_stale_reservations(&mut tx, now).await?;
        let budget = load_budget_tx(&mut tx, id).await?;
        let budget = match budget {
            Some(budget) => Some(ensure_current_period_tx(&mut tx, budget, now).await?),
            None => None,
        };
        tx.commit().await?;
        Ok(budget)
    }

    pub async fn list_enabled(&self) -> Result<Vec<Budget>> {
        self.list_enabled_at(Utc::now()).await
    }

    /// Lists enabled budgets after rolling daily/monthly state at UTC boundaries.
    pub async fn list_enabled_at(&self, now: DateTime<Utc>) -> Result<Vec<Budget>> {
        let mut tx = self.pool.begin().await?;
        expire_stale_reservations(&mut tx, now).await?;
        let ids = sqlx::query_scalar::<_, String>(
            "SELECT id FROM budgets WHERE enabled = 1 ORDER BY name",
        )
        .fetch_all(&mut *tx)
        .await?;
        let mut budgets = Vec::with_capacity(ids.len());
        for id in ids {
            let id = id.parse::<BudgetId>().map_err(|error| {
                StorageError::InvalidData(format!("invalid budget id {id}: {error}"))
            })?;
            let budget =
                load_budget_tx(&mut tx, id)
                    .await?
                    .ok_or_else(|| StorageError::NotFound {
                        entity: "budget",
                        id: id.to_string(),
                    })?;
            budgets.push(ensure_current_period_tx(&mut tx, budget, now).await?);
        }
        tx.commit().await?;
        Ok(budgets)
    }

    /// Returns reservations that consume the budget's current period.
    ///
    /// Job budgets deliberately return zero: their capacity is isolated to each
    /// reservation/job and has no meaningful global reserved total.
    pub async fn active_reserved(&self, id: BudgetId) -> Result<i64> {
        self.active_reserved_at(id, Utc::now()).await
    }

    pub async fn active_reserved_at(&self, id: BudgetId, now: DateTime<Utc>) -> Result<i64> {
        let mut tx = self.pool.begin().await?;
        expire_stale_reservations(&mut tx, now).await?;
        let budget = load_budget_tx(&mut tx, id)
            .await?
            .ok_or_else(|| StorageError::NotFound {
                entity: "budget",
                id: id.to_string(),
            })?;
        let budget = ensure_current_period_tx(&mut tx, budget, now).await?;
        let reserved = active_reserved_tx(&mut tx, &budget, now).await?;
        tx.commit().await?;
        Ok(reserved)
    }

    /// Atomically checks every allocation and reserves all or none of them.
    pub async fn reserve(&self, reservation: &BudgetReservation) -> Result<()> {
        let mut tx = self.pool.begin().await?;
        insert_budget_reservation_tx(&mut tx, reservation, true).await?;
        tx.commit().await?;
        Ok(())
    }

    /// Persists an explicit user override without enforcing the hard-capacity check. The
    /// reservation remains fully durable and is reconciled like every other admission cycle.
    pub async fn reserve_with_override(&self, reservation: &BudgetReservation) -> Result<()> {
        let mut tx = self.pool.begin().await?;
        insert_budget_reservation_tx(&mut tx, reservation, false).await?;
        tx.commit().await?;
        Ok(())
    }

    /// Returns the append-only usage-ledger boundary captured when this admission cycle began.
    pub async fn usage_sequence_start(&self, id: ReservationId) -> Result<u64> {
        let value = sqlx::query_scalar::<_, i64>(
            "SELECT usage_sequence_start FROM budget_reservations WHERE id = ?",
        )
        .bind(id.to_string())
        .fetch_optional(&self.pool)
        .await?
        .ok_or_else(|| StorageError::NotFound {
            entity: "budget_reservation",
            id: id.to_string(),
        })?;
        u64::try_from(value).map_err(|_| {
            StorageError::InvalidData(format!(
                "budget reservation {id} has a negative usage sequence boundary"
            ))
        })
    }

    pub async fn get_reservation(&self, id: ReservationId) -> Result<Option<BudgetReservation>> {
        let row = sqlx::query("SELECT status, payload FROM budget_reservations WHERE id = ?")
            .bind(id.to_string())
            .fetch_optional(&self.pool)
            .await?;
        row.as_ref().map(decode_reservation_row).transpose()
    }

    /// Reconciles actual consumption and releases the unused portion.
    pub async fn reconcile(
        &self,
        id: ReservationId,
        actuals: &BTreeMap<BudgetId, i64>,
        now: DateTime<Utc>,
    ) -> Result<BudgetReservation> {
        if actuals.values().any(|amount| *amount < 0) {
            return Err(StorageError::InvalidData(
                "actual budget usage must not be negative".into(),
            ));
        }
        let mut tx = self.pool.begin().await?;
        lock_reservation_tx(&mut tx, id).await?;
        let mut reservation =
            load_reservation_tx(&mut tx, id)
                .await?
                .ok_or_else(|| StorageError::NotFound {
                    entity: "budget_reservation",
                    id: id.to_string(),
                })?;
        if !matches!(
            reservation.status,
            ReservationStatus::Active | ReservationStatus::Expired
        ) {
            return Err(StorageError::Conflict {
                entity: "budget_reservation",
                id: id.to_string(),
            });
        }

        for allocation in &mut reservation.allocations {
            let actual = actuals.get(&allocation.budget_id).copied().unwrap_or(0);
            let budget = load_budget_tx(&mut tx, allocation.budget_id)
                .await?
                .ok_or_else(|| StorageError::NotFound {
                    entity: "budget",
                    id: allocation.budget_id.to_string(),
                })?;
            let mut budget = ensure_current_period_tx(&mut tx, budget, now).await?;
            match budget.period {
                BudgetPeriod::Job => {
                    // Per-job usage is durable on the allocation. It must not
                    // reduce capacity for any other job.
                }
                BudgetPeriod::Lifetime => {
                    budget.used = budget.used.saturating_add(actual);
                }
                BudgetPeriod::Daily | BudgetPeriod::Monthly => {
                    let reservation_window = period_window(budget.period, reservation.created_at)
                        .ok_or_else(|| {
                        StorageError::InvalidData(
                            "calendar budget is missing a period window".into(),
                        )
                    })?;
                    let period_used =
                        add_period_usage_tx(&mut tx, budget.id, reservation_window, actual, now)
                            .await?;
                    if period_window(budget.period, now) == Some(reservation_window) {
                        budget.used = period_used;
                    }
                }
            }
            budget.updated_at = now;
            persist_budget_state_tx(&mut tx, &budget).await?;
            sqlx::query(
                "UPDATE budget_allocations SET actual_amount = ? \
                 WHERE reservation_id = ? AND budget_id = ?",
            )
            .bind(actual)
            .bind(id.to_string())
            .bind(allocation.budget_id.to_string())
            .execute(&mut *tx)
            .await?;
            allocation.actual_amount = Some(actual);
        }
        reservation.status = ReservationStatus::Reconciled;
        reservation.reconciled_at = Some(now);
        sqlx::query(
            "UPDATE budget_reservations SET status = 'reconciled', reconciled_at = ?, payload = ? \
             WHERE id = ? AND status IN ('active', 'expired')",
        )
        .bind(now.to_rfc3339())
        .bind(encode(&reservation)?)
        .bind(id.to_string())
        .execute(&mut *tx)
        .await?;
        tx.commit().await?;
        Ok(reservation)
    }

    pub async fn release(
        &self,
        id: ReservationId,
        now: DateTime<Utc>,
    ) -> Result<BudgetReservation> {
        let mut tx = self.pool.begin().await?;
        lock_reservation_tx(&mut tx, id).await?;
        let mut reservation =
            load_reservation_tx(&mut tx, id)
                .await?
                .ok_or_else(|| StorageError::NotFound {
                    entity: "budget_reservation",
                    id: id.to_string(),
                })?;
        if !matches!(
            reservation.status,
            ReservationStatus::Active | ReservationStatus::Expired
        ) {
            return Err(StorageError::Conflict {
                entity: "budget_reservation",
                id: id.to_string(),
            });
        }
        reservation.status = ReservationStatus::Released;
        reservation.reconciled_at = Some(now);
        sqlx::query(
            "UPDATE budget_reservations SET status = 'released', reconciled_at = ?, payload = ? \
             WHERE id = ? AND status IN ('active', 'expired')",
        )
        .bind(now.to_rfc3339())
        .bind(encode(&reservation)?)
        .bind(id.to_string())
        .execute(&mut *tx)
        .await?;
        tx.commit().await?;
        Ok(reservation)
    }
}

/// Inserts one active reservation into an existing transaction. This is shared with retry job
/// admission so the fresh budget cycle, optional output claim, and Failed -> Queued transition
/// can commit atomically.
#[allow(clippy::too_many_lines)]
pub(super) async fn insert_budget_reservation_tx(
    tx: &mut Transaction<'_, Sqlite>,
    reservation: &BudgetReservation,
    enforce_capacity: bool,
) -> Result<()> {
    reservation.validate()?;
    if reservation.status != ReservationStatus::Active {
        return Err(StorageError::InvalidData(
            "new budget reservations must be active".into(),
        ));
    }
    let unique_ids = reservation
        .allocations
        .iter()
        .map(|allocation| allocation.budget_id)
        .collect::<BTreeSet<_>>();
    if unique_ids.len() != reservation.allocations.len() {
        return Err(StorageError::InvalidData(
            "reservation contains duplicate budget allocations".into(),
        ));
    }

    expire_stale_reservations(tx, reservation.created_at).await?;
    if enforce_capacity {
        for allocation in &reservation.allocations {
            let budget = load_budget_tx(tx, allocation.budget_id)
                .await?
                .ok_or_else(|| StorageError::NotFound {
                    entity: "budget",
                    id: allocation.budget_id.to_string(),
                })?;
            let budget = ensure_current_period_tx(tx, budget, reservation.created_at).await?;
            if !budget.enabled {
                return Err(StorageError::InvalidData(format!(
                    "budget {} is disabled",
                    budget.id
                )));
            }
            let active_reserved = active_reserved_tx(tx, &budget, reservation.created_at).await?;
            let remaining = match budget.period {
                BudgetPeriod::Job => {
                    let consumed = sqlx::query_scalar::<_, i64>(
                        "SELECT COALESCE(SUM(CASE \
                             WHEN r.status = 'reconciled' THEN COALESCE(a.actual_amount, 0) \
                             WHEN r.status = 'active' THEN a.reserved_amount \
                             ELSE 0 END), 0) \
                         FROM budget_allocations a \
                         JOIN budget_reservations r ON r.id = a.reservation_id \
                         WHERE r.job_id = ? AND a.budget_id = ?",
                    )
                    .bind(reservation.job_id.to_string())
                    .bind(budget.id.to_string())
                    .fetch_one(&mut **tx)
                    .await?;
                    budget.limit.saturating_sub(consumed.max(0))
                }
                _ => budget.remaining(active_reserved),
            };
            if budget.hard && allocation.reserved_amount > remaining {
                return Err(StorageError::BudgetExceeded {
                    budget_id: budget.id,
                    requested: allocation.reserved_amount,
                    remaining,
                });
            }
        }
    }

    let usage_sequence_start =
        sqlx::query_scalar::<_, i64>("SELECT COALESCE(MAX(sequence), 0) FROM usage_ledger")
            .fetch_one(&mut **tx)
            .await?;
    let result = sqlx::query(
        "INSERT INTO budget_reservations \
         (id, job_id, status, created_at, expires_at, reconciled_at, usage_sequence_start, payload) \
         VALUES (?, ?, ?, ?, ?, ?, ?, ?)",
    )
    .bind(reservation.id.to_string())
    .bind(reservation.job_id.to_string())
    .bind(enum_text(&reservation.status)?)
    .bind(reservation.created_at.to_rfc3339())
    .bind(reservation.expires_at.map(|value| value.to_rfc3339()))
    .bind(reservation.reconciled_at.map(|value| value.to_rfc3339()))
    .bind(usage_sequence_start)
    .bind(encode(reservation)?)
    .execute(&mut **tx)
    .await;
    match result {
        Ok(_) => {}
        Err(error) if StorageError::is_unique_violation(&error) => {
            return Err(StorageError::Conflict {
                entity: "active budget reservation",
                id: reservation.job_id.to_string(),
            });
        }
        Err(error) => return Err(error.into()),
    }
    for allocation in &reservation.allocations {
        sqlx::query(
            "INSERT INTO budget_allocations \
             (reservation_id, budget_id, reserved_amount, actual_amount) VALUES (?, ?, ?, ?)",
        )
        .bind(reservation.id.to_string())
        .bind(allocation.budget_id.to_string())
        .bind(allocation.reserved_amount)
        .bind(allocation.actual_amount)
        .execute(&mut **tx)
        .await?;
    }
    Ok(())
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct PeriodWindow {
    starts_at: DateTime<Utc>,
    ends_at: DateTime<Utc>,
}

impl PeriodWindow {
    fn contains(self, timestamp: DateTime<Utc>) -> bool {
        timestamp >= self.starts_at && timestamp < self.ends_at
    }
}

fn period_window(period: BudgetPeriod, timestamp: DateTime<Utc>) -> Option<PeriodWindow> {
    match period {
        BudgetPeriod::Job | BudgetPeriod::Lifetime => None,
        BudgetPeriod::Daily => {
            let starts_at = Utc
                .with_ymd_and_hms(
                    timestamp.year(),
                    timestamp.month(),
                    timestamp.day(),
                    0,
                    0,
                    0,
                )
                .single()
                .expect("a UTC date from an existing timestamp is always valid");
            Some(PeriodWindow {
                starts_at,
                ends_at: starts_at + Duration::days(1),
            })
        }
        BudgetPeriod::Monthly => {
            let starts_at = Utc
                .with_ymd_and_hms(timestamp.year(), timestamp.month(), 1, 0, 0, 0)
                .single()
                .expect("the first day of an existing UTC month is always valid");
            let (next_year, next_month) = if timestamp.month() == 12 {
                (timestamp.year() + 1, 1)
            } else {
                (timestamp.year(), timestamp.month() + 1)
            };
            let ends_at = Utc
                .with_ymd_and_hms(next_year, next_month, 1, 0, 0, 0)
                .single()
                .expect("the first day of the next UTC month is always valid");
            Some(PeriodWindow { starts_at, ends_at })
        }
    }
}

fn normalize_budget_for_upsert(budget: &mut Budget) {
    match budget.period {
        BudgetPeriod::Job => {
            budget.used = 0;
            budget.period_started_at = budget.created_at;
            budget.period_ends_at = None;
        }
        BudgetPeriod::Lifetime => {
            budget.period_ends_at = None;
        }
        BudgetPeriod::Daily | BudgetPeriod::Monthly => {
            let window = period_window(budget.period, budget.updated_at)
                .expect("daily/monthly budgets always have a period window");
            if !window.contains(budget.period_started_at) {
                budget.used = 0;
            }
            budget.period_started_at = window.starts_at;
            budget.period_ends_at = Some(window.ends_at);
        }
    }
}

async fn ensure_current_period_tx(
    tx: &mut Transaction<'_, Sqlite>,
    mut budget: Budget,
    now: DateTime<Utc>,
) -> Result<Budget> {
    let original = (budget.used, budget.period_started_at, budget.period_ends_at);
    match budget.period {
        BudgetPeriod::Job => {
            budget.used = 0;
            budget.period_started_at = budget.created_at;
            budget.period_ends_at = None;
        }
        BudgetPeriod::Lifetime => {
            budget.period_ends_at = None;
        }
        BudgetPeriod::Daily | BudgetPeriod::Monthly => {
            let window = period_window(budget.period, now)
                .expect("daily/monthly budgets always have a period window");
            let legacy_seed = if window.contains(budget.period_started_at) {
                budget.used
            } else {
                0
            };
            budget.used =
                get_or_create_period_usage_tx(tx, budget.id, window, legacy_seed, now).await?;
            budget.period_started_at = window.starts_at;
            budget.period_ends_at = Some(window.ends_at);
        }
    }
    if original != (budget.used, budget.period_started_at, budget.period_ends_at) {
        budget.updated_at = now;
        persist_budget_state_tx(tx, &budget).await?;
    }
    Ok(budget)
}

async fn persist_budget_state_tx(tx: &mut Transaction<'_, Sqlite>, budget: &Budget) -> Result<()> {
    sqlx::query(
        "UPDATE budgets SET used_value = ?, period_started_at = ?, period_ends_at = ?, \
         updated_at = ?, payload = ? WHERE id = ?",
    )
    .bind(budget.used)
    .bind(budget.period_started_at.to_rfc3339())
    .bind(budget.period_ends_at.map(|value| value.to_rfc3339()))
    .bind(budget.updated_at.to_rfc3339())
    .bind(encode(budget)?)
    .bind(budget.id.to_string())
    .execute(&mut **tx)
    .await?;
    Ok(())
}

async fn get_or_create_period_usage_tx(
    tx: &mut Transaction<'_, Sqlite>,
    budget_id: BudgetId,
    window: PeriodWindow,
    initial_used: i64,
    now: DateTime<Utc>,
) -> Result<i64> {
    let existing = sqlx::query_scalar::<_, i64>(
        "SELECT used_value FROM budget_period_usage \
         WHERE budget_id = ? AND period_started_at = ?",
    )
    .bind(budget_id.to_string())
    .bind(window.starts_at.to_rfc3339())
    .fetch_optional(&mut **tx)
    .await?;
    if let Some(used) = existing {
        return Ok(used);
    }
    sqlx::query(
        "INSERT INTO budget_period_usage \
         (budget_id, period_started_at, period_ends_at, used_value, updated_at) \
         VALUES (?, ?, ?, ?, ?)",
    )
    .bind(budget_id.to_string())
    .bind(window.starts_at.to_rfc3339())
    .bind(window.ends_at.to_rfc3339())
    .bind(initial_used)
    .bind(now.to_rfc3339())
    .execute(&mut **tx)
    .await?;
    Ok(initial_used)
}

async fn set_period_usage_tx(
    tx: &mut Transaction<'_, Sqlite>,
    budget_id: BudgetId,
    window: PeriodWindow,
    used: i64,
    now: DateTime<Utc>,
) -> Result<()> {
    sqlx::query(
        "INSERT INTO budget_period_usage \
         (budget_id, period_started_at, period_ends_at, used_value, updated_at) \
         VALUES (?, ?, ?, ?, ?) \
         ON CONFLICT(budget_id, period_started_at) DO UPDATE SET \
         period_ends_at = excluded.period_ends_at, used_value = excluded.used_value, \
         updated_at = excluded.updated_at",
    )
    .bind(budget_id.to_string())
    .bind(window.starts_at.to_rfc3339())
    .bind(window.ends_at.to_rfc3339())
    .bind(used)
    .bind(now.to_rfc3339())
    .execute(&mut **tx)
    .await?;
    Ok(())
}

async fn add_period_usage_tx(
    tx: &mut Transaction<'_, Sqlite>,
    budget_id: BudgetId,
    window: PeriodWindow,
    amount: i64,
    now: DateTime<Utc>,
) -> Result<i64> {
    let used = get_or_create_period_usage_tx(tx, budget_id, window, 0, now).await?;
    let updated = used.saturating_add(amount);
    set_period_usage_tx(tx, budget_id, window, updated, now).await?;
    Ok(updated)
}

async fn active_reserved_tx(
    tx: &mut Transaction<'_, Sqlite>,
    budget: &Budget,
    now: DateTime<Utc>,
) -> Result<i64> {
    if budget.period == BudgetPeriod::Job {
        return Ok(0);
    }
    let reserved = if let Some(window) = period_window(budget.period, now) {
        sqlx::query_scalar(
            "SELECT COALESCE(SUM(a.reserved_amount), 0) \
             FROM budget_allocations a \
             JOIN budget_reservations r ON r.id = a.reservation_id \
             WHERE a.budget_id = ? AND r.status = 'active' \
             AND (r.expires_at IS NULL OR r.expires_at > ?) \
             AND r.created_at >= ? AND r.created_at < ?",
        )
        .bind(budget.id.to_string())
        .bind(now.to_rfc3339())
        .bind(window.starts_at.to_rfc3339())
        .bind(window.ends_at.to_rfc3339())
        .fetch_one(&mut **tx)
        .await?
    } else {
        sqlx::query_scalar(
            "SELECT COALESCE(SUM(a.reserved_amount), 0) \
             FROM budget_allocations a \
             JOIN budget_reservations r ON r.id = a.reservation_id \
             WHERE a.budget_id = ? AND r.status = 'active' \
             AND (r.expires_at IS NULL OR r.expires_at > ?)",
        )
        .bind(budget.id.to_string())
        .bind(now.to_rfc3339())
        .fetch_one(&mut **tx)
        .await?
    };
    Ok(reserved)
}

async fn lock_reservation_tx(tx: &mut Transaction<'_, Sqlite>, id: ReservationId) -> Result<()> {
    // SQLite transactions are deferred. This harmless write obtains the writer
    // lock before state is read, keeping reserve/reconcile/release race-safe.
    sqlx::query("UPDATE budget_reservations SET status = status WHERE id = ?")
        .bind(id.to_string())
        .execute(&mut **tx)
        .await?;
    Ok(())
}

fn decode_budget_row(row: &sqlx::sqlite::SqliteRow) -> Result<Budget> {
    let mut budget: Budget = decode(row.get::<&str, _>("payload"))?;
    budget.used = row.get("used_value");
    Ok(budget)
}

async fn load_budget_tx(tx: &mut Transaction<'_, Sqlite>, id: BudgetId) -> Result<Option<Budget>> {
    let row = sqlx::query("SELECT used_value, payload FROM budgets WHERE id = ?")
        .bind(id.to_string())
        .fetch_optional(&mut **tx)
        .await?;
    row.as_ref().map(decode_budget_row).transpose()
}

async fn load_reservation_tx(
    tx: &mut Transaction<'_, Sqlite>,
    id: ReservationId,
) -> Result<Option<BudgetReservation>> {
    let row = sqlx::query("SELECT status, payload FROM budget_reservations WHERE id = ?")
        .bind(id.to_string())
        .fetch_optional(&mut **tx)
        .await?;
    row.as_ref().map(decode_reservation_row).transpose()
}

fn decode_reservation_row(row: &sqlx::sqlite::SqliteRow) -> Result<BudgetReservation> {
    let mut reservation: BudgetReservation = decode(row.get::<&str, _>("payload"))?;
    reservation.status = match row.get::<&str, _>("status") {
        "active" => ReservationStatus::Active,
        "reconciled" => ReservationStatus::Reconciled,
        "released" => ReservationStatus::Released,
        "expired" => ReservationStatus::Expired,
        value => {
            return Err(StorageError::InvalidData(format!(
                "unknown reservation status: {value}"
            )));
        }
    };
    Ok(reservation)
}

async fn expire_stale_reservations(
    tx: &mut Transaction<'_, Sqlite>,
    now: DateTime<Utc>,
) -> Result<()> {
    sqlx::query(
        "UPDATE budget_reservations SET status = 'expired' \
         WHERE status = 'active' AND expires_at IS NOT NULL AND expires_at <= ?",
    )
    .bind(now.to_rfc3339())
    .execute(&mut **tx)
    .await?;
    Ok(())
}
