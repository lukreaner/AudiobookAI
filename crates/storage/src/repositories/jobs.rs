use audiobookai_core::{Job, JobAttempt, JobId, JobState, JobUnit, JobUnitId, Validate};
use chrono::{DateTime, Utc};
use sqlx::{Row, SqlitePool};

use crate::{Result, StorageError};

use super::util::{decode, encode, enum_text};

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
             (id, job_id, chapter_id, segment_id, provider_id, kind, state, next_attempt_at, updated_at, payload) \
             VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?) \
             ON CONFLICT(id) DO UPDATE SET state = excluded.state, next_attempt_at = excluded.next_attempt_at, \
             updated_at = excluded.updated_at, payload = excluded.payload",
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

fn decode_job_row(row: &sqlx::sqlite::SqliteRow) -> Result<Job> {
    let mut job: Job = decode(row.get::<&str, _>("payload"))?;
    let revision: i64 = row.get("revision");
    job.revision = u64::try_from(revision)
        .map_err(|_| StorageError::InvalidData("job revision is negative".into()))?;
    Ok(job)
}
