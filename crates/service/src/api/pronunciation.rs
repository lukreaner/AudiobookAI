use super::{
    AppState, Arc, Deserialize, Json, Page, Path, PronunciationRuleView, Query, ServiceError,
    State, StatusCode, Utc, Uuid, reject_empty,
};

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct ProjectQuery {
    pub(super) project_id: Option<Uuid>,
}

pub(super) async fn list_pronunciation_rules(
    State(state): State<Arc<AppState>>,
    Query(query): Query<ProjectQuery>,
) -> Json<Page<PronunciationRuleView>> {
    let catalog = state.catalog.read().await;
    Json(Page::all(
        catalog
            .pronunciation_rules
            .iter()
            .filter(|rule| {
                query
                    .project_id
                    .is_none_or(|id| rule.project_id.is_none() || rule.project_id == Some(id))
            })
            .cloned()
            .collect(),
    ))
}

// Rule normalization, conflict validation, and durable ordering are a single
// consistency flow; splitting it risks changing precedence behavior.
#[allow(clippy::too_many_lines)]
pub(super) async fn create_pronunciation_rule(
    State(state): State<Arc<AppState>>,
    Json(mut rule): Json<PronunciationRuleView>,
) -> Result<(StatusCode, Json<PronunciationRuleView>), ServiceError> {
    use audiobookai_core::{
        CharacterId, DictionaryId, DictionaryRule, DictionaryRuleId, DictionaryRuleKind,
        DictionaryScope, PhonemeAlphabet, ProjectId, PronunciationDictionary,
    };

    let _model_lifecycle_guard = state.model_lifecycle.lock().await;

    reject_empty("source", &rule.source)?;
    reject_empty("replacement", &rule.replacement)?;
    if matches!(rule.kind, crate::models::PronunciationKindView::Regex) {
        regex::RegexBuilder::new(&rule.source)
            .case_insensitive(!rule.case_sensitive)
            .build()
            .map_err(|error| {
                ServiceError::InvalidRequest(format!("invalid pronunciation regex: {error}"))
            })?;
    }
    rule.id = Uuid::new_v4();
    let catalog = state.catalog.read().await;
    if rule
        .project_id
        .is_some_and(|id| !catalog.projects.contains_key(&id))
    {
        return Err(ServiceError::InvalidRequest("unknown project".to_owned()));
    }
    if matches!(rule.scope, crate::models::PronunciationScopeView::Project)
        && rule.project_id.is_none()
    {
        return Err(ServiceError::InvalidRequest(
            "project-scoped pronunciation rules require projectId".to_owned(),
        ));
    }
    if matches!(rule.scope, crate::models::PronunciationScopeView::Global)
        && rule.project_id.is_some()
    {
        return Err(ServiceError::InvalidRequest(
            "global pronunciation rules must omit projectId".to_owned(),
        ));
    }
    if rule.character_id.is_some_and(|character_id| {
        !catalog
            .characters
            .values()
            .flatten()
            .any(|character| character.id == character_id)
    }) {
        return Err(ServiceError::InvalidRequest("unknown character".to_owned()));
    }
    rule.conflict = catalog
        .pronunciation_rules
        .iter()
        .find(|existing| {
            existing.enabled
                && existing.project_id == rule.project_id
                && existing.language == rule.language
                && existing.character_id == rule.character_id
                && existing.source.eq_ignore_ascii_case(&rule.source)
        })
        .map(|existing| format!("overlaps rule {}", existing.id));
    drop(catalog);

    let scope_name = match rule.scope {
        crate::models::PronunciationScopeView::Global => "global",
        crate::models::PronunciationScopeView::Project => "project",
    };
    let project_key = rule.project_id.map(|id| id.to_string());
    let dictionary_payload = if let Some(payload) = sqlx::query_scalar::<_, String>(
        "SELECT payload FROM dictionaries WHERE scope = ? AND project_id IS ? ORDER BY updated_at LIMIT 1",
    )
    .bind(scope_name)
    .bind(project_key.as_deref())
    .fetch_optional(state.database.pool())
    .await
    .map_err(|error| ServiceError::Storage(error.to_string()))?
    {
        serde_json::from_str::<PronunciationDictionary>(&payload)
            .map_err(|error| ServiceError::Internal(error.to_string()))?
    } else {
        let now = Utc::now();
        PronunciationDictionary {
            id: DictionaryId::new(),
            name: rule.project_id.map_or_else(
                || "Global pronunciation dictionary".to_owned(),
                |_| "Project pronunciation dictionary".to_owned(),
            ),
            scope: match rule.scope {
                crate::models::PronunciationScopeView::Global => DictionaryScope::Global,
                crate::models::PronunciationScopeView::Project => DictionaryScope::Project,
            },
            project_id: rule.project_id.map(ProjectId::from_uuid),
            enabled: true,
            revision: 0,
            created_at: now,
            updated_at: now,
        }
    };
    let mut dictionary = dictionary_payload;
    dictionary.revision = dictionary.revision.saturating_add(1);
    dictionary.updated_at = Utc::now();
    let domain_rule = DictionaryRule {
        id: DictionaryRuleId::from_uuid(rule.id),
        dictionary_id: dictionary.id,
        ordinal: rule.order,
        kind: match rule.kind {
            crate::models::PronunciationKindView::Literal => DictionaryRuleKind::Literal,
            crate::models::PronunciationKindView::WholeWord => DictionaryRuleKind::WholeWord,
            crate::models::PronunciationKindView::Regex => DictionaryRuleKind::Regex,
            crate::models::PronunciationKindView::Alias => DictionaryRuleKind::Alias,
            crate::models::PronunciationKindView::Phoneme => DictionaryRuleKind::Phoneme,
        },
        pattern: rule.source.clone(),
        replacement: rule.replacement.clone(),
        case_sensitive: rule.case_sensitive,
        language: rule.language.clone(),
        character_id: rule.character_id.map(CharacterId::from_uuid),
        phoneme_alphabet: matches!(rule.kind, crate::models::PronunciationKindView::Phoneme)
            .then_some(PhonemeAlphabet::Ipa),
        enabled: rule.enabled,
    };
    let mut transaction = state
        .database
        .pool()
        .begin()
        .await
        .map_err(|error| ServiceError::Storage(error.to_string()))?;
    sqlx::query(
        "INSERT INTO dictionaries \
         (id, project_id, scope, name, revision, enabled, updated_at, payload) \
         VALUES (?, ?, ?, ?, ?, ?, ?, ?) \
         ON CONFLICT(id) DO UPDATE SET name = excluded.name, revision = excluded.revision, \
         enabled = excluded.enabled, updated_at = excluded.updated_at, payload = excluded.payload",
    )
    .bind(dictionary.id.to_string())
    .bind(dictionary.project_id.map(|id| id.to_string()))
    .bind(scope_name)
    .bind(&dictionary.name)
    .bind(i64::try_from(dictionary.revision).unwrap_or(i64::MAX))
    .bind(dictionary.enabled)
    .bind(dictionary.updated_at.to_rfc3339())
    .bind(
        serde_json::to_string(&dictionary)
            .map_err(|error| ServiceError::Internal(error.to_string()))?,
    )
    .execute(&mut *transaction)
    .await
    .map_err(|error| ServiceError::Storage(error.to_string()))?;
    let storage_ordinal = sqlx::query_scalar::<_, i64>(
        "SELECT COALESCE(MAX(ordinal), -1) + 1 FROM dictionary_rules WHERE dictionary_id = ?",
    )
    .bind(dictionary.id.to_string())
    .fetch_one(&mut *transaction)
    .await
    .map_err(|error| ServiceError::Storage(error.to_string()))?;
    sqlx::query(
        "INSERT INTO dictionary_rules \
         (id, dictionary_id, ordinal, kind, enabled, payload, character_id) \
         VALUES (?, ?, ?, ?, ?, ?, ?)",
    )
    .bind(domain_rule.id.to_string())
    .bind(DictionaryId::from_uuid(dictionary.id.as_uuid()).to_string())
    .bind(storage_ordinal)
    .bind(match domain_rule.kind {
        DictionaryRuleKind::Literal => "literal",
        DictionaryRuleKind::WholeWord => "whole_word",
        DictionaryRuleKind::Regex => "regex",
        DictionaryRuleKind::Alias => "alias",
        DictionaryRuleKind::Phoneme => "phoneme",
    })
    .bind(domain_rule.enabled)
    .bind(
        serde_json::to_string(&domain_rule)
            .map_err(|error| ServiceError::Internal(error.to_string()))?,
    )
    .bind(domain_rule.character_id.map(|id| id.to_string()))
    .execute(&mut *transaction)
    .await
    .map_err(|error| ServiceError::Storage(error.to_string()))?;
    transaction
        .commit()
        .await
        .map_err(|error| ServiceError::Storage(error.to_string()))?;
    state
        .catalog
        .write()
        .await
        .pronunciation_rules
        .push(rule.clone());
    Ok((StatusCode::CREATED, Json(rule)))
}

pub(super) async fn delete_pronunciation_rule(
    State(state): State<Arc<AppState>>,
    Path(id): Path<Uuid>,
) -> Result<StatusCode, ServiceError> {
    let _model_lifecycle_guard = state.model_lifecycle.lock().await;
    let dictionary_id =
        sqlx::query_scalar::<_, String>("SELECT dictionary_id FROM dictionary_rules WHERE id = ?")
            .bind(id.to_string())
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
    let result = sqlx::query("DELETE FROM dictionary_rules WHERE id = ?")
        .bind(id.to_string())
        .execute(&mut *transaction)
        .await
        .map_err(|error| ServiceError::Storage(error.to_string()))?;
    if result.rows_affected() == 0 {
        return Err(ServiceError::NotFound);
    }
    let payload = sqlx::query_scalar::<_, String>("SELECT payload FROM dictionaries WHERE id = ?")
        .bind(&dictionary_id)
        .fetch_one(&mut *transaction)
        .await
        .map_err(|error| ServiceError::Storage(error.to_string()))?;
    let mut dictionary: audiobookai_core::PronunciationDictionary = serde_json::from_str(&payload)
        .map_err(|error| ServiceError::Internal(error.to_string()))?;
    dictionary.revision = dictionary.revision.saturating_add(1);
    dictionary.updated_at = Utc::now();
    sqlx::query("UPDATE dictionaries SET revision = ?, updated_at = ?, payload = ? WHERE id = ?")
        .bind(i64::try_from(dictionary.revision).unwrap_or(i64::MAX))
        .bind(dictionary.updated_at.to_rfc3339())
        .bind(
            serde_json::to_string(&dictionary)
                .map_err(|error| ServiceError::Internal(error.to_string()))?,
        )
        .bind(dictionary_id)
        .execute(&mut *transaction)
        .await
        .map_err(|error| ServiceError::Storage(error.to_string()))?;
    transaction
        .commit()
        .await
        .map_err(|error| ServiceError::Storage(error.to_string()))?;
    state
        .catalog
        .write()
        .await
        .pronunciation_rules
        .retain(|rule| rule.id != id);
    Ok(StatusCode::NO_CONTENT)
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct PronunciationPreviewInput {
    pub(super) text: String,
    pub(super) project_id: Option<Uuid>,
    pub(super) character_id: Option<Uuid>,
    pub(super) language: Option<String>,
}

pub(super) async fn preview_pronunciation_rules(
    State(state): State<Arc<AppState>>,
    Json(input): Json<PronunciationPreviewInput>,
) -> Result<Json<serde_json::Value>, ServiceError> {
    if input.text.len() > 256 * 1024 {
        return Err(ServiceError::InvalidRequest(
            "pronunciation preview text exceeds 256 KiB".to_owned(),
        ));
    }
    let mut rules = state
        .catalog
        .read()
        .await
        .pronunciation_rules
        .iter()
        .filter(|rule| {
            rule.enabled
                && match rule.scope {
                    crate::models::PronunciationScopeView::Global => true,
                    crate::models::PronunciationScopeView::Project => {
                        rule.project_id == input.project_id
                    }
                }
                && rule
                    .character_id
                    .is_none_or(|id| Some(id) == input.character_id)
                && rule.language.as_ref().is_none_or(|language| {
                    input
                        .language
                        .as_ref()
                        .is_some_and(|requested| requested.eq_ignore_ascii_case(language))
                })
        })
        .cloned()
        .collect::<Vec<_>>();
    rules.sort_by_key(|rule| {
        (
            matches!(rule.scope, crate::models::PronunciationScopeView::Project),
            rule.order,
            rule.id,
        )
    });
    let original = input.text;
    let mut transformed = original.clone();
    let mut applied = Vec::new();
    let mut conflicts = Vec::new();
    for rule in rules {
        if let Some(conflict) = &rule.conflict {
            conflicts.push(serde_json::json!({ "ruleId": rule.id, "detail": conflict }));
        }
        let before = transformed.clone();
        transformed = apply_pronunciation_rule(&transformed, &rule)?;
        if transformed != before {
            applied.push(rule.id);
        }
    }
    Ok(Json(serde_json::json!({
        "originalText": original,
        "transformedText": transformed,
        "appliedRuleIds": applied,
        "conflicts": conflicts,
    })))
}

pub(crate) fn apply_pronunciation_rule(
    text: &str,
    rule: &PronunciationRuleView,
) -> Result<String, ServiceError> {
    use crate::models::PronunciationKindView;

    let pattern = match rule.kind {
        PronunciationKindView::Literal | PronunciationKindView::Phoneme => {
            regex::escape(&rule.source)
        }
        PronunciationKindView::WholeWord | PronunciationKindView::Alias => {
            format!(r"\b{}\b", regex::escape(&rule.source))
        }
        PronunciationKindView::Regex => rule.source.clone(),
    };
    let expression = regex::RegexBuilder::new(&pattern)
        .case_insensitive(!rule.case_sensitive)
        .unicode(true)
        .build()
        .map_err(|error| {
            ServiceError::InvalidRequest(format!("invalid pronunciation regex: {error}"))
        })?;
    if matches!(rule.kind, PronunciationKindView::Regex) {
        Ok(expression
            .replace_all(text, rule.replacement.as_str())
            .into_owned())
    } else {
        Ok(expression
            .replace_all(text, |_captures: &regex::Captures<'_>| {
                rule.replacement.as_str()
            })
            .into_owned())
    }
}
