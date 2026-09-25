use super::{
    AppState, BTreeSet, Chapter, ChapterPlan, ConversionPlan, DialogueSpan, ExportProfile, HashMap,
    Paragraph, PathBuf, Project, ProjectId, PronunciationKindView, PronunciationRuleView,
    PronunciationScopeView, ProviderError, ProviderModeView, ProviderProfileId,
    ProviderProfileView, Row, SegmentId, SegmentPlan, ServiceError, Speaker, SpeakerAssignment,
    SpeakerOverride, Uuid, Validate, storage_error,
};

pub(super) async fn load_conversion_plan(
    state: &AppState,
    project_id: Uuid,
    export: ExportProfile,
    music_path: Option<PathBuf>,
) -> Result<ConversionPlan, ServiceError> {
    let repositories = state.database.repositories();
    let project = repositories
        .projects
        .get_project(ProjectId::from_uuid(project_id))
        .await
        .map_err(storage_error)?
        .ok_or(ServiceError::NotFound)?;
    let book = repositories
        .projects
        .get_book(project.book_id)
        .await
        .map_err(storage_error)?
        .ok_or(ServiceError::NotFound)?;
    let selected = repositories
        .projects
        .list_chapters(book.id)
        .await
        .map_err(storage_error)?
        .into_iter()
        .filter(|chapter| chapter.selected)
        .collect::<Vec<_>>();
    if selected.is_empty() {
        return Err(ServiceError::Conflict(
            "select at least one chapter before conversion".to_owned(),
        ));
    }

    let (characters, voices, providers, rules) = {
        let catalog = state.catalog.read().await;
        (
            catalog
                .characters
                .get(&project_id)
                .cloned()
                .unwrap_or_default(),
            catalog.voice_sources.clone(),
            catalog.providers.clone(),
            catalog.pronunciation_rules.clone(),
        )
    };
    if !matches!(project.status, audiobookai_core::ProjectStatus::Ready)
        || project.character_reviewed_at.is_none()
    {
        return Err(ServiceError::Conflict(
            "approve the character review before paid synthesis".to_owned(),
        ));
    }
    let assignments = build_assignments(&project, &characters, &voices, &providers, state).await?;
    let narrator_id = characters
        .iter()
        .find(|character| matches!(character.role, audiobookai_core::CharacterRole::Narrator))
        .map(|character| character.id)
        .ok_or_else(|| {
            ServiceError::Conflict("the reviewed character set has no narrator".to_owned())
        })?;
    let detection_spans = load_detection_spans(state, project_id).await?;
    let overrides = load_speaker_overrides(state, project_id).await?;
    let mut chapter_plans = Vec::with_capacity(selected.len());
    let mut playback_ordinal = 0_usize;
    for chapter in selected {
        let paragraphs = repositories
            .projects
            .list_paragraphs(chapter.id)
            .await
            .map_err(storage_error)?;
        let mut segments = segment_chapter(
            &project,
            &chapter,
            &paragraphs,
            narrator_id,
            &assignments,
            &detection_spans,
            &overrides,
            &rules,
        )?;
        if segments.is_empty() {
            return Err(ServiceError::Conflict(format!(
                "chapter '{}' contains no speakable text",
                chapter.title
            )));
        }
        for segment in &mut segments {
            segment.playback_ordinal = playback_ordinal;
            playback_ordinal = playback_ordinal.saturating_add(1);
        }
        chapter_plans.push(ChapterPlan { chapter, segments });
    }
    Ok(ConversionPlan {
        project,
        book,
        chapters: chapter_plans,
        rules,
        export,
        music_path,
    })
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum AssignmentPurpose {
    Semantic,
    Dispatch,
}

pub(super) async fn build_assignments(
    project: &Project,
    characters: &[crate::models::CharacterView],
    voice_sources: &HashMap<Uuid, String>,
    providers: &HashMap<Uuid, ProviderProfileView>,
    state: &AppState,
) -> Result<HashMap<Uuid, SpeakerAssignment>, ServiceError> {
    build_assignments_for(
        project,
        characters,
        voice_sources,
        providers,
        state,
        AssignmentPurpose::Dispatch,
    )
    .await
}

#[allow(clippy::too_many_lines)]
pub(super) async fn build_assignments_for(
    project: &Project,
    characters: &[crate::models::CharacterView],
    voice_sources: &HashMap<Uuid, String>,
    providers: &HashMap<Uuid, ProviderProfileView>,
    state: &AppState,
    purpose: AssignmentPurpose,
) -> Result<HashMap<Uuid, SpeakerAssignment>, ServiceError> {
    let mut result = HashMap::new();
    for character in characters {
        let assignment = character.voice_assignment.as_ref().ok_or_else(|| {
            ServiceError::Conflict(format!(
                "assign a voice to '{}' before conversion",
                character.canonical_name
            ))
        })?;
        let provider = providers
            .get(&assignment.provider_profile_id)
            .ok_or_else(|| {
                ServiceError::Conflict(format!(
                    "the provider assigned to '{}' no longer exists",
                    character.canonical_name
                ))
            })?;
        if purpose == AssignmentPurpose::Dispatch {
            crate::api::validate_billable_tts_provider_readiness(provider)?;
            if matches!(provider.mode, ProviderModeView::CloudRemote)
                && !project.cloud_consent.book_text
            {
                return Err(ServiceError::Conflict(format!(
                    "grant project consent before sending book text to {}",
                    provider.name
                )));
            }
        }
        let voice_source = voice_sources
            .get(&assignment.voice_id)
            .cloned()
            .ok_or_else(|| {
                ServiceError::Conflict(format!(
                    "the voice assigned to '{}' is no longer available",
                    character.canonical_name
                ))
            })?;
        crate::api::validate_piper_voice_selection(
            provider,
            &voice_source,
            assignment.model.as_deref(),
        )?;
        let domain_provider = state
            .database
            .repositories()
            .providers
            .get(ProviderProfileId::from_uuid(assignment.provider_profile_id))
            .await
            .map_err(storage_error)?;
        let concurrency = domain_provider
            .as_ref()
            .map_or(1, audiobookai_core::ProviderProfile::effective_concurrency);
        let provider_version = domain_provider
            .as_ref()
            .and_then(|profile| profile.capability_snapshot.as_ref())
            .and_then(|snapshot| snapshot.provider_version.clone());
        let provider_snapshot_id = domain_provider
            .as_ref()
            .and_then(|profile| profile.capability_snapshot.as_ref())
            .map(|snapshot| snapshot.id.as_uuid());
        let model = assignment.model.clone().or_else(|| provider.model.clone());
        if purpose == AssignmentPurpose::Dispatch {
            crate::api::validate_voice_direction(
                &assignment.performance,
                &assignment.timing,
                model.as_deref(),
                provider.capabilities.as_ref(),
            )?;
        } else {
            if let Some(issue) = assignment
                .performance
                .validation_issues()
                .into_iter()
                .next()
            {
                return Err(ServiceError::InvalidRequest(issue.message));
            }
            if let Some(issue) = assignment.timing.validation_issues().into_iter().next() {
                return Err(ServiceError::InvalidRequest(issue.message));
            }
        }
        result.insert(
            character.id,
            SpeakerAssignment {
                character_id: character.id,
                character_name: character.canonical_name.clone(),
                provider_id: assignment.provider_profile_id,
                provider_name: provider.name.clone(),
                provider_kind: provider.kind.clone(),
                provider_role: Some(provider.role),
                provider_mode: Some(provider.mode),
                provider_endpoint: provider.endpoint.clone(),
                provider_snapshot_id,
                provider_version,
                provider_concurrency: concurrency,
                voice_id: assignment.voice_id,
                voice_source,
                voice_name: assignment.voice_name.clone(),
                model,
                performance: assignment.performance.clone(),
                timing: assignment.timing.clone(),
            },
        );
    }
    Ok(result)
}

pub(super) async fn validate_segment_dispatch_boundary(
    state: &AppState,
    project_id: Uuid,
    segment: &SegmentPlan,
) -> Result<(), ProviderError> {
    let (profile, voice_source, voice_belongs_to_provider, cloud_text_consent) = {
        let catalog = state.catalog.read().await;
        let profile = catalog
            .providers
            .get(&segment.assignment.provider_id)
            .cloned()
            .ok_or_else(|| ProviderError::Configuration("TTS provider was removed".to_owned()))?;
        let voice_source = catalog
            .voice_sources
            .get(&segment.assignment.voice_id)
            .cloned();
        let voice_belongs_to_provider = catalog.voices.iter().any(|voice| {
            voice.id == segment.assignment.voice_id
                && voice.provider_profile_id == segment.assignment.provider_id
        });
        let cloud_text_consent = catalog
            .projects
            .get(&project_id)
            .is_some_and(|project| project.consent_cloud_text);
        (
            profile,
            voice_source,
            voice_belongs_to_provider,
            cloud_text_consent,
        )
    };
    crate::api::validate_billable_tts_provider_readiness(&profile)
        .map_err(|error| ProviderError::Configuration(error.to_string()))?;
    if profile.kind != segment.assignment.provider_kind
        || !matches!(profile.role, crate::models::ProviderRoleView::Tts)
        || segment
            .assignment
            .provider_role
            .is_some_and(|role| role != profile.role)
        || Some(profile.mode) != segment.assignment.provider_mode
        || profile.endpoint != segment.assignment.provider_endpoint
    {
        return Err(ProviderError::Configuration(
            "TTS provider routing changed after this job was admitted".to_owned(),
        ));
    }
    if matches!(profile.mode, ProviderModeView::CloudRemote) && !cloud_text_consent {
        return Err(ProviderError::Configuration(
            "cloud-text consent is not active for this dispatch".to_owned(),
        ));
    }
    if !voice_belongs_to_provider
        || voice_source.as_deref() != Some(segment.assignment.voice_source.as_str())
    {
        return Err(ProviderError::Configuration(
            "the selected provider voice changed after this job was admitted".to_owned(),
        ));
    }
    crate::api::validate_voice_direction(
        &segment.assignment.performance,
        &segment.assignment.timing,
        segment.assignment.model.as_deref(),
        profile.capabilities.as_ref(),
    )
    .map_err(|error| ProviderError::Configuration(error.to_string()))?;

    let current_snapshot_id = state
        .database
        .repositories()
        .providers
        .get(ProviderProfileId::from_uuid(segment.assignment.provider_id))
        .await
        .map_err(|error| ProviderError::Process(error.to_string()))?
        .and_then(|provider| provider.capability_snapshot)
        .map(|snapshot| snapshot.id.as_uuid());
    if segment.assignment.provider_snapshot_id.is_none()
        || current_snapshot_id != segment.assignment.provider_snapshot_id
    {
        return Err(ProviderError::Configuration(
            "TTS provider capability or credential snapshot changed after this job was admitted"
                .to_owned(),
        ));
    }
    Ok(())
}

pub(super) async fn load_detection_spans(
    state: &AppState,
    project_id: Uuid,
) -> Result<HashMap<Uuid, Vec<DialogueSpan>>, ServiceError> {
    let run_id = sqlx::query_scalar::<_, String>(
        "SELECT id FROM detection_runs WHERE project_id = ? AND status = 'completed' \
         ORDER BY completed_at DESC, created_at DESC LIMIT 1",
    )
    .bind(project_id.to_string())
    .fetch_optional(state.database.pool())
    .await
    .map_err(storage_error)?;
    let Some(run_id) = run_id else {
        return Ok(HashMap::new());
    };
    let rows = sqlx::query(
        "SELECT paragraph_id, payload FROM dialogue_spans WHERE detection_run_id = ? \
         ORDER BY paragraph_id, byte_start, byte_end",
    )
    .bind(run_id)
    .fetch_all(state.database.pool())
    .await
    .map_err(storage_error)?;
    let mut spans = HashMap::<Uuid, Vec<DialogueSpan>>::new();
    for row in rows {
        let paragraph_id = row.get::<String, _>("paragraph_id");
        let Ok(paragraph_id) = Uuid::parse_str(&paragraph_id) else {
            continue;
        };
        let payload = row.get::<String, _>("payload");
        if let Ok(span) = serde_json::from_str::<DialogueSpan>(&payload) {
            spans.entry(paragraph_id).or_default().push(span);
        }
    }
    Ok(spans)
}

pub(super) async fn load_speaker_overrides(
    state: &AppState,
    project_id: Uuid,
) -> Result<HashMap<Uuid, Vec<SpeakerOverride>>, ServiceError> {
    let rows = sqlx::query(
        "SELECT paragraph_id, payload FROM speaker_overrides WHERE project_id = ? \
         ORDER BY paragraph_id, byte_start, byte_end, updated_at",
    )
    .bind(project_id.to_string())
    .fetch_all(state.database.pool())
    .await
    .map_err(storage_error)?;
    let mut overrides = HashMap::<Uuid, Vec<SpeakerOverride>>::new();
    for row in rows {
        let paragraph_id = row.get::<String, _>("paragraph_id");
        let Ok(paragraph_id) = Uuid::parse_str(&paragraph_id) else {
            continue;
        };
        let payload = row.get::<String, _>("payload");
        if let Ok(value) = serde_json::from_str::<SpeakerOverride>(&payload) {
            overrides.entry(paragraph_id).or_default().push(value);
        }
    }
    Ok(overrides)
}

#[allow(clippy::too_many_arguments)]
pub(super) fn segment_chapter(
    project: &Project,
    chapter: &Chapter,
    paragraphs: &[Paragraph],
    narrator_id: Uuid,
    assignments: &HashMap<Uuid, SpeakerAssignment>,
    detection_spans: &HashMap<Uuid, Vec<DialogueSpan>>,
    overrides: &HashMap<Uuid, Vec<SpeakerOverride>>,
    rules: &[PronunciationRuleView],
) -> Result<Vec<SegmentPlan>, ServiceError> {
    let mut output = Vec::new();
    for paragraph in paragraphs {
        if paragraph.text.trim().is_empty() {
            continue;
        }
        let paragraph_id = paragraph.id.as_uuid();
        let valid_overrides = overrides
            .get(&paragraph_id)
            .into_iter()
            .flatten()
            .filter(|value| value.source_content_hash == paragraph.content_hash)
            .collect::<Vec<_>>();
        let detected = detection_spans
            .get(&paragraph_id)
            .map(Vec::as_slice)
            .unwrap_or_default();
        let ranges = speaker_ranges(
            &paragraph.text,
            narrator_id,
            detected,
            &valid_overrides,
            assignments,
        );
        for (start, end, character_id) in ranges {
            let Some(text) = paragraph.text.get(start..end) else {
                continue;
            };
            let original_text = text.trim();
            if original_text.is_empty() {
                continue;
            }
            let assignment = assignments.get(&character_id).cloned().ok_or_else(|| {
                ServiceError::Conflict(format!(
                    "a speaker in '{}' has no valid voice assignment",
                    chapter.title
                ))
            })?;
            let (text, applied_rule_ids, dictionary_revision) = apply_pronunciation_rules(
                original_text,
                rules,
                project.id.as_uuid(),
                character_id,
                project.metadata.language.as_deref(),
            )?;
            let segment_ordinal = u32::try_from(output.len()).unwrap_or(u32::MAX);
            let key = segment_key(chapter.id.as_uuid(), paragraph_id, start, end, character_id);
            output.push(SegmentPlan {
                id: SegmentId::new(),
                proofing: true,
                key,
                chapter_id: chapter.id.as_uuid(),
                paragraph_id,
                source_content_hash: paragraph.content_hash.clone(),
                byte_start: u64::try_from(start).unwrap_or(u64::MAX),
                byte_end: u64::try_from(end).unwrap_or(u64::MAX),
                chapter_title: chapter.title.clone(),
                segment_ordinal,
                playback_ordinal: 0,
                original_text: original_text.to_owned(),
                text,
                context: None,
                assignment,
                applied_rule_ids,
                dictionary_revision,
            });
        }
    }
    let contexts = output
        .iter()
        .map(|segment| segment.text.clone())
        .collect::<Vec<_>>();
    for (index, segment) in output.iter_mut().enumerate() {
        let before = index
            .checked_sub(1)
            .and_then(|previous| contexts.get(previous))
            .map(|value| trailing_characters(value, 160));
        let after = contexts
            .get(index.saturating_add(1))
            .map(|value| leading_characters(value, 160));
        segment.context = match (before, after) {
            (Some(before), Some(after)) => Some(format!("{before}\n---\n{after}")),
            (Some(value), None) | (None, Some(value)) => Some(value),
            (None, None) => None,
        };
    }
    // Ranges left between dialogue spans can consist only of quotation marks or dashes. They are
    // removed after context assignment so neighbouring segments keep their cache and proofing
    // identities; ordinals only order segments, so the resulting gaps are harmless.
    output.retain(|segment| is_speakable(&segment.original_text));
    Ok(output)
}

/// Returns false for text that a TTS engine cannot voice, such as a lone quotation mark.
pub(super) fn is_speakable(text: &str) -> bool {
    text.chars().any(char::is_alphanumeric)
}

pub(super) fn speaker_ranges(
    text: &str,
    narrator_id: Uuid,
    detected: &[DialogueSpan],
    overrides: &[&SpeakerOverride],
    assignments: &HashMap<Uuid, SpeakerAssignment>,
) -> Vec<(usize, usize, Uuid)> {
    #[derive(Clone, Copy)]
    struct Span {
        start: usize,
        end: usize,
        speaker: Uuid,
        manual: bool,
    }
    let mut spans = Vec::new();
    for detected in detected {
        let speaker = detected.character_id.as_uuid();
        if !assignments.contains_key(&speaker) {
            continue;
        }
        if let Some((start, end)) = valid_text_range(text, detected.byte_start, detected.byte_end) {
            spans.push(Span {
                start,
                end,
                speaker,
                manual: false,
            });
        }
    }
    for value in overrides {
        let Some(speaker) = speaker_character_id(&value.speaker, narrator_id, assignments) else {
            continue;
        };
        if let Some((start, end)) = valid_text_range(text, value.byte_start, value.byte_end) {
            spans.push(Span {
                start,
                end,
                speaker,
                manual: true,
            });
        }
    }
    let mut boundaries = BTreeSet::from([0, text.len()]);
    for span in &spans {
        boundaries.insert(span.start);
        boundaries.insert(span.end);
    }
    let boundaries = boundaries.into_iter().collect::<Vec<_>>();
    let mut ranges = Vec::<(usize, usize, Uuid)>::new();
    for pair in boundaries.windows(2) {
        let start = pair[0];
        let end = pair[1];
        if start == end {
            continue;
        }
        let speaker = spans
            .iter()
            .filter(|span| span.start <= start && span.end >= end)
            .max_by_key(|span| (span.manual, span.start, std::cmp::Reverse(span.end)))
            .map_or(narrator_id, |span| span.speaker);
        if let Some(last) = ranges.last_mut()
            && last.2 == speaker
            && last.1 == start
        {
            last.1 = end;
        } else {
            ranges.push((start, end, speaker));
        }
    }
    ranges
}

pub(super) fn speaker_character_id(
    speaker: &Speaker,
    narrator_id: Uuid,
    assignments: &HashMap<Uuid, SpeakerAssignment>,
) -> Option<Uuid> {
    match speaker {
        Speaker::Narrator => Some(narrator_id),
        Speaker::Character(id) => assignments
            .contains_key(&id.as_uuid())
            .then(|| id.as_uuid()),
        Speaker::Named(name) => assignments
            .values()
            .find(|assignment| assignment.character_name.eq_ignore_ascii_case(name))
            .map(|assignment| assignment.character_id),
    }
}

pub(super) fn valid_text_range(text: &str, start: u64, end: u64) -> Option<(usize, usize)> {
    let mut start = usize::try_from(start).ok()?.min(text.len());
    let mut end = usize::try_from(end).ok()?.min(text.len());
    while start < text.len() && !text.is_char_boundary(start) {
        start = start.saturating_add(1);
    }
    while end > start && !text.is_char_boundary(end) {
        end = end.saturating_sub(1);
    }
    (start < end).then_some((start, end))
}

pub(super) fn segment_key(
    chapter_id: Uuid,
    paragraph_id: Uuid,
    start: usize,
    end: usize,
    speaker_id: Uuid,
) -> String {
    let mut hasher = blake3::Hasher::new();
    for value in [
        chapter_id.to_string(),
        paragraph_id.to_string(),
        start.to_string(),
        end.to_string(),
        speaker_id.to_string(),
    ] {
        hasher.update(value.as_bytes());
        hasher.update(&[0]);
    }
    hasher.finalize().to_hex().to_string()
}

pub(super) fn leading_characters(value: &str, count: usize) -> String {
    value.chars().take(count).collect()
}

pub(super) fn trailing_characters(value: &str, count: usize) -> String {
    let mut characters = value.chars().rev().take(count).collect::<Vec<_>>();
    characters.reverse();
    characters.into_iter().collect()
}

pub(crate) fn apply_pronunciation_rules(
    text: &str,
    rules: &[PronunciationRuleView],
    project_id: Uuid,
    character_id: Uuid,
    language: Option<&str>,
) -> Result<(String, Vec<Uuid>, String), ServiceError> {
    let mut applicable = rules
        .iter()
        .filter(|rule| {
            rule.enabled
                && match rule.scope {
                    PronunciationScopeView::Global => true,
                    PronunciationScopeView::Project => rule.project_id == Some(project_id),
                }
                && rule.character_id.is_none_or(|id| id == character_id)
                && rule.language.as_deref().is_none_or(|rule_language| {
                    language.is_some_and(|value| value.eq_ignore_ascii_case(rule_language))
                })
        })
        .collect::<Vec<_>>();
    applicable.sort_by_key(|rule| {
        (
            matches!(rule.scope, PronunciationScopeView::Project),
            rule.order,
            rule.id,
        )
    });
    let mut transformed = text.to_owned();
    let mut applied = Vec::new();
    let mut revision = blake3::Hasher::new();
    for rule in applicable {
        revision.update(rule.id.as_bytes());
        revision.update(rule.source.as_bytes());
        revision.update(rule.replacement.as_bytes());
        let pattern = match rule.kind {
            PronunciationKindView::Literal | PronunciationKindView::Phoneme => {
                regex::escape(&rule.source)
            }
            PronunciationKindView::WholeWord | PronunciationKindView::Alias => {
                format!(r"\b{}\b", regex::escape(&rule.source))
            }
            PronunciationKindView::Regex => rule.source.clone(),
        };
        let expression = regex::RegexBuilder::new(&pattern)
            .case_insensitive(!rule.case_sensitive)
            .unicode(true)
            .build()
            .map_err(|error| {
                ServiceError::InvalidRequest(format!("invalid pronunciation regex: {error}"))
            })?;
        let before = transformed.clone();
        transformed = if matches!(rule.kind, PronunciationKindView::Regex) {
            expression
                .replace_all(&transformed, rule.replacement.as_str())
                .into_owned()
        } else {
            expression
                .replace_all(&transformed, |_captures: &regex::Captures<'_>| {
                    rule.replacement.as_str()
                })
                .into_owned()
        };
        if transformed != before {
            applied.push(rule.id);
        }
    }
    Ok((
        transformed,
        applied,
        revision.finalize().to_hex().to_string(),
    ))
}
