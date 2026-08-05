use std::collections::BTreeMap;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::{
    AttemptId, BudgetId, JobId, Money, ProjectId, ProvenanceQuality, ProviderProfileId, RateCardId,
    ReservationId, UsageEventId, Validate, ValidationIssue, VoiceProfileId,
};

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum UsageWorkload {
    Tts,
    CharacterDetection,
}

#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
pub struct UsageQuantities {
    pub characters: Option<u64>,
    pub audio_milliseconds: Option<u64>,
    pub input_tokens: Option<u64>,
    pub output_tokens: Option<u64>,
    pub cache_read_tokens: Option<u64>,
    pub cache_write_tokens: Option<u64>,
    pub reasoning_tokens: Option<u64>,
    pub provider_credits: Option<i64>,
}

impl UsageQuantities {
    #[must_use]
    pub fn amount_for(&self, metric: BudgetMetric, cost: Option<&Money>) -> Option<i64> {
        let unsigned = match metric {
            BudgetMetric::Characters => self.characters,
            BudgetMetric::AudioMilliseconds => self.audio_milliseconds,
            BudgetMetric::InputTokens => self.input_tokens,
            BudgetMetric::OutputTokens => self.output_tokens,
            BudgetMetric::TotalTokens => Some(
                self.input_tokens?
                    .saturating_add(self.output_tokens.unwrap_or_default())
                    .saturating_add(self.reasoning_tokens.unwrap_or_default()),
            ),
            BudgetMetric::ProviderCredits => return self.provider_credits,
            BudgetMetric::MoneyMicros => return cost.map(|money| money.micros),
        }?;
        i64::try_from(unsigned).ok()
    }
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct UsageEvent {
    pub id: UsageEventId,
    pub occurred_at: DateTime<Utc>,
    pub workload: UsageWorkload,
    pub project_id: ProjectId,
    pub job_id: Option<JobId>,
    pub attempt_id: Option<AttemptId>,
    pub chapter_id: Option<crate::ChapterId>,
    pub segment_id: Option<crate::SegmentId>,
    pub provider_profile_id: ProviderProfileId,
    pub provider_family: String,
    pub endpoint_family: String,
    pub model: Option<String>,
    pub voice_profile_id: Option<VoiceProfileId>,
    pub provider_request_id: Option<String>,
    pub quantities: UsageQuantities,
    pub quantity_source: ProvenanceQuality,
    pub cost: Option<Money>,
    pub cost_source: ProvenanceQuality,
    pub rate_card_id: Option<RateCardId>,
    pub uncertain_charge: bool,
    #[serde(default)]
    pub redacted_raw_usage: BTreeMap<String, serde_json::Value>,
}

impl Validate for UsageEvent {
    fn validation_issues(&self) -> Vec<ValidationIssue> {
        let mut issues = Vec::new();
        if let Some(cost) = &self.cost {
            for issue in cost.validation_issues() {
                issues.push(ValidationIssue::new(
                    format!("cost.{}", issue.path),
                    issue.code,
                    issue.message,
                ));
            }
        }
        issues
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum BudgetScopeKind {
    Global,
    Provider,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct BudgetScope {
    pub kind: BudgetScopeKind,
    pub provider_profile_id: Option<ProviderProfileId>,
}

impl Validate for BudgetScope {
    fn validation_issues(&self) -> Vec<ValidationIssue> {
        let mut issues = Vec::new();
        match (self.kind, self.provider_profile_id) {
            (BudgetScopeKind::Global, Some(_)) => issues.push(ValidationIssue::new(
                "provider_profile_id",
                "must_be_omitted",
                "global budgets cannot name a provider",
            )),
            (BudgetScopeKind::Provider, None) => issues.push(ValidationIssue::new(
                "provider_profile_id",
                "required",
                "provider budgets require a provider profile id",
            )),
            _ => {}
        }
        issues
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum BudgetPeriod {
    Job,
    Daily,
    Monthly,
    Lifetime,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, Hash, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum BudgetMetric {
    MoneyMicros,
    Characters,
    AudioMilliseconds,
    InputTokens,
    OutputTokens,
    TotalTokens,
    ProviderCredits,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct Budget {
    pub id: BudgetId,
    pub name: String,
    pub scope: BudgetScope,
    pub period: BudgetPeriod,
    pub metric: BudgetMetric,
    pub currency: Option<String>,
    pub limit: i64,
    pub used: i64,
    pub warning_threshold_percent: u8,
    pub hard: bool,
    pub enabled: bool,
    pub period_started_at: DateTime<Utc>,
    pub period_ends_at: Option<DateTime<Utc>>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

impl Budget {
    #[must_use]
    pub fn remaining(&self, actively_reserved: i64) -> i64 {
        self.limit
            .saturating_sub(self.used)
            .saturating_sub(actively_reserved)
    }
}

impl Validate for Budget {
    fn validation_issues(&self) -> Vec<ValidationIssue> {
        let mut issues = self.scope.validation_issues();
        if self.name.trim().is_empty() {
            issues.push(ValidationIssue::new(
                "name",
                "required",
                "budget name must not be empty",
            ));
        }
        if self.limit < 0 || self.used < 0 {
            issues.push(ValidationIssue::new(
                "limit",
                "out_of_range",
                "budget limit and usage must not be negative",
            ));
        }
        if self.warning_threshold_percent > 100 {
            issues.push(ValidationIssue::new(
                "warning_threshold_percent",
                "out_of_range",
                "warning threshold must be between 0 and 100",
            ));
        }
        match (self.metric, self.currency.as_deref()) {
            (BudgetMetric::MoneyMicros, Some(currency))
                if currency.len() == 3
                    && currency.bytes().all(|byte| byte.is_ascii_uppercase()) => {}
            (BudgetMetric::MoneyMicros, _) => issues.push(ValidationIssue::new(
                "currency",
                "required",
                "monetary budgets require an ISO currency",
            )),
            (_, Some(_)) => issues.push(ValidationIssue::new(
                "currency",
                "must_be_omitted",
                "currency is only valid for monetary budgets",
            )),
            _ => {}
        }
        issues
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ReservationStatus {
    Active,
    Reconciled,
    Released,
    Expired,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct BudgetAllocation {
    pub budget_id: BudgetId,
    pub reserved_amount: i64,
    pub actual_amount: Option<i64>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct BudgetReservation {
    pub id: ReservationId,
    pub job_id: JobId,
    pub status: ReservationStatus,
    pub allocations: Vec<BudgetAllocation>,
    pub created_at: DateTime<Utc>,
    pub expires_at: Option<DateTime<Utc>>,
    pub reconciled_at: Option<DateTime<Utc>>,
}

impl Validate for BudgetReservation {
    fn validation_issues(&self) -> Vec<ValidationIssue> {
        let mut issues = Vec::new();
        if self.allocations.is_empty() {
            issues.push(ValidationIssue::new(
                "allocations",
                "required",
                "a reservation requires at least one budget allocation",
            ));
        }
        for (index, allocation) in self.allocations.iter().enumerate() {
            if allocation.reserved_amount < 0 || allocation.actual_amount.is_some_and(|v| v < 0) {
                issues.push(ValidationIssue::new(
                    format!("allocations[{index}]"),
                    "out_of_range",
                    "reserved and actual amounts must not be negative",
                ));
            }
        }
        issues
    }
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct RateCard {
    pub id: RateCardId,
    pub provider_profile_id: ProviderProfileId,
    pub model: Option<String>,
    pub workload: UsageWorkload,
    pub currency: String,
    pub effective_at: DateTime<Utc>,
    pub expires_at: Option<DateTime<Utc>>,
    pub source: String,
    pub source_url: Option<String>,
    pub pricing: BTreeMap<String, i64>,
    pub user_overridden: bool,
}

#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
pub struct UsageTotals {
    pub event_count: u64,
    pub quantities: UsageQuantities,
    pub cost_by_currency_micros: BTreeMap<String, i64>,
    pub uncertain_charge_count: u64,
}
