//! Deterministic media quality-control primitives.
//!
//! The analyzers in this module do not execute media tools and do not attempt speech
//! recognition. [`MediaQcPlanner`] builds the sidecar commands needed to obtain decoded PCM and
//! file metadata; callers execute those commands and pass their outputs to the pure analyzers.

mod mp3;
mod pcm;
mod probe;

use serde::{Deserialize, Serialize};

pub use mp3::*;
pub use pcm::*;
pub use probe::*;

/// Stable, machine-readable identifiers for QC findings.
///
/// Serialized names are part of the QC report contract and should not be renamed. New finding
/// kinds should be added as new variants instead.
#[derive(Clone, Copy, Debug, Deserialize, Eq, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum QcFindingCode {
    DecodeInvalid,
    FileMetadataInvalid,
    FileAudioStreamMissing,
    FileMultipleAudioStreams,
    FileContainerUnexpected,
    FileCodecUnexpected,
    FileSampleRateUnexpected,
    FileChannelCountUnexpected,
    FileBitrateOutOfRange,
    FileDurationOutOfRange,
    PcmEmpty,
    PcmNonFinite,
    PcmSilent,
    PcmClipping,
    PcmRmsOutOfRange,
    PcmSamplePeakExceeded,
    PcmTruePeakExceeded,
    PcmLeadingSilenceOutOfRange,
    PcmTrailingSilenceOutOfRange,
    PcmLongSilence,
    PcmAbruptJoin,
    Mp3Invalid,
    Mp3CbrUnverified,
    Mp3VariableBitrate,
    Mp3BitrateOutOfRange,
    Mp3SampleRateUnexpected,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum QcSeverity {
    Info,
    Warning,
    Error,
}

/// A typed value used for both measured and expected QC evidence.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(tag = "kind", content = "value", rename_all = "snake_case")]
pub enum QcValue {
    Boolean(bool),
    Count(u64),
    Ratio(f64),
    Decibels(f64),
    Milliseconds(u64),
    Hertz(u32),
    BitsPerSecond(u64),
    Text(String),
    TextList(Vec<String>),
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum QcExpectation {
    Exact {
        value: QcValue,
    },
    Range {
        min: Option<QcValue>,
        max: Option<QcValue>,
        inclusive: bool,
    },
    OneOf {
        values: Vec<QcValue>,
    },
    Description {
        value: String,
    },
}

/// A deterministic QC finding with optional evidence and media timestamps.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct QcFinding {
    pub code: QcFindingCode,
    pub severity: QcSeverity,
    pub message: String,
    pub actual: Option<QcValue>,
    pub expected: Option<QcExpectation>,
    pub start_milliseconds: Option<u64>,
    pub end_milliseconds: Option<u64>,
}

impl QcFinding {
    pub(crate) fn new(
        code: QcFindingCode,
        severity: QcSeverity,
        message: impl Into<String>,
    ) -> Self {
        Self {
            code,
            severity,
            message: message.into(),
            actual: None,
            expected: None,
            start_milliseconds: None,
            end_milliseconds: None,
        }
    }

    pub(crate) fn evidence(mut self, actual: QcValue, expected: QcExpectation) -> Self {
        self.actual = Some(actual);
        self.expected = Some(expected);
        self
    }

    pub(crate) fn at(mut self, start_milliseconds: u64, end_milliseconds: u64) -> Self {
        self.start_milliseconds = Some(start_milliseconds);
        self.end_milliseconds = Some(end_milliseconds);
        self
    }
}

#[derive(Clone, Copy, Debug, Default, Deserialize, PartialEq, Serialize)]
pub struct QcRangeF64 {
    pub min: Option<f64>,
    pub max: Option<f64>,
}

impl QcRangeF64 {
    #[must_use]
    pub fn contains(self, value: f64) -> bool {
        value.is_finite()
            && self.min.is_none_or(|minimum| value >= minimum)
            && self.max.is_none_or(|maximum| value <= maximum)
    }

    pub(crate) fn validate(self, name: &str) -> crate::Result<()> {
        if self.min.is_some_and(|value| !value.is_finite())
            || self.max.is_some_and(|value| !value.is_finite())
            || matches!((self.min, self.max), (Some(min), Some(max)) if min > max)
        {
            return Err(crate::MediaError::Configuration(format!(
                "{name} must use finite ordered bounds"
            )));
        }
        Ok(())
    }
}

#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
pub struct QcRangeU64 {
    pub min: Option<u64>,
    pub max: Option<u64>,
}

impl QcRangeU64 {
    #[must_use]
    pub fn contains(self, value: u64) -> bool {
        self.min.is_none_or(|minimum| value >= minimum)
            && self.max.is_none_or(|maximum| value <= maximum)
    }

    pub(crate) fn validate(self, name: &str) -> crate::Result<()> {
        if matches!((self.min, self.max), (Some(min), Some(max)) if min > max) {
            return Err(crate::MediaError::Configuration(format!(
                "{name} must use ordered bounds"
            )));
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn finding_codes_have_stable_serialized_names() {
        assert_eq!(
            serde_json::to_string(&QcFindingCode::PcmAbruptJoin).unwrap(),
            "\"pcm_abrupt_join\""
        );
        assert_eq!(
            serde_json::to_string(&QcFindingCode::Mp3VariableBitrate).unwrap(),
            "\"mp3_variable_bitrate\""
        );
    }
}
