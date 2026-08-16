//! Shared `AudiobookAI` `Axum` service and background runtime.

mod accounting;
mod api;
mod auth;
mod config;
mod conversion;
pub mod diagnostics;
mod distribution;
mod error;
mod events;
mod idempotency;
mod mlx_management;
mod models;
mod proofing;
mod provider_models;
pub mod runtime;
mod secrets;
mod state;
mod web;
mod workflows;

use std::{net::SocketAddr, path::PathBuf, sync::Arc, time::Duration};

use axum::{
    Router,
    http::{HeaderName, HeaderValue, Method},
    middleware,
};
use tokio::{net::TcpListener, sync::oneshot, task::JoinHandle};
use tower::ServiceBuilder;
use tower_http::{
    catch_panic::CatchPanicLayer,
    cors::CorsLayer,
    request_id::{MakeRequestUuid, PropagateRequestIdLayer, SetRequestIdLayer},
    trace::TraceLayer,
};

pub use config::{ServiceConfig, TlsConfig};
pub use error::{ProblemDetails, ServiceError};
pub use events::{EventHub, ServiceEvent};
pub use models::*;
pub use state::{AppState, RuntimeStatus};

#[derive(Debug)]
pub struct ServiceHandle {
    address: SocketAddr,
    state: Arc<AppState>,
    shutdown: Option<oneshot::Sender<()>>,
    task: JoinHandle<Result<(), std::io::Error>>,
}

impl ServiceHandle {
    pub const fn address(&self) -> SocketAddr {
        self.address
    }

    pub fn scheme(&self) -> &'static str {
        self.state.config.scheme()
    }

    pub fn state(&self) -> &Arc<AppState> {
        &self.state
    }

    /// Returns the one-time desktop bootstrap nonce without logging or persisting it.
    pub async fn desktop_bootstrap_nonce(&self) -> Option<zeroize::Zeroizing<String>> {
        self.state.auth.take_bootstrap_nonce().await
    }

    /// Gracefully stops the embedded service and every provider child it owns.
    ///
    /// # Errors
    ///
    /// Returns an error when the server task or an underlying I/O operation fails.
    pub async fn shutdown(mut self) -> Result<(), ServiceError> {
        self.state.close_shutdown_admission().await;
        if let Some(sender) = self.shutdown.take() {
            let _ = sender.send(());
        }
        match conversion::checkpoint_jobs_for_shutdown(Arc::clone(&self.state)).await {
            Ok(checkpointed_jobs) if checkpointed_jobs > 0 => {
                tracing::info!(
                    diagnostic_code = "jobs.shutdown.checkpointed",
                    checkpointed_jobs,
                    "active jobs were checkpointed for shutdown"
                );
            }
            Ok(_) => {}
            Err(error) => {
                tracing::warn!(
                    diagnostic_code = "jobs.shutdown.checkpoint_failed",
                    %error,
                    "one or more active jobs could not be checkpointed before shutdown"
                );
            }
        }
        if !self.state.mlx.shutdown_owned().await {
            tracing::warn!(
                diagnostic_code = "mlx.management.shutdown.timeout",
                "an app-owned MLX-audio operation did not stop before the shutdown deadline"
            );
        }
        let remaining_model_operations = self.state.provider_models.shutdown_owned().await;
        if remaining_model_operations > 0 {
            tracing::warn!(
                diagnostic_code = "provider.model.shutdown.timeout",
                remaining_model_operations,
                "some app-owned provider model operations did not stop before the shutdown deadline"
            );
        }
        let report = self.state.providers.shutdown_owned().await;
        if !report.failures.is_empty() {
            tracing::warn!(diagnostic_code = "provider.shutdown.partial", failures = ?report.failures, "some owned provider children did not stop cleanly");
        }
        if let Ok(result) = tokio::time::timeout(Duration::from_secs(15), &mut self.task).await {
            result??;
        } else {
            tracing::warn!(
                diagnostic_code = "service.shutdown.timeout",
                "the local service exceeded its graceful shutdown deadline"
            );
            self.task.abort();
            let _ = self.task.await;
        }
        Ok(())
    }

    /// Waits until the embedded service exits.
    ///
    /// # Errors
    ///
    /// Returns an error when the server task or an underlying I/O operation fails.
    pub async fn wait(self) -> Result<(), ServiceError> {
        self.task.await??;
        Ok(())
    }
}

/// Starts the authenticated embedded service using the supplied configuration.
///
/// # Errors
///
/// Returns an error when configuration, storage, TLS, provider initialization,
/// recovery, or listener startup fails.
pub async fn start(config: ServiceConfig) -> Result<ServiceHandle, ServiceError> {
    let managed_media_root = config.data_dir.clone();
    start_with_managed_media_root(config, managed_media_root).await
}

/// Starts the service while keeping its database and control state in
/// `config.data_dir` and placing only the managed library and cache under the
/// separately selected media root.
///
/// # Errors
///
/// Returns an error when configuration, either storage root, TLS, provider
/// initialization, recovery, or listener startup fails.
pub async fn start_with_managed_media_root(
    mut config: ServiceConfig,
    managed_media_root: PathBuf,
) -> Result<ServiceHandle, ServiceError> {
    config.validate()?;
    let paths = audiobookai_storage::AppPaths::from_roots(&config.data_dir, managed_media_root);
    paths
        .ensure()
        .await
        .map_err(|error| ServiceError::Storage(error.to_string()))?;
    let passphrase_salt = config.data_dir.join("secret-store.salt");
    match tokio::fs::symlink_metadata(&passphrase_salt).await {
        Ok(metadata) if metadata.is_file() && !metadata.file_type().is_symlink() => {
            audiobookai_storage::harden_private_file(&passphrase_salt)
                .await
                .map_err(|error| ServiceError::Storage(error.to_string()))?;
        }
        Ok(_) => {
            return Err(ServiceError::InvalidRequest(
                "secret-store salt path must be a regular managed file".to_owned(),
            ));
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(error.into()),
    }
    diagnostics::configure_global(&config.data_dir);

    let listener = TcpListener::bind(config.bind).await?;
    let address = listener.local_addr()?;
    // Authentication compares exact authorities. Preserve the OS-selected port
    // when callers requested port zero.
    config.bind = address;
    let rustls = if let Some(tls) = &config.tls {
        if rustls::crypto::CryptoProvider::get_default().is_none() {
            let _ = rustls::crypto::ring::default_provider().install_default();
        }
        Some(
            axum_server::tls_rustls::RustlsConfig::from_pem_file(
                &tls.certificate_chain_path,
                &tls.private_key_path,
            )
            .await
            .map_err(|error| ServiceError::TlsConfiguration(error.to_string()))?,
        )
    } else {
        None
    };

    let database = audiobookai_storage::Database::open(paths)
        .await
        .map_err(|error| ServiceError::Storage(error.to_string()))?;
    let state = Arc::new(AppState::new(config.clone(), database).await?);
    conversion::resume_durable_conversions(Arc::clone(&state)).await?;
    workflows::resume_durable_detections(Arc::clone(&state)).await?;
    let (shutdown, shutdown_rx) = oneshot::channel();

    let app = build_router(&state);
    state.events.publish(
        "service.started",
        serde_json::json!({
            "address": address.to_string(),
            "scheme": config.scheme(),
        }),
    );
    let task = if let Some(rustls) = rustls {
        let std_listener = listener.into_std()?;
        let server = axum_server::tls_rustls::from_tcp_rustls(std_listener, rustls)
            .map_err(|error| ServiceError::TlsConfiguration(error.to_string()))?;
        let handle = axum_server::Handle::new();
        let shutdown_handle = handle.clone();
        tokio::spawn(async move {
            let _ = shutdown_rx.await;
            shutdown_handle.graceful_shutdown(Some(Duration::from_secs(10)));
        });
        tokio::spawn(async move { server.handle(handle).serve(app.into_make_service()).await })
    } else {
        tokio::spawn(async move {
            axum::serve(listener, app)
                .with_graceful_shutdown(async {
                    let _ = shutdown_rx.await;
                })
                .await
        })
    };

    Ok(ServiceHandle {
        address,
        state,
        shutdown: Some(shutdown),
        task,
    })
}

fn build_router(state: &Arc<AppState>) -> Router {
    let protected_api = api::router(Arc::clone(state))
        .layer(middleware::from_fn(diagnostics::request_diagnostics))
        .layer(middleware::from_fn_with_state(
            Arc::clone(state),
            idempotency::enforce,
        ))
        .layer(middleware::from_fn_with_state(
            Arc::clone(state),
            auth::require_session,
        ));
    let cors = CorsLayer::new()
        .allow_origin([
            HeaderValue::from_static("tauri://localhost"),
            HeaderValue::from_static("http://tauri.localhost"),
            HeaderValue::from_static("https://tauri.localhost"),
        ])
        .allow_credentials(true)
        .allow_methods([
            Method::GET,
            Method::HEAD,
            Method::POST,
            Method::PUT,
            Method::PATCH,
            Method::DELETE,
            Method::OPTIONS,
        ])
        .allow_headers([
            http::header::ACCEPT,
            http::header::AUTHORIZATION,
            http::header::CONTENT_TYPE,
            HeaderName::from_static("idempotency-key"),
            HeaderName::from_static("x-csrf-token"),
            HeaderName::from_static("x-request-id"),
        ]);
    auth::router(Arc::clone(state))
        .merge(protected_api)
        .merge(web::router())
        .layer(middleware::from_fn_with_state(
            Arc::clone(state),
            auth::enforce_authority,
        ))
        .layer(cors)
        .layer(
            ServiceBuilder::new()
                .layer(SetRequestIdLayer::new(
                    http::HeaderName::from_static("x-request-id"),
                    MakeRequestUuid,
                ))
                .layer(PropagateRequestIdLayer::new(http::HeaderName::from_static(
                    "x-request-id",
                )))
                .layer(CatchPanicLayer::new())
                .layer(TraceLayer::new_for_http()),
        )
}

#[cfg(test)]
mod tests {
    use audiobookai_core::{BookId, Job, JobId, JobKind, JobState, ProjectId};
    use chrono::Utc;

    use super::*;

    #[tokio::test]
    async fn refuses_non_loopback_without_explicit_transport_override() {
        let directory = tempfile::tempdir().expect("tempdir");
        let config = ServiceConfig {
            bind: "0.0.0.0:0".parse().expect("address"),
            data_dir: directory.path().to_path_buf(),
            bundled_sidecar_dir: None,
            tls: None,
            lan_hostnames: vec!["127.0.0.1".to_owned()],
            allow_insecure_lan: false,
            desktop_bootstrap: false,
        };
        assert!(matches!(
            start(config).await,
            Err(ServiceError::TlsRequiredForLan(_))
        ));
    }

    #[tokio::test]
    async fn starts_on_an_ephemeral_loopback_port() {
        let directory = tempfile::tempdir().expect("tempdir");
        let handle = start(ServiceConfig {
            bind: "127.0.0.1:0".parse().expect("address"),
            data_dir: directory.path().to_path_buf(),
            bundled_sidecar_dir: None,
            tls: None,
            lan_hostnames: Vec::new(),
            allow_insecure_lan: false,
            desktop_bootstrap: true,
        })
        .await
        .expect("service starts");
        assert!(handle.address().ip().is_loopback());
        handle.shutdown().await.expect("clean shutdown");
    }

    #[tokio::test]
    async fn external_media_root_keeps_the_live_database_local() {
        let directory = tempfile::tempdir().expect("tempdir");
        let data_root = directory.path().join("data");
        let media_root = directory.path().join("media");
        std::fs::create_dir(&media_root).expect("media root");
        let handle = start_with_managed_media_root(
            ServiceConfig {
                bind: "127.0.0.1:0".parse().expect("address"),
                data_dir: data_root.clone(),
                bundled_sidecar_dir: None,
                tls: None,
                lan_hostnames: Vec::new(),
                allow_insecure_lan: false,
                desktop_bootstrap: true,
            },
            media_root.clone(),
        )
        .await
        .expect("service starts");

        let settings = handle.state().catalog.read().await.settings.clone();
        assert_eq!(
            settings.library_path,
            media_root.join("library").to_string_lossy()
        );
        assert_eq!(
            settings.cache_path,
            media_root.join("cache").to_string_lossy()
        );
        assert!(data_root.join("audiobookai.sqlite3").is_file());
        assert!(!media_root.join("audiobookai.sqlite3").exists());
        handle.shutdown().await.expect("clean shutdown");
    }

    #[tokio::test]
    async fn shutdown_persists_an_explicit_resume_checkpoint_for_active_jobs() {
        let directory = tempfile::tempdir().expect("tempdir");
        let handle = start(ServiceConfig {
            bind: "127.0.0.1:0".parse().expect("address"),
            data_dir: directory.path().to_path_buf(),
            bundled_sidecar_dir: None,
            tls: None,
            lan_hostnames: Vec::new(),
            allow_insecure_lan: false,
            desktop_bootstrap: true,
        })
        .await
        .expect("service starts");
        let database = handle.state().database.clone();
        let state = Arc::clone(handle.state());
        let now = Utc::now();
        let book_id = BookId::new();
        let project_id = ProjectId::new();
        sqlx::query(
            "INSERT INTO books (id, managed_epub_path, source_hash, imported_at, payload) VALUES (?, ?, ?, ?, '{}')",
        )
        .bind(book_id.to_string())
        .bind(directory.path().join("fixture.epub").to_string_lossy().into_owned())
        .bind("synthetic-source-hash")
        .bind(now.to_rfc3339())
        .execute(database.pool())
        .await
        .expect("book ownership fixture");
        sqlx::query(
            "INSERT INTO projects (id, book_id, name, status, created_at, updated_at, revision, payload) VALUES (?, ?, ?, 'draft', ?, ?, 0, '{}')",
        )
        .bind(project_id.to_string())
        .bind(book_id.to_string())
        .bind("Shutdown fixture")
        .bind(now.to_rfc3339())
        .bind(now.to_rfc3339())
        .execute(database.pool())
        .await
        .expect("project ownership fixture");
        let job = Job {
            id: JobId::new(),
            project_id,
            kind: JobKind::Conversion,
            state: JobState::Running,
            export_profile_id: None,
            reservation_id: None,
            progress_completed: 0,
            progress_total: 1,
            status_message: Some("Synthetic active job".to_owned()),
            allow_budget_override: false,
            created_at: now,
            started_at: Some(now),
            finished_at: None,
            updated_at: now,
            revision: 0,
        };
        database
            .repositories()
            .jobs
            .insert(&job)
            .await
            .expect("active job fixture");

        handle.shutdown().await.expect("clean shutdown");

        assert!(matches!(
            state.admit_shutdown_sensitive_work().await,
            Err(ServiceError::Conflict(message)) if message.contains("shutting down")
        ));

        let persisted = database
            .repositories()
            .jobs
            .get(job.id)
            .await
            .expect("read checkpoint")
            .expect("persisted job");
        assert_eq!(persisted.state, JobState::Pausing);
        assert_eq!(
            persisted.status_message.as_deref(),
            Some("Checkpointed for application shutdown")
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn startup_repairs_private_passphrase_salt_permissions() {
        use std::os::unix::fs::PermissionsExt;

        let directory = tempfile::tempdir().expect("tempdir");
        let salt = directory.path().join("secret-store.salt");
        tokio::fs::write(&salt, [0_u8; 16])
            .await
            .expect("salt fixture");
        tokio::fs::set_permissions(&salt, std::fs::Permissions::from_mode(0o644))
            .await
            .expect("permissive fixture mode");
        let handle = start(ServiceConfig {
            bind: "127.0.0.1:0".parse().expect("address"),
            data_dir: directory.path().to_path_buf(),
            bundled_sidecar_dir: None,
            tls: None,
            lan_hostnames: Vec::new(),
            allow_insecure_lan: false,
            desktop_bootstrap: true,
        })
        .await
        .expect("service starts");
        let mode = std::fs::metadata(&salt)
            .expect("salt metadata")
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(mode, 0o600);
        handle.shutdown().await.expect("clean shutdown");
    }

    #[tokio::test]
    async fn diagnostics_and_exports_require_an_authenticated_session() {
        let directory = tempfile::tempdir().expect("tempdir");
        let handle = start(ServiceConfig {
            bind: "127.0.0.1:0".parse().expect("address"),
            data_dir: directory.path().to_path_buf(),
            bundled_sidecar_dir: None,
            tls: None,
            lan_hostnames: Vec::new(),
            allow_insecure_lan: false,
            desktop_bootstrap: true,
        })
        .await
        .expect("service starts");
        let base = format!("http://{}", handle.address());
        let client = reqwest::Client::new();

        let unauthorized = client
            .get(format!("{base}/api/v1/diagnostics"))
            .send()
            .await
            .expect("diagnostics response");
        assert_eq!(unauthorized.status(), http::StatusCode::UNAUTHORIZED);

        let nonce = handle
            .desktop_bootstrap_nonce()
            .await
            .expect("desktop nonce");
        let bootstrap = client
            .post(format!("{base}/api/v1/auth/bootstrap"))
            .json(&serde_json::json!({ "nonce": nonce.as_str() }))
            .send()
            .await
            .expect("bootstrap response");
        assert!(bootstrap.status().is_success());
        let session_cookie = bootstrap
            .headers()
            .get_all(http::header::SET_COOKIE)
            .iter()
            .filter_map(|value| value.to_str().ok())
            .find(|value| value.starts_with("audiobookai_session="))
            .and_then(|value| value.split(';').next())
            .expect("session cookie")
            .to_owned();

        let diagnostics = client
            .get(format!("{base}/api/v1/diagnostics"))
            .header(http::header::COOKIE, &session_cookie)
            .send()
            .await
            .expect("authenticated diagnostics");
        assert!(diagnostics.status().is_success());
        assert_eq!(
            diagnostics
                .headers()
                .get(http::header::CACHE_CONTROL)
                .and_then(|value| value.to_str().ok()),
            Some("no-store")
        );
        assert!(
            diagnostics
                .json::<serde_json::Value>()
                .await
                .expect("diagnostics JSON")
                .get("items")
                .is_some()
        );

        let export = client
            .get(format!("{base}/api/v1/diagnostics/export"))
            .header(http::header::COOKIE, &session_cookie)
            .send()
            .await
            .expect("authenticated export");
        assert!(export.status().is_success());
        assert_eq!(
            export
                .headers()
                .get(http::header::CONTENT_TYPE)
                .and_then(|value| value.to_str().ok()),
            Some("application/x-ndjson; charset=utf-8")
        );
        let exported = export.text().await.expect("diagnostic export body");
        let session_value = session_cookie
            .split_once('=')
            .map(|(_, value)| value)
            .expect("session cookie value");
        assert!(!exported.contains(nonce.as_str()));
        assert!(!exported.contains(session_value));
        handle.shutdown().await.expect("clean shutdown");
    }

    #[tokio::test]
    async fn invalid_tls_identity_fails_closed() {
        let directory = tempfile::tempdir().expect("tempdir");
        let result = start(ServiceConfig {
            bind: "0.0.0.0:0".parse().expect("address"),
            data_dir: directory.path().join("data"),
            bundled_sidecar_dir: None,
            tls: Some(TlsConfig {
                certificate_chain_path: directory.path().join("missing-certificate.pem"),
                private_key_path: directory.path().join("missing-private-key.pem"),
            }),
            lan_hostnames: vec!["127.0.0.1".to_owned()],
            allow_insecure_lan: true,
            desktop_bootstrap: false,
        })
        .await;
        assert!(matches!(result, Err(ServiceError::TlsConfiguration(_))));
    }

    #[tokio::test]
    async fn serves_https_for_an_explicit_lan_identity() {
        let directory = tempfile::tempdir().expect("tempdir");
        let rcgen::CertifiedKey { cert, signing_key } =
            rcgen::generate_simple_self_signed(vec!["127.0.0.1".to_owned()])
                .expect("test certificate");
        let certificate_pem = cert.pem();
        let certificate_path = directory.path().join("certificate.pem");
        let private_key_path = directory.path().join("private-key.pem");
        tokio::fs::write(&certificate_path, &certificate_pem)
            .await
            .expect("certificate fixture");
        tokio::fs::write(&private_key_path, signing_key.serialize_pem())
            .await
            .expect("private-key fixture");

        let handle = start(ServiceConfig {
            bind: "0.0.0.0:0".parse().expect("address"),
            data_dir: directory.path().join("data"),
            bundled_sidecar_dir: None,
            tls: Some(TlsConfig {
                certificate_chain_path: certificate_path,
                private_key_path,
            }),
            lan_hostnames: vec!["127.0.0.1".to_owned()],
            allow_insecure_lan: false,
            desktop_bootstrap: false,
        })
        .await
        .expect("TLS service starts");
        assert_eq!(handle.scheme(), "https");
        let certificate =
            reqwest::Certificate::from_pem(certificate_pem.as_bytes()).expect("root certificate");
        let client = reqwest::Client::builder()
            .add_root_certificate(certificate)
            .build()
            .expect("client");
        let health_url = format!(
            "https://127.0.0.1:{}/api/v1/health",
            handle.address().port()
        );
        let response = client.get(&health_url).send().await.expect("HTTPS request");
        assert!(response.status().is_success());
        drop(response);
        let rejected = client
            .get(&health_url)
            .header(
                http::header::HOST,
                format!("other.home.arpa:{}", handle.address().port()),
            )
            .send()
            .await
            .expect("HTTPS request with untrusted Host");
        assert_eq!(rejected.status(), http::StatusCode::FORBIDDEN);
        drop(rejected);
        drop(client);
        handle.shutdown().await.expect("clean TLS shutdown");
    }
}
