use std::collections::BTreeMap;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::{Validate, ValidationIssue, error::require_currency};

#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
pub struct SourceProvenance {
    pub source: String,
    pub source_version: Option<String>,
    pub request_id: Option<String>,
    pub observed_at: Option<DateTime<Utc>>,
    #[serde(default)]
    pub attributes: BTreeMap<String, String>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct Money {
    pub micros: i64,
    pub currency: String,
}

impl Money {
    #[must_use]
    pub fn zero(currency: impl Into<String>) -> Self {
        Self {
            micros: 0,
            currency: currency.into(),
        }
    }
}

impl Validate for Money {
    fn validation_issues(&self) -> Vec<ValidationIssue> {
        let mut issues = Vec::new();
        require_currency(&mut issues, "currency", &self.currency);
        issues
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct FileFingerprint {
    pub algorithm: String,
    pub digest: String,
    pub size_bytes: u64,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ProvenanceQuality {
    Reported,
    Estimated,
    Derived,
    Unknown,
}

#[derive(Clone, Debug, Default, Deserialize, PartialEq, Serialize)]
#[serde(transparent)]
pub struct SettingsMap(pub BTreeMap<String, serde_json::Value>);
