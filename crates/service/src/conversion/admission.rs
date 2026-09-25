use super::{
    AppState, Arc, ArtifactId, BTreeMap, ChapterId, ChapterPlan, ConversionPlan,
    ExportOptionsInput, HashMap, Job, JobId, JobKind, JobState, JobUnit, JobUnitId, JobUnitKind,
    JobUnitState, JobView, OUTPUT_ADMISSION_LOCK, ProjectDisplayStatus, ProjectId,
    ProofExportSelection, ProofExportSnapshot, ProofExportSnapshotId, ProofingPlanStatus,
    ProviderProfileId, RateCardId, SegmentId, SegmentPlan, SegmentReviewState, SegmentTakeId,
    ServiceError, StartJobInput, UsageQuantities, UsageWorkload, Utc, Uuid, build_job_units,
    build_proofing_plan, create_export_profile, internal_error, job_status_view, job_view,
    load_artifact, load_conversion_plan, load_dispatchable_proofing_segment_plan,
    load_proofing_segment_plan, mark_domain_job_failed, ordered_units,
    output_reservation_admission_error, prepare_output_reservation, progress_ratio,
    reconcile_job_budgets, release_unattached_reservation, reserve_job_budgets, retry_policy,
    retry_reservation_multiplier, schedule_conversion_job, schedule_segment_regeneration_job,
    segment_semantic_input_hash, storage_error, unit_count, unit_view, update_staged_job_failure,
    validate_export_input, verify_selected_artifact_integrity,
};

/// Creates a durable conversion and starts its in-process worker.
#[allow(clippy::too_many_lines)]
pub async fn start_conversion(
    state: Arc<AppState>,
    input: StartJobInput,
) -> Result<JobView, ServiceError> {
    validate_export_input(&input.export)?;
    let _shutdown_admission = state.admit_shutdown_sensitive_work().await?;
    let output_admission = OUTPUT_ADMISSION_LOCK
        .get_or_init(|| tokio::sync::Mutex::new(()))
        .lock()
        .await;
    let job_id = JobId::new();
    let (export_profile, music_path) =
        create_export_profile(&state, job_id, input.project_id, &input.export).await?;
    let plan = load_conversion_plan(&state, input.project_id, export_profile, music_path).await?;
    let units = build_job_units(job_id, &plan);
    let now = Utc::now();
    let output_reservation =
        prepare_output_reservation(job_id, plan.project.id, &plan.export, now).await?;
    let total = u64::try_from(unit_count(&units)).unwrap_or(u64::MAX);
    let mut job = Job {
        id: job_id,
        project_id: ProjectId::from_uuid(input.project_id),
        kind: JobKind::Conversion,
        state: JobState::Queued,
        export_profile_id: Some(plan.export.id),
        reservation_id: None,
        progress_completed: 0,
        progress_total: total,
        status_message: Some("Queued for conversion".to_owned()),
        allow_budget_override: input.allow_budget_override,
        created_at: now,
        started_at: None,
        finished_at: None,
        updated_at: now,
        revision: 0,
    };
    state
        .database
        .repositories()
        .jobs
        .insert_with_output_reservation(&job, &output_reservation)
        .await
        .map_err(output_reservation_admission_error)?;
    drop(output_admission);
    let reservation_id = match reserve_job_budgets(&state, &job, &plan).await {
        Ok(reservation_id) => reservation_id,
        Err(error) => {
            mark_domain_job_failed(&state, job_id, &error.to_string()).await;
            return Err(error);
        }
    };
    if let Some(reservation_id) = reservation_id {
        let expected = job.revision;
        job.reservation_id = Some(reservation_id);
        job.updated_at = Utc::now();
        job = match state
            .database
            .repositories()
            .jobs
            .update(&job, expected)
            .await
        {
            Ok(job) => job,
            Err(error) => {
                release_unattached_reservation(&state, reservation_id).await;
                mark_domain_job_failed(&state, job_id, &error.to_string()).await;
                return Err(storage_error(error));
            }
        }
    }

    let (proofing_plan, proofing_segments) = match build_proofing_plan(&state, &job, &plan).await {
        Ok(value) => value,
        Err(error) => {
            mark_domain_job_failed(&state, job_id, &error.to_string()).await;
            let _ = reconcile_job_budgets(&state, job_id).await;
            return Err(error);
        }
    };
    let durable_units = ordered_units(&units);
    if let Err(error) = state
        .database
        .repositories()
        .proofing
        .replace_plan_with_units(&proofing_plan, &proofing_segments, &durable_units)
        .await
    {
        let error = storage_error(error);
        mark_domain_job_failed(&state, job_id, &error.to_string()).await;
        let _ = reconcile_job_budgets(&state, job_id).await;
        return Err(error);
    }

    let view = job_view(&job, &plan.project.metadata.title, &units);
    {
        let mut catalog = state.catalog.write().await;
        catalog.jobs.insert(job_id.as_uuid(), view.clone());
        if let Some(project) = catalog.projects.get_mut(&input.project_id) {
            project.summary.status = ProjectDisplayStatus::Processing;
            project.summary.progress = 0.0;
        }
    }
    state.events.publish(
        "job.queued",
        serde_json::json!({"jobId": job_id, "projectId": input.project_id}),
    );
    schedule_conversion_job(Arc::clone(&state), job_id);
    Ok(view)
}

/// Starts a provider-free export from the exact takes selected in the proofing workbench.
/// The serialized plan and selection snapshot make the job crash-resumable without consulting
/// mutable narration or assignment state again.
#[allow(clippy::too_many_lines)]
pub(crate) async fn start_proof_export(
    state: Arc<AppState>,
    project_id: Uuid,
    input: ExportOptionsInput,
    strict_retailer: bool,
) -> Result<JobView, ServiceError> {
    validate_export_input(&input)?;
    let _shutdown_admission = state.admit_shutdown_sensitive_work().await?;
    let output_admission = OUTPUT_ADMISSION_LOCK
        .get_or_init(|| tokio::sync::Mutex::new(()))
        .lock()
        .await;
    let repositories = state.database.repositories();
    let proof = repositories
        .proofing
        .get_plan(ProjectId::from_uuid(project_id))
        .await
        .map_err(storage_error)?
        .ok_or_else(|| ServiceError::Conflict("proofing data is unavailable".to_owned()))?;
    if proof.status != ProofingPlanStatus::Ready {
        return Err(ServiceError::Conflict(
            "finish the active proofing plan before exporting".to_owned(),
        ));
    }
    let segments = repositories
        .proofing
        .list_active_segments(ProjectId::from_uuid(project_id), None)
        .await
        .map_err(storage_error)?;
    if segments.is_empty() {
        return Err(ServiceError::Conflict(
            "the proofing plan contains no production segments".to_owned(),
        ));
    }
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
    let source_chapters = repositories
        .projects
        .list_chapters(project.book_id)
        .await
        .map_err(storage_error)?;

    let mut snapshot_selections = Vec::with_capacity(segments.len());
    let mut selected_artifacts = HashMap::new();
    let mut planned_segments = HashMap::<Uuid, Vec<SegmentPlan>>::new();
    for segment in &segments {
        if strict_retailer && !segment.review_state.is_accepted() {
            return Err(ServiceError::Conflict(format!(
                "segment {} must be approved or locked for a retailer export",
                segment.id
            )));
        }
        if segment.review_state == SegmentReviewState::Flagged {
            return Err(ServiceError::Conflict(format!(
                "resolve the flag on segment {} before exporting",
                segment.id
            )));
        }
        let selection = repositories
            .proofing
            .get_selection(segment.id)
            .await
            .map_err(storage_error)?
            .ok_or_else(|| {
                ServiceError::Conflict(format!("select a take for segment {}", segment.id))
            })?;
        let take = repositories
            .proofing
            .get_take(selection.take_id)
            .await
            .map_err(storage_error)?
            .ok_or_else(|| ServiceError::Conflict("a selected take is unavailable".to_owned()))?;
        if take.semantic_input_hash != segment.expected_input_hash {
            return Err(ServiceError::Conflict(format!(
                "the selected take for segment {} is stale",
                segment.id
            )));
        }
        let artifact = load_artifact(&state, take.artifact_id).await?;
        verify_selected_artifact_integrity(&artifact).await?;
        let segment_plan = load_proofing_segment_plan(&state, project_id, segment).await?;
        if segment_semantic_input_hash(&segment_plan)? != segment.expected_input_hash {
            return Err(ServiceError::Conflict(format!(
                "the narration inputs for segment {} changed; review or regenerate it first",
                segment.id
            )));
        }
        let chapter_id = segment_plan.chapter_id;
        selected_artifacts.insert(segment_plan.key.clone(), take.artifact_id);
        planned_segments
            .entry(chapter_id)
            .or_default()
            .push(segment_plan);
        snapshot_selections.push(ProofExportSelection {
            segment_id: segment.id,
            take_id: take.id,
            artifact_id: take.artifact_id,
        });
    }
    let mut chapters = Vec::new();
    for chapter in source_chapters {
        let Some(mut chapter_segments) = planned_segments.remove(&chapter.id.as_uuid()) else {
            continue;
        };
        chapter_segments.sort_by_key(|segment| segment.segment_ordinal);
        chapters.push(ChapterPlan {
            chapter,
            segments: chapter_segments,
        });
    }
    if !planned_segments.is_empty() {
        return Err(ServiceError::Conflict(
            "a proofing segment no longer belongs to an available chapter".to_owned(),
        ));
    }

    let job_id = JobId::new();
    let (export, music_path) = create_export_profile(&state, job_id, project_id, &input).await?;
    let rules = state.catalog.read().await.pronunciation_rules.clone();
    let plan = ConversionPlan {
        project,
        book,
        chapters,
        rules,
        export,
        music_path,
    };
    let mut units = build_job_units(job_id, &plan);
    let now = Utc::now();
    let output_reservation =
        prepare_output_reservation(job_id, plan.project.id, &plan.export, now).await?;
    for (key, unit) in &mut units.synthesis {
        unit.state = JobUnitState::Completed;
        unit.output_artifact_id = selected_artifacts.get(key).copied();
        unit.updated_at = now;
    }
    let snapshot = ProofExportSnapshot {
        id: ProofExportSnapshotId::new(),
        project_id: ProjectId::from_uuid(project_id),
        job_id,
        export_profile_id: plan.export.id,
        plan_revision: proof.plan_revision,
        plan_hash: proof.plan_hash.clone(),
        selections: snapshot_selections,
        created_at: now,
    };
    units.export.payload.insert(
        "proofExportPlan".to_owned(),
        serde_json::to_value(&plan).map_err(internal_error)?,
    );
    units.export.payload.insert(
        "proofExportSnapshotId".to_owned(),
        serde_json::json!(snapshot.id),
    );
    let completed = u64::try_from(units.synthesis.len()).unwrap_or(u64::MAX);
    let job = Job {
        id: job_id,
        project_id: ProjectId::from_uuid(project_id),
        kind: JobKind::Export,
        state: JobState::Queued,
        export_profile_id: Some(plan.export.id),
        reservation_id: None,
        progress_completed: completed,
        progress_total: u64::try_from(unit_count(&units)).unwrap_or(u64::MAX),
        status_message: Some("Queued proofing export".to_owned()),
        allow_budget_override: false,
        created_at: now,
        started_at: None,
        finished_at: None,
        updated_at: now,
        revision: 0,
    };
    repositories
        .proofing
        .insert_export_job_graph_with_output_reservation(
            &job,
            &ordered_units(&units),
            &snapshot,
            &output_reservation,
        )
        .await
        .map_err(output_reservation_admission_error)?;
    drop(output_admission);

    let view = job_view(&job, &plan.project.metadata.title, &units);
    state
        .catalog
        .write()
        .await
        .jobs
        .insert(job_id.as_uuid(), view.clone());
    state.events.publish(
        "job.created",
        serde_json::json!({"jobId": job_id, "projectId": project_id, "kind": "proof_export"}),
    );
    schedule_conversion_job(Arc::clone(&state), job_id);
    Ok(view)
}

#[derive(Clone, Debug)]
pub(crate) struct RegenerationQuote {
    pub segment_id: SegmentId,
    pub segment_revision: u64,
    pub semantic_input_hash: String,
    pub provider_profile_id: ProviderProfileId,
    pub provider_name: String,
    pub model: Option<String>,
    pub characters: u64,
    pub monetary_cost_micros: Option<i64>,
    pub currency: Option<String>,
    pub credits: Option<i64>,
    pub rate_card_id: Option<RateCardId>,
}

pub(crate) async fn quote_segment_regeneration(
    state: &AppState,
    project_id: Uuid,
    segment_id: SegmentId,
) -> Result<RegenerationQuote, ServiceError> {
    let segment = state
        .database
        .repositories()
        .proofing
        .get_segment(segment_id)
        .await
        .map_err(storage_error)?
        .ok_or(ServiceError::NotFound)?;
    if segment.project_id.as_uuid() != project_id || !segment.active {
        return Err(ServiceError::NotFound);
    }
    let plan = load_proofing_segment_plan(state, project_id, &segment).await?;
    let semantic_input_hash = segment_semantic_input_hash(&plan)?;
    if semantic_input_hash != segment.expected_input_hash {
        return Err(ServiceError::ConflictDetails {
            code: "proofing_plan_dirty",
            detail: "update the proofing plan before regenerating this segment".to_owned(),
            meta: serde_json::json!({"segmentId": segment_id}),
        });
    }
    let characters = u64::try_from(plan.text.chars().count()).unwrap_or(u64::MAX);
    let estimate = crate::accounting::rate_usage_estimate(
        state,
        ProviderProfileId::from_uuid(plan.assignment.provider_id),
        UsageWorkload::Tts,
        plan.assignment.model.clone(),
        UsageQuantities {
            characters: Some(characters),
            ..UsageQuantities::default()
        },
    )
    .await?;
    Ok(RegenerationQuote {
        segment_id,
        segment_revision: segment.revision,
        semantic_input_hash,
        provider_profile_id: estimate.provider_profile_id,
        provider_name: plan.assignment.provider_name,
        model: plan.assignment.model,
        characters,
        monetary_cost_micros: estimate.cost.as_ref().map(|cost| cost.micros),
        currency: estimate.cost.as_ref().map(|cost| cost.currency.clone()),
        credits: estimate.quantities.provider_credits,
        rate_card_id: estimate.rate_card_id,
    })
}

#[allow(clippy::too_many_lines)]
pub(crate) async fn start_segment_regeneration(
    state: Arc<AppState>,
    project_id: Uuid,
    segment_id: SegmentId,
    expected_segment_revision: u64,
    allow_budget_override: bool,
) -> Result<JobView, ServiceError> {
    let _shutdown_admission = state.admit_shutdown_sensitive_work().await?;
    let segment = state
        .database
        .repositories()
        .proofing
        .get_segment(segment_id)
        .await
        .map_err(storage_error)?
        .ok_or(ServiceError::NotFound)?;
    if segment.project_id.as_uuid() != project_id || !segment.active {
        return Err(ServiceError::NotFound);
    }
    if segment.revision != expected_segment_revision {
        return Err(ServiceError::ConflictDetails {
            code: "stale_segment_revision",
            detail: "the segment changed after it was estimated".to_owned(),
            meta: serde_json::json!({
                "segmentId": segment_id,
                "currentRevision": segment.revision,
            }),
        });
    }
    if segment.review_state == SegmentReviewState::Locked {
        return Err(ServiceError::ConflictDetails {
            code: "segment_locked",
            detail: "unlock this segment before regenerating it".to_owned(),
            meta: serde_json::json!({"segmentId": segment_id}),
        });
    }
    let segment_plan =
        load_dispatchable_proofing_segment_plan(&state, project_id, &segment).await?;
    if segment_semantic_input_hash(&segment_plan)? != segment.expected_input_hash {
        return Err(ServiceError::ConflictDetails {
            code: "estimate_changed",
            detail: "the segment synthesis inputs changed after estimation".to_owned(),
            meta: serde_json::json!({"segmentId": segment_id}),
        });
    }
    let active = state
        .database
        .repositories()
        .jobs
        .list_active()
        .await
        .map_err(storage_error)?;
    if let Some(existing) = active.into_iter().find(|job| {
        job.kind == JobKind::SegmentRegeneration && job.project_id.as_uuid() == project_id
    }) {
        return Err(ServiceError::ConflictDetails {
            code: "active_segment_regeneration",
            detail: "another segment regeneration is already active for this project".to_owned(),
            meta: serde_json::json!({"activeJobId": existing.id}),
        });
    }
    let estimate = crate::accounting::rate_usage_estimate(
        &state,
        ProviderProfileId::from_uuid(segment_plan.assignment.provider_id),
        UsageWorkload::Tts,
        segment_plan.assignment.model.clone(),
        UsageQuantities {
            characters: u64::try_from(segment_plan.text.chars().count()).ok(),
            ..UsageQuantities::default()
        },
    )
    .await?;
    let now = Utc::now();
    let take_id = SegmentTakeId::new();
    let mut job = Job {
        id: JobId::new(),
        project_id: ProjectId::from_uuid(project_id),
        kind: JobKind::SegmentRegeneration,
        // A terminal staging state makes a crash before budget admission fail closed. The job is
        // moved to Queued only after its graph and reservation association are both durable.
        state: JobState::Failed,
        export_profile_id: None,
        reservation_id: None,
        progress_completed: 0,
        progress_total: 1,
        status_message: Some("Segment regeneration was interrupted before admission".to_owned()),
        allow_budget_override,
        created_at: now,
        started_at: None,
        finished_at: Some(now),
        updated_at: now,
        revision: 0,
    };
    let unit = JobUnit {
        id: JobUnitId::new(),
        job_id: job.id,
        kind: JobUnitKind::SynthesisSegment,
        state: JobUnitState::Ready,
        chapter_id: Some(ChapterId::from_uuid(segment_plan.chapter_id)),
        segment_id: Some(segment_id),
        provider_profile_id: Some(ProviderProfileId::from_uuid(
            segment_plan.assignment.provider_id,
        )),
        dependencies: Vec::new(),
        attempt_count: 0,
        next_attempt_at: None,
        output_artifact_id: None,
        payload: BTreeMap::from([
            (
                "title".to_owned(),
                serde_json::json!(format!("Regenerate {}", segment_plan.chapter_title)),
            ),
            ("progress".to_owned(), serde_json::json!(0.0)),
            ("segmentKey".to_owned(), serde_json::json!(segment_plan.key)),
            ("takeId".to_owned(), serde_json::json!(take_id)),
            (
                "takeArtifactId".to_owned(),
                serde_json::json!(ArtifactId::new()),
            ),
            (
                "cacheOperation".to_owned(),
                serde_json::json!(format!("regeneration:{take_id}")),
            ),
            ("autoSelect".to_owned(), serde_json::json!(false)),
            (
                "segmentPlan".to_owned(),
                serde_json::to_value(&segment_plan).map_err(internal_error)?,
            ),
        ]),
        created_at: now,
        updated_at: now,
    };
    let repositories = state.database.repositories();
    repositories
        .proofing
        .insert_job_graph(&job, std::slice::from_ref(&unit), None)
        .await
        .map_err(storage_error)?;
    let policy = retry_policy(&state, &segment_plan).await?;
    let reservation_multiplier = retry_reservation_multiplier(&policy);
    let reservation_estimates = vec![estimate; reservation_multiplier];
    let reservation_id = match crate::accounting::reserve_for_estimates(
        &state,
        &job,
        &reservation_estimates,
    )
    .await
    {
        Ok(reservation_id) => reservation_id,
        Err(error) => {
            update_staged_job_failure(&state, job.id, &error.to_string()).await;
            return Err(error);
        }
    };
    let expected = job.revision;
    job.reservation_id = reservation_id;
    job.transition(JobState::Queued, Utc::now())
        .map_err(|error| ServiceError::Internal(error.to_string()))?;
    job.finished_at = None;
    job.status_message = Some("Queued segment regeneration".to_owned());
    job = match repositories.jobs.update(&job, expected).await {
        Ok(job) => job,
        Err(error) => {
            if let Some(reservation_id) = reservation_id {
                release_unattached_reservation(&state, reservation_id).await;
            }
            update_staged_job_failure(&state, job.id, &error.to_string()).await;
            return Err(storage_error(error));
        }
    };
    let project_title = state
        .catalog
        .read()
        .await
        .projects
        .get(&project_id)
        .map_or_else(
            || "Audiobook".to_owned(),
            |project| project.summary.title.clone(),
        );
    let view = single_unit_job_view(&job, &project_title, &unit);
    state
        .catalog
        .write()
        .await
        .jobs
        .insert(job.id.as_uuid(), view.clone());
    state.events.publish(
        "job.queued",
        serde_json::json!({"jobId": job.id, "projectId": project_id, "segmentId": segment_id}),
    );
    schedule_segment_regeneration_job(Arc::clone(&state), job.id);
    Ok(view)
}

pub(super) fn single_unit_job_view(job: &Job, title: &str, unit: &JobUnit) -> JobView {
    JobView {
        id: job.id.as_uuid(),
        project_id: job.project_id.as_uuid(),
        project_title: title.to_owned(),
        kind: crate::models::JobKindView::SegmentRegeneration,
        status: job_status_view(job.state),
        progress: progress_ratio(job.progress_completed, job.progress_total),
        current_stage: job.status_message.clone(),
        started_at: job.started_at,
        updated_at: job.updated_at,
        estimated_remaining_seconds: None,
        units: vec![unit_view(unit)],
        progressive_playback_url: None,
        uncertain_charge: false,
    }
}
