use std::{
    collections::BTreeMap,
    fmt,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};

use async_trait::async_trait;
use audiobookai_providers::{
    CancellationFlag, ModelDownloadProgressSink, ModelDownloadRequest, ModelDownloadState,
    ModelDownloadStatus, ProviderError, ProviderId, ProviderModelInfo,
};
use chrono::{DateTime, Utc};
use serde::Serialize;
use tokio::sync::RwLock;
use uuid::Uuid;

use crate::{
    EventHub, ServiceError,
    runtime::{NoopRetryJournal, ProviderRuntime, RetryPolicy, RuntimeError, execute_with_retry},
};

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ProviderModelOperationState {
    Running,
    Cancelling,
    Succeeded,
    Failed,
    Cancelled,
}

impl ProviderModelOperationState {
    pub const fn is_terminal(self) -> bool {
        matches!(self, Self::Succeeded | Self::Failed | Self::Cancelled)
    }
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ProviderModelOperationView {
    pub id: Uuid,
    pub provider_profile_id: Uuid,
    pub model: String,
    pub state: ProviderModelOperationState,
    pub downloaded_bytes: Option<u64>,
    pub total_size_bytes: Option<u64>,
    pub bytes_per_second: Option<f64>,
    pub progress_percent: Option<u8>,
    pub detail_code: Option<String>,
    pub started_at: DateTime<Utc>,
    pub finished_at: Option<DateTime<Utc>>,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ProviderModelView {
    pub id: String,
    pub name: String,
    pub size_bytes: Option<u64>,
    pub format: Option<String>,
    pub family: Option<String>,
    pub parameter_size: Option<String>,
    pub quantization: Option<String>,
    pub loaded_instances: Vec<String>,
}

impl From<ProviderModelInfo> for ProviderModelView {
    fn from(model: ProviderModelInfo) -> Self {
        Self {
            id: model.id,
            name: model.name,
            size_bytes: model.size_bytes,
            format: model.format,
            family: model.family,
            parameter_size: model.parameter_size,
            quantization: model.quantization,
            loaded_instances: model.loaded_instances,
        }
    }
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ProviderModelLibraryView {
    pub models: Vec<ProviderModelView>,
    pub models_error_code: Option<String>,
    pub operations: Vec<ProviderModelOperationView>,
}

#[derive(Clone)]
struct OperationEntry {
    view: ProviderModelOperationView,
    cancellation: CancellationFlag,
}

impl fmt::Debug for OperationEntry {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("OperationEntry")
            .field("view", &self.view)
            .field("cancellation", &"<cancellation flag>")
            .finish()
    }
}

#[derive(Clone, Debug)]
pub struct ProviderModelManager {
    providers: ProviderRuntime,
    events: EventHub,
    operations: Arc<RwLock<BTreeMap<Uuid, OperationEntry>>>,
    accepting_operations: Arc<AtomicBool>,
}

impl ProviderModelManager {
    pub fn new(providers: ProviderRuntime, events: EventHub) -> Self {
        Self {
            providers,
            events,
            operations: Arc::new(RwLock::new(BTreeMap::new())),
            accepting_operations: Arc::new(AtomicBool::new(true)),
        }
    }

    pub async fn library(
        &self,
        provider_id: Uuid,
    ) -> Result<ProviderModelLibraryView, ServiceError> {
        let runtime_id = runtime_provider_id(provider_id)?;
        let (models, models_error_code) =
            if let Ok(models) = self.providers.list_models(&runtime_id).await {
                (
                    models.into_iter().map(ProviderModelView::from).collect(),
                    None,
                )
            } else {
                tracing::warn!(
                    diagnostic_code = "provider.model.list.failed",
                    provider_profile_id = %provider_id,
                    "Provider model listing failed; preserving local operation visibility"
                );
                (
                    Vec::new(),
                    Some("provider_model_list_unavailable".to_owned()),
                )
            };
        Ok(ProviderModelLibraryView {
            models,
            models_error_code,
            operations: self.operations(provider_id).await,
        })
    }

    pub async fn operations(&self, provider_id: Uuid) -> Vec<ProviderModelOperationView> {
        let mut operations = self
            .operations
            .read()
            .await
            .values()
            .filter(|entry| entry.view.provider_profile_id == provider_id)
            .map(|entry| entry.view.clone())
            .collect::<Vec<_>>();
        operations.sort_by_key(|operation| std::cmp::Reverse(operation.started_at));
        operations
    }

    pub async fn start_download(
        &self,
        provider_id: Uuid,
        request: ModelDownloadRequest,
    ) -> Result<ProviderModelOperationView, ServiceError> {
        if !self.accepting_operations.load(Ordering::Acquire) {
            return Err(ServiceError::Conflict(
                "provider model operations are unavailable while AudiobookAI is shutting down"
                    .to_owned(),
            ));
        }
        let runtime_id = runtime_provider_id(provider_id)?;
        let cancellation = CancellationFlag::default();
        let operation = ProviderModelOperationView {
            id: Uuid::new_v4(),
            provider_profile_id: provider_id,
            model: request.model.clone(),
            state: ProviderModelOperationState::Running,
            downloaded_bytes: None,
            total_size_bytes: None,
            bytes_per_second: None,
            progress_percent: None,
            detail_code: None,
            started_at: Utc::now(),
            finished_at: None,
        };
        {
            let mut operations = self.operations.write().await;
            if !self.accepting_operations.load(Ordering::Acquire) {
                return Err(ServiceError::Conflict(
                    "provider model operations are unavailable while AudiobookAI is shutting down"
                        .to_owned(),
                ));
            }
            if operations.values().any(|entry| {
                entry.view.provider_profile_id == provider_id && !entry.view.state.is_terminal()
            }) {
                return Err(ServiceError::Conflict(
                    "this provider already has an active model download".to_owned(),
                ));
            }
            operations.insert(
                operation.id,
                OperationEntry {
                    view: operation.clone(),
                    cancellation: cancellation.clone(),
                },
            );
        }
        self.publish(&operation);
        let manager = self.clone();
        let operation_id = operation.id;
        tokio::spawn(async move {
            manager
                .run_download(operation_id, runtime_id, request, cancellation)
                .await;
        });
        Ok(operation)
    }

    pub async fn cancel(
        &self,
        provider_id: Uuid,
        operation_id: Uuid,
    ) -> Result<ProviderModelOperationView, ServiceError> {
        let updated = {
            let mut operations = self.operations.write().await;
            let entry = operations
                .get_mut(&operation_id)
                .filter(|entry| entry.view.provider_profile_id == provider_id)
                .ok_or(ServiceError::NotFound)?;
            if entry.view.state.is_terminal() {
                return Err(ServiceError::Conflict(
                    "the model download is already complete".to_owned(),
                ));
            }
            entry.cancellation.cancel();
            entry.view.state = ProviderModelOperationState::Cancelling;
            entry.view.detail_code = Some("cancellation_best_effort".to_owned());
            entry.view.clone()
        };
        self.publish(&updated);
        Ok(updated)
    }

    /// Cancels every model-download operation initiated by this service.
    pub async fn shutdown_owned(&self) -> usize {
        self.accepting_operations.store(false, Ordering::Release);
        {
            let mut operations = self.operations.write().await;
            for entry in operations.values_mut() {
                if !entry.view.state.is_terminal() {
                    entry.cancellation.cancel();
                    entry.view.state = ProviderModelOperationState::Cancelling;
                    entry.view.detail_code = Some("shutdown_requested".to_owned());
                }
            }
        }
        for _ in 0..100 {
            let remaining = self
                .operations
                .read()
                .await
                .values()
                .filter(|entry| !entry.view.state.is_terminal())
                .count();
            if remaining == 0 {
                return 0;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        self.operations
            .read()
            .await
            .values()
            .filter(|entry| !entry.view.state.is_terminal())
            .count()
    }

    pub async fn delete_model(
        &self,
        provider_id: Uuid,
        model: &str,
        confirmed: bool,
        in_use: bool,
    ) -> Result<(), ServiceError> {
        self.providers
            .delete_model(&runtime_provider_id(provider_id)?, model, confirmed, in_use)
            .await
            .map_err(provider_request_error)
    }

    async fn run_download(
        &self,
        operation_id: Uuid,
        provider_id: ProviderId,
        request: ModelDownloadRequest,
        cancellation: CancellationFlag,
    ) {
        let sink: Arc<dyn ModelDownloadProgressSink> = Arc::new(OperationProgressSink {
            manager: self.clone(),
            operation_id,
        });
        let result = self
            .providers
            .download_model(&provider_id, request, cancellation.clone(), sink)
            .await;
        if cancellation.is_cancelled() {
            self.finish_cancelled(operation_id).await;
            return;
        }
        match result {
            Ok(status) => {
                self.apply_status(operation_id, &status).await;
                if matches!(
                    status.state,
                    ModelDownloadState::Downloading | ModelDownloadState::Paused
                ) {
                    if let Some(job_id) = status.job_id {
                        self.poll_provider_download(
                            operation_id,
                            provider_id,
                            job_id,
                            cancellation,
                        )
                        .await;
                    } else {
                        self.finish_failed(operation_id, "provider_status_missing")
                            .await;
                    }
                }
            }
            Err(_) if cancellation.is_cancelled() => {
                self.finish_cancelled(operation_id).await;
            }
            Err(error) => {
                tracing::warn!(
                    diagnostic_code = "provider.model.download.failed",
                    operation_id = %operation_id,
                    error = %error,
                    "Provider model download failed"
                );
                self.finish_failed(operation_id, "provider_request_failed")
                    .await;
            }
        }
    }

    async fn poll_provider_download(
        &self,
        operation_id: Uuid,
        provider_id: ProviderId,
        job_id: String,
        cancellation: CancellationFlag,
    ) {
        loop {
            if cancellation.is_cancelled() {
                self.finish_cancelled(operation_id).await;
                return;
            }
            tokio::time::sleep(Duration::from_millis(750)).await;
            let status_result =
                execute_with_retry(&RetryPolicy::default(), &NoopRetryJournal, |_| async {
                    self.providers
                        .model_download_status(&provider_id, &job_id)
                        .await
                        .map_err(|error| match error {
                            RuntimeError::Provider(source) => source,
                            _ => ProviderError::Configuration(
                                "provider model status is unavailable".to_owned(),
                            ),
                        })
                })
                .await;
            match status_result {
                Ok(execution) => {
                    if execution.attempts.get() > 1 {
                        tracing::info!(
                            diagnostic_code = "provider.model.download_status.recovered",
                            operation_id = %operation_id,
                            attempts = execution.attempts.get(),
                            "Provider model download status recovered after a transient failure"
                        );
                    }
                    let status = execution.value;
                    let terminal = matches!(
                        status.state,
                        ModelDownloadState::Completed
                            | ModelDownloadState::AlreadyDownloaded
                            | ModelDownloadState::Failed
                            | ModelDownloadState::Cancelled
                    );
                    self.apply_status(operation_id, &status).await;
                    if terminal {
                        return;
                    }
                }
                Err(error) => {
                    tracing::warn!(
                        diagnostic_code = "provider.model.download_status.failed",
                        operation_id = %operation_id,
                        failure_class = ?error.failure_class(),
                        attempts = error.attempts().get(),
                        "Provider model download status failed"
                    );
                    self.finish_failed(operation_id, "provider_status_failed")
                        .await;
                    return;
                }
            }
        }
    }

    async fn apply_status(&self, operation_id: Uuid, status: &ModelDownloadStatus) {
        let updated = {
            let mut operations = self.operations.write().await;
            let Some(entry) = operations.get_mut(&operation_id) else {
                return;
            };
            entry.view.downloaded_bytes = status.downloaded_bytes;
            entry.view.total_size_bytes = status.total_size_bytes;
            entry.view.bytes_per_second = status.bytes_per_second;
            entry.view.progress_percent = progress_percent(status);
            entry.view.state = match status.state {
                ModelDownloadState::Downloading | ModelDownloadState::Paused => {
                    ProviderModelOperationState::Running
                }
                ModelDownloadState::Completed | ModelDownloadState::AlreadyDownloaded => {
                    ProviderModelOperationState::Succeeded
                }
                ModelDownloadState::Failed => ProviderModelOperationState::Failed,
                ModelDownloadState::Cancelled => ProviderModelOperationState::Cancelled,
            };
            if entry.view.state.is_terminal() {
                entry.view.finished_at = Some(Utc::now());
                entry.view.detail_code = match entry.view.state {
                    ProviderModelOperationState::Failed => {
                        Some("provider_reported_failure".to_owned())
                    }
                    _ => None,
                };
            }
            entry.view.clone()
        };
        self.publish(&updated);
    }

    async fn finish_cancelled(&self, operation_id: Uuid) {
        self.finish(
            operation_id,
            ProviderModelOperationState::Cancelled,
            Some("remote_download_may_continue"),
        )
        .await;
    }

    async fn finish_failed(&self, operation_id: Uuid, detail_code: &'static str) {
        self.finish(
            operation_id,
            ProviderModelOperationState::Failed,
            Some(detail_code),
        )
        .await;
    }

    async fn finish(
        &self,
        operation_id: Uuid,
        state: ProviderModelOperationState,
        detail_code: Option<&str>,
    ) {
        let updated = {
            let mut operations = self.operations.write().await;
            let Some(entry) = operations.get_mut(&operation_id) else {
                return;
            };
            entry.view.state = state;
            entry.view.detail_code = detail_code.map(ToOwned::to_owned);
            entry.view.finished_at = Some(Utc::now());
            entry.view.clone()
        };
        self.publish(&updated);
    }

    fn publish(&self, operation: &ProviderModelOperationView) {
        self.events.publish(
            "provider.model_operation.updated",
            serde_json::json!({
                "operationId": operation.id,
                "providerProfileId": operation.provider_profile_id,
                "state": operation.state,
            }),
        );
    }
}

#[derive(Clone, Debug)]
struct OperationProgressSink {
    manager: ProviderModelManager,
    operation_id: Uuid,
}

#[async_trait]
impl ModelDownloadProgressSink for OperationProgressSink {
    async fn update(&self, status: ModelDownloadStatus) -> audiobookai_providers::Result<()> {
        self.manager.apply_status(self.operation_id, &status).await;
        Ok(())
    }
}

fn runtime_provider_id(provider_id: Uuid) -> Result<ProviderId, ServiceError> {
    ProviderId::new(provider_id.to_string())
        .map_err(|error| ServiceError::Internal(error.to_string()))
}

fn provider_request_error(error: crate::runtime::RuntimeError) -> ServiceError {
    use audiobookai_providers::ProviderError;
    match error {
        crate::runtime::RuntimeError::Provider(ProviderError::Configuration(detail)) => {
            ServiceError::InvalidRequest(detail)
        }
        crate::runtime::RuntimeError::Provider(ProviderError::Unsupported { feature }) => {
            ServiceError::Conflict(format!("provider does not support {feature}"))
        }
        crate::runtime::RuntimeError::ProfileNotFound(_) => ServiceError::NotFound,
        other => ServiceError::Conflict(other.to_string()),
    }
}

fn progress_percent(status: &ModelDownloadStatus) -> Option<u8> {
    let completed = u128::from(status.downloaded_bytes?);
    let total = u128::from(status.total_size_bytes?);
    if total == 0 {
        return None;
    }
    u8::try_from((completed.saturating_mul(100) / total).min(100)).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn progress_is_bounded_and_unknown_without_a_total() {
        let mut status = ModelDownloadStatus {
            job_id: None,
            state: ModelDownloadState::Downloading,
            downloaded_bytes: Some(25),
            total_size_bytes: Some(100),
            bytes_per_second: Some(1.5),
            started_at: None,
            estimated_completion: None,
            completed_at: None,
        };
        assert_eq!(progress_percent(&status), Some(25));
        status.downloaded_bytes = Some(200);
        assert_eq!(progress_percent(&status), Some(100));
        status.total_size_bytes = None;
        assert_eq!(progress_percent(&status), None);
    }

    #[tokio::test]
    async fn shutdown_cancels_every_app_owned_model_operation() {
        let manager = ProviderModelManager::new(
            ProviderRuntime::new(crate::runtime::ProviderAdapterFactory::default()),
            EventHub::new(8),
        );
        let operation_id = Uuid::new_v4();
        let provider_id = Uuid::new_v4();
        let cancellation = CancellationFlag::default();
        manager.operations.write().await.insert(
            operation_id,
            OperationEntry {
                view: ProviderModelOperationView {
                    id: operation_id,
                    provider_profile_id: provider_id,
                    model: "public-model".to_owned(),
                    state: ProviderModelOperationState::Running,
                    downloaded_bytes: None,
                    total_size_bytes: None,
                    bytes_per_second: None,
                    progress_percent: None,
                    detail_code: None,
                    started_at: Utc::now(),
                    finished_at: None,
                },
                cancellation: cancellation.clone(),
            },
        );
        let completion = manager.clone();
        tokio::spawn(async move {
            while !cancellation.is_cancelled() {
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
            completion.finish_cancelled(operation_id).await;
        });

        assert_eq!(manager.shutdown_owned().await, 0);
        assert!(!manager.accepting_operations.load(Ordering::Acquire));
        let operations = manager.operations(provider_id).await;
        assert_eq!(operations[0].state, ProviderModelOperationState::Cancelled);
    }
}
