use std::{fmt, path::PathBuf, process::Stdio, sync::Arc, time::Duration};

use async_trait::async_trait;
use bytes::Bytes;
use tokio::{io::AsyncWriteExt, process::Command};

use crate::{
    AudioFormat, Model, ParameterSupport, ProviderCapabilities, ProviderDescriptor, ProviderError,
    ProviderHealth, ProviderId, ProviderKind, ProviderUsage, Result, SynthesisRequest,
    SynthesisResponse, TtsProvider, UsageSource, Voice,
};

const SAFE_NATIVE_ENVIRONMENT: &[&str] = &[
    "ComSpec",
    "LANG",
    "LC_ALL",
    "LC_CTYPE",
    "PATH",
    "PATHEXT",
    "SystemRoot",
    "TEMP",
    "TMP",
    "TMPDIR",
    "WINDIR",
];

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum NativePlatform {
    MacOs,
    Windows,
    Linux,
}

impl NativePlatform {
    pub const fn current() -> Option<Self> {
        if cfg!(target_os = "macos") {
            Some(Self::MacOs)
        } else if cfg!(target_os = "windows") {
            Some(Self::Windows)
        } else if cfg!(target_os = "linux") {
            Some(Self::Linux)
        } else {
            None
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct NativeTtsConfig {
    pub platform: NativePlatform,
    /// Absolute path to `say`, PowerShell, or the packaged `espeak-ng` sidecar.
    pub executable: PathBuf,
}

impl NativeTtsConfig {
    pub fn for_current_os(executable: PathBuf) -> Result<Self> {
        let platform = NativePlatform::current().ok_or(ProviderError::Unsupported {
            feature: "native TTS on this operating system",
        })?;
        Self::new(platform, executable)
    }

    pub fn new(platform: NativePlatform, executable: PathBuf) -> Result<Self> {
        if !executable.is_absolute() {
            return Err(ProviderError::Configuration(
                "native TTS executable path must be absolute".to_owned(),
            ));
        }
        Ok(Self {
            platform,
            executable,
        })
    }

    fn espeak_data_parent(&self) -> Option<PathBuf> {
        if self.platform != NativePlatform::Linux {
            return None;
        }
        let bin = self.executable.parent()?;
        if bin.file_name()? != "bin" {
            return None;
        }
        let share = bin.parent()?.join("share");
        share.join("espeak-ng-data").is_dir().then_some(share)
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum NativeCommandArgument {
    Literal(String),
    /// Replaced by the runner with a same-process temporary file path.
    OutputFile,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum NativeCapture {
    None,
    OutputFile { suffix: &'static str },
}

#[derive(Clone)]
pub struct NativeCommand {
    pub executable: PathBuf,
    pub arguments: Vec<NativeCommandArgument>,
    pub stdin: Bytes,
    pub capture: NativeCapture,
    pub timeout: Duration,
}

impl fmt::Debug for NativeCommand {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("NativeCommand")
            .field("executable", &self.executable)
            .field("arguments", &self.arguments)
            .field("stdin_len", &self.stdin.len())
            .field("capture", &self.capture)
            .field("timeout", &self.timeout)
            .finish()
    }
}

#[derive(Clone, Debug)]
pub struct NativeCommandOutput {
    pub stdout: Bytes,
    pub artifact: Option<Bytes>,
}

#[async_trait]
pub trait NativeCommandRunner: fmt::Debug + Send + Sync {
    async fn run(&self, command: NativeCommand) -> Result<NativeCommandOutput>;
}

/// Command runner used in packaged builds. It invokes an absolute executable directly and never a
/// shell; book text is provided over stdin rather than exposed in arguments.
#[derive(Clone, Debug, Default)]
pub struct TokioNativeCommandRunner;

#[async_trait]
impl NativeCommandRunner for TokioNativeCommandRunner {
    async fn run(&self, request: NativeCommand) -> Result<NativeCommandOutput> {
        if !request.executable.is_absolute() || !request.executable.is_file() {
            return Err(ProviderError::Configuration(
                "native TTS executable must be an existing absolute file".to_owned(),
            ));
        }
        let temporary = match request.capture {
            NativeCapture::None => None,
            NativeCapture::OutputFile { suffix } => Some(
                tempfile::Builder::new()
                    .prefix("audiobookai-native-tts-")
                    .suffix(suffix)
                    .tempfile()
                    .map_err(|error| ProviderError::Process(error.to_string()))?,
            ),
        };
        let mut command = Command::new(&request.executable);
        for argument in request.arguments {
            match argument {
                NativeCommandArgument::Literal(argument) => {
                    command.arg(argument);
                }
                NativeCommandArgument::OutputFile => {
                    let path = temporary.as_ref().ok_or_else(|| {
                        ProviderError::Configuration(
                            "command requested an output path without file capture".to_owned(),
                        )
                    })?;
                    command.arg(path.path());
                }
            }
        }
        command
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        // Native speech tools do not need provider, cloud, proxy, release, or shell credentials.
        // Clear the desktop environment so an OS helper or user shell profile cannot inherit
        // reusable secrets accidentally; copy only variables required to locate system binaries
        // and temporary storage.
        command.env_clear();
        for key in SAFE_NATIVE_ENVIRONMENT {
            if let Some(value) = std::env::var_os(key) {
                command.env(key, value);
            }
        }
        let mut child = command
            .spawn()
            .map_err(|error| ProviderError::Process(error.to_string()))?;
        if let Some(mut stdin) = child.stdin.take() {
            stdin
                .write_all(&request.stdin)
                .await
                .map_err(|error| ProviderError::Process(error.to_string()))?;
        }
        let output = tokio::time::timeout(request.timeout, child.wait_with_output())
            .await
            .map_err(|_| ProviderError::Process("native TTS command timed out".to_owned()))?
            .map_err(|error| ProviderError::Process(error.to_string()))?;
        if !output.status.success() {
            // Some speech engines echo input or their environment to stderr. Never retain that
            // provider-controlled text in API errors or diagnostics.
            return Err(ProviderError::Process(format!(
                "native TTS command failed with {}",
                output.status
            )));
        }
        let artifact = temporary
            .as_ref()
            .map(|file| std::fs::read(file.path()))
            .transpose()
            .map_err(|error| ProviderError::Process(error.to_string()))?
            .map(Bytes::from);
        Ok(NativeCommandOutput {
            stdout: Bytes::from(output.stdout),
            artifact,
        })
    }
}

#[derive(Clone, Debug)]
pub struct NativeTtsProvider {
    config: NativeTtsConfig,
    runner: Arc<dyn NativeCommandRunner>,
    descriptor: ProviderDescriptor,
    capabilities: ProviderCapabilities,
}

impl NativeTtsProvider {
    pub fn new(config: NativeTtsConfig, runner: Arc<dyn NativeCommandRunner>) -> Result<Self> {
        let (id, display_name, endpoint_family) = match config.platform {
            NativePlatform::MacOs => ("native-macos", "macOS System Voices", "macos-say"),
            NativePlatform::Windows => ("native-windows", "Windows System Voices", "windows-sapi"),
            NativePlatform::Linux => ("native-linux", "Linux eSpeak NG", "espeak-ng"),
        };
        Ok(Self {
            config,
            runner,
            descriptor: ProviderDescriptor {
                id: ProviderId::new(id)?,
                display_name: display_name.to_owned(),
                kind: ProviderKind::Native,
                endpoint_family: endpoint_family.to_owned(),
            },
            capabilities: ProviderCapabilities {
                max_concurrency: 1,
                temperature: ParameterSupport::Unsupported,
                ..ProviderCapabilities::default()
            },
        })
    }

    pub fn for_current_os(
        executable: PathBuf,
        runner: Arc<dyn NativeCommandRunner>,
    ) -> Result<Self> {
        Self::new(NativeTtsConfig::for_current_os(executable)?, runner)
    }

    fn voice_command(&self) -> NativeCommand {
        let mut arguments = match self.config.platform {
            NativePlatform::MacOs => vec![literal("-v"), literal("?")],
            NativePlatform::Linux => vec![literal("--voices")],
            NativePlatform::Windows => vec![
                literal("-NoLogo"),
                literal("-NoProfile"),
                literal("-NonInteractive"),
                literal("-Command"),
                literal(WINDOWS_VOICE_SCRIPT),
            ],
        };
        self.prepend_espeak_data_path(&mut arguments);
        NativeCommand {
            executable: self.config.executable.clone(),
            arguments,
            stdin: Bytes::new(),
            capture: NativeCapture::None,
            timeout: Duration::from_secs(30),
        }
    }

    fn synthesis_command(&self, request: &SynthesisRequest) -> Result<NativeCommand> {
        if request.format != AudioFormat::Wav {
            return Err(ProviderError::Unsupported {
                feature: "native TTS output other than WAV",
            });
        }
        if request.text.trim().is_empty() || request.voice.trim().is_empty() {
            return Err(ProviderError::Configuration(
                "text and voice are required".to_owned(),
            ));
        }
        let (mut arguments, capture) = match self.config.platform {
            NativePlatform::MacOs => (
                vec![
                    literal("-v"),
                    literal(&request.voice),
                    literal("--file-format=WAVE"),
                    literal("--data-format=LEI16@48000"),
                    literal("-o"),
                    NativeCommandArgument::OutputFile,
                ],
                NativeCapture::OutputFile { suffix: ".wav" },
            ),
            NativePlatform::Linux => (
                vec![
                    literal("--stdin"),
                    literal("--stdout"),
                    literal("-v"),
                    literal(&request.voice),
                ],
                NativeCapture::None,
            ),
            NativePlatform::Windows => (
                vec![
                    literal("-NoLogo"),
                    literal("-NoProfile"),
                    literal("-NonInteractive"),
                    literal("-Command"),
                    literal(WINDOWS_SYNTHESIS_SCRIPT),
                    literal(&request.voice),
                    NativeCommandArgument::OutputFile,
                ],
                NativeCapture::OutputFile { suffix: ".wav" },
            ),
        };
        self.prepend_espeak_data_path(&mut arguments);
        Ok(NativeCommand {
            executable: self.config.executable.clone(),
            arguments,
            stdin: Bytes::copy_from_slice(request.text.as_bytes()),
            capture,
            timeout: Duration::from_secs(120),
        })
    }

    fn prepend_espeak_data_path(&self, arguments: &mut Vec<NativeCommandArgument>) {
        if let Some(parent) = self.config.espeak_data_parent() {
            arguments.insert(0, literal(format!("--path={}", parent.to_string_lossy())));
        }
    }
}

fn literal(value: impl Into<String>) -> NativeCommandArgument {
    NativeCommandArgument::Literal(value.into())
}

const WINDOWS_VOICE_SCRIPT: &str = r"$v = New-Object -ComObject SAPI.SpVoice; @($v.GetVoices() | ForEach-Object { [pscustomobject]@{ id = $_.Id; name = $_.GetDescription() } }) | ConvertTo-Json -Compress";

// Voice id and output path are command arguments; the book text is read from stdin. No user value
// is interpolated into this constant PowerShell program.
const WINDOWS_SYNTHESIS_SCRIPT: &str = r"param([string]$VoiceId,[string]$OutputPath); $text=[Console]::In.ReadToEnd(); $voice=New-Object -ComObject SAPI.SpVoice; $selected=@($voice.GetVoices() | Where-Object { $_.Id -eq $VoiceId }); if($selected.Count -ne 1){ throw 'Voice not found' }; $voice.Voice=$selected[0]; $stream=New-Object -ComObject SAPI.SpFileStream; $stream.Open($OutputPath,3,$false); $voice.AudioOutputStream=$stream; [void]$voice.Speak($text); $stream.Close()";

#[async_trait]
impl TtsProvider for NativeTtsProvider {
    fn descriptor(&self) -> &ProviderDescriptor {
        &self.descriptor
    }

    fn capabilities(&self) -> &ProviderCapabilities {
        &self.capabilities
    }

    async fn health(&self) -> Result<ProviderHealth> {
        let voices = self.discover_voices().await?;
        Ok(ProviderHealth {
            available: !voices.is_empty(),
            version: None,
            message: if voices.is_empty() {
                Some("no native voices were discovered".to_owned())
            } else {
                None
            },
        })
    }

    async fn discover_voices(&self) -> Result<Vec<Voice>> {
        let output = self.runner.run(self.voice_command()).await?;
        match self.config.platform {
            NativePlatform::MacOs => parse_macos_voices(&output.stdout),
            NativePlatform::Linux => parse_espeak_voices(&output.stdout),
            NativePlatform::Windows => parse_windows_voices(&output.stdout),
        }
    }

    async fn discover_models(&self) -> Result<Vec<Model>> {
        Ok(Vec::new())
    }

    async fn synthesize(&self, request: SynthesisRequest) -> Result<SynthesisResponse> {
        let characters = u64::try_from(request.text.chars().count()).ok();
        let command = self.synthesis_command(&request)?;
        let output = self.runner.run(command).await?;
        let audio = output.artifact.unwrap_or(output.stdout);
        if audio.len() < 12 || &audio[..4] != b"RIFF" || &audio[8..12] != b"WAVE" {
            return Err(ProviderError::InvalidResponse(
                "native TTS did not produce a WAV stream".to_owned(),
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

fn parse_macos_voices(bytes: &[u8]) -> Result<Vec<Voice>> {
    let text = std::str::from_utf8(bytes)
        .map_err(|error| ProviderError::InvalidResponse(error.to_string()))?;
    Ok(text
        .lines()
        .filter_map(|line| {
            let mut fields = line.split_whitespace();
            let name = fields.next()?;
            let language = fields.next()?;
            Some(Voice {
                id: name.to_owned(),
                name: name.to_owned(),
                language: Some(language.to_owned()),
                owned_clone: false,
                metadata: Default::default(),
            })
        })
        .collect())
}

fn parse_espeak_voices(bytes: &[u8]) -> Result<Vec<Voice>> {
    let text = std::str::from_utf8(bytes)
        .map_err(|error| ProviderError::InvalidResponse(error.to_string()))?;
    Ok(text
        .lines()
        .skip_while(|line| !line.trim_start().starts_with("Pty"))
        .skip(1)
        .filter_map(|line| {
            let fields: Vec<_> = line.split_whitespace().collect();
            if fields.len() < 4 {
                return None;
            }
            Some(Voice {
                id: fields[3].to_owned(),
                name: fields[3].to_owned(),
                language: Some(fields[1].to_owned()),
                owned_clone: false,
                metadata: Default::default(),
            })
        })
        .collect())
}

fn parse_windows_voices(bytes: &[u8]) -> Result<Vec<Voice>> {
    #[derive(serde::Deserialize)]
    struct WindowsVoice {
        id: String,
        name: String,
    }
    let value: serde_json::Value = serde_json::from_slice(bytes)
        .map_err(|error| ProviderError::InvalidResponse(error.to_string()))?;
    let values = if value.is_array() {
        value.as_array().cloned().unwrap_or_default()
    } else if value.is_object() {
        vec![value]
    } else {
        return Err(ProviderError::InvalidResponse(
            "Windows voice list was not JSON object data".to_owned(),
        ));
    };
    values
        .into_iter()
        .map(|value| {
            let voice: WindowsVoice = serde_json::from_value(value)
                .map_err(|error| ProviderError::InvalidResponse(error.to_string()))?;
            Ok(Voice {
                id: voice.id,
                name: voice.name,
                language: None,
                owned_clone: false,
                metadata: Default::default(),
            })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use super::*;

    #[test]
    fn native_runner_environment_allowlist_excludes_credentials_and_user_profiles() {
        for forbidden in [
            "ANTHROPIC_API_KEY",
            "APPDATA",
            "AWS_SECRET_ACCESS_KEY",
            "GITHUB_TOKEN",
            "HF_TOKEN",
            "HOME",
            "HTTPS_PROXY",
            "OPENAI_API_KEY",
            "USERPROFILE",
            "XDG_CONFIG_HOME",
        ] {
            assert!(!SAFE_NATIVE_ENVIRONMENT.contains(&forbidden));
        }
    }

    #[derive(Debug)]
    struct FixtureRunner {
        output: NativeCommandOutput,
        command: Mutex<Option<NativeCommand>>,
    }

    #[async_trait]
    impl NativeCommandRunner for FixtureRunner {
        async fn run(&self, command: NativeCommand) -> Result<NativeCommandOutput> {
            *self.command.lock().unwrap() = Some(command);
            Ok(self.output.clone())
        }
    }

    fn fixture(stdout: &'static [u8], artifact: Option<&'static [u8]>) -> Arc<FixtureRunner> {
        Arc::new(FixtureRunner {
            output: NativeCommandOutput {
                stdout: Bytes::from_static(stdout),
                artifact: artifact.map(Bytes::from_static),
            },
            command: Mutex::new(None),
        })
    }

    fn fixture_executable() -> PathBuf {
        #[cfg(windows)]
        {
            PathBuf::from(r"C:\AudiobookAI\native-tts-fixture.exe")
        }
        #[cfg(not(windows))]
        {
            PathBuf::from("/audiobookai/native-tts-fixture")
        }
    }

    #[test]
    fn fixture_executable_is_host_absolute() {
        assert!(fixture_executable().is_absolute());
    }

    #[tokio::test]
    async fn parses_macos_voice_fixture() {
        let runner = fixture(b"Ava en_US # Hello\nAnna de_DE # Hallo\n", None);
        let provider = NativeTtsProvider::new(
            NativeTtsConfig::new(NativePlatform::MacOs, fixture_executable()).unwrap(),
            runner,
        )
        .unwrap();
        let voices = provider.discover_voices().await.unwrap();
        assert_eq!(voices.len(), 2);
        assert_eq!(voices[1].language.as_deref(), Some("de_DE"));
    }

    #[tokio::test]
    async fn parses_linux_espeak_fixture() {
        let runner = fixture(
            b"Pty Language Age/Gender VoiceName File Other Languages\n 5  en-us M  english-us en-us\n 5  de M  german de\n",
            None,
        );
        let provider = NativeTtsProvider::new(
            NativeTtsConfig::new(NativePlatform::Linux, fixture_executable()).unwrap(),
            runner,
        )
        .unwrap();
        let voices = provider.discover_voices().await.unwrap();
        assert_eq!(voices[0].id, "english-us");
        assert_eq!(voices[1].language.as_deref(), Some("de"));
    }

    #[tokio::test]
    async fn packaged_linux_espeak_uses_the_sibling_voice_data_tree() {
        let resources = tempfile::TempDir::new().unwrap();
        let bin = resources.path().join("sidecars").join("bin");
        let share = resources.path().join("sidecars").join("share");
        std::fs::create_dir_all(share.join("espeak-ng-data")).unwrap();
        std::fs::create_dir_all(&bin).unwrap();
        let runner = fixture(
            b"Pty Language Age/Gender VoiceName File Other Languages\n 5 en M english en\n",
            None,
        );
        let provider = NativeTtsProvider::new(
            NativeTtsConfig::new(NativePlatform::Linux, bin.join("espeak-ng")).unwrap(),
            runner.clone(),
        )
        .unwrap();

        provider.discover_voices().await.unwrap();

        let command = runner.command.lock().unwrap();
        assert_eq!(
            command.as_ref().unwrap().arguments.first(),
            Some(&literal(format!("--path={}", share.to_string_lossy())))
        );
    }

    #[tokio::test]
    async fn parses_windows_voice_fixture() {
        let runner = fixture(br#"[{"id":"SAPI\\Voice1","name":"Microsoft Voice"}]"#, None);
        let provider = NativeTtsProvider::new(
            NativeTtsConfig::new(NativePlatform::Windows, fixture_executable()).unwrap(),
            runner,
        )
        .unwrap();
        let voices = provider.discover_voices().await.unwrap();
        assert_eq!(voices[0].name, "Microsoft Voice");
    }

    #[tokio::test]
    async fn synthesis_keeps_text_on_stdin_and_voice_as_literal_argument() {
        const WAV: &[u8] = b"RIFF....WAVEfixture";
        let runner = fixture(b"", Some(WAV));
        let provider = NativeTtsProvider::new(
            NativeTtsConfig::new(NativePlatform::MacOs, fixture_executable()).unwrap(),
            runner.clone(),
        )
        .unwrap();
        let response = provider
            .synthesize(SynthesisRequest {
                request_id: uuid::Uuid::new_v4(),
                text: "Secret book text".to_owned(),
                model: None,
                voice: "Ava; rm -rf /".to_owned(),
                format: AudioFormat::Wav,
                options: Default::default(),
                pronunciation_dictionary_ids: Vec::new(),
            })
            .await
            .unwrap();
        assert_eq!(&response.audio[..4], b"RIFF");
        let command = runner.command.lock().unwrap();
        let command = command.as_ref().unwrap();
        assert_eq!(command.stdin, Bytes::from_static(b"Secret book text"));
        assert!(command.arguments.contains(&literal("Ava; rm -rf /")));
        assert!(!format!("{command:?}").contains("Secret book text"));
    }
}
