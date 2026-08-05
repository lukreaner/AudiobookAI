use std::collections::BTreeMap;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::{
    ArtifactId, BookMetadata, ChapterId, ExportProfileId, FileFingerprint, ProjectId,
    ProviderProfileId, Validate, ValidationIssue, VoiceProfileId,
};

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ExportFormat {
    Mp3,
    Wav,
    M4a,
    M4b,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ExportLayout {
    SingleFile,
    PerChapter,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct AudioEncodingSettings {
    pub sample_rate_hz: u32,
    pub channels: u8,
    pub bitrate_kbps: Option<u32>,
    pub target_lufs: f32,
    pub true_peak_db: f32,
}

impl Default for AudioEncodingSettings {
    fn default() -> Self {
        Self {
            sample_rate_hz: 48_000,
            channels: 1,
            bitrate_kbps: Some(128),
            target_lufs: -19.0,
            true_peak_db: -3.0,
        }
    }
}

impl Validate for AudioEncodingSettings {
    fn validation_issues(&self) -> Vec<ValidationIssue> {
        let mut issues = Vec::new();
        if !(8_000..=192_000).contains(&self.sample_rate_hz) {
            issues.push(ValidationIssue::new(
                "sample_rate_hz",
                "out_of_range",
                "sample rate must be between 8 kHz and 192 kHz",
            ));
        }
        if !(1..=2).contains(&self.channels) {
            issues.push(ValidationIssue::new(
                "channels",
                "out_of_range",
                "output must use one or two channels",
            ));
        }
        if !self.target_lufs.is_finite() || !(-40.0..=-5.0).contains(&self.target_lufs) {
            issues.push(ValidationIssue::new(
                "target_lufs",
                "out_of_range",
                "target loudness must be between -40 and -5 LUFS",
            ));
        }
        if !self.true_peak_db.is_finite() || !(-12.0..=0.0).contains(&self.true_peak_db) {
            issues.push(ValidationIssue::new(
                "true_peak_db",
                "out_of_range",
                "true peak must be between -12 and 0 dBTP",
            ));
        }
        issues
    }
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct BackgroundMusicSettings {
    pub artifact_id: ArtifactId,
    pub user_owned_confirmed: bool,
    pub gain_db: f32,
    pub loop_audio: bool,
    pub trim_start_ms: u64,
    pub trim_end_ms: Option<u64>,
    pub fade_in_ms: u64,
    pub fade_out_ms: u64,
    pub ducking: Option<DuckingSettings>,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct DuckingSettings {
    pub attenuation_db: f32,
    pub attack_ms: u32,
    pub release_ms: u32,
    pub threshold_db: f32,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct ExportProfile {
    pub id: ExportProfileId,
    pub project_id: ProjectId,
    pub name: String,
    pub format: ExportFormat,
    pub layout: ExportLayout,
    pub output_directory: String,
    pub filename_template: String,
    pub audio: AudioEncodingSettings,
    pub background_music: Option<BackgroundMusicSettings>,
    pub embed_cover: bool,
    pub embed_chapters: bool,
    pub write_sidecar_manifest: bool,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

impl Validate for ExportProfile {
    fn validation_issues(&self) -> Vec<ValidationIssue> {
        let mut issues = self.audio.validation_issues();
        if self.name.trim().is_empty() {
            issues.push(ValidationIssue::new(
                "name",
                "required",
                "export profile name must not be empty",
            ));
        }
        if self.output_directory.trim().is_empty() {
            issues.push(ValidationIssue::new(
                "output_directory",
                "required",
                "output directory must not be empty",
            ));
        }
        if self.filename_template.trim().is_empty() {
            issues.push(ValidationIssue::new(
                "filename_template",
                "required",
                "filename template must not be empty",
            ));
        }
        if self.layout == ExportLayout::SingleFile
            && self.embed_chapters
            && !matches!(
                self.format,
                ExportFormat::Mp3 | ExportFormat::M4a | ExportFormat::M4b
            )
        {
            issues.push(ValidationIssue::new(
                "embed_chapters",
                "unsupported",
                "embedded chapters are supported for MP3, M4A, and M4B single-file exports",
            ));
        }
        if self
            .background_music
            .as_ref()
            .is_some_and(|music| !music.user_owned_confirmed)
        {
            issues.push(ValidationIssue::new(
                "background_music.user_owned_confirmed",
                "consent_required",
                "background music ownership must be confirmed",
            ));
        }
        issues
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct ChapterMarker {
    pub chapter_id: ChapterId,
    pub title: String,
    pub start_ms: u64,
    pub end_ms: u64,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct VoiceProvenance {
    pub provider_profile_id: ProviderProfileId,
    pub provider_family: String,
    pub provider_version: Option<String>,
    pub model: Option<String>,
    pub voice_profile_id: VoiceProfileId,
    pub voice_name: String,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct ExportManifest {
    pub schema_version: u32,
    pub project_id: ProjectId,
    pub created_at: DateTime<Utc>,
    pub source: FileFingerprint,
    pub metadata: BookMetadata,
    pub output_format: ExportFormat,
    pub output_files: Vec<String>,
    pub chapter_markers: Vec<ChapterMarker>,
    pub voice_provenance: Vec<VoiceProvenance>,
    pub dictionary_revisions: BTreeMap<String, u64>,
    pub audio: AudioEncodingSettings,
    pub ffmpeg_version: String,
    pub ffmpeg_build_fingerprint: String,
    pub usage_event_ids: Vec<crate::UsageEventId>,
    #[serde(default)]
    pub attributes: BTreeMap<String, serde_json::Value>,
}
