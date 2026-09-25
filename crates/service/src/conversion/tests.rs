// The tests exercise private stage helpers across the whole production pipeline.
#![allow(clippy::wildcard_imports)]

use super::{
    actions::*, export::*, export_profile::*, media_files::*, planning::*, playback::*,
    provider_io::*, recovery::*, synthesis::*, units::*, worker::*, *,
};

#[test]
fn punctuation_only_ranges_are_not_speakable() {
    assert!(!is_speakable("\""));
    assert!(!is_speakable(" \u{201e} \u{2014} … "));
    assert!(is_speakable("Ja!"));
    assert!(is_speakable("1."));
    assert!(is_speakable("\u{00e4}"));
}

fn test_segment_plan() -> SegmentPlan {
    SegmentPlan {
        id: SegmentId::new(),
        proofing: true,
        key: "stable-segment".to_owned(),
        chapter_id: Uuid::from_u128(1),
        paragraph_id: Uuid::from_u128(2),
        source_content_hash: "source".to_owned(),
        byte_start: 0,
        byte_end: 5,
        chapter_title: "Chapter".to_owned(),
        segment_ordinal: 0,
        playback_ordinal: 0,
        original_text: "Hello".to_owned(),
        text: "Hello".to_owned(),
        context: None,
        assignment: SpeakerAssignment {
            character_id: Uuid::from_u128(3),
            character_name: "Narrator".to_owned(),
            provider_id: Uuid::from_u128(4),
            provider_name: "Provider".to_owned(),
            provider_kind: ProviderKindView::Elevenlabs,
            provider_role: Some(crate::models::ProviderRoleView::Tts),
            provider_mode: Some(ProviderModeView::CloudRemote),
            provider_endpoint: None,
            provider_snapshot_id: Some(Uuid::from_u128(6)),
            provider_version: Some("1".to_owned()),
            provider_concurrency: 1,
            voice_id: Uuid::from_u128(5),
            voice_source: "voice".to_owned(),
            voice_name: "Voice".to_owned(),
            model: Some("eleven_multilingual_v2".to_owned()),
            performance: PerformanceSettings {
                speed: Some(1.0),
                ..PerformanceSettings::default()
            },
            timing: TimingSettings::default(),
        },
        applied_rule_ids: Vec::new(),
        dictionary_revision: "dictionary".to_owned(),
    }
}

async fn filesystem_test_state() -> (tempfile::TempDir, Arc<AppState>) {
    let directory = tempfile::tempdir().expect("temporary directory");
    let database = audiobookai_storage::Database::open_in(directory.path())
        .await
        .expect("database");
    let state = Arc::new(
        AppState::new(
            crate::ServiceConfig {
                bind: "127.0.0.1:0".parse().expect("address"),
                data_dir: directory.path().to_path_buf(),
                bundled_sidecar_dir: None,
                tls: None,
                lan_hostnames: Vec::new(),
                allow_insecure_lan: false,
                desktop_bootstrap: false,
            },
            database,
        )
        .await
        .expect("application state"),
    );
    (directory, state)
}

async fn insert_recovery_project(state: &AppState, name: &str) -> ProjectId {
    let now = Utc::now();
    let book_id = audiobookai_core::BookId::new();
    let project_id = ProjectId::new();
    sqlx::query(
        "INSERT INTO books (id, managed_epub_path, source_hash, imported_at, payload) \
             VALUES (?, ?, ?, ?, '{}')",
    )
    .bind(book_id.to_string())
    .bind(format!("/fixtures/{book_id}.epub"))
    .bind(format!("fixture-{book_id}"))
    .bind(now.to_rfc3339())
    .execute(state.database.pool())
    .await
    .expect("recovery book fixture");
    sqlx::query(
        "INSERT INTO projects \
             (id, book_id, name, status, created_at, updated_at, revision, payload) \
             VALUES (?, ?, ?, 'draft', ?, ?, 0, '{}')",
    )
    .bind(project_id.to_string())
    .bind(book_id.to_string())
    .bind(name)
    .bind(now.to_rfc3339())
    .bind(now.to_rfc3339())
    .execute(state.database.pool())
    .await
    .expect("recovery project fixture");
    project_id
}

fn recovered_job(
    id: u128,
    project_id: ProjectId,
    kind: JobKind,
    state: JobState,
    created_at: chrono::DateTime<Utc>,
) -> Job {
    Job {
        id: JobId::from_uuid(Uuid::from_u128(id)),
        project_id,
        kind,
        state,
        export_profile_id: None,
        reservation_id: None,
        progress_completed: 0,
        progress_total: 1,
        status_message: Some("legacy active job".to_owned()),
        allow_budget_override: false,
        created_at,
        started_at: (state != JobState::Queued).then_some(created_at),
        finished_at: None,
        updated_at: created_at,
        revision: 0,
    }
}

async fn insert_recovery_export_profile(
    state: &AppState,
    project_id: ProjectId,
    output_directory: &Path,
) -> ExportProfile {
    let now = Utc::now();
    let output_directory = tokio::fs::canonicalize(output_directory)
        .await
        .expect("canonical recovery output directory");
    let profile = ExportProfile {
        id: ExportProfileId::new(),
        project_id,
        name: "Recovery M4B".to_owned(),
        format: ExportFormat::M4b,
        layout: ExportLayout::SingleFile,
        output_directory: output_directory.to_string_lossy().into_owned(),
        filename_template: "legacy-book".to_owned(),
        audio: audiobookai_core::AudioEncodingSettings::default(),
        background_music: None,
        embed_cover: true,
        embed_chapters: true,
        write_sidecar_manifest: true,
        created_at: now,
        updated_at: now,
    };
    sqlx::query(
        "INSERT INTO export_profiles (id, project_id, name, format, layout, updated_at, payload) \
             VALUES (?, ?, ?, 'm4b', 'single_file', ?, ?)",
    )
    .bind(profile.id.to_string())
    .bind(project_id.to_string())
    .bind(&profile.name)
    .bind(now.to_rfc3339())
    .bind(serde_json::to_string(&profile).unwrap())
    .execute(state.database.pool())
    .await
    .expect("recovery export profile");
    profile
}

#[test]
fn recovered_project_conflicts_choose_one_deterministic_production_survivor() {
    let project_id = ProjectId::new();
    let other_project_id = ProjectId::new();
    let now = Utc::now();
    let earlier = now - chrono::Duration::seconds(10);
    let preview = recovered_job(1, project_id, JobKind::Preview, JobState::Running, earlier);
    let deterministic_survivor = recovered_job(
        2,
        project_id,
        JobKind::CharacterDetection,
        JobState::Paused,
        now,
    );
    let same_timestamp = recovered_job(3, project_id, JobKind::Conversion, JobState::Running, now);
    let newer = recovered_job(
        4,
        project_id,
        JobKind::Export,
        JobState::Queued,
        now + chrono::Duration::seconds(1),
    );
    let unrelated = recovered_job(
        5,
        other_project_id,
        JobKind::SegmentRegeneration,
        JobState::Running,
        now,
    );

    let conflicts = recovered_production_conflicts(&[
        preview,
        deterministic_survivor.clone(),
        same_timestamp.clone(),
        newer.clone(),
        unrelated,
    ]);

    assert_eq!(conflicts, BTreeSet::from([same_timestamp.id, newer.id]));
    assert!(!conflicts.contains(&deterministic_survivor.id));
}

#[test]
fn worker_handoff_never_drops_an_accepted_retry_at_release_boundary() {
    // Conversion and proof export share the conversion worker; regeneration uses the same
    // ownership registry with its own runner. Exercise each scheduling class independently.
    for job_id in [JobId::new(), JobId::new(), JobId::new()] {
        assert!(
            request_production_worker(job_id, false),
            "the original worker owns the first start"
        );
        assert!(
            !request_production_worker(job_id, true),
            "a retry while cleanup is active is handed to the owner"
        );
        assert!(
            finish_production_worker_iteration(job_id),
            "the owner must consume the handed-off retry"
        );
        assert!(
            !finish_production_worker_iteration(job_id),
            "the owner releases itself after the retry iteration"
        );

        // If release wins the mutex race, the retry request becomes a fresh owner instead.
        assert!(
            request_production_worker(job_id, true),
            "a retry after release must create a new owner"
        );
        assert!(!finish_production_worker_iteration(job_id));
    }

    let paused_job_id = JobId::new();
    assert!(request_production_worker(paused_job_id, false));
    assert!(
        !request_production_worker(paused_job_id, false),
        "resume wakes an existing paused owner without requesting a second iteration"
    );
    assert!(
        !finish_production_worker_iteration(paused_job_id),
        "a successful resumed owner must be released without rerunning a completed job"
    );
}

#[tokio::test]
async fn startup_terminalizes_legacy_project_conflicts_before_resuming_survivor() {
    let (directory, state) = filesystem_test_state().await;
    let project_id = insert_recovery_project(&state, "Conflicting recovery").await;
    let output_directory = directory.path().join("survivor-exports");
    tokio::fs::create_dir_all(&output_directory).await.unwrap();
    let profile = insert_recovery_export_profile(&state, project_id, &output_directory).await;
    let now = Utc::now();
    let mut survivor = recovered_job(
        10,
        project_id,
        JobKind::Conversion,
        JobState::Paused,
        now - chrono::Duration::seconds(10),
    );
    survivor.export_profile_id = Some(profile.id);
    let conflicting_export = recovered_job(11, project_id, JobKind::Export, JobState::Queued, now);
    let conflicting_detection = recovered_job(
        12,
        project_id,
        JobKind::CharacterDetection,
        JobState::Queued,
        now + chrono::Duration::seconds(1),
    );
    let jobs = state.database.repositories().jobs;
    jobs.insert(&survivor).await.expect("survivor fixture");
    jobs.insert(&conflicting_export)
        .await
        .expect("export conflict fixture");
    jobs.insert(&conflicting_detection)
        .await
        .expect("detection conflict fixture");

    resume_durable_conversions(Arc::clone(&state))
        .await
        .expect("conflicts recovered");

    assert_eq!(
        jobs.get(survivor.id).await.unwrap().unwrap().state,
        JobState::Paused
    );
    let failed_export = jobs.get(conflicting_export.id).await.unwrap().unwrap();
    assert_eq!(failed_export.state, JobState::Failed);
    assert_eq!(
        failed_export.status_message.as_deref(),
        Some(RECOVERED_PRODUCTION_CONFLICT)
    );
    assert!(
        jobs.get(conflicting_detection.id)
            .await
            .unwrap()
            .unwrap()
            .state
            .is_terminal()
    );
    assert_eq!(
        jobs.list_active()
            .await
            .unwrap()
            .into_iter()
            .map(|job| job.id)
            .collect::<Vec<_>>(),
        vec![survivor.id]
    );
}

#[tokio::test]
async fn startup_acquires_legacy_export_destination_before_resume() {
    let (directory, state) = filesystem_test_state().await;
    let project_id = insert_recovery_project(&state, "Legacy output recovery").await;
    let output_directory = directory.path().join("exports");
    tokio::fs::create_dir_all(&output_directory).await.unwrap();
    let profile = insert_recovery_export_profile(&state, project_id, &output_directory).await;
    let mut legacy = recovered_job(
        13,
        project_id,
        JobKind::Conversion,
        JobState::Paused,
        Utc::now(),
    );
    legacy.export_profile_id = Some(profile.id);
    let jobs = state.database.repositories().jobs;
    jobs.insert(&legacy)
        .await
        .expect("legacy job without claim");

    resume_durable_conversions(Arc::clone(&state))
        .await
        .expect("legacy output claim recovered");

    let reservation = jobs
        .get_output_reservation(legacy.id)
        .await
        .unwrap()
        .expect("recovered output claim");
    assert_eq!(reservation.state, OutputReservationState::Reserved);
    assert_eq!(
        Path::new(&reservation.destination_path),
        Path::new(&profile.output_directory).join("legacy-book.m4b")
    );
    assert_eq!(
        jobs.get(legacy.id).await.unwrap().unwrap().state,
        JobState::Paused
    );
}

#[tokio::test]
async fn startup_fails_legacy_export_instead_of_adopting_existing_output() {
    let (directory, state) = filesystem_test_state().await;
    let project_id = insert_recovery_project(&state, "Legacy output conflict").await;
    let output_directory = directory.path().join("exports");
    tokio::fs::create_dir_all(&output_directory).await.unwrap();
    let profile = insert_recovery_export_profile(&state, project_id, &output_directory).await;
    let destination = output_directory.join("legacy-book.m4b");
    tokio::fs::write(&destination, b"foreign output")
        .await
        .unwrap();
    let mut legacy = recovered_job(
        15,
        project_id,
        JobKind::Export,
        JobState::Paused,
        Utc::now(),
    );
    legacy.export_profile_id = Some(profile.id);
    let jobs = state.database.repositories().jobs;
    jobs.insert(&legacy)
        .await
        .expect("legacy job without claim");

    resume_durable_conversions(Arc::clone(&state))
        .await
        .expect("legacy conflict terminalized");

    let recovered = jobs.get(legacy.id).await.unwrap().unwrap();
    assert_eq!(recovered.state, JobState::Failed);
    assert!(
        recovered
            .status_message
            .as_deref()
            .is_some_and(|message| message.contains("already exists"))
    );
    assert_eq!(jobs.get_output_reservation(legacy.id).await.unwrap(), None);
    assert_eq!(
        tokio::fs::read(destination).await.unwrap(),
        b"foreign output"
    );
}

#[tokio::test]
async fn reserved_destination_is_rejected_before_becoming_an_output_root() {
    let (directory, state) = filesystem_test_state().await;
    let project_id = insert_recovery_project(&state, "Hierarchy ownership").await;
    let export_root = directory.path().join("exports");
    tokio::fs::create_dir_all(&export_root).await.unwrap();
    let reserved_destination = tokio::fs::canonicalize(&export_root)
        .await
        .unwrap()
        .join("book.m4b");
    let owner = recovered_job(
        14,
        project_id,
        JobKind::Conversion,
        JobState::Paused,
        Utc::now(),
    );
    let now = Utc::now();
    let reservation = OutputDestinationReservation {
        job_id: owner.id,
        project_id,
        destination_key: output_destination_key(&reserved_destination),
        destination_path: reserved_destination.to_string_lossy().into_owned(),
        layout: ExportLayout::SingleFile,
        state: OutputReservationState::Reserved,
        created_at: now,
        updated_at: now,
        promoted_at: None,
    };
    state
        .database
        .repositories()
        .jobs
        .insert_with_output_reservation(&owner, &reservation)
        .await
        .unwrap();

    assert!(
        ensure_output_directory_not_reserved(&state, &reserved_destination)
            .await
            .is_err()
    );
    assert!(!reserved_destination.exists());
    let reserved_manifest = PathBuf::from(format!(
        "{}.manifest.json",
        reserved_destination.to_string_lossy()
    ));
    assert!(
        ensure_output_directory_not_reserved(&state, &reserved_manifest)
            .await
            .is_err()
    );
    assert!(!reserved_manifest.exists());
}

#[tokio::test]
async fn unresolved_paid_conflict_is_failed_without_releasing_its_reservation() {
    let (_directory, state) = filesystem_test_state().await;
    let project_id = insert_recovery_project(&state, "Uncertain recovery").await;
    let now = Utc::now();
    let survivor = recovered_job(
        20,
        project_id,
        JobKind::Conversion,
        JobState::Paused,
        now - chrono::Duration::seconds(10),
    );
    let reservation_id = ReservationId::new();
    let mut conflicting = recovered_job(
        21,
        project_id,
        JobKind::SegmentRegeneration,
        JobState::Running,
        now,
    );
    conflicting.reservation_id = Some(reservation_id);
    let unit = JobUnit {
        id: JobUnitId::new(),
        job_id: conflicting.id,
        kind: JobUnitKind::SynthesisSegment,
        state: JobUnitState::Running,
        chapter_id: None,
        segment_id: None,
        provider_profile_id: None,
        dependencies: Vec::new(),
        attempt_count: 0,
        next_attempt_at: None,
        output_artifact_id: None,
        payload: BTreeMap::new(),
        created_at: now,
        updated_at: now,
    };
    let jobs = state.database.repositories().jobs;
    jobs.insert(&survivor).await.expect("survivor fixture");
    jobs.insert(&conflicting)
        .await
        .expect("paid conflict fixture");
    jobs.upsert_unit(&unit).await.expect("paid unit fixture");
    sqlx::query(
        "INSERT INTO budget_reservations \
             (id, job_id, status, created_at, expires_at, reconciled_at, payload) \
             VALUES (?, ?, 'active', ?, NULL, NULL, '{}')",
    )
    .bind(reservation_id.to_string())
    .bind(conflicting.id.to_string())
    .bind(now.to_rfc3339())
    .execute(state.database.pool())
    .await
    .expect("legacy reservation fixture");

    resume_durable_conversions(Arc::clone(&state))
        .await
        .expect("uncertain conflict recovered");

    let recovered = jobs.get(conflicting.id).await.unwrap().unwrap();
    assert_eq!(recovered.state, JobState::Failed);
    assert!(
        recovered
            .status_message
            .as_deref()
            .is_some_and(|message| message.contains("may have been charged"))
    );
    let recovered_unit = jobs.get_unit(unit.id).await.unwrap().unwrap();
    assert_eq!(recovered_unit.state, JobUnitState::Failed);
    assert_eq!(
        recovered_unit
            .payload
            .get("uncertainUsageUnresolved")
            .and_then(serde_json::Value::as_bool),
        Some(true)
    );
    assert_eq!(
        sqlx::query_scalar::<_, String>("SELECT status FROM budget_reservations WHERE id = ?")
            .bind(reservation_id.to_string())
            .fetch_one(state.database.pool())
            .await
            .unwrap(),
        "active"
    );

    // A second startup keeps the unknown charge and its reservation durable instead of
    // retroactively releasing it as a zero-usage job.
    resume_durable_conversions(Arc::clone(&state))
        .await
        .expect("idempotent uncertain recovery");
    assert_eq!(
        sqlx::query_scalar::<_, String>("SELECT status FROM budget_reservations WHERE id = ?")
            .bind(reservation_id.to_string())
            .fetch_one(state.database.pool())
            .await
            .unwrap(),
        "active"
    );
}

#[allow(clippy::too_many_lines)]
#[tokio::test]
async fn startup_reconciles_expired_terminal_detection_cycle_usage() {
    let (_directory, state) = filesystem_test_state().await;
    let project_id = insert_recovery_project(&state, "Expired detection accounting").await;
    let now = Utc::now();
    let cycle_started_at = now - chrono::Duration::hours(2);
    let reservation_id = ReservationId::new();
    let mut job = recovered_job(
        22,
        project_id,
        JobKind::CharacterDetection,
        JobState::Failed,
        cycle_started_at - chrono::Duration::minutes(5),
    );
    job.reservation_id = Some(reservation_id);
    job.finished_at = Some(now - chrono::Duration::hours(1));
    state
        .database
        .repositories()
        .jobs
        .insert(&job)
        .await
        .expect("terminal detection fixture");

    let provider = audiobookai_core::ProviderProfile {
        id: ProviderProfileId::new(),
        name: "Detection provider".to_owned(),
        family: audiobookai_core::ProviderFamily::OpenAiCompatible,
        role: audiobookai_core::ProviderRole::CharacterDetection,
        deployment: audiobookai_core::ProviderDeployment::ExternalEndpoint,
        endpoint: Some("http://127.0.0.1:1234".to_owned()),
        executable_path: None,
        working_directory: None,
        arguments: Vec::new(),
        environment_secret_ids: BTreeMap::new(),
        credential_secret_id: None,
        enabled: true,
        concurrency_override: None,
        settings: audiobookai_core::SettingsMap::default(),
        capability_snapshot: None,
        created_at: cycle_started_at,
        updated_at: cycle_started_at,
    };
    state
        .database
        .repositories()
        .providers
        .upsert(&provider)
        .await
        .expect("provider fixture");
    let budget = audiobookai_core::Budget {
        id: audiobookai_core::BudgetId::new(),
        name: "Detection characters".to_owned(),
        scope: audiobookai_core::BudgetScope {
            kind: audiobookai_core::BudgetScopeKind::Global,
            provider_profile_id: None,
        },
        period: audiobookai_core::BudgetPeriod::Lifetime,
        metric: audiobookai_core::BudgetMetric::Characters,
        currency: None,
        limit: 100,
        used: 0,
        warning_threshold_percent: 80,
        hard: true,
        enabled: true,
        period_started_at: cycle_started_at,
        period_ends_at: None,
        created_at: cycle_started_at,
        updated_at: cycle_started_at,
    };
    state
        .database
        .repositories()
        .budgets
        .upsert(&budget)
        .await
        .expect("budget fixture");
    let reservation = audiobookai_core::BudgetReservation {
        id: reservation_id,
        job_id: job.id,
        status: audiobookai_core::ReservationStatus::Active,
        allocations: vec![audiobookai_core::BudgetAllocation {
            budget_id: budget.id,
            reserved_amount: 50,
            actual_amount: None,
        }],
        created_at: cycle_started_at,
        expires_at: Some(now - chrono::Duration::hours(1)),
        reconciled_at: None,
    };
    state
        .database
        .repositories()
        .budgets
        .reserve(&reservation)
        .await
        .expect("reservation fixture");
    state
        .database
        .repositories()
        .usage
        .append(&UsageEvent {
            id: UsageEventId::new(),
            occurred_at: cycle_started_at + chrono::Duration::minutes(1),
            workload: UsageWorkload::CharacterDetection,
            project_id,
            job_id: Some(job.id),
            attempt_id: None,
            chapter_id: None,
            segment_id: None,
            provider_profile_id: provider.id,
            provider_family: "openai_compatible".to_owned(),
            endpoint_family: "http://127.0.0.1:1234".to_owned(),
            model: Some("detection-model".to_owned()),
            voice_profile_id: None,
            provider_request_id: None,
            quantities: UsageQuantities {
                characters: Some(12),
                ..UsageQuantities::default()
            },
            quantity_source: ProvenanceQuality::Reported,
            cost: None,
            cost_source: ProvenanceQuality::Unknown,
            rate_card_id: None,
            uncertain_charge: false,
            redacted_raw_usage: BTreeMap::new(),
        })
        .await
        .expect("usage fixture");
    state
        .database
        .repositories()
        .budgets
        .get_at(budget.id, now)
        .await
        .expect("expire reservation");

    recover_terminal_paid_reservations(&state)
        .await
        .expect("startup accounting recovery");

    assert_eq!(
        state
            .database
            .repositories()
            .budgets
            .get_reservation(reservation_id)
            .await
            .unwrap()
            .unwrap()
            .status,
        audiobookai_core::ReservationStatus::Reconciled
    );
    assert_eq!(
        state
            .database
            .repositories()
            .budgets
            .get(budget.id)
            .await
            .unwrap()
            .unwrap()
            .used,
        12
    );
}

#[allow(clippy::too_many_lines)]
#[tokio::test]
async fn semantic_assignment_resolution_does_not_require_dispatch_readiness_or_consent() {
    let (_directory, state) = filesystem_test_state().await;
    let now = Utc::now();
    let project = Project {
        id: ProjectId::new(),
        book_id: audiobookai_core::BookId::new(),
        name: "Semantic fixture".to_owned(),
        status: audiobookai_core::ProjectStatus::Ready,
        metadata: audiobookai_core::BookMetadata::default(),
        cloud_consent: audiobookai_core::CloudConsent::default(),
        settings: audiobookai_core::ProjectSettings::default(),
        character_reviewed_at: Some(now),
        created_at: now,
        updated_at: now,
    };
    let provider_id = Uuid::new_v4();
    let voice_id = Uuid::new_v4();
    let character = crate::models::CharacterView {
        id: Uuid::new_v4(),
        role: audiobookai_core::CharacterRole::Narrator,
        canonical_name: "Narrator".to_owned(),
        aliases: Vec::new(),
        confidence: 1.0,
        dialogue_count: 1,
        voice_assignment: Some(VoiceAssignmentView {
            provider_profile_id: provider_id,
            provider_name: "Offline cloud".to_owned(),
            voice_id,
            voice_name: "Stored voice".to_owned(),
            model: Some("tts-1".to_owned()),
            performance: PerformanceSettings::default(),
            timing: TimingSettings::default(),
        }),
        evidence: Vec::new(),
    };
    let provider = ProviderProfileView {
        id: provider_id,
        name: "Offline cloud".to_owned(),
        kind: ProviderKindView::Openai,
        role: crate::models::ProviderRoleView::Tts,
        mode: ProviderModeView::CloudRemote,
        endpoint: Some("https://example.invalid".to_owned()),
        executable_path: None,
        working_directory: None,
        arguments: Vec::new(),
        status: crate::models::ProviderStatusView::Offline,
        model: Some("tts-1".to_owned()),
        context_window_tokens: None,
        credential_configured: false,
        capabilities: None,
        capability_source: None,
        capability_updated_at: None,
        last_error: None,
    };
    let voices = HashMap::from([(voice_id, "stored-provider-voice".to_owned())]);
    let providers = HashMap::from([(provider_id, provider)]);

    let semantic = build_assignments_for(
        &project,
        std::slice::from_ref(&character),
        &voices,
        &providers,
        &state,
        AssignmentPurpose::Semantic,
    )
    .await
    .expect("provider-free semantic identity");
    assert_eq!(semantic[&character.id].voice_id, voice_id);
    assert!(
        build_assignments(
            &project,
            std::slice::from_ref(&character),
            &voices,
            &providers,
            &state,
        )
        .await
        .is_err(),
        "dispatch still requires a ready provider and cloud consent"
    );

    let mut consent_only = providers[&provider_id].clone();
    consent_only.status = crate::models::ProviderStatusView::Online;
    consent_only.credential_configured = true;
    consent_only.capability_updated_at = Some(Utc::now());
    consent_only.capabilities = Some(crate::models::ProviderCapabilitiesView {
        tts: true,
        character_detection: false,
        streaming: false,
        voice_cloning: false,
        pronunciation: false,
        process_control: false,
        model_control: false,
        model_list: false,
        model_download: false,
        model_delete: false,
        model_load: false,
        model_unload: false,
        model_switch: false,
        temperature: "unsupported".to_owned(),
        reasoning: Vec::new(),
        max_concurrency: Some(1),
        model_performance: Vec::new(),
        reasoning_efforts: Vec::new(),
        min_reasoning_budget: None,
        max_reasoning_budget: None,
        max_temperature: None,
        generation_controls_model: None,
        generation_controls_source: None,
    });
    let consent_only = HashMap::from([(provider_id, consent_only)]);
    assert!(matches!(
        build_assignments(
            &project,
            std::slice::from_ref(&character),
            &voices,
            &consent_only,
            &state,
        )
        .await,
        Err(ServiceError::Conflict(detail)) if detail.contains("consent")
    ));
}

#[tokio::test]
async fn assembly_boundary_rechecks_selected_artifact_integrity() {
    let directory = tempfile::tempdir().expect("temporary directory");
    let path = directory.path().join("selected.flac");
    tokio::fs::write(&path, b"selected take")
        .await
        .expect("selected artifact");
    let now = Utc::now();
    let artifact = Artifact {
        id: ArtifactId::new(),
        kind: ArtifactKind::SegmentAudio,
        path: path.to_string_lossy().into_owned(),
        fingerprint: fingerprint_file(&path).await.expect("fingerprint"),
        media_type: Some("audio/flac".to_owned()),
        duration_ms: Some(1_000),
        cache_key: None,
        pinned_by_job_id: None,
        created_at: now,
        last_accessed_at: now,
    };
    verify_selected_artifact_integrity(&artifact)
        .await
        .expect("initial proof-export validation");

    let mut unknown = artifact.clone();
    unknown.fingerprint.algorithm = "sha256".to_owned();
    assert!(
        verify_selected_artifacts_before_use(&[&unknown])
            .await
            .is_err()
    );

    tokio::fs::write(&path, b"tampered take")
        .await
        .expect("tampered artifact");
    assert!(
        verify_selected_artifacts_before_use(&[&artifact])
            .await
            .is_err(),
        "the assembly boundary must reject a take changed after initial validation"
    );
}

#[tokio::test]
async fn proof_take_materialization_rejects_a_retained_partial_file() {
    let directory = tempfile::tempdir().expect("temporary directory");
    let source_path = directory.path().join("source.flac");
    let destination = directory.path().join("take.flac");
    tokio::fs::write(&source_path, b"complete normalized take")
        .await
        .unwrap();
    tokio::fs::write(&destination, b"retained partial")
        .await
        .unwrap();
    let source = artifact_for_file(
        ArtifactKind::SegmentAudio,
        &source_path,
        Some("audio/flac".to_owned()),
        Some(1_000),
        None,
        None,
    )
    .await
    .expect("source artifact");

    assert!(matches!(
        materialize_proof_take_file(&source, &destination).await,
        Err(ServiceError::Conflict(_))
    ));
    assert_eq!(
        tokio::fs::read(destination).await.unwrap(),
        b"retained partial",
        "mismatched partial data must remain fail-closed and never become a take"
    );
}

#[tokio::test]
async fn split_export_promotion_resumes_after_only_some_files_were_moved() {
    let (directory, state) = filesystem_test_state().await;
    let job_id = JobId::new();
    let output_directory = directory.path().join("exports");
    let staging = prepare_private_export_staging(&state, job_id)
        .await
        .expect("private export staging");
    let temporary = export_staging_output_path(&staging, ExportLayout::PerChapter, "m4b");
    let final_output = output_directory.join("book");
    tokio::fs::create_dir_all(&output_directory)
        .await
        .expect("public export directory");
    tokio::fs::create_dir_all(&temporary)
        .await
        .expect("temporary export directory");
    let first = temporary.join("01-first.m4b");
    let second = temporary.join("02-second.m4b");
    tokio::fs::write(&first, b"first chapter")
        .await
        .expect("first export file");
    tokio::fs::write(&second, b"second chapter")
        .await
        .expect("second export file");
    persist_export_promotion_marker(
        &state,
        job_id,
        &final_output,
        &[(first.clone(), 1_000), (second.clone(), 2_000)],
    )
    .await
    .expect("promotion marker");

    tokio::fs::create_dir(&final_output)
        .await
        .expect("final export directory");
    mark_split_export_directory_created(&state, job_id, &final_output)
        .await
        .expect("durable split-directory ownership marker");
    atomic_promote(&first, &final_output.join("01-first.m4b"))
        .await
        .expect("first promotion");

    let recovered = recover_promoted_export(
        &state,
        job_id,
        ExportLayout::PerChapter,
        &temporary,
        &final_output,
    )
    .await
    .expect("resume promotion");
    assert_eq!(recovered.len(), 2);
    assert_eq!(
        tokio::fs::read(final_output.join("01-first.m4b"))
            .await
            .unwrap(),
        b"first chapter"
    );
    assert_eq!(
        tokio::fs::read(final_output.join("02-second.m4b"))
            .await
            .unwrap(),
        b"second chapter"
    );
    assert!(!temporary.exists());
}

#[tokio::test]
async fn split_export_recovery_rejects_a_foreign_existing_directory() {
    let (directory, state) = filesystem_test_state().await;
    let job_id = JobId::new();
    let output_directory = directory.path().join("exports");
    let staging = prepare_private_export_staging(&state, job_id)
        .await
        .expect("private export staging");
    let temporary = export_staging_output_path(&staging, ExportLayout::PerChapter, "m4b");
    let final_output = output_directory.join("book");
    tokio::fs::create_dir_all(&output_directory)
        .await
        .expect("public export directory");
    tokio::fs::create_dir_all(&temporary)
        .await
        .expect("temporary export directory");
    let staged = temporary.join("01-first.m4b");
    tokio::fs::write(&staged, b"job-owned chapter")
        .await
        .expect("staged export file");
    persist_export_promotion_marker(&state, job_id, &final_output, &[(staged.clone(), 1_000)])
        .await
        .expect("promotion marker");

    tokio::fs::create_dir(&final_output)
        .await
        .expect("foreign final export directory");
    let foreign = final_output.join("foreign.txt");
    tokio::fs::write(&foreign, b"foreign data")
        .await
        .expect("foreign directory contents");

    assert!(matches!(
        recover_promoted_export(
            &state,
            job_id,
            ExportLayout::PerChapter,
            &temporary,
            &final_output,
        )
        .await,
        Err(ServiceError::Conflict(_))
    ));
    assert_eq!(
        tokio::fs::read(&staged).await.unwrap(),
        b"job-owned chapter"
    );
    assert_eq!(tokio::fs::read(&foreign).await.unwrap(), b"foreign data");
    assert!(!final_output.join("01-first.m4b").exists());
}

#[tokio::test]
async fn durable_file_sync_uses_platform_compatible_access() {
    let directory = tempfile::tempdir().expect("temporary directory");
    let path = directory.path().join("completed-output.m4b");
    tokio::fs::write(&path, b"complete output")
        .await
        .expect("completed output");

    sync_file(&path).await.expect("durable file sync");

    assert_eq!(tokio::fs::read(path).await.unwrap(), b"complete output");
}

#[tokio::test]
async fn promotion_never_replaces_an_existing_destination() {
    let directory = tempfile::tempdir().expect("temporary directory");
    let source = directory.path().join("staged.m4b");
    let destination = directory.path().join("book.m4b");
    tokio::fs::write(&source, b"job-owned output")
        .await
        .unwrap();
    tokio::fs::write(&destination, b"foreign output")
        .await
        .unwrap();

    assert!(matches!(
        atomic_promote(&source, &destination).await,
        Err(ServiceError::Conflict(_))
    ));
    assert_eq!(
        tokio::fs::read(&destination).await.unwrap(),
        b"foreign output"
    );
    assert_eq!(tokio::fs::read(&source).await.unwrap(), b"job-owned output");
}

#[tokio::test]
async fn permission_denied_is_a_conflict_when_the_no_clobber_destination_exists() {
    let directory = tempfile::tempdir().expect("temporary directory");
    let existing = directory.path().join("book.m4b");
    tokio::fs::write(&existing, b"foreign output")
        .await
        .unwrap();
    let permission_denied = std::io::Error::from(std::io::ErrorKind::PermissionDenied);

    assert!(
        failed_no_clobber_operation_is_conflict(&permission_denied, &existing).await,
        "Windows can report an existing no-clobber target as access denied"
    );
    assert!(
        !failed_no_clobber_operation_is_conflict(
            &permission_denied,
            &directory.path().join("available.m4b"),
        )
        .await,
        "an unrelated permission failure must remain an I/O error"
    );
    assert_eq!(tokio::fs::read(existing).await.unwrap(), b"foreign output");
}

#[cfg(unix)]
#[tokio::test]
async fn managed_export_staging_ignores_a_hostile_legacy_public_symlink() {
    use std::os::unix::fs::symlink;

    let (directory, state) = filesystem_test_state().await;
    let output_directory = directory.path().join("public-exports");
    tokio::fs::create_dir_all(&output_directory).await.unwrap();
    let victim = directory.path().join("victim.txt");
    tokio::fs::write(&victim, b"must remain untouched")
        .await
        .unwrap();
    let job_id = JobId::new();
    let legacy_public_staging = output_directory.join(format!(".book-{job_id}.partial.m4b"));
    symlink(&victim, &legacy_public_staging).expect("hostile public staging symlink");

    let private_staging = prepare_private_export_staging(&state, job_id)
        .await
        .expect("managed staging directory");
    let managed_output =
        export_staging_output_path(&private_staging, ExportLayout::SingleFile, "m4b");
    write_job_staging_file_atomically(&private_staging, &managed_output, b"new export bytes")
        .await
        .expect("managed staging write");

    assert_eq!(
        tokio::fs::read(&victim).await.unwrap(),
        b"must remain untouched"
    );
    assert!(
        tokio::fs::symlink_metadata(&legacy_public_staging)
            .await
            .unwrap()
            .file_type()
            .is_symlink()
    );
    assert_eq!(
        tokio::fs::read(managed_output).await.unwrap(),
        b"new export bytes"
    );
}

#[cfg(unix)]
#[tokio::test]
async fn swapped_export_root_symlink_is_rejected_before_promotion() {
    use std::os::unix::fs::symlink;

    let (directory, state) = filesystem_test_state().await;
    let project_id = insert_recovery_project(&state, "Swapped export root").await;
    let output_directory = directory.path().join("reserved-output");
    let replacement = directory.path().join("foreign-output");
    tokio::fs::create_dir_all(&output_directory).await.unwrap();
    tokio::fs::create_dir_all(&replacement).await.unwrap();
    let output_directory = tokio::fs::canonicalize(&output_directory).await.unwrap();
    let profile = insert_recovery_export_profile(&state, project_id, &output_directory).await;
    let displaced = directory.path().join("displaced-output");
    tokio::fs::rename(&output_directory, &displaced)
        .await
        .unwrap();
    symlink(&replacement, &output_directory).expect("replacement output symlink");

    assert!(matches!(
        ensure_export_root_identity(&profile).await,
        Err(ServiceError::Conflict(_))
    ));
    assert!(!replacement.join("legacy-book.m4b").exists());
}

#[tokio::test]
async fn failed_exclusive_copy_retains_its_partial_destination_fail_closed() {
    use std::{
        pin::Pin,
        task::{Context, Poll},
    };

    struct FailsAfterPrefix(bool);

    impl tokio::io::AsyncRead for FailsAfterPrefix {
        fn poll_read(
            mut self: Pin<&mut Self>,
            _context: &mut Context<'_>,
            buffer: &mut tokio::io::ReadBuf<'_>,
        ) -> Poll<std::io::Result<()>> {
            if self.0 {
                Poll::Ready(Err(std::io::Error::other("injected copy failure")))
            } else {
                buffer.put_slice(b"partial job output");
                self.0 = true;
                Poll::Ready(Ok(()))
            }
        }
    }

    let directory = tempfile::tempdir().expect("temporary directory");
    let destination = directory.path().join("book.m4b");
    let mut source = FailsAfterPrefix(false);

    assert!(matches!(
        copy_reader_no_clobber(&mut source, &destination).await,
        Err(ServiceError::Io(_))
    ));
    assert_eq!(
        tokio::fs::read(destination).await.unwrap(),
        b"partial job output",
        "a failed fallback must retain the exclusively-created path instead of deleting by name"
    );
}

#[tokio::test]
async fn retry_replaces_only_job_owned_staging_auxiliary_files() {
    let directory = tempfile::tempdir().expect("temporary directory");
    let auxiliary = directory.path().join("..book-job.partial.m4b.ffmetadata");
    write_job_staging_file_atomically(directory.path(), &auxiliary, b"first metadata")
        .await
        .unwrap();
    write_job_staging_file_atomically(directory.path(), &auxiliary, b"retry metadata")
        .await
        .unwrap();
    assert_eq!(
        tokio::fs::read(&auxiliary).await.unwrap(),
        b"retry metadata"
    );

    let public = directory.path().join("book.m4b.manifest.json");
    write_file_atomically(&public, b"first public sidecar")
        .await
        .unwrap();
    assert!(
        write_file_atomically(&public, b"different public sidecar")
            .await
            .is_err()
    );
    assert_eq!(
        tokio::fs::read(public).await.unwrap(),
        b"first public sidecar"
    );
}

#[tokio::test]
async fn provider_stream_sink_requires_order_and_a_final_chunk() {
    let request_id = Uuid::new_v4();
    let sink = ProviderStreamSink::new(request_id, AudioFormat::Wav, None);
    let out_of_order = sink
        .send(AudioChunk {
            request_id,
            sequence: 1,
            format: AudioFormat::Wav,
            sample_rate: None,
            channels: None,
            data: Bytes::from_static(b"late"),
            final_chunk: false,
        })
        .await;
    assert!(out_of_order.is_err());

    sink.send(AudioChunk {
        request_id,
        sequence: 0,
        format: AudioFormat::Wav,
        sample_rate: None,
        channels: None,
        data: Bytes::from_static(b"first"),
        final_chunk: false,
    })
    .await
    .expect("first chunk");
    sink.send(AudioChunk {
        request_id,
        sequence: 1,
        format: AudioFormat::Wav,
        sample_rate: None,
        channels: None,
        data: Bytes::from_static(b"second"),
        final_chunk: true,
    })
    .await
    .expect("final chunk");

    assert_eq!(
        sink.finish().await.expect("collected audio"),
        b"firstsecond"[..]
    );
}

#[tokio::test]
async fn streaming_local_finalization_happens_after_billable_dispatch_success() {
    let request_id = Uuid::new_v4();
    let dispatch = ProviderSynthesisDispatch::Streaming {
        metadata: StreamingSynthesisResponse {
            content_type: "audio/wav".to_owned(),
            usage: ProviderUsage {
                source: UsageSource::Reported,
                characters: Some(17),
                request_id: Some(request_id.to_string()),
                ..ProviderUsage::default()
            },
        },
        // Deliberately omit the provider's final chunk. This is a local response-validation
        // failure after the provider returned successful billing metadata.
        sink: Arc::new(ProviderStreamSink::new(request_id, AudioFormat::Wav, None)),
        decoder_task: None,
        job_id: JobId::new(),
        playback_ordinal: 0,
    };

    assert_eq!(dispatch.usage().characters, Some(17));
    assert!(matches!(
        finish_provider_audio(dispatch).await,
        Err(ProviderError::InvalidResponse(_))
    ));
}

#[test]
fn playback_coordinator_preserves_segment_order_and_resets_retries() {
    let job_id = JobId::new();
    prepare_playback(job_id, 0);
    let mut receiver = subscribe_playback(job_id.as_uuid());

    publish_playback_chunk(job_id, 1, Bytes::from_static(b"later"));
    complete_playback_segment(job_id, 1);
    assert!(matches!(
        receiver.try_recv(),
        Err(broadcast::error::TryRecvError::Empty)
    ));

    publish_playback_chunk(job_id, 0, Bytes::from_static(b"first"));
    assert!(matches!(
        receiver.try_recv(),
        Ok(PlaybackPacket::Audio(value)) if value == b"first"[..]
    ));
    reset_playback_segment(job_id, 0);
    assert!(matches!(receiver.try_recv(), Ok(PlaybackPacket::Reset)));

    publish_playback_chunk(job_id, 0, Bytes::from_static(b"retry"));
    complete_playback_segment(job_id, 0);
    assert!(matches!(
        receiver.try_recv(),
        Ok(PlaybackPacket::Audio(value)) if value == b"retry"[..]
    ));
    assert!(matches!(
        receiver.try_recv(),
        Ok(PlaybackPacket::Audio(value)) if value == b"later"[..]
    ));
}

#[test]
fn parses_bounded_open_and_suffix_byte_ranges() {
    assert_eq!(
        parse_byte_range(&HeaderValue::from_static("bytes=0-9"), 100),
        Ok((0, 9))
    );
    assert_eq!(
        parse_byte_range(&HeaderValue::from_static("bytes=90-"), 100),
        Ok((90, 99))
    );
    assert_eq!(
        parse_byte_range(&HeaderValue::from_static("bytes=-10"), 100),
        Ok((90, 99))
    );
    assert_eq!(
        parse_byte_range(&HeaderValue::from_static("bytes=-200"), 100),
        Ok((0, 99))
    );
    assert_eq!(
        parse_byte_range(&HeaderValue::from_static("bytes=90-200"), 100),
        Ok((90, 99))
    );
}

#[test]
fn rejects_unsatisfiable_or_multiple_byte_ranges() {
    for value in [
        "items=0-9",
        "bytes=",
        "bytes=-0",
        "bytes=100-",
        "bytes=20-10",
        "bytes=0-1,4-5",
    ] {
        assert!(
            parse_byte_range(&HeaderValue::from_str(value).expect("valid header"), 100).is_err(),
            "{value} should be rejected"
        );
    }
    assert!(
        parse_byte_range(&HeaderValue::from_static("bytes=0-0"), 0).is_err(),
        "an empty artifact cannot satisfy a range"
    );
}

#[test]
fn sanitizes_export_file_components() {
    assert_eq!(safe_file_component("  A/B: C?.  "), "A_B_ C_");
    assert_eq!(safe_file_component(" . "), "Audiobook");
    assert_eq!(safe_file_component("\u{0000}\n"), "__");
    assert_eq!(safe_file_component(&"a".repeat(121)).chars().count(), 120);
}

#[test]
fn export_parts_follow_manifest_order_and_retain_unlisted_fallbacks() {
    let now = Utc::now();
    let artifact = |id: u128, path: &str| Artifact {
        id: ArtifactId::from_uuid(Uuid::from_u128(id)),
        kind: ArtifactKind::Export,
        path: path.to_owned(),
        fingerprint: FileFingerprint {
            algorithm: "blake3".to_owned(),
            digest: "00".repeat(32),
            size_bytes: 1,
        },
        media_type: Some("audio/mp4".to_owned()),
        duration_ms: Some(1_000),
        cache_key: None,
        pinned_by_job_id: Some(JobId::from_uuid(Uuid::from_u128(99))),
        created_at: now,
        last_accessed_at: now,
    };
    let first = artifact(1, "/exports/part-1.m4b");
    let second = artifact(2, "/exports/part-2.m4b");
    let fallback = artifact(3, "/exports/recovered.m4b");
    let ordered = canonical_export_ids(
        &[first.clone(), second.clone(), fallback.clone()],
        &[second.path.clone(), first.path.clone()],
    );
    assert_eq!(ordered, [second.id, first.id, fallback.id]);
}

#[test]
fn pronunciation_rules_are_scoped_and_deterministic() {
    let project_id = Uuid::from_u128(1);
    let character_id = Uuid::from_u128(2);
    let other_character = Uuid::from_u128(3);
    let global_id = Uuid::from_u128(10);
    let project_rule_id = Uuid::from_u128(11);
    let rules = vec![
        PronunciationRuleView {
            id: project_rule_id,
            scope: PronunciationScopeView::Project,
            kind: PronunciationKindView::WholeWord,
            source: "Mister".to_owned(),
            replacement: "Doctor".to_owned(),
            language: Some("en".to_owned()),
            character_id: Some(character_id),
            case_sensitive: false,
            enabled: true,
            order: 1,
            conflict: None,
            project_id: Some(project_id),
        },
        PronunciationRuleView {
            id: global_id,
            scope: PronunciationScopeView::Global,
            kind: PronunciationKindView::Literal,
            source: "Mr.".to_owned(),
            replacement: "Mister".to_owned(),
            language: None,
            character_id: None,
            case_sensitive: true,
            enabled: true,
            order: 99,
            conflict: None,
            project_id: None,
        },
        PronunciationRuleView {
            id: Uuid::from_u128(12),
            scope: PronunciationScopeView::Global,
            kind: PronunciationKindView::Literal,
            source: "ignored".to_owned(),
            replacement: "applied".to_owned(),
            language: None,
            character_id: Some(other_character),
            case_sensitive: true,
            enabled: true,
            order: 0,
            conflict: None,
            project_id: None,
        },
    ];

    let (text, applied, revision) = apply_pronunciation_rules(
        "Mr. Smith ignored this.",
        &rules,
        project_id,
        character_id,
        Some("EN"),
    )
    .expect("valid rules");

    assert_eq!(text, "Doctor Smith ignored this.");
    assert_eq!(applied, vec![global_id, project_rule_id]);
    assert_eq!(revision.len(), 64);
}

#[test]
fn synthesis_identity_tracks_performance_but_not_local_timing() {
    let mut segment = test_segment_plan();
    let semantic = segment_semantic_input_hash(&segment).expect("semantic hash");
    let cache_key = segment_cache_fingerprint(&segment, "conversion")
        .key()
        .expect("cache key");

    segment.assignment.timing.pause_after_ms = Some(750);
    assert_eq!(
        segment_semantic_input_hash(&segment).expect("timing semantic hash"),
        semantic
    );
    assert_eq!(
        segment_cache_fingerprint(&segment, "conversion")
            .key()
            .expect("timing cache key"),
        cache_key
    );

    segment.assignment.performance.speed = Some(1.1);
    assert_ne!(
        segment_semantic_input_hash(&segment).expect("performance semantic hash"),
        semantic
    );
    assert_ne!(
        segment_cache_fingerprint(&segment, "conversion")
            .key()
            .expect("performance cache key"),
        cache_key
    );
}

#[test]
fn durable_conversion_snapshot_rejects_same_key_narration_or_model_drift() {
    let snapshot = test_segment_plan();
    let now = Utc::now();
    let unit = JobUnit {
        id: JobUnitId::new(),
        job_id: JobId::new(),
        kind: JobUnitKind::SynthesisSegment,
        state: JobUnitState::Failed,
        chapter_id: Some(ChapterId::from_uuid(snapshot.chapter_id)),
        segment_id: None,
        provider_profile_id: Some(ProviderProfileId::from_uuid(
            snapshot.assignment.provider_id,
        )),
        dependencies: Vec::new(),
        attempt_count: 1,
        next_attempt_at: None,
        output_artifact_id: None,
        payload: BTreeMap::from([
            (
                "segmentKey".to_owned(),
                serde_json::json!(snapshot.key.clone()),
            ),
            ("cacheOperation".to_owned(), serde_json::json!("conversion")),
            (
                "segmentPlan".to_owned(),
                serde_json::to_value(&snapshot).unwrap(),
            ),
        ]),
        created_at: now,
        updated_at: now,
    };
    validate_durable_segment_snapshot(&unit, &snapshot).expect("unchanged snapshot");

    let mut changed_text = snapshot.clone();
    changed_text.text = "Changed narration".to_owned();
    assert!(matches!(
        validate_durable_segment_snapshot(&unit, &changed_text),
        Err(ServiceError::Conflict(detail)) if detail.contains("start a new conversion")
    ));

    let mut changed_model = snapshot;
    changed_model.assignment.model = Some("changed-model".to_owned());
    assert!(matches!(
        validate_durable_segment_snapshot(&unit, &changed_model),
        Err(ServiceError::Conflict(detail)) if detail.contains("start a new conversion")
    ));

    let mut changed_routing = test_segment_plan();
    changed_routing.assignment.provider_endpoint =
        Some("https://different-provider.example/v1".to_owned());
    assert!(matches!(
        validate_durable_segment_snapshot(&unit, &changed_routing),
        Err(ServiceError::Conflict(detail)) if detail.contains("start a new conversion")
    ));
}

#[test]
fn regeneration_reservation_multiplier_follows_uncertain_charge_policy() {
    let base = RetryPolicy::new(4, Duration::from_millis(1), Duration::from_millis(10)).unwrap();
    assert_eq!(
        retry_reservation_multiplier(&base.clone().with_uncertain_charge_retries(false)),
        1
    );
    assert_eq!(
        retry_reservation_multiplier(&base.with_uncertain_charge_retries(true)),
        4
    );
}

#[test]
fn regeneration_runtime_mode_must_match_the_durable_snapshot() {
    assert!(provider_mode_matches_runtime(
        ProviderModeView::CloudRemote,
        audiobookai_providers::ProviderKind::CloudRemote,
    ));
    assert!(provider_mode_matches_runtime(
        ProviderModeView::ExternalEndpoint,
        audiobookai_providers::ProviderKind::ExternalEndpoint,
    ));
    assert!(provider_mode_matches_runtime(
        ProviderModeView::ManagedChild,
        audiobookai_providers::ProviderKind::ManagedChild,
    ));
    assert!(provider_mode_matches_runtime(
        ProviderModeView::Native,
        audiobookai_providers::ProviderKind::Native,
    ));
    assert!(!provider_mode_matches_runtime(
        ProviderModeView::CloudRemote,
        audiobookai_providers::ProviderKind::ExternalEndpoint,
    ));
}

#[test]
fn regeneration_retry_requires_an_exact_persisted_provider_identity() {
    assert!(persisted_provider_mode_matches(
        Some(ProviderModeView::CloudRemote),
        ProviderModeView::CloudRemote,
    ));
    assert!(!persisted_provider_mode_matches(
        None,
        ProviderModeView::CloudRemote,
    ));
    assert!(!persisted_provider_mode_matches(
        Some(ProviderModeView::ExternalEndpoint),
        ProviderModeView::CloudRemote,
    ));

    let snapshot = Uuid::new_v4();
    assert!(persisted_provider_snapshot_matches(
        Some(snapshot),
        Some(snapshot)
    ));
    assert!(!persisted_provider_snapshot_matches(None, Some(snapshot)));
    assert!(!persisted_provider_snapshot_matches(Some(snapshot), None));
    assert!(!persisted_provider_snapshot_matches(
        Some(snapshot),
        Some(Uuid::new_v4())
    ));
}
