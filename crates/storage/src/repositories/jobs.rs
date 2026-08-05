use audiobookai_core::{
    BudgetReservation, ExportLayout, Job, JobAttempt, JobId, JobState, JobUnit, JobUnitId,
    ProjectId, Validate,
};
use chrono::{DateTime, Utc};
use sqlx::{Row, Sqlite, SqlitePool, Transaction};
use unicode_normalization::UnicodeNormalization as _;

use crate::{Result, StorageError};

use super::util::{decode, encode, enum_text};

/// Produces the conservative cross-platform key used for export-path ownership.
///
/// NFC closes canonical-equivalence aliases on normalization-insensitive filesystems such as the
/// default macOS APFS configuration. Slash and case normalization intentionally over-reserve on
/// some filesystems rather than allowing two paid jobs to discover an alias only at promotion.
#[must_use]
pub fn normalize_output_destination_key(value: &str) -> String {
    value
        .replace('\\', "/")
        .trim_end_matches('/')
        .chars()
        // Upper-then-lower is deliberately more conservative than lowercase alone: it merges
        // context-sensitive forms such as Greek final sigma that case-insensitive APFS aliases.
        .flat_map(char::to_uppercase)
        .flat_map(char::to_lowercase)
        .nfc()
        .collect()
}

fn protected_output_keys(key: &str, layout: ExportLayout) -> Vec<String> {
    let normalized = normalize_output_destination_key(key);
    let mut keys = vec![normalized.clone()];
    if layout == ExportLayout::SingleFile {
        keys.push(format!("{normalized}.manifest.json"));
    }
    keys
}

fn output_keys_overlap(left: &str, right: &str) -> bool {
    left == right
        || left
            .strip_prefix(right)
            .is_some_and(|suffix| suffix.starts_with('/'))
        || right
            .strip_prefix(left)
            .is_some_and(|suffix| suffix.starts_with('/'))
}

fn reservation_keys_overlap(
    left_key: &str,
    left_layout: ExportLayout,
    right_key: &str,
    right_layout: ExportLayout,
) -> bool {
    protected_output_keys(left_key, left_layout)
        .iter()
        .any(|left| {
            protected_output_keys(right_key, right_layout)
                .iter()
                .any(|right| output_keys_overlap(left, right))
        })
}

/// Durable ownership of the exact filesystem destination used by an export job.
///
/// A failed/cancelled job retains a `Promoting` row so a later job cannot adopt partial output. A
/// still-`Reserved` terminal claim can be released, while a completed job releases `Promoted`
/// ownership because the finished filesystem path itself prevents reuse.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct OutputDestinationReservation {
    pub job_id: JobId,
    pub project_id: ProjectId,
    pub destination_key: String,
    pub destination_path: String,
    pub layout: ExportLayout,
    pub state: OutputReservationState,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
    pub promoted_at: Option<DateTime<Utc>>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum OutputReservationState {
    Reserved,
    Promoting,
    Promoted,
}

impl OutputReservationState {
    const fn as_str(self) -> &'static str {
        match self {
            Self::Reserved => "reserved",
            Self::Promoting => "promoting",
            Self::Promoted => "promoted",
        }
    }

    fn parse(value: &str) -> Result<Self> {
        match value {
            "reserved" => Ok(Self::Reserved),
            "promoting" => Ok(Self::Promoting),
            "promoted" => Ok(Self::Promoted),
            other => Err(StorageError::InvalidData(format!(
                "unknown output reservation state: {other}"
            ))),
        }
    }
}

#[derive(Clone, Debug)]
pub struct JobRepository {
    pool: SqlitePool,
}

impl JobRepository {
    #[must_use]
    pub fn new(pool: SqlitePool) -> Self {
        Self { pool }
    }

    pub async fn insert(&self, job: &Job) -> Result<()> {
        job.validate()?;
        let result = sqlx::query(
            "INSERT INTO jobs \
             (id, project_id, export_profile_id, reservation_id, kind, state, revision, created_at, updated_at, payload) \
             VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
        )
        .bind(job.id.to_string())
        .bind(job.project_id.to_string())
        .bind(job.export_profile_id.map(|id| id.to_string()))
        .bind(job.reservation_id.map(|id| id.to_string()))
        .bind(enum_text(&job.kind)?)
        .bind(enum_text(&job.state)?)
        .bind(i64::try_from(job.revision).unwrap_or(i64::MAX))
        .bind(job.created_at.to_rfc3339())
        .bind(job.updated_at.to_rfc3339())
        .bind(encode(job)?)
        .execute(&self.pool)
        .await;
        match result {
            Ok(_) => Ok(()),
            Err(error) if StorageError::is_unique_violation(&error) => {
                Err(StorageError::Conflict {
                    entity: "job",
                    id: job.id.to_string(),
                })
            }
            Err(error) => Err(error.into()),
        }
    }

    /// Makes a conversion job and its exclusive output claim visible atomically.
    pub async fn insert_with_output_reservation(
        &self,
        job: &Job,
        reservation: &OutputDestinationReservation,
    ) -> Result<()> {
        job.validate()?;
        validate_output_reservation(job, reservation)?;
        let mut tx = self.pool.begin().await?;
        insert_job(&mut tx, job).await?;
        insert_output_reservation(&mut tx, reservation).await?;
        tx.commit().await?;
        Ok(())
    }

    /// Atomically updates an existing job and reacquires a previously released output claim.
    /// This is used by retry/resume so the job can never become runnable without ownership.
    pub async fn update_with_output_reservation(
        &self,
        job: &Job,
        expected_revision: u64,
        reservation: &OutputDestinationReservation,
    ) -> Result<Job> {
        job.validate()?;
        validate_output_reservation(job, reservation)?;
        let next_revision = expected_revision.saturating_add(1);
        let mut stored = job.clone();
        stored.revision = next_revision;
        let mut tx = self.pool.begin().await?;
        insert_output_reservation(&mut tx, reservation).await?;
        let result = update_job(&mut tx, &stored, expected_revision, next_revision).await?;
        if result == 0 {
            return Err(StorageError::StaleRevision {
                entity: "job",
                id: stored.id.to_string(),
            });
        }
        tx.commit().await?;
        Ok(stored)
    }

    /// Atomically admits a manual retry. The job cannot become runnable unless its new budget
    /// cycle and any reacquired output destination are both durable in the same commit.
    pub async fn update_with_retry_admission(
        &self,
        job: &Job,
        expected_revision: u64,
        budget_reservation: Option<&BudgetReservation>,
        output_reservation: Option<&OutputDestinationReservation>,
    ) -> Result<Job> {
        job.validate()?;
        if job.state != JobState::Queued {
            return Err(StorageError::InvalidData(
                "retry admission requires a queued job".to_owned(),
            ));
        }
        match budget_reservation {
            Some(reservation)
                if reservation.job_id == job.id && job.reservation_id == Some(reservation.id) => {}
            None if job.reservation_id.is_none() => {}
            _ => {
                return Err(StorageError::InvalidData(
                    "retry budget reservation does not match its queued job".to_owned(),
                ));
            }
        }
        if let Some(reservation) = output_reservation {
            validate_output_reservation(job, reservation)?;
        }

        let next_revision = expected_revision.saturating_add(1);
        let mut stored = job.clone();
        stored.revision = next_revision;
        let mut tx = self.pool.begin().await?;
        // Obtain SQLite's writer lock before validating the predecessor cycle. A retry must not
        // replace the job pointer while its previous active/expired reservation still needs
        // reconciliation, even when the new retry itself is provider-free.
        sqlx::query("UPDATE jobs SET revision = revision WHERE id = ?")
            .bind(stored.id.to_string())
            .execute(&mut *tx)
            .await?;
        let predecessor =
            sqlx::query("SELECT revision, state, reservation_id FROM jobs WHERE id = ?")
                .bind(stored.id.to_string())
                .fetch_optional(&mut *tx)
                .await?
                .ok_or_else(|| StorageError::NotFound {
                    entity: "job",
                    id: stored.id.to_string(),
                })?;
        if predecessor.get::<i64, _>("revision")
            != i64::try_from(expected_revision).unwrap_or(i64::MAX)
        {
            return Err(StorageError::StaleRevision {
                entity: "job",
                id: stored.id.to_string(),
            });
        }
        if predecessor.get::<&str, _>("state") != "failed" {
            return Err(StorageError::Conflict {
                entity: "retry job state",
                id: stored.id.to_string(),
            });
        }
        if let Some(predecessor_id) = predecessor.get::<Option<&str>, _>("reservation_id") {
            let next_id = budget_reservation.map(|reservation| reservation.id.to_string());
            if next_id.as_deref() == Some(predecessor_id) {
                return Err(StorageError::InvalidData(
                    "manual retry requires a fresh budget reservation id".to_owned(),
                ));
            }
            let status = sqlx::query_scalar::<_, String>(
                "SELECT status FROM budget_reservations WHERE id = ?",
            )
            .bind(predecessor_id)
            .fetch_optional(&mut *tx)
            .await?;
            if status
                .as_deref()
                .is_some_and(|status| matches!(status, "active" | "expired"))
            {
                return Err(StorageError::Conflict {
                    entity: "retry budget predecessor",
                    id: predecessor_id.to_owned(),
                });
            }
        }
        if let Some(reservation) = budget_reservation {
            super::budgets::insert_budget_reservation_tx(
                &mut tx,
                reservation,
                !job.allow_budget_override,
            )
            .await?;
        }
        if let Some(reservation) = output_reservation {
            insert_output_reservation(&mut tx, reservation).await?;
        }
        let result = update_job(&mut tx, &stored, expected_revision, next_revision).await?;
        if result == 0 {
            return Err(StorageError::StaleRevision {
                entity: "job",
                id: stored.id.to_string(),
            });
        }
        tx.commit().await?;
        Ok(stored)
    }

    /// Persists a Failed/Cancelled transition and releases a never-promoted claim in one commit.
    /// A retry therefore cannot race between observing the terminal state and claim deletion.
    pub async fn update_terminal_with_output_release(
        &self,
        job: &Job,
        expected_revision: u64,
    ) -> Result<Job> {
        job.validate()?;
        if !matches!(job.state, JobState::Failed | JobState::Cancelled) {
            return Err(StorageError::InvalidData(
                "output claim release requires a failed or cancelled job".to_owned(),
            ));
        }
        let next_revision = expected_revision.saturating_add(1);
        let mut stored = job.clone();
        stored.revision = next_revision;
        let mut tx = self.pool.begin().await?;
        let result = update_job(&mut tx, &stored, expected_revision, next_revision).await?;
        if result == 0 {
            return Err(StorageError::StaleRevision {
                entity: "job",
                id: stored.id.to_string(),
            });
        }
        sqlx::query(
            "DELETE FROM output_destination_reservations \
             WHERE job_id = ? AND state = 'reserved'",
        )
        .bind(stored.id.to_string())
        .execute(&mut *tx)
        .await?;
        tx.commit().await?;
        Ok(stored)
    }

    pub async fn get_output_reservation(
        &self,
        job_id: JobId,
    ) -> Result<Option<OutputDestinationReservation>> {
        let row = sqlx::query(
            "SELECT job_id, project_id, destination_key, destination_path, layout, state, \
             created_at, updated_at, promoted_at \
             FROM output_destination_reservations WHERE job_id = ?",
        )
        .bind(job_id.to_string())
        .fetch_optional(&self.pool)
        .await?;
        row.as_ref().map(decode_output_reservation).transpose()
    }

    /// Finds a reservation whose destination is exactly this path or one of its ancestors.
    pub async fn find_output_reservation_containing_path(
        &self,
        destination_key: &str,
    ) -> Result<Option<OutputDestinationReservation>> {
        let rows = sqlx::query(
            "SELECT job_id, project_id, destination_key, destination_path, layout, state, \
             created_at, updated_at, promoted_at \
             FROM output_destination_reservations",
        )
        .fetch_all(&self.pool)
        .await?;
        let candidate = normalize_output_destination_key(destination_key);
        let mut containing = None;
        let mut containing_length = 0;
        for row in &rows {
            let reservation = decode_output_reservation(row)?;
            for protected in protected_output_keys(&reservation.destination_key, reservation.layout)
            {
                let contains = candidate == protected
                    || candidate
                        .strip_prefix(&protected)
                        .is_some_and(|suffix| suffix.starts_with('/'));
                if contains && protected.len() >= containing_length {
                    containing_length = protected.len();
                    containing = Some(reservation.clone());
                }
            }
        }
        Ok(containing)
    }

    /// Acquires a missing claim for a legacy active job before any worker is restarted.
    pub async fn acquire_output_reservation_for_existing_job(
        &self,
        job: &Job,
        reservation: &OutputDestinationReservation,
    ) -> Result<()> {
        job.validate()?;
        validate_output_reservation(job, reservation)?;
        let mut tx = self.pool.begin().await?;
        let stored = sqlx::query("SELECT project_id, revision FROM jobs WHERE id = ?")
            .bind(job.id.to_string())
            .fetch_optional(&mut *tx)
            .await?
            .ok_or_else(|| StorageError::NotFound {
                entity: "job",
                id: job.id.to_string(),
            })?;
        if stored.get::<String, _>("project_id") != job.project_id.to_string()
            || stored.get::<i64, _>("revision") != i64::try_from(job.revision).unwrap_or(i64::MAX)
        {
            return Err(StorageError::StaleRevision {
                entity: "job",
                id: job.id.to_string(),
            });
        }
        insert_output_reservation(&mut tx, reservation).await?;
        tx.commit().await?;
        Ok(())
    }

    /// Records that a job-owned promotion marker exists and final-path promotion may have begun.
    pub async fn mark_output_promoting(&self, job_id: JobId, now: DateTime<Utc>) -> Result<()> {
        let result = sqlx::query(
            "UPDATE output_destination_reservations SET state = 'promoting', updated_at = ? \
             WHERE job_id = ? AND state = 'reserved'",
        )
        .bind(now.to_rfc3339())
        .bind(job_id.to_string())
        .execute(&self.pool)
        .await?;
        if result.rows_affected() == 0 {
            let existing = self.get_output_reservation(job_id).await?;
            if !existing.is_some_and(|value| {
                matches!(
                    value.state,
                    OutputReservationState::Promoting | OutputReservationState::Promoted
                )
            }) {
                return Err(StorageError::NotFound {
                    entity: "output destination reservation",
                    id: job_id.to_string(),
                });
            }
        }
        Ok(())
    }

    /// Records completion without releasing ownership of the promoted output.
    pub async fn mark_output_promoted(&self, job_id: JobId, now: DateTime<Utc>) -> Result<()> {
        let result = sqlx::query(
            "UPDATE output_destination_reservations \
             SET state = 'promoted', updated_at = ?, promoted_at = COALESCE(promoted_at, ?) \
             WHERE job_id = ?",
        )
        .bind(now.to_rfc3339())
        .bind(now.to_rfc3339())
        .bind(job_id.to_string())
        .execute(&self.pool)
        .await?;
        if result.rows_affected() == 0 {
            return Err(StorageError::NotFound {
                entity: "output destination reservation",
                id: job_id.to_string(),
            });
        }
        Ok(())
    }

    /// Releases only an output claim for which final-path promotion never began.
    pub async fn release_unpromoted_output_reservation(&self, job_id: JobId) -> Result<bool> {
        let result = sqlx::query(
            "DELETE FROM output_destination_reservations \
             WHERE job_id = ? AND state = 'reserved'",
        )
        .bind(job_id.to_string())
        .execute(&self.pool)
        .await?;
        Ok(result.rows_affected() != 0)
    }

    /// Releases a promoted claim only after the owning job is durably completed. The output path
    /// itself then prevents reuse until the user removes it.
    pub async fn release_completed_output_reservation(&self, job_id: JobId) -> Result<bool> {
        let result = sqlx::query(
            "DELETE FROM output_destination_reservations \
             WHERE job_id = ? AND state = 'promoted' \
             AND EXISTS (SELECT 1 FROM jobs WHERE id = ? AND state = 'completed')",
        )
        .bind(job_id.to_string())
        .bind(job_id.to_string())
        .execute(&self.pool)
        .await?;
        Ok(result.rows_affected() != 0)
    }

    /// Repairs a crash between a terminal job transition and its safe claim release.
    pub async fn release_terminal_output_reservations(&self) -> Result<u64> {
        let result = sqlx::query(
            "DELETE FROM output_destination_reservations \
             WHERE (state = 'reserved' AND EXISTS ( \
                       SELECT 1 FROM jobs WHERE jobs.id = output_destination_reservations.job_id \
                       AND jobs.state IN ('failed', 'cancelled'))) \
                OR (state = 'promoted' AND EXISTS ( \
                       SELECT 1 FROM jobs WHERE jobs.id = output_destination_reservations.job_id \
                       AND jobs.state = 'completed'))",
        )
        .execute(&self.pool)
        .await?;
        Ok(result.rows_affected())
    }

    pub async fn get(&self, id: JobId) -> Result<Option<Job>> {
        let row = sqlx::query("SELECT revision, payload FROM jobs WHERE id = ?")
            .bind(id.to_string())
            .fetch_optional(&self.pool)
            .await?;
        row.as_ref().map(decode_job_row).transpose()
    }

    pub async fn list_for_project(
        &self,
        project_id: audiobookai_core::ProjectId,
    ) -> Result<Vec<Job>> {
        let rows = sqlx::query(
            "SELECT revision, payload FROM jobs WHERE project_id = ? ORDER BY created_at DESC",
        )
        .bind(project_id.to_string())
        .fetch_all(&self.pool)
        .await?;
        rows.iter().map(decode_job_row).collect()
    }

    pub async fn list_active(&self) -> Result<Vec<Job>> {
        let rows = sqlx::query(
            "SELECT revision, payload FROM jobs \
             WHERE state NOT IN ('cancelled', 'failed', 'completed') ORDER BY created_at",
        )
        .fetch_all(&self.pool)
        .await?;
        rows.iter().map(decode_job_row).collect()
    }

    pub async fn update(&self, job: &Job, expected_revision: u64) -> Result<Job> {
        job.validate()?;
        let next_revision = expected_revision.saturating_add(1);
        let mut stored = job.clone();
        stored.revision = next_revision;
        let result = sqlx::query(
            "UPDATE jobs SET export_profile_id = ?, reservation_id = ?, state = ?, revision = ?, \
             updated_at = ?, payload = ? WHERE id = ? AND revision = ?",
        )
        .bind(stored.export_profile_id.map(|id| id.to_string()))
        .bind(stored.reservation_id.map(|id| id.to_string()))
        .bind(enum_text(&stored.state)?)
        .bind(i64::try_from(next_revision).unwrap_or(i64::MAX))
        .bind(stored.updated_at.to_rfc3339())
        .bind(encode(&stored)?)
        .bind(stored.id.to_string())
        .bind(i64::try_from(expected_revision).unwrap_or(i64::MAX))
        .execute(&self.pool)
        .await?;
        if result.rows_affected() == 0 {
            return Err(self.stale_or_missing(stored.id).await?);
        }
        Ok(stored)
    }

    pub async fn transition(
        &self,
        id: JobId,
        expected_revision: u64,
        next: JobState,
        now: DateTime<Utc>,
    ) -> Result<Job> {
        let mut job = self.get(id).await?.ok_or_else(|| StorageError::NotFound {
            entity: "job",
            id: id.to_string(),
        })?;
        if job.revision != expected_revision {
            return Err(StorageError::StaleRevision {
                entity: "job",
                id: id.to_string(),
            });
        }
        job.transition(next, now)?;
        self.update(&job, expected_revision).await
    }

    pub async fn upsert_unit(&self, unit: &JobUnit) -> Result<()> {
        let mut tx = self.pool.begin().await?;
        sqlx::query(
            "INSERT INTO job_units \
             (id, job_id, chapter_id, segment_id, proof_segment_id, provider_id, kind, state, next_attempt_at, updated_at, payload) \
             VALUES (?, ?, ?, NULL, ?, ?, ?, ?, ?, ?, ?) \
             ON CONFLICT(id) DO UPDATE SET state = excluded.state, next_attempt_at = excluded.next_attempt_at, \
             proof_segment_id = excluded.proof_segment_id, updated_at = excluded.updated_at, payload = excluded.payload",
        )
        .bind(unit.id.to_string())
        .bind(unit.job_id.to_string())
        .bind(unit.chapter_id.map(|id| id.to_string()))
        .bind(unit.segment_id.map(|id| id.to_string()))
        .bind(unit.provider_profile_id.map(|id| id.to_string()))
        .bind(enum_text(&unit.kind)?)
        .bind(enum_text(&unit.state)?)
        .bind(unit.next_attempt_at.map(|value| value.to_rfc3339()))
        .bind(unit.updated_at.to_rfc3339())
        .bind(encode(unit)?)
        .execute(&mut *tx)
        .await?;
        sqlx::query("DELETE FROM job_unit_dependencies WHERE job_unit_id = ?")
            .bind(unit.id.to_string())
            .execute(&mut *tx)
            .await?;
        for dependency in &unit.dependencies {
            sqlx::query(
                "INSERT INTO job_unit_dependencies (job_unit_id, depends_on_id) VALUES (?, ?)",
            )
            .bind(unit.id.to_string())
            .bind(dependency.to_string())
            .execute(&mut *tx)
            .await?;
        }
        tx.commit().await?;
        Ok(())
    }

    pub async fn get_unit(&self, id: JobUnitId) -> Result<Option<JobUnit>> {
        let payload = sqlx::query_scalar::<_, String>("SELECT payload FROM job_units WHERE id = ?")
            .bind(id.to_string())
            .fetch_optional(&self.pool)
            .await?;
        payload.as_deref().map(decode).transpose()
    }

    pub async fn list_units(&self, job_id: JobId) -> Result<Vec<JobUnit>> {
        let rows = sqlx::query("SELECT payload FROM job_units WHERE job_id = ? ORDER BY rowid")
            .bind(job_id.to_string())
            .fetch_all(&self.pool)
            .await?;
        rows.into_iter()
            .map(|row| decode(row.get::<&str, _>("payload")))
            .collect()
    }

    pub async fn insert_attempt(&self, attempt: &JobAttempt) -> Result<()> {
        sqlx::query(
            "INSERT INTO job_attempts \
             (id, job_unit_id, ordinal, started_at, finished_at, failure_class, uncertain_charge, payload) \
             VALUES (?, ?, ?, ?, ?, ?, ?, ?)",
        )
        .bind(attempt.id.to_string())
        .bind(attempt.job_unit_id.to_string())
        .bind(i64::from(attempt.ordinal))
        .bind(attempt.started_at.to_rfc3339())
        .bind(attempt.finished_at.map(|value| value.to_rfc3339()))
        .bind(attempt.failure_class.as_ref().map(enum_text).transpose()?)
        .bind(attempt.uncertain_charge)
        .bind(encode(attempt)?)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    async fn stale_or_missing(&self, id: JobId) -> Result<StorageError> {
        let exists = sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM jobs WHERE id = ?")
            .bind(id.to_string())
            .fetch_one(&self.pool)
            .await?
            > 0;
        Ok(if exists {
            StorageError::StaleRevision {
                entity: "job",
                id: id.to_string(),
            }
        } else {
            StorageError::NotFound {
                entity: "job",
                id: id.to_string(),
            }
        })
    }
}

pub(super) fn validate_output_reservation(
    job: &Job,
    reservation: &OutputDestinationReservation,
) -> Result<()> {
    if reservation.job_id != job.id
        || reservation.project_id != job.project_id
        || normalize_output_destination_key(&reservation.destination_key)
            .trim()
            .is_empty()
        || reservation.destination_path.trim().is_empty()
        || reservation.state != OutputReservationState::Reserved
        || reservation.promoted_at.is_some()
    {
        return Err(StorageError::InvalidData(
            "output destination reservation does not match its admitted job".to_owned(),
        ));
    }
    Ok(())
}

pub(super) async fn insert_output_reservation(
    tx: &mut Transaction<'_, Sqlite>,
    reservation: &OutputDestinationReservation,
) -> Result<()> {
    let destination_key = normalize_output_destination_key(&reservation.destination_key);
    let existing_rows = sqlx::query(
        "SELECT job_id, project_id, destination_key, destination_path, layout, state, \
         created_at, updated_at, promoted_at FROM output_destination_reservations",
    )
    .fetch_all(&mut **tx)
    .await?;
    for row in &existing_rows {
        let existing = decode_output_reservation(row)?;
        if reservation_keys_overlap(
            &destination_key,
            reservation.layout,
            &existing.destination_key,
            existing.layout,
        ) {
            return Err(StorageError::Conflict {
                entity: "output destination",
                id: reservation.destination_path.clone(),
            });
        }
    }
    let result = sqlx::query(
        "INSERT INTO output_destination_reservations \
         (job_id, project_id, destination_key, destination_path, layout, state, created_at, updated_at, promoted_at) \
         SELECT ?, ?, ?, ?, ?, ?, ?, ?, ? \
         WHERE NOT EXISTS ( \
             WITH existing_keys(key) AS ( \
                 SELECT lower(destination_key) FROM output_destination_reservations \
                 UNION ALL \
                 SELECT lower(destination_key) || '.manifest.json' \
                 FROM output_destination_reservations WHERE layout = 'single_file' \
             ), candidate_keys(key) AS ( \
                 SELECT lower(?) \
                 UNION ALL SELECT lower(?) || '.manifest.json' WHERE ? = 'single_file' \
             ) \
             SELECT 1 FROM existing_keys CROSS JOIN candidate_keys \
             WHERE existing_keys.key = candidate_keys.key \
                OR instr(existing_keys.key, candidate_keys.key || '/') = 1 \
                OR instr(candidate_keys.key, existing_keys.key || '/') = 1 \
         )",
    )
    .bind(reservation.job_id.to_string())
    .bind(reservation.project_id.to_string())
    .bind(&destination_key)
    .bind(&reservation.destination_path)
    .bind(enum_text(&reservation.layout)?)
    .bind(reservation.state.as_str())
    .bind(reservation.created_at.to_rfc3339())
    .bind(reservation.updated_at.to_rfc3339())
    .bind(reservation.promoted_at.map(|value| value.to_rfc3339()))
    .bind(&destination_key)
    .bind(&destination_key)
    .bind(enum_text(&reservation.layout)?)
    .execute(&mut **tx)
    .await;
    match result {
        Ok(result) if result.rows_affected() == 1 => Ok(()),
        Ok(_) => Err(StorageError::Conflict {
            entity: "output destination",
            id: reservation.destination_path.clone(),
        }),
        Err(error) if StorageError::is_unique_violation(&error) => Err(StorageError::Conflict {
            entity: "output destination",
            id: reservation.destination_path.clone(),
        }),
        Err(error) => Err(error.into()),
    }
}

async fn insert_job(tx: &mut Transaction<'_, Sqlite>, job: &Job) -> Result<()> {
    let result = sqlx::query(
        "INSERT INTO jobs \
         (id, project_id, export_profile_id, reservation_id, kind, state, revision, created_at, updated_at, payload) \
         VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
    )
    .bind(job.id.to_string())
    .bind(job.project_id.to_string())
    .bind(job.export_profile_id.map(|id| id.to_string()))
    .bind(job.reservation_id.map(|id| id.to_string()))
    .bind(enum_text(&job.kind)?)
    .bind(enum_text(&job.state)?)
    .bind(i64::try_from(job.revision).unwrap_or(i64::MAX))
    .bind(job.created_at.to_rfc3339())
    .bind(job.updated_at.to_rfc3339())
    .bind(encode(job)?)
    .execute(&mut **tx)
    .await;
    match result {
        Ok(_) => Ok(()),
        Err(error) if StorageError::is_unique_violation(&error) => Err(StorageError::Conflict {
            entity: "job",
            id: job.id.to_string(),
        }),
        Err(error) => Err(error.into()),
    }
}

pub(super) async fn update_job(
    tx: &mut Transaction<'_, Sqlite>,
    job: &Job,
    expected_revision: u64,
    next_revision: u64,
) -> Result<u64> {
    let result = sqlx::query(
        "UPDATE jobs SET export_profile_id = ?, reservation_id = ?, state = ?, revision = ?, \
         updated_at = ?, payload = ? WHERE id = ? AND revision = ?",
    )
    .bind(job.export_profile_id.map(|id| id.to_string()))
    .bind(job.reservation_id.map(|id| id.to_string()))
    .bind(enum_text(&job.state)?)
    .bind(i64::try_from(next_revision).unwrap_or(i64::MAX))
    .bind(job.updated_at.to_rfc3339())
    .bind(encode(job)?)
    .bind(job.id.to_string())
    .bind(i64::try_from(expected_revision).unwrap_or(i64::MAX))
    .execute(&mut **tx)
    .await?;
    Ok(result.rows_affected())
}

fn decode_output_reservation(
    row: &sqlx::sqlite::SqliteRow,
) -> Result<OutputDestinationReservation> {
    let layout = serde_json::from_value(serde_json::Value::String(row.get("layout")))?;
    Ok(OutputDestinationReservation {
        job_id: row
            .get::<String, _>("job_id")
            .parse()
            .map_err(|error| StorageError::InvalidData(format!("invalid job id: {error}")))?,
        project_id: row
            .get::<String, _>("project_id")
            .parse()
            .map_err(|error| StorageError::InvalidData(format!("invalid project id: {error}")))?,
        destination_key: normalize_output_destination_key(row.get::<&str, _>("destination_key")),
        destination_path: row.get("destination_path"),
        layout,
        state: OutputReservationState::parse(row.get::<&str, _>("state"))?,
        created_at: DateTime::parse_from_rfc3339(row.get::<&str, _>("created_at"))
            .map_err(|error| StorageError::InvalidData(error.to_string()))?
            .with_timezone(&Utc),
        updated_at: DateTime::parse_from_rfc3339(row.get::<&str, _>("updated_at"))
            .map_err(|error| StorageError::InvalidData(error.to_string()))?
            .with_timezone(&Utc),
        promoted_at: row
            .get::<Option<&str>, _>("promoted_at")
            .map(DateTime::parse_from_rfc3339)
            .transpose()
            .map_err(|error| StorageError::InvalidData(error.to_string()))?
            .map(|value| value.with_timezone(&Utc)),
    })
}

fn decode_job_row(row: &sqlx::sqlite::SqliteRow) -> Result<Job> {
    let mut job: Job = decode(row.get::<&str, _>("payload"))?;
    let revision: i64 = row.get("revision");
    job.revision = u64::try_from(revision)
        .map_err(|_| StorageError::InvalidData("job revision is negative".into()))?;
    Ok(job)
}
