use audiobookai_core::{CapabilitySnapshot, ProviderProfile, ProviderProfileId, Validate};
use sqlx::{Row, SqlitePool};

use crate::{Result, StorageError};

use super::util::{decode, encode, enum_text};

#[derive(Clone, Debug)]
pub struct ProviderRepository {
    pool: SqlitePool,
}

impl ProviderRepository {
    #[must_use]
    pub fn new(pool: SqlitePool) -> Self {
        Self { pool }
    }

    pub async fn upsert(&self, profile: &ProviderProfile) -> Result<()> {
        profile.validate()?;
        sqlx::query(
            "INSERT INTO providers \
             (id, name, family, role, deployment, enabled, updated_at, payload) \
             VALUES (?, ?, ?, ?, ?, ?, ?, ?) \
             ON CONFLICT(id) DO UPDATE SET name = excluded.name, family = excluded.family, \
             role = excluded.role, deployment = excluded.deployment, enabled = excluded.enabled, \
             updated_at = excluded.updated_at, payload = excluded.payload",
        )
        .bind(profile.id.to_string())
        .bind(&profile.name)
        .bind(enum_text(&profile.family)?)
        .bind(enum_text(&profile.role)?)
        .bind(enum_text(&profile.deployment)?)
        .bind(profile.enabled)
        .bind(profile.updated_at.to_rfc3339())
        .bind(encode(profile)?)
        .execute(&self.pool)
        .await?;
        if let Some(snapshot) = &profile.capability_snapshot {
            self.save_capability_snapshot(snapshot).await?;
        }
        Ok(())
    }

    pub async fn get(&self, id: ProviderProfileId) -> Result<Option<ProviderProfile>> {
        let payload = sqlx::query_scalar::<_, String>("SELECT payload FROM providers WHERE id = ?")
            .bind(id.to_string())
            .fetch_optional(&self.pool)
            .await?;
        payload.as_deref().map(decode).transpose()
    }

    pub async fn list(&self, enabled_only: bool) -> Result<Vec<ProviderProfile>> {
        let rows = if enabled_only {
            sqlx::query("SELECT payload FROM providers WHERE enabled = 1 ORDER BY name")
                .fetch_all(&self.pool)
                .await?
        } else {
            sqlx::query("SELECT payload FROM providers ORDER BY name")
                .fetch_all(&self.pool)
                .await?
        };
        rows.into_iter()
            .map(|row| decode(row.get::<&str, _>("payload")))
            .collect()
    }

    pub async fn delete(&self, id: ProviderProfileId) -> Result<()> {
        let result = sqlx::query("DELETE FROM providers WHERE id = ?")
            .bind(id.to_string())
            .execute(&self.pool)
            .await?;
        if result.rows_affected() == 0 {
            return Err(StorageError::NotFound {
                entity: "provider",
                id: id.to_string(),
            });
        }
        Ok(())
    }

    pub async fn save_capability_snapshot(&self, snapshot: &CapabilitySnapshot) -> Result<()> {
        sqlx::query(
            "INSERT INTO capability_snapshots \
             (id, provider_id, model, endpoint_fingerprint, observed_at, expires_at, payload) \
             VALUES (?, ?, ?, ?, ?, ?, ?) \
             ON CONFLICT(id) DO UPDATE SET model = excluded.model, \
             endpoint_fingerprint = excluded.endpoint_fingerprint, observed_at = excluded.observed_at, \
             expires_at = excluded.expires_at, payload = excluded.payload",
        )
        .bind(snapshot.id.to_string())
        .bind(snapshot.provider_profile_id.to_string())
        .bind(&snapshot.model)
        .bind(&snapshot.endpoint_fingerprint)
        .bind(snapshot.observed_at.to_rfc3339())
        .bind(snapshot.expires_at.map(|value| value.to_rfc3339()))
        .bind(encode(snapshot)?)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    pub async fn latest_capability_snapshot(
        &self,
        provider_id: ProviderProfileId,
    ) -> Result<Option<CapabilitySnapshot>> {
        let row = sqlx::query(
            "SELECT payload FROM capability_snapshots \
             WHERE provider_id = ? ORDER BY observed_at DESC LIMIT 1",
        )
        .bind(provider_id.to_string())
        .fetch_optional(&self.pool)
        .await?;
        row.map(|row| decode(row.get::<&str, _>("payload")))
            .transpose()
    }
}
