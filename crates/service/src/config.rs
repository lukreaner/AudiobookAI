use std::{
    net::{IpAddr, Ipv4Addr, SocketAddr},
    path::{Path, PathBuf},
    str::FromStr as _,
};

use directories::ProjectDirs;
use serde::{Deserialize, Serialize};
use sqlx::sqlite::{SqliteConnectOptions, SqlitePoolOptions};

use crate::ServiceError;

/// PEM identity used by the Rustls listener.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct TlsConfig {
    /// PEM certificate followed by any intermediate certificates.
    pub certificate_chain_path: PathBuf,
    /// PKCS#8, PKCS#1, or SEC1 PEM private key.
    pub private_key_path: PathBuf,
}

/// Runtime configuration for the embedded desktop service and authenticated LAN listener.
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct ServiceConfig {
    pub bind: SocketAddr,
    pub data_dir: PathBuf,
    /// Installed `sidecars/bin` directory containing packaged FFmpeg/ffprobe
    /// and, on Linux, eSpeak NG. When present, media resolution must not fall
    /// back to the application data directory or `PATH`.
    #[serde(default)]
    pub bundled_sidecar_dir: Option<PathBuf>,
    /// TLS is used whenever an identity is present, including on loopback.
    #[serde(default)]
    pub tls: Option<TlsConfig>,
    /// DNS names or IP literals accepted in LAN Host/Origin authorities.
    ///
    /// A concrete non-loopback bind address is accepted automatically. A
    /// wildcard bind requires at least one explicit hostname or IP here.
    #[serde(default)]
    pub lan_hostnames: Vec<String>,
    /// Permit a non-loopback plaintext listener after a separate, explicit
    /// confirmation. This never disables TLS when `tls` is configured.
    pub allow_insecure_lan: bool,
    pub desktop_bootstrap: bool,
}

impl ServiceConfig {
    /// Safe local default. Port zero lets the OS select an unused loopback port.
    ///
    /// # Errors
    ///
    /// Returns [`ServiceError::DataDirectoryUnavailable`] when the operating
    /// system does not expose an application data directory.
    pub fn desktop_default() -> Result<Self, ServiceError> {
        let dirs = ProjectDirs::from("org", "AudiobookAI", "AudiobookAI")
            .ok_or(ServiceError::DataDirectoryUnavailable)?;
        Ok(Self {
            bind: SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 0),
            data_dir: dirs.data_local_dir().to_path_buf(),
            bundled_sidecar_dir: None,
            tls: None,
            lan_hostnames: Vec::new(),
            allow_insecure_lan: false,
            desktop_bootstrap: true,
        })
    }

    /// Validates network and TLS invariants before the listener starts.
    ///
    /// # Errors
    ///
    /// Returns an error for unsafe listener, hostname, or TLS configurations.
    pub fn validate(&self) -> Result<(), ServiceError> {
        if self.bind.ip().is_multicast() {
            return Err(ServiceError::InvalidRequest(
                "the listener bind address must not be multicast".to_owned(),
            ));
        }
        if let Some(tls) = &self.tls {
            if tls.certificate_chain_path.as_os_str().is_empty()
                || tls.private_key_path.as_os_str().is_empty()
            {
                return Err(ServiceError::InvalidRequest(
                    "TLS certificate and private-key paths must not be empty".to_owned(),
                ));
            }
            if !tls.certificate_chain_path.is_absolute() || !tls.private_key_path.is_absolute() {
                return Err(ServiceError::InvalidRequest(
                    "TLS certificate and private-key paths must be absolute".to_owned(),
                ));
            }
            if tls.certificate_chain_path == tls.private_key_path {
                return Err(ServiceError::InvalidRequest(
                    "TLS certificate and private key must use separate PEM files".to_owned(),
                ));
            }
        }

        if !self.bind.ip().is_loopback() && self.tls.is_none() && !self.allow_insecure_lan {
            return Err(ServiceError::TlsRequiredForLan(self.bind));
        }
        if !self.bind.ip().is_loopback()
            && self.bind.ip().is_unspecified()
            && self.lan_hostnames.is_empty()
        {
            return Err(ServiceError::InvalidRequest(
                "a wildcard LAN bind requires at least one explicit LAN hostname or IP".to_owned(),
            ));
        }
        if self
            .lan_hostnames
            .iter()
            .any(|hostname| !valid_hostname(hostname))
        {
            return Err(ServiceError::InvalidRequest(
                "LAN hostnames must be DNS names or IP literals without a scheme, path, or port"
                    .to_owned(),
            ));
        }
        Ok(())
    }

    /// Applies a previously validated owner LAN configuration from the desktop
    /// database. The caller is expected to fall back to `desktop_default` when
    /// this returns an error, keeping the service loopback-only and recoverable.
    ///
    /// # Errors
    ///
    /// Returns an error when the settings database cannot be read or contains
    /// invalid LAN configuration.
    pub async fn with_persisted_desktop_lan_settings(mut self) -> Result<Self, ServiceError> {
        let database_path = self.data_dir.join("audiobookai.sqlite3");
        match tokio::fs::metadata(&database_path).await {
            Ok(metadata) if metadata.is_file() => {}
            Ok(_) => {
                return Err(ServiceError::InvalidRequest(
                    "the persisted settings database path is not a file".to_owned(),
                ));
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(self),
            Err(error) => return Err(error.into()),
        }

        let options = SqliteConnectOptions::new()
            .filename(&database_path)
            .read_only(true)
            .foreign_keys(true);
        let pool = SqlitePoolOptions::new()
            .max_connections(1)
            .connect_with(options)
            .await
            .map_err(|error| ServiceError::Storage(error.to_string()))?;
        let payload = sqlx::query_scalar::<_, String>(
            "SELECT payload FROM application_settings WHERE key = 'owner'",
        )
        .fetch_optional(&pool)
        .await
        .map_err(|error| ServiceError::Storage(error.to_string()))?;
        let authentication_configured = if payload.is_some() {
            let password = sqlx::query_scalar::<_, i64>(
                "SELECT EXISTS(SELECT 1 FROM application_settings WHERE key = 'lan_password_secret')",
            )
            .fetch_one(&pool)
            .await
            .map_err(|error| ServiceError::Storage(error.to_string()))?
                != 0;
            let token = sqlx::query_scalar::<_, i64>(
                "SELECT EXISTS(SELECT 1 FROM api_tokens WHERE revoked_at IS NULL \
                 AND (expires_at IS NULL OR expires_at > ?))",
            )
            .bind(chrono::Utc::now().to_rfc3339())
            .fetch_one(&pool)
            .await
            .map_err(|error| ServiceError::Storage(error.to_string()))?
                != 0;
            password || token
        } else {
            false
        };
        pool.close().await;

        let Some(payload) = payload else {
            return Ok(self);
        };
        let settings: crate::models::AppSettingsView =
            serde_json::from_str(&payload).map_err(|error| {
                ServiceError::InvalidRequest(format!("persisted settings are invalid: {error}"))
            })?;
        if settings.lan.enabled {
            apply_lan_settings(&mut self, &settings.lan, authentication_configured)?;
        }
        Ok(self)
    }

    pub const fn uses_tls(&self) -> bool {
        self.tls.is_some()
    }

    pub const fn scheme(&self) -> &'static str {
        if self.uses_tls() { "https" } else { "http" }
    }
}

pub(crate) fn apply_lan_settings(
    config: &mut ServiceConfig,
    settings: &crate::models::LanSettingsView,
    authentication_configured: bool,
) -> Result<(), ServiceError> {
    let bind_address = IpAddr::from_str(settings.bind_address.trim()).map_err(|_| {
        ServiceError::InvalidRequest("LAN bind address must be an IP address".to_owned())
    })?;
    if bind_address.is_multicast() {
        return Err(ServiceError::InvalidRequest(
            "LAN bind address must not be multicast".to_owned(),
        ));
    }
    if !authentication_configured {
        return Err(ServiceError::InvalidRequest(
            "configure a LAN password or API token before enabling LAN access".to_owned(),
        ));
    }

    config.bind = SocketAddr::new(bind_address, settings.port);
    config.lan_hostnames.clone_from(&settings.advertised_hosts);
    config.allow_insecure_lan = !settings.tls && settings.insecure_http_confirmed;
    config.tls = if settings.tls {
        let certificate_chain_path = PathBuf::from(settings.certificate_chain_path.trim());
        let private_key_path = PathBuf::from(settings.private_key_path.trim());
        validate_tls_files(&certificate_chain_path, &private_key_path)?;
        Some(TlsConfig {
            certificate_chain_path,
            private_key_path,
        })
    } else {
        None
    };
    config.validate()
}

pub(crate) fn lan_restart_required(
    settings: &crate::models::LanSettingsView,
    config: &ServiceConfig,
) -> bool {
    if !settings.enabled {
        return !config.bind.ip().is_loopback();
    }
    let Ok(bind_address) = IpAddr::from_str(settings.bind_address.trim()) else {
        return true;
    };
    if config.bind != SocketAddr::new(bind_address, settings.port)
        || config.uses_tls() != settings.tls
        || config.lan_hostnames != settings.advertised_hosts
        || config.allow_insecure_lan != (!settings.tls && settings.insecure_http_confirmed)
    {
        return true;
    }
    match (&config.tls, settings.tls) {
        (Some(tls), true) => {
            tls.certificate_chain_path.as_path()
                != Path::new(settings.certificate_chain_path.trim())
                || tls.private_key_path.as_path() != Path::new(settings.private_key_path.trim())
        }
        (None, false) => false,
        _ => true,
    }
}

fn validate_tls_files(
    certificate: &std::path::Path,
    private_key: &std::path::Path,
) -> Result<(), ServiceError> {
    if certificate.as_os_str().is_empty() || private_key.as_os_str().is_empty() {
        return Err(ServiceError::InvalidRequest(
            "TLS certificate and private-key paths are required".to_owned(),
        ));
    }
    if !certificate.is_absolute() || !private_key.is_absolute() {
        return Err(ServiceError::InvalidRequest(
            "TLS certificate and private-key paths must be absolute".to_owned(),
        ));
    }
    let certificate = std::fs::canonicalize(certificate).map_err(|_| {
        ServiceError::InvalidRequest(
            "TLS certificate path must identify a readable file".to_owned(),
        )
    })?;
    let private_key = std::fs::canonicalize(private_key).map_err(|_| {
        ServiceError::InvalidRequest(
            "TLS private-key path must identify a readable file".to_owned(),
        )
    })?;
    if !certificate.is_file() || !private_key.is_file() {
        return Err(ServiceError::InvalidRequest(
            "TLS certificate and private-key paths must identify files".to_owned(),
        ));
    }
    if certificate == private_key {
        return Err(ServiceError::InvalidRequest(
            "TLS certificate and private key must use separate PEM files".to_owned(),
        ));
    }
    Ok(())
}

fn valid_hostname(value: &str) -> bool {
    if value.is_empty() || value.trim() != value {
        return false;
    }
    if let Ok(address) = IpAddr::from_str(value) {
        return !address.is_unspecified();
    }
    http::uri::Authority::from_str(value).is_ok_and(|authority| {
        authority.port_u16().is_none()
            && !authority.host().is_empty()
            && !authority.host().contains('*')
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_config(bind: &str) -> ServiceConfig {
        ServiceConfig {
            bind: bind.parse().expect("address"),
            data_dir: PathBuf::from("test-data"),
            bundled_sidecar_dir: None,
            tls: None,
            lan_hostnames: Vec::new(),
            allow_insecure_lan: false,
            desktop_bootstrap: false,
        }
    }

    #[test]
    fn wildcard_lan_requires_an_explicit_hostname() {
        let mut config = test_config("0.0.0.0:8787");
        config.allow_insecure_lan = true;
        assert!(matches!(
            config.validate(),
            Err(ServiceError::InvalidRequest(_))
        ));
        config.lan_hostnames.push("reader.home.arpa".to_owned());
        config.validate().expect("explicit hostname is safe");
    }

    #[test]
    fn rejects_authorities_in_the_hostname_list() {
        let mut config = test_config("192.0.2.10:8787");
        config.allow_insecure_lan = true;
        config
            .lan_hostnames
            .push("reader.home.arpa:8787".to_owned());
        assert!(matches!(
            config.validate(),
            Err(ServiceError::InvalidRequest(_))
        ));
        config.lan_hostnames = vec!["0.0.0.0".to_owned()];
        assert!(matches!(
            config.validate(),
            Err(ServiceError::InvalidRequest(_))
        ));
    }

    #[test]
    fn tls_identity_paths_must_be_absolute_and_distinct() {
        let mut config = test_config("127.0.0.1:8787");
        config.tls = Some(TlsConfig {
            certificate_chain_path: PathBuf::from("certificate.pem"),
            private_key_path: PathBuf::from("private-key.pem"),
        });
        assert!(matches!(
            config.validate(),
            Err(ServiceError::InvalidRequest(_))
        ));
    }

    #[test]
    fn lan_settings_require_authentication_before_exposure() {
        let mut config = test_config("127.0.0.1:0");
        let mut lan = crate::models::AppSettingsView::defaults(std::path::Path::new("test")).lan;
        lan.enabled = true;
        lan.bind_address = "192.0.2.10".to_owned();
        lan.tls = false;
        lan.insecure_http_confirmed = true;
        assert!(matches!(
            apply_lan_settings(&mut config, &lan, false),
            Err(ServiceError::InvalidRequest(_))
        ));
        assert_eq!(config.bind, "127.0.0.1:0".parse().expect("address"));
    }

    #[test]
    fn explicitly_confirmed_authenticated_http_lan_can_be_applied() {
        let mut config = test_config("127.0.0.1:0");
        let mut lan = crate::models::AppSettingsView::defaults(std::path::Path::new("test")).lan;
        lan.enabled = true;
        lan.bind_address = "0.0.0.0".to_owned();
        lan.port = 8787;
        lan.tls = false;
        lan.insecure_http_confirmed = true;
        lan.advertised_hosts = vec!["reader.home.arpa".to_owned()];
        apply_lan_settings(&mut config, &lan, true).expect("explicit configuration applies");
        assert_eq!(config.bind, "0.0.0.0:8787".parse().expect("address"));
        assert!(config.allow_insecure_lan);
        assert!(!lan_restart_required(&lan, &config));
    }

    #[tokio::test]
    async fn desktop_startup_reads_an_authenticated_persisted_lan_listener() {
        let directory = tempfile::tempdir().expect("temporary data directory");
        let database = audiobookai_storage::Database::open_in(directory.path())
            .await
            .expect("test database");
        let mut settings = crate::models::AppSettingsView::defaults(directory.path());
        settings.lan.enabled = true;
        settings.lan.bind_address = "0.0.0.0".to_owned();
        settings.lan.port = 8787;
        settings.lan.tls = false;
        settings.lan.insecure_http_confirmed = true;
        settings.lan.advertised_hosts = vec!["reader.home.arpa".to_owned()];
        sqlx::query(
            "INSERT INTO application_settings (key, updated_at, payload) VALUES ('owner', ?, ?)",
        )
        .bind(chrono::Utc::now().to_rfc3339())
        .bind(serde_json::to_string(&settings).expect("settings JSON"))
        .execute(database.pool())
        .await
        .expect("persist owner settings");
        sqlx::query(
            "INSERT INTO api_tokens \
             (id, label, token_hash, created_at, last_used_at, expires_at, revoked_at) \
             VALUES ('fixture-token', 'fixture', ?, ?, NULL, NULL, NULL)",
        )
        .bind(vec![0_u8; 32])
        .bind(chrono::Utc::now().to_rfc3339())
        .execute(database.pool())
        .await
        .expect("persist non-secret authentication fixture");
        database.close().await;

        let mut config = test_config("127.0.0.1:0");
        config.data_dir = directory.path().to_path_buf();
        let applied = config
            .with_persisted_desktop_lan_settings()
            .await
            .expect("persisted LAN settings apply");
        assert_eq!(applied.bind, "0.0.0.0:8787".parse().expect("address"));
        assert_eq!(applied.lan_hostnames, vec!["reader.home.arpa"]);
        assert!(applied.allow_insecure_lan);
    }
}
