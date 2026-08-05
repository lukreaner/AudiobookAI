use std::{
    collections::{BTreeMap, HashMap, HashSet},
    path::PathBuf,
    sync::Arc,
};

use chrono::{DateTime, Utc};
use serde::{Serialize, de::DeserializeOwned};
use sqlx::Row;
use tokio::sync::{Mutex, RwLock};
use uuid::Uuid;

use crate::{
    EventHub, ServiceConfig,
    models::{
        AppSettingsView, BudgetView, ChapterView, CharacterView, ExportArtifactView, JobView,
        ProjectDetail, PronunciationRuleView, ProviderProfileView, UsageRowView, VoiceView,
    },
};
use audiobookai_storage::Database;

#[derive(Clone, Debug)]
pub struct ImportRecord {
    pub view: crate::models::ImportDraft,
    pub managed_path: PathBuf,
    pub imported: audiobookai_epub::ImportedEpub,
}

#[derive(Debug)]
pub struct Catalog {
    pub projects: HashMap<Uuid, ProjectDetail>,
    pub import_drafts: HashMap<Uuid, ImportRecord>,
    pub characters: HashMap<Uuid, Vec<CharacterView>>,
    pub voices: Vec<VoiceView>,
    pub voice_sources: HashMap<Uuid, String>,
    pub pronunciation_rules: Vec<PronunciationRuleView>,
    pub providers: HashMap<Uuid, ProviderProfileView>,
    pub jobs: HashMap<Uuid, JobView>,
    pub exports: Vec<ExportArtifactView>,
    pub usage_rows: Vec<UsageRowView>,
    pub budgets: HashMap<Uuid, BudgetView>,
    pub settings: AppSettingsView,
    pub project_book_ids: HashMap<Uuid, Uuid>,
    pub provider_secret_ids: HashMap<Uuid, audiobookai_core::SecretId>,
}

impl Catalog {
    fn new(data_dir: &std::path::Path) -> Self {
        let mut providers = HashMap::new();
        let provider = native_provider();
        providers.insert(provider.id, provider);
        Self {
            projects: HashMap::new(),
            import_drafts: HashMap::new(),
            characters: HashMap::new(),
            voices: Vec::new(),
            voice_sources: HashMap::new(),
            pronunciation_rules: Vec::new(),
            providers,
            jobs: HashMap::new(),
            exports: Vec::new(),
            usage_rows: Vec::new(),
            budgets: HashMap::new(),
            settings: AppSettingsView::defaults(data_dir),
            project_book_ids: HashMap::new(),
            provider_secret_ids: HashMap::new(),
        }
    }
}

#[derive(Clone, Debug, Serialize)]
pub struct RuntimeStatus {
    pub instance_id: Uuid,
    pub started_at: DateTime<Utc>,
    pub version: &'static str,
    pub data_dir: PathBuf,
    pub ready: bool,
}

#[derive(Clone, Debug)]
pub struct AppState {
    pub config: ServiceConfig,
    pub events: EventHub,
    pub runtime: Arc<RwLock<RuntimeStatus>>,
    pub catalog: Arc<RwLock<Catalog>>,
    pub database: Database,
    pub secrets: crate::secrets::SecretVault,
    pub auth: crate::auth::AuthManager,
    pub providers: crate::runtime::ProviderRuntime,
    pub provider_models: crate::provider_models::ProviderModelManager,
    pub mlx: crate::mlx_management::MlxManager,
    /// Serializes model deletion with every operation that can create a new model reference.
    pub model_lifecycle: Arc<Mutex<()>>,
}

impl AppState {
    /// Builds application state from the persisted database and initializes provider runtimes.
    ///
    /// # Errors
    ///
    /// Returns an error when persisted state cannot be hydrated or a required runtime component
    /// cannot be initialized.
    pub async fn new(
        config: ServiceConfig,
        database: Database,
    ) -> Result<Self, crate::ServiceError> {
        let runtime = RuntimeStatus {
            instance_id: Uuid::new_v4(),
            started_at: Utc::now(),
            version: env!("CARGO_PKG_VERSION"),
            data_dir: config.data_dir.clone(),
            ready: true,
        };
        let mut catalog = Catalog::new(&config.data_dir);
        hydrate_settings(&database, &mut catalog).await?;
        hydrate_projects(&database, &mut catalog).await?;
        hydrate_providers(&database, &mut catalog).await?;
        hydrate_characters(&database, &mut catalog).await?;
        hydrate_voices(&database, &mut catalog).await?;
        hydrate_pronunciation_rules(&database, &mut catalog).await?;
        hydrate_jobs(&database, &mut catalog).await?;
        hydrate_usage(&database, &mut catalog).await?;
        hydrate_budgets(&database, &mut catalog).await?;
        let auth = crate::auth::AuthManager::initialize(
            database.clone(),
            config.desktop_bootstrap,
            config.bind.ip().is_loopback(),
        );
        let secrets = crate::secrets::SecretVault::initialize(database.clone()).await;
        let providers = crate::runtime::ProviderRuntime::production()
            .map_err(|error| crate::ServiceError::Internal(error.to_string()))?;
        let mlx = crate::mlx_management::MlxManager::initialize(&config).await?;
        let events = EventHub::new(512);
        let provider_models =
            crate::provider_models::ProviderModelManager::new(providers.clone(), events.clone());
        catalog.settings.secret_store = match secrets.key_source().await {
            Some(audiobookai_core::MasterKeySource::OsKeychain) => {
                crate::models::SecretStoreView::Keychain
            }
            Some(audiobookai_core::MasterKeySource::Argon2idPassphrase) => {
                crate::models::SecretStoreView::Passphrase
            }
            None => crate::models::SecretStoreView::Locked,
        };
        let state = Self {
            catalog: Arc::new(RwLock::new(catalog)),
            database,
            secrets,
            auth,
            providers,
            provider_models,
            mlx,
            model_lifecycle: Arc::new(Mutex::new(())),
            config,
            events,
            runtime: Arc::new(RwLock::new(runtime)),
        };
        let native_profiles = state
            .catalog
            .read()
            .await
            .providers
            .values()
            .filter(|profile| matches!(profile.mode, crate::models::ProviderModeView::Native))
            .cloned()
            .collect::<Vec<_>>();
        for profile in native_profiles {
            if state
                .database
                .repositories()
                .providers
                .get(audiobookai_core::ProviderProfileId::from_uuid(profile.id))
                .await
                .map_err(|error| crate::ServiceError::Storage(error.to_string()))?
                .is_none()
            {
                crate::api::persist_provider(&state, &profile, None).await?;
            }
        }
        state.bootstrap_provider_runtime().await;
        Ok(state)
    }

    async fn bootstrap_provider_runtime(&self) {
        let ids = self
            .catalog
            .read()
            .await
            .providers
            .keys()
            .copied()
            .collect::<Vec<_>>();
        for id in ids {
            if let Err(error) = self.sync_provider_runtime(id).await
                && let Some(profile) = self.catalog.write().await.providers.get_mut(&id)
            {
                profile.status = crate::models::ProviderStatusView::Unconfigured;
                profile.last_error = Some(error.to_string());
            }
        }
    }

    /// Replaces the runtime registration for a persisted provider profile.
    ///
    /// # Errors
    ///
    /// Returns an error when the profile does not exist, its credential cannot be exposed, its
    /// runtime configuration is invalid, or the runtime cannot unregister or register it.
    pub async fn sync_provider_runtime(&self, id: Uuid) -> Result<(), crate::ServiceError> {
        let (profile, secret_id) = {
            let catalog = self.catalog.read().await;
            (
                catalog
                    .providers
                    .get(&id)
                    .cloned()
                    .ok_or(crate::ServiceError::NotFound)?,
                catalog.provider_secret_ids.get(&id).copied(),
            )
        };
        let runtime_id = audiobookai_providers::ProviderId::new(id.to_string())
            .map_err(|error| crate::ServiceError::InvalidRequest(error.to_string()))?;
        if self.providers.profile_ids().await.contains(&runtime_id) {
            self.providers
                .unregister(&runtime_id)
                .await
                .map_err(|error| crate::ServiceError::Conflict(error.to_string()))?;
        }
        let credential = match secret_id {
            Some(secret_id) => Some(crate::runtime::CredentialMaterial::from_zeroizing_bytes(
                &self.secrets.expose(secret_id).await?,
            )),
            None => None,
        };
        let runtime_profile = runtime_profile_from_view(&profile, &self.config)?;
        self.providers
            .register(runtime_profile, credential.as_ref())
            .await
            .map_err(|error| crate::ServiceError::InvalidRequest(error.to_string()))
    }
}

fn runtime_profile_from_view(
    profile: &ProviderProfileView,
    config: &ServiceConfig,
) -> Result<crate::runtime::RuntimeProfile, crate::ServiceError> {
    use crate::{
        models::{ProviderKindView, ProviderModeView},
        runtime::{RuntimeAdapterKind, RuntimeProfile},
    };
    use audiobookai_providers::{ProviderId, ProviderKind};

    let runtime_id = ProviderId::new(profile.id.to_string())
        .map_err(|error| crate::ServiceError::InvalidRequest(error.to_string()))?;
    let adapter = match profile.kind {
        ProviderKindView::Elevenlabs => RuntimeAdapterKind::ElevenLabs,
        ProviderKindView::MlxAudio => RuntimeAdapterKind::MlxAudio,
        ProviderKindView::Localai => RuntimeAdapterKind::LocalAi,
        ProviderKindView::AlltalkV2 => RuntimeAdapterKind::AllTalkV2,
        ProviderKindView::NativeOs => RuntimeAdapterKind::NativeOs,
        ProviderKindView::Openai => RuntimeAdapterKind::OpenAi,
        ProviderKindView::OpenaiCompatible => RuntimeAdapterKind::OpenAiCompatible,
        ProviderKindView::Anthropic => RuntimeAdapterKind::Anthropic,
        ProviderKindView::Gemini => RuntimeAdapterKind::Gemini,
        ProviderKindView::Qwen => RuntimeAdapterKind::Qwen,
        ProviderKindView::Kimi => RuntimeAdapterKind::Kimi,
        ProviderKindView::Moonshot => RuntimeAdapterKind::Moonshot,
        ProviderKindView::LmStudio => RuntimeAdapterKind::LmStudio,
        ProviderKindView::Ollama => RuntimeAdapterKind::Ollama,
    };
    let mode = match profile.mode {
        ProviderModeView::CloudRemote => ProviderKind::CloudRemote,
        ProviderModeView::ExternalEndpoint => ProviderKind::ExternalEndpoint,
        ProviderModeView::ManagedChild => ProviderKind::ManagedChild,
        ProviderModeView::Native => ProviderKind::Native,
    };
    let mut runtime = RuntimeProfile::new(runtime_id, profile.name.clone(), adapter, mode);
    runtime.endpoint = profile
        .endpoint
        .as_deref()
        .map(url::Url::parse)
        .transpose()
        .map_err(|error| crate::ServiceError::InvalidRequest(error.to_string()))?
        .or_else(|| local_default_endpoint(adapter));
    runtime.executable = profile
        .executable_path
        .as_ref()
        .map(std::path::PathBuf::from)
        .or_else(|| native_executable(adapter, config));
    runtime.arguments.clone_from(&profile.arguments);
    runtime.working_directory = profile
        .working_directory
        .as_ref()
        .map(std::path::PathBuf::from);
    Ok(runtime)
}

fn local_default_endpoint(adapter: crate::runtime::RuntimeAdapterKind) -> Option<url::Url> {
    use crate::runtime::RuntimeAdapterKind;
    let endpoint = match adapter {
        RuntimeAdapterKind::MlxAudio => "http://127.0.0.1:8000/",
        RuntimeAdapterKind::LocalAi => "http://127.0.0.1:8080/",
        RuntimeAdapterKind::AllTalkV2 => "http://127.0.0.1:7851/",
        RuntimeAdapterKind::LmStudio => "http://127.0.0.1:1234/",
        RuntimeAdapterKind::Ollama => "http://127.0.0.1:11434/",
        _ => return None,
    };
    url::Url::parse(endpoint).ok()
}

fn native_executable(
    adapter: crate::runtime::RuntimeAdapterKind,
    config: &ServiceConfig,
) -> Option<PathBuf> {
    if !matches!(adapter, crate::runtime::RuntimeAdapterKind::NativeOs) {
        return None;
    }
    if std::env::consts::OS == "linux"
        && let Some(directory) = &config.bundled_sidecar_dir
    {
        return Some(directory.join("espeak-ng"));
    }
    if let Some(path) = std::env::var_os("AUDIOBOOKAI_NATIVE_TTS_EXECUTABLE") {
        return Some(PathBuf::from(path));
    }
    Some(native_executable_for_os(std::env::consts::OS, config))
}

fn native_executable_for_os(os: &str, config: &ServiceConfig) -> PathBuf {
    match os {
        "macos" => PathBuf::from("/usr/bin/say"),
        "windows" => PathBuf::from(r"C:\Windows\System32\WindowsPowerShell\v1.0\powershell.exe"),
        _ => config.bundled_sidecar_dir.as_ref().map_or_else(
            || PathBuf::from("/usr/bin/espeak-ng"),
            |directory| directory.join("espeak-ng"),
        ),
    }
}

async fn hydrate_settings(
    database: &Database,
    catalog: &mut Catalog,
) -> Result<(), crate::ServiceError> {
    let payload = sqlx::query_scalar::<_, String>(
        "SELECT payload FROM application_settings WHERE key = 'owner'",
    )
    .fetch_optional(database.pool())
    .await
    .map_err(|error| crate::ServiceError::Storage(error.to_string()))?;
    if let Some(payload) = payload {
        let mut settings: AppSettingsView = serde_json::from_str(&payload)
            .map_err(|error| crate::ServiceError::Internal(error.to_string()))?;
        // Runtime paths are authoritative for the active installation and cannot be
        // redirected by copying a settings database from another machine.
        settings.library_path = database.paths().library.to_string_lossy().into_owned();
        settings.cache_path = database.paths().cache.to_string_lossy().into_owned();
        settings.lan.active_sessions = 0;
        catalog.settings = settings;
    }
    Ok(())
}

async fn hydrate_budgets(
    database: &Database,
    catalog: &mut Catalog,
) -> Result<(), crate::ServiceError> {
    use crate::models::{BudgetMetricView, BudgetPeriodView, BudgetView};
    use audiobookai_core::{BudgetMetric, BudgetPeriod};

    let budgets = database
        .repositories()
        .budgets
        .list_enabled()
        .await
        .map_err(|error| crate::ServiceError::Storage(error.to_string()))?;
    for budget in budgets {
        let reserved = database
            .repositories()
            .budgets
            .active_reserved(budget.id)
            .await
            .map_err(|error| crate::ServiceError::Storage(error.to_string()))?;
        let view = BudgetView {
            id: budget.id.as_uuid(),
            name: budget.name,
            provider_profile_id: budget
                .scope
                .provider_profile_id
                .map(audiobookai_core::ProviderProfileId::as_uuid),
            period: match budget.period {
                BudgetPeriod::Job => BudgetPeriodView::Job,
                BudgetPeriod::Daily => BudgetPeriodView::Daily,
                BudgetPeriod::Monthly => BudgetPeriodView::Monthly,
                BudgetPeriod::Lifetime => BudgetPeriodView::Lifetime,
            },
            metric: match budget.metric {
                BudgetMetric::MoneyMicros => BudgetMetricView::Money,
                BudgetMetric::Characters | BudgetMetric::AudioMilliseconds => {
                    BudgetMetricView::Characters
                }
                BudgetMetric::ProviderCredits => BudgetMetricView::Credits,
                BudgetMetric::InputTokens
                | BudgetMetric::OutputTokens
                | BudgetMetric::TotalTokens => BudgetMetricView::Tokens,
            },
            limit: budget.limit,
            used: budget.used,
            reserved,
            hard: budget.hard,
            currency: budget.currency,
            warning_percent: budget.warning_threshold_percent,
        };
        catalog.budgets.insert(view.id, view);
    }
    Ok(())
}

// Provider hydration intentionally keeps relational validation, capability projection, and
// catalog insertion together so a partially validated profile can never escape this boundary.
#[allow(clippy::too_many_lines)]
async fn hydrate_providers(
    database: &Database,
    catalog: &mut Catalog,
) -> Result<(), crate::ServiceError> {
    use crate::models::{
        ProviderCapabilitiesView, ProviderKindView, ProviderModeView, ProviderStatusView,
    };
    use audiobookai_core::{ProviderDeployment, ProviderFamily, TemperatureCapability};

    let profiles = database
        .repositories()
        .providers
        .list(false)
        .await
        .map_err(|error| crate::ServiceError::Storage(error.to_string()))?;
    for profile in profiles {
        let id = profile.id.as_uuid();
        let kind = match profile.family {
            ProviderFamily::ElevenLabs => ProviderKindView::Elevenlabs,
            ProviderFamily::MlxAudio => ProviderKindView::MlxAudio,
            ProviderFamily::LocalAi => ProviderKindView::Localai,
            ProviderFamily::AllTalkV2 => ProviderKindView::AlltalkV2,
            ProviderFamily::NativeWindows
            | ProviderFamily::NativeMacos
            | ProviderFamily::EspeakNg => ProviderKindView::NativeOs,
            ProviderFamily::OpenAi => ProviderKindView::Openai,
            ProviderFamily::OpenAiCompatible | ProviderFamily::Custom(_) => {
                ProviderKindView::OpenaiCompatible
            }
            ProviderFamily::Anthropic => ProviderKindView::Anthropic,
            ProviderFamily::Gemini => ProviderKindView::Gemini,
            ProviderFamily::Qwen => ProviderKindView::Qwen,
            ProviderFamily::Kimi => ProviderKindView::Kimi,
            ProviderFamily::Moonshot => ProviderKindView::Moonshot,
            ProviderFamily::LmStudio => ProviderKindView::LmStudio,
            ProviderFamily::Ollama => ProviderKindView::Ollama,
        };
        let mode = match profile.deployment {
            ProviderDeployment::CloudRemote => ProviderModeView::CloudRemote,
            ProviderDeployment::ExternalEndpoint => ProviderModeView::ExternalEndpoint,
            ProviderDeployment::ManagedChild => ProviderModeView::ManagedChild,
            ProviderDeployment::NativeInProcess => ProviderModeView::Native,
        };
        let model = profile
            .settings
            .0
            .get("model")
            .and_then(serde_json::Value::as_str)
            .map(str::to_owned);
        let mut fingerprint = blake3::Hasher::new();
        fingerprint.update(profile.endpoint.as_deref().unwrap_or("native").as_bytes());
        fingerprint.update(model.as_deref().unwrap_or("default").as_bytes());
        let endpoint_fingerprint = fingerprint.finalize().to_hex().to_string();
        let snapshot = profile.capability_snapshot.as_ref().filter(|snapshot| {
            snapshot.model.as_deref() == model.as_deref()
                && snapshot.is_valid_at(Utc::now(), &endpoint_fingerprint)
        });
        let capabilities = snapshot.map(|snapshot| {
            let values = &snapshot.capabilities;
            let tts = values.tts.as_ref();
            let character = values.character_detection.as_ref();
            let control = values.control.as_ref();
            ProviderCapabilitiesView {
                tts: tts.is_some(),
                character_detection: character.is_some(),
                streaming: tts.is_some_and(|value| value.streaming),
                voice_cloning: tts.is_some_and(|value| {
                    value.voice_cloning.create
                        || value.voice_cloning.update
                        || value.voice_cloning.delete
                        || value.voice_cloning.local_reference_audio
                }),
                pronunciation: tts.is_some_and(|value| {
                    value.pronunciation.provider_dictionary
                        || value.pronunciation.ssml
                        || value.pronunciation.ipa
                        || value.pronunciation.alias
                }),
                process_control: control
                    .is_some_and(|value| value.start || value.stop || value.restart || value.logs),
                model_control: control.is_some_and(|value| {
                    value.list_installed_models
                        || value.download_model
                        || value.delete_model
                        || value.load_model
                        || value.unload_model
                        || value.switch_model
                }),
                model_list: control.is_some_and(|value| value.list_installed_models),
                model_download: control.is_some_and(|value| value.download_model),
                model_delete: control.is_some_and(|value| value.delete_model),
                model_load: control.is_some_and(|value| value.load_model),
                model_unload: control.is_some_and(|value| value.unload_model),
                model_switch: control.is_some_and(|value| value.switch_model),
                temperature: match character.map(|value| value.temperature) {
                    Some(TemperatureCapability::Numeric) => "number",
                    Some(TemperatureCapability::NumericOrNull) => "nullable",
                    Some(TemperatureCapability::Unsupported) | None => "unsupported",
                }
                .to_owned(),
                reasoning: character
                    .map(|value| reasoning_names(&value.reasoning))
                    .unwrap_or_default(),
                max_concurrency: values.recommended_concurrency,
            }
        });
        if let Some(secret_id) = profile.credential_secret_id {
            catalog.provider_secret_ids.insert(id, secret_id);
        }
        catalog.providers.insert(
            id,
            ProviderProfileView {
                id,
                name: profile.name,
                kind,
                mode,
                endpoint: profile.endpoint,
                executable_path: profile.executable_path,
                working_directory: profile.working_directory,
                arguments: profile.arguments,
                status: if matches!(mode, ProviderModeView::Native) {
                    ProviderStatusView::Online
                } else {
                    ProviderStatusView::Offline
                },
                model,
                credential_configured: profile.credential_secret_id.is_some()
                    || matches!(mode, ProviderModeView::Native),
                capabilities,
                capability_source: snapshot.map(|value| value.provenance.source.clone()),
                capability_updated_at: snapshot.map(|value| value.observed_at),
                last_error: None,
            },
        );
    }
    Ok(())
}

fn reasoning_names(capability: &audiobookai_core::ReasoningCapability) -> Vec<String> {
    let mut names = Vec::new();
    if capability.disable {
        names.push("disabled".to_owned());
    }
    if capability.effort {
        names.push("effort".to_owned());
    }
    if capability.adaptive {
        names.push("adaptive".to_owned());
    }
    if capability.token_budget {
        names.push("token_budget".to_owned());
    }
    names
}

async fn hydrate_projects(
    database: &Database,
    catalog: &mut Catalog,
) -> Result<(), crate::ServiceError> {
    let repositories = database.repositories();
    let projects = repositories
        .projects
        .list_projects(false)
        .await
        .map_err(|error| crate::ServiceError::Storage(error.to_string()))?;
    for project in projects {
        let Some(book) = repositories
            .projects
            .get_book(project.book_id)
            .await
            .map_err(|error| crate::ServiceError::Storage(error.to_string()))?
        else {
            continue;
        };
        let chapters = repositories
            .projects
            .list_chapters(book.id)
            .await
            .map_err(|error| crate::ServiceError::Storage(error.to_string()))?;
        let chapter_views = chapters
            .into_iter()
            .map(|chapter| ChapterView {
                id: chapter.id.as_uuid(),
                index: chapter.ordinal as usize,
                title: chapter.title,
                selected: chapter.selected,
                word_count: 0,
                character_count: usize::try_from(chapter.character_count).unwrap_or(usize::MAX),
                estimated_seconds: Some(chapter.character_count.div_ceil(14)),
                status: crate::models::ChapterDisplayStatus::Pending,
            })
            .collect::<Vec<_>>();
        let selected_count = chapter_views
            .iter()
            .filter(|chapter| chapter.selected)
            .count();
        let duration = chapter_views
            .iter()
            .filter(|chapter| chapter.selected)
            .filter_map(|chapter| chapter.estimated_seconds)
            .sum();
        let series = project.metadata.series.as_ref();
        let project_id = project.id.as_uuid();
        catalog
            .project_book_ids
            .insert(project_id, book.id.as_uuid());
        catalog.projects.insert(
            project_id,
            ProjectDetail {
                summary: crate::models::BookSummary {
                    id: project_id,
                    title: project.metadata.title.clone(),
                    author: project.metadata.authors.first().cloned(),
                    cover_url: book
                        .metadata
                        .cover_artifact_id
                        .map(|_| format!("/api/v1/projects/{project_id}/cover")),
                    chapter_count: chapter_views.len(),
                    selected_chapter_count: selected_count,
                    duration_seconds: Some(duration),
                    progress: 0.0,
                    status: match project.status {
                        audiobookai_core::ProjectStatus::Ready => {
                            crate::models::ProjectDisplayStatus::Ready
                        }
                        audiobookai_core::ProjectStatus::Archived => continue,
                        _ => crate::models::ProjectDisplayStatus::Draft,
                    },
                    updated_at: project.updated_at,
                    language: project.metadata.language.clone(),
                    series: series.map(|series| series.name.clone()),
                    series_position: series.and_then(|series| series.position),
                },
                narrator: project.metadata.narrator.clone(),
                publisher: project.metadata.publisher.clone(),
                description: project.metadata.description.clone(),
                consent_cloud_text: project.cloud_consent.book_text,
                consent_cloud_audio: project.cloud_consent.reference_audio,
                chapters: chapter_views,
                character_review_status: if project.character_reviewed_at.is_some() {
                    crate::models::ReviewStatus::Approved
                } else if matches!(
                    project.status,
                    audiobookai_core::ProjectStatus::NeedsCharacterReview
                ) {
                    crate::models::ReviewStatus::NeedsReview
                } else {
                    crate::models::ReviewStatus::NotStarted
                },
                output_name: Some(project.settings.output_name_template.clone()),
            },
        );
    }
    Ok(())
}

#[derive(Clone, Debug)]
struct ParagraphContext {
    project_id: Uuid,
    paragraph: audiobookai_core::Paragraph,
    chapter_title: String,
}

#[allow(clippy::too_many_lines)]
async fn hydrate_characters(
    database: &Database,
    catalog: &mut Catalog,
) -> Result<(), crate::ServiceError> {
    use audiobookai_core::{
        Character, CharacterDetectionRun, DetectionRunStatus, DialogueSpan, Speaker,
        SpeakerOverride, Validate,
    };

    let run_rows = sqlx::query(
        "SELECT id, project_id, provider_id, payload FROM detection_runs WHERE status = 'completed' \
         ORDER BY project_id, completed_at DESC, created_at DESC, id DESC",
    )
    .fetch_all(database.pool())
    .await
    .map_err(storage_error)?;
    let mut latest_runs = HashMap::<Uuid, Uuid>::new();
    let mut projects_with_completed_run = HashSet::<Uuid>::new();
    for row in run_rows {
        let row_id = row.get::<String, _>("id");
        let row_project_id = row.get::<String, _>("project_id");
        let row_provider_id = row.get::<String, _>("provider_id");
        let (Ok(run_id), Ok(project_id), Ok(provider_id)) = (
            Uuid::parse_str(&row_id),
            Uuid::parse_str(&row_project_id),
            Uuid::parse_str(&row_provider_id),
        ) else {
            warn_skipped("detection_run", &row_id, "invalid relational UUID");
            continue;
        };
        if !projects_with_completed_run.insert(project_id)
            || !catalog.projects.contains_key(&project_id)
        {
            continue;
        }
        let payload = row.get::<String, _>("payload");
        let Some(run) =
            decode_optional::<CharacterDetectionRun>(&payload, "detection_run", &row_id)
        else {
            continue;
        };
        if run.id.as_uuid() != run_id
            || run.project_id.as_uuid() != project_id
            || run.provider_profile_id.as_uuid() != provider_id
            || run.status != DetectionRunStatus::Completed
        {
            warn_skipped(
                "detection_run",
                &row_id,
                "payload identity or lifecycle does not match relational columns",
            );
            continue;
        }
        latest_runs.insert(project_id, run_id);
    }

    let alias_rows = sqlx::query(
        "SELECT character_id, alias FROM character_aliases ORDER BY character_id, rowid",
    )
    .fetch_all(database.pool())
    .await
    .map_err(storage_error)?;
    let mut stored_aliases = HashMap::<Uuid, Vec<String>>::new();
    for row in alias_rows {
        let id = row.get::<String, _>("character_id");
        let Ok(id) = Uuid::parse_str(&id) else {
            warn_skipped("character_alias", &id, "invalid character UUID");
            continue;
        };
        let alias = row.get::<String, _>("alias").trim().to_owned();
        if !alias.is_empty() {
            stored_aliases.entry(id).or_default().push(alias);
        }
    }

    let character_rows = sqlx::query(
        "SELECT id, project_id, canonical_name, payload FROM characters \
         ORDER BY project_id, canonical_name, id",
    )
    .fetch_all(database.pool())
    .await
    .map_err(storage_error)?;
    let mut all_character_names = HashMap::<(Uuid, Uuid), String>::new();
    let mut active_character_ids = HashSet::<(Uuid, Uuid)>::new();
    for row in character_rows {
        let row_id = row.get::<String, _>("id");
        let row_project_id = row.get::<String, _>("project_id");
        let (Ok(character_id), Ok(project_id)) =
            (Uuid::parse_str(&row_id), Uuid::parse_str(&row_project_id))
        else {
            warn_skipped("character", &row_id, "invalid relational UUID");
            continue;
        };
        if !catalog.projects.contains_key(&project_id) {
            continue;
        }
        let payload = row.get::<String, _>("payload");
        let Some(mut character) = decode_optional::<Character>(&payload, "character", &row_id)
        else {
            continue;
        };
        if character.id.as_uuid() != character_id
            || character.project_id.as_uuid() != project_id
            || row.get::<String, _>("canonical_name") != character.canonical_name.as_str()
        {
            warn_skipped(
                "character",
                &row_id,
                "payload identity does not match relational columns",
            );
            continue;
        }
        if let Some(aliases) = stored_aliases.get(&character_id) {
            character.aliases = deduplicated_aliases(aliases, &character.canonical_name);
        } else {
            character.aliases = deduplicated_aliases(&character.aliases, &character.canonical_name);
        }
        if !character.validation_issues().is_empty() {
            warn_skipped("character", &row_id, "domain validation failed");
            continue;
        }
        all_character_names.insert((project_id, character_id), character.canonical_name.clone());
        let belongs_to_latest_run = character
            .detection_run_id
            .is_some_and(|run_id| latest_runs.get(&project_id) == Some(&run_id.as_uuid()));
        if !character.manually_created && !belongs_to_latest_run {
            continue;
        }
        active_character_ids.insert((project_id, character_id));
        catalog
            .characters
            .entry(project_id)
            .or_default()
            .push(CharacterView {
                id: character_id,
                canonical_name: character.canonical_name,
                aliases: character.aliases,
                confidence: character.confidence.unwrap_or_default(),
                dialogue_count: 0,
                voice_assignment: None,
                evidence: Vec::new(),
            });
    }
    for characters in catalog.characters.values_mut() {
        characters.sort_by(|left, right| {
            left.canonical_name
                .to_lowercase()
                .cmp(&right.canonical_name.to_lowercase())
                .then_with(|| left.id.cmp(&right.id))
        });
    }

    let paragraph_rows = sqlx::query(
        "SELECT pr.id AS project_id, p.id AS paragraph_id, p.content_hash, \
         p.payload AS paragraph_payload, c.id AS chapter_id, c.payload AS chapter_payload \
         FROM paragraphs p JOIN chapters c ON c.id = p.chapter_id \
         JOIN projects pr ON pr.book_id = c.book_id",
    )
    .fetch_all(database.pool())
    .await
    .map_err(storage_error)?;
    let mut paragraphs = HashMap::<(Uuid, Uuid), ParagraphContext>::new();
    for row in paragraph_rows {
        let project_text = row.get::<String, _>("project_id");
        let paragraph_text = row.get::<String, _>("paragraph_id");
        let chapter_text = row.get::<String, _>("chapter_id");
        let (Ok(project_id), Ok(paragraph_id), Ok(chapter_id)) = (
            Uuid::parse_str(&project_text),
            Uuid::parse_str(&paragraph_text),
            Uuid::parse_str(&chapter_text),
        ) else {
            warn_skipped("paragraph", &paragraph_text, "invalid relational UUID");
            continue;
        };
        if !catalog.projects.contains_key(&project_id) {
            continue;
        }
        let paragraph_payload = row.get::<String, _>("paragraph_payload");
        let chapter_payload = row.get::<String, _>("chapter_payload");
        let Some(paragraph) = decode_optional::<audiobookai_core::Paragraph>(
            &paragraph_payload,
            "paragraph",
            &paragraph_text,
        ) else {
            continue;
        };
        let Some(chapter) = decode_optional::<audiobookai_core::Chapter>(
            &chapter_payload,
            "chapter",
            &chapter_text,
        ) else {
            continue;
        };
        let relational_hash = row.get::<String, _>("content_hash");
        if paragraph.id.as_uuid() != paragraph_id
            || paragraph.chapter_id.as_uuid() != chapter_id
            || chapter.id.as_uuid() != chapter_id
            || paragraph.content_hash != relational_hash
        {
            warn_skipped(
                "paragraph",
                &paragraph_text,
                "payload identity or content hash does not match relational columns",
            );
            continue;
        }
        paragraphs.insert(
            (project_id, paragraph_id),
            ParagraphContext {
                project_id,
                paragraph,
                chapter_title: chapter.title,
            },
        );
    }

    let override_rows = sqlx::query(
        "SELECT id, project_id, paragraph_id, source_content_hash, byte_start, byte_end, payload \
         FROM speaker_overrides ORDER BY updated_at, id",
    )
    .fetch_all(database.pool())
    .await
    .map_err(storage_error)?;
    let mut overrides = HashMap::<(Uuid, Uuid, String, u64, u64), String>::new();
    for row in override_rows {
        let row_id = row.get::<String, _>("id");
        let payload = row.get::<String, _>("payload");
        let Some(record) =
            decode_optional::<SpeakerOverride>(&payload, "speaker_override", &row_id)
        else {
            continue;
        };
        let Ok(record_id) = Uuid::parse_str(&row_id) else {
            warn_skipped("speaker_override", &row_id, "invalid override UUID");
            continue;
        };
        let row_project = row.get::<String, _>("project_id");
        let row_paragraph = row.get::<String, _>("paragraph_id");
        let (Ok(project_id), Ok(paragraph_id)) = (
            Uuid::parse_str(&row_project),
            Uuid::parse_str(&row_paragraph),
        ) else {
            warn_skipped("speaker_override", &row_id, "invalid owner UUID");
            continue;
        };
        let Some(context) = paragraphs.get(&(project_id, paragraph_id)) else {
            continue;
        };
        let row_hash = row.get::<String, _>("source_content_hash");
        let row_start = row.get::<i64, _>("byte_start");
        let row_end = row.get::<i64, _>("byte_end");
        let (Ok(row_start), Ok(row_end)) = (u64::try_from(row_start), u64::try_from(row_end))
        else {
            warn_skipped("speaker_override", &row_id, "negative byte offset");
            continue;
        };
        if record.id.as_uuid() != record_id
            || record.project_id.as_uuid() != project_id
            || record.paragraph_id.as_uuid() != paragraph_id
            || context.project_id != project_id
            || record.source_content_hash != row_hash
            || row_hash != context.paragraph.content_hash
            || record.byte_start != row_start
            || record.byte_end != row_end
            || !valid_text_range(&context.paragraph.text, row_start, row_end)
        {
            warn_skipped(
                "speaker_override",
                &row_id,
                "stale or inconsistent paragraph identity, hash, or byte range",
            );
            continue;
        }
        let speaker_name = match &record.speaker {
            Speaker::Narrator => Some("Narrator".to_owned()),
            Speaker::Character(character_id) => all_character_names
                .get(&(project_id, character_id.as_uuid()))
                .cloned(),
            Speaker::Named(name) if !name.trim().is_empty() => Some(name.trim().to_owned()),
            Speaker::Named(_) => None,
        };
        let Some(speaker_name) = speaker_name else {
            warn_skipped(
                "speaker_override",
                &row_id,
                "speaker no longer belongs to this project",
            );
            continue;
        };
        overrides.insert(
            (project_id, paragraph_id, row_hash, row_start, row_end),
            speaker_name,
        );
    }

    let latest_run_projects = latest_runs
        .iter()
        .map(|(project_id, run_id)| (*run_id, *project_id))
        .collect::<HashMap<_, _>>();
    let span_rows = sqlx::query(
        "SELECT detection_run_id, paragraph_id, character_id, byte_start, byte_end, payload \
         FROM dialogue_spans ORDER BY detection_run_id, paragraph_id, byte_start, byte_end",
    )
    .fetch_all(database.pool())
    .await
    .map_err(storage_error)?;
    for row in span_rows {
        let run_text = row.get::<String, _>("detection_run_id");
        let paragraph_text = row.get::<String, _>("paragraph_id");
        let character_text = row.get::<String, _>("character_id");
        let (Ok(run_id), Ok(paragraph_id), Ok(character_id)) = (
            Uuid::parse_str(&run_text),
            Uuid::parse_str(&paragraph_text),
            Uuid::parse_str(&character_text),
        ) else {
            warn_skipped("dialogue_span", &paragraph_text, "invalid relational UUID");
            continue;
        };
        let Some(project_id) = latest_run_projects.get(&run_id).copied() else {
            continue;
        };
        let Some(context) = paragraphs.get(&(project_id, paragraph_id)) else {
            continue;
        };
        if !active_character_ids.contains(&(context.project_id, character_id)) {
            continue;
        }
        let payload = row.get::<String, _>("payload");
        let Some(span) = decode_optional::<DialogueSpan>(
            &payload,
            "dialogue_span",
            &format!("{run_id}:{paragraph_id}"),
        ) else {
            continue;
        };
        let row_start = row.get::<i64, _>("byte_start");
        let row_end = row.get::<i64, _>("byte_end");
        let (Ok(start), Ok(end)) = (u64::try_from(row_start), u64::try_from(row_end)) else {
            warn_skipped("dialogue_span", &paragraph_text, "negative byte offset");
            continue;
        };
        if span.paragraph_id.as_uuid() != paragraph_id
            || span.character_id.as_uuid() != character_id
            || span.byte_start != start
            || span.byte_end != end
            || !span.confidence.is_finite()
            || !(0.0..=1.0).contains(&span.confidence)
            || !valid_text_range(&context.paragraph.text, start, end)
        {
            warn_skipped(
                "dialogue_span",
                &format!("{run_id}:{paragraph_id}:{start}:{end}"),
                "payload identity, confidence, or byte range is invalid",
            );
            continue;
        }
        let Ok(start_offset) = usize::try_from(start) else {
            continue;
        };
        let Ok(end_offset) = usize::try_from(end) else {
            continue;
        };
        let Some(excerpt) = context.paragraph.text.get(start_offset..end_offset) else {
            continue;
        };
        let evidence = crate::models::DialogueEvidenceView {
            id: stable_derived_uuid(&[
                &run_id.to_string(),
                &paragraph_id.to_string(),
                &character_id.to_string(),
                &start.to_string(),
                &end.to_string(),
            ]),
            paragraph_id,
            chapter_id: context.paragraph.chapter_id.as_uuid(),
            chapter_title: context.chapter_title.clone(),
            excerpt: excerpt.chars().take(240).collect(),
            confidence: span.confidence,
            start_offset,
            end_offset,
            speaker_override: overrides
                .get(&(
                    context.project_id,
                    paragraph_id,
                    context.paragraph.content_hash.clone(),
                    start,
                    end,
                ))
                .cloned(),
        };
        if let Some(character) = catalog
            .characters
            .get_mut(&context.project_id)
            .and_then(|characters| characters.iter_mut().find(|item| item.id == character_id))
        {
            character.evidence.push(evidence);
            character.dialogue_count = character.evidence.len();
        }
    }
    Ok(())
}

#[allow(clippy::too_many_lines)]
async fn hydrate_voices(
    database: &Database,
    catalog: &mut Catalog,
) -> Result<(), crate::ServiceError> {
    use crate::models::{VoiceAssignmentView, VoiceKindView};
    use audiobookai_core::{
        Character, Speaker, Validate, VoiceAssignment, VoiceOrigin, VoiceOwnership, VoiceProfile,
    };

    let profile_rows = sqlx::query(
        "SELECT id, provider_id, name, origin, ownership, provider_voice_id, payload \
         FROM voice_profiles ORDER BY provider_id, name, id",
    )
    .fetch_all(database.pool())
    .await
    .map_err(storage_error)?;
    let mut profiles = HashMap::<Uuid, VoiceProfile>::new();
    for row in profile_rows {
        let row_id = row.get::<String, _>("id");
        let row_provider = row.get::<String, _>("provider_id");
        let (Ok(id), Ok(provider_id)) = (Uuid::parse_str(&row_id), Uuid::parse_str(&row_provider))
        else {
            warn_skipped("voice_profile", &row_id, "invalid relational UUID");
            continue;
        };
        if !catalog.providers.contains_key(&provider_id) {
            continue;
        }
        let payload = row.get::<String, _>("payload");
        let Some(profile) = decode_optional::<VoiceProfile>(&payload, "voice_profile", &row_id)
        else {
            continue;
        };
        if profile.id.as_uuid() != id
            || profile.provider_profile_id.as_uuid() != provider_id
            || row.get::<String, _>("name") != profile.name.as_str()
            || row.get::<String, _>("origin") != voice_origin_name(profile.origin)
            || row.get::<String, _>("ownership") != voice_ownership_name(profile.ownership)
            || !profile.validation_issues().is_empty()
        {
            warn_skipped(
                "voice_profile",
                &row_id,
                "payload identity or domain validation is invalid",
            );
            continue;
        }
        let relational_source = row.get::<Option<String>, _>("provider_voice_id");
        if relational_source != profile.provider_voice_id {
            warn_skipped(
                "voice_profile",
                &row_id,
                "provider voice ID does not match relational column",
            );
            continue;
        }
        let source_id = profile
            .provider_voice_id
            .clone()
            .or_else(|| setting_string(&profile.settings, "sourceId"))
            .or_else(|| setting_string(&profile.settings, "referenceAudioPath"))
            .unwrap_or_else(|| profile.id.to_string());
        let preview_url = setting_string(&profile.settings, "previewUrl");
        let gender = setting_string(&profile.settings, "gender");
        catalog.voice_sources.insert(id, source_id);
        catalog.voices.push(VoiceView {
            id,
            provider_profile_id: provider_id,
            name: profile.name.clone(),
            locale: profile.language.clone(),
            gender,
            kind: match profile.origin {
                VoiceOrigin::ProviderCatalog => VoiceKindView::Catalog,
                VoiceOrigin::LocalReference => VoiceKindView::LocalReference,
                VoiceOrigin::ProviderClone => VoiceKindView::RemoteClone,
                VoiceOrigin::NativeSystem => VoiceKindView::Native,
            },
            owned: profile.ownership == VoiceOwnership::AudiobookAi,
            preview_url,
        });
        profiles.insert(id, profile);
    }
    catalog.voices.sort_by(|left, right| {
        left.provider_profile_id
            .cmp(&right.provider_profile_id)
            .then_with(|| left.name.to_lowercase().cmp(&right.name.to_lowercase()))
            .then_with(|| left.id.cmp(&right.id))
    });

    let character_rows = sqlx::query("SELECT id, project_id, payload FROM characters")
        .fetch_all(database.pool())
        .await
        .map_err(storage_error)?;
    let mut stored_character_names = HashMap::<(Uuid, Uuid), String>::new();
    for row in character_rows {
        let row_id = row.get::<String, _>("id");
        let payload = row.get::<String, _>("payload");
        let Some(character) = decode_optional::<Character>(&payload, "character", &row_id) else {
            continue;
        };
        stored_character_names.insert(
            (character.project_id.as_uuid(), character.id.as_uuid()),
            character.canonical_name,
        );
    }

    let assignment_rows = sqlx::query(
        "SELECT id, project_id, provider_id, voice_profile_id, speaker_key, payload \
         FROM voice_assignments ORDER BY updated_at, id",
    )
    .fetch_all(database.pool())
    .await
    .map_err(storage_error)?;
    for row in assignment_rows {
        let row_id = row.get::<String, _>("id");
        let payload = row.get::<String, _>("payload");
        let Some(assignment) =
            decode_optional::<VoiceAssignment>(&payload, "voice_assignment", &row_id)
        else {
            continue;
        };
        let row_project = row.get::<String, _>("project_id");
        let row_provider = row.get::<String, _>("provider_id");
        let row_voice = row.get::<String, _>("voice_profile_id");
        let (Ok(id), Ok(project_id), Ok(provider_id), Ok(voice_id)) = (
            Uuid::parse_str(&row_id),
            Uuid::parse_str(&row_project),
            Uuid::parse_str(&row_provider),
            Uuid::parse_str(&row_voice),
        ) else {
            warn_skipped("voice_assignment", &row_id, "invalid relational UUID");
            continue;
        };
        let Some(profile) = profiles.get(&voice_id) else {
            continue;
        };
        let Some(voice) = catalog.voices.iter().find(|voice| voice.id == voice_id) else {
            continue;
        };
        if assignment.id.as_uuid() != id
            || assignment.project_id.as_uuid() != project_id
            || assignment.provider_profile_id.as_uuid() != provider_id
            || assignment.voice_profile_id.as_uuid() != voice_id
            || profile.provider_profile_id.as_uuid() != provider_id
            || row.get::<String, _>("speaker_key") != speaker_key(&assignment.speaker)
        {
            warn_skipped(
                "voice_assignment",
                &row_id,
                "payload identity does not match relational columns or voice profile",
            );
            continue;
        }
        let target_id = match &assignment.speaker {
            Speaker::Narrator => active_character_by_name(catalog, project_id, "narrator"),
            Speaker::Character(character_id) => catalog
                .characters
                .get(&project_id)
                .and_then(|characters| {
                    characters
                        .iter()
                        .find(|character| character.id == character_id.as_uuid())
                        .map(|character| character.id)
                })
                .or_else(|| {
                    stored_character_names
                        .get(&(project_id, character_id.as_uuid()))
                        .and_then(|name| active_character_by_name(catalog, project_id, name))
                }),
            Speaker::Named(name) => active_character_by_name(catalog, project_id, name),
        };
        let Some(target_id) = target_id else {
            warn_skipped(
                "voice_assignment",
                &row_id,
                "speaker no longer maps to an active project character",
            );
            continue;
        };
        let provider_name = catalog.providers.get(&provider_id).map_or_else(
            || "Unknown provider".to_owned(),
            |provider| provider.name.clone(),
        );
        if let Some(character) = catalog
            .characters
            .get_mut(&project_id)
            .and_then(|characters| characters.iter_mut().find(|item| item.id == target_id))
        {
            character.voice_assignment = Some(VoiceAssignmentView {
                provider_profile_id: provider_id,
                provider_name,
                voice_id,
                voice_name: voice.name.clone(),
                model: assignment.model.clone().or_else(|| profile.model.clone()),
            });
        }
    }
    Ok(())
}

#[allow(clippy::too_many_lines)]
async fn hydrate_pronunciation_rules(
    database: &Database,
    catalog: &mut Catalog,
) -> Result<(), crate::ServiceError> {
    use crate::models::{PronunciationKindView, PronunciationScopeView};
    use audiobookai_core::{
        DictionaryRule, DictionaryRuleKind, DictionaryScope, PronunciationDictionary, Validate,
    };

    let dictionary_rows = sqlx::query(
        "SELECT id, project_id, scope, revision, enabled, payload FROM dictionaries \
         ORDER BY updated_at, id",
    )
    .fetch_all(database.pool())
    .await
    .map_err(storage_error)?;
    let mut dictionaries = HashMap::<Uuid, PronunciationDictionary>::new();
    for row in dictionary_rows {
        let row_id = row.get::<String, _>("id");
        let payload = row.get::<String, _>("payload");
        let Some(dictionary) = decode_optional::<PronunciationDictionary>(
            &payload,
            "pronunciation_dictionary",
            &row_id,
        ) else {
            continue;
        };
        let Ok(id) = Uuid::parse_str(&row_id) else {
            warn_skipped(
                "pronunciation_dictionary",
                &row_id,
                "invalid dictionary UUID",
            );
            continue;
        };
        let row_project_id = row
            .get::<Option<String>, _>("project_id")
            .as_deref()
            .map(Uuid::parse_str)
            .transpose();
        let Ok(row_project_id) = row_project_id else {
            warn_skipped("pronunciation_dictionary", &row_id, "invalid project UUID");
            continue;
        };
        let scope = row.get::<String, _>("scope");
        let expected_scope = match dictionary.scope {
            DictionaryScope::Global => "global",
            DictionaryScope::Project => "project",
        };
        let relational_revision = row.get::<i64, _>("revision");
        let Ok(relational_revision) = u64::try_from(relational_revision) else {
            warn_skipped(
                "pronunciation_dictionary",
                &row_id,
                "dictionary revision is negative",
            );
            continue;
        };
        if dictionary.id.as_uuid() != id
            || dictionary
                .project_id
                .map(audiobookai_core::ProjectId::as_uuid)
                != row_project_id
            || scope != expected_scope
            || dictionary.revision != relational_revision
            || dictionary.enabled != row.get::<bool, _>("enabled")
            || !dictionary.validation_issues().is_empty()
            || row_project_id.is_some_and(|id| !catalog.projects.contains_key(&id))
        {
            warn_skipped(
                "pronunciation_dictionary",
                &row_id,
                "payload identity, scope, or project ownership is invalid",
            );
            continue;
        }
        dictionaries.insert(id, dictionary);
    }

    let rule_rows = sqlx::query(
        "SELECT id, dictionary_id, kind, enabled, payload FROM dictionary_rules \
         ORDER BY dictionary_id, ordinal, id",
    )
    .fetch_all(database.pool())
    .await
    .map_err(storage_error)?;
    for row in rule_rows {
        let row_id = row.get::<String, _>("id");
        let dictionary_text = row.get::<String, _>("dictionary_id");
        let (Ok(id), Ok(dictionary_id)) =
            (Uuid::parse_str(&row_id), Uuid::parse_str(&dictionary_text))
        else {
            warn_skipped("dictionary_rule", &row_id, "invalid relational UUID");
            continue;
        };
        let Some(dictionary) = dictionaries.get(&dictionary_id) else {
            continue;
        };
        let payload = row.get::<String, _>("payload");
        let Some(rule) = decode_optional::<DictionaryRule>(&payload, "dictionary_rule", &row_id)
        else {
            continue;
        };
        if rule.id.as_uuid() != id
            || rule.dictionary_id.as_uuid() != dictionary_id
            || row.get::<String, _>("kind") != dictionary_rule_kind_name(rule.kind)
            || row.get::<bool, _>("enabled") != rule.enabled
            || !rule.validation_issues().is_empty()
        {
            warn_skipped(
                "dictionary_rule",
                &row_id,
                "payload identity or domain validation is invalid",
            );
            continue;
        }
        let project_id = dictionary
            .project_id
            .map(audiobookai_core::ProjectId::as_uuid);
        if let Some(character_id) = rule
            .character_id
            .map(audiobookai_core::CharacterId::as_uuid)
            && !catalog
                .characters
                .iter()
                .any(|(candidate_project, characters)| {
                    project_id.is_none_or(|project_id| project_id == *candidate_project)
                        && characters
                            .iter()
                            .any(|character| character.id == character_id)
                })
        {
            warn_skipped(
                "dictionary_rule",
                &row_id,
                "character scope no longer maps to an active character",
            );
            continue;
        }
        catalog.pronunciation_rules.push(PronunciationRuleView {
            id,
            scope: match dictionary.scope {
                DictionaryScope::Global => PronunciationScopeView::Global,
                DictionaryScope::Project => PronunciationScopeView::Project,
            },
            kind: match rule.kind {
                DictionaryRuleKind::Literal => PronunciationKindView::Literal,
                DictionaryRuleKind::WholeWord => PronunciationKindView::WholeWord,
                DictionaryRuleKind::Regex => PronunciationKindView::Regex,
                DictionaryRuleKind::Alias => PronunciationKindView::Alias,
                DictionaryRuleKind::Phoneme => PronunciationKindView::Phoneme,
            },
            source: rule.pattern,
            replacement: rule.replacement,
            language: rule.language,
            character_id: rule
                .character_id
                .map(audiobookai_core::CharacterId::as_uuid),
            case_sensitive: rule.case_sensitive,
            enabled: dictionary.enabled && rule.enabled,
            order: rule.ordinal,
            conflict: None,
            project_id,
        });
    }
    catalog.pronunciation_rules.sort_by_key(|rule| {
        (
            matches!(rule.scope, PronunciationScopeView::Project),
            rule.project_id,
            rule.order,
            rule.id,
        )
    });
    let mut first_rules =
        HashMap::<(Option<Uuid>, Option<String>, Option<Uuid>, String), Uuid>::new();
    for rule in &mut catalog.pronunciation_rules {
        if !rule.enabled {
            continue;
        }
        let key = (
            rule.project_id,
            rule.language.as_ref().map(|value| value.to_lowercase()),
            rule.character_id,
            rule.source.to_lowercase(),
        );
        if let Some(first_id) = first_rules.get(&key) {
            rule.conflict = Some(format!("overlaps rule {first_id}"));
        } else {
            first_rules.insert(key, rule.id);
        }
    }
    Ok(())
}

#[allow(clippy::too_many_lines)]
async fn hydrate_jobs(
    database: &Database,
    catalog: &mut Catalog,
) -> Result<(), crate::ServiceError> {
    use crate::models::{
        JobStageView, JobStatusView, JobUnitStatusView, JobUnitView, ProjectDisplayStatus,
    };
    use audiobookai_core::{
        Job, JobAttempt, JobState, JobUnit, JobUnitKind, JobUnitState, Validate,
    };

    let attempt_rows = sqlx::query(
        "SELECT id, job_unit_id, ordinal, uncertain_charge, payload FROM job_attempts \
         ORDER BY job_unit_id, ordinal, started_at",
    )
    .fetch_all(database.pool())
    .await
    .map_err(storage_error)?;
    let mut latest_attempts = HashMap::<Uuid, JobAttempt>::new();
    let mut uncertain_units = HashSet::<Uuid>::new();
    for row in attempt_rows {
        let row_id = row.get::<String, _>("id");
        let row_unit_id = row.get::<String, _>("job_unit_id");
        let (Ok(id), Ok(unit_id)) = (Uuid::parse_str(&row_id), Uuid::parse_str(&row_unit_id))
        else {
            warn_skipped("job_attempt", &row_id, "invalid relational UUID");
            continue;
        };
        if row.get::<bool, _>("uncertain_charge") {
            // Preserve a possible provider charge even if the optional attempt detail is damaged.
            uncertain_units.insert(unit_id);
        }
        let row_ordinal = row.get::<i64, _>("ordinal");
        let Ok(row_ordinal) = u16::try_from(row_ordinal) else {
            warn_skipped("job_attempt", &row_id, "attempt ordinal is out of range");
            continue;
        };
        let payload = row.get::<String, _>("payload");
        let Some(attempt) = decode_optional::<JobAttempt>(&payload, "job_attempt", &row_id) else {
            continue;
        };
        if attempt.id.as_uuid() != id
            || attempt.job_unit_id.as_uuid() != unit_id
            || attempt.ordinal != row_ordinal
            || attempt.uncertain_charge != row.get::<bool, _>("uncertain_charge")
        {
            warn_skipped(
                "job_attempt",
                &row_id,
                "payload identity or billing state does not match relational columns",
            );
            continue;
        }
        if latest_attempts
            .get(&unit_id)
            .is_none_or(|stored| stored.ordinal <= attempt.ordinal)
        {
            latest_attempts.insert(unit_id, attempt);
        }
    }

    let unit_rows =
        sqlx::query("SELECT id, job_id, state, payload FROM job_units ORDER BY job_id, rowid")
            .fetch_all(database.pool())
            .await
            .map_err(storage_error)?;
    let mut units_by_job = HashMap::<Uuid, Vec<JobUnit>>::new();
    let mut jobs_with_corrupt_units = HashSet::<Uuid>::new();
    for row in unit_rows {
        let row_id = row.get::<String, _>("id");
        let row_job = row.get::<String, _>("job_id");
        let (Ok(id), Ok(job_id)) = (Uuid::parse_str(&row_id), Uuid::parse_str(&row_job)) else {
            warn_skipped("job_unit", &row_id, "invalid relational UUID");
            continue;
        };
        let payload = row.get::<String, _>("payload");
        let Some(unit) = decode_optional::<JobUnit>(&payload, "job_unit", &row_id) else {
            jobs_with_corrupt_units.insert(job_id);
            continue;
        };
        if unit.id.as_uuid() != id
            || unit.job_id.as_uuid() != job_id
            || row.get::<String, _>("state") != job_unit_state_name(unit.state)
        {
            jobs_with_corrupt_units.insert(job_id);
            warn_skipped(
                "job_unit",
                &row_id,
                "payload identity does not match relational columns",
            );
            continue;
        }
        units_by_job.entry(job_id).or_default().push(unit);
    }

    let job_rows = sqlx::query(
        "SELECT id, project_id, state, revision, payload FROM jobs ORDER BY updated_at, id",
    )
    .fetch_all(database.pool())
    .await
    .map_err(storage_error)?;
    for row in job_rows {
        let row_id = row.get::<String, _>("id");
        let row_project = row.get::<String, _>("project_id");
        let relational_state = row.get::<String, _>("state");
        let active = !terminal_job_state_name(&relational_state);
        let (Ok(id), Ok(project_id)) = (Uuid::parse_str(&row_id), Uuid::parse_str(&row_project))
        else {
            if active {
                return Err(unsafe_active_job_corruption(
                    &row_id,
                    "relational owner UUID is invalid",
                ));
            }
            warn_skipped("job", &row_id, "invalid relational UUID");
            continue;
        };
        let Some(project) = catalog.projects.get(&project_id) else {
            continue;
        };
        let payload = row.get::<String, _>("payload");
        let Some(mut job) = decode_optional::<Job>(&payload, "job", &row_id) else {
            if active {
                return Err(unsafe_active_job_corruption(
                    &row_id,
                    "domain payload cannot be decoded",
                ));
            }
            continue;
        };
        let revision = row.get::<i64, _>("revision");
        let Ok(revision) = u64::try_from(revision) else {
            if active {
                return Err(unsafe_active_job_corruption(
                    &row_id,
                    "revision is negative",
                ));
            }
            warn_skipped("job", &row_id, "negative revision");
            continue;
        };
        job.revision = revision;
        if job.id.as_uuid() != id
            || job.project_id.as_uuid() != project_id
            || relational_state != job_state_name(job.state)
            || !job.validation_issues().is_empty()
        {
            if active {
                return Err(unsafe_active_job_corruption(
                    &row_id,
                    "identity, lifecycle, or domain validation is inconsistent",
                ));
            }
            warn_skipped(
                "job",
                &row_id,
                "payload identity or lifecycle does not match relational columns",
            );
            continue;
        }
        if active && jobs_with_corrupt_units.contains(&id) {
            return Err(unsafe_active_job_corruption(
                &row_id,
                "one or more durable units are corrupt",
            ));
        }
        let domain_units = units_by_job.remove(&id).unwrap_or_default();
        let units = domain_units
            .iter()
            .map(|unit| {
                let latest_attempt = latest_attempts.get(&unit.id.as_uuid());
                JobUnitView {
                    id: unit.id.as_uuid(),
                    title: job_unit_title(unit, catalog),
                    stage: match unit.kind {
                        JobUnitKind::DetectionBatch => JobStageView::Detect,
                        JobUnitKind::SynthesisSegment => JobStageView::Synthesize,
                        JobUnitKind::ChapterAssembly => JobStageView::Assemble,
                        JobUnitKind::MusicMix => JobStageView::Mix,
                        JobUnitKind::Normalization => JobStageView::Normalize,
                        JobUnitKind::FinalExport => JobStageView::Export,
                    },
                    status: match unit.state {
                        JobUnitState::Blocked | JobUnitState::Ready | JobUnitState::Retrying => {
                            JobUnitStatusView::Queued
                        }
                        JobUnitState::Running => JobUnitStatusView::Running,
                        JobUnitState::Paused => JobUnitStatusView::Paused,
                        JobUnitState::Cancelled => JobUnitStatusView::Cancelled,
                        JobUnitState::Failed => JobUnitStatusView::Failed,
                        JobUnitState::Completed => JobUnitStatusView::Complete,
                    },
                    progress: if unit.state == JobUnitState::Completed {
                        1.0
                    } else {
                        unit.payload
                            .get("progress")
                            .and_then(serde_json::Value::as_f64)
                            .and_then(|value| value.to_string().parse::<f32>().ok())
                            .unwrap_or_default()
                            .clamp(0.0, 1.0)
                    },
                    attempt: u32::from(unit.attempt_count),
                    last_error: unit
                        .payload
                        .get("lastError")
                        .and_then(serde_json::Value::as_str)
                        .map(str::to_owned)
                        .or_else(|| {
                            latest_attempt.and_then(|attempt| attempt.redacted_error.clone())
                        }),
                }
            })
            .collect::<Vec<_>>();
        let unit_progress = if units.is_empty() {
            0.0
        } else {
            ratio_f32(
                units
                    .iter()
                    .filter(|unit| matches!(unit.status, JobUnitStatusView::Complete))
                    .count()
                    .try_into()
                    .unwrap_or(u64::MAX),
                units.len().try_into().unwrap_or(u64::MAX),
            )
        };
        let domain_progress = ratio_f32(job.progress_completed, job.progress_total);
        let status = match job.state {
            JobState::Queued => JobStatusView::Queued,
            JobState::Running => JobStatusView::Running,
            JobState::Pausing | JobState::Cancelling => JobStatusView::Pausing,
            JobState::Paused => JobStatusView::Paused,
            JobState::Cancelled => JobStatusView::Cancelled,
            JobState::Failed => JobStatusView::Failed,
            JobState::Completed => JobStatusView::Complete,
        };
        let progress = if job.state == JobState::Completed {
            1.0
        } else {
            domain_progress.max(unit_progress)
        };
        let current_stage = job.status_message.clone().or_else(|| {
            units
                .iter()
                .find(|unit| {
                    matches!(
                        unit.status,
                        JobUnitStatusView::Running | JobUnitStatusView::Queued
                    )
                })
                .map(|unit| unit.title.clone())
        });
        let uncertain_charge = domain_units
            .iter()
            .any(|unit| uncertain_units.contains(&unit.id.as_uuid()));
        catalog.jobs.insert(
            id,
            JobView {
                id,
                project_id,
                project_title: project.summary.title.clone(),
                status,
                progress,
                current_stage,
                started_at: job.started_at,
                updated_at: job.updated_at,
                estimated_remaining_seconds: None,
                units,
                progressive_playback_url: None,
                uncertain_charge,
            },
        );
    }

    let mut latest_project_jobs = HashMap::<Uuid, JobView>::new();
    for job in catalog.jobs.values() {
        let replace = latest_project_jobs
            .get(&job.project_id)
            .is_none_or(|stored| stored.updated_at < job.updated_at);
        if replace {
            latest_project_jobs.insert(job.project_id, job.clone());
        }
    }
    for (project_id, job) in latest_project_jobs {
        let Some(project) = catalog.projects.get_mut(&project_id) else {
            continue;
        };
        project.summary.progress = job.progress;
        project.summary.status = match job.status {
            JobStatusView::Queued
            | JobStatusView::Running
            | JobStatusView::Pausing
            | JobStatusView::Paused => ProjectDisplayStatus::Processing,
            JobStatusView::Complete => ProjectDisplayStatus::Completed,
            JobStatusView::Failed => ProjectDisplayStatus::Failed,
            JobStatusView::Cancelled => project.summary.status,
        };
    }
    Ok(())
}

async fn hydrate_usage(
    database: &Database,
    catalog: &mut Catalog,
) -> Result<(), crate::ServiceError> {
    use audiobookai_core::{ProvenanceQuality, UsageEvent, UsageWorkload, Validate};

    let rows = sqlx::query(
        "SELECT id, project_id, provider_id, workload, payload FROM usage_ledger \
         ORDER BY sequence DESC",
    )
    .fetch_all(database.pool())
    .await
    .map_err(storage_error)?;
    for row in rows {
        let row_id = row.get::<String, _>("id");
        let payload = row.get::<String, _>("payload");
        let Some(event) = decode_optional::<UsageEvent>(&payload, "usage_event", &row_id) else {
            continue;
        };
        let row_project_id = row.get::<String, _>("project_id");
        let row_provider_id = row.get::<String, _>("provider_id");
        let (Ok(id), Ok(project_id), Ok(provider_id)) = (
            Uuid::parse_str(&row_id),
            Uuid::parse_str(&row_project_id),
            Uuid::parse_str(&row_provider_id),
        ) else {
            warn_skipped("usage_event", &row_id, "invalid usage UUID");
            continue;
        };
        if event.id.as_uuid() != id
            || event.project_id.as_uuid() != project_id
            || event.provider_profile_id.as_uuid() != provider_id
            || row.get::<String, _>("workload") != usage_workload_name(event.workload)
            || !event.validation_issues().is_empty()
        {
            warn_skipped(
                "usage_event",
                &row_id,
                "payload identity or domain validation is invalid",
            );
            continue;
        }
        let project_title = catalog
            .projects
            .get(&project_id)
            .map(|project| project.summary.title.clone());
        let provider_name = catalog.providers.get(&provider_id).map_or_else(
            || event.provider_family.clone(),
            |provider| provider.name.clone(),
        );
        let voice = event.voice_profile_id.and_then(|voice_id| {
            catalog
                .voices
                .iter()
                .find(|voice| voice.id == voice_id.as_uuid())
                .map(|voice| voice.name.clone())
        });
        catalog.usage_rows.push(UsageRowView {
            id,
            occurred_at: event.occurred_at,
            project_title,
            provider_name,
            operation: match event.workload {
                UsageWorkload::Tts => "tts",
                UsageWorkload::CharacterDetection => "character_detection",
            }
            .to_owned(),
            model: event.model,
            voice,
            characters: event.quantities.characters,
            input_tokens: event.quantities.input_tokens,
            output_tokens: event.quantities.output_tokens,
            cost_micros: event.cost.as_ref().map(|cost| cost.micros),
            currency: event.cost.map(|cost| cost.currency),
            provenance: match event.quantity_source {
                ProvenanceQuality::Reported => "reported",
                ProvenanceQuality::Estimated => "estimated",
                ProvenanceQuality::Derived => "derived",
                ProvenanceQuality::Unknown => "unknown",
            }
            .to_owned(),
            request_id: event.provider_request_id,
        });
    }
    Ok(())
}

fn deduplicated_aliases(aliases: &[String], canonical_name: &str) -> Vec<String> {
    let mut seen = HashSet::new();
    aliases
        .iter()
        .filter_map(|alias| {
            let alias = alias.trim();
            let normalized = alias.to_lowercase();
            (!alias.is_empty()
                && !alias.eq_ignore_ascii_case(canonical_name)
                && seen.insert(normalized))
            .then(|| alias.to_owned())
        })
        .collect()
}

fn active_character_by_name(catalog: &Catalog, project_id: Uuid, name: &str) -> Option<Uuid> {
    catalog
        .characters
        .get(&project_id)?
        .iter()
        .find(|character| {
            character.canonical_name.eq_ignore_ascii_case(name)
                || character
                    .aliases
                    .iter()
                    .any(|alias| alias.eq_ignore_ascii_case(name))
        })
        .map(|character| character.id)
}

fn speaker_key(speaker: &audiobookai_core::Speaker) -> String {
    match speaker {
        audiobookai_core::Speaker::Narrator => "narrator".to_owned(),
        audiobookai_core::Speaker::Character(id) => format!("character:{id}"),
        audiobookai_core::Speaker::Named(name) => format!("named:{}", name.to_lowercase()),
    }
}

const fn voice_origin_name(origin: audiobookai_core::VoiceOrigin) -> &'static str {
    match origin {
        audiobookai_core::VoiceOrigin::ProviderCatalog => "provider_catalog",
        audiobookai_core::VoiceOrigin::LocalReference => "local_reference",
        audiobookai_core::VoiceOrigin::ProviderClone => "provider_clone",
        audiobookai_core::VoiceOrigin::NativeSystem => "native_system",
    }
}

const fn voice_ownership_name(ownership: audiobookai_core::VoiceOwnership) -> &'static str {
    match ownership {
        audiobookai_core::VoiceOwnership::Provider => "provider",
        audiobookai_core::VoiceOwnership::User => "user",
        audiobookai_core::VoiceOwnership::AudiobookAi => "audiobook_ai",
    }
}

const fn dictionary_rule_kind_name(kind: audiobookai_core::DictionaryRuleKind) -> &'static str {
    match kind {
        audiobookai_core::DictionaryRuleKind::Literal => "literal",
        audiobookai_core::DictionaryRuleKind::WholeWord => "whole_word",
        audiobookai_core::DictionaryRuleKind::Regex => "regex",
        audiobookai_core::DictionaryRuleKind::Alias => "alias",
        audiobookai_core::DictionaryRuleKind::Phoneme => "phoneme",
    }
}

fn setting_string(settings: &BTreeMap<String, serde_json::Value>, key: &str) -> Option<String> {
    settings
        .get(key)
        .and_then(serde_json::Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_owned)
}

fn job_unit_title(unit: &audiobookai_core::JobUnit, catalog: &Catalog) -> String {
    use audiobookai_core::JobUnitKind;

    match unit.kind {
        JobUnitKind::DetectionBatch => unit
            .payload
            .get("batchIndex")
            .and_then(serde_json::Value::as_u64)
            .map_or_else(
                || "Detect characters".to_owned(),
                |index| format!("Detection batch {}", index.saturating_add(1)),
            ),
        JobUnitKind::SynthesisSegment => unit.chapter_id.map_or_else(
            || "Synthesize segment".to_owned(),
            |chapter_id| {
                let title = catalog
                    .projects
                    .values()
                    .flat_map(|project| &project.chapters)
                    .find(|chapter| chapter.id == chapter_id.as_uuid())
                    .map_or("chapter", |chapter| chapter.title.as_str());
                format!("Synthesize {title}")
            },
        ),
        JobUnitKind::ChapterAssembly => "Assemble chapter".to_owned(),
        JobUnitKind::MusicMix => "Mix background music".to_owned(),
        JobUnitKind::Normalization => "Normalize audio".to_owned(),
        JobUnitKind::FinalExport => "Export audiobook".to_owned(),
    }
}

const fn job_state_name(state: audiobookai_core::JobState) -> &'static str {
    use audiobookai_core::JobState;

    match state {
        JobState::Queued => "queued",
        JobState::Running => "running",
        JobState::Pausing => "pausing",
        JobState::Paused => "paused",
        JobState::Cancelling => "cancelling",
        JobState::Cancelled => "cancelled",
        JobState::Failed => "failed",
        JobState::Completed => "completed",
    }
}

fn terminal_job_state_name(state: &str) -> bool {
    matches!(state, "cancelled" | "failed" | "completed")
}

fn unsafe_active_job_corruption(id: &str, reason: &str) -> crate::ServiceError {
    crate::ServiceError::Storage(format!(
        "active job {id} is corrupt and cannot be resumed safely: {reason}"
    ))
}

const fn job_unit_state_name(state: audiobookai_core::JobUnitState) -> &'static str {
    use audiobookai_core::JobUnitState;

    match state {
        JobUnitState::Blocked => "blocked",
        JobUnitState::Ready => "ready",
        JobUnitState::Running => "running",
        JobUnitState::Retrying => "retrying",
        JobUnitState::Paused => "paused",
        JobUnitState::Cancelled => "cancelled",
        JobUnitState::Failed => "failed",
        JobUnitState::Completed => "completed",
    }
}

const fn usage_workload_name(workload: audiobookai_core::UsageWorkload) -> &'static str {
    match workload {
        audiobookai_core::UsageWorkload::Tts => "tts",
        audiobookai_core::UsageWorkload::CharacterDetection => "character_detection",
    }
}

fn ratio_f32(numerator: u64, denominator: u64) -> f32 {
    if denominator == 0 {
        return 0.0;
    }
    let basis_points = numerator
        .min(denominator)
        .saturating_mul(10_000)
        .checked_div(denominator)
        .unwrap_or_default();
    f32::from(u16::try_from(basis_points).unwrap_or(10_000)) / 10_000.0
}

fn valid_text_range(text: &str, start: u64, end: u64) -> bool {
    let (Ok(start), Ok(end)) = (usize::try_from(start), usize::try_from(end)) else {
        return false;
    };
    start < end && end <= text.len() && text.is_char_boundary(start) && text.is_char_boundary(end)
}

fn stable_derived_uuid(parts: &[&str]) -> Uuid {
    let mut hasher = blake3::Hasher::new();
    for part in parts {
        hasher.update(part.as_bytes());
        hasher.update(&[0]);
    }
    let hash = hasher.finalize();
    let mut bytes = [0_u8; 16];
    bytes.copy_from_slice(&hash.as_bytes()[..16]);
    Uuid::from_bytes(bytes)
}

fn decode_optional<T: DeserializeOwned>(payload: &str, entity: &str, id: &str) -> Option<T> {
    match serde_json::from_str(payload) {
        Ok(value) => Some(value),
        Err(error) => {
            tracing::warn!(diagnostic_code = "storage.record.corrupt", entity, id, %error, "skipping corrupt optional persisted record");
            None
        }
    }
}

fn warn_skipped(entity: &str, id: &str, reason: &str) {
    tracing::warn!(
        diagnostic_code = "storage.record.inconsistent",
        entity,
        id,
        reason,
        "skipping inconsistent optional persisted record"
    );
}

#[allow(clippy::needless_pass_by_value)]
fn storage_error(error: sqlx::Error) -> crate::ServiceError {
    crate::ServiceError::Storage(error.to_string())
}

fn native_provider() -> ProviderProfileView {
    use crate::models::{
        ProviderCapabilitiesView, ProviderKindView, ProviderModeView, ProviderStatusView,
    };
    ProviderProfileView {
        id: Uuid::parse_str(match std::env::consts::OS {
            "macos" => "9f85e64a-f687-4e86-8b6c-fc71938249eb",
            "windows" => "5dd70ee1-eb54-430e-bb3b-e4bb31d7ee91",
            _ => "e76afdb2-3458-46cb-874b-1c242d1336d9",
        })
        .expect("constant native provider UUID"),
        name: match std::env::consts::OS {
            "macos" => "macOS Speech",
            "windows" => "Windows Speech",
            _ => "eSpeak NG",
        }
        .to_owned(),
        kind: ProviderKindView::NativeOs,
        mode: ProviderModeView::Native,
        endpoint: None,
        executable_path: None,
        working_directory: None,
        arguments: Vec::new(),
        status: ProviderStatusView::Online,
        model: None,
        credential_configured: true,
        capabilities: Some(ProviderCapabilitiesView {
            tts: true,
            character_detection: false,
            streaming: cfg!(target_os = "macos"),
            voice_cloning: false,
            pronunciation: true,
            process_control: false,
            model_control: false,
            model_list: false,
            model_download: false,
            model_delete: false,
            model_load: false,
            model_unload: false,
            model_switch: false,
            temperature: "unsupported".to_owned(),
            reasoning: Vec::new(),
            max_concurrency: Some(1),
        }),
        capability_source: Some("native_runtime".to_owned()),
        capability_updated_at: Some(Utc::now()),
        last_error: None,
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use audiobookai_core::{
        AttemptId, Book, BookId, BookMetadata, Chapter, ChapterId, Character,
        CharacterDetectionRun, CharacterId, CloudConsent, DetectionRunId, DetectionRunStatus,
        DialogueSpan, DictionaryId, DictionaryRule, DictionaryRuleId, DictionaryRuleKind,
        DictionaryScope, FailureClass, FileFingerprint, Job, JobAttempt, JobId, JobKind, JobState,
        JobUnit, JobUnitId, JobUnitKind, JobUnitState, Money, Paragraph, ParagraphId,
        ParagraphKind, Project, ProjectId, ProjectSettings, ProjectStatus, PronunciationDictionary,
        ProvenanceQuality, ProviderDeployment, ProviderFamily, ProviderProfile, ProviderProfileId,
        ProviderRole, SettingsMap, Speaker, SpeakerOverride, SpeakerOverrideId, UsageEvent,
        UsageEventId, UsageQuantities, UsageWorkload, VoiceAssignment, VoiceAssignmentId,
        VoiceOrigin, VoiceOwnership, VoiceProfile, VoiceProfileId,
    };
    use chrono::{Duration, Utc};

    use super::*;

    #[derive(Clone, Copy, Debug)]
    struct TestIds {
        project: ProjectId,
        provider: ProviderProfileId,
        chapter: ChapterId,
        paragraph: ParagraphId,
    }

    #[test]
    fn packaged_linux_native_tts_resolves_from_the_installed_bin_directory() {
        let sidecars = PathBuf::from("/opt/AudiobookAI/resources/sidecars/bin");
        let config = ServiceConfig {
            bind: "127.0.0.1:0".parse().unwrap(),
            data_dir: PathBuf::from("/tmp/audiobookai-test-data"),
            bundled_sidecar_dir: Some(sidecars.clone()),
            tls: None,
            lan_hostnames: Vec::new(),
            allow_insecure_lan: false,
            desktop_bootstrap: true,
        };

        assert_eq!(
            native_executable_for_os("linux", &config),
            sidecars.join("espeak-ng")
        );
    }

    #[test]
    fn managed_launch_configuration_populates_the_runtime_profile() {
        use crate::models::{ProviderKindView, ProviderModeView, ProviderStatusView};

        let executable = std::env::current_exe().expect("test executable");
        let working_directory = std::env::current_dir().expect("test working directory");
        let profile = ProviderProfileView {
            id: Uuid::new_v4(),
            name: "Managed LocalAI".to_owned(),
            kind: ProviderKindView::Localai,
            mode: ProviderModeView::ManagedChild,
            endpoint: Some("http://127.0.0.1:8080".to_owned()),
            executable_path: Some(executable.to_string_lossy().into_owned()),
            working_directory: Some(working_directory.to_string_lossy().into_owned()),
            arguments: vec!["--address".to_owned(), "127.0.0.1:8080".to_owned()],
            status: ProviderStatusView::Offline,
            model: None,
            credential_configured: false,
            capabilities: None,
            capability_source: None,
            capability_updated_at: None,
            last_error: None,
        };
        let config = ServiceConfig {
            bind: "127.0.0.1:0".parse().unwrap(),
            data_dir: std::env::temp_dir().join("audiobookai-runtime-profile-test"),
            bundled_sidecar_dir: None,
            tls: None,
            lan_hostnames: Vec::new(),
            allow_insecure_lan: false,
            desktop_bootstrap: true,
        };

        let runtime = runtime_profile_from_view(&profile, &config).expect("runtime profile");
        assert_eq!(runtime.executable.as_deref(), Some(executable.as_path()));
        assert_eq!(
            runtime.working_directory.as_deref(),
            Some(working_directory.as_path())
        );
        assert_eq!(runtime.arguments, profile.arguments);
    }

    // This integration fixture intentionally seeds the complete related record
    // graph needed to prove restart hydration in one transaction-like helper.
    #[allow(clippy::too_many_lines)]
    async fn seed_project_and_provider(database: &Database) -> TestIds {
        let now = Utc::now();
        let book_id = BookId::new();
        let project_id = ProjectId::new();
        let chapter_id = ChapterId::new();
        let paragraph_id = ParagraphId::new();
        let metadata = BookMetadata {
            title: "Restartable Book".to_owned(),
            authors: vec!["Author".to_owned()],
            narrator: Some("Narrator".to_owned()),
            publisher: None,
            description: None,
            language: Some("en".to_owned()),
            identifier: None,
            series: None,
            cover_artifact_id: None,
            extra: BTreeMap::new(),
        };
        let book = Book {
            id: book_id,
            managed_epub_path: database
                .paths()
                .library
                .join("restartable.epub")
                .to_string_lossy()
                .into_owned(),
            original_filename: "restartable.epub".to_owned(),
            source_fingerprint: FileFingerprint {
                algorithm: "blake3".to_owned(),
                digest: "book-hash".to_owned(),
                size_bytes: 42,
            },
            epub_version: Some("3.0".to_owned()),
            metadata: metadata.clone(),
            imported_at: now,
        };
        let project = Project {
            id: project_id,
            book_id,
            name: metadata.title.clone(),
            status: ProjectStatus::NeedsCharacterReview,
            metadata,
            cloud_consent: CloudConsent::default(),
            settings: ProjectSettings::default(),
            character_reviewed_at: None,
            created_at: now,
            updated_at: now,
        };
        let chapter = Chapter {
            id: chapter_id,
            book_id,
            ordinal: 0,
            title: "Opening".to_owned(),
            source_href: "chapter.xhtml".to_owned(),
            selected: true,
            text_hash: "chapter-hash".to_owned(),
            character_count: 11,
        };
        let paragraph = Paragraph {
            id: paragraph_id,
            chapter_id,
            ordinal: 0,
            kind: ParagraphKind::Prose,
            text: "Hello Alice".to_owned(),
            source_start: 0,
            source_end: 11,
            content_hash: "paragraph-hash".to_owned(),
        };
        database
            .repositories()
            .projects
            .create_import(&book, &project, &[chapter], &[paragraph])
            .await
            .expect("project fixture");

        let provider_id = ProviderProfileId::new();
        database
            .repositories()
            .providers
            .upsert(&ProviderProfile {
                id: provider_id,
                name: "Local TTS".to_owned(),
                family: ProviderFamily::LocalAi,
                role: ProviderRole::Both,
                deployment: ProviderDeployment::ExternalEndpoint,
                endpoint: Some("http://127.0.0.1:8080/".to_owned()),
                executable_path: None,
                working_directory: None,
                arguments: Vec::new(),
                environment_secret_ids: BTreeMap::new(),
                credential_secret_id: None,
                enabled: true,
                concurrency_override: None,
                settings: SettingsMap::default(),
                capability_snapshot: None,
                created_at: now,
                updated_at: now,
            })
            .await
            .expect("provider fixture");
        TestIds {
            project: project_id,
            provider: provider_id,
            chapter: chapter_id,
            paragraph: paragraph_id,
        }
    }

    async fn insert_detection_run(database: &Database, run: &CharacterDetectionRun) {
        sqlx::query(
            "INSERT INTO detection_runs \
             (id, project_id, provider_id, status, created_at, completed_at, payload) \
             VALUES (?, ?, ?, 'completed', ?, ?, ?)",
        )
        .bind(run.id.to_string())
        .bind(run.project_id.to_string())
        .bind(run.provider_profile_id.to_string())
        .bind(run.created_at.to_rfc3339())
        .bind(run.completed_at.map(|time| time.to_rfc3339()))
        .bind(serde_json::to_string(run).expect("serialize detection run"))
        .execute(database.pool())
        .await
        .expect("insert detection run");
    }

    async fn insert_character(database: &Database, character: &Character) {
        sqlx::query(
            "INSERT INTO characters (id, project_id, canonical_name, updated_at, payload) \
             VALUES (?, ?, ?, ?, ?)",
        )
        .bind(character.id.to_string())
        .bind(character.project_id.to_string())
        .bind(&character.canonical_name)
        .bind(character.updated_at.to_rfc3339())
        .bind(serde_json::to_string(character).expect("serialize character"))
        .execute(database.pool())
        .await
        .expect("insert character");
        for alias in &character.aliases {
            sqlx::query(
                "INSERT INTO character_aliases (character_id, alias, normalized_alias) \
                 VALUES (?, ?, ?)",
            )
            .bind(character.id.to_string())
            .bind(alias)
            .bind(alias.to_lowercase())
            .execute(database.pool())
            .await
            .expect("insert alias");
        }
    }

    #[allow(clippy::too_many_lines)]
    #[tokio::test]
    async fn restart_hydrates_current_workflow_state_and_accounting() {
        let directory = tempfile::tempdir().expect("tempdir");
        let database = Database::open_in(directory.path()).await.expect("database");
        let ids = seed_project_and_provider(&database).await;
        let now = Utc::now();

        let old_run = CharacterDetectionRun {
            id: DetectionRunId::new(),
            project_id: ids.project,
            provider_profile_id: ids.provider,
            model: "old-model".to_owned(),
            status: DetectionRunStatus::Completed,
            paragraph_hashes: vec!["paragraph-hash".to_owned()],
            repair_attempted: false,
            created_at: now - Duration::minutes(2),
            completed_at: Some(now - Duration::minutes(2)),
        };
        let current_run = CharacterDetectionRun {
            id: DetectionRunId::new(),
            project_id: ids.project,
            provider_profile_id: ids.provider,
            model: "current-model".to_owned(),
            status: DetectionRunStatus::Completed,
            paragraph_hashes: vec!["paragraph-hash".to_owned()],
            repair_attempted: false,
            created_at: now - Duration::minutes(1),
            completed_at: Some(now - Duration::minutes(1)),
        };
        insert_detection_run(&database, &old_run).await;
        insert_detection_run(&database, &current_run).await;

        let old_character = Character {
            id: CharacterId::new(),
            project_id: ids.project,
            canonical_name: "Alice".to_owned(),
            aliases: Vec::new(),
            description: None,
            confidence: Some(0.5),
            detection_run_id: Some(old_run.id),
            manually_created: false,
            created_at: old_run.created_at,
            updated_at: old_run.created_at,
        };
        let alice = Character {
            id: CharacterId::new(),
            project_id: ids.project,
            canonical_name: "Alice".to_owned(),
            aliases: vec!["Al".to_owned()],
            description: None,
            confidence: Some(0.97),
            detection_run_id: Some(current_run.id),
            manually_created: false,
            created_at: current_run.created_at,
            updated_at: current_run.created_at,
        };
        let narrator = Character {
            id: CharacterId::new(),
            project_id: ids.project,
            canonical_name: "Narrator".to_owned(),
            aliases: Vec::new(),
            description: None,
            confidence: Some(1.0),
            detection_run_id: Some(current_run.id),
            manually_created: false,
            created_at: current_run.created_at,
            updated_at: current_run.created_at,
        };
        insert_character(&database, &old_character).await;
        insert_character(&database, &alice).await;
        insert_character(&database, &narrator).await;

        let span = DialogueSpan {
            paragraph_id: ids.paragraph,
            character_id: alice.id,
            byte_start: 6,
            byte_end: 11,
            confidence: 0.96,
            evidence: Some("Alice".to_owned()),
        };
        sqlx::query(
            "INSERT INTO dialogue_spans \
             (detection_run_id, paragraph_id, character_id, byte_start, byte_end, payload) \
             VALUES (?, ?, ?, 6, 11, ?)",
        )
        .bind(current_run.id.to_string())
        .bind(ids.paragraph.to_string())
        .bind(alice.id.to_string())
        .bind(serde_json::to_string(&span).expect("serialize dialogue span"))
        .execute(database.pool())
        .await
        .expect("insert dialogue span");
        let old_span = DialogueSpan {
            paragraph_id: ids.paragraph,
            character_id: old_character.id,
            byte_start: 0,
            byte_end: 5,
            confidence: 0.5,
            evidence: Some("Hello".to_owned()),
        };
        sqlx::query(
            "INSERT INTO dialogue_spans \
             (detection_run_id, paragraph_id, character_id, byte_start, byte_end, payload) \
             VALUES (?, ?, ?, 0, 5, ?)",
        )
        .bind(old_run.id.to_string())
        .bind(ids.paragraph.to_string())
        .bind(old_character.id.to_string())
        .bind(serde_json::to_string(&old_span).expect("serialize old dialogue span"))
        .execute(database.pool())
        .await
        .expect("insert old dialogue span");

        let speaker_override = SpeakerOverride {
            id: SpeakerOverrideId::new(),
            project_id: ids.project,
            paragraph_id: ids.paragraph,
            source_content_hash: "paragraph-hash".to_owned(),
            byte_start: 6,
            byte_end: 11,
            speaker: Speaker::Character(old_character.id),
            created_at: now,
            updated_at: now,
        };
        sqlx::query(
            "INSERT INTO speaker_overrides \
             (id, project_id, paragraph_id, source_content_hash, byte_start, byte_end, updated_at, payload) \
             VALUES (?, ?, ?, ?, 6, 11, ?, ?)",
        )
        .bind(speaker_override.id.to_string())
        .bind(ids.project.to_string())
        .bind(ids.paragraph.to_string())
        .bind(&speaker_override.source_content_hash)
        .bind(now.to_rfc3339())
        .bind(serde_json::to_string(&speaker_override).expect("serialize override"))
        .execute(database.pool())
        .await
        .expect("insert override");
        let stale_override = SpeakerOverride {
            id: SpeakerOverrideId::new(),
            source_content_hash: "stale-hash".to_owned(),
            updated_at: now + Duration::seconds(1),
            ..speaker_override.clone()
        };
        sqlx::query(
            "INSERT INTO speaker_overrides \
             (id, project_id, paragraph_id, source_content_hash, byte_start, byte_end, updated_at, payload) \
             VALUES (?, ?, ?, ?, 6, 11, ?, ?)",
        )
        .bind(stale_override.id.to_string())
        .bind(ids.project.to_string())
        .bind(ids.paragraph.to_string())
        .bind(&stale_override.source_content_hash)
        .bind(stale_override.updated_at.to_rfc3339())
        .bind(serde_json::to_string(&stale_override).expect("serialize stale override"))
        .execute(database.pool())
        .await
        .expect("insert stale override");

        let voice_profile = VoiceProfile {
            id: VoiceProfileId::new(),
            provider_profile_id: ids.provider,
            provider_voice_id: Some("alice-voice".to_owned()),
            name: "Alice Voice".to_owned(),
            origin: VoiceOrigin::ProviderCatalog,
            ownership: VoiceOwnership::Provider,
            reference_audio_artifact_ids: Vec::new(),
            language: Some("en".to_owned()),
            model: Some("tts-model".to_owned()),
            settings: BTreeMap::new(),
            created_at: now,
            updated_at: now,
        };
        sqlx::query(
            "INSERT INTO voice_profiles \
             (id, provider_id, name, origin, ownership, provider_voice_id, updated_at, payload) \
             VALUES (?, ?, ?, 'provider_catalog', 'provider', ?, ?, ?)",
        )
        .bind(voice_profile.id.to_string())
        .bind(ids.provider.to_string())
        .bind(&voice_profile.name)
        .bind(&voice_profile.provider_voice_id)
        .bind(now.to_rfc3339())
        .bind(serde_json::to_string(&voice_profile).expect("serialize voice"))
        .execute(database.pool())
        .await
        .expect("insert voice");
        let voice_assignment = VoiceAssignment {
            id: VoiceAssignmentId::new(),
            project_id: ids.project,
            speaker: Speaker::Character(old_character.id),
            voice_profile_id: voice_profile.id,
            provider_profile_id: ids.provider,
            model: Some("tts-model".to_owned()),
            settings: BTreeMap::new(),
            created_at: now,
            updated_at: now,
        };
        sqlx::query(
            "INSERT INTO voice_assignments \
             (id, project_id, provider_id, voice_profile_id, speaker_key, updated_at, payload) \
             VALUES (?, ?, ?, ?, ?, ?, ?)",
        )
        .bind(voice_assignment.id.to_string())
        .bind(ids.project.to_string())
        .bind(ids.provider.to_string())
        .bind(voice_profile.id.to_string())
        .bind(format!("character:{}", old_character.id))
        .bind(now.to_rfc3339())
        .bind(serde_json::to_string(&voice_assignment).expect("serialize assignment"))
        .execute(database.pool())
        .await
        .expect("insert assignment");

        let dictionary = PronunciationDictionary {
            id: DictionaryId::new(),
            name: "Project dictionary".to_owned(),
            scope: DictionaryScope::Project,
            project_id: Some(ids.project),
            enabled: true,
            revision: 1,
            created_at: now,
            updated_at: now,
        };
        sqlx::query(
            "INSERT INTO dictionaries \
             (id, project_id, scope, name, revision, enabled, updated_at, payload) \
             VALUES (?, ?, 'project', ?, 1, 1, ?, ?)",
        )
        .bind(dictionary.id.to_string())
        .bind(ids.project.to_string())
        .bind(&dictionary.name)
        .bind(now.to_rfc3339())
        .bind(serde_json::to_string(&dictionary).expect("serialize dictionary"))
        .execute(database.pool())
        .await
        .expect("insert dictionary");
        let dictionary_rule = DictionaryRule {
            id: DictionaryRuleId::new(),
            dictionary_id: dictionary.id,
            ordinal: 7,
            kind: DictionaryRuleKind::WholeWord,
            pattern: "Alice".to_owned(),
            replacement: "A-liss".to_owned(),
            case_sensitive: false,
            language: Some("en".to_owned()),
            character_id: Some(alice.id),
            phoneme_alphabet: None,
            enabled: true,
        };
        sqlx::query(
            "INSERT INTO dictionary_rules (id, dictionary_id, ordinal, kind, enabled, payload) \
             VALUES (?, ?, 0, 'whole_word', 1, ?)",
        )
        .bind(dictionary_rule.id.to_string())
        .bind(dictionary.id.to_string())
        .bind(serde_json::to_string(&dictionary_rule).expect("serialize dictionary rule"))
        .execute(database.pool())
        .await
        .expect("insert dictionary rule");

        let job = Job {
            id: JobId::new(),
            project_id: ids.project,
            kind: JobKind::Conversion,
            state: JobState::Running,
            export_profile_id: None,
            reservation_id: None,
            progress_completed: 1,
            progress_total: 2,
            status_message: Some("Synthesizing".to_owned()),
            allow_budget_override: false,
            created_at: now,
            started_at: Some(now),
            finished_at: None,
            updated_at: now,
            revision: 0,
        };
        database
            .repositories()
            .jobs
            .insert(&job)
            .await
            .expect("insert job");
        let unit = JobUnit {
            id: JobUnitId::new(),
            job_id: job.id,
            kind: JobUnitKind::SynthesisSegment,
            state: JobUnitState::Failed,
            chapter_id: Some(ids.chapter),
            segment_id: None,
            provider_profile_id: Some(ids.provider),
            dependencies: Vec::new(),
            attempt_count: 2,
            next_attempt_at: None,
            output_artifact_id: None,
            payload: BTreeMap::new(),
            created_at: now,
            updated_at: now,
        };
        database
            .repositories()
            .jobs
            .upsert_unit(&unit)
            .await
            .expect("insert unit");
        let attempt = JobAttempt {
            id: AttemptId::new(),
            job_unit_id: unit.id,
            ordinal: 2,
            started_at: now,
            finished_at: Some(now),
            failure_class: Some(FailureClass::TimeoutAfterDispatch),
            error_code: Some("timeout".to_owned()),
            redacted_error: Some("provider response timed out".to_owned()),
            provider_request_id: Some("request-1".to_owned()),
            uncertain_charge: true,
        };
        database
            .repositories()
            .jobs
            .insert_attempt(&attempt)
            .await
            .expect("insert attempt");

        let usage = UsageEvent {
            id: UsageEventId::new(),
            occurred_at: now,
            workload: UsageWorkload::Tts,
            project_id: ids.project,
            job_id: Some(job.id),
            attempt_id: Some(attempt.id),
            chapter_id: Some(ids.chapter),
            segment_id: None,
            provider_profile_id: ids.provider,
            provider_family: "local_ai".to_owned(),
            endpoint_family: "openai_compatible".to_owned(),
            model: Some("tts-model".to_owned()),
            voice_profile_id: Some(voice_profile.id),
            provider_request_id: Some("request-1".to_owned()),
            quantities: UsageQuantities {
                characters: Some(11),
                input_tokens: Some(3),
                output_tokens: Some(2),
                ..UsageQuantities::default()
            },
            quantity_source: ProvenanceQuality::Reported,
            cost: Some(Money {
                micros: 123,
                currency: "USD".to_owned(),
            }),
            cost_source: ProvenanceQuality::Reported,
            rate_card_id: None,
            uncertain_charge: true,
            redacted_raw_usage: BTreeMap::new(),
        };
        database
            .repositories()
            .usage
            .append(&usage)
            .await
            .expect("append usage");

        let mut catalog = Catalog::new(directory.path());
        hydrate_projects(&database, &mut catalog)
            .await
            .expect("hydrate projects");
        hydrate_providers(&database, &mut catalog)
            .await
            .expect("hydrate providers");
        hydrate_characters(&database, &mut catalog)
            .await
            .expect("hydrate characters");
        hydrate_voices(&database, &mut catalog)
            .await
            .expect("hydrate voices");
        hydrate_pronunciation_rules(&database, &mut catalog)
            .await
            .expect("hydrate dictionaries");
        hydrate_jobs(&database, &mut catalog)
            .await
            .expect("hydrate jobs");
        hydrate_usage(&database, &mut catalog)
            .await
            .expect("hydrate usage");

        let characters = catalog
            .characters
            .get(&ids.project.as_uuid())
            .expect("characters");
        assert_eq!(characters.len(), 2, "old-run characters must not reappear");
        let restored_alice = characters
            .iter()
            .find(|character| character.canonical_name == "Alice")
            .expect("Alice");
        assert_eq!(restored_alice.aliases, ["Al"]);
        assert_eq!(restored_alice.dialogue_count, 1);
        assert_eq!(restored_alice.evidence[0].excerpt, "Alice");
        assert_eq!(
            restored_alice.evidence[0].speaker_override.as_deref(),
            Some("Alice"),
            "the matching hash wins and the newer stale override is ignored"
        );
        let assignment = restored_alice
            .voice_assignment
            .as_ref()
            .expect("voice assignment");
        assert_eq!(assignment.voice_name, "Alice Voice");
        assert_eq!(
            catalog
                .voice_sources
                .get(&voice_profile.id.as_uuid())
                .map(String::as_str),
            Some("alice-voice")
        );
        assert_eq!(catalog.pronunciation_rules.len(), 1);
        assert_eq!(catalog.pronunciation_rules[0].order, 7);
        assert_eq!(
            catalog.pronunciation_rules[0].project_id,
            Some(ids.project.as_uuid())
        );
        let restored_job = catalog.jobs.get(&job.id.as_uuid()).expect("job");
        assert!(matches!(
            restored_job.status,
            crate::models::JobStatusView::Running
        ));
        assert!((restored_job.progress - 0.5).abs() < f32::EPSILON);
        assert!(restored_job.uncertain_charge);
        assert_eq!(
            restored_job.units[0].last_error.as_deref(),
            Some("provider response timed out")
        );
        assert!(matches!(
            catalog.projects[&ids.project.as_uuid()].summary.status,
            crate::models::ProjectDisplayStatus::Processing
        ));
        assert_eq!(catalog.usage_rows.len(), 1);
        assert_eq!(catalog.usage_rows[0].provider_name, "Local TTS");
        assert_eq!(catalog.usage_rows[0].voice.as_deref(), Some("Alice Voice"));
        assert_eq!(catalog.usage_rows[0].cost_micros, Some(123));
        assert_eq!(catalog.usage_rows[0].provenance, "reported");
    }

    #[tokio::test]
    async fn corrupt_newest_detection_payload_does_not_fall_back_to_stale_evidence() {
        let directory = tempfile::tempdir().expect("tempdir");
        let database = Database::open_in(directory.path()).await.expect("database");
        let ids = seed_project_and_provider(&database).await;
        let now = Utc::now();
        let old_run = CharacterDetectionRun {
            id: DetectionRunId::new(),
            project_id: ids.project,
            provider_profile_id: ids.provider,
            model: "old".to_owned(),
            status: DetectionRunStatus::Completed,
            paragraph_hashes: vec!["paragraph-hash".to_owned()],
            repair_attempted: false,
            created_at: now - Duration::minutes(1),
            completed_at: Some(now - Duration::minutes(1)),
        };
        insert_detection_run(&database, &old_run).await;
        let old_character = Character {
            id: CharacterId::new(),
            project_id: ids.project,
            canonical_name: "Stale".to_owned(),
            aliases: Vec::new(),
            description: None,
            confidence: Some(1.0),
            detection_run_id: Some(old_run.id),
            manually_created: false,
            created_at: old_run.created_at,
            updated_at: old_run.created_at,
        };
        insert_character(&database, &old_character).await;
        sqlx::query(
            "INSERT INTO detection_runs \
             (id, project_id, provider_id, status, created_at, completed_at, payload) \
             VALUES (?, ?, ?, 'completed', ?, ?, '{}')",
        )
        .bind(DetectionRunId::new().to_string())
        .bind(ids.project.to_string())
        .bind(ids.provider.to_string())
        .bind(now.to_rfc3339())
        .bind(now.to_rfc3339())
        .execute(database.pool())
        .await
        .expect("insert corrupt latest run");

        let mut catalog = Catalog::new(directory.path());
        hydrate_projects(&database, &mut catalog)
            .await
            .expect("hydrate projects");
        hydrate_characters(&database, &mut catalog)
            .await
            .expect("corrupt optional data is isolated");
        assert!(
            catalog
                .characters
                .get(&ids.project.as_uuid())
                .is_none_or(Vec::is_empty),
            "a corrupt latest result must not revive the older detection result"
        );
    }

    #[tokio::test]
    async fn corrupt_active_job_blocks_an_unsafe_restart() {
        let directory = tempfile::tempdir().expect("tempdir");
        let database = Database::open_in(directory.path()).await.expect("database");
        let ids = seed_project_and_provider(&database).await;
        let now = Utc::now();
        let job_id = JobId::new();
        sqlx::query(
            "INSERT INTO jobs \
             (id, project_id, export_profile_id, reservation_id, kind, state, revision, \
              created_at, updated_at, payload) \
             VALUES (?, ?, NULL, NULL, 'conversion', 'running', 0, ?, ?, '{}')",
        )
        .bind(job_id.to_string())
        .bind(ids.project.to_string())
        .bind(now.to_rfc3339())
        .bind(now.to_rfc3339())
        .execute(database.pool())
        .await
        .expect("insert corrupt active job");

        let mut catalog = Catalog::new(directory.path());
        hydrate_projects(&database, &mut catalog)
            .await
            .expect("hydrate projects");
        let error = hydrate_jobs(&database, &mut catalog)
            .await
            .expect_err("unsafe active job corruption must stop startup");
        assert!(error.to_string().contains("cannot be resumed safely"));
    }

    #[tokio::test]
    async fn hydration_preserves_defaults_but_never_redirects_managed_storage() {
        let directory = tempfile::tempdir().expect("tempdir");
        let database = Database::open_in(directory.path()).await.expect("database");
        let mut persisted = AppSettingsView::defaults(directory.path());
        persisted.library_path = "/untrusted/copied/library".to_owned();
        persisted.cache_path = "/untrusted/copied/cache".to_owned();
        persisted.default_lufs = -18.5;
        persisted.default_true_peak_db = -2.5;
        sqlx::query(
            "INSERT INTO application_settings (key, updated_at, payload) VALUES ('owner', ?, ?)",
        )
        .bind(Utc::now().to_rfc3339())
        .bind(serde_json::to_string(&persisted).expect("settings JSON"))
        .execute(database.pool())
        .await
        .expect("persist settings fixture");

        let mut catalog = Catalog::new(directory.path());
        hydrate_settings(&database, &mut catalog)
            .await
            .expect("hydrate settings");

        assert_eq!(
            catalog.settings.library_path,
            database.paths().library.to_string_lossy().into_owned()
        );
        assert_eq!(
            catalog.settings.cache_path,
            database.paths().cache.to_string_lossy().into_owned()
        );
        assert!((catalog.settings.default_lufs - -18.5).abs() < f32::EPSILON);
        assert!((catalog.settings.default_true_peak_db - -2.5).abs() < f32::EPSILON);
    }
}
