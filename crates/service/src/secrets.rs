use std::{fmt, sync::Arc};

use aes_gcm::{
    Aes256Gcm, KeyInit,
    aead::{Aead, Payload},
};
use audiobookai_core::{
    EncryptedSecretEnvelope, MasterKeySource, SecretId, SecretKind, SecretReference,
};
use chrono::Utc;
use rand::RngCore;
use sqlx::Row;
use tokio::sync::RwLock;
use zeroize::Zeroizing;

use crate::ServiceError;

const PASSPHRASE_CHECK_KEY: &str = "secret-vault-passphrase-check-v1";
const PASSPHRASE_CHECK_AAD: &[u8] = b"AudiobookAI secret vault passphrase check v1";
const PASSPHRASE_CHECK_PLAINTEXT: &[u8] = b"AudiobookAI passphrase verified";

#[derive(serde::Deserialize, serde::Serialize)]
struct PassphraseCheck {
    schema_version: u8,
    nonce: Vec<u8>,
    ciphertext: Vec<u8>,
}

struct MasterKey(Zeroizing<[u8; 32]>);

impl fmt::Debug for MasterKey {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("MasterKey([REDACTED])")
    }
}

#[derive(Clone, Debug)]
pub struct SecretVault {
    database: audiobookai_storage::Database,
    key: Arc<RwLock<Option<Arc<MasterKey>>>>,
    source: Arc<RwLock<Option<MasterKeySource>>>,
}

impl SecretVault {
    // Production initialization awaits the blocking keychain lookup; tests deliberately skip it.
    #[allow(clippy::unused_async)]
    pub async fn initialize(database: audiobookai_storage::Database) -> Self {
        #[cfg(test)]
        let key = None;
        #[cfg(not(test))]
        let key = tokio::task::spawn_blocking(load_or_create_keychain_key)
            .await
            .ok()
            .and_then(Result::ok)
            .map(Arc::new);
        let source = key.as_ref().map(|_| MasterKeySource::OsKeychain);
        Self {
            database,
            key: Arc::new(RwLock::new(key)),
            source: Arc::new(RwLock::new(source)),
        }
    }

    pub async fn is_unlocked(&self) -> bool {
        self.key.read().await.is_some()
    }

    pub async fn key_source(&self) -> Option<MasterKeySource> {
        *self.source.read().await
    }

    pub async fn unlock_with_passphrase(&self, passphrase: &str) -> Result<(), ServiceError> {
        if passphrase.chars().count() < 12 {
            return Err(ServiceError::InvalidRequest(
                "secret-store passphrase must contain at least 12 characters".to_owned(),
            ));
        }
        let salt_path = self.database.paths().root.join("secret-store.salt");
        let salt = match tokio::fs::read(&salt_path).await {
            Ok(value) if value.len() == 16 => value,
            Ok(_) => {
                return Err(ServiceError::Internal(
                    "secret-store salt has an invalid length".to_owned(),
                ));
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                let mut value = vec![0_u8; 16];
                rand::rng().fill_bytes(&mut value);
                tokio::fs::write(&salt_path, &value).await?;
                value
            }
            Err(error) => return Err(ServiceError::Io(error)),
        };
        audiobookai_storage::harden_private_file(&salt_path)
            .await
            .map_err(|error| ServiceError::Storage(error.to_string()))?;
        let mut output = [0_u8; 32];
        argon2::Argon2::default()
            .hash_password_into(passphrase.as_bytes(), &salt, &mut output)
            .map_err(|_| ServiceError::Internal("Argon2id key derivation failed".to_owned()))?;
        let key = Arc::new(MasterKey(Zeroizing::new(output)));
        self.validate_or_create_passphrase_check(&key).await?;
        *self.key.write().await = Some(key);
        *self.source.write().await = Some(MasterKeySource::Argon2idPassphrase);
        Ok(())
    }

    async fn validate_or_create_passphrase_check(
        &self,
        key: &MasterKey,
    ) -> Result<(), ServiceError> {
        let stored = sqlx::query_scalar::<_, String>(
            "SELECT payload FROM application_settings WHERE key = ?",
        )
        .bind(PASSPHRASE_CHECK_KEY)
        .fetch_optional(self.database.pool())
        .await
        .map_err(|error| ServiceError::Storage(error.to_string()))?;
        let cipher = Aes256Gcm::new_from_slice(key.0.as_ref())
            .map_err(|_| ServiceError::Internal("invalid master key".to_owned()))?;

        if let Some(payload) = stored {
            let check: PassphraseCheck = serde_json::from_str(&payload).map_err(|_| {
                ServiceError::Conflict(
                    "passphrase verification data is invalid or has been modified".to_owned(),
                )
            })?;
            if check.schema_version != 1 || check.nonce.len() != 12 {
                return Err(ServiceError::Conflict(
                    "passphrase verification data is invalid or has been modified".to_owned(),
                ));
            }
            let plaintext = cipher
                .decrypt(
                    aes_gcm::Nonce::from_slice(&check.nonce),
                    Payload {
                        msg: &check.ciphertext,
                        aad: PASSPHRASE_CHECK_AAD,
                    },
                )
                .map_err(|_| {
                    ServiceError::Conflict(
                        "passphrase is incorrect or verification data was modified".to_owned(),
                    )
                })?;
            if plaintext != PASSPHRASE_CHECK_PLAINTEXT {
                return Err(ServiceError::Conflict(
                    "passphrase is incorrect or verification data was modified".to_owned(),
                ));
            }
            return Ok(());
        }

        // Older passphrase stores may predate the explicit check record. Confirm the
        // candidate key against an existing authenticated secret before migrating.
        if let Some(row) =
            sqlx::query("SELECT nonce, ciphertext, associated_data FROM encrypted_secrets LIMIT 1")
                .fetch_optional(self.database.pool())
                .await
                .map_err(|error| ServiceError::Storage(error.to_string()))?
        {
            let nonce = row.get::<Vec<u8>, _>("nonce");
            if nonce.len() != 12 {
                return Err(ServiceError::Conflict(
                    "existing secret data has an invalid nonce".to_owned(),
                ));
            }
            let existing_plaintext = cipher
                .decrypt(
                    aes_gcm::Nonce::from_slice(&nonce),
                    Payload {
                        msg: &row.get::<Vec<u8>, _>("ciphertext"),
                        aad: &row.get::<Vec<u8>, _>("associated_data"),
                    },
                )
                .map_err(|_| {
                    ServiceError::Conflict(
                        "passphrase cannot unlock the existing secret store".to_owned(),
                    )
                })?;
            drop(Zeroizing::new(existing_plaintext));
        }

        let mut nonce = [0_u8; 12];
        rand::rng().fill_bytes(&mut nonce);
        let ciphertext = cipher
            .encrypt(
                aes_gcm::Nonce::from_slice(&nonce),
                Payload {
                    msg: PASSPHRASE_CHECK_PLAINTEXT,
                    aad: PASSPHRASE_CHECK_AAD,
                },
            )
            .map_err(|_| ServiceError::Internal("passphrase check encryption failed".to_owned()))?;
        let payload = serde_json::to_string(&PassphraseCheck {
            schema_version: 1,
            nonce: nonce.to_vec(),
            ciphertext,
        })
        .map_err(|error| ServiceError::Internal(error.to_string()))?;
        sqlx::query(
            "INSERT INTO application_settings (key, updated_at, payload) VALUES (?, ?, ?) \
             ON CONFLICT(key) DO UPDATE SET updated_at = excluded.updated_at, payload = excluded.payload",
        )
        .bind(PASSPHRASE_CHECK_KEY)
        .bind(Utc::now().to_rfc3339())
        .bind(payload)
        .execute(self.database.pool())
        .await
        .map_err(|error| ServiceError::Storage(error.to_string()))?;
        Ok(())
    }

    pub async fn lock(&self) {
        *self.key.write().await = None;
        *self.source.write().await = None;
    }

    pub async fn store(
        &self,
        kind: SecretKind,
        label: String,
        plaintext: &[u8],
    ) -> Result<SecretReference, ServiceError> {
        let key = self
            .key
            .read()
            .await
            .clone()
            .ok_or_else(|| ServiceError::Conflict("secret store is locked".to_owned()))?;
        if plaintext.is_empty() {
            return Err(ServiceError::InvalidRequest(
                "secret value must not be empty".to_owned(),
            ));
        }
        let source = self
            .source
            .read()
            .await
            .ok_or_else(|| ServiceError::Conflict("secret store is locked".to_owned()))?;
        let id = SecretId::new();
        let now = Utc::now();
        let reference = SecretReference {
            id,
            kind,
            label,
            created_at: now,
            updated_at: now,
            last_used_at: None,
        };
        let associated_data = serde_json::to_vec(&reference)
            .map_err(|error| ServiceError::Internal(error.to_string()))?;
        let mut nonce = [0_u8; 12];
        rand::rng().fill_bytes(&mut nonce);
        let cipher = Aes256Gcm::new_from_slice(key.0.as_ref())
            .map_err(|_| ServiceError::Internal("invalid master key".to_owned()))?;
        let ciphertext = cipher
            .encrypt(
                aes_gcm::Nonce::from_slice(&nonce),
                Payload {
                    msg: plaintext,
                    aad: &associated_data,
                },
            )
            .map_err(|_| ServiceError::Internal("secret encryption failed".to_owned()))?;
        let envelope = EncryptedSecretEnvelope {
            schema_version: 1,
            secret_id: id,
            algorithm: "AES-256-GCM".to_owned(),
            key_source: source,
            nonce: nonce.to_vec(),
            ciphertext,
            associated_data,
            created_at: now,
        };

        let mut transaction = self
            .database
            .pool()
            .begin()
            .await
            .map_err(|error| ServiceError::Storage(error.to_string()))?;
        sqlx::query(
            "INSERT INTO secret_references (id, kind, label, created_at, updated_at, last_used_at, payload) \
             VALUES (?, ?, ?, ?, ?, NULL, ?)",
        )
        .bind(id.to_string())
        .bind(secret_kind_name(kind))
        .bind(&reference.label)
        .bind(now.to_rfc3339())
        .bind(now.to_rfc3339())
        .bind(serde_json::to_string(&reference).map_err(|error| ServiceError::Internal(error.to_string()))?)
        .execute(&mut *transaction)
        .await
        .map_err(|error| ServiceError::Storage(error.to_string()))?;
        sqlx::query(
            "INSERT INTO encrypted_secrets \
             (secret_id, schema_version, algorithm, key_source, nonce, ciphertext, associated_data, created_at) \
             VALUES (?, ?, ?, ?, ?, ?, ?, ?)",
        )
        .bind(id.to_string())
        .bind(i64::from(envelope.schema_version))
        .bind(&envelope.algorithm)
        .bind(master_key_source_name(source))
        .bind(&envelope.nonce)
        .bind(&envelope.ciphertext)
        .bind(&envelope.associated_data)
        .bind(now.to_rfc3339())
        .execute(&mut *transaction)
        .await
        .map_err(|error| ServiceError::Storage(error.to_string()))?;
        transaction
            .commit()
            .await
            .map_err(|error| ServiceError::Storage(error.to_string()))?;
        Ok(reference)
    }

    pub async fn expose(&self, id: SecretId) -> Result<Zeroizing<Vec<u8>>, ServiceError> {
        let key = self
            .key
            .read()
            .await
            .clone()
            .ok_or_else(|| ServiceError::Conflict("secret store is locked".to_owned()))?;
        let row = sqlx::query(
            "SELECT r.payload, e.nonce, e.ciphertext, e.associated_data \
             FROM secret_references r JOIN encrypted_secrets e ON e.secret_id = r.id \
             WHERE r.id = ?",
        )
        .bind(id.to_string())
        .fetch_optional(self.database.pool())
        .await
        .map_err(|error| ServiceError::Storage(error.to_string()))?
        .ok_or(ServiceError::NotFound)?;
        let payload = row.get::<String, _>("payload");
        let reference: SecretReference = serde_json::from_str(&payload)
            .map_err(|error| ServiceError::Internal(error.to_string()))?;
        let expected_aad = serde_json::to_vec(&reference)
            .map_err(|error| ServiceError::Internal(error.to_string()))?;
        let stored_aad = row.get::<Vec<u8>, _>("associated_data");
        if expected_aad != stored_aad {
            return Err(ServiceError::Internal(
                "encrypted secret metadata failed authentication".to_owned(),
            ));
        }
        let nonce = row.get::<Vec<u8>, _>("nonce");
        if nonce.len() != 12 {
            return Err(ServiceError::Internal(
                "invalid encrypted secret nonce".to_owned(),
            ));
        }
        let cipher = Aes256Gcm::new_from_slice(key.0.as_ref())
            .map_err(|_| ServiceError::Internal("invalid master key".to_owned()))?;
        let plaintext = cipher
            .decrypt(
                aes_gcm::Nonce::from_slice(&nonce),
                Payload {
                    msg: &row.get::<Vec<u8>, _>("ciphertext"),
                    aad: &stored_aad,
                },
            )
            .map_err(|_| {
                ServiceError::Conflict(
                    "secret could not be decrypted with the active key".to_owned(),
                )
            })?;
        sqlx::query("UPDATE secret_references SET last_used_at = ? WHERE id = ?")
            .bind(Utc::now().to_rfc3339())
            .bind(id.to_string())
            .execute(self.database.pool())
            .await
            .map_err(|error| ServiceError::Storage(error.to_string()))?;
        Ok(Zeroizing::new(plaintext))
    }

    pub async fn delete(&self, id: SecretId) -> Result<(), ServiceError> {
        let result = sqlx::query("DELETE FROM secret_references WHERE id = ?")
            .bind(id.to_string())
            .execute(self.database.pool())
            .await
            .map_err(|error| ServiceError::Storage(error.to_string()))?;
        if result.rows_affected() == 0 {
            return Err(ServiceError::NotFound);
        }
        Ok(())
    }
}

#[cfg(not(test))]
fn load_or_create_keychain_key() -> Result<MasterKey, keyring::Error> {
    let entry = keyring::Entry::new("org.audiobookai.AudiobookAI", "master-key-v1")?;
    match entry.get_secret() {
        Ok(value) if value.len() == 32 => {
            let mut key = [0_u8; 32];
            key.copy_from_slice(&value);
            Ok(MasterKey(Zeroizing::new(key)))
        }
        Ok(_) | Err(keyring::Error::NoEntry) => {
            let mut key = [0_u8; 32];
            rand::rng().fill_bytes(&mut key);
            entry.set_secret(&key)?;
            Ok(MasterKey(Zeroizing::new(key)))
        }
        Err(error) => Err(error),
    }
}

fn secret_kind_name(kind: SecretKind) -> &'static str {
    match kind {
        SecretKind::ProviderCredential => "provider_credential",
        SecretKind::ProviderEnvironment => "provider_environment",
        SecretKind::LanPasswordVerifier => "lan_password_verifier",
        SecretKind::LanApiToken => "lan_api_token",
        SecretKind::TlsPrivateKey => "tls_private_key",
    }
}

fn master_key_source_name(source: MasterKeySource) -> &'static str {
    match source {
        MasterKeySource::OsKeychain => "os_keychain",
        MasterKeySource::Argon2idPassphrase => "argon2id_passphrase",
    }
}

#[cfg(test)]
mod tests {
    use audiobookai_core::SecretKind;

    use super::{PASSPHRASE_CHECK_KEY, SecretVault};
    use crate::ServiceError;

    async fn vault() -> (tempfile::TempDir, SecretVault) {
        let directory = tempfile::tempdir().expect("temporary directory");
        let database = audiobookai_storage::Database::open_in(directory.path())
            .await
            .expect("database");
        let vault = SecretVault::initialize(database).await;
        (directory, vault)
    }

    #[tokio::test]
    async fn passphrase_check_rejects_a_wrong_key_before_secret_access() {
        let (_directory, vault) = vault().await;
        vault
            .unlock_with_passphrase("correct horse battery staple")
            .await
            .expect("initial unlock");
        let reference = vault
            .store(
                SecretKind::ProviderCredential,
                "provider".to_owned(),
                b"sensitive value",
            )
            .await
            .expect("stored secret");
        vault.lock().await;

        let error = vault
            .unlock_with_passphrase("this is the wrong passphrase")
            .await
            .expect_err("wrong passphrase must fail");
        assert!(matches!(error, ServiceError::Conflict(_)));
        assert!(!vault.is_unlocked().await);

        vault
            .unlock_with_passphrase("correct horse battery staple")
            .await
            .expect("correct passphrase");
        assert_eq!(
            vault
                .expose(reference.id)
                .await
                .expect("decrypted")
                .as_slice(),
            b"sensitive value"
        );
    }

    #[tokio::test]
    async fn passphrase_check_detects_database_tampering() {
        let (_directory, vault) = vault().await;
        vault
            .unlock_with_passphrase("correct horse battery staple")
            .await
            .expect("initial unlock");
        vault.lock().await;
        sqlx::query("UPDATE application_settings SET payload = ? WHERE key = ?")
            .bind(r#"{"schema_version":1,"nonce":[0],"ciphertext":[]}"#)
            .bind(PASSPHRASE_CHECK_KEY)
            .execute(vault.database.pool())
            .await
            .expect("tamper verification record");

        let error = vault
            .unlock_with_passphrase("correct horse battery staple")
            .await
            .expect_err("tampered check must fail");
        assert!(matches!(error, ServiceError::Conflict(_)));
        assert!(!vault.is_unlocked().await);
    }
}
