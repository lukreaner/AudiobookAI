use super::{
    AppState, AssignmentPurpose, ChapterId, CharacterId, ConversionPlan, Job, PerformanceSettings,
    ProductionSegment, ProductionSegmentSource, ProjectId, ProofingPlan, ProofingPlanStatus,
    SegmentPlan, SegmentReviewState, ServiceError, Speaker, TimingSettings, Utc, Uuid,
    apply_pronunciation_rules, build_assignments_for, segment_semantic_input_hash, storage_error,
};

#[allow(clippy::too_many_lines)]
pub(crate) async fn load_proofing_segment_plan(
    state: &AppState,
    project_id: Uuid,
    segment: &ProductionSegment,
) -> Result<SegmentPlan, ServiceError> {
    load_proofing_segment_plan_for(state, project_id, segment, AssignmentPurpose::Semantic).await
}

pub(super) async fn load_dispatchable_proofing_segment_plan(
    state: &AppState,
    project_id: Uuid,
    segment: &ProductionSegment,
) -> Result<SegmentPlan, ServiceError> {
    load_proofing_segment_plan_for(state, project_id, segment, AssignmentPurpose::Dispatch).await
}

#[allow(clippy::too_many_lines)]
pub(super) async fn load_proofing_segment_plan_for(
    state: &AppState,
    project_id: Uuid,
    segment: &ProductionSegment,
    purpose: AssignmentPurpose,
) -> Result<SegmentPlan, ServiceError> {
    let project = state
        .database
        .repositories()
        .projects
        .get_project(ProjectId::from_uuid(project_id))
        .await
        .map_err(storage_error)?
        .ok_or(ServiceError::NotFound)?;
    let (characters, voices, providers, rules, chapter_title) = {
        let catalog = state.catalog.read().await;
        let characters = catalog
            .characters
            .get(&project_id)
            .cloned()
            .unwrap_or_default();
        let chapter_title = segment
            .chapter_id
            .and_then(|id| {
                catalog
                    .projects
                    .get(&project_id)
                    .and_then(|project| {
                        project
                            .chapters
                            .iter()
                            .find(|chapter| chapter.id == id.as_uuid())
                    })
                    .map(|chapter| chapter.title.clone())
            })
            .unwrap_or_else(|| "Production credit".to_owned());
        (
            characters,
            catalog.voice_sources.clone(),
            catalog.providers.clone(),
            catalog.pronunciation_rules.clone(),
            chapter_title,
        )
    };
    let character_id = match &segment.speaker {
        Speaker::Character(id) => id.as_uuid(),
        Speaker::Narrator => characters
            .iter()
            .find(|character| character.role == audiobookai_core::CharacterRole::Narrator)
            .map(|character| character.id)
            .ok_or_else(|| ServiceError::Conflict("the narrator is unavailable".to_owned()))?,
        Speaker::Named(name) => characters
            .iter()
            .find(|character| character.canonical_name.eq_ignore_ascii_case(name))
            .map(|character| character.id)
            .ok_or_else(|| {
                ServiceError::Conflict("the segment speaker is unavailable".to_owned())
            })?,
    };
    let character = characters
        .iter()
        .find(|character| character.id == character_id)
        .cloned()
        .ok_or_else(|| ServiceError::Conflict("the segment speaker is unavailable".to_owned()))?;
    let assignments = build_assignments_for(
        &project,
        std::slice::from_ref(&character),
        &voices,
        &providers,
        state,
        purpose,
    )
    .await?;
    let mut assignment = assignments
        .get(&character_id)
        .cloned()
        .ok_or_else(|| ServiceError::Conflict("the segment speaker has no voice".to_owned()))?;
    assignment.performance = assignment
        .performance
        .overlay(&segment.performance_override);
    assignment.timing = TimingSettings {
        pause_before_ms: segment
            .timing_override
            .pause_before_ms
            .or(assignment.timing.pause_before_ms),
        pause_after_ms: segment
            .timing_override
            .pause_after_ms
            .or(assignment.timing.pause_after_ms),
    };
    let base_text = segment
        .narration_text_override
        .as_deref()
        .unwrap_or(&segment.original_text);
    let (text, applied_rule_ids, dictionary_revision) = apply_pronunciation_rules(
        base_text,
        &rules,
        project_id,
        character_id,
        project.metadata.language.as_deref(),
    )?;
    let context = match (&segment.context_before, &segment.context_after) {
        (Some(before), Some(after)) => Some(format!("{before}\n---\n{after}")),
        (Some(value), None) | (None, Some(value)) => Some(value.clone()),
        (None, None) => None,
    };
    Ok(SegmentPlan {
        id: segment.id,
        proofing: true,
        key: segment.stable_key.clone(),
        chapter_id: segment
            .chapter_id
            .ok_or_else(|| {
                ServiceError::Conflict("credit regeneration is not available yet".to_owned())
            })?
            .as_uuid(),
        paragraph_id: segment
            .paragraph_id
            .ok_or_else(|| ServiceError::Conflict("segment source is unavailable".to_owned()))?
            .as_uuid(),
        source_content_hash: segment.source_content_hash.clone(),
        byte_start: segment.byte_start.unwrap_or_default(),
        byte_end: segment.byte_end.unwrap_or_default(),
        chapter_title,
        segment_ordinal: segment.ordinal,
        playback_ordinal: usize::try_from(segment.ordinal).unwrap_or(usize::MAX),
        original_text: segment.original_text.clone(),
        text,
        context,
        assignment,
        applied_rule_ids,
        dictionary_revision,
    })
}

pub(super) async fn build_proofing_plan(
    state: &AppState,
    job: &Job,
    conversion: &ConversionPlan,
) -> Result<(ProofingPlan, Vec<ProductionSegment>), ServiceError> {
    let repositories = state.database.repositories();
    let previous = repositories
        .proofing
        .get_plan(conversion.project.id)
        .await
        .map_err(storage_error)?;
    let now = Utc::now();
    let narrator_id = state
        .catalog
        .read()
        .await
        .characters
        .get(&conversion.project.id.as_uuid())
        .and_then(|characters| {
            characters
                .iter()
                .find(|character| character.role == audiobookai_core::CharacterRole::Narrator)
        })
        .map(|character| character.id);
    let mut segments = Vec::new();
    let mut plan_hasher = blake3::Hasher::new();
    for chapter in &conversion.chapters {
        for segment in &chapter.segments {
            let expected_input_hash = segment_semantic_input_hash(segment)?;
            plan_hasher.update(segment.key.as_bytes());
            plan_hasher.update(&[0]);
            plan_hasher.update(expected_input_hash.as_bytes());
            plan_hasher.update(&[0]);
            let speaker = if narrator_id == Some(segment.assignment.character_id) {
                Speaker::Narrator
            } else {
                Speaker::Character(CharacterId::from_uuid(segment.assignment.character_id))
            };
            segments.push(ProductionSegment {
                id: segment.id,
                project_id: conversion.project.id,
                chapter_id: Some(ChapterId::from_uuid(segment.chapter_id)),
                paragraph_id: Some(audiobookai_core::ParagraphId::from_uuid(
                    segment.paragraph_id,
                )),
                source: ProductionSegmentSource::EpubRange,
                stable_key: segment.key.clone(),
                ordinal: segment.segment_ordinal,
                source_content_hash: segment.source_content_hash.clone(),
                byte_start: Some(segment.byte_start),
                byte_end: Some(segment.byte_end),
                speaker,
                original_text: segment.original_text.clone(),
                narration_text_override: None,
                effective_text: segment.text.clone(),
                context_before: segment.context.clone(),
                context_after: None,
                performance_override: PerformanceSettings::default(),
                timing_override: TimingSettings::default(),
                expected_input_hash,
                review_state: SegmentReviewState::Unreviewed,
                active: true,
                revision: 0,
                created_at: now,
                updated_at: now,
            });
        }
    }
    let plan = ProofingPlan {
        project_id: conversion.project.id,
        source_conversion_job_id: job.id,
        plan_revision: previous
            .as_ref()
            .map_or(1, |value| value.plan_revision.saturating_add(1)),
        plan_hash: plan_hasher.finalize().to_hex().to_string(),
        status: ProofingPlanStatus::Incomplete,
        dirty_reasons: Vec::new(),
        created_at: previous.map_or(now, |value| value.created_at),
        updated_at: now,
    };
    Ok((plan, segments))
}
