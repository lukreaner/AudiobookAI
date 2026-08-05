use std::{fmt, sync::Arc};

use async_trait::async_trait;

use crate::{
    AudioChunk, CancellationFlag, CharacterDetectionRequest, CharacterDetectionResult, Model,
    ModelDownloadRequest, ModelDownloadStatus, OwnedProcessHandle, ProcessLogLine, ProcessSpec,
    ProcessStatus, ProviderCapabilities, ProviderDescriptor, ProviderHealth, ProviderModelInfo,
    Result, StreamingSynthesisResponse, SynthesisRequest, SynthesisResponse, Voice, VoiceClone,
    VoiceCloneRequest,
};

#[async_trait]
pub trait AudioChunkSink: fmt::Debug + Send + Sync {
    async fn send(&self, chunk: AudioChunk) -> Result<()>;
}

#[async_trait]
pub trait ModelDownloadProgressSink: fmt::Debug + Send + Sync {
    async fn update(&self, status: ModelDownloadStatus) -> Result<()>;
}

#[async_trait]
pub trait TtsProvider: fmt::Debug + Send + Sync {
    fn descriptor(&self) -> &ProviderDescriptor;
    fn capabilities(&self) -> &ProviderCapabilities;
    async fn health(&self) -> Result<ProviderHealth>;
    async fn discover_voices(&self) -> Result<Vec<Voice>>;
    async fn discover_models(&self) -> Result<Vec<Model>>;
    async fn synthesize(&self, request: SynthesisRequest) -> Result<SynthesisResponse>;

    async fn preview(&self, mut request: SynthesisRequest) -> Result<SynthesisResponse> {
        request.text = request.text.chars().take(500).collect();
        self.synthesize(request).await
    }

    async fn synthesize_stream(
        &self,
        _request: SynthesisRequest,
        _cancellation: CancellationFlag,
        _sink: Arc<dyn AudioChunkSink>,
    ) -> Result<StreamingSynthesisResponse> {
        Err(crate::ProviderError::Unsupported {
            feature: "streaming synthesis",
        })
    }

    async fn cancel(&self, _request_id: uuid::Uuid) -> Result<()> {
        Err(crate::ProviderError::Unsupported {
            feature: "provider-side cancellation",
        })
    }
}

#[async_trait]
pub trait CharacterProvider: fmt::Debug + Send + Sync {
    fn descriptor(&self) -> &ProviderDescriptor;
    fn capabilities(&self) -> &ProviderCapabilities;
    async fn health(&self) -> Result<ProviderHealth>;
    async fn discover_models(&self) -> Result<Vec<Model>>;
    async fn detect_characters(
        &self,
        request: CharacterDetectionRequest,
    ) -> Result<CharacterDetectionResult>;
}

#[async_trait]
pub trait VoiceCloneProvider: fmt::Debug + Send + Sync {
    fn descriptor(&self) -> &ProviderDescriptor;
    async fn create_clone(&self, request: VoiceCloneRequest) -> Result<VoiceClone>;
    async fn update_clone(&self, clone: &VoiceClone, name: String) -> Result<VoiceClone>;
    /// Implementations must reject catalog or otherwise unowned voices.
    async fn delete_owned_clone(&self, clone: &VoiceClone, confirmed: bool) -> Result<()>;
}

#[async_trait]
pub trait ProviderControl: fmt::Debug + Send + Sync {
    fn descriptor(&self) -> &ProviderDescriptor;
    async fn start(&self, spec: ProcessSpec) -> Result<OwnedProcessHandle>;
    async fn status(&self, handle: &OwnedProcessHandle) -> Result<ProcessStatus>;
    async fn stop(&self, handle: &OwnedProcessHandle) -> Result<()>;
    async fn restart(&self, handle: &OwnedProcessHandle) -> Result<OwnedProcessHandle>;
    async fn logs(&self, handle: &OwnedProcessHandle, limit: usize) -> Result<Vec<ProcessLogLine>>;
    async fn load_model(&self, _model: &str) -> Result<()> {
        Err(crate::ProviderError::Unsupported {
            feature: "model loading",
        })
    }
    async fn unload_model(&self, _model: &str) -> Result<()> {
        Err(crate::ProviderError::Unsupported {
            feature: "model unloading",
        })
    }
    async fn switch_model(&self, model: &str) -> Result<()> {
        self.load_model(model).await
    }
    async fn list_models(&self) -> Result<Vec<ProviderModelInfo>> {
        Err(crate::ProviderError::Unsupported {
            feature: "model listing",
        })
    }
    async fn download_model(
        &self,
        _request: ModelDownloadRequest,
        _cancellation: CancellationFlag,
        _progress: Arc<dyn ModelDownloadProgressSink>,
    ) -> Result<ModelDownloadStatus> {
        Err(crate::ProviderError::Unsupported {
            feature: "model downloading",
        })
    }
    async fn model_download_status(&self, _job_id: &str) -> Result<ModelDownloadStatus> {
        Err(crate::ProviderError::Unsupported {
            feature: "model download status",
        })
    }
    async fn delete_model(&self, _model: &str, _confirmed: bool, _in_use: bool) -> Result<()> {
        Err(crate::ProviderError::Unsupported {
            feature: "model deletion",
        })
    }
}
