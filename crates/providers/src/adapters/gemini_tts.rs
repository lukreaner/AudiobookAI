//! Google Gemini 3.8 text-to-speech through the Interactions API.
//!
//! Gemini 3.8 TTS treats input text as a verbatim transcript. Sustained delivery directions are
//! therefore sent as `speech_metadata` annotations instead of being prepended to the text, which
//! would otherwise be read aloud. Unary requests return a RIFF/WAV file; streaming requests return
//! headerless 24 kHz mono L16 PCM, which this adapter wraps in a streaming WAV header so the
//! service's container-detecting `FFmpeg` pipeline and progressive playback can consume it
//! unchanged.

use std::{collections::BTreeSet, sync::Arc};

use async_trait::async_trait;
use base64::{Engine as _, engine::general_purpose::STANDARD as BASE64};
use bytes::{Bytes, BytesMut};
use futures::StreamExt;
use serde_json::{Value, json};

use super::{HttpAdapter, json_body};
use crate::{
    AudioChunk, AudioChunkSink, AudioFormat, Authentication, CancellationFlag, DeliveryCue,
    EndpointConfig, HttpMethod, HttpRequest, HttpTransport, Model, ModelPerformanceCapabilities,
    ParameterSupport, PerformanceCapabilities, ProviderCapabilities, ProviderDescriptor,
    ProviderError, ProviderHealth, ProviderId, ProviderUsage, Result, StreamingSynthesisResponse,
    SynthesisRequest, SynthesisResponse, TtsProvider, UsageSource, Voice,
};

/// Flagship creative tier (over 130 languages).
pub const GEMINI_FLASH_TTS_MODEL: &str = "gemini-3.8-flash-tts";
/// Cost-efficient tier with the identical request schema (over 100 languages).
pub const GEMINI_FLASH_LITE_TTS_MODEL: &str = "gemini-3.8-flash-lite-tts";

const GEMINI_TTS_SAMPLE_RATE: u32 = 24_000;
const GEMINI_TTS_CHANNELS: u16 = 1;
const GEMINI_TTS_BITS_PER_SAMPLE: u16 = 16;
const DEFAULT_VOICE: &str = "Kore";

/// The 30 prebuilt voices shared by every Gemini 3.8 TTS model.
pub const GEMINI_PREBUILT_VOICES: [&str; 30] = [
    "Zephyr",
    "Puck",
    "Charon",
    "Kore",
    "Fenrir",
    "Leda",
    "Orus",
    "Aoede",
    "Callirrhoe",
    "Autonoe",
    "Enceladus",
    "Iapetus",
    "Umbriel",
    "Algieba",
    "Despina",
    "Erinome",
    "Algenib",
    "Rasalgethi",
    "Laomedeia",
    "Achernar",
    "Alnilam",
    "Schedar",
    "Gacrux",
    "Pulcherrima",
    "Achird",
    "Zubenelgenubi",
    "Vindemiatrix",
    "Sadachbia",
    "Sadaltager",
    "Sulafat",
];

/// Returns true only for the Gemini 3.8 TTS families that use the Interactions schema.
///
/// `gemini-3.1-flash-tts-preview` and the 2.5 TTS previews use an incompatible `generateContent`
/// schema with raw PCM output and are deliberately rejected. Versioned snapshots keep their
/// documented family prefix.
pub fn is_gemini_tts_model_id(id: &str) -> bool {
    let id = id.strip_prefix("models/").unwrap_or(id);
    [GEMINI_FLASH_TTS_MODEL, GEMINI_FLASH_LITE_TTS_MODEL]
        .into_iter()
        .any(|family| {
            id == family
                || id
                    .strip_prefix(family)
                    .and_then(|suffix| suffix.strip_prefix('-'))
                    .is_some_and(|suffix| {
                        !suffix.is_empty()
                            && suffix
                                .bytes()
                                .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-')
                    })
        })
}

/// Delivery cues are supported by both 3.8 TTS models through `speech_metadata.style`.
pub fn gemini_tts_model_performance_capabilities() -> Vec<ModelPerformanceCapabilities> {
    let performance = PerformanceCapabilities {
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
    [GEMINI_FLASH_TTS_MODEL, GEMINI_FLASH_LITE_TTS_MODEL]
        .into_iter()
        .map(|model| ModelPerformanceCapabilities {
            model: model.to_owned(),
            performance: performance.clone(),
        })
        .collect()
}

fn delivery_style(cue: DeliveryCue) -> &'static str {
    match cue {
        DeliveryCue::Whisper => "whispering",
        DeliveryCue::Shout => "shouting",
        DeliveryCue::Sarcastic => "sarcastic",
        DeliveryCue::Curious => "curious and inquisitive",
        DeliveryCue::Excited => "excited",
        DeliveryCue::Crying => "crying, with a tearful voice",
        DeliveryCue::Mischievous => "mischievous",
    }
}

#[derive(Clone, Debug)]
pub struct GeminiTtsProvider {
    descriptor: ProviderDescriptor,
    capabilities: ProviderCapabilities,
    http: HttpAdapter,
}

impl GeminiTtsProvider {
    pub fn new(api_key: crate::Credential, transport: Arc<dyn HttpTransport>) -> Result<Self> {
        let endpoint = EndpointConfig::cloud(
            url::Url::parse("https://generativelanguage.googleapis.com/")
                .map_err(|error| ProviderError::Configuration(error.to_string()))?,
            Authentication::Header {
                name: "x-goog-api-key".to_owned(),
                value: api_key,
            },
        )?;
        Self::with_endpoint(endpoint, transport)
    }

    pub fn with_endpoint(
        endpoint: EndpointConfig,
        transport: Arc<dyn HttpTransport>,
    ) -> Result<Self> {
        let mut capabilities = ProviderCapabilities {
            streaming: true,
            model_discovery: true,
            temperature: ParameterSupport::Unsupported,
            reasoning: BTreeSet::new(),
            ..ProviderCapabilities::default()
        };
        capabilities.set_model_performance(gemini_tts_model_performance_capabilities())?;
        Ok(Self {
            descriptor: ProviderDescriptor {
                id: ProviderId::new("gemini-tts")?,
                display_name: "Google Gemini Speech".to_owned(),
                kind: endpoint.kind,
                endpoint_family: "gemini-interactions-tts-v1".to_owned(),
            },
            capabilities,
            http: HttpAdapter::new(endpoint, transport),
        })
    }

    pub fn build_synthesis_request(&self, request: &SynthesisRequest) -> Result<HttpRequest> {
        self.build_request(request, false)
    }

    fn build_request(&self, request: &SynthesisRequest, stream: bool) -> Result<HttpRequest> {
        let model = request.model.as_deref().unwrap_or(GEMINI_FLASH_TTS_MODEL);
        if !is_gemini_tts_model_id(model) {
            return Err(ProviderError::Configuration(format!(
                "{model} is not a Gemini 3.8 TTS model"
            )));
        }
        request.validate_performance(&self.capabilities, model)?;
        if !request.options.is_empty() {
            return Err(ProviderError::Unsupported {
                feature: "untyped Gemini speech options",
            });
        }
        if !request.pronunciation_dictionary_ids.is_empty() {
            return Err(ProviderError::Unsupported {
                feature: "provider pronunciation dictionaries for Gemini speech",
            });
        }
        if request.text.trim().is_empty() {
            return Err(ProviderError::Configuration(
                "Gemini speech requires non-empty text".to_owned(),
            ));
        }
        let response_format = if stream {
            // The stream is wrapped locally, so its PCM layout is pinned explicitly.
            json!({
                "type": "audio",
                "mime_type": "audio/l16",
                "sample_rate": GEMINI_TTS_SAMPLE_RATE
            })
        } else {
            match request.format {
                AudioFormat::Wav => json!({ "type": "audio", "mime_type": "audio/wav" }),
                AudioFormat::PcmS16Le => json!({
                    "type": "audio",
                    "mime_type": "audio/l16",
                    "sample_rate": GEMINI_TTS_SAMPLE_RATE
                }),
                AudioFormat::PcmF32Le | AudioFormat::Mp3 | AudioFormat::Flac | AudioFormat::Aac => {
                    return Err(ProviderError::Unsupported {
                        feature: "the requested Gemini speech output format",
                    });
                }
            }
        };
        if stream && request.format != AudioFormat::Wav {
            return Err(ProviderError::Unsupported {
                feature: "streaming Gemini speech in a format other than WAV",
            });
        }
        let voice = if request.voice.trim().is_empty() {
            DEFAULT_VOICE
        } else {
            request.voice.trim()
        };
        let mut content = json!({ "type": "text", "text": request.text });
        if let Some(cue) = request.performance.delivery_cue {
            content["annotations"] = json!([{
                "type": "speech_metadata",
                "style": delivery_style(cue)
            }]);
        }
        let mut body = json!({
            "model": model,
            "input": [{ "type": "user_input", "content": [content] }],
            "response_format": response_format,
            "generation_config": { "speech_config": [{ "voice": voice }] }
        });
        if stream {
            body["stream"] = Value::Bool(true);
        }
        let mut http_request =
            self.http
                .json_request(HttpMethod::Post, "v1beta/interactions", &body)?;
        if stream {
            http_request
                .headers
                .insert("accept".to_owned(), "text/event-stream".to_owned());
        }
        Ok(http_request)
    }

    async fn synthesize_unary(&self, request: SynthesisRequest) -> Result<SynthesisResponse> {
        let text_len = request.text.chars().count();
        let format = request.format;
        let response = self
            .http
            .execute(self.build_request(&request, false)?)
            .await?;
        let envelope = json_body(&response)?;
        let (audio, mime_type) = last_audio_block(&envelope)?;
        let content_type = match format {
            AudioFormat::PcmS16Le => "audio/l16;rate=24000;channels=1".to_owned(),
            _ => mime_type.unwrap_or_else(|| "audio/wav".to_owned()),
        };
        Ok(SynthesisResponse {
            audio,
            content_type,
            usage: extract_usage(&envelope, text_len),
        })
    }

    async fn synthesize_streaming(
        &self,
        request: SynthesisRequest,
        cancellation: CancellationFlag,
        sink: Arc<dyn AudioChunkSink>,
    ) -> Result<StreamingSynthesisResponse> {
        let request_id = request.request_id;
        let text_len = request.text.chars().count();
        let http_request = self.build_request(&request, true)?;
        let mut response = self.http.transport.execute_stream(http_request).await?;
        let mut sequence = 0_u64;
        let mut send = |data: Bytes| {
            let chunk = AudioChunk {
                request_id,
                sequence,
                format: AudioFormat::Wav,
                sample_rate: Some(GEMINI_TTS_SAMPLE_RATE),
                channels: Some(GEMINI_TTS_CHANNELS),
                data,
                final_chunk: false,
            };
            sequence = sequence.saturating_add(1);
            chunk
        };
        sink.send(send(streaming_wav_header())).await?;
        let mut parser = SseParser::default();
        let mut usage = None;
        let mut audio_bytes = 0_usize;
        while let Some(chunk) = response.body.next().await {
            if cancellation.is_cancelled() {
                return Err(ProviderError::Cancelled);
            }
            // The provider accepted the request, so a later body failure has unknown billing.
            let data = chunk.map_err(|_| ProviderError::UncertainCharge)?;
            for event in parser.push(&data) {
                match interpret_stream_event(&event)? {
                    StreamEvent::Audio(pcm) => {
                        audio_bytes = audio_bytes.saturating_add(pcm.len());
                        sink.send(send(pcm)).await?;
                    }
                    StreamEvent::Usage(value) => usage = Some(value),
                    StreamEvent::Ignored => {}
                }
            }
        }
        if let Some(event) = parser.finish() {
            match interpret_stream_event(&event)? {
                StreamEvent::Audio(pcm) => {
                    audio_bytes = audio_bytes.saturating_add(pcm.len());
                    sink.send(send(pcm)).await?;
                }
                StreamEvent::Usage(value) => usage = Some(value),
                StreamEvent::Ignored => {}
            }
        }
        if audio_bytes == 0 {
            return Err(ProviderError::InvalidResponse(
                "Gemini speech stream contained no audio".to_owned(),
            ));
        }
        let mut final_chunk = send(Bytes::new());
        final_chunk.final_chunk = true;
        sink.send(final_chunk).await?;
        Ok(StreamingSynthesisResponse {
            content_type: "audio/wav".to_owned(),
            usage: usage.map_or_else(
                || estimated_usage(text_len),
                |value| extract_usage(&value, text_len),
            ),
        })
    }

    async fn voices(&self) -> Result<Vec<Voice>> {
        let mut voices = GEMINI_PREBUILT_VOICES
            .iter()
            .map(|name| Voice {
                id: (*name).to_owned(),
                name: (*name).to_owned(),
                language: None,
                owned_clone: false,
                metadata: [("type".to_owned(), "prebuilt".to_owned())].into(),
            })
            .collect::<Vec<_>>();
        // The Extended Voice Library is optional. Prebuilt voices remain usable when it is not
        // enabled for the key or its listing format changes.
        let Ok(mut request) = self
            .http
            .empty_request(HttpMethod::Get, "v1beta/voices?page_size=1000")
        else {
            return Ok(voices);
        };
        request.timeout = std::time::Duration::from_secs(10);
        let Ok(response) = self.http.execute(request).await else {
            return Ok(voices);
        };
        let Ok(value) = json_body(&response) else {
            return Ok(voices);
        };
        let known = voices
            .iter()
            .map(|voice| voice.id.to_ascii_lowercase())
            .collect::<BTreeSet<_>>();
        voices.extend(
            parse_voice_library(&value)
                .into_iter()
                .filter(|voice| !known.contains(&voice.id.to_ascii_lowercase())),
        );
        Ok(voices)
    }

    async fn models(&self) -> Result<Vec<Model>> {
        let response = self
            .http
            .execute(
                self.http
                    .empty_request(HttpMethod::Get, "v1beta/models?pageSize=1000")?,
            )
            .await?;
        let value = json_body(&response)?;
        let mut models = value
            .get("models")
            .and_then(Value::as_array)
            .ok_or_else(|| ProviderError::InvalidResponse("missing model list".to_owned()))?
            .iter()
            .filter_map(|item| {
                let id = item
                    .get("name")
                    .and_then(Value::as_str)?
                    .trim_start_matches("models/");
                is_gemini_tts_model_id(id).then(|| Model {
                    id: id.to_owned(),
                    name: item
                        .get("displayName")
                        .and_then(Value::as_str)
                        .unwrap_or(id)
                        .to_owned(),
                    metadata: Default::default(),
                })
            })
            .collect::<Vec<_>>();
        // The catalog does not always list TTS models; the documented families stay selectable.
        for id in [GEMINI_FLASH_TTS_MODEL, GEMINI_FLASH_LITE_TTS_MODEL] {
            if !models.iter().any(|model| model.id == id) {
                models.push(Model {
                    id: id.to_owned(),
                    name: id.to_owned(),
                    metadata: Default::default(),
                });
            }
        }
        Ok(models)
    }
}

#[async_trait]
impl TtsProvider for GeminiTtsProvider {
    fn descriptor(&self) -> &ProviderDescriptor {
        &self.descriptor
    }

    fn capabilities(&self) -> &ProviderCapabilities {
        &self.capabilities
    }

    async fn health(&self) -> Result<ProviderHealth> {
        self.http.basic_health("v1beta/models?pageSize=1").await
    }

    async fn discover_voices(&self) -> Result<Vec<Voice>> {
        self.voices().await
    }

    async fn discover_models(&self) -> Result<Vec<Model>> {
        self.models().await
    }

    async fn synthesize(&self, request: SynthesisRequest) -> Result<SynthesisResponse> {
        self.synthesize_unary(request).await
    }

    async fn synthesize_stream(
        &self,
        request: SynthesisRequest,
        cancellation: CancellationFlag,
        sink: Arc<dyn AudioChunkSink>,
    ) -> Result<StreamingSynthesisResponse> {
        self.synthesize_streaming(request, cancellation, sink).await
    }
}

/// A RIFF header with unknown (maximum) sizes, as used for live WAV streams.
fn streaming_wav_header() -> Bytes {
    let block_align = GEMINI_TTS_CHANNELS * (GEMINI_TTS_BITS_PER_SAMPLE / 8);
    let byte_rate = GEMINI_TTS_SAMPLE_RATE * u32::from(block_align);
    let mut header = BytesMut::with_capacity(44);
    header.extend_from_slice(b"RIFF");
    header.extend_from_slice(&u32::MAX.to_le_bytes());
    header.extend_from_slice(b"WAVEfmt ");
    header.extend_from_slice(&16_u32.to_le_bytes());
    header.extend_from_slice(&1_u16.to_le_bytes());
    header.extend_from_slice(&GEMINI_TTS_CHANNELS.to_le_bytes());
    header.extend_from_slice(&GEMINI_TTS_SAMPLE_RATE.to_le_bytes());
    header.extend_from_slice(&byte_rate.to_le_bytes());
    header.extend_from_slice(&block_align.to_le_bytes());
    header.extend_from_slice(&GEMINI_TTS_BITS_PER_SAMPLE.to_le_bytes());
    header.extend_from_slice(b"data");
    header.extend_from_slice(&u32::MAX.to_le_bytes());
    header.freeze()
}

/// Returns the last audio block of a unary interaction, matching the SDK's `output_audio`.
fn last_audio_block(envelope: &Value) -> Result<(Bytes, Option<String>)> {
    let blocks = envelope
        .get("steps")
        .or_else(|| envelope.get("outputs"))
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .flat_map(|step| {
            step.get("content")
                .and_then(Value::as_array)
                .map_or_else(|| std::slice::from_ref(step), Vec::as_slice)
        })
        .filter(|block| block.get("type").and_then(Value::as_str) == Some("audio"))
        .collect::<Vec<_>>();
    let block = blocks.last().ok_or_else(|| {
        ProviderError::InvalidResponse("Gemini speech response contained no audio".to_owned())
    })?;
    let data = block.get("data").and_then(Value::as_str).ok_or_else(|| {
        ProviderError::InvalidResponse("Gemini audio block has no data".to_owned())
    })?;
    let audio = BASE64.decode(data.as_bytes()).map_err(|_| {
        ProviderError::InvalidResponse("Gemini audio block is not valid base64".to_owned())
    })?;
    if audio.is_empty() {
        return Err(ProviderError::InvalidResponse(
            "Gemini speech response contained empty audio".to_owned(),
        ));
    }
    let mime_type = block
        .get("mime_type")
        .or_else(|| block.get("mimeType"))
        .and_then(Value::as_str)
        .map(ToOwned::to_owned);
    Ok((Bytes::from(audio), mime_type))
}

fn estimated_usage(text_len: usize) -> ProviderUsage {
    ProviderUsage {
        source: UsageSource::Estimated,
        characters: u64::try_from(text_len).ok(),
        ..ProviderUsage::default()
    }
}

fn extract_usage(envelope: &Value, text_len: usize) -> ProviderUsage {
    let usage = envelope
        .get("usage")
        .or_else(|| envelope.get("usageMetadata"));
    let number = |keys: &[&str]| {
        usage.and_then(|usage| {
            keys.iter()
                .find_map(|key| usage.get(*key).and_then(Value::as_u64))
        })
    };
    let input_tokens = number(&["total_input_tokens", "input_tokens", "promptTokenCount"]);
    let output_tokens = number(&[
        "total_output_tokens",
        "output_tokens",
        "candidatesTokenCount",
    ]);
    ProviderUsage {
        source: if input_tokens.is_some() || output_tokens.is_some() {
            UsageSource::Reported
        } else {
            UsageSource::Estimated
        },
        characters: u64::try_from(text_len).ok(),
        input_tokens,
        output_tokens,
        request_id: envelope
            .get("id")
            .and_then(Value::as_str)
            .map(ToOwned::to_owned),
        ..ProviderUsage::default()
    }
}

fn parse_voice_library(value: &Value) -> Vec<Voice> {
    value
        .get("voices")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|item| {
            let id = item
                .get("voice")
                .or_else(|| item.get("voice_id"))
                .or_else(|| item.get("id"))
                .or_else(|| item.get("name"))
                .and_then(Value::as_str)?
                .trim_start_matches("voices/")
                .to_owned();
            if id.is_empty() {
                return None;
            }
            let name = item
                .get("display_name")
                .or_else(|| item.get("displayName"))
                .and_then(Value::as_str)
                .unwrap_or(&id)
                .to_owned();
            let language = item
                .get("language_codes")
                .or_else(|| item.get("languageCodes"))
                .and_then(Value::as_array)
                .and_then(|codes| codes.first())
                .or_else(|| item.get("language_code"))
                .and_then(Value::as_str)
                .map(ToOwned::to_owned);
            let mut metadata = std::collections::BTreeMap::new();
            for key in [
                "type",
                "gender",
                "pitch",
                "accent",
                "persona",
                "description",
            ] {
                if let Some(value) = item.get(key).and_then(Value::as_str) {
                    metadata.insert(key.to_owned(), value.to_owned());
                }
            }
            Some(Voice {
                id,
                name,
                language,
                owned_clone: false,
                metadata,
            })
        })
        .collect()
}

enum StreamEvent {
    Audio(Bytes),
    Usage(Value),
    Ignored,
}

fn interpret_stream_event(data: &str) -> Result<StreamEvent> {
    let data = data.trim();
    if data.is_empty() || data == "[DONE]" {
        return Ok(StreamEvent::Ignored);
    }
    let value: Value = serde_json::from_str(data).map_err(|_| ProviderError::UncertainCharge)?;
    if let Some(error) = value.get("error") {
        let message = error
            .get("message")
            .and_then(Value::as_str)
            .unwrap_or("Gemini speech stream reported an error");
        return Err(ProviderError::InvalidResponse(message.to_owned()));
    }
    if let Some(delta) = value.get("delta")
        && delta.get("type").and_then(Value::as_str) == Some("audio")
    {
        let data = delta
            .get("data")
            .and_then(Value::as_str)
            .unwrap_or_default();
        let pcm = BASE64
            .decode(data.as_bytes())
            .map_err(|_| ProviderError::UncertainCharge)?;
        return Ok(if pcm.is_empty() {
            StreamEvent::Ignored
        } else {
            StreamEvent::Audio(Bytes::from(pcm))
        });
    }
    if let Some(usage) = value
        .get("interaction")
        .filter(|interaction| interaction.get("usage").is_some())
        .or_else(|| value.get("usage").map(|_| &value))
    {
        return Ok(StreamEvent::Usage(usage.clone()));
    }
    Ok(StreamEvent::Ignored)
}

/// Minimal server-sent-events decoder returning the joined `data:` payload of each event.
#[derive(Default)]
struct SseParser {
    buffer: String,
    pending: Vec<u8>,
}

impl SseParser {
    fn push(&mut self, bytes: &[u8]) -> Vec<String> {
        self.pending.extend_from_slice(bytes);
        let valid = match std::str::from_utf8(&self.pending) {
            Ok(_) => self.pending.len(),
            Err(error) => error.valid_up_to(),
        };
        let text = String::from_utf8_lossy(&self.pending[..valid]).into_owned();
        self.pending.drain(..valid);
        self.buffer.push_str(&text.replace("\r\n", "\n"));
        let mut events = Vec::new();
        while let Some(end) = self.buffer.find("\n\n") {
            let block = self.buffer[..end].to_owned();
            self.buffer.drain(..end + 2);
            if let Some(event) = event_data(&block) {
                events.push(event);
            }
        }
        events
    }

    fn finish(&mut self) -> Option<String> {
        let block = std::mem::take(&mut self.buffer);
        event_data(&block)
    }
}

fn event_data(block: &str) -> Option<String> {
    let lines = block
        .lines()
        .filter_map(|line| line.strip_prefix("data:"))
        .map(|line| line.strip_prefix(' ').unwrap_or(line))
        .collect::<Vec<_>>();
    (!lines.is_empty()).then(|| lines.join("\n"))
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use super::*;
    use crate::{HttpResponse, HttpStreamResponse};

    #[derive(Debug)]
    struct RecordingTransport {
        response: Value,
        stream_body: Vec<&'static str>,
        requests: Mutex<Vec<HttpRequest>>,
    }

    #[async_trait]
    impl HttpTransport for RecordingTransport {
        async fn execute(&self, request: HttpRequest) -> Result<HttpResponse> {
            self.requests.lock().unwrap().push(request);
            Ok(HttpResponse {
                status: 200,
                headers: Default::default(),
                body: serde_json::to_vec(&self.response).unwrap().into(),
            })
        }

        async fn execute_stream(&self, request: HttpRequest) -> Result<HttpStreamResponse> {
            self.requests.lock().unwrap().push(request);
            let chunks = self
                .stream_body
                .iter()
                .map(|chunk| Ok(Bytes::from_static(chunk.as_bytes())))
                .collect::<Vec<_>>();
            Ok(HttpStreamResponse {
                status: 200,
                headers: Default::default(),
                body: Box::pin(futures::stream::iter(chunks)),
            })
        }
    }

    #[derive(Debug, Default)]
    struct CollectingSink(Mutex<Vec<AudioChunk>>);

    #[async_trait]
    impl AudioChunkSink for CollectingSink {
        async fn send(&self, chunk: AudioChunk) -> Result<()> {
            self.0.lock().unwrap().push(chunk);
            Ok(())
        }
    }

    fn provider(transport: Arc<RecordingTransport>) -> GeminiTtsProvider {
        GeminiTtsProvider::new(crate::Credential::new("test-only-key"), transport).unwrap()
    }

    fn transport(response: Value, stream_body: Vec<&'static str>) -> Arc<RecordingTransport> {
        Arc::new(RecordingTransport {
            response,
            stream_body,
            requests: Mutex::new(Vec::new()),
        })
    }

    fn synthesis(model: &str) -> SynthesisRequest {
        SynthesisRequest {
            request_id: uuid::Uuid::new_v4(),
            text: "Guten Morgen, Käthe!".to_owned(),
            model: Some(model.to_owned()),
            voice: "Puck".to_owned(),
            format: AudioFormat::Wav,
            performance: Default::default(),
            options: Default::default(),
            pronunciation_dictionary_ids: Vec::new(),
        }
    }

    #[test]
    fn recognizes_only_gemini_3_8_tts_families() {
        for id in [
            "gemini-3.8-flash-tts",
            "gemini-3.8-flash-lite-tts",
            "models/gemini-3.8-flash-lite-tts",
            "gemini-3.8-flash-tts-001",
        ] {
            assert!(is_gemini_tts_model_id(id), "{id}");
        }
        for id in [
            "gemini-3.1-flash-tts-preview",
            "gemini-2.5-flash-preview-tts",
            "gemini-3.8-flash",
            "gemini-3.8-flash-tts-",
            "gemini-3.8-flash-ttsx",
        ] {
            assert!(!is_gemini_tts_model_id(id), "{id}");
        }
    }

    #[test]
    fn delivery_cues_become_speech_metadata_instead_of_spoken_text() {
        let provider = provider(transport(json!({}), Vec::new()));
        let mut request = synthesis(GEMINI_FLASH_LITE_TTS_MODEL);
        request.performance.delivery_cue = Some(DeliveryCue::Whisper);

        let http = provider.build_synthesis_request(&request).unwrap();
        let body: Value = serde_json::from_slice(&http.body).unwrap();

        assert_eq!(
            http.url.as_str(),
            "https://generativelanguage.googleapis.com/v1beta/interactions"
        );
        assert_eq!(http.headers["x-goog-api-key"], "test-only-key");
        assert_eq!(body["model"], GEMINI_FLASH_LITE_TTS_MODEL);
        assert_eq!(body["input"][0]["type"], "user_input");
        assert_eq!(
            body["input"][0]["content"][0]["text"],
            "Guten Morgen, Käthe!"
        );
        assert_eq!(
            body["input"][0]["content"][0]["annotations"][0],
            json!({"type": "speech_metadata", "style": "whispering"})
        );
        assert_eq!(
            body["generation_config"]["speech_config"][0]["voice"],
            "Puck"
        );
        assert_eq!(body["response_format"]["mime_type"], "audio/wav");
        assert!(body.get("stream").is_none());
    }

    #[test]
    fn unsupported_models_formats_and_controls_are_rejected_before_dispatch() {
        let provider = provider(transport(json!({}), Vec::new()));
        assert!(
            provider
                .build_synthesis_request(&synthesis("gemini-3.1-flash-tts-preview"))
                .is_err()
        );
        let mut mp3 = synthesis(GEMINI_FLASH_TTS_MODEL);
        mp3.format = AudioFormat::Mp3;
        assert!(matches!(
            provider.build_synthesis_request(&mp3),
            Err(ProviderError::Unsupported { .. })
        ));
        let mut speed = synthesis(GEMINI_FLASH_TTS_MODEL);
        speed.performance.speed = Some(1.2);
        assert!(provider.build_synthesis_request(&speed).is_err());
    }

    #[tokio::test]
    async fn unary_synthesis_returns_the_last_audio_block_and_reported_usage() {
        let wav = BASE64.encode(b"RIFF....WAVE");
        let transport = transport(
            json!({
                "id": "interaction-1",
                "steps": [
                    {"type": "thought", "content": [{"type": "text", "text": "ignored"}]},
                    {"type": "model_output", "content": [
                        {"type": "audio", "data": BASE64.encode(b"stale"), "mime_type": "audio/wav"},
                        {"type": "audio", "data": wav, "mime_type": "audio/wav"}
                    ]}
                ],
                "usage": {"total_input_tokens": 12, "total_output_tokens": 480}
            }),
            Vec::new(),
        );
        let response = provider(Arc::clone(&transport))
            .synthesize(synthesis(GEMINI_FLASH_TTS_MODEL))
            .await
            .unwrap();

        assert_eq!(&response.audio[..], b"RIFF....WAVE");
        assert_eq!(response.content_type, "audio/wav");
        assert_eq!(response.usage.source, UsageSource::Reported);
        assert_eq!(response.usage.output_tokens, Some(480));
        assert_eq!(response.usage.request_id.as_deref(), Some("interaction-1"));
    }

    #[tokio::test]
    async fn streaming_wraps_split_sse_pcm_deltas_in_a_wav_stream() {
        let transport = transport(
            json!({}),
            vec![
                "event: step.delta\ndata: {\"event_type\":\"step.delta\",\"delta\":{\"type\":\"audio\",\"data\":\"AQID\"}}\n",
                "\n",
                "data: {\"event_type\":\"step.delta\",\"delta\":{\"type\":\"au",
                "dio\",\"data\":\"BAUG\"}}\r\n\r\n",
                "data: {\"event_type\":\"interaction.complete\",\"interaction\":{\"usage\":{\"total_output_tokens\":7}}}",
            ],
        );
        let sink = Arc::new(CollectingSink::default());
        let response = provider(Arc::clone(&transport))
            .synthesize_stream(
                synthesis(GEMINI_FLASH_TTS_MODEL),
                CancellationFlag::default(),
                Arc::clone(&sink) as Arc<dyn AudioChunkSink>,
            )
            .await
            .unwrap();

        let chunks = sink.0.lock().unwrap();
        assert_eq!(chunks.len(), 4);
        assert_eq!(&chunks[0].data[..4], b"RIFF");
        assert_eq!(chunks[0].data.len(), 44);
        assert_eq!(&chunks[1].data[..], &[1, 2, 3]);
        assert_eq!(&chunks[2].data[..], &[4, 5, 6]);
        assert!(chunks[3].final_chunk && chunks[3].data.is_empty());
        assert!(chunks.iter().enumerate().all(
            |(index, chunk)| chunk.sequence == index as u64 && chunk.format == AudioFormat::Wav
        ));
        assert_eq!(response.content_type, "audio/wav");
        assert_eq!(response.usage.output_tokens, Some(7));

        let request = &transport.requests.lock().unwrap()[0];
        let body: Value = serde_json::from_slice(&request.body).unwrap();
        assert_eq!(body["stream"], true);
        assert_eq!(body["response_format"]["mime_type"], "audio/l16");
    }

    #[tokio::test]
    async fn stream_errors_and_empty_streams_fail() {
        let sink = Arc::new(CollectingSink::default());
        let error = provider(transport(
            json!({}),
            vec!["data: {\"error\":{\"message\":\"quota exceeded\"}}\n\n"],
        ))
        .synthesize_stream(
            synthesis(GEMINI_FLASH_TTS_MODEL),
            CancellationFlag::default(),
            sink,
        )
        .await
        .unwrap_err();
        assert!(error.to_string().contains("quota exceeded"));

        let error = provider(transport(json!({}), vec!["data: [DONE]\n\n"]))
            .synthesize_stream(
                synthesis(GEMINI_FLASH_TTS_MODEL),
                CancellationFlag::default(),
                Arc::new(CollectingSink::default()),
            )
            .await
            .unwrap_err();
        assert!(matches!(error, ProviderError::InvalidResponse(_)));
    }

    #[test]
    fn voice_library_entries_are_parsed_tolerantly() {
        let voices = parse_voice_library(&json!({"voices": [
            {"name": "voices/narrator-warm", "display_name": "Warm Narrator",
             "language_codes": ["de-DE"], "gender": "female", "type": "prompted"},
            {"display_name": "missing id"}
        ]}));
        assert_eq!(voices.len(), 1);
        assert_eq!(voices[0].id, "narrator-warm");
        assert_eq!(voices[0].language.as_deref(), Some("de-DE"));
        assert_eq!(voices[0].metadata["gender"], "female");
    }

    #[test]
    fn streaming_header_describes_24khz_mono_pcm() {
        let header = streaming_wav_header();
        assert_eq!(&header[..4], b"RIFF");
        assert_eq!(&header[8..16], b"WAVEfmt ");
        assert_eq!(
            u32::from_le_bytes(header[24..28].try_into().unwrap()),
            24_000
        );
        assert_eq!(u16::from_le_bytes(header[22..24].try_into().unwrap()), 1);
        assert_eq!(&header[36..40], b"data");
    }
}
