use std::{collections::BTreeSet, sync::Arc};

use async_trait::async_trait;
use bytes::{BufMut, Bytes, BytesMut};
use chrono::Utc;
use futures::StreamExt;
use serde_json::{Value, json};

use super::{HttpAdapter, json_body};
use crate::{
    AudioChunk, AudioChunkSink, AudioFormat, Authentication, CancellationFlag, DeliveryCue,
    EndpointConfig, HttpMethod, HttpRequest, HttpTransport, Model, ModelPerformanceCapabilities,
    ParameterSupport, PerformanceCapabilities, PerformanceRange, ProviderCapabilities,
    ProviderDescriptor, ProviderError, ProviderHealth, ProviderId, ProviderUsage, Result,
    StreamingSynthesisResponse, SynthesisRequest, SynthesisResponse, TtsProvider, UsageSource,
    Voice, VoiceClone, VoiceCloneProvider, VoiceCloneRequest,
};

fn capabilities(
    streaming: bool,
    voice_cloning: bool,
    pronunciation: bool,
    model_discovery: bool,
) -> ProviderCapabilities {
    ProviderCapabilities {
        streaming,
        voice_cloning,
        pronunciation,
        model_discovery,
        temperature: ParameterSupport::Unsupported,
        reasoning: BTreeSet::new(),
        ..ProviderCapabilities::default()
    }
}

fn elevenlabs_performance_capabilities() -> Vec<ModelPerformanceCapabilities> {
    let performance = PerformanceCapabilities {
        speed: Some(PerformanceRange::new(0.7, 1.2)),
        pitch: None,
        stability: Some(PerformanceRange::new(0.0, 1.0)),
        similarity: Some(PerformanceRange::new(0.0, 1.0)),
        style: Some(PerformanceRange::new(0.0, 1.0)),
        speaker_boost: true,
        delivery_cues: Vec::new(),
    };
    [
        "eleven_multilingual_v2",
        "eleven_flash_v2_5",
        "eleven_turbo_v2_5",
    ]
    .into_iter()
    .map(|model| ModelPerformanceCapabilities {
        model: model.to_owned(),
        performance: performance.clone(),
    })
    .collect()
}

fn audio_content_type(format: AudioFormat) -> &'static str {
    match format {
        AudioFormat::PcmS16Le | AudioFormat::PcmF32Le => "audio/pcm",
        AudioFormat::Mp3 => "audio/mpeg",
        AudioFormat::Wav => "audio/wav",
        AudioFormat::Flac => "audio/flac",
        AudioFormat::Aac => "audio/aac",
    }
}

fn response_content_type(response: &crate::HttpResponse, fallback: AudioFormat) -> String {
    response
        .headers
        .get("content-type")
        .cloned()
        .unwrap_or_else(|| audio_content_type(fallback).to_owned())
}

fn usage_from_headers(response: &crate::HttpResponse, text_len: usize) -> ProviderUsage {
    usage_from_header_values(&response.headers, text_len)
}

fn usage_from_header_values(
    headers: &std::collections::BTreeMap<String, String>,
    text_len: usize,
) -> ProviderUsage {
    let reported_characters = headers
        .get("x-character-count")
        .and_then(|value| value.parse().ok());
    ProviderUsage {
        source: if reported_characters.is_some() {
            UsageSource::Reported
        } else {
            UsageSource::Estimated
        },
        characters: reported_characters.or_else(|| u64::try_from(text_len).ok()),
        request_id: headers
            .get("request-id")
            .or_else(|| headers.get("x-request-id"))
            .cloned(),
        ..ProviderUsage::default()
    }
}

#[derive(Clone, Debug)]
pub struct ElevenLabsProvider {
    descriptor: ProviderDescriptor,
    capabilities: ProviderCapabilities,
    http: HttpAdapter,
}

impl ElevenLabsProvider {
    pub fn new(api_key: crate::Credential, transport: Arc<dyn HttpTransport>) -> Result<Self> {
        let endpoint = EndpointConfig::cloud(
            url::Url::parse("https://api.elevenlabs.io/")
                .map_err(|error| ProviderError::Configuration(error.to_string()))?,
            Authentication::Header {
                name: "xi-api-key".to_owned(),
                value: api_key,
            },
        )?;
        Self::with_endpoint(endpoint, transport)
    }

    pub fn with_endpoint(
        endpoint: EndpointConfig,
        transport: Arc<dyn HttpTransport>,
    ) -> Result<Self> {
        let mut capabilities = capabilities(true, true, true, true);
        capabilities.set_model_performance(elevenlabs_performance_capabilities())?;
        Ok(Self {
            descriptor: ProviderDescriptor {
                id: ProviderId::new("elevenlabs")?,
                display_name: "ElevenLabs".to_owned(),
                kind: endpoint.kind,
                endpoint_family: "elevenlabs-v1".to_owned(),
            },
            capabilities,
            http: HttpAdapter::new(endpoint, transport),
        })
    }

    pub fn build_synthesis_request(&self, request: &SynthesisRequest) -> Result<HttpRequest> {
        if request.text.trim().is_empty() || request.voice.trim().is_empty() {
            return Err(ProviderError::Configuration(
                "text and voice are required".to_owned(),
            ));
        }
        let output_format = match request.format {
            AudioFormat::Mp3 => "mp3_44100_128",
            AudioFormat::PcmS16Le | AudioFormat::PcmF32Le => "pcm_44100",
            _ => {
                return Err(ProviderError::Unsupported {
                    feature: "the requested ElevenLabs audio format",
                });
            }
        };
        let effective_model = request.model.as_deref().unwrap_or("eleven_multilingual_v2");
        request.validate_performance(&self.capabilities, effective_model)?;
        if !request.options.is_empty() {
            return Err(ProviderError::Unsupported {
                feature: "untyped ElevenLabs voice options",
            });
        }
        if request.performance.pitch.is_some() || request.performance.delivery_cue.is_some() {
            return Err(ProviderError::Unsupported {
                feature: "the requested ElevenLabs performance control",
            });
        }
        let mut body = json!({
            "text": request.text,
            "model_id": effective_model
        });
        if !request.pronunciation_dictionary_ids.is_empty() {
            body["pronunciation_dictionary_locators"] = Value::Array(
                request
                    .pronunciation_dictionary_ids
                    .iter()
                    .map(|id| json!({ "pronunciation_dictionary_id": id }))
                    .collect(),
            );
        }
        let mut voice_settings = serde_json::Map::new();
        for (name, value) in [
            ("speed", request.performance.speed),
            ("stability", request.performance.stability),
            ("similarity_boost", request.performance.similarity),
            ("style", request.performance.style),
        ] {
            if let Some(value) = value {
                voice_settings.insert(name.to_owned(), json!(value));
            }
        }
        if let Some(value) = request.performance.speaker_boost {
            voice_settings.insert("use_speaker_boost".to_owned(), json!(value));
        }
        if !voice_settings.is_empty() {
            body["voice_settings"] = Value::Object(voice_settings);
        }
        let path = format!(
            "v1/text-to-speech/{}/stream?output_format={output_format}",
            encode_path_segment(&request.voice)
        );
        let accept = audio_content_type(request.format);
        self.http
            .json_request(HttpMethod::Post, &path, &body)
            .map(|http_request| http_request.header("accept", accept))
    }

    async fn clone_multipart(
        &self,
        method: HttpMethod,
        path: &str,
        fields: &[(&str, &str)],
        samples: &[crate::VoiceSample],
    ) -> Result<crate::HttpResponse> {
        let boundary = format!("audiobookai-{}", uuid::Uuid::new_v4().simple());
        let mut body = BytesMut::new();
        for (name, value) in fields {
            body.put(format!("--{boundary}\r\n").as_bytes());
            body.put(format!("Content-Disposition: form-data; name=\"{name}\"\r\n\r\n").as_bytes());
            body.put(value.as_bytes());
            body.put(&b"\r\n"[..]);
        }
        for sample in samples {
            reject_multipart_value(&sample.file_name)?;
            reject_multipart_value(&sample.content_type)?;
            body.put(format!("--{boundary}\r\n").as_bytes());
            body.put(
                format!(
                    "Content-Disposition: form-data; name=\"files\"; filename=\"{}\"\r\nContent-Type: {}\r\n\r\n",
                    sample.file_name, sample.content_type
                )
                .as_bytes(),
            );
            body.put(sample.bytes.clone());
            body.put(&b"\r\n"[..]);
        }
        body.put(format!("--{boundary}--\r\n").as_bytes());

        let mut request = self.http.empty_request(method, path)?;
        request.headers.insert(
            "content-type".to_owned(),
            format!("multipart/form-data; boundary={boundary}"),
        );
        request.body = body.freeze();
        self.http.execute(request).await
    }
}

fn encode_path_segment(value: &str) -> String {
    url::form_urlencoded::byte_serialize(value.as_bytes()).collect()
}

fn reject_multipart_value(value: &str) -> Result<()> {
    if value.contains(['\r', '\n', '"']) {
        Err(ProviderError::Configuration(
            "unsafe multipart metadata".to_owned(),
        ))
    } else {
        Ok(())
    }
}

#[async_trait]
impl TtsProvider for ElevenLabsProvider {
    fn descriptor(&self) -> &ProviderDescriptor {
        &self.descriptor
    }

    fn capabilities(&self) -> &ProviderCapabilities {
        &self.capabilities
    }

    async fn health(&self) -> Result<ProviderHealth> {
        self.http.basic_health("v1/models").await
    }

    async fn discover_voices(&self) -> Result<Vec<Voice>> {
        let response = self
            .http
            .execute(self.http.empty_request(HttpMethod::Get, "v1/voices")?)
            .await?;
        let value = json_body(&response)?;
        Ok(value["voices"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|voice| {
                Some(Voice {
                    id: voice.get("voice_id")?.as_str()?.to_owned(),
                    name: voice.get("name")?.as_str()?.to_owned(),
                    language: voice
                        .pointer("/labels/language")
                        .and_then(Value::as_str)
                        .map(ToOwned::to_owned),
                    owned_clone: voice
                        .get("category")
                        .and_then(Value::as_str)
                        .is_some_and(|category| category == "cloned"),
                    metadata: Default::default(),
                })
            })
            .collect())
    }

    async fn discover_models(&self) -> Result<Vec<Model>> {
        let response = self
            .http
            .execute(self.http.empty_request(HttpMethod::Get, "v1/models")?)
            .await?;
        Ok(parse_elevenlabs_models(&json_body(&response)?))
    }

    async fn synthesize(&self, request: SynthesisRequest) -> Result<SynthesisResponse> {
        let text_len = request.text.chars().count();
        let format = request.format;
        let http_request = self.build_synthesis_request(&request)?;
        let response = self.http.execute(http_request).await?;
        let usage = usage_from_headers(&response, text_len);
        Ok(SynthesisResponse {
            content_type: response_content_type(&response, format),
            audio: response.body,
            usage,
        })
    }

    async fn synthesize_stream(
        &self,
        request: SynthesisRequest,
        cancellation: CancellationFlag,
        sink: Arc<dyn AudioChunkSink>,
    ) -> Result<StreamingSynthesisResponse> {
        let format = request.format;
        let request_id = request.request_id;
        let text_len = request.text.chars().count();
        let http_request = self.build_synthesis_request(&request)?;
        stream_response(
            &self.http,
            http_request,
            request_id,
            format,
            text_len,
            cancellation,
            sink,
        )
        .await
    }
}

fn parse_elevenlabs_models(value: &Value) -> Vec<Model> {
    value
        .as_array()
        .into_iter()
        .flatten()
        // The shared catalog also contains speech-to-text, music, and other models. Keep only
        // models that are not explicitly marked as unsuitable for this TTS adapter.
        .filter(|model| model.get("can_do_text_to_speech").and_then(Value::as_bool) != Some(false))
        .filter_map(|model| {
            let id = model.get("model_id")?.as_str()?;
            Some(Model {
                id: id.to_owned(),
                name: model
                    .get("name")
                    .and_then(Value::as_str)
                    .unwrap_or(id)
                    .to_owned(),
                metadata: Default::default(),
            })
        })
        .collect()
}

#[async_trait]
impl VoiceCloneProvider for ElevenLabsProvider {
    fn descriptor(&self) -> &ProviderDescriptor {
        &self.descriptor
    }

    async fn create_clone(&self, request: VoiceCloneRequest) -> Result<VoiceClone> {
        if request.samples.is_empty() {
            return Err(ProviderError::Configuration(
                "at least one voice sample is required".to_owned(),
            ));
        }
        let response = self
            .clone_multipart(
                HttpMethod::Post,
                "v1/voices/add",
                &[
                    ("name", request.name.as_str()),
                    ("description", request.description.as_deref().unwrap_or("")),
                ],
                &request.samples,
            )
            .await?;
        let value = json_body(&response)?;
        let voice_id = value
            .get("voice_id")
            .and_then(Value::as_str)
            .ok_or_else(|| ProviderError::InvalidResponse("missing voice_id".to_owned()))?;
        Ok(VoiceClone {
            provider_voice_id: voice_id.to_owned(),
            name: request.name,
            owned_by_audiobookai: true,
            created_at: Utc::now(),
        })
    }

    async fn update_clone(&self, clone: &VoiceClone, name: String) -> Result<VoiceClone> {
        if !clone.owned_by_audiobookai {
            return Err(ProviderError::NotOwned);
        }
        let path = format!(
            "v1/voices/{}/edit",
            encode_path_segment(&clone.provider_voice_id)
        );
        self.clone_multipart(HttpMethod::Post, &path, &[("name", &name)], &[])
            .await?;
        let mut updated = clone.clone();
        updated.name = name;
        Ok(updated)
    }

    async fn delete_owned_clone(&self, clone: &VoiceClone, confirmed: bool) -> Result<()> {
        if !clone.owned_by_audiobookai {
            return Err(ProviderError::NotOwned);
        }
        if !confirmed {
            return Err(ProviderError::Configuration(
                "remote clone deletion requires explicit confirmation".to_owned(),
            ));
        }
        let path = format!(
            "v1/voices/{}",
            encode_path_segment(&clone.provider_voice_id)
        );
        self.http
            .execute(self.http.empty_request(HttpMethod::Delete, &path)?)
            .await?;
        Ok(())
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum OpenAiSpeechFlavor {
    OpenAi,
    Mlx,
    LocalAi,
}

/// Exact model capabilities documented for `OpenAI`'s speech endpoint.
///
/// Unknown or newly discovered model identifiers remain selectable, but their optional
/// performance controls fail closed until their contract is positively known.
pub fn openai_tts_model_performance_capabilities() -> Vec<ModelPerformanceCapabilities> {
    let expressive = PerformanceCapabilities {
        speed: Some(PerformanceRange::new(0.25, 4.0)),
        delivery_cues: vec![
            DeliveryCue::Whisper,
            DeliveryCue::Shout,
            DeliveryCue::Sarcastic,
            DeliveryCue::Curious,
            DeliveryCue::Excited,
            DeliveryCue::Crying,
            DeliveryCue::Mischievous,
        ],
        ..PerformanceCapabilities::default()
    };
    let legacy = PerformanceCapabilities {
        speed: Some(PerformanceRange::new(0.25, 4.0)),
        ..PerformanceCapabilities::default()
    };
    [
        ("gpt-4o-mini-tts", expressive.clone()),
        ("gpt-4o-mini-tts-2025-12-15", expressive),
        ("tts-1", legacy.clone()),
        ("tts-1-hd", legacy),
    ]
    .into_iter()
    .map(|(model, performance)| ModelPerformanceCapabilities {
        model: model.to_owned(),
        performance,
    })
    .collect()
}

#[derive(Clone, Debug)]
struct OpenAiSpeechProvider {
    descriptor: ProviderDescriptor,
    capabilities: ProviderCapabilities,
    http: HttpAdapter,
    flavor: OpenAiSpeechFlavor,
}

impl OpenAiSpeechProvider {
    fn new(
        id: &str,
        name: &str,
        endpoint_family: &str,
        endpoint: EndpointConfig,
        transport: Arc<dyn HttpTransport>,
        flavor: OpenAiSpeechFlavor,
    ) -> Result<Self> {
        let mut provider_capabilities = capabilities(true, false, false, true);
        if matches!(flavor, OpenAiSpeechFlavor::OpenAi) {
            provider_capabilities
                .set_model_performance(openai_tts_model_performance_capabilities())?;
        }
        Ok(Self {
            descriptor: ProviderDescriptor {
                id: ProviderId::new(id)?,
                display_name: name.to_owned(),
                kind: endpoint.kind,
                endpoint_family: endpoint_family.to_owned(),
            },
            capabilities: provider_capabilities,
            http: HttpAdapter::new(endpoint, transport),
            flavor,
        })
    }

    fn build_request(&self, request: &SynthesisRequest) -> Result<HttpRequest> {
        let response_format = match request.format {
            AudioFormat::Mp3 => "mp3",
            AudioFormat::Wav => "wav",
            AudioFormat::Flac => "flac",
            AudioFormat::PcmS16Le | AudioFormat::PcmF32Le => "pcm",
            AudioFormat::Aac => "aac",
        };
        let default_model = match self.flavor {
            OpenAiSpeechFlavor::OpenAi => "gpt-4o-mini-tts",
            OpenAiSpeechFlavor::Mlx => "kokoro",
            OpenAiSpeechFlavor::LocalAi => "tts-1",
        };
        let effective_model = request.model.as_deref().unwrap_or(default_model);
        request.validate_performance(&self.capabilities, effective_model)?;
        let mut body = json!({
            "model": effective_model,
            "input": request.text,
            "voice": request.voice,
            "response_format": response_format
        });
        match self.flavor {
            OpenAiSpeechFlavor::OpenAi => {
                if !request.options.is_empty() {
                    return Err(ProviderError::Unsupported {
                        feature: "untyped OpenAI speech options",
                    });
                }
                if request.performance.pitch.is_some()
                    || request.performance.stability.is_some()
                    || request.performance.similarity.is_some()
                    || request.performance.style.is_some()
                    || request.performance.speaker_boost.is_some()
                {
                    return Err(ProviderError::Unsupported {
                        feature: "the requested OpenAI speech performance control",
                    });
                }
                if let Some(value) = request.performance.speed {
                    body["speed"] = json!(value);
                }
                if let Some(value) = request.performance.delivery_cue {
                    body["instructions"] = json!(value.as_str());
                }
            }
            OpenAiSpeechFlavor::Mlx => {
                if !request.options.is_empty() {
                    return Err(ProviderError::Unsupported {
                        feature: "untyped MLX-audio synthesis options",
                    });
                }
                if request.performance.stability.is_some()
                    || request.performance.similarity.is_some()
                    || request.performance.style.is_some()
                    || request.performance.speaker_boost.is_some()
                {
                    return Err(ProviderError::Unsupported {
                        feature: "the requested MLX-audio performance control",
                    });
                }
                if let Some(value) = request.performance.speed {
                    body["speed"] = json!(value);
                }
                if let Some(value) = request.performance.pitch {
                    body["pitch"] = json!(value);
                }
                if let Some(value) = &request.performance.delivery_cue {
                    body["instruct"] = json!(value.as_str());
                }
            }
            OpenAiSpeechFlavor::LocalAi => {
                if !request.options.is_empty() {
                    body["options"] = serde_json::to_value(&request.options)
                        .map_err(|error| ProviderError::Configuration(error.to_string()))?;
                }
            }
        }
        self.http
            .json_request(HttpMethod::Post, "v1/audio/speech", &body)
    }

    async fn voices(&self) -> Result<Vec<Voice>> {
        if matches!(self.flavor, OpenAiSpeechFlavor::OpenAi) {
            return Ok(openai_builtin_voices());
        }
        let paths: &[&str] = match self.flavor {
            OpenAiSpeechFlavor::OpenAi => unreachable!("handled above"),
            OpenAiSpeechFlavor::Mlx => &["v1/voices"],
            OpenAiSpeechFlavor::LocalAi => &["v1/models"],
        };
        let response = self
            .http
            .execute(self.http.empty_request(HttpMethod::Get, paths[0])?)
            .await?;
        let value = json_body(&response)?;
        let items = value
            .get("data")
            .or_else(|| value.get("voices"))
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        Ok(items
            .iter()
            .filter_map(|item| {
                let id = item.get("id").or_else(|| item.get("voice_id"))?.as_str()?;
                Some(Voice {
                    id: id.to_owned(),
                    name: item
                        .get("name")
                        .and_then(Value::as_str)
                        .unwrap_or(id)
                        .to_owned(),
                    language: item
                        .get("language")
                        .and_then(Value::as_str)
                        .map(ToOwned::to_owned),
                    owned_clone: false,
                    metadata: Default::default(),
                })
            })
            .collect())
    }

    async fn models(&self) -> Result<Vec<Model>> {
        let response = self
            .http
            .execute(self.http.empty_request(HttpMethod::Get, "v1/models")?)
            .await?;
        let value = json_body(&response)?;
        if matches!(self.flavor, OpenAiSpeechFlavor::OpenAi) {
            parse_openai_tts_models(&value)
        } else {
            parse_openai_models(&value)
        }
    }

    async fn health(&self) -> Result<ProviderHealth> {
        let path = if matches!(self.flavor, OpenAiSpeechFlavor::OpenAi) {
            "v1/models"
        } else {
            "health"
        };
        self.http.basic_health(path).await
    }

    async fn synthesize(&self, request: SynthesisRequest) -> Result<SynthesisResponse> {
        let format = request.format;
        let text_len = request.text.chars().count();
        let http_request = self.build_request(&request)?;
        let response = self.http.execute(http_request).await?;
        let usage = usage_from_headers(&response, text_len);
        Ok(SynthesisResponse {
            content_type: response_content_type(&response, format),
            audio: response.body,
            usage,
        })
    }

    async fn synthesize_stream(
        &self,
        request: SynthesisRequest,
        cancellation: CancellationFlag,
        sink: Arc<dyn AudioChunkSink>,
    ) -> Result<StreamingSynthesisResponse> {
        let format = request.format;
        let request_id = request.request_id;
        let text_len = request.text.chars().count();
        let http_request = self.build_request(&request)?;
        stream_response(
            &self.http,
            http_request,
            request_id,
            format,
            text_len,
            cancellation,
            sink,
        )
        .await
    }
}

macro_rules! openai_speech_wrapper {
    ($name:ident) => {
        #[derive(Clone, Debug)]
        pub struct $name(OpenAiSpeechProvider);

        impl $name {
            pub fn build_synthesis_request(
                &self,
                request: &SynthesisRequest,
            ) -> Result<HttpRequest> {
                self.0.build_request(request)
            }
        }

        #[async_trait]
        impl TtsProvider for $name {
            fn descriptor(&self) -> &ProviderDescriptor {
                &self.0.descriptor
            }
            fn capabilities(&self) -> &ProviderCapabilities {
                &self.0.capabilities
            }
            async fn health(&self) -> Result<ProviderHealth> {
                self.0.health().await
            }
            async fn discover_voices(&self) -> Result<Vec<Voice>> {
                self.0.voices().await
            }
            async fn discover_models(&self) -> Result<Vec<Model>> {
                self.0.models().await
            }
            async fn synthesize(&self, request: SynthesisRequest) -> Result<SynthesisResponse> {
                self.0.synthesize(request).await
            }
            async fn synthesize_stream(
                &self,
                request: SynthesisRequest,
                cancellation: CancellationFlag,
                sink: Arc<dyn AudioChunkSink>,
            ) -> Result<StreamingSynthesisResponse> {
                self.0.synthesize_stream(request, cancellation, sink).await
            }
        }
    };
}

openai_speech_wrapper!(MlxAudioProvider);
openai_speech_wrapper!(LocalAiProvider);
openai_speech_wrapper!(OpenAiTtsProvider);

impl OpenAiTtsProvider {
    pub fn new(endpoint: EndpointConfig, transport: Arc<dyn HttpTransport>) -> Result<Self> {
        Ok(Self(OpenAiSpeechProvider::new(
            "openai-tts",
            "OpenAI Speech",
            "openai-speech-v1",
            endpoint,
            transport,
            OpenAiSpeechFlavor::OpenAi,
        )?))
    }
}

impl MlxAudioProvider {
    pub fn new(endpoint: EndpointConfig, transport: Arc<dyn HttpTransport>) -> Result<Self> {
        Ok(Self(OpenAiSpeechProvider::new(
            "mlx-audio",
            "MLX-audio",
            "openai-audio-v1",
            endpoint,
            transport,
            OpenAiSpeechFlavor::Mlx,
        )?))
    }

    /// Installs exact, positively verified model capabilities used to gate MLX request fields.
    ///
    /// MLX-audio exposes a common request schema while individual loaded models implement only a
    /// subset of its controls. Callers must therefore provide descriptors obtained for the exact
    /// selected model; no provider-wide fallback is used.
    pub fn with_model_performance(
        mut self,
        descriptors: Vec<ModelPerformanceCapabilities>,
    ) -> Result<Self> {
        for descriptor in &descriptors {
            let performance = &descriptor.performance;
            if performance.stability.is_some()
                || performance.similarity.is_some()
                || performance.style.is_some()
                || performance.speaker_boost
            {
                return Err(ProviderError::Configuration(
                    "MLX-audio capability descriptors may advertise only speed, pitch, and delivery"
                        .to_owned(),
                ));
            }
        }
        self.0.capabilities.set_model_performance(descriptors)?;
        Ok(self)
    }
}

impl LocalAiProvider {
    pub fn new(endpoint: EndpointConfig, transport: Arc<dyn HttpTransport>) -> Result<Self> {
        Ok(Self(OpenAiSpeechProvider::new(
            "localai",
            "LocalAI",
            "openai-audio-v1",
            endpoint,
            transport,
            OpenAiSpeechFlavor::LocalAi,
        )?))
    }
}

fn parse_openai_models(value: &Value) -> Result<Vec<Model>> {
    let items = value
        .get("data")
        .and_then(Value::as_array)
        .ok_or_else(|| ProviderError::InvalidResponse("missing model data array".to_owned()))?;
    Ok(items
        .iter()
        .filter_map(|item| {
            let id = item.get("id")?.as_str()?;
            Some(Model {
                id: id.to_owned(),
                name: id.to_owned(),
                metadata: Default::default(),
            })
        })
        .collect())
}

fn parse_openai_tts_models(value: &Value) -> Result<Vec<Model>> {
    Ok(parse_openai_models(value)?
        .into_iter()
        .filter(|model| is_openai_tts_model_id(&model.id))
        .collect())
}

fn is_openai_tts_model_id(id: &str) -> bool {
    id == "tts-1" || id.starts_with("tts-1-") || id.ends_with("-tts") || id.contains("-tts-")
}

fn openai_builtin_voices() -> Vec<Voice> {
    [
        "alloy", "ash", "ballad", "coral", "echo", "fable", "onyx", "nova", "sage", "shimmer",
        "verse", "marin", "cedar",
    ]
    .into_iter()
    .map(|id| Voice {
        id: id.to_owned(),
        name: id.to_owned(),
        language: None,
        owned_clone: false,
        metadata: Default::default(),
    })
    .collect()
}

async fn stream_response(
    http: &HttpAdapter,
    request: HttpRequest,
    request_id: uuid::Uuid,
    format: AudioFormat,
    text_len: usize,
    cancellation: CancellationFlag,
    sink: Arc<dyn AudioChunkSink>,
) -> Result<StreamingSynthesisResponse> {
    let mut response = http.transport.execute_stream(request).await?;
    let content_type = response
        .headers
        .get("content-type")
        .cloned()
        .unwrap_or_else(|| audio_content_type(format).to_owned());
    let usage = usage_from_header_values(&response.headers, text_len);
    let mut sequence = 0_u64;
    while let Some(chunk) = response.body.next().await {
        if cancellation.is_cancelled() {
            return Err(ProviderError::Cancelled);
        }
        // A body-stream failure occurs only after the provider accepted the request and returned
        // a successful response. Billing is therefore unknown; classifying it as a transient
        // connection failure could automatically dispatch and charge the same synthesis twice.
        let data = chunk.map_err(|_| ProviderError::UncertainCharge)?;
        if data.is_empty() {
            continue;
        }
        sink.send(AudioChunk {
            request_id,
            sequence,
            format,
            sample_rate: None,
            channels: None,
            data,
            final_chunk: false,
        })
        .await?;
        sequence = sequence.saturating_add(1);
    }
    sink.send(AudioChunk {
        request_id,
        sequence,
        format,
        sample_rate: None,
        channels: None,
        data: Bytes::new(),
        final_chunk: true,
    })
    .await?;
    Ok(StreamingSynthesisResponse {
        content_type,
        usage,
    })
}

#[derive(Clone, Debug)]
pub struct AllTalkProvider {
    descriptor: ProviderDescriptor,
    capabilities: ProviderCapabilities,
    http: HttpAdapter,
}

impl AllTalkProvider {
    pub fn new(endpoint: EndpointConfig, transport: Arc<dyn HttpTransport>) -> Result<Self> {
        Ok(Self {
            descriptor: ProviderDescriptor {
                id: ProviderId::new("alltalk-v2")?,
                display_name: "AllTalk V2".to_owned(),
                kind: endpoint.kind,
                endpoint_family: "alltalk-v2".to_owned(),
            },
            capabilities: capabilities(false, false, false, false),
            http: HttpAdapter::new(endpoint, transport),
        })
    }

    pub fn build_synthesis_request(&self, request: &SynthesisRequest) -> Result<HttpRequest> {
        request.validate_performance(
            &self.capabilities,
            request.model.as_deref().unwrap_or("alltalk-v2"),
        )?;
        let language = request
            .options
            .get("language")
            .and_then(Value::as_str)
            .unwrap_or("en");
        let body = json!({
            "text_input": request.text,
            "character_voice_gen": request.voice,
            "language": language,
            "output_file_name": format!("{}.wav", request.request_id),
            "output_file_timestamp": false,
            "autoplay": false,
            "text_filtering": "standard"
        });
        self.http
            .json_request(HttpMethod::Post, "api/tts-generate", &body)
    }

    async fn fetch_audio_response(
        &self,
        response: crate::HttpResponse,
    ) -> Result<crate::HttpResponse> {
        if response
            .headers
            .get("content-type")
            .is_some_and(|value| value.starts_with("audio/"))
        {
            return Ok(response);
        }
        let value = json_body(&response)?;
        let path = ["output_file_url", "output_file", "url"]
            .iter()
            .find_map(|key| value.get(*key).and_then(Value::as_str))
            .ok_or_else(|| {
                ProviderError::InvalidResponse("missing AllTalk audio URL".to_owned())
            })?;
        let url = if let Ok(url) = url::Url::parse(path) {
            if url.origin() != self.http.endpoint.base_url.origin() {
                return Err(ProviderError::InvalidResponse(
                    "AllTalk returned a cross-origin audio URL".to_owned(),
                ));
            }
            url
        } else {
            self.http.endpoint.endpoint(path)?
        };
        let mut request = self.http.empty_request(HttpMethod::Get, "")?;
        request.url = url;
        self.http.execute(request).await
    }
}

#[async_trait]
impl TtsProvider for AllTalkProvider {
    fn descriptor(&self) -> &ProviderDescriptor {
        &self.descriptor
    }
    fn capabilities(&self) -> &ProviderCapabilities {
        &self.capabilities
    }

    async fn health(&self) -> Result<ProviderHealth> {
        self.http.basic_health("api/ready").await
    }

    async fn discover_voices(&self) -> Result<Vec<Voice>> {
        let response = self
            .http
            .execute(self.http.empty_request(HttpMethod::Get, "api/voices")?)
            .await?;
        let value = json_body(&response)?;
        let items = value
            .get("voices")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        Ok(items
            .iter()
            .filter_map(|item| {
                let id = item.as_str().or_else(|| item.get("name")?.as_str())?;
                Some(Voice {
                    id: id.to_owned(),
                    name: id.to_owned(),
                    language: None,
                    owned_clone: false,
                    metadata: Default::default(),
                })
            })
            .collect())
    }

    async fn discover_models(&self) -> Result<Vec<Model>> {
        Ok(Vec::new())
    }

    async fn synthesize(&self, request: SynthesisRequest) -> Result<SynthesisResponse> {
        let format = request.format;
        let text_len = request.text.chars().count();
        let http_request = self.build_synthesis_request(&request)?;
        let response = self.http.execute(http_request).await?;
        let response = self.fetch_audio_response(response).await?;
        Ok(SynthesisResponse {
            content_type: response_content_type(&response, format),
            usage: usage_from_headers(&response, text_len),
            audio: response.body,
        })
    }
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};

    use async_trait::async_trait;

    use super::*;
    use crate::{HttpResponse, HttpStreamResponse, Temperature};

    #[derive(Debug, Default)]
    struct RecordingTransport(Mutex<Vec<HttpRequest>>);

    #[derive(Debug, Default)]
    struct RecordingChunkSink(Mutex<Vec<AudioChunk>>);

    #[derive(Debug, Default)]
    struct InterruptedStreamingTransport;

    #[async_trait]
    impl AudioChunkSink for RecordingChunkSink {
        async fn send(&self, chunk: AudioChunk) -> Result<()> {
            self.0.lock().unwrap().push(chunk);
            Ok(())
        }
    }

    #[async_trait]
    impl HttpTransport for RecordingTransport {
        async fn execute(&self, request: HttpRequest) -> Result<HttpResponse> {
            self.0.lock().unwrap().push(request);
            Ok(HttpResponse {
                status: 200,
                headers: Default::default(),
                body: Bytes::from_static(b"audio"),
            })
        }
    }

    #[async_trait]
    impl HttpTransport for InterruptedStreamingTransport {
        async fn execute(&self, _request: HttpRequest) -> Result<HttpResponse> {
            unreachable!("streaming test uses execute_stream")
        }

        async fn execute_stream(&self, _request: HttpRequest) -> Result<HttpStreamResponse> {
            Ok(HttpStreamResponse {
                status: 200,
                headers: Default::default(),
                body: Box::pin(futures::stream::once(async {
                    Err(ProviderError::Transport(
                        "injected body interruption".to_owned(),
                    ))
                })),
            })
        }
    }

    fn endpoint() -> EndpointConfig {
        EndpointConfig::managed_loopback(
            url::Url::parse("http://127.0.0.1:8000/").unwrap(),
            Authentication::None,
        )
        .unwrap()
    }

    #[test]
    fn elevenlabs_model_discovery_keeps_only_tts_models() {
        let models = parse_elevenlabs_models(&json!([
            {
                "model_id": "eleven_multilingual_v2",
                "name": "Eleven Multilingual v2",
                "can_do_text_to_speech": true
            },
            {
                "model_id": "scribe_v2",
                "name": "Scribe v2",
                "can_do_text_to_speech": false
            }
        ]));

        assert_eq!(models.len(), 1);
        assert_eq!(models[0].id, "eleven_multilingual_v2");
    }

    fn synthesis_request(model: Option<&str>, format: AudioFormat) -> SynthesisRequest {
        SynthesisRequest {
            request_id: uuid::Uuid::new_v4(),
            text: "Hello".to_owned(),
            model: model.map(ToOwned::to_owned),
            voice: "af_sky".to_owned(),
            format,
            performance: Default::default(),
            options: Default::default(),
            pronunciation_dictionary_ids: Vec::new(),
        }
    }

    #[test]
    fn managed_endpoint_must_be_loopback() {
        assert!(
            EndpointConfig::managed_loopback(
                url::Url::parse("http://192.168.1.2:8000/").unwrap(),
                Authentication::None
            )
            .is_err()
        );
    }

    #[test]
    fn localai_serializes_openai_speech_request() {
        let provider =
            LocalAiProvider::new(endpoint(), Arc::new(RecordingTransport::default())).unwrap();
        let request = synthesis_request(Some("kokoro"), AudioFormat::Wav);
        let http = provider.build_synthesis_request(&request).unwrap();
        let body: Value = serde_json::from_slice(&http.body).unwrap();
        assert_eq!(body["input"], "Hello");
        assert_eq!(body["response_format"], "wav");
        assert!(body.get("temperature").is_none());
        let _ = Temperature::Default;
    }

    #[test]
    fn openai_speech_uses_the_documented_default_and_typed_controls() {
        let provider =
            OpenAiTtsProvider::new(endpoint(), Arc::new(RecordingTransport::default())).unwrap();
        let mut request = synthesis_request(None, AudioFormat::Mp3);
        request.voice = "alloy".to_owned();
        request.performance.speed = Some(1.25);
        request.performance.delivery_cue = Some(DeliveryCue::Whisper);

        let http = provider.build_synthesis_request(&request).unwrap();
        let body: Value = serde_json::from_slice(&http.body).unwrap();
        assert_eq!(body["model"], "gpt-4o-mini-tts");
        assert_eq!(body["input"], "Hello");
        assert_eq!(body["voice"], "alloy");
        assert_eq!(body["response_format"], "mp3");
        assert_eq!(body["speed"], 1.25);
        assert_eq!(body["instructions"], "whisper");
        assert!(http.url.as_str().ends_with("/v1/audio/speech"));
    }

    #[test]
    fn openai_speech_discovery_excludes_non_tts_models() {
        let models = parse_openai_tts_models(&json!({
            "data": [
                { "id": "gpt-4o-mini-tts" },
                { "id": "gpt-4o-mini-tts-2025-12-15" },
                { "id": "tts-1-hd" },
                { "id": "gpt-5.6-terra" },
                { "id": "gpt-4o-mini-transcribe" },
                { "id": "whisper-1" }
            ]
        }))
        .unwrap();

        assert_eq!(
            models
                .iter()
                .map(|model| model.id.as_str())
                .collect::<Vec<_>>(),
            ["gpt-4o-mini-tts", "gpt-4o-mini-tts-2025-12-15", "tts-1-hd"]
        );
    }

    #[test]
    fn legacy_openai_speech_models_reject_instructions() {
        let provider =
            OpenAiTtsProvider::new(endpoint(), Arc::new(RecordingTransport::default())).unwrap();
        let mut request = synthesis_request(Some("tts-1"), AudioFormat::Mp3);
        request.performance.delivery_cue = Some(DeliveryCue::Curious);

        assert!(provider.build_synthesis_request(&request).is_err());
    }

    #[tokio::test]
    async fn openai_speech_exposes_documented_builtin_voices_without_a_network_request() {
        let transport = Arc::new(RecordingTransport::default());
        let provider =
            OpenAiTtsProvider::new(endpoint(), Arc::clone(&transport) as Arc<dyn HttpTransport>)
                .unwrap();

        let voices = provider.discover_voices().await.unwrap();

        assert!(voices.iter().any(|voice| voice.id == "alloy"));
        assert!(voices.iter().any(|voice| voice.id == "marin"));
        assert!(transport.0.lock().unwrap().is_empty());
    }

    #[test]
    fn elevenlabs_maps_only_typed_allowlisted_voice_settings() {
        let provider =
            ElevenLabsProvider::with_endpoint(endpoint(), Arc::new(RecordingTransport::default()))
                .unwrap();
        let mut request = synthesis_request(Some("eleven_multilingual_v2"), AudioFormat::Mp3);
        request.performance = crate::PerformanceSettings {
            speed: Some(0.9),
            pitch: None,
            stability: Some(0.4),
            similarity: Some(0.8),
            style: Some(0.1),
            speaker_boost: Some(true),
            delivery_cue: None,
        };

        let http = provider.build_synthesis_request(&request).unwrap();
        let body: Value = serde_json::from_slice(&http.body).unwrap();
        assert_eq!(body["model_id"], "eleven_multilingual_v2");
        assert_eq!(
            body["voice_settings"],
            json!({
                "speed": 0.9,
                "stability": 0.4,
                "similarity_boost": 0.8,
                "style": 0.1,
                "use_speaker_boost": true
            })
        );
        assert!(body.get("pitch").is_none());
        assert!(body.get("delivery").is_none());
        assert!(body.get("instruct").is_none());
        assert!(body.get("options").is_none());
        assert!(body.get("pause_before_ms").is_none());
        assert!(body.get("pause_after_ms").is_none());

        request.performance.delivery_cue = Some(crate::DeliveryCue::Shout);
        assert!(provider.build_synthesis_request(&request).is_err());
    }

    #[test]
    fn elevenlabs_rejects_untyped_or_model_unsupported_controls() {
        let provider =
            ElevenLabsProvider::with_endpoint(endpoint(), Arc::new(RecordingTransport::default()))
                .unwrap();

        let mut untyped = synthesis_request(None, AudioFormat::Mp3);
        untyped.options.insert("stability".to_owned(), json!(0.5));
        assert!(provider.build_synthesis_request(&untyped).is_err());

        let mut pitch = synthesis_request(None, AudioFormat::Mp3);
        pitch.performance.pitch = Some(1.1);
        assert!(provider.build_synthesis_request(&pitch).is_err());

        let mut unknown_model = synthesis_request(Some("eleven_v3"), AudioFormat::Mp3);
        unknown_model.performance.speed = Some(1.0);
        assert!(provider.build_synthesis_request(&unknown_model).is_err());
    }

    #[test]
    fn mlx_maps_verified_controls_at_the_request_top_level() {
        let model = "mlx-community/Kokoro-82M-bf16";
        let provider = MlxAudioProvider::new(endpoint(), Arc::new(RecordingTransport::default()))
            .unwrap()
            .with_model_performance(vec![ModelPerformanceCapabilities {
                model: model.to_owned(),
                performance: PerformanceCapabilities {
                    speed: Some(PerformanceRange::new(0.5, 2.0)),
                    pitch: Some(PerformanceRange::new(0.5, 2.0)),
                    delivery_cues: vec![crate::DeliveryCue::Whisper],
                    ..PerformanceCapabilities::default()
                },
            }])
            .unwrap();
        let mut request = synthesis_request(Some(model), AudioFormat::Wav);
        request.performance.speed = Some(1.1);
        request.performance.pitch = Some(0.9);
        request.performance.delivery_cue = Some(crate::DeliveryCue::Whisper);

        let http = provider.build_synthesis_request(&request).unwrap();
        let body: Value = serde_json::from_slice(&http.body).unwrap();
        assert_eq!(body["speed"], 1.1);
        assert_eq!(body["pitch"], 0.9);
        assert_eq!(body["instruct"], "whisper");
        assert!(body.get("options").is_none());
        assert!(body.get("voice_settings").is_none());
        assert!(body.get("performance").is_none());
        assert!(body.get("pause_before_ms").is_none());
        assert!(body.get("pause_after_ms").is_none());

        request.performance.delivery_cue = Some(crate::DeliveryCue::Shout);
        assert!(provider.build_synthesis_request(&request).is_err());
    }

    #[test]
    fn mlx_requires_positive_exact_model_capabilities() {
        let provider =
            MlxAudioProvider::new(endpoint(), Arc::new(RecordingTransport::default())).unwrap();
        let mut request = synthesis_request(Some("unverified-model"), AudioFormat::Wav);
        request.performance.speed = Some(1.1);
        assert!(provider.build_synthesis_request(&request).is_err());

        let invalid_descriptor = ModelPerformanceCapabilities {
            model: "model".to_owned(),
            performance: PerformanceCapabilities {
                stability: Some(PerformanceRange::new(0.0, 1.0)),
                ..PerformanceCapabilities::default()
            },
        };
        assert!(
            MlxAudioProvider::new(endpoint(), Arc::new(RecordingTransport::default()))
                .unwrap()
                .with_model_performance(vec![invalid_descriptor])
                .is_err()
        );
    }

    #[test]
    fn adapters_without_positive_capabilities_reject_performance() {
        let local_ai =
            LocalAiProvider::new(endpoint(), Arc::new(RecordingTransport::default())).unwrap();
        let all_talk =
            AllTalkProvider::new(endpoint(), Arc::new(RecordingTransport::default())).unwrap();
        let mut request = synthesis_request(Some("model"), AudioFormat::Wav);
        request.performance.speed = Some(1.1);
        assert!(local_ai.build_synthesis_request(&request).is_err());
        assert!(all_talk.build_synthesis_request(&request).is_err());
    }

    #[test]
    fn elevenlabs_never_places_key_in_debug_output() {
        let credential = crate::Credential::new("top-secret");
        let provider =
            ElevenLabsProvider::new(credential, Arc::new(RecordingTransport::default())).unwrap();
        assert!(!format!("{provider:?}").contains("top-secret"));
    }

    #[tokio::test]
    async fn streaming_synthesis_preserves_audio_order_and_usage_metadata() {
        let provider =
            LocalAiProvider::new(endpoint(), Arc::new(RecordingTransport::default())).unwrap();
        let request_id = uuid::Uuid::new_v4();
        let sink = Arc::new(RecordingChunkSink::default());
        let metadata = provider
            .synthesize_stream(
                SynthesisRequest {
                    request_id,
                    text: "Hello".to_owned(),
                    model: None,
                    voice: "af_sky".to_owned(),
                    format: AudioFormat::Wav,
                    performance: Default::default(),
                    options: Default::default(),
                    pronunciation_dictionary_ids: Vec::new(),
                },
                CancellationFlag::default(),
                Arc::clone(&sink) as Arc<dyn AudioChunkSink>,
            )
            .await
            .expect("streamed response");

        assert_eq!(metadata.content_type, "audio/wav");
        assert_eq!(metadata.usage.source, UsageSource::Estimated);
        assert_eq!(metadata.usage.characters, Some(5));
        let chunks = sink.0.lock().unwrap();
        assert_eq!(chunks.len(), 2);
        assert_eq!(chunks[0].request_id, request_id);
        assert_eq!(chunks[0].sequence, 0);
        assert_eq!(chunks[0].data, Bytes::from_static(b"audio"));
        assert!(!chunks[0].final_chunk);
        assert_eq!(chunks[1].sequence, 1);
        assert!(chunks[1].data.is_empty());
        assert!(chunks[1].final_chunk);
    }

    #[tokio::test]
    async fn successful_http_stream_interruption_is_an_uncertain_charge() {
        let provider = LocalAiProvider::new(endpoint(), Arc::new(InterruptedStreamingTransport))
            .expect("provider");
        let error = provider
            .synthesize_stream(
                synthesis_request(None, AudioFormat::Wav),
                CancellationFlag::default(),
                Arc::new(RecordingChunkSink::default()),
            )
            .await
            .expect_err("interrupted 2xx stream must fail");
        assert!(matches!(error, ProviderError::UncertainCharge));
    }
}
