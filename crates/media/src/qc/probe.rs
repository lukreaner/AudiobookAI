use std::{path::Path, time::Duration};

use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::{QcExpectation, QcFinding, QcFindingCode, QcRangeU64, QcSeverity, QcValue};
use crate::{FfmpegInvocation, MediaError, Result, SidecarPair};

#[derive(Clone, Debug)]
pub struct MediaQcPlanner {
    sidecars: SidecarPair,
}

impl MediaQcPlanner {
    #[must_use]
    pub fn new(sidecars: SidecarPair) -> Self {
        Self { sidecars }
    }

    /// Builds deterministic sidecar commands for metadata inspection and normalized PCM decode.
    /// The caller is responsible for execution and for treating a failed decode as invalid.
    #[must_use]
    pub fn plan(&self, input: &Path) -> MediaQcPlan {
        let input = input.to_string_lossy().into_owned();
        MediaQcPlan {
            metadata_probe: FfmpegInvocation {
                executable: self.sidecars.ffprobe.clone(),
                arguments: vec![
                    "-v".to_owned(),
                    "error".to_owned(),
                    "-show_format".to_owned(),
                    "-show_streams".to_owned(),
                    "-of".to_owned(),
                    "json".to_owned(),
                    input.clone(),
                ],
                purpose: "inspect media metadata for quality control".to_owned(),
            },
            pcm_decode: FfmpegInvocation {
                executable: self.sidecars.ffmpeg.clone(),
                arguments: vec![
                    "-hide_banner".to_owned(),
                    "-loglevel".to_owned(),
                    "error".to_owned(),
                    "-nostdin".to_owned(),
                    "-xerror".to_owned(),
                    "-i".to_owned(),
                    input,
                    "-map".to_owned(),
                    "0:a:0".to_owned(),
                    "-vn".to_owned(),
                    "-f".to_owned(),
                    "f32le".to_owned(),
                    "-c:a".to_owned(),
                    "pcm_f32le".to_owned(),
                    "pipe:1".to_owned(),
                ],
                purpose: "decode the primary audio stream for quality control".to_owned(),
            },
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct MediaQcPlan {
    pub metadata_probe: FfmpegInvocation,
    pub pcm_decode: FfmpegInvocation,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DecodeValidity {
    Valid,
    Invalid,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct AudioStreamMetadata {
    pub index: u32,
    pub codec_name: Option<String>,
    pub sample_rate_hz: Option<u32>,
    pub channels: Option<u16>,
    pub channel_layout: Option<String>,
    pub bitrate_bps: Option<u64>,
    pub duration_milliseconds: Option<u64>,
    pub is_default: bool,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct MediaFileMetadata {
    pub format_names: Vec<String>,
    pub duration_milliseconds: Option<u64>,
    pub bitrate_bps: Option<u64>,
    pub size_bytes: Option<u64>,
    pub audio_streams: Vec<AudioStreamMetadata>,
}

impl MediaFileMetadata {
    /// Returns the default audio stream, falling back to the first audio stream.
    #[must_use]
    pub fn primary_audio_stream(&self) -> Option<&AudioStreamMetadata> {
        self.audio_streams
            .iter()
            .find(|stream| stream.is_default)
            .or_else(|| self.audio_streams.first())
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct FileQcExpectations {
    #[serde(default)]
    pub allowed_containers: Vec<String>,
    #[serde(default)]
    pub allowed_audio_codecs: Vec<String>,
    pub sample_rate_hz: Option<u32>,
    pub channels: Option<u16>,
    pub bitrate_bps: Option<QcRangeU64>,
    pub duration_milliseconds: Option<QcRangeU64>,
    pub require_single_audio_stream: bool,
    pub require_decode_valid: bool,
}

impl Default for FileQcExpectations {
    fn default() -> Self {
        Self {
            allowed_containers: Vec::new(),
            allowed_audio_codecs: Vec::new(),
            sample_rate_hz: None,
            channels: None,
            bitrate_bps: None,
            duration_milliseconds: None,
            require_single_audio_stream: true,
            require_decode_valid: true,
        }
    }
}

impl FileQcExpectations {
    fn validate(&self) -> Result<()> {
        self.bitrate_bps
            .map_or(Ok(()), |range| range.validate("file bitrate range"))?;
        self.duration_milliseconds
            .map_or(Ok(()), |range| range.validate("file duration range"))?;
        if self
            .allowed_containers
            .iter()
            .chain(&self.allowed_audio_codecs)
            .any(|name| name.trim().is_empty())
        {
            return Err(MediaError::Configuration(
                "allowed container and codec names may not be empty".to_owned(),
            ));
        }
        Ok(())
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct FileQcAnalysis {
    pub decode_validity: DecodeValidity,
    pub metadata: Option<MediaFileMetadata>,
    pub findings: Vec<QcFinding>,
}

/// Parses the JSON emitted by the metadata-probe invocation.
#[allow(clippy::too_many_lines)]
pub fn parse_ffprobe_metadata(output: &str) -> Result<MediaFileMetadata> {
    let root: Value = serde_json::from_str(output)?;
    let object = root.as_object().ok_or_else(|| {
        MediaError::Configuration("ffprobe metadata root must be a JSON object".to_owned())
    })?;

    let mut audio_streams = Vec::new();
    if let Some(streams) = object.get("streams") {
        let streams = streams.as_array().ok_or_else(|| {
            MediaError::Configuration("ffprobe streams must be a JSON array".to_owned())
        })?;
        for (position, stream) in streams.iter().enumerate() {
            let stream = stream.as_object().ok_or_else(|| {
                MediaError::Configuration("ffprobe stream must be a JSON object".to_owned())
            })?;
            if optional_string(stream.get("codec_type"), "stream codec_type")?.as_deref()
                != Some("audio")
            {
                continue;
            }
            let fallback_index = u32::try_from(position).unwrap_or(u32::MAX);
            let index = optional_u64(stream.get("index"), "stream index")?.map_or(
                Ok(fallback_index),
                |value| {
                    u32::try_from(value).map_err(|_| {
                        MediaError::Configuration("ffprobe stream index is too large".to_owned())
                    })
                },
            )?;
            let channels = optional_u64(stream.get("channels"), "stream channels")?
                .map(|value| {
                    u16::try_from(value).map_err(|_| {
                        MediaError::Configuration(
                            "ffprobe stream channel count is too large".to_owned(),
                        )
                    })
                })
                .transpose()?;
            let sample_rate_hz = optional_u64(stream.get("sample_rate"), "stream sample rate")?
                .map(|value| {
                    u32::try_from(value).map_err(|_| {
                        MediaError::Configuration("ffprobe sample rate is too large".to_owned())
                    })
                })
                .transpose()?;
            let disposition = match stream.get("disposition") {
                None | Some(Value::Null) => None,
                Some(value) => Some(value.as_object().ok_or_else(|| {
                    MediaError::Configuration(
                        "ffprobe stream disposition must be a JSON object".to_owned(),
                    )
                })?),
            };
            let is_default = disposition
                .and_then(|disposition| disposition.get("default"))
                .map_or(Ok(false), |value| {
                    parse_boolean_flag(value, "default disposition")
                })?;
            audio_streams.push(AudioStreamMetadata {
                index,
                codec_name: optional_string(stream.get("codec_name"), "stream codec name")?
                    .map(|name| normalize_name(&name)),
                sample_rate_hz,
                channels,
                channel_layout: optional_string(
                    stream.get("channel_layout"),
                    "stream channel layout",
                )?,
                bitrate_bps: optional_u64(stream.get("bit_rate"), "stream bitrate")?,
                duration_milliseconds: optional_duration_milliseconds(
                    stream.get("duration"),
                    "stream duration",
                )?,
                is_default,
            });
        }
    }

    let format = match object.get("format") {
        None | Some(Value::Null) => None,
        Some(value) => Some(value.as_object().ok_or_else(|| {
            MediaError::Configuration("ffprobe format must be a JSON object".to_owned())
        })?),
    };
    let format_names =
        format
            .and_then(|format| format.get("format_name"))
            .map_or(Ok(Vec::new()), |value| {
                optional_string(Some(value), "format name").map(|name| {
                    name.into_iter()
                        .flat_map(|name| {
                            name.split(',')
                                .map(normalize_name)
                                .filter(|name| !name.is_empty())
                                .collect::<Vec<_>>()
                        })
                        .collect()
                })
            })?;
    Ok(MediaFileMetadata {
        format_names,
        duration_milliseconds: optional_duration_milliseconds(
            format.and_then(|format| format.get("duration")),
            "format duration",
        )?,
        bitrate_bps: optional_u64(
            format.and_then(|format| format.get("bit_rate")),
            "format bitrate",
        )?,
        size_bytes: optional_u64(format.and_then(|format| format.get("size")), "format size")?,
        audio_streams,
    })
}

/// Parses `ffprobe` output, converts metadata failures into findings, and applies file policy.
pub fn analyze_ffprobe_output(
    output: &str,
    decode_validity: DecodeValidity,
    expectations: &FileQcExpectations,
) -> Result<FileQcAnalysis> {
    expectations.validate()?;
    match parse_ffprobe_metadata(output) {
        Ok(metadata) => analyze_file_metadata(metadata, decode_validity, expectations),
        Err(error) => {
            let mut findings = vec![
                QcFinding::new(
                    QcFindingCode::FileMetadataInvalid,
                    QcSeverity::Error,
                    "media metadata could not be parsed",
                )
                .evidence(
                    QcValue::Text(error.to_string()),
                    QcExpectation::Description {
                        value: "valid ffprobe JSON metadata".to_owned(),
                    },
                ),
            ];
            append_decode_finding(&mut findings, decode_validity, expectations);
            Ok(FileQcAnalysis {
                decode_validity,
                metadata: None,
                findings,
            })
        }
    }
}

/// Applies file-level expectations to already parsed metadata.
#[allow(clippy::too_many_lines)]
pub fn analyze_file_metadata(
    metadata: MediaFileMetadata,
    decode_validity: DecodeValidity,
    expectations: &FileQcExpectations,
) -> Result<FileQcAnalysis> {
    expectations.validate()?;
    let mut findings = Vec::new();
    append_decode_finding(&mut findings, decode_validity, expectations);

    let audio_stream_count = u64::try_from(metadata.audio_streams.len()).unwrap_or(u64::MAX);
    if metadata.audio_streams.is_empty() {
        findings.push(
            QcFinding::new(
                QcFindingCode::FileAudioStreamMissing,
                QcSeverity::Error,
                "media file contains no audio stream",
            )
            .evidence(
                QcValue::Count(0),
                QcExpectation::Range {
                    min: Some(QcValue::Count(1)),
                    max: None,
                    inclusive: true,
                },
            ),
        );
    } else if expectations.require_single_audio_stream && metadata.audio_streams.len() != 1 {
        findings.push(
            QcFinding::new(
                QcFindingCode::FileMultipleAudioStreams,
                QcSeverity::Error,
                "media file contains more than one audio stream",
            )
            .evidence(
                QcValue::Count(audio_stream_count),
                QcExpectation::Exact {
                    value: QcValue::Count(1),
                },
            ),
        );
    }

    if !expectations.allowed_containers.is_empty()
        && !names_overlap(&metadata.format_names, &expectations.allowed_containers)
    {
        findings.push(
            QcFinding::new(
                QcFindingCode::FileContainerUnexpected,
                QcSeverity::Error,
                "media container is not allowed by the configured policy",
            )
            .evidence(
                QcValue::TextList(metadata.format_names.clone()),
                names_expectation(&expectations.allowed_containers),
            ),
        );
    }

    if let Some(stream) = metadata.primary_audio_stream() {
        if !expectations.allowed_audio_codecs.is_empty()
            && stream.codec_name.as_ref().is_none_or(|actual| {
                !expectations
                    .allowed_audio_codecs
                    .iter()
                    .any(|expected| normalize_name(expected) == normalize_name(actual))
            })
        {
            findings.push(
                QcFinding::new(
                    QcFindingCode::FileCodecUnexpected,
                    QcSeverity::Error,
                    "primary audio codec is not allowed by the configured policy",
                )
                .evidence(
                    QcValue::Text(
                        stream
                            .codec_name
                            .clone()
                            .unwrap_or_else(|| "unknown".to_owned()),
                    ),
                    names_expectation(&expectations.allowed_audio_codecs),
                ),
            );
        }
        append_exact_u32_finding(
            &mut findings,
            QcFindingCode::FileSampleRateUnexpected,
            "primary audio sample rate does not match the configured policy",
            stream.sample_rate_hz,
            expectations.sample_rate_hz,
            QcValue::Hertz,
        );
        append_channel_finding(&mut findings, stream.channels, expectations.channels);

        let bitrate = stream.bitrate_bps.or(metadata.bitrate_bps);
        if let Some(expected) = expectations.bitrate_bps
            && bitrate.is_none_or(|actual| !expected.contains(actual))
        {
            findings.push(
                QcFinding::new(
                    QcFindingCode::FileBitrateOutOfRange,
                    QcSeverity::Error,
                    "primary audio bitrate is outside the configured range",
                )
                .evidence(
                    bitrate.map_or_else(
                        || QcValue::Text("unknown".to_owned()),
                        QcValue::BitsPerSecond,
                    ),
                    u64_range_expectation(expected, QcValue::BitsPerSecond),
                ),
            );
        }

        let duration = stream
            .duration_milliseconds
            .or(metadata.duration_milliseconds);
        if let Some(expected) = expectations.duration_milliseconds
            && duration.is_none_or(|actual| !expected.contains(actual))
        {
            findings.push(
                QcFinding::new(
                    QcFindingCode::FileDurationOutOfRange,
                    QcSeverity::Error,
                    "audio duration is outside the configured range",
                )
                .evidence(
                    duration.map_or_else(
                        || QcValue::Text("unknown".to_owned()),
                        QcValue::Milliseconds,
                    ),
                    u64_range_expectation(expected, QcValue::Milliseconds),
                ),
            );
        }
    }

    Ok(FileQcAnalysis {
        decode_validity,
        metadata: Some(metadata),
        findings,
    })
}

fn append_decode_finding(
    findings: &mut Vec<QcFinding>,
    decode_validity: DecodeValidity,
    expectations: &FileQcExpectations,
) {
    if expectations.require_decode_valid && decode_validity == DecodeValidity::Invalid {
        findings.push(
            QcFinding::new(
                QcFindingCode::DecodeInvalid,
                QcSeverity::Error,
                "media sidecar could not decode the primary audio stream",
            )
            .evidence(
                QcValue::Boolean(false),
                QcExpectation::Exact {
                    value: QcValue::Boolean(true),
                },
            ),
        );
    }
}

fn append_exact_u32_finding(
    findings: &mut Vec<QcFinding>,
    code: QcFindingCode,
    message: &str,
    actual: Option<u32>,
    expected: Option<u32>,
    value: fn(u32) -> QcValue,
) {
    if let Some(expected) = expected
        && actual != Some(expected)
    {
        findings.push(QcFinding::new(code, QcSeverity::Error, message).evidence(
            actual.map_or_else(|| QcValue::Text("unknown".to_owned()), value),
            QcExpectation::Exact {
                value: value(expected),
            },
        ));
    }
}

fn append_channel_finding(
    findings: &mut Vec<QcFinding>,
    actual: Option<u16>,
    expected: Option<u16>,
) {
    if let Some(expected) = expected
        && actual != Some(expected)
    {
        findings.push(
            QcFinding::new(
                QcFindingCode::FileChannelCountUnexpected,
                QcSeverity::Error,
                "primary audio channel count does not match the configured policy",
            )
            .evidence(
                actual.map_or_else(
                    || QcValue::Text("unknown".to_owned()),
                    |value| QcValue::Count(u64::from(value)),
                ),
                QcExpectation::Exact {
                    value: QcValue::Count(u64::from(expected)),
                },
            ),
        );
    }
}

fn names_overlap(actual: &[String], expected: &[String]) -> bool {
    actual.iter().any(|actual| {
        expected
            .iter()
            .any(|expected| normalize_name(expected) == normalize_name(actual))
    })
}

fn names_expectation(names: &[String]) -> QcExpectation {
    QcExpectation::OneOf {
        values: names
            .iter()
            .map(|name| QcValue::Text(normalize_name(name)))
            .collect(),
    }
}

fn u64_range_expectation(range: QcRangeU64, value: fn(u64) -> QcValue) -> QcExpectation {
    QcExpectation::Range {
        min: range.min.map(value),
        max: range.max.map(value),
        inclusive: true,
    }
}

fn normalize_name(name: &str) -> String {
    name.trim().to_ascii_lowercase()
}

fn optional_string(value: Option<&Value>, field: &str) -> Result<Option<String>> {
    match value {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(value)) if value == "N/A" || value.is_empty() => Ok(None),
        Some(Value::String(value)) => Ok(Some(value.clone())),
        Some(_) => Err(MediaError::Configuration(format!(
            "ffprobe {field} must be a string"
        ))),
    }
}

fn optional_u64(value: Option<&Value>, field: &str) -> Result<Option<u64>> {
    match value {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(value)) if value == "N/A" || value.is_empty() => Ok(None),
        Some(Value::String(value)) => value.parse::<u64>().map(Some).map_err(|_| {
            MediaError::Configuration(format!("ffprobe {field} must be an unsigned integer"))
        }),
        Some(Value::Number(value)) => value.as_u64().map(Some).ok_or_else(|| {
            MediaError::Configuration(format!("ffprobe {field} must be an unsigned integer"))
        }),
        Some(_) => Err(MediaError::Configuration(format!(
            "ffprobe {field} must be an unsigned integer"
        ))),
    }
}

fn optional_duration_milliseconds(value: Option<&Value>, field: &str) -> Result<Option<u64>> {
    let seconds = match value {
        None | Some(Value::Null) => return Ok(None),
        Some(Value::String(value)) if value == "N/A" || value.is_empty() => return Ok(None),
        Some(Value::String(value)) => value
            .parse::<f64>()
            .map_err(|_| MediaError::Configuration(format!("ffprobe {field} must be seconds")))?,
        Some(Value::Number(value)) => value
            .as_f64()
            .ok_or_else(|| MediaError::Configuration(format!("ffprobe {field} must be seconds")))?,
        Some(_) => {
            return Err(MediaError::Configuration(format!(
                "ffprobe {field} must be seconds"
            )));
        }
    };
    let duration = Duration::try_from_secs_f64(seconds).map_err(|_| {
        MediaError::Configuration(format!("ffprobe {field} must be finite and non-negative"))
    })?;
    Ok(Some(
        u64::try_from(duration.as_millis()).unwrap_or(u64::MAX),
    ))
}

fn parse_boolean_flag(value: &Value, field: &str) -> Result<bool> {
    match value {
        Value::Bool(value) => Ok(*value),
        Value::Number(value) => value.as_u64().map(|value| value != 0).ok_or_else(|| {
            MediaError::Configuration(format!("ffprobe {field} must be a boolean flag"))
        }),
        Value::String(value) if value == "0" => Ok(false),
        Value::String(value) if value == "1" => Ok(true),
        _ => Err(MediaError::Configuration(format!(
            "ffprobe {field} must be a boolean flag"
        ))),
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::*;

    fn sidecars() -> SidecarPair {
        SidecarPair {
            ffmpeg: PathBuf::from("/app/ffmpeg"),
            ffprobe: PathBuf::from("/app/ffprobe"),
        }
    }

    fn probe_json() -> &'static str {
        r#"{
          "streams": [
            {"index": 0, "codec_name": "mp3", "codec_type": "audio",
             "sample_rate": "44100", "channels": 2, "channel_layout": "stereo",
             "duration": "12.345", "bit_rate": "192000", "disposition": {"default": 1}},
            {"index": 1, "codec_name": "mjpeg", "codec_type": "video"}
          ],
          "format": {"format_name": "mp3", "duration": "12.346",
                     "size": "296000", "bit_rate": "192000"}
        }"#
    }

    #[test]
    fn planner_uses_argument_vectors_and_strict_decode() {
        let plan = MediaQcPlanner::new(sidecars()).plan(Path::new("/books/a b.mp3"));
        assert_eq!(
            plan.metadata_probe.executable,
            PathBuf::from("/app/ffprobe")
        );
        assert_eq!(
            plan.metadata_probe.arguments.last().unwrap(),
            "/books/a b.mp3"
        );
        assert!(
            plan.pcm_decode
                .arguments
                .windows(2)
                .any(|pair| pair == ["-map", "0:a:0"])
        );
        assert!(
            plan.pcm_decode
                .arguments
                .iter()
                .any(|argument| argument == "-xerror")
        );
        assert_eq!(plan.pcm_decode.arguments.last().unwrap(), "pipe:1");
    }

    #[test]
    fn parses_primary_audio_and_ignores_cover_stream() {
        let metadata = parse_ffprobe_metadata(probe_json()).unwrap();
        assert_eq!(metadata.format_names, ["mp3"]);
        assert_eq!(metadata.audio_streams.len(), 1);
        let stream = metadata.primary_audio_stream().unwrap();
        assert_eq!(stream.codec_name.as_deref(), Some("mp3"));
        assert_eq!(stream.sample_rate_hz, Some(44_100));
        assert_eq!(stream.duration_milliseconds, Some(12_345));
        assert!(stream.is_default);
    }

    #[test]
    fn matching_policy_has_no_findings() {
        let expectations = FileQcExpectations {
            allowed_containers: vec!["MP3".to_owned()],
            allowed_audio_codecs: vec!["mp3".to_owned()],
            sample_rate_hz: Some(44_100),
            channels: Some(2),
            bitrate_bps: Some(QcRangeU64 {
                min: Some(192_000),
                max: Some(320_000),
            }),
            duration_milliseconds: Some(QcRangeU64 {
                min: Some(10_000),
                max: Some(20_000),
            }),
            ..FileQcExpectations::default()
        };
        let analysis =
            analyze_ffprobe_output(probe_json(), DecodeValidity::Valid, &expectations).unwrap();
        assert!(analysis.findings.is_empty());
    }

    #[test]
    fn reports_metadata_and_decode_failures_as_stable_findings() {
        let analysis = analyze_ffprobe_output(
            "not json",
            DecodeValidity::Invalid,
            &FileQcExpectations::default(),
        )
        .unwrap();
        assert!(analysis.metadata.is_none());
        assert_eq!(
            analysis
                .findings
                .iter()
                .map(|finding| finding.code)
                .collect::<Vec<_>>(),
            [
                QcFindingCode::FileMetadataInvalid,
                QcFindingCode::DecodeInvalid
            ]
        );
    }

    #[test]
    fn reports_all_file_policy_mismatches() {
        let mut value: Value = serde_json::from_str(probe_json()).unwrap();
        let duplicate = value["streams"][0].clone();
        value["streams"].as_array_mut().unwrap().push(duplicate);
        let expectations = FileQcExpectations {
            allowed_containers: vec!["wav".to_owned()],
            allowed_audio_codecs: vec!["flac".to_owned()],
            sample_rate_hz: Some(48_000),
            channels: Some(1),
            bitrate_bps: Some(QcRangeU64 {
                min: Some(256_000),
                max: None,
            }),
            duration_milliseconds: Some(QcRangeU64 {
                min: None,
                max: Some(10_000),
            }),
            ..FileQcExpectations::default()
        };
        let analysis = analyze_ffprobe_output(
            &serde_json::to_string(&value).unwrap(),
            DecodeValidity::Valid,
            &expectations,
        )
        .unwrap();
        for code in [
            QcFindingCode::FileMultipleAudioStreams,
            QcFindingCode::FileContainerUnexpected,
            QcFindingCode::FileCodecUnexpected,
            QcFindingCode::FileSampleRateUnexpected,
            QcFindingCode::FileChannelCountUnexpected,
            QcFindingCode::FileBitrateOutOfRange,
            QcFindingCode::FileDurationOutOfRange,
        ] {
            assert!(analysis.findings.iter().any(|finding| finding.code == code));
        }
    }

    #[test]
    fn reports_missing_audio_stream() {
        let analysis = analyze_ffprobe_output(
            r#"{"streams": [], "format": {"format_name": "mp3"}}"#,
            DecodeValidity::Valid,
            &FileQcExpectations::default(),
        )
        .unwrap();
        assert_eq!(
            analysis.findings[0].code,
            QcFindingCode::FileAudioStreamMissing
        );
    }
}
