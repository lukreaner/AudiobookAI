use std::collections::BTreeSet;

use serde::{Deserialize, Serialize};

use super::{QcExpectation, QcFinding, QcFindingCode, QcRangeU64, QcSeverity, QcValue};
use crate::Result;

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Mp3CbrStatus {
    Constant,
    Variable,
    InsufficientFrames,
    Invalid,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Mp3VbrHeader {
    Xing,
    Info,
    Vbri,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Mp3ScanErrorKind {
    TooShort,
    InvalidId3,
    NoFrames,
    InvalidHeader,
    TruncatedFrame,
    TrailingData,
    InconsistentStream,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct Mp3ScanError {
    pub kind: Mp3ScanErrorKind,
    pub byte_offset: u64,
    pub timestamp_milliseconds: u64,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct Mp3FrameAnalysis {
    pub status: Mp3CbrStatus,
    pub frame_count: u64,
    pub bitrates_kbps: Vec<u16>,
    pub sample_rates_hz: Vec<u32>,
    pub first_frame_offset: Option<u64>,
    pub audio_bytes: u64,
    pub duration_milliseconds: u64,
    pub first_variable_frame_milliseconds: Option<u64>,
    pub vbr_header: Option<Mp3VbrHeader>,
    pub error: Option<Mp3ScanError>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Mp3QcExpectations {
    pub require_cbr: bool,
    /// Bounds are expressed in kilobits per second.
    pub bitrate_kbps: Option<QcRangeU64>,
    pub sample_rate_hz: Option<u32>,
}

impl Default for Mp3QcExpectations {
    fn default() -> Self {
        Self {
            require_cbr: true,
            bitrate_kbps: None,
            sample_rate_hz: None,
        }
    }
}

impl Mp3QcExpectations {
    fn validate(self) -> Result<()> {
        self.bitrate_kbps
            .map_or(Ok(()), |range| range.validate("MP3 bitrate range"))
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Mp3QcAnalysis {
    pub frames: Mp3FrameAnalysis,
    pub findings: Vec<QcFinding>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum MpegVersion {
    One,
    Two,
    TwoPointFive,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum MpegLayer {
    One,
    Two,
    Three,
}

#[derive(Clone, Copy, Debug)]
struct FrameHeader {
    version: MpegVersion,
    layer: MpegLayer,
    bitrate_kbps: u16,
    sample_rate_hz: u32,
    frame_length: usize,
    samples_per_frame: u16,
    crc_present: bool,
    mono: bool,
}

/// Verifies an MP3 by stepping through every declared frame rather than searching for sync bytes.
/// `ID3v2` prefixes and `ID3v1` suffixes are excluded from the frame scan.
#[allow(clippy::too_many_lines)]
#[must_use]
pub fn scan_mp3_frames(bytes: &[u8]) -> Mp3FrameAnalysis {
    if bytes.len() < 4 {
        return invalid_analysis(Mp3ScanErrorKind::TooShort, 0, 0, None);
    }
    let audio_end = if bytes.len() >= 128 && &bytes[bytes.len() - 128..bytes.len() - 125] == b"TAG"
    {
        bytes.len() - 128
    } else {
        bytes.len()
    };
    let audio_start = match id3v2_end(bytes, audio_end) {
        Ok(offset) => offset,
        Err(offset) => {
            return invalid_analysis(Mp3ScanErrorKind::InvalidId3, offset, 0, None);
        }
    };
    if audio_end.saturating_sub(audio_start) < 4 {
        return invalid_analysis(Mp3ScanErrorKind::NoFrames, audio_start, 0, None);
    }

    let mut offset = audio_start;
    let mut frame_count = 0_u64;
    let mut audio_bytes = 0_u64;
    let mut total_samples = 0_u128;
    let mut bitrates = BTreeSet::new();
    let mut sample_rates = BTreeSet::new();
    let mut stream_identity = None;
    let mut first_bitrate = None;
    let mut first_variable_frame_milliseconds = None;
    let mut vbr_header = None;

    while offset < audio_end {
        let elapsed = stream_identity.map_or(0, |(_, _, sample_rate)| {
            samples_to_milliseconds(total_samples, sample_rate)
        });
        if audio_end - offset < 4 {
            return completed_invalid_analysis(
                frame_count,
                bitrates,
                sample_rates,
                audio_start,
                audio_bytes,
                elapsed,
                first_variable_frame_milliseconds,
                vbr_header,
                Mp3ScanErrorKind::TrailingData,
                offset,
            );
        }
        let Ok(header) = parse_frame_header(&bytes[offset..offset + 4]) else {
            let kind = if frame_count == 0 {
                Mp3ScanErrorKind::InvalidHeader
            } else {
                Mp3ScanErrorKind::TrailingData
            };
            return completed_invalid_analysis(
                frame_count,
                bitrates,
                sample_rates,
                audio_start,
                audio_bytes,
                elapsed,
                first_variable_frame_milliseconds,
                vbr_header,
                kind,
                offset,
            );
        };
        let frame_end = match offset.checked_add(header.frame_length) {
            Some(frame_end) if frame_end <= audio_end => frame_end,
            _ => {
                return completed_invalid_analysis(
                    frame_count,
                    bitrates,
                    sample_rates,
                    audio_start,
                    audio_bytes,
                    elapsed,
                    first_variable_frame_milliseconds,
                    vbr_header,
                    Mp3ScanErrorKind::TruncatedFrame,
                    offset,
                );
            }
        };

        let identity = (header.version, header.layer, header.sample_rate_hz);
        if stream_identity.is_some_and(|expected| expected != identity) {
            sample_rates.insert(header.sample_rate_hz);
            return completed_invalid_analysis(
                frame_count,
                bitrates,
                sample_rates,
                audio_start,
                audio_bytes,
                elapsed,
                first_variable_frame_milliseconds,
                vbr_header,
                Mp3ScanErrorKind::InconsistentStream,
                offset,
            );
        }
        stream_identity.get_or_insert(identity);

        if frame_count == 0 {
            vbr_header = detect_vbr_header(&bytes[offset..frame_end], header);
            if matches!(vbr_header, Some(Mp3VbrHeader::Xing | Mp3VbrHeader::Vbri)) {
                first_variable_frame_milliseconds = Some(0);
            }
        }
        if first_bitrate.is_some_and(|bitrate| bitrate != header.bitrate_kbps)
            && first_variable_frame_milliseconds.is_none()
        {
            first_variable_frame_milliseconds = Some(elapsed);
        }
        first_bitrate.get_or_insert(header.bitrate_kbps);
        bitrates.insert(header.bitrate_kbps);
        sample_rates.insert(header.sample_rate_hz);
        frame_count = frame_count.saturating_add(1);
        audio_bytes =
            audio_bytes.saturating_add(u64::try_from(header.frame_length).unwrap_or(u64::MAX));
        total_samples = total_samples.saturating_add(u128::from(header.samples_per_frame));
        offset = frame_end;
    }

    let duration_milliseconds = stream_identity.map_or(0, |(_, _, sample_rate)| {
        samples_to_milliseconds(total_samples, sample_rate)
    });
    let variable =
        bitrates.len() > 1 || matches!(vbr_header, Some(Mp3VbrHeader::Xing | Mp3VbrHeader::Vbri));
    let status = if variable {
        Mp3CbrStatus::Variable
    } else if frame_count < 2 {
        Mp3CbrStatus::InsufficientFrames
    } else {
        Mp3CbrStatus::Constant
    };
    Mp3FrameAnalysis {
        status,
        frame_count,
        bitrates_kbps: bitrates.into_iter().collect(),
        sample_rates_hz: sample_rates.into_iter().collect(),
        first_frame_offset: Some(u64::try_from(audio_start).unwrap_or(u64::MAX)),
        audio_bytes,
        duration_milliseconds,
        first_variable_frame_milliseconds,
        vbr_header,
        error: None,
    }
}

/// Scans the MP3 frame structure and evaluates CBR, bitrate, and sample-rate requirements.
#[allow(clippy::too_many_lines)]
pub fn analyze_mp3(bytes: &[u8], expectations: Mp3QcExpectations) -> Result<Mp3QcAnalysis> {
    expectations.validate()?;
    let frames = scan_mp3_frames(bytes);
    let mut findings = Vec::new();
    if let Some(error) = &frames.error {
        findings.push(
            QcFinding::new(
                QcFindingCode::Mp3Invalid,
                QcSeverity::Error,
                "MP3 frame structure is invalid",
            )
            .evidence(
                QcValue::Text(format!("{:?} at byte {}", error.kind, error.byte_offset)),
                QcExpectation::Description {
                    value: "consecutive complete MP3 frames with supported headers".to_owned(),
                },
            )
            .at(error.timestamp_milliseconds, error.timestamp_milliseconds),
        );
    } else if expectations.require_cbr {
        match frames.status {
            Mp3CbrStatus::Variable => {
                let timestamp = frames.first_variable_frame_milliseconds.unwrap_or_default();
                findings.push(
                    QcFinding::new(
                        QcFindingCode::Mp3VariableBitrate,
                        QcSeverity::Error,
                        "MP3 does not have a constant bitrate across all frames",
                    )
                    .evidence(
                        QcValue::TextList(variable_evidence(&frames)),
                        QcExpectation::Description {
                            value: "one bitrate across all frames and no Xing or VBRI marker"
                                .to_owned(),
                        },
                    )
                    .at(timestamp, timestamp),
                );
            }
            Mp3CbrStatus::InsufficientFrames => findings.push(
                QcFinding::new(
                    QcFindingCode::Mp3CbrUnverified,
                    QcSeverity::Error,
                    "MP3 contains too few frames to verify constant bitrate",
                )
                .evidence(
                    QcValue::Count(frames.frame_count),
                    QcExpectation::Range {
                        min: Some(QcValue::Count(2)),
                        max: None,
                        inclusive: true,
                    },
                ),
            ),
            Mp3CbrStatus::Constant | Mp3CbrStatus::Invalid => {}
        }
    }

    if let Some(expected) = expectations.bitrate_kbps {
        findings.extend(
            frames
                .bitrates_kbps
                .iter()
                .copied()
                .map(u64::from)
                .filter(|bitrate| !expected.contains(*bitrate))
                .map(|bitrate| {
                    QcFinding::new(
                        QcFindingCode::Mp3BitrateOutOfRange,
                        QcSeverity::Error,
                        "MP3 frame bitrate is outside the configured range",
                    )
                    .evidence(
                        QcValue::BitsPerSecond(bitrate.saturating_mul(1_000)),
                        QcExpectation::Range {
                            min: expected
                                .min
                                .map(|value| QcValue::BitsPerSecond(value.saturating_mul(1_000))),
                            max: expected
                                .max
                                .map(|value| QcValue::BitsPerSecond(value.saturating_mul(1_000))),
                            inclusive: true,
                        },
                    )
                }),
        );
    }
    if let Some(expected) = expectations.sample_rate_hz {
        findings.extend(
            frames
                .sample_rates_hz
                .iter()
                .copied()
                .filter(|sample_rate| *sample_rate != expected)
                .map(|sample_rate| {
                    QcFinding::new(
                        QcFindingCode::Mp3SampleRateUnexpected,
                        QcSeverity::Error,
                        "MP3 frame sample rate does not match the configured policy",
                    )
                    .evidence(
                        QcValue::Hertz(sample_rate),
                        QcExpectation::Exact {
                            value: QcValue::Hertz(expected),
                        },
                    )
                }),
        );
    }

    Ok(Mp3QcAnalysis { frames, findings })
}

fn id3v2_end(bytes: &[u8], audio_end: usize) -> std::result::Result<usize, usize> {
    if !bytes.starts_with(b"ID3") {
        return Ok(0);
    }
    if bytes.len() < 10 {
        return Err(0);
    }
    let size_bytes = &bytes[6..10];
    if size_bytes.iter().any(|value| value & 0x80 != 0) {
        return Err(6);
    }
    let size = (usize::from(size_bytes[0]) << 21)
        | (usize::from(size_bytes[1]) << 14)
        | (usize::from(size_bytes[2]) << 7)
        | usize::from(size_bytes[3]);
    let footer_size = usize::from(bytes[5] & 0x10 != 0) * 10;
    let end = 10_usize
        .checked_add(size)
        .and_then(|value| value.checked_add(footer_size))
        .ok_or(6_usize)?;
    if end > audio_end { Err(6) } else { Ok(end) }
}

fn parse_frame_header(bytes: &[u8]) -> std::result::Result<FrameHeader, ()> {
    let header = u32::from_be_bytes(bytes.try_into().map_err(|_| ())?);
    if header & 0xffe0_0000 != 0xffe0_0000 {
        return Err(());
    }
    let version = match (header >> 19) & 0b11 {
        0b00 => MpegVersion::TwoPointFive,
        0b10 => MpegVersion::Two,
        0b11 => MpegVersion::One,
        _ => return Err(()),
    };
    let layer = match (header >> 17) & 0b11 {
        0b01 => MpegLayer::Three,
        0b10 => MpegLayer::Two,
        0b11 => MpegLayer::One,
        _ => return Err(()),
    };
    let bitrate_index = usize::try_from((header >> 12) & 0b1111).map_err(|_| ())?;
    if bitrate_index == 0 || bitrate_index == 15 {
        return Err(());
    }
    let sample_rate_index = usize::try_from((header >> 10) & 0b11).map_err(|_| ())?;
    if sample_rate_index == 3 {
        return Err(());
    }
    let bitrate_kbps = bitrate(version, layer, bitrate_index).ok_or(())?;
    let sample_rate_hz = sample_rate(version, sample_rate_index).ok_or(())?;
    let padding = u64::from((header >> 9) & 1);
    let bits_per_second = u64::from(bitrate_kbps).saturating_mul(1_000);
    let sample_rate = u64::from(sample_rate_hz);
    let frame_length = match layer {
        MpegLayer::One => (12_u64.saturating_mul(bits_per_second) / sample_rate)
            .saturating_add(padding)
            .saturating_mul(4),
        MpegLayer::Two => 144_u64.saturating_mul(bits_per_second) / sample_rate + padding,
        MpegLayer::Three if version == MpegVersion::One => {
            144_u64.saturating_mul(bits_per_second) / sample_rate + padding
        }
        MpegLayer::Three => 72_u64.saturating_mul(bits_per_second) / sample_rate + padding,
    };
    let samples_per_frame = match (version, layer) {
        (_, MpegLayer::One) => 384,
        (_, MpegLayer::Two) | (MpegVersion::One, MpegLayer::Three) => 1_152,
        (MpegVersion::Two | MpegVersion::TwoPointFive, MpegLayer::Three) => 576,
    };
    Ok(FrameHeader {
        version,
        layer,
        bitrate_kbps,
        sample_rate_hz,
        frame_length: usize::try_from(frame_length).map_err(|_| ())?,
        samples_per_frame,
        crc_present: header & (1 << 16) == 0,
        mono: (header >> 6) & 0b11 == 0b11,
    })
}

fn bitrate(version: MpegVersion, layer: MpegLayer, index: usize) -> Option<u16> {
    const MPEG1_LAYER1: [u16; 14] = [
        32, 64, 96, 128, 160, 192, 224, 256, 288, 320, 352, 384, 416, 448,
    ];
    const MPEG1_LAYER2: [u16; 14] = [
        32, 48, 56, 64, 80, 96, 112, 128, 160, 192, 224, 256, 320, 384,
    ];
    const MPEG1_LAYER3: [u16; 14] = [
        32, 40, 48, 56, 64, 80, 96, 112, 128, 160, 192, 224, 256, 320,
    ];
    const MPEG2_LAYER1: [u16; 14] = [
        32, 48, 56, 64, 80, 96, 112, 128, 144, 160, 176, 192, 224, 256,
    ];
    const MPEG2_LAYER23: [u16; 14] = [8, 16, 24, 32, 40, 48, 56, 64, 80, 96, 112, 128, 144, 160];
    let table = match (version, layer) {
        (MpegVersion::One, MpegLayer::One) => &MPEG1_LAYER1,
        (MpegVersion::One, MpegLayer::Two) => &MPEG1_LAYER2,
        (MpegVersion::One, MpegLayer::Three) => &MPEG1_LAYER3,
        (MpegVersion::Two | MpegVersion::TwoPointFive, MpegLayer::One) => &MPEG2_LAYER1,
        (MpegVersion::Two | MpegVersion::TwoPointFive, MpegLayer::Two | MpegLayer::Three) => {
            &MPEG2_LAYER23
        }
    };
    table.get(index - 1).copied()
}

fn sample_rate(version: MpegVersion, index: usize) -> Option<u32> {
    let rates = match version {
        MpegVersion::One => [44_100, 48_000, 32_000],
        MpegVersion::Two => [22_050, 24_000, 16_000],
        MpegVersion::TwoPointFive => [11_025, 12_000, 8_000],
    };
    rates.get(index).copied()
}

fn detect_vbr_header(frame: &[u8], header: FrameHeader) -> Option<Mp3VbrHeader> {
    let crc_bytes = usize::from(header.crc_present) * 2;
    let side_information_bytes = match (header.version, header.mono) {
        (MpegVersion::One, false) => 32,
        (MpegVersion::Two | MpegVersion::TwoPointFive, true) => 9,
        (MpegVersion::One, true) | (MpegVersion::Two | MpegVersion::TwoPointFive, false) => 17,
    };
    let xing_offset = 4 + crc_bytes + side_information_bytes;
    if frame.get(xing_offset..xing_offset + 4) == Some(b"Xing") {
        return Some(Mp3VbrHeader::Xing);
    }
    if frame.get(xing_offset..xing_offset + 4) == Some(b"Info") {
        return Some(Mp3VbrHeader::Info);
    }
    for vbri_offset in [36, 38] {
        if frame.get(vbri_offset..vbri_offset + 4) == Some(b"VBRI") {
            return Some(Mp3VbrHeader::Vbri);
        }
    }
    None
}

fn samples_to_milliseconds(samples: u128, sample_rate_hz: u32) -> u64 {
    let milliseconds = samples
        .saturating_mul(1_000)
        .checked_div(u128::from(sample_rate_hz))
        .unwrap_or_default();
    u64::try_from(milliseconds).unwrap_or(u64::MAX)
}

fn variable_evidence(frames: &Mp3FrameAnalysis) -> Vec<String> {
    let mut values = frames
        .bitrates_kbps
        .iter()
        .map(|bitrate| format!("{bitrate} kbps"))
        .collect::<Vec<_>>();
    if let Some(header) = frames.vbr_header
        && matches!(header, Mp3VbrHeader::Xing | Mp3VbrHeader::Vbri)
    {
        values.push(format!(
            "{} header",
            format!("{header:?}").to_ascii_lowercase()
        ));
    }
    values
}

fn invalid_analysis(
    kind: Mp3ScanErrorKind,
    offset: usize,
    timestamp_milliseconds: u64,
    first_frame_offset: Option<u64>,
) -> Mp3FrameAnalysis {
    Mp3FrameAnalysis {
        status: Mp3CbrStatus::Invalid,
        frame_count: 0,
        bitrates_kbps: Vec::new(),
        sample_rates_hz: Vec::new(),
        first_frame_offset,
        audio_bytes: 0,
        duration_milliseconds: 0,
        first_variable_frame_milliseconds: None,
        vbr_header: None,
        error: Some(Mp3ScanError {
            kind,
            byte_offset: u64::try_from(offset).unwrap_or(u64::MAX),
            timestamp_milliseconds,
        }),
    }
}

#[allow(clippy::too_many_arguments)]
fn completed_invalid_analysis(
    frame_count: u64,
    bitrates: BTreeSet<u16>,
    sample_rates: BTreeSet<u32>,
    audio_start: usize,
    audio_bytes: u64,
    timestamp_milliseconds: u64,
    first_variable_frame_milliseconds: Option<u64>,
    vbr_header: Option<Mp3VbrHeader>,
    kind: Mp3ScanErrorKind,
    offset: usize,
) -> Mp3FrameAnalysis {
    Mp3FrameAnalysis {
        status: Mp3CbrStatus::Invalid,
        frame_count,
        bitrates_kbps: bitrates.into_iter().collect(),
        sample_rates_hz: sample_rates.into_iter().collect(),
        first_frame_offset: Some(u64::try_from(audio_start).unwrap_or(u64::MAX)),
        audio_bytes,
        duration_milliseconds: timestamp_milliseconds,
        first_variable_frame_milliseconds,
        vbr_header,
        error: Some(Mp3ScanError {
            kind,
            byte_offset: u64::try_from(offset).unwrap_or(u64::MAX),
            timestamp_milliseconds,
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn frame(bitrate_index: u8, sample_rate_index: u8) -> Vec<u8> {
        let header = 0xffe0_0000_u32
            | (0b11 << 19) // MPEG-1
            | (0b01 << 17) // Layer III
            | (1 << 16) // no CRC
            | (u32::from(bitrate_index) << 12)
            | (u32::from(sample_rate_index) << 10)
            | (0b11 << 6); // mono
        let header_bytes = header.to_be_bytes();
        let parsed = parse_frame_header(&header_bytes).unwrap();
        let mut frame = vec![0_u8; parsed.frame_length];
        frame[..4].copy_from_slice(&header_bytes);
        frame
    }

    fn frames(bitrate_indexes: &[u8]) -> Vec<u8> {
        bitrate_indexes
            .iter()
            .flat_map(|index| frame(*index, 0))
            .collect()
    }

    #[test]
    fn verifies_constant_bitrate_across_all_frames() {
        let bytes = frames(&[11, 11, 11]);
        let analysis = scan_mp3_frames(&bytes);
        assert_eq!(analysis.status, Mp3CbrStatus::Constant);
        assert_eq!(analysis.frame_count, 3);
        assert_eq!(analysis.bitrates_kbps, [192]);
        assert_eq!(analysis.sample_rates_hz, [44_100]);
        assert_eq!(analysis.duration_milliseconds, 78);
        assert!(analysis.error.is_none());
    }

    #[test]
    fn detects_frame_level_variable_bitrate_at_timestamp() {
        let bytes = frames(&[11, 9, 11]);
        let analysis = analyze_mp3(&bytes, Mp3QcExpectations::default()).unwrap();
        assert_eq!(analysis.frames.status, Mp3CbrStatus::Variable);
        assert_eq!(analysis.frames.bitrates_kbps, [128, 192]);
        let finding = analysis
            .findings
            .iter()
            .find(|finding| finding.code == QcFindingCode::Mp3VariableBitrate)
            .unwrap();
        assert_eq!(finding.start_milliseconds, Some(26));
    }

    #[test]
    fn distinguishes_xing_vbr_from_info_cbr_headers() {
        let mut xing = frames(&[11, 11]);
        xing[21..25].copy_from_slice(b"Xing");
        let xing = scan_mp3_frames(&xing);
        assert_eq!(xing.vbr_header, Some(Mp3VbrHeader::Xing));
        assert_eq!(xing.status, Mp3CbrStatus::Variable);

        let mut info = frames(&[11, 11]);
        info[21..25].copy_from_slice(b"Info");
        let info = scan_mp3_frames(&info);
        assert_eq!(info.vbr_header, Some(Mp3VbrHeader::Info));
        assert_eq!(info.status, Mp3CbrStatus::Constant);
    }

    #[test]
    fn skips_id3v2_and_id3v1_tags() {
        let mut bytes = b"ID3\x04\x00\x00\x00\x00\x00\x03abc".to_vec();
        bytes.extend(frames(&[11, 11]));
        let mut id3v1 = vec![0_u8; 128];
        id3v1[..3].copy_from_slice(b"TAG");
        bytes.extend(id3v1);
        let analysis = scan_mp3_frames(&bytes);
        assert_eq!(analysis.status, Mp3CbrStatus::Constant);
        assert_eq!(analysis.first_frame_offset, Some(13));
        assert_eq!(analysis.frame_count, 2);
    }

    #[test]
    fn identifies_truncated_and_trailing_data() {
        let mut truncated = frame(11, 0);
        truncated.truncate(truncated.len() - 1);
        assert_eq!(
            scan_mp3_frames(&truncated).error.unwrap().kind,
            Mp3ScanErrorKind::TruncatedFrame
        );

        let mut trailing = frames(&[11, 11]);
        trailing.push(0);
        assert_eq!(
            scan_mp3_frames(&trailing).error.unwrap().kind,
            Mp3ScanErrorKind::TrailingData
        );
    }

    #[test]
    fn one_frame_cannot_prove_cbr() {
        let analysis = analyze_mp3(&frame(11, 0), Mp3QcExpectations::default()).unwrap();
        assert_eq!(analysis.frames.status, Mp3CbrStatus::InsufficientFrames);
        assert_eq!(analysis.findings[0].code, QcFindingCode::Mp3CbrUnverified);
    }

    #[test]
    fn evaluates_bitrate_and_sample_rate_policy() {
        let expectations = Mp3QcExpectations {
            require_cbr: true,
            bitrate_kbps: Some(QcRangeU64 {
                min: Some(256),
                max: Some(320),
            }),
            sample_rate_hz: Some(48_000),
        };
        let analysis = analyze_mp3(&frames(&[11, 11]), expectations).unwrap();
        assert!(
            analysis
                .findings
                .iter()
                .any(|finding| finding.code == QcFindingCode::Mp3BitrateOutOfRange)
        );
        assert!(
            analysis
                .findings
                .iter()
                .any(|finding| finding.code == QcFindingCode::Mp3SampleRateUnexpected)
        );
    }

    #[test]
    fn invalid_id3_size_is_reported() {
        let bytes = b"ID3\x04\x00\x00\x7f\x7f\x7f\x7f";
        let analysis = analyze_mp3(bytes, Mp3QcExpectations::default()).unwrap();
        assert_eq!(analysis.frames.status, Mp3CbrStatus::Invalid);
        assert_eq!(
            analysis.frames.error.unwrap().kind,
            Mp3ScanErrorKind::InvalidId3
        );
        assert_eq!(analysis.findings[0].code, QcFindingCode::Mp3Invalid);
    }
}
