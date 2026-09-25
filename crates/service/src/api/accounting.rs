use super::{
    AppState, Arc, BTreeMap, BudgetView, ChronoDuration, CreateBudgetInput, Deserialize, Json,
    Page, Path, ServiceError, State, StatusCode, UsageSummaryView, Utc, Uuid,
    provider_capabilities_are_fresh, reject_empty,
};

pub(super) async fn usage_summary(State(state): State<Arc<AppState>>) -> Json<UsageSummaryView> {
    let catalog = state.catalog.read().await;
    let period_end = Utc::now();
    let period_start = period_end - ChronoDuration::days(30);
    let rows = catalog.usage_rows.clone();
    let unknown_cost_requests = rows.iter().filter(|row| row.cost_micros.is_none()).count() as u64;
    let cost = rows.iter().filter_map(|row| row.cost_micros).sum::<i64>();
    let characters = rows.iter().filter_map(|row| row.characters).sum::<u64>();
    let input_tokens = rows.iter().filter_map(|row| row.input_tokens).sum::<u64>();
    let output_tokens = rows.iter().filter_map(|row| row.output_tokens).sum::<u64>();
    Json(UsageSummaryView {
        period_start,
        period_end,
        currency: None,
        monetary_cost_micros: (!rows.is_empty()).then_some(cost),
        characters: (!rows.is_empty()).then_some(characters),
        input_tokens: (!rows.is_empty()).then_some(input_tokens),
        output_tokens: (!rows.is_empty()).then_some(output_tokens),
        credits: None,
        unknown_cost_requests,
        rows,
    })
}

pub(super) async fn list_budgets(State(state): State<Arc<AppState>>) -> Json<Page<BudgetView>> {
    let catalog = state.catalog.read().await;
    Json(Page::all(catalog.budgets.values().cloned().collect()))
}

pub(super) async fn create_budget(
    State(state): State<Arc<AppState>>,
    Json(input): Json<CreateBudgetInput>,
) -> Result<(StatusCode, Json<BudgetView>), ServiceError> {
    reject_empty("name", &input.name)?;
    if input.limit < 0 || input.warning_percent > 100 {
        return Err(ServiceError::InvalidRequest(
            "budget limit must be non-negative and warningPercent must be 0..100".to_owned(),
        ));
    }
    if matches!(input.metric, crate::models::BudgetMetricView::Money)
        && input.currency.as_ref().is_none_or(|currency| {
            currency.len() != 3 || !currency.bytes().all(|byte| byte.is_ascii_uppercase())
        })
    {
        return Err(ServiceError::InvalidRequest(
            "monetary budgets require a three-letter uppercase currency".to_owned(),
        ));
    }
    if !matches!(input.metric, crate::models::BudgetMetricView::Money) && input.currency.is_some() {
        return Err(ServiceError::InvalidRequest(
            "currency is valid only for monetary budgets".to_owned(),
        ));
    }
    if let Some(provider_id) = input.provider_profile_id
        && !state
            .catalog
            .read()
            .await
            .providers
            .contains_key(&provider_id)
    {
        return Err(ServiceError::InvalidRequest(
            "providerProfileId does not identify a configured provider".to_owned(),
        ));
    }
    let budget = BudgetView {
        id: Uuid::new_v4(),
        name: input.name,
        provider_profile_id: input.provider_profile_id,
        period: input.period,
        metric: input.metric,
        limit: input.limit,
        used: 0,
        reserved: 0,
        hard: input.hard,
        currency: input.currency,
        warning_percent: input.warning_percent,
    };
    persist_budget(&state, &budget).await?;
    state
        .catalog
        .write()
        .await
        .budgets
        .insert(budget.id, budget.clone());
    Ok((StatusCode::CREATED, Json(budget)))
}

pub(super) async fn persist_budget(
    state: &AppState,
    view: &BudgetView,
) -> Result<(), ServiceError> {
    use audiobookai_core::{
        Budget, BudgetId, BudgetMetric, BudgetPeriod, BudgetScope, BudgetScopeKind,
        ProviderProfileId,
    };
    let now = Utc::now();
    let period = match view.period {
        crate::models::BudgetPeriodView::Job => BudgetPeriod::Job,
        crate::models::BudgetPeriodView::Daily => BudgetPeriod::Daily,
        crate::models::BudgetPeriodView::Monthly => BudgetPeriod::Monthly,
        crate::models::BudgetPeriodView::Lifetime => BudgetPeriod::Lifetime,
    };
    let period_ends_at = match period {
        BudgetPeriod::Job | BudgetPeriod::Lifetime => None,
        BudgetPeriod::Daily => Some(now + ChronoDuration::days(1)),
        BudgetPeriod::Monthly => Some(now + ChronoDuration::days(31)),
    };
    let budget = Budget {
        id: BudgetId::from_uuid(view.id),
        name: view.name.clone(),
        scope: BudgetScope {
            kind: if view.provider_profile_id.is_some() {
                BudgetScopeKind::Provider
            } else {
                BudgetScopeKind::Global
            },
            provider_profile_id: view.provider_profile_id.map(ProviderProfileId::from_uuid),
        },
        period,
        metric: match view.metric {
            crate::models::BudgetMetricView::Money => BudgetMetric::MoneyMicros,
            crate::models::BudgetMetricView::Tokens => BudgetMetric::TotalTokens,
            crate::models::BudgetMetricView::Characters => BudgetMetric::Characters,
            crate::models::BudgetMetricView::Credits => BudgetMetric::ProviderCredits,
        },
        currency: view.currency.clone(),
        limit: view.limit,
        used: view.used,
        warning_threshold_percent: view.warning_percent,
        hard: view.hard,
        enabled: true,
        period_started_at: now,
        period_ends_at,
        created_at: now,
        updated_at: now,
    };
    state
        .database
        .repositories()
        .budgets
        .upsert(&budget)
        .await
        .map_err(|error| ServiceError::Storage(error.to_string()))
}

pub(super) async fn delete_budget(
    State(state): State<Arc<AppState>>,
    Path(id): Path<Uuid>,
) -> Result<StatusCode, ServiceError> {
    let active = sqlx::query_scalar::<_, i64>(
        "SELECT COUNT(*) FROM budget_allocations a \
         JOIN budget_reservations r ON r.id = a.reservation_id \
         WHERE a.budget_id = ? AND r.status = 'active'",
    )
    .bind(id.to_string())
    .fetch_one(state.database.pool())
    .await
    .map_err(|error| ServiceError::Storage(error.to_string()))?;
    if active > 0 {
        return Err(ServiceError::Conflict(
            "an active job still holds a reservation against this budget".to_owned(),
        ));
    }
    let result = sqlx::query("DELETE FROM budgets WHERE id = ?")
        .bind(id.to_string())
        .execute(state.database.pool())
        .await
        .map_err(|error| ServiceError::Storage(error.to_string()))?;
    if result.rows_affected() == 0 {
        return Err(ServiceError::NotFound);
    }
    state.catalog.write().await.budgets.remove(&id);
    Ok(StatusCode::NO_CONTENT)
}

#[derive(Clone, Debug, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct RateCardView {
    pub(super) id: Uuid,
    pub(super) provider_profile_id: Uuid,
    pub(super) model: Option<String>,
    pub(super) workload: audiobookai_core::UsageWorkload,
    pub(super) currency: String,
    pub(super) effective_at: chrono::DateTime<Utc>,
    pub(super) expires_at: Option<chrono::DateTime<Utc>>,
    pub(super) source: String,
    pub(super) source_url: Option<String>,
    pub(super) pricing: BTreeMap<String, i64>,
    pub(super) user_overridden: bool,
}

impl From<audiobookai_core::RateCard> for RateCardView {
    fn from(card: audiobookai_core::RateCard) -> Self {
        Self {
            id: card.id.as_uuid(),
            provider_profile_id: card.provider_profile_id.as_uuid(),
            model: card.model,
            workload: card.workload,
            currency: card.currency,
            effective_at: card.effective_at,
            expires_at: card.expires_at,
            source: card.source,
            source_url: card.source_url,
            pricing: card.pricing,
            user_overridden: card.user_overridden,
        }
    }
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct RateCardInput {
    pub(super) provider_profile_id: Uuid,
    pub(super) model: Option<String>,
    pub(super) workload: audiobookai_core::UsageWorkload,
    pub(super) currency: String,
    pub(super) effective_at: Option<chrono::DateTime<Utc>>,
    pub(super) expires_at: Option<chrono::DateTime<Utc>>,
    pub(super) source: String,
    pub(super) source_url: Option<String>,
    pub(super) pricing: BTreeMap<String, i64>,
}

pub(super) async fn list_rate_cards(
    State(state): State<Arc<AppState>>,
) -> Result<Json<Page<RateCardView>>, ServiceError> {
    use sqlx::Row;

    let rows = sqlx::query("SELECT payload FROM rate_cards ORDER BY effective_at DESC")
        .fetch_all(state.database.pool())
        .await
        .map_err(|error| ServiceError::Storage(error.to_string()))?;
    let cards = rows
        .into_iter()
        .map(|row| {
            serde_json::from_str::<audiobookai_core::RateCard>(row.get::<&str, _>("payload"))
                .map(RateCardView::from)
                .map_err(|error| ServiceError::Internal(error.to_string()))
        })
        .collect::<Result<Vec<_>, _>>()?;
    Ok(Json(Page::all(cards)))
}

pub(super) async fn create_rate_card(
    State(state): State<Arc<AppState>>,
    Json(input): Json<RateCardInput>,
) -> Result<(StatusCode, Json<RateCardView>), ServiceError> {
    use audiobookai_core::{RateCard, RateCardId, UsageWorkload};

    let provider = state
        .catalog
        .read()
        .await
        .providers
        .get(&input.provider_profile_id)
        .cloned()
        .ok_or_else(|| {
            ServiceError::InvalidRequest(
                "providerProfileId does not identify a configured provider".to_owned(),
            )
        })?;
    let capability_matches = provider_capabilities_are_fresh(&provider)
        && provider
            .capabilities
            .as_ref()
            .is_some_and(|capabilities| match input.workload {
                UsageWorkload::Tts => capabilities.tts,
                UsageWorkload::CharacterDetection => capabilities.character_detection,
            });
    if !capability_matches {
        return Err(ServiceError::InvalidRequest(
            "the selected provider does not support this rate-card workload".to_owned(),
        ));
    }
    let currency = input.currency.trim().to_ascii_uppercase();
    if currency.len() != 3 || !currency.bytes().all(|value| value.is_ascii_uppercase()) {
        return Err(ServiceError::InvalidRequest(
            "currency must be a three-letter ISO code".to_owned(),
        ));
    }
    let source = input.source.trim();
    if source.is_empty() || source.chars().count() > 160 {
        return Err(ServiceError::InvalidRequest(
            "rate-card source must contain 1 to 160 characters".to_owned(),
        ));
    }
    validate_rate_pricing(input.workload, &input.pricing)?;
    let effective_at = input.effective_at.unwrap_or_else(Utc::now);
    if input
        .expires_at
        .is_some_and(|expires| expires <= effective_at)
    {
        return Err(ServiceError::InvalidRequest(
            "rate-card expiry must be later than its effective time".to_owned(),
        ));
    }
    let card = RateCard {
        id: RateCardId::new(),
        provider_profile_id: audiobookai_core::ProviderProfileId::from_uuid(
            input.provider_profile_id,
        ),
        model: input
            .model
            .map(|model| model.trim().to_owned())
            .filter(|model| !model.is_empty()),
        workload: input.workload,
        currency,
        effective_at,
        expires_at: input.expires_at,
        source: source.to_owned(),
        source_url: input
            .source_url
            .map(|value| value.trim().to_owned())
            .filter(|value| !value.is_empty()),
        pricing: input.pricing,
        user_overridden: true,
    };
    persist_rate_card(&state, &card).await?;
    Ok((StatusCode::CREATED, Json(RateCardView::from(card))))
}

pub(super) fn validate_rate_pricing(
    workload: audiobookai_core::UsageWorkload,
    pricing: &BTreeMap<String, i64>,
) -> Result<(), ServiceError> {
    const TTS_KEYS: &[&str] = &[
        "per_character_micros",
        "character_micros",
        "per_1000_characters_micros",
        "provider_credits_per_character_micros",
        "credits_per_character_micros",
    ];
    const AI_KEYS: &[&str] = &[
        "per_input_token_micros",
        "input_token_micros",
        "per_1m_input_tokens_micros",
        "per_output_token_micros",
        "output_token_micros",
        "per_1m_output_tokens_micros",
        "per_cached_input_token_micros",
        "cached_input_token_micros",
        "per_1m_cached_input_tokens_micros",
        "per_reasoning_token_micros",
        "reasoning_token_micros",
        "per_1m_reasoning_tokens_micros",
    ];
    let allowed = match workload {
        audiobookai_core::UsageWorkload::Tts => TTS_KEYS,
        audiobookai_core::UsageWorkload::CharacterDetection => AI_KEYS,
    };
    if pricing.is_empty() {
        return Err(ServiceError::InvalidRequest(
            "a rate card requires at least one price".to_owned(),
        ));
    }
    if let Some((key, _)) = pricing
        .iter()
        .find(|(key, value)| !allowed.contains(&key.as_str()) || **value < 0)
    {
        return Err(ServiceError::InvalidRequest(format!(
            "unsupported or negative rate-card price: {key}"
        )));
    }
    Ok(())
}

pub(super) async fn persist_rate_card(
    state: &AppState,
    card: &audiobookai_core::RateCard,
) -> Result<(), ServiceError> {
    sqlx::query(
        "INSERT INTO rate_cards \
         (id, provider_id, model, workload, currency, effective_at, expires_at, payload) \
         VALUES (?, ?, ?, ?, ?, ?, ?, ?)",
    )
    .bind(card.id.to_string())
    .bind(card.provider_profile_id.to_string())
    .bind(&card.model)
    .bind(crate::accounting::workload_name(card.workload))
    .bind(&card.currency)
    .bind(card.effective_at.to_rfc3339())
    .bind(card.expires_at.map(|value| value.to_rfc3339()))
    .bind(serde_json::to_string(card).map_err(|error| ServiceError::Internal(error.to_string()))?)
    .execute(state.database.pool())
    .await
    .map_err(|error| ServiceError::Storage(error.to_string()))?;
    Ok(())
}

pub(super) async fn delete_rate_card(
    State(state): State<Arc<AppState>>,
    Path(id): Path<Uuid>,
) -> Result<StatusCode, ServiceError> {
    let result = sqlx::query("DELETE FROM rate_cards WHERE id = ?")
        .bind(id.to_string())
        .execute(state.database.pool())
        .await
        .map_err(|error| ServiceError::Storage(error.to_string()))?;
    if result.rows_affected() == 0 {
        return Err(ServiceError::NotFound);
    }
    Ok(StatusCode::NO_CONTENT)
}
