use serde::{Deserialize, Serialize};

use super::{QcExpectation, QcFinding, QcFindingCode, QcRangeF64, QcRangeU64, QcSeverity, QcValue};
use crate::{MediaError, Result};

const TRUE_PEAK_HALF_TAPS: isize = 8;

#[derive(Clone, Copy, Debug)]
pub struct DecodedPcm<'a> {
    /// Interleaved, normalized floating-point samples. Nominal full scale is `[-1.0, 1.0]`.
    pub samples: &'a [f32],
    pub sample_rate_hz: u32,
    pub channels: u16,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct PcmJoin {
    /// Frame index immediately after the join.
    pub frame_index: u64,
    pub label: Option<String>,
}

#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Serialize)]
pub struct PcmQcPolicy {
    pub silence_threshold_dbfs: f64,
    pub long_silence_min_milliseconds: u64,
    pub clipping_threshold: f64,
    pub true_peak_oversample_factor: u8,
    pub rms_dbfs: Option<QcRangeF64>,
    pub max_sample_peak_dbfs: Option<f64>,
    pub max_estimated_true_peak_dbfs: Option<f64>,
    pub leading_silence_milliseconds: Option<QcRangeU64>,
    pub trailing_silence_milliseconds: Option<QcRangeU64>,
    pub abrupt_join_min_delta: f64,
    pub join_window_milliseconds: u64,
}

impl Default for PcmQcPolicy {
    fn default() -> Self {
        Self {
            silence_threshold_dbfs: -60.0,
            long_silence_min_milliseconds: 2_000,
            // The positive full-scale value of signed 16-bit PCM is 32767 / 32768.
            clipping_threshold: 32_767.0 / 32_768.0,
            true_peak_oversample_factor: 4,
            rms_dbfs: None,
            max_sample_peak_dbfs: None,
            max_estimated_true_peak_dbfs: None,
            leading_silence_milliseconds: None,
            trailing_silence_milliseconds: None,
            abrupt_join_min_delta: 0.25,
            join_window_milliseconds: 5,
        }
    }
}

impl PcmQcPolicy {
    fn validate(self) -> Result<()> {
        if !self.silence_threshold_dbfs.is_finite()
            || !(-160.0..=0.0).contains(&self.silence_threshold_dbfs)
        {
            return Err(MediaError::Configuration(
                "silence threshold must be between -160 and 0 dBFS".to_owned(),
            ));
        }
        if !self.clipping_threshold.is_finite() || !(0.0..=1.0).contains(&self.clipping_threshold) {
            return Err(MediaError::Configuration(
                "clipping threshold must be between zero and one".to_owned(),
            ));
        }
        if !(1..=8).contains(&self.true_peak_oversample_factor) {
            return Err(MediaError::Configuration(
                "true-peak oversampling must be between one and eight".to_owned(),
            ));
        }
        if !self.abrupt_join_min_delta.is_finite()
            || !(0.0..=2.0).contains(&self.abrupt_join_min_delta)
        {
            return Err(MediaError::Configuration(
                "abrupt-join delta must be between zero and two".to_owned(),
            ));
        }
        self.rms_dbfs
            .map_or(Ok(()), |range| range.validate("RMS range"))?;
        self.leading_silence_milliseconds
            .map_or(Ok(()), |range| range.validate("leading-silence range"))?;
        self.trailing_silence_milliseconds
            .map_or(Ok(()), |range| range.validate("trailing-silence range"))?;
        for (name, value) in [
            ("sample-peak ceiling", self.max_sample_peak_dbfs),
            (
                "estimated true-peak ceiling",
                self.max_estimated_true_peak_dbfs,
            ),
        ] {
            if value.is_some_and(|value| !value.is_finite() || value > 0.0) {
                return Err(MediaError::Configuration(format!(
                    "{name} must be finite and no greater than 0 dBFS"
                )));
            }
        }
        Ok(())
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum SilenceRegionKind {
    Leading,
    Trailing,
    Internal,
    EntireFile,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct SilenceRegion {
    pub kind: SilenceRegionKind,
    pub start_milliseconds: u64,
    pub end_milliseconds: u64,
    pub duration_milliseconds: u64,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct PcmJoinMeasurement {
    pub frame_index: u64,
    pub timestamp_milliseconds: u64,
    pub label: Option<String>,
    pub discontinuity: f64,
    pub before_rms_dbfs: Option<f64>,
    pub after_rms_dbfs: Option<f64>,
    pub flagged: bool,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct PcmQcMetrics {
    pub sample_count: u64,
    pub frame_count: u64,
    pub duration_milliseconds: u64,
    /// `None` represents digital silence (`-inf dBFS`).
    pub rms_dbfs: Option<f64>,
    /// `None` represents digital silence (`-inf dBFS`).
    pub sample_peak_dbfs: Option<f64>,
    /// Four-times oversampled by default. This is an estimate, not a certified BS.1770 meter.
    pub estimated_true_peak_dbfs: Option<f64>,
    pub clipping_sample_count: u64,
    pub non_finite_sample_count: u64,
    pub leading_silence_milliseconds: u64,
    pub trailing_silence_milliseconds: u64,
    pub silence_regions: Vec<SilenceRegion>,
    pub joins: Vec<PcmJoinMeasurement>,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct PcmQcAnalysis {
    pub metrics: PcmQcMetrics,
    pub findings: Vec<QcFinding>,
}

/// Incremental PCM analyzer for long media files.
///
/// Chunks must contain complete interleaved frames. Streaming analysis intentionally supports a
/// true-peak oversampling factor of one: sinc reconstruction requires neighboring samples and is
/// available through [`analyze_pcm`] for callers that can retain the complete signal.
#[derive(Debug)]
pub struct StreamingPcmQcAnalyzer {
    sample_rate_hz: u32,
    channels: usize,
    policy: PcmQcPolicy,
    silence_amplitude: f64,
    sample_count: u64,
    frame_count: u64,
    finite_sample_count: u64,
    finite_sample_count_f64: f64,
    non_finite_sample_count: u64,
    clipping_sample_count: u64,
    sum_squares: f64,
    sample_peak: f64,
    first_non_finite_frame: Option<u64>,
    last_non_finite_frame: Option<u64>,
    first_clipping_frame: Option<u64>,
    last_clipping_frame: Option<u64>,
    silence_start: Option<u64>,
    raw_silence_regions: Vec<(u64, u64)>,
}

impl StreamingPcmQcAnalyzer {
    /// Creates an analyzer for frame-aligned chunks.
    pub fn new(sample_rate_hz: u32, channels: u16, policy: PcmQcPolicy) -> Result<Self> {
        policy.validate()?;
        if sample_rate_hz == 0 || channels == 0 {
            return Err(MediaError::Configuration(
                "streaming PCM requires a positive sample rate and channel count".to_owned(),
            ));
        }
        if policy.true_peak_oversample_factor != 1 {
            return Err(MediaError::Configuration(
                "streaming PCM analysis requires a true-peak oversampling factor of one".to_owned(),
            ));
        }
        Ok(Self {
            sample_rate_hz,
            channels: usize::from(channels),
            policy,
            silence_amplitude: 10_f64.powf(policy.silence_threshold_dbfs / 20.0),
            sample_count: 0,
            frame_count: 0,
            finite_sample_count: 0,
            finite_sample_count_f64: 0.0,
            non_finite_sample_count: 0,
            clipping_sample_count: 0,
            sum_squares: 0.0,
            sample_peak: 0.0,
            first_non_finite_frame: None,
            last_non_finite_frame: None,
            first_clipping_frame: None,
            last_clipping_frame: None,
            silence_start: None,
            raw_silence_regions: Vec::new(),
        })
    }

    /// Adds one or more complete interleaved frames.
    pub fn push_samples(&mut self, samples: &[f32]) -> Result<()> {
        if !samples.len().is_multiple_of(self.channels) {
            return Err(MediaError::Configuration(
                "streaming PCM chunks must contain complete interleaved frames".to_owned(),
            ));
        }
        self.sample_count = self
            .sample_count
            .saturating_add(u64::try_from(samples.len()).unwrap_or(u64::MAX));
        for frame_samples in samples.chunks_exact(self.channels) {
            let frame_index = self.frame_count;
            let mut silent = true;
            for sample in frame_samples {
                if !sample.is_finite() {
                    self.non_finite_sample_count = self.non_finite_sample_count.saturating_add(1);
                    self.first_non_finite_frame.get_or_insert(frame_index);
                    self.last_non_finite_frame = Some(frame_index);
                    silent = false;
                    continue;
                }
                let amplitude = f64::from(sample.abs());
                self.finite_sample_count = self.finite_sample_count.saturating_add(1);
                self.finite_sample_count_f64 += 1.0;
                self.sum_squares += amplitude * amplitude;
                self.sample_peak = self.sample_peak.max(amplitude);
                if amplitude >= self.policy.clipping_threshold {
                    self.clipping_sample_count = self.clipping_sample_count.saturating_add(1);
                    self.first_clipping_frame.get_or_insert(frame_index);
                    self.last_clipping_frame = Some(frame_index);
                }
                if amplitude > self.silence_amplitude {
                    silent = false;
                }
            }
            match (silent, self.silence_start) {
                (true, None) => self.silence_start = Some(frame_index),
                (false, Some(start)) => {
                    self.raw_silence_regions.push((start, frame_index));
                    self.silence_start = None;
                }
                _ => {}
            }
            self.frame_count = self.frame_count.saturating_add(1);
        }
        Ok(())
    }

    /// Completes analysis and evaluates the configured policy.
    #[must_use]
    pub fn finish(mut self) -> PcmQcAnalysis {
        if let Some(start) = self.silence_start {
            self.raw_silence_regions.push((start, self.frame_count));
        }
        let rms = (self.finite_sample_count > 0)
            .then(|| (self.sum_squares / self.finite_sample_count_f64).sqrt());
        let rms_dbfs = rms.and_then(amplitude_to_dbfs);
        let sample_peak_dbfs = amplitude_to_dbfs(self.sample_peak);
        let estimated_true_peak_dbfs = sample_peak_dbfs;
        let silence_regions = self
            .raw_silence_regions
            .iter()
            .map(|&(start, end)| {
                let kind = if start == 0 && end == self.frame_count {
                    SilenceRegionKind::EntireFile
                } else if start == 0 {
                    SilenceRegionKind::Leading
                } else if end == self.frame_count {
                    SilenceRegionKind::Trailing
                } else {
                    SilenceRegionKind::Internal
                };
                let start_milliseconds = frame_u64_to_milliseconds(start, self.sample_rate_hz);
                let end_milliseconds = frame_u64_to_milliseconds(end, self.sample_rate_hz);
                SilenceRegion {
                    kind,
                    start_milliseconds,
                    end_milliseconds,
                    duration_milliseconds: end_milliseconds.saturating_sub(start_milliseconds),
                }
            })
            .collect::<Vec<_>>();
        let leading_silence_milliseconds = silence_regions
            .iter()
            .find(|region| {
                matches!(
                    region.kind,
                    SilenceRegionKind::Leading | SilenceRegionKind::EntireFile
                )
            })
            .map_or(0, |region| region.duration_milliseconds);
        let trailing_silence_milliseconds = silence_regions
            .iter()
            .find(|region| {
                matches!(
                    region.kind,
                    SilenceRegionKind::Trailing | SilenceRegionKind::EntireFile
                )
            })
            .map_or(0, |region| region.duration_milliseconds);
        let findings = streaming_findings(
            &self,
            rms_dbfs,
            sample_peak_dbfs,
            estimated_true_peak_dbfs,
            leading_silence_milliseconds,
            trailing_silence_milliseconds,
            &silence_regions,
        );
        PcmQcAnalysis {
            metrics: PcmQcMetrics {
                sample_count: self.sample_count,
                frame_count: self.frame_count,
                duration_milliseconds: frame_u64_to_milliseconds(
                    self.frame_count,
                    self.sample_rate_hz,
                ),
                rms_dbfs,
                sample_peak_dbfs,
                estimated_true_peak_dbfs,
                clipping_sample_count: self.clipping_sample_count,
                non_finite_sample_count: self.non_finite_sample_count,
                leading_silence_milliseconds,
                trailing_silence_milliseconds,
                silence_regions,
                joins: Vec::new(),
            },
            findings,
        }
    }
}

/// Decodes little-endian `f32` PCM bytes without changing sample values.
pub fn decode_f32le(bytes: &[u8]) -> Result<Vec<f32>> {
    if !bytes.len().is_multiple_of(4) {
        return Err(MediaError::Configuration(
            "f32le PCM byte length must be divisible by four".to_owned(),
        ));
    }
    Ok(bytes
        .chunks_exact(4)
        .map(|chunk| f32::from_le_bytes([chunk[0], chunk[1], chunk[2], chunk[3]]))
        .collect())
}

/// Decodes little-endian signed 16-bit PCM into normalized floating-point samples.
pub fn decode_s16le(bytes: &[u8]) -> Result<Vec<f32>> {
    if !bytes.len().is_multiple_of(2) {
        return Err(MediaError::Configuration(
            "s16le PCM byte length must be divisible by two".to_owned(),
        ));
    }
    Ok(bytes
        .chunks_exact(2)
        .map(|chunk| f32::from(i16::from_le_bytes([chunk[0], chunk[1]])) / 32_768.0)
        .collect())
}

/// Analyzes interleaved PCM and evaluates optional numeric policy bounds.
///
/// Abrupt joins are checked only at caller-provided frame boundaries. Scanning every sample for a
/// "join" would incorrectly classify ordinary high-frequency speech as an edit discontinuity.
#[allow(clippy::too_many_lines)]
pub fn analyze_pcm(
    pcm: DecodedPcm<'_>,
    policy: &PcmQcPolicy,
    joins: &[PcmJoin],
) -> Result<PcmQcAnalysis> {
    policy.validate()?;
    if pcm.sample_rate_hz == 0 || pcm.channels == 0 {
        return Err(MediaError::Configuration(
            "decoded PCM requires a positive sample rate and channel count".to_owned(),
        ));
    }
    let channels = usize::from(pcm.channels);
    if !pcm.samples.len().is_multiple_of(channels) {
        return Err(MediaError::Configuration(
            "interleaved PCM sample count must be divisible by the channel count".to_owned(),
        ));
    }
    let frames = pcm.samples.len() / channels;
    for join in joins {
        let frame = usize::try_from(join.frame_index).map_err(|_| {
            MediaError::Configuration("PCM join frame does not fit this platform".to_owned())
        })?;
        if frame == 0 || frame >= frames {
            return Err(MediaError::Configuration(
                "PCM join frames must fall between decoded frames".to_owned(),
            ));
        }
    }

    let silence_amplitude = 10_f64.powf(policy.silence_threshold_dbfs / 20.0);
    let mut sum_squares = 0.0_f64;
    let mut finite_sample_count_f64 = 0.0_f64;
    let mut finite_sample_count = 0_u64;
    let mut non_finite_sample_count = 0_u64;
    let mut clipping_sample_count = 0_u64;
    let mut sample_peak = 0.0_f64;
    let mut first_non_finite_frame = None;
    let mut last_non_finite_frame = None;
    let mut first_clipping_frame = None;
    let mut last_clipping_frame = None;
    let mut raw_silence_regions = Vec::<(usize, usize)>::new();
    let mut silence_start = None;

    for frame_index in 0..frames {
        let start = frame_index * channels;
        let frame_samples = &pcm.samples[start..start + channels];
        let mut silent = true;
        for sample in frame_samples {
            if !sample.is_finite() {
                non_finite_sample_count = non_finite_sample_count.saturating_add(1);
                first_non_finite_frame.get_or_insert(frame_index);
                last_non_finite_frame = Some(frame_index);
                silent = false;
                continue;
            }
            let amplitude = f64::from(sample.abs());
            finite_sample_count = finite_sample_count.saturating_add(1);
            finite_sample_count_f64 += 1.0;
            sum_squares += amplitude * amplitude;
            sample_peak = sample_peak.max(amplitude);
            if amplitude >= policy.clipping_threshold {
                clipping_sample_count = clipping_sample_count.saturating_add(1);
                first_clipping_frame.get_or_insert(frame_index);
                last_clipping_frame = Some(frame_index);
            }
            if amplitude > silence_amplitude {
                silent = false;
            }
        }
        match (silent, silence_start) {
            (true, None) => silence_start = Some(frame_index),
            (false, Some(start)) => {
                raw_silence_regions.push((start, frame_index));
                silence_start = None;
            }
            _ => {}
        }
    }
    if let Some(start) = silence_start {
        raw_silence_regions.push((start, frames));
    }

    let rms = (finite_sample_count > 0).then(|| (sum_squares / finite_sample_count_f64).sqrt());
    let rms_dbfs = rms.and_then(amplitude_to_dbfs);
    let sample_peak_dbfs = amplitude_to_dbfs(sample_peak);
    let estimated_true_peak = estimate_true_peak(
        pcm.samples,
        frames,
        channels,
        policy.true_peak_oversample_factor,
    );
    let estimated_true_peak_dbfs = estimated_true_peak.and_then(amplitude_to_dbfs);

    let silence_regions = raw_silence_regions
        .iter()
        .map(|&(start, end)| {
            let kind = if start == 0 && end == frames {
                SilenceRegionKind::EntireFile
            } else if start == 0 {
                SilenceRegionKind::Leading
            } else if end == frames {
                SilenceRegionKind::Trailing
            } else {
                SilenceRegionKind::Internal
            };
            let start_milliseconds = frames_to_milliseconds(start, pcm.sample_rate_hz);
            let end_milliseconds = frames_to_milliseconds(end, pcm.sample_rate_hz);
            SilenceRegion {
                kind,
                start_milliseconds,
                end_milliseconds,
                duration_milliseconds: end_milliseconds.saturating_sub(start_milliseconds),
            }
        })
        .collect::<Vec<_>>();
    let leading_silence_milliseconds = silence_regions
        .iter()
        .find(|region| {
            matches!(
                region.kind,
                SilenceRegionKind::Leading | SilenceRegionKind::EntireFile
            )
        })
        .map_or(0, |region| region.duration_milliseconds);
    let trailing_silence_milliseconds = silence_regions
        .iter()
        .find(|region| {
            matches!(
                region.kind,
                SilenceRegionKind::Trailing | SilenceRegionKind::EntireFile
            )
        })
        .map_or(0, |region| region.duration_milliseconds);
    let join_measurements = analyze_joins(pcm, policy, joins, silence_amplitude)?;

    let mut findings = Vec::new();
    if frames == 0 {
        findings.push(QcFinding::new(
            QcFindingCode::PcmEmpty,
            QcSeverity::Error,
            "decoded audio contains no PCM frames",
        ));
    }
    if non_finite_sample_count > 0 {
        let start = frames_to_milliseconds(
            first_non_finite_frame.unwrap_or_default(),
            pcm.sample_rate_hz,
        );
        let end = frames_to_milliseconds(
            last_non_finite_frame.unwrap_or_default().saturating_add(1),
            pcm.sample_rate_hz,
        );
        findings.push(
            QcFinding::new(
                QcFindingCode::PcmNonFinite,
                QcSeverity::Error,
                "decoded audio contains non-finite PCM samples",
            )
            .evidence(
                QcValue::Count(non_finite_sample_count),
                QcExpectation::Exact {
                    value: QcValue::Count(0),
                },
            )
            .at(start, end),
        );
    }
    if frames > 0 && rms_dbfs.is_none() {
        findings.push(
            QcFinding::new(
                QcFindingCode::PcmSilent,
                QcSeverity::Error,
                "decoded audio is digitally silent",
            )
            .evidence(
                QcValue::Boolean(true),
                QcExpectation::Exact {
                    value: QcValue::Boolean(false),
                },
            )
            .at(0, frames_to_milliseconds(frames, pcm.sample_rate_hz)),
        );
    }
    if clipping_sample_count > 0 {
        let start =
            frames_to_milliseconds(first_clipping_frame.unwrap_or_default(), pcm.sample_rate_hz);
        let end = frames_to_milliseconds(
            last_clipping_frame.unwrap_or_default().saturating_add(1),
            pcm.sample_rate_hz,
        );
        findings.push(
            QcFinding::new(
                QcFindingCode::PcmClipping,
                QcSeverity::Error,
                "decoded audio reaches the configured clipping threshold",
            )
            .evidence(
                QcValue::Count(clipping_sample_count),
                QcExpectation::Exact {
                    value: QcValue::Count(0),
                },
            )
            .at(start, end),
        );
    }
    if let Some(expected) = policy.rms_dbfs
        && rms_dbfs.is_none_or(|actual| !expected.contains(actual))
    {
        findings.push(
            QcFinding::new(
                QcFindingCode::PcmRmsOutOfRange,
                QcSeverity::Error,
                "PCM RMS is outside the configured range",
            )
            .evidence(
                rms_dbfs.map_or_else(|| QcValue::Text("-inf dBFS".to_owned()), QcValue::Decibels),
                db_range_expectation(expected),
            ),
        );
    }
    if let (Some(actual), Some(maximum)) = (sample_peak_dbfs, policy.max_sample_peak_dbfs)
        && actual > maximum
    {
        findings.push(
            QcFinding::new(
                QcFindingCode::PcmSamplePeakExceeded,
                QcSeverity::Error,
                "PCM sample peak exceeds the configured ceiling",
            )
            .evidence(
                QcValue::Decibels(actual),
                QcExpectation::Range {
                    min: None,
                    max: Some(QcValue::Decibels(maximum)),
                    inclusive: true,
                },
            ),
        );
    }
    if let (Some(actual), Some(maximum)) = (
        estimated_true_peak_dbfs,
        policy.max_estimated_true_peak_dbfs,
    ) && actual > maximum
    {
        findings.push(
            QcFinding::new(
                QcFindingCode::PcmTruePeakExceeded,
                QcSeverity::Error,
                "estimated PCM true peak exceeds the configured ceiling",
            )
            .evidence(
                QcValue::Decibels(actual),
                QcExpectation::Range {
                    min: None,
                    max: Some(QcValue::Decibels(maximum)),
                    inclusive: true,
                },
            ),
        );
    }
    append_silence_policy_findings(
        &mut findings,
        policy,
        leading_silence_milliseconds,
        trailing_silence_milliseconds,
        &silence_regions,
    );
    findings.extend(
        join_measurements
            .iter()
            .filter(|join| join.flagged)
            .map(|join| {
                let label = join
                    .label
                    .as_deref()
                    .map_or_else(String::new, |label| format!(" at {label}"));
                QcFinding::new(
                    QcFindingCode::PcmAbruptJoin,
                    QcSeverity::Warning,
                    format!("possible abrupt edit discontinuity{label}"),
                )
                .evidence(
                    QcValue::Ratio(join.discontinuity),
                    QcExpectation::Range {
                        min: None,
                        max: Some(QcValue::Ratio(policy.abrupt_join_min_delta)),
                        inclusive: false,
                    },
                )
                .at(join.timestamp_milliseconds, join.timestamp_milliseconds)
            }),
    );

    Ok(PcmQcAnalysis {
        metrics: PcmQcMetrics {
            sample_count: u64::try_from(pcm.samples.len()).unwrap_or(u64::MAX),
            frame_count: u64::try_from(frames).unwrap_or(u64::MAX),
            duration_milliseconds: frames_to_milliseconds(frames, pcm.sample_rate_hz),
            rms_dbfs,
            sample_peak_dbfs,
            estimated_true_peak_dbfs,
            clipping_sample_count,
            non_finite_sample_count,
            leading_silence_milliseconds,
            trailing_silence_milliseconds,
            silence_regions,
            joins: join_measurements,
        },
        findings,
    })
}

#[allow(clippy::too_many_arguments, clippy::too_many_lines)]
fn streaming_findings(
    analyzer: &StreamingPcmQcAnalyzer,
    rms_dbfs: Option<f64>,
    sample_peak_dbfs: Option<f64>,
    estimated_true_peak_dbfs: Option<f64>,
    leading_silence_milliseconds: u64,
    trailing_silence_milliseconds: u64,
    silence_regions: &[SilenceRegion],
) -> Vec<QcFinding> {
    let mut findings = Vec::new();
    if analyzer.frame_count == 0 {
        findings.push(QcFinding::new(
            QcFindingCode::PcmEmpty,
            QcSeverity::Error,
            "decoded audio contains no PCM frames",
        ));
    }
    if analyzer.non_finite_sample_count > 0 {
        let start = frame_u64_to_milliseconds(
            analyzer.first_non_finite_frame.unwrap_or_default(),
            analyzer.sample_rate_hz,
        );
        let end = frame_u64_to_milliseconds(
            analyzer
                .last_non_finite_frame
                .unwrap_or_default()
                .saturating_add(1),
            analyzer.sample_rate_hz,
        );
        findings.push(
            QcFinding::new(
                QcFindingCode::PcmNonFinite,
                QcSeverity::Error,
                "decoded audio contains non-finite PCM samples",
            )
            .evidence(
                QcValue::Count(analyzer.non_finite_sample_count),
                QcExpectation::Exact {
                    value: QcValue::Count(0),
                },
            )
            .at(start, end),
        );
    }
    if analyzer.frame_count > 0 && rms_dbfs.is_none() {
        findings.push(
            QcFinding::new(
                QcFindingCode::PcmSilent,
                QcSeverity::Error,
                "decoded audio is digitally silent",
            )
            .evidence(
                QcValue::Boolean(true),
                QcExpectation::Exact {
                    value: QcValue::Boolean(false),
                },
            )
            .at(
                0,
                frame_u64_to_milliseconds(analyzer.frame_count, analyzer.sample_rate_hz),
            ),
        );
    }
    if analyzer.clipping_sample_count > 0 {
        let start = frame_u64_to_milliseconds(
            analyzer.first_clipping_frame.unwrap_or_default(),
            analyzer.sample_rate_hz,
        );
        let end = frame_u64_to_milliseconds(
            analyzer
                .last_clipping_frame
                .unwrap_or_default()
                .saturating_add(1),
            analyzer.sample_rate_hz,
        );
        findings.push(
            QcFinding::new(
                QcFindingCode::PcmClipping,
                QcSeverity::Error,
                "decoded audio reaches the configured clipping threshold",
            )
            .evidence(
                QcValue::Count(analyzer.clipping_sample_count),
                QcExpectation::Exact {
                    value: QcValue::Count(0),
                },
            )
            .at(start, end),
        );
    }
    if let Some(expected) = analyzer.policy.rms_dbfs
        && rms_dbfs.is_none_or(|actual| !expected.contains(actual))
    {
        findings.push(
            QcFinding::new(
                QcFindingCode::PcmRmsOutOfRange,
                QcSeverity::Error,
                "PCM RMS is outside the configured range",
            )
            .evidence(
                rms_dbfs.map_or_else(|| QcValue::Text("-inf dBFS".to_owned()), QcValue::Decibels),
                db_range_expectation(expected),
            ),
        );
    }
    if let (Some(actual), Some(maximum)) = (sample_peak_dbfs, analyzer.policy.max_sample_peak_dbfs)
        && actual > maximum
    {
        findings.push(
            QcFinding::new(
                QcFindingCode::PcmSamplePeakExceeded,
                QcSeverity::Error,
                "PCM sample peak exceeds the configured ceiling",
            )
            .evidence(
                QcValue::Decibels(actual),
                QcExpectation::Range {
                    min: None,
                    max: Some(QcValue::Decibels(maximum)),
                    inclusive: true,
                },
            ),
        );
    }
    if let (Some(actual), Some(maximum)) = (
        estimated_true_peak_dbfs,
        analyzer.policy.max_estimated_true_peak_dbfs,
    ) && actual > maximum
    {
        findings.push(
            QcFinding::new(
                QcFindingCode::PcmTruePeakExceeded,
                QcSeverity::Error,
                "estimated PCM true peak exceeds the configured ceiling",
            )
            .evidence(
                QcValue::Decibels(actual),
                QcExpectation::Range {
                    min: None,
                    max: Some(QcValue::Decibels(maximum)),
                    inclusive: true,
                },
            ),
        );
    }
    append_silence_policy_findings(
        &mut findings,
        &analyzer.policy,
        leading_silence_milliseconds,
        trailing_silence_milliseconds,
        silence_regions,
    );
    findings
}

fn append_silence_policy_findings(
    findings: &mut Vec<QcFinding>,
    policy: &PcmQcPolicy,
    leading: u64,
    trailing: u64,
    regions: &[SilenceRegion],
) {
    for (code, label, actual, expected) in [
        (
            QcFindingCode::PcmLeadingSilenceOutOfRange,
            "leading silence",
            leading,
            policy.leading_silence_milliseconds,
        ),
        (
            QcFindingCode::PcmTrailingSilenceOutOfRange,
            "trailing silence",
            trailing,
            policy.trailing_silence_milliseconds,
        ),
    ] {
        if let Some(expected) = expected
            && !expected.contains(actual)
        {
            findings.push(
                QcFinding::new(
                    code,
                    QcSeverity::Error,
                    format!("{label} is outside the configured range"),
                )
                .evidence(
                    QcValue::Milliseconds(actual),
                    milliseconds_range_expectation(expected),
                ),
            );
        }
    }
    findings.extend(
        regions
            .iter()
            .filter(|region| {
                region.kind == SilenceRegionKind::Internal
                    && region.duration_milliseconds >= policy.long_silence_min_milliseconds
            })
            .map(|region| {
                QcFinding::new(
                    QcFindingCode::PcmLongSilence,
                    QcSeverity::Warning,
                    "long internal silence may require listening review",
                )
                .evidence(
                    QcValue::Milliseconds(region.duration_milliseconds),
                    QcExpectation::Range {
                        min: None,
                        max: Some(QcValue::Milliseconds(policy.long_silence_min_milliseconds)),
                        inclusive: false,
                    },
                )
                .at(region.start_milliseconds, region.end_milliseconds)
            }),
    );
}

fn analyze_joins(
    pcm: DecodedPcm<'_>,
    policy: &PcmQcPolicy,
    joins: &[PcmJoin],
    silence_amplitude: f64,
) -> Result<Vec<PcmJoinMeasurement>> {
    let channels = usize::from(pcm.channels);
    let frames = pcm.samples.len() / channels;
    let window_frames_u128 = u128::from(policy.join_window_milliseconds)
        .saturating_mul(u128::from(pcm.sample_rate_hz))
        .div_ceil(1_000);
    let window_frames = usize::try_from(window_frames_u128.max(1)).unwrap_or(usize::MAX);
    joins
        .iter()
        .map(|join| {
            let frame = usize::try_from(join.frame_index).map_err(|_| {
                MediaError::Configuration("PCM join frame does not fit this platform".to_owned())
            })?;
            let mut discontinuity = 0.0_f64;
            for channel in 0..channels {
                let before = pcm.samples[(frame - 1) * channels + channel];
                let after = pcm.samples[frame * channels + channel];
                if before.is_finite() && after.is_finite() {
                    discontinuity = discontinuity.max(f64::from((after - before).abs()));
                }
            }
            let before_start = frame.saturating_sub(window_frames);
            let after_end = frame.saturating_add(window_frames).min(frames);
            let before_rms = rms_for_frames(pcm.samples, channels, before_start, frame);
            let after_rms = rms_for_frames(pcm.samples, channels, frame, after_end);
            let energized = before_rms.is_some_and(|value| value > silence_amplitude)
                || after_rms.is_some_and(|value| value > silence_amplitude);
            Ok(PcmJoinMeasurement {
                frame_index: join.frame_index,
                timestamp_milliseconds: frames_to_milliseconds(frame, pcm.sample_rate_hz),
                label: join.label.clone(),
                discontinuity,
                before_rms_dbfs: before_rms.and_then(amplitude_to_dbfs),
                after_rms_dbfs: after_rms.and_then(amplitude_to_dbfs),
                flagged: energized && discontinuity >= policy.abrupt_join_min_delta,
            })
        })
        .collect()
}

fn rms_for_frames(
    samples: &[f32],
    channels: usize,
    start_frame: usize,
    end_frame: usize,
) -> Option<f64> {
    let mut sum = 0.0_f64;
    let mut count = 0_u64;
    let mut count_f64 = 0.0_f64;
    for sample in &samples[start_frame * channels..end_frame * channels] {
        if sample.is_finite() {
            let sample = f64::from(*sample);
            sum += sample * sample;
            count = count.saturating_add(1);
            count_f64 += 1.0;
        }
    }
    (count > 0).then(|| (sum / count_f64).sqrt())
}

/// Estimates inter-sample peaks with a four-times, windowed-sinc reconstruction by default.
/// This deliberately carries an "estimated" label because certified true-peak meters require a
/// standardized and independently validated filter implementation.
#[allow(clippy::cast_precision_loss)]
fn estimate_true_peak(samples: &[f32], frames: usize, channels: usize, factor: u8) -> Option<f64> {
    let mut peak = samples
        .iter()
        .filter(|sample| sample.is_finite())
        .map(|sample| f64::from(sample.abs()))
        .fold(0.0_f64, f64::max);
    if peak == 0.0 {
        return None;
    }
    if factor == 1 || frames < 2 {
        return Some(peak);
    }
    let factor = usize::from(factor);
    for channel in 0..channels {
        for frame in 0..frames - 1 {
            let Ok(base) = isize::try_from(frame) else {
                return Some(peak);
            };
            for phase in 1..factor {
                let position = base as f64 + phase as f64 / factor as f64;
                let mut reconstructed = 0.0_f64;
                let mut weight_sum = 0.0_f64;
                for source_frame in base - TRUE_PEAK_HALF_TAPS + 1..=base + TRUE_PEAK_HALF_TAPS {
                    let Ok(source_index) = usize::try_from(source_frame) else {
                        continue;
                    };
                    if source_index >= frames {
                        continue;
                    }
                    let distance = position - source_frame as f64;
                    let normalized_distance = distance / TRUE_PEAK_HALF_TAPS as f64;
                    if normalized_distance.abs() >= 1.0 {
                        continue;
                    }
                    let sinc = if distance.abs() < f64::EPSILON {
                        1.0
                    } else {
                        (std::f64::consts::PI * distance).sin() / (std::f64::consts::PI * distance)
                    };
                    let window = 0.5 * (1.0 + (std::f64::consts::PI * normalized_distance).cos());
                    let weight = sinc * window;
                    let sample = samples[source_index * channels + channel];
                    if sample.is_finite() {
                        reconstructed += f64::from(sample) * weight;
                        weight_sum += weight;
                    }
                }
                if weight_sum.abs() > f64::EPSILON {
                    peak = peak.max((reconstructed / weight_sum).abs());
                }
            }
        }
    }
    Some(peak)
}

fn amplitude_to_dbfs(amplitude: f64) -> Option<f64> {
    (amplitude.is_finite() && amplitude > 0.0).then(|| 20.0 * amplitude.log10())
}

fn frames_to_milliseconds(frames: usize, sample_rate_hz: u32) -> u64 {
    frame_u64_to_milliseconds(u64::try_from(frames).unwrap_or(u64::MAX), sample_rate_hz)
}

fn frame_u64_to_milliseconds(frames: u64, sample_rate_hz: u32) -> u64 {
    let value = u128::from(frames)
        .saturating_mul(1_000)
        .checked_div(u128::from(sample_rate_hz))
        .unwrap_or_default();
    u64::try_from(value).unwrap_or(u64::MAX)
}

fn db_range_expectation(range: QcRangeF64) -> QcExpectation {
    QcExpectation::Range {
        min: range.min.map(QcValue::Decibels),
        max: range.max.map(QcValue::Decibels),
        inclusive: true,
    }
}

fn milliseconds_range_expectation(range: QcRangeU64) -> QcExpectation {
    QcExpectation::Range {
        min: range.min.map(QcValue::Milliseconds),
        max: range.max.map(QcValue::Milliseconds),
        inclusive: true,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn assert_near(actual: f64, expected: f64, tolerance: f64) {
        assert!(
            (actual - expected).abs() <= tolerance,
            "expected {expected}, got {actual}"
        );
    }

    #[test]
    fn decoders_validate_alignment_and_normalize_s16() {
        assert!(decode_f32le(&[0, 1, 2]).is_err());
        assert!(decode_s16le(&[0]).is_err());
        let decoded = decode_s16le(&[
            0x00, 0x80, // -32768
            0x00, 0x00, // 0
            0xff, 0x7f, // 32767
        ])
        .unwrap();
        assert_near(f64::from(decoded[0]), -1.0, f64::EPSILON);
        assert_near(f64::from(decoded[1]), 0.0, f64::EPSILON);
        assert_near(f64::from(decoded[2]), 32_767.0 / 32_768.0, 1e-7);
    }

    #[test]
    fn measures_rms_peaks_and_classified_silence_regions() {
        // 100 ms lead, 100 ms signal, 250 ms internal silence, 100 ms signal, 150 ms tail.
        let mut samples = vec![0.0; 100];
        samples.extend(vec![0.5; 100]);
        samples.extend(vec![0.0; 250]);
        samples.extend(vec![0.5; 100]);
        samples.extend(vec![0.0; 150]);
        let analysis = analyze_pcm(
            DecodedPcm {
                samples: &samples,
                sample_rate_hz: 1_000,
                channels: 1,
            },
            &PcmQcPolicy::default(),
            &[],
        )
        .unwrap();
        assert_eq!(analysis.metrics.duration_milliseconds, 700);
        assert_eq!(analysis.metrics.leading_silence_milliseconds, 100);
        assert_eq!(analysis.metrics.trailing_silence_milliseconds, 150);
        assert_eq!(analysis.metrics.silence_regions.len(), 3);
        assert_eq!(
            analysis.metrics.silence_regions[1].kind,
            SilenceRegionKind::Internal
        );
        assert_eq!(
            analysis.metrics.silence_regions[1].duration_milliseconds,
            250
        );
        assert_near(analysis.metrics.rms_dbfs.unwrap(), -11.461, 0.01);
        assert_near(analysis.metrics.sample_peak_dbfs.unwrap(), -6.0206, 0.001);
        assert!(
            !analysis
                .findings
                .iter()
                .any(|finding| finding.code == QcFindingCode::PcmLongSilence)
        );
    }

    #[test]
    fn emits_long_silence_and_boundary_policy_findings_with_timestamps() {
        let mut samples = vec![0.0; 100];
        samples.extend(vec![0.2; 100]);
        samples.extend(vec![0.0; 250]);
        samples.extend(vec![0.2; 100]);
        samples.extend(vec![0.0; 150]);
        let policy = PcmQcPolicy {
            long_silence_min_milliseconds: 200,
            leading_silence_milliseconds: Some(QcRangeU64 {
                min: Some(500),
                max: Some(1_000),
            }),
            trailing_silence_milliseconds: Some(QcRangeU64 {
                min: Some(100),
                max: Some(120),
            }),
            ..PcmQcPolicy::default()
        };
        let analysis = analyze_pcm(
            DecodedPcm {
                samples: &samples,
                sample_rate_hz: 1_000,
                channels: 1,
            },
            &policy,
            &[],
        )
        .unwrap();
        let long = analysis
            .findings
            .iter()
            .find(|finding| finding.code == QcFindingCode::PcmLongSilence)
            .unwrap();
        assert_eq!(
            (long.start_milliseconds, long.end_milliseconds),
            (Some(200), Some(450))
        );
        assert!(
            analysis
                .findings
                .iter()
                .any(|finding| finding.code == QcFindingCode::PcmLeadingSilenceOutOfRange)
        );
        assert!(
            analysis
                .findings
                .iter()
                .any(|finding| finding.code == QcFindingCode::PcmTrailingSilenceOutOfRange)
        );
    }

    #[test]
    fn detects_non_finite_clipping_and_silence_only_audio() {
        let samples = [f32::NAN, 1.0, -1.0, 0.0];
        let analysis = analyze_pcm(
            DecodedPcm {
                samples: &samples,
                sample_rate_hz: 1_000,
                channels: 1,
            },
            &PcmQcPolicy::default(),
            &[],
        )
        .unwrap();
        assert_eq!(analysis.metrics.non_finite_sample_count, 1);
        assert_eq!(analysis.metrics.clipping_sample_count, 2);
        assert!(
            analysis
                .findings
                .iter()
                .any(|finding| finding.code == QcFindingCode::PcmNonFinite)
        );
        assert!(
            analysis
                .findings
                .iter()
                .any(|finding| finding.code == QcFindingCode::PcmClipping)
        );

        let silent = analyze_pcm(
            DecodedPcm {
                samples: &[0.0; 10],
                sample_rate_hz: 1_000,
                channels: 1,
            },
            &PcmQcPolicy::default(),
            &[],
        )
        .unwrap();
        assert!(
            silent
                .findings
                .iter()
                .any(|finding| finding.code == QcFindingCode::PcmSilent)
        );
        assert_eq!(
            silent.metrics.silence_regions[0].kind,
            SilenceRegionKind::EntireFile
        );
    }

    #[test]
    fn checks_only_declared_join_boundaries() {
        let mut samples = vec![0.5; 50];
        samples.extend(vec![-0.5; 50]);
        let analysis = analyze_pcm(
            DecodedPcm {
                samples: &samples,
                sample_rate_hz: 1_000,
                channels: 1,
            },
            &PcmQcPolicy::default(),
            &[PcmJoin {
                frame_index: 50,
                label: Some("segment 2".to_owned()),
            }],
        )
        .unwrap();
        assert_eq!(analysis.metrics.joins.len(), 1);
        assert!(analysis.metrics.joins[0].flagged);
        let finding = analysis
            .findings
            .iter()
            .find(|finding| finding.code == QcFindingCode::PcmAbruptJoin)
            .unwrap();
        assert_eq!(finding.start_milliseconds, Some(50));
        assert!(finding.message.contains("segment 2"));
    }

    #[test]
    fn estimated_true_peak_is_never_below_sample_peak() {
        let samples = [0.0, 0.8, -0.8, 0.8, -0.8, 0.0];
        let analysis = analyze_pcm(
            DecodedPcm {
                samples: &samples,
                sample_rate_hz: 48_000,
                channels: 1,
            },
            &PcmQcPolicy::default(),
            &[],
        )
        .unwrap();
        assert!(
            analysis.metrics.estimated_true_peak_dbfs.unwrap()
                >= analysis.metrics.sample_peak_dbfs.unwrap()
        );
    }

    #[test]
    fn rejects_misaligned_pcm_and_out_of_bounds_joins() {
        assert!(
            analyze_pcm(
                DecodedPcm {
                    samples: &[0.0, 0.0, 0.0],
                    sample_rate_hz: 48_000,
                    channels: 2,
                },
                &PcmQcPolicy::default(),
                &[],
            )
            .is_err()
        );
        assert!(
            analyze_pcm(
                DecodedPcm {
                    samples: &[0.0, 0.1],
                    sample_rate_hz: 48_000,
                    channels: 1,
                },
                &PcmQcPolicy::default(),
                &[PcmJoin {
                    frame_index: 2,
                    label: None,
                }],
            )
            .is_err()
        );
    }

    #[test]
    fn streaming_analysis_matches_batch_analysis_across_chunks() {
        let mut samples = vec![0.0; 100];
        samples.extend(vec![0.2; 100]);
        samples.extend(vec![0.0; 250]);
        samples.extend(vec![0.8; 100]);
        samples.extend(vec![0.0; 150]);
        let policy = PcmQcPolicy {
            long_silence_min_milliseconds: 200,
            true_peak_oversample_factor: 1,
            rms_dbfs: Some(QcRangeF64 {
                min: Some(-30.0),
                max: Some(-5.0),
            }),
            max_sample_peak_dbfs: Some(-3.0),
            leading_silence_milliseconds: Some(QcRangeU64 {
                min: Some(50),
                max: Some(120),
            }),
            trailing_silence_milliseconds: Some(QcRangeU64 {
                min: Some(100),
                max: Some(200),
            }),
            ..PcmQcPolicy::default()
        };
        let batch = analyze_pcm(
            DecodedPcm {
                samples: &samples,
                sample_rate_hz: 1_000,
                channels: 1,
            },
            &policy,
            &[],
        )
        .unwrap();
        let mut streaming = StreamingPcmQcAnalyzer::new(1_000, 1, policy).unwrap();
        for chunk in samples.chunks(137) {
            streaming.push_samples(chunk).unwrap();
        }
        let streamed = streaming.finish();

        assert_eq!(streamed.metrics, batch.metrics);
        assert_eq!(streamed.findings, batch.findings);
    }

    #[test]
    fn streaming_analysis_rejects_oversampling_and_partial_frames() {
        assert!(StreamingPcmQcAnalyzer::new(48_000, 2, PcmQcPolicy::default()).is_err());
        let policy = PcmQcPolicy {
            true_peak_oversample_factor: 1,
            ..PcmQcPolicy::default()
        };
        let mut streaming = StreamingPcmQcAnalyzer::new(48_000, 2, policy).unwrap();
        assert!(streaming.push_samples(&[0.0]).is_err());
        streaming.push_samples(&[0.0, 0.0]).unwrap();
        assert_eq!(streaming.finish().metrics.frame_count, 1);
    }
}
