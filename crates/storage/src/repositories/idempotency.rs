use chrono::{DateTime, Utc};
use sqlx::{Row, SqlitePool};

use crate::{Result, StorageError};

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct IdempotentResponse {
    pub status: u16,
    pub body: Vec<u8>,
    pub content_type: String,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum IdempotencyClaim {
    Acquired,
    InProgress,
    Replay(IdempotentResponse),
}

#[derive(Clone, Debug)]
pub struct IdempotencyRepository {
    pool: SqlitePool,
}

impl IdempotencyRepository {
    #[must_use]
    pub fn new(pool: SqlitePool) -> Self {
        Self { pool }
    }

    pub async fn claim(
        &self,
        scope: &str,
        key: &str,
        request_hash: &str,
        now: DateTime<Utc>,
        expires_at: DateTime<Utc>,
    ) -> Result<IdempotencyClaim> {
        validate_key(scope, key, request_hash, now, expires_at)?;
        let mut tx = self.pool.begin().await?;
        // Expiry is enforced transactionally before every new non-secret claim. Delete globally,
        // not only for the incoming random key, so replay bodies cannot accumulate indefinitely.
        sqlx::query("DELETE FROM idempotency_keys WHERE expires_at <= ?")
            .bind(now.to_rfc3339())
            .execute(&mut *tx)
            .await?;

        let result = sqlx::query(
            "INSERT INTO idempotency_keys \
             (scope, key, request_hash, created_at, expires_at) VALUES (?, ?, ?, ?, ?) \
             ON CONFLICT(scope, key) DO NOTHING",
        )
        .bind(scope)
        .bind(key)
        .bind(request_hash)
        .bind(now.to_rfc3339())
        .bind(expires_at.to_rfc3339())
        .execute(&mut *tx)
        .await?;
        if result.rows_affected() == 1 {
            tx.commit().await?;
            return Ok(IdempotencyClaim::Acquired);
        }

        let row = sqlx::query(
            "SELECT request_hash, response_status, response_body, response_content_type \
             FROM idempotency_keys WHERE scope = ? AND key = ?",
        )
        .bind(scope)
        .bind(key)
        .fetch_one(&mut *tx)
        .await?;
        let stored_hash: String = row.get("request_hash");
        if stored_hash != request_hash {
            return Err(StorageError::IdempotencyMismatch);
        }
        let response_status: Option<i64> = row.get("response_status");
        let claim = match response_status {
            None => IdempotencyClaim::InProgress,
            Some(status) => {
                let status = u16::try_from(status).map_err(|_| {
                    StorageError::InvalidData("invalid idempotent response status".into())
                })?;
                IdempotencyClaim::Replay(IdempotentResponse {
                    status,
                    body: row
                        .get::<Option<Vec<u8>>, _>("response_body")
                        .unwrap_or_default(),
                    content_type: row
                        .get::<Option<String>, _>("response_content_type")
                        .unwrap_or_else(|| "application/octet-stream".to_owned()),
                })
            }
        };
        tx.commit().await?;
        Ok(claim)
    }

    pub async fn complete(
        &self,
        scope: &str,
        key: &str,
        request_hash: &str,
        response: &IdempotentResponse,
    ) -> Result<()> {
        let result = sqlx::query(
            "UPDATE idempotency_keys SET response_status = ?, response_body = ?, response_content_type = ? \
             WHERE scope = ? AND key = ? AND request_hash = ? AND response_status IS NULL",
        )
        .bind(i64::from(response.status))
        .bind(&response.body)
        .bind(&response.content_type)
        .bind(scope)
        .bind(key)
        .bind(request_hash)
        .execute(&self.pool)
        .await?;
        if result.rows_affected() == 0 {
            let row = sqlx::query(
                "SELECT request_hash FROM idempotency_keys WHERE scope = ? AND key = ?",
            )
            .bind(scope)
            .bind(key)
            .fetch_optional(&self.pool)
            .await?;
            return Err(match row {
                Some(row) if row.get::<String, _>("request_hash") != request_hash => {
                    StorageError::IdempotencyMismatch
                }
                Some(_) => StorageError::Conflict {
                    entity: "idempotency_key",
                    id: format!("{scope}:{key}"),
                },
                None => StorageError::NotFound {
                    entity: "idempotency_key",
                    id: format!("{scope}:{key}"),
                },
            });
        }
        Ok(())
    }

    pub async fn forget(&self, scope: &str, key: &str) -> Result<bool> {
        Ok(
            sqlx::query("DELETE FROM idempotency_keys WHERE scope = ? AND key = ?")
                .bind(scope)
                .bind(key)
                .execute(&self.pool)
                .await?
                .rows_affected()
                > 0,
        )
    }

    pub async fn cleanup_expired(&self, now: DateTime<Utc>) -> Result<u64> {
        Ok(
            sqlx::query("DELETE FROM idempotency_keys WHERE expires_at <= ?")
                .bind(now.to_rfc3339())
                .execute(&self.pool)
                .await?
                .rows_affected(),
        )
    }
}

fn validate_key(
    scope: &str,
    key: &str,
    request_hash: &str,
    now: DateTime<Utc>,
    expires_at: DateTime<Utc>,
) -> Result<()> {
    if scope.trim().is_empty() || key.trim().is_empty() || request_hash.trim().is_empty() {
        return Err(StorageError::InvalidData(
            "idempotency scope, key, and request hash must not be empty".into(),
        ));
    }
    if expires_at <= now {
        return Err(StorageError::InvalidData(
            "idempotency expiry must be in the future".into(),
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use chrono::Duration;

    use super::*;

    #[tokio::test]
    async fn every_claim_purges_all_expired_replay_records() {
        let directory = tempfile::tempdir().expect("temporary directory");
        let database = crate::Database::open_in(directory.path())
            .await
            .expect("database");
        let repository = database.repositories().idempotency;
        let now = Utc::now();

        sqlx::query(
            "INSERT INTO idempotency_keys \
             (scope, key, request_hash, response_status, response_body, response_content_type, created_at, expires_at) \
             VALUES (?, ?, ?, ?, ?, ?, ?, ?)",
        )
        .bind("POST /api/v1/expired-fixture")
        .bind("expired-key")
        .bind("expired-hash")
        .bind(200_i64)
        .bind(b"expired replay body".as_slice())
        .bind("application/json")
        .bind((now - Duration::hours(2)).to_rfc3339())
        .bind((now - Duration::hours(1)).to_rfc3339())
        .execute(database.pool())
        .await
        .expect("expired fixture");

        assert_eq!(
            repository
                .claim(
                    "POST /api/v1/current-fixture",
                    "current-key",
                    "current-hash",
                    now,
                    now + Duration::hours(24),
                )
                .await
                .expect("claim"),
            IdempotencyClaim::Acquired
        );

        let expired = sqlx::query_scalar::<_, i64>(
            "SELECT COUNT(*) FROM idempotency_keys WHERE key = 'expired-key'",
        )
        .fetch_one(database.pool())
        .await
        .expect("expired count");
        let current = sqlx::query_scalar::<_, i64>(
            "SELECT COUNT(*) FROM idempotency_keys WHERE key = 'current-key'",
        )
        .fetch_one(database.pool())
        .await
        .expect("current count");
        assert_eq!(expired, 0);
        assert_eq!(current, 1);
    }
}
