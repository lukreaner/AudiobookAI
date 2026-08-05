use std::{collections::BTreeMap, path::Path};

use chrono::{DateTime, Utc};
use serde::{Deserialize, Deserializer, Serialize, Serializer, de};
use url::Url;

use crate::{
    CapabilitySnapshotId, DeliveryCue, ProviderProfileId, SecretId, SettingsMap, SourceProvenance,
    Validate, ValidationIssue, error::require_non_empty,
};

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ProviderFamily {
    ElevenLabs,
    MlxAudio,
    LocalAi,
    AllTalkV2,
    NativeWindows,
    NativeMacos,
    EspeakNg,
    OpenAi,
    OpenAiCompatible,
    Anthropic,
    Gemini,
    Qwen,
    Kimi,
    Moonshot,
    LmStudio,
    Ollama,
    Custom(String),
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ProviderRole {
    Tts,
    CharacterDetection,
    Both,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ProviderDeployment {
    CloudRemote,
    ExternalEndpoint,
    ManagedChild,
    NativeInProcess,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct ProviderProfile {
    pub id: ProviderProfileId,
    pub name: String,
    pub family: ProviderFamily,
    pub role: ProviderRole,
    pub deployment: ProviderDeployment,
    pub endpoint: Option<String>,
    pub executable_path: Option<String>,
    pub working_directory: Option<String>,
    #[serde(default)]
    pub arguments: Vec<String>,
    #[serde(default)]
    pub environment_secret_ids: BTreeMap<String, SecretId>,
    pub credential_secret_id: Option<SecretId>,
    pub enabled: bool,
    pub concurrency_override: Option<u16>,
    #[serde(default)]
    pub settings: SettingsMap,
    pub capability_snapshot: Option<CapabilitySnapshot>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

impl ProviderProfile {
    #[must_use]
    pub fn effective_concurrency(&self) -> u16 {
        self.concurrency_override
            .or_else(|| {
                self.capability_snapshot
                    .as_ref()
                    .and_then(|snapshot| snapshot.capabilities.recommended_concurrency)
            })
            .unwrap_or(1)
            .max(1)
    }

    #[must_use]
    pub fn uses_cloud(&self) -> bool {
        self.deployment == ProviderDeployment::CloudRemote
    }
}

impl Validate for ProviderProfile {
    fn validation_issues(&self) -> Vec<ValidationIssue> {
        let mut issues = Vec::new();
        require_non_empty(&mut issues, "name", &self.name);
        if let Some(endpoint) = &self.endpoint
            && Url::parse(endpoint).is_err()
        {
            issues.push(ValidationIssue::new(
                "endpoint",
                "invalid_url",
                "provider endpoint must be an absolute URL",
            ));
        }
        if self.deployment == ProviderDeployment::ManagedChild {
            match self.executable_path.as_deref() {
                Some(value) if Path::new(value).is_absolute() => {}
                _ => issues.push(ValidationIssue::new(
                    "executable_path",
                    "absolute_path_required",
                    "managed providers require an absolute executable path",
                )),
            }
            if self
                .working_directory
                .as_deref()
                .is_some_and(|value| !Path::new(value).is_absolute())
            {
                issues.push(ValidationIssue::new(
                    "working_directory",
                    "absolute_path_required",
                    "managed provider working directories must be absolute",
                ));
            }
            if self.arguments.len() > 256 {
                issues.push(ValidationIssue::new(
                    "arguments",
                    "too_many_items",
                    "managed providers may define at most 256 arguments",
                ));
            }
            for (index, argument) in self.arguments.iter().enumerate() {
                if argument.contains('\0') || argument.len() > 16_384 {
                    issues.push(ValidationIssue::new(
                        format!("arguments.{index}"),
                        "invalid_argument",
                        "provider arguments may not contain NUL bytes or exceed 16 KiB",
                    ));
                }
                if argument_name_is_sensitive(argument) {
                    issues.push(ValidationIssue::new(
                        format!("arguments.{index}"),
                        "credential_not_allowed",
                        "provider arguments must not contain credentials; use encrypted credential storage",
                    ));
                }
            }
        } else if self.executable_path.is_some()
            || self.working_directory.is_some()
            || !self.arguments.is_empty()
        {
            issues.push(ValidationIssue::new(
                "deployment",
                "managed_configuration_not_allowed",
                "only managed providers may define an executable, working directory, or arguments",
            ));
        }
        if self.concurrency_override == Some(0) {
            issues.push(ValidationIssue::new(
                "concurrency_override",
                "out_of_range",
                "provider concurrency must be greater than zero",
            ));
        }
        if self.updated_at < self.created_at {
            issues.push(ValidationIssue::new(
                "updated_at",
                "invalid_time_order",
                "updated_at must not precede created_at",
            ));
        }
        issues
    }
}

fn argument_name_is_sensitive(argument: &str) -> bool {
    let normalized = argument
        .trim()
        .trim_start_matches(['-', '/'])
        .to_ascii_lowercase()
        .replace('_', "-");
    let name = normalized
        .split_once(['=', ':'])
        .map_or(normalized.as_str(), |(name, _)| name);
    matches!(
        name,
        "api-key"
            | "apikey"
            | "api-token"
            | "access-token"
            | "auth-token"
            | "authorization"
            | "bearer-token"
            | "client-secret"
            | "credential"
            | "credentials"
            | "password"
            | "passwd"
            | "secret"
            | "token"
    ) || normalized.starts_with("bearer ")
        || normalized.starts_with("basic ")
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct CapabilitySnapshot {
    pub id: CapabilitySnapshotId,
    pub provider_profile_id: ProviderProfileId,
    pub model: Option<String>,
    pub provider_version: Option<String>,
    pub endpoint_fingerprint: String,
    pub capabilities: ProviderCapabilities,
    pub provenance: SourceProvenance,
    pub observed_at: DateTime<Utc>,
    pub expires_at: Option<DateTime<Utc>>,
}

impl CapabilitySnapshot {
    #[must_use]
    pub fn is_valid_at(&self, now: DateTime<Utc>, endpoint_fingerprint: &str) -> bool {
        self.endpoint_fingerprint == endpoint_fingerprint
            && self.expires_at.is_none_or(|expires_at| expires_at > now)
    }
}

#[derive(Clone, Debug, Default, Deserialize, PartialEq, Serialize)]
pub struct ProviderCapabilities {
    pub tts: Option<TtsCapabilities>,
    pub character_detection: Option<CharacterDetectionCapabilities>,
    pub control: Option<ControlCapabilities>,
    pub recommended_concurrency: Option<u16>,
}

#[allow(clippy::struct_excessive_bools)]
#[derive(Clone, Debug, Default, Deserialize, PartialEq, Serialize)]
pub struct TtsCapabilities {
    pub streaming: bool,
    pub cancellation: bool,
    pub voice_discovery: bool,
    pub voice_cloning: VoiceCloneCapabilities,
    pub pronunciation: PronunciationCapabilities,
    #[serde(default)]
    pub output_formats: Vec<ProviderAudioFormat>,
    pub reports_character_usage: bool,
    pub reports_audio_seconds: bool,
    pub reports_cost: bool,
    pub max_input_characters: Option<u64>,
    /// Exact-model performance controls. An absent model is deliberately unsupported.
    #[serde(default)]
    pub model_performance: Vec<ModelPerformanceCapabilities>,
}

/// Inclusive numeric bounds for a model-supported voice performance setting.
///
/// A missing range in [`PerformanceCapabilities`] means unsupported, not "unknown". This makes
/// stale or incomplete capability snapshots fail closed instead of leaking provider-specific
/// fields into a request.
#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Serialize)]
pub struct PerformanceRange {
    pub minimum: f64,
    pub maximum: f64,
}

impl PerformanceRange {
    #[must_use]
    pub const fn new(minimum: f64, maximum: f64) -> Self {
        Self { minimum, maximum }
    }

    #[must_use]
    pub fn is_valid(self) -> bool {
        self.minimum.is_finite() && self.maximum.is_finite() && self.minimum <= self.maximum
    }

    #[must_use]
    pub fn contains(self, value: f64) -> bool {
        self.is_valid() && value.is_finite() && (self.minimum..=self.maximum).contains(&value)
    }
}

/// Provider-neutral performance controls positively supported by one exact provider model.
#[derive(Clone, Debug, Default, Deserialize, PartialEq, Serialize)]
#[serde(default)]
pub struct PerformanceCapabilities {
    pub speed: Option<PerformanceRange>,
    pub pitch: Option<PerformanceRange>,
    pub stability: Option<PerformanceRange>,
    pub similarity: Option<PerformanceRange>,
    pub style: Option<PerformanceRange>,
    pub speaker_boost: bool,
    pub delivery_cues: Vec<DeliveryCue>,
}

/// An exact model binding for performance controls.
///
/// Model identifiers are intentionally matched exactly by adapters. Prefix, family, and fallback
/// matches would turn an unknown model revision into implicit support and violate fail-closed
/// request construction.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct ModelPerformanceCapabilities {
    pub model: String,
    pub performance: PerformanceCapabilities,
}

#[allow(clippy::struct_excessive_bools)]
#[derive(Clone, Debug, Default, Deserialize, PartialEq, Serialize)]
pub struct CharacterDetectionCapabilities {
    pub streaming: bool,
    pub structured_output: bool,
    pub model_discovery: bool,
    pub reports_token_usage: bool,
    pub reports_cost: bool,
    pub temperature: TemperatureCapability,
    pub reasoning: ReasoningCapability,
    pub context_window_tokens: Option<u64>,
}

#[allow(clippy::struct_excessive_bools)]
#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
pub struct VoiceCloneCapabilities {
    pub create: bool,
    pub update: bool,
    pub delete: bool,
    pub local_reference_audio: bool,
}

#[allow(clippy::struct_excessive_bools)]
#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
pub struct PronunciationCapabilities {
    pub provider_dictionary: bool,
    pub ssml: bool,
    pub ipa: bool,
    pub alias: bool,
}

#[allow(clippy::struct_excessive_bools)]
#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
pub struct ControlCapabilities {
    pub start: bool,
    pub stop: bool,
    pub restart: bool,
    pub logs: bool,
    pub list_installed_models: bool,
    #[serde(default)]
    pub download_model: bool,
    #[serde(default)]
    pub delete_model: bool,
    pub load_model: bool,
    pub unload_model: bool,
    pub switch_model: bool,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ProviderAudioFormat {
    PcmS16Le,
    PcmF32Le,
    Wav,
    Mp3,
    Flac,
    OggOpus,
    Aac,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum Temperature {
    #[default]
    Omitted,
    Null,
    Value(TemperatureValue),
}

impl Temperature {
    #[must_use]
    pub const fn is_omitted(&self) -> bool {
        matches!(self, Self::Omitted)
    }

    /// Creates an explicit numeric temperature.
    ///
    /// # Errors
    ///
    /// Returns a validation issue if the value is not finite or is outside
    /// the provider-neutral range from zero through two.
    pub fn from_value(value: f32) -> Result<Self, ValidationIssue> {
        TemperatureValue::new(value).map(Self::Value)
    }
}

impl Serialize for Temperature {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        match self {
            Self::Omitted | Self::Null => serializer.serialize_none(),
            Self::Value(value) => serializer.serialize_f32(value.get()),
        }
    }
}

impl<'de> Deserialize<'de> for Temperature {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let value = Option::<f32>::deserialize(deserializer)?;
        match value {
            None => Ok(Self::Null),
            Some(value) => TemperatureValue::new(value)
                .map(Self::Value)
                .map_err(|issue| de::Error::custom(issue.message)),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct TemperatureValue(f32);

impl TemperatureValue {
    /// Creates a validated numeric temperature value.
    ///
    /// # Errors
    ///
    /// Returns a validation issue if the value is not finite or is outside
    /// the provider-neutral range from zero through two.
    pub fn new(value: f32) -> Result<Self, ValidationIssue> {
        if value.is_finite() && (0.0..=2.0).contains(&value) {
            Ok(Self(value))
        } else {
            Err(ValidationIssue::new(
                "temperature",
                "out_of_range",
                "temperature must be finite and between 0 and 2",
            ))
        }
    }

    #[must_use]
    pub const fn get(self) -> f32 {
        self.0
    }
}

impl Eq for TemperatureValue {}

#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum TemperatureCapability {
    #[default]
    Unsupported,
    Numeric,
    NumericOrNull,
}

impl TemperatureCapability {
    /// Checks a requested temperature against this capability declaration.
    ///
    /// # Errors
    ///
    /// Returns a validation issue when a numeric or explicit-null value is not
    /// supported by the selected provider and model.
    pub fn validate(self, temperature: Temperature) -> Result<(), ValidationIssue> {
        match (self, temperature) {
            (_, Temperature::Omitted)
            | (Self::Numeric, Temperature::Value(_))
            | (Self::NumericOrNull, Temperature::Value(_) | Temperature::Null) => Ok(()),
            (Self::Unsupported, _) => Err(ValidationIssue::new(
                "temperature",
                "unsupported",
                "the selected provider/model does not accept temperature",
            )),
            (Self::Numeric, Temperature::Null) => Err(ValidationIssue::new(
                "temperature",
                "null_unsupported",
                "the selected provider/model accepts numeric temperature but not null",
            )),
        }
    }
}

#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(tag = "mode", content = "value", rename_all = "snake_case")]
pub enum ReasoningControl {
    #[default]
    Inherit,
    Disabled,
    Effort(ReasoningEffort),
    Adaptive,
    TokenBudget(u32),
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ReasoningEffort {
    Minimal,
    Low,
    Medium,
    High,
}

#[allow(clippy::struct_excessive_bools)]
#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
pub struct ReasoningCapability {
    pub disable: bool,
    pub effort: bool,
    pub adaptive: bool,
    pub token_budget: bool,
    pub min_token_budget: Option<u32>,
    pub max_token_budget: Option<u32>,
}

impl ReasoningCapability {
    /// Checks a requested reasoning mode against this capability declaration.
    ///
    /// # Errors
    ///
    /// Returns a validation issue when the mode or token budget is unsupported.
    pub fn validate(&self, control: &ReasoningControl) -> Result<(), ValidationIssue> {
        let supported = match control {
            ReasoningControl::Inherit => true,
            ReasoningControl::Disabled => self.disable,
            ReasoningControl::Effort(_) => self.effort,
            ReasoningControl::Adaptive => self.adaptive,
            ReasoningControl::TokenBudget(value) => {
                self.token_budget
                    && self
                        .min_token_budget
                        .is_none_or(|minimum| *value >= minimum)
                    && self
                        .max_token_budget
                        .is_none_or(|maximum| *value <= maximum)
            }
        };
        if supported {
            Ok(())
        } else {
            Err(ValidationIssue::new(
                "reasoning",
                "unsupported",
                "reasoning control is not supported by the selected provider/model",
            ))
        }
    }
}

#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
pub struct GenerationControls {
    #[serde(default, skip_serializing_if = "Temperature::is_omitted")]
    pub temperature: Temperature,
    #[serde(default)]
    pub reasoning: ReasoningControl,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ProviderHealthStatus {
    Unknown,
    Starting,
    Ready,
    Degraded,
    Unavailable,
    AuthenticationFailed,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct ProviderHealth {
    pub status: ProviderHealthStatus,
    pub message: Option<String>,
    pub checked_at: DateTime<Utc>,
    pub latency_ms: Option<u64>,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ManagedProcessState {
    Stopped,
    Starting,
    Running,
    Stopping,
    Failed,
    Unknown,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct ManagedProcessStatus {
    pub state: ManagedProcessState,
    pub owned_by_audiobookai: bool,
    pub process_id: Option<u32>,
    pub active_model: Option<String>,
    pub started_at: Option<DateTime<Utc>>,
    pub last_error: Option<String>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct ProviderModel {
    pub id: String,
    pub display_name: String,
    pub installed: Option<bool>,
    pub loaded: Option<bool>,
    #[serde(default)]
    pub metadata: BTreeMap<String, String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn omitted_temperature_is_not_serialized() {
        let value = GenerationControls::default();
        let json = serde_json::to_value(value).expect("controls serialize");
        assert!(json.get("temperature").is_none());
    }

    #[test]
    fn null_temperature_round_trips_as_explicit_null() {
        let value: GenerationControls = serde_json::from_value(serde_json::json!({
            "temperature": null,
            "reasoning": {"mode": "inherit"}
        }))
        .expect("controls deserialize");
        assert_eq!(value.temperature, Temperature::Null);
        assert_eq!(
            serde_json::to_value(value)
                .expect("controls serialize")
                .get("temperature"),
            Some(&serde_json::Value::Null)
        );
    }

    #[test]
    fn capability_rejects_unsupported_reasoning() {
        let capability = ReasoningCapability::default();
        assert!(
            capability
                .validate(&ReasoningControl::Effort(ReasoningEffort::High))
                .is_err()
        );
    }

    #[test]
    fn recognizes_inline_credential_arguments() {
        let flag = ["--client", "-secret"].concat();
        assert!(argument_name_is_sensitive(&flag));
        assert!(argument_name_is_sensitive(&format!("{flag}=runtime-value")));
        assert!(argument_name_is_sensitive(
            &["Bearer", "runtime-value"].join(" ")
        ));
        assert!(!argument_name_is_sensitive("--listen=127.0.0.1"));
    }
}
