use std::{
    collections::{BTreeMap, BTreeSet, VecDeque},
    str::FromStr,
    sync::{Arc, Mutex as StdMutex, OnceLock},
    time::Duration,
};

use audiobookai_core::{
    AttemptId, Character, CharacterDetectionRun, CharacterId, DetectionRunId, DetectionRunStatus,
    FailureClass as CoreFailureClass, Job, JobAttempt, JobId, JobKind, JobState, JobUnit,
    JobUnitId, JobUnitKind, JobUnitState, ParagraphId, ProjectId, ProjectStatus, ProvenanceQuality,
    ProviderProfileId, RateCardId, UsageEvent, UsageEventId, UsageQuantities, UsageWorkload,
};
use audiobookai_providers::{
    CharacterDetectionRequest, CharacterDetectionResult, DetectedCharacter, DetectedDialogue,
    DetectionParagraph, ProviderError, ProviderUsage, ReasoningControl, Temperature, UsageSource,
};
use chrono::{Duration as ChronoDuration, Utc};
use futures::future::BoxFuture;
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::{
    AppState, ServiceError,
    models::{
        CharacterView, DialogueEvidenceView, JobStageView, JobStatusView, JobUnitStatusView,
        JobUnitView, JobView, ProviderModeView, ProviderProfileView, ProviderStatusView,
        ReviewStatus, UsageRowView,
    },
    runtime::{
        FailureClass, RetryEvent, RetryEventOutcome, RetryJournal, RetryJournalError, RetryPolicy,
        execute_with_retry,
    },
};

mod batching;
mod cast;
mod execution;
mod persistence;
mod recovery;
mod runner;
#[cfg(test)]
mod tests;
mod units;

// Character detection is grouped by stage: admission and the worker loop (`runner`), token-aware
// batching, provider execution with its retry journal, restart recovery, durable units, cast
// merging, and result persistence. Items used elsewhere keep their `crate::workflows::…` paths.
use self::{
    batching::{
        DetectionBatch, DetectionBatching, DetectionContextBudget, DetectionSourceParagraph,
        detection_batches_for_config, detection_request_estimate, paragraph_batches,
        selected_paragraphs,
    },
    cast::{
        apply_persisted_overrides, canonicalize_detection_result, character_views,
        load_previous_characters, merge_characters,
    },
    execution::{core_failure_class_name, execute_detection_batch},
    persistence::{
        append_detection_usage, fail_job, insert_detection_run, persist_detection_results,
        update_detection_run, update_job_progress,
    },
    units::{
        combined_detection_results, completed_detection_units, consistent_detection_config,
        detection_batch_index, detection_config, detection_profile_matches_dispatch_contract,
        detection_provider_is_non_billable_local, detection_request,
        detection_runtime_profile_matches, detection_unit, detection_unit_estimate,
        detection_unit_rate_card, detection_unit_view, detection_units, finalize_detection_unit,
        load_detection_run, mark_detection_unit, persist_detection_unit_result,
        persisted_detection_result, validate_detection_profile,
    },
};
pub(crate) use self::{recovery::*, runner::*};

const DETECTION_BATCH_PARAGRAPHS: usize = 24;
const DETECTION_CONTEXT_OVERLAP: usize = 2;
const DETECTION_JOB_SCHEMA_VERSION: u32 = 6;
/// First schema whose batches are sized by the provider context window.
const DETECTION_TOKEN_AWARE_SCHEMA_VERSION: u32 = 5;
const DETECTION_DEFAULT_CONTEXT_TOKENS: u64 = 16_384;
const LM_STUDIO_DEFAULT_CONTEXT_TOKENS: u64 = 4_096;
const DETECTION_PROMPT_TOKEN_RESERVE: u64 = 1_024;
const DETECTION_MIN_OUTPUT_TOKENS: u64 = 512;
const DETECTION_MAX_OUTPUT_TOKENS: u64 = 4_096;
const DETECTION_MIN_PARAGRAPH_TOKENS: u64 = 256;
const DETECTION_PARAGRAPH_OVERHEAD_TOKENS: u64 = 128;
const DETECTION_CONTEXT_FALLBACK_LIMIT: usize = 8;
const DETECTION_OUTPUT_FALLBACK_LIMIT: usize = 8;

/// Instructions shared by every detection request.
///
/// The provider adapter resolves `quote_start`/`quote_end` to exact byte ranges, so the model only
/// copies text and never counts bytes. Paragraph IDs are short request-local aliases.
const DETECTION_SYSTEM_PROMPT: &str = "\
You attribute direct speech in an excerpt of a book to the characters who speak it.

Input: a JSON array of paragraphs with an \"id\" and \"text\". Paragraphs marked \"context_only\": true \
are surrounding context: use them to work out who is speaking, but never return dialogue for them.

Return JSON with:
- \"characters\": every character who speaks in the excerpt. \"canonical_name\" is the fullest name the \
text uses for the person (for example \"Harry Potter\"); \"aliases\" lists other names, nicknames or \
titles the text uses for the same person (for example \"Harry\", \"Mr Potter\"); \"confidence\" is 0 to 1.
- \"dialogue\": one entry per passage of direct speech in a paragraph that is not context_only:
  - \"paragraph_id\": the paragraph id.
  - \"quote_start\": the first 3 to 8 words of the spoken passage, copied exactly, without the opening \
quotation mark.
  - \"quote_end\": the last 3 to 8 words of the spoken passage, copied exactly, without the closing \
quotation mark. For a short passage, repeat the whole passage.
  - \"character\": the speaker's canonical_name exactly as listed in \"characters\".
  - \"confidence\": 0 to 1.

Rules:
- Direct speech is text in quotation marks (\"...\", \u{201e}...\u{201c}, \u{bb}...\u{ab}, \u{ab}...\u{bb}, '...') or dialogue introduced \
by a dash. Narration, speech tags such as \"he said\" and unspoken thoughts are not dialogue.
- When a speech tag interrupts one utterance (\"Wait,\" she said, \"come back.\"), return two entries.
- Infer unnamed speakers from context: alternating turns, who was addressed, and speech tags. If the \
speaker cannot be determined, omit the passage instead of guessing.
- Use one canonical name per person. Do not list the narrator unless the narrator speaks aloud as a \
named character.
- Copy text exactly as written. Do not translate, correct, normalize or paraphrase it.";

const DETECTION_REPAIR_SUFFIX: &str = "Your previous answer could not be parsed. Answer again with a \
single JSON object that strictly matches the required schema, without markdown or commentary.";

static ACTIVE_DETECTION_WORKERS: OnceLock<StdMutex<BTreeSet<Uuid>>> = OnceLock::new();

#[derive(Debug)]
struct ActiveDetectionWorker {
    job_id: Uuid,
}

impl ActiveDetectionWorker {
    fn acquire(job_id: Uuid) -> Option<Self> {
        let workers = ACTIVE_DETECTION_WORKERS.get_or_init(|| StdMutex::new(BTreeSet::new()));
        let mut active = workers
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        active.insert(job_id).then_some(Self { job_id })
    }
}

impl Drop for ActiveDetectionWorker {
    fn drop(&mut self) {
        ACTIVE_DETECTION_WORKERS
            .get_or_init(|| StdMutex::new(BTreeSet::new()))
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .remove(&self.job_id);
    }
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
struct DetectionJobConfig {
    schema_version: u32,
    provider_profile_id: Uuid,
    model: String,
    provider_endpoint: Option<String>,
    #[serde(default)]
    provider_mode: Option<ProviderModeView>,
    #[serde(default)]
    provider_snapshot_id: Option<Uuid>,
    temperature: Temperature,
    reasoning: ReasoningControl,
    #[serde(default)]
    context_window_tokens: Option<u64>,
    #[serde(default)]
    max_output_tokens: Option<u32>,
    detection_run_id: DetectionRunId,
    #[serde(default)]
    base_character_revision: u64,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
struct PersistedDetectionResult {
    characters: Vec<DetectedCharacter>,
    dialogue: Vec<DetectedDialogue>,
    usage: ProviderUsage,
}

impl From<CharacterDetectionResult> for PersistedDetectionResult {
    fn from(result: CharacterDetectionResult) -> Self {
        Self {
            characters: result.characters,
            dialogue: result.dialogue,
            usage: result.usage,
        }
    }
}

impl From<PersistedDetectionResult> for CharacterDetectionResult {
    fn from(result: PersistedDetectionResult) -> Self {
        Self {
            characters: result.characters,
            dialogue: result.dialogue,
            usage: result.usage,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum RecoveryDecision {
    Keep,
    FinalizePersistedResult,
    RedispatchSafe,
    FailUncertain,
    FailTerminal,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum DetectionPermission {
    Run,
    Cancelled,
    Terminal,
}

async fn effective_detection_context_window(
    state: &AppState,
    profile: &ProviderProfileView,
    model: &str,
) -> Result<u64, ServiceError> {
    let runtime_id = audiobookai_providers::ProviderId::new(profile.id.to_string())
        .map_err(|error| ServiceError::Internal(error.to_string()))?;
    let observed = match state.providers.character(&runtime_id).await {
        Ok(provider) => {
            match tokio::time::timeout(Duration::from_secs(4), provider.model_context_window(model))
                .await
            {
                Ok(Ok(window)) => window,
                Ok(Err(error)) => {
                    tracing::debug!(
                        diagnostic_code = "detection.context.discovery.failed",
                        provider_id = %profile.id,
                        %error,
                        "provider context-window discovery failed; using the safe configured fallback"
                    );
                    audiobookai_providers::ModelContextWindow::default()
                }
                Err(_) => {
                    tracing::debug!(
                        diagnostic_code = "detection.context.discovery.timeout",
                        provider_id = %profile.id,
                        "provider context-window discovery timed out; using the safe configured fallback"
                    );
                    audiobookai_providers::ModelContextWindow::default()
                }
            }
        }
        Err(error) => {
            tracing::debug!(
                diagnostic_code = "detection.context.runtime_unavailable",
                provider_id = %profile.id,
                %error,
                "provider context-window runtime is unavailable; using the safe configured fallback"
            );
            audiobookai_providers::ModelContextWindow::default()
        }
    };
    Ok(select_effective_context_window(profile, observed))
}

fn select_effective_context_window(
    profile: &ProviderProfileView,
    observed: audiobookai_providers::ModelContextWindow,
) -> u64 {
    let configured = profile.context_window_tokens;
    let detected_or_configured = match (observed.loaded_tokens, configured) {
        (Some(loaded), Some(configured)) => Some(loaded.min(configured)),
        (Some(loaded), None) => Some(loaded),
        (None, configured) => configured,
    };
    let fallback = if matches!(profile.kind, crate::models::ProviderKindView::LmStudio) {
        LM_STUDIO_DEFAULT_CONTEXT_TOKENS
    } else {
        DETECTION_DEFAULT_CONTEXT_TOKENS
    };
    let selected = detected_or_configured
        .or_else(|| {
            (!matches!(profile.kind, crate::models::ProviderKindView::LmStudio))
                .then_some(observed.maximum_tokens)
                .flatten()
        })
        .unwrap_or(fallback);
    observed
        .maximum_tokens
        .map_or(selected, |maximum| selected.min(maximum))
}

fn storage_error(error: impl std::fmt::Display) -> ServiceError {
    ServiceError::Storage(error.to_string())
}
