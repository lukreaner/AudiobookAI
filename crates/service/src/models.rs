use std::fmt;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Deserializer, Serialize};
use uuid::Uuid;

pub type Id = Uuid;

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Page<T> {
    pub items: Vec<T>,
    pub next_cursor: Option<String>,
    pub total: Option<usize>,
}

impl<T> Page<T> {
    pub fn all(items: Vec<T>) -> Self {
        let total = items.len();
        Self {
            items,
            next_cursor: None,
            total: Some(total),
        }
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct BookSummary {
    pub id: Id,
    pub title: String,
    pub author: Option<String>,
    pub cover_url: Option<String>,
    pub chapter_count: usize,
    pub selected_chapter_count: usize,
    pub duration_seconds: Option<u64>,
    pub progress: f32,
    pub status: ProjectDisplayStatus,
    pub updated_at: DateTime<Utc>,
    pub language: Option<String>,
    pub series: Option<String>,
    pub series_position: Option<f32>,
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ProjectDisplayStatus {
    Draft,
    Ready,
    Processing,
    Completed,
    Failed,
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ChapterDisplayStatus {
    Pending,
    Cached,
    Processing,
    Complete,
    Failed,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ChapterView {
    pub id: Id,
    pub index: usize,
    pub title: String,
    pub selected: bool,
    pub word_count: usize,
    pub character_count: usize,
    pub estimated_seconds: Option<u64>,
    pub status: ChapterDisplayStatus,
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ReviewStatus {
    NotStarted,
    NeedsReview,
    Approved,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ProjectDetail {
    #[serde(flatten)]
    pub summary: BookSummary,
    pub narrator: Option<String>,
    pub publisher: Option<String>,
    pub description: Option<String>,
    pub consent_cloud_text: bool,
    pub consent_cloud_audio: bool,
    pub chapters: Vec<ChapterView>,
    pub character_review_status: ReviewStatus,
    pub character_revision: u64,
    pub output_name: Option<String>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ImportDraft {
    pub draft_id: Id,
    pub source_name: String,
    pub title: String,
    pub author: Option<String>,
    pub language: Option<String>,
    pub cover_url: Option<String>,
    pub chapters: Vec<ChapterView>,
    pub warnings: Vec<String>,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CommitImport {
    pub chapter_ids: Vec<Id>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DialogueEvidenceView {
    pub id: Id,
    pub paragraph_id: Id,
    pub chapter_id: Id,
    pub chapter_title: String,
    pub excerpt: String,
    pub confidence: f32,
    pub start_offset: usize,
    pub end_offset: usize,
    pub speaker_override: Option<String>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct VoiceAssignmentView {
    pub provider_profile_id: Id,
    pub provider_name: String,
    pub voice_id: Id,
    pub voice_name: String,
    pub model: Option<String>,
    #[serde(default)]
    pub performance: audiobookai_core::PerformanceSettings,
    #[serde(default)]
    pub timing: audiobookai_core::TimingSettings,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CharacterView {
    pub id: Id,
    pub role: audiobookai_core::CharacterRole,
    pub canonical_name: String,
    pub aliases: Vec<String>,
    pub confidence: f32,
    pub dialogue_count: usize,
    pub voice_assignment: Option<VoiceAssignmentView>,
    pub evidence: Vec<DialogueEvidenceView>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CharacterPageView {
    pub items: Vec<CharacterView>,
    pub total: usize,
    pub character_revision: u64,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CharacterMutationView {
    pub character: Option<CharacterView>,
    pub removed_character_id: Option<Id>,
    pub inherited_voice: Option<bool>,
    pub character_revision: u64,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SpeakerOverrideView {
    pub character_id: Option<Id>,
    pub character_name: String,
    pub start_offset: usize,
    pub end_offset: usize,
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum JobKindView {
    CharacterDetection,
    Preview,
    Conversion,
    SegmentRegeneration,
    Export,
    QualityControl,
    CacheCleanup,
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum VoiceKindView {
    Catalog,
    LocalReference,
    RemoteClone,
    Native,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct VoiceView {
    pub id: Id,
    pub provider_profile_id: Id,
    pub name: String,
    pub locale: Option<String>,
    pub gender: Option<String>,
    pub kind: VoiceKindView,
    pub owned: bool,
    pub preview_url: Option<String>,
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum PronunciationScopeView {
    Global,
    Project,
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum PronunciationKindView {
    Literal,
    WholeWord,
    Regex,
    Alias,
    Phoneme,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PronunciationRuleView {
    pub id: Id,
    pub scope: PronunciationScopeView,
    pub kind: PronunciationKindView,
    pub source: String,
    pub replacement: String,
    pub language: Option<String>,
    pub character_id: Option<Id>,
    pub case_sensitive: bool,
    pub enabled: bool,
    pub order: u32,
    pub conflict: Option<String>,
    #[serde(default)]
    pub project_id: Option<Id>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ProviderKindView {
    Elevenlabs,
    MlxAudio,
    Localai,
    AlltalkV2,
    NativeOs,
    OpenaiTts,
    Openai,
    OpenaiCompatible,
    Anthropic,
    Gemini,
    Qwen,
    Kimi,
    Moonshot,
    LmStudio,
    Ollama,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ProviderModeView {
    CloudRemote,
    ExternalEndpoint,
    ManagedChild,
    Native,
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ProviderStatusView {
    Online,
    Offline,
    Starting,
    Stopping,
    Error,
    Unconfigured,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
// This wire capability matrix intentionally mirrors independent provider
// feature flags; collapsing them would make API evolution less explicit.
#[allow(clippy::struct_excessive_bools)]
pub struct ProviderCapabilitiesView {
    pub tts: bool,
    pub character_detection: bool,
    pub streaming: bool,
    pub voice_cloning: bool,
    pub pronunciation: bool,
    pub process_control: bool,
    pub model_control: bool,
    #[serde(default)]
    pub model_list: bool,
    #[serde(default)]
    pub model_download: bool,
    #[serde(default)]
    pub model_delete: bool,
    #[serde(default)]
    pub model_load: bool,
    #[serde(default)]
    pub model_unload: bool,
    #[serde(default)]
    pub model_switch: bool,
    pub temperature: String,
    pub reasoning: Vec<String>,
    pub max_concurrency: Option<u16>,
    /// Performance controls positively verified for exact model identifiers.
    #[serde(default)]
    pub model_performance: Vec<audiobookai_core::ModelPerformanceCapabilities>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ProviderProfileView {
    pub id: Id,
    pub name: String,
    pub kind: ProviderKindView,
    pub mode: ProviderModeView,
    pub endpoint: Option<String>,
    pub executable_path: Option<String>,
    pub working_directory: Option<String>,
    pub arguments: Vec<String>,
    pub status: ProviderStatusView,
    pub model: Option<String>,
    pub credential_configured: bool,
    pub capabilities: Option<ProviderCapabilitiesView>,
    pub capability_source: Option<String>,
    pub capability_updated_at: Option<DateTime<Utc>>,
    pub last_error: Option<String>,
}

#[derive(Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ProviderProfileInput {
    pub name: Option<String>,
    pub kind: Option<ProviderKindView>,
    pub mode: Option<ProviderModeView>,
    #[serde(default, deserialize_with = "deserialize_nullable_patch")]
    pub endpoint: Option<Option<String>>,
    #[serde(default, deserialize_with = "deserialize_nullable_patch")]
    pub executable_path: Option<Option<String>>,
    #[serde(default, deserialize_with = "deserialize_nullable_patch")]
    pub working_directory: Option<Option<String>>,
    pub arguments: Option<Vec<String>>,
    #[serde(default, deserialize_with = "deserialize_nullable_patch")]
    pub model: Option<Option<String>>,
    pub credential: Option<zeroize::Zeroizing<String>>,
}

impl fmt::Debug for ProviderProfileInput {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ProviderProfileInput")
            .field("has_name", &self.name.is_some())
            .field("kind", &self.kind)
            .field("mode", &self.mode)
            .field(
                "endpoint_patch",
                &self.endpoint.as_ref().map(Option::is_some),
            )
            .field(
                "executable_path_patch",
                &self.executable_path.as_ref().map(Option::is_some),
            )
            .field(
                "working_directory_patch",
                &self.working_directory.as_ref().map(Option::is_some),
            )
            .field(
                "argument_count",
                &self.arguments.as_ref().map(std::vec::Vec::len),
            )
            .field("model_patch", &self.model.as_ref().map(Option::is_some))
            .field(
                "credential",
                &self.credential.as_ref().map(|_| "[REDACTED]"),
            )
            .finish()
    }
}

// PATCH fields need three states: omitted, explicit null, and a concrete value.
#[allow(clippy::option_option)]
fn deserialize_nullable_patch<'de, D, T>(deserializer: D) -> Result<Option<Option<T>>, D::Error>
where
    D: Deserializer<'de>,
    T: Deserialize<'de>,
{
    Option::<T>::deserialize(deserializer).map(Some)
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ProviderEstimateView {
    pub provider_profile_id: Id,
    pub provider_name: String,
    pub model: Option<String>,
    pub characters: u64,
    pub estimated_duration_seconds: u64,
    pub monetary_cost_micros: Option<i64>,
    pub currency: Option<String>,
    pub credits: Option<i64>,
    pub rate_card_id: Option<Id>,
    pub price_source: Option<String>,
    pub price_effective_at: Option<DateTime<Utc>>,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct EstimateView {
    pub selected_chapters: usize,
    pub characters: u64,
    pub estimated_tokens: Option<u64>,
    pub estimated_duration_seconds: u64,
    pub estimated_disk_bytes: u64,
    pub estimated_completion_seconds_low: Option<u64>,
    pub estimated_completion_seconds_high: Option<u64>,
    pub monetary_cost_micros: Option<i64>,
    pub currency: Option<String>,
    pub credits: Option<i64>,
    pub price_source: Option<String>,
    pub price_effective_at: Option<DateTime<Utc>>,
    pub provider_estimates: Vec<ProviderEstimateView>,
    pub unknown_fields: Vec<String>,
}

#[derive(Clone, Copy, Debug, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum CheckStatus {
    Pass,
    Warning,
    Fail,
    Pending,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DryRunCheckView {
    pub id: String,
    pub label: String,
    pub status: CheckStatus,
    pub detail: String,
    pub action: Option<String>,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DryRunView {
    pub ready: bool,
    pub checked_at: DateTime<Utc>,
    pub checks: Vec<DryRunCheckView>,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PreviewView {
    pub artifact_id: Id,
    pub audio_url: String,
    pub text: String,
    pub duration_seconds: u64,
    pub billable: bool,
    pub cached: bool,
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum JobStageView {
    Detect,
    Synthesize,
    Assemble,
    Mix,
    Normalize,
    Export,
    QualityControl,
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum JobUnitStatusView {
    Queued,
    Running,
    Paused,
    Complete,
    Failed,
    Cancelled,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct JobUnitView {
    pub id: Id,
    pub title: String,
    pub stage: JobStageView,
    pub status: JobUnitStatusView,
    pub progress: f32,
    pub attempt: u32,
    pub last_error: Option<String>,
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum JobStatusView {
    Queued,
    Running,
    Pausing,
    Paused,
    Cancelling,
    Complete,
    Failed,
    Cancelled,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct JobView {
    pub id: Id,
    pub project_id: Id,
    pub project_title: String,
    pub kind: JobKindView,
    pub status: JobStatusView,
    pub progress: f32,
    pub current_stage: Option<String>,
    pub started_at: Option<DateTime<Utc>>,
    pub updated_at: DateTime<Utc>,
    pub estimated_remaining_seconds: Option<u64>,
    pub units: Vec<JobUnitView>,
    pub progressive_playback_url: Option<String>,
    pub uncertain_charge: bool,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CharacterDetectionStatusView {
    pub active_job: Option<JobView>,
    pub latest_job: Option<JobView>,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct StartJobInput {
    pub project_id: Id,
    #[serde(default)]
    pub allow_budget_override: bool,
    #[serde(default)]
    pub export: ExportOptionsInput,
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ExportFormatView {
    Mp3,
    Wav,
    M4a,
    M4b,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ExportOptionsInput {
    #[serde(default = "default_export_format")]
    pub format: ExportFormatView,
    #[serde(default)]
    pub split_per_chapter: bool,
    pub output_directory: Option<String>,
    pub file_name: Option<String>,
    #[serde(default = "default_bitrate")]
    pub bitrate_kbps: u16,
    pub background_music_path: Option<String>,
    #[serde(default)]
    pub confirm_background_music_owned: bool,
    #[serde(default = "default_music_gain")]
    pub music_gain_db: f32,
    #[serde(default = "default_ducking")]
    pub ducking: bool,
}

impl Default for ExportOptionsInput {
    fn default() -> Self {
        Self {
            format: default_export_format(),
            split_per_chapter: false,
            output_directory: None,
            file_name: None,
            bitrate_kbps: default_bitrate(),
            background_music_path: None,
            confirm_background_music_owned: false,
            music_gain_db: default_music_gain(),
            ducking: default_ducking(),
        }
    }
}

const fn default_export_format() -> ExportFormatView {
    ExportFormatView::M4b
}

const fn default_bitrate() -> u16 {
    128
}

fn default_music_gain() -> f32 {
    -24.0
}

const fn default_ducking() -> bool {
    true
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ExportArtifactView {
    pub id: Id,
    pub project_id: Id,
    pub job_id: Id,
    /// Zero-based position in the export manifest's canonical output-file order.
    pub part_index: u32,
    pub part_count: u32,
    pub project_title: String,
    pub format: String,
    pub split_mode: String,
    pub file_name: String,
    pub size_bytes: u64,
    pub duration_seconds: u64,
    pub created_at: DateTime<Utc>,
    pub download_url: String,
    pub manifest_url: String,
    pub chapter_markers: bool,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct UsageRowView {
    pub id: Id,
    pub occurred_at: DateTime<Utc>,
    pub project_title: Option<String>,
    pub provider_name: String,
    pub operation: String,
    pub model: Option<String>,
    pub voice: Option<String>,
    pub characters: Option<u64>,
    pub input_tokens: Option<u64>,
    pub output_tokens: Option<u64>,
    pub cost_micros: Option<i64>,
    pub currency: Option<String>,
    pub provenance: String,
    pub request_id: Option<String>,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct UsageSummaryView {
    pub period_start: DateTime<Utc>,
    pub period_end: DateTime<Utc>,
    pub currency: Option<String>,
    pub monetary_cost_micros: Option<i64>,
    pub characters: Option<u64>,
    pub input_tokens: Option<u64>,
    pub output_tokens: Option<u64>,
    pub credits: Option<i64>,
    pub unknown_cost_requests: u64,
    pub rows: Vec<UsageRowView>,
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum BudgetPeriodView {
    Job,
    Daily,
    Monthly,
    Lifetime,
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum BudgetMetricView {
    Money,
    Tokens,
    Characters,
    Credits,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct BudgetView {
    pub id: Id,
    pub name: String,
    pub provider_profile_id: Option<Id>,
    pub period: BudgetPeriodView,
    pub metric: BudgetMetricView,
    pub limit: i64,
    pub used: i64,
    pub reserved: i64,
    pub hard: bool,
    pub currency: Option<String>,
    pub warning_percent: u8,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CreateBudgetInput {
    pub name: String,
    pub provider_profile_id: Option<Id>,
    pub period: BudgetPeriodView,
    pub metric: BudgetMetricView,
    pub limit: i64,
    pub hard: bool,
    pub currency: Option<String>,
    pub warning_percent: u8,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
// Each boolean is an independent security or runtime state exposed by the LAN
// settings API and cannot be represented as a single enum.
#[allow(clippy::struct_excessive_bools)]
pub struct LanSettingsView {
    pub enabled: bool,
    pub tls: bool,
    pub insecure_http_confirmed: bool,
    pub bind_address: String,
    pub port: u16,
    #[serde(default)]
    pub certificate_chain_path: String,
    #[serde(default)]
    pub private_key_path: String,
    #[serde(default)]
    pub advertised_hosts: Vec<String>,
    #[serde(default, skip_deserializing)]
    pub password_configured: bool,
    #[serde(default, skip_deserializing)]
    pub api_token_count: usize,
    pub active_sessions: usize,
    #[serde(default, skip_deserializing)]
    pub restart_required: bool,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum SecretStoreView {
    Keychain,
    Passphrase,
    Locked,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AppSettingsView {
    pub language: String,
    pub theme: String,
    pub library_path: String,
    pub cache_path: String,
    pub cache_limit_bytes: u64,
    pub default_concurrency: u16,
    pub default_retry_count: u16,
    pub default_lufs: f32,
    pub default_true_peak_db: f32,
    pub close_to_tray: bool,
    pub check_for_updates: bool,
    pub lan: LanSettingsView,
    pub secret_store: SecretStoreView,
    pub first_run_complete: bool,
}

impl AppSettingsView {
    pub fn defaults(data_dir: &std::path::Path) -> Self {
        Self {
            language: "en".to_owned(),
            theme: "system".to_owned(),
            library_path: data_dir.join("library").to_string_lossy().into_owned(),
            cache_path: data_dir.join("cache").to_string_lossy().into_owned(),
            cache_limit_bytes: 20 * 1024 * 1024 * 1024,
            default_concurrency: 4,
            default_retry_count: 3,
            default_lufs: -19.0,
            default_true_peak_db: -3.0,
            close_to_tray: true,
            check_for_updates: true,
            lan: LanSettingsView {
                enabled: false,
                tls: false,
                insecure_http_confirmed: false,
                bind_address: "127.0.0.1".to_owned(),
                port: 8787,
                certificate_chain_path: String::new(),
                private_key_path: String::new(),
                advertised_hosts: Vec::new(),
                password_configured: false,
                api_token_count: 0,
                active_sessions: 0,
                restart_required: false,
            },
            secret_store: SecretStoreView::Locked,
            first_run_complete: false,
        }
    }
}
