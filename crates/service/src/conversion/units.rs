use super::{
    AppState, ArtifactId, BTreeMap, BTreeSet, ChapterId, ConversionPlan, HashMap, Job, JobId,
    JobKind, JobStageView, JobState, JobStatusView, JobUnit, JobUnitId, JobUnitKind, JobUnitState,
    JobUnitStatusView, JobUnitView, JobView, PersistedUnitPlan, ProviderProfileId, SegmentPlan,
    SegmentTakeId, ServiceError, Utc, Uuid, internal_error, is_speakable, media_error,
    segment_cache_fingerprint, segment_semantic_input_hash, storage_error,
};

// The dependency graph is built as one cohesive value so every edge remains
// visible and cannot drift across partially shared helper state.
#[allow(clippy::too_many_lines)]
pub(super) fn build_job_units(job_id: JobId, plan: &ConversionPlan) -> PersistedUnitPlan {
    let now = Utc::now();
    let mut synthesis = HashMap::new();
    let mut synthesis_by_chapter = HashMap::<Uuid, Vec<JobUnitId>>::new();
    for chapter in &plan.chapters {
        for segment in &chapter.segments {
            let take_id = SegmentTakeId::new();
            let take_artifact_id = ArtifactId::new();
            let unit = JobUnit {
                id: JobUnitId::new(),
                job_id,
                kind: JobUnitKind::SynthesisSegment,
                state: JobUnitState::Ready,
                chapter_id: Some(ChapterId::from_uuid(segment.chapter_id)),
                segment_id: segment.proofing.then_some(segment.id),
                provider_profile_id: Some(ProviderProfileId::from_uuid(
                    segment.assignment.provider_id,
                )),
                dependencies: Vec::new(),
                attempt_count: 0,
                next_attempt_at: None,
                output_artifact_id: None,
                payload: BTreeMap::from([
                    ("segmentKey".to_owned(), serde_json::json!(segment.key)),
                    ("takeId".to_owned(), serde_json::json!(take_id)),
                    (
                        "takeArtifactId".to_owned(),
                        serde_json::json!(take_artifact_id),
                    ),
                    ("cacheOperation".to_owned(), serde_json::json!("conversion")),
                    ("autoSelect".to_owned(), serde_json::json!(true)),
                    (
                        "segmentPlan".to_owned(),
                        serde_json::to_value(segment)
                            .expect("a validated conversion segment plan is serializable"),
                    ),
                    (
                        "title".to_owned(),
                        serde_json::json!(format!(
                            "{} — {}",
                            segment.chapter_title, segment.assignment.character_name
                        )),
                    ),
                    ("progress".to_owned(), serde_json::json!(0.0)),
                ]),
                created_at: now,
                updated_at: now,
            };
            synthesis_by_chapter
                .entry(segment.chapter_id)
                .or_default()
                .push(unit.id);
            synthesis.insert(segment.key.clone(), unit);
        }
    }
    let mut assembly = HashMap::new();
    for chapter in &plan.chapters {
        let unit = JobUnit {
            id: JobUnitId::new(),
            job_id,
            kind: JobUnitKind::ChapterAssembly,
            state: JobUnitState::Blocked,
            chapter_id: Some(chapter.chapter.id),
            segment_id: None,
            provider_profile_id: None,
            dependencies: synthesis_by_chapter
                .remove(&chapter.chapter.id.as_uuid())
                .unwrap_or_default(),
            attempt_count: 0,
            next_attempt_at: None,
            output_artifact_id: None,
            payload: BTreeMap::from([
                (
                    "title".to_owned(),
                    serde_json::json!(format!("Assemble {}", chapter.chapter.title)),
                ),
                ("progress".to_owned(), serde_json::json!(0.0)),
            ]),
            created_at: now,
            updated_at: now,
        };
        assembly.insert(chapter.chapter.id.as_uuid(), unit);
    }
    let assembly_ids = assembly.values().map(|unit| unit.id).collect::<Vec<_>>();
    let mix = plan.export.background_music.as_ref().map(|_| JobUnit {
        id: JobUnitId::new(),
        job_id,
        kind: JobUnitKind::MusicMix,
        state: JobUnitState::Blocked,
        chapter_id: None,
        segment_id: None,
        provider_profile_id: None,
        dependencies: assembly_ids.clone(),
        attempt_count: 0,
        next_attempt_at: None,
        output_artifact_id: None,
        payload: BTreeMap::from([
            (
                "title".to_owned(),
                serde_json::json!("Mix background music"),
            ),
            ("progress".to_owned(), serde_json::json!(0.0)),
        ]),
        created_at: now,
        updated_at: now,
    });
    let normalize_dependencies = mix
        .as_ref()
        .map_or_else(|| assembly_ids.clone(), |unit| vec![unit.id]);
    let normalize = JobUnit {
        id: JobUnitId::new(),
        job_id,
        kind: JobUnitKind::Normalization,
        state: JobUnitState::Blocked,
        chapter_id: None,
        segment_id: None,
        provider_profile_id: None,
        dependencies: normalize_dependencies,
        attempt_count: 0,
        next_attempt_at: None,
        output_artifact_id: None,
        payload: BTreeMap::from([
            (
                "title".to_owned(),
                serde_json::json!("Measure final loudness"),
            ),
            ("progress".to_owned(), serde_json::json!(0.0)),
        ]),
        created_at: now,
        updated_at: now,
    };
    let export = JobUnit {
        id: JobUnitId::new(),
        job_id,
        kind: JobUnitKind::FinalExport,
        state: JobUnitState::Blocked,
        chapter_id: None,
        segment_id: None,
        provider_profile_id: None,
        dependencies: vec![normalize.id],
        attempt_count: 0,
        next_attempt_at: None,
        output_artifact_id: None,
        payload: BTreeMap::from([
            ("title".to_owned(), serde_json::json!("Export audiobook")),
            ("progress".to_owned(), serde_json::json!(0.0)),
        ]),
        created_at: now,
        updated_at: now,
    };
    PersistedUnitPlan {
        synthesis,
        assembly,
        mix,
        normalize,
        export,
    }
}

pub(super) fn unit_count(units: &PersistedUnitPlan) -> usize {
    units.synthesis.len() + units.assembly.len() + 2 + usize::from(units.mix.is_some())
}

pub(super) fn ordered_units(units: &PersistedUnitPlan) -> Vec<JobUnit> {
    let mut ordered = Vec::with_capacity(unit_count(units));
    let mut synthesis = units.synthesis.values().cloned().collect::<Vec<_>>();
    synthesis.sort_by_key(|unit| unit.id);
    ordered.extend(synthesis);
    let mut assembly = units.assembly.values().cloned().collect::<Vec<_>>();
    assembly.sort_by_key(|unit| unit.id);
    ordered.extend(assembly);
    if let Some(unit) = &units.mix {
        ordered.push(unit.clone());
    }
    ordered.push(units.normalize.clone());
    ordered.push(units.export.clone());
    ordered
}

pub(super) async fn load_unit_plan(
    state: &AppState,
    job_id: JobId,
    plan: &ConversionPlan,
) -> Result<PersistedUnitPlan, ServiceError> {
    let existing = state
        .database
        .repositories()
        .jobs
        .list_units(job_id)
        .await
        .map_err(storage_error)?;
    if existing.is_empty() {
        return Err(ServiceError::Conflict(
            "the durable job graph is missing; the job was not admitted and cannot be resumed"
                .to_owned(),
        ));
    }
    let mut synthesis = HashMap::new();
    let mut assembly = HashMap::new();
    let mut mix = None;
    let mut normalize = None;
    let mut export = None;
    for unit in existing {
        match unit.kind {
            JobUnitKind::SynthesisSegment => {
                if let Some(key) = unit
                    .payload
                    .get("segmentKey")
                    .and_then(serde_json::Value::as_str)
                {
                    synthesis.insert(key.to_owned(), unit);
                }
            }
            JobUnitKind::ChapterAssembly => {
                if let Some(chapter_id) = unit.chapter_id {
                    assembly.insert(chapter_id.as_uuid(), unit);
                }
            }
            JobUnitKind::MusicMix => mix = Some(unit),
            JobUnitKind::Normalization => normalize = Some(unit),
            JobUnitKind::FinalExport => export = Some(unit),
            JobUnitKind::DetectionBatch | JobUnitKind::QualityControl => {}
        }
    }
    let expected_keys = plan
        .chapters
        .iter()
        .flat_map(|chapter| chapter.segments.iter().map(|segment| segment.key.clone()))
        .collect::<BTreeSet<_>>();
    // Jobs admitted before unspeakable punctuation-only segments were excluded may still own a
    // unit for one. Such a unit has no audible content, so it is ignored rather than failing the
    // resumed job graph.
    synthesis.retain(|key, unit| {
        expected_keys.contains(key)
            || unit
                .payload
                .get("segmentPlan")
                .and_then(|plan| {
                    plan.get("original_text")
                        .or_else(|| plan.get("originalText"))
                })
                .and_then(serde_json::Value::as_str)
                .is_none_or(is_speakable)
    });
    if synthesis.keys().cloned().collect::<BTreeSet<_>>() != expected_keys
        || assembly.len() != plan.chapters.len()
        || normalize.is_none()
        || export.is_none()
        || mix.is_some() != plan.export.background_music.is_some()
    {
        return Err(ServiceError::Conflict(
            "the durable job graph no longer matches the reviewed project; start a new conversion"
                .to_owned(),
        ));
    }
    for segment in plan
        .chapters
        .iter()
        .flat_map(|chapter| chapter.segments.iter())
    {
        let unit = synthesis
            .get(&segment.key)
            .expect("the durable synthesis key set was checked above");
        validate_durable_segment_snapshot(unit, segment)?;
    }
    Ok(PersistedUnitPlan {
        synthesis,
        assembly,
        mix,
        normalize: normalize.expect("checked above"),
        export: export.expect("checked above"),
    })
}

pub(super) fn validate_durable_segment_snapshot(
    unit: &JobUnit,
    current: &SegmentPlan,
) -> Result<(), ServiceError> {
    let snapshot = unit
        .payload
        .get("segmentPlan")
        .cloned()
        .ok_or_else(|| {
            ServiceError::Conflict(
                "the conversion predates durable narration snapshots; start a new conversion"
                    .to_owned(),
            )
        })
        .and_then(|value| serde_json::from_value::<SegmentPlan>(value).map_err(internal_error))?;
    let operation = unit
        .payload
        .get("cacheOperation")
        .and_then(serde_json::Value::as_str)
        .unwrap_or("conversion");
    let semantic_matches =
        segment_semantic_input_hash(&snapshot)? == segment_semantic_input_hash(current)?;
    let provider_identity_matches = snapshot.assignment.provider_kind
        == current.assignment.provider_kind
        && snapshot.assignment.provider_role == current.assignment.provider_role
        && snapshot.assignment.provider_mode == current.assignment.provider_mode
        && snapshot.assignment.provider_endpoint == current.assignment.provider_endpoint
        && snapshot.assignment.provider_snapshot_id == current.assignment.provider_snapshot_id;
    let snapshot_cache_key = segment_cache_fingerprint(&snapshot, operation)
        .key()
        .map_err(media_error)?;
    let current_cache_key = segment_cache_fingerprint(current, operation)
        .key()
        .map_err(media_error)?;
    if !semantic_matches || !provider_identity_matches || snapshot_cache_key != current_cache_key {
        return Err(ServiceError::Conflict(
            "the narration inputs changed after this job was admitted; start a new conversion"
                .to_owned(),
        ));
    }
    Ok(())
}

pub(super) fn job_view(job: &Job, title: &str, units: &PersistedUnitPlan) -> JobView {
    let mut views = units
        .synthesis
        .values()
        .chain(units.assembly.values())
        .chain(units.mix.iter())
        .chain(std::iter::once(&units.normalize))
        .chain(std::iter::once(&units.export))
        .map(unit_view)
        .collect::<Vec<_>>();
    views.sort_by_key(|unit| match unit.stage {
        JobStageView::Detect => 0,
        JobStageView::Synthesize => 1,
        JobStageView::Assemble => 2,
        JobStageView::Mix => 3,
        JobStageView::Normalize => 4,
        JobStageView::Export => 5,
        JobStageView::QualityControl => 6,
    });
    JobView {
        id: job.id.as_uuid(),
        project_id: job.project_id.as_uuid(),
        project_title: title.to_owned(),
        kind: match job.kind {
            JobKind::CharacterDetection => crate::models::JobKindView::CharacterDetection,
            JobKind::Preview => crate::models::JobKindView::Preview,
            JobKind::Conversion => crate::models::JobKindView::Conversion,
            JobKind::SegmentRegeneration => crate::models::JobKindView::SegmentRegeneration,
            JobKind::Export => crate::models::JobKindView::Export,
            JobKind::QualityControl => crate::models::JobKindView::QualityControl,
            JobKind::CacheCleanup => crate::models::JobKindView::CacheCleanup,
        },
        status: job_status_view(job.state),
        progress: progress_ratio(job.progress_completed, job.progress_total),
        current_stage: job.status_message.clone(),
        started_at: job.started_at,
        updated_at: job.updated_at,
        estimated_remaining_seconds: None,
        units: views,
        progressive_playback_url: Some(format!("/api/v1/jobs/{}/playback", job.id)),
        uncertain_charge: false,
    }
}

pub(super) fn unit_view(unit: &JobUnit) -> JobUnitView {
    JobUnitView {
        id: unit.id.as_uuid(),
        title: unit
            .payload
            .get("title")
            .and_then(serde_json::Value::as_str)
            .unwrap_or("Conversion step")
            .to_owned(),
        stage: match unit.kind {
            JobUnitKind::DetectionBatch => JobStageView::Detect,
            JobUnitKind::SynthesisSegment => JobStageView::Synthesize,
            JobUnitKind::ChapterAssembly => JobStageView::Assemble,
            JobUnitKind::MusicMix => JobStageView::Mix,
            JobUnitKind::Normalization => JobStageView::Normalize,
            JobUnitKind::FinalExport => JobStageView::Export,
            JobUnitKind::QualityControl => JobStageView::QualityControl,
        },
        status: unit_status_view(unit.state),
        progress: if unit.state == JobUnitState::Completed {
            100.0
        } else {
            unit.payload
                .get("progress")
                .and_then(serde_json::Value::as_f64)
                .map_or(0.0, unit_interval_f32)
                * 100.0
        },
        attempt: u32::from(unit.attempt_count),
        last_error: unit
            .payload
            .get("lastError")
            .and_then(serde_json::Value::as_str)
            .map(str::to_owned),
    }
}

pub(super) const fn job_status_view(state: JobState) -> JobStatusView {
    match state {
        JobState::Queued => JobStatusView::Queued,
        JobState::Running => JobStatusView::Running,
        JobState::Pausing => JobStatusView::Pausing,
        JobState::Cancelling => JobStatusView::Cancelling,
        JobState::Paused => JobStatusView::Paused,
        JobState::Cancelled => JobStatusView::Cancelled,
        JobState::Failed => JobStatusView::Failed,
        JobState::Completed => JobStatusView::Complete,
    }
}

pub(super) const fn unit_status_view(state: JobUnitState) -> JobUnitStatusView {
    match state {
        JobUnitState::Blocked | JobUnitState::Ready | JobUnitState::Retrying => {
            JobUnitStatusView::Queued
        }
        JobUnitState::Running => JobUnitStatusView::Running,
        JobUnitState::Paused => JobUnitStatusView::Paused,
        JobUnitState::Cancelled => JobUnitStatusView::Cancelled,
        JobUnitState::Failed => JobUnitStatusView::Failed,
        JobUnitState::Completed => JobUnitStatusView::Complete,
    }
}

pub(super) fn progress_ratio(completed: u64, total: u64) -> f32 {
    if total == 0 {
        0.0
    } else {
        let scaled = u128::from(completed.min(total)).saturating_mul(10_000) / u128::from(total);
        let basis_points = u16::try_from(scaled).unwrap_or(10_000);
        f32::from(basis_points) / 100.0
    }
}

// Persisted progress is constrained to [0, 1], where every finite f64 value
// has a safe, bounded f32 representation for presentation purposes.
#[allow(clippy::cast_possible_truncation)]
pub(super) fn unit_interval_f32(value: f64) -> f32 {
    if value.is_finite() {
        value.clamp(0.0, 1.0) as f32
    } else {
        0.0
    }
}
