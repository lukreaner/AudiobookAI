use super::{
    AppState, Arc, Deserialize, Json, Multipart, Page, Path, ProviderCapabilitiesView,
    ProviderKindView, ProviderModeView, ProviderProfileView, ProviderStatusView, Query,
    ServiceError, State, StatusCode, Utc, Uuid, VoiceAssignmentView, VoiceView,
    active_job_conflict, advance_character_revision_tx, blocking_character_job,
    provider_capabilities_are_fresh, reject_empty, stable_voice_id, sync_character_review_catalog,
};

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct VoiceAssignmentInput {
    pub(super) provider_profile_id: Uuid,
    pub(super) provider_name: String,
    pub(super) voice_id: Uuid,
    pub(super) voice_name: String,
    pub(super) model: Option<String>,
    #[serde(default)]
    pub(super) performance: audiobookai_core::PerformanceSettings,
    #[serde(default)]
    pub(super) timing: audiobookai_core::TimingSettings,
    pub(super) expected_character_revision: u64,
}

#[allow(clippy::too_many_lines)]
pub(super) async fn assign_voice(
    State(state): State<Arc<AppState>>,
    Path((project_id, character_id)): Path<(Uuid, Uuid)>,
    Json(input): Json<VoiceAssignmentInput>,
) -> Result<Json<crate::models::CharacterMutationView>, ServiceError> {
    let _model_lifecycle_guard = state.model_lifecycle.lock().await;
    let project_lock = state.character_lifecycle_lock(project_id).await;
    let _project_guard = project_lock.lock().await;
    if let Some(job) = blocking_character_job(&state, project_id).await {
        return Err(active_job_conflict(&job));
    }
    let mut assignment = VoiceAssignmentView {
        provider_profile_id: input.provider_profile_id,
        provider_name: input.provider_name,
        voice_id: input.voice_id,
        voice_name: input.voice_name,
        model: input.model,
        performance: input.performance,
        timing: input.timing,
    };
    let (voice, source_id, provider) = {
        let catalog = state.catalog.read().await;
        let provider = catalog
            .providers
            .get(&assignment.provider_profile_id)
            .cloned()
            .ok_or_else(|| ServiceError::InvalidRequest("unknown provider profile".to_owned()))?;
        let voice = catalog
            .voices
            .iter()
            .find(|voice| {
                voice.id == assignment.voice_id
                    && voice.provider_profile_id == assignment.provider_profile_id
            })
            .cloned()
            .ok_or_else(|| {
                ServiceError::InvalidRequest(
                    "the selected voice does not belong to the selected provider".to_owned(),
                )
            })?;
        let source_id = catalog
            .voice_sources
            .get(&voice.id)
            .cloned()
            .ok_or_else(|| {
                ServiceError::Conflict("refresh the provider voice catalog first".to_owned())
            })?;
        (voice, source_id, provider)
    };
    validate_piper_voice_selection(&provider, &source_id, assignment.model.as_deref())?;
    assignment.provider_name.clone_from(&provider.name);
    assignment.voice_name.clone_from(&voice.name);
    validate_voice_direction(
        &assignment.performance,
        &assignment.timing,
        assignment.model.as_deref().or(provider.model.as_deref()),
        provider.capabilities.as_ref(),
    )?;
    let mut transaction = state
        .database
        .pool()
        .begin()
        .await
        .map_err(|error| ServiceError::Storage(error.to_string()))?;
    persist_voice_assignment(
        &state,
        &mut transaction,
        project_id,
        character_id,
        &voice,
        &source_id,
        &assignment,
    )
    .await?;
    let character_revision = advance_character_revision_tx(
        &mut transaction,
        project_id,
        input.expected_character_revision,
        false,
    )
    .await?;
    transaction
        .commit()
        .await
        .map_err(|error| ServiceError::Storage(error.to_string()))?;
    let updated = {
        let mut catalog = state.catalog.write().await;
        let characters = catalog
            .characters
            .get_mut(&project_id)
            .ok_or(ServiceError::NotFound)?;
        let character = characters
            .iter_mut()
            .find(|character| character.id == character_id)
            .ok_or(ServiceError::NotFound)?;
        character.voice_assignment = Some(assignment);
        character.clone()
    };
    sync_character_review_catalog(&state, project_id, false, character_revision).await?;
    Ok(Json(crate::models::CharacterMutationView {
        character: Some(updated),
        removed_character_id: None,
        inherited_voice: None,
        character_revision,
    }))
}

/// Piper treats one installed voice bundle as both the connection model and its only voice.
/// Keep that invariant at assignment and dispatch boundaries even if stale catalog rows exist.
pub(crate) fn validate_piper_voice_selection(
    provider: &ProviderProfileView,
    provider_voice_id: &str,
    assignment_model: Option<&str>,
) -> Result<(), ServiceError> {
    if !matches!(provider.kind, ProviderKindView::Piper) {
        return Ok(());
    }
    let selected = provider.model.as_deref().ok_or_else(|| {
        ServiceError::InvalidRequest(
            "the Piper connection has no verified selected voice".to_owned(),
        )
    })?;
    if provider_voice_id != selected || assignment_model.is_some_and(|model| model != selected) {
        return Err(ServiceError::InvalidRequest(
            "the assigned Piper voice must exactly match the connection's selected model"
                .to_owned(),
        ));
    }
    Ok(())
}

pub(crate) fn validate_voice_direction(
    performance: &audiobookai_core::PerformanceSettings,
    timing: &audiobookai_core::TimingSettings,
    model: Option<&str>,
    capabilities: Option<&ProviderCapabilitiesView>,
) -> Result<(), ServiceError> {
    use audiobookai_core::Validate;

    if let Some(issue) = performance.validation_issues().into_iter().next() {
        return Err(ServiceError::InvalidRequest(issue.message));
    }
    if let Some(issue) = timing.validation_issues().into_iter().next() {
        return Err(ServiceError::InvalidRequest(issue.message));
    }
    if performance.is_empty() {
        return Ok(());
    }
    let model = model.ok_or_else(|| {
        ServiceError::InvalidRequest(
            "select an exact TTS model before setting performance controls".to_owned(),
        )
    })?;
    let descriptor = capabilities
        .and_then(|values| {
            values
                .model_performance
                .iter()
                .find(|descriptor| descriptor.model == model)
        })
        .ok_or_else(|| {
            ServiceError::InvalidRequest(
                "the selected provider model has no verified performance controls".to_owned(),
            )
        })?;
    validate_performance_value("speed", performance.speed, descriptor.performance.speed)?;
    validate_performance_value("pitch", performance.pitch, descriptor.performance.pitch)?;
    validate_performance_value(
        "stability",
        performance.stability,
        descriptor.performance.stability,
    )?;
    validate_performance_value(
        "similarity",
        performance.similarity,
        descriptor.performance.similarity,
    )?;
    validate_performance_value("style", performance.style, descriptor.performance.style)?;
    if performance.speaker_boost.is_some() && !descriptor.performance.speaker_boost {
        return Err(ServiceError::InvalidRequest(
            "speaker boost is not supported by the selected model".to_owned(),
        ));
    }
    if let Some(cue) = performance.delivery_cue
        && !descriptor.performance.delivery_cues.contains(&cue)
    {
        return Err(ServiceError::InvalidRequest(
            "the selected delivery cue is not supported by the selected model".to_owned(),
        ));
    }
    Ok(())
}

pub(super) fn validate_performance_value(
    name: &str,
    value: Option<f64>,
    range: Option<audiobookai_core::PerformanceRange>,
) -> Result<(), ServiceError> {
    let Some(value) = value else {
        return Ok(());
    };
    if !range.is_some_and(|range| range.contains(value)) {
        return Err(ServiceError::InvalidRequest(format!(
            "{name} is not supported at this value by the selected model"
        )));
    }
    Ok(())
}

// Voice-profile and speaker-assignment upserts must remain visibly ordered so
// their relational identities cannot drift during future changes.
#[allow(clippy::too_many_lines)]
pub(super) async fn persist_voice_assignment(
    state: &AppState,
    transaction: &mut sqlx::Transaction<'_, sqlx::Sqlite>,
    project_id: Uuid,
    character_id: Uuid,
    voice: &VoiceView,
    provider_voice_id: &str,
    assignment: &VoiceAssignmentView,
) -> Result<(), ServiceError> {
    use audiobookai_core::{
        CharacterId, ProjectId, ProviderProfileId, Speaker, VoiceAssignment, VoiceAssignmentId,
        VoiceOrigin, VoiceOwnership, VoiceProfile, VoiceProfileId,
    };

    let now = Utc::now();
    let existing_profile =
        sqlx::query_scalar::<_, String>("SELECT payload FROM voice_profiles WHERE id = ?")
            .bind(voice.id.to_string())
            .fetch_optional(&mut **transaction)
            .await
            .map_err(|error| ServiceError::Storage(error.to_string()))?
            .and_then(|payload| serde_json::from_str::<VoiceProfile>(&payload).ok());
    let profile = VoiceProfile {
        id: VoiceProfileId::from_uuid(voice.id),
        provider_profile_id: ProviderProfileId::from_uuid(voice.provider_profile_id),
        provider_voice_id: Some(provider_voice_id.to_owned()),
        name: voice.name.clone(),
        origin: match voice.kind {
            crate::models::VoiceKindView::Catalog => VoiceOrigin::ProviderCatalog,
            crate::models::VoiceKindView::LocalReference => VoiceOrigin::LocalReference,
            crate::models::VoiceKindView::RemoteClone => VoiceOrigin::ProviderClone,
            crate::models::VoiceKindView::Native => VoiceOrigin::NativeSystem,
        },
        ownership: if voice.owned {
            VoiceOwnership::AudiobookAi
        } else if matches!(voice.kind, crate::models::VoiceKindView::LocalReference) {
            VoiceOwnership::User
        } else {
            VoiceOwnership::Provider
        },
        reference_audio_artifact_ids: existing_profile.as_ref().map_or_else(Vec::new, |profile| {
            profile.reference_audio_artifact_ids.clone()
        }),
        language: voice.locale.clone(),
        model: assignment.model.clone(),
        settings: existing_profile
            .as_ref()
            .map_or_else(std::collections::BTreeMap::new, |profile| {
                profile.settings.clone()
            }),
        created_at: existing_profile
            .as_ref()
            .map_or(now, |profile| profile.created_at),
        updated_at: now,
    };
    sqlx::query(
        "INSERT INTO voice_profiles \
         (id, provider_id, name, origin, ownership, provider_voice_id, updated_at, payload) \
         VALUES (?, ?, ?, ?, ?, ?, ?, ?) \
         ON CONFLICT(id) DO UPDATE SET provider_id = excluded.provider_id, name = excluded.name, \
         origin = excluded.origin, ownership = excluded.ownership, \
         provider_voice_id = excluded.provider_voice_id, updated_at = excluded.updated_at, \
         payload = excluded.payload",
    )
    .bind(profile.id.to_string())
    .bind(profile.provider_profile_id.to_string())
    .bind(&profile.name)
    .bind(match profile.origin {
        VoiceOrigin::ProviderCatalog => "provider_catalog",
        VoiceOrigin::LocalReference => "local_reference",
        VoiceOrigin::ProviderClone => "provider_clone",
        VoiceOrigin::NativeSystem => "native_system",
    })
    .bind(match profile.ownership {
        VoiceOwnership::Provider => "provider",
        VoiceOwnership::User => "user",
        VoiceOwnership::AudiobookAi => "audiobook_ai",
    })
    .bind(&profile.provider_voice_id)
    .bind(profile.updated_at.to_rfc3339())
    .bind(
        serde_json::to_string(&profile)
            .map_err(|error| ServiceError::Internal(error.to_string()))?,
    )
    .execute(&mut **transaction)
    .await
    .map_err(|error| ServiceError::Storage(error.to_string()))?;

    let speaker = if state
        .catalog
        .read()
        .await
        .characters
        .get(&project_id)
        .and_then(|characters| {
            characters
                .iter()
                .find(|character| character.id == character_id)
        })
        .is_some_and(|character| {
            matches!(character.role, audiobookai_core::CharacterRole::Narrator)
        }) {
        Speaker::Narrator
    } else {
        Speaker::Character(CharacterId::from_uuid(character_id))
    };
    let speaker_key = match &speaker {
        Speaker::Narrator => "narrator".to_owned(),
        Speaker::Character(id) => format!("character:{id}"),
        Speaker::Named(name) => format!("named:{}", name.to_lowercase()),
    };
    let existing = sqlx::query_scalar::<_, String>(
        "SELECT payload FROM voice_assignments WHERE project_id = ? AND speaker_key = ?",
    )
    .bind(project_id.to_string())
    .bind(&speaker_key)
    .fetch_optional(&mut **transaction)
    .await
    .map_err(|error| ServiceError::Storage(error.to_string()))?
    .and_then(|payload| serde_json::from_str::<VoiceAssignment>(&payload).ok());
    let domain_assignment = VoiceAssignment {
        id: existing
            .as_ref()
            .map_or_else(VoiceAssignmentId::new, |stored| stored.id),
        project_id: ProjectId::from_uuid(project_id),
        speaker,
        voice_profile_id: profile.id,
        provider_profile_id: profile.provider_profile_id,
        model: assignment.model.clone(),
        performance: assignment.performance.clone(),
        timing: assignment.timing.clone(),
        settings: std::collections::BTreeMap::new(),
        created_at: existing.as_ref().map_or(now, |stored| stored.created_at),
        updated_at: now,
    };
    sqlx::query(
        "INSERT INTO voice_assignments \
         (id, project_id, provider_id, voice_profile_id, speaker_key, updated_at, payload, character_id) \
         VALUES (?, ?, ?, ?, ?, ?, ?, ?) \
         ON CONFLICT(project_id, speaker_key) DO UPDATE SET provider_id = excluded.provider_id, \
         voice_profile_id = excluded.voice_profile_id, updated_at = excluded.updated_at, \
         payload = excluded.payload, character_id = excluded.character_id",
    )
    .bind(domain_assignment.id.to_string())
    .bind(domain_assignment.project_id.to_string())
    .bind(domain_assignment.provider_profile_id.to_string())
    .bind(domain_assignment.voice_profile_id.to_string())
    .bind(speaker_key)
    .bind(domain_assignment.updated_at.to_rfc3339())
    .bind(
        serde_json::to_string(&domain_assignment)
            .map_err(|error| ServiceError::Internal(error.to_string()))?,
    )
    .bind(character_id.to_string())
    .execute(&mut **transaction)
    .await
    .map_err(|error| ServiceError::Storage(error.to_string()))?;
    Ok(())
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct VoiceQuery {
    pub(super) provider_profile_id: Option<Uuid>,
}

pub(super) async fn list_voices(
    State(state): State<Arc<AppState>>,
    Query(query): Query<VoiceQuery>,
) -> Json<Page<VoiceView>> {
    let catalog = state.catalog.read().await;
    Json(Page::all(
        catalog
            .voices
            .iter()
            .filter(|voice| {
                query
                    .provider_profile_id
                    .is_none_or(|provider_id| voice.provider_profile_id == provider_id)
            })
            .cloned()
            .collect(),
    ))
}

// Multipart validation, consent enforcement, provider dispatch, and ownership
// persistence are intentionally colocated to preserve the security sequence.
#[allow(clippy::too_many_lines)]
pub(super) async fn create_voice_clone(
    State(state): State<Arc<AppState>>,
    Path(provider_id): Path<Uuid>,
    mut multipart: Multipart,
) -> Result<(StatusCode, Json<VoiceView>), ServiceError> {
    use audiobookai_core::{
        ProviderProfileId, VoiceOrigin, VoiceOwnership, VoiceProfile, VoiceProfileId,
    };
    use audiobookai_providers::{VoiceCloneRequest, VoiceSample};

    let mut name = None;
    let mut description = None;
    let mut project_id = None;
    let mut samples = Vec::new();
    let mut sample_hashes = Vec::new();
    while let Some(field) = multipart
        .next_field()
        .await
        .map_err(|error| ServiceError::InvalidRequest(error.to_string()))?
    {
        match field.name() {
            Some("name") => {
                name = Some(
                    field
                        .text()
                        .await
                        .map_err(|error| ServiceError::InvalidRequest(error.to_string()))?,
                );
            }
            Some("description") => {
                description = Some(
                    field
                        .text()
                        .await
                        .map_err(|error| ServiceError::InvalidRequest(error.to_string()))?,
                );
            }
            Some("projectId") => {
                let value = field
                    .text()
                    .await
                    .map_err(|error| ServiceError::InvalidRequest(error.to_string()))?;
                project_id = Some(Uuid::parse_str(value.trim()).map_err(|_| {
                    ServiceError::InvalidRequest("projectId must be a UUID".to_owned())
                })?);
            }
            Some("referenceAudio") => {
                let file_name = field.file_name().unwrap_or("reference-audio").to_owned();
                let content_type = field
                    .content_type()
                    .unwrap_or("application/octet-stream")
                    .to_owned();
                let bytes = field
                    .bytes()
                    .await
                    .map_err(|error| ServiceError::InvalidRequest(error.to_string()))?;
                if bytes.is_empty() {
                    return Err(ServiceError::InvalidRequest(
                        "reference audio must not be empty".to_owned(),
                    ));
                }
                sample_hashes.push(blake3::hash(&bytes).to_hex().to_string());
                samples.push(VoiceSample {
                    file_name,
                    content_type,
                    bytes,
                });
            }
            _ => {}
        }
    }
    let name = name.ok_or_else(|| ServiceError::InvalidRequest("name is required".to_owned()))?;
    reject_empty("name", &name)?;
    if samples.is_empty() {
        return Err(ServiceError::InvalidRequest(
            "at least one referenceAudio field is required".to_owned(),
        ));
    }
    // Multipart parsing is local and bounded. Acquire lifecycle guards only for the final
    // validation-and-dispatch window so provider routing and project consent cannot change after
    // the checks below but before reference audio leaves the device.
    let _model_lifecycle_guard = state.model_lifecycle.lock().await;
    let provider = state
        .catalog
        .read()
        .await
        .providers
        .get(&provider_id)
        .cloned()
        .ok_or(ServiceError::NotFound)?;
    if !provider_capabilities_are_fresh(&provider)
        || !provider
            .capabilities
            .as_ref()
            .is_some_and(|capabilities| capabilities.voice_cloning)
        || !matches!(provider.status, ProviderStatusView::Online)
    {
        return Err(ServiceError::Conflict(
            "refresh an online provider that supports voice cloning before uploading reference audio"
                .to_owned(),
        ));
    }
    if matches!(provider.mode, ProviderModeView::CloudRemote) && !provider.credential_configured {
        return Err(ServiceError::Conflict(
            "configure the cloud provider credential before uploading reference audio".to_owned(),
        ));
    }
    let dispatch_consent_lock = if matches!(provider.mode, ProviderModeView::CloudRemote) {
        let project_id = project_id.ok_or_else(|| {
            ServiceError::InvalidRequest(
                "projectId is required before reference audio is sent to a cloud provider"
                    .to_owned(),
            )
        })?;
        Some(state.dispatch_consent_lifecycle_lock(project_id).await)
    } else {
        None
    };
    let _dispatch_consent_guard = if let Some(lock) = dispatch_consent_lock.as_ref() {
        Some(lock.read().await)
    } else {
        None
    };
    if matches!(provider.mode, ProviderModeView::CloudRemote) {
        let project_id = project_id.expect("cloud provider requires project id above");
        let consented = state
            .catalog
            .read()
            .await
            .projects
            .get(&project_id)
            .is_some_and(|project| project.consent_cloud_audio);
        if !consented {
            return Err(ServiceError::Forbidden(
                "grant this project permission to send reference audio to cloud providers"
                    .to_owned(),
            ));
        }
    }

    let runtime_id = audiobookai_providers::ProviderId::new(provider_id.to_string())
        .map_err(|error| ServiceError::Internal(error.to_string()))?;
    let clone = state
        .providers
        .voice_cloner(&runtime_id)
        .await
        .map_err(|error| ServiceError::Conflict(error.to_string()))?
        .create_clone(VoiceCloneRequest {
            name: name.trim().to_owned(),
            description,
            samples,
        })
        .await
        .map_err(|error| ServiceError::Conflict(error.to_string()))?;
    if !clone.owned_by_audiobookai {
        return Err(ServiceError::Conflict(
            "the provider did not confirm ownership of the newly created clone".to_owned(),
        ));
    }
    let voice_id = stable_voice_id(provider_id, &clone.provider_voice_id);
    let now = Utc::now();
    let profile = VoiceProfile {
        id: VoiceProfileId::from_uuid(voice_id),
        provider_profile_id: ProviderProfileId::from_uuid(provider_id),
        provider_voice_id: Some(clone.provider_voice_id.clone()),
        name: clone.name.clone(),
        origin: VoiceOrigin::ProviderClone,
        ownership: VoiceOwnership::AudiobookAi,
        reference_audio_artifact_ids: Vec::new(),
        language: None,
        model: provider.model,
        settings: std::collections::BTreeMap::from([
            ("projectId".to_owned(), serde_json::json!(project_id)),
            (
                "referenceAudioHashes".to_owned(),
                serde_json::json!(sample_hashes),
            ),
        ]),
        created_at: clone.created_at,
        updated_at: now,
    };
    persist_voice_profile(&state, &profile).await?;
    let view = VoiceView {
        id: voice_id,
        provider_profile_id: provider_id,
        name: clone.name,
        locale: None,
        gender: None,
        kind: crate::models::VoiceKindView::RemoteClone,
        owned: true,
        preview_url: None,
    };
    let mut catalog = state.catalog.write().await;
    catalog
        .voice_sources
        .insert(voice_id, clone.provider_voice_id);
    catalog.voices.retain(|voice| voice.id != voice_id);
    catalog.voices.push(view.clone());
    Ok((StatusCode::CREATED, Json(view)))
}

#[derive(Debug, Deserialize)]
pub(super) struct VoiceCloneUpdateInput {
    pub(super) name: String,
}

pub(super) async fn update_voice_clone(
    State(state): State<Arc<AppState>>,
    Path(id): Path<Uuid>,
    Json(input): Json<VoiceCloneUpdateInput>,
) -> Result<Json<VoiceView>, ServiceError> {
    use audiobookai_core::{VoiceOrigin, VoiceOwnership, VoiceProfile};
    use audiobookai_providers::VoiceClone;

    reject_empty("name", &input.name)?;
    let payload =
        sqlx::query_scalar::<_, String>("SELECT payload FROM voice_profiles WHERE id = ?")
            .bind(id.to_string())
            .fetch_optional(state.database.pool())
            .await
            .map_err(|error| ServiceError::Storage(error.to_string()))?
            .ok_or(ServiceError::NotFound)?;
    let mut profile: VoiceProfile = serde_json::from_str(&payload)
        .map_err(|error| ServiceError::Internal(error.to_string()))?;
    if profile.origin != VoiceOrigin::ProviderClone
        || profile.ownership != VoiceOwnership::AudiobookAi
    {
        return Err(ServiceError::Forbidden(
            "only app-owned remote clones can be edited".to_owned(),
        ));
    }
    let provider_voice_id = profile.provider_voice_id.clone().ok_or_else(|| {
        ServiceError::Internal("clone is missing its provider voice id".to_owned())
    })?;
    let runtime_id =
        audiobookai_providers::ProviderId::new(profile.provider_profile_id.to_string())
            .map_err(|error| ServiceError::Internal(error.to_string()))?;
    let updated = state
        .providers
        .voice_cloner(&runtime_id)
        .await
        .map_err(|error| ServiceError::Conflict(error.to_string()))?
        .update_clone(
            &VoiceClone {
                provider_voice_id,
                name: profile.name.clone(),
                owned_by_audiobookai: true,
                created_at: profile.created_at,
            },
            input.name.trim().to_owned(),
        )
        .await
        .map_err(|error| ServiceError::Conflict(error.to_string()))?;
    profile.name.clone_from(&updated.name);
    profile.updated_at = Utc::now();
    persist_voice_profile(&state, &profile).await?;
    let mut catalog = state.catalog.write().await;
    let voice = catalog
        .voices
        .iter_mut()
        .find(|voice| voice.id == id)
        .ok_or(ServiceError::NotFound)?;
    voice.name = updated.name;
    Ok(Json(voice.clone()))
}

#[derive(Debug, Deserialize)]
pub(super) struct DeleteCloneQuery {
    #[serde(default)]
    pub(super) confirmed: bool,
}

pub(super) async fn delete_voice_clone(
    State(state): State<Arc<AppState>>,
    Path(id): Path<Uuid>,
    Query(query): Query<DeleteCloneQuery>,
) -> Result<StatusCode, ServiceError> {
    use audiobookai_core::{VoiceOrigin, VoiceOwnership, VoiceProfile};
    use audiobookai_providers::VoiceClone;

    if !query.confirmed {
        return Err(ServiceError::InvalidRequest(
            "deleting an app-owned remote clone requires confirmed=true".to_owned(),
        ));
    }
    let payload =
        sqlx::query_scalar::<_, String>("SELECT payload FROM voice_profiles WHERE id = ?")
            .bind(id.to_string())
            .fetch_optional(state.database.pool())
            .await
            .map_err(|error| ServiceError::Storage(error.to_string()))?
            .ok_or(ServiceError::NotFound)?;
    let profile: VoiceProfile = serde_json::from_str(&payload)
        .map_err(|error| ServiceError::Internal(error.to_string()))?;
    if profile.origin != VoiceOrigin::ProviderClone
        || profile.ownership != VoiceOwnership::AudiobookAi
    {
        return Err(ServiceError::Forbidden(
            "catalog, native, user-owned, and unowned voices are never remotely deleted".to_owned(),
        ));
    }
    let in_use = sqlx::query_scalar::<_, i64>(
        "SELECT COUNT(*) FROM voice_assignments WHERE voice_profile_id = ?",
    )
    .bind(id.to_string())
    .fetch_one(state.database.pool())
    .await
    .map_err(|error| ServiceError::Storage(error.to_string()))?;
    if in_use > 0 {
        return Err(ServiceError::Conflict(
            "remove this clone from every character assignment before deleting it".to_owned(),
        ));
    }
    let provider_voice_id = profile.provider_voice_id.clone().ok_or_else(|| {
        ServiceError::Internal("clone is missing its provider voice id".to_owned())
    })?;
    let runtime_id =
        audiobookai_providers::ProviderId::new(profile.provider_profile_id.to_string())
            .map_err(|error| ServiceError::Internal(error.to_string()))?;
    state
        .providers
        .voice_cloner(&runtime_id)
        .await
        .map_err(|error| ServiceError::Conflict(error.to_string()))?
        .delete_owned_clone(
            &VoiceClone {
                provider_voice_id,
                name: profile.name,
                owned_by_audiobookai: true,
                created_at: profile.created_at,
            },
            true,
        )
        .await
        .map_err(|error| ServiceError::Conflict(error.to_string()))?;
    sqlx::query("DELETE FROM voice_profiles WHERE id = ?")
        .bind(id.to_string())
        .execute(state.database.pool())
        .await
        .map_err(|error| ServiceError::Storage(error.to_string()))?;
    let mut catalog = state.catalog.write().await;
    catalog.voices.retain(|voice| voice.id != id);
    catalog.voice_sources.remove(&id);
    Ok(StatusCode::NO_CONTENT)
}

pub(super) async fn persist_voice_profile(
    state: &AppState,
    profile: &audiobookai_core::VoiceProfile,
) -> Result<(), ServiceError> {
    use audiobookai_core::{VoiceOrigin, VoiceOwnership};

    sqlx::query(
        "INSERT INTO voice_profiles \
         (id, provider_id, name, origin, ownership, provider_voice_id, updated_at, payload) \
         VALUES (?, ?, ?, ?, ?, ?, ?, ?) \
         ON CONFLICT(id) DO UPDATE SET provider_id = excluded.provider_id, name = excluded.name, \
         origin = excluded.origin, ownership = excluded.ownership, \
         provider_voice_id = excluded.provider_voice_id, updated_at = excluded.updated_at, \
         payload = excluded.payload",
    )
    .bind(profile.id.to_string())
    .bind(profile.provider_profile_id.to_string())
    .bind(&profile.name)
    .bind(match profile.origin {
        VoiceOrigin::ProviderCatalog => "provider_catalog",
        VoiceOrigin::LocalReference => "local_reference",
        VoiceOrigin::ProviderClone => "provider_clone",
        VoiceOrigin::NativeSystem => "native_system",
    })
    .bind(match profile.ownership {
        VoiceOwnership::Provider => "provider",
        VoiceOwnership::User => "user",
        VoiceOwnership::AudiobookAi => "audiobook_ai",
    })
    .bind(&profile.provider_voice_id)
    .bind(profile.updated_at.to_rfc3339())
    .bind(
        serde_json::to_string(profile)
            .map_err(|error| ServiceError::Internal(error.to_string()))?,
    )
    .execute(state.database.pool())
    .await
    .map_err(|error| ServiceError::Storage(error.to_string()))?;
    Ok(())
}
