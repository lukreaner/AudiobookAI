//! Built-in HTTP provider adapters.

mod character;
mod native;
mod tts;

pub use character::{
    AnthropicProvider, GeminiProvider, OllamaProvider, OpenAiChatPreset, OpenAiCompatibleProvider,
    OpenAiResponsesProvider,
};
pub use native::{
    NativeCapture, NativeCommand, NativeCommandArgument, NativeCommandOutput, NativeCommandRunner,
    NativePlatform, NativeTtsConfig, NativeTtsProvider, TokioNativeCommandRunner,
};
pub use tts::{
    AllTalkProvider, ElevenLabsProvider, LocalAiProvider, MlxAudioProvider, OpenAiTtsProvider,
    openai_tts_model_performance_capabilities,
};

use std::{collections::BTreeMap, sync::Arc};

use crate::{
    EndpointConfig, HttpMethod, HttpRequest, HttpTransport, ProviderError, ProviderHealth, Result,
};

#[derive(Clone, Debug)]
struct HttpAdapter {
    endpoint: EndpointConfig,
    transport: Arc<dyn HttpTransport>,
}

impl HttpAdapter {
    fn new(endpoint: EndpointConfig, transport: Arc<dyn HttpTransport>) -> Self {
        Self {
            endpoint,
            transport,
        }
    }

    fn json_request(
        &self,
        method: HttpMethod,
        path: &str,
        body: &serde_json::Value,
    ) -> Result<HttpRequest> {
        let mut request = HttpRequest::json(method, self.endpoint.endpoint(path)?, body)?;
        self.endpoint.authentication.apply(&mut request.headers);
        Ok(request)
    }

    fn empty_request(&self, method: HttpMethod, path: &str) -> Result<HttpRequest> {
        let mut request = HttpRequest {
            method,
            url: self.endpoint.endpoint(path)?,
            headers: BTreeMap::new(),
            body: bytes::Bytes::new(),
            timeout: std::time::Duration::from_secs(20),
        };
        self.endpoint.authentication.apply(&mut request.headers);
        Ok(request)
    }

    async fn execute(&self, request: HttpRequest) -> Result<crate::HttpResponse> {
        self.transport.execute(request).await?.require_success()
    }

    async fn basic_health(&self, path: &str) -> Result<ProviderHealth> {
        let response = self
            .execute(self.empty_request(HttpMethod::Get, path)?)
            .await?;
        let json = serde_json::from_slice::<serde_json::Value>(&response.body).ok();
        let version = json
            .as_ref()
            .and_then(|value| value.get("version"))
            .and_then(serde_json::Value::as_str)
            .map(ToOwned::to_owned);
        Ok(ProviderHealth {
            available: true,
            version,
            message: None,
        })
    }
}

fn json_body(response: &crate::HttpResponse) -> Result<serde_json::Value> {
    serde_json::from_slice(&response.body)
        .map_err(|error| ProviderError::InvalidResponse(error.to_string()))
}
