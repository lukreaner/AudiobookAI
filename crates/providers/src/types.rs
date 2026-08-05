use std::{
    collections::{BTreeMap, BTreeSet},
    fmt,
    path::PathBuf,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
};

use chrono::{DateTime, Utc};
use secrecy::{ExposeSecret, SecretString};
use serde::{Deserialize, Serialize};
use url::Url;
use uuid::Uuid;

use crate::{ProviderError, Result};

#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize, Deserialize)]
#[serde(transparent)]
pub struct ProviderId(pub String);

impl ProviderId {
    pub fn new(value: impl Into<String>) -> Result<Self> {
        let value = value.into();
        if value.is_empty()
            || !value.chars().all(|character| {
                character.is_ascii_alphanumeric() || matches!(character, '-' | '_')
            })
        {
            return Err(ProviderError::Configuration(
                "provider id may contain only ASCII letters, digits, '-' and '_'".to_owned(),
            ));
        }
        Ok(Self(value))
    }
}

impl fmt::Display for ProviderId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProviderKind {
    CloudRemote,
    ExternalEndpoint,
    ManagedChild,
    Native,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct ProviderDescriptor {
    pub id: ProviderId,
    pub display_name: String,
    pub kind: ProviderKind,
    pub endpoint_family: String,
}

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReasoningMode {
    Disabled,
    Effort,
    Adaptive,
    TokenBudget,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ParameterSupport {
    Unsupported,
    OmitOnly,
    Value,
    NullableValue,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct ProviderCapabilities {
    pub streaming: bool,
    pub cancellation: bool,
    pub voice_cloning: bool,
    pub pronunciation: bool,
    pub model_discovery: bool,
    pub process_control: bool,
    pub model_control: bool,
    pub max_concurrency: u16,
    pub temperature: ParameterSupport,
    pub reasoning: BTreeSet<ReasoningMode>,
    pub source: CapabilitySource,
}

impl Default for ProviderCapabilities {
    fn default() -> Self {
        Self {
            streaming: false,
            cancellation: false,
            voice_cloning: false,
            pronunciation: false,
            model_discovery: false,
            process_control: false,
            model_control: false,
            max_concurrency: 1,
            temperature: ParameterSupport::Unsupported,
            reasoning: BTreeSet::new(),
            source: CapabilitySource::BuiltIn {
                adapter_version: env!("CARGO_PKG_VERSION").to_owned(),
            },
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum CapabilitySource {
    BuiltIn {
        adapter_version: String,
    },
    Discovered {
        endpoint: Url,
        provider_version: Option<String>,
        model: Option<String>,
        discovered_at: DateTime<Utc>,
    },
    UserOverride {
        updated_at: DateTime<Utc>,
    },
}

#[derive(Clone, Debug)]
pub struct Credential(SecretString);

impl Credential {
    pub fn new(value: impl Into<Box<str>>) -> Self {
        Self(SecretString::from(value.into()))
    }

    pub fn expose(&self) -> &str {
        self.0.expose_secret()
    }
}

#[derive(Clone, Debug)]
pub enum Authentication {
    None,
    Bearer(Credential),
    Header { name: String, value: Credential },
}

impl Authentication {
    pub fn apply(&self, headers: &mut BTreeMap<String, String>) {
        match self {
            Self::None => {}
            Self::Bearer(value) => {
                headers.insert(
                    "authorization".to_owned(),
                    format!("Bearer {}", value.expose()),
                );
            }
            Self::Header { name, value } => {
                headers.insert(name.clone(), value.expose().to_owned());
            }
        }
    }
}

#[derive(Clone)]
pub struct EndpointConfig {
    pub base_url: Url,
    pub authentication: Authentication,
    pub kind: ProviderKind,
}

impl fmt::Debug for EndpointConfig {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let authentication = match &self.authentication {
            Authentication::None => "none",
            Authentication::Bearer(_) => "bearer",
            Authentication::Header { .. } => "header",
        };
        formatter
            .debug_struct("EndpointConfig")
            .field("url_scheme", &self.base_url.scheme())
            .field("url_has_host", &self.base_url.host_str().is_some())
            .field("authentication", &authentication)
            .field("kind", &self.kind)
            .finish()
    }
}

impl EndpointConfig {
    pub fn cloud(base_url: Url, authentication: Authentication) -> Result<Self> {
        validate_http_url(&base_url)?;
        if base_url.scheme() != "https" {
            return Err(ProviderError::Configuration(
                "cloud provider endpoints must use HTTPS".to_owned(),
            ));
        }
        Ok(Self {
            base_url,
            authentication,
            kind: ProviderKind::CloudRemote,
        })
    }

    pub fn external(base_url: Url, authentication: Authentication) -> Result<Self> {
        validate_http_url(&base_url)?;
        if !matches!(authentication, Authentication::None)
            && base_url.scheme() != "https"
            && !has_exact_loopback_host(&base_url)
        {
            return Err(ProviderError::Configuration(
                "authenticated external provider endpoints must use HTTPS unless they are loopback-only"
                    .to_owned(),
            ));
        }
        Ok(Self {
            base_url,
            authentication,
            kind: ProviderKind::ExternalEndpoint,
        })
    }

    pub fn managed_loopback(base_url: Url, authentication: Authentication) -> Result<Self> {
        validate_http_url(&base_url)?;
        if !has_exact_loopback_host(&base_url) {
            return Err(ProviderError::Configuration(
                "managed providers must bind to a loopback endpoint".to_owned(),
            ));
        }
        Ok(Self {
            base_url,
            authentication,
            kind: ProviderKind::ManagedChild,
        })
    }

    pub fn endpoint(&self, path: &str) -> Result<Url> {
        self.base_url
            .join(path.trim_start_matches('/'))
            .map_err(|error| ProviderError::Configuration(error.to_string()))
    }
}

fn has_exact_loopback_host(url: &Url) -> bool {
    match url.host() {
        Some(url::Host::Domain("localhost")) => true,
        Some(url::Host::Ipv4(address)) => address == std::net::Ipv4Addr::LOCALHOST,
        Some(url::Host::Ipv6(address)) => address == std::net::Ipv6Addr::LOCALHOST,
        _ => false,
    }
}

fn validate_http_url(url: &Url) -> Result<()> {
    if !matches!(url.scheme(), "http" | "https") || url.host_str().is_none() {
        return Err(ProviderError::Configuration(
            "provider endpoint must be an absolute HTTP(S) URL".to_owned(),
        ));
    }
    if !url.username().is_empty() || url.password().is_some() {
        return Err(ProviderError::Configuration(
            "provider credentials must not be embedded in URLs".to_owned(),
        ));
    }
    if url.query().is_some() || url.fragment().is_some() {
        return Err(ProviderError::Configuration(
            "provider base endpoints must not contain query parameters or fragments".to_owned(),
        ));
    }
    Ok(())
}

/// A three-state temperature value. Adapters must preserve `Default` as omission.
#[derive(Clone, Copy, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(tag = "mode", content = "value", rename_all = "snake_case")]
pub enum Temperature {
    #[default]
    Default,
    Null,
    Value(f32),
}

impl Temperature {
    pub fn validate(self, support: ParameterSupport) -> Result<()> {
        match (self, support) {
            (Self::Default, _) | (Self::Null, ParameterSupport::NullableValue) => Ok(()),
            (Self::Value(value), ParameterSupport::Value | ParameterSupport::NullableValue)
                if value.is_finite() && (0.0..=2.0).contains(&value) =>
            {
                Ok(())
            }
            (Self::Value(value), _) if !value.is_finite() || !(0.0..=2.0).contains(&value) => Err(
                ProviderError::Configuration("temperature must be between 0 and 2".to_owned()),
            ),
            _ => Err(ProviderError::Unsupported {
                feature: "the selected temperature mode",
            }),
        }
    }
}

#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
#[serde(tag = "mode", rename_all = "snake_case")]
pub enum ReasoningControl {
    #[default]
    Inherit,
    Disabled,
    Effort {
        effort: ReasoningEffort,
    },
    Adaptive,
    TokenBudget {
        tokens: u32,
    },
}

impl ReasoningControl {
    pub fn validate(&self, capabilities: &ProviderCapabilities) -> Result<()> {
        let mode = match self {
            Self::Inherit => return Ok(()),
            Self::Disabled => ReasoningMode::Disabled,
            Self::Effort { .. } => ReasoningMode::Effort,
            Self::Adaptive => ReasoningMode::Adaptive,
            Self::TokenBudget { tokens } if *tokens < 1_024 => {
                return Err(ProviderError::Configuration(
                    "reasoning token budget must be at least 1024".to_owned(),
                ));
            }
            Self::TokenBudget { .. } => ReasoningMode::TokenBudget,
        };
        if capabilities.reasoning.contains(&mode) {
            Ok(())
        } else {
            Err(ProviderError::Unsupported {
                feature: "the selected reasoning mode",
            })
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReasoningEffort {
    Minimal,
    Low,
    Medium,
    High,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AudioFormat {
    PcmS16Le,
    PcmF32Le,
    Mp3,
    Wav,
    Flac,
    Aac,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SynthesisRequest {
    pub request_id: Uuid,
    pub text: String,
    pub model: Option<String>,
    pub voice: String,
    pub format: AudioFormat,
    #[serde(default)]
    pub options: BTreeMap<String, serde_json::Value>,
    #[serde(default)]
    pub pronunciation_dictionary_ids: Vec<String>,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct ProviderUsage {
    pub source: UsageSource,
    pub characters: Option<u64>,
    pub audio_milliseconds: Option<u64>,
    pub input_tokens: Option<u64>,
    pub output_tokens: Option<u64>,
    pub cached_tokens: Option<u64>,
    pub reasoning_tokens: Option<u64>,
    pub credits_micros: Option<i64>,
    pub raw_redacted: Option<serde_json::Value>,
    pub request_id: Option<String>,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum UsageSource {
    Reported,
    Estimated,
    #[default]
    Unknown,
}

#[derive(Clone, Debug)]
pub struct SynthesisResponse {
    pub audio: bytes::Bytes,
    pub content_type: String,
    pub usage: ProviderUsage,
}

/// Response metadata returned after a chunked synthesis stream is complete.
///
/// Audio bytes are delivered exclusively through [`crate::AudioChunkSink`], which lets the
/// caller persist and play them as they arrive without duplicating the full payload here.
#[derive(Clone, Debug)]
pub struct StreamingSynthesisResponse {
    pub content_type: String,
    pub usage: ProviderUsage,
}

#[derive(Clone, Debug)]
pub struct AudioChunk {
    pub request_id: Uuid,
    pub sequence: u64,
    pub format: AudioFormat,
    pub sample_rate: Option<u32>,
    pub channels: Option<u16>,
    pub data: bytes::Bytes,
    pub final_chunk: bool,
}

#[derive(Clone, Debug)]
pub struct CancellationFlag(Arc<AtomicBool>);

impl Default for CancellationFlag {
    fn default() -> Self {
        Self(Arc::new(AtomicBool::new(false)))
    }
}

impl CancellationFlag {
    pub fn cancel(&self) {
        self.0.store(true, Ordering::Release);
    }

    pub fn is_cancelled(&self) -> bool {
        self.0.load(Ordering::Acquire)
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct Voice {
    pub id: String,
    pub name: String,
    pub language: Option<String>,
    pub owned_clone: bool,
    #[serde(default)]
    pub metadata: BTreeMap<String, String>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct Model {
    pub id: String,
    pub name: String,
    #[serde(default)]
    pub metadata: BTreeMap<String, String>,
}

/// Sanitized model-library metadata exposed by a provider control adapter.
///
/// Provider response bodies and free-form status text deliberately do not cross this boundary.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct ProviderModelInfo {
    pub id: String,
    pub name: String,
    pub size_bytes: Option<u64>,
    pub format: Option<String>,
    pub family: Option<String>,
    pub parameter_size: Option<String>,
    pub quantization: Option<String>,
    #[serde(default)]
    pub loaded_instances: Vec<String>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct ModelDownloadRequest {
    pub model: String,
    pub quantization: Option<String>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ModelDownloadState {
    Downloading,
    Paused,
    Completed,
    Failed,
    AlreadyDownloaded,
    Cancelled,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ModelDownloadStatus {
    pub job_id: Option<String>,
    pub state: ModelDownloadState,
    pub downloaded_bytes: Option<u64>,
    pub total_size_bytes: Option<u64>,
    pub bytes_per_second: Option<f64>,
    pub started_at: Option<DateTime<Utc>>,
    pub estimated_completion: Option<DateTime<Utc>>,
    pub completed_at: Option<DateTime<Utc>>,
}

impl ModelDownloadStatus {
    #[must_use]
    pub fn cancelled() -> Self {
        Self {
            job_id: None,
            state: ModelDownloadState::Cancelled,
            downloaded_bytes: None,
            total_size_bytes: None,
            bytes_per_second: None,
            started_at: None,
            estimated_completion: None,
            completed_at: None,
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct CharacterDetectionRequest {
    pub request_id: Uuid,
    pub model: String,
    pub system_prompt: String,
    pub paragraphs: Vec<DetectionParagraph>,
    #[serde(default)]
    pub temperature: Temperature,
    #[serde(default)]
    pub reasoning: ReasoningControl,
    pub max_output_tokens: u32,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct DetectionParagraph {
    pub id: String,
    pub text: String,
    pub context_only: bool,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct CharacterDetectionResult {
    #[serde(default)]
    pub characters: Vec<DetectedCharacter>,
    #[serde(default)]
    pub dialogue: Vec<DetectedDialogue>,
    #[serde(skip)]
    pub usage: ProviderUsage,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct DetectedCharacter {
    pub canonical_name: String,
    #[serde(default)]
    pub aliases: Vec<String>,
    pub confidence: f32,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct DetectedDialogue {
    pub paragraph_id: String,
    pub character: String,
    pub start: u32,
    pub end: u32,
    pub confidence: f32,
}

impl CharacterDetectionResult {
    pub fn validate(self, request: &CharacterDetectionRequest) -> Result<Self> {
        let paragraphs: BTreeMap<_, _> = request
            .paragraphs
            .iter()
            .map(|paragraph| (paragraph.id.as_str(), paragraph.text.len()))
            .collect();
        for character in &self.characters {
            if character.canonical_name.trim().is_empty()
                || !(0.0..=1.0).contains(&character.confidence)
            {
                return Err(ProviderError::InvalidResponse(
                    "invalid detected character".to_owned(),
                ));
            }
        }
        for span in &self.dialogue {
            let Some(length) = paragraphs.get(span.paragraph_id.as_str()) else {
                return Err(ProviderError::InvalidResponse(format!(
                    "unknown paragraph id {}",
                    span.paragraph_id
                )));
            };
            if span.start >= span.end
                || usize::try_from(span.end).map_or(true, |end| end > *length)
                || !(0.0..=1.0).contains(&span.confidence)
            {
                return Err(ProviderError::InvalidResponse(
                    "dialogue span is outside its paragraph".to_owned(),
                ));
            }
        }
        Ok(self)
    }
}

#[derive(Clone, Debug)]
pub struct VoiceCloneRequest {
    pub name: String,
    pub description: Option<String>,
    pub samples: Vec<VoiceSample>,
}

#[derive(Clone, Debug)]
pub struct VoiceSample {
    pub file_name: String,
    pub content_type: String,
    pub bytes: bytes::Bytes,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct VoiceClone {
    pub provider_voice_id: String,
    pub name: String,
    pub owned_by_audiobookai: bool,
    pub created_at: DateTime<Utc>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct ProviderHealth {
    pub available: bool,
    pub version: Option<String>,
    pub message: Option<String>,
}

#[derive(Clone, Eq, PartialEq, Serialize, Deserialize)]
pub struct ProcessSpec {
    pub executable: PathBuf,
    #[serde(default)]
    pub arguments: Vec<String>,
    pub working_directory: Option<PathBuf>,
    #[serde(default)]
    pub environment: BTreeMap<String, String>,
}

impl fmt::Debug for ProcessSpec {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ProcessSpec")
            .field("executable", &self.executable)
            .field("argument_count", &self.arguments.len())
            .field("working_directory", &self.working_directory)
            .field(
                "environment_keys",
                &self.environment.keys().collect::<Vec<_>>(),
            )
            .finish()
    }
}

#[derive(Clone, Eq, PartialEq, Serialize, Deserialize)]
pub struct OwnedProcessHandle {
    pub process_id: Uuid,
    pub ownership_token: Uuid,
}

impl fmt::Debug for OwnedProcessHandle {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("OwnedProcessHandle")
            .field("process_id", &self.process_id)
            .field("ownership_token", &"[REDACTED]")
            .finish()
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProcessState {
    Stopped,
    Starting,
    Running,
    Exited,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct ProcessStatus {
    pub state: ProcessState,
    pub operating_system_pid: Option<u32>,
    pub exit_code: Option<i32>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct ProcessLogLine {
    pub timestamp: DateTime<Utc>,
    pub stream: ProcessLogStream,
    pub line: String,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProcessLogStream {
    Stdout,
    Stderr,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn provider_base_endpoint_rejects_query_and_fragment_material() {
        let query = Url::parse("https://provider.invalid/v1?option=value").unwrap();
        let fragment = Url::parse("https://provider.invalid/v1#fragment").unwrap();
        assert!(EndpointConfig::cloud(query, Authentication::None).is_err());
        assert!(EndpointConfig::cloud(fragment, Authentication::None).is_err());
    }

    #[test]
    fn authenticated_external_http_is_allowed_only_on_exact_loopback_hosts() {
        let remote_http = Url::parse("http://192.0.2.1:1234/").unwrap();
        let credential = || Authentication::Bearer(Credential::new("fixture-value"));
        assert!(EndpointConfig::external(remote_http.clone(), credential()).is_err());
        assert!(EndpointConfig::external(remote_http, Authentication::None).is_ok());

        for endpoint in [
            "http://localhost:1234/",
            "http://127.0.0.1:1234/",
            "http://[::1]:1234/",
        ] {
            assert!(EndpointConfig::external(Url::parse(endpoint).unwrap(), credential()).is_ok());
        }
    }
}
