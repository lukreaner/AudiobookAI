// The tests exercise private helpers of every detection stage.
#![allow(clippy::wildcard_imports)]

use audiobookai_providers::ReasoningEffort;

use super::{batching::*, cast::*, execution::*, persistence::*, recovery::*, units::*, *};

fn detected(name: &str, aliases: &[&str]) -> DetectedCharacter {
    DetectedCharacter {
        canonical_name: name.to_owned(),
        aliases: aliases.iter().map(|alias| (*alias).to_owned()).collect(),
        confidence: 0.8,
    }
}

fn spoken(character: &str, start: u32) -> DetectedDialogue {
    DetectedDialogue {
        paragraph_id: "paragraph".to_owned(),
        character: character.to_owned(),
        start,
        end: start + 1,
        confidence: 0.9,
    }
}

#[test]
fn cross_batch_names_merge_into_one_speaker() {
    let result = canonicalize_detection_result(CharacterDetectionResult {
        characters: vec![
            detected("Harry Potter", &["Harry"]),
            detected("Harry", &["Potter"]),
            detected("Harry Potter", &[]),
            detected("Hermione", &[]),
        ],
        dialogue: vec![
            spoken("Harry", 0),
            spoken("harry potter", 2),
            spoken("Potter", 4),
            spoken("Hermione", 6),
        ],
        usage: ProviderUsage::default(),
    });

    let names = result
        .characters
        .iter()
        .map(|character| character.canonical_name.as_str())
        .collect::<Vec<_>>();
    assert_eq!(names, ["Harry Potter", "Hermione"]);
    assert!(
        result.characters[0]
            .aliases
            .iter()
            .any(|alias| alias == "Harry")
    );
    assert!(
        result.dialogue[..3]
            .iter()
            .all(|dialogue| dialogue.character == "Harry Potter")
    );
}

#[test]
fn a_shared_alias_does_not_merge_different_people() {
    let result = canonicalize_detection_result(CharacterDetectionResult {
        characters: vec![
            detected("Arthur Weasley", &["Dad"]),
            detected("Vernon Dursley", &["Dad"]),
        ],
        dialogue: vec![spoken("Arthur Weasley", 0), spoken("Vernon Dursley", 2)],
        usage: ProviderUsage::default(),
    });

    assert_eq!(result.characters.len(), 2);
    assert_eq!(result.dialogue[0].character, "Arthur Weasley");
    assert_eq!(result.dialogue[1].character, "Vernon Dursley");
}

#[test]
fn undeclared_speakers_are_added_and_silent_characters_dropped() {
    let result = canonicalize_detection_result(CharacterDetectionResult {
        characters: vec![detected("Jürgen", &[]), detected("Mentioned Only", &[])],
        dialogue: vec![spoken("JÜRGEN", 0), spoken("Käthe", 2), spoken("  ", 4)],
        usage: ProviderUsage::default(),
    });

    let names = result
        .characters
        .iter()
        .map(|character| character.canonical_name.as_str())
        .collect::<Vec<_>>();
    assert_eq!(names, ["Jürgen", "Käthe"]);
    assert_eq!(result.dialogue.len(), 2);
    assert_eq!(result.dialogue[0].character, "Jürgen");
}

#[test]
fn repair_requests_keep_the_complete_task_instructions() {
    let request = detection_request(
        "model",
        &[],
        1_024,
        true,
        Temperature::Default,
        ReasoningControl::Inherit,
    );
    assert!(request.system_prompt.starts_with(DETECTION_SYSTEM_PROMPT));
    assert!(request.system_prompt.contains("could not be parsed"));
    assert!(DETECTION_SYSTEM_PROMPT.contains("quote_start"));
    assert!(!DETECTION_SYSTEM_PROMPT.contains("byte offset"));
}

fn config() -> DetectionJobConfig {
    DetectionJobConfig {
        schema_version: DETECTION_JOB_SCHEMA_VERSION,
        provider_profile_id: Uuid::new_v4(),
        model: "model".to_owned(),
        provider_endpoint: Some("http://127.0.0.1:1234".to_owned()),
        provider_mode: Some(ProviderModeView::ExternalEndpoint),
        provider_snapshot_id: Some(Uuid::new_v4()),
        temperature: Temperature::Default,
        reasoning: ReasoningControl::Inherit,
        context_window_tokens: Some(4_096),
        max_output_tokens: Some(1_024),
        detection_run_id: DetectionRunId::new(),
        base_character_revision: 0,
    }
}

fn source_paragraph(text: impl Into<String>) -> DetectionSourceParagraph {
    DetectionSourceParagraph {
        id: ParagraphId::new(),
        text: text.into(),
        hash: "hash".to_owned(),
        chapter_title: "Chapter".to_owned(),
        chapter_id: audiobookai_core::ChapterId::new(),
    }
}

fn unit(state: JobUnitState) -> JobUnit {
    let config = config();
    let mut unit = detection_unit(
        Uuid::new_v4(),
        config.provider_profile_id,
        0,
        &config,
        &UsageQuantities::default(),
        None,
    )
    .expect("detection unit");
    unit.state = state;
    unit
}

fn attempt(
    unit_id: JobUnitId,
    finished: bool,
    failure_class: Option<CoreFailureClass>,
    uncertain_charge: bool,
) -> JobAttempt {
    let now = Utc::now();
    JobAttempt {
        id: AttemptId::new(),
        job_unit_id: unit_id,
        ordinal: 1,
        started_at: now,
        finished_at: finished.then_some(now),
        failure_class,
        error_code: None,
        redacted_error: None,
        provider_request_id: None,
        uncertain_charge,
    }
}

#[test]
fn detection_request_preserves_explicit_provider_controls() {
    let request = detection_request(
        "model",
        &[],
        1_024,
        false,
        Temperature::Null,
        ReasoningControl::Effort {
            effort: ReasoningEffort::new("high").expect("effort"),
        },
    );

    assert_eq!(request.temperature, Temperature::Null);
    assert_eq!(
        request.reasoning,
        ReasoningControl::Effort {
            effort: ReasoningEffort::new("high").expect("effort"),
        }
    );
}

#[test]
fn displayed_detection_progress_is_bounded_without_lossy_integer_casts() {
    assert!(progress_fraction(0, 0).abs() < f32::EPSILON);
    assert!((progress_fraction(1, 2) - 50.0).abs() < f32::EPSILON);
    assert!((progress_fraction(2, 1) - 100.0).abs() < f32::EPSILON);
}

#[test]
fn four_thousand_token_context_builds_only_batches_that_fit() {
    let budget = DetectionContextBudget::new(4_096).expect("4K context budget");
    assert_eq!(budget.max_output, 1_024);
    let paragraphs = (0..30)
        .map(|index| source_paragraph(format!("Paragraph {index}: {}", "dialogue ".repeat(50))))
        .collect::<Vec<_>>();
    let batches =
        paragraph_batches(&paragraphs, budget, DetectionBatching::V5).expect("token-aware batches");

    assert!(batches.len() > 1);
    let mut core_sources = BTreeSet::new();
    for batch in &batches {
        let estimate = detection_request_estimate(batch, &ReasoningControl::Inherit);
        assert!(
            estimate.input_tokens.unwrap() + estimate.output_tokens.unwrap() + budget.safety_margin
                <= budget.context_window
        );
        assert!(batch.core_paragraphs().len() <= DETECTION_BATCH_PARAGRAPHS);
        for fragment in batch.core_paragraphs() {
            assert!(core_sources.insert(fragment.source_id));
        }
    }
    assert_eq!(core_sources.len(), paragraphs.len());
}

#[test]
fn current_batching_packs_more_context_while_fitting_real_token_counts() {
    let budget = DetectionContextBudget::new(4_096).expect("4K context budget");
    let paragraphs = (0..60)
        .map(|index| {
            source_paragraph(format!(
                "„Absatz {index}“, sagte Käthe. {}",
                "Wörter ".repeat(30)
            ))
        })
        .collect::<Vec<_>>();
    let legacy = paragraph_batches(&paragraphs, budget, DetectionBatching::V5).unwrap();
    let current = paragraph_batches(&paragraphs, budget, DetectionBatching::CURRENT).unwrap();

    assert!(
        current.len() * 3 <= legacy.len() * 2,
        "{} vs {}",
        current.len(),
        legacy.len()
    );
    for batch in &current {
        // Even at the conservative two bytes per token, the request fits the loaded window.
        let text_tokens = batch
            .paragraphs
            .iter()
            .map(|paragraph| DetectionBatching::CURRENT.fragment_tokens(&paragraph.fragment))
            .sum::<u64>();
        assert!(
            DETECTION_PROMPT_TOKEN_RESERVE
                + text_tokens
                + u64::from(batch.max_output_tokens)
                + budget.safety_margin
                <= budget.context_window
        );
        assert!(batch.core_paragraphs().len() <= DetectionBatching::CURRENT.max_paragraphs);
    }
}

#[test]
fn oversized_paragraphs_are_split_and_dialogue_offsets_are_rebased() {
    let budget = DetectionContextBudget::new(4_096).expect("4K context budget");
    let source = source_paragraph("Ärger und Dialog. ".repeat(700));
    let source_id = source.id.to_string();
    let batches = paragraph_batches(std::slice::from_ref(&source), budget, DetectionBatching::V5)
        .expect("split paragraph batches");
    let batch = batches
        .iter()
        .find(|batch| {
            batch
                .core_paragraphs()
                .iter()
                .any(|fragment| fragment.source_byte_start > 0)
        })
        .expect("a later paragraph fragment");
    let fragment = batch
        .core_paragraphs()
        .into_iter()
        .find(|fragment| fragment.source_byte_start > 0)
        .expect("later fragment");
    let first_character_bytes = fragment
        .text
        .chars()
        .next()
        .expect("fragment text")
        .len_utf8();
    let result = rebase_detection_result(
        CharacterDetectionResult {
            characters: Vec::new(),
            dialogue: vec![DetectedDialogue {
                paragraph_id: fragment.request_id,
                character: "Speaker".to_owned(),
                start: 0,
                end: u32::try_from(first_character_bytes).unwrap(),
                confidence: 0.9,
            }],
            usage: ProviderUsage::default(),
        },
        batch,
    )
    .expect("rebase dialogue");

    assert!(batches.len() > 1);
    assert_eq!(result.dialogue[0].paragraph_id, source_id);
    assert_eq!(
        result.dialogue[0].start,
        u32::try_from(fragment.source_byte_start).unwrap()
    );
    assert_eq!(
        result.dialogue[0].end,
        u32::try_from(fragment.source_byte_start + first_character_bytes).unwrap()
    );
}

#[test]
fn context_error_fallback_halves_the_core_batch_and_drops_overlap() {
    let fragments = (0..4)
        .map(|index| DetectionBatchParagraph {
            fragment: DetectionFragment {
                request_id: format!("p{index}"),
                source_id: format!("p{index}"),
                source_byte_start: 0,
                text: "dialogue".to_owned(),
            },
            context_only: index == 0,
        })
        .collect();
    let batch = DetectionBatch {
        paragraphs: fragments,
        max_output_tokens: 1_024,
    };
    let children = batch.split_for_context_retry().expect("context split");

    assert_eq!(children.len(), 2);
    assert_eq!(
        children
            .iter()
            .map(|child| child.paragraphs.len())
            .sum::<usize>(),
        3
    );
    assert!(
        children
            .iter()
            .flat_map(|child| &child.paragraphs)
            .all(|paragraph| !paragraph.context_only)
    );
    assert!(children.iter().all(|child| child.max_output_tokens == 512));
}

#[test]
fn output_truncation_fallback_halves_the_core_batch_and_preserves_output_budget() {
    let fragments = (0..4)
        .map(|index| DetectionBatchParagraph {
            fragment: DetectionFragment {
                request_id: format!("p{index}"),
                source_id: format!("p{index}"),
                source_byte_start: 0,
                text: "dialogue".to_owned(),
            },
            context_only: index == 0,
        })
        .collect();
    let batch = DetectionBatch {
        paragraphs: fragments,
        max_output_tokens: 1_024,
    };
    let children = batch.split_for_output_retry().expect("output split");

    assert_eq!(children.len(), 2);
    assert_eq!(
        children
            .iter()
            .map(|child| child.paragraphs.len())
            .sum::<usize>(),
        3
    );
    assert!(
        children
            .iter()
            .flat_map(|child| &child.paragraphs)
            .all(|paragraph| !paragraph.context_only)
    );
    assert!(
        children
            .iter()
            .all(|child| child.max_output_tokens == 1_024)
    );
}

#[test]
fn output_truncation_splits_one_fragment_without_losing_source_offsets() {
    let source_text = "First half. Second half.";
    let batch = DetectionBatch {
        paragraphs: vec![DetectionBatchParagraph {
            fragment: DetectionFragment {
                request_id: "p1@11".to_owned(),
                source_id: "p1".to_owned(),
                source_byte_start: 11,
                text: source_text.to_owned(),
            },
            context_only: false,
        }],
        max_output_tokens: 1_024,
    };
    let children = batch.split_for_output_retry().expect("fragment split");
    let fragments = children
        .iter()
        .flat_map(DetectionBatch::core_paragraphs)
        .collect::<Vec<_>>();

    assert_eq!(fragments.len(), 2);
    assert_eq!(
        fragments
            .iter()
            .map(|fragment| fragment.text.as_str())
            .collect::<String>(),
        source_text
    );
    assert_eq!(fragments[0].source_byte_start, 11);
    assert_eq!(fragments[1].source_byte_start, 11 + fragments[0].text.len());
    assert!(
        children
            .iter()
            .all(|child| child.max_output_tokens == 1_024)
    );
}

#[test]
fn a_new_detection_run_retains_manual_characters_absent_from_the_model_result() {
    let project_id = Uuid::new_v4();
    let manual_id = CharacterId::new();
    let now = Utc::now();
    let manual = Character {
        id: manual_id,
        project_id: ProjectId::from_uuid(project_id),
        role: audiobookai_core::CharacterRole::Character,
        canonical_name: "Archivist".to_owned(),
        aliases: vec!["The Keeper".to_owned()],
        description: Some("Manually curated".to_owned()),
        confidence: Some(1.0),
        detection_run_id: None,
        manually_created: true,
        created_at: now,
        updated_at: now,
    };
    let previous = BTreeMap::from([("archivist".to_owned(), manual)]);
    let result = CharacterDetectionResult {
        characters: vec![DetectedCharacter {
            canonical_name: "Visitor".to_owned(),
            aliases: Vec::new(),
            confidence: 0.8,
        }],
        dialogue: Vec::new(),
        usage: ProviderUsage::default(),
    };

    let merged = merge_characters(&result, &[], project_id, DetectionRunId::new(), &previous);

    let retained = merged
        .iter()
        .find(|character| character.id == manual_id)
        .expect("manual character is retained");
    assert_eq!(retained.canonical_name, "Archivist");
    assert!(retained.manually_created);
    assert!(merged.iter().any(|character| {
        character.role == audiobookai_core::CharacterRole::Narrator
            && character.canonical_name == "Narrator"
    }));
}

#[test]
fn detection_reservation_estimate_is_conservative_and_keeps_reasoning_separate() {
    let paragraphs = [DetectionParagraph {
        id: "paragraph-1".to_owned(),
        text: "Grüße from the narrator".to_owned(),
        context_only: false,
    }];
    let batch = DetectionBatch {
        paragraphs: vec![DetectionBatchParagraph {
            fragment: DetectionFragment {
                request_id: paragraphs[0].id.clone(),
                source_id: paragraphs[0].id.clone(),
                source_byte_start: 0,
                text: paragraphs[0].text.clone(),
            },
            context_only: false,
        }],
        max_output_tokens: 4_096,
    };
    let estimate = detection_request_estimate(
        &batch,
        &ReasoningControl::Effort {
            effort: ReasoningEffort::new("high").expect("effort"),
        },
    );
    assert_eq!(
        estimate.characters,
        Some(u64::try_from(paragraphs[0].text.chars().count()).unwrap())
    );
    assert!(estimate.input_tokens.unwrap() >= paragraphs[0].text.len() as u64);
    assert_eq!(estimate.output_tokens, Some(4_096));
    assert_eq!(estimate.reasoning_tokens, Some(16_384));
}

#[test]
fn missing_detection_usage_uses_nonzero_estimates_instead_of_zero() {
    let estimate = UsageQuantities {
        characters: Some(900),
        input_tokens: Some(1_500),
        output_tokens: Some(4_096),
        reasoning_tokens: Some(8_192),
        ..UsageQuantities::default()
    };
    let (quantities, source) = merge_detection_usage(&ProviderUsage::default(), &estimate);
    assert_eq!(quantities.input_tokens, Some(1_500));
    assert_eq!(quantities.output_tokens, Some(4_096));
    assert_eq!(quantities.reasoning_tokens, Some(8_192));
    assert_eq!(source, ProvenanceQuality::Estimated);
    assert_eq!(quantities.provider_credits, None);
}

#[test]
fn detection_usage_endpoint_provenance_never_keeps_url_credentials_or_queries() {
    assert_eq!(
        safe_endpoint_family(Some(
            "https://user:credential-placeholder@example.test:8443/v1?debug=removed"
        )),
        "https://example.test:8443"
    );
}

#[test]
fn detection_unit_persists_complete_dispatch_configuration() {
    let config = config();
    let estimate = UsageQuantities {
        input_tokens: Some(1_000),
        output_tokens: Some(4_096),
        ..UsageQuantities::default()
    };
    let unit = detection_unit(
        Uuid::new_v4(),
        config.provider_profile_id,
        3,
        &config,
        &estimate,
        None,
    )
    .expect("detection unit");

    assert_eq!(detection_batch_index(&unit), Some(3));
    assert_eq!(detection_config(&unit).expect("config"), config);
    assert!(
        unit.payload
            .get("usageEventId")
            .and_then(serde_json::Value::as_str)
            .and_then(|value| value.parse::<UsageEventId>().ok())
            .is_some()
    );
    assert_eq!(detection_unit_estimate(&unit).expect("estimate"), estimate);
}

#[test]
fn detection_retry_profile_rejects_model_endpoint_or_mode_drift() {
    let config = config();
    let profile = ProviderProfileView {
        id: config.provider_profile_id,
        name: "Detection provider".to_owned(),
        kind: crate::models::ProviderKindView::OpenaiCompatible,
        role: crate::models::ProviderRoleView::Llm,
        mode: ProviderModeView::ExternalEndpoint,
        endpoint: config.provider_endpoint.clone(),
        executable_path: None,
        working_directory: None,
        arguments: Vec::new(),
        status: ProviderStatusView::Online,
        model: Some(config.model.clone()),
        context_window_tokens: None,
        credential_configured: true,
        capabilities: None,
        capability_source: None,
        capability_updated_at: Some(Utc::now()),
        last_error: None,
    };
    validate_detection_profile(&profile, &config).expect("matching durable profile");

    let mut changed_model = profile.clone();
    changed_model.model = Some("different-model".to_owned());
    assert!(validate_detection_profile(&changed_model, &config).is_err());

    let mut changed_endpoint = profile.clone();
    changed_endpoint.endpoint = Some("http://127.0.0.1:9999".to_owned());
    assert!(validate_detection_profile(&changed_endpoint, &config).is_err());

    let mut changed_mode = profile;
    changed_mode.mode = ProviderModeView::CloudRemote;
    assert!(validate_detection_profile(&changed_mode, &config).is_err());
}

#[test]
fn lm_studio_uses_loaded_context_then_configured_or_safe_default() {
    let config = config();
    let mut profile = ProviderProfileView {
        id: config.provider_profile_id,
        name: "LM Studio".to_owned(),
        kind: crate::models::ProviderKindView::LmStudio,
        role: crate::models::ProviderRoleView::Llm,
        mode: ProviderModeView::ExternalEndpoint,
        endpoint: config.provider_endpoint,
        executable_path: None,
        working_directory: None,
        arguments: Vec::new(),
        status: ProviderStatusView::Online,
        model: Some(config.model),
        context_window_tokens: Some(32_768),
        credential_configured: false,
        capabilities: None,
        capability_source: None,
        capability_updated_at: Some(Utc::now()),
        last_error: None,
    };
    let loaded = audiobookai_providers::ModelContextWindow {
        loaded_tokens: Some(4_096),
        maximum_tokens: Some(262_144),
    };
    assert_eq!(select_effective_context_window(&profile, loaded), 4_096);

    let unloaded = audiobookai_providers::ModelContextWindow {
        loaded_tokens: None,
        maximum_tokens: Some(262_144),
    };
    assert_eq!(select_effective_context_window(&profile, unloaded), 32_768);
    profile.context_window_tokens = None;
    assert_eq!(
        select_effective_context_window(&profile, unloaded),
        LM_STUDIO_DEFAULT_CONTEXT_TOKENS
    );
}

#[test]
fn restart_never_redispatches_an_unfinished_or_lost_successful_billable_request() {
    let mut unit = unit(JobUnitState::Running);
    unit.payload
        .insert("dispatchState".to_owned(), serde_json::json!("dispatched"));
    let unfinished = attempt(unit.id, false, None, true);
    assert_eq!(
        recovery_decision(&unit, Some(&unfinished), false).expect("decision"),
        RecoveryDecision::FailUncertain
    );

    let returned_but_not_persisted = attempt(unit.id, true, None, false);
    unit.payload.insert(
        "dispatchState".to_owned(),
        serde_json::json!("response_received"),
    );
    assert_eq!(
        recovery_decision(&unit, Some(&returned_but_not_persisted), false).expect("decision"),
        RecoveryDecision::FailUncertain
    );
}

#[test]
fn restart_redispatches_an_interrupted_non_billable_local_request() {
    let mut unit = unit(JobUnitState::Running);
    unit.payload
        .insert("dispatchState".to_owned(), serde_json::json!("dispatched"));
    assert_eq!(
        recovery_decision(&unit, None, true).expect("decision"),
        RecoveryDecision::RedispatchSafe
    );
    let unfinished = attempt(unit.id, false, None, true);
    assert_eq!(
        recovery_decision(&unit, Some(&unfinished), true).expect("decision"),
        RecoveryDecision::RedispatchSafe
    );

    let returned_but_not_persisted = attempt(unit.id, true, None, false);
    unit.payload.insert(
        "dispatchState".to_owned(),
        serde_json::json!("response_received"),
    );
    assert_eq!(
        recovery_decision(&unit, Some(&returned_but_not_persisted), true).expect("decision"),
        RecoveryDecision::RedispatchSafe
    );
}

#[test]
fn restart_resumes_only_safe_or_durable_batch_work() {
    let mut transient_unit = unit(JobUnitState::Running);
    transient_unit
        .payload
        .insert("dispatchState".to_owned(), serde_json::json!("retry_wait"));
    let transient = attempt(
        transient_unit.id,
        true,
        Some(CoreFailureClass::Transport),
        false,
    );
    assert_eq!(
        recovery_decision(&transient_unit, Some(&transient), false).expect("decision"),
        RecoveryDecision::RedispatchSafe
    );

    let mut paused_before_dispatch = unit(JobUnitState::Running);
    paused_before_dispatch.payload.insert(
        "dispatchState".to_owned(),
        serde_json::json!("cancelled_before_dispatch"),
    );
    let cancelled = attempt(
        paused_before_dispatch.id,
        true,
        Some(CoreFailureClass::Cancelled),
        false,
    );
    assert_eq!(
        recovery_decision(&paused_before_dispatch, Some(&cancelled), false).expect("decision"),
        RecoveryDecision::RedispatchSafe
    );

    let mut durable = unit(JobUnitState::Running);
    durable.payload.insert(
        "result".to_owned(),
        serde_json::to_value(PersistedDetectionResult {
            characters: Vec::new(),
            dialogue: Vec::new(),
            usage: ProviderUsage::default(),
        })
        .expect("result"),
    );
    assert_eq!(
        recovery_decision(&durable, None, false).expect("decision"),
        RecoveryDecision::FinalizePersistedResult
    );
}

#[tokio::test]
async fn detection_retry_uses_the_registered_runtime_profile_identity() {
    let runtime_id =
        audiobookai_providers::ProviderId::new(Uuid::new_v4().to_string()).expect("runtime id");
    let mut runtime = crate::runtime::RuntimeProfile::new(
        runtime_id.clone(),
        "LM Studio",
        crate::runtime::RuntimeAdapterKind::LmStudio,
        audiobookai_providers::ProviderKind::ExternalEndpoint,
    );
    runtime.endpoint = Some(url::Url::parse("http://127.0.0.1:1234").expect("endpoint"));
    let registry =
        crate::runtime::ProviderRuntime::new(crate::runtime::ProviderAdapterFactory::default());
    registry
        .register(runtime, None)
        .await
        .expect("register runtime");
    let adapter = registry
        .character(&runtime_id)
        .await
        .expect("character adapter");
    let registered = registry
        .profile(&runtime_id)
        .await
        .expect("registered profile");

    assert_eq!(adapter.descriptor().id.to_string(), "lmstudio");
    assert_ne!(adapter.descriptor().id, runtime_id);
    assert!(detection_runtime_profile_matches(
        &runtime_id,
        Some(ProviderModeView::ExternalEndpoint),
        &registered,
    ));
}

#[test]
fn pause_and_cancel_states_gate_every_new_provider_dispatch() {
    assert!(detection_state_allows_dispatch(JobState::Running));
    for state in [
        JobState::Queued,
        JobState::Pausing,
        JobState::Paused,
        JobState::Cancelling,
        JobState::Cancelled,
        JobState::Failed,
        JobState::Completed,
    ] {
        assert!(!detection_state_allows_dispatch(state), "state {state:?}");
    }
}
