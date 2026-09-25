use std::{collections::BTreeSet, sync::Arc, time::Duration};

use async_trait::async_trait;
use serde_json::{Map, Value, json};

use super::{
    HttpAdapter,
    dialogue::{DETECTION_SCHEMA, parse_and_resolve, wire_input},
    generation_controls::{
        anthropic_model_controls, gemini_model_controls, ollama_model_controls,
        openai_effort_probe, openai_model_controls, openai_temperature_probe,
    },
    json_body,
};
use crate::{
    Authentication, CharacterDetectionRequest, CharacterDetectionResult, CharacterProvider,
    EndpointConfig, HttpMethod, HttpRequest, HttpTransport, Model, ModelContextWindow,
    ModelGenerationControls, ParameterSupport, ProviderCapabilities, ProviderDescriptor,
    ProviderError, ProviderHealth, ProviderId, ProviderUsage, ReasoningControl, ReasoningMode,
    Result, Temperature, UsageSource,
};

// Local model startup plus schema-constrained generation can take several minutes, especially on
// the first request after LM Studio or Ollama has unloaded a model. Cloud requests retain the
// conservative two-minute default from `HttpRequest::json`.
const LOCAL_DETECTION_TIMEOUT: Duration = Duration::from_secs(15 * 60);
const LM_STUDIO_HEALTH_TIMEOUT: Duration = Duration::from_secs(3);

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum CharacterFlavor {
    OpenAiResponses,
    OpenAiChat(OpenAiChatPreset),
    Anthropic,
    Gemini,
    Ollama,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum OpenAiChatPreset {
    Generic,
    Qwen,
    Kimi,
    Moonshot,
    LmStudio,
}

impl OpenAiChatPreset {
    fn label(self) -> &'static str {
        match self {
            Self::Generic => "OpenAI-compatible",
            Self::Qwen => "Qwen",
            Self::Kimi => "Kimi",
            Self::Moonshot => "Moonshot",
            Self::LmStudio => "LM Studio",
        }
    }

    fn id(self) -> &'static str {
        match self {
            Self::Generic => "openai-compatible",
            Self::Qwen => "qwen",
            Self::Kimi => "kimi",
            Self::Moonshot => "moonshot",
            Self::LmStudio => "lmstudio",
        }
    }
}

#[derive(Clone, Debug)]
struct JsonCharacterProvider {
    descriptor: ProviderDescriptor,
    capabilities: ProviderCapabilities,
    http: HttpAdapter,
    flavor: CharacterFlavor,
}

impl JsonCharacterProvider {
    fn new(
        descriptor: ProviderDescriptor,
        capabilities: ProviderCapabilities,
        endpoint: EndpointConfig,
        transport: Arc<dyn HttpTransport>,
        flavor: CharacterFlavor,
    ) -> Self {
        Self {
            descriptor,
            capabilities,
            http: HttpAdapter::new(endpoint, transport),
            flavor,
        }
    }

    fn build_detection_request(&self, request: &CharacterDetectionRequest) -> Result<HttpRequest> {
        request
            .temperature
            .validate(self.capabilities.temperature)?;
        request.reasoning.validate(&self.capabilities)?;
        if request.paragraphs.is_empty() || request.max_output_tokens == 0 {
            return Err(ProviderError::Configuration(
                "detection needs paragraphs and a non-zero output token limit".to_owned(),
            ));
        }
        let input = wire_input(request)?;
        let (path, mut body) = match self.flavor {
            CharacterFlavor::OpenAiResponses => {
                let schema = schema_placeholder();
                (
                    "v1/responses".to_owned(),
                    json!({
                        "model": request.model,
                        "instructions": request.system_prompt,
                        "input": input,
                        "max_output_tokens": request.max_output_tokens,
                        "text": { "format": {
                            "type": "json_schema",
                            "name": "character_detection",
                            "strict": true,
                            "schema": schema
                        }}
                    }),
                )
            }
            CharacterFlavor::OpenAiChat(_) => {
                let schema = schema_placeholder();
                (
                    "v1/chat/completions".to_owned(),
                    json!({
                        "model": request.model,
                        "messages": [
                            { "role": "system", "content": request.system_prompt },
                            { "role": "user", "content": input }
                        ],
                        "max_tokens": request.max_output_tokens,
                        "response_format": {
                            "type": "json_schema",
                            "json_schema": {
                                "name": "character_detection",
                                "strict": true,
                                "schema": schema
                            }
                        }
                    }),
                )
            }
            CharacterFlavor::Anthropic => (
                "v1/messages".to_owned(),
                json!({
                    "model": request.model,
                    "system": request.system_prompt,
                    "messages": [{ "role": "user", "content": format!(
                        "Return only JSON matching this schema: {DETECTION_SCHEMA}\n\n{input}"
                    )}],
                    "max_tokens": request.max_output_tokens
                }),
            ),
            CharacterFlavor::Gemini => (
                format!(
                    "v1beta/models/{}:generateContent",
                    encode_path_segment(&request.model)
                ),
                json!({
                    "systemInstruction": { "parts": [{ "text": request.system_prompt }] },
                    "contents": [{ "role": "user", "parts": [{ "text": input }] }],
                    "generationConfig": {
                        "maxOutputTokens": request.max_output_tokens,
                        "responseMimeType": "application/json",
                        "responseJsonSchema": schema_placeholder()
                    }
                }),
            ),
            CharacterFlavor::Ollama => (
                "api/chat".to_owned(),
                json!({
                    "model": request.model,
                    "messages": [
                        { "role": "system", "content": request.system_prompt },
                        { "role": "user", "content": input }
                    ],
                    "stream": false,
                    "format": schema_placeholder(),
                    "options": { "num_predict": request.max_output_tokens }
                }),
            ),
        };
        apply_temperature(&mut body, request.temperature, self.flavor)?;
        apply_reasoning(&mut body, &request.reasoning, self.flavor)?;
        self.detection_http_request(&path, &body)
    }

    async fn detect(&self, request: CharacterDetectionRequest) -> Result<CharacterDetectionResult> {
        let response = match self
            .http
            .execute(self.build_detection_request(&request)?)
            .await
        {
            // Reqwest cannot know whether a timed-out POST was billable. LM Studio and a
            // non-cloud Ollama are local runtimes, so retain the transient timeout semantics
            // without claiming an uncertain provider charge or requiring the duplicate-billing
            // override.
            Err(ProviderError::UncertainCharge) if self.is_local_runtime() => {
                return Err(ProviderError::Transport(format!(
                    "{} character detection timed out",
                    self.descriptor.display_name
                )));
            }
            result => result?,
        };
        let envelope = json_body(&response)?;
        if output_was_truncated(&envelope, self.flavor) {
            return Err(ProviderError::OutputTruncated);
        }
        let content = extract_content(&envelope, self.flavor)?;
        let mut result = parse_and_resolve(&content, &request)?;
        result.usage = extract_usage(&envelope, self.flavor);
        result.validate(&request)
    }

    async fn health(&self) -> Result<ProviderHealth> {
        let path = match self.flavor {
            CharacterFlavor::OpenAiResponses
            | CharacterFlavor::OpenAiChat(_)
            | CharacterFlavor::Anthropic => "v1/models",
            CharacterFlavor::Gemini => "v1beta/models",
            CharacterFlavor::Ollama => "api/tags",
        };
        let mut request = self.decorate_request(self.http.empty_request(HttpMethod::Get, path)?);
        if self.is_lm_studio() {
            request.timeout = LM_STUDIO_HEALTH_TIMEOUT;
        }
        let response = self.http.execute(request).await?;
        let value = serde_json::from_slice::<Value>(&response.body).ok();
        Ok(ProviderHealth {
            available: true,
            version: value
                .as_ref()
                .and_then(|value| value.get("version"))
                .and_then(Value::as_str)
                .map(ToOwned::to_owned),
            message: None,
        })
    }

    async fn models(&self) -> Result<Vec<Model>> {
        let path = match self.flavor {
            CharacterFlavor::OpenAiResponses
            | CharacterFlavor::OpenAiChat(_)
            | CharacterFlavor::Anthropic => "v1/models",
            CharacterFlavor::Gemini => "v1beta/models",
            CharacterFlavor::Ollama => "api/tags",
        };
        let response = self
            .http
            .execute(self.decorate_request(self.http.empty_request(HttpMethod::Get, path)?))
            .await?;
        parse_models(&json_body(&response)?, self.flavor)
    }

    async fn model_context_window(&self, model: &str) -> Result<ModelContextWindow> {
        if !self.is_lm_studio() {
            return Ok(ModelContextWindow::default());
        }
        let mut request =
            self.decorate_request(self.http.empty_request(HttpMethod::Get, "api/v1/models")?);
        request.timeout = LM_STUDIO_HEALTH_TIMEOUT;
        let response = self.http.execute(request).await?;
        parse_lm_studio_context_window(&json_body(&response)?, model)
    }

    async fn generation_controls(&self, model: &str) -> Result<ModelGenerationControls> {
        match self.flavor {
            CharacterFlavor::Anthropic => {
                let path = format!("v1/models/{}", encode_path_segment(model));
                let response = self
                    .http
                    .execute(
                        self.decorate_request(self.http.empty_request(HttpMethod::Get, &path)?),
                    )
                    .await?;
                Ok(anthropic_model_controls(&json_body(&response)?))
            }
            CharacterFlavor::Gemini => {
                let path = format!("v1beta/models/{}", encode_path_segment(model));
                let response = self
                    .http
                    .execute(self.http.empty_request(HttpMethod::Get, &path)?)
                    .await?;
                Ok(gemini_model_controls(&json_body(&response)?))
            }
            CharacterFlavor::Ollama => {
                let response = self
                    .http
                    .execute(self.http.json_request(
                        HttpMethod::Post,
                        "api/show",
                        &json!({ "model": model }),
                    )?)
                    .await?;
                Ok(ollama_model_controls(model, &json_body(&response)?))
            }
            CharacterFlavor::OpenAiResponses => {
                // OpenAI publishes no per-model metadata, but validates enumerations and ranges
                // before generating and names the accepted values. Both probes carry an invalid
                // value, so neither is ever executed or billed.
                let effort = self
                    .probe(json!({
                        "model": model,
                        "input": "capability probe",
                        "max_output_tokens": 16,
                        "reasoning": { "effort": "__audiobookai_probe__" }
                    }))
                    .await?;
                let temperature = self
                    .probe(json!({
                        "model": model,
                        "input": "capability probe",
                        "max_output_tokens": 16,
                        "temperature": 3
                    }))
                    .await?;
                Ok(openai_model_controls(
                    &openai_effort_probe(effort.0, &effort.1),
                    &openai_temperature_probe(temperature.0, &temperature.1),
                ))
            }
            CharacterFlavor::OpenAiChat(_) => Ok(ModelGenerationControls::from_adapter_contract(
                &self.capabilities,
            )),
        }
    }

    /// Sends a validation probe and returns its status and body without treating 4xx as failure.
    async fn probe(&self, body: Value) -> Result<(u16, bytes::Bytes)> {
        let mut request = self
            .http
            .json_request(HttpMethod::Post, "v1/responses", &body)?;
        request.timeout = Duration::from_secs(20);
        let response = self.http.transport.execute(request).await?;
        Ok((response.status, response.body))
    }

    fn decorate_request(&self, mut request: HttpRequest) -> HttpRequest {
        if matches!(self.flavor, CharacterFlavor::Anthropic) {
            request
                .headers
                .insert("anthropic-version".to_owned(), "2023-06-01".to_owned());
        }
        request
    }

    fn detection_http_request(&self, path: &str, body: &Value) -> Result<HttpRequest> {
        let mut request =
            self.decorate_request(self.http.json_request(HttpMethod::Post, path, body)?);
        request.body = with_ordered_schema(&request.body)?;
        if self.is_local_runtime() {
            request.timeout = LOCAL_DETECTION_TIMEOUT;
        }
        Ok(request)
    }

    const fn is_lm_studio(&self) -> bool {
        matches!(
            self.flavor,
            CharacterFlavor::OpenAiChat(OpenAiChatPreset::LmStudio)
        )
    }

    fn is_local_runtime(&self) -> bool {
        self.is_lm_studio()
            || (matches!(self.flavor, CharacterFlavor::Ollama)
                && !matches!(self.http.endpoint.kind, crate::ProviderKind::CloudRemote))
    }
}

const SCHEMA_PLACEHOLDER: &str = "__audiobookai_detection_schema__";

fn schema_placeholder() -> Value {
    Value::String(SCHEMA_PLACEHOLDER.to_owned())
}

/// Splices the detection schema into a serialized body without re-sorting its properties.
///
/// `serde_json::Value` orders object keys alphabetically. Grammar-constrained decoders (LM Studio,
/// llama.cpp, Ollama, and `OpenAI` structured outputs) generate properties in schema order, and
/// the model should copy the quoted passage before it commits to a speaker.
fn with_ordered_schema(body: &bytes::Bytes) -> Result<bytes::Bytes> {
    let serialized = std::str::from_utf8(body)
        .map_err(|error| ProviderError::Configuration(error.to_string()))?;
    Ok(serialized
        .replacen(&format!("\"{SCHEMA_PLACEHOLDER}\""), DETECTION_SCHEMA, 1)
        .into_bytes()
        .into())
}

fn base_capabilities(temperature: ParameterSupport) -> ProviderCapabilities {
    ProviderCapabilities {
        model_discovery: true,
        temperature,
        ..ProviderCapabilities::default()
    }
}

fn descriptor(
    id: &str,
    name: &str,
    endpoint_family: &str,
    endpoint: &EndpointConfig,
) -> Result<ProviderDescriptor> {
    Ok(ProviderDescriptor {
        id: ProviderId::new(id)?,
        display_name: name.to_owned(),
        kind: endpoint.kind,
        endpoint_family: endpoint_family.to_owned(),
    })
}

macro_rules! provider_wrapper {
    ($name:ident) => {
        #[derive(Clone, Debug)]
        pub struct $name(JsonCharacterProvider);

        impl $name {
            pub fn build_detection_request(
                &self,
                request: &CharacterDetectionRequest,
            ) -> Result<HttpRequest> {
                self.0.build_detection_request(request)
            }
        }

        #[async_trait]
        impl CharacterProvider for $name {
            fn descriptor(&self) -> &ProviderDescriptor {
                &self.0.descriptor
            }
            fn capabilities(&self) -> &ProviderCapabilities {
                &self.0.capabilities
            }
            async fn health(&self) -> Result<ProviderHealth> {
                self.0.health().await
            }
            async fn discover_models(&self) -> Result<Vec<Model>> {
                self.0.models().await
            }
            async fn model_context_window(&self, model: &str) -> Result<ModelContextWindow> {
                self.0.model_context_window(model).await
            }
            async fn model_generation_controls(
                &self,
                model: &str,
            ) -> Result<ModelGenerationControls> {
                self.0.generation_controls(model).await
            }
            async fn detect_characters(
                &self,
                request: CharacterDetectionRequest,
            ) -> Result<CharacterDetectionResult> {
                self.0.detect(request).await
            }
        }
    };
}

provider_wrapper!(OpenAiResponsesProvider);
provider_wrapper!(OpenAiCompatibleProvider);
provider_wrapper!(AnthropicProvider);
provider_wrapper!(GeminiProvider);
provider_wrapper!(OllamaProvider);

impl OpenAiResponsesProvider {
    pub fn new(api_key: crate::Credential, transport: Arc<dyn HttpTransport>) -> Result<Self> {
        let endpoint = EndpointConfig::cloud(
            url::Url::parse("https://api.openai.com/")
                .map_err(|error| ProviderError::Configuration(error.to_string()))?,
            Authentication::Bearer(api_key),
        )?;
        Self::with_endpoint(endpoint, transport)
    }

    pub fn with_endpoint(
        endpoint: EndpointConfig,
        transport: Arc<dyn HttpTransport>,
    ) -> Result<Self> {
        let mut capabilities = base_capabilities(ParameterSupport::NullableValue);
        capabilities.reasoning = BTreeSet::from([ReasoningMode::Disabled, ReasoningMode::Effort]);
        Ok(Self(JsonCharacterProvider::new(
            descriptor("openai", "OpenAI", "openai-responses-v1", &endpoint)?,
            capabilities,
            endpoint,
            transport,
            CharacterFlavor::OpenAiResponses,
        )))
    }
}

impl OpenAiCompatibleProvider {
    pub fn new(
        preset: OpenAiChatPreset,
        endpoint: EndpointConfig,
        transport: Arc<dyn HttpTransport>,
    ) -> Result<Self> {
        let mut capabilities = base_capabilities(ParameterSupport::Value);
        if matches!(preset, OpenAiChatPreset::Qwen | OpenAiChatPreset::Kimi) {
            capabilities.reasoning.insert(ReasoningMode::Disabled);
        }
        Ok(Self(JsonCharacterProvider::new(
            descriptor(preset.id(), preset.label(), "openai-chat-v1", &endpoint)?,
            capabilities,
            endpoint,
            transport,
            CharacterFlavor::OpenAiChat(preset),
        )))
    }
}

impl AnthropicProvider {
    pub fn new(api_key: crate::Credential, transport: Arc<dyn HttpTransport>) -> Result<Self> {
        let endpoint = EndpointConfig::cloud(
            url::Url::parse("https://api.anthropic.com/")
                .map_err(|error| ProviderError::Configuration(error.to_string()))?,
            Authentication::Header {
                name: "x-api-key".to_owned(),
                value: api_key,
            },
        )?;
        let mut capabilities = base_capabilities(ParameterSupport::Value);
        capabilities.reasoning = BTreeSet::from([
            ReasoningMode::Disabled,
            ReasoningMode::Effort,
            ReasoningMode::Adaptive,
            ReasoningMode::TokenBudget,
        ]);
        Ok(Self(JsonCharacterProvider::new(
            descriptor("anthropic", "Claude", "anthropic-messages-v1", &endpoint)?,
            capabilities,
            endpoint,
            transport,
            CharacterFlavor::Anthropic,
        )))
    }
}

impl GeminiProvider {
    pub fn new(api_key: crate::Credential, transport: Arc<dyn HttpTransport>) -> Result<Self> {
        let endpoint = EndpointConfig::cloud(
            url::Url::parse("https://generativelanguage.googleapis.com/")
                .map_err(|error| ProviderError::Configuration(error.to_string()))?,
            Authentication::Header {
                name: "x-goog-api-key".to_owned(),
                value: api_key,
            },
        )?;
        let mut capabilities = base_capabilities(ParameterSupport::Value);
        capabilities.reasoning =
            BTreeSet::from([ReasoningMode::Disabled, ReasoningMode::TokenBudget]);
        Ok(Self(JsonCharacterProvider::new(
            descriptor("gemini", "Google Gemini", "google-gemini-v1beta", &endpoint)?,
            capabilities,
            endpoint,
            transport,
            CharacterFlavor::Gemini,
        )))
    }
}

impl OllamaProvider {
    pub fn new(endpoint: EndpointConfig, transport: Arc<dyn HttpTransport>) -> Result<Self> {
        let mut capabilities = base_capabilities(ParameterSupport::Value);
        capabilities.reasoning = BTreeSet::from([ReasoningMode::Disabled, ReasoningMode::Effort]);
        Ok(Self(JsonCharacterProvider::new(
            descriptor("ollama", "Ollama", "ollama-chat", &endpoint)?,
            capabilities,
            endpoint,
            transport,
            CharacterFlavor::Ollama,
        )))
    }
}

fn apply_temperature(
    body: &mut Value,
    temperature: Temperature,
    flavor: CharacterFlavor,
) -> Result<()> {
    if matches!(temperature, Temperature::Default) {
        return Ok(());
    }
    let value = match temperature {
        Temperature::Null => Value::Null,
        Temperature::Value(value) => json!(value),
        Temperature::Default => unreachable!(),
    };
    let object = body
        .as_object_mut()
        .ok_or_else(|| ProviderError::Configuration("request body is not an object".to_owned()))?;
    match flavor {
        CharacterFlavor::OpenAiResponses
        | CharacterFlavor::OpenAiChat(_)
        | CharacterFlavor::Anthropic => {
            object.insert("temperature".to_owned(), value);
        }
        CharacterFlavor::Gemini => {
            nested_object(object, "generationConfig")?.insert("temperature".to_owned(), value);
        }
        CharacterFlavor::Ollama => {
            nested_object(object, "options")?.insert("temperature".to_owned(), value);
        }
    }
    Ok(())
}

fn apply_reasoning(
    body: &mut Value,
    reasoning: &ReasoningControl,
    flavor: CharacterFlavor,
) -> Result<()> {
    if matches!(reasoning, ReasoningControl::Inherit) {
        return Ok(());
    }
    let object = body
        .as_object_mut()
        .ok_or_else(|| ProviderError::Configuration("request body is not an object".to_owned()))?;
    match (flavor, reasoning) {
        (CharacterFlavor::OpenAiResponses, ReasoningControl::Disabled) => {
            object.insert("reasoning".to_owned(), json!({ "effort": "none" }));
        }
        (CharacterFlavor::OpenAiResponses, ReasoningControl::Effort { effort }) => {
            object.insert("reasoning".to_owned(), json!({ "effort": effort.as_str() }));
        }
        (CharacterFlavor::OpenAiChat(_), ReasoningControl::Disabled) => {
            object.insert("enable_thinking".to_owned(), Value::Bool(false));
        }
        (CharacterFlavor::Anthropic, ReasoningControl::Effort { effort }) => {
            object.insert(
                "output_config".to_owned(),
                json!({ "effort": effort.as_str() }),
            );
        }
        (CharacterFlavor::Anthropic, ReasoningControl::Disabled) => {
            object.insert("thinking".to_owned(), json!({ "type": "disabled" }));
        }
        (CharacterFlavor::Anthropic, ReasoningControl::Adaptive) => {
            object.insert("thinking".to_owned(), json!({ "type": "adaptive" }));
        }
        (CharacterFlavor::Anthropic, ReasoningControl::TokenBudget { tokens }) => {
            object.insert(
                "thinking".to_owned(),
                json!({ "type": "enabled", "budget_tokens": tokens }),
            );
        }
        (CharacterFlavor::Gemini, ReasoningControl::Disabled) => {
            nested_object(object, "generationConfig")?
                .insert("thinkingConfig".to_owned(), json!({ "thinkingBudget": 0 }));
        }
        (CharacterFlavor::Gemini, ReasoningControl::TokenBudget { tokens }) => {
            nested_object(object, "generationConfig")?.insert(
                "thinkingConfig".to_owned(),
                json!({ "thinkingBudget": tokens }),
            );
        }
        (CharacterFlavor::Ollama, ReasoningControl::Disabled) => {
            object.insert("think".to_owned(), Value::Bool(false));
        }
        (CharacterFlavor::Ollama, ReasoningControl::Effort { effort }) => {
            object.insert("think".to_owned(), json!(effort.as_str()));
        }
        _ => {
            return Err(ProviderError::Unsupported {
                feature: "the selected reasoning mode",
            });
        }
    }
    Ok(())
}

fn nested_object<'a>(
    object: &'a mut Map<String, Value>,
    key: &str,
) -> Result<&'a mut Map<String, Value>> {
    object
        .get_mut(key)
        .and_then(Value::as_object_mut)
        .ok_or_else(|| ProviderError::Configuration(format!("missing {key} object")))
}

fn extract_content(envelope: &Value, flavor: CharacterFlavor) -> Result<String> {
    let content = match flavor {
        CharacterFlavor::OpenAiResponses => envelope
            .get("output_text")
            .and_then(Value::as_str)
            .map(ToOwned::to_owned)
            .or_else(|| {
                envelope
                    .get("output")?
                    .as_array()?
                    .iter()
                    .flat_map(|item| {
                        item.get("content")
                            .and_then(Value::as_array)
                            .into_iter()
                            .flatten()
                    })
                    .find_map(|item| {
                        item.get("text")
                            .and_then(Value::as_str)
                            .map(ToOwned::to_owned)
                    })
            }),
        CharacterFlavor::OpenAiChat(_) => envelope
            .pointer("/choices/0/message/content")
            .and_then(Value::as_str)
            .map(ToOwned::to_owned),
        CharacterFlavor::Anthropic => envelope
            .get("content")
            .and_then(Value::as_array)
            .and_then(|items| {
                items
                    .iter()
                    .find_map(|item| item.get("text").and_then(Value::as_str))
            })
            .map(ToOwned::to_owned),
        CharacterFlavor::Gemini => envelope
            .pointer("/candidates/0/content/parts")
            .and_then(Value::as_array)
            .and_then(|parts| {
                parts
                    .iter()
                    .filter(|part| part.get("thought").and_then(Value::as_bool) != Some(true))
                    .find_map(|part| part.get("text").and_then(Value::as_str))
            })
            .map(ToOwned::to_owned),
        CharacterFlavor::Ollama => envelope
            .pointer("/message/content")
            .and_then(Value::as_str)
            .map(ToOwned::to_owned),
    };
    content.ok_or_else(|| ProviderError::InvalidResponse("missing model output text".to_owned()))
}

fn output_was_truncated(envelope: &Value, flavor: CharacterFlavor) -> bool {
    match flavor {
        CharacterFlavor::OpenAiResponses => {
            envelope
                .pointer("/incomplete_details/reason")
                .and_then(Value::as_str)
                .is_some_and(|reason| matches!(reason, "max_output_tokens" | "max_tokens"))
                || (envelope.get("status").and_then(Value::as_str) == Some("incomplete")
                    && envelope
                        .get("incomplete_details")
                        .is_none_or(Value::is_null))
        }
        CharacterFlavor::OpenAiChat(_) => {
            envelope
                .pointer("/choices/0/finish_reason")
                .and_then(Value::as_str)
                == Some("length")
        }
        CharacterFlavor::Anthropic => {
            envelope.get("stop_reason").and_then(Value::as_str) == Some("max_tokens")
        }
        CharacterFlavor::Gemini => {
            envelope
                .pointer("/candidates/0/finishReason")
                .and_then(Value::as_str)
                == Some("MAX_TOKENS")
        }
        CharacterFlavor::Ollama => {
            envelope.get("done_reason").and_then(Value::as_str) == Some("length")
        }
    }
}

fn extract_usage(envelope: &Value, flavor: CharacterFlavor) -> ProviderUsage {
    let (input, output, cached, reasoning) = match flavor {
        CharacterFlavor::OpenAiResponses | CharacterFlavor::OpenAiChat(_) => (
            envelope
                .pointer("/usage/input_tokens")
                .or_else(|| envelope.pointer("/usage/prompt_tokens")),
            envelope
                .pointer("/usage/output_tokens")
                .or_else(|| envelope.pointer("/usage/completion_tokens")),
            envelope.pointer("/usage/input_tokens_details/cached_tokens"),
            envelope.pointer("/usage/output_tokens_details/reasoning_tokens"),
        ),
        CharacterFlavor::Anthropic => (
            envelope.pointer("/usage/input_tokens"),
            envelope.pointer("/usage/output_tokens"),
            envelope.pointer("/usage/cache_read_input_tokens"),
            None,
        ),
        CharacterFlavor::Gemini => (
            envelope.pointer("/usageMetadata/promptTokenCount"),
            envelope.pointer("/usageMetadata/candidatesTokenCount"),
            envelope.pointer("/usageMetadata/cachedContentTokenCount"),
            envelope.pointer("/usageMetadata/thoughtsTokenCount"),
        ),
        CharacterFlavor::Ollama => (
            envelope.get("prompt_eval_count"),
            envelope.get("eval_count"),
            None,
            None,
        ),
    };
    let numeric = |value: Option<&Value>| value.and_then(Value::as_u64);
    let any = input.is_some() || output.is_some() || cached.is_some() || reasoning.is_some();
    ProviderUsage {
        source: if any {
            UsageSource::Reported
        } else {
            UsageSource::Unknown
        },
        input_tokens: numeric(input),
        output_tokens: numeric(output),
        cached_tokens: numeric(cached),
        reasoning_tokens: numeric(reasoning),
        request_id: envelope
            .get("id")
            .and_then(Value::as_str)
            .map(ToOwned::to_owned),
        ..ProviderUsage::default()
    }
}

fn parse_models(value: &Value, flavor: CharacterFlavor) -> Result<Vec<Model>> {
    let items = match flavor {
        CharacterFlavor::Ollama => value.get("models"),
        _ => value.get("data").or_else(|| value.get("models")),
    }
    .and_then(Value::as_array)
    .ok_or_else(|| ProviderError::InvalidResponse("missing model list".to_owned()))?;
    Ok(items
        .iter()
        .filter(|item| {
            match flavor {
                CharacterFlavor::Gemini => item
                    .get("supportedGenerationMethods")
                    .and_then(Value::as_array)
                    .is_some_and(|methods| {
                        methods
                            .iter()
                            .any(|method| method.as_str() == Some("generateContent"))
                    }),
                // These endpoints expose mixed catalogs without positive per-model chat
                // capability metadata. Keep manual configuration available, but discovery must
                // not present any entry as verified compatible.
                CharacterFlavor::OpenAiChat(_) | CharacterFlavor::Ollama => false,
                CharacterFlavor::OpenAiResponses | CharacterFlavor::Anthropic => true,
            }
        })
        .filter_map(|item| {
            let id = item.get("id").or_else(|| item.get("name"))?.as_str()?;
            if matches!(flavor, CharacterFlavor::OpenAiResponses)
                && !is_openai_responses_model_id(id)
            {
                return None;
            }
            Some(Model {
                id: id.trim_start_matches("models/").to_owned(),
                name: item
                    .get("displayName")
                    .or_else(|| item.get("name"))
                    .and_then(Value::as_str)
                    .unwrap_or(id)
                    .to_owned(),
                metadata: Default::default(),
            })
        })
        .collect())
}

fn parse_lm_studio_context_window(value: &Value, requested: &str) -> Result<ModelContextWindow> {
    let models = value
        .get("models")
        .and_then(Value::as_array)
        .ok_or_else(|| ProviderError::InvalidResponse("missing LM Studio model list".to_owned()))?;
    let Some(model) = models.iter().find(|model| {
        model.get("type").and_then(Value::as_str) == Some("llm")
            && (model.get("key").and_then(Value::as_str) == Some(requested)
                || model.get("selected_variant").and_then(Value::as_str) == Some(requested)
                || model
                    .get("variants")
                    .and_then(Value::as_array)
                    .is_some_and(|variants| {
                        variants
                            .iter()
                            .any(|variant| variant.as_str() == Some(requested))
                    })
                || model
                    .get("loaded_instances")
                    .and_then(Value::as_array)
                    .is_some_and(|instances| {
                        instances.iter().any(|instance| {
                            instance.get("id").and_then(Value::as_str) == Some(requested)
                        })
                    }))
    }) else {
        return Ok(ModelContextWindow::default());
    };
    let instances = model
        .get("loaded_instances")
        .and_then(Value::as_array)
        .map(Vec::as_slice)
        .unwrap_or_default();
    let exact_loaded = instances
        .iter()
        .find(|instance| instance.get("id").and_then(Value::as_str) == Some(requested));
    let loaded_tokens = exact_loaded
        .and_then(|instance| instance.pointer("/config/context_length"))
        .and_then(Value::as_u64)
        .filter(|tokens| *tokens > 0)
        .or_else(|| {
            instances
                .iter()
                .filter_map(|instance| {
                    instance
                        .pointer("/config/context_length")
                        .and_then(Value::as_u64)
                        .filter(|tokens| *tokens > 0)
                })
                .min()
        });
    Ok(ModelContextWindow {
        loaded_tokens,
        maximum_tokens: model
            .get("max_context_length")
            .and_then(Value::as_u64)
            .filter(|tokens| *tokens > 0),
    })
}

/// `OpenAI`'s model-list response has no endpoint/capability metadata. Character detection requires
/// the Responses API plus structured text output, so discovery must positively recognize a text
/// generation family and reject specialized audio, image, search, realtime, and Codex models.
///
/// Every general GPT generation from 5 on (`gpt-5`, `gpt-5.6-luna`, `gpt-6-luna`, `gpt-6-sol`,
/// ...) supports both, so new generations are accepted without an adapter release; the marker list
/// keeps their specialized variants hidden. Other unknown families remain hidden.
pub fn is_openai_responses_model_id(id: &str) -> bool {
    let normalized = id.to_ascii_lowercase();
    if [
        "audio",
        "codex",
        "computer-use",
        "deep-research",
        "embedding",
        "image",
        "moderation",
        "realtime",
        "search",
        "transcribe",
        "tts",
        "video",
        "whisper",
    ]
    .into_iter()
    .any(|marker| normalized.contains(marker))
    {
        return false;
    }

    is_general_gpt_generation(&normalized)
        || ["gpt-4.1", "gpt-4o"]
            .into_iter()
            .any(|family| normalized == family || normalized.starts_with(&format!("{family}-")))
}

/// Matches `gpt-<major>[.<minor>][-<variant>]` for major versions 5 and later.
fn is_general_gpt_generation(normalized: &str) -> bool {
    let Some(version) = normalized.strip_prefix("gpt-") else {
        return false;
    };
    let version = version
        .split_once('-')
        .map_or(version, |(version, _)| version);
    let (major, minor) = version
        .split_once('.')
        .map_or((version, None), |(major, minor)| (major, Some(minor)));
    let is_number =
        |value: &str| !value.is_empty() && value.bytes().all(|byte| byte.is_ascii_digit());
    is_number(major)
        && major.parse::<u32>().is_ok_and(|major| major >= 5)
        && minor.is_none_or(is_number)
}

fn encode_path_segment(value: &str) -> String {
    url::form_urlencoded::byte_serialize(value.as_bytes()).collect()
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use super::*;
    use crate::{Authentication, DetectionParagraph};

    #[derive(Debug)]
    struct NeverTransport;

    #[async_trait]
    impl HttpTransport for NeverTransport {
        async fn execute(&self, _request: HttpRequest) -> Result<crate::HttpResponse> {
            panic!("serialization tests must not make HTTP calls")
        }
    }

    #[derive(Debug)]
    struct TimeoutTransport;

    #[async_trait]
    impl HttpTransport for TimeoutTransport {
        async fn execute(&self, _request: HttpRequest) -> Result<crate::HttpResponse> {
            Err(ProviderError::UncertainCharge)
        }
    }

    fn request(temperature: Temperature, reasoning: ReasoningControl) -> CharacterDetectionRequest {
        CharacterDetectionRequest {
            request_id: uuid::Uuid::new_v4(),
            model: "model".to_owned(),
            system_prompt: "Detect speakers".to_owned(),
            paragraphs: vec![DetectionParagraph {
                id: "p1".to_owned(),
                text: "Hello!".to_owned(),
                context_only: false,
            }],
            temperature,
            reasoning,
            max_output_tokens: 2048,
        }
    }

    fn local_endpoint() -> EndpointConfig {
        EndpointConfig::managed_loopback(
            url::Url::parse("http://127.0.0.1:11434/").unwrap(),
            Authentication::None,
        )
        .unwrap()
    }

    #[test]
    fn omitted_and_null_temperature_remain_distinct() {
        let endpoint = EndpointConfig::external(
            url::Url::parse("http://127.0.0.1:8080/").unwrap(),
            Authentication::None,
        )
        .unwrap();
        let provider =
            OpenAiResponsesProvider::with_endpoint(endpoint, Arc::new(NeverTransport)).unwrap();
        let omitted = provider
            .build_detection_request(&request(Temperature::Default, ReasoningControl::Inherit))
            .unwrap();
        let null = provider
            .build_detection_request(&request(Temperature::Null, ReasoningControl::Inherit))
            .unwrap();
        let omitted: Value = serde_json::from_slice(&omitted.body).unwrap();
        let null: Value = serde_json::from_slice(&null.body).unwrap();
        assert!(!omitted.as_object().unwrap().contains_key("temperature"));
        assert!(null.as_object().unwrap().contains_key("temperature"));
        assert!(null["temperature"].is_null());
    }

    #[test]
    fn structured_output_schema_keeps_quote_before_speaker_order() {
        let provider = OpenAiCompatibleProvider::new(
            OpenAiChatPreset::LmStudio,
            local_endpoint(),
            Arc::new(NeverTransport),
        )
        .unwrap();
        let request = provider
            .build_detection_request(&request(Temperature::Default, ReasoningControl::Inherit))
            .unwrap();
        let body = std::str::from_utf8(&request.body).unwrap();

        assert!(!body.contains(SCHEMA_PLACEHOLDER));
        let position = |needle: &str| body.find(needle).unwrap();
        assert!(position("\"paragraph_id\":{") < position("\"quote_start\""));
        assert!(position("\"quote_start\"") < position("\"quote_end\""));
        assert!(position("\"quote_end\"") < position("\"character\":{"));
        let value: Value = serde_json::from_str(body).unwrap();
        assert_eq!(
            value["response_format"]["json_schema"]["schema"]["required"],
            json!(["characters", "dialogue"])
        );
    }

    #[test]
    fn gemini_model_discovery_keeps_generation_models() {
        let models = parse_models(
            &json!({
                "models": [
                    {
                        "name": "models/gemini-generation",
                        "displayName": "Gemini Generation",
                        "supportedGenerationMethods": ["generateContent"]
                    },
                    {
                        "name": "models/text-embedding",
                        "displayName": "Text Embedding",
                        "supportedGenerationMethods": ["embedContent"]
                    },
                    {
                        "name": "models/unknown-without-capabilities",
                        "displayName": "Unknown"
                    },
                    {
                        "name": "models/malformed-capabilities",
                        "displayName": "Malformed",
                        "supportedGenerationMethods": "generateContent"
                    }
                ]
            }),
            CharacterFlavor::Gemini,
        )
        .unwrap();

        assert_eq!(models.len(), 1);
        assert_eq!(models[0].id, "gemini-generation");
    }

    #[test]
    fn openai_compatible_mixed_catalogs_discover_no_unverified_models() {
        let catalog = json!({
            "data": [
                { "id": "qwen-chat" },
                { "id": "text-embedding" },
                { "id": "image-model" }
            ]
        });

        for preset in [
            OpenAiChatPreset::Generic,
            OpenAiChatPreset::Qwen,
            OpenAiChatPreset::Kimi,
            OpenAiChatPreset::Moonshot,
            OpenAiChatPreset::LmStudio,
        ] {
            assert!(
                parse_models(&catalog, CharacterFlavor::OpenAiChat(preset))
                    .unwrap()
                    .is_empty(),
                "{preset:?} must fail closed"
            );
        }
    }

    #[test]
    fn ollama_tags_discover_no_models_without_positive_chat_metadata() {
        let models = parse_models(
            &json!({
                "models": [
                    { "name": "llama3.2:latest" },
                    { "name": "nomic-embed-text:latest" }
                ]
            }),
            CharacterFlavor::Ollama,
        )
        .unwrap();

        assert!(models.is_empty());
    }

    #[test]
    fn lm_studio_context_discovery_prefers_the_exact_loaded_instance() {
        let catalog = json!({
            "models": [{
                "type": "llm",
                "key": "owner/model",
                "selected_variant": "owner/model@q4",
                "variants": ["owner/model@q4"],
                "max_context_length": 131_072,
                "loaded_instances": [
                    {"id": "model-small", "config": {"context_length": 4096}},
                    {"id": "model-large", "config": {"context_length": 32768}}
                ]
            }]
        });

        assert_eq!(
            parse_lm_studio_context_window(&catalog, "model-large").unwrap(),
            ModelContextWindow {
                loaded_tokens: Some(32_768),
                maximum_tokens: Some(131_072),
            }
        );
        assert_eq!(
            parse_lm_studio_context_window(&catalog, "owner/model").unwrap(),
            ModelContextWindow {
                // Routing by the model key can select either instance, so use the safe minimum.
                loaded_tokens: Some(4_096),
                maximum_tokens: Some(131_072),
            }
        );
    }

    #[test]
    fn unloaded_lm_studio_model_reports_only_its_architectural_maximum() {
        let window = parse_lm_studio_context_window(
            &json!({"models": [{
                "type": "llm",
                "key": "owner/model",
                "max_context_length": 262_144,
                "loaded_instances": []
            }]}),
            "owner/model",
        )
        .unwrap();

        assert_eq!(window.loaded_tokens, None);
        assert_eq!(window.maximum_tokens, Some(262_144));
    }

    #[test]
    fn provider_completion_limits_are_classified_as_output_truncation() {
        let limited = [
            (
                json!({"status": "incomplete", "incomplete_details": {"reason": "max_output_tokens"}}),
                CharacterFlavor::OpenAiResponses,
            ),
            (
                json!({"choices": [{"finish_reason": "length"}]}),
                CharacterFlavor::OpenAiChat(OpenAiChatPreset::LmStudio),
            ),
            (
                json!({"stop_reason": "max_tokens"}),
                CharacterFlavor::Anthropic,
            ),
            (
                json!({"candidates": [{"finishReason": "MAX_TOKENS"}]}),
                CharacterFlavor::Gemini,
            ),
            (json!({"done_reason": "length"}), CharacterFlavor::Ollama),
        ];

        for (envelope, flavor) in limited {
            assert!(output_was_truncated(&envelope, flavor), "{flavor:?}");
        }
        assert!(!output_was_truncated(
            &json!({"choices": [{"finish_reason": "stop"}]}),
            CharacterFlavor::OpenAiChat(OpenAiChatPreset::LmStudio),
        ));
    }

    #[test]
    fn openai_model_discovery_keeps_only_responses_text_models() {
        let models = parse_models(
            &json!({
                "data": [
                    { "id": "gpt-6-luna" },
                    { "id": "gpt-5.6-luna" },
                    { "id": "gpt-4.1-mini" },
                    { "id": "gpt-4o-mini-tts" },
                    { "id": "gpt-image-1" },
                    { "id": "text-embedding-3-large" },
                    { "id": "whisper-1" }
                ]
            }),
            CharacterFlavor::OpenAiResponses,
        )
        .unwrap();

        assert_eq!(
            models
                .iter()
                .map(|model| model.id.as_str())
                .collect::<Vec<_>>(),
            ["gpt-6-luna", "gpt-5.6-luna", "gpt-4.1-mini"]
        );
    }

    #[test]
    fn current_and_future_gpt_generations_are_recognized_for_detection() {
        for id in [
            "gpt-5",
            "gpt-5-mini",
            "gpt-5.6-luna",
            "gpt-6-luna",
            "gpt-6-sol",
            "GPT-6-Luna",
            "gpt-6.1-luna",
            "gpt-7",
            "gpt-4.1-mini",
            "gpt-4o",
        ] {
            assert!(is_openai_responses_model_id(id), "{id} should be offered");
        }
        for id in [
            "gpt-6-realtime",
            "gpt-6-luna-audio",
            "gpt-6-luna-tts",
            "gpt-6-image",
            "gpt-6-codex",
            "gpt-3.5-turbo",
            "gpt-4",
            "gpt-6x",
            "gpt-6.x-luna",
            "gpt-",
            "o3-mini",
        ] {
            assert!(!is_openai_responses_model_id(id), "{id} should stay hidden");
        }
    }

    #[test]
    fn unsupported_reasoning_is_rejected_before_dispatch() {
        let provider = OpenAiCompatibleProvider::new(
            OpenAiChatPreset::LmStudio,
            local_endpoint(),
            Arc::new(NeverTransport),
        )
        .unwrap();
        let result = provider.build_detection_request(&request(
            Temperature::Default,
            ReasoningControl::TokenBudget { tokens: 4096 },
        ));
        assert!(matches!(result, Err(ProviderError::Unsupported { .. })));
    }

    #[test]
    fn lm_studio_detection_allows_slow_local_generation() {
        let provider = OpenAiCompatibleProvider::new(
            OpenAiChatPreset::LmStudio,
            local_endpoint(),
            Arc::new(NeverTransport),
        )
        .unwrap();

        let request = provider
            .build_detection_request(&request(Temperature::Default, ReasoningControl::Inherit))
            .unwrap();

        assert_eq!(request.timeout, LOCAL_DETECTION_TIMEOUT);
    }

    #[tokio::test]
    async fn lm_studio_timeout_does_not_claim_an_uncertain_charge() {
        let provider = OpenAiCompatibleProvider::new(
            OpenAiChatPreset::LmStudio,
            local_endpoint(),
            Arc::new(TimeoutTransport),
        )
        .unwrap();

        let error = provider
            .detect_characters(request(Temperature::Default, ReasoningControl::Inherit))
            .await
            .unwrap_err();

        assert!(matches!(&error, ProviderError::Transport(_)));
        assert!(!error.to_string().contains("billing"));
    }

    #[tokio::test]
    async fn local_ollama_timeout_is_transient_and_allows_slow_generation() {
        let provider = OllamaProvider::new(local_endpoint(), Arc::new(TimeoutTransport)).unwrap();
        let prepared = provider
            .build_detection_request(&request(Temperature::Default, ReasoningControl::Inherit))
            .unwrap();
        assert_eq!(prepared.timeout, LOCAL_DETECTION_TIMEOUT);

        let error = provider
            .detect_characters(request(Temperature::Default, ReasoningControl::Inherit))
            .await
            .unwrap_err();
        assert!(matches!(&error, ProviderError::Transport(_)));
    }

    #[test]
    fn gemini_thought_summaries_are_not_parsed_as_output() {
        let envelope = json!({"candidates": [{"content": {"parts": [
            {"text": "thinking about speakers", "thought": true},
            {"text": "{\"characters\":[],\"dialogue\":[]}"}
        ]}}]});
        assert_eq!(
            extract_content(&envelope, CharacterFlavor::Gemini).unwrap(),
            "{\"characters\":[],\"dialogue\":[]}"
        );
    }

    #[tokio::test]
    async fn generic_timeout_retains_the_billing_safe_classification() {
        let provider = OpenAiCompatibleProvider::new(
            OpenAiChatPreset::Generic,
            local_endpoint(),
            Arc::new(TimeoutTransport),
        )
        .unwrap();

        let error = provider
            .detect_characters(request(Temperature::Default, ReasoningControl::Inherit))
            .await
            .unwrap_err();

        assert!(matches!(error, ProviderError::UncertainCharge));
    }

    #[test]
    fn anthropic_effort_uses_output_config_and_passes_new_levels_through() {
        let provider = AnthropicProvider::new(
            crate::Credential::new("test-only"),
            Arc::new(NeverTransport),
        )
        .unwrap();
        let request = provider
            .build_detection_request(&request(
                Temperature::Default,
                ReasoningControl::Effort {
                    effort: crate::ReasoningEffort::new("xhigh").unwrap(),
                },
            ))
            .unwrap();
        let body: Value = serde_json::from_slice(&request.body).unwrap();
        assert_eq!(body["output_config"], json!({ "effort": "xhigh" }));
        assert!(body.get("thinking").is_none());
    }

    #[test]
    fn ollama_reasoning_effort_uses_think_field() {
        let provider = OllamaProvider::new(local_endpoint(), Arc::new(NeverTransport)).unwrap();
        let request = provider
            .build_detection_request(&request(
                Temperature::Value(0.2),
                ReasoningControl::Effort {
                    effort: crate::ReasoningEffort::new("low").unwrap(),
                },
            ))
            .unwrap();
        let body: Value = serde_json::from_slice(&request.body).unwrap();
        assert_eq!(body["think"], "low");
        let temperature = body["options"]["temperature"].as_f64().unwrap();
        assert!((temperature - 0.2).abs() < 0.000_01);
    }
}
