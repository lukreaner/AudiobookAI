use super::{
    AppState, Arc, Deserialize, HashSet, JobView, Json, Path, ProjectDisplayStatus,
    ProviderModeView, ProviderStatusView, ReviewStatus, ServiceError, State, StatusCode, Utc, Uuid,
    active_job_conflict, active_job_status, blocking_character_job, new_job, reject_empty,
};

pub(super) async fn advance_character_revision_tx(
    transaction: &mut sqlx::Transaction<'_, sqlx::Sqlite>,
    project_id: Uuid,
    expected_character_revision: u64,
    approved: bool,
) -> Result<u64, ServiceError> {
    use audiobookai_core::{Project, ProjectStatus};

    let row = sqlx::query_as::<_, (i64, i64, String)>(
        "SELECT revision, character_revision, payload FROM projects WHERE id = ?",
    )
    .bind(project_id.to_string())
    .fetch_optional(&mut **transaction)
    .await
    .map_err(|error| ServiceError::Storage(error.to_string()))?
    .ok_or(ServiceError::NotFound)?;
    let stored_character_revision = u64::try_from(row.1)
        .map_err(|_| ServiceError::Internal("stored character revision is invalid".to_owned()))?;
    if stored_character_revision != expected_character_revision {
        return Err(ServiceError::ConflictDetails {
            code: "stale_character_revision",
            detail: "character review changed; refresh before saving".to_owned(),
            meta: serde_json::json!({
                "currentCharacterRevision": stored_character_revision,
            }),
        });
    }
    let mut project: Project =
        serde_json::from_str(&row.2).map_err(|error| ServiceError::Internal(error.to_string()))?;
    project.status = if approved {
        ProjectStatus::Ready
    } else {
        ProjectStatus::NeedsCharacterReview
    };
    project.character_reviewed_at = approved.then(Utc::now);
    project.updated_at = Utc::now();
    let next_character_revision = stored_character_revision.saturating_add(1);
    let next_revision = u64::try_from(row.0)
        .map_err(|_| ServiceError::Internal("stored project revision is invalid".to_owned()))?
        .saturating_add(1);
    let result = sqlx::query(
        "UPDATE projects SET status = ?, updated_at = ?, revision = ?, character_revision = ?, \
         payload = ? WHERE id = ? AND revision = ? AND character_revision = ?",
    )
    .bind(if approved {
        "ready"
    } else {
        "needs_character_review"
    })
    .bind(project.updated_at.to_rfc3339())
    .bind(i64::try_from(next_revision).unwrap_or(i64::MAX))
    .bind(i64::try_from(next_character_revision).unwrap_or(i64::MAX))
    .bind(
        serde_json::to_string(&project)
            .map_err(|error| ServiceError::Internal(error.to_string()))?,
    )
    .bind(project_id.to_string())
    .bind(row.0)
    .bind(row.1)
    .execute(&mut **transaction)
    .await
    .map_err(|error| ServiceError::Storage(error.to_string()))?;
    if result.rows_affected() != 1 {
        return Err(ServiceError::Conflict(
            "project review changed concurrently".to_owned(),
        ));
    }
    Ok(next_character_revision)
}

pub(super) async fn sync_character_review_catalog(
    state: &AppState,
    project_id: Uuid,
    approved: bool,
    character_revision: u64,
) -> Result<(), ServiceError> {
    let mut catalog = state.catalog.write().await;
    let project = catalog
        .projects
        .get_mut(&project_id)
        .ok_or(ServiceError::NotFound)?;
    project.character_review_status = if approved {
        ReviewStatus::Approved
    } else {
        ReviewStatus::NeedsReview
    };
    project.character_revision = character_revision;
    project.summary.status = if approved {
        ProjectDisplayStatus::Ready
    } else {
        ProjectDisplayStatus::Draft
    };
    project.summary.updated_at = Utc::now();
    Ok(())
}

pub(super) async fn list_characters(
    State(state): State<Arc<AppState>>,
    Path(id): Path<Uuid>,
) -> Result<Json<crate::models::CharacterPageView>, ServiceError> {
    let catalog = state.catalog.read().await;
    let project = catalog.projects.get(&id).ok_or(ServiceError::NotFound)?;
    let items = catalog.characters.get(&id).cloned().unwrap_or_default();
    Ok(Json(crate::models::CharacterPageView {
        total: items.len(),
        items,
        character_revision: project.character_revision,
    }))
}

pub(super) async fn character_detection_status(
    State(state): State<Arc<AppState>>,
    Path(project_id): Path<Uuid>,
) -> Result<Json<crate::models::CharacterDetectionStatusView>, ServiceError> {
    let catalog = state.catalog.read().await;
    if !catalog.projects.contains_key(&project_id) {
        return Err(ServiceError::NotFound);
    }
    let mut jobs = catalog
        .jobs
        .values()
        .filter(|job| {
            job.project_id == project_id
                && matches!(job.kind, crate::models::JobKindView::CharacterDetection)
        })
        .cloned()
        .collect::<Vec<_>>();
    jobs.sort_by_key(|job| std::cmp::Reverse(job.updated_at));
    let latest_job = jobs.first().cloned();
    let active_job = jobs.into_iter().find(|job| active_job_status(job.status));
    Ok(Json(crate::models::CharacterDetectionStatusView {
        active_job,
        latest_job,
    }))
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct DetectionInput {
    pub(super) provider_profile_id: Uuid,
    pub(super) expected_character_revision: u64,
    #[serde(default)]
    pub(super) temperature: audiobookai_providers::Temperature,
    #[serde(default)]
    pub(super) reasoning: audiobookai_providers::ReasoningControl,
}

#[allow(clippy::too_many_lines)]
pub(super) async fn start_character_detection(
    State(state): State<Arc<AppState>>,
    Path(project_id): Path<Uuid>,
    Json(input): Json<DetectionInput>,
) -> Result<(StatusCode, Json<JobView>), ServiceError> {
    let _shutdown_admission = state.admit_shutdown_sensitive_work().await?;
    let _model_lifecycle_guard = state.model_lifecycle.lock().await;
    let project_lock = state.character_lifecycle_lock(project_id).await;
    let _project_guard = project_lock.lock().await;
    if let Some(job) = blocking_character_job(&state, project_id).await {
        return Err(active_job_conflict(&job));
    }
    let mut catalog = state.catalog.write().await;
    let provider = catalog
        .providers
        .get(&input.provider_profile_id)
        .ok_or_else(|| ServiceError::InvalidRequest("unknown provider profile".to_owned()))?;
    let provider_is_cloud = matches!(provider.mode, ProviderModeView::CloudRemote);
    let supports_detection = provider
        .capabilities
        .as_ref()
        .is_some_and(|caps| caps.character_detection);
    if !matches!(provider.status, ProviderStatusView::Online) {
        return Err(ServiceError::InvalidRequest(
            "refresh the selected provider and confirm it is online before character detection"
                .to_owned(),
        ));
    }
    if provider_is_cloud && !provider.credential_configured {
        return Err(ServiceError::InvalidRequest(
            "configure the selected cloud provider credential before character detection"
                .to_owned(),
        ));
    }
    let model = provider
        .model
        .as_deref()
        .filter(|model| !model.trim().is_empty())
        .map(str::to_owned)
        .ok_or_else(|| {
            ServiceError::InvalidRequest(
                "select a model on the character-detection provider before starting detection"
                    .to_owned(),
            )
        })?;
    let provider_endpoint = provider.endpoint.clone();
    let project = catalog
        .projects
        .get_mut(&project_id)
        .ok_or(ServiceError::NotFound)?;
    if project.character_revision != input.expected_character_revision {
        return Err(ServiceError::ConflictDetails {
            code: "stale_character_revision",
            detail: "character review changed; refresh before starting detection".to_owned(),
            meta: serde_json::json!({
                "currentCharacterRevision": project.character_revision,
            }),
        });
    }
    if provider_is_cloud && !project.consent_cloud_text {
        return Err(ServiceError::InvalidRequest(
            "grant this project permission to send book text to the selected cloud provider"
                .to_owned(),
        ));
    }
    if !supports_detection {
        return Err(ServiceError::InvalidRequest(
            "selected provider does not support character detection".to_owned(),
        ));
    }
    let project_title = project.summary.title.clone();
    drop(catalog);
    let runtime_id = audiobookai_providers::ProviderId::new(input.provider_profile_id.to_string())
        .map_err(|error| ServiceError::Internal(error.to_string()))?;
    let runtime_provider = state
        .providers
        .character(&runtime_id)
        .await
        .map_err(|error| ServiceError::Conflict(error.to_string()))?;
    input
        .temperature
        .validate(runtime_provider.capabilities().temperature)
        .map_err(|error| ServiceError::InvalidRequest(error.to_string()))?;
    input
        .reasoning
        .validate(runtime_provider.capabilities())
        .map_err(|error| ServiceError::InvalidRequest(error.to_string()))?;
    let job = new_job(
        project_id,
        project_title,
        crate::models::JobKindView::CharacterDetection,
        Vec::new(),
    );
    let job = crate::workflows::persist_detection_job(
        &state,
        &job,
        input.provider_profile_id,
        model,
        provider_endpoint,
        input.temperature,
        input.reasoning,
        input.expected_character_revision.saturating_add(1),
    )
    .await?;
    let mut transaction = state
        .database
        .pool()
        .begin()
        .await
        .map_err(|error| ServiceError::Storage(error.to_string()))?;
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
    state.catalog.write().await.jobs.insert(job.id, job.clone());
    sync_character_review_catalog(&state, project_id, false, character_revision).await?;
    state.events.publish(
        "job.queued",
        serde_json::json!({ "jobId": job.id, "projectId": project_id }),
    );
    crate::workflows::spawn_character_detection(Arc::clone(&state), job.id);
    Ok((StatusCode::ACCEPTED, Json(job)))
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct ReviewInput {
    pub(super) approved: bool,
    pub(super) expected_character_revision: u64,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct CharacterPatchInput {
    #[serde(alias = "name")]
    pub(super) canonical_name: String,
    #[serde(default)]
    pub(super) aliases: Vec<String>,
    pub(super) expected_character_revision: u64,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct MergeCharacterInput {
    pub(super) target_character_id: Uuid,
    pub(super) expected_character_revision: u64,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct CharacterRevisionInput {
    pub(super) expected_character_revision: u64,
}

pub(super) fn normalized_character_aliases(
    canonical_name: &str,
    aliases: Vec<String>,
) -> Vec<String> {
    let mut normalized = HashSet::new();
    aliases
        .into_iter()
        .filter_map(|alias| {
            let alias = alias.trim();
            if alias.is_empty() || alias.eq_ignore_ascii_case(canonical_name) {
                return None;
            }
            normalized
                .insert(alias.to_lowercase())
                .then(|| alias.to_owned())
        })
        .collect()
}

pub(super) fn ensure_character_name_available(
    characters: &[crate::models::CharacterView],
    canonical_name: &str,
    aliases: &[String],
    except_id: Option<Uuid>,
) -> Result<(), ServiceError> {
    let requested_names = std::iter::once(canonical_name)
        .chain(aliases.iter().map(String::as_str))
        .collect::<Vec<_>>();
    let conflict = characters
        .iter()
        .filter(|character| Some(character.id) != except_id)
        .find_map(|character| {
            std::iter::once(character.canonical_name.as_str())
                .chain(character.aliases.iter().map(String::as_str))
                .find(|existing| {
                    requested_names
                        .iter()
                        .any(|requested| existing.eq_ignore_ascii_case(requested))
                })
                .map(str::to_owned)
        });
    if let Some(conflicting_name) = conflict {
        return Err(ServiceError::ConflictDetails {
            code: "identity_conflict",
            detail: "another project character already uses that name or alias".to_owned(),
            meta: serde_json::json!({
                "canonicalName": canonical_name,
                "conflictingName": conflicting_name,
            }),
        });
    }
    Ok(())
}

#[allow(clippy::too_many_lines)]
pub(super) async fn create_character(
    State(state): State<Arc<AppState>>,
    Path(project_id): Path<Uuid>,
    Json(input): Json<CharacterPatchInput>,
) -> Result<(StatusCode, Json<crate::models::CharacterMutationView>), ServiceError> {
    use audiobookai_core::{Character, CharacterId, CharacterRole, ProjectId, Validate};

    reject_empty("canonicalName", &input.canonical_name)?;
    let canonical_name = input.canonical_name.trim().to_owned();
    let aliases = normalized_character_aliases(&canonical_name, input.aliases);
    let project_lock = state.character_lifecycle_lock(project_id).await;
    let _project_guard = project_lock.lock().await;
    if let Some(job) = blocking_character_job(&state, project_id).await {
        return Err(active_job_conflict(&job));
    }
    {
        let catalog = state.catalog.read().await;
        let project = catalog
            .projects
            .get(&project_id)
            .ok_or(ServiceError::NotFound)?;
        if project.character_revision != input.expected_character_revision {
            return Err(ServiceError::ConflictDetails {
                code: "stale_character_revision",
                detail: "character review changed; refresh before saving".to_owned(),
                meta: serde_json::json!({
                    "currentCharacterRevision": project.character_revision,
                }),
            });
        }
        ensure_character_name_available(
            catalog
                .characters
                .get(&project_id)
                .map_or(&[], Vec::as_slice),
            &canonical_name,
            &aliases,
            None,
        )?;
    }
    let now = Utc::now();
    let character = Character {
        id: CharacterId::new(),
        project_id: ProjectId::from_uuid(project_id),
        role: CharacterRole::Character,
        canonical_name: canonical_name.clone(),
        aliases: aliases.clone(),
        description: None,
        confidence: Some(1.0),
        detection_run_id: None,
        manually_created: true,
        created_at: now,
        updated_at: now,
    };
    character
        .validate()
        .map_err(|error| ServiceError::InvalidRequest(error.to_string()))?;
    let mut transaction = state
        .database
        .pool()
        .begin()
        .await
        .map_err(|error| ServiceError::Storage(error.to_string()))?;
    sqlx::query(
        "INSERT INTO characters (id, project_id, role, canonical_name, updated_at, payload) \
         VALUES (?, ?, 'character', ?, ?, ?)",
    )
    .bind(character.id.to_string())
    .bind(project_id.to_string())
    .bind(&canonical_name)
    .bind(now.to_rfc3339())
    .bind(
        serde_json::to_string(&character)
            .map_err(|error| ServiceError::Internal(error.to_string()))?,
    )
    .execute(&mut *transaction)
    .await
    .map_err(|error| ServiceError::Storage(error.to_string()))?;
    for alias in &aliases {
        sqlx::query(
            "INSERT INTO character_aliases (character_id, alias, normalized_alias) VALUES (?, ?, ?)",
        )
        .bind(character.id.to_string())
        .bind(alias)
        .bind(alias.to_lowercase())
        .execute(&mut *transaction)
        .await
        .map_err(|error| ServiceError::Storage(error.to_string()))?;
    }
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
    let view = crate::models::CharacterView {
        id: character.id.as_uuid(),
        role: CharacterRole::Character,
        canonical_name,
        aliases,
        confidence: 1.0,
        dialogue_count: 0,
        voice_assignment: None,
        evidence: Vec::new(),
    };
    {
        let mut catalog = state.catalog.write().await;
        let characters = catalog.characters.entry(project_id).or_default();
        characters.push(view.clone());
        characters.sort_by(|left, right| {
            left.canonical_name
                .to_lowercase()
                .cmp(&right.canonical_name.to_lowercase())
        });
    }
    sync_character_review_catalog(&state, project_id, false, character_revision).await?;
    state.events.publish(
        "character.updated",
        serde_json::json!({
            "projectId": project_id,
            "characterId": character.id.as_uuid(),
            "characterRevision": character_revision,
            "operation": "created",
        }),
    );
    Ok((
        StatusCode::CREATED,
        Json(crate::models::CharacterMutationView {
            character: Some(view),
            removed_character_id: None,
            inherited_voice: None,
            character_revision,
        }),
    ))
}

// Character identity, aliases, evidence, and review invalidation form one
// consistency boundary; extracting fragments would make ordering less clear.
#[allow(clippy::too_many_lines)]
pub(super) async fn update_character(
    State(state): State<Arc<AppState>>,
    Path((project_id, character_id)): Path<(Uuid, Uuid)>,
    Json(input): Json<CharacterPatchInput>,
) -> Result<Json<crate::models::CharacterMutationView>, ServiceError> {
    use audiobookai_core::{Character, CharacterId, Validate};

    reject_empty("canonicalName", &input.canonical_name)?;
    let canonical_name = input.canonical_name.trim().to_owned();
    let mut aliases = normalized_character_aliases(&canonical_name, input.aliases);
    let mut normalized = aliases
        .iter()
        .map(|alias| alias.to_lowercase())
        .collect::<HashSet<_>>();
    let project_lock = state.character_lifecycle_lock(project_id).await;
    let _project_guard = project_lock.lock().await;
    if let Some(job) = blocking_character_job(&state, project_id).await {
        return Err(active_job_conflict(&job));
    }
    {
        let catalog = state.catalog.read().await;
        let project = catalog
            .projects
            .get(&project_id)
            .ok_or(ServiceError::NotFound)?;
        if project.character_revision != input.expected_character_revision {
            return Err(ServiceError::ConflictDetails {
                code: "stale_character_revision",
                detail: "character review changed; refresh before saving".to_owned(),
                meta: serde_json::json!({
                    "currentCharacterRevision": project.character_revision,
                }),
            });
        }
        ensure_character_name_available(
            catalog
                .characters
                .get(&project_id)
                .map_or(&[], Vec::as_slice),
            &canonical_name,
            &aliases,
            Some(character_id),
        )?;
    }

    let (role, payload) = sqlx::query_as::<_, (String, String)>(
        "SELECT role, payload FROM characters WHERE id = ? AND project_id = ?",
    )
    .bind(character_id.to_string())
    .bind(project_id.to_string())
    .fetch_optional(state.database.pool())
    .await
    .map_err(|error| ServiceError::Storage(error.to_string()))?
    .ok_or(ServiceError::NotFound)?;
    let mut character: Character = serde_json::from_str(&payload)
        .map_err(|error| ServiceError::Internal(error.to_string()))?;
    character.role = if role == "narrator" {
        audiobookai_core::CharacterRole::Narrator
    } else {
        audiobookai_core::CharacterRole::Character
    };
    if !character
        .canonical_name
        .eq_ignore_ascii_case(&canonical_name)
    {
        let previous_name = character.canonical_name.trim();
        if !previous_name.is_empty() && normalized.insert(previous_name.to_lowercase()) {
            aliases.push(previous_name.to_owned());
        }
    }
    character.canonical_name = canonical_name.clone();
    character.aliases.clone_from(&aliases);
    character.manually_created = true;
    character.updated_at = Utc::now();
    character
        .validate()
        .map_err(|error| ServiceError::InvalidRequest(error.to_string()))?;

    let mut transaction = state
        .database
        .pool()
        .begin()
        .await
        .map_err(|error| ServiceError::Storage(error.to_string()))?;
    sqlx::query(
        "UPDATE characters SET canonical_name = ?, updated_at = ?, payload = ? WHERE id = ?",
    )
    .bind(&character.canonical_name)
    .bind(character.updated_at.to_rfc3339())
    .bind(
        serde_json::to_string(&character)
            .map_err(|error| ServiceError::Internal(error.to_string()))?,
    )
    .bind(CharacterId::from_uuid(character_id).to_string())
    .execute(&mut *transaction)
    .await
    .map_err(|error| ServiceError::Storage(error.to_string()))?;
    sqlx::query("DELETE FROM character_aliases WHERE character_id = ?")
        .bind(character_id.to_string())
        .execute(&mut *transaction)
        .await
        .map_err(|error| ServiceError::Storage(error.to_string()))?;
    for alias in &aliases {
        sqlx::query(
            "INSERT INTO character_aliases (character_id, alias, normalized_alias) VALUES (?, ?, ?)",
        )
        .bind(character_id.to_string())
        .bind(alias)
        .bind(alias.to_lowercase())
        .execute(&mut *transaction)
        .await
        .map_err(|error| ServiceError::Storage(error.to_string()))?;
    }
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
        let view = characters
            .iter_mut()
            .find(|candidate| candidate.id == character_id)
            .ok_or(ServiceError::NotFound)?;
        view.canonical_name = canonical_name;
        view.aliases = aliases;
        view.clone()
    };
    sync_character_review_catalog(&state, project_id, false, character_revision).await?;
    state.events.publish(
        "character.updated",
        serde_json::json!({
            "projectId": project_id,
            "characterId": character_id,
            "characterRevision": character_revision,
            "operation": "updated",
        }),
    );
    Ok(Json(crate::models::CharacterMutationView {
        character: Some(updated),
        removed_character_id: None,
        inherited_voice: None,
        character_revision,
    }))
}

#[allow(clippy::too_many_lines)]
pub(super) async fn merge_character(
    State(state): State<Arc<AppState>>,
    Path((project_id, source_id)): Path<(Uuid, Uuid)>,
    Json(input): Json<MergeCharacterInput>,
) -> Result<Json<crate::models::CharacterMutationView>, ServiceError> {
    use audiobookai_core::{
        Character, CharacterId, CharacterRole, DictionaryRule, Speaker, SpeakerOverride,
        VoiceAssignment,
    };

    if source_id == input.target_character_id {
        return Err(ServiceError::InvalidRequest(
            "merge source and target must be different characters".to_owned(),
        ));
    }
    let project_lock = state.character_lifecycle_lock(project_id).await;
    let _project_guard = project_lock.lock().await;
    if let Some(job) = blocking_character_job(&state, project_id).await {
        return Err(active_job_conflict(&job));
    }
    let (source_role, source_payload, target_role, target_payload) = {
        let rows = sqlx::query_as::<_, (String, String, String)>(
            "SELECT id, role, payload FROM characters WHERE project_id = ? AND id IN (?, ?)",
        )
        .bind(project_id.to_string())
        .bind(source_id.to_string())
        .bind(input.target_character_id.to_string())
        .fetch_all(state.database.pool())
        .await
        .map_err(|error| ServiceError::Storage(error.to_string()))?;
        let source = rows
            .iter()
            .find(|row| row.0 == source_id.to_string())
            .map(|row| (row.1.clone(), row.2.clone()))
            .ok_or(ServiceError::NotFound)?;
        let target = rows
            .iter()
            .find(|row| row.0 == input.target_character_id.to_string())
            .map(|row| (row.1.clone(), row.2.clone()))
            .ok_or(ServiceError::NotFound)?;
        (source.0, source.1, target.0, target.1)
    };
    let mut source: Character = serde_json::from_str(&source_payload)
        .map_err(|error| ServiceError::Internal(error.to_string()))?;
    let mut target: Character = serde_json::from_str(&target_payload)
        .map_err(|error| ServiceError::Internal(error.to_string()))?;
    source.role = if source_role == "narrator" {
        CharacterRole::Narrator
    } else {
        CharacterRole::Character
    };
    target.role = if target_role == "narrator" {
        CharacterRole::Narrator
    } else {
        CharacterRole::Character
    };
    if source.role == CharacterRole::Narrator {
        return Err(ServiceError::ConflictDetails {
            code: "protected_narrator",
            detail: "the narrator cannot be merged into another character".to_owned(),
            meta: serde_json::json!({ "characterId": source_id }),
        });
    }
    let mut merged_aliases = target.aliases.clone();
    merged_aliases.push(source.canonical_name.clone());
    merged_aliases.extend(source.aliases.clone());
    target.aliases = normalized_character_aliases(&target.canonical_name, merged_aliases);
    target.manually_created = true;
    target.updated_at = Utc::now();

    let mut transaction = state
        .database
        .pool()
        .begin()
        .await
        .map_err(|error| ServiceError::Storage(error.to_string()))?;
    let current_revision =
        sqlx::query_scalar::<_, i64>("SELECT character_revision FROM projects WHERE id = ?")
            .bind(project_id.to_string())
            .fetch_one(&mut *transaction)
            .await
            .map_err(|error| ServiceError::Storage(error.to_string()))?;
    if u64::try_from(current_revision).ok() != Some(input.expected_character_revision) {
        return Err(ServiceError::ConflictDetails {
            code: "stale_character_revision",
            detail: "character review changed; refresh before saving".to_owned(),
            meta: serde_json::json!({ "currentCharacterRevision": current_revision }),
        });
    }

    sqlx::query(
        "UPDATE dialogue_spans SET character_id = ?, \
         payload = json_set(payload, '$.character_id', ?) WHERE character_id = ?",
    )
    .bind(input.target_character_id.to_string())
    .bind(input.target_character_id.to_string())
    .bind(source_id.to_string())
    .execute(&mut *transaction)
    .await
    .map_err(|error| ServiceError::Storage(error.to_string()))?;

    let override_rows = sqlx::query_as::<_, (String, String)>(
        "SELECT id, payload FROM speaker_overrides WHERE speaker_character_id = ?",
    )
    .bind(source_id.to_string())
    .fetch_all(&mut *transaction)
    .await
    .map_err(|error| ServiceError::Storage(error.to_string()))?;
    for (override_id, payload) in override_rows {
        let mut record: SpeakerOverride = serde_json::from_str(&payload)
            .map_err(|error| ServiceError::Internal(error.to_string()))?;
        record.speaker = if target.role == CharacterRole::Narrator {
            Speaker::Narrator
        } else {
            Speaker::Character(CharacterId::from_uuid(input.target_character_id))
        };
        record.updated_at = Utc::now();
        sqlx::query(
            "UPDATE speaker_overrides SET speaker_character_id = ?, updated_at = ?, payload = ? \
             WHERE id = ?",
        )
        .bind(
            (target.role != CharacterRole::Narrator).then(|| input.target_character_id.to_string()),
        )
        .bind(record.updated_at.to_rfc3339())
        .bind(
            serde_json::to_string(&record)
                .map_err(|error| ServiceError::Internal(error.to_string()))?,
        )
        .bind(override_id)
        .execute(&mut *transaction)
        .await
        .map_err(|error| ServiceError::Storage(error.to_string()))?;
    }

    let rule_rows = sqlx::query_as::<_, (String, String)>(
        "SELECT id, payload FROM dictionary_rules WHERE character_id = ?",
    )
    .bind(source_id.to_string())
    .fetch_all(&mut *transaction)
    .await
    .map_err(|error| ServiceError::Storage(error.to_string()))?;
    for (rule_id, payload) in rule_rows {
        let mut rule: DictionaryRule = serde_json::from_str(&payload)
            .map_err(|error| ServiceError::Internal(error.to_string()))?;
        rule.character_id = Some(CharacterId::from_uuid(input.target_character_id));
        sqlx::query("UPDATE dictionary_rules SET character_id = ?, payload = ? WHERE id = ?")
            .bind(input.target_character_id.to_string())
            .bind(
                serde_json::to_string(&rule)
                    .map_err(|error| ServiceError::Internal(error.to_string()))?,
            )
            .bind(rule_id)
            .execute(&mut *transaction)
            .await
            .map_err(|error| ServiceError::Storage(error.to_string()))?;
    }

    let target_assignment = sqlx::query_scalar::<_, String>(
        "SELECT id FROM voice_assignments WHERE project_id = ? AND character_id = ?",
    )
    .bind(project_id.to_string())
    .bind(input.target_character_id.to_string())
    .fetch_optional(&mut *transaction)
    .await
    .map_err(|error| ServiceError::Storage(error.to_string()))?;
    let source_assignment = sqlx::query_as::<_, (String, String)>(
        "SELECT id, payload FROM voice_assignments WHERE project_id = ? AND character_id = ?",
    )
    .bind(project_id.to_string())
    .bind(source_id.to_string())
    .fetch_optional(&mut *transaction)
    .await
    .map_err(|error| ServiceError::Storage(error.to_string()))?;
    let inherited_voice = target_assignment.is_none() && source_assignment.is_some();
    if let Some((assignment_id, payload)) = source_assignment {
        if target_assignment.is_some() {
            sqlx::query("DELETE FROM voice_assignments WHERE id = ?")
                .bind(assignment_id)
                .execute(&mut *transaction)
                .await
                .map_err(|error| ServiceError::Storage(error.to_string()))?;
        } else {
            let mut assignment: VoiceAssignment = serde_json::from_str(&payload)
                .map_err(|error| ServiceError::Internal(error.to_string()))?;
            assignment.speaker = if target.role == CharacterRole::Narrator {
                Speaker::Narrator
            } else {
                Speaker::Character(CharacterId::from_uuid(input.target_character_id))
            };
            assignment.updated_at = Utc::now();
            let speaker_key = if target.role == CharacterRole::Narrator {
                "narrator".to_owned()
            } else {
                format!("character:{}", input.target_character_id)
            };
            sqlx::query(
                "UPDATE voice_assignments SET character_id = ?, speaker_key = ?, updated_at = ?, \
                 payload = ? WHERE id = ?",
            )
            .bind(input.target_character_id.to_string())
            .bind(speaker_key)
            .bind(assignment.updated_at.to_rfc3339())
            .bind(
                serde_json::to_string(&assignment)
                    .map_err(|error| ServiceError::Internal(error.to_string()))?,
            )
            .bind(assignment_id)
            .execute(&mut *transaction)
            .await
            .map_err(|error| ServiceError::Storage(error.to_string()))?;
        }
    }

    sqlx::query(
        "UPDATE characters SET role = ?, canonical_name = ?, updated_at = ?, payload = ? WHERE id = ?",
    )
    .bind(if target.role == CharacterRole::Narrator {
        "narrator"
    } else {
        "character"
    })
    .bind(&target.canonical_name)
    .bind(target.updated_at.to_rfc3339())
    .bind(
        serde_json::to_string(&target)
            .map_err(|error| ServiceError::Internal(error.to_string()))?,
    )
    .bind(input.target_character_id.to_string())
    .execute(&mut *transaction)
    .await
    .map_err(|error| ServiceError::Storage(error.to_string()))?;
    sqlx::query("DELETE FROM character_aliases WHERE character_id = ?")
        .bind(input.target_character_id.to_string())
        .execute(&mut *transaction)
        .await
        .map_err(|error| ServiceError::Storage(error.to_string()))?;
    for alias in &target.aliases {
        sqlx::query(
            "INSERT INTO character_aliases (character_id, alias, normalized_alias) VALUES (?, ?, ?)",
        )
        .bind(input.target_character_id.to_string())
        .bind(alias)
        .bind(alias.to_lowercase())
        .execute(&mut *transaction)
        .await
        .map_err(|error| ServiceError::Storage(error.to_string()))?;
    }
    sqlx::query("DELETE FROM characters WHERE id = ? AND project_id = ?")
        .bind(source_id.to_string())
        .bind(project_id.to_string())
        .execute(&mut *transaction)
        .await
        .map_err(|error| ServiceError::Storage(error.to_string()))?;
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
        for rule in &mut catalog.pronunciation_rules {
            if rule.character_id == Some(source_id) {
                rule.character_id = Some(input.target_character_id);
            }
        }
        let characters = catalog
            .characters
            .get_mut(&project_id)
            .ok_or(ServiceError::NotFound)?;
        let source_view = characters
            .iter()
            .find(|character| character.id == source_id)
            .cloned()
            .ok_or(ServiceError::NotFound)?;
        characters.retain(|character| character.id != source_id);
        let target_view = characters
            .iter_mut()
            .find(|character| character.id == input.target_character_id)
            .ok_or(ServiceError::NotFound)?;
        target_view.aliases = target.aliases;
        target_view.evidence.extend(source_view.evidence);
        target_view.dialogue_count = target_view.evidence.len();
        if target_view.voice_assignment.is_none() {
            target_view.voice_assignment = source_view.voice_assignment;
        }
        target_view.clone()
    };
    sync_character_review_catalog(&state, project_id, false, character_revision).await?;
    state.events.publish(
        "character.updated",
        serde_json::json!({
            "projectId": project_id,
            "characterId": input.target_character_id,
            "removedCharacterId": source_id,
            "characterRevision": character_revision,
            "operation": "merged",
        }),
    );
    Ok(Json(crate::models::CharacterMutationView {
        character: Some(updated),
        removed_character_id: Some(source_id),
        inherited_voice: Some(inherited_voice),
        character_revision,
    }))
}

#[allow(clippy::too_many_lines)]
pub(super) async fn delete_character(
    State(state): State<Arc<AppState>>,
    Path((project_id, character_id)): Path<(Uuid, Uuid)>,
    Json(input): Json<CharacterRevisionInput>,
) -> Result<Json<crate::models::CharacterMutationView>, ServiceError> {
    use audiobookai_core::{Character, CharacterRole};

    let project_lock = state.character_lifecycle_lock(project_id).await;
    let _project_guard = project_lock.lock().await;
    if let Some(job) = blocking_character_job(&state, project_id).await {
        return Err(active_job_conflict(&job));
    }
    let (role, payload) = sqlx::query_as::<_, (String, String)>(
        "SELECT role, payload FROM characters WHERE id = ? AND project_id = ?",
    )
    .bind(character_id.to_string())
    .bind(project_id.to_string())
    .fetch_optional(state.database.pool())
    .await
    .map_err(|error| ServiceError::Storage(error.to_string()))?
    .ok_or(ServiceError::NotFound)?;
    let mut character: Character = serde_json::from_str(&payload)
        .map_err(|error| ServiceError::Internal(error.to_string()))?;
    character.role = if role == "narrator" {
        CharacterRole::Narrator
    } else {
        CharacterRole::Character
    };
    if character.role == CharacterRole::Narrator {
        return Err(ServiceError::ConflictDetails {
            code: "protected_narrator",
            detail: "the narrator cannot be deleted".to_owned(),
            meta: serde_json::json!({ "characterId": character_id }),
        });
    }
    let dialogue_count =
        sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM dialogue_spans WHERE character_id = ?")
            .bind(character_id.to_string())
            .fetch_one(state.database.pool())
            .await
            .map_err(|error| ServiceError::Storage(error.to_string()))?;
    let override_count = sqlx::query_scalar::<_, i64>(
        "SELECT COUNT(*) FROM speaker_overrides WHERE speaker_character_id = ?",
    )
    .bind(character_id.to_string())
    .fetch_one(state.database.pool())
    .await
    .map_err(|error| ServiceError::Storage(error.to_string()))?;
    let rule_count = sqlx::query_scalar::<_, i64>(
        "SELECT COUNT(*) FROM dictionary_rules WHERE character_id = ?",
    )
    .bind(character_id.to_string())
    .fetch_one(state.database.pool())
    .await
    .map_err(|error| ServiceError::Storage(error.to_string()))?;
    if dialogue_count + override_count + rule_count > 0 {
        return Err(ServiceError::ConflictDetails {
            code: "character_in_use",
            detail: "merge this referenced character into another identity before deleting it"
                .to_owned(),
            meta: serde_json::json!({
                "dialogueSpans": dialogue_count,
                "speakerOverrides": override_count,
                "pronunciationRules": rule_count,
                "mergeRequired": true,
            }),
        });
    }
    let mut transaction = state
        .database
        .pool()
        .begin()
        .await
        .map_err(|error| ServiceError::Storage(error.to_string()))?;
    sqlx::query("DELETE FROM voice_assignments WHERE project_id = ? AND character_id = ?")
        .bind(project_id.to_string())
        .bind(character_id.to_string())
        .execute(&mut *transaction)
        .await
        .map_err(|error| ServiceError::Storage(error.to_string()))?;
    sqlx::query("DELETE FROM characters WHERE id = ? AND project_id = ?")
        .bind(character_id.to_string())
        .bind(project_id.to_string())
        .execute(&mut *transaction)
        .await
        .map_err(|error| ServiceError::Storage(error.to_string()))?;
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
    {
        let mut catalog = state.catalog.write().await;
        let characters = catalog
            .characters
            .get_mut(&project_id)
            .ok_or(ServiceError::NotFound)?;
        characters.retain(|character| character.id != character_id);
    }
    sync_character_review_catalog(&state, project_id, false, character_revision).await?;
    state.events.publish(
        "character.updated",
        serde_json::json!({
            "projectId": project_id,
            "removedCharacterId": character_id,
            "characterRevision": character_revision,
            "operation": "deleted",
        }),
    );
    Ok(Json(crate::models::CharacterMutationView {
        character: None,
        removed_character_id: Some(character_id),
        inherited_voice: None,
        character_revision,
    }))
}

pub(super) async fn approve_character_review(
    State(state): State<Arc<AppState>>,
    Path(project_id): Path<Uuid>,
    Json(input): Json<ReviewInput>,
) -> Result<Json<serde_json::Value>, ServiceError> {
    let project_lock = state.character_lifecycle_lock(project_id).await;
    let _project_guard = project_lock.lock().await;
    if let Some(job) = blocking_character_job(&state, project_id).await {
        return Err(active_job_conflict(&job));
    }
    if input.approved {
        let catalog = state.catalog.read().await;
        let characters = catalog.characters.get(&project_id).ok_or_else(|| {
            ServiceError::Conflict("run and review character detection first".to_owned())
        })?;
        if characters.is_empty()
            || characters
                .iter()
                .any(|character| character.voice_assignment.is_none())
        {
            return Err(ServiceError::Conflict(
                "assign a voice to the narrator and every detected character before approval"
                    .to_owned(),
            ));
        }
        if characters
            .iter()
            .filter(|character| matches!(character.role, audiobookai_core::CharacterRole::Narrator))
            .count()
            != 1
        {
            return Err(ServiceError::Conflict(
                "character review must contain exactly one narrator".to_owned(),
            ));
        }
    }
    let mut transaction = state
        .database
        .pool()
        .begin()
        .await
        .map_err(|error| ServiceError::Storage(error.to_string()))?;
    let character_revision = advance_character_revision_tx(
        &mut transaction,
        project_id,
        input.expected_character_revision,
        input.approved,
    )
    .await?;
    transaction
        .commit()
        .await
        .map_err(|error| ServiceError::Storage(error.to_string()))?;
    sync_character_review_catalog(&state, project_id, input.approved, character_revision).await?;
    state.events.publish(
        "character-review.updated",
        serde_json::json!({
            "projectId": project_id,
            "approved": input.approved,
            "characterRevision": character_revision,
        }),
    );
    Ok(Json(serde_json::json!({
        "reviewStatus": if input.approved { "approved" } else { "needs_review" },
        "characterRevision": character_revision,
    })))
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct SpeakerOverrideInput {
    pub(super) character_id: Option<Uuid>,
    pub(super) start_offset: usize,
    pub(super) end_offset: usize,
    pub(super) expected_character_revision: u64,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct DeleteSpeakerOverrideInput {
    pub(super) start_offset: usize,
    pub(super) end_offset: usize,
    pub(super) expected_character_revision: u64,
}

// Override validation, durable storage, and catalog projection are one atomic
// application operation and are kept linear for auditability.
#[allow(clippy::too_many_lines)]
pub(super) async fn upsert_speaker_override(
    State(state): State<Arc<AppState>>,
    Path((project_id, paragraph_id)): Path<(Uuid, Uuid)>,
    Json(input): Json<SpeakerOverrideInput>,
) -> Result<Json<serde_json::Value>, ServiceError> {
    use audiobookai_core::{
        CharacterId, Paragraph, ParagraphId, ProjectId, Speaker, SpeakerOverride, SpeakerOverrideId,
    };

    let project_lock = state.character_lifecycle_lock(project_id).await;
    let _project_guard = project_lock.lock().await;
    if let Some(job) = blocking_character_job(&state, project_id).await {
        return Err(active_job_conflict(&job));
    }
    let paragraph_payload = sqlx::query_scalar::<_, String>(
        "SELECT p.payload FROM paragraphs p \
         JOIN chapters c ON c.id = p.chapter_id \
         JOIN projects pr ON pr.book_id = c.book_id \
         WHERE p.id = ? AND pr.id = ?",
    )
    .bind(paragraph_id.to_string())
    .bind(project_id.to_string())
    .fetch_optional(state.database.pool())
    .await
    .map_err(|error| ServiceError::Storage(error.to_string()))?
    .ok_or(ServiceError::NotFound)?;
    let paragraph: Paragraph = serde_json::from_str(&paragraph_payload)
        .map_err(|error| ServiceError::Internal(error.to_string()))?;
    if input.start_offset >= input.end_offset
        || input.end_offset > paragraph.text.len()
        || !paragraph.text.is_char_boundary(input.start_offset)
        || !paragraph.text.is_char_boundary(input.end_offset)
    {
        return Err(ServiceError::InvalidRequest(
            "speaker override offsets must be valid UTF-8 byte boundaries inside the paragraph"
                .to_owned(),
        ));
    }
    let (speaker, speaker_name, speaker_character_id) = if let Some(character_id) =
        input.character_id
    {
        let character = state
            .catalog
            .read()
            .await
            .characters
            .get(&project_id)
            .and_then(|characters| characters.iter().find(|item| item.id == character_id))
            .cloned()
            .ok_or_else(|| ServiceError::InvalidRequest("unknown project character".to_owned()))?;
        if matches!(character.role, audiobookai_core::CharacterRole::Narrator) {
            (Speaker::Narrator, character.canonical_name, None)
        } else {
            (
                Speaker::Character(CharacterId::from_uuid(character_id)),
                character.canonical_name,
                Some(character_id),
            )
        }
    } else {
        (Speaker::Narrator, "Narrator".to_owned(), None)
    };
    let existing_id = sqlx::query_scalar::<_, String>(
        "SELECT id FROM speaker_overrides \
         WHERE project_id = ? AND paragraph_id = ? AND source_content_hash = ? \
         AND byte_start = ? AND byte_end = ? \
         ORDER BY updated_at DESC LIMIT 1",
    )
    .bind(project_id.to_string())
    .bind(paragraph_id.to_string())
    .bind(&paragraph.content_hash)
    .bind(
        i64::try_from(input.start_offset)
            .map_err(|_| ServiceError::InvalidRequest("offset is too large".to_owned()))?,
    )
    .bind(
        i64::try_from(input.end_offset)
            .map_err(|_| ServiceError::InvalidRequest("offset is too large".to_owned()))?,
    )
    .fetch_optional(state.database.pool())
    .await
    .map_err(|error| ServiceError::Storage(error.to_string()))?;
    let now = Utc::now();
    let record = SpeakerOverride {
        id: existing_id
            .as_deref()
            .and_then(|id| Uuid::parse_str(id).ok())
            .map_or_else(SpeakerOverrideId::new, SpeakerOverrideId::from_uuid),
        project_id: ProjectId::from_uuid(project_id),
        paragraph_id: ParagraphId::from_uuid(paragraph_id),
        source_content_hash: paragraph.content_hash,
        byte_start: input.start_offset as u64,
        byte_end: input.end_offset as u64,
        speaker,
        created_at: now,
        updated_at: now,
    };
    let mut transaction = state
        .database
        .pool()
        .begin()
        .await
        .map_err(|error| ServiceError::Storage(error.to_string()))?;
    sqlx::query(
        "INSERT INTO speaker_overrides \
         (id, project_id, paragraph_id, source_content_hash, byte_start, byte_end, updated_at, payload, speaker_character_id) \
         VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?) \
         ON CONFLICT(project_id, paragraph_id, source_content_hash, byte_start, byte_end) \
         DO UPDATE SET updated_at = excluded.updated_at, payload = excluded.payload, \
         speaker_character_id = excluded.speaker_character_id",
    )
    .bind(record.id.to_string())
    .bind(record.project_id.to_string())
    .bind(record.paragraph_id.to_string())
    .bind(&record.source_content_hash)
    .bind(i64::try_from(record.byte_start).unwrap_or(i64::MAX))
    .bind(i64::try_from(record.byte_end).unwrap_or(i64::MAX))
    .bind(record.updated_at.to_rfc3339())
    .bind(serde_json::to_string(&record).map_err(|error| ServiceError::Internal(error.to_string()))?)
    .bind(speaker_character_id.map(|id| id.to_string()))
    .execute(&mut *transaction)
    .await
    .map_err(|error| ServiceError::Storage(error.to_string()))?;
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
    apply_speaker_override_to_catalog(
        &state,
        project_id,
        paragraph_id,
        input.start_offset,
        input.end_offset,
        Some(speaker_name),
    )
    .await;
    sync_character_review_catalog(&state, project_id, false, character_revision).await?;
    Ok(Json(serde_json::json!({
        "id": record.id.as_uuid(),
        "projectId": project_id,
        "paragraphId": paragraph_id,
        "startOffset": input.start_offset,
        "endOffset": input.end_offset,
        "characterId": speaker_character_id,
        "characterRevision": character_revision,
    })))
}

pub(super) async fn delete_speaker_override(
    State(state): State<Arc<AppState>>,
    Path((project_id, paragraph_id)): Path<(Uuid, Uuid)>,
    Json(input): Json<DeleteSpeakerOverrideInput>,
) -> Result<Json<serde_json::Value>, ServiceError> {
    let project_lock = state.character_lifecycle_lock(project_id).await;
    let _project_guard = project_lock.lock().await;
    if let Some(job) = blocking_character_job(&state, project_id).await {
        return Err(active_job_conflict(&job));
    }
    let content_hash = sqlx::query_scalar::<_, String>(
        "SELECT p.content_hash FROM paragraphs p \
         JOIN chapters c ON c.id = p.chapter_id \
         JOIN projects pr ON pr.book_id = c.book_id \
         WHERE p.id = ? AND pr.id = ?",
    )
    .bind(paragraph_id.to_string())
    .bind(project_id.to_string())
    .fetch_optional(state.database.pool())
    .await
    .map_err(|error| ServiceError::Storage(error.to_string()))?
    .ok_or(ServiceError::NotFound)?;
    let mut transaction = state
        .database
        .pool()
        .begin()
        .await
        .map_err(|error| ServiceError::Storage(error.to_string()))?;
    let result = sqlx::query(
        "DELETE FROM speaker_overrides WHERE project_id = ? AND paragraph_id = ? \
         AND source_content_hash = ? AND byte_start = ? AND byte_end = ?",
    )
    .bind(project_id.to_string())
    .bind(paragraph_id.to_string())
    .bind(content_hash)
    .bind(
        i64::try_from(input.start_offset)
            .map_err(|_| ServiceError::InvalidRequest("offset is too large".to_owned()))?,
    )
    .bind(
        i64::try_from(input.end_offset)
            .map_err(|_| ServiceError::InvalidRequest("offset is too large".to_owned()))?,
    )
    .execute(&mut *transaction)
    .await
    .map_err(|error| ServiceError::Storage(error.to_string()))?;
    if result.rows_affected() == 0 {
        return Err(ServiceError::NotFound);
    }
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
    apply_speaker_override_to_catalog(
        &state,
        project_id,
        paragraph_id,
        input.start_offset,
        input.end_offset,
        None,
    )
    .await;
    sync_character_review_catalog(&state, project_id, false, character_revision).await?;
    Ok(Json(serde_json::json!({
        "paragraphId": paragraph_id,
        "startOffset": input.start_offset,
        "endOffset": input.end_offset,
        "characterRevision": character_revision,
    })))
}

pub(super) async fn apply_speaker_override_to_catalog(
    state: &AppState,
    project_id: Uuid,
    paragraph_id: Uuid,
    start_offset: usize,
    end_offset: usize,
    speaker: Option<String>,
) {
    let mut catalog = state.catalog.write().await;
    let Some(characters) = catalog.characters.get_mut(&project_id) else {
        return;
    };
    for evidence in characters
        .iter_mut()
        .flat_map(|character| &mut character.evidence)
    {
        if evidence.paragraph_id == paragraph_id
            && (end_offset == usize::MAX
                || (evidence.start_offset == start_offset && evidence.end_offset == end_offset))
        {
            evidence.speaker_override.clone_from(&speaker);
        }
    }
}
