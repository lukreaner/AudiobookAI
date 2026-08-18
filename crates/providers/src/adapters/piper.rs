use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
    sync::Arc,
    time::Duration,
};

use async_trait::async_trait;
use bytes::Bytes;
use serde::Deserialize;

use super::{
    NativeCapture, NativeCommand, NativeCommandArgument, NativeCommandOutput, NativeCommandRunner,
};
use crate::{
    AudioFormat, Model, ParameterSupport, ProviderCapabilities, ProviderDescriptor, ProviderError,
    ProviderHealth, ProviderId, ProviderKind, ProviderUsage, Result, SynthesisRequest,
    SynthesisResponse, TtsProvider, UsageSource, Voice,
};

pub const PIPER_VERSION: &str = "1.2.0";

const MAX_VOICE_ID_BYTES: usize = 128;
const MAX_MODEL_BYTES: u64 = 2 * 1024 * 1024 * 1024;
const MAX_CONFIG_BYTES: u64 = 1024 * 1024;

/// Filesystem locations used by the direct Piper CLI adapter.
///
/// `voices_dir` contains one direct child directory per voice. A voice named
/// `de_DE-thorsten-medium` is represented by the exact pair
/// `de_DE-thorsten-medium/{de_DE-thorsten-medium.onnx,de_DE-thorsten-medium.onnx.json}`.
/// `selected_voice_id` is the only model this provider connection may discover
/// or synthesize with.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PiperTtsConfig {
    pub executable: PathBuf,
    pub voices_dir: PathBuf,
    pub selected_voice_id: String,
}

impl PiperTtsConfig {
    pub fn new(
        executable: PathBuf,
        voices_dir: PathBuf,
        selected_voice_id: impl Into<String>,
    ) -> Result<Self> {
        if !executable.is_absolute() {
            return Err(ProviderError::Configuration(
                "Piper executable path must be absolute".to_owned(),
            ));
        }
        if !voices_dir.is_absolute() {
            return Err(ProviderError::Configuration(
                "Piper voices directory must be absolute".to_owned(),
            ));
        }
        let selected_voice_id = selected_voice_id.into();
        validate_voice_id(&selected_voice_id)?;
        Ok(Self {
            executable,
            voices_dir,
            selected_voice_id,
        })
    }
}

#[derive(Clone, Debug)]
pub struct PiperTtsProvider {
    config: PiperTtsConfig,
    runner: Arc<dyn NativeCommandRunner>,
    descriptor: ProviderDescriptor,
    capabilities: ProviderCapabilities,
}

impl PiperTtsProvider {
    pub fn new(config: PiperTtsConfig, runner: Arc<dyn NativeCommandRunner>) -> Result<Self> {
        Ok(Self {
            config,
            runner,
            descriptor: ProviderDescriptor {
                id: ProviderId::new("piper")?,
                display_name: "Piper (local)".to_owned(),
                kind: ProviderKind::Native,
                endpoint_family: "piper-cli-v1".to_owned(),
            },
            capabilities: ProviderCapabilities {
                model_discovery: true,
                max_concurrency: 1,
                temperature: ParameterSupport::Unsupported,
                ..ProviderCapabilities::default()
            },
        })
    }

    fn help_command(&self) -> NativeCommand {
        NativeCommand {
            executable: self.config.executable.clone(),
            arguments: vec![literal("--help")],
            stdin: Bytes::new(),
            capture: NativeCapture::None,
            timeout: Duration::from_secs(10),
        }
    }

    fn espeak_data_path(&self) -> Result<PathBuf> {
        let executable_parent = self.config.executable.parent().ok_or_else(|| {
            ProviderError::Configuration("Piper executable has no parent directory".to_owned())
        })?;
        let path = executable_parent.join("espeak-ng-data");
        let metadata = std::fs::symlink_metadata(&path).map_err(|_| {
            ProviderError::Configuration(
                "Piper's bundled eSpeak NG data directory is missing".to_owned(),
            )
        })?;
        if !metadata.is_dir() || metadata.file_type().is_symlink() {
            return Err(ProviderError::Configuration(
                "Piper's bundled eSpeak NG data path is unsafe".to_owned(),
            ));
        }
        let executable_parent = executable_parent.canonicalize().map_err(|error| {
            ProviderError::Configuration(format!("Piper runtime path is invalid: {error}"))
        })?;
        let canonical = path.canonicalize().map_err(|error| {
            ProviderError::Configuration(format!("Piper voice data path is invalid: {error}"))
        })?;
        if canonical.parent() != Some(executable_parent.as_path()) {
            return Err(ProviderError::Configuration(
                "Piper's bundled eSpeak NG data path escaped its runtime".to_owned(),
            ));
        }
        Ok(canonical)
    }

    fn discover_selected(&self) -> Result<Vec<InstalledVoice>> {
        let root_metadata = match std::fs::symlink_metadata(&self.config.voices_dir) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(error) => return Err(ProviderError::Configuration(error.to_string())),
        };
        if !root_metadata.is_dir() || root_metadata.file_type().is_symlink() {
            return Err(ProviderError::Configuration(
                "Piper voices path must be a regular directory".to_owned(),
            ));
        }

        let selected = self.config.voices_dir.join(&self.config.selected_voice_id);
        match std::fs::symlink_metadata(selected) {
            Ok(_) => self
                .resolve_voice(&self.config.selected_voice_id)
                .map(|voice| vec![voice]),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(Vec::new()),
            Err(error) => Err(ProviderError::Configuration(error.to_string())),
        }
    }

    fn resolve_voice(&self, id: &str) -> Result<InstalledVoice> {
        validate_voice_id(id)?;
        let root_metadata = std::fs::symlink_metadata(&self.config.voices_dir).map_err(|_| {
            ProviderError::Configuration("Piper voices directory is missing".to_owned())
        })?;
        if !root_metadata.is_dir() || root_metadata.file_type().is_symlink() {
            return Err(ProviderError::Configuration(
                "Piper voices path must be a regular directory".to_owned(),
            ));
        }
        let canonical_root = self.config.voices_dir.canonicalize().map_err(|error| {
            ProviderError::Configuration(format!("Piper voices path is invalid: {error}"))
        })?;
        let voice_dir = self.config.voices_dir.join(id);
        require_direct_directory(&voice_dir, &canonical_root)?;

        let model = voice_dir.join(format!("{id}.onnx"));
        let config = voice_dir.join(format!("{id}.onnx.json"));
        let model_bytes = require_direct_file(&model, &voice_dir, MAX_MODEL_BYTES, "model")?;
        let config_bytes = require_direct_file(&config, &voice_dir, MAX_CONFIG_BYTES, "config")?;
        let parsed = parse_voice_config(&config, config_bytes)?;

        Ok(InstalledVoice {
            id: id.to_owned(),
            model,
            config,
            model_bytes,
            parsed,
        })
    }

    fn synthesis_command(
        &self,
        request: &SynthesisRequest,
        voice: &InstalledVoice,
    ) -> Result<NativeCommand> {
        if request.voice != self.config.selected_voice_id
            || voice.id != self.config.selected_voice_id
        {
            return Err(ProviderError::Configuration(
                "Piper may only use the exact model selected for this provider connection"
                    .to_owned(),
            ));
        }
        request.validate_performance(&self.capabilities, &voice.id)?;
        if request.format != AudioFormat::Wav {
            return Err(ProviderError::Unsupported {
                feature: "Piper output other than WAV",
            });
        }
        if request.text.trim().is_empty() {
            return Err(ProviderError::Configuration("text is required".to_owned()));
        }
        if let Some(model) = request.model.as_deref()
            && model != self.config.selected_voice_id
        {
            return Err(ProviderError::Configuration(
                "Piper model must match the exact model selected for this provider connection"
                    .to_owned(),
            ));
        }
        if !request.options.is_empty() {
            return Err(ProviderError::Unsupported {
                feature: "untyped Piper voice options",
            });
        }
        if !request.pronunciation_dictionary_ids.is_empty() {
            return Err(ProviderError::Unsupported {
                feature: "pronunciation dictionaries in Piper",
            });
        }
        let espeak_data = self.espeak_data_path()?;
        Ok(NativeCommand {
            executable: self.config.executable.clone(),
            arguments: vec![
                literal("--model"),
                literal(voice.model.to_string_lossy()),
                literal("--config"),
                literal(voice.config.to_string_lossy()),
                literal("--espeak_data"),
                literal(espeak_data.to_string_lossy()),
                literal("--output_file"),
                NativeCommandArgument::OutputFile,
            ],
            stdin: Bytes::copy_from_slice(request.text.as_bytes()),
            capture: NativeCapture::OutputFile { suffix: ".wav" },
            timeout: Duration::from_secs(300),
        })
    }
}

#[async_trait]
impl TtsProvider for PiperTtsProvider {
    fn descriptor(&self) -> &ProviderDescriptor {
        &self.descriptor
    }

    fn capabilities(&self) -> &ProviderCapabilities {
        &self.capabilities
    }

    async fn health(&self) -> Result<ProviderHealth> {
        self.runner.run(self.help_command()).await?;
        self.espeak_data_path()?;
        let voices = self.discover_selected()?;
        Ok(ProviderHealth {
            available: !voices.is_empty(),
            version: Some(PIPER_VERSION.to_owned()),
            message: voices
                .is_empty()
                .then(|| "Piper is installed, but no compatible voice is installed".to_owned()),
        })
    }

    async fn discover_voices(&self) -> Result<Vec<Voice>> {
        self.discover_selected()?
            .into_iter()
            .map(|voice| Ok(voice.as_voice()))
            .collect()
    }

    async fn discover_models(&self) -> Result<Vec<Model>> {
        self.discover_selected()?
            .into_iter()
            .map(|voice| Ok(voice.as_model()))
            .collect()
    }

    async fn synthesize(&self, request: SynthesisRequest) -> Result<SynthesisResponse> {
        if request.voice != self.config.selected_voice_id {
            return Err(ProviderError::Configuration(
                "Piper may only use the exact model selected for this provider connection"
                    .to_owned(),
            ));
        }
        let voice = self.resolve_voice(&self.config.selected_voice_id)?;
        let characters = u64::try_from(request.text.chars().count()).ok();
        let command = self.synthesis_command(&request, &voice)?;
        let NativeCommandOutput { artifact, .. } = self.runner.run(command).await?;
        let audio = artifact.ok_or_else(|| {
            ProviderError::InvalidResponse("Piper did not create its WAV output file".to_owned())
        })?;
        if audio.len() < 12 || &audio[..4] != b"RIFF" || &audio[8..12] != b"WAVE" {
            return Err(ProviderError::InvalidResponse(
                "Piper did not produce a WAV stream".to_owned(),
            ));
        }
        Ok(SynthesisResponse {
            audio,
            content_type: "audio/wav".to_owned(),
            usage: ProviderUsage {
                source: UsageSource::Estimated,
                characters,
                ..ProviderUsage::default()
            },
        })
    }
}

fn literal(value: impl Into<String>) -> NativeCommandArgument {
    NativeCommandArgument::Literal(value.into())
}

fn validate_voice_id(id: &str) -> Result<()> {
    let mut bytes = id.bytes();
    let first = bytes.next().ok_or_else(|| {
        ProviderError::Configuration("Piper voice id must not be empty".to_owned())
    })?;
    if id.len() > MAX_VOICE_ID_BYTES
        || !first.is_ascii_alphanumeric()
        || !bytes.all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-'))
    {
        return Err(ProviderError::Configuration(
            "Piper voice id may contain only ASCII letters, digits, '-' and '_'".to_owned(),
        ));
    }
    Ok(())
}

fn require_direct_directory(path: &Path, canonical_parent: &Path) -> Result<()> {
    let metadata = std::fs::symlink_metadata(path).map_err(|_| {
        ProviderError::Configuration("selected Piper voice is not installed".to_owned())
    })?;
    if !metadata.is_dir() || metadata.file_type().is_symlink() {
        return Err(ProviderError::Configuration(
            "Piper voice path must be a regular directory".to_owned(),
        ));
    }
    let canonical = path.canonicalize().map_err(|error| {
        ProviderError::Configuration(format!("Piper voice path is invalid: {error}"))
    })?;
    if canonical.parent() != Some(canonical_parent) {
        return Err(ProviderError::Configuration(
            "Piper voice path escaped the managed voices directory".to_owned(),
        ));
    }
    Ok(())
}

fn require_direct_file(path: &Path, parent: &Path, maximum: u64, kind: &str) -> Result<u64> {
    let metadata = std::fs::symlink_metadata(path)
        .map_err(|_| ProviderError::Configuration(format!("Piper voice {kind} file is missing")))?;
    if !metadata.is_file() || metadata.file_type().is_symlink() {
        return Err(ProviderError::Configuration(format!(
            "Piper voice {kind} path must be a regular file"
        )));
    }
    if metadata.len() == 0 || metadata.len() > maximum {
        return Err(ProviderError::Configuration(format!(
            "Piper voice {kind} file has an invalid size"
        )));
    }
    let canonical_parent = parent.canonicalize().map_err(|error| {
        ProviderError::Configuration(format!("Piper voice directory is invalid: {error}"))
    })?;
    let canonical = path.canonicalize().map_err(|error| {
        ProviderError::Configuration(format!("Piper voice {kind} path is invalid: {error}"))
    })?;
    if canonical.parent() != Some(canonical_parent.as_path()) {
        return Err(ProviderError::Configuration(format!(
            "Piper voice {kind} path escaped its voice directory"
        )));
    }
    Ok(metadata.len())
}

#[derive(Debug, Deserialize)]
struct PiperVoiceConfigFile {
    audio: PiperAudioConfig,
    #[serde(default)]
    language: Option<PiperLanguageConfig>,
    #[serde(default)]
    dataset: Option<String>,
    #[serde(default = "default_speaker_count")]
    num_speakers: u16,
    #[serde(default)]
    speaker_id_map: BTreeMap<String, u16>,
}

#[derive(Debug, Deserialize)]
struct PiperAudioConfig {
    sample_rate: u32,
    #[serde(default)]
    quality: Option<String>,
}

#[derive(Debug, Deserialize)]
struct PiperLanguageConfig {
    code: String,
}

const fn default_speaker_count() -> u16 {
    1
}

#[derive(Clone, Debug)]
struct ParsedVoiceConfig {
    sample_rate: u32,
    quality: Option<String>,
    language: Option<String>,
    dataset: Option<String>,
    speakers: u16,
}

fn parse_voice_config(path: &Path, size: u64) -> Result<ParsedVoiceConfig> {
    let capacity = usize::try_from(size)
        .map_err(|_| ProviderError::Configuration("Piper voice config is too large".to_owned()))?;
    let bytes = std::fs::read(path).map_err(|error| {
        ProviderError::Configuration(format!("Piper config read failed: {error}"))
    })?;
    if bytes.len() != capacity {
        return Err(ProviderError::Configuration(
            "Piper voice config changed while it was being read".to_owned(),
        ));
    }
    let config: PiperVoiceConfigFile = serde_json::from_slice(&bytes).map_err(|error| {
        ProviderError::Configuration(format!("Piper voice config is invalid: {error}"))
    })?;
    if config.audio.sample_rate < 8_000
        || config.audio.sample_rate > 192_000
        || config
            .speaker_id_map
            .values()
            .any(|speaker| *speaker >= config.num_speakers)
    {
        return Err(ProviderError::Configuration(
            "Piper voice config contains invalid audio or speaker metadata".to_owned(),
        ));
    }
    if config.num_speakers != 1 {
        return Err(ProviderError::Configuration(
            "Piper multi-speaker models are unsupported until a speaker is selected explicitly"
                .to_owned(),
        ));
    }
    if config
        .language
        .as_ref()
        .is_some_and(|language| language.code.trim().is_empty() || language.code.len() > 32)
        || config
            .audio
            .quality
            .as_ref()
            .is_some_and(|quality| quality.trim().is_empty() || quality.len() > 32)
    {
        return Err(ProviderError::Configuration(
            "Piper voice config contains invalid catalog metadata".to_owned(),
        ));
    }
    Ok(ParsedVoiceConfig {
        sample_rate: config.audio.sample_rate,
        quality: config.audio.quality,
        language: config.language.map(|language| language.code),
        dataset: config.dataset,
        speakers: config.num_speakers,
    })
}

#[derive(Clone, Debug)]
struct InstalledVoice {
    id: String,
    model: PathBuf,
    config: PathBuf,
    model_bytes: u64,
    parsed: ParsedVoiceConfig,
}

impl InstalledVoice {
    fn metadata(&self) -> BTreeMap<String, String> {
        let mut metadata = BTreeMap::from([
            ("format".to_owned(), "onnx".to_owned()),
            ("sampleRate".to_owned(), self.parsed.sample_rate.to_string()),
            ("speakers".to_owned(), self.parsed.speakers.to_string()),
            ("sizeBytes".to_owned(), self.model_bytes.to_string()),
        ]);
        if let Some(quality) = &self.parsed.quality {
            metadata.insert("quality".to_owned(), quality.clone());
        }
        if let Some(language) = &self.parsed.language {
            metadata.insert("language".to_owned(), language.clone());
        }
        if let Some(dataset) = &self.parsed.dataset {
            metadata.insert("dataset".to_owned(), dataset.clone());
        }
        metadata
    }

    fn as_voice(&self) -> Voice {
        Voice {
            id: self.id.clone(),
            name: self.id.clone(),
            language: self.parsed.language.clone(),
            owned_clone: false,
            metadata: self.metadata(),
        }
    }

    fn as_model(&self) -> Model {
        Model {
            id: self.id.clone(),
            name: self.id.clone(),
            metadata: self.metadata(),
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use super::*;
    use audiobookai_core::PerformanceSettings;
    use uuid::Uuid;

    const SELECTED_VOICE: &str = "de_DE-thorsten-medium";

    #[derive(Debug)]
    struct FixtureRunner {
        output: NativeCommandOutput,
        commands: Mutex<Vec<NativeCommand>>,
    }

    #[async_trait]
    impl NativeCommandRunner for FixtureRunner {
        async fn run(&self, command: NativeCommand) -> Result<NativeCommandOutput> {
            self.commands.lock().unwrap().push(command);
            Ok(self.output.clone())
        }
    }

    fn runtime() -> (tempfile::TempDir, PathBuf, PathBuf) {
        let temporary = tempfile::TempDir::new().unwrap();
        let runtime = temporary.path().join("engine").join("piper");
        std::fs::create_dir_all(runtime.join("espeak-ng-data")).unwrap();
        let executable = runtime.join("piper");
        std::fs::write(&executable, b"fixture").unwrap();
        let voices = temporary.path().join("voices");
        std::fs::create_dir(&voices).unwrap();
        (temporary, executable, voices)
    }

    fn add_voice(voices: &Path, id: &str, speakers: u16) {
        let root = voices.join(id);
        std::fs::create_dir(&root).unwrap();
        std::fs::write(root.join(format!("{id}.onnx")), b"onnx").unwrap();
        std::fs::write(
            root.join(format!("{id}.onnx.json")),
            format!(
                r#"{{"audio":{{"sample_rate":22050,"quality":"medium"}},"language":{{"code":"de_DE"}},"dataset":"thorsten","num_speakers":{speakers},"speaker_id_map":{{"neutral":0}}}}"#
            ),
        )
        .unwrap();
    }

    fn provider_for(
        executable: PathBuf,
        voices: PathBuf,
        selected_voice_id: &str,
    ) -> (PiperTtsProvider, Arc<FixtureRunner>) {
        let runner = Arc::new(FixtureRunner {
            output: NativeCommandOutput {
                stdout: Bytes::new(),
                artifact: Some(Bytes::from_static(b"RIFF\0\0\0\0WAVEfixture")),
            },
            commands: Mutex::new(Vec::new()),
        });
        let provider = PiperTtsProvider::new(
            PiperTtsConfig::new(executable, voices, selected_voice_id).unwrap(),
            runner.clone(),
        )
        .unwrap();
        (provider, runner)
    }

    fn provider(executable: PathBuf, voices: PathBuf) -> (PiperTtsProvider, Arc<FixtureRunner>) {
        provider_for(executable, voices, SELECTED_VOICE)
    }

    fn request(voice: &str) -> SynthesisRequest {
        SynthesisRequest {
            request_id: Uuid::new_v4(),
            text: "Hallo Welt".to_owned(),
            model: Some(voice.to_owned()),
            voice: voice.to_owned(),
            format: AudioFormat::Wav,
            performance: PerformanceSettings::default(),
            options: BTreeMap::new(),
            pronunciation_dictionary_ids: Vec::new(),
        }
    }

    #[test]
    fn paths_must_be_absolute() {
        assert!(PiperTtsConfig::new("piper".into(), "/voices".into(), SELECTED_VOICE).is_err());
        assert!(PiperTtsConfig::new("/piper".into(), "voices".into(), SELECTED_VOICE).is_err());
        assert!(PiperTtsConfig::new("/piper".into(), "/voices".into(), "../escape").is_err());
    }

    #[test]
    fn voice_ids_cannot_address_paths() {
        for invalid in [
            "",
            ".hidden",
            "../escape",
            "a/b",
            "a\\b",
            "ümlaut",
            "a.json",
        ] {
            assert!(validate_voice_id(invalid).is_err(), "accepted {invalid:?}");
        }
        assert!(validate_voice_id("de_DE-thorsten-medium").is_ok());
    }

    #[tokio::test]
    async fn discovers_only_the_exact_selected_model() {
        let (_temporary, executable, voices) = runtime();
        add_voice(&voices, "de_DE-other-medium", 1);
        std::fs::create_dir(voices.join("unrelated-incomplete")).unwrap();
        add_voice(&voices, SELECTED_VOICE, 1);
        let (provider, _) = provider(executable, voices);

        let discovered = provider.discover_models().await.unwrap();

        assert_eq!(discovered.len(), 1);
        assert_eq!(discovered[0].id, SELECTED_VOICE);
        assert_eq!(discovered[0].metadata.get("speakers").unwrap(), "1");
        assert_eq!(discovered[0].metadata.get("language").unwrap(), "de_DE");
    }

    #[tokio::test]
    async fn selected_multi_speaker_model_is_rejected() {
        let (_temporary, executable, voices) = runtime();
        add_voice(&voices, "de_DE-thorsten-emotional-medium", 8);
        let (provider, _) = provider_for(executable, voices, "de_DE-thorsten-emotional-medium");

        assert!(provider.discover_models().await.is_err());
    }

    #[tokio::test]
    async fn incomplete_voice_directory_is_not_silently_accepted() {
        let (_temporary, executable, voices) = runtime();
        std::fs::create_dir(voices.join("de_DE-incomplete-medium")).unwrap();
        let (provider, _) = provider_for(executable, voices, "de_DE-incomplete-medium");

        assert!(provider.discover_voices().await.is_err());
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn symlinked_model_is_rejected_even_when_its_target_exists() {
        use std::os::unix::fs::symlink;

        let (_temporary, executable, voices) = runtime();
        add_voice(&voices, "de_DE-thorsten-medium", 1);
        let root = voices.join("de_DE-thorsten-medium");
        let outside = voices.join("outside.onnx");
        std::fs::write(&outside, b"outside").unwrap();
        std::fs::remove_file(root.join("de_DE-thorsten-medium.onnx")).unwrap();
        symlink(&outside, root.join("de_DE-thorsten-medium.onnx")).unwrap();
        let (provider, _) = provider(executable, voices);

        assert!(provider.discover_models().await.is_err());
    }

    #[tokio::test]
    async fn synthesis_uses_stdin_exact_paths_and_an_output_file() {
        let (_temporary, executable, voices) = runtime();
        add_voice(&voices, "de_DE-thorsten-medium", 1);
        let (provider, runner) = provider(executable, voices.clone());

        let response = provider
            .synthesize(request("de_DE-thorsten-medium"))
            .await
            .unwrap();

        assert_eq!(response.content_type, "audio/wav");
        let commands = runner.commands.lock().unwrap();
        let command = commands.last().unwrap();
        assert_eq!(command.stdin, Bytes::from_static(b"Hallo Welt"));
        assert_eq!(
            command.capture,
            NativeCapture::OutputFile { suffix: ".wav" }
        );
        assert_eq!(
            command.arguments,
            vec![
                literal("--model"),
                literal(
                    voices
                        .join("de_DE-thorsten-medium/de_DE-thorsten-medium.onnx")
                        .to_string_lossy(),
                ),
                literal("--config"),
                literal(
                    voices
                        .join("de_DE-thorsten-medium/de_DE-thorsten-medium.onnx.json")
                        .to_string_lossy(),
                ),
                literal("--espeak_data"),
                literal(
                    command
                        .executable
                        .parent()
                        .unwrap()
                        .join("espeak-ng-data")
                        .to_string_lossy(),
                ),
                literal("--output_file"),
                NativeCommandArgument::OutputFile,
            ]
        );
    }

    #[tokio::test]
    async fn no_stdout_or_model_fallback_is_allowed() {
        let (_temporary, executable, voices) = runtime();
        add_voice(&voices, "de_DE-thorsten-medium", 1);
        let runner = Arc::new(FixtureRunner {
            output: NativeCommandOutput {
                stdout: Bytes::from_static(b"RIFF\0\0\0\0WAVEstdout"),
                artifact: None,
            },
            commands: Mutex::new(Vec::new()),
        });
        let provider = PiperTtsProvider::new(
            PiperTtsConfig::new(executable, voices, SELECTED_VOICE).unwrap(),
            runner,
        )
        .unwrap();

        assert!(
            provider
                .synthesize(request("de_DE-thorsten-medium"))
                .await
                .is_err()
        );
    }

    #[tokio::test]
    async fn request_model_must_exactly_match_the_selected_voice() {
        let (_temporary, executable, voices) = runtime();
        add_voice(&voices, "de_DE-thorsten-medium", 1);
        let (provider, _) = provider(executable, voices);
        let mut mismatched = request("de_DE-thorsten-medium");
        mismatched.model = Some("some-other-model".to_owned());

        assert!(provider.synthesize(mismatched).await.is_err());
    }

    #[tokio::test]
    async fn request_voice_must_exactly_match_the_connection_selection() {
        let (_temporary, executable, voices) = runtime();
        add_voice(&voices, SELECTED_VOICE, 1);
        add_voice(&voices, "de_DE-other-medium", 1);
        let (provider, runner) = provider(executable, voices);

        assert!(
            provider
                .synthesize(request("de_DE-other-medium"))
                .await
                .is_err()
        );
        assert!(runner.commands.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn health_requires_both_a_working_cli_and_an_installed_voice() {
        let (_temporary, executable, voices) = runtime();
        let (provider, runner) = provider(executable, voices.clone());

        assert!(!provider.health().await.unwrap().available);
        add_voice(&voices, "de_DE-other-medium", 1);
        assert!(!provider.health().await.unwrap().available);
        add_voice(&voices, SELECTED_VOICE, 1);
        assert!(provider.health().await.unwrap().available);
        assert_eq!(
            runner.commands.lock().unwrap()[0].arguments,
            vec![literal("--help")]
        );
    }
}
