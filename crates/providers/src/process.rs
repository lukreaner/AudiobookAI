use std::{
    collections::{HashMap, VecDeque},
    process::Stdio,
    sync::Arc,
    time::Duration,
};

use async_trait::async_trait;
use chrono::Utc;
use tokio::{
    io::{AsyncBufReadExt, BufReader},
    process::{Child, Command},
    sync::Mutex,
};
use uuid::Uuid;

use crate::model_library::{HttpModelLibraryClient, validate_runtime_model_identifier};
use crate::{
    CancellationFlag, EndpointConfig, HttpMethod, HttpRequest, HttpTransport, ModelControlProtocol,
    ModelDownloadProgressSink, ModelDownloadRequest, ModelDownloadStatus, OwnedProcessHandle,
    ProcessLogLine, ProcessLogStream, ProcessSpec, ProcessState, ProcessStatus, ProviderControl,
    ProviderDescriptor, ProviderError, ProviderModelInfo, Result,
};

const MAX_LOG_LINES: usize = 5_000;
const SAFE_INHERITED_ENVIRONMENT: &[&str] = &[
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

#[derive(Debug)]
struct ManagedEntry {
    ownership_token: Uuid,
    spec: ProcessSpec,
    child: Child,
    logs: Arc<Mutex<VecDeque<ProcessLogLine>>>,
    exit_code: Option<i32>,
}

/// Supervises only processes spawned through this instance.
///
/// An operating-system PID is never accepted as authority. Every mutating operation requires the
/// unguessable ownership token returned by [`start`](Self::start).
#[derive(Clone, Debug, Default)]
pub struct ManagedProcessSupervisor {
    entries: Arc<Mutex<HashMap<Uuid, ManagedEntry>>>,
}

impl ManagedProcessSupervisor {
    pub async fn start(&self, spec: ProcessSpec) -> Result<OwnedProcessHandle> {
        validate_process_spec(&spec)?;
        let mut command = Command::new(&spec.executable);
        command
            .args(&spec.arguments)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(false);
        // A desktop process can inherit developer, shell, proxy, and release credentials. A
        // managed provider must never receive those implicitly. Only process mechanics are copied;
        // provider-specific values must come from the explicit, encrypted configuration path.
        command.env_clear();
        for key in SAFE_INHERITED_ENVIRONMENT {
            if let Some(value) = std::env::var_os(key) {
                command.env(key, value);
            }
        }
        if let Some(directory) = &spec.working_directory {
            command.current_dir(directory);
        }
        command.envs(&spec.environment);

        let mut child = command
            .spawn()
            .map_err(|error| ProviderError::Process(error.to_string()))?;
        let stdout = child.stdout.take();
        let stderr = child.stderr.take();
        let logs = Arc::new(Mutex::new(VecDeque::new()));
        if let Some(stdout) = stdout {
            spawn_log_reader(stdout, ProcessLogStream::Stdout, Arc::clone(&logs));
        }
        if let Some(stderr) = stderr {
            spawn_log_reader(stderr, ProcessLogStream::Stderr, Arc::clone(&logs));
        }

        let handle = OwnedProcessHandle {
            process_id: Uuid::new_v4(),
            ownership_token: Uuid::new_v4(),
        };
        self.entries.lock().await.insert(
            handle.process_id,
            ManagedEntry {
                ownership_token: handle.ownership_token,
                spec,
                child,
                logs,
                exit_code: None,
            },
        );
        Ok(handle)
    }

    pub async fn status(&self, handle: &OwnedProcessHandle) -> Result<ProcessStatus> {
        let mut entries = self.entries.lock().await;
        let entry = owned_entry_mut(&mut entries, handle)?;
        if entry.exit_code.is_none()
            && let Some(status) = entry
                .child
                .try_wait()
                .map_err(|error| ProviderError::Process(error.to_string()))?
        {
            entry.exit_code = status.code();
        }
        Ok(ProcessStatus {
            state: if entry.exit_code.is_some() {
                ProcessState::Exited
            } else {
                ProcessState::Running
            },
            operating_system_pid: entry.child.id(),
            exit_code: entry.exit_code,
        })
    }

    pub async fn stop(&self, handle: &OwnedProcessHandle) -> Result<()> {
        let mut entry = {
            let mut entries = self.entries.lock().await;
            let entry = owned_entry_mut(&mut entries, handle)?;
            // Verify authority before removing the only record that proves ownership.
            if entry.ownership_token != handle.ownership_token {
                return Err(ProviderError::NotOwned);
            }
            entries
                .remove(&handle.process_id)
                .ok_or(ProviderError::ProcessNotFound)?
        };

        if entry
            .child
            .try_wait()
            .map_err(|error| ProviderError::Process(error.to_string()))?
            .is_some()
        {
            return Ok(());
        }
        entry
            .child
            .start_kill()
            .map_err(|error| ProviderError::Process(error.to_string()))?;
        match tokio::time::timeout(Duration::from_secs(10), entry.child.wait()).await {
            Ok(Ok(_)) => Ok(()),
            Ok(Err(error)) => Err(ProviderError::Process(error.to_string())),
            Err(_) => {
                entry
                    .child
                    .kill()
                    .await
                    .map_err(|error| ProviderError::Process(error.to_string()))?;
                Ok(())
            }
        }
    }

    pub async fn restart(&self, handle: &OwnedProcessHandle) -> Result<OwnedProcessHandle> {
        let spec = {
            let mut entries = self.entries.lock().await;
            owned_entry_mut(&mut entries, handle)?.spec.clone()
        };
        self.stop(handle).await?;
        self.start(spec).await
    }

    pub async fn logs(
        &self,
        handle: &OwnedProcessHandle,
        limit: usize,
    ) -> Result<Vec<ProcessLogLine>> {
        let logs = {
            let mut entries = self.entries.lock().await;
            Arc::clone(&owned_entry_mut(&mut entries, handle)?.logs)
        };
        let lines = logs.lock().await;
        let skip = lines.len().saturating_sub(limit.min(MAX_LOG_LINES));
        Ok(lines.iter().skip(skip).cloned().collect())
    }
}

fn owned_entry_mut<'a>(
    entries: &'a mut HashMap<Uuid, ManagedEntry>,
    handle: &OwnedProcessHandle,
) -> Result<&'a mut ManagedEntry> {
    let entry = entries
        .get_mut(&handle.process_id)
        .ok_or(ProviderError::ProcessNotFound)?;
    if entry.ownership_token != handle.ownership_token {
        return Err(ProviderError::NotOwned);
    }
    Ok(entry)
}

fn validate_process_spec(spec: &ProcessSpec) -> Result<()> {
    if !spec.executable.is_absolute() {
        return Err(ProviderError::Configuration(
            "managed provider executable path must be absolute".to_owned(),
        ));
    }
    let metadata = std::fs::metadata(&spec.executable)
        .map_err(|error| ProviderError::Configuration(error.to_string()))?;
    if !metadata.is_file() {
        return Err(ProviderError::Configuration(
            "managed provider executable must be a file".to_owned(),
        ));
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if metadata.permissions().mode() & 0o111 == 0 {
            return Err(ProviderError::Configuration(
                "managed provider executable is not marked executable".to_owned(),
            ));
        }
    }
    if let Some(directory) = &spec.working_directory
        && (!directory.is_absolute() || !directory.is_dir())
    {
        return Err(ProviderError::Configuration(
            "managed provider working directory must be an existing absolute directory".to_owned(),
        ));
    }
    validate_managed_process_arguments(&spec.arguments)?;
    if spec
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

/// Validates literal arguments before a managed provider process can receive them.
pub fn validate_managed_process_arguments(arguments: &[String]) -> Result<()> {
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
        argument_uses_credential_name(argument) || contains_secret_shaped_value(argument)
    }) {
        return Err(ProviderError::Configuration(
            "managed provider arguments must not contain credentials; use encrypted credential storage"
                .to_owned(),
        ));
    }
    Ok(())
}

fn argument_uses_credential_name(argument: &str) -> bool {
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

fn spawn_log_reader<R>(
    reader: R,
    stream: ProcessLogStream,
    logs: Arc<Mutex<VecDeque<ProcessLogLine>>>,
) where
    R: tokio::io::AsyncRead + Send + Unpin + 'static,
{
    tokio::spawn(async move {
        let mut lines = BufReader::new(reader).lines();
        while let Ok(Some(mut line)) = lines.next_line().await {
            line.truncate(8_192);
            let mut log = logs.lock().await;
            if log.len() == MAX_LOG_LINES {
                log.pop_front();
            }
            log.push_back(ProcessLogLine {
                timestamp: Utc::now(),
                stream,
                line: redact_log_line(&line),
            });
        }
    });
}

fn redact_log_line(line: &str) -> String {
    // Managed programs occasionally echo environment variables, request headers, or JSON config.
    // Be deliberately conservative: a false-positive hides one diagnostic line, while a
    // false-negative can expose a reusable credential through the desktop log viewer.
    const SENSITIVE: [&str; 10] = [
        "api_key",
        "apikey",
        "authorization",
        "bearer ",
        "access_key",
        "client_secret",
        "password",
        "secret",
        "token",
        "x-api-key",
    ];
    let lowercase = line.to_ascii_lowercase();
    if SENSITIVE.iter().any(|marker| lowercase.contains(marker))
        || line.contains("-----BEGIN ")
        || line.split_whitespace().any(looks_like_opaque_secret)
    {
        return "[REDACTED SENSITIVE LOG LINE]".to_owned();
    }
    if let Some((key, _)) = line.split_once('=') {
        let uppercase = key.to_ascii_uppercase();
        if ["KEY", "CREDENTIAL", "PASS", "AUTH"]
            .iter()
            .any(|marker| uppercase.contains(marker))
        {
            return format!("{key}=[REDACTED]");
        }
    }
    line.to_owned()
}

/// Detects credential-shaped opaque values without retaining or returning the candidate.
pub fn contains_secret_shaped_value(value: &str) -> bool {
    looks_like_opaque_secret(value)
        || value
            .split(|character: char| {
                character.is_ascii_whitespace()
                    || matches!(
                        character,
                        '=' | ':'
                            | '/'
                            | '\\'
                            | '@'
                            | '?'
                            | '&'
                            | ','
                            | ';'
                            | '"'
                            | '\''
                            | '`'
                            | '('
                            | ')'
                            | '['
                            | ']'
                            | '{'
                            | '}'
                    )
            })
            .any(looks_like_opaque_secret)
        || value
            .split_once('=')
            .is_some_and(|(_, candidate)| looks_like_opaque_secret(candidate.trim()))
        || value
            .split_once(':')
            .is_some_and(|(_, candidate)| looks_like_opaque_secret(candidate.trim()))
}

fn looks_like_opaque_secret(value: &str) -> bool {
    let value = value.trim_matches(|character: char| {
        matches!(
            character,
            '"' | '\'' | '`' | ',' | ';' | ':' | '(' | ')' | '[' | ']' | '{' | '}'
        )
    });
    let lowercase = value.to_ascii_lowercase();
    if ["sk-", "hf_", "github_pat_"]
        .iter()
        .any(|prefix| lowercase.starts_with(prefix) && value.len() >= prefix.len() + 8)
    {
        return true;
    }
    let jwt_segments = value.split('.').collect::<Vec<_>>();
    if jwt_segments.len() == 3
        && value.len() >= 24
        && jwt_segments.iter().all(|segment| {
            segment.len() >= 6
                && segment
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-'))
        })
    {
        return true;
    }
    if value.len() < 24
        || value.len() > 512
        || !value.bytes().all(|byte| {
            byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-' | b'+' | b'/' | b'=')
        })
    {
        return false;
    }
    let lower = value.bytes().any(|byte| byte.is_ascii_lowercase());
    let upper = value.bytes().any(|byte| byte.is_ascii_uppercase());
    let digit = value.bytes().any(|byte| byte.is_ascii_digit());
    let mut unique = [false; 128];
    for byte in value.bytes().filter(u8::is_ascii) {
        unique[usize::from(byte)] = true;
    }
    let diversity = unique.into_iter().filter(|present| *present).count();
    (lower && upper && digit && diversity >= 14)
        || (matches!(value.len(), 32 | 48)
            && value.bytes().all(|byte| byte.is_ascii_hexdigit())
            && diversity >= 12)
}

#[derive(Clone, Debug)]
pub struct ManagedProcessController {
    descriptor: ProviderDescriptor,
    supervisor: ManagedProcessSupervisor,
    model_control: Option<HttpModelControl>,
}

impl ManagedProcessController {
    pub fn new(descriptor: ProviderDescriptor, supervisor: ManagedProcessSupervisor) -> Self {
        Self {
            descriptor,
            supervisor,
            model_control: None,
        }
    }

    pub fn with_model_control(
        mut self,
        protocol: ModelControlProtocol,
        endpoint: EndpointConfig,
        transport: Arc<dyn HttpTransport>,
    ) -> Self {
        self.model_control = Some(HttpModelControl {
            protocol,
            library: HttpModelLibraryClient::new(
                protocol,
                endpoint.clone(),
                Arc::clone(&transport),
            ),
            endpoint,
            transport,
        });
        self
    }
}

#[derive(Clone, Debug)]
struct HttpModelControl {
    protocol: ModelControlProtocol,
    endpoint: EndpointConfig,
    transport: Arc<dyn HttpTransport>,
    library: HttpModelLibraryClient,
}

impl HttpModelControl {
    async fn command(&self, model: &str, load: bool) -> Result<()> {
        validate_runtime_model_identifier(self.protocol, model)?;
        let (path, body) = match self.protocol {
            ModelControlProtocol::Ollama => (
                "api/generate",
                serde_json::json!({
                    "model": model,
                    "prompt": "",
                    "stream": false,
                    "keep_alive": if load { serde_json::json!("30m") } else { serde_json::json!(0) }
                }),
            ),
            ModelControlProtocol::LmStudio if load => {
                ("api/v1/models/load", serde_json::json!({ "model": model }))
            }
            ModelControlProtocol::LmStudio => {
                let instance_id = self.lm_studio_instance_id(model).await?;
                (
                    "api/v1/models/unload",
                    serde_json::json!({ "instance_id": instance_id }),
                )
            }
            ModelControlProtocol::LocalAi if load => {
                ("backend/load", serde_json::json!({ "model": model }))
            }
            ModelControlProtocol::LocalAi => {
                ("backend/shutdown", serde_json::json!({ "model": model }))
            }
        };
        let mut request =
            HttpRequest::json(HttpMethod::Post, self.endpoint.endpoint(path)?, &body)?;
        self.endpoint.authentication.apply(&mut request.headers);
        self.transport.execute(request).await?.require_success()?;
        Ok(())
    }

    async fn lm_studio_instance_id(&self, requested: &str) -> Result<String> {
        let models = self.library.list_models().await?;
        if let Some(instance) = models
            .iter()
            .flat_map(|model| model.loaded_instances.iter())
            .find(|instance| instance.as_str() == requested)
        {
            return Ok(instance.clone());
        }
        let model = models
            .into_iter()
            .find(|model| model.id == requested)
            .ok_or_else(|| {
                ProviderError::Configuration(
                    "the selected LM Studio model was not found".to_owned(),
                )
            })?;
        match model.loaded_instances.as_slice() {
            [instance] => Ok(instance.clone()),
            [] => Err(ProviderError::Configuration(
                "the selected LM Studio model has no loaded instance".to_owned(),
            )),
            _ => Err(ProviderError::Configuration(
                "the selected LM Studio model has multiple loaded instances; select a specific instance"
                    .to_owned(),
            )),
        }
    }
}

#[async_trait]
impl ProviderControl for ManagedProcessController {
    fn descriptor(&self) -> &ProviderDescriptor {
        &self.descriptor
    }

    async fn start(&self, spec: ProcessSpec) -> Result<OwnedProcessHandle> {
        self.supervisor.start(spec).await
    }

    async fn status(&self, handle: &OwnedProcessHandle) -> Result<ProcessStatus> {
        self.supervisor.status(handle).await
    }

    async fn stop(&self, handle: &OwnedProcessHandle) -> Result<()> {
        self.supervisor.stop(handle).await
    }

    async fn restart(&self, handle: &OwnedProcessHandle) -> Result<OwnedProcessHandle> {
        self.supervisor.restart(handle).await
    }

    async fn logs(&self, handle: &OwnedProcessHandle, limit: usize) -> Result<Vec<ProcessLogLine>> {
        self.supervisor.logs(handle, limit).await
    }

    async fn load_model(&self, model: &str) -> Result<()> {
        self.model_control
            .as_ref()
            .ok_or(ProviderError::Unsupported {
                feature: "model loading",
            })?
            .command(model, true)
            .await
    }

    async fn unload_model(&self, model: &str) -> Result<()> {
        self.model_control
            .as_ref()
            .ok_or(ProviderError::Unsupported {
                feature: "model unloading",
            })?
            .command(model, false)
            .await
    }

    async fn list_models(&self) -> Result<Vec<ProviderModelInfo>> {
        self.model_control
            .as_ref()
            .ok_or(ProviderError::Unsupported {
                feature: "model listing",
            })?
            .library
            .list_models()
            .await
    }

    async fn download_model(
        &self,
        request: ModelDownloadRequest,
        cancellation: CancellationFlag,
        progress: Arc<dyn ModelDownloadProgressSink>,
    ) -> Result<ModelDownloadStatus> {
        self.model_control
            .as_ref()
            .ok_or(ProviderError::Unsupported {
                feature: "model downloading",
            })?
            .library
            .download_model(request, cancellation, progress)
            .await
    }

    async fn model_download_status(&self, job_id: &str) -> Result<ModelDownloadStatus> {
        self.model_control
            .as_ref()
            .ok_or(ProviderError::Unsupported {
                feature: "model download status",
            })?
            .library
            .download_status(job_id)
            .await
    }

    async fn delete_model(&self, model: &str, confirmed: bool, in_use: bool) -> Result<()> {
        self.model_control
            .as_ref()
            .ok_or(ProviderError::Unsupported {
                feature: "model deletion",
            })?
            .library
            .delete_model(model, confirmed, in_use)
            .await
    }
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use super::*;

    #[derive(Debug)]
    struct RecordingHttpTransport {
        requests: Mutex<Vec<HttpRequest>>,
        responses: Mutex<VecDeque<crate::HttpResponse>>,
    }

    impl RecordingHttpTransport {
        fn new(responses: Vec<serde_json::Value>) -> Self {
            Self {
                requests: Mutex::new(Vec::new()),
                responses: Mutex::new(
                    responses
                        .into_iter()
                        .map(|value| crate::HttpResponse {
                            status: 200,
                            headers: Default::default(),
                            body: serde_json::to_vec(&value).unwrap().into(),
                        })
                        .collect(),
                ),
            }
        }
    }

    #[async_trait]
    impl HttpTransport for RecordingHttpTransport {
        async fn execute(&self, request: HttpRequest) -> Result<crate::HttpResponse> {
            self.requests.lock().await.push(request);
            self.responses.lock().await.pop_front().ok_or_else(|| {
                ProviderError::Transport("missing model-control fixture response".to_owned())
            })
        }
    }

    #[tokio::test]
    async fn lm_studio_unloads_the_resolved_instance_id() {
        let transport = Arc::new(RecordingHttpTransport::new(vec![
            serde_json::json!({
                "models": [{
                    "key": "publisher/model",
                    "display_name": "Model",
                    "loaded_instances": [{"id": "model-instance-1"}]
                }]
            }),
            serde_json::json!({}),
        ]));
        let endpoint = EndpointConfig::managed_loopback(
            url::Url::parse("http://127.0.0.1:1234/").unwrap(),
            crate::Authentication::None,
        )
        .unwrap();
        let control = HttpModelControl {
            protocol: ModelControlProtocol::LmStudio,
            endpoint: endpoint.clone(),
            transport: transport.clone(),
            library: HttpModelLibraryClient::new(
                ModelControlProtocol::LmStudio,
                endpoint,
                transport.clone(),
            ),
        };

        control.command("publisher/model", false).await.unwrap();

        let requests = transport.requests.lock().await;
        assert_eq!(requests.len(), 2);
        assert_eq!(requests[1].url.path(), "/api/v1/models/unload");
        let body: serde_json::Value = serde_json::from_slice(&requests[1].body).unwrap();
        assert_eq!(
            body,
            serde_json::json!({ "instance_id": "model-instance-1" })
        );
        assert!(body.get("model").is_none());
    }

    #[tokio::test]
    async fn local_ai_loads_and_unloads_with_the_documented_admin_contract() {
        let transport = Arc::new(RecordingHttpTransport::new(vec![
            serde_json::json!({"loaded": ["voice-model"], "message": "model loaded"}),
            serde_json::json!({}),
        ]));
        let endpoint = EndpointConfig::managed_loopback(
            url::Url::parse("http://127.0.0.1:8080/").unwrap(),
            crate::Authentication::Bearer(crate::Credential::new("fixture-value")),
        )
        .unwrap();
        let control = HttpModelControl {
            protocol: ModelControlProtocol::LocalAi,
            endpoint: endpoint.clone(),
            transport: transport.clone(),
            library: HttpModelLibraryClient::new(
                ModelControlProtocol::LocalAi,
                endpoint,
                transport.clone(),
            ),
        };

        control.command("voice-model", true).await.unwrap();
        control.command("voice-model", false).await.unwrap();

        let requests = transport.requests.lock().await;
        assert_eq!(requests.len(), 2);
        assert_eq!(requests[0].method, HttpMethod::Post);
        assert_eq!(requests[0].url.path(), "/backend/load");
        assert_eq!(requests[1].method, HttpMethod::Post);
        assert_eq!(requests[1].url.path(), "/backend/shutdown");
        assert!(requests.iter().all(|request| {
            request.headers.get("authorization").map(String::as_str) == Some("Bearer fixture-value")
        }));
        for request in requests.iter() {
            let body: serde_json::Value = serde_json::from_slice(&request.body).unwrap();
            assert_eq!(body, serde_json::json!({"model": "voice-model"}));
        }
        assert!(!format!("{control:?}").contains("fixture-value"));
    }

    #[tokio::test]
    async fn local_ai_control_rejects_secret_shaped_model_ids_before_dispatch() {
        let transport = Arc::new(RecordingHttpTransport::new(Vec::new()));
        let endpoint = EndpointConfig::managed_loopback(
            url::Url::parse("http://127.0.0.1:8080/").unwrap(),
            crate::Authentication::None,
        )
        .unwrap();
        let control = HttpModelControl {
            protocol: ModelControlProtocol::LocalAi,
            endpoint: endpoint.clone(),
            transport: transport.clone(),
            library: HttpModelLibraryClient::new(
                ModelControlProtocol::LocalAi,
                endpoint,
                transport.clone(),
            ),
        };
        let secret_shaped = ["sk", "syntheticcredential0123456789"].join("-");

        let error = control
            .command(&secret_shaped, true)
            .await
            .expect_err("secret-shaped model input must be rejected");

        assert!(error.to_string().contains("sensitive credential material"));
        assert!(!error.to_string().contains(&secret_shaped));
        assert!(transport.requests.lock().await.is_empty());
    }

    #[cfg(unix)]
    fn shell_spec() -> ProcessSpec {
        ProcessSpec {
            executable: Path::new("/bin/sh").to_path_buf(),
            arguments: vec!["-c".to_owned(), "echo ready; sleep 30".to_owned()],
            working_directory: None,
            environment: Default::default(),
        }
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn refuses_forged_ownership_token() {
        let supervisor = ManagedProcessSupervisor::default();
        let handle = supervisor.start(shell_spec()).await.unwrap();
        let forged = OwnedProcessHandle {
            process_id: handle.process_id,
            ownership_token: Uuid::new_v4(),
        };
        assert!(matches!(
            supervisor.stop(&forged).await,
            Err(ProviderError::NotOwned)
        ));
        supervisor.stop(&handle).await.unwrap();
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn captures_logs_from_owned_child() {
        let supervisor = ManagedProcessSupervisor::default();
        let handle = supervisor.start(shell_spec()).await.unwrap();
        tokio::time::sleep(Duration::from_millis(50)).await;
        let logs = supervisor.logs(&handle, 10).await.unwrap();
        assert!(logs.iter().any(|line| line.line == "ready"));
        supervisor.stop(&handle).await.unwrap();
    }

    #[test]
    fn requires_absolute_executable_path() {
        let spec = ProcessSpec {
            executable: Path::new("server").to_path_buf(),
            arguments: Vec::new(),
            working_directory: None,
            environment: Default::default(),
        };
        assert!(validate_process_spec(&spec).is_err());
    }

    #[cfg(unix)]
    #[test]
    fn rejects_nul_bytes_in_literal_arguments() {
        let mut spec = shell_spec();
        spec.arguments = vec!["bad\0argument".to_owned()];
        assert!(validate_process_spec(&spec).is_err());
    }

    #[cfg(unix)]
    #[test]
    fn rejects_credentials_in_process_arguments() {
        let sensitive_flag = ["--api", "-key"].concat();
        for arguments in [
            vec![sensitive_flag.clone(), "runtime-value".to_owned()],
            vec![format!("{sensitive_flag}=runtime-value")],
            vec![["Bearer", "runtime-value"].join(" ")],
        ] {
            let mut spec = shell_spec();
            spec.arguments = arguments;
            assert!(validate_process_spec(&spec).is_err());
        }
    }

    #[test]
    fn rejects_secret_shaped_whole_argument_values_without_echoing_them() {
        let prefixed = [
            ["s", "k"].concat(),
            "syntheticcredential0123456789".to_owned(),
        ]
        .join("-");
        let jwt = ["headerpart0123", "payloadpart4567", "signaturepart89"].join(".");
        let opaque = ["X7qP", "2mN9", "rT4v", "B8kL", "6sW3", "yH5c"].concat();
        for argument in [
            prefixed.clone(),
            jwt.clone(),
            opaque.clone(),
            format!("--cache={prefixed}"),
            format!("--session:{jwt}"),
            format!("owner/{opaque}"),
        ] {
            let error = validate_managed_process_arguments(std::slice::from_ref(&argument))
                .expect_err("secret-shaped argument must be rejected");
            let message = error.to_string();
            assert!(message.contains("encrypted credential storage"));
            assert!(!message.contains(&argument));
        }
        assert!(
            validate_managed_process_arguments(&[
                "--listen".to_owned(),
                "127.0.0.1; echo is still a literal argument".to_owned(),
            ])
            .is_ok()
        );
    }

    #[test]
    fn redacts_credentials_in_common_log_shapes() {
        for line in [
            "OPENAI_API_KEY=sk-example",
            "Authorization: Bearer reusable-secret",
            r#"request={\"api_key\":\"secret\"}"#,
            "connecting?access_token=secret",
        ] {
            assert_eq!(redact_log_line(line), "[REDACTED SENSITIVE LOG LINE]");
        }
        assert_eq!(
            redact_log_line("server ready on port 8080"),
            "server ready on port 8080"
        );
        let opaque = ["X7qP", "2mN9", "rT4v", "B8kL", "6sW3", "yH5c"].concat();
        assert_eq!(
            redact_log_line(&format!("provider emitted {opaque}")),
            "[REDACTED SENSITIVE LOG LINE]"
        );
    }

    #[test]
    fn redacts_unlabelled_jwts_and_known_lowercase_provider_prefixes() {
        let jwt = ["headerpart", "payloadpart", "signaturepart"].join(".");
        let prefixed = [
            ["sk", "fixturevalue123"].join("-"),
            ["hf", "fixturevalue123"].join("_"),
            ["github", "pat", "fixturevalue123"].join("_"),
        ];
        for value in prefixed.iter().chain(std::iter::once(&jwt)) {
            let output = redact_log_line(&format!("provider emitted {value}"));
            assert_eq!(output, "[REDACTED SENSITIVE LOG LINE]");
            assert!(!output.contains(value));
        }
    }

    #[test]
    fn process_debug_output_never_contains_capabilities_or_environment_values() {
        let process_id = Uuid::new_v4();
        let ownership_token = Uuid::new_v4();
        let handle = OwnedProcessHandle {
            process_id,
            ownership_token,
        };
        let debug = format!("{handle:?}");
        assert!(debug.contains(&process_id.to_string()));
        assert!(!debug.contains(&ownership_token.to_string()));

        let spec = ProcessSpec {
            executable: Path::new("/absolute/provider").to_path_buf(),
            arguments: vec!["--credential=argument-secret".to_owned()],
            working_directory: None,
            environment: [("API_KEY".to_owned(), "environment-secret".to_owned())]
                .into_iter()
                .collect(),
        };
        let debug = format!("{spec:?}");
        assert!(!debug.contains("argument-secret"));
        assert!(!debug.contains("environment-secret"));
    }

    #[test]
    fn inherited_environment_allowlist_excludes_credentials_and_user_config_roots() {
        for forbidden in [
            "APPDATA",
            "AWS_SECRET_ACCESS_KEY",
            "GITHUB_TOKEN",
            "HF_TOKEN",
            "HOME",
            "HTTPS_PROXY",
            "NETRC",
            "OPENAI_API_KEY",
            "USERPROFILE",
            "XDG_CONFIG_HOME",
        ] {
            assert!(!SAFE_INHERITED_ENVIRONMENT.contains(&forbidden));
        }
    }
}
