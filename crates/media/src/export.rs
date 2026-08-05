use std::{collections::BTreeMap, path::PathBuf};

use serde::{Deserialize, Serialize};

use crate::{MediaError, Result, SidecarPair};

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ExportFormat {
    Mp3,
    Wav,
    M4a,
    M4b,
}

impl ExportFormat {
    pub fn extension(self) -> &'static str {
        match self {
            Self::Mp3 => "mp3",
            Self::Wav => "wav",
            Self::M4a => "m4a",
            Self::M4b => "m4b",
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct ChapterAudio {
    pub title: String,
    pub path: PathBuf,
    pub duration_milliseconds: u64,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct BookMetadata {
    pub title: String,
    pub authors: Vec<String>,
    pub narrator: Option<String>,
    pub series: Option<String>,
    pub series_position: Option<f64>,
    pub language: Option<String>,
    pub date: Option<String>,
    pub description: Option<String>,
    pub isbn: Option<String>,
    #[serde(default)]
    pub additional: BTreeMap<String, String>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct BackgroundMusic {
    pub path: PathBuf,
    pub trim_start_seconds: f64,
    pub trim_end_seconds: Option<f64>,
    pub gain_db: f64,
    pub fade_in_seconds: f64,
    pub fade_out_seconds: f64,
    pub duck_threshold: f64,
    pub duck_ratio: f64,
}

impl Default for BackgroundMusic {
    fn default() -> Self {
        Self {
            path: PathBuf::new(),
            trim_start_seconds: 0.0,
            trim_end_seconds: None,
            gain_db: -20.0,
            fade_in_seconds: 2.0,
            fade_out_seconds: 3.0,
            duck_threshold: 0.03,
            duck_ratio: 8.0,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub struct LoudnessSettings {
    pub target_lufs: f64,
    pub true_peak_db: f64,
    pub loudness_range: f64,
}

impl Default for LoudnessSettings {
    fn default() -> Self {
        Self {
            target_lufs: -19.0,
            true_peak_db: -3.0,
            loudness_range: 7.0,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub struct LoudnessMeasurement {
    pub input_i: f64,
    pub input_tp: f64,
    pub input_lra: f64,
    pub input_thresh: f64,
    pub target_offset: f64,
}

/// Extracts the final JSON object emitted by `FFmpeg`'s `loudnorm=print_format=json` pass.
pub fn parse_loudness_measurement(stderr: &str) -> Result<LoudnessMeasurement> {
    let start = stderr
        .rfind('{')
        .ok_or_else(|| MediaError::Configuration("loudness output contained no JSON".to_owned()))?;
    let end = stderr[start..]
        .find('}')
        .map(|offset| start + offset + 1)
        .ok_or_else(|| MediaError::Configuration("loudness JSON was truncated".to_owned()))?;
    let value: serde_json::Value = serde_json::from_str(&stderr[start..end])?;
    let field = |name: &str| -> Result<f64> {
        let value = value
            .get(name)
            .ok_or_else(|| MediaError::Configuration(format!("loudness JSON omitted {name}")))?;
        let number = value
            .as_f64()
            .or_else(|| value.as_str().and_then(|value| value.parse().ok()))
            .ok_or_else(|| MediaError::Configuration(format!("invalid loudness value {name}")))?;
        if number.is_finite() {
            Ok(number)
        } else {
            Err(MediaError::Configuration(format!(
                "non-finite loudness value {name}"
            )))
        }
    };
    Ok(LoudnessMeasurement {
        input_i: field("input_i")?,
        input_tp: field("input_tp")?,
        input_lra: field("input_lra")?,
        input_thresh: field("input_thresh")?,
        target_offset: field("target_offset")?,
    })
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ExportRequest {
    pub chapters: Vec<ChapterAudio>,
    /// Exact file path for single-file exports; output directory for split exports.
    pub output: PathBuf,
    pub format: ExportFormat,
    pub split_per_chapter: bool,
    pub metadata: BookMetadata,
    pub cover_art: Option<PathBuf>,
    pub background_music: Option<BackgroundMusic>,
    pub loudness: Option<LoudnessSettings>,
    pub preview: bool,
    pub overwrite: bool,
    pub bitrate_kbps: u16,
    pub sample_rate: u32,
    pub channels: u8,
}

impl ExportRequest {
    pub fn audiobook_defaults(
        chapters: Vec<ChapterAudio>,
        output: PathBuf,
        format: ExportFormat,
        metadata: BookMetadata,
    ) -> Self {
        Self {
            chapters,
            output,
            format,
            split_per_chapter: false,
            metadata,
            cover_art: None,
            background_music: None,
            loudness: Some(LoudnessSettings::default()),
            preview: false,
            overwrite: false,
            bitrate_kbps: 128,
            sample_rate: 48_000,
            channels: 2,
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct AuxiliaryFile {
    pub path: PathBuf,
    pub contents: String,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct FfmpegInvocation {
    pub executable: PathBuf,
    pub arguments: Vec<String>,
    pub purpose: String,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct ExportPlan {
    pub invocations: Vec<FfmpegInvocation>,
    pub auxiliary_files: Vec<AuxiliaryFile>,
    pub outputs: Vec<PathBuf>,
    pub manifest_path: PathBuf,
}

#[derive(Clone, Debug)]
pub struct ExportPlanner {
    sidecars: SidecarPair,
}

impl ExportPlanner {
    pub fn new(sidecars: SidecarPair) -> Self {
        Self { sidecars }
    }

    /// Builds the first loudness pass. Its stderr JSON is parsed into `LoudnessMeasurement` by the
    /// service and passed to [`render`](Self::render).
    pub fn loudness_analysis(&self, request: &ExportRequest) -> Result<Vec<FfmpegInvocation>> {
        validate_request(request)?;
        let Some(settings) = request.loudness else {
            return Ok(Vec::new());
        };
        output_units(request)
            .into_iter()
            .enumerate()
            .map(|(index, unit)| {
                let inputs = build_inputs(request, &unit, None);
                let (mut arguments, indexes) = inputs.arguments;
                let prefilter = build_prefilter(request, &unit, &indexes)?;
                let output = format!(
                    "{prefilter}loudnorm=I={}:TP={}:LRA={}:print_format=json[outa]",
                    number(settings.target_lufs),
                    number(settings.true_peak_db),
                    number(settings.loudness_range)
                );
                arguments.extend([
                    "-filter_complex".to_owned(),
                    output,
                    "-map".to_owned(),
                    "[outa]".to_owned(),
                    "-f".to_owned(),
                    "null".to_owned(),
                    "-".to_owned(),
                ]);
                Ok(FfmpegInvocation {
                    executable: self.sidecars.ffmpeg.clone(),
                    arguments,
                    purpose: format!("analyze loudness for output {}", index + 1),
                })
            })
            .collect()
    }

    /// Builds final render commands. Production loudness normalization requires one two-pass
    /// measurement per output; previews intentionally use single-pass normalization.
    pub fn render(
        &self,
        request: &ExportRequest,
        measurements: &[LoudnessMeasurement],
    ) -> Result<ExportPlan> {
        validate_request(request)?;
        let units = output_units(request);
        if request.loudness.is_some() && !request.preview && measurements.len() != units.len() {
            return Err(MediaError::LoudnessMeasurementRequired);
        }
        let mut invocations = Vec::with_capacity(units.len());
        let mut auxiliary_files = Vec::new();
        let mut outputs = Vec::new();

        for (index, unit) in units.into_iter().enumerate() {
            let ffmetadata = if unit.chapters.len() > 1 {
                let path = auxiliary_path(&unit.output, "ffmetadata");
                auxiliary_files.push(AuxiliaryFile {
                    path: path.clone(),
                    contents: ffmetadata(&request.metadata, &unit.chapters),
                });
                Some(path)
            } else {
                None
            };
            let inputs = build_inputs(request, &unit, ffmetadata.as_ref());
            let (mut arguments, indexes) = inputs.arguments;
            let prefilter = build_prefilter(request, &unit, &indexes)?;
            let filter = match request.loudness {
                Some(settings) if request.preview => format!(
                    "{prefilter}loudnorm=I={}:TP={}:LRA={}[outa]",
                    number(settings.target_lufs),
                    number(settings.true_peak_db),
                    number(settings.loudness_range)
                ),
                Some(settings) => {
                    let measured = measurements
                        .get(index)
                        .ok_or(MediaError::LoudnessMeasurementRequired)?;
                    format!(
                        "{prefilter}loudnorm=I={}:TP={}:LRA={}:measured_I={}:measured_TP={}:measured_LRA={}:measured_thresh={}:offset={}:linear=true[outa]",
                        number(settings.target_lufs),
                        number(settings.true_peak_db),
                        number(settings.loudness_range),
                        number(measured.input_i),
                        number(measured.input_tp),
                        number(measured.input_lra),
                        number(measured.input_thresh),
                        number(measured.target_offset),
                    )
                }
                None => format!("{prefilter}anull[outa]"),
            };
            arguments.extend([
                "-filter_complex".to_owned(),
                filter,
                "-map".to_owned(),
                "[outa]".to_owned(),
            ]);
            if let Some(cover_index) = indexes.cover {
                arguments.extend(["-map".to_owned(), format!("{cover_index}:v:0")]);
            }
            if let Some(metadata_index) = indexes.ffmetadata {
                arguments.extend([
                    "-map_metadata".to_owned(),
                    metadata_index.to_string(),
                    "-map_chapters".to_owned(),
                    metadata_index.to_string(),
                ]);
            }
            add_metadata_arguments(&mut arguments, &request.metadata, &unit.title);
            add_codec_arguments(
                &mut arguments,
                request,
                indexes.cover.is_some(),
                unit.chapters.len() > 1,
            );
            arguments.push(unit.output.to_string_lossy().into_owned());
            invocations.push(FfmpegInvocation {
                executable: self.sidecars.ffmpeg.clone(),
                arguments,
                purpose: format!("render {}", unit.output.display()),
            });
            outputs.push(unit.output);
        }

        let manifest_path = if request.split_per_chapter {
            request.output.join("audiobookai-export-manifest.json")
        } else {
            auxiliary_path(&request.output, "manifest.json")
        };
        Ok(ExportPlan {
            invocations,
            auxiliary_files,
            outputs,
            manifest_path,
        })
    }
}

#[derive(Debug)]
struct OutputUnit {
    chapters: Vec<ChapterAudio>,
    output: PathBuf,
    title: String,
}

fn output_units(request: &ExportRequest) -> Vec<OutputUnit> {
    if !request.split_per_chapter {
        return vec![OutputUnit {
            chapters: request.chapters.clone(),
            output: request.output.clone(),
            title: request.metadata.title.clone(),
        }];
    }
    request
        .chapters
        .iter()
        .enumerate()
        .map(|(index, chapter)| OutputUnit {
            chapters: vec![chapter.clone()],
            output: request.output.join(format!(
                "{:03} - {}.{}",
                index + 1,
                safe_file_component(&chapter.title),
                request.format.extension()
            )),
            title: chapter.title.clone(),
        })
        .collect()
}

#[derive(Debug)]
struct InputIndexes {
    music: Option<usize>,
    cover: Option<usize>,
    ffmetadata: Option<usize>,
}

#[derive(Debug)]
struct InputArguments {
    arguments: (Vec<String>, InputIndexes),
}

fn build_inputs(
    request: &ExportRequest,
    unit: &OutputUnit,
    ffmetadata: Option<&PathBuf>,
) -> InputArguments {
    let mut arguments = vec![
        "-hide_banner".to_owned(),
        "-nostdin".to_owned(),
        if request.overwrite { "-y" } else { "-n" }.to_owned(),
    ];
    for chapter in &unit.chapters {
        arguments.extend(["-i".to_owned(), chapter.path.to_string_lossy().into_owned()]);
    }
    let mut index = unit.chapters.len();
    let music = request.background_music.as_ref().map(|music| {
        arguments.extend(["-stream_loop".to_owned(), "-1".to_owned()]);
        if music.trim_start_seconds > 0.0 {
            arguments.extend(["-ss".to_owned(), number(music.trim_start_seconds)]);
        }
        if let Some(end) = music.trim_end_seconds {
            arguments.extend(["-to".to_owned(), number(end)]);
        }
        arguments.extend(["-i".to_owned(), music.path.to_string_lossy().into_owned()]);
        let value = index;
        index += 1;
        value
    });
    let cover = request.cover_art.as_ref().map(|cover| {
        arguments.extend(["-i".to_owned(), cover.to_string_lossy().into_owned()]);
        let value = index;
        index += 1;
        value
    });
    let metadata = ffmetadata.map(|path| {
        arguments.extend([
            "-f".to_owned(),
            "ffmetadata".to_owned(),
            "-i".to_owned(),
            path.to_string_lossy().into_owned(),
        ]);
        index
    });
    InputArguments {
        arguments: (
            arguments,
            InputIndexes {
                music,
                cover,
                ffmetadata: metadata,
            },
        ),
    }
}

fn build_prefilter(
    request: &ExportRequest,
    unit: &OutputUnit,
    indexes: &InputIndexes,
) -> Result<String> {
    let mut filter = String::new();
    if unit.chapters.len() == 1 {
        filter.push_str("[0:a]aresample=48000:async=1:first_pts=0[voice];");
    } else {
        for index in 0..unit.chapters.len() {
            filter.push_str(&format!(
                "[{index}:a]aresample=48000:async=1:first_pts=0[a{index}];"
            ));
        }
        for index in 0..unit.chapters.len() {
            filter.push_str(&format!("[a{index}]"));
        }
        filter.push_str(&format!("concat=n={}:v=0:a=1[voice];", unit.chapters.len()));
    }

    if let (Some(music), Some(music_index)) = (&request.background_music, indexes.music) {
        let duration_milliseconds = unit
            .chapters
            .iter()
            .map(|chapter| chapter.duration_milliseconds)
            .sum::<u64>();
        let duration = std::time::Duration::from_millis(duration_milliseconds).as_secs_f64();
        if !duration.is_finite() || duration <= 0.0 {
            return Err(MediaError::Configuration(
                "chapter duration must be positive for music mixing".to_owned(),
            ));
        }
        let fade_out_start = (duration - music.fade_out_seconds).max(0.0);
        filter.push_str(&format!(
            "[{music_index}:a]aresample=48000,atrim=duration={},volume={}dB,afade=t=in:st=0:d={},afade=t=out:st={}:d={}[musicbed];",
            number(duration),
            number(music.gain_db),
            number(music.fade_in_seconds),
            number(fade_out_start),
            number(music.fade_out_seconds)
        ));
        filter.push_str(&format!(
            "[musicbed][voice]sidechaincompress=threshold={}:ratio={}:attack=20:release=500[ducked];[voice][ducked]amix=inputs=2:duration=first:normalize=0[mixed];",
            number(music.duck_threshold),
            number(music.duck_ratio)
        ));
        filter.push_str("[mixed]");
    } else {
        filter.push_str("[voice]");
    }
    Ok(filter)
}

fn add_codec_arguments(
    arguments: &mut Vec<String>,
    request: &ExportRequest,
    has_cover: bool,
    has_chapters: bool,
) {
    arguments.extend([
        "-ar".to_owned(),
        request.sample_rate.to_string(),
        "-ac".to_owned(),
        request.channels.to_string(),
    ]);
    match request.format {
        ExportFormat::Mp3 => arguments.extend([
            "-c:a".to_owned(),
            "libmp3lame".to_owned(),
            "-b:a".to_owned(),
            format!("{}k", request.bitrate_kbps),
            "-id3v2_version".to_owned(),
            "3".to_owned(),
            "-write_id3v1".to_owned(),
            "1".to_owned(),
        ]),
        ExportFormat::Wav => arguments.extend([
            "-c:a".to_owned(),
            "pcm_s24le".to_owned(),
            "-rf64".to_owned(),
            "auto".to_owned(),
            "-write_id3v2".to_owned(),
            "1".to_owned(),
        ]),
        ExportFormat::M4a | ExportFormat::M4b => {
            arguments.extend([
                "-c:a".to_owned(),
                "aac".to_owned(),
                "-b:a".to_owned(),
                format!("{}k", request.bitrate_kbps),
                "-movflags".to_owned(),
                "+faststart+use_metadata_tags".to_owned(),
                "-brand".to_owned(),
                if request.format == ExportFormat::M4b {
                    "M4B"
                } else {
                    "M4A"
                }
                .to_owned(),
            ]);
            if request.format == ExportFormat::M4b {
                arguments.extend([
                    "-metadata:s:a:0".to_owned(),
                    "media_type=2".to_owned(),
                    "-metadata".to_owned(),
                    "stik=2".to_owned(),
                ]);
            }
            if has_chapters {
                // FFmpeg writes both the MP4 chapter track and chpl atom where supported.
                arguments.extend([
                    "-movflags".to_owned(),
                    "+faststart+use_metadata_tags".to_owned(),
                ]);
            }
        }
    }
    if has_cover {
        arguments.extend([
            "-c:v".to_owned(),
            "mjpeg".to_owned(),
            "-frames:v".to_owned(),
            "1".to_owned(),
            "-disposition:v:0".to_owned(),
            "attached_pic".to_owned(),
            "-metadata:s:v:0".to_owned(),
            "title=Cover".to_owned(),
        ]);
    } else {
        arguments.push("-vn".to_owned());
    }
}

fn add_metadata_arguments(arguments: &mut Vec<String>, metadata: &BookMetadata, title: &str) {
    let mut values = vec![
        ("title", title.to_owned()),
        ("album", metadata.title.clone()),
        ("genre", "Audiobook".to_owned()),
    ];
    if !metadata.authors.is_empty() {
        values.push(("artist", metadata.authors.join("; ")));
        values.push(("album_artist", metadata.authors.join("; ")));
    }
    for (key, value) in [
        ("composer", metadata.narrator.as_deref()),
        ("grouping", metadata.series.as_deref()),
        ("series", metadata.series.as_deref()),
        ("language", metadata.language.as_deref()),
        ("date", metadata.date.as_deref()),
        ("comment", metadata.description.as_deref()),
        ("isbn", metadata.isbn.as_deref()),
    ] {
        if let Some(value) = value {
            values.push((key, value.to_owned()));
        }
    }
    if let Some(position) = metadata.series_position {
        values.push(("series-part", number(position)));
    }
    values.extend(
        metadata
            .additional
            .iter()
            .map(|(key, value)| (key.as_str(), value.clone())),
    );
    for (key, value) in values {
        arguments.extend(["-metadata".to_owned(), format!("{key}={value}")]);
    }
}

/// Builds an ffmetadata document with millisecond chapter markers.
pub fn ffmetadata(metadata: &BookMetadata, chapters: &[ChapterAudio]) -> String {
    let mut output = String::from(";FFMETADATA1\n");
    output.push_str(&format!("title={}\n", escape_ffmetadata(&metadata.title)));
    if !metadata.authors.is_empty() {
        output.push_str(&format!(
            "artist={}\n",
            escape_ffmetadata(&metadata.authors.join("; "))
        ));
    }
    if let Some(series) = &metadata.series {
        output.push_str(&format!("series={}\n", escape_ffmetadata(series)));
    }
    if let Some(position) = metadata.series_position {
        output.push_str(&format!("series-part={}\n", number(position)));
    }
    let mut start = 0_u64;
    for chapter in chapters {
        let end = start.saturating_add(chapter.duration_milliseconds);
        output.push_str("\n[CHAPTER]\nTIMEBASE=1/1000\n");
        output.push_str(&format!("START={start}\nEND={end}\n"));
        output.push_str(&format!("title={}\n", escape_ffmetadata(&chapter.title)));
        start = end;
    }
    output
}

fn escape_ffmetadata(value: &str) -> String {
    let mut escaped = String::with_capacity(value.len());
    for character in value.chars() {
        match character {
            '\\' | '=' | ';' | '#' => {
                escaped.push('\\');
                escaped.push(character);
            }
            '\n' => escaped.push_str("\\n"),
            '\r' => {}
            _ => escaped.push(character),
        }
    }
    escaped
}

fn auxiliary_path(output: &std::path::Path, suffix: &str) -> PathBuf {
    let name = output
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("audiobook");
    output.with_file_name(format!(".{name}.{suffix}"))
}

fn safe_file_component(value: &str) -> String {
    let sanitized: String = value
        .chars()
        .map(|character| {
            if character.is_control()
                || matches!(
                    character,
                    '/' | '\\' | ':' | '*' | '?' | '"' | '<' | '>' | '|'
                )
            {
                '_'
            } else {
                character
            }
        })
        .collect();
    let sanitized = sanitized.trim().trim_end_matches(['.', ' ']);
    if sanitized.is_empty() {
        "Chapter".to_owned()
    } else {
        sanitized.chars().take(120).collect()
    }
}

fn validate_request(request: &ExportRequest) -> Result<()> {
    if request.chapters.is_empty() {
        return Err(MediaError::Configuration(
            "at least one chapter is required".to_owned(),
        ));
    }
    if request.metadata.title.trim().is_empty() {
        return Err(MediaError::Configuration(
            "book title is required".to_owned(),
        ));
    }
    if request.bitrate_kbps < 32
        || request.sample_rate < 8_000
        || !(1..=2).contains(&request.channels)
    {
        return Err(MediaError::Configuration(
            "invalid audio encoding settings".to_owned(),
        ));
    }
    if request
        .chapters
        .iter()
        .any(|chapter| chapter.duration_milliseconds == 0 || chapter.title.trim().is_empty())
    {
        return Err(MediaError::Configuration(
            "every chapter requires a title and positive duration".to_owned(),
        ));
    }
    if let Some(settings) = request.loudness
        && (!(-70.0..=0.0).contains(&settings.target_lufs)
            || !(-20.0..=0.0).contains(&settings.true_peak_db)
            || !(1.0..=20.0).contains(&settings.loudness_range))
    {
        return Err(MediaError::Configuration(
            "invalid loudness targets".to_owned(),
        ));
    }
    if let Some(music) = &request.background_music
        && (music.path.as_os_str().is_empty()
            || !music.trim_start_seconds.is_finite()
            || music.trim_start_seconds < 0.0
            || music
                .trim_end_seconds
                .is_some_and(|end| !end.is_finite() || end <= music.trim_start_seconds)
            || !music.gain_db.is_finite()
            || music.fade_in_seconds < 0.0
            || music.fade_out_seconds < 0.0
            || !(0.000_001..=1.0).contains(&music.duck_threshold)
            || !(1.0..=20.0).contains(&music.duck_ratio))
    {
        return Err(MediaError::Configuration(
            "invalid background music settings".to_owned(),
        ));
    }
    Ok(())
}

fn number(value: f64) -> String {
    let mut value = format!("{value:.6}");
    while value.contains('.') && value.ends_with('0') {
        value.pop();
    }
    if value.ends_with('.') {
        value.push('0');
    }
    value
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ExportManifest {
    pub source_hashes: BTreeMap<String, String>,
    pub metadata: BookMetadata,
    pub chapter_timestamps_milliseconds: Vec<u64>,
    pub provider_provenance: Vec<serde_json::Value>,
    pub dictionary_revision: String,
    pub audio_settings: serde_json::Value,
    pub ffmpeg_build: String,
    pub usage_totals: serde_json::Value,
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use super::*;

    fn sidecars() -> SidecarPair {
        SidecarPair {
            ffmpeg: PathBuf::from("/app/ffmpeg"),
            ffprobe: PathBuf::from("/app/ffprobe"),
        }
    }

    fn request(format: ExportFormat) -> ExportRequest {
        ExportRequest::audiobook_defaults(
            vec![
                ChapterAudio {
                    title: "One".to_owned(),
                    path: PathBuf::from("/library/one.flac"),
                    duration_milliseconds: 10_000,
                },
                ChapterAudio {
                    title: "Two; #2".to_owned(),
                    path: PathBuf::from("/library/two.flac"),
                    duration_milliseconds: 12_500,
                },
            ],
            PathBuf::from(format!("/exports/book.{}", format.extension())),
            format,
            BookMetadata {
                title: "Book".to_owned(),
                authors: vec!["Author".to_owned()],
                series: Some("Series".to_owned()),
                series_position: Some(2.0),
                ..BookMetadata::default()
            },
        )
    }

    fn measurement() -> LoudnessMeasurement {
        LoudnessMeasurement {
            input_i: -27.0,
            input_tp: -8.0,
            input_lra: 4.0,
            input_thresh: -38.0,
            target_offset: 0.1,
        }
    }

    #[test]
    fn ffmetadata_has_accumulated_chapter_markers_and_escaping() {
        let request = request(ExportFormat::Mp3);
        let data = ffmetadata(&request.metadata, &request.chapters);
        assert!(data.contains("START=0\nEND=10000"));
        assert!(data.contains("START=10000\nEND=22500"));
        assert!(data.contains("title=Two\\; \\#2"));
    }

    #[test]
    fn mp3_plan_uses_lame_id3_cover_and_chapters() {
        let mut request = request(ExportFormat::Mp3);
        request.cover_art = Some(PathBuf::from("/library/cover.jpg"));
        let plan = ExportPlanner::new(sidecars())
            .render(&request, &[measurement()])
            .unwrap();
        let args = &plan.invocations[0].arguments;
        assert!(args.windows(2).any(|pair| pair == ["-c:a", "libmp3lame"]));
        assert!(args.windows(2).any(|pair| pair == ["-map_chapters", "3"]));
        assert!(args.iter().any(|arg| arg == "attached_pic"));
        assert_eq!(plan.auxiliary_files.len(), 1);
    }

    #[test]
    fn m4b_plan_sets_audiobook_kind_and_faststart() {
        let plan = ExportPlanner::new(sidecars())
            .render(&request(ExportFormat::M4b), &[measurement()])
            .unwrap();
        let args = &plan.invocations[0].arguments;
        assert!(
            args.iter()
                .any(|argument| argument == "+faststart+use_metadata_tags")
        );
        assert!(args.iter().any(|argument| argument == "media_type=2"));
        assert!(args.iter().any(|argument| argument == "stik=2"));
    }

    #[test]
    fn wav_plan_enables_rf64_automatically() {
        let plan = ExportPlanner::new(sidecars())
            .render(&request(ExportFormat::Wav), &[measurement()])
            .unwrap();
        assert!(
            plan.invocations[0]
                .arguments
                .windows(2)
                .any(|pair| pair == ["-rf64", "auto"])
        );
    }

    #[test]
    fn production_render_requires_two_pass_measurement() {
        assert!(matches!(
            ExportPlanner::new(sidecars()).render(&request(ExportFormat::M4a), &[]),
            Err(MediaError::LoudnessMeasurementRequired)
        ));
    }

    #[test]
    fn parses_ffmpeg_loudnorm_json_from_noisy_stderr() {
        let stderr = r#"frame=12
{
  "input_i": "-27.00",
  "input_tp": "-8.00",
  "input_lra": "4.00",
  "input_thresh": "-38.00",
  "target_offset": "0.10"
}
done"#;
        assert_eq!(parse_loudness_measurement(stderr).unwrap(), measurement());
    }

    #[test]
    fn music_plan_loops_ducks_and_normalizes_after_mixing() {
        let mut request = request(ExportFormat::M4a);
        request.background_music = Some(BackgroundMusic {
            path: PathBuf::from("/music/bed.flac"),
            ..BackgroundMusic::default()
        });
        let plan = ExportPlanner::new(sidecars())
            .render(&request, &[measurement()])
            .unwrap();
        let args = &plan.invocations[0].arguments;
        assert!(args.windows(2).any(|pair| pair == ["-stream_loop", "-1"]));
        let filter = args
            .windows(2)
            .find(|pair| pair[0] == "-filter_complex")
            .map(|pair| pair[1].as_str())
            .unwrap();
        assert!(filter.contains("sidechaincompress"));
        assert!(filter.find("amix").unwrap() < filter.find("loudnorm").unwrap());
    }

    #[test]
    fn split_outputs_sanitize_chapter_file_names() {
        let mut request = request(ExportFormat::Mp3);
        request.output = PathBuf::from("/exports/book");
        request.split_per_chapter = true;
        request.chapters[0].title = "Bad/name?".to_owned();
        let plan = ExportPlanner::new(sidecars())
            .render(&request, &[measurement(), measurement()])
            .unwrap();
        assert_eq!(
            plan.outputs[0],
            Path::new("/exports/book/001 - Bad_name_.mp3")
        );
    }
}
