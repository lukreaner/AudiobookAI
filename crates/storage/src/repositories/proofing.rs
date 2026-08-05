use std::collections::BTreeSet;

use audiobookai_core::{
    Job, JobId, JobKind, JobUnit, ProductionSegment, ProjectId, ProofExportSnapshot, ProofingPlan,
    SegmentId, SegmentSelection, SegmentTake, SegmentTakeId, Validate,
};
use sqlx::{Row, Sqlite, SqlitePool, Transaction};

use crate::{Result, StorageError};

use super::jobs::{
    OutputDestinationReservation, insert_output_reservation, validate_output_reservation,
};
use super::util::{decode, encode, enum_text};

#[derive(Clone, Debug)]
pub struct ProofingRepository {
    pool: SqlitePool,
}

impl ProofingRepository {
    #[must_use]
    pub fn new(pool: SqlitePool) -> Self {
        Self { pool }
    }

    pub async fn replace_plan(
        &self,
        plan: &ProofingPlan,
        segments: &[ProductionSegment],
    ) -> Result<()> {
        validate_plan_segments(plan, segments)?;
        let mut tx = self.pool.begin().await?;
        deactivate_active_segments(&mut tx, plan.project_id).await?;
        for segment in segments {
            insert_segment(&mut tx, segment).await?;
        }
        upsert_plan(&mut tx, plan).await?;
        tx.commit().await?;
        Ok(())
    }

    /// Replaces the active proofing plan and persists its complete job graph atomically.
    /// A failed unit insert therefore cannot retire the previously usable proofing plan.
    pub async fn replace_plan_with_units(
        &self,
        plan: &ProofingPlan,
        segments: &[ProductionSegment],
        units: &[JobUnit],
    ) -> Result<()> {
        validate_plan_segments(plan, segments)?;
        validate_job_units(plan.source_conversion_job_id, units)?;
        let mut tx = self.pool.begin().await?;
        deactivate_active_segments(&mut tx, plan.project_id).await?;
        for segment in segments {
            insert_segment(&mut tx, segment).await?;
        }
        // Units reference the newly inserted proof segments. A later graph failure still restores
        // the prior active plan because the deactivation and inserts share this transaction.
        for unit in units {
            upsert_job_unit(&mut tx, unit).await?;
        }
        upsert_plan(&mut tx, plan).await?;
        tx.commit().await?;
        Ok(())
    }

    /// Inserts a complete job graph, including the proof-export audit snapshot when supplied.
    /// The job never becomes restart-visible without every required durable input.
    pub async fn insert_job_graph(
        &self,
        job: &Job,
        units: &[JobUnit],
        snapshot: Option<&ProofExportSnapshot>,
    ) -> Result<()> {
        self.insert_job_graph_inner(job, units, snapshot, None)
            .await
    }

    /// Atomically admits a proof export, its immutable graph/snapshot, and its output claim.
    pub async fn insert_export_job_graph_with_output_reservation(
        &self,
        job: &Job,
        units: &[JobUnit],
        snapshot: &ProofExportSnapshot,
        reservation: &OutputDestinationReservation,
    ) -> Result<()> {
        validate_output_reservation(job, reservation)?;
        self.insert_job_graph_inner(job, units, Some(snapshot), Some(reservation))
            .await
    }

    async fn insert_job_graph_inner(
        &self,
        job: &Job,
        units: &[JobUnit],
        snapshot: Option<&ProofExportSnapshot>,
        output_reservation: Option<&OutputDestinationReservation>,
    ) -> Result<()> {
        job.validate()?;
        validate_job_units(job.id, units)?;
        match (job.kind, snapshot) {
            (JobKind::Export, Some(snapshot)) => {
                validate_proof_export_snapshot_graph(snapshot, units)?;
            }
            (JobKind::Export, None) => {
                return Err(StorageError::InvalidData(
                    "proof export job requires an audit snapshot".to_owned(),
                ));
            }
            (_, Some(_)) => {
                return Err(StorageError::InvalidData(
                    "proof export snapshot belongs to a non-export job".to_owned(),
                ));
            }
            (_, None) => {}
        }
        if let Some(snapshot) = snapshot
            && (snapshot.job_id != job.id
                || snapshot.project_id != job.project_id
                || Some(snapshot.export_profile_id) != job.export_profile_id)
        {
            return Err(StorageError::InvalidData(
                "proof export snapshot does not match its job".to_owned(),
            ));
        }
        let mut tx = self.pool.begin().await?;
        insert_job(&mut tx, job).await?;
        if let Some(reservation) = output_reservation {
            insert_output_reservation(&mut tx, reservation).await?;
        }
        for unit in units {
            upsert_job_unit(&mut tx, unit).await?;
        }
        if let Some(snapshot) = snapshot {
            insert_export_snapshot(&mut tx, snapshot).await?;
        }
        tx.commit().await?;
        Ok(())
    }

    pub async fn get_plan(&self, project_id: ProjectId) -> Result<Option<ProofingPlan>> {
        let payload = sqlx::query_scalar::<_, String>(
            "SELECT payload FROM proofing_projects WHERE project_id = ?",
        )
        .bind(project_id.to_string())
        .fetch_optional(&self.pool)
        .await?;
        payload.as_deref().map(decode).transpose()
    }

    pub async fn update_plan(&self, plan: &ProofingPlan, expected_revision: u64) -> Result<()> {
        let result = sqlx::query(
            "UPDATE proofing_projects SET plan_revision = ?, plan_hash = ?, status = ?, \
             updated_at = ?, payload = ? WHERE project_id = ? AND plan_revision = ?",
        )
        .bind(i64::try_from(plan.plan_revision).unwrap_or(i64::MAX))
        .bind(&plan.plan_hash)
        .bind(enum_text(&plan.status)?)
        .bind(plan.updated_at.to_rfc3339())
        .bind(encode(plan)?)
        .bind(plan.project_id.to_string())
        .bind(i64::try_from(expected_revision).unwrap_or(i64::MAX))
        .execute(&self.pool)
        .await?;
        if result.rows_affected() == 0 {
            return Err(StorageError::StaleRevision {
                entity: "proofing plan",
                id: plan.project_id.to_string(),
            });
        }
        Ok(())
    }

    pub async fn get_segment(&self, id: SegmentId) -> Result<Option<ProductionSegment>> {
        let row = sqlx::query("SELECT active, payload FROM production_segments WHERE id = ?")
            .bind(id.to_string())
            .fetch_optional(&self.pool)
            .await?;
        row.map(|row| {
            let mut segment: ProductionSegment = decode(row.get::<&str, _>("payload"))?;
            // `active` is an authorization/admission boundary. Always trust the relational
            // column even when reading data written by an older build with a stale JSON payload.
            segment.active = row.get::<bool, _>("active");
            Ok(segment)
        })
        .transpose()
    }

    pub async fn list_active_segments(
        &self,
        project_id: ProjectId,
        chapter_id: Option<audiobookai_core::ChapterId>,
    ) -> Result<Vec<ProductionSegment>> {
        let rows = if let Some(chapter_id) = chapter_id {
            sqlx::query(
                "SELECT active, payload FROM production_segments \
                 WHERE project_id = ? AND chapter_id = ? AND active = 1 ORDER BY ordinal",
            )
            .bind(project_id.to_string())
            .bind(chapter_id.to_string())
            .fetch_all(&self.pool)
            .await?
        } else {
            sqlx::query(
                "SELECT active, payload FROM production_segments \
                 WHERE project_id = ? AND active = 1 ORDER BY chapter_id, ordinal",
            )
            .bind(project_id.to_string())
            .fetch_all(&self.pool)
            .await?
        };
        rows.into_iter()
            .map(|row| {
                let mut segment: ProductionSegment = decode(row.get::<&str, _>("payload"))?;
                segment.active = row.get::<bool, _>("active");
                Ok(segment)
            })
            .collect()
    }

    pub async fn update_segment(
        &self,
        segment: &ProductionSegment,
        expected_revision: u64,
    ) -> Result<ProductionSegment> {
        let mut stored = segment.clone();
        stored.revision = expected_revision.saturating_add(1);
        let result = sqlx::query(
            "UPDATE production_segments SET expected_input_hash = ?, active = ?, review_state = ?, \
             revision = ?, updated_at = ?, payload = ? WHERE id = ? AND project_id = ? AND revision = ?",
        )
        .bind(&stored.expected_input_hash)
        .bind(stored.active)
        .bind(enum_text(&stored.review_state)?)
        .bind(i64::try_from(stored.revision).unwrap_or(i64::MAX))
        .bind(stored.updated_at.to_rfc3339())
        .bind(encode(&stored)?)
        .bind(stored.id.to_string())
        .bind(stored.project_id.to_string())
        .bind(i64::try_from(expected_revision).unwrap_or(i64::MAX))
        .execute(&self.pool)
        .await?;
        if result.rows_affected() == 0 {
            return Err(self.stale_or_missing_segment(stored.id).await?);
        }
        Ok(stored)
    }

    pub async fn update_segment_and_plan(
        &self,
        segment: &ProductionSegment,
        expected_segment_revision: u64,
        plan: &ProofingPlan,
        expected_plan_revision: u64,
    ) -> Result<ProductionSegment> {
        let mut stored = segment.clone();
        stored.revision = expected_segment_revision.saturating_add(1);
        let mut tx = self.pool.begin().await?;
        let segment_result = sqlx::query(
            "UPDATE production_segments SET expected_input_hash = ?, active = ?, review_state = ?, \
             revision = ?, updated_at = ?, payload = ? WHERE id = ? AND project_id = ? AND revision = ?",
        )
        .bind(&stored.expected_input_hash)
        .bind(stored.active)
        .bind(enum_text(&stored.review_state)?)
        .bind(i64::try_from(stored.revision).unwrap_or(i64::MAX))
        .bind(stored.updated_at.to_rfc3339())
        .bind(encode(&stored)?)
        .bind(stored.id.to_string())
        .bind(stored.project_id.to_string())
        .bind(i64::try_from(expected_segment_revision).unwrap_or(i64::MAX))
        .execute(&mut *tx)
        .await?;
        if segment_result.rows_affected() == 0 {
            return Err(StorageError::StaleRevision {
                entity: "production segment",
                id: stored.id.to_string(),
            });
        }
        let plan_result = sqlx::query(
            "UPDATE proofing_projects SET plan_revision = ?, plan_hash = ?, status = ?, \
             updated_at = ?, payload = ? WHERE project_id = ? AND plan_revision = ?",
        )
        .bind(i64::try_from(plan.plan_revision).unwrap_or(i64::MAX))
        .bind(&plan.plan_hash)
        .bind(enum_text(&plan.status)?)
        .bind(plan.updated_at.to_rfc3339())
        .bind(encode(plan)?)
        .bind(plan.project_id.to_string())
        .bind(i64::try_from(expected_plan_revision).unwrap_or(i64::MAX))
        .execute(&mut *tx)
        .await?;
        if plan_result.rows_affected() == 0 {
            return Err(StorageError::StaleRevision {
                entity: "proofing plan",
                id: plan.project_id.to_string(),
            });
        }
        tx.commit().await?;
        Ok(stored)
    }

    pub async fn insert_take_and_select(
        &self,
        take: &SegmentTake,
        selection: &SegmentSelection,
    ) -> Result<()> {
        if take.segment_id != selection.segment_id || take.id != selection.take_id {
            return Err(StorageError::InvalidData(
                "take selection does not match inserted take".to_owned(),
            ));
        }
        let mut tx = self.pool.begin().await?;
        insert_take(&mut tx, take).await?;
        let current_revision = sqlx::query_scalar::<_, i64>(
            "SELECT revision FROM segment_selections WHERE segment_id = ?",
        )
        .bind(selection.segment_id.to_string())
        .fetch_optional(&mut *tx)
        .await?
        .and_then(|revision| u64::try_from(revision).ok());
        let mut stored = selection.clone();
        if let Some(current_revision) = current_revision {
            stored.revision = current_revision.saturating_add(1);
        }
        sqlx::query(
            "INSERT INTO segment_selections (segment_id, take_id, revision, selected_at, payload) \
             VALUES (?, ?, ?, ?, ?) \
             ON CONFLICT(segment_id) DO UPDATE SET take_id = excluded.take_id, \
             revision = excluded.revision, selected_at = excluded.selected_at, \
             payload = excluded.payload",
        )
        .bind(stored.segment_id.to_string())
        .bind(stored.take_id.to_string())
        .bind(i64::try_from(stored.revision).unwrap_or(i64::MAX))
        .bind(stored.selected_at.to_rfc3339())
        .bind(encode(&stored)?)
        .execute(&mut *tx)
        .await?;
        tx.commit().await?;
        Ok(())
    }

    pub async fn insert_take(&self, take: &SegmentTake) -> Result<()> {
        let mut tx = self.pool.begin().await?;
        insert_take(&mut tx, take).await?;
        tx.commit().await?;
        Ok(())
    }

    pub async fn list_takes(&self, segment_id: SegmentId) -> Result<Vec<SegmentTake>> {
        let rows = sqlx::query(
            "SELECT payload FROM segment_takes WHERE segment_id = ? ORDER BY ordinal DESC",
        )
        .bind(segment_id.to_string())
        .fetch_all(&self.pool)
        .await?;
        rows.into_iter()
            .map(|row| decode(row.get::<&str, _>("payload")))
            .collect()
    }

    pub async fn get_take(&self, id: SegmentTakeId) -> Result<Option<SegmentTake>> {
        let payload =
            sqlx::query_scalar::<_, String>("SELECT payload FROM segment_takes WHERE id = ?")
                .bind(id.to_string())
                .fetch_optional(&self.pool)
                .await?;
        payload.as_deref().map(decode).transpose()
    }

    pub async fn get_selection(&self, segment_id: SegmentId) -> Result<Option<SegmentSelection>> {
        let payload = sqlx::query_scalar::<_, String>(
            "SELECT payload FROM segment_selections WHERE segment_id = ?",
        )
        .bind(segment_id.to_string())
        .fetch_optional(&self.pool)
        .await?;
        payload.as_deref().map(decode).transpose()
    }

    pub async fn select_take(
        &self,
        selection: &SegmentSelection,
        expected_revision: u64,
    ) -> Result<SegmentSelection> {
        let belongs = sqlx::query_scalar::<_, i64>(
            "SELECT COUNT(*) FROM segment_takes WHERE id = ? AND segment_id = ?",
        )
        .bind(selection.take_id.to_string())
        .bind(selection.segment_id.to_string())
        .fetch_one(&self.pool)
        .await?
            == 1;
        if !belongs {
            return Err(StorageError::InvalidData(
                "selected take belongs to another segment".to_owned(),
            ));
        }
        let mut stored = selection.clone();
        stored.revision = expected_revision.saturating_add(1);
        let result = sqlx::query(
            "UPDATE segment_selections SET take_id = ?, revision = ?, selected_at = ?, payload = ? \
             WHERE segment_id = ? AND revision = ?",
        )
        .bind(stored.take_id.to_string())
        .bind(i64::try_from(stored.revision).unwrap_or(i64::MAX))
        .bind(stored.selected_at.to_rfc3339())
        .bind(encode(&stored)?)
        .bind(stored.segment_id.to_string())
        .bind(i64::try_from(expected_revision).unwrap_or(i64::MAX))
        .execute(&self.pool)
        .await?;
        if result.rows_affected() == 0 {
            return Err(StorageError::StaleRevision {
                entity: "segment selection",
                id: stored.segment_id.to_string(),
            });
        }
        Ok(stored)
    }

    /// Selects a take and resets the segment review state in one optimistic transaction.
    pub async fn select_take_and_update_segment(
        &self,
        selection: &SegmentSelection,
        expected_selection_revision: u64,
        segment: &ProductionSegment,
        expected_segment_revision: u64,
    ) -> Result<(SegmentSelection, ProductionSegment)> {
        if selection.segment_id != segment.id {
            return Err(StorageError::InvalidData(
                "selection and segment ids do not match".to_owned(),
            ));
        }
        let mut tx = self.pool.begin().await?;
        let belongs = sqlx::query_scalar::<_, i64>(
            "SELECT COUNT(*) FROM segment_takes WHERE id = ? AND segment_id = ?",
        )
        .bind(selection.take_id.to_string())
        .bind(selection.segment_id.to_string())
        .fetch_one(&mut *tx)
        .await?
            == 1;
        if !belongs {
            return Err(StorageError::InvalidData(
                "selected take belongs to another segment".to_owned(),
            ));
        }
        let mut stored_selection = selection.clone();
        stored_selection.revision = expected_selection_revision.saturating_add(1);
        let selection_result = sqlx::query(
            "UPDATE segment_selections SET take_id = ?, revision = ?, selected_at = ?, payload = ? \
             WHERE segment_id = ? AND revision = ?",
        )
        .bind(stored_selection.take_id.to_string())
        .bind(i64::try_from(stored_selection.revision).unwrap_or(i64::MAX))
        .bind(stored_selection.selected_at.to_rfc3339())
        .bind(encode(&stored_selection)?)
        .bind(stored_selection.segment_id.to_string())
        .bind(i64::try_from(expected_selection_revision).unwrap_or(i64::MAX))
        .execute(&mut *tx)
        .await?;
        if selection_result.rows_affected() == 0 {
            return Err(StorageError::StaleRevision {
                entity: "segment selection",
                id: stored_selection.segment_id.to_string(),
            });
        }

        let mut stored_segment = segment.clone();
        stored_segment.revision = expected_segment_revision.saturating_add(1);
        let segment_result = sqlx::query(
            "UPDATE production_segments SET expected_input_hash = ?, active = ?, review_state = ?, \
             revision = ?, updated_at = ?, payload = ? WHERE id = ? AND project_id = ? AND revision = ?",
        )
        .bind(&stored_segment.expected_input_hash)
        .bind(stored_segment.active)
        .bind(enum_text(&stored_segment.review_state)?)
        .bind(i64::try_from(stored_segment.revision).unwrap_or(i64::MAX))
        .bind(stored_segment.updated_at.to_rfc3339())
        .bind(encode(&stored_segment)?)
        .bind(stored_segment.id.to_string())
        .bind(stored_segment.project_id.to_string())
        .bind(i64::try_from(expected_segment_revision).unwrap_or(i64::MAX))
        .execute(&mut *tx)
        .await?;
        if segment_result.rows_affected() == 0 {
            return Err(StorageError::StaleRevision {
                entity: "production segment",
                id: stored_segment.id.to_string(),
            });
        }
        tx.commit().await?;
        Ok((stored_selection, stored_segment))
    }

    async fn stale_or_missing_segment(&self, id: SegmentId) -> Result<StorageError> {
        let exists =
            sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM production_segments WHERE id = ?")
                .bind(id.to_string())
                .fetch_one(&self.pool)
                .await?
                > 0;
        Ok(if exists {
            StorageError::StaleRevision {
                entity: "production segment",
                id: id.to_string(),
            }
        } else {
            StorageError::NotFound {
                entity: "production segment",
                id: id.to_string(),
            }
        })
    }
}

fn validate_plan_segments(plan: &ProofingPlan, segments: &[ProductionSegment]) -> Result<()> {
    if segments
        .iter()
        .any(|segment| segment.project_id != plan.project_id || !segment.active)
    {
        return Err(StorageError::InvalidData(
            "proofing segment does not belong to the active project plan".to_owned(),
        ));
    }
    Ok(())
}

fn validate_job_units(job_id: JobId, units: &[JobUnit]) -> Result<()> {
    if units.is_empty() {
        return Err(StorageError::InvalidData(
            "durable job graph cannot be empty".to_owned(),
        ));
    }
    let ids = units.iter().map(|unit| unit.id).collect::<BTreeSet<_>>();
    if ids.len() != units.len() {
        return Err(StorageError::InvalidData(
            "durable job graph contains duplicate unit ids".to_owned(),
        ));
    }
    if units.iter().any(|unit| {
        unit.job_id != job_id
            || unit
                .dependencies
                .iter()
                .any(|dependency| *dependency == unit.id || !ids.contains(dependency))
    }) {
        return Err(StorageError::InvalidData(
            "durable job graph contains a foreign or invalid dependency".to_owned(),
        ));
    }
    Ok(())
}

fn validate_proof_export_snapshot_graph(
    snapshot: &ProofExportSnapshot,
    units: &[JobUnit],
) -> Result<()> {
    let mut expected = std::collections::BTreeMap::new();
    for selection in &snapshot.selections {
        if expected
            .insert(selection.segment_id, selection.artifact_id)
            .is_some()
        {
            return Err(StorageError::InvalidData(
                "proof export snapshot contains duplicate segment selections".to_owned(),
            ));
        }
    }
    let mut actual = std::collections::BTreeMap::new();
    for unit in units
        .iter()
        .filter(|unit| unit.kind == audiobookai_core::JobUnitKind::SynthesisSegment)
    {
        let (Some(segment_id), Some(artifact_id)) = (unit.segment_id, unit.output_artifact_id)
        else {
            return Err(StorageError::InvalidData(
                "proof export synthesis unit is missing its selected artifact".to_owned(),
            ));
        };
        if unit.state != audiobookai_core::JobUnitState::Completed
            || actual.insert(segment_id, artifact_id).is_some()
        {
            return Err(StorageError::InvalidData(
                "proof export graph contains a duplicate or incomplete selection".to_owned(),
            ));
        }
    }
    let export_units = units
        .iter()
        .filter(|unit| unit.kind == audiobookai_core::JobUnitKind::FinalExport)
        .collect::<Vec<_>>();
    let snapshot_id = export_units
        .first()
        .and_then(|unit| unit.payload.get("proofExportSnapshotId"))
        .cloned()
        .map(serde_json::from_value::<audiobookai_core::ProofExportSnapshotId>)
        .transpose()
        .map_err(|error| StorageError::InvalidData(error.to_string()))?;
    if expected.is_empty()
        || actual != expected
        || export_units.len() != 1
        || snapshot_id != Some(snapshot.id)
    {
        return Err(StorageError::InvalidData(
            "proof export graph does not match its audit snapshot".to_owned(),
        ));
    }
    Ok(())
}

async fn deactivate_active_segments(
    tx: &mut Transaction<'_, Sqlite>,
    project_id: ProjectId,
) -> Result<()> {
    let rows = sqlx::query(
        "SELECT id, payload FROM production_segments WHERE project_id = ? AND active = 1",
    )
    .bind(project_id.to_string())
    .fetch_all(&mut **tx)
    .await?;
    for row in rows {
        let id = row.get::<String, _>("id");
        let mut segment: ProductionSegment = decode(row.get::<&str, _>("payload"))?;
        segment.active = false;
        sqlx::query("UPDATE production_segments SET active = 0, payload = ? WHERE id = ?")
            .bind(encode(&segment)?)
            .bind(id)
            .execute(&mut **tx)
            .await?;
    }
    Ok(())
}

async fn upsert_plan(tx: &mut Transaction<'_, Sqlite>, plan: &ProofingPlan) -> Result<()> {
    sqlx::query(
        "INSERT INTO proofing_projects \
         (project_id, source_conversion_job_id, plan_revision, plan_hash, status, created_at, updated_at, payload) \
         VALUES (?, ?, ?, ?, ?, ?, ?, ?) \
         ON CONFLICT(project_id) DO UPDATE SET \
         source_conversion_job_id = excluded.source_conversion_job_id, \
         plan_revision = excluded.plan_revision, plan_hash = excluded.plan_hash, \
         status = excluded.status, updated_at = excluded.updated_at, payload = excluded.payload",
    )
    .bind(plan.project_id.to_string())
    .bind(plan.source_conversion_job_id.to_string())
    .bind(i64::try_from(plan.plan_revision).unwrap_or(i64::MAX))
    .bind(&plan.plan_hash)
    .bind(enum_text(&plan.status)?)
    .bind(plan.created_at.to_rfc3339())
    .bind(plan.updated_at.to_rfc3339())
    .bind(encode(plan)?)
    .execute(&mut **tx)
    .await?;
    Ok(())
}

async fn insert_job(tx: &mut Transaction<'_, Sqlite>, job: &Job) -> Result<()> {
    sqlx::query(
        "INSERT INTO jobs \
         (id, project_id, export_profile_id, reservation_id, kind, state, revision, created_at, updated_at, payload) \
         VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
    )
    .bind(job.id.to_string())
    .bind(job.project_id.to_string())
    .bind(job.export_profile_id.map(|id| id.to_string()))
    .bind(job.reservation_id.map(|id| id.to_string()))
    .bind(enum_text(&job.kind)?)
    .bind(enum_text(&job.state)?)
    .bind(i64::try_from(job.revision).unwrap_or(i64::MAX))
    .bind(job.created_at.to_rfc3339())
    .bind(job.updated_at.to_rfc3339())
    .bind(encode(job)?)
    .execute(&mut **tx)
    .await?;
    Ok(())
}

async fn upsert_job_unit(tx: &mut Transaction<'_, Sqlite>, unit: &JobUnit) -> Result<()> {
    sqlx::query(
        "INSERT INTO job_units \
         (id, job_id, chapter_id, segment_id, proof_segment_id, provider_id, kind, state, next_attempt_at, updated_at, payload) \
         VALUES (?, ?, ?, NULL, ?, ?, ?, ?, ?, ?, ?) \
         ON CONFLICT(id) DO UPDATE SET state = excluded.state, next_attempt_at = excluded.next_attempt_at, \
         proof_segment_id = excluded.proof_segment_id, updated_at = excluded.updated_at, payload = excluded.payload",
    )
    .bind(unit.id.to_string())
    .bind(unit.job_id.to_string())
    .bind(unit.chapter_id.map(|id| id.to_string()))
    .bind(unit.segment_id.map(|id| id.to_string()))
    .bind(unit.provider_profile_id.map(|id| id.to_string()))
    .bind(enum_text(&unit.kind)?)
    .bind(enum_text(&unit.state)?)
    .bind(unit.next_attempt_at.map(|value| value.to_rfc3339()))
    .bind(unit.updated_at.to_rfc3339())
    .bind(encode(unit)?)
    .execute(&mut **tx)
    .await?;
    sqlx::query("DELETE FROM job_unit_dependencies WHERE job_unit_id = ?")
        .bind(unit.id.to_string())
        .execute(&mut **tx)
        .await?;
    for dependency in &unit.dependencies {
        sqlx::query("INSERT INTO job_unit_dependencies (job_unit_id, depends_on_id) VALUES (?, ?)")
            .bind(unit.id.to_string())
            .bind(dependency.to_string())
            .execute(&mut **tx)
            .await?;
    }
    Ok(())
}

async fn insert_export_snapshot(
    tx: &mut Transaction<'_, Sqlite>,
    snapshot: &ProofExportSnapshot,
) -> Result<()> {
    sqlx::query(
        "INSERT INTO proof_export_snapshots \
         (id, project_id, job_id, plan_revision, plan_hash, created_at, payload) \
         VALUES (?, ?, ?, ?, ?, ?, ?)",
    )
    .bind(snapshot.id.to_string())
    .bind(snapshot.project_id.to_string())
    .bind(snapshot.job_id.to_string())
    .bind(i64::try_from(snapshot.plan_revision).unwrap_or(i64::MAX))
    .bind(&snapshot.plan_hash)
    .bind(snapshot.created_at.to_rfc3339())
    .bind(encode(snapshot)?)
    .execute(&mut **tx)
    .await?;
    Ok(())
}

async fn insert_segment(
    tx: &mut Transaction<'_, Sqlite>,
    segment: &ProductionSegment,
) -> Result<()> {
    sqlx::query(
        "INSERT INTO production_segments \
         (id, project_id, chapter_id, paragraph_id, source_kind, stable_key, ordinal, \
          source_content_hash, byte_start, byte_end, speaker_key, expected_input_hash, active, \
          review_state, revision, created_at, updated_at, payload) \
         VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
    )
    .bind(segment.id.to_string())
    .bind(segment.project_id.to_string())
    .bind(segment.chapter_id.map(|id| id.to_string()))
    .bind(segment.paragraph_id.map(|id| id.to_string()))
    .bind(enum_text(&segment.source)?)
    .bind(&segment.stable_key)
    .bind(i64::from(segment.ordinal))
    .bind(&segment.source_content_hash)
    .bind(
        segment
            .byte_start
            .and_then(|value| i64::try_from(value).ok()),
    )
    .bind(segment.byte_end.and_then(|value| i64::try_from(value).ok()))
    .bind(encode(&segment.speaker)?)
    .bind(&segment.expected_input_hash)
    .bind(segment.active)
    .bind(enum_text(&segment.review_state)?)
    .bind(i64::try_from(segment.revision).unwrap_or(i64::MAX))
    .bind(segment.created_at.to_rfc3339())
    .bind(segment.updated_at.to_rfc3339())
    .bind(encode(segment)?)
    .execute(&mut **tx)
    .await?;
    Ok(())
}

async fn insert_take(tx: &mut Transaction<'_, Sqlite>, take: &SegmentTake) -> Result<()> {
    sqlx::query(
        "INSERT INTO segment_takes \
         (id, segment_id, artifact_id, ordinal, source_job_id, source_job_unit_id, \
          semantic_input_hash, duration_ms, created_at, payload) \
         VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
    )
    .bind(take.id.to_string())
    .bind(take.segment_id.to_string())
    .bind(take.artifact_id.to_string())
    .bind(i64::from(take.ordinal))
    .bind(take.source_job_id.to_string())
    .bind(take.source_job_unit_id.to_string())
    .bind(&take.semantic_input_hash)
    .bind(i64::try_from(take.duration_ms).unwrap_or(i64::MAX))
    .bind(take.created_at.to_rfc3339())
    .bind(encode(take)?)
    .execute(&mut **tx)
    .await?;
    Ok(())
}
