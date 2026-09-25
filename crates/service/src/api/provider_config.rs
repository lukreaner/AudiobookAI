use super::{
    AppState, ChronoDuration, ProviderCapabilitiesView, ProviderKindView, ProviderModeView,
    ProviderProfileView, ProviderRoleView, ProviderStatusView, ServiceError, Utc,
    piper_uninstall_in_progress,
};

// This is the explicit view-to-domain mapping boundary; keeping the mapping in
// one function prevents capability and provenance fields from drifting apart.
#[allow(clippy::too_many_lines)]
pub(crate) async fn persist_provider(
    state: &AppState,
    profile: &ProviderProfileView,
    secret_id: Option<audiobookai_core::SecretId>,
) -> Result<(), ServiceError> {
    use audiobookai_core::{
        CapabilitySnapshot, CapabilitySnapshotId, CharacterDetectionCapabilities,
        ControlCapabilities, PronunciationCapabilities, ProviderAudioFormat, ProviderCapabilities,
        ProviderDeployment, ProviderFamily, ProviderProfile, ProviderProfileId, ProviderRole,
        ReasoningCapability, SettingsMap, SourceProvenance, TemperatureCapability, TtsCapabilities,
        VoiceCloneCapabilities,
    };
    validate_provider_model_compatibility(&profile.kind, profile.role, profile.model.as_deref())?;
    if matches!(profile.kind, ProviderKindView::Piper) && profile.model.is_none() {
        return Err(ServiceError::InvalidRequest(
            "a verified installed Piper voice must be selected".to_owned(),
        ));
    }
    if matches!(profile.kind, ProviderKindView::Piper) {
        let management = state.piper.view().await;
        if piper_uninstall_in_progress(management.active_operation.as_ref()) {
            return Err(ServiceError::Conflict(
                "Piper is being uninstalled; wait for the operation to finish before saving a Piper connection"
                    .to_owned(),
            ));
        }
        if !management.installed {
            return Err(ServiceError::InvalidRequest(
                "install the managed Piper runtime before creating a Piper connection".to_owned(),
            ));
        }
        if let Some(model) = profile.model.as_deref()
            && !management
                .installed_voices
                .iter()
                .any(|voice| voice.id == model)
        {
            return Err(ServiceError::InvalidRequest(
                "the selected Piper voice is not installed and verified".to_owned(),
            ));
        }
    }
    let provider_id = ProviderProfileId::from_uuid(profile.id);
    let existing = state
        .database
        .repositories()
        .providers
        .get(provider_id)
        .await
        .map_err(|error| ServiceError::Storage(error.to_string()))?;
    let family = match profile.kind {
        ProviderKindView::Elevenlabs => ProviderFamily::ElevenLabs,
        ProviderKindView::MlxAudio => ProviderFamily::MlxAudio,
        ProviderKindView::Localai => ProviderFamily::LocalAi,
        ProviderKindView::AlltalkV2 => ProviderFamily::AllTalkV2,
        ProviderKindView::Piper => ProviderFamily::Piper,
        ProviderKindView::NativeOs => match std::env::consts::OS {
            "windows" => ProviderFamily::NativeWindows,
            "macos" => ProviderFamily::NativeMacos,
            _ => ProviderFamily::EspeakNg,
        },
        ProviderKindView::OpenaiTts | ProviderKindView::Openai => ProviderFamily::OpenAi,
        ProviderKindView::OpenaiCompatible => ProviderFamily::OpenAiCompatible,
        ProviderKindView::Anthropic => ProviderFamily::Anthropic,
        ProviderKindView::Gemini => ProviderFamily::Gemini,
        ProviderKindView::Qwen => ProviderFamily::Qwen,
        ProviderKindView::Kimi => ProviderFamily::Kimi,
        ProviderKindView::Moonshot => ProviderFamily::Moonshot,
        ProviderKindView::LmStudio => ProviderFamily::LmStudio,
        ProviderKindView::Ollama => ProviderFamily::Ollama,
    };
    let role = match profile.role {
        ProviderRoleView::Tts => ProviderRole::Tts,
        ProviderRoleView::Llm => ProviderRole::CharacterDetection,
    };
    let deployment = match profile.mode {
        ProviderModeView::CloudRemote => ProviderDeployment::CloudRemote,
        ProviderModeView::ExternalEndpoint => ProviderDeployment::ExternalEndpoint,
        ProviderModeView::ManagedChild => ProviderDeployment::ManagedChild,
        ProviderModeView::Native => ProviderDeployment::NativeInProcess,
    };
    let now = Utc::now();
    let mut settings = std::collections::BTreeMap::new();
    if let Some(model) = &profile.model {
        settings.insert("model".to_owned(), serde_json::Value::String(model.clone()));
    }
    if let Some(tokens) = profile.context_window_tokens {
        settings.insert(
            "context_window_tokens".to_owned(),
            serde_json::json!(tokens),
        );
    }
    let mut capability_snapshot = profile.capabilities.as_ref().map(|capabilities| {
        let observed_at = profile.capability_updated_at.unwrap_or(now);
        let reasoning = ReasoningCapability {
            disable: capabilities
                .reasoning
                .iter()
                .any(|value| value == "disabled"),
            effort: capabilities.reasoning.iter().any(|value| value == "effort"),
            adaptive: capabilities
                .reasoning
                .iter()
                .any(|value| value == "adaptive"),
            token_budget: capabilities
                .reasoning
                .iter()
                .any(|value| value == "token_budget"),
            min_token_budget: None,
            max_token_budget: None,
        };
        let mut fingerprint = blake3::Hasher::new();
        fingerprint.update(profile.endpoint.as_deref().unwrap_or("native").as_bytes());
        fingerprint.update(profile.model.as_deref().unwrap_or("default").as_bytes());
        fingerprint.update(match profile.role {
            ProviderRoleView::Tts => b"tts",
            ProviderRoleView::Llm => b"llm",
        });
        CapabilitySnapshot {
            id: CapabilitySnapshotId::new(),
            provider_profile_id: provider_id,
            model: profile.model.clone(),
            provider_version: None,
            endpoint_fingerprint: fingerprint.finalize().to_hex().to_string(),
            capabilities: ProviderCapabilities {
                tts: capabilities.tts.then(|| TtsCapabilities {
                    streaming: capabilities.streaming,
                    // Streaming support does not imply a provider-side cancellation endpoint.
                    // None of the current TTS adapters overrides the fail-closed cancel contract.
                    cancellation: false,
                    voice_discovery: true,
                    voice_cloning: VoiceCloneCapabilities {
                        create: capabilities.voice_cloning,
                        update: capabilities.voice_cloning,
                        delete: capabilities.voice_cloning,
                        local_reference_audio: capabilities.voice_cloning,
                    },
                    pronunciation: PronunciationCapabilities {
                        provider_dictionary: capabilities.pronunciation,
                        ssml: capabilities.pronunciation,
                        ipa: capabilities.pronunciation,
                        alias: capabilities.pronunciation,
                    },
                    output_formats: match profile.kind {
                        ProviderKindView::Elevenlabs => {
                            vec![ProviderAudioFormat::PcmS16Le, ProviderAudioFormat::Mp3]
                        }
                        ProviderKindView::MlxAudio | ProviderKindView::Localai => vec![
                            ProviderAudioFormat::PcmS16Le,
                            ProviderAudioFormat::Wav,
                            ProviderAudioFormat::Mp3,
                            ProviderAudioFormat::Flac,
                            ProviderAudioFormat::Aac,
                        ],
                        ProviderKindView::Openai | ProviderKindView::OpenaiTts
                            if matches!(profile.role, ProviderRoleView::Tts) =>
                        {
                            vec![
                                ProviderAudioFormat::PcmS16Le,
                                ProviderAudioFormat::Wav,
                                ProviderAudioFormat::Mp3,
                                ProviderAudioFormat::Flac,
                                ProviderAudioFormat::Aac,
                            ]
                        }
                        ProviderKindView::Gemini
                            if matches!(profile.role, ProviderRoleView::Tts) =>
                        {
                            vec![ProviderAudioFormat::PcmS16Le, ProviderAudioFormat::Wav]
                        }
                        ProviderKindView::AlltalkV2
                        | ProviderKindView::Piper
                        | ProviderKindView::NativeOs => {
                            vec![ProviderAudioFormat::Wav]
                        }
                        _ => Vec::new(),
                    },
                    reports_character_usage: matches!(profile.kind, ProviderKindView::Elevenlabs),
                    reports_audio_seconds: false,
                    reports_cost: false,
                    max_input_characters: (matches!(
                        profile.kind,
                        ProviderKindView::Openai | ProviderKindView::OpenaiTts
                    ) && matches!(profile.role, ProviderRoleView::Tts))
                    .then_some(4096),
                    model_performance: capabilities.model_performance.clone(),
                }),
                character_detection: capabilities.character_detection.then_some({
                    CharacterDetectionCapabilities {
                        streaming: capabilities.streaming,
                        structured_output: true,
                        model_discovery: true,
                        reports_token_usage: matches!(
                            profile.kind,
                            ProviderKindView::Openai
                                | ProviderKindView::Anthropic
                                | ProviderKindView::Gemini
                                | ProviderKindView::Ollama
                        ),
                        reports_cost: false,
                        temperature: match capabilities.temperature.as_str() {
                            "number" => TemperatureCapability::Numeric,
                            "nullable" => TemperatureCapability::NumericOrNull,
                            _ => TemperatureCapability::Unsupported,
                        },
                        reasoning,
                        context_window_tokens: profile.context_window_tokens,
                    }
                }),
                control: (capabilities.process_control || capabilities.model_control).then_some({
                    ControlCapabilities {
                        start: capabilities.process_control,
                        stop: capabilities.process_control,
                        restart: capabilities.process_control,
                        logs: capabilities.process_control,
                        list_installed_models: capabilities.model_list,
                        download_model: capabilities.model_download,
                        delete_model: capabilities.model_delete,
                        load_model: capabilities.model_load,
                        unload_model: capabilities.model_unload,
                        switch_model: capabilities.model_switch,
                    }
                }),
                recommended_concurrency: capabilities.max_concurrency,
            },
            provenance: SourceProvenance {
                source: profile
                    .capability_source
                    .clone()
                    .unwrap_or_else(|| "adapter_probe".to_owned()),
                source_version: Some(env!("CARGO_PKG_VERSION").to_owned()),
                request_id: None,
                observed_at: Some(observed_at),
                attributes: std::collections::BTreeMap::new(),
            },
            observed_at,
            expires_at: Some(observed_at + ChronoDuration::hours(24)),
        }
    });
    // Snapshot identity represents the dispatch contract, not the time of the latest health
    // observation. Reuse it when routing, credentials, model, and effective capabilities are
    // unchanged so a startup/connection check cannot invalidate already admitted durable work.
    if let (Some(snapshot), Some(existing_profile)) =
        (capability_snapshot.as_mut(), existing.as_ref())
        && let Some(existing_snapshot) = existing_profile.capability_snapshot.as_ref()
        && existing_profile.family == family
        && existing_profile.role == role
        && existing_profile.deployment == deployment
        && existing_profile.endpoint == profile.endpoint
        && existing_profile.executable_path == profile.executable_path
        && existing_profile.working_directory == profile.working_directory
        && existing_profile.arguments == profile.arguments
        && existing_profile.credential_secret_id == secret_id
        && existing_profile.settings.0 == settings
        && existing_snapshot.model == snapshot.model
        && existing_snapshot.provider_version == snapshot.provider_version
        && existing_snapshot.endpoint_fingerprint == snapshot.endpoint_fingerprint
        && existing_snapshot.capabilities == snapshot.capabilities
    {
        snapshot.id = existing_snapshot.id;
    }
    let domain = ProviderProfile {
        id: provider_id,
        name: profile.name.clone(),
        family,
        role,
        deployment,
        endpoint: profile.endpoint.clone(),
        executable_path: profile.executable_path.clone(),
        working_directory: profile.working_directory.clone(),
        arguments: profile.arguments.clone(),
        environment_secret_ids: existing
            .as_ref()
            .map(|profile| profile.environment_secret_ids.clone())
            .unwrap_or_default(),
        credential_secret_id: secret_id,
        enabled: true,
        concurrency_override: existing
            .as_ref()
            .and_then(|profile| profile.concurrency_override),
        settings: SettingsMap(settings),
        capability_snapshot,
        created_at: existing.as_ref().map_or(now, |profile| profile.created_at),
        updated_at: now,
    };
    state
        .database
        .repositories()
        .providers
        .upsert(&domain)
        .await
        .map_err(|error| ServiceError::Storage(error.to_string()))
}

pub(super) fn validate_provider_location(
    mode: ProviderModeView,
    endpoint: Option<&str>,
    executable: Option<&str>,
    working_directory: Option<&str>,
    arguments: &[String],
) -> Result<(), ServiceError> {
    if let Some(endpoint) = endpoint {
        let url = url::Url::parse(endpoint).map_err(|_| {
            ServiceError::InvalidRequest("endpoint must be an absolute URL".to_owned())
        })?;
        if !matches!(url.scheme(), "http" | "https") || url.host_str().is_none() {
            return Err(ServiceError::InvalidRequest(
                "endpoint must be an absolute HTTP(S) URL".to_owned(),
            ));
        }
        if !url.username().is_empty() || url.password().is_some() {
            return Err(ServiceError::InvalidRequest(
                "credentials must not be embedded in provider URLs".to_owned(),
            ));
        }
        if url.query().is_some() || url.fragment().is_some() {
            return Err(ServiceError::InvalidRequest(
                "provider endpoints must not contain query parameters or fragments".to_owned(),
            ));
        }
    }
    if matches!(mode, ProviderModeView::ManagedChild) {
        let executable = executable.ok_or_else(|| {
            ServiceError::InvalidRequest(
                "managed providers require an absolute executable path".to_owned(),
            )
        })?;
        let executable_path = std::path::Path::new(executable);
        if executable.contains('\0') || !executable_path.is_absolute() {
            return Err(ServiceError::InvalidRequest(
                "managed providers require an absolute executable path".to_owned(),
            ));
        }
        let metadata = executable_path.metadata().map_err(|_| {
            ServiceError::InvalidRequest(
                "managed provider executable must be an existing file".to_owned(),
            )
        })?;
        if !metadata.is_file() {
            return Err(ServiceError::InvalidRequest(
                "managed provider executable must be an existing file".to_owned(),
            ));
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            if metadata.permissions().mode() & 0o111 == 0 {
                return Err(ServiceError::InvalidRequest(
                    "managed provider executable is not marked executable".to_owned(),
                ));
            }
        }
        if let Some(directory) = working_directory {
            let path = std::path::Path::new(directory);
            if directory.contains('\0') || !path.is_absolute() || !path.is_dir() {
                return Err(ServiceError::InvalidRequest(
                    "managed provider working directory must be an existing absolute directory"
                        .to_owned(),
                ));
            }
        }
        audiobookai_providers::validate_managed_process_arguments(arguments)
            .map_err(|error| ServiceError::InvalidRequest(error.to_string()))?;
    } else if executable.is_some() || working_directory.is_some() || !arguments.is_empty() {
        return Err(ServiceError::InvalidRequest(
            "only managed providers may define an executable, working directory, or arguments"
                .to_owned(),
        ));
    }
    Ok(())
}

pub(super) fn validate_optional_provider_value(
    field: &str,
    value: Option<&str>,
) -> Result<(), ServiceError> {
    if value.is_some_and(|value| value.trim().is_empty()) {
        return Err(ServiceError::InvalidRequest(format!(
            "{field} must be omitted or null instead of empty"
        )));
    }
    Ok(())
}

pub(super) fn validate_provider_sensitive_fields(
    kind: &ProviderKindView,
    role: ProviderRoleView,
    mode: ProviderModeView,
    model: Option<&str>,
    has_credential: bool,
) -> Result<(), ServiceError> {
    validate_provider_model_compatibility(kind, role, model)?;
    if has_credential && is_native_provider(kind, mode) {
        return Err(ServiceError::InvalidRequest(
            "native provider profiles must not be configured with credentials".to_owned(),
        ));
    }
    validate_provider_deployment(kind, mode)?;
    Ok(())
}

pub(super) fn validate_provider_context_window(
    role: ProviderRoleView,
    context_window_tokens: Option<u64>,
) -> Result<(), ServiceError> {
    let Some(tokens) = context_window_tokens else {
        return Ok(());
    };
    if !matches!(role, ProviderRoleView::Llm) {
        return Err(ServiceError::InvalidRequest(
            "only LLM provider profiles may define a context window".to_owned(),
        ));
    }
    if !(2_048..=2_097_152).contains(&tokens) {
        return Err(ServiceError::InvalidRequest(
            "provider context window must be between 2048 and 2097152 tokens".to_owned(),
        ));
    }
    Ok(())
}

/// `openai_tts` is retained only as a wire-compatible legacy input. New and edited profiles use
/// the single dual-role `OpenAI` kind, with `role` selecting the speech or Responses adapter.
pub(super) fn canonical_provider_kind(kind: ProviderKindView) -> ProviderKindView {
    if matches!(kind, ProviderKindView::OpenaiTts) {
        ProviderKindView::Openai
    } else {
        kind
    }
}

pub(super) fn validate_provider_deployment(
    kind: &ProviderKindView,
    mode: ProviderModeView,
) -> Result<(), ServiceError> {
    let native_kind = matches!(kind, ProviderKindView::NativeOs | ProviderKindView::Piper);
    if native_kind != matches!(mode, ProviderModeView::Native) {
        return Err(ServiceError::InvalidRequest(
            "native provider kinds and the native connection type must be selected together"
                .to_owned(),
        ));
    }
    Ok(())
}

pub(super) fn validate_provider_role(
    kind: &ProviderKindView,
    role: ProviderRoleView,
) -> Result<(), ServiceError> {
    let supported = match role {
        ProviderRoleView::Tts => matches!(
            kind,
            ProviderKindView::Elevenlabs
                | ProviderKindView::MlxAudio
                | ProviderKindView::Localai
                | ProviderKindView::AlltalkV2
                | ProviderKindView::Piper
                | ProviderKindView::NativeOs
                | ProviderKindView::OpenaiTts
                | ProviderKindView::Openai
                | ProviderKindView::Gemini
        ),
        ProviderRoleView::Llm => matches!(
            kind,
            ProviderKindView::Openai
                | ProviderKindView::OpenaiCompatible
                | ProviderKindView::Anthropic
                | ProviderKindView::Gemini
                | ProviderKindView::Qwen
                | ProviderKindView::Kimi
                | ProviderKindView::Moonshot
                | ProviderKindView::LmStudio
                | ProviderKindView::Ollama
        ),
    };
    supported.then_some(()).ok_or_else(|| {
        ServiceError::InvalidRequest(
            "the selected provider does not support this TTS/LLM role".to_owned(),
        )
    })
}

pub(super) fn validate_provider_model(model: Option<&str>) -> Result<(), ServiceError> {
    validate_optional_provider_value("model", model)?;
    if model.is_some_and(audiobookai_providers::contains_secret_shaped_value) {
        return Err(ServiceError::InvalidRequest(
            "provider model resembles sensitive credential material and cannot be stored"
                .to_owned(),
        ));
    }
    Ok(())
}

pub(super) fn validate_provider_model_compatibility(
    kind: &ProviderKindView,
    role: ProviderRoleView,
    model: Option<&str>,
) -> Result<(), ServiceError> {
    validate_provider_model(model)?;
    validate_provider_role(kind, role)?;
    let Some(model) = model else {
        return Ok(());
    };
    if provider_model_catalog_is_strict(kind, role)
        && !provider_model_is_compatible(kind, role, model)
    {
        return Err(ServiceError::InvalidRequest(
            "the selected model is not verified as compatible with this provider role".to_owned(),
        ));
    }
    Ok(())
}

pub(super) fn provider_model_catalog_is_strict(
    kind: &ProviderKindView,
    role: ProviderRoleView,
) -> bool {
    matches!(
        (kind, role),
        (
            ProviderKindView::Openai
                | ProviderKindView::OpenaiTts
                | ProviderKindView::Piper
                | ProviderKindView::Gemini,
            ProviderRoleView::Tts
        ) | (ProviderKindView::Openai, ProviderRoleView::Llm)
    )
}

pub(super) fn provider_model_is_compatible(
    kind: &ProviderKindView,
    role: ProviderRoleView,
    model: &str,
) -> bool {
    match (kind, role) {
        (ProviderKindView::Openai | ProviderKindView::OpenaiTts, ProviderRoleView::Tts) => {
            audiobookai_providers::adapters::is_openai_tts_model_id(model)
        }
        (ProviderKindView::Gemini, ProviderRoleView::Tts) => {
            audiobookai_providers::adapters::is_gemini_tts_model_id(model)
        }
        (ProviderKindView::Openai, ProviderRoleView::Llm) => {
            audiobookai_providers::adapters::is_openai_responses_model_id(model)
        }
        _ => true,
    }
}

pub(super) fn is_native_provider(kind: &ProviderKindView, mode: ProviderModeView) -> bool {
    matches!(kind, ProviderKindView::NativeOs | ProviderKindView::Piper)
        || matches!(mode, ProviderModeView::Native)
}

pub(super) fn default_capabilities(
    kind: &ProviderKindView,
    role: ProviderRoleView,
    mode: ProviderModeView,
) -> ProviderCapabilitiesView {
    let tts = matches!(role, ProviderRoleView::Tts);
    let character_detection = matches!(role, ProviderRoleView::Llm);
    let provider_model_library = matches!(
        kind,
        ProviderKindView::LmStudio | ProviderKindView::Ollama | ProviderKindView::Localai
    );
    let provider_model_runtime = matches!(
        kind,
        ProviderKindView::LmStudio | ProviderKindView::Ollama | ProviderKindView::Localai
    );
    ProviderCapabilitiesView {
        tts,
        character_detection,
        streaming: tts
            && matches!(
                kind,
                ProviderKindView::Elevenlabs
                    | ProviderKindView::MlxAudio
                    | ProviderKindView::Localai
                    | ProviderKindView::OpenaiTts
                    | ProviderKindView::Openai
                    | ProviderKindView::Gemini
            ),
        voice_cloning: tts && matches!(kind, ProviderKindView::Elevenlabs),
        pronunciation: tts && matches!(kind, ProviderKindView::Elevenlabs),
        process_control: matches!(mode, ProviderModeView::ManagedChild),
        model_control: provider_model_library,
        model_list: provider_model_library,
        model_download: provider_model_library,
        // LocalAI's adapter checks its authenticated /system view and fails closed unless the
        // target's loaded state can be proven before the provider deletion endpoint is called.
        model_delete: matches!(kind, ProviderKindView::Ollama | ProviderKindView::Localai),
        model_load: provider_model_runtime,
        model_unload: provider_model_runtime,
        // The current provider trait's default switch operation is only an alias for load. Do
        // not advertise a true atomic switch until an adapter implements that contract.
        model_switch: false,
        temperature: if character_detection {
            match kind {
                ProviderKindView::Openai => "nullable",
                ProviderKindView::OpenaiCompatible
                | ProviderKindView::Anthropic
                | ProviderKindView::Gemini
                | ProviderKindView::Qwen
                | ProviderKindView::Kimi
                | ProviderKindView::Moonshot
                | ProviderKindView::LmStudio
                | ProviderKindView::Ollama => "number",
                _ => "unsupported",
            }
        } else {
            "unsupported"
        }
        .to_owned(),
        reasoning: if character_detection {
            match kind {
                ProviderKindView::Openai | ProviderKindView::Ollama => {
                    vec!["disabled".to_owned(), "effort".to_owned()]
                }
                ProviderKindView::Anthropic => vec![
                    "disabled".to_owned(),
                    "adaptive".to_owned(),
                    "token_budget".to_owned(),
                ],
                ProviderKindView::Gemini => {
                    vec!["disabled".to_owned(), "token_budget".to_owned()]
                }
                ProviderKindView::Qwen | ProviderKindView::Kimi => {
                    vec!["disabled".to_owned()]
                }
                _ => Vec::new(),
            }
        } else {
            Vec::new()
        },
        max_concurrency: Some(1),
        model_performance: default_model_performance(kind, role),
    }
}

pub(super) fn default_model_performance(
    kind: &ProviderKindView,
    role: ProviderRoleView,
) -> Vec<audiobookai_core::ModelPerformanceCapabilities> {
    use audiobookai_core::{
        ModelPerformanceCapabilities, PerformanceCapabilities, PerformanceRange,
    };

    if matches!(role, ProviderRoleView::Tts)
        && matches!(kind, ProviderKindView::Openai | ProviderKindView::OpenaiTts)
    {
        return audiobookai_providers::adapters::openai_tts_model_performance_capabilities();
    }
    if matches!(role, ProviderRoleView::Tts) && matches!(kind, ProviderKindView::Gemini) {
        return audiobookai_providers::adapters::gemini_tts_model_performance_capabilities();
    }
    if !matches!(kind, ProviderKindView::Elevenlabs) {
        return Vec::new();
    }
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

pub(crate) fn provider_capabilities_are_fresh(profile: &ProviderProfileView) -> bool {
    profile
        .capability_updated_at
        .is_some_and(|observed_at| observed_at + ChronoDuration::hours(24) > Utc::now())
}

pub(crate) fn validate_billable_tts_provider_readiness(
    profile: &ProviderProfileView,
) -> Result<(), ServiceError> {
    if !matches!(profile.role, ProviderRoleView::Tts)
        || !provider_capabilities_are_fresh(profile)
        || !profile
            .capabilities
            .as_ref()
            .is_some_and(|capabilities| capabilities.tts)
    {
        return Err(ServiceError::Conflict(format!(
            "refresh provider '{}' to verify its current TTS capability",
            profile.name
        )));
    }
    if !matches!(profile.status, ProviderStatusView::Online) {
        return Err(ServiceError::Conflict(format!(
            "provider '{}' must be online before paid synthesis",
            profile.name
        )));
    }
    if matches!(profile.mode, ProviderModeView::CloudRemote) && !profile.credential_configured {
        return Err(ServiceError::Conflict(format!(
            "configure a credential for cloud provider '{}' before paid synthesis",
            profile.name
        )));
    }
    Ok(())
}
