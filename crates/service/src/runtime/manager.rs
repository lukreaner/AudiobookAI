use std::{
    collections::BTreeMap,
    fmt,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
};

use audiobookai_providers::{
    CancellationFlag, CharacterProvider, Model, ModelDownloadProgressSink, ModelDownloadRequest,
    ModelDownloadStatus, OwnedProcessHandle, ProcessLogLine, ProcessState, ProcessStatus,
    ProviderControl, ProviderError, ProviderId, ProviderModelInfo, TtsProvider, Voice,
    VoiceCloneProvider,
};
use tokio::sync::{Mutex, RwLock};

use super::{
    CredentialMaterial, ProviderAdapterBundle, ProviderAdapterFactory, RuntimeAdapterKind,
    RuntimeProfile,
};

#[derive(Debug, thiserror::Error)]
pub enum RuntimeError {
    #[error(transparent)]
    Provider(#[from] ProviderError),
    #[error("provider runtime profile {0} is not registered")]
    ProfileNotFound(ProviderId),
    #[error("provider runtime profile {0} is already registered")]
    ProfileExists(ProviderId),
    #[error("provider runtime profile {0} has no process control")]
    ProcessControlUnavailable(ProviderId),
    #[error("provider runtime profile {0} has no model control")]
    ModelControlUnavailable(ProviderId),
    #[error("provider runtime profile {0} is still running")]
    ProcessStillRunning(ProviderId),
    #[error("provider runtime profile {0} is not a native TTS provider")]
    NotNativeTts(ProviderId),
    #[error("provider runtime profile {0} has no TTS adapter")]
    TtsUnavailable(ProviderId),
    #[error("the provider runtime is shutting down and cannot start another process")]
    ShuttingDown,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ManagedProcessView {
    pub handle: Option<OwnedProcessHandle>,
    pub status: ProcessStatus,
}

impl ManagedProcessView {
    fn stopped() -> Self {
        Self {
            handle: None,
            status: ProcessStatus {
                state: ProcessState::Stopped,
                operating_system_pid: None,
                exit_code: None,
            },
        }
    }
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct ShutdownReport {
    pub stopped: usize,
    pub already_stopped: usize,
    pub failures: BTreeMap<ProviderId, String>,
}

struct RuntimeEntry {
    profile: RuntimeProfile,
    adapters: ProviderAdapterBundle,
    process_spec: Option<audiobookai_providers::ProcessSpec>,
    owned_handle: Mutex<Option<OwnedProcessHandle>>,
}

impl fmt::Debug for RuntimeEntry {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("RuntimeEntry")
            .field("profile", &self.profile)
            .field("adapters", &self.adapters)
            .field("has_process_spec", &self.process_spec.is_some())
            .field("owned_handle", &"<runtime-owned>")
            .finish()
    }
}

/// Registry and ownership boundary for configured provider instances.
#[derive(Clone, Debug)]
pub struct ProviderRuntime {
    factory: ProviderAdapterFactory,
    entries: Arc<RwLock<BTreeMap<ProviderId, Arc<RuntimeEntry>>>>,
    accepting_process_starts: Arc<AtomicBool>,
}

impl ProviderRuntime {
    pub fn new(factory: ProviderAdapterFactory) -> Self {
        Self {
            factory,
            entries: Arc::new(RwLock::new(BTreeMap::new())),
            accepting_process_starts: Arc::new(AtomicBool::new(true)),
        }
    }

    pub fn production() -> Result<Self, RuntimeError> {
        Ok(Self::new(ProviderAdapterFactory::production()?))
    }

    pub async fn register(
        &self,
        profile: RuntimeProfile,
        credential: Option<&CredentialMaterial>,
    ) -> Result<(), RuntimeError> {
        let adapters = self.factory.build(&profile, credential)?;
        let process_spec = profile.process_spec()?;
        let id = profile.id.clone();
        let entry = Arc::new(RuntimeEntry {
            profile,
            adapters,
            process_spec,
            owned_handle: Mutex::new(None),
        });
        let mut entries = self.entries.write().await;
        if entries.contains_key(&id) {
            return Err(RuntimeError::ProfileExists(id));
        }
        entries.insert(id, entry);
        Ok(())
    }

    pub async fn unregister(&self, id: &ProviderId) -> Result<(), RuntimeError> {
        let entry = self.entry(id).await?;
        if entry.owned_handle.lock().await.is_some() {
            return Err(RuntimeError::ProcessStillRunning(id.clone()));
        }
        self.entries.write().await.remove(id);
        Ok(())
    }

    pub async fn profile(&self, id: &ProviderId) -> Result<RuntimeProfile, RuntimeError> {
        Ok(self.entry(id).await?.profile.clone())
    }

    pub async fn profile_ids(&self) -> Vec<ProviderId> {
        self.entries.read().await.keys().cloned().collect()
    }

    pub async fn tts(&self, id: &ProviderId) -> Result<Arc<dyn TtsProvider>, RuntimeError> {
        self.entry(id)
            .await?
            .adapters
            .tts
            .clone()
            .ok_or_else(|| RuntimeError::TtsUnavailable(id.clone()))
    }

    pub async fn character(
        &self,
        id: &ProviderId,
    ) -> Result<Arc<dyn CharacterProvider>, RuntimeError> {
        self.entry(id).await?.adapters.character.clone().ok_or({
            RuntimeError::Provider(ProviderError::Unsupported {
                feature: "character detection",
            })
        })
    }

    /// Discovers selectable synthesis or character-detection models for a registered profile.
    pub async fn discover_models(&self, id: &ProviderId) -> Result<Vec<Model>, RuntimeError> {
        let entry = self.entry(id).await?;
        discover_adapter_models(&entry.adapters).await
    }

    /// Builds an unregistered adapter and discovers its models without persisting credentials or
    /// creating process ownership. This is used only by the provider setup preview.
    pub async fn preview_models(
        &self,
        profile: &RuntimeProfile,
        credential: Option<&CredentialMaterial>,
    ) -> Result<Vec<Model>, RuntimeError> {
        let adapters = self.factory.build(profile, credential)?;
        discover_adapter_models(&adapters).await
    }

    pub async fn voice_cloner(
        &self,
        id: &ProviderId,
    ) -> Result<Arc<dyn VoiceCloneProvider>, RuntimeError> {
        self.entry(id).await?.adapters.voice_cloner.clone().ok_or({
            RuntimeError::Provider(ProviderError::Unsupported {
                feature: "voice cloning",
            })
        })
    }

    pub async fn start(&self, id: &ProviderId) -> Result<OwnedProcessHandle, RuntimeError> {
        self.ensure_accepting_process_starts()?;
        let entry = self.entry(id).await?;
        let control = process_control(&entry, id)?;
        let spec = entry
            .process_spec
            .clone()
            .ok_or_else(|| RuntimeError::ProcessControlUnavailable(id.clone()))?;
        let mut owned = entry.owned_handle.lock().await;
        self.ensure_accepting_process_starts()?;
        if let Some(handle) = owned.as_ref() {
            let status = control.status(handle).await?;
            if matches!(status.state, ProcessState::Running | ProcessState::Starting) {
                return Ok(handle.clone());
            }
            control.stop(handle).await?;
            *owned = None;
        }
        let handle = control.start(spec).await?;
        *owned = Some(handle.clone());
        Ok(handle)
    }

    pub async fn status(&self, id: &ProviderId) -> Result<ManagedProcessView, RuntimeError> {
        let entry = self.entry(id).await?;
        let Some(control) = entry.adapters.control.as_ref() else {
            return Ok(ManagedProcessView::stopped());
        };
        let owned = entry.owned_handle.lock().await;
        let Some(handle) = owned.as_ref() else {
            return Ok(ManagedProcessView::stopped());
        };
        Ok(ManagedProcessView {
            handle: Some(handle.clone()),
            status: control.status(handle).await?,
        })
    }

    pub async fn stop(&self, id: &ProviderId) -> Result<bool, RuntimeError> {
        let entry = self.entry(id).await?;
        let control = process_control(&entry, id)?;
        let mut owned = entry.owned_handle.lock().await;
        let Some(handle) = owned.as_ref() else {
            return Ok(false);
        };
        control.stop(handle).await?;
        *owned = None;
        Ok(true)
    }

    pub async fn restart(&self, id: &ProviderId) -> Result<OwnedProcessHandle, RuntimeError> {
        self.ensure_accepting_process_starts()?;
        let entry = self.entry(id).await?;
        let control = process_control(&entry, id)?;
        let mut owned = entry.owned_handle.lock().await;
        self.ensure_accepting_process_starts()?;
        if let Some(handle) = owned.as_ref() {
            let restarted = control.restart(handle).await?;
            *owned = Some(restarted.clone());
            return Ok(restarted);
        }
        let spec = entry
            .process_spec
            .clone()
            .ok_or_else(|| RuntimeError::ProcessControlUnavailable(id.clone()))?;
        let started = control.start(spec).await?;
        *owned = Some(started.clone());
        Ok(started)
    }

    pub async fn logs(
        &self,
        id: &ProviderId,
        limit: usize,
    ) -> Result<Vec<ProcessLogLine>, RuntimeError> {
        let entry = self.entry(id).await?;
        let control = process_control(&entry, id)?;
        let owned = entry.owned_handle.lock().await;
        let Some(handle) = owned.as_ref() else {
            return Ok(Vec::new());
        };
        Ok(control.logs(handle, limit).await?)
    }

    pub async fn load_model(&self, id: &ProviderId, model: &str) -> Result<(), RuntimeError> {
        let entry = self.entry(id).await?;
        model_control(&entry, id)?.load_model(model).await?;
        Ok(())
    }

    pub async fn unload_model(&self, id: &ProviderId, model: &str) -> Result<(), RuntimeError> {
        let entry = self.entry(id).await?;
        model_control(&entry, id)?.unload_model(model).await?;
        Ok(())
    }

    pub async fn switch_model(&self, id: &ProviderId, model: &str) -> Result<(), RuntimeError> {
        let entry = self.entry(id).await?;
        model_control(&entry, id)?.switch_model(model).await?;
        Ok(())
    }

    pub async fn list_models(
        &self,
        id: &ProviderId,
    ) -> Result<Vec<ProviderModelInfo>, RuntimeError> {
        let entry = self.entry(id).await?;
        Ok(model_control(&entry, id)?.list_models().await?)
    }

    pub async fn download_model(
        &self,
        id: &ProviderId,
        request: ModelDownloadRequest,
        cancellation: CancellationFlag,
        progress: Arc<dyn ModelDownloadProgressSink>,
    ) -> Result<ModelDownloadStatus, RuntimeError> {
        let entry = self.entry(id).await?;
        Ok(model_control(&entry, id)?
            .download_model(request, cancellation, progress)
            .await?)
    }

    pub async fn model_download_status(
        &self,
        id: &ProviderId,
        job_id: &str,
    ) -> Result<ModelDownloadStatus, RuntimeError> {
        let entry = self.entry(id).await?;
        Ok(model_control(&entry, id)?
            .model_download_status(job_id)
            .await?)
    }

    pub async fn delete_model(
        &self,
        id: &ProviderId,
        model: &str,
        confirmed: bool,
        in_use: bool,
    ) -> Result<(), RuntimeError> {
        let entry = self.entry(id).await?;
        model_control(&entry, id)?
            .delete_model(model, confirmed, in_use)
            .await?;
        Ok(())
    }

    pub async fn discover_voices(&self, id: &ProviderId) -> Result<Vec<Voice>, RuntimeError> {
        Ok(self.tts(id).await?.discover_voices().await?)
    }

    pub async fn discover_native_voices(
        &self,
        id: &ProviderId,
    ) -> Result<Vec<Voice>, RuntimeError> {
        let entry = self.entry(id).await?;
        if !matches!(entry.profile.adapter, RuntimeAdapterKind::NativeOs) {
            return Err(RuntimeError::NotNativeTts(id.clone()));
        }
        let provider = entry
            .adapters
            .tts
            .clone()
            .ok_or_else(|| RuntimeError::TtsUnavailable(id.clone()))?;
        Ok(provider.discover_voices().await?)
    }

    /// Stops every child for which this runtime still holds the ownership handle.
    pub async fn shutdown_owned(&self) -> ShutdownReport {
        self.accepting_process_starts
            .store(false, Ordering::Release);
        let entries = self
            .entries
            .read()
            .await
            .values()
            .cloned()
            .collect::<Vec<_>>();
        let mut report = ShutdownReport::default();
        for entry in entries {
            let id = entry.profile.id.clone();
            let Some(control) = entry.adapters.control.as_ref() else {
                report.already_stopped = report.already_stopped.saturating_add(1);
                continue;
            };
            let mut owned = entry.owned_handle.lock().await;
            let Some(handle) = owned.as_ref() else {
                report.already_stopped = report.already_stopped.saturating_add(1);
                continue;
            };
            match control.stop(handle).await {
                Ok(()) => {
                    *owned = None;
                    report.stopped = report.stopped.saturating_add(1);
                }
                Err(error) => {
                    report.failures.insert(id, error.to_string());
                }
            }
        }
        report
    }

    fn ensure_accepting_process_starts(&self) -> Result<(), RuntimeError> {
        if self.accepting_process_starts.load(Ordering::Acquire) {
            Ok(())
        } else {
            Err(RuntimeError::ShuttingDown)
        }
    }

    async fn entry(&self, id: &ProviderId) -> Result<Arc<RuntimeEntry>, RuntimeError> {
        self.entries
            .read()
            .await
            .get(id)
            .cloned()
            .ok_or_else(|| RuntimeError::ProfileNotFound(id.clone()))
    }
}

async fn discover_adapter_models(
    adapters: &ProviderAdapterBundle,
) -> Result<Vec<Model>, RuntimeError> {
    if let Some(provider) = &adapters.tts {
        return Ok(provider.discover_models().await?);
    }
    if let Some(provider) = &adapters.character {
        return Ok(provider.discover_models().await?);
    }
    Err(RuntimeError::Provider(ProviderError::Unsupported {
        feature: "model discovery",
    }))
}

fn process_control<'a>(
    entry: &'a RuntimeEntry,
    id: &ProviderId,
) -> Result<&'a Arc<dyn ProviderControl>, RuntimeError> {
    if entry.process_spec.is_none() {
        return Err(RuntimeError::ProcessControlUnavailable(id.clone()));
    }
    entry
        .adapters
        .control
        .as_ref()
        .ok_or_else(|| RuntimeError::ProcessControlUnavailable(id.clone()))
}

fn model_control<'a>(
    entry: &'a RuntimeEntry,
    id: &ProviderId,
) -> Result<&'a Arc<dyn ProviderControl>, RuntimeError> {
    if entry.profile.effective_model_control().is_none() {
        return Err(RuntimeError::ModelControlUnavailable(id.clone()));
    }
    entry
        .adapters
        .control
        .as_ref()
        .ok_or_else(|| RuntimeError::ModelControlUnavailable(id.clone()))
}

#[cfg(test)]
mod tests {
    use std::{collections::BTreeMap, path::PathBuf, sync::Arc, time::Duration};

    use async_trait::async_trait;
    use audiobookai_providers::{
        HttpRequest, HttpResponse, HttpTransport, ManagedProcessSupervisor, ProviderKind,
        adapters::TokioNativeCommandRunner,
    };
    use bytes::Bytes;
    use url::Url;

    use super::*;

    #[derive(Debug)]
    struct AvailableModelsTransport;

    #[async_trait]
    impl HttpTransport for AvailableModelsTransport {
        async fn execute(
            &self,
            request: HttpRequest,
        ) -> audiobookai_providers::Result<HttpResponse> {
            assert_eq!(request.url.as_str(), "http://127.0.0.1:19090/v1/models");
            Ok(HttpResponse {
                status: 200,
                headers: BTreeMap::new(),
                body: Bytes::from_static(
                    br#"{"data":[{"id":"gpt-5.6-luna"},{"id":"gpt-4.1-mini"}]}"#,
                ),
            })
        }
    }

    #[tokio::test]
    async fn previews_available_models_without_registering_the_profile() {
        let runtime = ProviderRuntime::new(ProviderAdapterFactory::new(
            Arc::new(AvailableModelsTransport),
            ManagedProcessSupervisor::default(),
            Arc::new(TokioNativeCommandRunner),
        ));
        let mut profile = RuntimeProfile::new(
            ProviderId::new("model-preview").unwrap(),
            "Model preview",
            RuntimeAdapterKind::OpenAi,
            ProviderKind::ExternalEndpoint,
        );
        profile.endpoint = Some(Url::parse("http://127.0.0.1:19090/").unwrap());

        let models = runtime.preview_models(&profile, None).await.unwrap();

        assert_eq!(
            models
                .iter()
                .map(|model| model.id.as_str())
                .collect::<Vec<_>>(),
            ["gpt-5.6-luna", "gpt-4.1-mini"]
        );
        assert!(runtime.profile_ids().await.is_empty());
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn controls_only_the_process_handle_it_owns() {
        let runtime = ProviderRuntime::new(ProviderAdapterFactory::default());
        let id = ProviderId::new("managed-test").unwrap();
        let mut profile = RuntimeProfile::new(
            id.clone(),
            "Managed test",
            RuntimeAdapterKind::LocalAi,
            ProviderKind::ManagedChild,
        );
        profile.endpoint = Some(Url::parse("http://127.0.0.1:19091/").unwrap());
        profile.executable = Some(PathBuf::from("/bin/sh"));
        profile.arguments = vec!["-c".to_owned(), "echo runtime-ready; sleep 30".to_owned()];
        runtime.register(profile, None).await.unwrap();

        let handle = runtime.start(&id).await.unwrap();
        assert_eq!(runtime.start(&id).await.unwrap(), handle);
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert!(
            runtime
                .logs(&id, 20)
                .await
                .unwrap()
                .iter()
                .any(|line| line.line == "runtime-ready")
        );
        assert!(runtime.stop(&id).await.unwrap());
        assert!(!runtime.stop(&id).await.unwrap());
        assert_eq!(
            runtime.status(&id).await.unwrap(),
            ManagedProcessView::stopped()
        );
    }

    #[tokio::test]
    async fn rejects_duplicate_runtime_ids() {
        let runtime = ProviderRuntime::new(ProviderAdapterFactory::default());
        let id = ProviderId::new("openai-test").unwrap();
        let profile = RuntimeProfile::new(
            id.clone(),
            "OpenAI",
            RuntimeAdapterKind::OpenAi,
            ProviderKind::CloudRemote,
        );
        let credential = CredentialMaterial::new(b"test-only".to_vec());
        runtime
            .register(profile.clone(), Some(&credential))
            .await
            .unwrap();
        assert!(matches!(
            runtime.register(profile, Some(&credential)).await,
            Err(RuntimeError::ProfileExists(existing)) if existing == id
        ));
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn shutdown_permanently_closes_the_managed_process_start_gate() {
        let runtime = ProviderRuntime::new(ProviderAdapterFactory::default());
        let id = ProviderId::new("managed-shutdown-test").unwrap();
        let mut profile = RuntimeProfile::new(
            id.clone(),
            "Managed shutdown test",
            RuntimeAdapterKind::LocalAi,
            ProviderKind::ManagedChild,
        );
        profile.endpoint = Some(Url::parse("http://127.0.0.1:19092/").unwrap());
        profile.executable = Some(PathBuf::from("/bin/sh"));
        profile.arguments = vec!["-c".to_owned(), "sleep 30".to_owned()];
        runtime.register(profile, None).await.unwrap();

        let report = runtime.shutdown_owned().await;
        assert_eq!(report.already_stopped, 1);
        assert!(matches!(
            runtime.start(&id).await,
            Err(RuntimeError::ShuttingDown)
        ));
    }
}
