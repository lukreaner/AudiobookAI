use std::{
    collections::BTreeMap,
    fmt,
    path::{Path, PathBuf},
};

use audiobookai_providers::{Credential, ProcessSpec, ProviderError, ProviderId, ProviderKind};
use url::Url;
use zeroize::Zeroizing;

/// A concrete adapter family selected by a provider profile.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RuntimeAdapterKind {
    ElevenLabs,
    MlxAudio,
    LocalAi,
    AllTalkV2,
    Piper,
    NativeOs,
    OpenAiTts,
    OpenAi,
    OpenAiCompatible,
    Anthropic,
    Gemini,
    Qwen,
    Kimi,
    Moonshot,
    LmStudio,
    Ollama,
}

impl RuntimeAdapterKind {
    pub const fn is_native(self) -> bool {
        matches!(self, Self::NativeOs | Self::Piper)
    }

    pub const fn is_character_provider(self) -> bool {
        matches!(
            self,
            Self::OpenAi
                | Self::OpenAiCompatible
                | Self::Anthropic
                | Self::Gemini
                | Self::Qwen
                | Self::Kimi
                | Self::Moonshot
                | Self::LmStudio
                | Self::Ollama
        )
    }

    pub const fn is_tts_provider(self) -> bool {
        matches!(
            self,
            Self::ElevenLabs
                | Self::MlxAudio
                | Self::LocalAi
                | Self::AllTalkV2
                | Self::Piper
                | Self::NativeOs
                | Self::OpenAiTts
        )
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RuntimeModelControl {
    Ollama,
    LmStudio,
    LocalAi,
}

/// Secret bytes decrypted for one adapter construction.
///
/// The value zeroizes on drop and deliberately has no serialization support. Its debug output
/// never includes the value or its length.
pub struct CredentialMaterial(Zeroizing<Vec<u8>>);

impl CredentialMaterial {
    pub fn new(bytes: impl Into<Vec<u8>>) -> Self {
        Self(Zeroizing::new(bytes.into()))
    }

    pub fn from_string(value: &Zeroizing<String>) -> Self {
        Self::new(value.as_bytes().to_vec())
    }

    pub fn from_zeroizing_bytes(value: &Zeroizing<Vec<u8>>) -> Self {
        Self::new(value.as_slice().to_vec())
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    pub(crate) fn to_provider_credential(&self) -> Result<Credential, ProviderError> {
        let value = std::str::from_utf8(self.0.as_slice()).map_err(|_| {
            ProviderError::Configuration("provider credentials must be valid UTF-8".to_owned())
        })?;
        if value.is_empty() {
            return Err(ProviderError::Configuration(
                "provider credentials may not be empty".to_owned(),
            ));
        }
        Ok(Credential::new(value.to_owned().into_boxed_str()))
    }
}

impl fmt::Debug for CredentialMaterial {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("CredentialMaterial([REDACTED])")
    }
}

/// A neutral, credential-free service profile used to construct provider adapters.
#[derive(Clone)]
pub struct RuntimeProfile {
    pub id: ProviderId,
    pub display_name: String,
    pub adapter: RuntimeAdapterKind,
    pub mode: ProviderKind,
    pub endpoint: Option<Url>,
    pub executable: Option<PathBuf>,
    pub arguments: Vec<String>,
    pub working_directory: Option<PathBuf>,
    /// Exact model selected by this logical provider connection. Adapters that expose a
    /// connection-scoped catalog (notably Piper) must fail closed outside this value.
    pub model: Option<String>,
    /// App-owned voice root for the command-based Piper adapter.
    pub piper_voices_dir: Option<PathBuf>,
    pub environment: BTreeMap<String, String>,
    pub model_control: Option<RuntimeModelControl>,
    pub model_performance: Vec<audiobookai_core::ModelPerformanceCapabilities>,
}

impl RuntimeProfile {
    pub fn new(
        id: ProviderId,
        display_name: impl Into<String>,
        adapter: RuntimeAdapterKind,
        mode: ProviderKind,
    ) -> Self {
        Self {
            id,
            display_name: display_name.into(),
            adapter,
            mode,
            endpoint: None,
            executable: None,
            arguments: Vec::new(),
            working_directory: None,
            model: None,
            piper_voices_dir: None,
            environment: BTreeMap::new(),
            model_control: None,
            model_performance: Vec::new(),
        }
    }

    pub fn validate(&self) -> Result<(), ProviderError> {
        if self.display_name.trim().is_empty() {
            return Err(ProviderError::Configuration(
                "provider display name may not be empty".to_owned(),
            ));
        }
        if self.adapter.is_native() != matches!(self.mode, ProviderKind::Native) {
            return Err(ProviderError::Configuration(
                "native adapter and provider mode must be selected together".to_owned(),
            ));
        }
        if matches!(self.mode, ProviderKind::ManagedChild) {
            let executable = self.executable.as_deref().ok_or_else(|| {
                ProviderError::Configuration(
                    "managed providers require an absolute executable path".to_owned(),
                )
            })?;
            require_absolute(executable, "managed provider executable")?;
        } else if matches!(self.mode, ProviderKind::Native) {
            let executable = self.executable.as_deref().ok_or_else(|| {
                ProviderError::Configuration(
                    "native TTS requires an absolute executable path".to_owned(),
                )
            })?;
            require_absolute(executable, "native TTS executable")?;
            if self.endpoint.is_some() {
                return Err(ProviderError::Configuration(
                    "native TTS profiles may not define an HTTP endpoint".to_owned(),
                ));
            }
            if self.working_directory.is_some()
                || !self.arguments.is_empty()
                || !self.environment.is_empty()
            {
                return Err(ProviderError::Configuration(
                    "native TTS profiles may not define managed-process settings".to_owned(),
                ));
            }
        } else if self.executable.is_some()
            || self.working_directory.is_some()
            || !self.arguments.is_empty()
            || !self.environment.is_empty()
        {
            return Err(ProviderError::Configuration(
                "only managed or native profiles may define an executable".to_owned(),
            ));
        }
        if let Some(directory) = &self.working_directory {
            require_absolute(directory, "managed provider working directory")?;
        }
        match (self.adapter, self.piper_voices_dir.as_deref()) {
            (RuntimeAdapterKind::Piper, Some(directory)) => {
                require_absolute(directory, "Piper voice directory")?;
            }
            (RuntimeAdapterKind::Piper, None) => {
                return Err(ProviderError::Configuration(
                    "Piper requires an absolute app-owned voice directory".to_owned(),
                ));
            }
            (_, Some(_)) => {
                return Err(ProviderError::Configuration(
                    "only Piper profiles may define a Piper voice directory".to_owned(),
                ));
            }
            (_, None) => {}
        }
        validate_arguments(&self.arguments)?;
        if self
            .environment
            .keys()
            .any(|key| key.is_empty() || key.contains(['=', '\0']))
        {
            return Err(ProviderError::Configuration(
                "managed provider environment contains an invalid key".to_owned(),
            ));
        }
        Ok(())
    }

    pub fn process_spec(&self) -> Result<Option<ProcessSpec>, ProviderError> {
        self.validate()?;
        if !matches!(self.mode, ProviderKind::ManagedChild) {
            return Ok(None);
        }
        Ok(Some(ProcessSpec {
            executable: self.executable.clone().ok_or_else(|| {
                ProviderError::Configuration("managed providers require an executable".to_owned())
            })?,
            arguments: self.arguments.clone(),
            working_directory: self.working_directory.clone(),
            environment: self.environment.clone(),
        }))
    }

    pub const fn effective_model_control(&self) -> Option<RuntimeModelControl> {
        match self.model_control {
            Some(protocol) => Some(protocol),
            None => match self.adapter {
                RuntimeAdapterKind::Ollama => Some(RuntimeModelControl::Ollama),
                RuntimeAdapterKind::LmStudio => Some(RuntimeModelControl::LmStudio),
                RuntimeAdapterKind::LocalAi => Some(RuntimeModelControl::LocalAi),
                _ => None,
            },
        }
    }
}

impl fmt::Debug for RuntimeProfile {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("RuntimeProfile")
            .field("id", &self.id)
            .field("display_name", &self.display_name)
            .field("adapter", &self.adapter)
            .field("mode", &self.mode)
            .field(
                "endpoint_scheme",
                &self.endpoint.as_ref().map(url::Url::scheme),
            )
            .field(
                "endpoint_has_host",
                &self
                    .endpoint
                    .as_ref()
                    .map(|endpoint| endpoint.host_str().is_some()),
            )
            .field("executable", &self.executable)
            .field("argument_count", &self.arguments.len())
            .field("working_directory", &self.working_directory)
            .field("model_configured", &self.model.is_some())
            .field("piper_voices_dir", &self.piper_voices_dir)
            .field(
                "environment_keys",
                &self.environment.keys().collect::<Vec<_>>(),
            )
            .field("model_control", &self.model_control)
            .field("model_performance", &self.model_performance)
            .finish()
    }
}

fn require_absolute(path: &Path, label: &str) -> Result<(), ProviderError> {
    if path.is_absolute() {
        Ok(())
    } else {
        Err(ProviderError::Configuration(format!(
            "{label} must be absolute"
        )))
    }
}

fn validate_arguments(arguments: &[String]) -> Result<(), ProviderError> {
    if arguments.len() > 256
        || arguments
            .iter()
            .any(|argument| argument.contains('\0') || argument.len() > 16_384)
        || arguments.iter().map(String::len).sum::<usize>() > 131_072
    {
        return Err(ProviderError::Configuration(
            "managed provider arguments must contain at most 256 NUL-free values and 128 KiB total"
                .to_owned(),
        ));
    }
    if arguments.iter().any(|argument| {
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
    }) {
        return Err(ProviderError::Configuration(
            "managed provider arguments must not contain credentials; use encrypted credential storage"
                .to_owned(),
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn debug_output_never_contains_credentials_or_environment_values() {
        let credential = CredentialMaterial::new(b"super-secret".to_vec());
        assert_eq!(format!("{credential:?}"), "CredentialMaterial([REDACTED])");

        let mut profile = RuntimeProfile::new(
            ProviderId::new("managed").unwrap(),
            "Managed",
            RuntimeAdapterKind::LocalAi,
            ProviderKind::ManagedChild,
        );
        profile.executable = Some(absolute_test_path("server"));
        profile
            .environment
            .insert("API_KEY".to_owned(), "do-not-print".to_owned());
        let argument_value = ["runtime", "argument", "value"].join("-");
        profile.arguments = vec![argument_value.clone()];
        let debug = format!("{profile:?}");
        assert!(debug.contains("API_KEY"));
        assert!(!debug.contains("do-not-print"));
        assert!(!debug.contains(&argument_value));
    }

    #[test]
    fn managed_profiles_require_an_absolute_executable() {
        let profile = RuntimeProfile::new(
            ProviderId::new("managed").unwrap(),
            "Managed",
            RuntimeAdapterKind::LocalAi,
            ProviderKind::ManagedChild,
        );
        assert!(profile.validate().is_err());
    }

    #[test]
    fn managed_profiles_reject_invalid_argument_values() {
        let mut profile = RuntimeProfile::new(
            ProviderId::new("managed").unwrap(),
            "Managed",
            RuntimeAdapterKind::LocalAi,
            ProviderKind::ManagedChild,
        );
        profile.executable = Some(absolute_test_path("server"));
        profile.arguments = vec!["bad\0argument".to_owned()];
        assert!(profile.validate().is_err());
    }

    #[test]
    fn managed_profiles_reject_credentials_in_arguments() {
        let mut profile = RuntimeProfile::new(
            ProviderId::new("managed").unwrap(),
            "Managed",
            RuntimeAdapterKind::LocalAi,
            ProviderKind::ManagedChild,
        );
        profile.executable = Some(absolute_test_path("server"));
        profile.arguments = vec![["--auth", "-token=", "runtime-value"].concat()];
        assert!(profile.validate().is_err());
    }

    fn absolute_test_path(file: &str) -> PathBuf {
        if cfg!(windows) {
            PathBuf::from(format!(r"C:\AudiobookAI\{file}"))
        } else {
            PathBuf::from(format!("/opt/audiobookai/{file}"))
        }
    }
}
