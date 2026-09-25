use super::{
    AppState, Arc, AttemptId, BTreeMap, BTreeSet, FromStr, Job, JobAttempt, JobId, JobKind,
    JobState, JobUnit, JobUnitKind, JobUnitState, OUTPUT_ADMISSION_LOCK, ProjectId, ProviderUsage,
    RECOVERED_PRODUCTION_CONFLICT, ReservationId, Row, SegmentPlan, ServiceError, UsageSource, Utc,
    append_tts_usage, ensure_existing_job_output_reservation, fail_interrupted_paid_job,
    internal_error, reconcile_job_budgets, schedule_conversion_job,
    schedule_segment_regeneration_job, storage_error, transition_job, update_unit_state,
};

/// Restarts conversion workers after an application restart without duplicating completed units.
pub async fn resume_durable_conversions(state: Arc<AppState>) -> Result<(), ServiceError> {
    release_orphaned_admission_reservations(&state).await?;
    recover_terminal_paid_reservations(&state).await?;
    state
        .database
        .repositories()
        .jobs
        .release_terminal_output_reservations()
        .await
        .map_err(storage_error)?;
    let active = state
        .database
        .repositories()
        .jobs
        .list_active()
        .await
        .map_err(storage_error)?;

    // Older databases could contain more than one active project-production job because the
    // invariant was previously enforced only at API admission time. Resolve every conflicting
    // record durably before any worker is spawned. Retaining the oldest record matches the job
    // that would already have blocked later admissions; the UUID tie-break makes legacy rows with
    // identical timestamps deterministic.
    let recovered_conflicts = recovered_production_conflicts(&active);
    for job in active
        .iter()
        .filter(|job| recovered_conflicts.contains(&job.id))
    {
        fail_recovered_production_conflict(&state, job).await?;
    }

    for job in active {
        if recovered_conflicts.contains(&job.id) {
            continue;
        }
        if job.kind == JobKind::Preview {
            recover_interrupted_preview(&state, &job).await?;
            continue;
        }
        if !matches!(
            job.kind,
            JobKind::Conversion | JobKind::SegmentRegeneration | JobKind::Export
        ) {
            continue;
        }
        if fail_recovered_interrupted_paid_job(&state, &job).await? {
            continue;
        }
        if matches!(job.kind, JobKind::Conversion | JobKind::Export) {
            let recovery = {
                let _output_admission = OUTPUT_ADMISSION_LOCK
                    .get_or_init(|| tokio::sync::Mutex::new(()))
                    .lock()
                    .await;
                ensure_existing_job_output_reservation(&state, &job).await
            };
            if let Err(error) = recovery {
                let message = format!(
                    "legacy export job could not acquire its output destination before restart: {error}"
                );
                fail_interrupted_paid_job(&state, job.id, &message).await?;
                continue;
            }
        }
        match job.state {
            JobState::Queued | JobState::Running => {
                if job.kind == JobKind::SegmentRegeneration {
                    schedule_segment_regeneration_job(Arc::clone(&state), job.id);
                } else {
                    schedule_conversion_job(Arc::clone(&state), job.id);
                }
            }
            JobState::Pausing => {
                transition_job(&state, job.id, JobState::Paused, "Paused").await?;
            }
            JobState::Cancelling => {
                transition_job(&state, job.id, JobState::Cancelled, "Cancelled").await?;
            }
            JobState::Paused | JobState::Cancelled | JobState::Failed | JobState::Completed => {}
        }
    }
    Ok(())
}

pub(super) fn recovered_production_conflicts(active: &[Job]) -> BTreeSet<JobId> {
    let mut projects = BTreeMap::<ProjectId, Vec<&Job>>::new();
    for job in active.iter().filter(|job| {
        matches!(
            job.kind,
            JobKind::CharacterDetection
                | JobKind::Conversion
                | JobKind::SegmentRegeneration
                | JobKind::Export
        )
    }) {
        projects.entry(job.project_id).or_default().push(job);
    }

    let mut conflicts = BTreeSet::new();
    for jobs in projects.values_mut() {
        jobs.sort_by(|left, right| {
            left.created_at
                .cmp(&right.created_at)
                .then_with(|| left.id.to_string().cmp(&right.id.to_string()))
        });
        conflicts.extend(jobs.iter().skip(1).map(|job| job.id));
    }
    conflicts
}

pub(super) async fn fail_recovered_production_conflict(
    state: &AppState,
    job: &Job,
) -> Result<(), ServiceError> {
    if job.kind == JobKind::CharacterDetection {
        return crate::workflows::fail_recovered_production_conflict(
            state,
            job,
            RECOVERED_PRODUCTION_CONFLICT,
        )
        .await;
    }

    let interrupted = interrupted_paid_dispatches(state, job.id).await?;
    let uncertainty_recorded = record_recovered_paid_dispatches(state, &interrupted).await?;
    let message = if interrupted.is_empty() {
        RECOVERED_PRODUCTION_CONFLICT.to_owned()
    } else {
        format!(
            "{RECOVERED_PRODUCTION_CONFLICT}; one or more provider requests may have been charged and were not retried automatically"
        )
    };
    fail_interrupted_paid_job(state, job.id, &message).await?;
    if uncertainty_recorded {
        reconcile_job_budgets(state, job.id).await?;
    }
    Ok(())
}

pub(super) async fn fail_recovered_interrupted_paid_job(
    state: &AppState,
    job: &Job,
) -> Result<bool, ServiceError> {
    let interrupted = interrupted_paid_dispatches(state, job.id).await?;
    if interrupted.is_empty() {
        return Ok(false);
    }
    let uncertainty_recorded = record_recovered_paid_dispatches(state, &interrupted).await?;
    let message = match job.kind {
        JobKind::SegmentRegeneration => {
            "the application stopped while a paid regeneration dispatch was in progress; the provider may have charged it, so it was not retried automatically"
        }
        JobKind::Conversion | JobKind::Export => {
            "the application stopped while one or more paid synthesis dispatches were in progress; the provider may have charged them, so they were not retried automatically"
        }
        JobKind::CharacterDetection
        | JobKind::Preview
        | JobKind::QualityControl
        | JobKind::CacheCleanup => {
            return Err(ServiceError::Internal(
                "paid synthesis recovery was requested for an unsupported job kind".to_owned(),
            ));
        }
    };
    fail_interrupted_paid_job(state, job.id, message).await?;
    if uncertainty_recorded {
        reconcile_job_budgets(state, job.id).await?;
    }
    Ok(true)
}

pub(super) async fn record_recovered_paid_dispatches(
    state: &AppState,
    units: &[JobUnit],
) -> Result<bool, ServiceError> {
    let mut uncertainty_recorded = true;
    for unit in units {
        if let Err(error) = record_interrupted_paid_unit_uncertainty(state, unit).await {
            // Legacy rows may not contain a complete segment snapshot. Keep their reservation
            // active for manual reconciliation instead of treating an unknown paid request as
            // zero usage.
            uncertainty_recorded = false;
            let mut unresolved = unit.clone();
            unresolved.payload.insert(
                "uncertainUsageUnresolved".to_owned(),
                serde_json::json!(true),
            );
            state
                .database
                .repositories()
                .jobs
                .upsert_unit(&unresolved)
                .await
                .map_err(storage_error)?;
            tracing::warn!(diagnostic_code = "production.recovery.usage_unresolved", job_id = %unit.job_id, unit_id = %unit.id, %error, "interrupted production usage could not be reconstructed; reservation retained");
        }
    }
    Ok(uncertainty_recorded)
}

pub(super) async fn recover_interrupted_preview(
    state: &AppState,
    job: &Job,
) -> Result<(), ServiceError> {
    let units = state
        .database
        .repositories()
        .jobs
        .list_units(job.id)
        .await
        .map_err(storage_error)?;
    let mut uncertainty_recorded = true;
    for unit in units.iter().filter(|unit| {
        unit.kind == JobUnitKind::SynthesisSegment
            && matches!(unit.state, JobUnitState::Running | JobUnitState::Retrying)
    }) {
        if let Err(error) = record_interrupted_paid_unit_uncertainty(state, unit).await {
            // Legacy preview units did not persist their segment snapshot. Fail the job without
            // redispatch, but retain its reservation instead of incorrectly treating usage as zero.
            uncertainty_recorded = false;
            let mut unresolved = unit.clone();
            unresolved.payload.insert(
                "uncertainUsageUnresolved".to_owned(),
                serde_json::json!(true),
            );
            state
                .database
                .repositories()
                .jobs
                .upsert_unit(&unresolved)
                .await
                .map_err(storage_error)?;
            tracing::warn!(diagnostic_code = "preview.recovery.usage_unresolved", job_id = %job.id, %error, "interrupted preview usage could not be reconstructed; reservation retained");
        }
    }
    let message =
        "the application stopped during a billable preview; it was not redispatched automatically";
    fail_interrupted_paid_job(state, job.id, message).await?;
    if uncertainty_recorded {
        crate::accounting::finalize_job_reservation(state, job.id).await?;
    }
    Ok(())
}

pub(super) async fn recover_terminal_paid_reservations(
    state: &AppState,
) -> Result<(), ServiceError> {
    let rows = sqlx::query(
        "SELECT j.id, j.state FROM jobs j \
         JOIN budget_reservations r ON r.id = j.reservation_id \
         WHERE j.kind IN \
         ('conversion', 'segment_regeneration', 'export', 'preview', 'character_detection') \
         AND j.state IN ('failed', 'cancelled', 'completed') \
         AND r.status IN ('active', 'expired')",
    )
    .fetch_all(state.database.pool())
    .await
    .map_err(storage_error)?;
    for row in rows {
        let job_id = JobId::from_str(row.get::<&str, _>("id")).map_err(internal_error)?;
        let recover_interrupted_dispatch = row.get::<&str, _>("state") == "failed";
        let mut uncertainty_recorded = true;
        for mut unit in state
            .database
            .repositories()
            .jobs
            .list_units(job_id)
            .await
            .map_err(storage_error)?
        {
            if unit
                .payload
                .get("uncertainUsageUnresolved")
                .and_then(serde_json::Value::as_bool)
                .unwrap_or(false)
            {
                uncertainty_recorded = false;
            }
            if recover_interrupted_dispatch
                && unit.kind == JobUnitKind::SynthesisSegment
                && matches!(unit.state, JobUnitState::Running | JobUnitState::Retrying)
            {
                if let Err(error) = record_interrupted_paid_unit_uncertainty(state, &unit).await {
                    uncertainty_recorded = false;
                    unit.payload.insert(
                        "uncertainUsageUnresolved".to_owned(),
                        serde_json::json!(true),
                    );
                    tracing::warn!(diagnostic_code = "paid_job.recovery.usage_unresolved", %job_id, %error, "interrupted paid usage could not be reconstructed; reservation retained");
                }
                update_unit_state(
                    state,
                    &mut unit,
                    JobUnitState::Failed,
                    Some("interrupted provider dispatch may have been charged"),
                )
                .await?;
            }
        }
        if uncertainty_recorded {
            crate::accounting::finalize_job_reservation(state, job_id).await?;
        }
    }
    Ok(())
}

pub(super) async fn release_orphaned_admission_reservations(
    state: &AppState,
) -> Result<(), ServiceError> {
    let rows = sqlx::query(
        "SELECT r.id, r.job_id, r.usage_sequence_start FROM budget_reservations r \
         JOIN jobs j ON j.id = r.job_id \
         WHERE r.status IN ('active', 'expired') \
         AND (j.reservation_id IS NULL OR j.reservation_id != r.id)",
    )
    .fetch_all(state.database.pool())
    .await
    .map_err(storage_error)?;
    let mut released_any = false;
    for row in rows {
        let reservation_id =
            ReservationId::from_str(row.get::<&str, _>("id")).map_err(internal_error)?;
        let job_id = JobId::from_str(row.get::<&str, _>("job_id")).map_err(internal_error)?;
        let usage_sequence_start = row.get::<i64, _>("usage_sequence_start");
        let usage_count = sqlx::query_scalar::<_, i64>(
            "SELECT COUNT(*) FROM usage_ledger WHERE job_id = ? AND sequence > ?",
        )
        .bind(job_id.to_string())
        .bind(usage_sequence_start)
        .fetch_one(state.database.pool())
        .await
        .map_err(storage_error)?;
        if usage_count > 0 {
            tracing::warn!(diagnostic_code = "budget.admission.orphaned_with_usage", %job_id, %reservation_id, "an unattached reservation has usage and was retained for manual reconciliation");
            continue;
        }
        state
            .database
            .repositories()
            .budgets
            .release(reservation_id, Utc::now())
            .await
            .map_err(storage_error)?;
        released_any = true;
    }
    if released_any {
        crate::accounting::refresh_budget_views(state).await?;
    }
    Ok(())
}

pub(super) async fn interrupted_paid_dispatches(
    state: &AppState,
    job_id: JobId,
) -> Result<Vec<JobUnit>, ServiceError> {
    Ok(state
        .database
        .repositories()
        .jobs
        .list_units(job_id)
        .await
        .map_err(storage_error)?
        .into_iter()
        .filter(|unit| {
            unit.kind == JobUnitKind::SynthesisSegment
                && matches!(unit.state, JobUnitState::Running | JobUnitState::Retrying)
        })
        .collect())
}

pub(super) async fn record_interrupted_paid_unit_uncertainty(
    state: &AppState,
    unit: &JobUnit,
) -> Result<(), ServiceError> {
    let segment = unit
        .payload
        .get("segmentPlan")
        .cloned()
        .ok_or_else(|| {
            ServiceError::Conflict(
                "interrupted paid synthesis has no durable input snapshot".to_owned(),
            )
        })
        .and_then(|value| serde_json::from_value::<SegmentPlan>(value).map_err(internal_error))?;
    let existing = sqlx::query_scalar::<_, String>(
        "SELECT payload FROM job_attempts WHERE job_unit_id = ? AND uncertain_charge = 1 \
         ORDER BY ordinal DESC LIMIT 1",
    )
    .bind(unit.id.to_string())
    .fetch_optional(state.database.pool())
    .await
    .map_err(storage_error)?;
    let attempt = if let Some(payload) = existing {
        serde_json::from_str::<JobAttempt>(&payload).map_err(internal_error)?
    } else {
        let maximum = sqlx::query_scalar::<_, Option<i64>>(
            "SELECT MAX(ordinal) FROM job_attempts WHERE job_unit_id = ?",
        )
        .bind(unit.id.to_string())
        .fetch_one(state.database.pool())
        .await
        .map_err(storage_error)?
        .unwrap_or(0);
        let ordinal = u16::try_from(maximum.saturating_add(1)).map_err(|_| {
            ServiceError::Conflict(
                "interrupted regeneration exhausted its durable attempt counter".to_owned(),
            )
        })?;
        let now = Utc::now();
        let attempt = JobAttempt {
            id: AttemptId::new(),
            job_unit_id: unit.id,
            ordinal,
            started_at: unit.updated_at,
            finished_at: Some(now),
            failure_class: Some(audiobookai_core::FailureClass::TimeoutAfterDispatch),
            error_code: Some("application_interrupted_after_dispatch".to_owned()),
            redacted_error: Some(
                "application stopped before the paid provider response was durably recorded"
                    .to_owned(),
            ),
            provider_request_id: None,
            uncertain_charge: true,
        };
        state
            .database
            .repositories()
            .jobs
            .insert_attempt(&attempt)
            .await
            .map_err(storage_error)?;
        attempt
    };
    append_tts_usage(
        state,
        unit.job_id,
        &segment,
        Some(attempt.id),
        &ProviderUsage {
            source: UsageSource::Estimated,
            characters: u64::try_from(segment.text.chars().count()).ok(),
            ..ProviderUsage::default()
        },
        true,
        None,
    )
    .await
}

/// Persists a pause boundary for every active durable job before desktop shutdown.
pub async fn checkpoint_jobs_for_shutdown(state: Arc<AppState>) -> Result<usize, ServiceError> {
    let active = state
        .database
        .repositories()
        .jobs
        .list_active()
        .await
        .map_err(storage_error)?;
    let mut checkpointed = 0_usize;
    for job in active {
        if !matches!(
            job.kind,
            JobKind::Conversion
                | JobKind::CharacterDetection
                | JobKind::SegmentRegeneration
                | JobKind::Export
        ) {
            continue;
        }
        match job.state {
            JobState::Queued => {
                transition_job(
                    &state,
                    job.id,
                    JobState::Running,
                    "Preparing shutdown checkpoint",
                )
                .await?;
                transition_job(
                    &state,
                    job.id,
                    JobState::Pausing,
                    "Checkpointed for application shutdown",
                )
                .await?;
                if job.kind == JobKind::CharacterDetection {
                    crate::workflows::spawn_character_detection(
                        Arc::clone(&state),
                        job.id.as_uuid(),
                    );
                }
                checkpointed = checkpointed.saturating_add(1);
            }
            JobState::Running => {
                transition_job(
                    &state,
                    job.id,
                    JobState::Pausing,
                    "Checkpointed for application shutdown",
                )
                .await?;
                if job.kind == JobKind::CharacterDetection {
                    crate::workflows::spawn_character_detection(
                        Arc::clone(&state),
                        job.id.as_uuid(),
                    );
                }
                checkpointed = checkpointed.saturating_add(1);
            }
            JobState::Pausing => {
                checkpointed = checkpointed.saturating_add(1);
            }
            JobState::Paused
            | JobState::Cancelling
            | JobState::Cancelled
            | JobState::Failed
            | JobState::Completed => {}
        }
    }
    Ok(checkpointed)
}
