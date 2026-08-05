use audiobookai_core::{
    JobId, ProjectId, ProviderProfileId, UsageEvent, UsageEventId, UsageQuantities, UsageTotals,
    Validate,
};
use chrono::{DateTime, Utc};
use sqlx::{QueryBuilder, Row, Sqlite, SqlitePool};

use crate::{Result, StorageError};

use super::util::{decode, encode, enum_text};

#[derive(Clone, Debug, Default)]
pub struct UsageFilter {
    pub project_id: Option<ProjectId>,
    pub job_id: Option<JobId>,
    pub provider_profile_id: Option<ProviderProfileId>,
    /// Excludes ledger rows at or before this append-only sequence boundary.
    pub sequence_after: Option<u64>,
    pub from: Option<DateTime<Utc>>,
    pub until: Option<DateTime<Utc>>,
    pub limit: Option<u32>,
}

#[derive(Clone, Debug)]
pub struct UsageRepository {
    pool: SqlitePool,
}

impl UsageRepository {
    #[must_use]
    pub fn new(pool: SqlitePool) -> Self {
        Self { pool }
    }

    pub async fn append(&self, event: &UsageEvent) -> Result<u64> {
        event.validate()?;
        let result = sqlx::query(
            "INSERT INTO usage_ledger \
             (id, occurred_at, workload, project_id, job_id, attempt_id, provider_id, proof_segment_id, \
              characters, audio_milliseconds, input_tokens, output_tokens, provider_credits, \
              cost_micros, currency, uncertain_charge, payload) \
             VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
        )
        .bind(event.id.to_string())
        .bind(event.occurred_at.to_rfc3339())
        .bind(enum_text(&event.workload)?)
        .bind(event.project_id.to_string())
        .bind(event.job_id.map(|id| id.to_string()))
        .bind(event.attempt_id.map(|id| id.to_string()))
        .bind(event.provider_profile_id.to_string())
        .bind(event.segment_id.map(|id| id.to_string()))
        .bind(unsigned(event.quantities.characters, "characters")?)
        .bind(unsigned(
            event.quantities.audio_milliseconds,
            "audio_milliseconds",
        )?)
        .bind(unsigned(event.quantities.input_tokens, "input_tokens")?)
        .bind(unsigned(event.quantities.output_tokens, "output_tokens")?)
        .bind(event.quantities.provider_credits)
        .bind(event.cost.as_ref().map(|cost| cost.micros))
        .bind(event.cost.as_ref().map(|cost| cost.currency.as_str()))
        .bind(event.uncertain_charge)
        .bind(encode(event)?)
        .execute(&self.pool)
        .await;

        match result {
            Ok(result) => u64::try_from(result.last_insert_rowid())
                .map_err(|_| StorageError::InvalidData("negative usage sequence".into())),
            Err(error) if StorageError::is_unique_violation(&error) => {
                Err(StorageError::Conflict {
                    entity: "usage_event",
                    id: event.id.to_string(),
                })
            }
            Err(error) => Err(error.into()),
        }
    }

    pub async fn get(&self, id: UsageEventId) -> Result<Option<UsageEvent>> {
        let payload =
            sqlx::query_scalar::<_, String>("SELECT payload FROM usage_ledger WHERE id = ?")
                .bind(id.to_string())
                .fetch_optional(&self.pool)
                .await?;
        payload.as_deref().map(decode).transpose()
    }

    pub async fn list(&self, filter: &UsageFilter) -> Result<Vec<UsageEvent>> {
        let mut query = QueryBuilder::<Sqlite>::new("SELECT payload FROM usage_ledger WHERE 1 = 1");
        if let Some(project_id) = filter.project_id {
            query
                .push(" AND project_id = ")
                .push_bind(project_id.to_string());
        }
        if let Some(job_id) = filter.job_id {
            query.push(" AND job_id = ").push_bind(job_id.to_string());
        }
        if let Some(provider_id) = filter.provider_profile_id {
            query
                .push(" AND provider_id = ")
                .push_bind(provider_id.to_string());
        }
        if let Some(sequence) = filter.sequence_after {
            query
                .push(" AND sequence > ")
                .push_bind(i64::try_from(sequence).unwrap_or(i64::MAX));
        }
        if let Some(from) = filter.from {
            query
                .push(" AND occurred_at >= ")
                .push_bind(from.to_rfc3339());
        }
        if let Some(until) = filter.until {
            query
                .push(" AND occurred_at < ")
                .push_bind(until.to_rfc3339());
        }
        query.push(" ORDER BY sequence DESC");
        if let Some(limit) = filter.limit {
            query.push(" LIMIT ").push_bind(i64::from(limit));
        }

        query
            .build()
            .fetch_all(&self.pool)
            .await?
            .into_iter()
            .map(|row| decode(row.get::<&str, _>("payload")))
            .collect()
    }

    pub async fn totals(&self, filter: &UsageFilter) -> Result<UsageTotals> {
        let mut all_filter = filter.clone();
        all_filter.limit = None;
        let events = self.list(&all_filter).await?;
        let mut totals = UsageTotals {
            event_count: events.len() as u64,
            ..UsageTotals::default()
        };
        for event in events {
            add_quantities(&mut totals.quantities, &event.quantities);
            if let Some(cost) = event.cost {
                *totals
                    .cost_by_currency_micros
                    .entry(cost.currency)
                    .or_default() += cost.micros;
            }
            if event.uncertain_charge {
                totals.uncertain_charge_count += 1;
            }
        }
        Ok(totals)
    }
}

fn unsigned(value: Option<u64>, field: &str) -> Result<Option<i64>> {
    value
        .map(|value| {
            i64::try_from(value).map_err(|_| {
                StorageError::InvalidData(format!("{field} exceeds SQLite's signed integer range"))
            })
        })
        .transpose()
}

fn add_quantities(total: &mut UsageQuantities, value: &UsageQuantities) {
    add_optional(&mut total.characters, value.characters);
    add_optional(&mut total.audio_milliseconds, value.audio_milliseconds);
    add_optional(&mut total.input_tokens, value.input_tokens);
    add_optional(&mut total.output_tokens, value.output_tokens);
    add_optional(&mut total.cache_read_tokens, value.cache_read_tokens);
    add_optional(&mut total.cache_write_tokens, value.cache_write_tokens);
    add_optional(&mut total.reasoning_tokens, value.reasoning_tokens);
    match (total.provider_credits.as_mut(), value.provider_credits) {
        (Some(total), Some(value)) => *total = total.saturating_add(value),
        (None, Some(value)) => total.provider_credits = Some(value),
        _ => {}
    }
}

fn add_optional(total: &mut Option<u64>, value: Option<u64>) {
    match (total.as_mut(), value) {
        (Some(total), Some(value)) => *total = total.saturating_add(value),
        (None, Some(value)) => *total = Some(value),
        _ => {}
    }
}
