// The tests exercise private handlers and helpers across every API domain.
#![allow(clippy::wildcard_imports)]

use super::{
    accounting::*, characters::*, managed_runtimes::*, preflight::*, projects::*,
    provider_config::*, providers::*, settings::*, voices::*, *,
};

#[tokio::test]
async fn durable_copy_does_not_inherit_read_only_source_permissions() {
    let directory = tempfile::tempdir().expect("temporary directory");
    let source = directory.path().join("source.epub");
    let destination = directory.path().join("import.epub");
    tokio::fs::write(&source, b"epub bytes").await.unwrap();
    let original_permissions = tokio::fs::metadata(&source).await.unwrap().permissions();
    let mut read_only_permissions = original_permissions.clone();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        read_only_permissions.set_mode(read_only_permissions.mode() & !0o222);
    }
    #[cfg(windows)]
    read_only_permissions.set_readonly(true);
    tokio::fs::set_permissions(&source, read_only_permissions)
        .await
        .unwrap();

    copy_file_durably(&source, &destination)
        .await
        .expect("durable copy");

    assert_eq!(tokio::fs::read(destination).await.unwrap(), b"epub bytes");
    tokio::fs::set_permissions(source, original_permissions)
        .await
        .unwrap();
}

fn billable_tts_provider_fixture() -> ProviderProfileView {
    ProviderProfileView {
        id: Uuid::new_v4(),
        name: "TTS fixture".to_owned(),
        kind: ProviderKindView::Localai,
        role: ProviderRoleView::Tts,
        mode: ProviderModeView::ExternalEndpoint,
        endpoint: Some("http://127.0.0.1:8080".to_owned()),
        executable_path: None,
        working_directory: None,
        arguments: Vec::new(),
        status: ProviderStatusView::Online,
        model: Some("tts-model".to_owned()),
        context_window_tokens: None,
        credential_configured: false,
        capabilities: Some(default_capabilities(
            &ProviderKindView::Localai,
            ProviderRoleView::Tts,
            ProviderModeView::ExternalEndpoint,
        )),
        capability_source: Some("test".to_owned()),
        capability_updated_at: Some(Utc::now()),
        last_error: None,
    }
}

#[test]
fn billable_tts_readiness_requires_fresh_online_and_configured_cloud_provider() {
    let ready = billable_tts_provider_fixture();
    validate_billable_tts_provider_readiness(&ready).expect("fresh local provider");

    let mut stale = ready.clone();
    stale.capability_updated_at = Some(Utc::now() - ChronoDuration::hours(25));
    assert!(validate_billable_tts_provider_readiness(&stale).is_err());

    let mut offline = ready.clone();
    offline.status = ProviderStatusView::Offline;
    assert!(validate_billable_tts_provider_readiness(&offline).is_err());

    let mut cloud = ready;
    cloud.mode = ProviderModeView::CloudRemote;
    assert!(validate_billable_tts_provider_readiness(&cloud).is_err());
    cloud.credential_configured = true;
    validate_billable_tts_provider_readiness(&cloud).expect("configured cloud provider");
}

#[test]
fn piper_assignment_is_scoped_to_the_connections_exact_model() {
    let mut provider = billable_tts_provider_fixture();
    provider.kind = ProviderKindView::Piper;
    provider.mode = ProviderModeView::Native;
    provider.model = Some("de_DE-thorsten-medium".to_owned());

    validate_piper_voice_selection(&provider, "de_DE-thorsten-medium", None)
        .expect("the connection's selected voice");
    validate_piper_voice_selection(
        &provider,
        "de_DE-thorsten-medium",
        Some("de_DE-thorsten-medium"),
    )
    .expect("matching explicit model");
    assert!(
        validate_piper_voice_selection(&provider, "planted-voice", None).is_err(),
        "an unrelated voice directory must never enter this connection"
    );
    assert!(
        validate_piper_voice_selection(&provider, "de_DE-thorsten-medium", Some("other-model"),)
            .is_err(),
        "an assignment cannot override the connection model"
    );
}

#[test]
fn piper_uninstall_detection_only_matches_the_active_uninstall_operation() {
    use crate::piper_management::{PiperOperationKind, PiperOperationState, PiperOperationView};

    let mut operation = PiperOperationView {
        id: Uuid::new_v4(),
        kind: PiperOperationKind::Uninstall,
        state: PiperOperationState::Running,
        progress_percent: 0,
        phase: "preparing".to_owned(),
        message: "Preparing Piper uninstall".to_owned(),
        voice_id: None,
        bytes_downloaded: None,
        bytes_total: None,
        started_at: Utc::now(),
        finished_at: None,
    };
    assert!(piper_uninstall_in_progress(Some(&operation)));

    operation.kind = PiperOperationKind::Install;
    assert!(!piper_uninstall_in_progress(Some(&operation)));
    assert!(!piper_uninstall_in_progress(None));
}

#[tokio::test]
#[allow(clippy::too_many_lines)]
async fn health_observation_preserves_an_unchanged_dispatch_snapshot_identity() {
    let directory = tempfile::tempdir().expect("tempdir");
    let database = audiobookai_storage::Database::open_in(directory.path())
        .await
        .expect("database");
    let state = AppState::new(
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
    .expect("state");
    let mut profile = ProviderProfileView {
        id: Uuid::new_v4(),
        name: "Local LLM".to_owned(),
        kind: ProviderKindView::OpenaiCompatible,
        role: ProviderRoleView::Llm,
        mode: ProviderModeView::ExternalEndpoint,
        endpoint: Some("http://127.0.0.1:1234/".to_owned()),
        executable_path: None,
        working_directory: None,
        arguments: Vec::new(),
        status: ProviderStatusView::Offline,
        model: Some("local-model".to_owned()),
        context_window_tokens: None,
        credential_configured: false,
        capabilities: Some(default_capabilities(
            &ProviderKindView::OpenaiCompatible,
            ProviderRoleView::Llm,
            ProviderModeView::ExternalEndpoint,
        )),
        capability_source: Some("adapter-contract".to_owned()),
        capability_updated_at: Some(Utc::now()),
        last_error: None,
    };
    persist_provider(&state, &profile, None)
        .await
        .expect("initial snapshot");
    let first = state
        .database
        .repositories()
        .providers
        .get(audiobookai_core::ProviderProfileId::from_uuid(profile.id))
        .await
        .expect("read provider")
        .and_then(|stored| stored.capability_snapshot)
        .expect("initial capability snapshot");

    profile.status = ProviderStatusView::Online;
    profile.capability_source = Some("adapter-contract+health-probe".to_owned());
    profile.capability_updated_at = Some(Utc::now() + ChronoDuration::minutes(1));
    persist_provider(&state, &profile, None)
        .await
        .expect("health observation");
    let observed = state
        .database
        .repositories()
        .providers
        .get(audiobookai_core::ProviderProfileId::from_uuid(profile.id))
        .await
        .expect("read observed provider")
        .and_then(|stored| stored.capability_snapshot)
        .expect("observed capability snapshot");
    assert_eq!(observed.id, first.id);
    assert!(observed.observed_at > first.observed_at);

    profile.model = Some("different-model".to_owned());
    persist_provider(&state, &profile, None)
        .await
        .expect("changed dispatch contract");
    let changed = state
        .database
        .repositories()
        .providers
        .get(audiobookai_core::ProviderProfileId::from_uuid(profile.id))
        .await
        .expect("read changed provider")
        .and_then(|stored| stored.capability_snapshot)
        .expect("changed capability snapshot");
    assert_ne!(changed.id, first.id);

    profile.context_window_tokens = Some(4_096);
    persist_provider(&state, &profile, None)
        .await
        .expect("changed context contract");
    let stored = state
        .database
        .repositories()
        .providers
        .get(audiobookai_core::ProviderProfileId::from_uuid(profile.id))
        .await
        .expect("read context-aware provider")
        .expect("stored provider");
    assert_eq!(
        stored
            .settings
            .0
            .get("context_window_tokens")
            .and_then(serde_json::Value::as_u64),
        Some(4_096)
    );
    let context_snapshot = stored.capability_snapshot.expect("context snapshot");
    assert_ne!(context_snapshot.id, changed.id);
    assert_eq!(
        context_snapshot
            .capabilities
            .character_detection
            .expect("character capabilities")
            .context_window_tokens,
        Some(4_096)
    );
}

#[cfg(target_os = "linux")]
#[tokio::test]
async fn unavailable_native_provider_creation_is_rejected_before_persistence() {
    let directory = tempfile::tempdir().expect("tempdir");
    let database = audiobookai_storage::Database::open_in(directory.path())
        .await
        .expect("database");
    let state = Arc::new(
        AppState::new(
            crate::ServiceConfig {
                bind: "127.0.0.1:0".parse().expect("address"),
                data_dir: directory.path().to_path_buf(),
                bundled_sidecar_dir: Some(directory.path().join("missing-sidecars/bin")),
                tls: None,
                lan_hostnames: Vec::new(),
                allow_insecure_lan: false,
                desktop_bootstrap: false,
            },
            database.clone(),
        )
        .await
        .expect("state"),
    );

    let error = create_provider(
        State(state),
        Json(ProviderProfileInput {
            name: Some("Native system voices".to_owned()),
            kind: Some(ProviderKindView::NativeOs),
            role: Some(ProviderRoleView::Tts),
            mode: Some(ProviderModeView::Native),
            endpoint: None,
            executable_path: None,
            working_directory: None,
            arguments: None,
            model: None,
            context_window_tokens: None,
            credential: None,
        }),
    )
    .await
    .expect_err("missing packaged eSpeak must reject creation");
    let detail = error.to_string();
    assert!(detail.contains("eSpeak NG"));
    assert!(detail.contains("Piper"));
    assert!(
        database
            .repositories()
            .providers
            .list(false)
            .await
            .expect("list providers")
            .is_empty(),
        "failed creation must not leave a provider or tombstone row"
    );
}

#[cfg(target_os = "linux")]
#[tokio::test]
async fn native_availability_endpoint_reports_typed_setup_guidance() {
    let directory = tempfile::tempdir().expect("tempdir");
    let database = audiobookai_storage::Database::open_in(directory.path())
        .await
        .expect("database");
    let state = Arc::new(
        AppState::new(
            crate::ServiceConfig {
                bind: "127.0.0.1:0".parse().expect("address"),
                data_dir: directory.path().to_path_buf(),
                bundled_sidecar_dir: Some(directory.path().join("missing-sidecars/bin")),
                tls: None,
                lan_hostnames: Vec::new(),
                allow_insecure_lan: false,
                desktop_bootstrap: false,
            },
            database,
        )
        .await
        .expect("state"),
    );

    let response = native_provider_availability(State(state)).await;
    assert_eq!(
        response.headers().get(header::CACHE_CONTROL),
        Some(&http::HeaderValue::from_static("no-store"))
    );
    let body = axum::body::to_bytes(response.into_body(), 16 * 1024)
        .await
        .expect("availability response body");
    let wire: serde_json::Value = serde_json::from_slice(&body).expect("availability JSON");
    assert_eq!(
        wire.get("platform").and_then(serde_json::Value::as_str),
        Some("linux")
    );
    assert_eq!(
        wire.get("providerName").and_then(serde_json::Value::as_str),
        Some("eSpeak NG")
    );
    assert_eq!(
        wire.get("available").and_then(serde_json::Value::as_bool),
        Some(false)
    );
    assert!(
        wire.get("detail")
            .and_then(serde_json::Value::as_str)
            .is_some_and(|value| value.contains("Piper"))
    );
    assert!(wire.get("executable").is_none());
}

#[tokio::test]
async fn provider_deletion_removes_its_unassigned_discovered_voice_rows() {
    let directory = tempfile::tempdir().expect("tempdir");
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
        .expect("state"),
    );
    let profile = billable_tts_provider_fixture();
    persist_provider(&state, &profile, None)
        .await
        .expect("persist provider");
    state
        .catalog
        .write()
        .await
        .providers
        .insert(profile.id, profile.clone());
    let voice_id = stable_voice_id(profile.id, "fixture-voice");
    let voice = VoiceView {
        id: voice_id,
        provider_profile_id: profile.id,
        name: "Fixture voice".to_owned(),
        locale: Some("en".to_owned()),
        gender: None,
        kind: crate::models::VoiceKindView::Catalog,
        owned: false,
        preview_url: None,
    };
    persist_discovered_voice(&state, &profile, voice_id, "fixture-voice", &voice)
        .await
        .expect("persist discovered voice");
    let budget = BudgetView {
        id: Uuid::new_v4(),
        name: "Provider budget".to_owned(),
        provider_profile_id: Some(profile.id),
        period: crate::models::BudgetPeriodView::Lifetime,
        metric: crate::models::BudgetMetricView::Characters,
        limit: 10_000,
        used: 0,
        reserved: 0,
        hard: true,
        currency: None,
        warning_percent: 80,
    };
    persist_budget(&state, &budget)
        .await
        .expect("persist provider budget");
    state
        .catalog
        .write()
        .await
        .budgets
        .insert(budget.id, budget.clone());

    let response = delete_provider(State(Arc::clone(&state)), Path(profile.id))
        .await
        .expect("delete provider with unassigned catalog voice");
    assert_eq!(response, StatusCode::NO_CONTENT);
    let tombstone = state
        .database
        .repositories()
        .providers
        .get(audiobookai_core::ProviderProfileId::from_uuid(profile.id))
        .await
        .expect("read deleted provider")
        .expect("audit tombstone");
    assert!(!tombstone.enabled);
    assert!(tombstone.credential_secret_id.is_none());
    assert!(tombstone.capability_snapshot.is_none());
    let remaining =
        sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM voice_profiles WHERE provider_id = ?")
            .bind(profile.id.to_string())
            .fetch_one(state.database.pool())
            .await
            .expect("count voice rows");
    assert_eq!(remaining, 0);
    assert!(
        !state.catalog.read().await.budgets.contains_key(&budget.id),
        "the disabled provider budget must not remain visible in the live catalog"
    );
    let budget_enabled = sqlx::query_scalar::<_, i64>("SELECT enabled FROM budgets WHERE id = ?")
        .bind(budget.id.to_string())
        .fetch_one(state.database.pool())
        .await
        .expect("read disabled provider budget");
    assert_eq!(budget_enabled, 0);
}

async fn ensure_existing_native_fixture(state: &Arc<AppState>) -> ProviderProfileView {
    let canonical_id = Uuid::parse_str(match std::env::consts::OS {
        "macos" => "9f85e64a-f687-4e86-8b6c-fc71938249eb",
        "windows" => "5dd70ee1-eb54-430e-bb3b-e4bb31d7ee91",
        _ => "e76afdb2-3458-46cb-874b-1c242d1336d9",
    })
    .expect("canonical native provider UUID");
    if let Some(profile) = state
        .catalog
        .read()
        .await
        .providers
        .get(&canonical_id)
        .cloned()
    {
        return profile;
    }
    let profile = ProviderProfileView {
        id: canonical_id,
        name: "Native system voices".to_owned(),
        kind: ProviderKindView::NativeOs,
        role: ProviderRoleView::Tts,
        mode: ProviderModeView::Native,
        endpoint: None,
        executable_path: None,
        working_directory: None,
        arguments: Vec::new(),
        status: ProviderStatusView::Unconfigured,
        model: None,
        context_window_tokens: None,
        credential_configured: true,
        capabilities: Some(default_capabilities(
            &ProviderKindView::NativeOs,
            ProviderRoleView::Tts,
            ProviderModeView::Native,
        )),
        capability_source: Some("test".to_owned()),
        capability_updated_at: Some(Utc::now()),
        last_error: Some("setup required".to_owned()),
    };
    persist_provider(state, &profile, None)
        .await
        .expect("persist unavailable existing native profile");
    state
        .catalog
        .write()
        .await
        .providers
        .insert(profile.id, profile.clone());
    profile
}

#[tokio::test]
async fn native_provider_and_legacy_duplicates_can_be_removed_without_respawning() {
    let directory = tempfile::tempdir().expect("tempdir");
    let database = audiobookai_storage::Database::open_in(directory.path())
        .await
        .expect("database");
    let config = crate::ServiceConfig {
        bind: "127.0.0.1:0".parse().expect("address"),
        data_dir: directory.path().to_path_buf(),
        bundled_sidecar_dir: None,
        tls: None,
        lan_hostnames: Vec::new(),
        allow_insecure_lan: false,
        desktop_bootstrap: false,
    };
    let state = Arc::new(
        AppState::new(config.clone(), database.clone())
            .await
            .expect("state"),
    );
    let canonical = ensure_existing_native_fixture(&state).await;
    let mut legacy_duplicate = canonical.clone();
    legacy_duplicate.id = Uuid::new_v4();
    legacy_duplicate.name = "Legacy native duplicate".to_owned();
    persist_provider(&state, &legacy_duplicate, None)
        .await
        .expect("persist legacy duplicate");
    state
        .catalog
        .write()
        .await
        .providers
        .insert(legacy_duplicate.id, legacy_duplicate.clone());

    for id in [canonical.id, legacy_duplicate.id] {
        let response = delete_provider(State(Arc::clone(&state)), Path(id))
            .await
            .expect("delete native profile");
        assert_eq!(response, StatusCode::NO_CONTENT);
        let tombstone = state
            .database
            .repositories()
            .providers
            .get(audiobookai_core::ProviderProfileId::from_uuid(id))
            .await
            .expect("read deleted provider")
            .expect("retained audit tombstone");
        assert!(!tombstone.enabled);
        assert!(tombstone.credential_secret_id.is_none());
    }

    drop(state);
    let restarted = AppState::new(config, database)
        .await
        .expect("restarted state");
    assert!(
        restarted
            .catalog
            .read()
            .await
            .providers
            .values()
            .all(|profile| !matches!(profile.kind, ProviderKindView::NativeOs)),
        "neither the canonical nor legacy native connection may respawn"
    );
}

#[test]
fn project_mutation_admission_includes_regeneration_and_proof_export() {
    for kind in [
        crate::models::JobKindView::CharacterDetection,
        crate::models::JobKindView::Conversion,
        crate::models::JobKindView::SegmentRegeneration,
        crate::models::JobKindView::Export,
    ] {
        assert!(
            blocks_project_mutation(kind),
            "{kind:?} must block mutation"
        );
    }
    for kind in [
        crate::models::JobKindView::Preview,
        crate::models::JobKindView::QualityControl,
        crate::models::JobKindView::CacheCleanup,
    ] {
        assert!(
            !blocks_project_mutation(kind),
            "{kind:?} must not block mutation"
        );
    }
}

#[test]
fn character_review_revision_uses_the_public_camel_case_contract() {
    let input: ReviewInput = serde_json::from_value(serde_json::json!({
        "approved": true,
        "expectedCharacterRevision": 7,
    }))
    .expect("review input");

    assert!(input.approved);
    assert_eq!(input.expected_character_revision, 7);
}

fn project_fixture() -> ProjectDetail {
    let now = Utc::now();
    ProjectDetail {
        summary: BookSummary {
            id: Uuid::new_v4(),
            title: "Book".to_owned(),
            author: None,
            cover_url: None,
            chapter_count: 1,
            selected_chapter_count: 1,
            duration_seconds: None,
            progress: 0.0,
            status: ProjectDisplayStatus::Draft,
            updated_at: now,
            language: None,
            series: None,
            series_position: None,
        },
        narrator: None,
        publisher: None,
        description: None,
        consent_cloud_text: false,
        consent_cloud_audio: false,
        chapters: vec![ChapterView {
            id: Uuid::new_v4(),
            index: 0,
            title: "One".to_owned(),
            selected: true,
            word_count: 20,
            character_count: 140,
            estimated_seconds: None,
            status: ChapterDisplayStatus::Pending,
        }],
        character_review_status: ReviewStatus::NotStarted,
        character_revision: 0,
        output_name: None,
    }
}

#[tokio::test]
async fn estimates_without_making_provider_requests() {
    let directory = tempfile::tempdir().expect("tempdir");
    let database = audiobookai_storage::Database::open_in(directory.path())
        .await
        .expect("database");
    let state = AppState::new(
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
    .expect("state");
    let project = project_fixture();
    let estimate = estimate_project(&state, &project, &[], &HashMap::new())
        .await
        .expect("estimate");
    assert_eq!(estimate.characters, 140);
    assert_eq!(estimate.estimated_duration_seconds, 10);
    assert!(estimate.estimated_tokens.is_none());
    assert!(estimate.monetary_cost_micros.is_none());
}

#[tokio::test]
async fn estimate_uses_stored_assignment_and_rate_card_provenance() {
    use audiobookai_core::{ProviderProfileId, RateCard, RateCardId, UsageWorkload};

    let directory = tempfile::tempdir().expect("tempdir");
    let database = audiobookai_storage::Database::open_in(directory.path())
        .await
        .expect("database");
    let state = AppState::new(
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
    .expect("state");
    let provider_id = Uuid::new_v4();
    let provider = ProviderProfileView {
        id: provider_id,
        name: "Local fixture".to_owned(),
        kind: ProviderKindView::Localai,
        role: ProviderRoleView::Tts,
        mode: ProviderModeView::ExternalEndpoint,
        endpoint: Some("http://127.0.0.1:8080".to_owned()),
        executable_path: None,
        working_directory: None,
        arguments: Vec::new(),
        status: ProviderStatusView::Online,
        model: Some("fixture-model".to_owned()),
        context_window_tokens: None,
        credential_configured: false,
        capabilities: Some(default_capabilities(
            &ProviderKindView::Localai,
            ProviderRoleView::Tts,
            ProviderModeView::ExternalEndpoint,
        )),
        capability_source: Some("fixture".to_owned()),
        capability_updated_at: Some(Utc::now()),
        last_error: None,
    };
    persist_provider(&state, &provider, None)
        .await
        .expect("provider");
    let effective_at = Utc::now() - ChronoDuration::minutes(1);
    let card = RateCard {
        id: RateCardId::new(),
        provider_profile_id: ProviderProfileId::from_uuid(provider_id),
        model: Some("fixture-model".to_owned()),
        workload: UsageWorkload::Tts,
        currency: "EUR".to_owned(),
        effective_at,
        expires_at: None,
        source: "Local fixture rate".to_owned(),
        source_url: None,
        pricing: BTreeMap::from([
            ("per_1000_characters_micros".to_owned(), 1_000),
            ("credits_per_character_micros".to_owned(), 2),
        ]),
        user_overridden: true,
    };
    persist_rate_card(&state, &card).await.expect("rate card");
    let project = project_fixture();
    let characters = vec![crate::models::CharacterView {
        id: Uuid::new_v4(),
        role: audiobookai_core::CharacterRole::Narrator,
        canonical_name: "Narrator".to_owned(),
        aliases: Vec::new(),
        confidence: 1.0,
        dialogue_count: 0,
        voice_assignment: Some(VoiceAssignmentView {
            provider_profile_id: provider_id,
            provider_name: provider.name.clone(),
            voice_id: Uuid::new_v4(),
            voice_name: "Fixture voice".to_owned(),
            model: None,
            performance: audiobookai_core::PerformanceSettings::default(),
            timing: audiobookai_core::TimingSettings::default(),
        }),
        evidence: Vec::new(),
    }];
    let providers = HashMap::from([(provider_id, provider)]);
    let estimate = estimate_project(&state, &project, &characters, &providers)
        .await
        .expect("estimate");

    assert_eq!(estimate.monetary_cost_micros, Some(140));
    assert_eq!(estimate.currency.as_deref(), Some("EUR"));
    assert_eq!(estimate.credits, Some(280));
    assert_eq!(estimate.price_source.as_deref(), Some("Local fixture rate"));
    assert_eq!(estimate.price_effective_at, Some(effective_at));
    assert_eq!(estimate.provider_estimates.len(), 1);
    assert_eq!(estimate.provider_estimates[0].characters, 140);
    assert_eq!(
        estimate.provider_estimates[0].model.as_deref(),
        Some("fixture-model")
    );
}

#[test]
fn dry_run_accepts_an_online_local_tts_provider_without_a_credential() {
    let provider_id = Uuid::new_v4();
    let voice_id = Uuid::new_v4();
    let mut project = project_fixture();
    project.character_review_status = ReviewStatus::Approved;
    let characters = vec![crate::models::CharacterView {
        id: Uuid::new_v4(),
        role: audiobookai_core::CharacterRole::Narrator,
        canonical_name: "Narrator".to_owned(),
        aliases: Vec::new(),
        confidence: 1.0,
        dialogue_count: 1,
        voice_assignment: Some(VoiceAssignmentView {
            provider_profile_id: provider_id,
            provider_name: "LocalAI".to_owned(),
            voice_id,
            voice_name: "Local voice".to_owned(),
            model: None,
            performance: audiobookai_core::PerformanceSettings::default(),
            timing: audiobookai_core::TimingSettings::default(),
        }),
        evidence: Vec::new(),
    }];
    let provider = ProviderProfileView {
        id: provider_id,
        name: "LocalAI".to_owned(),
        kind: ProviderKindView::Localai,
        role: ProviderRoleView::Tts,
        mode: ProviderModeView::ExternalEndpoint,
        endpoint: Some("http://127.0.0.1:8080".to_owned()),
        executable_path: None,
        working_directory: None,
        arguments: Vec::new(),
        status: ProviderStatusView::Online,
        model: None,
        context_window_tokens: None,
        credential_configured: false,
        capabilities: Some(default_capabilities(
            &ProviderKindView::Localai,
            ProviderRoleView::Tts,
            ProviderModeView::ExternalEndpoint,
        )),
        capability_source: Some("test".to_owned()),
        capability_updated_at: Some(Utc::now()),
        last_error: None,
    };
    let result = dry_run_project(
        &project,
        &characters,
        &[(provider_id, provider)].into_iter().collect(),
    );
    assert!(result.ready, "{:#?}", result.checks);
}

#[test]
fn managed_provider_requires_an_absolute_executable() {
    assert!(
        validate_provider_location(
            ProviderModeView::ManagedChild,
            None,
            Some("bin/server"),
            None,
            &[],
        )
        .is_err()
    );
}

#[test]
fn provider_patch_distinguishes_omitted_values_from_explicit_clears() {
    let omitted: ProviderProfileInput =
        serde_json::from_value(serde_json::json!({})).expect("omitted patch");
    let cleared: ProviderProfileInput = serde_json::from_value(serde_json::json!({
        "endpoint": null,
        "executablePath": null,
        "workingDirectory": null,
        "model": null,
        "contextWindowTokens": null,
        "arguments": []
    }))
    .expect("clear patch");

    assert!(omitted.endpoint.is_none());
    assert!(matches!(cleared.endpoint, Some(None)));
    assert!(matches!(cleared.executable_path, Some(None)));
    assert!(matches!(cleared.working_directory, Some(None)));
    assert!(matches!(cleared.model, Some(None)));
    assert!(matches!(cleared.context_window_tokens, Some(None)));
    assert_eq!(cleared.arguments, Some(Vec::new()));
}

#[test]
fn provider_context_window_is_llm_only_and_bounded() {
    assert!(validate_provider_context_window(ProviderRoleView::Llm, Some(4_096)).is_ok());
    assert!(validate_provider_context_window(ProviderRoleView::Tts, Some(4_096)).is_err());
    assert!(validate_provider_context_window(ProviderRoleView::Llm, Some(2_047)).is_err());
    assert!(validate_provider_context_window(ProviderRoleView::Llm, Some(2_097_153)).is_err());
}

#[test]
fn provider_model_discovery_uses_the_flat_provider_contract() {
    let provider_id = Uuid::new_v4();
    let input: ProviderModelDiscoveryInput = serde_json::from_value(serde_json::json!({
        "providerId": provider_id,
        "name": "Local model preview",
        "kind": "ollama",
        "role": "llm",
        "mode": "external_endpoint",
        "endpoint": "http://127.0.0.1:11434/",
        "model": null
    }))
    .expect("provider model discovery input");

    assert_eq!(input.provider_id, Some(provider_id));
    assert!(matches!(input.profile.kind, Some(ProviderKindView::Ollama)));
    assert_eq!(input.profile.role, Some(ProviderRoleView::Llm));
    assert!(matches!(
        input.profile.endpoint,
        Some(Some(ref endpoint)) if endpoint == "http://127.0.0.1:11434/"
    ));
    assert!(matches!(input.profile.model, Some(None)));
}

#[test]
fn provider_input_debug_never_contains_the_credential() {
    let credential = ["runtime", "credential", "value"].join("-");
    let input: ProviderProfileInput = serde_json::from_value(serde_json::json!({
        "credential": credential
    }))
    .expect("provider input");
    let debug = format!("{input:?}");
    assert!(!debug.contains(&credential));
    assert!(debug.contains("[REDACTED]"));
}

#[test]
fn managed_arguments_are_bounded_and_never_shell_parsed() {
    let executable = std::env::current_exe().expect("test executable");
    let working_directory = std::env::current_dir().expect("test working directory");
    assert!(
        validate_provider_location(
            ProviderModeView::ManagedChild,
            Some("http://127.0.0.1:8080"),
            executable.to_str(),
            working_directory.to_str(),
            &["--listen".to_owned(), "127.0.0.1; echo ignored".to_owned()],
        )
        .is_ok()
    );
    assert!(
        validate_provider_location(
            ProviderModeView::ManagedChild,
            None,
            executable.to_str(),
            None,
            &["bad\0argument".to_owned()],
        )
        .is_err()
    );
    assert!(
        validate_provider_location(
            ProviderModeView::ManagedChild,
            None,
            working_directory.to_str(),
            None,
            &[],
        )
        .is_err()
    );
}

#[test]
fn managed_arguments_reject_credential_flags() {
    let executable = std::env::current_exe().expect("test executable");
    let working_directory = std::env::current_dir().expect("test working directory");
    let arguments = [
        "--api-key".to_owned(),
        format!("--{}={}", "TOKEN", "fixture-value"),
        "--auth_token".to_owned(),
        format!("--{}={}", "password", "fixture-value"),
        "--authorization".to_owned(),
    ];
    for argument in arguments {
        let result = validate_provider_location(
            ProviderModeView::ManagedChild,
            None,
            executable.to_str(),
            working_directory.to_str(),
            &[argument],
        );
        assert!(result.is_err(), "credential flag should be rejected");
        assert!(
            result
                .expect_err("credential flag rejection")
                .to_string()
                .contains("encrypted credential storage")
        );
    }
}

#[test]
fn managed_arguments_reject_secret_shaped_whole_values_without_echoing_them() {
    let executable = std::env::current_exe().expect("test executable");
    let working_directory = std::env::current_dir().expect("test working directory");
    let prefixed = [
        ["h", "f"].concat(),
        "syntheticcredential0123456789".to_owned(),
    ]
    .join("_");
    let jwt = ["headerpart0123", "payloadpart4567", "signaturepart89"].join(".");
    for argument in [prefixed.clone(), jwt.clone(), format!("--cache={prefixed}")] {
        let error = validate_provider_location(
            ProviderModeView::ManagedChild,
            None,
            executable.to_str(),
            working_directory.to_str(),
            std::slice::from_ref(&argument),
        )
        .expect_err("secret-shaped process argument must be rejected");
        let message = error.to_string();
        assert!(message.contains("encrypted credential storage"));
        assert!(!message.contains(&argument));
    }
}

#[test]
fn provider_models_reject_secret_shapes_and_native_profiles_reject_credentials() {
    let prefixed = [
        ["s", "k"].concat(),
        "syntheticcredential0123456789".to_owned(),
    ]
    .join("-");
    let jwt = ["headerpart0123", "payloadpart4567", "signaturepart89"].join(".");
    for model in [prefixed.clone(), jwt, format!("owner/{prefixed}")] {
        let error = validate_provider_sensitive_fields(
            &ProviderKindView::Ollama,
            ProviderRoleView::Llm,
            ProviderModeView::ExternalEndpoint,
            Some(&model),
            false,
        )
        .expect_err("secret-shaped model must be rejected before persistence");
        let message = error.to_string();
        assert!(message.contains("sensitive credential material"));
        assert!(!message.contains(&model));
    }
    assert!(
        validate_provider_sensitive_fields(
            &ProviderKindView::Ollama,
            ProviderRoleView::Llm,
            ProviderModeView::ExternalEndpoint,
            Some("gemma3:latest"),
            false,
        )
        .is_ok()
    );

    for (kind, mode) in [
        (ProviderKindView::NativeOs, ProviderModeView::Native),
        (
            ProviderKindView::NativeOs,
            ProviderModeView::ExternalEndpoint,
        ),
        (ProviderKindView::Elevenlabs, ProviderModeView::Native),
    ] {
        let error =
            validate_provider_sensitive_fields(&kind, ProviderRoleView::Tts, mode, None, true)
                .expect_err("native credentials must be rejected");
        assert!(error.to_string().contains("must not be configured"));
    }
    assert!(
        validate_provider_sensitive_fields(
            &ProviderKindView::NativeOs,
            ProviderRoleView::Tts,
            ProviderModeView::Native,
            None,
            false,
        )
        .is_ok()
    );
}

#[test]
fn provider_roles_and_openai_models_are_validated_fail_closed() {
    assert_eq!(
        canonical_provider_kind(ProviderKindView::OpenaiTts),
        ProviderKindView::Openai,
        "legacy OpenAI Speech input must enter the dual-role provider path"
    );
    for role in [ProviderRoleView::Tts, ProviderRoleView::Llm] {
        validate_provider_role(&ProviderKindView::Openai, role)
            .expect("OpenAI supports independent TTS and LLM connections");
    }
    assert!(validate_provider_role(&ProviderKindView::OpenaiTts, ProviderRoleView::Llm).is_err());
    assert!(validate_provider_role(&ProviderKindView::Elevenlabs, ProviderRoleView::Llm).is_err());
    assert!(validate_provider_role(&ProviderKindView::Ollama, ProviderRoleView::Tts).is_err());

    validate_provider_model_compatibility(
        &ProviderKindView::Openai,
        ProviderRoleView::Tts,
        Some("tts-1-hd-1106"),
    )
    .expect("documented OpenAI speech model");
    validate_provider_model_compatibility(
        &ProviderKindView::Openai,
        ProviderRoleView::Llm,
        Some("gpt-5.6-luna"),
    )
    .expect("documented OpenAI Responses model family");

    for (role, model) in [
        (ProviderRoleView::Tts, "gpt-5.6-luna"),
        (ProviderRoleView::Llm, "tts-1-hd-1106"),
        (ProviderRoleView::Llm, "text-embedding-3-large"),
        (ProviderRoleView::Llm, "gpt-4o-mini-transcribe"),
    ] {
        assert!(
            validate_provider_model_compatibility(&ProviderKindView::Openai, role, Some(model),)
                .is_err(),
            "{model} must not be accepted for {role:?}"
        );
    }
    assert!(provider_model_catalog_is_strict(
        &ProviderKindView::Openai,
        ProviderRoleView::Tts
    ));
    assert!(provider_model_catalog_is_strict(
        &ProviderKindView::Openai,
        ProviderRoleView::Llm
    ));
}

#[tokio::test]
async fn openai_connections_persist_independent_roles_and_models() {
    let directory = tempfile::tempdir().expect("tempdir");
    let database = audiobookai_storage::Database::open_in(directory.path())
        .await
        .expect("database");
    let state = AppState::new(
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
    .expect("state");
    let tts_id = Uuid::new_v4();
    let llm_id = Uuid::new_v4();
    for (id, role, model) in [
        (tts_id, ProviderRoleView::Tts, "tts-1-hd-1106"),
        (llm_id, ProviderRoleView::Llm, "gpt-5.6-luna"),
    ] {
        let profile = ProviderProfileView {
            id,
            name: "OpenAI".to_owned(),
            kind: ProviderKindView::Openai,
            role,
            mode: ProviderModeView::CloudRemote,
            endpoint: Some("https://api.openai.com/".to_owned()),
            executable_path: None,
            working_directory: None,
            arguments: Vec::new(),
            status: ProviderStatusView::Offline,
            model: Some(model.to_owned()),
            context_window_tokens: None,
            credential_configured: false,
            capabilities: Some(default_capabilities(
                &ProviderKindView::Openai,
                role,
                ProviderModeView::CloudRemote,
            )),
            capability_source: Some("test".to_owned()),
            capability_updated_at: Some(Utc::now()),
            last_error: None,
        };
        persist_provider(&state, &profile, None)
            .await
            .expect("persist role-specific OpenAI connection");
    }

    let tts = state
        .database
        .repositories()
        .providers
        .get(audiobookai_core::ProviderProfileId::from_uuid(tts_id))
        .await
        .expect("read TTS provider")
        .expect("stored TTS provider");
    let llm = state
        .database
        .repositories()
        .providers
        .get(audiobookai_core::ProviderProfileId::from_uuid(llm_id))
        .await
        .expect("read LLM provider")
        .expect("stored LLM provider");
    assert_eq!(tts.role, audiobookai_core::ProviderRole::Tts);
    assert_eq!(llm.role, audiobookai_core::ProviderRole::CharacterDetection);
    assert_eq!(
        tts.settings
            .0
            .get("model")
            .and_then(serde_json::Value::as_str),
        Some("tts-1-hd-1106")
    );
    assert_eq!(
        llm.settings
            .0
            .get("model")
            .and_then(serde_json::Value::as_str),
        Some("gpt-5.6-luna")
    );
}

fn gpt6_profile(model: &str) -> ProviderProfileView {
    let mut profile = ProviderProfileView {
        id: Uuid::new_v4(),
        name: "OpenAI".to_owned(),
        kind: ProviderKindView::Openai,
        role: ProviderRoleView::Llm,
        mode: ProviderModeView::CloudRemote,
        endpoint: Some("https://api.openai.com/".to_owned()),
        executable_path: None,
        working_directory: None,
        arguments: Vec::new(),
        status: ProviderStatusView::Online,
        model: Some(model.to_owned()),
        context_window_tokens: None,
        credential_configured: true,
        capabilities: Some(default_capabilities(
            &ProviderKindView::Openai,
            ProviderRoleView::Llm,
            ProviderModeView::CloudRemote,
        )),
        capability_source: Some("built_in_adapter_contract+health_probe".to_owned()),
        capability_updated_at: Some(Utc::now()),
        last_error: None,
    };
    let effort = |level: &str| audiobookai_providers::ReasoningEffort::new(level).unwrap();
    apply_generation_controls(
        &mut profile,
        &audiobookai_providers::ModelGenerationControls {
            temperature: audiobookai_providers::ParameterSupport::Unsupported,
            max_temperature: None,
            reasoning: [
                audiobookai_providers::ReasoningMode::Disabled,
                audiobookai_providers::ReasoningMode::Effort,
            ]
            .into(),
            efforts: ["low", "medium", "high", "xhigh", "max"]
                .map(effort)
                .to_vec(),
            min_token_budget: None,
            max_token_budget: None,
            source: audiobookai_providers::GenerationControlsSource::ValidationProbe,
        },
        Some(model.to_owned()),
    );
    profile
}

#[test]
fn only_options_determined_for_the_selected_model_are_accepted() {
    use audiobookai_providers::{ReasoningControl, ReasoningEffort, Temperature};

    let profile = gpt6_profile("gpt-6-luna");
    let capabilities = profile.capabilities.as_ref().unwrap();
    assert_eq!(capabilities.reasoning, ["disabled", "effort"]);
    assert_eq!(
        capabilities.reasoning_efforts,
        ["low", "medium", "high", "xhigh", "max"]
    );
    assert_eq!(capabilities.temperature, "unsupported");

    let controls = model_generation_controls_for(&profile, "gpt-6-luna");
    let effort = |level: &str| ReasoningControl::Effort {
        effort: ReasoningEffort::new(level).unwrap(),
    };
    assert!(
        controls
            .validate(Temperature::Default, &effort("xhigh"))
            .is_ok()
    );
    assert!(
        controls
            .validate(Temperature::Default, &ReasoningControl::Disabled)
            .is_ok()
    );
    assert!(
        controls
            .validate(Temperature::Default, &effort("minimal"))
            .is_err()
    );
    assert!(
        controls
            .validate(Temperature::Value(0.5), &ReasoningControl::Inherit)
            .is_err()
    );
    assert!(
        controls
            .validate(Temperature::Default, &ReasoningControl::Inherit)
            .is_ok()
    );

    // Options determined for another model never carry over.
    let other = model_generation_controls_for(&profile, "gpt-5");
    assert!(
        other
            .validate(Temperature::Default, &effort("low"))
            .is_err()
    );
    assert!(
        other
            .validate(Temperature::Default, &ReasoningControl::Inherit)
            .is_ok()
    );
}

#[tokio::test]
async fn model_generation_controls_survive_a_restart() {
    let directory = tempfile::tempdir().expect("tempdir");
    let config = crate::ServiceConfig {
        bind: "127.0.0.1:0".parse().expect("address"),
        data_dir: directory.path().to_path_buf(),
        bundled_sidecar_dir: None,
        tls: None,
        lan_hostnames: Vec::new(),
        allow_insecure_lan: false,
        desktop_bootstrap: false,
    };
    let profile = gpt6_profile("gpt-6-luna");
    {
        let database = audiobookai_storage::Database::open_in(directory.path())
            .await
            .expect("database");
        let state = AppState::new(config.clone(), database)
            .await
            .expect("state");
        persist_provider(&state, &profile, None)
            .await
            .expect("persist provider");
        state.database.close().await;
    }
    let database = audiobookai_storage::Database::open_in(directory.path())
        .await
        .expect("database");
    let state = AppState::new(config, database).await.expect("state");
    let restored = state.catalog.read().await.providers[&profile.id].clone();
    let capabilities = restored.capabilities.expect("capabilities");
    assert_eq!(capabilities.reasoning, ["disabled", "effort"]);
    assert_eq!(
        capabilities.reasoning_efforts,
        ["low", "medium", "high", "xhigh", "max"]
    );
    assert_eq!(capabilities.temperature, "unsupported");
    assert_eq!(
        capabilities.generation_controls_model.as_deref(),
        Some("gpt-6-luna")
    );
}

#[tokio::test]
async fn successful_llm_health_probe_replaces_the_hydrated_offline_status() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("fixture listener");
    let address = listener.local_addr().expect("fixture address");
    let server = tokio::spawn(async move {
        axum::serve(
            listener,
            axum::Router::new().route(
                "/v1/models",
                axum::routing::get(|| async { axum::Json(serde_json::json!({ "data": [] })) }),
            ),
        )
        .await
        .expect("fixture server");
    });
    let directory = tempfile::tempdir().expect("tempdir");
    let database = audiobookai_storage::Database::open_in(directory.path())
        .await
        .expect("database");
    let state = AppState::new(
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
    .expect("state");
    let id = Uuid::new_v4();
    let profile = ProviderProfileView {
        id,
        name: "Loopback LLM".to_owned(),
        kind: ProviderKindView::OpenaiCompatible,
        role: ProviderRoleView::Llm,
        mode: ProviderModeView::ExternalEndpoint,
        endpoint: Some(format!("http://{address}/")),
        executable_path: None,
        working_directory: None,
        arguments: Vec::new(),
        status: ProviderStatusView::Offline,
        model: Some("fixture-chat".to_owned()),
        context_window_tokens: None,
        credential_configured: false,
        capabilities: Some(default_capabilities(
            &ProviderKindView::OpenaiCompatible,
            ProviderRoleView::Llm,
            ProviderModeView::ExternalEndpoint,
        )),
        capability_source: Some("built_in_adapter_contract".to_owned()),
        capability_updated_at: Some(Utc::now()),
        last_error: None,
    };
    persist_provider(&state, &profile, None)
        .await
        .expect("persist provider");
    state.catalog.write().await.providers.insert(id, profile);
    state
        .sync_provider_runtime(id)
        .await
        .expect("register runtime");

    let refreshed = refresh_provider(&state, id).await.expect("health refresh");

    assert!(matches!(refreshed.status, ProviderStatusView::Online));
    assert_eq!(
        refreshed.capability_source.as_deref(),
        Some("built_in_adapter_contract+health_probe+generation_controls:adapter_contract")
    );
    let capabilities = refreshed.capabilities.as_ref().expect("capabilities");
    assert_eq!(
        capabilities.generation_controls_model.as_deref(),
        Some("fixture-chat")
    );
    server.abort();
}

#[tokio::test]
async fn provider_listing_recovers_an_external_lm_studio_started_late() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("fixture listener");
    let address = listener.local_addr().expect("fixture address");
    let server = tokio::spawn(async move {
        axum::serve(
            listener,
            axum::Router::new().route(
                "/v1/models",
                axum::routing::get(|| async { axum::Json(serde_json::json!({ "data": [] })) }),
            ),
        )
        .await
        .expect("fixture server");
    });
    let directory = tempfile::tempdir().expect("tempdir");
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
        .expect("state"),
    );
    let id = Uuid::new_v4();
    let profile = ProviderProfileView {
        id,
        name: "LM Studio".to_owned(),
        kind: ProviderKindView::LmStudio,
        role: ProviderRoleView::Llm,
        mode: ProviderModeView::ExternalEndpoint,
        endpoint: Some(format!("http://{address}/")),
        executable_path: None,
        working_directory: None,
        arguments: Vec::new(),
        status: ProviderStatusView::Offline,
        model: Some("fixture-chat".to_owned()),
        context_window_tokens: None,
        credential_configured: false,
        capabilities: Some(default_capabilities(
            &ProviderKindView::LmStudio,
            ProviderRoleView::Llm,
            ProviderModeView::ExternalEndpoint,
        )),
        capability_source: Some("built_in_adapter_contract".to_owned()),
        capability_updated_at: Some(Utc::now()),
        last_error: None,
    };
    persist_provider(&state, &profile, None)
        .await
        .expect("persist provider");
    state.catalog.write().await.providers.insert(id, profile);
    state
        .sync_provider_runtime(id)
        .await
        .expect("register runtime");

    recover_stale_external_lm_studio_statuses(&state).await;

    assert!(matches!(
        state
            .catalog
            .read()
            .await
            .providers
            .get(&id)
            .map(|profile| profile.status),
        Some(ProviderStatusView::Online)
    ));
    server.abort();
}

#[test]
fn built_in_capabilities_only_advertise_reachable_controls() {
    let local_ai = default_capabilities(
        &ProviderKindView::Localai,
        ProviderRoleView::Tts,
        ProviderModeView::ManagedChild,
    );
    assert!(local_ai.tts);
    assert!(!local_ai.character_detection);
    assert!(!local_ai.voice_cloning);
    assert!(local_ai.process_control);
    assert!(local_ai.model_control);
    assert!(local_ai.model_list);
    assert!(local_ai.model_download);
    assert!(local_ai.model_delete);
    assert!(local_ai.model_load);
    assert!(local_ai.model_unload);
    assert!(!local_ai.model_switch);

    let ollama = default_capabilities(
        &ProviderKindView::Ollama,
        ProviderRoleView::Llm,
        ProviderModeView::ExternalEndpoint,
    );
    assert!(!ollama.tts);
    assert!(ollama.character_detection);
    assert!(ollama.model_control);
    assert_eq!(ollama.temperature, "number");
    assert_eq!(ollama.reasoning, ["disabled", "effort"]);

    let openai_speech = default_capabilities(
        &ProviderKindView::Openai,
        ProviderRoleView::Tts,
        ProviderModeView::CloudRemote,
    );
    assert!(openai_speech.tts);
    assert!(openai_speech.streaming);
    assert!(!openai_speech.character_detection);
    assert_eq!(openai_speech.temperature, "unsupported");
    assert!(
        openai_speech
            .model_performance
            .iter()
            .any(|capability| capability.model == "gpt-4o-mini-tts")
    );
}

#[test]
fn ollama_deletion_guards_canonicalize_latest_and_include_character_assignments() {
    let project_id = Uuid::new_v4();
    let provider_id = Uuid::new_v4();
    let other_provider_id = Uuid::new_v4();
    let character = crate::models::CharacterView {
        id: Uuid::new_v4(),
        role: audiobookai_core::CharacterRole::Character,
        canonical_name: "Character".to_owned(),
        aliases: Vec::new(),
        confidence: 1.0,
        dialogue_count: 1,
        voice_assignment: Some(VoiceAssignmentView {
            provider_profile_id: provider_id,
            provider_name: "Ollama".to_owned(),
            voice_id: Uuid::new_v4(),
            voice_name: "Voice".to_owned(),
            model: Some("gemma3:latest".to_owned()),
            performance: audiobookai_core::PerformanceSettings::default(),
            timing: audiobookai_core::TimingSettings::default(),
        }),
        evidence: Vec::new(),
    };
    let characters = HashMap::from([(project_id, vec![character])]);

    assert!(character_assignments_reference_provider_model(
        &characters,
        provider_id,
        &ProviderKindView::Ollama,
        "gemma3"
    ));
    assert!(!character_assignments_reference_provider_model(
        &characters,
        other_provider_id,
        &ProviderKindView::Ollama,
        "gemma3"
    ));
    assert!(payload_references_provider_model(
        &serde_json::json!({"settings": {"model": "gemma3:latest"}}),
        &ProviderKindView::Ollama,
        "gemma3"
    ));
    assert!(!payload_references_provider_model(
        &serde_json::json!({"settings": {"model": "gemma3:v2"}}),
        &ProviderKindView::Ollama,
        "gemma3"
    ));
    assert!(provider_models_equal(
        &ProviderKindView::Localai,
        "localai@voice-model",
        "voice-model"
    ));
}

#[tokio::test]
#[allow(clippy::too_many_lines)]
async fn provider_model_removal_detects_durable_assignment_missing_from_catalog() {
    let directory = tempfile::tempdir().expect("tempdir");
    let database = audiobookai_storage::Database::open_in(directory.path())
        .await
        .expect("database");
    let state = AppState::new(
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
    .expect("state");
    let provider_id = Uuid::new_v4();
    let provider = ProviderProfileView {
        id: provider_id,
        name: "Ollama fixture".to_owned(),
        kind: ProviderKindView::Ollama,
        role: ProviderRoleView::Llm,
        mode: ProviderModeView::ExternalEndpoint,
        endpoint: Some("http://127.0.0.1:11434/".to_owned()),
        executable_path: None,
        working_directory: None,
        arguments: Vec::new(),
        status: ProviderStatusView::Offline,
        model: None,
        context_window_tokens: None,
        credential_configured: false,
        capabilities: Some(default_capabilities(
            &ProviderKindView::Ollama,
            ProviderRoleView::Llm,
            ProviderModeView::ExternalEndpoint,
        )),
        capability_source: Some("test".to_owned()),
        capability_updated_at: Some(Utc::now()),
        last_error: None,
    };
    persist_provider(&state, &provider, None)
        .await
        .expect("persist provider");
    state
        .catalog
        .write()
        .await
        .providers
        .insert(provider_id, provider);

    let book_id = Uuid::new_v4();
    let project_id = Uuid::new_v4();
    let voice_id = Uuid::new_v4();
    let assignment_id = Uuid::new_v4();
    let now_at = Utc::now();
    let now = now_at.to_rfc3339();
    sqlx::query(
        "INSERT INTO books (id, managed_epub_path, source_hash, imported_at, payload) \
             VALUES (?, ?, ?, ?, ?)",
    )
    .bind(book_id.to_string())
    .bind(
        directory
            .path()
            .join("fixture.epub")
            .to_string_lossy()
            .into_owned(),
    )
    .bind("fixture-hash")
    .bind(&now)
    .bind("{}")
    .execute(state.database.pool())
    .await
    .expect("book row");
    sqlx::query(
        "INSERT INTO projects \
             (id, book_id, name, status, created_at, updated_at, revision, payload) \
             VALUES (?, ?, ?, ?, ?, ?, 0, ?)",
    )
    .bind(project_id.to_string())
    .bind(book_id.to_string())
    .bind("Fixture")
    .bind("ready")
    .bind(&now)
    .bind(&now)
    .bind("{}")
    .execute(state.database.pool())
    .await
    .expect("project row");
    sqlx::query(
        "INSERT INTO voice_profiles \
             (id, provider_id, name, origin, ownership, provider_voice_id, updated_at, payload) \
             VALUES (?, ?, ?, ?, ?, ?, ?, ?)",
    )
    .bind(voice_id.to_string())
    .bind(provider_id.to_string())
    .bind("Fixture voice")
    .bind("provider_catalog")
    .bind("provider")
    .bind("fixture-voice")
    .bind(&now)
    .bind("{}")
    .execute(state.database.pool())
    .await
    .expect("voice row");
    let durable_assignment = audiobookai_core::VoiceAssignment {
        id: audiobookai_core::VoiceAssignmentId::from_uuid(assignment_id),
        project_id: audiobookai_core::ProjectId::from_uuid(project_id),
        speaker: audiobookai_core::Speaker::Narrator,
        voice_profile_id: audiobookai_core::VoiceProfileId::from_uuid(voice_id),
        provider_profile_id: audiobookai_core::ProviderProfileId::from_uuid(provider_id),
        model: Some("gemma3:latest".to_owned()),
        performance: audiobookai_core::PerformanceSettings::default(),
        timing: audiobookai_core::TimingSettings::default(),
        settings: BTreeMap::new(),
        created_at: now_at,
        updated_at: now_at,
    };
    sqlx::query(
        "INSERT INTO voice_assignments \
             (id, project_id, provider_id, voice_profile_id, speaker_key, updated_at, payload) \
             VALUES (?, ?, ?, ?, ?, ?, ?)",
    )
    .bind(assignment_id.to_string())
    .bind(project_id.to_string())
    .bind(provider_id.to_string())
    .bind(voice_id.to_string())
    .bind("narrator")
    .bind(&now)
    .bind(serde_json::to_string(&durable_assignment).expect("assignment payload"))
    .execute(state.database.pool())
    .await
    .expect("assignment row");

    assert!(
        provider_model_is_in_use(&state, provider_id, "gemma3")
            .await
            .expect("in-use check")
    );
    sqlx::query("UPDATE voice_assignments SET payload = ? WHERE id = ?")
        .bind("{}")
        .bind(assignment_id.to_string())
        .execute(state.database.pool())
        .await
        .expect("corrupt assignment payload");
    assert!(
        provider_model_is_in_use(&state, provider_id, "gemma3")
            .await
            .is_err(),
        "unverifiable durable assignment metadata must block deletion"
    );
}

#[tokio::test]
async fn model_lifecycle_lock_serializes_delete_and_reference_creation_windows() {
    let directory = tempfile::tempdir().expect("tempdir");
    let database = audiobookai_storage::Database::open_in(directory.path())
        .await
        .expect("database");
    let state = AppState::new(
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
    .expect("state");
    let deletion_guard = state.model_lifecycle.lock().await;
    let contender = Arc::clone(&state.model_lifecycle);
    let (acquired_tx, mut acquired_rx) = tokio::sync::oneshot::channel();
    let waiter = tokio::spawn(async move {
        let _reference_guard = contender.lock().await;
        let _ = acquired_tx.send(());
    });

    assert!(
        tokio::time::timeout(Duration::from_millis(25), &mut acquired_rx)
            .await
            .is_err(),
        "reference creation must wait for the delete window"
    );
    drop(deletion_guard);
    tokio::time::timeout(Duration::from_secs(1), acquired_rx)
        .await
        .expect("reference lock acquisition timed out")
        .expect("reference waiter stopped");
    waiter.await.expect("reference waiter");
}

#[tokio::test]
async fn runtime_affecting_provider_patch_waits_for_dispatch_lifecycle() {
    let directory = tempfile::tempdir().expect("tempdir");
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
        .expect("state"),
    );
    let lifecycle_guard = state.model_lifecycle.lock().await;
    let contender = Arc::clone(&state);
    let missing_provider_id = Uuid::new_v4();
    let mut patch = tokio::spawn(async move {
        update_provider(
            State(contender),
            Path(missing_provider_id),
            Json(ProviderProfileInput {
                name: None,
                kind: None,
                role: None,
                mode: None,
                endpoint: Some(Some("http://127.0.0.1:9999/".to_owned())),
                executable_path: None,
                working_directory: None,
                arguments: None,
                model: None,
                context_window_tokens: None,
                credential: None,
            }),
        )
        .await
    });

    assert!(
        tokio::time::timeout(Duration::from_millis(25), &mut patch)
            .await
            .is_err(),
        "provider routing changes must wait for the dispatch validation window"
    );
    drop(lifecycle_guard);
    assert!(matches!(
        tokio::time::timeout(Duration::from_secs(1), patch)
            .await
            .expect("provider patch timed out")
            .expect("provider patch task"),
        Err(ServiceError::NotFound)
    ));
}

#[tokio::test]
async fn consent_revocation_waits_for_an_in_flight_dispatch_boundary() {
    let directory = tempfile::tempdir().expect("tempdir");
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
        .expect("state"),
    );
    let project_id = Uuid::new_v4();
    let lifecycle = state.dispatch_consent_lifecycle_lock(project_id).await;
    let dispatch_guard = lifecycle.read().await;
    let contender = Arc::clone(&state);
    let mut revocation = tokio::spawn(async move {
        update_project(
            State(contender),
            Path(project_id),
            Json(serde_json::json!({ "consentCloudText": false })),
        )
        .await
    });

    assert!(
        tokio::time::timeout(Duration::from_millis(25), &mut revocation)
            .await
            .is_err(),
        "consent revocation must wait until the provider dispatch returns"
    );
    drop(dispatch_guard);
    assert!(matches!(
        tokio::time::timeout(Duration::from_secs(1), revocation)
            .await
            .expect("consent revocation timed out")
            .expect("consent revocation task"),
        Err(ServiceError::NotFound)
    ));
}

#[test]
fn owner_audio_and_job_defaults_are_validated_and_applied() {
    let mut settings = AppSettingsView::defaults(std::path::Path::new("/managed"));
    apply_owner_settings_patch(
        &mut settings,
        &serde_json::json!({
            "cacheLimitBytes": 12_000_000_000_u64,
            "defaultConcurrency": 7,
            "defaultRetryCount": 0,
            "defaultLufs": -18.5,
            "defaultTruePeakDb": -2.5,
        }),
    )
    .expect("valid settings patch");

    assert_eq!(settings.cache_limit_bytes, 12_000_000_000);
    assert_eq!(settings.default_concurrency, 7);
    assert_eq!(settings.default_retry_count, 0);
    assert!((settings.default_lufs - -18.5).abs() < f32::EPSILON);
    assert!((settings.default_true_peak_db - -2.5).abs() < f32::EPSILON);

    let project_settings = imported_project_settings(&settings, "{title}".to_owned());
    assert_eq!(project_settings.global_chapter_concurrency, 7);
    assert_eq!(project_settings.reliability.max_transient_retries, 0);
}

#[test]
fn owner_settings_reject_unsafe_ranges_and_managed_path_changes() {
    for invalid in [
        serde_json::json!({"cacheLimitBytes": 999_999_999_u64}),
        serde_json::json!({"defaultConcurrency": 0}),
        serde_json::json!({"defaultConcurrency": 33}),
        serde_json::json!({"defaultRetryCount": 11}),
        serde_json::json!({"defaultLufs": -30.5}),
        serde_json::json!({"defaultLufs": -9.5}),
        serde_json::json!({"defaultTruePeakDb": -10.5}),
        serde_json::json!({"defaultTruePeakDb": 0.5}),
        serde_json::json!({"libraryPath": "/tmp/moved-library"}),
        serde_json::json!({"cachePath": "/tmp/moved-cache"}),
    ] {
        let mut settings = AppSettingsView::defaults(std::path::Path::new("/managed"));
        assert!(
            apply_owner_settings_patch(&mut settings, &invalid).is_err(),
            "patch should be rejected: {invalid}"
        );
    }
}

#[test]
fn mlx_runtime_uninstall_requires_explicit_confirmation() {
    assert!(require_mlx_uninstall_confirmation(false).is_err());
    assert!(require_mlx_uninstall_confirmation(true).is_ok());
}

#[tokio::test]
async fn mlx_model_removal_detects_character_specific_assignment() {
    let directory = tempfile::tempdir().expect("tempdir");
    let database = audiobookai_storage::Database::open_in(directory.path())
        .await
        .expect("database");
    let state = AppState::new(
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
    .expect("state");
    let provider_id = Uuid::new_v4();
    let model_path = directory
        .path()
        .join("managed-providers/mlx-audio/models")
        .join(Uuid::new_v4().to_string())
        .join("payload");
    tokio::fs::create_dir_all(&model_path)
        .await
        .expect("model payload");
    let model_alias = model_path.join("..").join("payload");
    let provider = ProviderProfileView {
        id: provider_id,
        name: "MLX fixture".to_owned(),
        kind: ProviderKindView::MlxAudio,
        role: ProviderRoleView::Tts,
        mode: ProviderModeView::ManagedChild,
        endpoint: Some("http://127.0.0.1:8000/".to_owned()),
        executable_path: Some("/app-owned/mlx_audio.server".to_owned()),
        working_directory: Some("/app-owned".to_owned()),
        arguments: Vec::new(),
        status: ProviderStatusView::Offline,
        model: None,
        context_window_tokens: None,
        credential_configured: false,
        capabilities: None,
        capability_source: None,
        capability_updated_at: None,
        last_error: None,
    };
    let project_id = Uuid::new_v4();
    {
        let mut catalog = state.catalog.write().await;
        catalog.providers.insert(provider_id, provider);
        catalog.characters.insert(
            project_id,
            vec![crate::models::CharacterView {
                id: Uuid::new_v4(),
                role: audiobookai_core::CharacterRole::Character,
                canonical_name: "Character".to_owned(),
                aliases: Vec::new(),
                confidence: 1.0,
                dialogue_count: 1,
                voice_assignment: Some(VoiceAssignmentView {
                    provider_profile_id: provider_id,
                    provider_name: "MLX fixture".to_owned(),
                    voice_id: Uuid::new_v4(),
                    voice_name: "Voice".to_owned(),
                    model: Some(model_alias.to_string_lossy().into_owned()),
                    performance: audiobookai_core::PerformanceSettings::default(),
                    timing: audiobookai_core::TimingSettings::default(),
                }),
                evidence: Vec::new(),
            }],
        );
    }

    assert!(
        mlx_model_is_in_use(&state, &model_path)
            .await
            .expect("in-use check")
    );
}

#[tokio::test]
async fn mlx_model_removal_payload_checks_are_canonical_recursive_and_fail_closed() {
    let directory = tempfile::tempdir().expect("tempdir");
    let database = audiobookai_storage::Database::open_in(directory.path())
        .await
        .expect("database");
    let state = AppState::new(
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
    .expect("state");
    let model_path = directory
        .path()
        .join("managed-providers/mlx-audio/models")
        .join(Uuid::new_v4().to_string())
        .join("payload");
    tokio::fs::create_dir_all(&model_path)
        .await
        .expect("model payload");
    let canonical = canonical_mlx_model_path(&state, &model_path)
        .await
        .expect("canonical model");
    let alias = model_path.join("..").join("payload");
    let nested = serde_json::json!({"segment": {"settings": {"model": alias}}});
    assert!(
        payload_references_model_path(&nested, &canonical)
            .await
            .expect("payload check")
    );
    assert!(
        json_payloads_reference_model_path(vec!["not valid JSON".to_owned()], &canonical)
            .await
            .is_err()
    );
}

#[cfg(unix)]
#[tokio::test]
async fn mlx_model_path_comparison_resolves_symlink_aliases_but_contains_targets() {
    use std::os::unix::fs::symlink;

    let directory = tempfile::tempdir().expect("tempdir");
    let database = audiobookai_storage::Database::open_in(directory.path())
        .await
        .expect("database");
    let state = AppState::new(
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
    .expect("state");
    let model_path = directory
        .path()
        .join("managed-providers/mlx-audio/models")
        .join(Uuid::new_v4().to_string())
        .join("payload");
    tokio::fs::create_dir_all(&model_path)
        .await
        .expect("model payload");
    let alias = directory.path().join("model-alias");
    symlink(&model_path, &alias).expect("symlink alias");
    let canonical = canonical_mlx_model_path(&state, &model_path)
        .await
        .expect("canonical model");

    assert!(
        mlx_model_path_matches(alias.to_str().expect("UTF-8 alias"), &canonical)
            .await
            .expect("alias check")
    );
    assert!(
        canonical_mlx_model_path(&state, directory.path())
            .await
            .is_err(),
        "a deletion target outside managed storage must fail closed"
    );
}
