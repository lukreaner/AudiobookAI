use std::{fmt, sync::Arc};

use audiobookai_providers::{
    Authentication, CharacterProvider, EndpointConfig, HttpTransport, ManagedProcessController,
    ManagedProcessSupervisor, ModelControlProtocol, ProviderControl, ProviderDescriptor,
    ProviderError, ProviderId, ProviderKind, ReqwestTransport, TtsProvider, VoiceCloneProvider,
    adapters::{
        AllTalkProvider, AnthropicProvider, ElevenLabsProvider, GeminiProvider, LocalAiProvider,
        MlxAudioProvider, NativeCommandRunner, NativeTtsConfig, NativeTtsProvider, OllamaProvider,
        OpenAiChatPreset, OpenAiCompatibleProvider, OpenAiResponsesProvider, OpenAiTtsProvider,
        PiperTtsConfig, PiperTtsProvider, TokioNativeCommandRunner,
    },
};
use url::Url;

use super::{CredentialMaterial, RuntimeAdapterKind, RuntimeModelControl, RuntimeProfile};

#[derive(Clone)]
pub struct ProviderAdapterBundle {
    pub runtime_id: ProviderId,
    pub tts: Option<Arc<dyn TtsProvider>>,
    pub character: Option<Arc<dyn CharacterProvider>>,
    pub voice_cloner: Option<Arc<dyn VoiceCloneProvider>>,
    pub control: Option<Arc<dyn ProviderControl>>,
}

impl fmt::Debug for ProviderAdapterBundle {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ProviderAdapterBundle")
            .field("runtime_id", &self.runtime_id)
            .field("tts", &self.tts.is_some())
            .field("character", &self.character.is_some())
            .field("voice_cloner", &self.voice_cloner.is_some())
            .field("control", &self.control.is_some())
            .finish()
    }
}

#[derive(Clone, Debug)]
pub struct ProviderAdapterFactory {
    transport: Arc<dyn HttpTransport>,
    supervisor: ManagedProcessSupervisor,
    native_runner: Arc<dyn NativeCommandRunner>,
}

impl ProviderAdapterFactory {
    pub fn new(
        transport: Arc<dyn HttpTransport>,
        supervisor: ManagedProcessSupervisor,
        native_runner: Arc<dyn NativeCommandRunner>,
    ) -> Self {
        Self {
            transport,
            supervisor,
            native_runner,
        }
    }

    pub fn production() -> Result<Self, ProviderError> {
        Ok(Self::new(
            Arc::new(ReqwestTransport::new()?),
            ManagedProcessSupervisor::default(),
            Arc::new(TokioNativeCommandRunner),
        ))
    }

    pub fn build(
        &self,
        profile: &RuntimeProfile,
        credential: Option<&CredentialMaterial>,
    ) -> Result<ProviderAdapterBundle, ProviderError> {
        profile.validate()?;
        let mut bundle = ProviderAdapterBundle {
            runtime_id: profile.id.clone(),
            tts: None,
            character: None,
            voice_cloner: None,
            control: None,
        };

        let control_endpoint = match profile.adapter {
            RuntimeAdapterKind::NativeOs | RuntimeAdapterKind::Piper => None,
            _ => Some(Self::endpoint_for(profile, credential)?),
        };

        match profile.adapter {
            RuntimeAdapterKind::ElevenLabs => {
                let provider = Arc::new(ElevenLabsProvider::with_endpoint(
                    required_endpoint(control_endpoint.as_ref())?.clone(),
                    Arc::clone(&self.transport),
                )?);
                bundle.tts = Some(provider.clone());
                bundle.voice_cloner = Some(provider);
            }
            RuntimeAdapterKind::MlxAudio => {
                bundle.tts = Some(Arc::new(
                    MlxAudioProvider::new(
                        required_endpoint(control_endpoint.as_ref())?.clone(),
                        Arc::clone(&self.transport),
                    )?
                    .with_model_performance(profile.model_performance.clone())?,
                ));
            }
            RuntimeAdapterKind::LocalAi => {
                bundle.tts = Some(Arc::new(LocalAiProvider::new(
                    required_endpoint(control_endpoint.as_ref())?.clone(),
                    Arc::clone(&self.transport),
                )?));
            }
            RuntimeAdapterKind::AllTalkV2 => {
                bundle.tts = Some(Arc::new(AllTalkProvider::new(
                    required_endpoint(control_endpoint.as_ref())?.clone(),
                    Arc::clone(&self.transport),
                )?));
            }
            RuntimeAdapterKind::NativeOs => {
                let executable = profile.executable.clone().ok_or_else(|| {
                    ProviderError::Configuration(
                        "native TTS requires an executable path".to_owned(),
                    )
                })?;
                let config = NativeTtsConfig::for_current_os(executable)?;
                bundle.tts = Some(Arc::new(NativeTtsProvider::new(
                    config,
                    Arc::clone(&self.native_runner),
                )?));
            }
            RuntimeAdapterKind::Piper => {
                let executable = profile.executable.clone().ok_or_else(|| {
                    ProviderError::Configuration(
                        "Piper requires its app-managed executable".to_owned(),
                    )
                })?;
                let voices_dir = profile.piper_voices_dir.clone().ok_or_else(|| {
                    ProviderError::Configuration(
                        "Piper requires its app-managed voice directory".to_owned(),
                    )
                })?;
                let selected_voice = profile.model.clone().ok_or_else(|| {
                    ProviderError::Configuration(
                        "Piper requires one exact selected installed voice".to_owned(),
                    )
                })?;
                bundle.tts = Some(Arc::new(PiperTtsProvider::new(
                    PiperTtsConfig::new(executable, voices_dir, selected_voice)?,
                    Arc::clone(&self.native_runner),
                )?));
            }
            RuntimeAdapterKind::OpenAiTts => {
                bundle.tts = Some(Arc::new(OpenAiTtsProvider::new(
                    required_endpoint(control_endpoint.as_ref())?.clone(),
                    Arc::clone(&self.transport),
                )?));
            }
            RuntimeAdapterKind::OpenAi => {
                bundle.character = Some(Arc::new(OpenAiResponsesProvider::with_endpoint(
                    required_endpoint(control_endpoint.as_ref())?.clone(),
                    Arc::clone(&self.transport),
                )?));
            }
            RuntimeAdapterKind::OpenAiCompatible
            | RuntimeAdapterKind::Qwen
            | RuntimeAdapterKind::Kimi
            | RuntimeAdapterKind::Moonshot
            | RuntimeAdapterKind::LmStudio => {
                let preset = match profile.adapter {
                    RuntimeAdapterKind::OpenAiCompatible => OpenAiChatPreset::Generic,
                    RuntimeAdapterKind::Qwen => OpenAiChatPreset::Qwen,
                    RuntimeAdapterKind::Kimi => OpenAiChatPreset::Kimi,
                    RuntimeAdapterKind::Moonshot => OpenAiChatPreset::Moonshot,
                    RuntimeAdapterKind::LmStudio => OpenAiChatPreset::LmStudio,
                    _ => unreachable!("covered by the containing match pattern"),
                };
                bundle.character = Some(Arc::new(OpenAiCompatibleProvider::new(
                    preset,
                    required_endpoint(control_endpoint.as_ref())?.clone(),
                    Arc::clone(&self.transport),
                )?));
            }
            RuntimeAdapterKind::Anthropic => {
                reject_custom_cloud_endpoint(profile, "Anthropic")?;
                bundle.character = Some(Arc::new(AnthropicProvider::new(
                    required_credential(credential)?.to_provider_credential()?,
                    Arc::clone(&self.transport),
                )?));
            }
            RuntimeAdapterKind::Gemini => {
                reject_custom_cloud_endpoint(profile, "Gemini")?;
                bundle.character = Some(Arc::new(GeminiProvider::new(
                    required_credential(credential)?.to_provider_credential()?,
                    Arc::clone(&self.transport),
                )?));
            }
            RuntimeAdapterKind::Ollama => {
                bundle.character = Some(Arc::new(OllamaProvider::new(
                    required_endpoint(control_endpoint.as_ref())?.clone(),
                    Arc::clone(&self.transport),
                )?));
            }
        }

        if matches!(profile.mode, ProviderKind::ManagedChild)
            || profile.effective_model_control().is_some()
        {
            let descriptor = ProviderDescriptor {
                id: profile.id.clone(),
                display_name: profile.display_name.clone(),
                kind: profile.mode,
                endpoint_family: "audiobookai-runtime-control".to_owned(),
            };
            let mut control = ManagedProcessController::new(descriptor, self.supervisor.clone());
            if let Some(protocol) = profile.effective_model_control() {
                control = control.with_model_control(
                    match protocol {
                        RuntimeModelControl::Ollama => ModelControlProtocol::Ollama,
                        RuntimeModelControl::LmStudio => ModelControlProtocol::LmStudio,
                        RuntimeModelControl::LocalAi => ModelControlProtocol::LocalAi,
                    },
                    required_endpoint(control_endpoint.as_ref())?.clone(),
                    Arc::clone(&self.transport),
                );
            }
            bundle.control = Some(Arc::new(control));
        }

        Ok(bundle)
    }

    fn endpoint_for(
        profile: &RuntimeProfile,
        credential: Option<&CredentialMaterial>,
    ) -> Result<EndpointConfig, ProviderError> {
        let url = profile
            .endpoint
            .clone()
            .or_else(|| default_endpoint(profile.adapter))
            .ok_or_else(|| {
                ProviderError::Configuration(format!(
                    "{} requires an HTTP endpoint",
                    profile.display_name
                ))
            })?;
        let authentication = authentication_for(profile, credential)?;
        match profile.mode {
            ProviderKind::CloudRemote => EndpointConfig::cloud(url, authentication),
            ProviderKind::ExternalEndpoint => EndpointConfig::external(url, authentication),
            ProviderKind::ManagedChild => EndpointConfig::managed_loopback(url, authentication),
            ProviderKind::Native => Err(ProviderError::Configuration(
                "native providers do not use HTTP endpoints".to_owned(),
            )),
        }
    }
}

impl Default for ProviderAdapterFactory {
    fn default() -> Self {
        Self::production().expect("the default HTTP client configuration is valid")
    }
}

fn authentication_for(
    profile: &RuntimeProfile,
    credential: Option<&CredentialMaterial>,
) -> Result<Authentication, ProviderError> {
    let requires_credential = matches!(profile.mode, ProviderKind::CloudRemote);
    let provider_credential = match credential {
        Some(value) => Some(value.to_provider_credential()?),
        None if requires_credential => {
            return Err(ProviderError::Configuration(format!(
                "{} requires a configured credential",
                profile.display_name
            )));
        }
        None => None,
    };
    Ok(match (profile.adapter, provider_credential) {
        (RuntimeAdapterKind::ElevenLabs, Some(value)) => Authentication::Header {
            name: "xi-api-key".to_owned(),
            value,
        },
        (RuntimeAdapterKind::Gemini, Some(value)) => Authentication::Header {
            name: "x-goog-api-key".to_owned(),
            value,
        },
        (RuntimeAdapterKind::Anthropic, Some(value)) => Authentication::Header {
            name: "x-api-key".to_owned(),
            value,
        },
        (_, Some(value)) => Authentication::Bearer(value),
        (_, None) => Authentication::None,
    })
}

fn default_endpoint(adapter: RuntimeAdapterKind) -> Option<Url> {
    let value = match adapter {
        RuntimeAdapterKind::ElevenLabs => "https://api.elevenlabs.io/",
        RuntimeAdapterKind::OpenAi | RuntimeAdapterKind::OpenAiTts => "https://api.openai.com/",
        RuntimeAdapterKind::Anthropic => "https://api.anthropic.com/",
        RuntimeAdapterKind::Gemini => "https://generativelanguage.googleapis.com/",
        _ => return None,
    };
    Url::parse(value).ok()
}

fn required_endpoint(endpoint: Option<&EndpointConfig>) -> Result<&EndpointConfig, ProviderError> {
    endpoint.ok_or_else(|| ProviderError::Configuration("provider endpoint is missing".to_owned()))
}

fn required_credential(
    credential: Option<&CredentialMaterial>,
) -> Result<&CredentialMaterial, ProviderError> {
    credential.ok_or_else(|| {
        ProviderError::Configuration("provider credential is not configured".to_owned())
    })
}

fn reject_custom_cloud_endpoint(
    profile: &RuntimeProfile,
    provider_name: &str,
) -> Result<(), ProviderError> {
    let official = default_endpoint(profile.adapter);
    if matches!(profile.mode, ProviderKind::CloudRemote)
        && (profile.endpoint.is_none() || profile.endpoint == official)
    {
        Ok(())
    } else {
        Err(ProviderError::Configuration(format!(
            "{provider_name} uses its official cloud endpoint; use the OpenAI-compatible adapter for a custom endpoint"
        )))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn constructs_required_adapter_families_without_exposing_credentials() {
        let factory = ProviderAdapterFactory::default();
        let credential = CredentialMaterial::new(b"not-a-real-key".to_vec());
        let profile = RuntimeProfile::new(
            ProviderId::new("eleven-primary").unwrap(),
            "ElevenLabs primary",
            RuntimeAdapterKind::ElevenLabs,
            ProviderKind::CloudRemote,
        );
        let bundle = factory.build(&profile, Some(&credential)).unwrap();
        assert!(bundle.tts.is_some());
        assert!(bundle.voice_cloner.is_some());
        assert!(bundle.character.is_none());
        assert!(!format!("{bundle:?}").contains("not-a-real-key"));
    }

    #[test]
    fn constructs_openai_speech_as_tts_without_an_llm_adapter() {
        let factory = ProviderAdapterFactory::default();
        let credential = CredentialMaterial::new(b"not-a-real-key".to_vec());
        let profile = RuntimeProfile::new(
            ProviderId::new("openai-speech").unwrap(),
            "OpenAI Speech",
            RuntimeAdapterKind::OpenAiTts,
            ProviderKind::CloudRemote,
        );

        let bundle = factory.build(&profile, Some(&credential)).unwrap();

        assert!(bundle.tts.is_some());
        assert!(bundle.character.is_none());
        assert!(bundle.voice_cloner.is_none());
        assert!(!format!("{bundle:?}").contains("not-a-real-key"));
    }

    #[test]
    fn cloud_profiles_require_credentials() {
        let factory = ProviderAdapterFactory::default();
        let profile = RuntimeProfile::new(
            ProviderId::new("openai-primary").unwrap(),
            "OpenAI primary",
            RuntimeAdapterKind::OpenAi,
            ProviderKind::CloudRemote,
        );
        assert!(matches!(
            factory.build(&profile, None),
            Err(ProviderError::Configuration(_))
        ));
    }

    #[test]
    fn fixed_cloud_adapters_accept_only_their_official_endpoint() {
        let factory = ProviderAdapterFactory::default();
        let credential = CredentialMaterial::new(b"test-only".to_vec());
        for adapter in [RuntimeAdapterKind::Anthropic, RuntimeAdapterKind::Gemini] {
            let mut profile = RuntimeProfile::new(
                ProviderId::new(format!("{adapter:?}").to_lowercase()).unwrap(),
                format!("{adapter:?}"),
                adapter,
                ProviderKind::CloudRemote,
            );
            profile.endpoint = default_endpoint(adapter);
            assert!(factory.build(&profile, Some(&credential)).is_ok());

            profile.endpoint = Some(Url::parse("https://example.invalid/").unwrap());
            assert!(matches!(
                factory.build(&profile, Some(&credential)),
                Err(ProviderError::Configuration(_))
            ));
        }
    }

    #[test]
    fn managed_endpoints_must_be_loopback() {
        let factory = ProviderAdapterFactory::default();
        let mut profile = RuntimeProfile::new(
            ProviderId::new("localai").unwrap(),
            "LocalAI",
            RuntimeAdapterKind::LocalAi,
            ProviderKind::ManagedChild,
        );
        profile.endpoint = Some(Url::parse("http://192.0.2.1:8080/").unwrap());
        profile.executable = Some(absolute_test_path("local-ai"));
        assert!(matches!(
            factory.build(&profile, None),
            Err(ProviderError::Configuration(_))
        ));
    }

    fn absolute_test_path(file: &str) -> std::path::PathBuf {
        if cfg!(windows) {
            std::path::PathBuf::from(format!(r"C:\AudiobookAI\{file}"))
        } else {
            std::path::PathBuf::from(format!("/opt/audiobookai/{file}"))
        }
    }
}
