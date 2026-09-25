use super::{
    AppState, Arc, BTreeMap, BTreeSet, CheckStatus, Deserialize, DryRunView, EstimateView,
    ExportOptionsInput, FilePath, HashMap, HashSet, Json, Path, PathBuf, PreviewView,
    ProjectDetail, ProviderModeView, ProviderProfileView, ProviderStatusView, ReviewStatus,
    ServiceError, State, Utc, Uuid, VoiceAssignmentView, check, estimated_seconds,
    provider_capabilities_are_fresh, status_check, validate_billable_tts_provider_readiness,
    validate_piper_voice_selection, validate_voice_direction,
};

pub(super) async fn preflight_estimate(
    State(state): State<Arc<AppState>>,
    Path(id): Path<Uuid>,
) -> Result<Json<EstimateView>, ServiceError> {
    let (project, characters, providers) = {
        let catalog = state.catalog.read().await;
        (
            catalog
                .projects
                .get(&id)
                .cloned()
                .ok_or(ServiceError::NotFound)?,
            catalog.characters.get(&id).cloned().unwrap_or_default(),
            catalog.providers.clone(),
        )
    };
    Ok(Json(
        estimate_project(&state, &project, &characters, &providers).await?,
    ))
}

pub(super) async fn preflight_dry_run(
    State(state): State<Arc<AppState>>,
    Path(id): Path<Uuid>,
    Json(input): Json<DryRunInput>,
) -> Result<Json<DryRunView>, ServiceError> {
    let (project, characters, providers) = {
        let catalog = state.catalog.read().await;
        (
            catalog
                .projects
                .get(&id)
                .cloned()
                .ok_or(ServiceError::NotFound)?,
            catalog.characters.get(&id).cloned().unwrap_or_default(),
            catalog.providers.clone(),
        )
    };
    let dry_run = dry_run_project(&project, &characters, &providers);
    Ok(Json(
        extend_dry_run_environment(
            &state,
            &project,
            &characters,
            &providers,
            &input.export,
            dry_run,
        )
        .await?,
    ))
}

#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct DryRunInput {
    #[serde(default)]
    pub(super) export: ExportOptionsInput,
}

#[derive(Debug, Deserialize)]
pub(super) struct PreviewInput {
    pub(super) text: Option<String>,
}

pub(super) async fn preflight_preview(
    State(state): State<Arc<AppState>>,
    Path(id): Path<Uuid>,
    Json(input): Json<PreviewInput>,
) -> Result<Json<PreviewView>, ServiceError> {
    let _model_lifecycle_guard = state.model_lifecycle.lock().await;
    crate::conversion::preview(Arc::clone(&state), id, input.text)
        .await
        .map(Json)
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct VoiceAuditionCandidateInput {
    pub(super) candidate_id: String,
    pub(super) provider_profile_id: Uuid,
    pub(super) voice_id: Uuid,
    pub(super) model: Option<String>,
    #[serde(default)]
    pub(super) performance: audiobookai_core::PerformanceSettings,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct VoiceAuditionInput {
    pub(super) text: Option<String>,
    pub(super) character_id: Option<Uuid>,
    #[serde(default)]
    pub(super) confirm_billable: bool,
    pub(super) candidates: Vec<VoiceAuditionCandidateInput>,
}

#[derive(Debug, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct VoiceAuditionResult {
    pub(super) candidate_id: String,
    pub(super) provider_profile_id: Uuid,
    pub(super) voice_id: Uuid,
    pub(super) preview: Option<PreviewView>,
    pub(super) error: Option<String>,
}

#[derive(Debug, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct VoiceAuditionResponse {
    pub(super) results: Vec<VoiceAuditionResult>,
    pub(super) potentially_billable: bool,
}

#[allow(clippy::too_many_lines)]
pub(super) async fn voice_auditions(
    State(state): State<Arc<AppState>>,
    Path(project_id): Path<Uuid>,
    Json(input): Json<VoiceAuditionInput>,
) -> Result<Json<VoiceAuditionResponse>, ServiceError> {
    if !input.confirm_billable {
        return Err(ServiceError::Conflict(
            "confirm that voice auditions may consume provider credits or incur cost".to_owned(),
        ));
    }
    if input.candidates.is_empty() || input.candidates.len() > 6 {
        return Err(ServiceError::InvalidRequest(
            "voice auditions require between one and six candidates".to_owned(),
        ));
    }
    let mut ids = HashSet::new();
    if input.candidates.iter().any(|candidate| {
        candidate.candidate_id.trim().is_empty() || !ids.insert(&candidate.candidate_id)
    }) {
        return Err(ServiceError::InvalidRequest(
            "voice audition candidate ids must be non-empty and unique".to_owned(),
        ));
    }

    // Lock ordering is global model lifecycle, then project character lifecycle. Keep both
    // guards through validation and dispatch so no candidate can be billed against state that
    // changed after the batch-wide preflight.
    let _model_lifecycle_guard = state.model_lifecycle.lock().await;
    let project_lock = state.character_lifecycle_lock(project_id).await;
    let _project_guard = project_lock.lock().await;
    let project = state
        .database
        .repositories()
        .projects
        .get_project(audiobookai_core::ProjectId::from_uuid(project_id))
        .await
        .map_err(|error| ServiceError::Storage(error.to_string()))?
        .ok_or(ServiceError::NotFound)?;

    // Resolve and validate every candidate before the first potentially billable dispatch.
    let assignments = {
        let catalog = state.catalog.read().await;
        input
            .candidates
            .iter()
            .map(|candidate| {
                let provider = catalog
                    .providers
                    .get(&candidate.provider_profile_id)
                    .ok_or_else(|| {
                        ServiceError::InvalidRequest("unknown audition provider".to_owned())
                    })?;
                validate_billable_tts_provider_readiness(provider)?;
                if matches!(provider.mode, ProviderModeView::CloudRemote)
                    && !project.cloud_consent.book_text
                {
                    return Err(super::cloud_text_consent_required(&provider.name));
                }
                let voice = catalog
                    .voices
                    .iter()
                    .find(|voice| {
                        voice.id == candidate.voice_id
                            && voice.provider_profile_id == candidate.provider_profile_id
                    })
                    .ok_or_else(|| {
                        ServiceError::InvalidRequest(
                            "an audition voice does not belong to its provider".to_owned(),
                        )
                    })?;
                let source_id = catalog
                    .voice_sources
                    .get(&candidate.voice_id)
                    .filter(|source| !source.trim().is_empty())
                    .ok_or_else(|| {
                        ServiceError::Conflict(
                            "an audition voice has no usable provider source".to_owned(),
                        )
                    })?;
                validate_piper_voice_selection(provider, source_id, candidate.model.as_deref())?;
                validate_voice_direction(
                    &candidate.performance,
                    &audiobookai_core::TimingSettings::default(),
                    candidate.model.as_deref().or(provider.model.as_deref()),
                    provider.capabilities.as_ref(),
                )?;
                Ok(VoiceAssignmentView {
                    provider_profile_id: candidate.provider_profile_id,
                    provider_name: provider.name.clone(),
                    voice_id: candidate.voice_id,
                    voice_name: voice.name.clone(),
                    model: candidate.model.clone(),
                    performance: candidate.performance.clone(),
                    timing: audiobookai_core::TimingSettings::default(),
                })
            })
            .collect::<Result<Vec<_>, ServiceError>>()?
    };

    let mut results = Vec::with_capacity(input.candidates.len());
    let text = input.text.clone();
    let character_id = input.character_id;
    for (candidate, assignment) in input.candidates.into_iter().zip(assignments) {
        let preview = crate::conversion::audition(
            Arc::clone(&state),
            project_id,
            text.clone(),
            character_id,
            assignment,
        )
        .await;
        let (preview, error) = match preview {
            Ok(preview) => (Some(preview), None),
            Err(error) => (None, Some(public_audition_error(&error))),
        };
        results.push(VoiceAuditionResult {
            candidate_id: candidate.candidate_id,
            provider_profile_id: candidate.provider_profile_id,
            voice_id: candidate.voice_id,
            preview,
            error,
        });
    }
    Ok(Json(VoiceAuditionResponse {
        results,
        potentially_billable: true,
    }))
}

pub(super) fn public_audition_error(error: &ServiceError) -> String {
    match error {
        ServiceError::InvalidRequest(detail)
        | ServiceError::Conflict(detail)
        | ServiceError::Unauthorized(detail)
        | ServiceError::Forbidden(detail)
        | ServiceError::RateLimited(detail)
        | ServiceError::ConflictDetails { detail, .. } => detail.clone(),
        ServiceError::NotFound => "an audition resource is unavailable".to_owned(),
        ServiceError::DataDirectoryUnavailable
        | ServiceError::TlsRequiredForLan(_)
        | ServiceError::TlsConfiguration(_)
        | ServiceError::Io(_)
        | ServiceError::Join(_)
        | ServiceError::Storage(_)
        | ServiceError::Internal(_) => "the audition could not be completed".to_owned(),
    }
}

#[derive(Clone, Debug)]
pub(super) struct ProviderCharacterEstimate {
    pub(super) provider_id: Uuid,
    pub(super) provider_name: String,
    pub(super) model: Option<String>,
    pub(super) characters: u64,
    pub(super) duration_seconds: u64,
    pub(super) card: Option<audiobookai_core::RateCard>,
    pub(super) cost: Option<audiobookai_core::Money>,
    pub(super) credits: Option<i64>,
}

#[allow(clippy::too_many_lines)]
pub(super) async fn estimate_project(
    state: &AppState,
    project: &ProjectDetail,
    characters: &[crate::models::CharacterView],
    providers: &HashMap<Uuid, ProviderProfileView>,
) -> Result<EstimateView, ServiceError> {
    let selected = project.chapters.iter().filter(|chapter| chapter.selected);
    let selected_chapters = selected.clone().count();
    let total_characters = selected
        .clone()
        .map(|chapter| u64::try_from(chapter.character_count).unwrap_or(u64::MAX))
        .sum();
    let estimated_duration_seconds = estimated_seconds(total_characters);
    let lines = priced_assignment_estimates(state, project, characters, providers).await?;
    let all_costs_known = !lines.is_empty() && lines.iter().all(|line| line.cost.is_some());
    let currencies = lines
        .iter()
        .filter_map(|line| line.cost.as_ref().map(|cost| cost.currency.clone()))
        .collect::<BTreeSet<_>>();
    let currency = (all_costs_known && currencies.len() == 1)
        .then(|| currencies.iter().next().cloned())
        .flatten();
    let monetary_cost_micros = currency.as_ref().map(|_| {
        lines
            .iter()
            .filter_map(|line| line.cost.as_ref())
            .fold(0_i64, |total, cost| total.saturating_add(cost.micros))
    });
    let credits =
        (!lines.is_empty() && lines.iter().all(|line| line.credits.is_some())).then(|| {
            lines
                .iter()
                .filter_map(|line| line.credits)
                .fold(0_i64, i64::saturating_add)
        });
    let priced_cards = lines
        .iter()
        .filter(|line| line.cost.is_some())
        .filter_map(|line| line.card.as_ref())
        .collect::<Vec<_>>();
    let price_source = (priced_cards.len() == lines.len() && !priced_cards.is_empty()).then(|| {
        priced_cards
            .iter()
            .map(|card| card.source.clone())
            .collect::<BTreeSet<_>>()
            .into_iter()
            .collect::<Vec<_>>()
            .join("; ")
    });
    let price_effective_at = (priced_cards.len() == lines.len() && !priced_cards.is_empty())
        .then(|| priced_cards.iter().map(|card| card.effective_at).min())
        .flatten();
    let mut unknown_fields = vec![
        "TTS token count (no provider tokenizer selected)".to_owned(),
        "provider throughput".to_owned(),
    ];
    if lines.is_empty() {
        unknown_fields.push("provider pricing (voice assignments incomplete)".to_owned());
        unknown_fields.push("provider credits (voice assignments incomplete)".to_owned());
    } else {
        for line in &lines {
            let model = line.model.as_deref().unwrap_or("provider default");
            if line.cost.is_none() {
                unknown_fields.push(format!(
                    "provider pricing ({} / {model})",
                    line.provider_name
                ));
            }
            if line.credits.is_none() {
                unknown_fields.push(format!(
                    "provider credits ({} / {model})",
                    line.provider_name
                ));
            }
        }
        if all_costs_known && currencies.len() > 1 {
            unknown_fields.push("aggregate cost (multiple currencies)".to_owned());
        }
    }
    unknown_fields.sort();
    unknown_fields.dedup();
    let provider_estimates = lines
        .iter()
        .map(|line| crate::models::ProviderEstimateView {
            provider_profile_id: line.provider_id,
            provider_name: line.provider_name.clone(),
            model: line.model.clone(),
            characters: line.characters,
            estimated_duration_seconds: line.duration_seconds,
            monetary_cost_micros: line.cost.as_ref().map(|cost| cost.micros),
            currency: line.cost.as_ref().map(|cost| cost.currency.clone()),
            credits: line.credits,
            rate_card_id: line.card.as_ref().map(|card| card.id.as_uuid()),
            price_source: line.card.as_ref().map(|card| card.source.clone()),
            price_effective_at: line.card.as_ref().map(|card| card.effective_at),
        })
        .collect();
    Ok(EstimateView {
        selected_chapters,
        characters: total_characters,
        estimated_tokens: None,
        estimated_duration_seconds,
        estimated_disk_bytes: estimated_duration_seconds.saturating_mul(96_000),
        estimated_completion_seconds_low: None,
        estimated_completion_seconds_high: None,
        monetary_cost_micros,
        currency,
        credits,
        price_source,
        price_effective_at,
        provider_estimates,
        unknown_fields,
    })
}

#[allow(clippy::too_many_lines)]
pub(super) async fn priced_assignment_estimates(
    state: &AppState,
    project: &ProjectDetail,
    characters: &[crate::models::CharacterView],
    providers: &HashMap<Uuid, ProviderProfileView>,
) -> Result<Vec<ProviderCharacterEstimate>, ServiceError> {
    use audiobookai_core::{ProviderProfileId, UsageQuantities, UsageWorkload};

    let selected_chapters = project
        .chapters
        .iter()
        .filter(|chapter| chapter.selected)
        .map(|chapter| chapter.id)
        .collect::<HashSet<_>>();
    let total_characters = project
        .chapters
        .iter()
        .filter(|chapter| chapter.selected)
        .map(|chapter| u64::try_from(chapter.character_count).unwrap_or(u64::MAX))
        .fold(0_u64, u64::saturating_add);
    let mut by_character = HashMap::<Uuid, u64>::new();
    let mut remaining_characters = total_characters;
    for character in characters
        .iter()
        .filter(|character| !matches!(character.role, audiobookai_core::CharacterRole::Narrator))
    {
        let count = character
            .evidence
            .iter()
            .filter(|evidence| selected_chapters.contains(&evidence.chapter_id))
            .map(|evidence| {
                u64::try_from(evidence.end_offset.saturating_sub(evidence.start_offset))
                    .unwrap_or(u64::MAX)
            })
            .fold(0_u64, u64::saturating_add);
        let count = count.min(remaining_characters);
        remaining_characters = remaining_characters.saturating_sub(count);
        by_character.insert(character.id, count);
    }
    if let Some(narrator) = characters
        .iter()
        .find(|character| matches!(character.role, audiobookai_core::CharacterRole::Narrator))
    {
        by_character.insert(narrator.id, remaining_characters);
    }
    let mut grouped = BTreeMap::<(Uuid, Option<String>), (String, u64)>::new();
    for character in characters {
        let Some(assignment) = &character.voice_assignment else {
            continue;
        };
        let count = by_character.get(&character.id).copied().unwrap_or_default();
        if count == 0 {
            continue;
        }
        let provider = providers.get(&assignment.provider_profile_id);
        let model = assignment
            .model
            .clone()
            .or_else(|| provider.and_then(|provider| provider.model.clone()));
        let name = provider.map_or_else(
            || assignment.provider_name.clone(),
            |provider| provider.name.clone(),
        );
        let entry = grouped
            .entry((assignment.provider_profile_id, model))
            .or_insert((name, 0));
        entry.1 = entry.1.saturating_add(count);
    }
    let mut lines = Vec::with_capacity(grouped.len());
    for ((provider_id, model), (provider_name, characters)) in grouped {
        let card = crate::accounting::applicable_rate_card(
            state,
            ProviderProfileId::from_uuid(provider_id),
            UsageWorkload::Tts,
            model.as_deref(),
        )
        .await?;
        let duration_seconds = estimated_seconds(characters);
        let quantities = UsageQuantities {
            characters: Some(characters),
            audio_milliseconds: Some(duration_seconds.saturating_mul(1_000)),
            ..UsageQuantities::default()
        };
        let cost = card
            .as_ref()
            .and_then(|card| crate::accounting::price_quantities(card, &quantities));
        let credits = card.as_ref().and_then(|card| {
            let rate = [
                "provider_credits_per_character_micros",
                "credits_per_character_micros",
            ]
            .iter()
            .find_map(|key| card.pricing.get(*key).copied())?;
            i64::try_from(characters)
                .ok()
                .map(|count| count.saturating_mul(rate))
        });
        lines.push(ProviderCharacterEstimate {
            provider_id,
            provider_name,
            model,
            characters,
            duration_seconds,
            card,
            cost,
            credits,
        });
    }
    Ok(lines)
}

pub(super) fn dry_run_project(
    project: &ProjectDetail,
    characters: &[crate::models::CharacterView],
    providers: &std::collections::HashMap<Uuid, ProviderProfileView>,
) -> DryRunView {
    let selected = project
        .chapters
        .iter()
        .filter(|chapter| chapter.selected)
        .count();
    let assignments_complete = !characters.is_empty()
        && characters
            .iter()
            .all(|character| character.voice_assignment.is_some());
    let mut provider_issues = BTreeSet::new();
    for character in characters {
        let Some(assignment) = &character.voice_assignment else {
            continue;
        };
        let Some(provider) = providers.get(&assignment.provider_profile_id) else {
            provider_issues.insert(format!(
                "{} uses a provider that no longer exists",
                character.canonical_name
            ));
            continue;
        };
        if !provider_capabilities_are_fresh(provider)
            || !provider
                .capabilities
                .as_ref()
                .is_some_and(|capabilities| capabilities.tts)
        {
            provider_issues.insert(format!("{} has no verified TTS capability", provider.name));
        }
        if !matches!(provider.status, ProviderStatusView::Online) {
            provider_issues.insert(format!("{} is not online", provider.name));
        }
        if matches!(provider.mode, ProviderModeView::CloudRemote) && !provider.credential_configured
        {
            provider_issues.insert(format!("{} has no credential", provider.name));
        }
        if matches!(provider.mode, ProviderModeView::CloudRemote) && !project.consent_cloud_text {
            provider_issues.insert(format!("{} has no project text consent", provider.name));
        }
    }
    let providers_ready = assignments_complete && provider_issues.is_empty();
    let provider_detail = if provider_issues.is_empty() {
        "Every assigned TTS provider is online and permitted".to_owned()
    } else {
        provider_issues.into_iter().collect::<Vec<_>>().join("; ")
    };
    let checks = vec![
        check(
            "chapters",
            "Chapter selection",
            selected > 0,
            format!("{selected} chapter(s) selected"),
            "Select at least one chapter",
        ),
        check(
            "character_review",
            "Character review",
            matches!(project.character_review_status, ReviewStatus::Approved),
            "Character dialogue has been reviewed".to_owned(),
            "Review and approve detected speakers",
        ),
        check(
            "voice_assignments",
            "Voice assignments",
            assignments_complete,
            "Every detected character has a voice".to_owned(),
            "Assign narrator and character voices",
        ),
        check(
            "providers",
            "TTS providers",
            providers_ready,
            provider_detail.clone(),
            &provider_detail,
        ),
    ];
    DryRunView {
        ready: checks
            .iter()
            .all(|check| matches!(check.status, CheckStatus::Pass)),
        checked_at: Utc::now(),
        checks,
    }
}

#[allow(clippy::too_many_lines)]
pub(super) async fn extend_dry_run_environment(
    state: &AppState,
    project: &ProjectDetail,
    characters: &[crate::models::CharacterView],
    providers: &HashMap<Uuid, ProviderProfileView>,
    export: &ExportOptionsInput,
    mut dry_run: DryRunView,
) -> Result<DryRunView, ServiceError> {
    let (export_valid, export_detail) = validate_dry_run_export(export).await;
    dry_run.checks.push(check(
        "export_settings",
        "Export settings",
        export_valid,
        export_detail.clone(),
        &export_detail,
    ));

    let output = export
        .output_directory
        .as_deref()
        .map_or_else(|| state.config.data_dir.join("exports"), PathBuf::from);
    let (output_valid, output_detail, existing_output_ancestor) =
        inspect_output_location(&output).await;
    dry_run.checks.push(check(
        "output_location",
        "Output location",
        output_valid,
        output_detail.clone(),
        &output_detail,
    ));

    let (codecs_ready, codec_detail) = inspect_media_tools(state).await;
    dry_run.checks.push(check(
        "media_codecs",
        "Media codecs",
        codecs_ready,
        codec_detail.clone(),
        &codec_detail,
    ));

    let estimate = estimate_project(state, project, characters, providers).await?;
    let estimated_disk_bytes = estimate.estimated_disk_bytes;
    let (disk_ready, disk_detail) = if let Some(directory) = existing_output_ancestor {
        let available = tokio::task::spawn_blocking(move || fs2::available_space(directory))
            .await
            .ok()
            .and_then(Result::ok);
        available.map_or_else(
            || {
                (
                    false,
                    "Available output disk space could not be measured".to_owned(),
                )
            },
            |available| {
                (
                    available >= estimated_disk_bytes,
                    format!(
                        "{available} bytes available; {estimated_disk_bytes} bytes estimated for this conversion"
                    ),
                )
            },
        )
    } else {
        (
            false,
            "Output disk space cannot be checked until the path is valid".to_owned(),
        )
    };
    dry_run.checks.push(check(
        "disk_space",
        "Disk space",
        disk_ready,
        disk_detail.clone(),
        &disk_detail,
    ));

    let (dictionary_status, dictionary_detail, dictionary_action) =
        inspect_pronunciation_rules(state, project, characters).await;
    dry_run.checks.push(status_check(
        "pronunciation_dictionaries",
        "Pronunciation dictionaries",
        dictionary_status,
        dictionary_detail,
        dictionary_action,
    ));

    let (concurrency_status, concurrency_detail, concurrency_action) =
        inspect_concurrency(state, characters, providers).await?;
    dry_run.checks.push(status_check(
        "concurrency",
        "Provider concurrency",
        concurrency_status,
        concurrency_detail,
        concurrency_action,
    ));

    let (budget_status, budget_detail, budget_action) =
        inspect_budget_capacity(state, project, characters, providers).await?;
    dry_run.checks.push(status_check(
        "budgets",
        "Budgets and reservations",
        budget_status,
        budget_detail,
        budget_action,
    ));

    let cache_path = state.catalog.read().await.settings.cache_path.clone();
    let cache_limit = state.catalog.read().await.settings.cache_limit_bytes;
    let (cache_status, cache_detail, cache_action) =
        inspect_cache_readiness(&cache_path, cache_limit, estimated_disk_bytes).await;
    dry_run.checks.push(status_check(
        "cache",
        "Audio cache",
        cache_status,
        cache_detail,
        cache_action,
    ));
    dry_run.ready = dry_run
        .checks
        .iter()
        .all(|item| !matches!(item.status, CheckStatus::Fail | CheckStatus::Pending));
    Ok(dry_run)
}

pub(super) async fn inspect_pronunciation_rules(
    state: &AppState,
    project: &ProjectDetail,
    characters: &[crate::models::CharacterView],
) -> (CheckStatus, String, Option<String>) {
    let character_ids = characters
        .iter()
        .map(|character| character.id)
        .collect::<HashSet<_>>();
    let rules = state.catalog.read().await.pronunciation_rules.clone();
    let relevant = rules
        .iter()
        .filter(|rule| {
            rule.enabled
                && match rule.scope {
                    crate::models::PronunciationScopeView::Global => true,
                    crate::models::PronunciationScopeView::Project => {
                        rule.project_id == Some(project.summary.id)
                    }
                }
        })
        .collect::<Vec<_>>();
    let mut invalid = Vec::new();
    let mut conflicts = Vec::new();
    for rule in &relevant {
        if rule.source.trim().is_empty() || rule.replacement.trim().is_empty() {
            invalid.push(rule.id.to_string());
        }
        if rule
            .character_id
            .is_some_and(|character_id| !character_ids.contains(&character_id))
        {
            invalid.push(rule.id.to_string());
        }
        if matches!(rule.kind, crate::models::PronunciationKindView::Regex)
            && regex::RegexBuilder::new(&rule.source)
                .case_insensitive(!rule.case_sensitive)
                .build()
                .is_err()
        {
            invalid.push(rule.id.to_string());
        }
    }
    for (index, rule) in relevant.iter().enumerate() {
        if relevant.iter().skip(index.saturating_add(1)).any(|other| {
            rule.project_id == other.project_id
                && rule.language == other.language
                && rule.character_id == other.character_id
                && rule.source.eq_ignore_ascii_case(&other.source)
        }) {
            conflicts.push(rule.id.to_string());
        }
    }
    invalid.sort();
    invalid.dedup();
    if !invalid.is_empty() {
        return (
            CheckStatus::Fail,
            format!(
                "{} enabled pronunciation rule(s) are invalid",
                invalid.len()
            ),
            Some("Repair or disable the invalid pronunciation rules".to_owned()),
        );
    }
    if !conflicts.is_empty() {
        return (
            CheckStatus::Warning,
            format!(
                "{} enabled rule(s) overlap; deterministic precedence will be used",
                conflicts.len()
            ),
            Some("Review the pronunciation conflict preview".to_owned()),
        );
    }
    (
        CheckStatus::Pass,
        format!(
            "{} applicable enabled pronunciation rule(s) validated",
            relevant.len()
        ),
        None,
    )
}

pub(super) async fn inspect_concurrency(
    state: &AppState,
    characters: &[crate::models::CharacterView],
    providers: &HashMap<Uuid, ProviderProfileView>,
) -> Result<(CheckStatus, String, Option<String>), ServiceError> {
    let global = state.catalog.read().await.settings.default_concurrency;
    if !(1..=32).contains(&global) {
        return Ok((
            CheckStatus::Fail,
            format!("Global chapter concurrency {global} is outside the supported range"),
            Some("Set global chapter concurrency between 1 and 32".to_owned()),
        ));
    }
    let assigned = characters
        .iter()
        .filter_map(|character| {
            character
                .voice_assignment
                .as_ref()
                .map(|assignment| assignment.provider_profile_id)
        })
        .collect::<BTreeSet<_>>();
    let mut serialized = Vec::new();
    let mut verified = Vec::new();
    for provider_id in assigned {
        let Some(provider) = providers.get(&provider_id) else {
            continue;
        };
        match provider_capabilities_are_fresh(provider)
            .then_some(provider.capabilities.as_ref())
            .flatten()
            .and_then(|capabilities| capabilities.max_concurrency)
        {
            Some(limit) if limit > 0 => verified.push(format!("{}={limit}", provider.name)),
            Some(_) => {
                return Ok((
                    CheckStatus::Fail,
                    format!("{} reports an invalid zero concurrency", provider.name),
                    Some("Refresh or correct the provider capability snapshot".to_owned()),
                ));
            }
            None => serialized.push(provider.name.clone()),
        }
    }
    if !serialized.is_empty() {
        return Ok((
            CheckStatus::Warning,
            format!(
                "Global chapter concurrency is {global}; unknown provider concurrency defaults to one for {}",
                serialized.join(", ")
            ),
            Some("Optionally configure a verified provider concurrency override".to_owned()),
        ));
    }
    Ok((
        CheckStatus::Pass,
        if verified.is_empty() {
            format!("Global chapter concurrency is {global}")
        } else {
            format!(
                "Global chapter concurrency is {global}; provider limits: {}",
                verified.join(", ")
            )
        },
        None,
    ))
}

#[allow(clippy::too_many_lines)]
pub(super) async fn inspect_budget_capacity(
    state: &AppState,
    project: &ProjectDetail,
    characters: &[crate::models::CharacterView],
    providers: &HashMap<Uuid, ProviderProfileView>,
) -> Result<(CheckStatus, String, Option<String>), ServiceError> {
    use audiobookai_core::{BudgetPeriod, BudgetScopeKind};

    let budgets = state
        .database
        .repositories()
        .budgets
        .list_enabled()
        .await
        .map_err(|error| ServiceError::Storage(error.to_string()))?;
    if budgets.is_empty() {
        return Ok((
            CheckStatus::Pass,
            "No enabled budget applies to this conversion".to_owned(),
            None,
        ));
    }
    let lines = priced_assignment_estimates(state, project, characters, providers).await?;
    let mut failures = Vec::new();
    let mut warnings = Vec::new();
    let mut checked = 0_usize;
    for budget in budgets {
        let applicable = lines
            .iter()
            .filter(|line| match budget.scope.kind {
                BudgetScopeKind::Global => true,
                BudgetScopeKind::Provider => budget
                    .scope
                    .provider_profile_id
                    .is_some_and(|id| id.as_uuid() == line.provider_id),
            })
            .collect::<Vec<_>>();
        if applicable.is_empty() {
            continue;
        }
        checked = checked.saturating_add(1);
        let projected = applicable.iter().try_fold(0_i64, |total, line| {
            budget_line_amount(&budget, line).map(|amount| total.saturating_add(amount.max(0)))
        });
        let reserved = state
            .database
            .repositories()
            .budgets
            .active_reserved(budget.id)
            .await
            .map_err(|error| ServiceError::Storage(error.to_string()))?;
        let remaining = if matches!(budget.period, BudgetPeriod::Job) {
            budget.limit
        } else {
            budget.remaining(reserved)
        };
        let issue = match projected {
            None => Some(format!(
                "{} has an unknown {:?} estimate",
                budget.name, budget.metric
            )),
            Some(amount) if amount > remaining => Some(format!(
                "{} needs {amount}, but only {remaining} remains",
                budget.name
            )),
            Some(amount) => {
                let projected_total = if matches!(budget.period, BudgetPeriod::Job) {
                    amount
                } else {
                    budget.used.saturating_add(reserved).saturating_add(amount)
                };
                let threshold = budget
                    .limit
                    .saturating_mul(i64::from(budget.warning_threshold_percent))
                    / 100;
                (projected_total >= threshold && budget.warning_threshold_percent > 0)
                    .then(|| format!("{} will cross its warning threshold", budget.name))
            }
        };
        if let Some(issue) = issue {
            if budget.hard && projected.is_none_or(|amount| amount > remaining) {
                failures.push(issue);
            } else {
                warnings.push(issue);
            }
        }
    }
    if !failures.is_empty() {
        return Ok((
            CheckStatus::Fail,
            failures.join("; "),
            Some(
                "Configure compatible rate cards, free budget capacity, or use an explicit per-job override"
                    .to_owned(),
            ),
        ));
    }
    if !warnings.is_empty() {
        return Ok((
            CheckStatus::Warning,
            warnings.join("; "),
            Some("Review soft-budget warnings before conversion".to_owned()),
        ));
    }
    Ok((
        CheckStatus::Pass,
        format!(
            "{checked} applicable budget(s) have sufficient unreserved capacity; no reservation was created"
        ),
        None,
    ))
}

pub(super) fn budget_line_amount(
    budget: &audiobookai_core::Budget,
    line: &ProviderCharacterEstimate,
) -> Option<i64> {
    use audiobookai_core::BudgetMetric;

    match budget.metric {
        BudgetMetric::MoneyMicros => {
            let cost = line.cost.as_ref()?;
            budget
                .currency
                .as_deref()?
                .eq_ignore_ascii_case(&cost.currency)
                .then_some(cost.micros)
        }
        BudgetMetric::Characters => i64::try_from(line.characters).ok(),
        BudgetMetric::AudioMilliseconds => {
            i64::try_from(line.duration_seconds.saturating_mul(1_000)).ok()
        }
        BudgetMetric::ProviderCredits => line.credits,
        BudgetMetric::InputTokens | BudgetMetric::OutputTokens | BudgetMetric::TotalTokens => None,
    }
}

pub(super) async fn inspect_cache_readiness(
    raw_path: &str,
    cache_limit: u64,
    estimated_bytes: u64,
) -> (CheckStatus, String, Option<String>) {
    let path = PathBuf::from(raw_path);
    let (valid, detail, ancestor) = inspect_output_location(&path).await;
    if !valid {
        return (
            CheckStatus::Fail,
            detail,
            Some("Repair the managed cache directory permissions".to_owned()),
        );
    }
    if cache_limit < estimated_bytes {
        return (
            CheckStatus::Fail,
            format!(
                "Cache limit is {cache_limit} bytes, below the {estimated_bytes}-byte conversion estimate"
            ),
            Some("Increase the cache limit before conversion".to_owned()),
        );
    }
    let Some(ancestor) = ancestor else {
        return (
            CheckStatus::Fail,
            "Cache disk space could not be inspected".to_owned(),
            Some("Repair the managed cache directory".to_owned()),
        );
    };
    let available = tokio::task::spawn_blocking(move || fs2::available_space(ancestor))
        .await
        .ok()
        .and_then(Result::ok);
    match available {
        Some(available) if available >= estimated_bytes => (
            CheckStatus::Pass,
            format!(
                "Cache is ready with {available} bytes available and a {cache_limit}-byte limit"
            ),
            None,
        ),
        Some(available) => (
            CheckStatus::Fail,
            format!(
                "Cache has {available} bytes available, below the {estimated_bytes}-byte estimate"
            ),
            Some("Free cache disk space before conversion".to_owned()),
        ),
        None => (
            CheckStatus::Fail,
            "Available cache disk space could not be measured".to_owned(),
            Some("Repair the managed cache directory".to_owned()),
        ),
    }
}

pub(super) async fn validate_dry_run_export(export: &ExportOptionsInput) -> (bool, String) {
    if !(32..=512).contains(&export.bitrate_kbps) {
        return (
            false,
            "Audio bitrate must be between 32 and 512 kbps".to_owned(),
        );
    }
    if !export.music_gain_db.is_finite() || !(-60.0..=0.0).contains(&export.music_gain_db) {
        return (
            false,
            "Background music gain must be between -60 and 0 dB".to_owned(),
        );
    }
    let Some(music) = export.background_music_path.as_deref() else {
        return (
            true,
            "Export format and audio settings are valid".to_owned(),
        );
    };
    if !export.confirm_background_music_owned {
        return (
            false,
            "Confirm that you own or may use the selected background audio".to_owned(),
        );
    }
    let path = FilePath::new(music);
    if !path.is_absolute() {
        return (
            false,
            "Background music must use an absolute path".to_owned(),
        );
    }
    match tokio::fs::metadata(path).await {
        Ok(metadata) if metadata.is_file() => (
            true,
            "Export settings and background audio are valid".to_owned(),
        ),
        Ok(_) => (false, "Background music is not a regular file".to_owned()),
        Err(error) => (false, format!("Background music is unavailable: {error}")),
    }
}

pub(super) async fn inspect_output_location(output: &FilePath) -> (bool, String, Option<PathBuf>) {
    if !output.is_absolute() {
        return (
            false,
            "The output directory must be an absolute path".to_owned(),
            None,
        );
    }
    let mut candidate = output.to_path_buf();
    loop {
        match tokio::fs::metadata(&candidate).await {
            Ok(metadata) if metadata.is_dir() => {
                if metadata.permissions().readonly() {
                    return (false, format!("{} is read-only", candidate.display()), None);
                }
                return (
                    true,
                    format!("Output will be written under {}", output.display()),
                    Some(candidate),
                );
            }
            Ok(_) => {
                return (
                    false,
                    format!("{} is not a directory", candidate.display()),
                    None,
                );
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                if !candidate.pop() {
                    return (
                        false,
                        "No existing output-directory ancestor was found".to_owned(),
                        None,
                    );
                }
            }
            Err(error) => {
                return (
                    false,
                    format!("Output location is unavailable: {error}"),
                    None,
                );
            }
        }
    }
}

pub(super) async fn inspect_media_tools(state: &AppState) -> (bool, String) {
    let executable = |name: &str| {
        if cfg!(windows) {
            format!("{name}.exe")
        } else {
            name.to_owned()
        }
    };
    if let Some(directory) = &state.config.bundled_sidecar_dir {
        let ffmpeg = directory.join(executable("ffmpeg"));
        let ffprobe = directory.join(executable("ffprobe"));
        let valid = ffmpeg.is_file() && ffprobe.is_file();
        return (
            valid,
            if valid {
                format!(
                    "Bundled FFmpeg and ffprobe found in {}",
                    directory.display()
                )
            } else {
                format!(
                    "Bundled FFmpeg or ffprobe is missing from {}",
                    directory.display()
                )
            },
        );
    }
    let ffmpeg = tokio::process::Command::new(executable("ffmpeg"))
        .args(["-hide_banner", "-version"])
        .kill_on_drop(true)
        .output()
        .await;
    let ffprobe = tokio::process::Command::new(executable("ffprobe"))
        .args(["-hide_banner", "-version"])
        .kill_on_drop(true)
        .output()
        .await;
    let valid = ffmpeg.as_ref().is_ok_and(|output| output.status.success())
        && ffprobe.as_ref().is_ok_and(|output| output.status.success());
    (
        valid,
        if valid {
            "Developer FFmpeg and ffprobe are available on PATH".to_owned()
        } else {
            "FFmpeg and ffprobe are unavailable; packaged releases require bundled sidecars"
                .to_owned()
        },
    )
}
