use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::{SecretId, SessionId};

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum SecretKind {
    ProviderCredential,
    ProviderEnvironment,
    LanPasswordVerifier,
    LanApiToken,
    TlsPrivateKey,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct SecretReference {
    pub id: SecretId,
    pub kind: SecretKind,
    pub label: String,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
    pub last_used_at: Option<DateTime<Utc>>,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum MasterKeySource {
    OsKeychain,
    Argon2idPassphrase,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct EncryptedSecretEnvelope {
    pub schema_version: u32,
    pub secret_id: SecretId,
    pub algorithm: String,
    pub key_source: MasterKeySource,
    pub nonce: Vec<u8>,
    pub ciphertext: Vec<u8>,
    pub associated_data: Vec<u8>,
    pub created_at: DateTime<Utc>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct LanSettings {
    pub enabled: bool,
    pub bind_address: String,
    pub port: u16,
    pub tls_mode: TlsMode,
    pub insecure_http_confirmed: bool,
    pub certificate_path: Option<String>,
}

impl Default for LanSettings {
    fn default() -> Self {
        Self {
            enabled: false,
            bind_address: "127.0.0.1".to_owned(),
            port: 8787,
            tls_mode: TlsMode::Disabled,
            insecure_http_confirmed: false,
            certificate_path: None,
        }
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum TlsMode {
    Disabled,
    Generated,
    UserSupplied,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum SessionKind {
    Desktop,
    LanBrowser,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct AuthSession {
    pub id: SessionId,
    pub kind: SessionKind,
    pub token_hash: Vec<u8>,
    pub csrf_hash: Option<Vec<u8>>,
    pub created_at: DateTime<Utc>,
    pub expires_at: DateTime<Utc>,
    pub last_seen_at: DateTime<Utc>,
    pub revoked_at: Option<DateTime<Utc>>,
    pub peer_address: Option<String>,
}

impl AuthSession {
    #[must_use]
    pub fn is_active_at(&self, now: DateTime<Utc>) -> bool {
        self.revoked_at.is_none() && self.expires_at > now
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct ApiTokenRecord {
    pub id: String,
    pub label: String,
    pub token_hash: Vec<u8>,
    pub created_at: DateTime<Utc>,
    pub last_used_at: Option<DateTime<Utc>>,
    pub expires_at: Option<DateTime<Utc>>,
    pub revoked_at: Option<DateTime<Utc>>,
}
