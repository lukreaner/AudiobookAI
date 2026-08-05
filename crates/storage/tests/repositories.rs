use std::collections::BTreeMap;

use audiobookai_core::{
    Book, BookId, BookMetadata, Budget, BudgetAllocation, BudgetId, BudgetMetric, BudgetPeriod,
    BudgetReservation, BudgetScope, BudgetScopeKind, Chapter, ChapterId, CloudConsent,
    ExportLayout, ExportProfileId, FileFingerprint, Job, JobId, JobKind, JobState, JobUnit,
    JobUnitId, JobUnitKind, JobUnitState, Money, Paragraph, ParagraphId, ParagraphKind,
    PerformanceSettings, ProductionSegment, ProductionSegmentSource, Project, ProjectId,
    ProjectSettings, ProjectStatus, ProofingPlan, ProofingPlanStatus, ProvenanceQuality,
    ProviderDeployment, ProviderFamily, ProviderProfile, ProviderProfileId, ProviderRole,
    ReservationId, ReservationStatus, SegmentId, SegmentReviewState, SettingsMap, Speaker,
    TimingSettings, UsageEvent, UsageEventId, UsageQuantities, UsageWorkload,
};
use audiobookai_storage::{
    Database, OutputDestinationReservation, OutputReservationState, StorageError,
    repositories::{IdempotencyClaim, IdempotentResponse, Repositories, UsageFilter},
};
use chrono::{DateTime, Duration, TimeZone, Utc};
use tempfile::TempDir;

fn imported_entities() -> (Book, Project, Chapter, Paragraph) {
    let now = Utc::now();
    let book_id = BookId::new();
    let chapter_id = ChapterId::new();
    let metadata = BookMetadata {
        title: "A Test Book".into(),
        authors: vec!["Ada Author".into()],
        language: Some("en".into()),
        ..BookMetadata::default()
    };
    let book = Book {
        id: book_id,
        managed_epub_path: "/managed/library/test.epub".into(),
        original_filename: "test.epub".into(),
        source_fingerprint: FileFingerprint {
            algorithm: "blake3".into(),
            digest: "abc123".into(),
            size_bytes: 42,
        },
        epub_version: Some("3.0".into()),
        metadata: metadata.clone(),
        imported_at: now,
    };
    let project = Project {
        id: ProjectId::new(),
        book_id,
        name: "Test Project".into(),
        status: ProjectStatus::Draft,
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
        title: "Chapter One".into(),
        source_href: "chapter1.xhtml".into(),
        selected: true,
        text_hash: "chapter-hash".into(),
        character_count: 11,
    };
    let paragraph = Paragraph {
        id: ParagraphId::new(),
        chapter_id,
        ordinal: 0,
        kind: ParagraphKind::Prose,
        text: "Hello world".into(),
        source_start: 0,
        source_end: 11,
        content_hash: "paragraph-hash".into(),
    };
    (book, project, chapter, paragraph)
}

fn provider(now: chrono::DateTime<Utc>) -> ProviderProfile {
    ProviderProfile {
        id: ProviderProfileId::new(),
        name: "LocalAI".into(),
        family: ProviderFamily::LocalAi,
        role: ProviderRole::Both,
        deployment: ProviderDeployment::ExternalEndpoint,
        endpoint: Some("http://127.0.0.1:8080".into()),
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
    }
}

fn job(project_id: ProjectId, reservation_id: Option<ReservationId>) -> Job {
    let now = Utc::now();
    Job {
        id: JobId::new(),
        project_id,
        kind: JobKind::Conversion,
        state: JobState::Queued,
        export_profile_id: None,
        reservation_id,
        progress_completed: 0,
        progress_total: 1,
        status_message: None,
        allow_budget_override: false,
        created_at: now,
        started_at: None,
        finished_at: None,
        updated_at: now,
        revision: 0,
    }
}

fn output_reservation(
    job: &Job,
    destination_key: &str,
    destination_path: &str,
) -> OutputDestinationReservation {
    OutputDestinationReservation {
        job_id: job.id,
        project_id: job.project_id,
        destination_key: destination_key.to_owned(),
        destination_path: destination_path.to_owned(),
        layout: ExportLayout::SingleFile,
        state: OutputReservationState::Reserved,
        created_at: job.created_at,
        updated_at: job.created_at,
        promoted_at: None,
    }
}

fn proofing_segment(project_id: ProjectId, stable_key: &str) -> ProductionSegment {
    let now = Utc::now();
    ProductionSegment {
        id: SegmentId::new(),
        project_id,
        chapter_id: None,
        paragraph_id: None,
        source: ProductionSegmentSource::OpeningCredit,
        stable_key: stable_key.to_owned(),
        ordinal: 0,
        source_content_hash: format!("source-{stable_key}"),
        byte_start: None,
        byte_end: None,
        speaker: Speaker::Narrator,
        original_text: "Opening credit".to_owned(),
        narration_text_override: None,
        effective_text: "Opening credit".to_owned(),
        context_before: None,
        context_after: None,
        performance_override: PerformanceSettings::default(),
        timing_override: TimingSettings::default(),
        expected_input_hash: format!("input-{stable_key}"),
        review_state: SegmentReviewState::Unreviewed,
        active: true,
        revision: 0,
        created_at: now,
        updated_at: now,
    }
}

fn proofing_plan(project_id: ProjectId, job_id: JobId, revision: u64) -> ProofingPlan {
    let now = Utc::now();
    ProofingPlan {
        project_id,
        source_conversion_job_id: job_id,
        plan_revision: revision,
        plan_hash: format!("plan-{revision}"),
        status: ProofingPlanStatus::Ready,
        dirty_reasons: Vec::new(),
        created_at: now,
        updated_at: now,
    }
}

fn job_unit(job_id: JobId, dependencies: Vec<JobUnitId>) -> JobUnit {
    let now = Utc::now();
    JobUnit {
        id: JobUnitId::new(),
        job_id,
        kind: JobUnitKind::SynthesisSegment,
        state: JobUnitState::Ready,
        chapter_id: None,
        segment_id: None,
        provider_profile_id: None,
        dependencies,
        attempt_count: 0,
        next_attempt_at: None,
        output_artifact_id: None,
        payload: BTreeMap::new(),
        created_at: now,
        updated_at: now,
    }
}

fn utc(year: i32, month: u32, day: u32, hour: u32, minute: u32) -> DateTime<Utc> {
    Utc.with_ymd_and_hms(year, month, day, hour, minute, 0)
        .single()
        .expect("test timestamp")
}

fn global_budget(period: BudgetPeriod, limit: i64, used: i64, now: DateTime<Utc>) -> Budget {
    Budget {
        id: BudgetId::new(),
        name: format!("{period:?} test budget"),
        scope: BudgetScope {
            kind: BudgetScopeKind::Global,
            provider_profile_id: None,
        },
        period,
        metric: BudgetMetric::Characters,
        currency: None,
        limit,
        used,
        warning_threshold_percent: 80,
        hard: true,
        enabled: true,
        period_started_at: now,
        period_ends_at: match period {
            BudgetPeriod::Daily => Some(now + Duration::days(1)),
            BudgetPeriod::Monthly => Some(now + Duration::days(31)),
            BudgetPeriod::Job | BudgetPeriod::Lifetime => None,
        },
        created_at: now,
        updated_at: now,
    }
}

async fn insert_budget_job(
    repositories: &Repositories,
    project_id: ProjectId,
    reservation_id: ReservationId,
) -> JobId {
    let job = job(project_id, Some(reservation_id));
    let id = job.id;
    repositories.jobs.insert(&job).await.expect("insert job");
    id
}

async fn insert_test_project(repositories: &Repositories) -> ProjectId {
    let (book, project, _, _) = imported_entities();
    repositories
        .projects
        .insert_book(&book)
        .await
        .expect("insert book");
    repositories
        .projects
        .insert_project(&project)
        .await
        .expect("insert project");
    project.id
}

fn reservation(
    id: ReservationId,
    job_id: JobId,
    budget_id: BudgetId,
    amount: i64,
    created_at: DateTime<Utc>,
    expires_at: Option<DateTime<Utc>>,
) -> BudgetReservation {
    BudgetReservation {
        id,
        job_id,
        status: ReservationStatus::Active,
        allocations: vec![BudgetAllocation {
            budget_id,
            reserved_amount: amount,
            actual_amount: None,
        }],
        created_at,
        expires_at,
        reconciled_at: None,
    }
}

fn usage_event(
    project_id: ProjectId,
    job_id: JobId,
    provider_id: ProviderProfileId,
    characters: u64,
    occurred_at: DateTime<Utc>,
) -> UsageEvent {
    UsageEvent {
        id: UsageEventId::new(),
        occurred_at,
        workload: UsageWorkload::Tts,
        project_id,
        job_id: Some(job_id),
        attempt_id: None,
        chapter_id: None,
        segment_id: None,
        provider_profile_id: provider_id,
        provider_family: "local_ai".into(),
        endpoint_family: "openai_compatible".into(),
        model: Some("tts-1".into()),
        voice_profile_id: None,
        provider_request_id: None,
        quantities: UsageQuantities {
            characters: Some(characters),
            ..UsageQuantities::default()
        },
        quantity_source: ProvenanceQuality::Reported,
        cost: None,
        cost_source: ProvenanceQuality::Unknown,
        rate_card_id: None,
        uncertain_charge: false,
        redacted_raw_usage: BTreeMap::new(),
    }
}

async fn budget_at(repositories: &Repositories, budget_id: BudgetId, now: DateTime<Utc>) -> Budget {
    repositories
        .budgets
        .get_at(budget_id, now)
        .await
        .expect("read budget")
        .expect("budget exists")
}

async fn reserved_at(repositories: &Repositories, budget_id: BudgetId, now: DateTime<Utc>) -> i64 {
    repositories
        .budgets
        .active_reserved_at(budget_id, now)
        .await
        .expect("read reservations")
}

fn assert_budget_window(
    budget: &Budget,
    used: i64,
    starts_at: DateTime<Utc>,
    ends_at: DateTime<Utc>,
) {
    assert_eq!(budget.used, used);
    assert_eq!(budget.period_started_at, starts_at);
    assert_eq!(budget.period_ends_at, Some(ends_at));
}

#[tokio::test]
async fn opens_with_wal_foreign_keys_and_single_writer_lock() {
    let root = TempDir::new().expect("temp dir");
    let database = Database::open_in(root.path()).await.expect("open database");
    let journal_mode: String = sqlx::query_scalar("PRAGMA journal_mode")
        .fetch_one(database.pool())
        .await
        .expect("journal mode");
    let foreign_keys: i64 = sqlx::query_scalar("PRAGMA foreign_keys")
        .fetch_one(database.pool())
        .await
        .expect("foreign keys");
    assert_eq!(journal_mode, "wal");
    assert_eq!(foreign_keys, 1);

    let second = Database::open_in(root.path()).await;
    assert!(matches!(second, Err(StorageError::AlreadyRunning(_))));
}

#[tokio::test]
async fn import_round_trips_relational_children() {
    let root = TempDir::new().expect("temp dir");
    let database = Database::open_in(root.path()).await.expect("open database");
    let repositories = database.repositories();
    let (book, project, chapter, paragraph) = imported_entities();

    repositories
        .projects
        .create_import(
            &book,
            &project,
            std::slice::from_ref(&chapter),
            std::slice::from_ref(&paragraph),
        )
        .await
        .expect("create import");

    assert_eq!(
        repositories.projects.get_project(project.id).await.unwrap(),
        Some(project)
    );
    assert_eq!(
        repositories.projects.list_chapters(book.id).await.unwrap(),
        vec![chapter]
    );
    assert_eq!(
        repositories
            .projects
            .list_paragraphs(paragraph.chapter_id)
            .await
            .unwrap(),
        vec![paragraph]
    );
}

#[tokio::test]
async fn jobs_use_optimistic_revisions_and_valid_transitions() {
    let root = TempDir::new().expect("temp dir");
    let database = Database::open_in(root.path()).await.expect("open database");
    let repositories = database.repositories();
    let (book, project, _, _) = imported_entities();
    repositories.projects.insert_book(&book).await.unwrap();
    repositories
        .projects
        .insert_project(&project)
        .await
        .unwrap();
    let queued = job(project.id, None);
    repositories.jobs.insert(&queued).await.unwrap();

    let running = repositories
        .jobs
        .transition(queued.id, 0, JobState::Running, Utc::now())
        .await
        .expect("start job");
    assert_eq!(running.state, JobState::Running);
    assert_eq!(running.revision, 1);
    assert!(matches!(
        repositories
            .jobs
            .transition(queued.id, 0, JobState::Pausing, Utc::now())
            .await,
        Err(StorageError::StaleRevision { .. })
    ));
}

#[tokio::test]
async fn output_destination_admission_is_exclusive_and_atomic() {
    let root = TempDir::new().expect("temp dir");
    let database = Database::open_in(root.path()).await.expect("open database");
    let repositories = database.repositories();
    let project_id = insert_test_project(&repositories).await;
    let first = job(project_id, None);
    let second = job(project_id, None);
    let path = "/exports/the-book.m4b";
    let first_reservation = output_reservation(&first, path, path);
    let second_reservation = output_reservation(&second, &path.to_uppercase(), path);

    let (first_result, second_result) = tokio::join!(
        repositories
            .jobs
            .insert_with_output_reservation(&first, &first_reservation),
        repositories
            .jobs
            .insert_with_output_reservation(&second, &second_reservation),
    );
    assert_ne!(first_result.is_ok(), second_result.is_ok());
    let (winner, loser, error) = if first_result.is_ok() {
        (
            &first,
            &second,
            second_result.expect_err("second destination claim"),
        )
    } else {
        (
            &second,
            &first,
            first_result.expect_err("first destination claim"),
        )
    };
    assert!(matches!(
        error,
        StorageError::Conflict {
            entity: "output destination",
            ..
        }
    ));
    assert!(repositories.jobs.get(winner.id).await.unwrap().is_some());
    assert_eq!(repositories.jobs.get(loser.id).await.unwrap(), None);
    assert_eq!(
        repositories
            .jobs
            .get_output_reservation(winner.id)
            .await
            .unwrap()
            .expect("winner reservation")
            .destination_path,
        path
    );
}

#[tokio::test]
#[allow(clippy::too_many_lines)]
async fn output_destination_hierarchy_is_exclusive_in_both_directions() {
    let root = TempDir::new().expect("temp dir");
    let database = Database::open_in(root.path()).await.expect("open database");
    let repositories = database.repositories();
    let project_id = insert_test_project(&repositories).await;
    let mut other_project = repositories
        .projects
        .get_project(project_id)
        .await
        .unwrap()
        .unwrap();
    other_project.id = ProjectId::new();
    other_project.name = "Other project".to_owned();
    repositories
        .projects
        .insert_project(&other_project)
        .await
        .unwrap();
    let owner = job(project_id, None);
    let owner_path = "/exports/book.m4b";
    repositories
        .jobs
        .insert_with_output_reservation(&owner, &output_reservation(&owner, owner_path, owner_path))
        .await
        .unwrap();

    let descendant = job(project_id, None);
    let descendant_path = "/exports/book.m4b/other.m4b";
    assert!(matches!(
        repositories
            .jobs
            .insert_with_output_reservation(
                &descendant,
                &output_reservation(&descendant, descendant_path, descendant_path),
            )
            .await,
        Err(StorageError::Conflict {
            entity: "output destination",
            ..
        })
    ));
    assert_eq!(repositories.jobs.get(descendant.id).await.unwrap(), None);

    let ancestor = job(project_id, None);
    let ancestor_path = "/exports";
    assert!(matches!(
        repositories
            .jobs
            .insert_with_output_reservation(
                &ancestor,
                &output_reservation(&ancestor, ancestor_path, ancestor_path),
            )
            .await,
        Err(StorageError::Conflict {
            entity: "output destination",
            ..
        })
    ));
    assert_eq!(repositories.jobs.get(ancestor.id).await.unwrap(), None);

    let manifest_collision = job(other_project.id, None);
    let manifest_path = format!("{owner_path}.manifest.json");
    let mut manifest_reservation =
        output_reservation(&manifest_collision, &manifest_path, &manifest_path);
    manifest_reservation.layout = ExportLayout::PerChapter;
    assert!(matches!(
        repositories
            .jobs
            .insert_with_output_reservation(&manifest_collision, &manifest_reservation)
            .await,
        Err(StorageError::Conflict {
            entity: "output destination",
            ..
        })
    ));
    assert_eq!(
        repositories.jobs.get(manifest_collision.id).await.unwrap(),
        None
    );

    let reverse_blocker = job(project_id, None);
    let reverse_manifest_path = "/exports/reverse.m4b.manifest.json";
    let mut reverse_blocker_reservation = output_reservation(
        &reverse_blocker,
        reverse_manifest_path,
        reverse_manifest_path,
    );
    reverse_blocker_reservation.layout = ExportLayout::PerChapter;
    repositories
        .jobs
        .insert_with_output_reservation(&reverse_blocker, &reverse_blocker_reservation)
        .await
        .unwrap();
    let reverse_single = job(other_project.id, None);
    let reverse_single_path = "/exports/reverse.m4b";
    assert!(matches!(
        repositories
            .jobs
            .insert_with_output_reservation(
                &reverse_single,
                &output_reservation(&reverse_single, reverse_single_path, reverse_single_path,),
            )
            .await,
        Err(StorageError::Conflict {
            entity: "output destination",
            ..
        })
    ));
    assert_eq!(
        repositories.jobs.get(reverse_single.id).await.unwrap(),
        None
    );
}

#[tokio::test]
async fn unicode_equivalent_output_keys_and_legacy_rows_cannot_alias() {
    let root = TempDir::new().expect("temp dir");
    let database = Database::open_in(root.path()).await.expect("open database");
    let repositories = database.repositories();
    let project_id = insert_test_project(&repositories).await;
    let mut other_project = repositories
        .projects
        .get_project(project_id)
        .await
        .unwrap()
        .unwrap();
    other_project.id = ProjectId::new();
    other_project.name = "Unicode alias project".to_owned();
    repositories
        .projects
        .insert_project(&other_project)
        .await
        .unwrap();

    let owner = job(project_id, None);
    let composed = "/exports/caf\u{e9}.m4b";
    let decomposed = "/exports/cafe\u{301}.m4b";
    repositories
        .jobs
        .insert_with_output_reservation(&owner, &output_reservation(&owner, composed, composed))
        .await
        .unwrap();

    // Simulate a reservation written before Unicode normalization was added. Decoding, lookup,
    // and new admission must all compare its normalized representation without requiring a risky
    // in-place migration of an active claim.
    sqlx::query("UPDATE output_destination_reservations SET destination_key = ? WHERE job_id = ?")
        .bind(decomposed)
        .bind(owner.id.to_string())
        .execute(database.pool())
        .await
        .unwrap();
    assert_eq!(
        repositories
            .jobs
            .get_output_reservation(owner.id)
            .await
            .unwrap()
            .unwrap()
            .destination_key,
        audiobookai_storage::normalize_output_destination_key(composed)
    );
    let contained_manifest = format!("{composed}.manifest.json/nested");
    assert_eq!(
        repositories
            .jobs
            .find_output_reservation_containing_path(&contained_manifest)
            .await
            .unwrap()
            .map(|reservation| reservation.job_id),
        Some(owner.id)
    );

    let alias = job(other_project.id, None);
    assert!(matches!(
        repositories
            .jobs
            .insert_with_output_reservation(
                &alias,
                &output_reservation(&alias, composed, composed),
            )
            .await,
        Err(StorageError::Conflict {
            entity: "output destination",
            ..
        })
    ));
    assert_eq!(repositories.jobs.get(alias.id).await.unwrap(), None);
    assert_eq!(
        audiobookai_storage::normalize_output_destination_key("/exports/\u{3c2}.m4b"),
        audiobookai_storage::normalize_output_destination_key("/exports/\u{3c3}.m4b"),
        "filesystem-conservative case folding must merge final and ordinary sigma"
    );
}

#[tokio::test]
async fn only_unpromoted_terminal_output_claims_can_be_reacquired() {
    let root = TempDir::new().expect("temp dir");
    let database = Database::open_in(root.path()).await.expect("open database");
    let repositories = database.repositories();
    let project_id = insert_test_project(&repositories).await;
    let mut first = job(project_id, None);
    let path = "/exports/retry.m4b";
    let reservation = output_reservation(&first, path, path);
    repositories
        .jobs
        .insert_with_output_reservation(&first, &reservation)
        .await
        .unwrap();
    first.transition(JobState::Failed, Utc::now()).unwrap();
    first = repositories
        .jobs
        .update_terminal_with_output_release(&first, 0)
        .await
        .unwrap();
    assert_eq!(
        repositories
            .jobs
            .get_output_reservation(first.id)
            .await
            .unwrap(),
        None
    );
    first.transition(JobState::Queued, Utc::now()).unwrap();
    let reacquired = output_reservation(&first, path, path);
    first = repositories
        .jobs
        .update_with_output_reservation(&first, 1, &reacquired)
        .await
        .expect("retry state and claim commit together");
    assert_eq!(first.state, JobState::Queued);

    repositories
        .jobs
        .mark_output_promoting(first.id, Utc::now())
        .await
        .unwrap();
    assert!(
        !repositories
            .jobs
            .release_unpromoted_output_reservation(first.id)
            .await
            .unwrap()
    );
    let second = job(project_id, None);
    assert!(matches!(
        repositories
            .jobs
            .insert_with_output_reservation(&second, &output_reservation(&second, path, path))
            .await,
        Err(StorageError::Conflict {
            entity: "output destination",
            ..
        })
    ));
    assert_eq!(repositories.jobs.get(second.id).await.unwrap(), None);

    repositories
        .jobs
        .mark_output_promoted(first.id, Utc::now())
        .await
        .unwrap();
    assert_eq!(
        repositories
            .jobs
            .get_output_reservation(first.id)
            .await
            .unwrap()
            .expect("promoted reservation")
            .state,
        OutputReservationState::Promoted
    );
    first.transition(JobState::Running, Utc::now()).unwrap();
    first = repositories.jobs.update(&first, 2).await.unwrap();
    first.transition(JobState::Completed, Utc::now()).unwrap();
    repositories.jobs.update(&first, 3).await.unwrap();
    assert!(
        repositories
            .jobs
            .release_completed_output_reservation(first.id)
            .await
            .unwrap()
    );
    assert_eq!(
        repositories
            .jobs
            .get_output_reservation(first.id)
            .await
            .unwrap(),
        None
    );
}

#[tokio::test]
async fn replaced_proofing_segments_are_authoritatively_inactive() {
    let root = TempDir::new().expect("temp dir");
    let database = Database::open_in(root.path()).await.expect("open database");
    let repositories = database.repositories();
    let (book, project, _, _) = imported_entities();
    repositories.projects.insert_book(&book).await.unwrap();
    repositories
        .projects
        .insert_project(&project)
        .await
        .unwrap();

    let first_job = job(project.id, None);
    let second_job = job(project.id, None);
    repositories.jobs.insert(&first_job).await.unwrap();
    repositories.jobs.insert(&second_job).await.unwrap();
    let first = proofing_segment(project.id, "opening-credit");
    repositories
        .proofing
        .replace_plan(
            &proofing_plan(project.id, first_job.id, 1),
            std::slice::from_ref(&first),
        )
        .await
        .unwrap();
    let replacement = proofing_segment(project.id, "opening-credit");
    repositories
        .proofing
        .replace_plan(
            &proofing_plan(project.id, second_job.id, 2),
            std::slice::from_ref(&replacement),
        )
        .await
        .unwrap();

    let retired = repositories
        .proofing
        .get_segment(first.id)
        .await
        .unwrap()
        .expect("retired segment remains auditable");
    assert!(!retired.active);
    let retired_payload: ProductionSegment = serde_json::from_str(
        &sqlx::query_scalar::<_, String>("SELECT payload FROM production_segments WHERE id = ?")
            .bind(first.id.to_string())
            .fetch_one(database.pool())
            .await
            .unwrap(),
    )
    .unwrap();
    assert!(!retired_payload.active);
    assert_eq!(
        repositories
            .proofing
            .list_active_segments(project.id, None)
            .await
            .unwrap(),
        vec![replacement]
    );
}

#[tokio::test]
async fn job_graph_insert_rolls_back_every_row_when_a_dependency_insert_fails() {
    let root = TempDir::new().expect("temp dir");
    let database = Database::open_in(root.path()).await.expect("open database");
    let repositories = database.repositories();
    let project_id = insert_test_project(&repositories).await;
    let queued = job(project_id, None);
    let dependency = job_unit(queued.id, Vec::new());
    let dependent = job_unit(queued.id, vec![dependency.id]);

    assert!(
        repositories
            .proofing
            .insert_job_graph(&queued, &[dependent, dependency], None)
            .await
            .is_err()
    );
    assert_eq!(repositories.jobs.get(queued.id).await.unwrap(), None);
    let stored_units =
        sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM job_units WHERE job_id = ?")
            .bind(queued.id.to_string())
            .fetch_one(database.pool())
            .await
            .unwrap();
    assert_eq!(stored_units, 0);
}

#[tokio::test]
async fn failed_conversion_graph_preserves_the_previous_active_proofing_plan() {
    let root = TempDir::new().expect("temp dir");
    let database = Database::open_in(root.path()).await.expect("open database");
    let repositories = database.repositories();
    let project_id = insert_test_project(&repositories).await;
    let first_job = job(project_id, None);
    let second_job = job(project_id, None);
    repositories.jobs.insert(&first_job).await.unwrap();
    repositories.jobs.insert(&second_job).await.unwrap();
    let first_plan = proofing_plan(project_id, first_job.id, 1);
    let first_segment = proofing_segment(project_id, "opening-credit");
    repositories
        .proofing
        .replace_plan(&first_plan, std::slice::from_ref(&first_segment))
        .await
        .unwrap();

    let second_plan = proofing_plan(project_id, second_job.id, 2);
    let second_segment = proofing_segment(project_id, "opening-credit");
    let mut dependency = job_unit(second_job.id, Vec::new());
    dependency.segment_id = Some(second_segment.id);
    let mut dependent = job_unit(second_job.id, vec![dependency.id]);
    dependent.segment_id = Some(second_segment.id);
    assert!(
        repositories
            .proofing
            .replace_plan_with_units(
                &second_plan,
                std::slice::from_ref(&second_segment),
                &[dependent.clone(), dependency.clone()],
            )
            .await
            .is_err()
    );

    assert_eq!(
        repositories.proofing.get_plan(project_id).await.unwrap(),
        Some(first_plan)
    );
    assert!(
        repositories
            .proofing
            .get_segment(first_segment.id)
            .await
            .unwrap()
            .expect("prior segment")
            .active
    );
    assert_eq!(
        repositories
            .proofing
            .get_segment(second_segment.id)
            .await
            .unwrap(),
        None
    );
    let stored_units =
        sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM job_units WHERE job_id = ?")
            .bind(second_job.id.to_string())
            .fetch_one(database.pool())
            .await
            .unwrap();
    assert_eq!(stored_units, 0);

    repositories
        .proofing
        .replace_plan_with_units(
            &second_plan,
            std::slice::from_ref(&second_segment),
            &[dependency, dependent],
        )
        .await
        .expect("correctly ordered graph commits");
    assert!(
        !repositories
            .proofing
            .get_segment(first_segment.id)
            .await
            .unwrap()
            .expect("retired segment")
            .active
    );
    assert!(
        repositories
            .proofing
            .get_segment(second_segment.id)
            .await
            .unwrap()
            .expect("new segment")
            .active
    );
}

#[tokio::test]
async fn proof_export_job_graph_requires_its_audit_snapshot() {
    let root = TempDir::new().expect("temp dir");
    let database = Database::open_in(root.path()).await.expect("open database");
    let repositories = database.repositories();
    let project_id = insert_test_project(&repositories).await;
    let mut export = job(project_id, None);
    export.kind = JobKind::Export;
    export.export_profile_id = Some(ExportProfileId::new());
    let unit = job_unit(export.id, Vec::new());

    assert!(matches!(
        repositories
            .proofing
            .insert_job_graph(&export, &[unit], None)
            .await,
        Err(StorageError::InvalidData(message)) if message.contains("audit snapshot")
    ));
    assert_eq!(repositories.jobs.get(export.id).await.unwrap(), None);
}

#[tokio::test]
async fn budgets_reserve_atomically_and_reconcile_actual_usage() {
    let root = TempDir::new().expect("temp dir");
    let database = Database::open_in(root.path()).await.expect("open database");
    let repositories = database.repositories();
    let (book, project, _, _) = imported_entities();
    repositories.projects.insert_book(&book).await.unwrap();
    repositories
        .projects
        .insert_project(&project)
        .await
        .unwrap();
    let now = Utc::now();
    let provider = provider(now);
    repositories.providers.upsert(&provider).await.unwrap();

    let budget = Budget {
        id: BudgetId::new(),
        name: "ElevenLabs credits".into(),
        scope: BudgetScope {
            kind: BudgetScopeKind::Provider,
            provider_profile_id: Some(provider.id),
        },
        period: BudgetPeriod::Monthly,
        metric: BudgetMetric::ProviderCredits,
        currency: None,
        limit: 100,
        used: 20,
        warning_threshold_percent: 80,
        hard: true,
        enabled: true,
        period_started_at: now,
        period_ends_at: Some(now + Duration::days(30)),
        created_at: now,
        updated_at: now,
    };
    repositories.budgets.upsert(&budget).await.unwrap();

    let reservation_id = ReservationId::new();
    let first_job = job(project.id, Some(reservation_id));
    repositories.jobs.insert(&first_job).await.unwrap();
    let reservation = BudgetReservation {
        id: reservation_id,
        job_id: first_job.id,
        status: ReservationStatus::Active,
        allocations: vec![BudgetAllocation {
            budget_id: budget.id,
            reserved_amount: 60,
            actual_amount: None,
        }],
        created_at: now,
        expires_at: Some(now + Duration::hours(1)),
        reconciled_at: None,
    };
    repositories.budgets.reserve(&reservation).await.unwrap();

    let second_reservation_id = ReservationId::new();
    let second_job = job(project.id, Some(second_reservation_id));
    repositories.jobs.insert(&second_job).await.unwrap();
    let over_limit = BudgetReservation {
        id: second_reservation_id,
        job_id: second_job.id,
        status: ReservationStatus::Active,
        allocations: vec![BudgetAllocation {
            budget_id: budget.id,
            reserved_amount: 30,
            actual_amount: None,
        }],
        created_at: now,
        expires_at: Some(now + Duration::hours(1)),
        reconciled_at: None,
    };
    assert!(matches!(
        repositories.budgets.reserve(&over_limit).await,
        Err(StorageError::BudgetExceeded { .. })
    ));

    let reconciled = repositories
        .budgets
        .reconcile(
            reservation.id,
            &BTreeMap::from([(budget.id, 50)]),
            Utc::now(),
        )
        .await
        .unwrap();
    assert_eq!(reconciled.status, ReservationStatus::Reconciled);
    assert_eq!(
        repositories
            .budgets
            .get(budget.id)
            .await
            .unwrap()
            .unwrap()
            .used,
        70
    );
}

#[tokio::test]
async fn daily_budget_rolls_at_utc_midnight_and_late_usage_stays_in_its_window() {
    let root = TempDir::new().expect("temp dir");
    let database = Database::open_in(root.path()).await.expect("open database");
    let repositories = database.repositories();
    let project_id = insert_test_project(&repositories).await;

    let before_midnight = utc(2026, 8, 4, 23, 50);
    let after_midnight = utc(2026, 8, 5, 0, 1);
    let budget = global_budget(BudgetPeriod::Daily, 100, 70, before_midnight);
    repositories.budgets.upsert(&budget).await.unwrap();
    let normalized = budget_at(&repositories, budget.id, before_midnight).await;
    assert_budget_window(
        &normalized,
        70,
        utc(2026, 8, 4, 0, 0),
        utc(2026, 8, 5, 0, 0),
    );

    let old_reservation_id = ReservationId::new();
    let old_job_id = insert_budget_job(&repositories, project_id, old_reservation_id).await;
    let old_reservation = reservation(
        old_reservation_id,
        old_job_id,
        budget.id,
        20,
        before_midnight,
        Some(before_midnight + Duration::days(2)),
    );
    repositories
        .budgets
        .reserve(&old_reservation)
        .await
        .unwrap();
    assert_eq!(
        reserved_at(&repositories, budget.id, before_midnight).await,
        20
    );

    let rolled = budget_at(&repositories, budget.id, after_midnight).await;
    assert_budget_window(&rolled, 0, utc(2026, 8, 5, 0, 0), utc(2026, 8, 6, 0, 0));
    assert_eq!(
        reserved_at(&repositories, budget.id, after_midnight).await,
        0,
        "an unexpired reservation from yesterday must not consume today's capacity"
    );

    let new_reservation_id = ReservationId::new();
    let new_job_id = insert_budget_job(&repositories, project_id, new_reservation_id).await;
    repositories
        .budgets
        .reserve(&reservation(
            new_reservation_id,
            new_job_id,
            budget.id,
            100,
            after_midnight,
            Some(after_midnight + Duration::hours(12)),
        ))
        .await
        .unwrap();

    repositories
        .budgets
        .reconcile(
            old_reservation_id,
            &BTreeMap::from([(budget.id, 15)]),
            after_midnight,
        )
        .await
        .unwrap();
    assert_eq!(
        budget_at(&repositories, budget.id, after_midnight)
            .await
            .used,
        0,
        "late reconciliation must not charge the new UTC day"
    );
    let prior_day_used: i64 = sqlx::query_scalar(
        "SELECT used_value FROM budget_period_usage \
         WHERE budget_id = ? AND period_started_at = ?",
    )
    .bind(budget.id.to_string())
    .bind(utc(2026, 8, 4, 0, 0).to_rfc3339())
    .fetch_one(database.pool())
    .await
    .unwrap();
    assert_eq!(prior_day_used, 85);
    assert_eq!(
        reserved_at(&repositories, budget.id, after_midnight).await,
        100
    );
}

#[tokio::test]
async fn monthly_budget_uses_utc_calendar_months_including_leap_february() {
    let root = TempDir::new().expect("temp dir");
    let database = Database::open_in(root.path()).await.expect("open database");
    let repositories = database.repositories();
    let leap_day = utc(2028, 2, 29, 12, 0);
    let budget = global_budget(BudgetPeriod::Monthly, 100, 40, leap_day);
    repositories.budgets.upsert(&budget).await.unwrap();

    let february = repositories
        .budgets
        .get_at(budget.id, leap_day)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(february.used, 40);
    assert_eq!(february.period_started_at, utc(2028, 2, 1, 0, 0));
    assert_eq!(february.period_ends_at, Some(utc(2028, 3, 1, 0, 0)));

    let march = repositories
        .budgets
        .get_at(budget.id, utc(2028, 3, 1, 0, 0))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(march.used, 0);
    assert_eq!(march.period_started_at, utc(2028, 3, 1, 0, 0));
    assert_eq!(march.period_ends_at, Some(utc(2028, 4, 1, 0, 0)));
}

#[tokio::test]
async fn lifetime_budget_remains_cumulative_across_dates() {
    let root = TempDir::new().expect("temp dir");
    let database = Database::open_in(root.path()).await.expect("open database");
    let repositories = database.repositories();
    let project_id = insert_test_project(&repositories).await;
    let now = utc(2026, 8, 4, 10, 0);
    let budget = global_budget(BudgetPeriod::Lifetime, 100, 20, now);
    repositories.budgets.upsert(&budget).await.unwrap();

    let first_id = ReservationId::new();
    let first_job = insert_budget_job(&repositories, project_id, first_id).await;
    repositories
        .budgets
        .reserve(&reservation(first_id, first_job, budget.id, 60, now, None))
        .await
        .unwrap();
    let rejected_id = ReservationId::new();
    let rejected_job = insert_budget_job(&repositories, project_id, rejected_id).await;
    assert!(matches!(
        repositories
            .budgets
            .reserve(&reservation(
                rejected_id,
                rejected_job,
                budget.id,
                30,
                now,
                None,
            ))
            .await,
        Err(StorageError::BudgetExceeded { remaining: 20, .. })
    ));

    repositories
        .budgets
        .reconcile(first_id, &BTreeMap::from([(budget.id, 50)]), now)
        .await
        .unwrap();
    let years_later = utc(2031, 1, 1, 0, 0);
    let cumulative = repositories
        .budgets
        .get_at(budget.id, years_later)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(cumulative.used, 70);
    assert_eq!(cumulative.period_ends_at, None);

    let final_id = ReservationId::new();
    let final_job = insert_budget_job(&repositories, project_id, final_id).await;
    repositories
        .budgets
        .reserve(&reservation(
            final_id,
            final_job,
            budget.id,
            30,
            years_later,
            None,
        ))
        .await
        .unwrap();
}

#[tokio::test]
async fn per_job_budget_is_independent_for_each_concurrent_and_prior_job() {
    let root = TempDir::new().expect("temp dir");
    let database = Database::open_in(root.path()).await.expect("open database");
    let repositories = database.repositories();
    let project_id = insert_test_project(&repositories).await;
    let now = utc(2026, 8, 4, 10, 0);
    let budget = global_budget(BudgetPeriod::Job, 100, 80, now);
    repositories.budgets.upsert(&budget).await.unwrap();
    assert_eq!(
        repositories
            .budgets
            .get_at(budget.id, now)
            .await
            .unwrap()
            .unwrap()
            .used,
        0,
        "a per-job budget has no cross-job aggregate usage"
    );

    let first_id = ReservationId::new();
    let first_job = insert_budget_job(&repositories, project_id, first_id).await;
    repositories
        .budgets
        .reserve(&reservation(first_id, first_job, budget.id, 90, now, None))
        .await
        .unwrap();
    let second_id = ReservationId::new();
    let second_job = insert_budget_job(&repositories, project_id, second_id).await;
    repositories
        .budgets
        .reserve(&reservation(
            second_id, second_job, budget.id, 90, now, None,
        ))
        .await
        .unwrap();
    assert_eq!(
        repositories
            .budgets
            .active_reserved_at(budget.id, now)
            .await
            .unwrap(),
        0
    );

    repositories
        .budgets
        .reconcile(first_id, &BTreeMap::from([(budget.id, 95)]), now)
        .await
        .unwrap();
    assert_eq!(
        repositories
            .budgets
            .get_at(budget.id, now)
            .await
            .unwrap()
            .unwrap()
            .used,
        0
    );
    let first_actual: i64 = sqlx::query_scalar(
        "SELECT actual_amount FROM budget_allocations \
         WHERE reservation_id = ? AND budget_id = ?",
    )
    .bind(first_id.to_string())
    .bind(budget.id.to_string())
    .fetch_one(database.pool())
    .await
    .unwrap();
    assert_eq!(first_actual, 95);

    let third_id = ReservationId::new();
    let third_job = insert_budget_job(&repositories, project_id, third_id).await;
    repositories
        .budgets
        .reserve(&reservation(third_id, third_job, budget.id, 100, now, None))
        .await
        .unwrap();
    let over_id = ReservationId::new();
    let over_job = insert_budget_job(&repositories, project_id, over_id).await;
    assert!(matches!(
        repositories
            .budgets
            .reserve(&reservation(over_id, over_job, budget.id, 101, now, None,))
            .await,
        Err(StorageError::BudgetExceeded { remaining: 100, .. })
    ));
}

#[tokio::test]
#[allow(clippy::too_many_lines)]
async fn manual_retry_uses_fresh_history_and_prior_job_usage_reduces_capacity() {
    let root = TempDir::new().expect("temp dir");
    let database = Database::open_in(root.path()).await.expect("open database");
    let repositories = database.repositories();
    let project_id = insert_test_project(&repositories).await;
    let now = utc(2026, 8, 5, 10, 0);
    let budget = global_budget(BudgetPeriod::Job, 100, 0, now);
    repositories.budgets.upsert(&budget).await.unwrap();
    let provider = provider(now);
    repositories.providers.upsert(&provider).await.unwrap();

    let old_id = ReservationId::new();
    let mut failed = job(project_id, Some(old_id));
    failed.state = JobState::Failed;
    failed.finished_at = Some(now);
    failed.updated_at = now;
    repositories.jobs.insert(&failed).await.unwrap();
    repositories
        .budgets
        .reserve(&reservation(
            old_id,
            failed.id,
            budget.id,
            80,
            now,
            Some(now + Duration::minutes(1)),
        ))
        .await
        .unwrap();

    let competing_id = ReservationId::new();
    assert!(matches!(
        repositories
            .budgets
            .reserve(&reservation(
                competing_id,
                failed.id,
                budget.id,
                1,
                now,
                None,
            ))
            .await,
        Err(StorageError::Conflict {
            entity: "active budget reservation",
            ..
        })
    ));
    repositories
        .budgets
        .reconcile(old_id, &BTreeMap::from([(budget.id, 40)]), now)
        .await
        .unwrap();
    let prior_usage_sequence = repositories
        .usage
        .append(&usage_event(project_id, failed.id, provider.id, 40, now))
        .await
        .unwrap();

    let rejected_id = ReservationId::new();
    let mut queued = failed.clone();
    queued.reservation_id = Some(rejected_id);
    queued.transition(JobState::Queued, now).unwrap();
    queued.finished_at = None;
    let rejected = reservation(
        rejected_id,
        failed.id,
        budget.id,
        61,
        now,
        Some(now + Duration::minutes(1)),
    );
    assert!(matches!(
        repositories
            .jobs
            .update_with_retry_admission(&queued, 0, Some(&rejected), None)
            .await,
        Err(StorageError::BudgetExceeded { remaining: 60, .. })
    ));
    assert_eq!(
        repositories
            .jobs
            .get(failed.id)
            .await
            .unwrap()
            .unwrap()
            .state,
        JobState::Failed,
        "a rejected retry must remain non-runnable"
    );
    assert!(
        repositories
            .budgets
            .get_reservation(rejected_id)
            .await
            .unwrap()
            .is_none(),
        "the failed atomic admission must not leave a reservation"
    );

    let retry_id = ReservationId::new();
    queued.reservation_id = Some(retry_id);
    let retry = reservation(
        retry_id,
        failed.id,
        budget.id,
        60,
        now,
        Some(now + Duration::minutes(1)),
    );
    let admitted = repositories
        .jobs
        .update_with_retry_admission(&queued, 0, Some(&retry), None)
        .await
        .expect("remaining per-job capacity admits retry atomically");
    assert_eq!(admitted.state, JobState::Queued);
    assert_eq!(admitted.reservation_id, Some(retry_id));
    assert_eq!(
        repositories
            .budgets
            .usage_sequence_start(retry_id)
            .await
            .unwrap(),
        prior_usage_sequence,
        "the fresh cycle starts after all prior-cycle ledger rows"
    );
    let history_count =
        sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM budget_reservations WHERE job_id = ?")
            .bind(failed.id.to_string())
            .fetch_one(database.pool())
            .await
            .unwrap();
    assert_eq!(history_count, 2, "retry cycles remain append-only history");

    repositories
        .budgets
        .get_at(budget.id, now + Duration::minutes(2))
        .await
        .unwrap();
    assert_eq!(
        repositories
            .budgets
            .get_reservation(retry_id)
            .await
            .unwrap()
            .unwrap()
            .status,
        ReservationStatus::Expired
    );
    let reconciled = repositories
        .budgets
        .reconcile(
            retry_id,
            &BTreeMap::from([(budget.id, 10)]),
            now + Duration::minutes(2),
        )
        .await
        .expect("expired retry usage remains reconcilable");
    assert_eq!(reconciled.status, ReservationStatus::Reconciled);
}

#[tokio::test]
async fn concurrent_hard_budget_reservations_cannot_oversubscribe() {
    let root = TempDir::new().expect("temp dir");
    let database = Database::open_in(root.path()).await.expect("open database");
    let repositories = database.repositories();
    let project_id = insert_test_project(&repositories).await;
    let now = utc(2026, 8, 4, 10, 0);
    let budget = global_budget(BudgetPeriod::Daily, 100, 0, now);
    repositories.budgets.upsert(&budget).await.unwrap();

    let first_id = ReservationId::new();
    let first_job = insert_budget_job(&repositories, project_id, first_id).await;
    let second_id = ReservationId::new();
    let second_job = insert_budget_job(&repositories, project_id, second_id).await;
    let first = reservation(first_id, first_job, budget.id, 60, now, None);
    let second = reservation(second_id, second_job, budget.id, 60, now, None);
    let first_repository = database.repositories().budgets;
    let second_repository = database.repositories().budgets;
    let (first_result, second_result) = tokio::join!(
        first_repository.reserve(&first),
        second_repository.reserve(&second)
    );
    assert_eq!(
        usize::from(first_result.is_ok()) + usize::from(second_result.is_ok()),
        1
    );
    let failure = first_result.err().or_else(|| second_result.err()).unwrap();
    assert!(matches!(failure, StorageError::BudgetExceeded { .. }));
    assert_eq!(
        repositories
            .budgets
            .active_reserved_at(budget.id, now)
            .await
            .unwrap(),
        60
    );
}

#[tokio::test]
async fn usage_is_append_only_and_preserves_unknown_quantities() {
    let root = TempDir::new().expect("temp dir");
    let database = Database::open_in(root.path()).await.expect("open database");
    let repositories = database.repositories();
    let (book, project, _, _) = imported_entities();
    repositories.projects.insert_book(&book).await.unwrap();
    repositories
        .projects
        .insert_project(&project)
        .await
        .unwrap();
    let provider = provider(Utc::now());
    repositories.providers.upsert(&provider).await.unwrap();
    let event = UsageEvent {
        id: UsageEventId::new(),
        occurred_at: Utc::now(),
        workload: UsageWorkload::Tts,
        project_id: project.id,
        job_id: None,
        attempt_id: None,
        chapter_id: None,
        segment_id: None,
        provider_profile_id: provider.id,
        provider_family: "local_ai".into(),
        endpoint_family: "openai_compatible".into(),
        model: Some("tts-1".into()),
        voice_profile_id: None,
        provider_request_id: None,
        quantities: UsageQuantities {
            characters: Some(11),
            input_tokens: None,
            ..UsageQuantities::default()
        },
        quantity_source: ProvenanceQuality::Reported,
        cost: Some(Money {
            micros: 500,
            currency: "EUR".into(),
        }),
        cost_source: ProvenanceQuality::Estimated,
        rate_card_id: None,
        uncertain_charge: false,
        redacted_raw_usage: BTreeMap::new(),
    };
    repositories.usage.append(&event).await.unwrap();
    let totals = repositories
        .usage
        .totals(&UsageFilter {
            project_id: Some(project.id),
            ..UsageFilter::default()
        })
        .await
        .unwrap();
    assert_eq!(totals.quantities.characters, Some(11));
    assert_eq!(totals.quantities.input_tokens, None);
    assert_eq!(totals.cost_by_currency_micros["EUR"], 500);

    let update = sqlx::query("UPDATE usage_ledger SET characters = 99 WHERE id = ?")
        .bind(event.id.to_string())
        .execute(database.pool())
        .await;
    assert!(update.is_err());
}

#[tokio::test]
async fn idempotency_replays_only_matching_requests() {
    let root = TempDir::new().expect("temp dir");
    let database = Database::open_in(root.path()).await.expect("open database");
    let repository = database.repositories().idempotency;
    let now = Utc::now();
    let expires = now + Duration::minutes(10);
    assert_eq!(
        repository
            .claim("projects", "key-1", "hash-a", now, expires)
            .await
            .unwrap(),
        IdempotencyClaim::Acquired
    );
    let response = IdempotentResponse {
        status: 201,
        body: br#"{"id":"example"}"#.to_vec(),
        content_type: "application/json".into(),
    };
    repository
        .complete("projects", "key-1", "hash-a", &response)
        .await
        .unwrap();
    assert_eq!(
        repository
            .claim("projects", "key-1", "hash-a", now, expires)
            .await
            .unwrap(),
        IdempotencyClaim::Replay(response)
    );
    assert!(matches!(
        repository
            .claim("projects", "key-1", "hash-b", now, expires)
            .await,
        Err(StorageError::IdempotencyMismatch)
    ));
}
