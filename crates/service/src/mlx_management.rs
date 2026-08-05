use std::{
    collections::{BTreeMap, BTreeSet, HashSet},
    ffi::OsString,
    path::{Path, PathBuf},
    process::Stdio,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
};

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use regex::Regex;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use tokio::{
    io::{AsyncRead, AsyncReadExt, AsyncWriteExt},
    process::Command,
    sync::Mutex,
};
use uuid::Uuid;

use crate::{ServiceConfig, ServiceError};

pub const MLX_AUDIO_VERSION: &str = "0.4.6";
pub const UV_VERSION: &str = "0.12.1";
const OWNER: &str = "AudiobookAI";
const RUNTIME_KIND: &str = "mlx-audio-runtime";
const MODEL_KIND: &str = "mlx-audio-model";
const INSTALLER_ROOT: &str = "share/mlx-audio-installer";
const INSTALLER_LOCK: &str = "installer.lock.json";
const REQUIREMENTS_LOCK: &str = "requirements.lock";
const WHEELHOUSE: &str = "wheelhouse";
const BUNDLED_PYTHON: &str = "python/bin/python3";
const MAX_INSTALLER_LOCK_BYTES: u64 = 4 * 1024 * 1024;
const MAX_REQUIREMENTS_BYTES: u64 = 4 * 1024 * 1024;
const MAX_WHEELS: usize = 2_048;
const MAX_TOOL_OUTPUT_BYTES: usize = 64 * 1024;
const MAX_TOOL_DIAGNOSTICS: usize = 16;

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum MlxOperationKind {
    Install,
    Uninstall,
    DownloadModel,
}

impl MlxOperationKind {
    const fn diagnostic_action(self) -> &'static str {
        match self {
            Self::Install => "install",
            Self::Uninstall => "uninstall",
            Self::DownloadModel => "download_model",
        }
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum MlxOperationState {
    Queued,
    Running,
    Cancelling,
    Succeeded,
    Failed,
    Cancelled,
}

impl MlxOperationState {
    const fn active(self) -> bool {
        matches!(self, Self::Queued | Self::Running | Self::Cancelling)
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct MlxOperationView {
    pub id: Uuid,
    pub kind: MlxOperationKind,
    pub state: MlxOperationState,
    pub progress_percent: u8,
    pub phase: String,
    pub message: String,
    pub model_id: Option<Uuid>,
    pub exit_code: Option<i32>,
    pub diagnostics: Vec<String>,
    pub started_at: DateTime<Utc>,
    pub finished_at: Option<DateTime<Utc>>,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum MlxModelState {
    Downloading,
    Ready,
    Failed,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct MlxModelView {
    pub id: Uuid,
    pub repository: String,
    pub revision: String,
    pub resolved_commit: Option<String>,
    pub local_path: PathBuf,
    pub state: MlxModelState,
    pub bytes: Option<u64>,
    pub created_at: DateTime<Utc>,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
#[allow(clippy::struct_excessive_bools)]
pub struct MlxManagementView {
    pub supported: bool,
    pub support_detail: String,
    pub uv_available: bool,
    pub required_uv_version: &'static str,
    pub installer_payload_available: bool,
    pub installed: bool,
    pub installed_version: Option<&'static str>,
    pub server_executable: Option<PathBuf>,
    pub models: Vec<MlxModelView>,
    pub active_operation: Option<MlxOperationView>,
    pub last_operation: Option<MlxOperationView>,
    pub profile_action_required: bool,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
struct OwnershipMarker {
    schema_version: u8,
    owner: String,
    kind: String,
    id: String,
    version: Option<String>,
    repository: Option<String>,
    revision: Option<String>,
    #[serde(default)]
    resolved_commit: Option<String>,
    #[serde(default)]
    python_version: Option<String>,
    #[serde(default)]
    installer_lock_sha256: Option<String>,
    #[serde(default)]
    completed_at: Option<DateTime<Utc>>,
    created_at: DateTime<Utc>,
}

#[derive(Clone, Debug)]
struct ActiveOperation {
    view: MlxOperationView,
    cancel: Arc<AtomicBool>,
}

#[derive(Debug, Default)]
struct ManagerState {
    installed: bool,
    models: BTreeMap<Uuid, MlxModelView>,
    active: Option<ActiveOperation>,
    last: Option<MlxOperationView>,
    profile_action_required: bool,
}

#[derive(Clone, Debug)]
struct CommandSpec {
    executable: PathBuf,
    arguments: Vec<OsString>,
    environment: BTreeMap<OsString, OsString>,
}

#[derive(Clone, Debug)]
struct InstallerPayload {
    uv: PathBuf,
    python: PathBuf,
    requirements: PathBuf,
    wheelhouse: PathBuf,
    python_version: String,
    lock_sha256: String,
}

#[derive(Clone, Copy, Debug, thiserror::Error)]
enum InstallerPayloadError {
    #[error("offline installer payload is missing")]
    Missing,
    #[error("offline installer payload has unsafe filesystem entries")]
    UnsafeFilesystem,
    #[error("offline installer metadata is invalid")]
    InvalidMetadata,
    #[error("offline installer payload is incomplete")]
    Incomplete,
}

impl InstallerPayloadError {
    const fn support_detail(self) -> &'static str {
        match self {
            Self::Missing => "The complete bundled offline installer payload is unavailable.",
            Self::UnsafeFilesystem => {
                "The bundled offline installer payload failed its filesystem safety checks."
            }
            Self::InvalidMetadata => {
                "The bundled offline installer payload failed its locked metadata checks."
            }
            Self::Incomplete => {
                "The bundled offline installer payload does not contain its complete locked wheel set."
            }
        }
    }
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct InstallerArtifactLock {
    schema_version: u8,
    package: String,
    version: String,
    target: String,
    python_version: String,
    complete_transitive_closure: bool,
    artifacts: Vec<InstallerArtifact>,
}

#[derive(Debug, Deserialize)]
struct InstallerArtifact {
    package: String,
    version: String,
    filename: String,
    url: String,
    sha256: String,
}

#[derive(Clone, Debug, Eq, PartialEq)]
enum ToolDiagnostic {
    UsingBundledPython(String),
    ResolvedPackages(u32),
    PreparedPackages(u32),
    InstalledPackages(u32),
    AuditedPackages(u32),
    HashVerificationFailed,
    OfflineArtifactUnavailable,
    PythonRuntimeFailed,
    PermissionDenied,
    DiskSpaceUnavailable,
    DependencyResolutionFailed,
    InstalledMetadataInvalid,
}

impl ToolDiagnostic {
    fn message(&self) -> String {
        match self {
            Self::UsingBundledPython(version) => {
                format!("Using bundled CPython {version}.")
            }
            Self::ResolvedPackages(count) => format!("Resolved {count} packages offline."),
            Self::PreparedPackages(count) => format!("Prepared {count} packages offline."),
            Self::InstalledPackages(count) => format!("Installed {count} packages."),
            Self::AuditedPackages(count) => format!("Audited {count} installed packages."),
            Self::HashVerificationFailed => {
                "The bundled artifact failed hash verification.".to_owned()
            }
            Self::OfflineArtifactUnavailable => {
                "A required bundled wheel is unavailable in the offline payload.".to_owned()
            }
            Self::PythonRuntimeFailed => "The bundled Python runtime could not be used.".to_owned(),
            Self::PermissionDenied => {
                "The installer could not write to its app-owned runtime directory.".to_owned()
            }
            Self::DiskSpaceUnavailable => {
                "The installer could not continue because local disk space is unavailable."
                    .to_owned()
            }
            Self::DependencyResolutionFailed => {
                "The hash-locked offline dependency set could not be resolved.".to_owned()
            }
            Self::InstalledMetadataInvalid => {
                "The installed MLX-audio package metadata did not match version 0.4.6.".to_owned()
            }
        }
    }
}

#[derive(Clone, Debug, Default)]
struct ToolRunReport {
    exit_code: Option<i32>,
    diagnostics: Vec<ToolDiagnostic>,
}

impl ToolRunReport {
    fn merge(&mut self, mut other: Self) {
        self.exit_code = other.exit_code;
        for diagnostic in other.diagnostics.drain(..) {
            if self.diagnostics.len() >= MAX_TOOL_DIAGNOSTICS {
                break;
            }
            if !self.diagnostics.contains(&diagnostic) {
                self.diagnostics.push(diagnostic);
            }
        }
    }
}

#[derive(Debug, thiserror::Error)]
enum ToolRunError {
    #[error("operation cancelled")]
    Cancelled(ToolRunReport),
    #[error("managed tool failed")]
    Failed(ToolRunReport),
    #[error("managed tool could not be started")]
    Start,
}

impl ToolRunError {
    fn failed() -> Self {
        Self::Failed(ToolRunReport::default())
    }
}

#[async_trait]
trait ToolRunner: Send + Sync + std::fmt::Debug {
    async fn run(
        &self,
        spec: CommandSpec,
        cancel: Arc<AtomicBool>,
    ) -> Result<ToolRunReport, ToolRunError>;
}

#[derive(Debug)]
struct ProcessToolRunner;

#[async_trait]
impl ToolRunner for ProcessToolRunner {
    async fn run(
        &self,
        spec: CommandSpec,
        cancel: Arc<AtomicBool>,
    ) -> Result<ToolRunReport, ToolRunError> {
        let mut command = Command::new(spec.executable);
        command
            .args(spec.arguments)
            .env_clear()
            .envs(spec.environment)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        let mut child = command.spawn().map_err(|_| ToolRunError::Start)?;
        let stdout = child.stdout.take().ok_or(ToolRunError::Start)?;
        let stderr = child.stderr.take().ok_or(ToolRunError::Start)?;
        let stdout_task = tokio::spawn(read_bounded_output(stdout));
        let stderr_task = tokio::spawn(read_bounded_output(stderr));
        loop {
            if cancel.load(Ordering::Acquire) {
                let _ = child.kill().await;
                let _ = child.wait().await;
                let report = collect_tool_report(None, stdout_task, stderr_task).await;
                return Err(ToolRunError::Cancelled(report));
            }
            if let Some(status) = child.try_wait().map_err(|_| ToolRunError::failed())? {
                let report = collect_tool_report(status.code(), stdout_task, stderr_task).await;
                return if status.success() {
                    Ok(report)
                } else {
                    Err(ToolRunError::Failed(report))
                };
            }
            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        }
    }
}

async fn read_bounded_output(mut stream: impl AsyncRead + Unpin) -> Vec<u8> {
    let mut retained = Vec::with_capacity(MAX_TOOL_OUTPUT_BYTES.min(8 * 1024));
    let mut buffer = [0_u8; 4 * 1024];
    while let Ok(read) = stream.read(&mut buffer).await {
        if read == 0 {
            break;
        }
        let remaining = MAX_TOOL_OUTPUT_BYTES.saturating_sub(retained.len());
        retained.extend_from_slice(&buffer[..read.min(remaining)]);
    }
    retained
}

async fn collect_tool_report(
    exit_code: Option<i32>,
    stdout: tokio::task::JoinHandle<Vec<u8>>,
    stderr: tokio::task::JoinHandle<Vec<u8>>,
) -> ToolRunReport {
    let mut diagnostics = Vec::new();
    if let Ok(output) = stdout.await {
        append_sanitized_tool_output(&mut diagnostics, &output);
    }
    if let Ok(output) = stderr.await {
        append_sanitized_tool_output(&mut diagnostics, &output);
    }
    ToolRunReport {
        exit_code,
        diagnostics,
    }
}

fn append_sanitized_tool_output(diagnostics: &mut Vec<ToolDiagnostic>, output: &[u8]) {
    let python = Regex::new(r"(?i)\bCPython\s+(3\.(?:10|11|12|13)\.\d{1,3})\b")
        .expect("constant CPython version expression");
    for line in String::from_utf8_lossy(output).lines() {
        if diagnostics.len() >= MAX_TOOL_DIAGNOSTICS {
            return;
        }
        let lower = line.to_ascii_lowercase();
        let diagnostic = phase_count(&lower, "resolved")
            .map(ToolDiagnostic::ResolvedPackages)
            .or_else(|| phase_count(&lower, "prepared").map(ToolDiagnostic::PreparedPackages))
            .or_else(|| phase_count(&lower, "installed").map(ToolDiagnostic::InstalledPackages))
            .or_else(|| phase_count(&lower, "audited").map(ToolDiagnostic::AuditedPackages))
            .or_else(|| {
                python
                    .captures(line)
                    .and_then(|captures| captures.get(1))
                    .map(|version| ToolDiagnostic::UsingBundledPython(version.as_str().to_owned()))
            })
            .or_else(|| {
                (lower.contains("hash")
                    && (lower.contains("mismatch")
                        || lower.contains("verification")
                        || lower.contains("expected")))
                .then_some(ToolDiagnostic::HashVerificationFailed)
            })
            .or_else(|| {
                (lower.contains("offline")
                    || lower.contains("no matching distribution")
                    || lower.contains("not found in the package cache"))
                .then_some(ToolDiagnostic::OfflineArtifactUnavailable)
            })
            .or_else(|| {
                (lower.contains("python") && lower.contains("error"))
                    .then_some(ToolDiagnostic::PythonRuntimeFailed)
            })
            .or_else(|| {
                (lower.contains("permission denied") || lower.contains("access is denied"))
                    .then_some(ToolDiagnostic::PermissionDenied)
            })
            .or_else(|| {
                (lower.contains("no space left") || lower.contains("disk full"))
                    .then_some(ToolDiagnostic::DiskSpaceUnavailable)
            })
            .or_else(|| {
                (lower.contains("resolution") || lower.contains("no solution found"))
                    .then_some(ToolDiagnostic::DependencyResolutionFailed)
            });
        if let Some(diagnostic) = diagnostic
            && !diagnostics.contains(&diagnostic)
        {
            diagnostics.push(diagnostic);
        }
    }
}

fn phase_count(line: &str, phase: &str) -> Option<u32> {
    let mut words = line.split(|character: char| !character.is_ascii_alphanumeric());
    while let Some(word) = words.next() {
        if word != phase {
            continue;
        }
        let count = words.find(|candidate| !candidate.is_empty())?;
        return count.parse::<u32>().ok().filter(|count| *count <= 100_000);
    }
    None
}

#[derive(Clone)]
pub struct MlxManager {
    root: Arc<PathBuf>,
    bundled_sidecar_bin: Option<PathBuf>,
    uv_available: bool,
    installer_payload_available: bool,
    installer_support_detail: Arc<str>,
    supported: bool,
    runner: Arc<dyn ToolRunner>,
    state: Arc<Mutex<ManagerState>>,
}

impl std::fmt::Debug for MlxManager {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("MlxManager")
            .field("root", &self.root)
            .field("uv_available", &self.uv_available)
            .field(
                "installer_payload_available",
                &self.installer_payload_available,
            )
            .field("supported", &self.supported)
            .finish_non_exhaustive()
    }
}

impl MlxManager {
    pub async fn initialize(config: &ServiceConfig) -> Result<Self, ServiceError> {
        let bundled_sidecar_bin = config.bundled_sidecar_dir.clone();
        let uv_available = if let Some(directory) = &bundled_sidecar_bin {
            bundled_regular_file(directory, &directory.join("uv"))
                .await
                .is_ok()
        } else {
            false
        };
        let (installer_payload_available, installer_support_detail) = if let Some(directory) =
            &bundled_sidecar_bin
        {
            match validate_installer_payload(directory, false).await {
                Ok(_) => (
                    true,
                    "The complete hash-locked offline installer payload is available.".to_owned(),
                ),
                Err(error) => (false, error.support_detail().to_owned()),
            }
        } else {
            (
                false,
                InstallerPayloadError::Missing.support_detail().to_owned(),
            )
        };
        let manager = Self {
            root: Arc::new(config.data_dir.join("managed-providers").join("mlx-audio")),
            bundled_sidecar_bin,
            uv_available,
            installer_payload_available,
            installer_support_detail: Arc::from(installer_support_detail),
            supported: cfg!(all(target_os = "macos", target_arch = "aarch64")),
            runner: Arc::new(ProcessToolRunner),
            state: Arc::new(Mutex::new(ManagerState::default())),
        };
        manager.hydrate().await?;
        Ok(manager)
    }

    #[cfg(test)]
    async fn for_test(
        root: PathBuf,
        bundled_sidecar_bin: PathBuf,
        runner: Arc<dyn ToolRunner>,
    ) -> Result<Self, ServiceError> {
        let uv_available =
            bundled_regular_file(&bundled_sidecar_bin, &bundled_sidecar_bin.join("uv"))
                .await
                .is_ok();
        let (installer_payload_available, installer_support_detail) =
            match validate_installer_payload(&bundled_sidecar_bin, false).await {
                Ok(_) => (
                    true,
                    "The complete hash-locked offline installer payload is available.".to_owned(),
                ),
                Err(error) => (false, error.support_detail().to_owned()),
            };
        let manager = Self {
            root: Arc::new(root),
            bundled_sidecar_bin: Some(bundled_sidecar_bin),
            uv_available,
            installer_payload_available,
            installer_support_detail: Arc::from(installer_support_detail),
            supported: true,
            runner,
            state: Arc::new(Mutex::new(ManagerState::default())),
        };
        manager.hydrate().await?;
        Ok(manager)
    }

    pub async fn view(&self) -> MlxManagementView {
        let state = self.state.lock().await;
        let mut models = state.models.values().cloned().collect::<Vec<_>>();
        models.sort_by(|left, right| left.repository.cmp(&right.repository));
        MlxManagementView {
            supported: self.supported,
            support_detail: if !self.supported {
                "MLX-audio app management requires macOS on Apple Silicon.".to_owned()
            } else if self.installer_payload_available {
                self.installer_support_detail.to_string()
            } else {
                format!(
                    "Managed installation is disabled. {}",
                    self.installer_support_detail
                )
            },
            uv_available: self.uv_available,
            required_uv_version: UV_VERSION,
            installer_payload_available: self.installer_payload_available,
            installed: state.installed,
            installed_version: state.installed.then_some(MLX_AUDIO_VERSION),
            server_executable: state.installed.then(|| self.server_executable()),
            models,
            active_operation: state.active.as_ref().map(|active| active.view.clone()),
            last_operation: state.last.clone(),
            profile_action_required: state.profile_action_required,
        }
    }

    pub async fn start_install(&self) -> Result<MlxOperationView, ServiceError> {
        self.require_supported()?;
        let sidecar_bin = self.bundled_sidecar_bin.as_ref().ok_or_else(|| {
            ServiceError::Conflict(
                "the complete verified offline MLX-audio installer payload is unavailable; installation is disabled"
                    .to_owned(),
            )
        })?;
        let payload = validate_installer_payload(sidecar_bin, true).await.map_err(|_| {
            ServiceError::Conflict(
                "the complete verified offline MLX-audio installer payload is unavailable or invalid; installation is disabled"
                    .to_owned(),
            )
        })?;
        {
            let state = self.state.lock().await;
            if state.installed {
                return Err(ServiceError::Conflict(
                    "the app-managed MLX-audio runtime is already installed".to_owned(),
                ));
            }
            ensure_idle(&state)?;
        }
        let operation = self
            .begin_operation(MlxOperationKind::Install, None)
            .await?;
        let manager = self.clone();
        let operation_id = operation.id;
        tokio::spawn(async move {
            manager.run_install(operation_id, payload).await;
        });
        Ok(operation)
    }

    pub async fn start_uninstall(&self) -> Result<MlxOperationView, ServiceError> {
        self.require_supported()?;
        {
            let state = self.state.lock().await;
            if !state.installed {
                return Err(ServiceError::Conflict(
                    "the app-managed MLX-audio runtime is not installed".to_owned(),
                ));
            }
            ensure_idle(&state)?;
        }
        let operation = self
            .begin_operation(MlxOperationKind::Uninstall, None)
            .await?;
        let manager = self.clone();
        let operation_id = operation.id;
        tokio::spawn(async move {
            manager.run_uninstall(operation_id).await;
        });
        Ok(operation)
    }

    pub async fn start_model_download(
        &self,
        repository: String,
        revision: String,
    ) -> Result<MlxOperationView, ServiceError> {
        self.require_supported()?;
        validate_repository(&repository)?;
        validate_revision(&revision)?;
        {
            let state = self.state.lock().await;
            if !state.installed {
                return Err(ServiceError::Conflict(
                    "install the app-managed MLX-audio runtime before downloading a model"
                        .to_owned(),
                ));
            }
            ensure_idle(&state)?;
        }
        let hf = self.hf_executable();
        if !hf.is_file() {
            return Err(ServiceError::Conflict(
                "the installed MLX-audio tool environment has no public model downloader"
                    .to_owned(),
            ));
        }
        let model_id = Uuid::new_v4();
        let operation = self
            .begin_operation(MlxOperationKind::DownloadModel, Some(model_id))
            .await?;
        {
            let mut state = self.state.lock().await;
            state.models.insert(
                model_id,
                MlxModelView {
                    id: model_id,
                    repository: repository.clone(),
                    revision: revision.clone(),
                    resolved_commit: None,
                    local_path: self.model_payload_root(model_id),
                    state: MlxModelState::Downloading,
                    bytes: None,
                    created_at: Utc::now(),
                },
            );
        }
        let manager = self.clone();
        tokio::spawn(async move {
            manager
                .run_model_download(operation.id, model_id, repository, revision, hf)
                .await;
        });
        Ok(operation)
    }

    pub async fn cancel(&self, operation_id: Uuid) -> Result<MlxOperationView, ServiceError> {
        let mut state = self.state.lock().await;
        let active = state.active.as_mut().ok_or(ServiceError::NotFound)?;
        if active.view.id != operation_id {
            return Err(ServiceError::NotFound);
        }
        active.cancel.store(true, Ordering::Release);
        active.view.state = MlxOperationState::Cancelling;
        "cancelling".clone_into(&mut active.view.phase);
        "Cancelling the app-owned operation.".clone_into(&mut active.view.message);
        Ok(active.view.clone())
    }

    pub async fn remove_model(&self, id: Uuid) -> Result<(), ServiceError> {
        {
            let state = self.state.lock().await;
            ensure_idle(&state)?;
            let model = state.models.get(&id).ok_or(ServiceError::NotFound)?;
            if matches!(model.state, MlxModelState::Downloading) {
                return Err(ServiceError::Conflict(
                    "cancel the model download before removing it".to_owned(),
                ));
            }
        }
        remove_owned_tree(
            &self.model_root(id),
            &self.models_root(),
            MODEL_KIND,
            &id.to_string(),
        )
        .await?;
        self.state.lock().await.models.remove(&id);
        Ok(())
    }

    pub async fn set_profile_action_required(&self, required: bool) {
        self.state.lock().await.profile_action_required = required;
    }

    async fn hydrate(&self) -> Result<(), ServiceError> {
        tokio::fs::create_dir_all(self.root.as_ref()).await?;
        let runtime = self.runtime_root();
        let installed = validate_completed_runtime(&runtime).await;
        let models_root = self.models_root();
        tokio::fs::create_dir_all(&models_root).await?;
        let mut models = BTreeMap::new();
        let mut entries = tokio::fs::read_dir(&models_root).await?;
        while let Some(entry) = entries.next_entry().await? {
            let Some(name) = entry.file_name().to_str().map(str::to_owned) else {
                continue;
            };
            let Ok(id) = Uuid::parse_str(&name) else {
                continue;
            };
            let path = entry.path();
            let Ok(marker) = read_marker(&path).await else {
                let _ =
                    recover_abandoned_owned_root(&path, &models_root, MODEL_KIND, &id.to_string())
                        .await;
                continue;
            };
            if marker.owner != OWNER || marker.kind != MODEL_KIND || marker.id != id.to_string() {
                continue;
            }
            let (Some(repository), Some(revision)) = (marker.repository, marker.revision) else {
                continue;
            };
            let payload = path.join("payload");
            let bytes = if marker.completed_at.is_some() {
                validate_model_tree(&payload).await.ok()
            } else {
                None
            };
            models.insert(
                id,
                MlxModelView {
                    id,
                    repository,
                    revision,
                    resolved_commit: marker.resolved_commit,
                    local_path: payload,
                    state: if bytes.is_some() {
                        MlxModelState::Ready
                    } else {
                        MlxModelState::Failed
                    },
                    bytes,
                    created_at: marker.created_at,
                },
            );
        }
        let mut state = self.state.lock().await;
        state.installed = installed;
        state.models = models;
        Ok(())
    }

    async fn begin_operation(
        &self,
        kind: MlxOperationKind,
        model_id: Option<Uuid>,
    ) -> Result<MlxOperationView, ServiceError> {
        let mut state = self.state.lock().await;
        ensure_idle(&state)?;
        let view = MlxOperationView {
            id: Uuid::new_v4(),
            kind,
            state: MlxOperationState::Queued,
            progress_percent: 0,
            phase: "queued".to_owned(),
            message: "The app-owned operation is queued.".to_owned(),
            model_id,
            exit_code: None,
            diagnostics: Vec::new(),
            started_at: Utc::now(),
            finished_at: None,
        };
        state.active = Some(ActiveOperation {
            view: view.clone(),
            cancel: Arc::new(AtomicBool::new(false)),
        });
        Ok(view)
    }

    async fn update_operation(&self, id: Uuid, progress: u8, phase: &str, message: &str) {
        let mut state = self.state.lock().await;
        if let Some(active) = state.active.as_mut().filter(|active| active.view.id == id) {
            active.view.state = MlxOperationState::Running;
            active.view.progress_percent = progress;
            phase.clone_into(&mut active.view.phase);
            message.clone_into(&mut active.view.message);
        }
    }

    async fn finish_operation(
        &self,
        id: Uuid,
        result: Result<ToolRunReport, ToolRunError>,
        success_message: &str,
    ) {
        let mut state = self.state.lock().await;
        let Some(mut active) = state.active.take().filter(|active| active.view.id == id) else {
            return;
        };
        active.view.finished_at = Some(Utc::now());
        active.view.progress_percent = if result.is_ok() {
            100
        } else {
            active.view.progress_percent
        };
        match result {
            Ok(report) => {
                apply_tool_report(&mut active.view, report);
                active.view.state = MlxOperationState::Succeeded;
                "complete".clone_into(&mut active.view.phase);
                success_message.clone_into(&mut active.view.message);
            }
            Err(ToolRunError::Cancelled(report)) => {
                apply_tool_report(&mut active.view, report);
                active.view.state = MlxOperationState::Cancelled;
                "cancelled".clone_into(&mut active.view.phase);
                "The app-owned operation was cancelled.".clone_into(&mut active.view.message);
            }
            Err(error @ (ToolRunError::Failed(_) | ToolRunError::Start)) => {
                if let ToolRunError::Failed(report) = error {
                    apply_tool_report(&mut active.view, report);
                } else {
                    active
                        .view
                        .diagnostics
                        .push("The bundled installer process could not be started.".to_owned());
                }
                active.view.state = MlxOperationState::Failed;
                "failed".clone_into(&mut active.view.phase);
                "The managed tool failed. Only allowlisted, redacted diagnostics were retained."
                    .clone_into(&mut active.view.message);
                tracing::warn!(
                    diagnostic_code = "mlx.management.operation.failed",
                    operation_id = %active.view.id,
                    action = active.view.kind.diagnostic_action(),
                    exit_code = active.view.exit_code,
                    "MLX-audio management operation failed"
                );
            }
        }
        state.last = Some(active.view);
    }

    // The validation, two process phases, exact metadata check, marker promotion,
    // and failure cleanup intentionally stay together to preserve their ordering.
    #[allow(clippy::too_many_lines)]
    async fn run_install(&self, id: Uuid, payload: InstallerPayload) {
        self.update_operation(
            id,
            5,
            "preparing",
            "Preparing an isolated app-owned runtime.",
        )
        .await;
        let runtime = self.runtime_root();
        let preparation = self.prepare_runtime_root(&payload).await;
        if preparation.is_err() {
            self.finish_operation(id, Err(ToolRunError::failed()), "")
                .await;
            return;
        }
        let cancel = self.active_cancel(id).await;
        self.update_operation(
            id,
            20,
            "creating_environment",
            "Creating an isolated environment with the bundled Python runtime.",
        )
        .await;
        let mut result = match self
            .runner
            .run(self.venv_spec(&payload), Arc::clone(&cancel))
            .await
        {
            Ok(report) if cancel.load(Ordering::Acquire) => Err(ToolRunError::Cancelled(report)),
            Ok(mut report) => {
                if validate_venv_python(&runtime, &payload.python)
                    .await
                    .is_err()
                {
                    report.diagnostics.push(ToolDiagnostic::PythonRuntimeFailed);
                    Err(ToolRunError::Failed(report))
                } else {
                    self.update_operation(
                        id,
                        55,
                        "installing_offline",
                        "Installing the complete hash-locked wheel set without network access.",
                    )
                    .await;
                    match self
                        .runner
                        .run(self.pip_install_spec(&payload), Arc::clone(&cancel))
                        .await
                    {
                        Ok(install_report) => {
                            report.merge(install_report);
                            if cancel.load(Ordering::Acquire) {
                                Err(ToolRunError::Cancelled(report))
                            } else {
                                Ok(report)
                            }
                        }
                        Err(ToolRunError::Failed(failure)) => {
                            report.merge(failure);
                            Err(ToolRunError::Failed(report))
                        }
                        Err(ToolRunError::Cancelled(cancelled)) => {
                            report.merge(cancelled);
                            Err(ToolRunError::Cancelled(report))
                        }
                        Err(ToolRunError::Start) => Err(ToolRunError::Start),
                    }
                }
            }
            Err(error) => Err(error),
        };
        result = match result {
            Ok(mut report) => {
                self.update_operation(
                    id,
                    90,
                    "verifying",
                    "Verifying installed MLX-audio package metadata and entry points.",
                )
                .await;
                if validate_installed_runtime(&runtime, &payload.python_version)
                    .await
                    .is_err()
                {
                    report
                        .diagnostics
                        .push(ToolDiagnostic::InstalledMetadataInvalid);
                    Err(ToolRunError::Failed(report))
                } else {
                    match self.complete_marker(&runtime).await {
                        Ok(()) => {
                            self.state.lock().await.installed = true;
                            Ok(report)
                        }
                        Err(_) => Err(ToolRunError::Failed(report)),
                    }
                }
            }
            Err(error) => Err(error),
        };
        if result.is_err() {
            let _ = remove_owned_tree(&runtime, self.root.as_ref(), RUNTIME_KIND, "runtime").await;
        }
        self.finish_operation(id, result, "MLX-audio 0.4.6 is installed locally.")
            .await;
    }

    async fn run_uninstall(&self, id: Uuid) {
        self.update_operation(
            id,
            30,
            "removing",
            "Removing only the app-owned MLX-audio runtime.",
        )
        .await;
        let result = remove_owned_tree(
            &self.runtime_root(),
            self.root.as_ref(),
            RUNTIME_KIND,
            "runtime",
        )
        .await
        .map(|()| ToolRunReport::default())
        .map_err(|_| ToolRunError::failed());
        if result.is_ok() {
            self.state.lock().await.installed = false;
        }
        self.finish_operation(
            id,
            result,
            "The app-owned runtime was removed. Downloaded models were retained.",
        )
        .await;
    }

    async fn run_model_download(
        &self,
        operation_id: Uuid,
        model_id: Uuid,
        repository: String,
        revision: String,
        hf: PathBuf,
    ) {
        let root = self.model_root(model_id);
        let payload = self.model_payload_root(model_id);
        let result = async {
            tokio::fs::create_dir(&root)
                .await
                .map_err(|_| ToolRunError::failed())?;
            write_marker(
                &root,
                OwnershipMarker {
                    schema_version: 1,
                    owner: OWNER.to_owned(),
                    kind: MODEL_KIND.to_owned(),
                    id: model_id.to_string(),
                    version: Some(MLX_AUDIO_VERSION.to_owned()),
                    repository: Some(repository.clone()),
                    revision: Some(revision.clone()),
                    resolved_commit: None,
                    python_version: None,
                    installer_lock_sha256: None,
                    completed_at: None,
                    created_at: Utc::now(),
                },
            )
            .await
            .map_err(|_| ToolRunError::failed())?;
            tokio::fs::create_dir(&payload)
                .await
                .map_err(|_| ToolRunError::failed())?;
            self.update_operation(
                operation_id,
                15,
                "downloading",
                "Downloading a public model into an app-owned directory.",
            )
            .await;
            let cancel = self.active_cancel(operation_id).await;
            let report = self
                .runner
                .run(
                    self.model_spec(hf, &repository, &revision, &payload),
                    Arc::clone(&cancel),
                )
                .await?;
            if cancel.load(Ordering::Acquire) {
                return Err(ToolRunError::Cancelled(report));
            }
            let bytes = validate_model_tree(&payload)
                .await
                .map_err(|_| ToolRunError::Failed(report.clone()))?;
            let resolved_commit = resolve_hf_commit(&payload, &revision).await;
            if let Some(model) = self.state.lock().await.models.get_mut(&model_id) {
                model.state = MlxModelState::Ready;
                model.bytes = Some(bytes);
                model.resolved_commit.clone_from(&resolved_commit);
            }
            let mut marker = read_marker(&root)
                .await
                .map_err(|_| ToolRunError::Failed(report.clone()))?;
            marker.resolved_commit = resolved_commit;
            marker.completed_at = Some(Utc::now());
            write_marker(&root, marker)
                .await
                .map_err(|_| ToolRunError::Failed(report.clone()))?;
            Ok(report)
        }
        .await;
        if result.is_err() {
            if remove_owned_tree(
                &root,
                &self.models_root(),
                MODEL_KIND,
                &model_id.to_string(),
            )
            .await
            .is_err()
            {
                let _ = recover_abandoned_owned_root(
                    &root,
                    &self.models_root(),
                    MODEL_KIND,
                    &model_id.to_string(),
                )
                .await;
            }
            self.state.lock().await.models.remove(&model_id);
        }
        self.finish_operation(
            operation_id,
            result,
            "The public model is available in its app-owned directory.",
        )
        .await;
    }

    async fn active_cancel(&self, id: Uuid) -> Arc<AtomicBool> {
        self.state
            .lock()
            .await
            .active
            .as_ref()
            .filter(|active| active.view.id == id)
            .map_or_else(
                || Arc::new(AtomicBool::new(true)),
                |active| Arc::clone(&active.cancel),
            )
    }

    async fn prepare_runtime_root(&self, payload: &InstallerPayload) -> Result<(), ServiceError> {
        let root = self.runtime_root();
        if tokio::fs::symlink_metadata(&root).await.is_ok()
            && let Err(removal_error) =
                remove_owned_tree(&root, self.root.as_ref(), RUNTIME_KIND, "runtime").await
            && !recover_abandoned_owned_root(&root, self.root.as_ref(), RUNTIME_KIND, "runtime")
                .await?
        {
            return Err(removal_error);
        }
        tokio::fs::create_dir_all(&root).await?;
        write_marker(
            &root,
            OwnershipMarker {
                schema_version: 1,
                owner: OWNER.to_owned(),
                kind: RUNTIME_KIND.to_owned(),
                id: "runtime".to_owned(),
                version: Some(MLX_AUDIO_VERSION.to_owned()),
                repository: None,
                revision: None,
                resolved_commit: None,
                python_version: Some(payload.python_version.clone()),
                installer_lock_sha256: Some(payload.lock_sha256.clone()),
                completed_at: None,
                created_at: Utc::now(),
            },
        )
        .await?;
        for relative in [
            "home",
            "xdg/config",
            "xdg/data",
            "xdg/cache",
            "cache/uv",
            "hf-home",
        ] {
            tokio::fs::create_dir_all(root.join(relative)).await?;
        }
        Ok(())
    }

    fn venv_spec(&self, payload: &InstallerPayload) -> CommandSpec {
        CommandSpec {
            executable: payload.uv.clone(),
            arguments: vec![
                OsString::from("venv"),
                OsString::from("--offline"),
                OsString::from("--no-config"),
                OsString::from("--no-python-downloads"),
                OsString::from("--python"),
                payload.python.as_os_str().to_owned(),
                self.venv_root().into_os_string(),
            ],
            environment: isolated_environment(&self.runtime_root()),
        }
    }

    fn pip_install_spec(&self, payload: &InstallerPayload) -> CommandSpec {
        CommandSpec {
            executable: payload.uv.clone(),
            arguments: vec![
                OsString::from("pip"),
                OsString::from("install"),
                OsString::from("--offline"),
                OsString::from("--no-index"),
                OsString::from("--no-sources"),
                OsString::from("--no-python-downloads"),
                OsString::from("--require-hashes"),
                OsString::from("--only-binary"),
                OsString::from(":all:"),
                OsString::from("--find-links"),
                payload.wheelhouse.as_os_str().to_owned(),
                OsString::from("--python"),
                self.venv_python().into_os_string(),
                OsString::from("--requirements"),
                payload.requirements.as_os_str().to_owned(),
            ],
            environment: isolated_environment(&self.runtime_root()),
        }
    }

    fn model_spec(
        &self,
        hf: PathBuf,
        repository: &str,
        revision: &str,
        local_dir: &Path,
    ) -> CommandSpec {
        let mut environment = isolated_environment(&self.runtime_root());
        environment.insert(
            OsString::from("HF_HOME"),
            self.runtime_root().join("hf-home").into_os_string(),
        );
        environment.insert(
            OsString::from("HF_HUB_DISABLE_IMPLICIT_TOKEN"),
            OsString::from("1"),
        );
        environment.insert(
            OsString::from("HF_HUB_DISABLE_TELEMETRY"),
            OsString::from("1"),
        );
        CommandSpec {
            executable: hf,
            arguments: vec![
                OsString::from("download"),
                OsString::from(repository),
                OsString::from("--revision"),
                OsString::from(revision),
                OsString::from("--local-dir"),
                local_dir.as_os_str().to_owned(),
            ],
            environment,
        }
    }

    fn require_supported(&self) -> Result<(), ServiceError> {
        self.supported.then_some(()).ok_or_else(|| {
            ServiceError::InvalidRequest(
                "app-managed MLX-audio is supported only on Apple Silicon macOS".to_owned(),
            )
        })
    }

    fn runtime_root(&self) -> PathBuf {
        self.root.join("runtime")
    }

    fn models_root(&self) -> PathBuf {
        self.root.join("models")
    }

    fn model_root(&self, id: Uuid) -> PathBuf {
        self.models_root().join(id.to_string())
    }

    fn model_payload_root(&self, id: Uuid) -> PathBuf {
        self.model_root(id).join("payload")
    }

    fn server_executable(&self) -> PathBuf {
        self.venv_root().join("bin").join("mlx_audio.server")
    }

    fn hf_executable(&self) -> PathBuf {
        self.venv_root().join("bin").join("hf")
    }

    fn venv_root(&self) -> PathBuf {
        self.runtime_root().join("venv")
    }

    fn venv_python(&self) -> PathBuf {
        self.venv_root().join("bin").join("python3")
    }

    async fn complete_marker(&self, root: &Path) -> Result<(), ServiceError> {
        let mut marker = read_marker(root).await?;
        marker.completed_at = Some(Utc::now());
        write_marker(root, marker).await
    }
}

fn apply_tool_report(view: &mut MlxOperationView, report: ToolRunReport) {
    view.exit_code = report.exit_code;
    view.diagnostics = report
        .diagnostics
        .into_iter()
        .take(MAX_TOOL_DIAGNOSTICS)
        .map(|diagnostic| diagnostic.message())
        .collect();
}

#[derive(Clone, Copy)]
enum ExpectedEntry {
    Directory,
    RegularFile,
}

#[derive(Debug)]
struct LockedRequirement {
    version: String,
    hashes: HashSet<String>,
}

async fn validate_installer_payload(
    sidecar_bin: &Path,
    verify_wheel_hashes: bool,
) -> Result<InstallerPayload, InstallerPayloadError> {
    if sidecar_bin.file_name().and_then(|name| name.to_str()) != Some("bin") {
        return Err(InstallerPayloadError::UnsafeFilesystem);
    }
    let sidecar_root = sidecar_bin
        .parent()
        .ok_or(InstallerPayloadError::UnsafeFilesystem)?;
    let canonical_root =
        canonical_bundled_entry(sidecar_root, sidecar_root, ExpectedEntry::Directory).await?;
    let canonical_bin =
        canonical_bundled_entry(sidecar_root, sidecar_bin, ExpectedEntry::Directory).await?;
    if canonical_bin.parent() != Some(canonical_root.as_path()) {
        return Err(InstallerPayloadError::UnsafeFilesystem);
    }

    let installer_root = sidecar_root.join(INSTALLER_ROOT);
    canonical_bundled_entry(sidecar_root, &installer_root, ExpectedEntry::Directory).await?;
    let uv = canonical_executable(sidecar_root, &sidecar_bin.join("uv")).await?;
    let python = canonical_executable(sidecar_root, &installer_root.join(BUNDLED_PYTHON)).await?;
    let lock_path = canonical_bundled_entry(
        sidecar_root,
        &installer_root.join(INSTALLER_LOCK),
        ExpectedEntry::RegularFile,
    )
    .await?;
    let requirements = canonical_bundled_entry(
        sidecar_root,
        &installer_root.join(REQUIREMENTS_LOCK),
        ExpectedEntry::RegularFile,
    )
    .await?;
    let wheelhouse = canonical_bundled_entry(
        sidecar_root,
        &installer_root.join(WHEELHOUSE),
        ExpectedEntry::Directory,
    )
    .await?;

    let lock_bytes = read_limited(&lock_path, MAX_INSTALLER_LOCK_BYTES).await?;
    let requirements_bytes = read_limited(&requirements, MAX_REQUIREMENTS_BYTES).await?;
    let lock = serde_json::from_slice::<InstallerArtifactLock>(&lock_bytes)
        .map_err(|_| InstallerPayloadError::InvalidMetadata)?;
    let requirements_text = std::str::from_utf8(&requirements_bytes)
        .map_err(|_| InstallerPayloadError::InvalidMetadata)?;
    let pins = parse_hash_locked_requirements(requirements_text)?;
    validate_artifact_lock(&lock, &pins)?;
    validate_wheelhouse(
        &canonical_root,
        &wheelhouse,
        &lock.artifacts,
        verify_wheel_hashes,
    )
    .await?;

    Ok(InstallerPayload {
        uv,
        python,
        requirements,
        wheelhouse,
        python_version: lock.python_version,
        lock_sha256: sha256_bytes(&lock_bytes),
    })
}

async fn bundled_regular_file(root: &Path, path: &Path) -> Result<PathBuf, InstallerPayloadError> {
    canonical_bundled_entry(root, path, ExpectedEntry::RegularFile).await
}

async fn canonical_executable(root: &Path, path: &Path) -> Result<PathBuf, InstallerPayloadError> {
    let canonical = bundled_regular_file(root, path).await?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        let metadata = tokio::fs::metadata(&canonical)
            .await
            .map_err(|_| InstallerPayloadError::Missing)?;
        if metadata.permissions().mode() & 0o111 == 0 {
            return Err(InstallerPayloadError::UnsafeFilesystem);
        }
    }
    Ok(canonical)
}

async fn canonical_bundled_entry(
    root: &Path,
    path: &Path,
    expected: ExpectedEntry,
) -> Result<PathBuf, InstallerPayloadError> {
    let relative = path
        .strip_prefix(root)
        .map_err(|_| InstallerPayloadError::UnsafeFilesystem)?;
    let root_metadata = tokio::fs::symlink_metadata(root)
        .await
        .map_err(|_| InstallerPayloadError::Missing)?;
    if !root_metadata.is_dir() || root_metadata.file_type().is_symlink() {
        return Err(InstallerPayloadError::UnsafeFilesystem);
    }
    let canonical_root = tokio::fs::canonicalize(root)
        .await
        .map_err(|_| InstallerPayloadError::Missing)?;
    let mut current = root.to_path_buf();
    for component in relative.components() {
        let std::path::Component::Normal(component) = component else {
            return Err(InstallerPayloadError::UnsafeFilesystem);
        };
        current.push(component);
        let metadata = tokio::fs::symlink_metadata(&current)
            .await
            .map_err(|_| InstallerPayloadError::Missing)?;
        if metadata.file_type().is_symlink() {
            return Err(InstallerPayloadError::UnsafeFilesystem);
        }
    }
    let metadata = tokio::fs::symlink_metadata(path)
        .await
        .map_err(|_| InstallerPayloadError::Missing)?;
    let matches_expected = match expected {
        ExpectedEntry::Directory => metadata.is_dir(),
        ExpectedEntry::RegularFile => metadata.is_file(),
    };
    if !matches_expected || metadata.file_type().is_symlink() {
        return Err(InstallerPayloadError::UnsafeFilesystem);
    }
    let canonical = tokio::fs::canonicalize(path)
        .await
        .map_err(|_| InstallerPayloadError::Missing)?;
    if canonical != canonical_root && !canonical.starts_with(&canonical_root) {
        return Err(InstallerPayloadError::UnsafeFilesystem);
    }
    Ok(canonical)
}

async fn read_limited(path: &Path, limit: u64) -> Result<Vec<u8>, InstallerPayloadError> {
    let metadata = tokio::fs::symlink_metadata(path)
        .await
        .map_err(|_| InstallerPayloadError::Missing)?;
    if !metadata.is_file() || metadata.file_type().is_symlink() || metadata.len() > limit {
        return Err(InstallerPayloadError::InvalidMetadata);
    }
    tokio::fs::read(path)
        .await
        .map_err(|_| InstallerPayloadError::Missing)
}

fn parse_hash_locked_requirements(
    requirements: &str,
) -> Result<BTreeMap<String, LockedRequirement>, InstallerPayloadError> {
    let mut logical_lines = Vec::new();
    let mut current = String::new();
    for source_line in requirements.lines() {
        let line = source_line.trim();
        if current.is_empty() && (line.is_empty() || line.starts_with('#')) {
            continue;
        }
        if line.starts_with('#') || line.contains(" #") {
            return Err(InstallerPayloadError::InvalidMetadata);
        }
        let continuation = line.ends_with('\\');
        let fragment = line.strip_suffix('\\').unwrap_or(line).trim();
        if !current.is_empty() {
            current.push(' ');
        }
        current.push_str(fragment);
        if !continuation {
            logical_lines.push(std::mem::take(&mut current));
        }
    }
    if !current.is_empty() || logical_lines.is_empty() || logical_lines.len() > MAX_WHEELS {
        return Err(InstallerPayloadError::InvalidMetadata);
    }

    let package =
        Regex::new(r"^[A-Za-z0-9][A-Za-z0-9_.-]*(?:\[[A-Za-z0-9_.-]+(?:,[A-Za-z0-9_.-]+)*\])?$")
            .expect("constant requirement package expression");
    let version = Regex::new(r"^[A-Za-z0-9][A-Za-z0-9._+!-]*$")
        .expect("constant requirement version expression");
    let digest = Regex::new(r"^[0-9a-f]{64}$").expect("constant SHA-256 expression");
    let mut pins = BTreeMap::new();
    let mut exact_mlx_pin = false;
    for line in logical_lines {
        let mut fields = line.split_ascii_whitespace();
        let requirement = fields
            .next()
            .ok_or(InstallerPayloadError::InvalidMetadata)?;
        let (name_with_extras, pinned_version) = requirement
            .split_once("==")
            .ok_or(InstallerPayloadError::InvalidMetadata)?;
        if requirement.matches("==").count() != 1
            || !package.is_match(name_with_extras)
            || !version.is_match(pinned_version)
        {
            return Err(InstallerPayloadError::InvalidMetadata);
        }
        let base_name = name_with_extras
            .split('[')
            .next()
            .unwrap_or(name_with_extras);
        let normalized = normalize_package_name(base_name);
        if normalized == "mlx-audio" {
            if requirement != "mlx-audio[tts,server]==0.4.6" {
                return Err(InstallerPayloadError::InvalidMetadata);
            }
            exact_mlx_pin = true;
        }
        let mut hashes = HashSet::new();
        for field in fields {
            let value = field
                .strip_prefix("--hash=sha256:")
                .ok_or(InstallerPayloadError::InvalidMetadata)?;
            if !digest.is_match(value) {
                return Err(InstallerPayloadError::InvalidMetadata);
            }
            hashes.insert(value.to_owned());
        }
        if hashes.is_empty()
            || pins
                .insert(
                    normalized,
                    LockedRequirement {
                        version: pinned_version.to_owned(),
                        hashes,
                    },
                )
                .is_some()
        {
            return Err(InstallerPayloadError::InvalidMetadata);
        }
    }
    if !exact_mlx_pin {
        return Err(InstallerPayloadError::InvalidMetadata);
    }
    Ok(pins)
}

fn validate_artifact_lock(
    lock: &InstallerArtifactLock,
    requirements: &BTreeMap<String, LockedRequirement>,
) -> Result<(), InstallerPayloadError> {
    let python =
        Regex::new(r"^3\.(?:10|11|12|13)\.\d+$").expect("constant Python version expression");
    let package =
        Regex::new(r"^[A-Za-z0-9][A-Za-z0-9_.-]*$").expect("constant artifact package expression");
    let version = Regex::new(r"^[A-Za-z0-9][A-Za-z0-9._+!-]*$")
        .expect("constant artifact version expression");
    let digest = Regex::new(r"^[0-9a-f]{64}$").expect("constant SHA-256 expression");
    if lock.schema_version != 1
        || lock.package != "mlx-audio[tts,server]"
        || lock.version != MLX_AUDIO_VERSION
        || lock.target != "aarch64-apple-darwin"
        || !python.is_match(&lock.python_version)
        || !lock.complete_transitive_closure
        || lock.artifacts.is_empty()
        || lock.artifacts.len() > MAX_WHEELS
    {
        return Err(InstallerPayloadError::InvalidMetadata);
    }
    let mut identities = BTreeMap::new();
    for artifact in &lock.artifacts {
        if !package.is_match(&artifact.package)
            || !version.is_match(&artifact.version)
            || !digest.is_match(&artifact.sha256)
            || !Path::new(&artifact.filename)
                .extension()
                .is_some_and(|extension| extension.eq_ignore_ascii_case("whl"))
            || Path::new(&artifact.filename)
                .file_name()
                .and_then(|name| name.to_str())
                != Some(artifact.filename.as_str())
            || !public_artifact_url(&artifact.url)
        {
            return Err(InstallerPayloadError::InvalidMetadata);
        }
        let normalized = normalize_package_name(&artifact.package);
        if identities.insert(normalized, artifact).is_some() {
            return Err(InstallerPayloadError::InvalidMetadata);
        }
    }
    if identities.len() != requirements.len() || identities.keys().ne(requirements.keys()) {
        return Err(InstallerPayloadError::Incomplete);
    }
    for (name, artifact) in identities {
        let requirement = &requirements[&name];
        if artifact.version != requirement.version || !requirement.hashes.contains(&artifact.sha256)
        {
            return Err(InstallerPayloadError::InvalidMetadata);
        }
    }
    let mlx = requirements
        .get("mlx-audio")
        .ok_or(InstallerPayloadError::InvalidMetadata)?;
    if mlx.version != MLX_AUDIO_VERSION {
        return Err(InstallerPayloadError::InvalidMetadata);
    }
    Ok(())
}

async fn validate_wheelhouse(
    sidecar_root: &Path,
    wheelhouse: &Path,
    artifacts: &[InstallerArtifact],
    verify_hashes: bool,
) -> Result<(), InstallerPayloadError> {
    let expected = artifacts
        .iter()
        .map(|artifact| (artifact.filename.as_str(), artifact.sha256.as_str()))
        .collect::<BTreeMap<_, _>>();
    let mut actual = BTreeSet::new();
    let mut entries = tokio::fs::read_dir(wheelhouse)
        .await
        .map_err(|_| InstallerPayloadError::Missing)?;
    while let Some(entry) = entries
        .next_entry()
        .await
        .map_err(|_| InstallerPayloadError::Missing)?
    {
        if actual.len() >= MAX_WHEELS {
            return Err(InstallerPayloadError::Incomplete);
        }
        let filename = entry
            .file_name()
            .to_str()
            .map(str::to_owned)
            .ok_or(InstallerPayloadError::UnsafeFilesystem)?;
        let path = canonical_bundled_entry(sidecar_root, &entry.path(), ExpectedEntry::RegularFile)
            .await?;
        let expected_hash = expected
            .get(filename.as_str())
            .ok_or(InstallerPayloadError::Incomplete)?;
        if verify_hashes && sha256_file(&path).await? != **expected_hash {
            return Err(InstallerPayloadError::InvalidMetadata);
        }
        actual.insert(filename);
    }
    let expected_names = expected.keys().copied().collect::<BTreeSet<_>>();
    if actual.iter().map(String::as_str).collect::<BTreeSet<_>>() != expected_names {
        return Err(InstallerPayloadError::Incomplete);
    }
    Ok(())
}

async fn sha256_file(path: &Path) -> Result<String, InstallerPayloadError> {
    let mut file = tokio::fs::File::open(path)
        .await
        .map_err(|_| InstallerPayloadError::Missing)?;
    let mut hasher = Sha256::new();
    let mut buffer = [0_u8; 8 * 1024];
    loop {
        let read = file
            .read(&mut buffer)
            .await
            .map_err(|_| InstallerPayloadError::Missing)?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
    }
    Ok(format!("{:x}", hasher.finalize()))
}

fn sha256_bytes(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}

fn normalize_package_name(name: &str) -> String {
    let mut normalized = String::with_capacity(name.len());
    let mut separator = false;
    for character in name.chars() {
        if matches!(character, '-' | '_' | '.') {
            if !separator {
                normalized.push('-');
            }
            separator = true;
        } else {
            normalized.push(character.to_ascii_lowercase());
            separator = false;
        }
    }
    normalized
}

fn public_artifact_url(value: &str) -> bool {
    url::Url::parse(value).is_ok_and(|url| {
        url.scheme() == "https"
            && url.host_str().is_some()
            && url.username().is_empty()
            && url.password().is_none()
            && url.query().is_none()
            && url.fragment().is_none()
    })
}

async fn validate_installed_runtime(
    runtime: &Path,
    python_version: &str,
) -> Result<(), InstallerPayloadError> {
    let version = Regex::new(r"^(3\.(?:10|11|12|13))\.\d+$")
        .expect("constant installed Python version expression");
    let major_minor = version
        .captures(python_version)
        .and_then(|captures| captures.get(1))
        .ok_or(InstallerPayloadError::InvalidMetadata)?
        .as_str();
    let venv = runtime.join("venv");
    canonical_bundled_entry(runtime, &venv, ExpectedEntry::Directory).await?;
    for executable in [venv.join("bin/mlx_audio.server"), venv.join("bin/hf")] {
        canonical_executable(runtime, &executable).await?;
    }
    let dist_info = venv
        .join("lib")
        .join(format!("python{major_minor}"))
        .join("site-packages")
        .join("mlx_audio-0.4.6.dist-info");
    canonical_bundled_entry(runtime, &dist_info, ExpectedEntry::Directory).await?;
    let metadata = canonical_bundled_entry(
        runtime,
        &dist_info.join("METADATA"),
        ExpectedEntry::RegularFile,
    )
    .await?;
    canonical_bundled_entry(
        runtime,
        &dist_info.join("RECORD"),
        ExpectedEntry::RegularFile,
    )
    .await?;
    let payload = read_limited(&metadata, 256 * 1024).await?;
    let text = std::str::from_utf8(&payload).map_err(|_| InstallerPayloadError::InvalidMetadata)?;
    let mut names = text.lines().filter(|line| line.starts_with("Name: "));
    let mut versions = text.lines().filter(|line| line.starts_with("Version: "));
    if names.next() != Some("Name: mlx-audio")
        || names.next().is_some()
        || versions.next() != Some("Version: 0.4.6")
        || versions.next().is_some()
    {
        return Err(InstallerPayloadError::InvalidMetadata);
    }
    Ok(())
}

async fn validate_venv_python(
    runtime: &Path,
    bundled_python: &Path,
) -> Result<(), InstallerPayloadError> {
    let venv_python = runtime.join("venv/bin/python3");
    let metadata = tokio::fs::symlink_metadata(&venv_python)
        .await
        .map_err(|_| InstallerPayloadError::Incomplete)?;
    if !metadata.is_file() && !metadata.file_type().is_symlink() {
        return Err(InstallerPayloadError::UnsafeFilesystem);
    }
    let canonical_runtime = tokio::fs::canonicalize(runtime)
        .await
        .map_err(|_| InstallerPayloadError::Incomplete)?;
    let canonical_python = tokio::fs::canonicalize(&venv_python)
        .await
        .map_err(|_| InstallerPayloadError::Incomplete)?;
    if metadata.file_type().is_symlink() {
        if canonical_python != bundled_python {
            return Err(InstallerPayloadError::UnsafeFilesystem);
        }
    } else if !canonical_python.starts_with(&canonical_runtime) {
        return Err(InstallerPayloadError::UnsafeFilesystem);
    }
    Ok(())
}

async fn validate_completed_runtime(runtime: &Path) -> bool {
    let Ok(marker) = read_marker(runtime).await else {
        return false;
    };
    let digest = Regex::new(r"^[0-9a-f]{64}$").expect("constant SHA-256 expression");
    let (Some(python_version), Some(lock_sha256)) = (
        marker.python_version.as_deref(),
        marker.installer_lock_sha256.as_deref(),
    ) else {
        return false;
    };
    marker.schema_version == 1
        && marker.owner == OWNER
        && marker.kind == RUNTIME_KIND
        && marker.id == "runtime"
        && marker.version.as_deref() == Some(MLX_AUDIO_VERSION)
        && marker.completed_at.is_some()
        && digest.is_match(lock_sha256)
        && validate_installed_runtime(runtime, python_version)
            .await
            .is_ok()
}

fn ensure_idle(state: &ManagerState) -> Result<(), ServiceError> {
    if state
        .active
        .as_ref()
        .is_some_and(|active| active.view.state.active())
    {
        Err(ServiceError::Conflict(
            "another MLX-audio management operation is active".to_owned(),
        ))
    } else {
        Ok(())
    }
}

fn isolated_environment(root: &Path) -> BTreeMap<OsString, OsString> {
    let mut environment = BTreeMap::new();
    for (key, value) in [
        ("HOME", root.join("home")),
        ("XDG_CONFIG_HOME", root.join("xdg/config")),
        ("XDG_DATA_HOME", root.join("xdg/data")),
        ("XDG_CACHE_HOME", root.join("xdg/cache")),
        ("UV_CACHE_DIR", root.join("cache/uv")),
        ("HF_HOME", root.join("hf-home")),
    ] {
        environment.insert(OsString::from(key), value.into_os_string());
    }
    for (key, value) in [
        ("UV_NO_CONFIG", "1"),
        ("UV_NO_PROGRESS", "1"),
        ("UV_OFFLINE", "1"),
        ("UV_NO_INDEX", "1"),
        ("UV_NO_SOURCES", "1"),
        ("UV_REQUIRE_HASHES", "1"),
        ("UV_PYTHON_DOWNLOADS", "never"),
        ("PIP_NO_INDEX", "1"),
        ("PIP_DISABLE_PIP_VERSION_CHECK", "1"),
        ("HF_HUB_DISABLE_IMPLICIT_TOKEN", "1"),
        ("HF_HUB_DISABLE_TELEMETRY", "1"),
        ("DO_NOT_TRACK", "1"),
    ] {
        environment.insert(OsString::from(key), OsString::from(value));
    }
    environment
}

fn validate_repository(repository: &str) -> Result<(), ServiceError> {
    reject_credential_shaped_identifier(repository)?;
    let expression =
        Regex::new(r"^[A-Za-z0-9][A-Za-z0-9._-]{0,95}/[A-Za-z0-9][A-Za-z0-9._-]{0,95}$")
            .expect("constant repository expression");
    let has_git_extension = Path::new(repository)
        .extension()
        .and_then(|extension| extension.to_str())
        .is_some_and(|extension| extension.eq_ignore_ascii_case("git"));
    if !expression.is_match(repository) || repository.contains("..") || has_git_extension {
        return Err(ServiceError::InvalidRequest(
            "model repository must be a public owner/name identifier".to_owned(),
        ));
    }
    Ok(())
}

fn validate_revision(revision: &str) -> Result<(), ServiceError> {
    reject_credential_shaped_identifier(revision)?;
    let expression =
        Regex::new(r"^[A-Za-z0-9][A-Za-z0-9._/-]{0,127}$").expect("constant revision expression");
    if !expression.is_match(revision)
        || revision.contains("..")
        || revision.contains("//")
        || revision.ends_with('/')
    {
        return Err(ServiceError::InvalidRequest(
            "model revision contains unsupported characters".to_owned(),
        ));
    }
    Ok(())
}

fn reject_credential_shaped_identifier(value: &str) -> Result<(), ServiceError> {
    let lower = value.to_ascii_lowercase();
    let credential_prefixes = [
        "hf_",
        "sk-",
        "sk_",
        "ghp_",
        "github_pat_",
        "xox",
        "glpat-",
        "bearer-",
        "bearer_",
        "bearer.",
        "authorization-",
        "authorization_",
        "api-key-",
        "api_key_",
        "access-token-",
        "access_token_",
    ];
    let segments = lower.split(['/', ':']);
    if segments.clone().any(|segment| {
        credential_prefixes
            .iter()
            .any(|prefix| segment.starts_with(prefix))
    }) {
        return Err(ServiceError::InvalidRequest(
            "the model identifier resembles credential material and was rejected".to_owned(),
        ));
    }

    let jwt = Regex::new(r"^[A-Za-z0-9_-]{8,}\.[A-Za-z0-9_-]{8,}\.[A-Za-z0-9_-]{8,}$")
        .expect("constant JWT expression");
    let aws_access_id =
        Regex::new(r"^(?:AKIA|ASIA)[A-Z0-9]{16}$").expect("constant AWS access ID expression");
    if value
        .split(['/', ':'])
        .any(|segment| jwt.is_match(segment) || aws_access_id.is_match(segment))
    {
        return Err(ServiceError::InvalidRequest(
            "the model identifier resembles credential material and was rejected".to_owned(),
        ));
    }
    Ok(())
}

async fn write_marker(root: &Path, marker: OwnershipMarker) -> Result<(), ServiceError> {
    let payload = serde_json::to_vec_pretty(&marker)
        .map_err(|error| ServiceError::Internal(error.to_string()))?;
    let temporary = root.join(".audiobookai-owned.json.tmp");
    let destination = root.join(".audiobookai-owned.json");
    if tokio::fs::symlink_metadata(&temporary).await.is_ok() {
        tokio::fs::remove_file(&temporary).await?;
    }
    let mut file = tokio::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&temporary)
        .await?;
    file.write_all(&payload).await?;
    file.sync_all().await?;
    drop(file);
    tokio::fs::rename(&temporary, &destination).await?;
    sync_directory(root).await?;
    Ok(())
}

#[cfg(unix)]
async fn sync_directory(path: &Path) -> Result<(), ServiceError> {
    tokio::fs::File::open(path).await?.sync_all().await?;
    Ok(())
}

#[cfg(not(unix))]
async fn sync_directory(_path: &Path) -> Result<(), ServiceError> {
    Ok(())
}

async fn read_marker(root: &Path) -> Result<OwnershipMarker, ServiceError> {
    let metadata = tokio::fs::symlink_metadata(root).await?;
    if !metadata.is_dir() || metadata.file_type().is_symlink() {
        return Err(ServiceError::Conflict(
            "app-owned MLX path is not a safe directory".to_owned(),
        ));
    }
    let payload = tokio::fs::read(root.join(".audiobookai-owned.json")).await?;
    serde_json::from_slice(&payload)
        .map_err(|_| ServiceError::Conflict("app-owned MLX marker is invalid".to_owned()))
}

async fn marker_matches(root: &Path, kind: &str, id: &str) -> bool {
    read_marker(root).await.is_ok_and(|marker| {
        marker.schema_version == 1
            && marker.owner == OWNER
            && marker.kind == kind
            && marker.id == id
    })
}

async fn canonical_owned_child(
    root: &Path,
    expected_parent: &Path,
) -> Result<PathBuf, ServiceError> {
    let root_metadata = tokio::fs::symlink_metadata(root).await?;
    if !root_metadata.is_dir() || root_metadata.file_type().is_symlink() {
        return Err(ServiceError::Conflict(
            "refusing to remove an unsafe app-owned path".to_owned(),
        ));
    }
    let canonical_parent = tokio::fs::canonicalize(expected_parent).await?;
    let canonical_root = tokio::fs::canonicalize(root).await?;
    if canonical_root.parent() != Some(canonical_parent.as_path()) {
        return Err(ServiceError::Conflict(
            "refusing to remove a path outside its managed parent".to_owned(),
        ));
    }
    Ok(canonical_root)
}

async fn remove_owned_tree(
    root: &Path,
    expected_parent: &Path,
    kind: &str,
    id: &str,
) -> Result<(), ServiceError> {
    if !marker_matches(root, kind, id).await {
        return Err(ServiceError::Conflict(
            "refusing to remove a directory without the matching AudiobookAI ownership marker"
                .to_owned(),
        ));
    }
    let canonical_root = canonical_owned_child(root, expected_parent).await?;
    if !marker_matches(&canonical_root, kind, id).await {
        return Err(ServiceError::Conflict(
            "refusing to remove a directory whose ownership changed during verification".to_owned(),
        ));
    }
    tokio::fs::remove_dir_all(&canonical_root).await?;
    sync_directory(expected_parent).await?;
    Ok(())
}

async fn recover_abandoned_owned_root(
    root: &Path,
    expected_parent: &Path,
    kind: &str,
    id: &str,
) -> Result<bool, ServiceError> {
    let canonical_root = canonical_owned_child(root, expected_parent).await?;

    let mut entries = tokio::fs::read_dir(&canonical_root).await?;
    let Some(entry) = entries.next_entry().await? else {
        tokio::fs::remove_dir(&canonical_root).await?;
        sync_directory(expected_parent).await?;
        return Ok(true);
    };
    if entries.next_entry().await?.is_some()
        || entry.file_name() != std::ffi::OsStr::new(".audiobookai-owned.json.tmp")
    {
        return Ok(false);
    }
    let temporary_metadata = tokio::fs::symlink_metadata(entry.path()).await?;
    if !temporary_metadata.is_file()
        || temporary_metadata.file_type().is_symlink()
        || temporary_metadata.len() > 64 * 1024
    {
        return Ok(false);
    }
    let payload = tokio::fs::read(entry.path()).await?;
    let Ok(marker) = serde_json::from_slice::<OwnershipMarker>(&payload) else {
        return Ok(false);
    };
    if marker.schema_version != 1
        || marker.owner != OWNER
        || marker.kind != kind
        || marker.id != id
        || marker.completed_at.is_some()
    {
        return Ok(false);
    }
    tokio::fs::remove_dir_all(&canonical_root).await?;
    sync_directory(expected_parent).await?;
    Ok(true)
}

async fn validate_model_tree(root: &Path) -> Result<u64, ServiceError> {
    let mut total = 0_u64;
    let mut files = 0_u64;
    let mut pending = vec![root.to_path_buf()];
    while let Some(directory) = pending.pop() {
        let mut entries = tokio::fs::read_dir(directory).await?;
        while let Some(entry) = entries.next_entry().await? {
            let metadata = tokio::fs::symlink_metadata(entry.path()).await?;
            if metadata.file_type().is_symlink() {
                return Err(ServiceError::Conflict(
                    "downloaded model contains an unsupported symbolic link".to_owned(),
                ));
            }
            if metadata.is_dir() {
                pending.push(entry.path());
            } else if metadata.is_file() {
                files += 1;
                total = total.checked_add(metadata.len()).ok_or_else(|| {
                    ServiceError::Conflict("downloaded model size overflow".to_owned())
                })?;
            }
        }
    }
    if files == 0 {
        return Err(ServiceError::Conflict(
            "downloaded model directory contains no model files".to_owned(),
        ));
    }
    Ok(total)
}

async fn resolve_hf_commit(root: &Path, requested_revision: &str) -> Option<String> {
    let commit =
        Regex::new(r"^[0-9a-fA-F]{40}(?:[0-9a-fA-F]{24})?$").expect("constant commit expression");
    if commit.is_match(requested_revision) {
        return Some(requested_revision.to_ascii_lowercase());
    }

    let mut pending = vec![root.join(".cache").join("huggingface").join("download")];
    let mut resolved: Option<String> = None;
    while let Some(directory) = pending.pop() {
        let Ok(mut entries) = tokio::fs::read_dir(directory).await else {
            continue;
        };
        while let Ok(Some(entry)) = entries.next_entry().await {
            let Ok(metadata) = tokio::fs::symlink_metadata(entry.path()).await else {
                continue;
            };
            if metadata.is_dir() {
                pending.push(entry.path());
                continue;
            }
            if !metadata.is_file() || metadata.len() > 4096 {
                continue;
            }
            let Ok(payload) = tokio::fs::read_to_string(entry.path()).await else {
                continue;
            };
            for line in payload.lines().take(3) {
                if !commit.is_match(line) {
                    continue;
                }
                let candidate = line.to_ascii_lowercase();
                if resolved.as_ref().is_some_and(|value| value != &candidate) {
                    return None;
                }
                resolved = Some(candidate);
            }
        }
    }
    resolved
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fmt::Write as _;

    #[derive(Debug, Default)]
    struct FakeRunner {
        specs: Mutex<Vec<CommandSpec>>,
        include_reserved_payload_name: bool,
        invalid_installed_metadata: bool,
    }

    #[derive(Debug, Default)]
    struct SanitizingFailureRunner {
        specs: Mutex<Vec<CommandSpec>>,
    }

    #[async_trait]
    impl ToolRunner for SanitizingFailureRunner {
        async fn run(
            &self,
            spec: CommandSpec,
            _cancel: Arc<AtomicBool>,
        ) -> Result<ToolRunReport, ToolRunError> {
            self.specs.lock().await.push(spec);
            let synthetic_credential = ["s", "k-", "synthetic-do-not-retain"].concat();
            let raw = format!(
                "Resolved 2 packages in 1ms\nerror: hash mismatch at /private/runtime with {synthetic_credential}\n"
            );
            let mut diagnostics = Vec::new();
            append_sanitized_tool_output(&mut diagnostics, raw.as_bytes());
            Err(ToolRunError::Failed(ToolRunReport {
                exit_code: Some(23),
                diagnostics,
            }))
        }
    }

    #[async_trait]
    impl ToolRunner for FakeRunner {
        async fn run(
            &self,
            spec: CommandSpec,
            cancel: Arc<AtomicBool>,
        ) -> Result<ToolRunReport, ToolRunError> {
            if cancel.load(Ordering::Acquire) {
                return Err(ToolRunError::Cancelled(ToolRunReport::default()));
            }
            let arguments = spec
                .arguments
                .iter()
                .map(|argument| argument.to_string_lossy().into_owned())
                .collect::<Vec<_>>();
            if arguments.first().map(String::as_str) == Some("venv") {
                let venv = arguments
                    .last()
                    .map(PathBuf::from)
                    .ok_or_else(ToolRunError::failed)?;
                let bin = venv.join("bin");
                tokio::fs::create_dir_all(&bin)
                    .await
                    .map_err(|_| ToolRunError::failed())?;
                write_test_executable(&bin.join("python3"), b"bundled python venv")
                    .await
                    .map_err(|_| ToolRunError::failed())?;
            } else if arguments.first().map(String::as_str) == Some("pip") {
                let python = arguments
                    .iter()
                    .position(|argument| argument == "--python")
                    .and_then(|index| arguments.get(index + 1))
                    .map(PathBuf::from)
                    .ok_or_else(ToolRunError::failed)?;
                let bin = python.parent().ok_or_else(ToolRunError::failed)?;
                tokio::fs::write(bin.join("mlx_audio.server"), b"dummy executable")
                    .await
                    .map_err(|_| ToolRunError::failed())?;
                tokio::fs::write(bin.join("hf"), b"dummy executable")
                    .await
                    .map_err(|_| ToolRunError::failed())?;
                make_test_executable(&bin.join("mlx_audio.server"))
                    .await
                    .map_err(|_| ToolRunError::failed())?;
                make_test_executable(&bin.join("hf"))
                    .await
                    .map_err(|_| ToolRunError::failed())?;
                let venv = bin.parent().ok_or_else(ToolRunError::failed)?;
                let dist_info = venv.join("lib/python3.12/site-packages/mlx_audio-0.4.6.dist-info");
                tokio::fs::create_dir_all(&dist_info)
                    .await
                    .map_err(|_| ToolRunError::failed())?;
                tokio::fs::write(
                    dist_info.join("METADATA"),
                    if self.invalid_installed_metadata {
                        b"Metadata-Version: 2.4\nName: mlx-audio\nVersion: 9.9.9\n"
                    } else {
                        b"Metadata-Version: 2.4\nName: mlx-audio\nVersion: 0.4.6\n"
                    },
                )
                .await
                .map_err(|_| ToolRunError::failed())?;
                tokio::fs::write(dist_info.join("RECORD"), b"verified fixture\n")
                    .await
                    .map_err(|_| ToolRunError::failed())?;
            } else {
                let output = arguments
                    .iter()
                    .position(|argument| argument == "--local-dir")
                    .and_then(|index| arguments.get(index + 1))
                    .map(PathBuf::from)
                    .ok_or_else(ToolRunError::failed)?;
                tokio::fs::write(output.join("weights.safetensors"), b"dummy model")
                    .await
                    .map_err(|_| ToolRunError::failed())?;
                if self.include_reserved_payload_name {
                    tokio::fs::write(
                        output.join(".audiobookai-owned.json"),
                        b"public repository payload, not an ownership marker",
                    )
                    .await
                    .map_err(|_| ToolRunError::failed())?;
                }
            }
            self.specs.lock().await.push(spec);
            Ok(ToolRunReport {
                exit_code: Some(0),
                diagnostics: Vec::new(),
            })
        }
    }

    async fn write_test_executable(path: &Path, contents: &[u8]) -> std::io::Result<()> {
        tokio::fs::write(path, contents).await?;
        make_test_executable(path).await
    }

    async fn make_test_executable(path: &Path) -> std::io::Result<()> {
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            let mut permissions = tokio::fs::metadata(path).await?.permissions();
            permissions.set_mode(0o755);
            tokio::fs::set_permissions(path, permissions).await?;
        }
        Ok(())
    }

    async fn create_offline_installer_fixture(root: &Path) -> PathBuf {
        let sidecars = root.join("sidecars");
        let bin = sidecars.join("bin");
        let installer = sidecars.join(INSTALLER_ROOT);
        let wheelhouse = installer.join(WHEELHOUSE);
        tokio::fs::create_dir_all(&bin).await.expect("sidecar bin");
        tokio::fs::create_dir_all(installer.join("python/bin"))
            .await
            .expect("bundled Python root");
        tokio::fs::create_dir_all(&wheelhouse)
            .await
            .expect("wheelhouse");
        write_test_executable(&bin.join("uv"), b"uv 0.12.1 fixture")
            .await
            .expect("uv fixture");
        write_test_executable(&installer.join(BUNDLED_PYTHON), b"CPython 3.12.12 fixture")
            .await
            .expect("Python fixture");

        let wheels = [
            (
                "mlx-audio",
                "0.4.6",
                "mlx_audio-0.4.6-py3-none-any.whl",
                b"mlx wheel".as_slice(),
            ),
            (
                "huggingface-hub",
                "1.0.0",
                "huggingface_hub-1.0.0-py3-none-any.whl",
                b"huggingface wheel".as_slice(),
            ),
        ];
        let mut artifacts = Vec::new();
        let mut requirements = String::new();
        for (package, version, filename, contents) in wheels {
            tokio::fs::write(wheelhouse.join(filename), contents)
                .await
                .expect("wheel fixture");
            let hash = sha256_bytes(contents);
            let requirement_name = if package == "mlx-audio" {
                "mlx-audio[tts,server]"
            } else {
                package
            };
            writeln!(
                &mut requirements,
                "{requirement_name}=={version} --hash=sha256:{hash}"
            )
            .expect("writing to a String cannot fail");
            artifacts.push(serde_json::json!({
                "package": package,
                "version": version,
                "filename": filename,
                "url": format!("https://files.example.test/{filename}"),
                "sha256": hash,
            }));
        }
        tokio::fs::write(installer.join(REQUIREMENTS_LOCK), requirements)
            .await
            .expect("requirements fixture");
        tokio::fs::write(
            installer.join(INSTALLER_LOCK),
            serde_json::to_vec(&serde_json::json!({
                "schemaVersion": 1,
                "package": "mlx-audio[tts,server]",
                "version": "0.4.6",
                "target": "aarch64-apple-darwin",
                "pythonVersion": "3.12.12",
                "completeTransitiveClosure": true,
                "artifacts": artifacts,
            }))
            .expect("installer lock payload"),
        )
        .await
        .expect("installer lock fixture");
        bin
    }

    async fn wait_for_idle(manager: &MlxManager) {
        for _ in 0..100 {
            if manager.view().await.active_operation.is_none() {
                return;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        panic!("fake managed operation did not finish");
    }

    // Keeping the two command specifications in one test makes the no-network and
    // no-ambient-Python contract reviewable as a single atomic assertion.
    #[allow(clippy::too_many_lines)]
    #[tokio::test]
    async fn installs_exact_version_with_isolated_credential_free_environment() {
        let temporary = tempfile::tempdir().expect("temporary directory");
        let sidecar_bin = create_offline_installer_fixture(temporary.path()).await;
        validate_installer_payload(&sidecar_bin, true)
            .await
            .expect("complete offline fixture");
        let runner = Arc::new(FakeRunner::default());
        let manager = MlxManager::for_test(
            temporary.path().join("management"),
            sidecar_bin.clone(),
            runner.clone(),
        )
        .await
        .expect("manager");
        manager.start_install().await.expect("start install");
        wait_for_idle(&manager).await;

        let view = manager.view().await;
        assert!(view.installed);
        let specs = runner.specs.lock().await;
        assert_eq!(specs.len(), 2);
        let venv_arguments = specs[0]
            .arguments
            .iter()
            .map(|argument| argument.to_string_lossy().into_owned())
            .collect::<Vec<_>>();
        assert_eq!(venv_arguments[0], "venv");
        assert!(
            venv_arguments
                .iter()
                .any(|argument| argument == "--offline")
        );
        assert!(
            venv_arguments
                .iter()
                .any(|argument| argument == "--no-python-downloads")
        );
        let bundled_python = sidecar_bin
            .parent()
            .expect("sidecar root")
            .join(INSTALLER_ROOT)
            .join(BUNDLED_PYTHON)
            .canonicalize()
            .expect("canonical Python fixture");
        let python_index = venv_arguments
            .iter()
            .position(|argument| argument == "--python")
            .expect("explicit Python argument");
        assert_eq!(Path::new(&venv_arguments[python_index + 1]), bundled_python);

        let install_arguments = specs[1]
            .arguments
            .iter()
            .map(|argument| argument.to_string_lossy().into_owned())
            .collect::<Vec<_>>();
        for required in [
            "--offline",
            "--no-index",
            "--no-sources",
            "--no-python-downloads",
            "--require-hashes",
            "--only-binary",
            ":all:",
            "--find-links",
            "--requirements",
        ] {
            assert!(
                install_arguments
                    .iter()
                    .any(|argument| argument == required)
            );
        }
        assert!(!install_arguments.iter().any(|argument| {
            argument.starts_with("http://")
                || argument.starts_with("https://")
                || argument == "mlx-audio[tts,server]==0.4.6"
        }));
        for spec in specs.iter() {
            for (key, value) in [
                ("UV_NO_CONFIG", "1"),
                ("UV_OFFLINE", "1"),
                ("UV_NO_INDEX", "1"),
                ("UV_NO_SOURCES", "1"),
                ("UV_REQUIRE_HASHES", "1"),
                ("UV_PYTHON_DOWNLOADS", "never"),
                ("PIP_NO_INDEX", "1"),
            ] {
                assert_eq!(
                    spec.environment.get(std::ffi::OsStr::new(key)),
                    Some(&OsString::from(value))
                );
            }
            assert!(!spec.environment.keys().any(|key| {
                matches!(
                    key.to_string_lossy().as_ref(),
                    "PATH"
                        | "PYTHONPATH"
                        | "HF_TOKEN"
                        | "HUGGING_FACE_HUB_TOKEN"
                        | "UV_PASSWORD"
                        | "UV_CLIENT_SECRET"
                )
            }));
        }
    }

    #[tokio::test]
    async fn corrupt_or_incomplete_payload_fails_before_runtime_mutation() {
        let temporary = tempfile::tempdir().expect("temporary directory");
        let sidecar_bin = create_offline_installer_fixture(temporary.path()).await;
        let wheel = sidecar_bin
            .parent()
            .expect("sidecar root")
            .join(INSTALLER_ROOT)
            .join(WHEELHOUSE)
            .join("mlx_audio-0.4.6-py3-none-any.whl");
        tokio::fs::write(wheel, b"corrupted after release verification")
            .await
            .expect("corrupt wheel fixture");
        let runner = Arc::new(FakeRunner::default());
        let management_root = temporary.path().join("management");
        let manager = MlxManager::for_test(management_root.clone(), sidecar_bin, runner.clone())
            .await
            .expect("manager");

        assert!(manager.view().await.installer_payload_available);
        assert!(manager.start_install().await.is_err());
        assert!(!management_root.join("runtime").exists());
        assert!(runner.specs.lock().await.is_empty());
        assert!(manager.view().await.active_operation.is_none());
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn symlinked_bundled_input_is_rejected_without_execution() {
        use std::os::unix::fs::symlink;

        let temporary = tempfile::tempdir().expect("temporary directory");
        let sidecar_bin = create_offline_installer_fixture(temporary.path()).await;
        let installer = sidecar_bin
            .parent()
            .expect("sidecar root")
            .join(INSTALLER_ROOT);
        let requirements = installer.join(REQUIREMENTS_LOCK);
        let external = temporary.path().join("external-requirements.lock");
        tokio::fs::rename(&requirements, &external)
            .await
            .expect("move requirements fixture");
        symlink(&external, &requirements).expect("symlink fixture");
        let runner = Arc::new(FakeRunner::default());
        let management_root = temporary.path().join("management");
        let manager = MlxManager::for_test(management_root.clone(), sidecar_bin, runner.clone())
            .await
            .expect("manager");

        let view = manager.view().await;
        assert!(!view.installer_payload_available);
        assert!(view.support_detail.contains("filesystem safety"));
        assert!(manager.start_install().await.is_err());
        assert!(!management_root.join("runtime").exists());
        assert!(runner.specs.lock().await.is_empty());
    }

    #[tokio::test]
    async fn installed_dist_info_must_confirm_exact_mlx_audio_version() {
        let temporary = tempfile::tempdir().expect("temporary directory");
        let sidecar_bin = create_offline_installer_fixture(temporary.path()).await;
        let runner = Arc::new(FakeRunner {
            specs: Mutex::new(Vec::new()),
            include_reserved_payload_name: false,
            invalid_installed_metadata: true,
        });
        let management_root = temporary.path().join("management");
        let manager = MlxManager::for_test(management_root.clone(), sidecar_bin, runner)
            .await
            .expect("manager");
        manager.start_install().await.expect("start install");
        wait_for_idle(&manager).await;

        let view = manager.view().await;
        assert!(!view.installed);
        assert!(!management_root.join("runtime").exists());
        let operation = view.last_operation.expect("failed install operation");
        assert_eq!(operation.state, MlxOperationState::Failed);
        assert!(
            operation
                .diagnostics
                .iter()
                .any(|line| line.contains("did not match version 0.4.6"))
        );
    }

    #[tokio::test]
    async fn operation_diagnostics_keep_only_allowlisted_output_and_exit_status() {
        let temporary = tempfile::tempdir().expect("temporary directory");
        let sidecar_bin = create_offline_installer_fixture(temporary.path()).await;
        let runner = Arc::new(SanitizingFailureRunner::default());
        let manager =
            MlxManager::for_test(temporary.path().join("management"), sidecar_bin, runner)
                .await
                .expect("manager");
        manager.start_install().await.expect("start install");
        wait_for_idle(&manager).await;

        let operation = manager
            .view()
            .await
            .last_operation
            .expect("failed operation");
        assert_eq!(operation.exit_code, Some(23));
        assert_eq!(
            operation.diagnostics,
            [
                "Resolved 2 packages offline.",
                "The bundled artifact failed hash verification."
            ]
        );
        let serialized = serde_json::to_string(&operation).expect("operation JSON");
        for forbidden in ["/private/runtime", "synthetic-do-not-retain", "--python"] {
            assert!(!serialized.contains(forbidden));
        }
    }

    #[tokio::test]
    async fn downloads_public_model_and_uninstall_retains_it() {
        let temporary = tempfile::tempdir().expect("temporary directory");
        let sidecar_bin = create_offline_installer_fixture(temporary.path()).await;
        let manager = MlxManager::for_test(
            temporary.path().join("management"),
            sidecar_bin,
            Arc::new(FakeRunner::default()),
        )
        .await
        .expect("manager");
        manager.start_install().await.expect("install");
        wait_for_idle(&manager).await;
        let operation = manager
            .start_model_download("mlx-community/test-model".to_owned(), "main".to_owned())
            .await
            .expect("model download");
        wait_for_idle(&manager).await;
        let model_id = operation.model_id.expect("model id");
        let model_path = manager.model_root(model_id);
        let payload_path = model_path.join("payload");
        assert!(payload_path.join("weights.safetensors").is_file());
        assert_eq!(manager.view().await.models[0].local_path, payload_path);

        manager.start_uninstall().await.expect("uninstall");
        wait_for_idle(&manager).await;
        assert!(!manager.view().await.installed);
        assert!(model_path.is_dir());
        manager.remove_model(model_id).await.expect("remove model");
        assert!(!model_path.exists());
    }

    #[tokio::test]
    async fn repository_reserved_filename_cannot_replace_the_ownership_marker() {
        let temporary = tempfile::tempdir().expect("temporary directory");
        let sidecar_bin = create_offline_installer_fixture(temporary.path()).await;
        let runner = Arc::new(FakeRunner {
            specs: Mutex::new(Vec::new()),
            include_reserved_payload_name: true,
            invalid_installed_metadata: false,
        });
        let manager =
            MlxManager::for_test(temporary.path().join("management"), sidecar_bin, runner)
                .await
                .expect("manager");
        manager.start_install().await.expect("install");
        wait_for_idle(&manager).await;
        let operation = manager
            .start_model_download("owner/public-model".to_owned(), "main".to_owned())
            .await
            .expect("model download");
        wait_for_idle(&manager).await;

        let model_id = operation.model_id.expect("model id");
        let root = manager.model_root(model_id);
        assert!(root.join("payload/.audiobookai-owned.json").is_file());
        assert!(marker_matches(&root, MODEL_KIND, &model_id.to_string()).await);
        manager
            .remove_model(model_id)
            .await
            .expect("remove model with reserved payload filename");
        assert!(!root.exists());
    }

    #[tokio::test]
    async fn hydrate_recovers_only_safe_pre_marker_crash_roots() {
        let temporary = tempfile::tempdir().expect("temporary directory");
        let root = temporary.path().join("management");
        let models = root.join("models");
        tokio::fs::create_dir_all(&models)
            .await
            .expect("models fixture");

        let empty_id = Uuid::new_v4();
        let empty_root = models.join(empty_id.to_string());
        tokio::fs::create_dir(&empty_root)
            .await
            .expect("empty crash root");

        let temporary_marker_id = Uuid::new_v4();
        let temporary_marker_root = models.join(temporary_marker_id.to_string());
        tokio::fs::create_dir(&temporary_marker_root)
            .await
            .expect("temporary marker root");
        let marker = OwnershipMarker {
            schema_version: 1,
            owner: OWNER.to_owned(),
            kind: MODEL_KIND.to_owned(),
            id: temporary_marker_id.to_string(),
            version: Some(MLX_AUDIO_VERSION.to_owned()),
            repository: Some("owner/public-model".to_owned()),
            revision: Some("main".to_owned()),
            resolved_commit: None,
            python_version: None,
            installer_lock_sha256: None,
            completed_at: None,
            created_at: Utc::now(),
        };
        tokio::fs::write(
            temporary_marker_root.join(".audiobookai-owned.json.tmp"),
            serde_json::to_vec(&marker).expect("marker payload"),
        )
        .await
        .expect("temporary marker fixture");

        let foreign_id = Uuid::new_v4();
        let foreign_root = models.join(foreign_id.to_string());
        tokio::fs::create_dir(&foreign_root)
            .await
            .expect("foreign root");
        tokio::fs::write(foreign_root.join("keep.txt"), b"not app-owned")
            .await
            .expect("foreign data");

        let sidecar_bin = create_offline_installer_fixture(temporary.path()).await;
        let _manager = MlxManager::for_test(root, sidecar_bin, Arc::new(FakeRunner::default()))
            .await
            .expect("manager");

        assert!(!empty_root.exists());
        assert!(!temporary_marker_root.exists());
        assert!(foreign_root.join("keep.txt").is_file());
    }

    #[tokio::test]
    async fn ownership_marker_never_authorizes_deletion_outside_managed_parent() {
        let temporary = tempfile::tempdir().expect("temporary directory");
        let managed_parent = temporary.path().join("managed-models");
        let external_parent = temporary.path().join("external-models");
        tokio::fs::create_dir_all(&managed_parent)
            .await
            .expect("managed parent");
        tokio::fs::create_dir_all(&external_parent)
            .await
            .expect("external parent");
        let id = Uuid::new_v4();
        let external_root = external_parent.join(id.to_string());
        tokio::fs::create_dir(&external_root)
            .await
            .expect("external root");
        write_marker(
            &external_root,
            OwnershipMarker {
                schema_version: 1,
                owner: OWNER.to_owned(),
                kind: MODEL_KIND.to_owned(),
                id: id.to_string(),
                version: Some(MLX_AUDIO_VERSION.to_owned()),
                repository: Some("owner/public-model".to_owned()),
                revision: Some("main".to_owned()),
                resolved_commit: None,
                python_version: None,
                installer_lock_sha256: None,
                completed_at: None,
                created_at: Utc::now(),
            },
        )
        .await
        .expect("ownership fixture");

        assert!(
            remove_owned_tree(&external_root, &managed_parent, MODEL_KIND, &id.to_string())
                .await
                .is_err()
        );
        assert!(external_root.is_dir());
    }

    #[tokio::test]
    async fn interrupted_runtime_and_model_are_never_hydrated_as_ready() {
        let temporary = tempfile::tempdir().expect("temporary directory");
        let root = temporary.path().join("management");
        let runtime = root.join("runtime");
        tokio::fs::create_dir_all(runtime.join("bin"))
            .await
            .expect("runtime fixture");
        tokio::fs::write(runtime.join("bin/mlx_audio.server"), b"partial")
            .await
            .expect("partial server");
        write_marker(
            &runtime,
            OwnershipMarker {
                schema_version: 1,
                owner: OWNER.to_owned(),
                kind: RUNTIME_KIND.to_owned(),
                id: "runtime".to_owned(),
                version: Some(MLX_AUDIO_VERSION.to_owned()),
                repository: None,
                revision: None,
                resolved_commit: None,
                python_version: Some("3.12.12".to_owned()),
                installer_lock_sha256: Some("0".repeat(64)),
                completed_at: None,
                created_at: Utc::now(),
            },
        )
        .await
        .expect("runtime marker");

        let model_id = Uuid::new_v4();
        let model = root.join("models").join(model_id.to_string());
        tokio::fs::create_dir_all(model.join("payload"))
            .await
            .expect("model fixture");
        tokio::fs::write(model.join("payload/partial.safetensors"), b"partial")
            .await
            .expect("partial model");
        write_marker(
            &model,
            OwnershipMarker {
                schema_version: 1,
                owner: OWNER.to_owned(),
                kind: MODEL_KIND.to_owned(),
                id: model_id.to_string(),
                version: Some(MLX_AUDIO_VERSION.to_owned()),
                repository: Some("owner/public-model".to_owned()),
                revision: Some("main".to_owned()),
                resolved_commit: None,
                python_version: None,
                installer_lock_sha256: None,
                completed_at: None,
                created_at: Utc::now(),
            },
        )
        .await
        .expect("model marker");

        let sidecar_bin = create_offline_installer_fixture(temporary.path()).await;
        let manager = MlxManager::for_test(root, sidecar_bin, Arc::new(FakeRunner::default()))
            .await
            .expect("manager");
        let view = manager.view().await;
        assert!(!view.installed);
        assert_eq!(view.models.len(), 1);
        assert_eq!(view.models[0].state, MlxModelState::Failed);
        manager
            .remove_model(model_id)
            .await
            .expect("remove incomplete owned model");
    }

    #[test]
    fn rejects_private_or_ambiguous_model_identifiers() {
        assert!(validate_repository("owner/model").is_ok());
        assert!(validate_repository("owner/../model").is_err());
        assert!(validate_repository("https://example.test/model").is_err());
        assert!(validate_revision("refs/pr/1").is_ok());
        assert!(validate_revision("../private").is_err());
    }

    #[test]
    fn rejects_credential_shaped_repository_and_revision_values() {
        let fixtures = [
            ["h", "f_", "synthetic_value_only"].concat(),
            ["s", "k-", "synthetic_value_only"].concat(),
            ["gh", "p_", "synthetic_value_only"].concat(),
            ["github", "_pat_", "synthetic_value_only"].concat(),
            ["xo", "xb-", "synthetic_value_only"].concat(),
            ["AK", "IA", "0000000000000000"].concat(),
            ["eyJsynthetic", "payload000", "signature0"].join("."),
            ["Bearer", "-synthetic-value"].concat(),
        ];
        for fixture in fixtures {
            assert!(validate_revision(&fixture).is_err());
            assert!(validate_repository(&format!("owner/{fixture}")).is_err());
        }
    }
}
