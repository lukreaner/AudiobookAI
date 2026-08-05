use std::{fmt, sync::Arc, time::Duration};

use bytes::Bytes;
use chrono::{DateTime, Utc};
use futures::StreamExt;
use serde::Deserialize;
use url::Url;

use crate::{
    CancellationFlag, EndpointConfig, HttpMethod, HttpRequest, HttpTransport,
    ModelDownloadProgressSink, ModelDownloadRequest, ModelDownloadState, ModelDownloadStatus,
    ProviderError, ProviderModelInfo, Result, contains_secret_shaped_value,
};

const REQUEST_TIMEOUT: Duration = Duration::from_secs(120);
const DOWNLOAD_TIMEOUT: Duration = Duration::from_secs(24 * 60 * 60);
const MAX_STREAM_EVENT_BYTES: usize = 64 * 1024;
const MAX_MODEL_IDENTIFIER_BYTES: usize = 512;
const MAX_JOB_IDENTIFIER_BYTES: usize = 128;
const MAX_LOCAL_AI_GALLERY_BYTES: usize = 8 * 1024 * 1024;
const MAX_LOCAL_AI_SYSTEM_BYTES: usize = 1024 * 1024;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ModelControlProtocol {
    Ollama,
    LmStudio,
    LocalAi,
}

#[derive(Clone)]
pub(crate) struct HttpModelLibraryClient {
    protocol: ModelControlProtocol,
    endpoint: EndpointConfig,
    transport: Arc<dyn HttpTransport>,
}

pub(crate) fn validate_runtime_model_identifier(
    protocol: ModelControlProtocol,
    value: &str,
) -> Result<()> {
    match protocol {
        ModelControlProtocol::Ollama => validate_ollama_model_identifier(value),
        ModelControlProtocol::LmStudio => validate_lm_runtime_identifier(value),
        ModelControlProtocol::LocalAi => validate_local_ai_installed_identifier(value),
    }
}

impl fmt::Debug for HttpModelLibraryClient {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("HttpModelLibraryClient")
            .field("protocol", &self.protocol)
            .field("endpoint", &self.endpoint)
            .field("transport", &"<http transport>")
            .finish()
    }
}

impl HttpModelLibraryClient {
    pub(crate) fn new(
        protocol: ModelControlProtocol,
        endpoint: EndpointConfig,
        transport: Arc<dyn HttpTransport>,
    ) -> Self {
        Self {
            protocol,
            endpoint,
            transport,
        }
    }

    pub(crate) async fn list_models(&self) -> Result<Vec<ProviderModelInfo>> {
        match self.protocol {
            ModelControlProtocol::Ollama => self.list_ollama_models().await,
            ModelControlProtocol::LmStudio => self.list_lm_studio_models().await,
            ModelControlProtocol::LocalAi => self.list_local_ai_models().await,
        }
    }

    pub(crate) async fn download_model(
        &self,
        request: ModelDownloadRequest,
        cancellation: CancellationFlag,
        progress: Arc<dyn ModelDownloadProgressSink>,
    ) -> Result<ModelDownloadStatus> {
        match self.protocol {
            ModelControlProtocol::Ollama => {
                self.pull_ollama_model(request, cancellation, progress)
                    .await
            }
            ModelControlProtocol::LmStudio => {
                self.download_lm_studio_model(request, cancellation, progress)
                    .await
            }
            ModelControlProtocol::LocalAi => {
                self.download_local_ai_model(request, cancellation, progress)
                    .await
            }
        }
    }

    pub(crate) async fn download_status(&self, job_id: &str) -> Result<ModelDownloadStatus> {
        validate_job_identifier(job_id)?;
        match self.protocol {
            ModelControlProtocol::LmStudio => {
                let response = self
                    .execute(
                        HttpMethod::Get,
                        &format!("api/v1/models/download/status/{job_id}"),
                        None,
                        REQUEST_TIMEOUT,
                    )
                    .await?;
                parse_lm_download_status(&response.body)
            }
            ModelControlProtocol::LocalAi => {
                let response = self
                    .execute(
                        HttpMethod::Get,
                        &format!("models/jobs/{job_id}"),
                        None,
                        REQUEST_TIMEOUT,
                    )
                    .await?;
                parse_local_ai_download_status(job_id, &response.body)
            }
            ModelControlProtocol::Ollama => Err(ProviderError::Unsupported {
                feature: "model download status",
            }),
        }
    }

    pub(crate) async fn delete_model(
        &self,
        model: &str,
        confirmed: bool,
        in_use: bool,
    ) -> Result<()> {
        match self.protocol {
            ModelControlProtocol::Ollama => validate_ollama_model_identifier(model)?,
            ModelControlProtocol::LocalAi => validate_local_ai_installed_identifier(model)?,
            ModelControlProtocol::LmStudio => {
                return Err(ProviderError::Unsupported {
                    feature: "model deletion",
                });
            }
        }
        if !confirmed {
            return Err(ProviderError::Configuration(
                "model deletion requires explicit confirmation".to_owned(),
            ));
        }
        if in_use {
            return Err(ProviderError::Configuration(
                "a model assigned to an active project or job cannot be deleted".to_owned(),
            ));
        }
        match self.protocol {
            ModelControlProtocol::Ollama => {
                if self.ollama_model_is_loaded(model).await? {
                    return Err(ProviderError::Configuration(
                        "a loaded model must be unloaded before deletion".to_owned(),
                    ));
                }
                self.execute(
                    HttpMethod::Delete,
                    "api/delete",
                    Some(&serde_json::json!({ "model": model })),
                    REQUEST_TIMEOUT,
                )
                .await?;
            }
            ModelControlProtocol::LocalAi => {
                if self.local_ai_model_is_loaded(model).await? {
                    return Err(ProviderError::Configuration(
                        "a loaded model must be unloaded before deletion".to_owned(),
                    ));
                }
                let encoded = percent_encode_path_segment(model);
                self.execute(
                    HttpMethod::Post,
                    &format!("models/delete/{encoded}"),
                    None,
                    REQUEST_TIMEOUT,
                )
                .await?;
            }
            ModelControlProtocol::LmStudio => unreachable!("handled above"),
        }
        Ok(())
    }

    async fn list_ollama_models(&self) -> Result<Vec<ProviderModelInfo>> {
        let response = self
            .execute(HttpMethod::Get, "api/tags", None, REQUEST_TIMEOUT)
            .await?;
        let payload: OllamaTagsResponse = serde_json::from_slice(&response.body).map_err(|_| {
            ProviderError::InvalidResponse("Ollama returned invalid model metadata".to_owned())
        })?;
        payload.models.into_iter().map(ollama_model_info).collect()
    }

    async fn list_lm_studio_models(&self) -> Result<Vec<ProviderModelInfo>> {
        let response = self
            .execute(HttpMethod::Get, "api/v1/models", None, REQUEST_TIMEOUT)
            .await?;
        let payload: LmModelsResponse = serde_json::from_slice(&response.body).map_err(|_| {
            ProviderError::InvalidResponse("LM Studio returned invalid model metadata".to_owned())
        })?;
        payload.models.into_iter().map(lm_model_info).collect()
    }

    async fn list_local_ai_models(&self) -> Result<Vec<ProviderModelInfo>> {
        let response = self
            .execute(HttpMethod::Get, "v1/models", None, REQUEST_TIMEOUT)
            .await?;
        let payload: LocalAiModelsResponse =
            serde_json::from_slice(&response.body).map_err(|_| {
                ProviderError::InvalidResponse("LocalAI returned invalid model metadata".to_owned())
            })?;
        let loaded_models = self.local_ai_loaded_models().await?;
        payload
            .data
            .into_iter()
            .map(|model| {
                validate_local_ai_installed_identifier(&model.id).map_err(|_| {
                    ProviderError::InvalidResponse(
                        "LocalAI returned an invalid model identifier".to_owned(),
                    )
                })?;
                let loaded_instances = loaded_models
                    .iter()
                    .filter(|loaded| local_ai_model_identifiers_match(&model.id, loaded))
                    .cloned()
                    .collect();
                Ok(ProviderModelInfo {
                    name: model.id.clone(),
                    id: model.id,
                    size_bytes: None,
                    format: None,
                    family: None,
                    parameter_size: None,
                    quantization: None,
                    loaded_instances,
                })
            })
            .collect()
    }

    async fn pull_ollama_model(
        &self,
        request: ModelDownloadRequest,
        cancellation: CancellationFlag,
        progress: Arc<dyn ModelDownloadProgressSink>,
    ) -> Result<ModelDownloadStatus> {
        validate_ollama_model_identifier(&request.model)?;
        if request.quantization.is_some() {
            return Err(ProviderError::Configuration(
                "Ollama pull does not accept a quantization override".to_owned(),
            ));
        }
        if cancellation.is_cancelled() {
            progress.update(ModelDownloadStatus::cancelled()).await?;
            return Err(ProviderError::Cancelled);
        }

        let mut http_request = HttpRequest::json(
            HttpMethod::Post,
            self.endpoint.endpoint("api/pull")?,
            &serde_json::json!({
                "model": request.model,
                "insecure": false,
                "stream": true
            }),
        )?;
        http_request.timeout = DOWNLOAD_TIMEOUT;
        self.endpoint
            .authentication
            .apply(&mut http_request.headers);
        let mut response = self.transport.execute_stream(http_request).await?;
        let mut pending = Vec::new();
        let mut last = None;

        while let Some(chunk) = response.body.next().await {
            if cancellation.is_cancelled() {
                progress.update(ModelDownloadStatus::cancelled()).await?;
                return Err(ProviderError::Cancelled);
            }
            pending.extend_from_slice(&chunk?);
            if pending.len() > MAX_STREAM_EVENT_BYTES && !pending.contains(&b'\n') {
                return Err(ProviderError::InvalidResponse(
                    "Ollama returned an oversized progress event".to_owned(),
                ));
            }
            while let Some(newline) = pending.iter().position(|byte| *byte == b'\n') {
                let line = pending.drain(..=newline).collect::<Vec<_>>();
                if let Some(status) = parse_ollama_progress_line(&line)? {
                    progress.update(status.clone()).await?;
                    last = Some(status);
                }
            }
            if pending.len() > MAX_STREAM_EVENT_BYTES {
                return Err(ProviderError::InvalidResponse(
                    "Ollama returned an oversized progress event".to_owned(),
                ));
            }
        }
        if !pending.iter().all(u8::is_ascii_whitespace)
            && let Some(status) = parse_ollama_progress_line(&pending)?
        {
            progress.update(status.clone()).await?;
            last = Some(status);
        }
        match last {
            Some(status) if status.state == ModelDownloadState::Completed => Ok(status),
            _ => Err(ProviderError::InvalidResponse(
                "Ollama download ended without a success event".to_owned(),
            )),
        }
    }

    async fn download_lm_studio_model(
        &self,
        request: ModelDownloadRequest,
        cancellation: CancellationFlag,
        progress: Arc<dyn ModelDownloadProgressSink>,
    ) -> Result<ModelDownloadStatus> {
        let source = validate_lm_download_identifier(&request.model)?;
        if request.quantization.is_some() && source != LmDownloadSource::HuggingFace {
            return Err(ProviderError::Configuration(
                "LM Studio quantization may only be selected for a Hugging Face repository URL"
                    .to_owned(),
            ));
        }
        if let Some(quantization) = request.quantization.as_deref() {
            validate_symbolic_value(quantization, 64, "quantization")?;
        }
        if cancellation.is_cancelled() {
            progress.update(ModelDownloadStatus::cancelled()).await?;
            return Err(ProviderError::Cancelled);
        }
        let body = match request.quantization {
            Some(quantization) => {
                serde_json::json!({ "model": request.model, "quantization": quantization })
            }
            None => serde_json::json!({ "model": request.model }),
        };
        let response = self
            .execute(
                HttpMethod::Post,
                "api/v1/models/download",
                Some(&body),
                REQUEST_TIMEOUT,
            )
            .await?;
        let status = parse_lm_download_status(&response.body)?;
        progress.update(status.clone()).await?;
        Ok(status)
    }

    async fn download_local_ai_model(
        &self,
        request: ModelDownloadRequest,
        cancellation: CancellationFlag,
        progress: Arc<dyn ModelDownloadProgressSink>,
    ) -> Result<ModelDownloadStatus> {
        validate_local_ai_gallery_identifier(&request.model)?;
        if request.quantization.is_some() {
            return Err(ProviderError::Configuration(
                "LocalAI gallery installation does not accept a quantization override".to_owned(),
            ));
        }
        if cancellation.is_cancelled() {
            progress.update(ModelDownloadStatus::cancelled()).await?;
            return Err(ProviderError::Cancelled);
        }
        let available = self
            .execute(HttpMethod::Get, "models/available", None, REQUEST_TIMEOUT)
            .await?;
        require_local_ai_gallery_entry(&request.model, &available.body)?;
        let response = self
            .execute(
                HttpMethod::Post,
                "models/apply",
                Some(&serde_json::json!({ "id": request.model })),
                REQUEST_TIMEOUT,
            )
            .await?;
        let started: LocalAiDownloadStarted =
            serde_json::from_slice(&response.body).map_err(|_| {
                ProviderError::InvalidResponse(
                    "LocalAI returned invalid download status".to_owned(),
                )
            })?;
        validate_job_identifier(&started.uuid).map_err(|_| {
            ProviderError::InvalidResponse(
                "LocalAI returned an invalid download job identifier".to_owned(),
            )
        })?;
        let status = ModelDownloadStatus {
            job_id: Some(started.uuid),
            state: ModelDownloadState::Downloading,
            downloaded_bytes: None,
            total_size_bytes: None,
            bytes_per_second: None,
            started_at: None,
            estimated_completion: None,
            completed_at: None,
        };
        progress.update(status.clone()).await?;
        Ok(status)
    }

    async fn ollama_model_is_loaded(&self, model: &str) -> Result<bool> {
        let response = self
            .execute(HttpMethod::Get, "api/ps", None, REQUEST_TIMEOUT)
            .await?;
        let payload: OllamaRunningResponse =
            serde_json::from_slice(&response.body).map_err(|_| {
                ProviderError::InvalidResponse(
                    "Ollama returned invalid loaded-model metadata".to_owned(),
                )
            })?;
        let expected = canonical_ollama_model_identifier(model);
        Ok(payload.models.iter().any(|item| {
            item.model
                .as_deref()
                .or(item.name.as_deref())
                .filter(|candidate| validate_ollama_model_identifier(candidate).is_ok())
                .is_some_and(|candidate| canonical_ollama_model_identifier(candidate) == expected)
        }))
    }

    async fn local_ai_loaded_models(&self) -> Result<Vec<String>> {
        let response = self
            .execute(HttpMethod::Get, "system", None, REQUEST_TIMEOUT)
            .await?;
        if response.body.len() > MAX_LOCAL_AI_SYSTEM_BYTES {
            return Err(ProviderError::InvalidResponse(
                "LocalAI returned oversized system metadata".to_owned(),
            ));
        }
        let payload: LocalAiSystemResponse =
            serde_json::from_slice(&response.body).map_err(|_| {
                ProviderError::InvalidResponse(
                    "LocalAI returned invalid loaded-model metadata".to_owned(),
                )
            })?;
        payload
            .loaded_models
            .into_iter()
            .map(|model| {
                validate_local_ai_installed_identifier(&model.id).map_err(|_| {
                    ProviderError::InvalidResponse(
                        "LocalAI returned an invalid loaded-model identifier".to_owned(),
                    )
                })?;
                Ok(model.id)
            })
            .collect()
    }

    async fn local_ai_model_is_loaded(&self, model: &str) -> Result<bool> {
        let loaded_models = self.local_ai_loaded_models().await?;
        Ok(loaded_models
            .iter()
            .any(|loaded| local_ai_model_identifiers_match(model, loaded)))
    }

    async fn execute(
        &self,
        method: HttpMethod,
        path: &str,
        body: Option<&serde_json::Value>,
        timeout: Duration,
    ) -> Result<crate::HttpResponse> {
        let mut request = match body {
            Some(body) => HttpRequest::json(method, self.endpoint.endpoint(path)?, body)?,
            None => HttpRequest {
                method,
                url: self.endpoint.endpoint(path)?,
                headers: Default::default(),
                body: Bytes::new(),
                timeout,
            },
        };
        request.timeout = timeout;
        self.endpoint.authentication.apply(&mut request.headers);
        self.transport.execute(request).await?.require_success()
    }
}

#[derive(Debug, Deserialize)]
struct OllamaTagsResponse {
    models: Vec<OllamaModel>,
}

#[derive(Debug, Deserialize)]
struct OllamaModel {
    name: Option<String>,
    model: Option<String>,
    size: Option<u64>,
    details: Option<OllamaModelDetails>,
}

#[derive(Debug, Deserialize)]
struct OllamaModelDetails {
    format: Option<String>,
    family: Option<String>,
    parameter_size: Option<String>,
    quantization_level: Option<String>,
}

fn ollama_model_info(model: OllamaModel) -> Result<ProviderModelInfo> {
    let id = model.model.or(model.name).ok_or_else(|| {
        ProviderError::InvalidResponse("Ollama model metadata is missing an identifier".to_owned())
    })?;
    validate_ollama_model_identifier(&id).map_err(|_| {
        ProviderError::InvalidResponse("Ollama returned an invalid model identifier".to_owned())
    })?;
    let details = model.details.unwrap_or(OllamaModelDetails {
        format: None,
        family: None,
        parameter_size: None,
        quantization_level: None,
    });
    Ok(ProviderModelInfo {
        name: id.clone(),
        id,
        size_bytes: model.size,
        format: sanitize_symbolic_response(details.format),
        family: sanitize_symbolic_response(details.family),
        parameter_size: sanitize_symbolic_response(details.parameter_size),
        quantization: sanitize_symbolic_response(details.quantization_level),
        loaded_instances: Vec::new(),
    })
}

#[derive(Debug, Deserialize)]
struct OllamaRunningResponse {
    models: Vec<OllamaRunningModel>,
}

#[derive(Debug, Deserialize)]
struct OllamaRunningModel {
    name: Option<String>,
    model: Option<String>,
}

#[derive(Debug, Deserialize)]
struct OllamaPullProgress {
    status: Option<String>,
    total: Option<u64>,
    completed: Option<u64>,
    error: Option<serde_json::Value>,
}

fn parse_ollama_progress_line(line: &[u8]) -> Result<Option<ModelDownloadStatus>> {
    let line = line.strip_suffix(b"\n").unwrap_or(line);
    let line = line.strip_suffix(b"\r").unwrap_or(line);
    if line.iter().all(u8::is_ascii_whitespace) {
        return Ok(None);
    }
    if line.len() > MAX_STREAM_EVENT_BYTES {
        return Err(ProviderError::InvalidResponse(
            "Ollama returned an oversized progress event".to_owned(),
        ));
    }
    let value: OllamaPullProgress = serde_json::from_slice(line).map_err(|_| {
        ProviderError::InvalidResponse("Ollama returned invalid download progress".to_owned())
    })?;
    if value.error.is_some() {
        return Err(ProviderError::InvalidResponse(
            "Ollama reported a model download failure".to_owned(),
        ));
    }
    let state = if value.status.as_deref() == Some("success") {
        ModelDownloadState::Completed
    } else {
        ModelDownloadState::Downloading
    };
    Ok(Some(ModelDownloadStatus {
        job_id: None,
        state,
        downloaded_bytes: value.completed,
        total_size_bytes: value.total,
        bytes_per_second: None,
        started_at: None,
        estimated_completion: None,
        completed_at: None,
    }))
}

#[derive(Debug, Deserialize)]
struct LmModelsResponse {
    models: Vec<LmModel>,
}

#[derive(Debug, Deserialize)]
struct LmModel {
    key: String,
    display_name: String,
    size_bytes: Option<u64>,
    format: Option<String>,
    params_string: Option<String>,
    quantization: Option<LmQuantization>,
    #[serde(default)]
    loaded_instances: Vec<LmLoadedInstance>,
}

#[derive(Debug, Deserialize)]
struct LmQuantization {
    name: Option<String>,
}

#[derive(Debug, Deserialize)]
struct LmLoadedInstance {
    id: String,
}

fn lm_model_info(model: LmModel) -> Result<ProviderModelInfo> {
    validate_lm_runtime_identifier(&model.key).map_err(|_| {
        ProviderError::InvalidResponse("LM Studio returned an invalid model identifier".to_owned())
    })?;
    let loaded_instances = model
        .loaded_instances
        .into_iter()
        .map(|instance| {
            validate_lm_runtime_identifier(&instance.id).map_err(|_| {
                ProviderError::InvalidResponse(
                    "LM Studio returned an invalid loaded-instance identifier".to_owned(),
                )
            })?;
            Ok(instance.id)
        })
        .collect::<Result<Vec<_>>>()?;
    let name = sanitize_display_name(&model.display_name).unwrap_or_else(|| model.key.clone());
    Ok(ProviderModelInfo {
        id: model.key,
        name,
        size_bytes: model.size_bytes,
        format: sanitize_symbolic_response(model.format),
        family: None,
        parameter_size: sanitize_symbolic_response(model.params_string),
        quantization: model
            .quantization
            .and_then(|value| sanitize_symbolic_response(value.name)),
        loaded_instances,
    })
}

#[derive(Debug, Deserialize)]
struct LmDownloadStatus {
    job_id: Option<String>,
    status: String,
    downloaded_bytes: Option<u64>,
    total_size_bytes: Option<u64>,
    bytes_per_second: Option<f64>,
    started_at: Option<DateTime<Utc>>,
    estimated_completion: Option<DateTime<Utc>>,
    completed_at: Option<DateTime<Utc>>,
}

fn parse_lm_download_status(body: &[u8]) -> Result<ModelDownloadStatus> {
    let value: LmDownloadStatus = serde_json::from_slice(body).map_err(|_| {
        ProviderError::InvalidResponse("LM Studio returned invalid download status".to_owned())
    })?;
    let state = match value.status.as_str() {
        "downloading" => ModelDownloadState::Downloading,
        "paused" => ModelDownloadState::Paused,
        "completed" => ModelDownloadState::Completed,
        "failed" => ModelDownloadState::Failed,
        "already_downloaded" => ModelDownloadState::AlreadyDownloaded,
        _ => {
            return Err(ProviderError::InvalidResponse(
                "LM Studio returned an unknown download state".to_owned(),
            ));
        }
    };
    if let Some(job_id) = value.job_id.as_deref() {
        validate_job_identifier(job_id).map_err(|_| {
            ProviderError::InvalidResponse(
                "LM Studio returned an invalid download job identifier".to_owned(),
            )
        })?;
    } else if state != ModelDownloadState::AlreadyDownloaded {
        return Err(ProviderError::InvalidResponse(
            "LM Studio download status is missing its job identifier".to_owned(),
        ));
    }
    if value
        .bytes_per_second
        .is_some_and(|rate| !rate.is_finite() || rate.is_sign_negative())
    {
        return Err(ProviderError::InvalidResponse(
            "LM Studio returned an invalid download rate".to_owned(),
        ));
    }
    Ok(ModelDownloadStatus {
        job_id: value.job_id,
        state,
        downloaded_bytes: value.downloaded_bytes,
        total_size_bytes: value.total_size_bytes,
        bytes_per_second: value.bytes_per_second,
        started_at: value.started_at,
        estimated_completion: value.estimated_completion,
        completed_at: value.completed_at,
    })
}

#[derive(Debug, Deserialize)]
struct LocalAiModelsResponse {
    data: Vec<LocalAiInstalledModel>,
}

#[derive(Debug, Deserialize)]
struct LocalAiInstalledModel {
    id: String,
}

#[derive(Debug, Deserialize)]
struct LocalAiSystemResponse {
    loaded_models: Vec<LocalAiLoadedModel>,
}

#[derive(Debug, Deserialize)]
struct LocalAiLoadedModel {
    id: String,
}

#[derive(Debug, Deserialize)]
struct LocalAiGalleryEntry {
    name: String,
}

#[derive(Debug, Deserialize)]
struct LocalAiDownloadStarted {
    uuid: String,
}

#[derive(Debug, Deserialize)]
struct LocalAiJobStatus {
    #[serde(default)]
    processed: bool,
    error: Option<serde_json::Value>,
}

fn require_local_ai_gallery_entry(model: &str, body: &[u8]) -> Result<()> {
    if body.len() > MAX_LOCAL_AI_GALLERY_BYTES {
        return Err(ProviderError::InvalidResponse(
            "LocalAI returned an oversized gallery response".to_owned(),
        ));
    }
    let entries: Vec<LocalAiGalleryEntry> = serde_json::from_slice(body).map_err(|_| {
        ProviderError::InvalidResponse("LocalAI returned invalid gallery metadata".to_owned())
    })?;
    if entries.iter().any(|entry| entry.name == model) {
        Ok(())
    } else {
        Err(ProviderError::Configuration(
            "the requested LocalAI model is not an exact gallery entry".to_owned(),
        ))
    }
}

fn parse_local_ai_download_status(job_id: &str, body: &[u8]) -> Result<ModelDownloadStatus> {
    let value: LocalAiJobStatus = serde_json::from_slice(body).map_err(|_| {
        ProviderError::InvalidResponse("LocalAI returned invalid download status".to_owned())
    })?;
    let failed = value.error.as_ref().is_some_and(|error| !error.is_null());
    let state = if failed {
        ModelDownloadState::Failed
    } else if value.processed {
        ModelDownloadState::Completed
    } else {
        ModelDownloadState::Downloading
    };
    Ok(ModelDownloadStatus {
        job_id: Some(job_id.to_owned()),
        state,
        downloaded_bytes: None,
        total_size_bytes: None,
        bytes_per_second: None,
        started_at: None,
        estimated_completion: None,
        completed_at: None,
    })
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum LmDownloadSource {
    Catalog,
    HuggingFace,
}

fn validate_ollama_model_identifier(value: &str) -> Result<()> {
    reject_secret_shaped_identifier(value)?;
    if value.is_empty()
        || value.len() > MAX_MODEL_IDENTIFIER_BYTES
        || value != value.trim()
        || value.contains("://")
        || value.contains(['?', '#', '@', '%', '\\'])
        || !value.bytes().all(|byte| {
            byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-' | b'/' | b':')
        })
        || value.starts_with(['/', ':'])
        || value.ends_with(['/', ':'])
        || value
            .split('/')
            .any(|part| part.is_empty() || matches!(part, "." | ".."))
    {
        return Err(ProviderError::Configuration(
            "Ollama model identifier is invalid".to_owned(),
        ));
    }
    Ok(())
}

fn canonical_ollama_model_identifier(value: &str) -> String {
    let final_segment = value.rsplit('/').next().unwrap_or(value);
    if final_segment.contains(':') {
        value.to_owned()
    } else {
        format!("{value}:latest")
    }
}

/// Compares validated Ollama identifiers while treating the implicit and explicit `latest` tag
/// as the same model. Invalid or secret-shaped values are rejected without being echoed.
pub fn ollama_model_identifiers_equal(left: &str, right: &str) -> Result<bool> {
    validate_ollama_model_identifier(left)?;
    validate_ollama_model_identifier(right)?;
    Ok(canonical_ollama_model_identifier(left) == canonical_ollama_model_identifier(right))
}

/// Compares validated `LocalAI` gallery or installed names conservatively. A gallery prefix is
/// ignored so an active `gallery@model` installation also protects the resulting `model` entry.
pub fn local_ai_model_identifiers_equal(left: &str, right: &str) -> Result<bool> {
    validate_local_ai_installed_identifier(left)?;
    validate_local_ai_installed_identifier(right)?;
    Ok(local_ai_model_identifiers_match(left, right))
}

fn local_ai_model_identifiers_match(left: &str, right: &str) -> bool {
    canonical_local_ai_model_identifier(left) == canonical_local_ai_model_identifier(right)
}

fn canonical_local_ai_model_identifier(value: &str) -> &str {
    value.rsplit_once('@').map_or(value, |(_, model)| model)
}

fn validate_lm_download_identifier(value: &str) -> Result<LmDownloadSource> {
    reject_secret_shaped_identifier(value)?;
    if value.starts_with("https://") {
        validate_hugging_face_repository_url(value)?;
        Ok(LmDownloadSource::HuggingFace)
    } else {
        validate_lm_catalog_identifier(value)?;
        Ok(LmDownloadSource::Catalog)
    }
}

fn validate_lm_catalog_identifier(value: &str) -> Result<()> {
    validate_segmented_identifier(value, true, "LM Studio catalog")
}

fn validate_lm_runtime_identifier(value: &str) -> Result<()> {
    reject_secret_shaped_identifier(value)?;
    validate_segmented_identifier(value, false, "LM Studio model")
}

fn validate_local_ai_gallery_identifier(value: &str) -> Result<()> {
    reject_secret_shaped_identifier(value)?;
    let parts = value.split('@').collect::<Vec<_>>();
    if value.is_empty()
        || value.len() > MAX_MODEL_IDENTIFIER_BYTES
        || value != value.trim()
        || !matches!(parts.len(), 1 | 2)
        || parts.iter().any(|part| {
            part.is_empty()
                || part.len() > 255
                || !part.bytes().all(|byte| {
                    byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-' | b':')
                })
                || !part
                    .as_bytes()
                    .first()
                    .is_some_and(u8::is_ascii_alphanumeric)
                || !part
                    .as_bytes()
                    .last()
                    .is_some_and(u8::is_ascii_alphanumeric)
        })
    {
        return Err(ProviderError::Configuration(
            "LocalAI gallery identifier is invalid".to_owned(),
        ));
    }
    Ok(())
}

fn validate_local_ai_installed_identifier(value: &str) -> Result<()> {
    reject_secret_shaped_identifier(value)?;
    if value.is_empty()
        || value.len() > MAX_MODEL_IDENTIFIER_BYTES
        || value != value.trim()
        || value.contains("://")
        || value.contains(['?', '#', '%', '\\'])
        || value.matches('@').count() > 1
        || value.starts_with(['/', '@'])
        || value.ends_with(['/', '@'])
        || !value.bytes().all(|byte| {
            byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-' | b'/' | b':' | b'@')
        })
        || value
            .split('/')
            .any(|part| part.is_empty() || matches!(part, "." | ".."))
    {
        return Err(ProviderError::Configuration(
            "LocalAI model identifier is invalid".to_owned(),
        ));
    }
    Ok(())
}

fn reject_secret_shaped_identifier(value: &str) -> Result<()> {
    if contains_secret_shaped_value(value) {
        return Err(ProviderError::Configuration(
            "model identifier appears to contain sensitive credential material".to_owned(),
        ));
    }
    Ok(())
}

fn percent_encode_path_segment(value: &str) -> String {
    const HEX: &[u8; 16] = b"0123456789ABCDEF";
    let mut encoded = String::with_capacity(value.len());
    for byte in value.bytes() {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'.' | b'_' | b'~') {
            encoded.push(char::from(byte));
        } else {
            encoded.push('%');
            encoded.push(char::from(HEX[usize::from(byte >> 4)]));
            encoded.push(char::from(HEX[usize::from(byte & 0x0f)]));
        }
    }
    encoded
}

fn validate_segmented_identifier(value: &str, exactly_two: bool, label: &str) -> Result<()> {
    let parts = value.split('/').collect::<Vec<_>>();
    let valid_count = if exactly_two {
        parts.len() == 2
    } else {
        matches!(parts.len(), 1 | 2)
    };
    if value.is_empty()
        || value.len() > MAX_MODEL_IDENTIFIER_BYTES
        || value != value.trim()
        || !valid_count
        || parts.iter().any(|part| {
            part.is_empty()
                || part.len() > 255
                || !part
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-'))
                || !part
                    .as_bytes()
                    .first()
                    .is_some_and(u8::is_ascii_alphanumeric)
                || !part
                    .as_bytes()
                    .last()
                    .is_some_and(u8::is_ascii_alphanumeric)
        })
    {
        return Err(ProviderError::Configuration(format!(
            "{label} identifier is invalid"
        )));
    }
    Ok(())
}

fn validate_hugging_face_repository_url(value: &str) -> Result<()> {
    if value.len() > MAX_MODEL_IDENTIFIER_BYTES || value.contains('%') {
        return Err(ProviderError::Configuration(
            "Hugging Face model URL is invalid".to_owned(),
        ));
    }
    let url = Url::parse(value).map_err(|_| {
        ProviderError::Configuration("Hugging Face model URL is invalid".to_owned())
    })?;
    let segments = url
        .path_segments()
        .map(Iterator::collect::<Vec<_>>)
        .unwrap_or_default();
    if url.scheme() != "https"
        || url.host_str() != Some("huggingface.co")
        || url.port().is_some()
        || !url.username().is_empty()
        || url.password().is_some()
        || url.query().is_some()
        || url.fragment().is_some()
        || segments.len() != 2
        || segments.iter().any(|part| {
            part.is_empty()
                || !part
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-'))
                || !part
                    .as_bytes()
                    .first()
                    .is_some_and(u8::is_ascii_alphanumeric)
                || !part
                    .as_bytes()
                    .last()
                    .is_some_and(u8::is_ascii_alphanumeric)
        })
    {
        return Err(ProviderError::Configuration(
            "LM Studio accepts only an exact HTTPS Hugging Face repository URL".to_owned(),
        ));
    }
    Ok(())
}

fn validate_job_identifier(value: &str) -> Result<()> {
    if value.is_empty()
        || value.len() > MAX_JOB_IDENTIFIER_BYTES
        || !value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-'))
    {
        return Err(ProviderError::Configuration(
            "download job identifier is invalid".to_owned(),
        ));
    }
    Ok(())
}

fn validate_symbolic_value(value: &str, maximum: usize, label: &str) -> Result<()> {
    if value.is_empty()
        || value.len() > maximum
        || !value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-'))
    {
        return Err(ProviderError::Configuration(format!(
            "{label} value is invalid"
        )));
    }
    Ok(())
}

fn sanitize_symbolic_response(value: Option<String>) -> Option<String> {
    value.filter(|value| validate_symbolic_value(value, 64, "provider metadata").is_ok())
}

fn sanitize_display_name(value: &str) -> Option<String> {
    (!value.is_empty() && value.len() <= 200 && !value.chars().any(char::is_control))
        .then(|| value.to_owned())
}

#[cfg(test)]
mod tests {
    use std::{collections::BTreeMap, sync::Mutex};

    use async_trait::async_trait;
    use futures::stream;

    use super::*;
    use crate::{Authentication, Credential, HttpResponse, HttpStreamResponse};

    #[derive(Debug, Default)]
    struct RecordingTransport {
        requests: Mutex<Vec<HttpRequest>>,
        responses: Mutex<Vec<HttpResponse>>,
        stream_chunks: Mutex<Vec<Bytes>>,
    }

    impl RecordingTransport {
        fn with_responses(responses: Vec<HttpResponse>) -> Self {
            Self {
                requests: Mutex::new(Vec::new()),
                responses: Mutex::new(responses.into_iter().rev().collect()),
                stream_chunks: Mutex::new(Vec::new()),
            }
        }

        fn with_stream(chunks: Vec<Bytes>) -> Self {
            Self {
                stream_chunks: Mutex::new(chunks),
                ..Self::default()
            }
        }

        fn requests(&self) -> Vec<HttpRequest> {
            self.requests.lock().unwrap().clone()
        }
    }

    #[async_trait]
    impl HttpTransport for RecordingTransport {
        async fn execute(&self, request: HttpRequest) -> Result<HttpResponse> {
            self.requests.lock().unwrap().push(request);
            self.responses
                .lock()
                .unwrap()
                .pop()
                .ok_or_else(|| ProviderError::Transport("missing fake response".to_owned()))
        }

        async fn execute_stream(&self, request: HttpRequest) -> Result<HttpStreamResponse> {
            self.requests.lock().unwrap().push(request);
            let chunks = std::mem::take(&mut *self.stream_chunks.lock().unwrap());
            Ok(HttpStreamResponse {
                status: 200,
                headers: BTreeMap::new(),
                body: Box::pin(stream::iter(chunks.into_iter().map(Ok))),
            })
        }
    }

    #[derive(Debug, Default)]
    struct RecordingProgress(Mutex<Vec<ModelDownloadStatus>>);

    #[async_trait]
    impl ModelDownloadProgressSink for RecordingProgress {
        async fn update(&self, status: ModelDownloadStatus) -> Result<()> {
            self.0.lock().unwrap().push(status);
            Ok(())
        }
    }

    fn response(value: &serde_json::Value) -> HttpResponse {
        HttpResponse {
            status: 200,
            headers: BTreeMap::new(),
            body: serde_json::to_vec(value).unwrap().into(),
        }
    }

    fn response_with_status(status: u16, value: &serde_json::Value) -> HttpResponse {
        HttpResponse {
            status,
            headers: BTreeMap::new(),
            body: serde_json::to_vec(value).unwrap().into(),
        }
    }

    fn endpoint(authentication: Authentication) -> EndpointConfig {
        EndpointConfig::external(
            Url::parse("http://127.0.0.1:11434/").unwrap(),
            authentication,
        )
        .unwrap()
    }

    #[tokio::test]
    async fn ollama_contract_lists_and_pulls_with_secure_registry_policy() {
        let transport = Arc::new(RecordingTransport::with_responses(vec![response(
            &serde_json::json!({
                "models": [{
                    "name": "gemma3:latest", "model": "gemma3:latest", "size": 42,
                    "details": {"format": "gguf", "family": "gemma3", "parameter_size": "4B", "quantization_level": "Q4_K_M"}
                }]
            }),
        )]));
        let client = HttpModelLibraryClient::new(
            ModelControlProtocol::Ollama,
            endpoint(Authentication::None),
            transport.clone(),
        );
        let models = client.list_models().await.unwrap();
        assert_eq!(models[0].id, "gemma3:latest");
        assert_eq!(transport.requests()[0].method, HttpMethod::Get);
        assert_eq!(transport.requests()[0].url.path(), "/api/tags");

        let transport = Arc::new(RecordingTransport::with_stream(vec![
            Bytes::from_static(
                b"{\"status\":\"pulling manifest\"}\n{\"status\":\"downloading\",\"digest\":\"sha256:abcd\",\"total\":100,\"completed\":4}",
            ),
            Bytes::from_static(b"\n{\"status\":\"success\"}\n"),
        ]));
        let client = HttpModelLibraryClient::new(
            ModelControlProtocol::Ollama,
            endpoint(Authentication::None),
            transport.clone(),
        );
        let progress = Arc::new(RecordingProgress::default());
        let status = client
            .download_model(
                ModelDownloadRequest {
                    model: "gemma3:latest".to_owned(),
                    quantization: None,
                },
                CancellationFlag::default(),
                progress.clone(),
            )
            .await
            .unwrap();
        assert_eq!(status.state, ModelDownloadState::Completed);
        assert_eq!(progress.0.lock().unwrap().len(), 3);
        let request = &transport.requests()[0];
        assert_eq!(request.method, HttpMethod::Post);
        assert_eq!(request.url.path(), "/api/pull");
        let body: serde_json::Value = serde_json::from_slice(&request.body).unwrap();
        assert_eq!(body["insecure"], false);
        assert_eq!(body["stream"], true);
    }

    #[tokio::test]
    async fn ollama_delete_requires_confirmation_and_refuses_loaded_or_in_use_models() {
        let client = HttpModelLibraryClient::new(
            ModelControlProtocol::Ollama,
            endpoint(Authentication::None),
            Arc::new(RecordingTransport::default()),
        );
        assert!(client.delete_model("gemma3", false, false).await.is_err());
        assert!(client.delete_model("gemma3", true, true).await.is_err());

        let transport = Arc::new(RecordingTransport::with_responses(vec![response(
            &serde_json::json!({"models": [{"name": "gemma3:latest"}]}),
        )]));
        let client = HttpModelLibraryClient::new(
            ModelControlProtocol::Ollama,
            endpoint(Authentication::None),
            transport.clone(),
        );
        assert!(client.delete_model("gemma3", true, false).await.is_err());
        assert_eq!(transport.requests().len(), 1);
        assert_eq!(transport.requests()[0].url.path(), "/api/ps");

        let transport = Arc::new(RecordingTransport::with_responses(vec![
            response(&serde_json::json!({"models": []})),
            response(&serde_json::json!({})),
        ]));
        let client = HttpModelLibraryClient::new(
            ModelControlProtocol::Ollama,
            endpoint(Authentication::None),
            transport.clone(),
        );
        client.delete_model("gemma3", true, false).await.unwrap();
        let requests = transport.requests();
        assert_eq!(requests.len(), 2);
        assert_eq!(requests[1].method, HttpMethod::Delete);
        assert_eq!(requests[1].url.path(), "/api/delete");
        let body: serde_json::Value = serde_json::from_slice(&requests[1].body).unwrap();
        assert_eq!(body["model"], "gemma3");
    }

    #[tokio::test]
    async fn lm_studio_contract_lists_starts_and_polls_downloads_with_auth() {
        let transport = Arc::new(RecordingTransport::with_responses(vec![
            response(&serde_json::json!({
                "models": [{
                    "type": "llm", "publisher": "lmstudio-community", "key": "gemma-3-270m-it-qat",
                    "display_name": "Gemma 3 270m", "size_bytes": 42, "format": "gguf",
                    "params_string": "270M", "quantization": {"name": "Q4_0"},
                    "loaded_instances": [{"id": "gemma-3-270m-it-qat"}]
                }]
            })),
            response(&serde_json::json!({
                "job_id": "job_example", "status": "downloading", "total_size_bytes": 100,
                "bytes_per_second": 7834.71, "started_at": "2026-01-02T03:04:05Z"
            })),
            response(&serde_json::json!({
                "job_id": "job_example", "status": "completed", "downloaded_bytes": 100,
                "total_size_bytes": 100, "started_at": "2026-01-02T03:04:05Z",
                "completed_at": "2026-01-02T03:05:05Z"
            })),
        ]));
        let client = HttpModelLibraryClient::new(
            ModelControlProtocol::LmStudio,
            endpoint(Authentication::Bearer(Credential::new("fixture-value"))),
            transport.clone(),
        );
        assert_eq!(client.list_models().await.unwrap().len(), 1);
        let started = client
            .download_model(
                ModelDownloadRequest {
                    model: "ibm/granite-4-micro".to_owned(),
                    quantization: None,
                },
                CancellationFlag::default(),
                Arc::new(RecordingProgress::default()),
            )
            .await
            .unwrap();
        assert_eq!(started.job_id.as_deref(), Some("job_example"));
        assert_eq!(started.bytes_per_second, Some(7834.71));
        let completed = client.download_status("job_example").await.unwrap();
        assert_eq!(completed.state, ModelDownloadState::Completed);
        let requests = transport.requests();
        assert_eq!(requests[0].url.path(), "/api/v1/models");
        assert_eq!(requests[1].url.path(), "/api/v1/models/download");
        assert_eq!(
            requests[2].url.path(),
            "/api/v1/models/download/status/job_example"
        );
        assert!(
            requests
                .iter()
                .all(|request| request.headers.contains_key("authorization"))
        );
        let debug = format!("{client:?}");
        assert!(!debug.contains("fixture-value"));
    }

    #[tokio::test]
    async fn local_ai_contract_lists_verifies_gallery_and_polls_without_following_status_url() {
        let transport = Arc::new(RecordingTransport::with_responses(vec![
            response(&serde_json::json!({
                "object": "list",
                "data": [{"id": "voice-model", "object": "model"}]
            })),
            response(&serde_json::json!({
                "backends": ["piper"],
                "loaded_models": [{"id": "voice-model"}]
            })),
            response(&serde_json::json!([
                {"name": "localai@voice-model", "urls": ["ignored"]}
            ])),
            response(&serde_json::json!({
                "uuid": "job-localai",
                "status": "https://untrusted.invalid/models/jobs/other?ignored=true"
            })),
            response(&serde_json::json!({
                "processed": true,
                "error": null,
                "message": "completed"
            })),
            response(&serde_json::json!({
                "backends": ["piper"],
                "loaded_models": []
            })),
            response(&serde_json::json!({})),
        ]));
        let client = HttpModelLibraryClient::new(
            ModelControlProtocol::LocalAi,
            endpoint(Authentication::Bearer(Credential::new("fixture-value"))),
            transport.clone(),
        );

        let models = client.list_models().await.unwrap();
        assert_eq!(models[0].id, "voice-model");
        assert_eq!(models[0].loaded_instances, ["voice-model"]);
        let started = client
            .download_model(
                ModelDownloadRequest {
                    model: "localai@voice-model".to_owned(),
                    quantization: None,
                },
                CancellationFlag::default(),
                Arc::new(RecordingProgress::default()),
            )
            .await
            .unwrap();
        assert_eq!(started.job_id.as_deref(), Some("job-localai"));
        let completed = client.download_status("job-localai").await.unwrap();
        assert_eq!(completed.state, ModelDownloadState::Completed);

        let requests = transport.requests();
        assert_eq!(requests.len(), 5);
        assert_eq!(requests[0].method, HttpMethod::Get);
        assert_eq!(requests[0].url.path(), "/v1/models");
        assert_eq!(requests[1].method, HttpMethod::Get);
        assert_eq!(requests[1].url.path(), "/system");
        assert_eq!(requests[2].method, HttpMethod::Get);
        assert_eq!(requests[2].url.path(), "/models/available");
        assert_eq!(requests[3].method, HttpMethod::Post);
        assert_eq!(requests[3].url.path(), "/models/apply");
        assert_eq!(requests[4].method, HttpMethod::Get);
        assert_eq!(requests[4].url.path(), "/models/jobs/job-localai");
        assert!(
            requests
                .iter()
                .all(|request| request.url.host_str() == Some("127.0.0.1"))
        );
        assert!(
            requests
                .iter()
                .all(|request| request.headers.contains_key("authorization"))
        );
        let body: serde_json::Value = serde_json::from_slice(&requests[3].body).unwrap();
        assert_eq!(body, serde_json::json!({"id": "localai@voice-model"}));
        assert!(
            client
                .delete_model("localai@voice-model:v1", false, false)
                .await
                .is_err()
        );
        assert!(
            client
                .delete_model("localai@voice-model:v1", true, true)
                .await
                .is_err()
        );
        assert_eq!(transport.requests().len(), 5);
        client
            .delete_model("localai@voice-model:v1", true, false)
            .await
            .unwrap();
        let requests = transport.requests();
        assert_eq!(requests.len(), 7);
        assert_eq!(requests[5].method, HttpMethod::Get);
        assert_eq!(requests[5].url.path(), "/system");
        assert_eq!(requests[6].method, HttpMethod::Post);
        assert_eq!(
            requests[6].url.path(),
            "/models/delete/localai%40voice-model%3Av1"
        );
        assert!(requests[6].body.is_empty());
    }

    #[tokio::test]
    async fn local_ai_delete_refuses_a_loaded_model_before_mutating_the_provider() {
        let transport = Arc::new(RecordingTransport::with_responses(vec![response(
            &serde_json::json!({
                "backends": ["piper"],
                "loaded_models": [{"id": "voice-model"}]
            }),
        )]));
        let client = HttpModelLibraryClient::new(
            ModelControlProtocol::LocalAi,
            endpoint(Authentication::None),
            transport.clone(),
        );

        let error = client
            .delete_model("localai@voice-model", true, false)
            .await
            .expect_err("loaded LocalAI models must not be deleted");

        assert!(error.to_string().contains("must be unloaded"));
        let requests = transport.requests();
        assert_eq!(requests.len(), 1);
        assert_eq!(requests[0].method, HttpMethod::Get);
        assert_eq!(requests[0].url.path(), "/system");
    }

    #[tokio::test]
    async fn local_ai_delete_fails_closed_when_loaded_state_cannot_be_verified() {
        let echoed_secret = "fixture-secret-that-must-not-survive";
        let transport = Arc::new(RecordingTransport::with_responses(vec![
            response_with_status(
                500,
                &serde_json::json!({
                    "error": {"code": "backend_failure", "message": echoed_secret}
                }),
            ),
        ]));
        let client = HttpModelLibraryClient::new(
            ModelControlProtocol::LocalAi,
            endpoint(Authentication::None),
            transport.clone(),
        );

        let error = client
            .delete_model("voice-model", true, false)
            .await
            .expect_err("an unverifiable loaded state must block deletion");

        let message = error.to_string();
        assert!(message.contains("backend_failure"));
        assert!(!message.contains(echoed_secret));
        let requests = transport.requests();
        assert_eq!(requests.len(), 1);
        assert_eq!(requests[0].method, HttpMethod::Get);
        assert_eq!(requests[0].url.path(), "/system");

        let transport = Arc::new(RecordingTransport::with_responses(vec![response(
            &serde_json::json!({"backends": ["piper"]}),
        )]));
        let client = HttpModelLibraryClient::new(
            ModelControlProtocol::LocalAi,
            endpoint(Authentication::None),
            transport.clone(),
        );
        assert!(
            client
                .delete_model("voice-model", true, false)
                .await
                .is_err()
        );
        assert_eq!(transport.requests().len(), 1);
        assert_eq!(transport.requests()[0].url.path(), "/system");
    }

    #[tokio::test]
    async fn local_ai_rejects_non_gallery_and_path_or_url_install_inputs() {
        for invalid in [
            "https://example.invalid/model.yaml",
            "file:///tmp/model.yaml",
            "../model",
            "gallery@model@extra",
            "gallery/model",
            "gallery@model?variant=x",
        ] {
            assert!(
                validate_local_ai_gallery_identifier(invalid).is_err(),
                "{invalid}"
            );
        }

        let transport = Arc::new(RecordingTransport::with_responses(vec![response(
            &serde_json::json!([{"name": "other-model"}]),
        )]));
        let client = HttpModelLibraryClient::new(
            ModelControlProtocol::LocalAi,
            endpoint(Authentication::None),
            transport.clone(),
        );
        assert!(
            client
                .download_model(
                    ModelDownloadRequest {
                        model: "wanted-model".to_owned(),
                        quantization: None,
                    },
                    CancellationFlag::default(),
                    Arc::new(RecordingProgress::default()),
                )
                .await
                .is_err()
        );
        assert_eq!(transport.requests().len(), 1);
        assert_eq!(transport.requests()[0].url.path(), "/models/available");
    }

    #[test]
    fn model_identifier_validation_rejects_url_and_userinfo_tricks() {
        for invalid in [
            "http://example.test/model",
            "user@example.test/model",
            "model?parameter=value",
            "model#fragment",
            "model%2fother",
            "../model",
        ] {
            assert!(
                validate_ollama_model_identifier(invalid).is_err(),
                "{invalid}"
            );
        }
        for invalid in [
            "granite",
            "owner/model/extra",
            "https://user@huggingface.co/owner/model",
            "https://huggingface.co/owner/model?download=true",
            "https://huggingface.co/owner/model/tree/main",
            "http://huggingface.co/owner/model",
            "https://example.test/owner/model",
        ] {
            assert!(
                validate_lm_download_identifier(invalid).is_err(),
                "{invalid}"
            );
        }
        assert_eq!(
            validate_lm_download_identifier("owner/model-name").unwrap(),
            LmDownloadSource::Catalog
        );
        assert_eq!(
            validate_lm_download_identifier("https://huggingface.co/owner/model-name").unwrap(),
            LmDownloadSource::HuggingFace
        );
        assert!(ollama_model_identifiers_equal("gemma3", "gemma3:latest").unwrap());
        assert!(!ollama_model_identifiers_equal("gemma3:v1", "gemma3:latest").unwrap());
        assert!(local_ai_model_identifiers_equal("localai@voice-model", "voice-model").unwrap());
    }

    #[test]
    fn model_identifiers_reject_secret_shaped_values_without_echoing_them() {
        let prefixed = [
            ["s", "k"].concat(),
            "syntheticcredential0123456789".to_owned(),
        ]
        .join("-");
        let jwt = ["headerpart0123", "payloadpart4567", "signaturepart89"].join(".");
        let cases = [
            (ModelControlProtocol::Ollama, prefixed.clone()),
            (ModelControlProtocol::Ollama, jwt.clone()),
            (ModelControlProtocol::LmStudio, format!("owner/{prefixed}")),
            (
                ModelControlProtocol::LmStudio,
                format!("https://huggingface.co/owner/{prefixed}"),
            ),
            (ModelControlProtocol::LocalAi, format!("localai@{prefixed}")),
            (ModelControlProtocol::LocalAi, jwt.clone()),
        ];
        for (protocol, value) in cases {
            let error = match protocol {
                ModelControlProtocol::Ollama => validate_ollama_model_identifier(&value),
                ModelControlProtocol::LmStudio => {
                    validate_lm_download_identifier(&value).map(|_| ())
                }
                ModelControlProtocol::LocalAi => validate_local_ai_gallery_identifier(&value),
            }
            .expect_err("secret-shaped model input must be rejected");
            let message = error.to_string();
            assert!(message.contains("sensitive credential material"));
            assert!(!message.contains(&value));
        }
    }
}
