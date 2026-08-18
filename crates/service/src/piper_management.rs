use std::{
    collections::{BTreeMap, BTreeSet},
    fs::OpenOptions,
    io::{Read as _, Seek as _, Write as _},
    path::{Component, Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};

use chrono::{DateTime, Utc};
use futures::StreamExt as _;
use serde::{Deserialize, Serialize};
use sha2::{Digest as _, Sha256};
use tokio::{io::AsyncWriteExt as _, sync::Mutex};
use uuid::Uuid;

use crate::{ServiceConfig, ServiceError};

pub const PIPER_VERSION: &str = "1.2.0";
pub const PIPER_ARCHIVE_URL: &str =
    "https://github.com/rhasspy/piper/releases/download/v1.2.0/piper_amd64.tar.gz";
pub const PIPER_ARCHIVE_SHA256: &str =
    "467c17935d2a22dcce9dc9e08ba07485e29be813097e7cf08c5627aa09d32e42";
pub const PIPER_VOICES_REVISION: &str = "f5a6e9094787fd865d65cb024472f977f9c542b5";

const PIPER_ARCHIVE_BYTES: u64 = 25_916_047;
const OWNER: &str = "AudiobookAI";
const ENGINE_KIND: &str = "piper-engine";
const VOICE_KIND: &str = "piper-voice";
const VOICE_LICENSE_MARKER: &str = "source-dataset:CC0-1.0";
const MARKER_FILE: &str = ".audiobookai-owner.json";
const MAX_MARKER_BYTES: u64 = 32 * 1024;
const MAX_ENGINE_FILES: usize = 20_000;
const MAX_ENGINE_BYTES: u64 = 512 * 1024 * 1024;

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum PiperInstallerStatus {
    Ready,
    UnsupportedPlatform,
    NotBundled,
    PayloadMissing,
    UnsafeFilesystem,
    InvalidMetadata,
    Incomplete,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum PiperOperationKind {
    Install,
    Uninstall,
    DownloadVoice,
    RemoveVoice,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum PiperOperationState {
    Queued,
    Running,
    Cancelling,
    Succeeded,
    Failed,
    Cancelled,
}

impl PiperOperationState {
    const fn active(self) -> bool {
        matches!(self, Self::Queued | Self::Running | Self::Cancelling)
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PiperOperationView {
    pub id: Uuid,
    pub kind: PiperOperationKind,
    pub state: PiperOperationState,
    pub progress_percent: u8,
    pub phase: String,
    pub message: String,
    pub voice_id: Option<String>,
    pub bytes_downloaded: Option<u64>,
    pub bytes_total: Option<u64>,
    pub started_at: DateTime<Utc>,
    pub finished_at: Option<DateTime<Utc>>,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PiperCatalogVoiceView {
    pub id: &'static str,
    pub name: &'static str,
    pub language: &'static str,
    pub quality: &'static str,
    pub speakers: u16,
    pub sample_rate: u32,
    pub size_bytes: u64,
    pub license: &'static str,
    pub license_url: &'static str,
    pub license_summary: &'static str,
    pub model_card_url: String,
    pub source_url: &'static str,
    pub installed: bool,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PiperInstalledVoiceView {
    pub id: String,
    pub name: String,
    pub language: String,
    pub quality: String,
    pub model_path: PathBuf,
    pub config_path: PathBuf,
    pub size_bytes: u64,
    pub license: String,
    pub installed_at: DateTime<Utc>,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PiperVoiceIssueView {
    pub id: String,
    pub status: PiperInstallerStatus,
    pub removable: bool,
    pub detail: String,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PiperManagementView {
    pub supported: bool,
    pub support_detail: String,
    pub installer_status: PiperInstallerStatus,
    pub installed: bool,
    pub installed_version: Option<&'static str>,
    pub executable_path: Option<PathBuf>,
    pub voices_dir: Option<PathBuf>,
    pub catalog: Vec<PiperCatalogVoiceView>,
    pub installed_voices: Vec<PiperInstalledVoiceView>,
    pub voice_issues: Vec<PiperVoiceIssueView>,
    pub active_operation: Option<PiperOperationView>,
    pub last_operation: Option<PiperOperationView>,
    pub profile_action_required: bool,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
struct OwnershipMarker {
    schema_version: u8,
    owner: String,
    kind: String,
    id: String,
    version: String,
    #[serde(default)]
    revision: Option<String>,
    #[serde(default)]
    license: Option<String>,
    #[serde(default)]
    license_confirmed_at: Option<DateTime<Utc>>,
    created_at: DateTime<Utc>,
    completed_at: DateTime<Utc>,
}

#[derive(Clone, Debug)]
struct ActiveOperation {
    view: PiperOperationView,
    cancel: Arc<AtomicBool>,
}

#[derive(Debug, Default)]
struct ManagerState {
    installed: bool,
    engine_issue: Option<PiperInstallerStatus>,
    engine_owned: bool,
    voices: BTreeMap<String, PiperInstalledVoiceView>,
    voice_issues: BTreeMap<String, PiperInstallerStatus>,
    active: Option<ActiveOperation>,
    last: Option<PiperOperationView>,
    profile_action_required: bool,
}

#[derive(Clone)]
pub struct PiperManager {
    root: Arc<PathBuf>,
    supported: bool,
    installer_status: PiperInstallerStatus,
    support_detail: Arc<str>,
    http: reqwest::Client,
    state: Arc<Mutex<ManagerState>>,
    accepting_operations: Arc<AtomicBool>,
}

impl std::fmt::Debug for PiperManager {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("PiperManager")
            .field("root", &self.root)
            .field("supported", &self.supported)
            .field("installer_status", &self.installer_status)
            .finish_non_exhaustive()
    }
}

impl PiperManager {
    pub async fn initialize(config: &ServiceConfig) -> Result<Self, ServiceError> {
        let supported = cfg!(all(target_os = "linux", target_arch = "x86_64"));
        let (installer_status, support_detail) = if supported {
            (
                PiperInstallerStatus::Ready,
                "Piper can be installed into AudiobookAI's private application data directory.",
            )
        } else {
            (
                PiperInstallerStatus::UnsupportedPlatform,
                "Managed Piper is currently available on Linux x86_64 only.",
            )
        };
        let root = config.data_dir.join("managed-providers").join("piper");
        ensure_managed_directory(&root).await?;
        ensure_managed_directory(&root.join("voices")).await?;
        let http = reqwest::Client::builder()
            .https_only(true)
            .no_proxy()
            .redirect(reqwest::redirect::Policy::limited(5))
            .connect_timeout(Duration::from_secs(20))
            .timeout(Duration::from_mins(30))
            .user_agent(concat!("AudiobookAI/", env!("CARGO_PKG_VERSION")))
            .build()
            .map_err(|error| {
                ServiceError::Internal(format!("Piper downloader setup failed: {error}"))
            })?;
        let manager = Self {
            root: Arc::new(root),
            supported,
            installer_status,
            support_detail: Arc::from(support_detail),
            http,
            state: Arc::new(Mutex::new(ManagerState::default())),
            accepting_operations: Arc::new(AtomicBool::new(true)),
        };
        manager.hydrate().await?;
        Ok(manager)
    }

    pub fn executable_path(&self) -> PathBuf {
        self.engine_root().join("piper").join("piper")
    }

    pub fn voices_dir(&self) -> PathBuf {
        self.root.join("voices")
    }

    pub async fn view(&self) -> PiperManagementView {
        let state = self.state.lock().await;
        let installer_status = state.engine_issue.unwrap_or(self.installer_status);
        let support_detail = match state.engine_issue {
            Some(PiperInstallerStatus::Incomplete) => {
                "An incomplete app-owned Piper engine must be removed before it can be reinstalled."
            }
            Some(PiperInstallerStatus::UnsafeFilesystem) => {
                "The Piper engine path is not owned by AudiobookAI and will not be modified. Resolve that path manually before installing."
            }
            _ => self.support_detail.as_ref(),
        };
        PiperManagementView {
            supported: self.supported,
            support_detail: support_detail.to_owned(),
            installer_status,
            installed: state.installed,
            installed_version: state.installed.then_some(PIPER_VERSION),
            executable_path: state.installed.then(|| self.executable_path()),
            voices_dir: self.supported.then(|| self.voices_dir()),
            catalog: VOICES
                .iter()
                .map(|voice| voice.view(state.voices.contains_key(voice.id)))
                .collect(),
            installed_voices: state.voices.values().cloned().collect(),
            voice_issues: state
                .voice_issues
                .iter()
                .map(|(id, status)| voice_issue_view(id, *status))
                .collect(),
            active_operation: state.active.as_ref().map(|active| active.view.clone()),
            last_operation: state.last.clone(),
            profile_action_required: state.profile_action_required,
        }
    }

    pub async fn start_install(&self) -> Result<PiperOperationView, ServiceError> {
        self.ensure_ready()?;
        let state = self.state.lock().await;
        if state.installed {
            return Err(ServiceError::Conflict(
                "Piper is already installed".to_owned(),
            ));
        }
        if state.engine_issue.is_some() {
            return Err(ServiceError::Conflict(
                "resolve the existing Piper engine path before reinstalling".to_owned(),
            ));
        }
        drop(state);
        let operation = self
            .begin_operation(PiperOperationKind::Install, None)
            .await?;
        let manager = self.clone();
        tokio::spawn(async move { manager.run_install(operation.id).await });
        Ok(operation)
    }

    pub async fn start_uninstall(&self) -> Result<PiperOperationView, ServiceError> {
        self.ensure_supported()?;
        let state = self.state.lock().await;
        let removable_incomplete = state.engine_owned
            && matches!(state.engine_issue, Some(PiperInstallerStatus::Incomplete));
        if !state.installed && !removable_incomplete {
            return Err(ServiceError::Conflict("Piper is not installed".to_owned()));
        }
        drop(state);
        let operation = self
            .begin_operation(PiperOperationKind::Uninstall, None)
            .await?;
        let manager = self.clone();
        tokio::spawn(async move { manager.run_uninstall(operation.id).await });
        Ok(operation)
    }

    pub async fn start_voice_download(
        &self,
        voice_id: &str,
        license_confirmed: bool,
    ) -> Result<PiperOperationView, ServiceError> {
        self.ensure_ready()?;
        validate_voice_id(voice_id)?;
        if !license_confirmed {
            return Err(ServiceError::InvalidRequest(
                "the pinned voice license must be confirmed before downloading".to_owned(),
            ));
        }
        let voice = catalog_voice(voice_id).ok_or(ServiceError::NotFound)?;
        let state = self.state.lock().await;
        if !state.installed {
            return Err(ServiceError::Conflict(
                "install Piper before downloading a voice".to_owned(),
            ));
        }
        if state.voices.contains_key(voice_id) {
            return Err(ServiceError::Conflict(
                "the selected Piper voice is already installed".to_owned(),
            ));
        }
        if let Some(issue) = state.voice_issues.get(voice_id) {
            return Err(ServiceError::Conflict(match issue {
                PiperInstallerStatus::Incomplete => {
                    "remove the incomplete app-owned Piper voice before downloading it again"
                        .to_owned()
                }
                _ => "the Piper voice path is not owned by AudiobookAI and will not be overwritten"
                    .to_owned(),
            }));
        }
        drop(state);
        let operation = self
            .begin_operation(PiperOperationKind::DownloadVoice, Some(voice_id.to_owned()))
            .await?;
        let manager = self.clone();
        tokio::spawn(async move { manager.run_voice_download(operation.id, voice).await });
        Ok(operation)
    }

    pub async fn remove_voice(
        &self,
        voice_id: &str,
        confirmed: bool,
        in_use: bool,
    ) -> Result<(), ServiceError> {
        validate_voice_id(voice_id)?;
        if !confirmed {
            return Err(ServiceError::InvalidRequest(
                "explicit confirmation is required before removing a Piper voice".to_owned(),
            ));
        }
        if in_use {
            return Err(ServiceError::Conflict(
                "the Piper voice is still referenced by a provider, assignment, or active job"
                    .to_owned(),
            ));
        }
        {
            let state = self.state.lock().await;
            ensure_idle(&state)?;
            if !state.voices.contains_key(voice_id) {
                match state.voice_issues.get(voice_id) {
                    Some(PiperInstallerStatus::Incomplete) => {}
                    Some(_) => {
                        return Err(ServiceError::Conflict(
                            "the Piper voice path is not owned by AudiobookAI and will not be removed"
                                .to_owned(),
                        ));
                    }
                    None => return Err(ServiceError::NotFound),
                }
            }
        }
        remove_owned_tree(
            &self.voice_root(voice_id),
            &self.voices_dir(),
            VOICE_KIND,
            voice_id,
        )
        .await?;
        let mut state = self.state.lock().await;
        state.voices.remove(voice_id);
        state.voice_issues.remove(voice_id);
        Ok(())
    }

    pub async fn cancel(&self, operation_id: Uuid) -> Result<PiperOperationView, ServiceError> {
        let mut state = self.state.lock().await;
        let active = state.active.as_mut().ok_or(ServiceError::NotFound)?;
        if active.view.id != operation_id {
            return Err(ServiceError::NotFound);
        }
        active.cancel.store(true, Ordering::Release);
        active.view.state = PiperOperationState::Cancelling;
        "cancelling".clone_into(&mut active.view.phase);
        "Cancelling the app-owned Piper operation.".clone_into(&mut active.view.message);
        Ok(active.view.clone())
    }

    pub async fn shutdown_owned(&self) -> bool {
        self.accepting_operations.store(false, Ordering::Release);
        {
            let mut state = self.state.lock().await;
            let Some(active) = state.active.as_mut() else {
                return true;
            };
            active.cancel.store(true, Ordering::Release);
            active.view.state = PiperOperationState::Cancelling;
            "cancelling".clone_into(&mut active.view.phase);
            "Cancelling the Piper operation before shutdown.".clone_into(&mut active.view.message);
        }
        for _ in 0..100 {
            if self.state.lock().await.active.is_none() {
                return true;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        false
    }

    pub async fn set_profile_action_required(&self, required: bool) {
        self.state.lock().await.profile_action_required = required;
    }

    fn ensure_ready(&self) -> Result<(), ServiceError> {
        self.ensure_supported()?;
        if self.installer_status != PiperInstallerStatus::Ready {
            return Err(ServiceError::Conflict(
                "the managed Piper installer is unavailable".to_owned(),
            ));
        }
        Ok(())
    }

    fn ensure_supported(&self) -> Result<(), ServiceError> {
        if !self.supported {
            return Err(ServiceError::InvalidRequest(
                "managed Piper is supported only on Linux x86_64".to_owned(),
            ));
        }
        Ok(())
    }

    fn engine_root(&self) -> PathBuf {
        self.root.join("engine")
    }

    fn voice_root(&self, id: &str) -> PathBuf {
        self.voices_dir().join(id)
    }

    async fn hydrate(&self) -> Result<(), ServiceError> {
        let engine_root = self.engine_root();
        let (installed, engine_issue, engine_owned) = if validate_engine_install(&engine_root)
            .await
            .is_ok()
        {
            (true, None, true)
        } else {
            match tokio::fs::symlink_metadata(&engine_root).await {
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => (false, None, false),
                Ok(metadata) if metadata.is_dir() && !metadata.file_type().is_symlink() => {
                    let owned = read_marker(&engine_root)
                        .await
                        .is_ok_and(|marker| marker_owns(&marker, ENGINE_KIND, "engine"));
                    (
                        false,
                        Some(if owned {
                            PiperInstallerStatus::Incomplete
                        } else {
                            PiperInstallerStatus::UnsafeFilesystem
                        }),
                        owned,
                    )
                }
                Ok(_) | Err(_) => (false, Some(PiperInstallerStatus::UnsafeFilesystem), false),
            }
        };
        let hydrated_voices = hydrate_voices(&self.voices_dir()).await;
        let mut state = self.state.lock().await;
        state.installed = installed;
        state.engine_issue = engine_issue;
        state.engine_owned = engine_owned;
        state.voices = hydrated_voices.installed;
        state.voice_issues = hydrated_voices.issues;
        Ok(())
    }

    async fn begin_operation(
        &self,
        kind: PiperOperationKind,
        voice_id: Option<String>,
    ) -> Result<PiperOperationView, ServiceError> {
        if !self.accepting_operations.load(Ordering::Acquire) {
            return Err(ServiceError::Conflict(
                "Piper management is unavailable while AudiobookAI is shutting down".to_owned(),
            ));
        }
        let mut state = self.state.lock().await;
        ensure_idle(&state)?;
        let view = PiperOperationView {
            id: Uuid::new_v4(),
            kind,
            state: PiperOperationState::Queued,
            progress_percent: 0,
            phase: "queued".to_owned(),
            message: "The app-owned Piper operation is queued.".to_owned(),
            voice_id,
            bytes_downloaded: None,
            bytes_total: None,
            started_at: Utc::now(),
            finished_at: None,
        };
        state.active = Some(ActiveOperation {
            view: view.clone(),
            cancel: Arc::new(AtomicBool::new(false)),
        });
        Ok(view)
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

    async fn update_operation(&self, id: Uuid, progress: u8, phase: &str, message: &str) {
        let mut state = self.state.lock().await;
        if let Some(active) = state.active.as_mut().filter(|active| active.view.id == id) {
            if active.view.state != PiperOperationState::Cancelling {
                active.view.state = PiperOperationState::Running;
            }
            active.view.progress_percent = progress;
            phase.clone_into(&mut active.view.phase);
            message.clone_into(&mut active.view.message);
        }
    }

    async fn set_transfer(&self, id: Uuid, downloaded: u64, total: u64, start: u8, end: u8) {
        let mut state = self.state.lock().await;
        if let Some(active) = state.active.as_mut().filter(|active| active.view.id == id) {
            active.view.bytes_downloaded = Some(downloaded.min(total));
            active.view.bytes_total = Some(total);
            let span_percent = end.saturating_sub(start);
            let span = u64::from(span_percent);
            let relative = downloaded
                .min(total)
                .saturating_mul(span)
                .checked_div(total)
                .unwrap_or(span);
            active.view.progress_percent =
                start.saturating_add(u8::try_from(relative).unwrap_or(span_percent));
        }
    }

    async fn finish_operation(&self, id: Uuid, result: Result<(), OperationError>, success: &str) {
        let mut state = self.state.lock().await;
        let Some(mut active) = state.active.take().filter(|active| active.view.id == id) else {
            return;
        };
        active.view.finished_at = Some(Utc::now());
        match result {
            Ok(()) => {
                active.view.state = PiperOperationState::Succeeded;
                active.view.progress_percent = 100;
                "complete".clone_into(&mut active.view.phase);
                success.clone_into(&mut active.view.message);
            }
            Err(OperationError::Cancelled) => {
                active.view.state = PiperOperationState::Cancelled;
                "cancelled".clone_into(&mut active.view.phase);
                "The app-owned Piper operation was cancelled.".clone_into(&mut active.view.message);
            }
            Err(OperationError::Failed(message)) => {
                active.view.state = PiperOperationState::Failed;
                "failed".clone_into(&mut active.view.phase);
                message.clone_into(&mut active.view.message);
                tracing::warn!(
                    diagnostic_code = "piper.management.operation.failed",
                    operation_id = %active.view.id,
                    action = ?active.view.kind,
                    "Piper management operation failed"
                );
            }
        }
        state.last = Some(active.view);
    }

    async fn run_install(&self, id: Uuid) {
        let result = self.install(id).await;
        if result.is_ok() {
            let mut state = self.state.lock().await;
            state.installed = true;
            state.engine_issue = None;
            state.engine_owned = true;
        }
        self.finish_operation(id, result, "Piper 1.2.0 is installed locally.")
            .await;
    }

    async fn install(&self, id: Uuid) -> Result<(), OperationError> {
        self.update_operation(
            id,
            2,
            "preparing",
            "Preparing an app-owned Piper installation.",
        )
        .await;
        if self.engine_root().exists() {
            return Err(OperationError::Failed(
                "The Piper engine path already exists and was not overwritten.",
            ));
        }
        let work = tempfile::Builder::new()
            .prefix(".piper-install-")
            .tempdir_in(self.root.as_ref())
            .map_err(|_| {
                OperationError::Failed("A temporary Piper installation could not be created.")
            })?;
        let archive = work.path().join("piper_amd64.tar.gz");
        self.update_operation(
            id,
            5,
            "downloading",
            "Downloading the pinned Piper 1.2.0 archive.",
        )
        .await;
        self.download_verified(
            id,
            &ENGINE_ARCHIVE,
            &archive,
            DownloadProgress::new(5, 55, 0, PIPER_ARCHIVE_BYTES),
        )
        .await?;
        self.check_cancelled(id).await?;
        let extracted = work.path().join("extracted");
        tokio::fs::create_dir(&extracted).await.map_err(|_| {
            OperationError::Failed("The Piper extraction directory could not be created.")
        })?;
        self.update_operation(
            id,
            65,
            "extracting",
            "Extracting the verified Piper archive.",
        )
        .await;
        extract_archive(
            &archive,
            &extracted,
            self.active_cancel(id).await,
            ArchiveLimits::ENGINE,
        )
        .await?;
        self.update_operation(
            id,
            82,
            "validating",
            "Validating the isolated Piper runtime.",
        )
        .await;
        validate_extracted_root(&extracted).await?;
        let candidate = work.path().join("engine");
        tokio::fs::create_dir(&candidate)
            .await
            .map_err(|_| OperationError::Failed("The Piper runtime could not be staged."))?;
        tokio::fs::rename(extracted.join("piper"), candidate.join("piper"))
            .await
            .map_err(|_| OperationError::Failed("The Piper runtime could not be staged."))?;
        write_marker(&candidate, &OwnershipMarker::engine())
            .await
            .map_err(|_| {
                OperationError::Failed("Piper ownership metadata could not be written.")
            })?;
        validate_engine_install(&candidate)
            .await
            .map_err(|_| OperationError::Failed("The staged Piper runtime failed validation."))?;
        self.check_cancelled(id).await?;
        self.update_operation(
            id,
            95,
            "installing",
            "Activating the verified Piper runtime.",
        )
        .await;
        promote_directory_no_replace(&candidate, &self.engine_root())?;
        Ok(())
    }

    async fn run_uninstall(&self, id: Uuid) {
        self.update_operation(
            id,
            25,
            "removing",
            "Removing only the app-owned Piper engine; installed voices are retained.",
        )
        .await;
        let result = if self.active_cancel(id).await.load(Ordering::Acquire) {
            Err(OperationError::Cancelled)
        } else {
            remove_owned_tree(
                &self.engine_root(),
                self.root.as_ref(),
                ENGINE_KIND,
                "engine",
            )
            .await
            .map_err(|_| OperationError::Failed("The app-owned Piper engine could not be removed."))
        };
        if result.is_ok() {
            let mut state = self.state.lock().await;
            state.installed = false;
            state.engine_issue = None;
            state.engine_owned = false;
        }
        self.finish_operation(
            id,
            result,
            "The Piper engine was removed. Downloaded voices were retained.",
        )
        .await;
    }

    async fn run_voice_download(&self, id: Uuid, voice: &'static VoiceSpec) {
        let result = self.download_voice(id, voice).await;
        self.finish_operation(id, result, "The selected Piper voice is installed.")
            .await;
    }

    async fn download_voice(
        &self,
        id: Uuid,
        voice: &'static VoiceSpec,
    ) -> Result<(), OperationError> {
        let target = self.voice_root(voice.id);
        if target.exists() {
            return Err(OperationError::Failed(
                "The selected Piper voice path already exists and was not overwritten.",
            ));
        }
        let work = tempfile::Builder::new()
            .prefix(".piper-voice-")
            .tempdir_in(self.root.as_ref())
            .map_err(|_| {
                OperationError::Failed("A temporary voice directory could not be created.")
            })?;
        let candidate = work.path().join(voice.id);
        tokio::fs::create_dir(&candidate).await.map_err(|_| {
            OperationError::Failed("The voice staging directory could not be created.")
        })?;
        let total = voice.total_bytes();
        self.update_operation(id, 5, "downloading", "Downloading the pinned voice model.")
            .await;
        let model_path = candidate.join(format!("{}.onnx", voice.id));
        self.download_verified(
            id,
            &voice.model,
            &model_path,
            DownloadProgress::new(5, 85, 0, total),
        )
        .await?;
        let config_path = candidate.join(format!("{}.onnx.json", voice.id));
        self.download_verified(
            id,
            &voice.config,
            &config_path,
            DownloadProgress::new(5, 85, voice.model.bytes, total),
        )
        .await?;
        let card_path = candidate.join("MODEL_CARD");
        self.download_verified(
            id,
            &voice.card,
            &card_path,
            DownloadProgress::new(5, 85, voice.model.bytes + voice.config.bytes, total),
        )
        .await?;
        self.set_transfer(id, total, total, 5, 85).await;
        self.update_operation(
            id,
            90,
            "validating",
            "Validating the downloaded voice files.",
        )
        .await;
        validate_voice_payload(&candidate, voice).await?;
        self.check_cancelled(id).await?;
        let marker = OwnershipMarker::voice(voice);
        write_marker(&candidate, &marker).await.map_err(|_| {
            OperationError::Failed("Voice ownership metadata could not be written.")
        })?;
        promote_directory_no_replace(&candidate, &target)?;
        let installed = installed_voice(voice, &target, marker.completed_at);
        self.state
            .lock()
            .await
            .voices
            .insert(voice.id.to_owned(), installed);
        Ok(())
    }

    async fn download_verified(
        &self,
        operation_id: Uuid,
        artifact: &ArtifactSpec,
        destination: &Path,
        progress: DownloadProgress,
    ) -> Result<(), OperationError> {
        let cancel = self.active_cancel(operation_id).await;
        if destination.exists() {
            return Err(OperationError::Failed(
                "The Piper download destination already exists.",
            ));
        }
        let request = self.http.get(artifact.url).send();
        tokio::pin!(request);
        let response = loop {
            tokio::select! {
                result = &mut request => {
                    break result
                        .and_then(reqwest::Response::error_for_status)
                        .map_err(|_| OperationError::Failed(
                            "The pinned Piper artifact could not be downloaded.",
                        ))?;
                }
                () = tokio::time::sleep(Duration::from_millis(100)) => {
                    if cancel.load(Ordering::Acquire) {
                        return Err(OperationError::Cancelled);
                    }
                }
            }
        };
        if response.url().scheme() != "https" {
            return Err(OperationError::Failed(
                "The pinned Piper artifact was redirected outside HTTPS.",
            ));
        }
        if response
            .content_length()
            .is_some_and(|length| length > artifact.bytes)
        {
            return Err(OperationError::Failed(
                "The pinned Piper artifact exceeded its expected size.",
            ));
        }
        let mut file = tokio::fs::OpenOptions::new()
            .create_new(true)
            .write(true)
            .open(destination)
            .await
            .map_err(|_| {
                OperationError::Failed("The pinned Piper artifact could not be staged.")
            })?;
        let mut stream = response.bytes_stream();
        let mut downloaded = 0_u64;
        loop {
            let next = tokio::select! {
                chunk = stream.next() => chunk,
                () = tokio::time::sleep(Duration::from_millis(100)) => {
                    if cancel.load(Ordering::Acquire) {
                        return Err(OperationError::Cancelled);
                    }
                    continue;
                }
            };
            let Some(chunk) = next else { break };
            let chunk = chunk.map_err(|_| {
                OperationError::Failed("The pinned Piper artifact download was interrupted.")
            })?;
            downloaded = downloaded
                .checked_add(u64::try_from(chunk.len()).map_err(|_| {
                    OperationError::Failed("The pinned Piper artifact was too large.")
                })?)
                .ok_or(OperationError::Failed(
                    "The pinned Piper artifact was too large.",
                ))?;
            if downloaded > artifact.bytes {
                return Err(OperationError::Failed(
                    "The pinned Piper artifact exceeded its expected size.",
                ));
            }
            file.write_all(&chunk).await.map_err(|_| {
                OperationError::Failed("The pinned Piper artifact could not be staged.")
            })?;
            self.set_transfer(
                operation_id,
                progress.bytes_before + downloaded,
                progress.bytes_total,
                progress.start,
                progress.end,
            )
            .await;
        }
        file.flush().await.map_err(|_| {
            OperationError::Failed("The pinned Piper artifact could not be staged.")
        })?;
        file.sync_all().await.map_err(|_| {
            OperationError::Failed("The pinned Piper artifact could not be staged.")
        })?;
        drop(file);
        let path = destination.to_path_buf();
        let artifact = *artifact;
        tokio::task::spawn_blocking(move || verify_file(&path, &artifact))
            .await
            .map_err(|_| OperationError::Failed("The pinned Piper artifact verifier failed."))?
    }

    async fn check_cancelled(&self, id: Uuid) -> Result<(), OperationError> {
        if self.active_cancel(id).await.load(Ordering::Acquire) {
            Err(OperationError::Cancelled)
        } else {
            Ok(())
        }
    }
}

#[derive(Clone, Copy, Debug)]
struct ArtifactSpec {
    url: &'static str,
    sha256: &'static str,
    bytes: u64,
}

#[derive(Clone, Copy, Debug)]
struct DownloadProgress {
    start: u8,
    end: u8,
    bytes_before: u64,
    bytes_total: u64,
}

impl DownloadProgress {
    const fn new(start: u8, end: u8, bytes_before: u64, bytes_total: u64) -> Self {
        Self {
            start,
            end,
            bytes_before,
            bytes_total,
        }
    }
}

#[derive(Clone, Copy, Debug)]
struct VoiceSpec {
    id: &'static str,
    name: &'static str,
    language: &'static str,
    quality: &'static str,
    speakers: u16,
    sample_rate: u32,
    model: ArtifactSpec,
    config: ArtifactSpec,
    card: ArtifactSpec,
    model_card_path: &'static str,
}

impl VoiceSpec {
    const fn total_bytes(self) -> u64 {
        self.model.bytes + self.config.bytes + self.card.bytes
    }

    fn view(self, installed: bool) -> PiperCatalogVoiceView {
        PiperCatalogVoiceView {
            id: self.id,
            name: self.name,
            language: self.language,
            quality: self.quality,
            speakers: self.speakers,
            sample_rate: self.sample_rate,
            size_bytes: self.total_bytes(),
            license: "Source dataset: CC0-1.0",
            license_url: "https://creativecommons.org/publicdomain/zero/1.0/",
            license_summary: "The pinned model card declares the source dataset as CC0; review the model card and source before downloading.",
            model_card_url: format!(
                "https://huggingface.co/rhasspy/piper-voices/blob/{PIPER_VOICES_REVISION}/{}",
                self.model_card_path
            ),
            source_url: "https://github.com/thorstenMueller/deep-learning-german-tts",
            installed,
        }
    }
}

const ENGINE_ARCHIVE: ArtifactSpec = ArtifactSpec {
    url: PIPER_ARCHIVE_URL,
    sha256: PIPER_ARCHIVE_SHA256,
    bytes: PIPER_ARCHIVE_BYTES,
};

const VOICES: [VoiceSpec; 1] = [VoiceSpec {
    id: "de_DE-thorsten-medium",
    name: "Thorsten",
    language: "de_DE",
    quality: "medium",
    speakers: 1,
    sample_rate: 22_050,
    model: ArtifactSpec {
        url: "https://huggingface.co/rhasspy/piper-voices/resolve/f5a6e9094787fd865d65cb024472f977f9c542b5/de/de_DE/thorsten/medium/de_DE-thorsten-medium.onnx?download=true",
        sha256: "7e64762d8e5118bb578f2eea6207e1a35a8e0c30595010b666f983fc87bb7819",
        bytes: 63_201_294,
    },
    config: ArtifactSpec {
        url: "https://huggingface.co/rhasspy/piper-voices/resolve/f5a6e9094787fd865d65cb024472f977f9c542b5/de/de_DE/thorsten/medium/de_DE-thorsten-medium.onnx.json?download=true",
        sha256: "974adee790533adb273a1ac88f49027d2a1b8f0f2cf4905954a4791e79264e85",
        bytes: 4_819,
    },
    card: ArtifactSpec {
        url: "https://huggingface.co/rhasspy/piper-voices/resolve/f5a6e9094787fd865d65cb024472f977f9c542b5/de/de_DE/thorsten/medium/MODEL_CARD?download=true",
        sha256: "5196b5ab0794e6056263a1f37c18bec407b61ac187529bee29d1c366871e5c9e",
        bytes: 285,
    },
    model_card_path: "de/de_DE/thorsten/medium/MODEL_CARD",
}];

#[derive(Clone, Copy, Debug, thiserror::Error)]
enum OperationError {
    #[error("operation cancelled")]
    Cancelled,
    #[error("{0}")]
    Failed(&'static str),
}

impl OwnershipMarker {
    fn engine() -> Self {
        let now = Utc::now();
        Self {
            schema_version: 1,
            owner: OWNER.to_owned(),
            kind: ENGINE_KIND.to_owned(),
            id: "engine".to_owned(),
            version: PIPER_VERSION.to_owned(),
            revision: None,
            license: None,
            license_confirmed_at: None,
            created_at: now,
            completed_at: now,
        }
    }

    fn voice(voice: &VoiceSpec) -> Self {
        let now = Utc::now();
        Self {
            schema_version: 1,
            owner: OWNER.to_owned(),
            kind: VOICE_KIND.to_owned(),
            id: voice.id.to_owned(),
            version: PIPER_VERSION.to_owned(),
            revision: Some(PIPER_VOICES_REVISION.to_owned()),
            license: Some(VOICE_LICENSE_MARKER.to_owned()),
            license_confirmed_at: Some(now),
            created_at: now,
            completed_at: now,
        }
    }
}

fn catalog_voice(id: &str) -> Option<&'static VoiceSpec> {
    VOICES.iter().find(|voice| voice.id == id)
}

fn validate_voice_id(id: &str) -> Result<(), ServiceError> {
    let mut bytes = id.bytes();
    let first = bytes.next().ok_or_else(|| {
        ServiceError::InvalidRequest("Piper voice id must not be empty".to_owned())
    })?;
    if id.len() > 128
        || !first.is_ascii_alphanumeric()
        || !bytes.all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-'))
    {
        return Err(ServiceError::InvalidRequest(
            "Piper voice id may contain only ASCII letters, digits, '-' and '_'".to_owned(),
        ));
    }
    Ok(())
}

fn ensure_idle(state: &ManagerState) -> Result<(), ServiceError> {
    if state
        .active
        .as_ref()
        .is_some_and(|operation| operation.view.state.active())
    {
        Err(ServiceError::Conflict(
            "another Piper management operation is already active".to_owned(),
        ))
    } else {
        Ok(())
    }
}

fn regular_non_symlink(path: &Path) -> bool {
    std::fs::symlink_metadata(path)
        .is_ok_and(|metadata| metadata.is_file() && !metadata.file_type().is_symlink())
}

#[cfg(target_os = "linux")]
fn promote_directory_no_replace(source: &Path, target: &Path) -> Result<(), OperationError> {
    match rustix::fs::renameat_with(
        rustix::fs::CWD,
        source,
        rustix::fs::CWD,
        target,
        rustix::fs::RenameFlags::NOREPLACE,
    ) {
        Ok(()) => Ok(()),
        Err(rustix::io::Errno::EXIST | rustix::io::Errno::NOTEMPTY) => Err(OperationError::Failed(
            "The managed Piper target appeared during activation and was not overwritten.",
        )),
        Err(_) => Err(OperationError::Failed(
            "The managed Piper target could not be atomically activated.",
        )),
    }
}

#[cfg(not(target_os = "linux"))]
fn promote_directory_no_replace(_source: &Path, _target: &Path) -> Result<(), OperationError> {
    Err(OperationError::Failed(
        "Managed Piper activation is supported only on Linux.",
    ))
}

async fn ensure_managed_directory(path: &Path) -> Result<(), ServiceError> {
    match tokio::fs::symlink_metadata(path).await {
        Ok(metadata) if metadata.is_dir() && !metadata.file_type().is_symlink() => Ok(()),
        Ok(_) => Err(ServiceError::InvalidRequest(
            "the managed Piper path is not a safe directory".to_owned(),
        )),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            tokio::fs::create_dir_all(path).await?;
            let metadata = tokio::fs::symlink_metadata(path).await?;
            if metadata.is_dir() && !metadata.file_type().is_symlink() {
                Ok(())
            } else {
                Err(ServiceError::InvalidRequest(
                    "the managed Piper directory could not be created safely".to_owned(),
                ))
            }
        }
        Err(error) => Err(error.into()),
    }
}

async fn write_marker(root: &Path, marker: &OwnershipMarker) -> Result<(), ServiceError> {
    let bytes = serde_json::to_vec_pretty(marker)
        .map_err(|error| ServiceError::Internal(error.to_string()))?;
    let temporary = root.join(format!(".{MARKER_FILE}.{}.tmp", Uuid::new_v4().simple()));
    let mut file = tokio::fs::OpenOptions::new()
        .create_new(true)
        .write(true)
        .open(&temporary)
        .await?;
    file.write_all(&bytes).await?;
    file.flush().await?;
    file.sync_all().await?;
    drop(file);
    tokio::fs::rename(temporary, root.join(MARKER_FILE)).await?;
    Ok(())
}

async fn read_marker(root: &Path) -> Result<OwnershipMarker, ServiceError> {
    let path = root.join(MARKER_FILE);
    let metadata = tokio::fs::symlink_metadata(&path).await?;
    if !metadata.is_file()
        || metadata.file_type().is_symlink()
        || metadata.len() == 0
        || metadata.len() > MAX_MARKER_BYTES
    {
        return Err(ServiceError::InvalidRequest(
            "Piper ownership metadata is unsafe".to_owned(),
        ));
    }
    let bytes = tokio::fs::read(path).await?;
    serde_json::from_slice(&bytes)
        .map_err(|_| ServiceError::InvalidRequest("Piper ownership metadata is invalid".to_owned()))
}

fn marker_owns(marker: &OwnershipMarker, kind: &str, id: &str) -> bool {
    marker.schema_version == 1
        && marker.owner == OWNER
        && marker.kind == kind
        && marker.id == id
        && marker.version == PIPER_VERSION
}

fn voice_marker_matches(marker: &OwnershipMarker, voice: &VoiceSpec) -> bool {
    marker_owns(marker, VOICE_KIND, voice.id)
        && marker.revision.as_deref() == Some(PIPER_VOICES_REVISION)
        && marker.license.as_deref() == Some(VOICE_LICENSE_MARKER)
        && marker.license_confirmed_at.is_some_and(|confirmed| {
            marker.created_at <= confirmed
                && confirmed <= marker.completed_at
                && marker.created_at <= marker.completed_at
        })
}

async fn remove_owned_tree(
    target: &Path,
    parent: &Path,
    kind: &str,
    id: &str,
) -> Result<(), ServiceError> {
    let metadata = tokio::fs::symlink_metadata(target).await?;
    if !metadata.is_dir() || metadata.file_type().is_symlink() {
        return Err(ServiceError::Conflict(
            "the Piper path is not an app-owned directory".to_owned(),
        ));
    }
    let canonical_parent = tokio::fs::canonicalize(parent).await?;
    let canonical_target = tokio::fs::canonicalize(target).await?;
    if canonical_target.parent() != Some(canonical_parent.as_path()) {
        return Err(ServiceError::Conflict(
            "the Piper path escaped its app-owned parent".to_owned(),
        ));
    }
    let marker = read_marker(target).await?;
    if !marker_owns(&marker, kind, id) {
        return Err(ServiceError::Conflict(
            "the Piper directory is not owned by this AudiobookAI installation".to_owned(),
        ));
    }
    tokio::fs::remove_dir_all(target).await?;
    Ok(())
}

async fn validate_engine_install(root: &Path) -> Result<(), ServiceError> {
    let marker = read_marker(root).await?;
    if !marker_owns(&marker, ENGINE_KIND, "engine") {
        return Err(ServiceError::InvalidRequest(
            "Piper engine ownership metadata is invalid".to_owned(),
        ));
    }
    validate_engine_tree(root.join("piper")).await
}

async fn validate_extracted_root(root: &Path) -> Result<(), OperationError> {
    let mut entries = tokio::fs::read_dir(root).await.map_err(|_| {
        OperationError::Failed("The extracted Piper archive could not be inspected.")
    })?;
    let entry = entries
        .next_entry()
        .await
        .map_err(|_| OperationError::Failed("The extracted Piper archive could not be inspected."))?
        .ok_or(OperationError::Failed("The Piper archive was empty."))?;
    if entry.file_name() != "piper" || entries.next_entry().await.ok().flatten().is_some() {
        return Err(OperationError::Failed(
            "The Piper archive did not contain its exact expected root.",
        ));
    }
    validate_engine_tree(entry.path()).await.map_err(|_| {
        OperationError::Failed("The extracted Piper runtime failed safety validation.")
    })
}

async fn validate_engine_tree(root: PathBuf) -> Result<(), ServiceError> {
    tokio::task::spawn_blocking(move || validate_engine_tree_blocking(&root))
        .await
        .map_err(ServiceError::Join)?
}

fn validate_engine_tree_blocking(root: &Path) -> Result<(), ServiceError> {
    let metadata = std::fs::symlink_metadata(root)?;
    if !metadata.is_dir() || metadata.file_type().is_symlink() {
        return Err(ServiceError::InvalidRequest(
            "Piper runtime root is unsafe".to_owned(),
        ));
    }
    let canonical_root = root.canonicalize()?;
    let mut stack = vec![root.to_path_buf()];
    let mut files = 0_usize;
    let mut bytes = 0_u64;
    while let Some(directory) = stack.pop() {
        for entry in std::fs::read_dir(directory)? {
            let entry = entry?;
            files = files.saturating_add(1);
            if files > MAX_ENGINE_FILES {
                return Err(ServiceError::InvalidRequest(
                    "Piper runtime contains too many entries".to_owned(),
                ));
            }
            let path = entry.path();
            let metadata = std::fs::symlink_metadata(&path)?;
            let file_type = metadata.file_type();
            if file_type.is_dir() {
                stack.push(path);
            } else if file_type.is_file() {
                bytes = bytes.checked_add(metadata.len()).ok_or_else(|| {
                    ServiceError::InvalidRequest("Piper runtime is too large".to_owned())
                })?;
                if bytes > MAX_ENGINE_BYTES {
                    return Err(ServiceError::InvalidRequest(
                        "Piper runtime is too large".to_owned(),
                    ));
                }
            } else if file_type.is_symlink() {
                let link = std::fs::read_link(&path)?;
                if link.is_absolute()
                    || link.components().any(|component| {
                        matches!(
                            component,
                            Component::ParentDir | Component::RootDir | Component::Prefix(_)
                        )
                    })
                {
                    return Err(ServiceError::InvalidRequest(
                        "Piper runtime contains an unsafe symbolic link".to_owned(),
                    ));
                }
                let resolved = path.canonicalize()?;
                if !resolved.starts_with(&canonical_root) {
                    return Err(ServiceError::InvalidRequest(
                        "Piper runtime symbolic link escaped its root".to_owned(),
                    ));
                }
            } else {
                return Err(ServiceError::InvalidRequest(
                    "Piper runtime contains a special filesystem entry".to_owned(),
                ));
            }
        }
    }
    for required in [
        root.join("piper"),
        root.join("libonnxruntime.so.1.14.1"),
        root.join("libpiper_phonemize.so"),
    ] {
        if !regular_non_symlink(&required) {
            return Err(ServiceError::InvalidRequest(
                "Piper runtime is incomplete".to_owned(),
            ));
        }
    }
    let data = std::fs::symlink_metadata(root.join("espeak-ng-data"))?;
    if !data.is_dir() || data.file_type().is_symlink() {
        return Err(ServiceError::InvalidRequest(
            "Piper runtime voice data is incomplete".to_owned(),
        ));
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        if std::fs::metadata(root.join("piper"))?.permissions().mode() & 0o111 == 0 {
            return Err(ServiceError::InvalidRequest(
                "Piper executable permission is missing".to_owned(),
            ));
        }
    }
    Ok(())
}

#[cfg(test)]
fn safe_archive_path(value: &str) -> bool {
    normalize_archive_path(Path::new(value)).is_some()
}

#[derive(Clone, Copy, Debug)]
struct ArchiveLimits {
    entries: usize,
    uncompressed_bytes: u64,
}

impl ArchiveLimits {
    const ENGINE: Self = Self {
        entries: MAX_ENGINE_FILES,
        uncompressed_bytes: MAX_ENGINE_BYTES,
    };
}

async fn extract_archive(
    archive: &Path,
    destination: &Path,
    cancel: Arc<AtomicBool>,
    limits: ArchiveLimits,
) -> Result<(), OperationError> {
    let archive = archive.to_path_buf();
    let destination = destination.to_path_buf();
    tokio::task::spawn_blocking(move || {
        extract_archive_blocking(
            &archive,
            &destination,
            &cancel,
            limits,
            Some(ENGINE_ARCHIVE),
        )
    })
    .await
    .map_err(|_| OperationError::Failed("The Piper archive extractor failed."))?
}

fn extract_archive_blocking(
    archive_path: &Path,
    destination: &Path,
    cancel: &AtomicBool,
    limits: ArchiveLimits,
    expected: Option<ArtifactSpec>,
) -> Result<(), OperationError> {
    validate_empty_extraction_root(destination)?;
    let mut archive_file = open_regular_file_no_follow(archive_path)?;
    let metadata = archive_file.metadata().map_err(|_| {
        OperationError::Failed("The verified Piper archive could not be inspected.")
    })?;
    let path_metadata = std::fs::symlink_metadata(archive_path).map_err(|_| {
        OperationError::Failed("The verified Piper archive could not be inspected.")
    })?;
    if !metadata.is_file() || !path_metadata.is_file() || path_metadata.file_type().is_symlink() {
        return Err(OperationError::Failed(
            "The verified Piper archive had an unsafe filesystem shape.",
        ));
    }
    if let Some(expected) = expected {
        verify_open_file(&mut archive_file, &expected)?;
    }

    let decoder = flate2::read::GzDecoder::new(archive_file);
    let mut archive = tar::Archive::new(decoder);
    let entries = archive
        .entries()
        .map_err(|_| OperationError::Failed("The verified Piper archive was invalid."))?;
    let mut seen = BTreeSet::new();
    let mut entry_count = 0_usize;
    let mut uncompressed_bytes = 0_u64;
    let mut root_seen = false;

    for entry in entries {
        if cancel.load(Ordering::Acquire) {
            return Err(OperationError::Cancelled);
        }
        entry_count = entry_count.checked_add(1).ok_or(OperationError::Failed(
            "The Piper archive contained too many entries.",
        ))?;
        if entry_count > limits.entries {
            return Err(OperationError::Failed(
                "The Piper archive contained too many entries.",
            ));
        }
        let mut entry =
            entry.map_err(|_| OperationError::Failed("The verified Piper archive was invalid."))?;
        let path = entry
            .path()
            .map_err(|_| OperationError::Failed("The Piper archive path was invalid."))?;
        let relative = normalize_archive_path(path.as_ref()).ok_or(OperationError::Failed(
            "The Piper archive contained an unsafe path.",
        ))?;
        if !seen.insert(relative.clone()) {
            return Err(OperationError::Failed(
                "The Piper archive contained a duplicate path.",
            ));
        }

        let entry_type = entry.header().entry_type();
        if entry_count == 1 && (relative != Path::new("piper") || !entry_type.is_dir()) {
            return Err(OperationError::Failed(
                "The Piper archive did not begin with its exact expected root.",
            ));
        }
        if relative == Path::new("piper") {
            if root_seen || !entry_type.is_dir() {
                return Err(OperationError::Failed(
                    "The Piper archive root was invalid.",
                ));
            }
            root_seen = true;
        }

        extract_archive_entry(
            destination,
            &relative,
            &mut entry,
            limits,
            &mut uncompressed_bytes,
            cancel,
        )?;
    }

    if entry_count == 0 || !root_seen {
        return Err(OperationError::Failed("The Piper archive was empty."));
    }
    Ok(())
}

fn extract_archive_entry<R: std::io::Read>(
    destination: &Path,
    relative: &Path,
    entry: &mut tar::Entry<'_, R>,
    limits: ArchiveLimits,
    uncompressed_bytes: &mut u64,
    cancel: &AtomicBool,
) -> Result<(), OperationError> {
    let size = entry.size();
    let entry_type = entry.header().entry_type();
    if entry_type.is_file() {
        *uncompressed_bytes =
            uncompressed_bytes
                .checked_add(size)
                .ok_or(OperationError::Failed(
                    "The Piper archive expanded beyond its safety limit.",
                ))?;
        if *uncompressed_bytes > limits.uncompressed_bytes {
            return Err(OperationError::Failed(
                "The Piper archive expanded beyond its safety limit.",
            ));
        }
        let mode = entry
            .header()
            .mode()
            .map_err(|_| OperationError::Failed("The Piper archive permissions were invalid."))?;
        extract_regular_file(destination, relative, entry, size, mode, cancel)
    } else if entry_type.is_dir() {
        if size != 0 {
            return Err(OperationError::Failed(
                "The Piper archive directory entry was invalid.",
            ));
        }
        create_archive_directory(destination, relative)
    } else if entry_type.is_symlink() {
        if size != 0 {
            return Err(OperationError::Failed(
                "The Piper archive symbolic link was invalid.",
            ));
        }
        let target = entry
            .link_name()
            .map_err(|_| OperationError::Failed("The Piper archive symbolic link was invalid."))?
            .ok_or(OperationError::Failed(
                "The Piper archive symbolic link had no target.",
            ))?;
        if !safe_archive_symlink_target(relative, target.as_ref()) {
            return Err(OperationError::Failed(
                "The Piper archive contained an unsafe symbolic link.",
            ));
        }
        create_archive_symlink(destination, relative, target.as_ref())
    } else {
        Err(OperationError::Failed(
            "The Piper archive contained a special filesystem entry.",
        ))
    }
}

fn normalize_archive_path(path: &Path) -> Option<PathBuf> {
    if path.as_os_str().is_empty()
        || path.as_os_str().as_encoded_bytes().contains(&0)
        || path.is_absolute()
    {
        return None;
    }
    let mut normalized = PathBuf::new();
    for component in path.components() {
        match component {
            Component::Normal(value) => normalized.push(value),
            Component::CurDir => {}
            Component::ParentDir | Component::RootDir | Component::Prefix(_) => return None,
        }
    }
    normalized
        .components()
        .next()
        .is_some_and(|component| component.as_os_str() == "piper")
        .then_some(normalized)
}

fn safe_archive_symlink_target(link_path: &Path, target: &Path) -> bool {
    if target.as_os_str().is_empty()
        || target.as_os_str().as_encoded_bytes().contains(&0)
        || target.is_absolute()
    {
        return false;
    }
    let Some(parent) = link_path.parent() else {
        return false;
    };
    let mut resolved = parent
        .components()
        .filter_map(|component| match component {
            Component::Normal(value) => Some(value.to_os_string()),
            _ => None,
        })
        .collect::<Vec<_>>();
    for component in target.components() {
        match component {
            Component::Normal(value) => resolved.push(value.to_os_string()),
            Component::CurDir => {}
            Component::ParentDir if resolved.len() > 1 => {
                resolved.pop();
            }
            Component::ParentDir | Component::RootDir | Component::Prefix(_) => return false,
        }
    }
    resolved
        .first()
        .is_some_and(|component| component == "piper")
}

fn validate_empty_extraction_root(destination: &Path) -> Result<(), OperationError> {
    let metadata = std::fs::symlink_metadata(destination).map_err(|_| {
        OperationError::Failed("The Piper extraction directory could not be inspected.")
    })?;
    if !metadata.is_dir() || metadata.file_type().is_symlink() {
        return Err(OperationError::Failed(
            "The Piper extraction directory was unsafe.",
        ));
    }
    if std::fs::read_dir(destination)
        .map_err(|_| {
            OperationError::Failed("The Piper extraction directory could not be inspected.")
        })?
        .next()
        .is_some()
    {
        return Err(OperationError::Failed(
            "The Piper extraction directory was not empty.",
        ));
    }
    Ok(())
}

fn ensure_archive_parents(destination: &Path, relative: &Path) -> Result<(), OperationError> {
    let mut current = destination.to_path_buf();
    let Some(parent) = relative.parent() else {
        return Ok(());
    };
    for component in parent.components() {
        let Component::Normal(value) = component else {
            return Err(OperationError::Failed(
                "The Piper archive contained an unsafe path.",
            ));
        };
        current.push(value);
        match std::fs::symlink_metadata(&current) {
            Ok(metadata) if metadata.is_dir() && !metadata.file_type().is_symlink() => {}
            Ok(_) => {
                return Err(OperationError::Failed(
                    "The Piper archive path crossed an unsafe filesystem entry.",
                ));
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                std::fs::create_dir(&current).map_err(|_| {
                    OperationError::Failed("The Piper archive directory could not be staged.")
                })?;
            }
            Err(_) => {
                return Err(OperationError::Failed(
                    "The Piper archive path could not be inspected.",
                ));
            }
        }
    }
    Ok(())
}

fn create_archive_directory(destination: &Path, relative: &Path) -> Result<(), OperationError> {
    ensure_archive_parents(destination, relative)?;
    let target = destination.join(relative);
    match std::fs::symlink_metadata(&target) {
        Ok(metadata) if metadata.is_dir() && !metadata.file_type().is_symlink() => Ok(()),
        Ok(_) => Err(OperationError::Failed(
            "The Piper archive directory collided with an existing filesystem entry.",
        )),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => std::fs::create_dir(target)
            .map_err(|_| {
                OperationError::Failed("The Piper archive directory could not be staged.")
            }),
        Err(_) => Err(OperationError::Failed(
            "The Piper archive directory could not be inspected.",
        )),
    }
}

fn extract_regular_file<R: std::io::Read>(
    destination: &Path,
    relative: &Path,
    entry: &mut R,
    size: u64,
    mode: u32,
    cancel: &AtomicBool,
) -> Result<(), OperationError> {
    ensure_archive_parents(destination, relative)?;
    let target = destination.join(relative);
    let mut options = OpenOptions::new();
    options.create_new(true).write(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt as _;
        options.mode(if mode & 0o111 == 0 { 0o600 } else { 0o700 });
    }
    let mut output = options.open(&target).map_err(|_| {
        OperationError::Failed("The Piper archive file could not be staged without overwriting.")
    })?;
    let mut remaining = size;
    let mut buffer = vec![0_u8; 128 * 1024].into_boxed_slice();
    while remaining > 0 {
        if cancel.load(Ordering::Acquire) {
            return Err(OperationError::Cancelled);
        }
        let limit = usize::try_from(remaining.min(buffer.len() as u64))
            .map_err(|_| OperationError::Failed("The Piper archive entry was too large."))?;
        let count = entry
            .read(&mut buffer[..limit])
            .map_err(|_| OperationError::Failed("The Piper archive entry could not be read."))?;
        if count == 0 {
            return Err(OperationError::Failed(
                "The Piper archive entry ended unexpectedly.",
            ));
        }
        output
            .write_all(&buffer[..count])
            .map_err(|_| OperationError::Failed("The Piper archive file could not be staged."))?;
        remaining -= u64::try_from(count)
            .map_err(|_| OperationError::Failed("The Piper archive entry was too large."))?;
    }
    output
        .flush()
        .map_err(|_| OperationError::Failed("The Piper archive file could not be staged."))?;
    output
        .sync_all()
        .map_err(|_| OperationError::Failed("The Piper archive file could not be staged."))?;
    Ok(())
}

#[cfg(unix)]
fn create_archive_symlink(
    destination: &Path,
    relative: &Path,
    link_target: &Path,
) -> Result<(), OperationError> {
    use std::os::unix::fs::symlink;

    ensure_archive_parents(destination, relative)?;
    let target = destination.join(relative);
    if std::fs::symlink_metadata(&target).is_ok() {
        return Err(OperationError::Failed(
            "The Piper archive symbolic link would overwrite an existing entry.",
        ));
    }
    symlink(link_target, target)
        .map_err(|_| OperationError::Failed("The Piper archive symbolic link could not be staged."))
}

#[cfg(not(unix))]
fn create_archive_symlink(
    _destination: &Path,
    _relative: &Path,
    _link_target: &Path,
) -> Result<(), OperationError> {
    Err(OperationError::Failed(
        "Piper archive symbolic links are supported only on Unix.",
    ))
}

async fn validate_voice_payload(root: &Path, voice: &VoiceSpec) -> Result<(), OperationError> {
    let root = root.to_path_buf();
    let voice = *voice;
    tokio::task::spawn_blocking(move || validate_voice_payload_blocking(&root, &voice))
        .await
        .map_err(|_| OperationError::Failed("The downloaded voice validator failed."))?
}

fn validate_voice_payload_blocking(root: &Path, voice: &VoiceSpec) -> Result<(), OperationError> {
    let metadata = std::fs::symlink_metadata(root)
        .map_err(|_| OperationError::Failed("The downloaded voice directory was missing."))?;
    if !metadata.is_dir() || metadata.file_type().is_symlink() {
        return Err(OperationError::Failed(
            "The downloaded voice directory was unsafe.",
        ));
    }
    verify_file(&root.join(format!("{}.onnx", voice.id)), &voice.model)?;
    let config = root.join(format!("{}.onnx.json", voice.id));
    verify_file(&config, &voice.config)?;
    verify_file(&root.join("MODEL_CARD"), &voice.card)?;
    let value: serde_json::Value = serde_json::from_slice(
        &std::fs::read(config)
            .map_err(|_| OperationError::Failed("The voice config could not be read."))?,
    )
    .map_err(|_| OperationError::Failed("The voice config was not valid JSON."))?;
    let sample_rate = value
        .pointer("/audio/sample_rate")
        .and_then(serde_json::Value::as_u64);
    let speakers = value
        .get("num_speakers")
        .and_then(serde_json::Value::as_u64);
    let language = value
        .pointer("/language/code")
        .and_then(serde_json::Value::as_str);
    if sample_rate != Some(u64::from(voice.sample_rate))
        || speakers != Some(u64::from(voice.speakers))
        || language != Some(voice.language)
    {
        return Err(OperationError::Failed(
            "The voice config metadata did not match the pinned catalog.",
        ));
    }
    Ok(())
}

#[cfg(target_os = "linux")]
fn open_regular_file_no_follow(path: &Path) -> Result<std::fs::File, OperationError> {
    let descriptor = rustix::fs::open(
        path,
        rustix::fs::OFlags::RDONLY | rustix::fs::OFlags::CLOEXEC | rustix::fs::OFlags::NOFOLLOW,
        rustix::fs::Mode::empty(),
    )
    .map_err(|_| OperationError::Failed("A pinned Piper artifact could not be opened."))?;
    Ok(descriptor.into())
}

#[cfg(not(target_os = "linux"))]
fn open_regular_file_no_follow(path: &Path) -> Result<std::fs::File, OperationError> {
    OpenOptions::new()
        .read(true)
        .open(path)
        .map_err(|_| OperationError::Failed("A pinned Piper artifact could not be opened."))
}

fn verify_open_file(
    file: &mut std::fs::File,
    artifact: &ArtifactSpec,
) -> Result<(), OperationError> {
    let metadata = file
        .metadata()
        .map_err(|_| OperationError::Failed("A pinned Piper artifact could not be inspected."))?;
    if !metadata.is_file() || metadata.len() != artifact.bytes {
        return Err(OperationError::Failed(
            "A pinned Piper artifact had an invalid filesystem shape or size.",
        ));
    }
    file.rewind()
        .map_err(|_| OperationError::Failed("A pinned Piper artifact could not be verified."))?;
    let mut hasher = Sha256::new();
    let mut buffer = vec![0_u8; 128 * 1024].into_boxed_slice();
    loop {
        let count = file.read(&mut buffer).map_err(|_| {
            OperationError::Failed("A pinned Piper artifact could not be verified.")
        })?;
        if count == 0 {
            break;
        }
        hasher.update(&buffer[..count]);
    }
    if format!("{:x}", hasher.finalize()) != artifact.sha256 {
        return Err(OperationError::Failed(
            "A pinned Piper artifact failed its SHA-256 check.",
        ));
    }
    file.rewind()
        .map_err(|_| OperationError::Failed("A pinned Piper artifact could not be verified."))?;
    Ok(())
}

fn verify_file(path: &Path, artifact: &ArtifactSpec) -> Result<(), OperationError> {
    let metadata = std::fs::symlink_metadata(path)
        .map_err(|_| OperationError::Failed("A pinned Piper artifact was missing."))?;
    if !metadata.is_file() || metadata.file_type().is_symlink() || metadata.len() != artifact.bytes
    {
        return Err(OperationError::Failed(
            "A pinned Piper artifact had an invalid filesystem shape or size.",
        ));
    }
    let mut file = open_regular_file_no_follow(path)?;
    verify_open_file(&mut file, artifact)
}

#[derive(Debug, Default)]
struct HydratedVoices {
    installed: BTreeMap<String, PiperInstalledVoiceView>,
    issues: BTreeMap<String, PiperInstallerStatus>,
}

async fn hydrate_voices(root: &Path) -> HydratedVoices {
    let mut hydrated = HydratedVoices::default();
    for spec in &VOICES {
        let target = root.join(spec.id);
        let metadata = match tokio::fs::symlink_metadata(&target).await {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
            Err(_) => {
                hydrated
                    .issues
                    .insert(spec.id.to_owned(), PiperInstallerStatus::UnsafeFilesystem);
                continue;
            }
        };
        if !metadata.is_dir() || metadata.file_type().is_symlink() {
            hydrated
                .issues
                .insert(spec.id.to_owned(), PiperInstallerStatus::UnsafeFilesystem);
            continue;
        }
        let marker = match read_marker(&target).await {
            Ok(marker) if marker_owns(&marker, VOICE_KIND, spec.id) => marker,
            _ => {
                hydrated
                    .issues
                    .insert(spec.id.to_owned(), PiperInstallerStatus::UnsafeFilesystem);
                continue;
            }
        };
        if !voice_marker_matches(&marker, spec)
            || validate_voice_payload(&target, spec).await.is_err()
        {
            hydrated
                .issues
                .insert(spec.id.to_owned(), PiperInstallerStatus::Incomplete);
            continue;
        }
        hydrated.installed.insert(
            spec.id.to_owned(),
            installed_voice(spec, &target, marker.completed_at),
        );
    }
    hydrated
}

fn voice_issue_view(id: &str, status: PiperInstallerStatus) -> PiperVoiceIssueView {
    let (removable, detail) = match status {
        PiperInstallerStatus::Incomplete => (
            true,
            "The app-owned voice is incomplete and can be removed before downloading it again.",
        ),
        _ => (
            false,
            "The voice path is not owned by AudiobookAI and must be resolved manually.",
        ),
    };
    PiperVoiceIssueView {
        id: id.to_owned(),
        status,
        removable,
        detail: detail.to_owned(),
    }
}

fn installed_voice(
    voice: &VoiceSpec,
    root: &Path,
    installed_at: DateTime<Utc>,
) -> PiperInstalledVoiceView {
    PiperInstalledVoiceView {
        id: voice.id.to_owned(),
        name: voice.name.to_owned(),
        language: voice.language.to_owned(),
        quality: voice.quality.to_owned(),
        model_path: root.join(format!("{}.onnx", voice.id)),
        config_path: root.join(format!("{}.onnx.json", voice.id)),
        size_bytes: voice.total_bytes(),
        license: "Source dataset: CC0-1.0".to_owned(),
        installed_at,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    type TestArchiveBuilder = tar::Builder<flate2::write::GzEncoder<std::fs::File>>;

    fn build_test_archive(path: &Path, populate: impl FnOnce(&mut TestArchiveBuilder)) {
        let file = std::fs::File::create(path).expect("test archive file");
        let encoder = flate2::write::GzEncoder::new(file, flate2::Compression::fast());
        let mut builder = tar::Builder::new(encoder);
        populate(&mut builder);
        builder
            .into_inner()
            .expect("complete test tar stream")
            .finish()
            .expect("complete test gzip stream");
    }

    fn append_test_directory(builder: &mut TestArchiveBuilder, path: &str) {
        let mut header = tar::Header::new_gnu();
        header.set_entry_type(tar::EntryType::Directory);
        header.set_mode(0o755);
        header.set_size(0);
        builder
            .append_data(&mut header, path, std::io::empty())
            .expect("test directory entry");
    }

    fn append_test_file(builder: &mut TestArchiveBuilder, path: &str, bytes: &[u8]) {
        let mut header = tar::Header::new_gnu();
        header.set_entry_type(tar::EntryType::Regular);
        header.set_mode(0o755);
        header.set_size(u64::try_from(bytes.len()).expect("test file size"));
        builder
            .append_data(&mut header, path, bytes)
            .expect("test file entry");
    }

    fn append_test_symlink(builder: &mut TestArchiveBuilder, path: &str, target: &str) {
        let mut header = tar::Header::new_gnu();
        header.set_entry_type(tar::EntryType::Symlink);
        header.set_mode(0o777);
        header.set_size(0);
        builder
            .append_link(&mut header, path, target)
            .expect("test symbolic link entry");
    }

    fn append_test_hardlink(builder: &mut TestArchiveBuilder, path: &str, target: &str) {
        let mut header = tar::Header::new_gnu();
        header.set_entry_type(tar::EntryType::Link);
        header.set_mode(0o600);
        header.set_size(0);
        builder
            .append_link(&mut header, path, target)
            .expect("test hard link entry");
    }

    fn append_test_special(builder: &mut TestArchiveBuilder, path: &str) {
        let mut header = tar::Header::new_gnu();
        header.set_entry_type(tar::EntryType::Fifo);
        header.set_mode(0o600);
        header.set_size(0);
        builder
            .append_data(&mut header, path, std::io::empty())
            .expect("test special entry");
    }

    fn append_raw_test_file(builder: &mut TestArchiveBuilder, path: &[u8], bytes: &[u8]) {
        assert!(path.len() <= 100);
        let mut header = tar::Header::new_old();
        header.set_entry_type(tar::EntryType::Regular);
        header.set_mode(0o600);
        header.set_size(u64::try_from(bytes.len()).expect("test file size"));
        header.as_old_mut().name[..path.len()].copy_from_slice(path);
        header.set_cksum();
        builder
            .append(&header, bytes)
            .expect("raw test archive entry");
    }

    fn extract_test_archive(
        archive: &Path,
        destination: &Path,
        limits: ArchiveLimits,
    ) -> Result<(), OperationError> {
        std::fs::create_dir(destination).expect("test extraction root");
        extract_archive_blocking(archive, destination, &AtomicBool::new(false), limits, None)
    }

    fn test_config(data_dir: &Path) -> ServiceConfig {
        ServiceConfig {
            bind: "127.0.0.1:0".parse().expect("loopback address"),
            data_dir: data_dir.to_path_buf(),
            bundled_sidecar_dir: None,
            tls: None,
            lan_hostnames: Vec::new(),
            allow_insecure_lan: false,
            desktop_bootstrap: true,
        }
    }

    async fn wait_for_operation(manager: &PiperManager, id: Uuid) -> PiperOperationView {
        for _ in 0..12_000 {
            let view = manager.view().await;
            if let Some(operation) = view.last_operation
                && operation.id == id
            {
                return operation;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        panic!("timed out waiting for Piper operation {id}");
    }

    #[test]
    fn curated_catalog_is_pinned_and_has_unique_safe_ids() {
        let mut ids = std::collections::BTreeSet::new();
        for voice in VOICES {
            validate_voice_id(voice.id).unwrap();
            assert!(ids.insert(voice.id));
            assert!(voice.model.url.contains(PIPER_VOICES_REVISION));
            assert!(voice.config.url.contains(PIPER_VOICES_REVISION));
            assert!(voice.card.url.contains(PIPER_VOICES_REVISION));
            for artifact in [voice.model, voice.config, voice.card] {
                assert!(artifact.url.starts_with("https://"));
                assert_eq!(artifact.sha256.len(), 64);
                assert!(artifact.sha256.bytes().all(|byte| byte.is_ascii_hexdigit()));
                assert!(artifact.bytes > 0);
            }
        }
    }

    #[test]
    fn archive_paths_cannot_escape_the_staging_root() {
        for unsafe_path in [
            "",
            "/piper/file",
            "../piper/file",
            "piper/../escape",
            "other/file",
        ] {
            assert!(!safe_archive_path(unsafe_path), "accepted {unsafe_path:?}");
        }
        assert!(safe_archive_path("piper/piper"));
        assert!(safe_archive_path("piper/espeak-ng-data/de_dict"));
    }

    #[cfg(unix)]
    #[test]
    fn in_process_extractor_accepts_only_contained_relative_symlinks() {
        use std::os::unix::fs::PermissionsExt as _;

        let temporary = tempfile::tempdir().expect("temporary archive root");
        let archive = temporary.path().join("safe.tar.gz");
        build_test_archive(&archive, |builder| {
            append_test_directory(builder, "piper");
            append_test_directory(builder, "piper/lib");
            append_test_file(builder, "piper/lib/library.so", b"library");
            append_test_symlink(builder, "piper/library.so", "lib/library.so");
        });
        let destination = temporary.path().join("safe-output");

        extract_test_archive(&archive, &destination, ArchiveLimits::ENGINE)
            .expect("safe archive extraction");

        assert_eq!(
            std::fs::read_link(destination.join("piper/library.so")).unwrap(),
            Path::new("lib/library.so")
        );
        assert_eq!(
            std::fs::read(destination.join("piper/library.so")).unwrap(),
            b"library"
        );
        assert_ne!(
            std::fs::metadata(destination.join("piper/lib/library.so"))
                .unwrap()
                .permissions()
                .mode()
                & 0o100,
            0
        );

        let unsafe_archive = temporary.path().join("unsafe-link.tar.gz");
        build_test_archive(&unsafe_archive, |builder| {
            append_test_directory(builder, "piper");
            append_test_symlink(builder, "piper/escape", "../outside");
        });
        let unsafe_destination = temporary.path().join("unsafe-link-output");
        assert!(
            extract_test_archive(&unsafe_archive, &unsafe_destination, ArchiveLimits::ENGINE)
                .is_err()
        );
        assert!(!temporary.path().join("outside").exists());
    }

    #[cfg(unix)]
    #[test]
    fn in_process_extractor_never_follows_a_staged_symlink() {
        let temporary = tempfile::tempdir().expect("temporary archive root");
        let archive = temporary.path().join("symlink-parent.tar.gz");
        build_test_archive(&archive, |builder| {
            append_test_directory(builder, "piper");
            append_test_directory(builder, "piper/real");
            append_test_symlink(builder, "piper/link", "real");
            append_test_file(builder, "piper/link/escaped", b"must-not-be-written");
        });
        let destination = temporary.path().join("symlink-parent-output");

        assert!(extract_test_archive(&archive, &destination, ArchiveLimits::ENGINE).is_err());
        assert!(!destination.join("piper/real/escaped").exists());
    }

    #[test]
    fn in_process_extractor_rejects_traversal_duplicates_and_special_files() {
        let temporary = tempfile::tempdir().expect("temporary archive root");

        let traversal = temporary.path().join("traversal.tar.gz");
        build_test_archive(&traversal, |builder| {
            append_test_directory(builder, "piper");
            append_raw_test_file(builder, b"piper/../escape", b"escape");
        });
        let traversal_output = temporary.path().join("traversal-output");
        assert!(
            extract_test_archive(&traversal, &traversal_output, ArchiveLimits::ENGINE).is_err()
        );
        assert!(!temporary.path().join("escape").exists());

        let absolute = temporary.path().join("absolute.tar.gz");
        build_test_archive(&absolute, |builder| {
            append_test_directory(builder, "piper");
            append_raw_test_file(builder, b"/piper/escape", b"escape");
        });
        let absolute_output = temporary.path().join("absolute-output");
        assert!(extract_test_archive(&absolute, &absolute_output, ArchiveLimits::ENGINE).is_err());

        let duplicate = temporary.path().join("duplicate.tar.gz");
        build_test_archive(&duplicate, |builder| {
            append_test_directory(builder, "piper");
            append_test_file(builder, "piper/repeated", b"first");
            append_raw_test_file(builder, b"piper/./repeated", b"second");
        });
        let duplicate_output = temporary.path().join("duplicate-output");
        assert!(
            extract_test_archive(&duplicate, &duplicate_output, ArchiveLimits::ENGINE).is_err()
        );

        let special = temporary.path().join("special.tar.gz");
        build_test_archive(&special, |builder| {
            append_test_directory(builder, "piper");
            append_test_special(builder, "piper/fifo");
        });
        let special_output = temporary.path().join("special-output");
        assert!(extract_test_archive(&special, &special_output, ArchiveLimits::ENGINE).is_err());
        assert!(!special_output.join("piper/fifo").exists());

        let hardlink = temporary.path().join("hardlink.tar.gz");
        build_test_archive(&hardlink, |builder| {
            append_test_directory(builder, "piper");
            append_test_file(builder, "piper/original", b"original");
            append_test_hardlink(builder, "piper/hardlink", "piper/original");
        });
        let hardlink_output = temporary.path().join("hardlink-output");
        assert!(extract_test_archive(&hardlink, &hardlink_output, ArchiveLimits::ENGINE).is_err());
        assert!(!hardlink_output.join("piper/hardlink").exists());
    }

    #[test]
    fn in_process_extractor_enforces_entry_and_uncompressed_size_limits() {
        let temporary = tempfile::tempdir().expect("temporary archive root");
        let archive = temporary.path().join("limits.tar.gz");
        build_test_archive(&archive, |builder| {
            append_test_directory(builder, "piper");
            append_test_file(builder, "piper/payload", b"1234");
        });

        let entry_output = temporary.path().join("entry-limit-output");
        assert!(
            extract_test_archive(
                &archive,
                &entry_output,
                ArchiveLimits {
                    entries: 1,
                    uncompressed_bytes: 4,
                },
            )
            .is_err()
        );

        let byte_output = temporary.path().join("byte-limit-output");
        assert!(
            extract_test_archive(
                &archive,
                &byte_output,
                ArchiveLimits {
                    entries: 2,
                    uncompressed_bytes: 3,
                },
            )
            .is_err()
        );
        assert!(!byte_output.join("piper/payload").exists());
    }

    #[test]
    fn voice_ids_cannot_address_arbitrary_paths() {
        for unsafe_id in [
            "",
            ".hidden",
            "../escape",
            "voice/file",
            "voice\\file",
            "ümlaut",
        ] {
            assert!(
                validate_voice_id(unsafe_id).is_err(),
                "accepted {unsafe_id:?}"
            );
        }
        assert!(validate_voice_id("de_DE-thorsten-medium").is_ok());
    }

    #[tokio::test]
    async fn ownership_marker_is_required_before_removal() {
        let temporary = tempfile::TempDir::new().unwrap();
        let parent = temporary.path().join("voices");
        let target = parent.join("de_DE-thorsten-medium");
        std::fs::create_dir_all(&target).unwrap();
        assert!(
            remove_owned_tree(&target, &parent, VOICE_KIND, "de_DE-thorsten-medium")
                .await
                .is_err()
        );
        assert!(target.exists());
    }

    #[tokio::test]
    async fn exact_owned_voice_tree_can_be_removed_without_touching_siblings() {
        let temporary = tempfile::TempDir::new().unwrap();
        let parent = temporary.path().join("voices");
        let target = parent.join("de_DE-thorsten-medium");
        let sibling = parent.join("keep-me");
        std::fs::create_dir_all(&target).unwrap();
        std::fs::create_dir(&sibling).unwrap();
        write_marker(&target, &OwnershipMarker::voice(&VOICES[0]))
            .await
            .unwrap();

        remove_owned_tree(&target, &parent, VOICE_KIND, VOICES[0].id)
            .await
            .unwrap();

        assert!(!target.exists());
        assert!(sibling.exists());
    }

    #[test]
    fn voice_marker_requires_exact_revision_license_and_confirmation() {
        let spec = &VOICES[0];
        let marker = OwnershipMarker::voice(spec);
        assert!(voice_marker_matches(&marker, spec));

        let mut wrong_revision = marker.clone();
        wrong_revision.revision = Some("different-revision".to_owned());
        assert!(!voice_marker_matches(&wrong_revision, spec));
        assert!(marker_owns(&wrong_revision, VOICE_KIND, spec.id));

        let mut wrong_license = marker.clone();
        wrong_license.license = Some("different-license".to_owned());
        assert!(!voice_marker_matches(&wrong_license, spec));
        assert!(marker_owns(&wrong_license, VOICE_KIND, spec.id));

        let mut unconfirmed = marker;
        unconfirmed.license_confirmed_at = None;
        assert!(!voice_marker_matches(&unconfirmed, spec));
        assert!(marker_owns(&unconfirmed, VOICE_KIND, spec.id));
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn atomic_promotion_never_replaces_an_existing_target() {
        let temporary = tempfile::tempdir().expect("temporary promotion root");
        let source = temporary.path().join("source");
        let target = temporary.path().join("target");
        std::fs::create_dir(&source).expect("source directory");
        std::fs::write(source.join("candidate"), b"candidate").expect("candidate file");
        std::fs::create_dir(&target).expect("existing target directory");
        std::fs::write(target.join("unowned"), b"keep").expect("unowned target file");

        assert!(promote_directory_no_replace(&source, &target).is_err());
        assert_eq!(std::fs::read(target.join("unowned")).unwrap(), b"keep");
        assert!(source.join("candidate").exists());

        let unused_target = temporary.path().join("unused-target");
        promote_directory_no_replace(&source, &unused_target).expect("new target promotion");
        assert!(unused_target.join("candidate").exists());
        assert!(!source.exists());
    }

    #[cfg(all(target_os = "linux", target_arch = "x86_64"))]
    #[tokio::test]
    async fn owned_incomplete_voice_is_recoverable_but_unowned_collision_is_preserved() {
        let temporary = tempfile::tempdir().expect("temporary Piper data root");
        let voices = temporary.path().join("managed-providers/piper/voices");
        let target = voices.join(VOICES[0].id);
        tokio::fs::create_dir_all(&target)
            .await
            .expect("incomplete voice root");
        let mut incomplete_marker = OwnershipMarker::voice(&VOICES[0]);
        incomplete_marker.license_confirmed_at = None;
        write_marker(&target, &incomplete_marker)
            .await
            .expect("owned incomplete marker");
        let manager = PiperManager::initialize(&test_config(temporary.path()))
            .await
            .expect("Piper manager initializes");

        let view = manager.view().await;
        assert!(view.installed_voices.is_empty());
        assert!(!view.catalog[0].installed);
        assert_eq!(view.voice_issues.len(), 1);
        assert_eq!(
            view.voice_issues[0].status,
            PiperInstallerStatus::Incomplete
        );
        assert!(view.voice_issues[0].removable);
        manager.state.lock().await.installed = true;
        assert!(
            manager
                .start_voice_download(VOICES[0].id, true)
                .await
                .is_err()
        );
        manager
            .remove_voice(VOICES[0].id, true, false)
            .await
            .expect("owned incomplete voice removal");
        assert!(!target.exists());
        assert!(manager.view().await.voice_issues.is_empty());

        tokio::fs::create_dir(&target)
            .await
            .expect("unowned collision root");
        let manager = PiperManager::initialize(&test_config(temporary.path()))
            .await
            .expect("Piper manager reinitializes");
        let view = manager.view().await;
        assert_eq!(view.voice_issues.len(), 1);
        assert_eq!(
            view.voice_issues[0].status,
            PiperInstallerStatus::UnsafeFilesystem
        );
        assert!(!view.voice_issues[0].removable);
        manager.state.lock().await.installed = true;
        assert!(
            manager
                .start_voice_download(VOICES[0].id, true)
                .await
                .is_err()
        );
        assert!(
            manager
                .remove_voice(VOICES[0].id, true, false)
                .await
                .is_err()
        );
        assert!(target.exists());
    }

    #[test]
    fn pinned_archive_identity_is_exact() {
        assert_eq!(PIPER_VERSION, "1.2.0");
        assert_eq!(PIPER_ARCHIVE_BYTES, 25_916_047);
        assert_eq!(
            PIPER_ARCHIVE_SHA256,
            "467c17935d2a22dcce9dc9e08ba07485e29be813097e7cf08c5627aa09d32e42"
        );
    }

    #[cfg(all(target_os = "linux", target_arch = "x86_64"))]
    #[tokio::test]
    async fn incomplete_owned_engine_can_be_removed_but_is_never_overwritten() {
        let temporary = tempfile::tempdir().expect("temporary Piper data root");
        let engine = temporary.path().join("managed-providers/piper/engine");
        tokio::fs::create_dir_all(&engine)
            .await
            .expect("incomplete engine root");
        write_marker(&engine, &OwnershipMarker::engine())
            .await
            .expect("ownership marker");
        let manager = PiperManager::initialize(&test_config(temporary.path()))
            .await
            .expect("Piper manager initializes");

        let view = manager.view().await;
        assert!(!view.installed);
        assert_eq!(view.installer_status, PiperInstallerStatus::Incomplete);
        assert!(manager.start_install().await.is_err());

        let uninstall = manager
            .start_uninstall()
            .await
            .expect("owned incomplete engine removal starts");
        let uninstall = wait_for_operation(&manager, uninstall.id).await;
        assert_eq!(uninstall.state, PiperOperationState::Succeeded);
        assert!(!engine.exists());
        assert_eq!(
            manager.view().await.installer_status,
            PiperInstallerStatus::Ready
        );
    }

    #[cfg(all(target_os = "linux", target_arch = "x86_64"))]
    #[tokio::test]
    async fn installed_engine_can_be_uninstalled_when_installer_is_unavailable() {
        use std::os::unix::fs::PermissionsExt as _;

        let temporary = tempfile::tempdir().expect("temporary Piper data root");
        let managed_root = temporary.path().join("managed-providers/piper");
        let engine = managed_root.join("engine");
        let runtime = engine.join("piper");
        tokio::fs::create_dir_all(runtime.join("espeak-ng-data"))
            .await
            .expect("fake Piper runtime root");
        for required in [
            runtime.join("piper"),
            runtime.join("libonnxruntime.so.1.14.1"),
            runtime.join("libpiper_phonemize.so"),
        ] {
            tokio::fs::write(&required, b"test")
                .await
                .expect("fake Piper runtime file");
        }
        let mut permissions = std::fs::metadata(runtime.join("piper"))
            .expect("fake Piper executable metadata")
            .permissions();
        permissions.set_mode(0o700);
        std::fs::set_permissions(runtime.join("piper"), permissions)
            .expect("fake Piper executable permissions");
        write_marker(&engine, &OwnershipMarker::engine())
            .await
            .expect("ownership marker");
        let retained_voice = managed_root.join("voices/retained-by-uninstall");
        tokio::fs::create_dir_all(&retained_voice)
            .await
            .expect("retained voice directory");
        tokio::fs::write(retained_voice.join("voice.onnx"), b"voice")
            .await
            .expect("retained voice file");

        let mut manager = PiperManager::initialize(&test_config(temporary.path()))
            .await
            .expect("Piper manager initializes");
        assert!(manager.view().await.installed);
        manager.installer_status = PiperInstallerStatus::PayloadMissing;
        assert_eq!(
            manager.view().await.installer_status,
            PiperInstallerStatus::PayloadMissing
        );

        let uninstall = manager
            .start_uninstall()
            .await
            .expect("uninstall starts without installer readiness");
        let uninstall = wait_for_operation(&manager, uninstall.id).await;
        assert_eq!(uninstall.state, PiperOperationState::Succeeded);
        assert!(!engine.exists());
        assert!(retained_voice.join("voice.onnx").exists());
    }

    /// Opt-in release smoke test for the real pinned network artifacts and native CLI.
    #[cfg(all(target_os = "linux", target_arch = "x86_64"))]
    #[tokio::test]
    #[ignore = "downloads about 90 MiB from the pinned public Piper sources"]
    async fn live_install_download_synthesize_and_uninstall_retains_voice() {
        use std::sync::Arc;

        use audiobookai_core::PerformanceSettings;
        use audiobookai_providers::{
            AudioFormat, SynthesisRequest, TtsProvider,
            adapters::{PiperTtsConfig, PiperTtsProvider, TokioNativeCommandRunner},
        };

        let temporary = tempfile::tempdir().expect("temporary Piper data root");
        let manager = PiperManager::initialize(&test_config(temporary.path()))
            .await
            .expect("Piper manager initializes");

        let install = manager.start_install().await.expect("install starts");
        let install = wait_for_operation(&manager, install.id).await;
        assert_eq!(install.state, PiperOperationState::Succeeded, "{install:?}");

        let download = manager
            .start_voice_download(VOICES[0].id, true)
            .await
            .expect("voice download starts");
        let download = wait_for_operation(&manager, download.id).await;
        assert_eq!(
            download.state,
            PiperOperationState::Succeeded,
            "{download:?}"
        );

        let provider = PiperTtsProvider::new(
            PiperTtsConfig::new(
                manager.executable_path(),
                manager.voices_dir(),
                VOICES[0].id,
            )
            .expect("managed Piper paths"),
            Arc::new(TokioNativeCommandRunner),
        )
        .expect("Piper adapter");
        assert!(provider.health().await.expect("Piper health").available);
        assert_eq!(
            provider
                .discover_models()
                .await
                .expect("installed model discovery")[0]
                .id,
            VOICES[0].id
        );
        let response = provider
            .synthesize(SynthesisRequest {
                request_id: Uuid::new_v4(),
                text: "Dies ist ein kurzer AudiobookAI-Test.".to_owned(),
                model: Some(VOICES[0].id.to_owned()),
                voice: VOICES[0].id.to_owned(),
                format: AudioFormat::Wav,
                performance: PerformanceSettings::default(),
                options: BTreeMap::new(),
                pronunciation_dictionary_ids: Vec::new(),
            })
            .await
            .expect("real Piper synthesis");
        assert!(response.audio.len() > 44);
        assert_eq!(&response.audio[..4], b"RIFF");
        assert_eq!(&response.audio[8..12], b"WAVE");

        let uninstall = manager.start_uninstall().await.expect("uninstall starts");
        let uninstall = wait_for_operation(&manager, uninstall.id).await;
        assert_eq!(
            uninstall.state,
            PiperOperationState::Succeeded,
            "{uninstall:?}"
        );
        let final_view = manager.view().await;
        assert!(!final_view.installed);
        assert_eq!(final_view.installed_voices.len(), 1);
    }
}
