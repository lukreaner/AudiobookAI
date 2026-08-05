//! Retailer distribution metadata, package registration, and deterministic quality reports.

use std::{
    collections::{BTreeMap, BTreeSet},
    path::{Path as FilePath, PathBuf},
    process::Stdio,
    str::FromStr,
    sync::Arc,
};

use audiobookai_core::{
    Artifact, ArtifactId, ArtifactKind, DistributionMetadata, DistributionPolicyRef,
    DistributionSegmentEvidence, DistributionTarget, ExportPackage, ExportPackageId, Job, JobId,
    JobKind, JobState, ProductionSegmentSource, Project, ProjectId, ProofingPlanStatus,
    QualityFinding, QualityFindingStatus, QualityReport, QualityReportId,
};
use audiobookai_media::{
    DecodeValidity, FfmpegInvocation, FileQcExpectations, MediaQcPlanner, Mp3CbrStatus,
    Mp3QcAnalysis, Mp3QcExpectations, PcmQcAnalysis, PcmQcPolicy, QcFinding, QcFindingCode,
    QcRangeF64, QcRangeU64, QcSeverity, SidecarPair, SidecarResolver, StreamingPcmQcAnalyzer,
    analyze_ffprobe_output, analyze_mp3, decode_f32le, parse_ffprobe_metadata,
};
use axum::{
    Json, Router,
    body::Body,
    extract::{Path, State},
    http::{StatusCode, header},
    response::Response,
    routing::get,
};
use chrono::{DateTime, NaiveDate, Utc};
use serde::{Deserialize, Serialize};
use sqlx::Row;
use tokio::{io::AsyncReadExt, process::Command};
use uuid::Uuid;

use crate::{AppState, ServiceError};

const ANALYZER_VERSION: &str = "audiobookai-distribution-qc/1";
const MAX_UPLOAD_ARTIFACTS: usize = 512;
const MAX_REVIEW_ARTIFACTS: usize = 64;
const MAX_METADATA_TEXT_BYTES: usize = 100_000;
const MAX_IN_MEMORY_MP3_SCAN_BYTES: u64 = 256 * 1024 * 1024;

pub(crate) fn routes() -> Router<Arc<AppState>> {
    Router::new()
        .route("/api/v1/distribution/policies", get(list_policies))
        .route(
            "/api/v1/projects/{project_id}/distribution/metadata",
            get(get_metadata).put(put_metadata),
        )
        .route(
            "/api/v1/projects/{project_id}/distribution/packages",
            get(list_packages).post(create_package),
        )
        .route(
            "/api/v1/distribution/packages/{package_id}",
            get(get_package),
        )
        .route(
            "/api/v1/distribution/packages/{package_id}/reports",
            get(list_reports).post(rerun_quality_control),
        )
        .route("/api/v1/distribution/reports/{report_id}", get(get_report))
        .route(
            "/api/v1/distribution/reports/{report_id}/html",
            get(get_report_html),
        )
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct DistributionPolicyView {
    target: DistributionTarget,
    policy_version: String,
    effective_date: NaiveDate,
    source_urls: Vec<String>,
    display_name: String,
    rules: Vec<PolicyRuleView>,
}

impl DistributionPolicyView {
    fn policy_ref(&self) -> DistributionPolicyRef {
        DistributionPolicyRef {
            target: self.target,
            policy_version: self.policy_version.clone(),
            effective_date: self.effective_date,
            source_urls: self.source_urls.clone(),
        }
    }
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct PolicyRuleView {
    code: &'static str,
    level: PolicyRuleLevel,
    automated: bool,
    expected: serde_json::Value,
    description: &'static str,
}

#[derive(Clone, Copy, Debug, Serialize)]
#[serde(rename_all = "snake_case")]
enum PolicyRuleLevel {
    Required,
    Recommended,
    ManualGate,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct PolicyListView {
    items: Vec<DistributionPolicyView>,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct DistributionMetadataView {
    revision: u64,
    updated_at: Option<DateTime<Utc>>,
    metadata: DistributionMetadata,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct PutDistributionMetadataInput {
    expected_revision: u64,
    metadata: DistributionMetadata,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct CreatePackageInput {
    target: DistributionTarget,
    upload_artifact_ids: Vec<Uuid>,
    #[serde(default)]
    review_artifact_ids: Vec<Uuid>,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct PackageView {
    package: ExportPackage,
    latest_report: Option<QualityReport>,
    latest_report_current: bool,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct PackageListView {
    items: Vec<PackageView>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct ReportListView {
    items: Vec<QualityReport>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct QualityRunView {
    package: ExportPackage,
    report: QualityReport,
}

#[derive(Clone, Debug)]
struct ArtifactRecord {
    artifact: Artifact,
    project_id: ProjectId,
    job_id: Option<JobId>,
}

#[derive(Clone, Copy, Debug)]
#[allow(clippy::struct_excessive_bools)]
struct PolicyBehavior {
    target: DistributionTarget,
    allowed_extensions: &'static [&'static str],
    max_duration_milliseconds: Option<u64>,
    require_cbr: bool,
    prefer_cbr: bool,
    require_consistent_channels: bool,
    rms_dbfs: Option<QcRangeF64>,
    rms_is_recommendation: bool,
    max_sample_peak_dbfs: Option<f64>,
    boundary_silence_is_recommendation: bool,
    leading_silence_milliseconds: Option<QcRangeU64>,
    trailing_silence_milliseconds: Option<QcRangeU64>,
}

async fn list_policies() -> Json<PolicyListView> {
    Json(PolicyListView {
        items: [
            DistributionTarget::GenericM4b,
            DistributionTarget::Acx,
            DistributionTarget::SpotifyForAuthors,
            DistributionTarget::GooglePlay,
        ]
        .into_iter()
        .map(policy_view)
        .collect(),
    })
}

#[allow(clippy::too_many_lines)]
fn policy_view(target: DistributionTarget) -> DistributionPolicyView {
    match target {
        DistributionTarget::GenericM4b => DistributionPolicyView {
            target,
            policy_version: "audiobookai-generic-m4b-1".to_owned(),
            effective_date: date(2026, 8, 5),
            source_urls: vec!["https://www.w3.org/TR/audiobooks/".to_owned()],
            display_name: "Generic M4B".to_owned(),
            rules: vec![
                rule(
                    "generic_m4b_container",
                    PolicyRuleLevel::Required,
                    true,
                    serde_json::json!({"extensions": ["m4b"], "minimumBitrateBps": 64_000}),
                    "One decodable AAC audiobook at 64 kbps or higher in an M4B-compatible MP4 container.",
                ),
                rule(
                    "generic_chapter_navigation",
                    PolicyRuleLevel::Recommended,
                    false,
                    serde_json::json!("review chapter navigation before delivery"),
                    "Chapter navigation and metadata require a listening review.",
                ),
            ],
        },
        DistributionTarget::Acx => DistributionPolicyView {
            target,
            policy_version: "acx-2026-04-15".to_owned(),
            effective_date: date(2026, 4, 15),
            source_urls: vec![
                "https://help.acx.com/s/article/what-are-the-acx-audio-submission-requirements"
                    .to_owned(),
                "https://help.acx.com/s/article/cover-art-requirements".to_owned(),
            ],
            display_name: "ACX".to_owned(),
            rules: vec![
                rule(
                    "acx_audio_format",
                    PolicyRuleLevel::Required,
                    true,
                    serde_json::json!({
                        "container": "mp3",
                        "sampleRateHz": 44100,
                        "minimumBitrateBps": 192_000,
                        "constantBitrate": true,
                        "maximumFileDurationMilliseconds": 7_200_000
                    }),
                    "Every audio file must be a 44.1 kHz MP3 at 192 kbps or higher CBR and no longer than 120 minutes.",
                ),
                rule(
                    "acx_audio_levels",
                    PolicyRuleLevel::Required,
                    true,
                    serde_json::json!({
                        "rmsDbfs": {"min": -23.0, "max": -18.0},
                        "samplePeakDbfsStrictlyBelow": -3.0
                    }),
                    "Files must satisfy ACX RMS and peak limits.",
                ),
                rule(
                    "acx_boundary_room_tone_manual_review",
                    PolicyRuleLevel::ManualGate,
                    false,
                    serde_json::json!({"minimumMilliseconds": 1000, "maximumMilliseconds": 5000}),
                    "Room tone must not exceed five seconds; ACX recommends one to five seconds at each file boundary.",
                ),
                rule(
                    "acx_noise_floor_manual_review",
                    PolicyRuleLevel::ManualGate,
                    false,
                    serde_json::json!({"noiseFloorDbfsStrictlyBelow": -60.0}),
                    "Noise floor must be reviewed with a method that can distinguish room tone from narration.",
                ),
                rule(
                    "acx_cover_manual_review",
                    PolicyRuleLevel::ManualGate,
                    false,
                    serde_json::json!({"square": true, "rgb": true, "minimumPixels": 2400}),
                    "Cover dimensions, color space, and visual content require review.",
                ),
                rule(
                    "acx_human_narration_authorization",
                    PolicyRuleLevel::ManualGate,
                    false,
                    serde_json::json!("external ACX authorization reference and date"),
                    "Synthetic narration is blocked unless ACX has separately authorized it.",
                ),
                rule(
                    "acx_structure_and_metadata",
                    PolicyRuleLevel::Required,
                    false,
                    serde_json::json!("credits, one section per file, sample, cover, rights"),
                    "Credits, section files, sample, cover art, and rights metadata must be complete.",
                ),
                rule(
                    "acx_credits_and_sample_manual_review",
                    PolicyRuleLevel::ManualGate,
                    false,
                    serde_json::json!("listen to credit wording, file order, and sample content"),
                    "Credit wording and placement plus sample content require a listening review.",
                ),
                rule(
                    "acx_file_structure_manual_review",
                    PolicyRuleLevel::ManualGate,
                    false,
                    serde_json::json!("one labeled chapter or section per correctly ordered file"),
                    "Section boundaries, spoken headers, filenames, and upload order require review.",
                ),
            ],
        },
        DistributionTarget::SpotifyForAuthors => DistributionPolicyView {
            target,
            policy_version: "spotify-for-authors-2024-11".to_owned(),
            effective_date: date(2024, 11, 1),
            source_urls: vec![
                "https://support.spotify.com/by-en/authors/article/uploading-audiobooks/"
                    .to_owned(),
                "https://support.spotifycdn.com/pdf/SFA%20Metadata_Asset%20Guide_2024.pdf"
                    .to_owned(),
                "https://support.spotify.com/ws/authors/article/digital-voice-narration/"
                    .to_owned(),
            ],
            display_name: "Spotify for Authors".to_owned(),
            rules: vec![
                rule(
                    "spotify_audio_format",
                    PolicyRuleLevel::Required,
                    true,
                    serde_json::json!({
                        "extensions": ["mp3", "wav", "flac"],
                        "maximumFileDurationMilliseconds": 7_200_000
                    }),
                    "Audio must use MP3, WAV, or FLAC and each standalone file may be at most 120 minutes.",
                ),
                rule(
                    "spotify_audio_recommendations",
                    PolicyRuleLevel::Recommended,
                    true,
                    serde_json::json!({
                        "sampleRateHz": 44100,
                        "mp3MinimumBitrateBps": 192_000,
                        "mp3ConstantBitrate": true,
                        "consistentChannels": true,
                        "rmsDbfs": {"min": -24.0, "max": -14.0},
                        "noiseFloorDbfsMaximum": -60.0,
                        "leadingSilenceMilliseconds": {"min": 500, "max": 1000},
                        "trailingSilenceMilliseconds": {"min": 1000, "max": 5000}
                    }),
                    "Spotify recommends 44.1 kHz, 192 kbps or higher CBR MP3, and its documented level and silence ranges.",
                ),
                rule(
                    "spotify_metadata",
                    PolicyRuleLevel::Required,
                    false,
                    serde_json::json!([
                        "publisher",
                        "language",
                        "opening credits",
                        "closing credits"
                    ]),
                    "Publisher, language, and spoken credits must be supplied.",
                ),
                rule(
                    "spotify_retail_sample",
                    PolicyRuleLevel::Recommended,
                    false,
                    serde_json::json!(
                        "optional custom sample up to five minutes; Spotify may generate one when omitted"
                    ),
                    "A custom retail sample is optional and may be generated by Spotify when omitted.",
                ),
                rule(
                    "spotify_cover_and_credits_manual_review",
                    PolicyRuleLevel::ManualGate,
                    false,
                    serde_json::json!("square cover plus correctly worded and ordered credits"),
                    "Cover presentation and spoken-credit wording and placement require review.",
                ),
                rule(
                    "spotify_file_structure_manual_review",
                    PolicyRuleLevel::ManualGate,
                    false,
                    serde_json::json!("one labeled chapter or section per correctly ordered file"),
                    "Section boundaries, spoken headings, and upload order require review.",
                ),
                rule(
                    "spotify_digital_voice_disclosure",
                    PolicyRuleLevel::ManualGate,
                    false,
                    serde_json::json!("digital voice disclosed during upload"),
                    "The uploader must disclose digital voice narration.",
                ),
            ],
        },
        DistributionTarget::GooglePlay => DistributionPolicyView {
            target,
            policy_version: "google-play-2026-08-05-snapshot".to_owned(),
            effective_date: date(2026, 8, 5),
            source_urls: vec![
                "https://support.google.com/books/partner/answer/3424254?hl=en".to_owned(),
                "https://support.google.com/books/partner/answer/7504302?hl=en".to_owned(),
            ],
            display_name: "Google Play Books".to_owned(),
            rules: vec![
                rule(
                    "google_audio_format",
                    PolicyRuleLevel::Required,
                    true,
                    serde_json::json!({
                        "extensions": ["mp3", "m4a", "aac", "flac", "wav"],
                        "losslessMinimumSampleRateHz": 44100,
                        "losslessBitDepth": 16,
                        "constantBitratePreferred": true,
                        "mp3MinimumBitrateBps": {"mono": 128_000, "stereo": 256_000},
                        "m4aMinimumBitrateBps": {
                            "monoExclusive": 128_000,
                            "stereoInclusive": 256_000
                        }
                    }),
                    "Audio must use a supported format and satisfy the channel-dependent bitrate or lossless sample-rate requirements.",
                ),
                rule(
                    "google_total_duration",
                    PolicyRuleLevel::Required,
                    true,
                    serde_json::json!({
                        "minimumMilliseconds": 300_000,
                        "maximumMilliseconds": 360_000_000_u64
                    }),
                    "The complete audiobook must be between five minutes and one hundred hours.",
                ),
                rule(
                    "google_metadata_and_packaging",
                    PolicyRuleLevel::ManualGate,
                    false,
                    serde_json::json!([
                        "identifier type",
                        "identifier",
                        "abridged status",
                        "delivery filenames",
                        "distribution rights"
                    ]),
                    "Identifier validity, filename packaging, abridged status, and distribution rights must be confirmed.",
                ),
                rule(
                    "google_preview",
                    PolicyRuleLevel::Required,
                    false,
                    serde_json::json!("do not include a preview in uploaded customer audio"),
                    "Google automated audiobook delivery does not accept a separate preview file.",
                ),
                rule(
                    "google_cover_and_lossless_manual_review",
                    PolicyRuleLevel::ManualGate,
                    false,
                    serde_json::json!("cover presentation and 16-bit FLAC encoding"),
                    "Cover presentation and FLAC bit depth require independent review.",
                ),
            ],
        },
    }
}

fn rule(
    code: &'static str,
    level: PolicyRuleLevel,
    automated: bool,
    expected: serde_json::Value,
    description: &'static str,
) -> PolicyRuleView {
    PolicyRuleView {
        code,
        level,
        automated,
        expected,
        description,
    }
}

const fn behavior(target: DistributionTarget) -> PolicyBehavior {
    match target {
        DistributionTarget::GenericM4b => PolicyBehavior {
            target,
            allowed_extensions: &["m4b"],
            max_duration_milliseconds: None,
            require_cbr: false,
            prefer_cbr: false,
            require_consistent_channels: false,
            rms_dbfs: None,
            rms_is_recommendation: false,
            max_sample_peak_dbfs: Some(0.0),
            boundary_silence_is_recommendation: false,
            leading_silence_milliseconds: None,
            trailing_silence_milliseconds: None,
        },
        DistributionTarget::Acx => PolicyBehavior {
            target,
            allowed_extensions: &["mp3"],
            max_duration_milliseconds: Some(7_200_000),
            require_cbr: true,
            prefer_cbr: false,
            require_consistent_channels: true,
            rms_dbfs: Some(QcRangeF64 {
                min: Some(-23.0),
                max: Some(-18.0),
            }),
            rms_is_recommendation: false,
            // The media primitive uses an inclusive ceiling, so a tiny epsilon implements ACX's
            // documented "below -3 dB" boundary.
            max_sample_peak_dbfs: Some(-3.000_001),
            boundary_silence_is_recommendation: false,
            leading_silence_milliseconds: None,
            trailing_silence_milliseconds: None,
        },
        DistributionTarget::SpotifyForAuthors => PolicyBehavior {
            target,
            allowed_extensions: &["mp3", "wav", "flac"],
            max_duration_milliseconds: Some(7_200_000),
            require_cbr: false,
            prefer_cbr: true,
            require_consistent_channels: true,
            rms_dbfs: Some(QcRangeF64 {
                min: Some(-24.0),
                max: Some(-14.0),
            }),
            rms_is_recommendation: true,
            max_sample_peak_dbfs: None,
            boundary_silence_is_recommendation: true,
            leading_silence_milliseconds: Some(QcRangeU64 {
                min: Some(500),
                max: Some(1_000),
            }),
            trailing_silence_milliseconds: Some(QcRangeU64 {
                min: Some(1_000),
                max: Some(5_000),
            }),
        },
        DistributionTarget::GooglePlay => PolicyBehavior {
            target,
            allowed_extensions: &["mp3", "m4a", "aac", "flac", "wav"],
            max_duration_milliseconds: None,
            require_cbr: false,
            prefer_cbr: true,
            require_consistent_channels: false,
            rms_dbfs: None,
            rms_is_recommendation: false,
            max_sample_peak_dbfs: Some(0.0),
            boundary_silence_is_recommendation: false,
            leading_silence_milliseconds: None,
            trailing_silence_milliseconds: None,
        },
    }
}

fn date(year: i32, month: u32, day: u32) -> NaiveDate {
    NaiveDate::from_ymd_opt(year, month, day).expect("hard-coded policy date must be valid")
}

async fn get_metadata(
    State(state): State<Arc<AppState>>,
    Path(project_id): Path<Uuid>,
) -> Result<Json<DistributionMetadataView>, ServiceError> {
    let project = load_project(&state, project_id).await?;
    Ok(Json(load_metadata_view(&state, &project).await?))
}

async fn put_metadata(
    State(state): State<Arc<AppState>>,
    Path(project_id): Path<Uuid>,
    Json(input): Json<PutDistributionMetadataInput>,
) -> Result<Json<DistributionMetadataView>, ServiceError> {
    let project_lock = state.character_lifecycle_lock(project_id).await;
    let _project_guard = project_lock.lock().await;
    let project = load_project(&state, project_id).await?;
    let metadata = normalize_metadata(input.metadata);
    validate_metadata(&state, project.id, &metadata).await?;
    let now = Utc::now();
    let next_revision = input.expected_revision.checked_add(1).ok_or_else(|| {
        ServiceError::Conflict("distribution metadata revision is exhausted".to_owned())
    })?;
    let payload = serde_json::to_string(&metadata).map_err(internal_error)?;
    let result = if input.expected_revision == 0 {
        sqlx::query(
            "INSERT INTO distribution_metadata (project_id, revision, updated_at, payload) \
             VALUES (?, ?, ?, ?) ON CONFLICT(project_id) DO NOTHING",
        )
        .bind(project.id.to_string())
        .bind(i64::try_from(next_revision).unwrap_or(i64::MAX))
        .bind(now.to_rfc3339())
        .bind(&payload)
        .execute(state.database.pool())
        .await
        .map_err(storage_error)?
    } else {
        sqlx::query(
            "UPDATE distribution_metadata SET revision = ?, updated_at = ?, payload = ? \
             WHERE project_id = ? AND revision = ?",
        )
        .bind(i64::try_from(next_revision).unwrap_or(i64::MAX))
        .bind(now.to_rfc3339())
        .bind(&payload)
        .bind(project.id.to_string())
        .bind(i64::try_from(input.expected_revision).unwrap_or(i64::MAX))
        .execute(state.database.pool())
        .await
        .map_err(storage_error)?
    };
    if result.rows_affected() != 1 {
        let actual_revision = sqlx::query_scalar::<_, i64>(
            "SELECT revision FROM distribution_metadata WHERE project_id = ?",
        )
        .bind(project.id.to_string())
        .fetch_optional(state.database.pool())
        .await
        .map_err(storage_error)?
        .map_or(Ok(0), u64::try_from)
        .map_err(invalid_stored_data)?;
        return Err(stale_metadata(actual_revision));
    }
    Ok(Json(DistributionMetadataView {
        revision: next_revision,
        updated_at: Some(now),
        metadata,
    }))
}

async fn list_packages(
    State(state): State<Arc<AppState>>,
    Path(project_id): Path<Uuid>,
) -> Result<Json<PackageListView>, ServiceError> {
    let _model_lifecycle_guard = state.model_lifecycle.lock().await;
    let project_lock = state.character_lifecycle_lock(project_id).await;
    let _project_guard = project_lock.lock().await;
    let project = load_project(&state, project_id).await?;
    let metadata = load_metadata_view(&state, &project).await?;
    let packages = load_packages_for_project(&state, project.id).await?;
    let mut items = Vec::with_capacity(packages.len());
    for package in packages {
        let latest_report = load_latest_report(&state, package.id).await?;
        let latest_report_current = latest_report_is_current_locked(
            &state,
            &package,
            &project,
            &metadata,
            latest_report.as_ref(),
        )
        .await?;
        items.push(PackageView {
            package,
            latest_report,
            latest_report_current,
        });
    }
    Ok(Json(PackageListView { items }))
}

#[allow(clippy::too_many_lines)]
async fn create_package(
    State(state): State<Arc<AppState>>,
    Path(project_id): Path<Uuid>,
    Json(input): Json<CreatePackageInput>,
) -> Result<(StatusCode, Json<PackageView>), ServiceError> {
    let _model_lifecycle_guard = state.model_lifecycle.lock().await;
    let project_lock = state.character_lifecycle_lock(project_id).await;
    let _project_guard = project_lock.lock().await;
    let project = load_project(&state, project_id).await?;
    validate_artifact_id_list(
        &input.upload_artifact_ids,
        MAX_UPLOAD_ARTIFACTS,
        "upload artifacts",
        false,
    )?;
    validate_artifact_id_list(
        &input.review_artifact_ids,
        MAX_REVIEW_ARTIFACTS,
        "review artifacts",
        true,
    )?;
    let upload_set = input
        .upload_artifact_ids
        .iter()
        .copied()
        .collect::<BTreeSet<_>>();
    if input
        .review_artifact_ids
        .iter()
        .any(|id| upload_set.contains(id))
    {
        return Err(ServiceError::InvalidRequest(
            "an artifact may not be both an upload and a review artifact".to_owned(),
        ));
    }

    let mut uploads = Vec::with_capacity(input.upload_artifact_ids.len());
    for id in &input.upload_artifact_ids {
        let record = load_artifact_record(&state, ArtifactId::from_uuid(*id)).await?;
        if record.project_id != project.id || record.artifact.kind != ArtifactKind::Export {
            return Err(ServiceError::InvalidRequest(format!(
                "upload artifact {id} is not an export owned by this project"
            )));
        }
        uploads.push(record);
    }
    let job_id = uploads
        .first()
        .and_then(|record| record.job_id)
        .ok_or_else(|| {
            ServiceError::InvalidRequest(
                "distribution upload artifacts must belong to an export job".to_owned(),
            )
        })?;
    if uploads.iter().any(|record| record.job_id != Some(job_id)) {
        return Err(ServiceError::InvalidRequest(
            "all upload artifacts in a package must come from the same export job".to_owned(),
        ));
    }
    load_completed_distribution_job(&state, job_id, project.id).await?;
    let export_manifest = load_current_export_manifest(&state, job_id, project.id).await?;
    let canonical_ids = crate::conversion::canonical_export_artifact_ids(&state, job_id).await?;
    require_complete_export_set(&upload_set, &canonical_ids)?;
    let mut uploads_by_id = uploads
        .into_iter()
        .map(|record| (record.artifact.id, record))
        .collect::<BTreeMap<_, _>>();
    let uploads = canonical_ids
        .iter()
        .filter_map(|id| uploads_by_id.remove(id))
        .collect::<Vec<_>>();
    let output_directory = common_output_directory(&uploads)?;

    let mut reviews = Vec::with_capacity(input.review_artifact_ids.len());
    for id in &input.review_artifact_ids {
        let record = load_artifact_record(&state, ArtifactId::from_uuid(*id)).await?;
        if record.project_id != project.id
            || !matches!(
                record.artifact.kind,
                ArtifactKind::Cover
                    | ArtifactKind::Preview
                    | ArtifactKind::Export
                    | ArtifactKind::ExportManifest
            )
            || (record.artifact.kind == ArtifactKind::ExportManifest
                && record.job_id != Some(job_id))
        {
            return Err(ServiceError::InvalidRequest(format!(
                "review artifact {id} is not a supported artifact owned by this project"
            )));
        }
        reviews.push(record);
    }
    if !reviews
        .iter()
        .any(|record| record.artifact.id == export_manifest.artifact.id)
    {
        reviews.push(export_manifest);
    }
    if reviews.len() > MAX_REVIEW_ARTIFACTS {
        return Err(ServiceError::InvalidRequest(format!(
            "review artifacts may contain at most {MAX_REVIEW_ARTIFACTS} entries including the export manifest"
        )));
    }

    let package = ExportPackage {
        id: ExportPackageId::new(),
        project_id: project.id,
        job_id,
        target: input.target,
        output_directory,
        upload_artifact_ids: uploads.iter().map(|record| record.artifact.id).collect(),
        review_artifact_ids: reviews.iter().map(|record| record.artifact.id).collect(),
        quality_report_id: None,
        created_at: Utc::now(),
    };
    let result = sqlx::query(
        "INSERT INTO export_packages \
         (id, project_id, job_id, target, output_directory, created_at, payload) \
         VALUES (?, ?, ?, ?, ?, ?, ?) ON CONFLICT(job_id, target) DO NOTHING",
    )
    .bind(package.id.to_string())
    .bind(package.project_id.to_string())
    .bind(package.job_id.to_string())
    .bind(target_text(package.target))
    .bind(&package.output_directory)
    .bind(package.created_at.to_rfc3339())
    .bind(serde_json::to_string(&package).map_err(internal_error)?)
    .execute(state.database.pool())
    .await
    .map_err(storage_error)?;
    if result.rows_affected() == 0 {
        let existing = load_package_for_job(&state, job_id, input.target)
            .await?
            .ok_or_else(|| {
                ServiceError::Conflict(
                    "distribution package creation conflicted with another request".to_owned(),
                )
            })?;
        if !same_package_inputs(&existing, &package) {
            return Err(ServiceError::Conflict(
                "this export job is already registered as a different distribution package"
                    .to_owned(),
            ));
        }
        let latest_report = load_latest_report(&state, existing.id).await?;
        let metadata = load_metadata_view(&state, &project).await?;
        let latest_report_current = latest_report_is_current_locked(
            &state,
            &existing,
            &project,
            &metadata,
            latest_report.as_ref(),
        )
        .await?;
        return Ok((
            StatusCode::OK,
            Json(PackageView {
                package: existing,
                latest_report,
                latest_report_current,
            }),
        ));
    }
    Ok((
        StatusCode::CREATED,
        Json(PackageView {
            package,
            latest_report: None,
            latest_report_current: false,
        }),
    ))
}

async fn get_package(
    State(state): State<Arc<AppState>>,
    Path(package_id): Path<Uuid>,
) -> Result<Json<PackageView>, ServiceError> {
    let package = load_package(&state, ExportPackageId::from_uuid(package_id)).await?;
    let _model_lifecycle_guard = state.model_lifecycle.lock().await;
    let project_lock = state
        .character_lifecycle_lock(package.project_id.as_uuid())
        .await;
    let _project_guard = project_lock.lock().await;
    let project = load_project(&state, package.project_id.as_uuid()).await?;
    let metadata = load_metadata_view(&state, &project).await?;
    let latest_report = load_latest_report(&state, package.id).await?;
    let latest_report_current = latest_report_is_current_locked(
        &state,
        &package,
        &project,
        &metadata,
        latest_report.as_ref(),
    )
    .await?;
    Ok(Json(PackageView {
        package,
        latest_report,
        latest_report_current,
    }))
}

async fn list_reports(
    State(state): State<Arc<AppState>>,
    Path(package_id): Path<Uuid>,
) -> Result<Json<ReportListView>, ServiceError> {
    let package = load_package(&state, ExportPackageId::from_uuid(package_id)).await?;
    Ok(Json(ReportListView {
        items: load_reports_for_package(&state, package.id).await?,
    }))
}

async fn get_report(
    State(state): State<Arc<AppState>>,
    Path(report_id): Path<Uuid>,
) -> Result<Json<QualityReport>, ServiceError> {
    Ok(Json(
        load_report(&state, QualityReportId::from_uuid(report_id)).await?,
    ))
}

async fn get_report_html(
    State(state): State<Arc<AppState>>,
    Path(report_id): Path<Uuid>,
) -> Result<Response, ServiceError> {
    let report = load_report(&state, QualityReportId::from_uuid(report_id)).await?;
    let html = render_report_html(&report);
    Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, "text/html; charset=utf-8")
        .header(header::CACHE_CONTROL, "no-store")
        .body(Body::from(html))
        .map_err(|error| ServiceError::Internal(error.to_string()))
}

async fn load_project(state: &AppState, project_id: Uuid) -> Result<Project, ServiceError> {
    state
        .database
        .repositories()
        .projects
        .get_project(ProjectId::from_uuid(project_id))
        .await
        .map_err(|error| ServiceError::Storage(error.to_string()))?
        .ok_or(ServiceError::NotFound)
}

async fn load_completed_distribution_job(
    state: &AppState,
    job_id: JobId,
    project_id: ProjectId,
) -> Result<Job, ServiceError> {
    let job = state
        .database
        .repositories()
        .jobs
        .get(job_id)
        .await
        .map_err(|error| ServiceError::Storage(error.to_string()))?
        .ok_or_else(|| {
            ServiceError::InvalidRequest("distribution export job does not exist".to_owned())
        })?;
    if job.project_id != project_id {
        return Err(ServiceError::InvalidRequest(
            "distribution export job is not owned by this project".to_owned(),
        ));
    }
    if !matches!(job.kind, JobKind::Conversion | JobKind::Export) {
        return Err(ServiceError::InvalidRequest(
            "distribution packages require a conversion or export job".to_owned(),
        ));
    }
    if job.state != JobState::Completed {
        return Err(ServiceError::Conflict(
            "distribution export job must be completed before packaging".to_owned(),
        ));
    }
    Ok(job)
}

async fn export_manifest_ids(
    state: &AppState,
    job_id: JobId,
) -> Result<Vec<ArtifactId>, ServiceError> {
    let ids = sqlx::query_scalar::<_, String>(
        "SELECT id FROM artifacts WHERE pinned_by_job_id = ? AND kind = 'export_manifest' \
         ORDER BY created_at, id",
    )
    .bind(job_id.to_string())
    .fetch_all(state.database.pool())
    .await
    .map_err(storage_error)?;
    ids.into_iter()
        .map(|id| ArtifactId::from_str(&id).map_err(invalid_stored_data))
        .collect()
}

async fn load_current_export_manifest(
    state: &AppState,
    job_id: JobId,
    project_id: ProjectId,
) -> Result<ArtifactRecord, ServiceError> {
    let ids = export_manifest_ids(state, job_id).await?;
    if ids.len() != 1 {
        return Err(ServiceError::Conflict(format!(
            "completed export job must have exactly one manifest (found {})",
            ids.len()
        )));
    }
    let record = load_artifact_record(state, ids[0]).await?;
    if record.project_id != project_id
        || record.job_id != Some(job_id)
        || record.artifact.kind != ArtifactKind::ExportManifest
    {
        return Err(ServiceError::Conflict(
            "export manifest identity does not match the completed project export".to_owned(),
        ));
    }
    let path = FilePath::new(&record.artifact.path);
    if !path.is_file() {
        return Err(ServiceError::Conflict(
            "completed export manifest file is missing".to_owned(),
        ));
    }
    if !record
        .artifact
        .fingerprint
        .algorithm
        .eq_ignore_ascii_case("blake3")
    {
        return Err(ServiceError::Conflict(
            "completed export manifest uses an unsupported fingerprint algorithm".to_owned(),
        ));
    }
    if hash_file(path).await? != record.artifact.fingerprint.digest {
        return Err(ServiceError::Conflict(
            "completed export manifest changed after it was registered".to_owned(),
        ));
    }
    Ok(record)
}

async fn load_metadata_view(
    state: &AppState,
    project: &Project,
) -> Result<DistributionMetadataView, ServiceError> {
    let row = sqlx::query(
        "SELECT revision, updated_at, payload FROM distribution_metadata WHERE project_id = ?",
    )
    .bind(project.id.to_string())
    .fetch_optional(state.database.pool())
    .await
    .map_err(storage_error)?;
    if let Some(row) = row {
        let revision = u64::try_from(row.get::<i64, _>("revision")).map_err(invalid_stored_data)?;
        let updated_at = DateTime::parse_from_rfc3339(row.get::<&str, _>("updated_at"))
            .map_err(invalid_stored_data)?
            .with_timezone(&Utc);
        let metadata =
            serde_json::from_str(row.get::<&str, _>("payload")).map_err(invalid_stored_data)?;
        Ok(DistributionMetadataView {
            revision,
            updated_at: Some(updated_at),
            metadata,
        })
    } else {
        Ok(DistributionMetadataView {
            revision: 0,
            updated_at: None,
            metadata: default_metadata(project),
        })
    }
}

fn default_metadata(project: &Project) -> DistributionMetadata {
    DistributionMetadata {
        authors: project.metadata.authors.clone(),
        narrators: project
            .metadata
            .narrator
            .iter()
            .cloned()
            .collect::<Vec<_>>(),
        publisher: project.metadata.publisher.clone(),
        description: project.metadata.description.clone(),
        language: project.metadata.language.clone(),
        identifier: project.metadata.identifier.clone(),
        cover_artifact_id: project.metadata.cover_artifact_id,
        ..DistributionMetadata::default()
    }
}

fn normalize_metadata(mut metadata: DistributionMetadata) -> DistributionMetadata {
    fn normalized(value: Option<String>) -> Option<String> {
        value
            .map(|value| value.trim().to_owned())
            .filter(|value| !value.is_empty())
    }
    fn normalized_list(values: Vec<String>) -> Vec<String> {
        let mut unique = BTreeSet::new();
        values
            .into_iter()
            .map(|value| value.trim().to_owned())
            .filter(|value| !value.is_empty())
            .filter(|value| unique.insert(value.to_lowercase()))
            .collect()
    }
    metadata.subtitle = normalized(metadata.subtitle);
    metadata.authors = normalized_list(metadata.authors);
    metadata.narrators = normalized_list(metadata.narrators);
    metadata.publisher = normalized(metadata.publisher);
    metadata.imprint = normalized(metadata.imprint);
    metadata.description = normalized(metadata.description);
    metadata.language = normalized(metadata.language);
    metadata.identifier = normalized(metadata.identifier);
    metadata.identifier_kind = normalized(metadata.identifier_kind);
    metadata.source_rights = normalized(metadata.source_rights);
    metadata.audio_rights = normalized(metadata.audio_rights);
    metadata.attestations.acx_authorization_reference =
        normalized(metadata.attestations.acx_authorization_reference);
    metadata
}

async fn validate_metadata(
    state: &AppState,
    project_id: ProjectId,
    metadata: &DistributionMetadata,
) -> Result<(), ServiceError> {
    let serialized = serde_json::to_vec(metadata).map_err(internal_error)?;
    if serialized.len() > MAX_METADATA_TEXT_BYTES {
        return Err(ServiceError::InvalidRequest(
            "distribution metadata is too large".to_owned(),
        ));
    }
    if metadata.attestations.acx_external_authorization.is_some()
        != metadata.attestations.acx_authorization_reference.is_some()
    {
        return Err(ServiceError::InvalidRequest(
            "ACX authorization requires both a confirmation date and a reference".to_owned(),
        ));
    }
    if let Some(cover_id) = metadata.cover_artifact_id {
        let record = load_artifact_record(state, cover_id).await?;
        if record.project_id != project_id || record.artifact.kind != ArtifactKind::Cover {
            return Err(ServiceError::InvalidRequest(
                "distribution cover must be a cover artifact owned by this project".to_owned(),
            ));
        }
    }
    let mut segment_ids = BTreeSet::new();
    let references = metadata
        .opening_credit_segment_ids
        .iter()
        .copied()
        .map(|id| (id, Some(ProductionSegmentSource::OpeningCredit)))
        .chain(
            metadata
                .closing_credit_segment_ids
                .iter()
                .copied()
                .map(|id| (id, Some(ProductionSegmentSource::ClosingCredit))),
        )
        .chain(
            metadata
                .sample_segment_ids
                .iter()
                .copied()
                .map(|id| (id, None)),
        );
    for (segment_id, expected_source) in references {
        if !segment_ids.insert(segment_id) {
            return Err(ServiceError::InvalidRequest(
                "distribution segment lists may not contain duplicates".to_owned(),
            ));
        }
        let evidence = distribution_segment_evidence(state, project_id, segment_id).await?;
        if !evidence.current {
            return Err(ServiceError::InvalidRequest(format!(
                "distribution segment {segment_id} is not a current selected proofing take: {}",
                evidence
                    .problem
                    .as_deref()
                    .unwrap_or("its proofing identity could not be verified")
            )));
        }
        if expected_source.is_some() && evidence.source != expected_source {
            return Err(ServiceError::InvalidRequest(format!(
                "distribution segment {segment_id} has the wrong credit type"
            )));
        }
    }
    Ok(())
}

fn validate_artifact_id_list(
    values: &[Uuid],
    maximum: usize,
    label: &str,
    allow_empty: bool,
) -> Result<(), ServiceError> {
    if !allow_empty && values.is_empty() {
        return Err(ServiceError::InvalidRequest(format!(
            "{label} may not be empty"
        )));
    }
    if values.len() > maximum {
        return Err(ServiceError::InvalidRequest(format!(
            "{label} may contain at most {maximum} entries"
        )));
    }
    if values.iter().copied().collect::<BTreeSet<_>>().len() != values.len() {
        return Err(ServiceError::InvalidRequest(format!(
            "{label} may not contain duplicates"
        )));
    }
    Ok(())
}

fn require_complete_export_set(
    upload_ids: &BTreeSet<Uuid>,
    canonical_ids: &[ArtifactId],
) -> Result<(), ServiceError> {
    let canonical_set = canonical_ids
        .iter()
        .map(|id| id.as_uuid())
        .collect::<BTreeSet<_>>();
    if canonical_ids.is_empty() || upload_ids != &canonical_set {
        return Err(ServiceError::InvalidRequest(format!(
            "upload artifacts must contain the complete export job output (expected {}, received {})",
            canonical_ids.len(),
            upload_ids.len()
        )));
    }
    Ok(())
}

fn common_output_directory(uploads: &[ArtifactRecord]) -> Result<String, ServiceError> {
    let first = uploads.first().ok_or_else(|| {
        ServiceError::InvalidRequest("upload artifacts may not be empty".to_owned())
    })?;
    let directory = FilePath::new(&first.artifact.path)
        .parent()
        .ok_or_else(|| {
            ServiceError::InvalidRequest("upload artifact has no output directory".to_owned())
        })?;
    if uploads
        .iter()
        .any(|record| FilePath::new(&record.artifact.path).parent() != Some(directory))
    {
        return Err(ServiceError::InvalidRequest(
            "all upload artifacts must share one output directory".to_owned(),
        ));
    }
    Ok(directory.to_string_lossy().into_owned())
}

fn same_package_inputs(left: &ExportPackage, right: &ExportPackage) -> bool {
    left.project_id == right.project_id
        && left.job_id == right.job_id
        && left.target == right.target
        && left.output_directory == right.output_directory
        && left.upload_artifact_ids == right.upload_artifact_ids
        && left.review_artifact_ids == right.review_artifact_ids
}

async fn load_artifact_record(
    state: &AppState,
    id: ArtifactId,
) -> Result<ArtifactRecord, ServiceError> {
    find_artifact_record(state, id)
        .await?
        .ok_or(ServiceError::NotFound)
}

async fn find_artifact_record(
    state: &AppState,
    id: ArtifactId,
) -> Result<Option<ArtifactRecord>, ServiceError> {
    let row =
        sqlx::query("SELECT project_id, pinned_by_job_id, payload FROM artifacts WHERE id = ?")
            .bind(id.to_string())
            .fetch_optional(state.database.pool())
            .await
            .map_err(storage_error)?;
    let Some(row) = row else {
        return Ok(None);
    };
    let project_id = row
        .try_get::<Option<String>, _>("project_id")
        .map_err(invalid_stored_data)?
        .ok_or_else(|| {
            ServiceError::InvalidRequest(
                "distribution artifacts must be owned by a project".to_owned(),
            )
        })?;
    let project_id = ProjectId::from_str(&project_id).map_err(invalid_stored_data)?;
    let job_id = row
        .get::<Option<String>, _>("pinned_by_job_id")
        .map(|value| JobId::from_str(&value).map_err(invalid_stored_data))
        .transpose()?;
    let artifact =
        serde_json::from_str(row.get::<&str, _>("payload")).map_err(invalid_stored_data)?;
    Ok(Some(ArtifactRecord {
        artifact,
        project_id,
        job_id,
    }))
}

async fn load_package_for_job(
    state: &AppState,
    job_id: JobId,
    target: DistributionTarget,
) -> Result<Option<ExportPackage>, ServiceError> {
    let payload = sqlx::query_scalar::<_, String>(
        "SELECT payload FROM export_packages WHERE job_id = ? AND target = ?",
    )
    .bind(job_id.to_string())
    .bind(target_text(target))
    .fetch_optional(state.database.pool())
    .await
    .map_err(storage_error)?;
    payload
        .as_deref()
        .map(|payload| serde_json::from_str(payload).map_err(invalid_stored_data))
        .transpose()
}

async fn load_package(
    state: &AppState,
    id: ExportPackageId,
) -> Result<ExportPackage, ServiceError> {
    let payload =
        sqlx::query_scalar::<_, String>("SELECT payload FROM export_packages WHERE id = ?")
            .bind(id.to_string())
            .fetch_optional(state.database.pool())
            .await
            .map_err(storage_error)?
            .ok_or(ServiceError::NotFound)?;
    serde_json::from_str(&payload).map_err(invalid_stored_data)
}

async fn load_packages_for_project(
    state: &AppState,
    project_id: ProjectId,
) -> Result<Vec<ExportPackage>, ServiceError> {
    let payloads = sqlx::query_scalar::<_, String>(
        "SELECT payload FROM export_packages WHERE project_id = ? ORDER BY created_at DESC, id",
    )
    .bind(project_id.to_string())
    .fetch_all(state.database.pool())
    .await
    .map_err(storage_error)?;
    payloads
        .iter()
        .map(|payload| serde_json::from_str(payload).map_err(invalid_stored_data))
        .collect()
}

async fn load_report(state: &AppState, id: QualityReportId) -> Result<QualityReport, ServiceError> {
    let payload =
        sqlx::query_scalar::<_, String>("SELECT payload FROM quality_reports WHERE id = ?")
            .bind(id.to_string())
            .fetch_optional(state.database.pool())
            .await
            .map_err(storage_error)?
            .ok_or(ServiceError::NotFound)?;
    serde_json::from_str(&payload).map_err(invalid_stored_data)
}

async fn load_latest_report(
    state: &AppState,
    package_id: ExportPackageId,
) -> Result<Option<QualityReport>, ServiceError> {
    let payload = sqlx::query_scalar::<_, String>(
        "SELECT payload FROM quality_reports WHERE package_id = ? \
         ORDER BY generated_at DESC, id DESC LIMIT 1",
    )
    .bind(package_id.to_string())
    .fetch_optional(state.database.pool())
    .await
    .map_err(storage_error)?;
    payload
        .as_deref()
        .map(|payload| serde_json::from_str(payload).map_err(invalid_stored_data))
        .transpose()
}

async fn load_reports_for_package(
    state: &AppState,
    package_id: ExportPackageId,
) -> Result<Vec<QualityReport>, ServiceError> {
    let payloads = sqlx::query_scalar::<_, String>(
        "SELECT payload FROM quality_reports WHERE package_id = ? \
         ORDER BY generated_at DESC, id DESC",
    )
    .bind(package_id.to_string())
    .fetch_all(state.database.pool())
    .await
    .map_err(storage_error)?;
    payloads
        .iter()
        .map(|payload| serde_json::from_str(payload).map_err(invalid_stored_data))
        .collect()
}

const fn target_text(target: DistributionTarget) -> &'static str {
    match target {
        DistributionTarget::GenericM4b => "generic_m4b",
        DistributionTarget::Acx => "acx",
        DistributionTarget::SpotifyForAuthors => "spotify_for_authors",
        DistributionTarget::GooglePlay => "google_play",
    }
}

fn package_input_snapshot(package: &ExportPackage) -> ExportPackage {
    let mut snapshot = package.clone();
    snapshot.quality_report_id = None;
    snapshot
}

fn package_digest(package: &ExportPackage) -> Result<String, ServiceError> {
    let bytes = serde_json::to_vec(&package_input_snapshot(package)).map_err(internal_error)?;
    Ok(blake3::hash(&bytes).to_hex().to_string())
}

fn metadata_digest(
    project: &Project,
    metadata: &DistributionMetadata,
) -> Result<String, ServiceError> {
    let bytes =
        serde_json::to_vec(&(project.metadata.title.as_str(), metadata)).map_err(internal_error)?;
    Ok(blake3::hash(&bytes).to_hex().to_string())
}

fn policy_digest(policy: &DistributionPolicyView) -> Result<String, ServiceError> {
    let bytes = serde_json::to_vec(policy).map_err(internal_error)?;
    Ok(blake3::hash(&bytes).to_hex().to_string())
}

fn report_static_inputs_are_current(
    package: &ExportPackage,
    project: &Project,
    metadata: &DistributionMetadataView,
    report: &QualityReport,
) -> Result<bool, ServiceError> {
    let active_policy = policy_view(package.target);
    Ok(report.package_id == package.id
        && report.policy.target == package.target
        && report.policy.policy_version == active_policy.policy_version
        && report.policy_digest == policy_digest(&active_policy)?
        && report.metadata_revision == metadata.revision
        && report.metadata_digest == metadata_digest(project, &metadata.metadata)?
        && report.package_digest == package_digest(package)?
        && report.analyzer_version == ANALYZER_VERSION)
}

struct CurrentReportInputs {
    export_manifest_artifact_id: Option<ArtifactId>,
    segment_evidence: Vec<DistributionSegmentEvidence>,
    file_hashes: BTreeMap<String, String>,
    input_digest: String,
}

async fn current_report_inputs(
    state: &AppState,
    package: &ExportPackage,
    metadata: &DistributionMetadata,
) -> Result<CurrentReportInputs, ServiceError> {
    let mut findings = Vec::new();
    let artifacts = collect_report_artifacts(state, package, metadata, &mut findings).await?;
    let file_hashes = hash_report_artifacts(&artifacts.records, &mut findings).await;
    let input_digest = report_input_digest(state, package, &artifacts, &file_hashes).await?;
    Ok(CurrentReportInputs {
        export_manifest_artifact_id: artifacts.export_manifest_artifact_id,
        segment_evidence: artifacts.segment_evidence,
        file_hashes,
        input_digest,
    })
}

/// Compares a stored report with live proofing and artifact provenance while the caller holds the
/// project's lifecycle lock. Keeping the expensive file hashing outside a database transaction
/// avoids a long `SQLite` writer lock; the lifecycle lock serializes every in-process proof mutation.
async fn latest_report_is_current_locked(
    state: &AppState,
    package: &ExportPackage,
    project: &Project,
    metadata: &DistributionMetadataView,
    report: Option<&QualityReport>,
) -> Result<bool, ServiceError> {
    let Some(report) = report else {
        return Ok(false);
    };
    if report.input_digest.is_empty()
        || !report_static_inputs_are_current(package, project, metadata, report)?
    {
        return Ok(false);
    }
    let current = current_report_inputs(state, package, &metadata.metadata).await?;
    Ok(
        report.export_manifest_artifact_id == current.export_manifest_artifact_id
            && report.segment_evidence == current.segment_evidence
            && report.file_hashes == current.file_hashes
            && report.input_digest == current.input_digest,
    )
}

fn stale_metadata(actual_revision: u64) -> ServiceError {
    ServiceError::ConflictDetails {
        code: "stale_distribution_metadata",
        detail: "distribution metadata changed since it was loaded".to_owned(),
        meta: serde_json::json!({"actualRevision": actual_revision}),
    }
}

async fn rerun_quality_control(
    State(state): State<Arc<AppState>>,
    Path(package_id): Path<Uuid>,
) -> Result<(StatusCode, Json<QualityRunView>), ServiceError> {
    let package = load_package(&state, ExportPackageId::from_uuid(package_id)).await?;
    // Proofing semantics depend on model/provider/voice/dictionary configuration. Follow the
    // global lock ordering used by proofing mutations: model lifecycle, then project lifecycle.
    let _model_lifecycle_guard = state.model_lifecycle.lock().await;
    let project_lock = state
        .character_lifecycle_lock(package.project_id.as_uuid())
        .await;
    let _project_guard = project_lock.lock().await;
    let project = load_project(&state, package.project_id.as_uuid()).await?;
    let metadata = load_metadata_view(&state, &project).await?;
    let report = build_quality_report(&state, &package, &project, &metadata).await?;
    let package = persist_quality_report(&state, package, &project, &metadata, &report).await?;
    Ok((
        StatusCode::CREATED,
        Json(QualityRunView { package, report }),
    ))
}

async fn persist_quality_report(
    state: &AppState,
    package: ExportPackage,
    project: &Project,
    metadata: &DistributionMetadataView,
    report: &QualityReport,
) -> Result<ExportPackage, ServiceError> {
    if report.package_id != package.id {
        return Err(ServiceError::InvalidRequest(
            "quality report belongs to another package".to_owned(),
        ));
    }
    if !latest_report_is_current_locked(state, &package, project, metadata, Some(report)).await? {
        return Err(ServiceError::ConflictDetails {
            code: "quality_report_inputs_changed",
            detail: "proofing, metadata, or an influencing artifact changed while quality control was running; rerun quality control".to_owned(),
            meta: serde_json::json!({"packageId": package.id}),
        });
    }
    let mut transaction = state.database.pool().begin().await.map_err(storage_error)?;
    sqlx::query(
        "INSERT INTO quality_reports \
         (id, package_id, policy_version, policy_digest, metadata_revision, metadata_digest, \
          package_digest, technical_ready, submission_ready, generated_at, payload) \
         VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
    )
    .bind(report.id.to_string())
    .bind(report.package_id.to_string())
    .bind(&report.policy.policy_version)
    .bind(&report.policy_digest)
    .bind(i64::try_from(report.metadata_revision).unwrap_or(i64::MAX))
    .bind(&report.metadata_digest)
    .bind(&report.package_digest)
    .bind(report.technical_ready)
    .bind(report.submission_ready)
    .bind(report.generated_at.to_rfc3339())
    .bind(serde_json::to_string(report).map_err(internal_error)?)
    .execute(&mut *transaction)
    .await
    .map_err(storage_error)?;
    let package_payload =
        sqlx::query_scalar::<_, String>("SELECT payload FROM export_packages WHERE id = ?")
            .bind(package.id.to_string())
            .fetch_optional(&mut *transaction)
            .await
            .map_err(storage_error)?
            .ok_or(ServiceError::NotFound)?;
    let mut stored_package: ExportPackage =
        serde_json::from_str(&package_payload).map_err(invalid_stored_data)?;
    let latest_report_id = sqlx::query_scalar::<_, String>(
        "SELECT id FROM quality_reports WHERE package_id = ? \
         ORDER BY generated_at DESC, id DESC LIMIT 1",
    )
    .bind(package.id.to_string())
    .fetch_one(&mut *transaction)
    .await
    .map_err(storage_error)?;
    stored_package.quality_report_id =
        Some(QualityReportId::from_str(&latest_report_id).map_err(invalid_stored_data)?);
    let result = sqlx::query("UPDATE export_packages SET payload = ? WHERE id = ?")
        .bind(serde_json::to_string(&stored_package).map_err(internal_error)?)
        .bind(stored_package.id.to_string())
        .execute(&mut *transaction)
        .await
        .map_err(storage_error)?;
    if result.rows_affected() != 1 {
        return Err(ServiceError::NotFound);
    }
    transaction.commit().await.map_err(storage_error)?;
    Ok(stored_package)
}

async fn build_quality_report(
    state: &AppState,
    package: &ExportPackage,
    project: &Project,
    metadata_view: &DistributionMetadataView,
) -> Result<QualityReport, ServiceError> {
    let metadata = &metadata_view.metadata;
    let policy = policy_view(package.target);
    let policy_snapshot = serde_json::to_value(&policy).map_err(internal_error)?;
    let policy_digest = policy_digest(&policy)?;
    let sidecars = resolve_sidecars(state)?;
    let (ffmpeg_version, ffmpeg_build) = ffmpeg_description(&sidecars).await?;
    let mut findings = Vec::new();
    append_package_job_invariant_findings(state, package, &mut findings).await?;
    let artifacts = collect_report_artifacts(state, package, metadata, &mut findings).await?;
    let file_hashes = hash_report_artifacts(&artifacts.records, &mut findings).await;
    findings.extend(metadata_findings(package, project, metadata, &artifacts).await?);
    let mut channels = Vec::new();
    let mut durations = Vec::new();
    for artifact_id in &package.upload_artifact_ids {
        let Some(record) = artifacts.records.get(artifact_id) else {
            continue;
        };
        if artifacts.invalid_ids.contains(artifact_id) {
            continue;
        }
        let Some(file_hash) = file_hashes.get(&artifact_id.to_string()) else {
            continue;
        };
        let evidence = analyze_artifact(package.target, record, file_hash, &sidecars).await?;
        channels.extend(evidence.channels);
        durations.push(evidence.duration_milliseconds);
        findings.extend(evidence.findings);
    }
    if behavior(package.target).require_consistent_channels
        && channels
            .first()
            .is_some_and(|first| channels.iter().any(|channels| channels != first))
    {
        findings.push(quality_finding(
            "file_channel_layout_inconsistent",
            QualityFindingStatus::Fail,
            "technical.package",
            "all audio files in this package must use the same channel count",
            Some(serde_json::json!(channels)),
            Some(serde_json::json!("one consistent channel count")),
            Some("re-export every file as consistently mono or stereo"),
            false,
        ));
    }
    append_package_duration_findings(package.target, &durations, &mut findings);
    append_artifact_stability_findings(&artifacts.records, &file_hashes, &mut findings).await;
    let input_digest = report_input_digest(state, package, &artifacts, &file_hashes).await?;
    let (technical_ready, submission_ready) = report_readiness(&findings);
    Ok(QualityReport {
        id: QualityReportId::new(),
        package_id: package.id,
        policy: policy.policy_ref(),
        policy_digest,
        policy_snapshot: Some(policy_snapshot),
        metadata_revision: metadata_view.revision,
        metadata_digest: metadata_digest(project, metadata)?,
        metadata_snapshot: Some(metadata.clone()),
        project_title: Some(project.metadata.title.clone()),
        package_digest: package_digest(package)?,
        package_snapshot: Some(package_input_snapshot(package)),
        export_manifest_artifact_id: artifacts.export_manifest_artifact_id,
        segment_evidence: artifacts.segment_evidence,
        technical_ready,
        submission_ready,
        findings,
        analyzer_version: ANALYZER_VERSION.to_owned(),
        ffmpeg_version,
        ffmpeg_build_fingerprint: blake3::hash(ffmpeg_build.as_bytes()).to_hex().to_string(),
        file_hashes,
        input_digest,
        generated_at: Utc::now(),
    })
}

#[derive(Debug)]
struct ArtifactEvidence {
    channels: Vec<u16>,
    duration_milliseconds: Option<u64>,
    findings: Vec<QualityFinding>,
}

#[derive(Debug)]
struct ReportArtifactSet {
    records: BTreeMap<ArtifactId, ArtifactRecord>,
    invalid_ids: BTreeSet<ArtifactId>,
    export_manifest_artifact_id: Option<ArtifactId>,
    segment_evidence: Vec<DistributionSegmentEvidence>,
}

async fn report_input_digest(
    state: &AppState,
    package: &ExportPackage,
    artifacts: &ReportArtifactSet,
    file_hashes: &BTreeMap<String, String>,
) -> Result<String, ServiceError> {
    let package_job = state
        .database
        .repositories()
        .jobs
        .get(package.job_id)
        .await
        .map_err(|error| ServiceError::Storage(error.to_string()))?;
    let package_job = package_job.map(|job| {
        serde_json::json!({
            "id": job.id,
            "projectId": job.project_id,
            "kind": job.kind,
            "state": job.state,
        })
    });
    let artifact_inputs = artifacts
        .records
        .iter()
        .map(|(id, record)| {
            serde_json::json!({
                "id": id,
                "projectId": record.project_id,
                "jobId": record.job_id,
                "kind": record.artifact.kind,
                "path": record.artifact.path,
                "fingerprint": record.artifact.fingerprint,
                "mediaType": record.artifact.media_type,
                "durationMilliseconds": record.artifact.duration_ms,
            })
        })
        .collect::<Vec<_>>();
    let snapshot = serde_json::json!({
        "schemaVersion": 1,
        "packageJob": package_job,
        "artifactInputs": artifact_inputs,
        "invalidArtifactIds": artifacts.invalid_ids,
        "exportManifestArtifactId": artifacts.export_manifest_artifact_id,
        "segmentEvidence": artifacts.segment_evidence,
        "fileHashes": file_hashes,
    });
    let bytes = serde_json::to_vec(&snapshot).map_err(internal_error)?;
    Ok(blake3::hash(&bytes).to_hex().to_string())
}

async fn append_package_job_invariant_findings(
    state: &AppState,
    package: &ExportPackage,
    findings: &mut Vec<QualityFinding>,
) -> Result<(), ServiceError> {
    let job = state
        .database
        .repositories()
        .jobs
        .get(package.job_id)
        .await
        .map_err(|error| ServiceError::Storage(error.to_string()))?;
    let valid = job.as_ref().is_some_and(|job| {
        job.project_id == package.project_id
            && matches!(job.kind, JobKind::Conversion | JobKind::Export)
            && job.state == JobState::Completed
    });
    if !valid {
        findings.push(quality_finding(
            "package_job_invariant_invalid",
            QualityFindingStatus::Fail,
            "technical.package",
            "the package export job is missing, incomplete, or owned by another project",
            job.map(|job| {
                serde_json::json!({
                    "projectId": job.project_id,
                    "kind": job.kind,
                    "state": job.state
                })
            }),
            Some(serde_json::json!({
                "projectId": package.project_id,
                "kind": ["conversion", "export"],
                "state": "completed"
            })),
            Some("recreate the package from a completed export owned by this project"),
            false,
        ));
    }
    Ok(())
}

#[allow(clippy::too_many_lines)]
async fn collect_report_artifacts(
    state: &AppState,
    package: &ExportPackage,
    metadata: &DistributionMetadata,
    findings: &mut Vec<QualityFinding>,
) -> Result<ReportArtifactSet, ServiceError> {
    let mut ids = package
        .upload_artifact_ids
        .iter()
        .chain(&package.review_artifact_ids)
        .copied()
        .collect::<BTreeSet<_>>();
    ids.extend(metadata.cover_artifact_id);
    let manifest_ids = export_manifest_ids(state, package.job_id).await?;
    let export_manifest_artifact_id = (manifest_ids.len() == 1).then(|| manifest_ids[0]);
    if manifest_ids.len() != 1 {
        findings.push(quality_finding(
            "export_manifest_identity_invalid",
            QualityFindingStatus::Fail,
            "technical.package",
            "the package export job must have exactly one immutable manifest",
            Some(serde_json::json!(manifest_ids)),
            Some(serde_json::json!({"count": 1, "jobId": package.job_id})),
            Some("re-export the audiobook before recreating the distribution package"),
            false,
        ));
    }
    ids.extend(manifest_ids.iter().copied());
    let segment_ids = metadata
        .opening_credit_segment_ids
        .iter()
        .chain(&metadata.closing_credit_segment_ids)
        .chain(&metadata.sample_segment_ids)
        .copied()
        .collect::<BTreeSet<_>>();
    let mut segment_evidence = Vec::with_capacity(segment_ids.len());
    let mut segment_artifact_ids = BTreeSet::new();
    for segment_id in segment_ids {
        let evidence = distribution_segment_evidence(state, package.project_id, segment_id).await?;
        if let Some(artifact_id) = evidence.take_artifact_id {
            ids.insert(artifact_id);
            segment_artifact_ids.insert(artifact_id);
        }
        segment_evidence.push(evidence);
    }
    let mut records = BTreeMap::new();
    let mut invalid_ids = BTreeSet::new();
    for id in ids {
        let Some(record) = find_artifact_record(state, id).await? else {
            invalid_ids.insert(id);
            findings.push(quality_finding(
                "package_artifact_reference_missing",
                QualityFindingStatus::Fail,
                "technical.package",
                "an artifact referenced by the package or metadata no longer exists",
                Some(serde_json::json!(id)),
                Some(serde_json::json!("existing project-owned artifact")),
                Some("restore the artifact or recreate the distribution package"),
                false,
            ));
            continue;
        };
        records.insert(id, record);
    }

    for id in &package.upload_artifact_ids {
        if records.get(id).is_some_and(|record| {
            record.project_id != package.project_id
                || record.artifact.kind != ArtifactKind::Export
                || record.job_id != Some(package.job_id)
        }) {
            invalid_ids.insert(*id);
        }
    }
    for id in &package.review_artifact_ids {
        if records.get(id).is_some_and(|record| {
            record.project_id != package.project_id
                || !matches!(
                    record.artifact.kind,
                    ArtifactKind::Cover
                        | ArtifactKind::Preview
                        | ArtifactKind::Export
                        | ArtifactKind::ExportManifest
                )
        }) {
            invalid_ids.insert(*id);
        }
    }
    if let Some(id) = metadata.cover_artifact_id
        && records.get(&id).is_some_and(|record| {
            record.project_id != package.project_id || record.artifact.kind != ArtifactKind::Cover
        })
    {
        invalid_ids.insert(id);
    }
    for id in &manifest_ids {
        if records.get(id).is_none_or(|record| {
            record.project_id != package.project_id
                || record.job_id != Some(package.job_id)
                || record.artifact.kind != ArtifactKind::ExportManifest
        }) {
            invalid_ids.insert(*id);
        }
    }
    for id in &segment_artifact_ids {
        if records.get(id).is_none_or(|record| {
            record.project_id != package.project_id
                || record.artifact.kind != ArtifactKind::SegmentAudio
        }) {
            invalid_ids.insert(*id);
        }
    }
    for evidence in &mut segment_evidence {
        if evidence
            .take_artifact_id
            .is_some_and(|id| invalid_ids.contains(&id))
        {
            evidence.current = false;
            evidence.problem = Some(
                "the selected take artifact is missing or is not project-owned segment audio"
                    .to_owned(),
            );
        }
    }
    for id in &invalid_ids {
        if records.contains_key(id) {
            findings.push(quality_finding(
                "package_artifact_invariant_invalid",
                QualityFindingStatus::Fail,
                "technical.package",
                "an artifact has the wrong project, kind, or export-job association",
                Some(serde_json::json!(id)),
                Some(serde_json::json!({
                    "projectId": package.project_id,
                    "exportJobId": package.job_id
                })),
                Some("recreate the package from artifacts owned by this project and export job"),
                false,
            ));
        }
    }
    Ok(ReportArtifactSet {
        records,
        invalid_ids,
        export_manifest_artifact_id,
        segment_evidence,
    })
}

#[allow(clippy::too_many_lines)]
async fn distribution_segment_evidence(
    state: &AppState,
    project_id: ProjectId,
    segment_id: audiobookai_core::SegmentId,
) -> Result<DistributionSegmentEvidence, ServiceError> {
    let repositories = state.database.repositories();
    let repository = &repositories.proofing;
    let segment = repository
        .get_segment(segment_id)
        .await
        .map_err(|error| ServiceError::Storage(error.to_string()))?;
    let mut evidence = DistributionSegmentEvidence {
        segment_id,
        source: segment.as_ref().map(|segment| segment.source),
        active: segment.as_ref().is_some_and(|segment| segment.active),
        segment_revision: segment.as_ref().map(|segment| segment.revision),
        plan_source_conversion_job_id: None,
        plan_status: None,
        selection_revision: None,
        take_id: None,
        take_artifact_id: None,
        expected_input_hash: segment
            .as_ref()
            .map(|segment| segment.expected_input_hash.clone()),
        selected_take_input_hash: None,
        current_input_hash: None,
        current: false,
        problem: None,
    };
    let Some(segment) = segment else {
        evidence.problem = Some("the proofing segment no longer exists".to_owned());
        return Ok(evidence);
    };
    if segment.project_id != project_id {
        evidence.problem = Some("the proofing segment belongs to another project".to_owned());
        return Ok(evidence);
    }
    if !segment.active {
        evidence.problem = Some("the proofing segment is superseded".to_owned());
        return Ok(evidence);
    }
    let plan = repository
        .get_plan(project_id)
        .await
        .map_err(|error| ServiceError::Storage(error.to_string()))?;
    evidence.plan_source_conversion_job_id =
        plan.as_ref().map(|plan| plan.source_conversion_job_id);
    evidence.plan_status = plan.as_ref().map(|plan| plan.status);
    if plan
        .as_ref()
        .is_none_or(|plan| plan.status != ProofingPlanStatus::Ready)
    {
        evidence.problem = Some("the project's active proofing plan is not ready".to_owned());
        return Ok(evidence);
    }
    let Some(selection) = repository
        .get_selection(segment_id)
        .await
        .map_err(|error| ServiceError::Storage(error.to_string()))?
    else {
        evidence.problem = Some("the proofing segment has no selected take".to_owned());
        return Ok(evidence);
    };
    evidence.selection_revision = Some(selection.revision);
    evidence.take_id = Some(selection.take_id);
    let Some(take) = repository
        .get_take(selection.take_id)
        .await
        .map_err(|error| ServiceError::Storage(error.to_string()))?
    else {
        evidence.problem = Some("the selected take no longer exists".to_owned());
        return Ok(evidence);
    };
    evidence.take_artifact_id = Some(take.artifact_id);
    evidence.selected_take_input_hash = Some(take.semantic_input_hash.clone());
    if take.segment_id != segment_id {
        evidence.problem = Some("the selected take belongs to another segment".to_owned());
        return Ok(evidence);
    }
    let current_input_hash =
        match crate::conversion::load_proofing_segment_plan(state, project_id.as_uuid(), &segment)
            .await
        {
            Ok(plan) => crate::conversion::segment_semantic_input_hash(&plan).ok(),
            Err(_) => None,
        };
    evidence.current_input_hash.clone_from(&current_input_hash);
    if take.semantic_input_hash != segment.expected_input_hash {
        evidence.problem = Some("the selected take is stale".to_owned());
        return Ok(evidence);
    }
    if current_input_hash.as_deref() != Some(segment.expected_input_hash.as_str()) {
        evidence.problem = Some("the segment's current narration inputs have changed".to_owned());
        return Ok(evidence);
    }
    let Some(record) = find_artifact_record(state, take.artifact_id).await? else {
        evidence.problem = Some("the selected take artifact no longer exists".to_owned());
        return Ok(evidence);
    };
    if record.project_id != project_id || record.artifact.kind != ArtifactKind::SegmentAudio {
        evidence.problem =
            Some("the selected take artifact is not project-owned segment audio".to_owned());
        return Ok(evidence);
    }
    if !FilePath::new(&record.artifact.path).is_file() {
        evidence.problem = Some("the selected take audio file is missing".to_owned());
        return Ok(evidence);
    }
    evidence.current = true;
    Ok(evidence)
}

async fn hash_report_artifacts(
    records: &BTreeMap<ArtifactId, ArtifactRecord>,
    findings: &mut Vec<QualityFinding>,
) -> BTreeMap<String, String> {
    let mut hashes = BTreeMap::new();
    for (id, record) in records {
        let path = FilePath::new(&record.artifact.path);
        if !path.is_file() {
            findings.push(quality_finding(
                "influencing_artifact_file_missing",
                QualityFindingStatus::Fail,
                format!("technical.artifact.{id}"),
                "an artifact that influences this report is missing",
                Some(serde_json::json!(id)),
                Some(serde_json::json!("readable stable file")),
                Some("restore the artifact or recreate the distribution package"),
                false,
            ));
            continue;
        }
        match hash_file(path).await {
            Ok(hash) => {
                if !record
                    .artifact
                    .fingerprint
                    .algorithm
                    .eq_ignore_ascii_case("blake3")
                {
                    findings.push(quality_finding(
                        "influencing_artifact_fingerprint_algorithm_unsupported",
                        QualityFindingStatus::Fail,
                        format!("technical.artifact.{id}"),
                        "an artifact that influences this report uses an unsupported fingerprint algorithm",
                        Some(serde_json::json!(&record.artifact.fingerprint.algorithm)),
                        Some(serde_json::json!("blake3")),
                        Some("recreate the managed export with a BLAKE3 fingerprint"),
                        false,
                    ));
                } else if record.artifact.fingerprint.digest != hash {
                    findings.push(quality_finding(
                        "influencing_artifact_fingerprint_changed",
                        QualityFindingStatus::Fail,
                        format!("technical.artifact.{id}"),
                        "an artifact that influences this report changed after registration",
                        Some(serde_json::json!(&hash)),
                        Some(serde_json::json!(&record.artifact.fingerprint.digest)),
                        Some("restore the registered artifact or recreate the export"),
                        false,
                    ));
                }
                hashes.insert(id.to_string(), hash);
            }
            Err(error) => findings.push(quality_finding(
                "influencing_artifact_unreadable",
                QualityFindingStatus::Fail,
                format!("technical.artifact.{id}"),
                "an artifact that influences this report could not be hashed",
                Some(serde_json::json!(error.to_string())),
                Some(serde_json::json!("readable stable file")),
                Some("restore file permissions or recreate the distribution package"),
                false,
            )),
        }
    }
    hashes
}

async fn append_artifact_stability_findings(
    records: &BTreeMap<ArtifactId, ArtifactRecord>,
    initial_hashes: &BTreeMap<String, String>,
    findings: &mut Vec<QualityFinding>,
) {
    for (id, initial) in initial_hashes {
        let Ok(id) = ArtifactId::from_str(id) else {
            continue;
        };
        let Some(record) = records.get(&id) else {
            continue;
        };
        let path = FilePath::new(&record.artifact.path);
        let final_hash = if path.is_file() {
            hash_file(path).await.ok()
        } else {
            None
        };
        if final_hash.as_deref() != Some(initial) {
            findings.push(quality_finding(
                "artifact_changed_during_quality_control",
                QualityFindingStatus::Fail,
                format!("technical.artifact.{id}"),
                "an artifact changed or became unreadable while quality control was running",
                Some(serde_json::json!({"before": initial, "after": final_hash})),
                Some(serde_json::json!(
                    "identical BLAKE3 hashes before and after analysis"
                )),
                Some("stop processes modifying the export and rerun quality control"),
                false,
            ));
        }
    }
}

fn append_package_duration_findings(
    target: DistributionTarget,
    durations: &[Option<u64>],
    findings: &mut Vec<QualityFinding>,
) {
    if target != DistributionTarget::GooglePlay {
        return;
    }
    let Some(total) = durations
        .iter()
        .copied()
        .try_fold(0_u64, |total, duration| {
            duration.and_then(|duration| total.checked_add(duration))
        })
    else {
        findings.push(quality_finding(
            "google_total_duration_unknown",
            QualityFindingStatus::Fail,
            "technical.package",
            "Google Play audiobook duration could not be verified",
            None,
            Some(
                serde_json::json!({"minMilliseconds": 300_000, "maxMilliseconds": 360_000_000_u64}),
            ),
            Some("re-export every file with readable duration metadata"),
            false,
        ));
        return;
    };
    if !(300_000..=360_000_000).contains(&total) {
        findings.push(quality_finding(
            "google_total_duration_out_of_range",
            QualityFindingStatus::Fail,
            "technical.package",
            "Google Play audiobooks must be between five minutes and one hundred hours",
            Some(serde_json::json!(total)),
            Some(
                serde_json::json!({"minMilliseconds": 300_000, "maxMilliseconds": 360_000_000_u64}),
            ),
            Some("adjust the delivered audiobook duration and re-export"),
            false,
        ));
    }
}

#[allow(clippy::too_many_lines)]
async fn metadata_findings(
    package: &ExportPackage,
    project: &Project,
    metadata: &DistributionMetadata,
    artifacts: &ReportArtifactSet,
) -> Result<Vec<QualityFinding>, ServiceError> {
    let mut findings = Vec::new();
    require_metadata(
        &mut findings,
        "metadata_title_required",
        "book title",
        !project.metadata.title.trim().is_empty(),
    );
    require_metadata(
        &mut findings,
        "metadata_author_required",
        "at least one author",
        !metadata.authors.is_empty(),
    );
    append_segment_evidence_findings(metadata, artifacts, &mut findings);
    match package.target {
        DistributionTarget::GenericM4b => {
            if package.upload_artifact_ids.len() != 1 {
                findings.push(quality_finding(
                    "generic_m4b_single_file_required",
                    QualityFindingStatus::Fail,
                    "technical.package",
                    "the generic M4B package must contain exactly one audiobook file",
                    Some(serde_json::json!(package.upload_artifact_ids.len())),
                    Some(serde_json::json!(1)),
                    Some("create one chaptered M4B export for the generic package"),
                    false,
                ));
            }
            findings.push(quality_finding(
                "generic_chapter_navigation_manual_review",
                QualityFindingStatus::Warning,
                "manual.chapter_navigation",
                "chapter navigation and metadata should be reviewed in a player",
                None,
                Some(serde_json::json!("working chapter navigation")),
                Some("listen through chapter transitions before delivery"),
                false,
            ));
        }
        DistributionTarget::Acx => {
            require_metadata(
                &mut findings,
                "metadata_narrator_required",
                "at least one narrator",
                !metadata.narrators.is_empty(),
            );
            require_metadata(
                &mut findings,
                "acx_opening_credits_required",
                "opening credit segments",
                !metadata.opening_credit_segment_ids.is_empty(),
            );
            require_metadata(
                &mut findings,
                "acx_closing_credits_required",
                "closing credit segments",
                !metadata.closing_credit_segment_ids.is_empty(),
            );
            require_metadata(
                &mut findings,
                "acx_sample_required",
                "sample segments",
                !metadata.sample_segment_ids.is_empty(),
            );
            findings.push(manual_gate(
                "acx_external_authorization",
                "manual.acx_authorization",
                "ACX separately authorizes this synthetic narration",
                metadata.attestations.acx_external_authorization.is_some()
                    && metadata.attestations.acx_authorization_reference.is_some(),
                "record the ACX authorization date and reference before submission",
            ));
            findings.push(manual_gate(
                "rights_and_eligibility",
                "manual.rights",
                "distribution rights and eligibility are confirmed",
                metadata
                    .attestations
                    .rights_and_eligibility_confirmed
                    .is_some(),
                "confirm source, audio, and distribution rights before submission",
            ));
        }
        DistributionTarget::SpotifyForAuthors => {
            require_metadata(
                &mut findings,
                "metadata_narrator_required",
                "at least one narrator",
                !metadata.narrators.is_empty(),
            );
            require_metadata(
                &mut findings,
                "spotify_publisher_required",
                "publisher",
                metadata.publisher.is_some(),
            );
            require_metadata(
                &mut findings,
                "spotify_language_required",
                "language",
                metadata.language.is_some(),
            );
            require_metadata(
                &mut findings,
                "spotify_abridgement_required",
                "abridged or unabridged status",
                metadata.abridged.is_some(),
            );
            require_metadata(
                &mut findings,
                "spotify_opening_credits_required",
                "opening credit segments",
                !metadata.opening_credit_segment_ids.is_empty(),
            );
            require_metadata(
                &mut findings,
                "spotify_closing_credits_required",
                "closing credit segments",
                !metadata.closing_credit_segment_ids.is_empty(),
            );
            findings.push(manual_gate(
                "spotify_digital_voice_disclosure",
                "manual.spotify_disclosure",
                "digital voice narration will be disclosed during upload",
                metadata
                    .attestations
                    .spotify_digital_voice_disclosure
                    .is_some(),
                "confirm the digital-voice disclosure before submission",
            ));
            findings.push(manual_gate(
                "rights_and_eligibility",
                "manual.rights",
                "distribution rights and eligibility are confirmed",
                metadata
                    .attestations
                    .rights_and_eligibility_confirmed
                    .is_some(),
                "confirm source, audio, and distribution rights before submission",
            ));
        }
        DistributionTarget::GooglePlay => {
            require_metadata(
                &mut findings,
                "google_identifier_required",
                "book identifier",
                metadata.identifier.is_some(),
            );
            require_metadata(
                &mut findings,
                "google_identifier_kind_required",
                "book identifier type",
                metadata.identifier_kind.is_some(),
            );
            require_metadata(
                &mut findings,
                "google_abridgement_required",
                "abridged or unabridged status",
                metadata.abridged.is_some(),
            );
            if metadata.identifier.is_some() && metadata.identifier_kind.is_some() {
                findings.push(unverified_manual_finding(
                    "google_identifier_and_filename_manual_review",
                    "manual.google_packaging",
                    "Google identifier type, identifier value, and delivery filenames are valid",
                    "verify the identifier and every filename against the selected Google delivery channel",
                ));
            }
            findings.push(manual_gate(
                "rights_and_eligibility",
                "manual.rights",
                "distribution rights and eligibility are confirmed",
                metadata
                    .attestations
                    .rights_and_eligibility_confirmed
                    .is_some(),
                "confirm source, audio, and distribution rights before submission",
            ));
            if metadata.narrators.is_empty() {
                findings.push(quality_finding(
                    "google_narrator_recommended",
                    QualityFindingStatus::Warning,
                    "metadata",
                    "Google strongly recommends narrator metadata",
                    Some(serde_json::json!([])),
                    Some(serde_json::json!("at least one narrator")),
                    Some("add narrator metadata before delivery"),
                    false,
                ));
            }
        }
    }
    if matches!(
        package.target,
        DistributionTarget::Acx | DistributionTarget::SpotifyForAuthors
    ) {
        findings.push(unverified_manual_finding(
            "credit_files_and_content_manual_review",
            "manual.credits",
            "opening and closing credit content is present in correctly ordered standalone files",
            "listen to the opening and closing files and verify their wording and package positions",
        ));
        let (structure_code, structure_message, structure_remediation) = if package.target
            == DistributionTarget::Acx
        {
            (
                "acx_file_structure_manual_review",
                "every upload file contains one labeled chapter or section and uses a valid filename and order",
                "listen to each file boundary and verify spoken headers, filenames, and ACX upload order",
            )
        } else {
            (
                "spotify_file_structure_manual_review",
                "every upload file contains one labeled chapter or section in canonical playback order",
                "listen to each file boundary and verify spoken headings and Spotify upload order",
            )
        };
        findings.push(unverified_manual_finding(
            structure_code,
            "manual.file_structure",
            structure_message,
            structure_remediation,
        ));
        if package.target == DistributionTarget::Acx {
            findings.push(unverified_manual_finding(
                "acx_sample_content_manual_review",
                "manual.sample",
                "the attached ACX sample contains the selected current proofing segments and no credits",
                "listen to the standalone sample and verify its content against the selected sample segments",
            ));
        }
    }
    append_cover_findings(package, metadata, artifacts, &mut findings).await?;
    append_sample_artifact_findings(package, artifacts, &mut findings);
    Ok(findings)
}

fn append_segment_evidence_findings(
    metadata: &DistributionMetadata,
    artifacts: &ReportArtifactSet,
    findings: &mut Vec<QualityFinding>,
) {
    let references = metadata
        .opening_credit_segment_ids
        .iter()
        .copied()
        .map(|id| (id, Some(ProductionSegmentSource::OpeningCredit)))
        .chain(
            metadata
                .closing_credit_segment_ids
                .iter()
                .copied()
                .map(|id| (id, Some(ProductionSegmentSource::ClosingCredit))),
        )
        .chain(
            metadata
                .sample_segment_ids
                .iter()
                .copied()
                .map(|id| (id, None)),
        );
    for (segment_id, expected_source) in references {
        let evidence = artifacts
            .segment_evidence
            .iter()
            .find(|evidence| evidence.segment_id == segment_id);
        if evidence.is_none_or(|evidence| !evidence.current) {
            findings.push(quality_finding(
                "distribution_segment_not_current",
                QualityFindingStatus::Fail,
                format!("proofing.segment.{segment_id}"),
                "a distribution credit or sample segment is not an active current selected take",
                evidence.map(|evidence| serde_json::json!(evidence)),
                Some(serde_json::json!({
                    "projectOwned": true,
                    "active": true,
                    "proofingPlan": "ready",
                    "selectedTakeCurrent": true
                })),
                Some("replace the reference with an active segment whose current take is selected"),
                false,
            ));
            continue;
        }
        if expected_source
            .is_some_and(|source| evidence.is_none_or(|item| item.source != Some(source)))
        {
            findings.push(quality_finding(
                "distribution_credit_segment_type_invalid",
                QualityFindingStatus::Fail,
                format!("proofing.segment.{segment_id}"),
                "a credit reference points to the wrong proofing segment type",
                evidence
                    .and_then(|evidence| evidence.source)
                    .map(|source| serde_json::json!(source)),
                expected_source.map(|source| serde_json::json!(source)),
                Some("select a current opening-credit or closing-credit segment as appropriate"),
                false,
            ));
        }
    }
}

async fn analyze_artifact(
    target: DistributionTarget,
    record: &ArtifactRecord,
    file_hash: &str,
    sidecars: &SidecarPair,
) -> Result<ArtifactEvidence, ServiceError> {
    let path = PathBuf::from(&record.artifact.path);
    if !path.is_file() {
        return Ok(ArtifactEvidence {
            channels: Vec::new(),
            duration_milliseconds: None,
            findings: vec![quality_finding(
                "file_missing",
                QualityFindingStatus::Fail,
                format!("technical.file.{}", record.artifact.id),
                "export artifact file is missing",
                Some(serde_json::json!(record.artifact.id)),
                Some(serde_json::json!("readable file")),
                Some("re-export the audiobook before rerunning quality control"),
                false,
            )],
        });
    }
    let mut findings = Vec::new();
    if record
        .artifact
        .fingerprint
        .algorithm
        .eq_ignore_ascii_case("blake3")
        && file_hash != record.artifact.fingerprint.digest
    {
        findings.push(quality_finding(
            "file_fingerprint_changed",
            QualityFindingStatus::Fail,
            format!("technical.file.{}", record.artifact.id),
            "export artifact changed after it was registered",
            Some(serde_json::json!(file_hash)),
            Some(serde_json::json!(record.artifact.fingerprint.digest)),
            Some("re-export or restore the original artifact"),
            false,
        ));
    }
    let extension = path
        .extension()
        .and_then(std::ffi::OsStr::to_str)
        .unwrap_or_default()
        .to_ascii_lowercase();
    let rules = behavior(target);
    if !rules.allowed_extensions.contains(&extension.as_str()) {
        findings.push(quality_finding(
            "file_extension_unexpected",
            QualityFindingStatus::Fail,
            format!("technical.file.{}", record.artifact.id),
            "file extension is not supported by the selected distribution target",
            Some(serde_json::json!(extension)),
            Some(serde_json::json!(rules.allowed_extensions)),
            Some("re-export in a supported target format"),
            false,
        ));
    }
    let media = analyze_media_file(&path, record, rules, sidecars).await?;
    findings.extend(media.findings);
    if target == DistributionTarget::Acx {
        findings.push(unverified_manual_finding(
            "acx_noise_floor_manual_review",
            format!("manual.file.{}.noise_floor", record.artifact.id),
            "ACX noise floor is below -60 dB RMS",
            "verify the noise floor with an ACX-compatible meter before submission",
        ));
        findings.push(unverified_manual_finding(
            "acx_boundary_room_tone_manual_review",
            format!("manual.file.{}.room_tone", record.artifact.id),
            "ACX boundary room tone does not exceed five seconds and ideally lasts one to five seconds",
            "listen to and measure the opening and closing room tone before submission",
        ));
    } else if target == DistributionTarget::SpotifyForAuthors {
        findings.push(quality_finding(
            "spotify_noise_floor_manual_review",
            QualityFindingStatus::Warning,
            format!("manual.file.{}.noise_floor", record.artifact.id),
            "Spotify recommends a noise floor below -60 dB RMS, which was not independently verified",
            None,
            Some(serde_json::json!({"maximumDbfs": -60.0})),
            Some("review the noise floor with a suitable meter before submission"),
            false,
        ));
    }
    Ok(ArtifactEvidence {
        channels: media.channels,
        duration_milliseconds: media.duration_milliseconds,
        findings,
    })
}

#[derive(Debug)]
struct MediaEvidence {
    channels: Vec<u16>,
    duration_milliseconds: Option<u64>,
    findings: Vec<QualityFinding>,
}

#[derive(Debug)]
enum BoundedMp3Analysis {
    Analyzed(Mp3QcAnalysis),
    TooLarge { size_bytes: u64 },
}

async fn analyze_mp3_bounded(
    path: &FilePath,
    expectations: Mp3QcExpectations,
) -> Result<BoundedMp3Analysis, ServiceError> {
    let size_bytes = tokio::fs::metadata(path).await?.len();
    if size_bytes > MAX_IN_MEMORY_MP3_SCAN_BYTES {
        return Ok(BoundedMp3Analysis::TooLarge { size_bytes });
    }
    let bytes = tokio::fs::read(path).await?;
    Ok(BoundedMp3Analysis::Analyzed(
        analyze_mp3(&bytes, expectations).map_err(internal_error)?,
    ))
}

#[derive(Debug)]
struct PcmDecodeEvidence {
    validity: DecodeValidity,
    analysis: Option<PcmQcAnalysis>,
    malformed_output: Option<String>,
}

fn require_metadata(
    findings: &mut Vec<QualityFinding>,
    code: &'static str,
    label: &'static str,
    present: bool,
) {
    if !present {
        findings.push(quality_finding(
            code,
            QualityFindingStatus::Fail,
            "metadata",
            format!("{label} is required for this distribution target"),
            Some(serde_json::Value::Null),
            Some(serde_json::json!(label)),
            Some("complete the project distribution metadata"),
            false,
        ));
    }
}

fn manual_gate(
    code: &'static str,
    scope: &'static str,
    message: &'static str,
    acknowledged: bool,
    remediation: &'static str,
) -> QualityFinding {
    quality_finding(
        code,
        if acknowledged {
            QualityFindingStatus::Pass
        } else {
            QualityFindingStatus::Manual
        },
        scope,
        message,
        Some(serde_json::json!(acknowledged)),
        Some(serde_json::json!(true)),
        (!acknowledged).then_some(remediation),
        acknowledged,
    )
}

fn unverified_manual_finding(
    code: impl Into<String>,
    scope: impl Into<String>,
    message: impl Into<String>,
    remediation: impl Into<String>,
) -> QualityFinding {
    quality_finding(
        code,
        QualityFindingStatus::Manual,
        scope,
        message,
        None,
        Some(serde_json::json!(true)),
        Some(remediation),
        false,
    )
}

fn report_readiness(findings: &[QualityFinding]) -> (bool, bool) {
    let technical_ready = !findings.iter().any(|finding| {
        finding.status == QualityFindingStatus::Fail && finding.scope.starts_with("technical.")
    });
    let submission_ready = technical_ready
        && !findings
            .iter()
            .any(|finding| finding.status == QualityFindingStatus::Fail)
        && !findings
            .iter()
            .any(|finding| finding.status == QualityFindingStatus::Manual && !finding.acknowledged);
    (technical_ready, submission_ready)
}

#[allow(clippy::too_many_arguments)]
fn quality_finding(
    code: impl Into<String>,
    status: QualityFindingStatus,
    scope: impl Into<String>,
    message: impl Into<String>,
    actual: Option<serde_json::Value>,
    expected: Option<serde_json::Value>,
    remediation: Option<impl Into<String>>,
    acknowledged: bool,
) -> QualityFinding {
    QualityFinding {
        code: code.into(),
        status,
        scope: scope.into(),
        message: message.into(),
        actual,
        expected,
        start_ms: None,
        end_ms: None,
        remediation: remediation.map(Into::into),
        acknowledged,
    }
}

fn resolve_sidecars(state: &AppState) -> Result<SidecarPair, ServiceError> {
    if let Some(packaged_root) = &state.config.bundled_sidecar_dir {
        return SidecarResolver::bundled(packaged_root)
            .resolve()
            .map_err(|error| {
                ServiceError::Conflict(format!(
                    "packaged FFmpeg sidecars failed validation: {error}"
                ))
            });
    }
    let mut directories = vec![state.config.data_dir.join("sidecars").join("bin")];
    if let Ok(executable) = std::env::current_exe()
        && let Some(parent) = executable.parent()
    {
        directories.push(parent.join("sidecars").join("bin"));
        directories.push(parent.join("../Resources/sidecars/bin"));
    }
    let explicit = match (
        std::env::var_os("AUDIOBOOKAI_FFMPEG"),
        std::env::var_os("AUDIOBOOKAI_FFPROBE"),
    ) {
        (Some(ffmpeg), Some(ffprobe)) => Some((PathBuf::from(ffmpeg), PathBuf::from(ffprobe))),
        (None, None) => None,
        _ => {
            return Err(ServiceError::InvalidRequest(
                "set both AUDIOBOOKAI_FFMPEG and AUDIOBOOKAI_FFPROBE".to_owned(),
            ));
        }
    };
    let allow_system = cfg!(debug_assertions)
        || std::env::var("AUDIOBOOKAI_ALLOW_SYSTEM_FFMPEG").is_ok_and(|value| value == "1");
    let mut last_error = None;
    for directory in directories {
        let mut resolver = SidecarResolver::bundled(directory).allow_system_path(allow_system);
        if let Some((ffmpeg, ffprobe)) = &explicit {
            resolver = resolver.explicit(ffmpeg, ffprobe);
        }
        match resolver.resolve() {
            Ok(pair) => return Ok(pair),
            Err(error) => last_error = Some(error),
        }
    }
    Err(ServiceError::Conflict(format!(
        "FFmpeg and ffprobe are unavailable: {}",
        last_error.map_or_else(
            || "no candidate paths were found".to_owned(),
            |error| error.to_string()
        )
    )))
}

async fn ffmpeg_description(sidecars: &SidecarPair) -> Result<(String, String), ServiceError> {
    let output = Command::new(&sidecars.ffmpeg)
        .args(["-hide_banner", "-version"])
        .kill_on_drop(true)
        .output()
        .await?;
    if !output.status.success() {
        return Err(ServiceError::Internal(
            "could not identify the FFmpeg sidecar".to_owned(),
        ));
    }
    let build = String::from_utf8_lossy(&output.stdout)
        .lines()
        .take(2)
        .collect::<Vec<_>>()
        .join("\n");
    let version = build.lines().next().unwrap_or("unknown FFmpeg").to_owned();
    Ok((version, build))
}

async fn hash_file(path: &FilePath) -> Result<String, ServiceError> {
    let mut file = tokio::fs::File::open(path).await?;
    let mut hasher = blake3::Hasher::new();
    let mut buffer = vec![0_u8; 128 * 1024];
    loop {
        let read = file.read(&mut buffer).await?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
    }
    Ok(hasher.finalize().to_hex().to_string())
}

async fn append_cover_findings(
    package: &ExportPackage,
    metadata: &DistributionMetadata,
    artifacts: &ReportArtifactSet,
    findings: &mut Vec<QualityFinding>,
) -> Result<(), ServiceError> {
    let target = package.target;
    if target == DistributionTarget::GenericM4b {
        return Ok(());
    }
    let Some(cover_id) = metadata.cover_artifact_id else {
        require_metadata(findings, "cover_artifact_required", "cover artwork", false);
        return Ok(());
    };
    let Some(record) = artifacts.records.get(&cover_id) else {
        return Ok(());
    };
    if artifacts.invalid_ids.contains(&cover_id) {
        return Ok(());
    }
    let path = FilePath::new(&record.artifact.path);
    if !path.is_file() {
        findings.push(quality_finding(
            "cover_file_missing",
            QualityFindingStatus::Fail,
            "technical.cover",
            "cover artifact file is missing",
            Some(serde_json::json!(cover_id)),
            Some(serde_json::json!("readable cover file")),
            Some("restore or replace the project cover"),
            false,
        ));
        return Ok(());
    }
    let extension = path
        .extension()
        .and_then(std::ffi::OsStr::to_str)
        .unwrap_or_default()
        .to_ascii_lowercase();
    let allowed = match target {
        DistributionTarget::Acx => &["jpg", "jpeg", "png", "tif", "tiff"][..],
        DistributionTarget::SpotifyForAuthors => &["jpg", "jpeg", "png"][..],
        DistributionTarget::GooglePlay => &["jpg", "png"][..],
        DistributionTarget::GenericM4b => &[][..],
    };
    if !allowed.contains(&extension.as_str()) {
        findings.push(quality_finding(
            "cover_format_unexpected",
            QualityFindingStatus::Fail,
            "technical.cover",
            "cover format is not supported by the selected distribution target",
            Some(serde_json::json!(extension)),
            Some(serde_json::json!(allowed)),
            Some("replace the cover with a supported square image"),
            false,
        ));
    }
    let size = tokio::fs::metadata(path).await?.len();
    if target == DistributionTarget::Acx && size > 8_000_000 {
        findings.push(quality_finding(
            "acx_cover_file_too_large",
            QualityFindingStatus::Fail,
            "technical.cover",
            "ACX cover artwork exceeds 8 MB",
            Some(serde_json::json!(size)),
            Some(serde_json::json!({"maxBytes": 8_000_000})),
            Some("optimize the cover image below the ACX size limit"),
            false,
        ));
    }
    findings.push(quality_finding(
        "cover_dimensions_manual_review",
        QualityFindingStatus::Manual,
        "manual.cover",
        "cover dimensions, color space, and visual content still require review",
        None,
        Some(match target {
            DistributionTarget::Acx => {
                serde_json::json!("square RGB artwork at least 2400 x 2400")
            }
            DistributionTarget::SpotifyForAuthors => {
                serde_json::json!("square artwork, 3000 x 3000 recommended")
            }
            DistributionTarget::GooglePlay => {
                serde_json::json!("1024 to 7200 pixels, square recommended")
            }
            DistributionTarget::GenericM4b => serde_json::Value::Null,
        }),
        Some("inspect the cover against the linked current retailer policy"),
        false,
    ));
    Ok(())
}

fn append_sample_artifact_findings(
    package: &ExportPackage,
    artifacts: &ReportArtifactSet,
    findings: &mut Vec<QualityFinding>,
) {
    let mut samples = Vec::new();
    for artifact_id in &package.review_artifact_ids {
        let Some(record) = artifacts.records.get(artifact_id) else {
            continue;
        };
        if artifacts.invalid_ids.contains(artifact_id) {
            continue;
        }
        if record.artifact.kind == ArtifactKind::Preview {
            samples.push(&record.artifact);
        }
    }
    if package.target == DistributionTarget::Acx && samples.is_empty() {
        findings.push(quality_finding(
            "sample_audio_artifact_required",
            QualityFindingStatus::Fail,
            "metadata",
            "a standalone sample audio artifact is required",
            Some(serde_json::json!(0)),
            Some(serde_json::json!({"minimum": 1, "maximumDurationMilliseconds": 300_000})),
            Some("generate and attach a clean sample of no more than five minutes"),
            false,
        ));
    } else if package.target == DistributionTarget::SpotifyForAuthors && samples.is_empty() {
        findings.push(quality_finding(
            "spotify_sample_will_be_generated",
            QualityFindingStatus::Warning,
            "metadata",
            "no custom Spotify retail sample is attached; Spotify may generate one",
            Some(serde_json::json!(0)),
            Some(serde_json::json!(
                "optional custom sample up to five minutes"
            )),
            Some("attach a clean custom sample only if you want to control the preview"),
            false,
        ));
    }
    for sample in samples.into_iter().filter(|_| {
        matches!(
            package.target,
            DistributionTarget::Acx | DistributionTarget::SpotifyForAuthors
        )
    }) {
        if sample.duration_ms.is_none_or(|duration| duration > 300_000) {
            findings.push(quality_finding(
                "sample_audio_duration_invalid",
                QualityFindingStatus::Fail,
                "technical.package",
                "sample audio must have a known duration no greater than five minutes",
                Some(serde_json::json!(sample.duration_ms)),
                Some(serde_json::json!({"maxMilliseconds": 300_000})),
                Some("regenerate a shorter clean sample"),
                false,
            ));
        }
    }
}

async fn decode_pcm_stream(
    invocation: &FfmpegInvocation,
    layout: Option<(u32, u16, PcmQcPolicy)>,
) -> Result<PcmDecodeEvidence, ServiceError> {
    let Some((sample_rate_hz, channels, policy)) = layout else {
        let status = Command::new(&invocation.executable)
            .args(&invocation.arguments)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .kill_on_drop(true)
            .status()
            .await?;
        return Ok(PcmDecodeEvidence {
            validity: if status.success() {
                DecodeValidity::Valid
            } else {
                DecodeValidity::Invalid
            },
            analysis: None,
            malformed_output: None,
        });
    };

    let mut analyzer =
        StreamingPcmQcAnalyzer::new(sample_rate_hz, channels, policy).map_err(internal_error)?;
    let mut child = Command::new(&invocation.executable)
        .args(&invocation.arguments)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .kill_on_drop(true)
        .spawn()?;
    let mut stdout = child
        .stdout
        .take()
        .ok_or_else(|| internal_error("FFmpeg PCM stdout pipe was not created"))?;
    let frame_bytes = usize::from(channels)
        .checked_mul(std::mem::size_of::<f32>())
        .ok_or_else(|| internal_error("decoded PCM frame size overflowed"))?;
    let mut buffer = vec![0_u8; 128 * 1024];
    let mut pending = Vec::with_capacity(buffer.len().saturating_add(frame_bytes));
    loop {
        let read = stdout.read(&mut buffer).await?;
        if read == 0 {
            break;
        }
        pending.extend_from_slice(&buffer[..read]);
        let aligned_len = pending.len() - pending.len() % frame_bytes;
        if aligned_len == 0 {
            continue;
        }
        let samples = decode_f32le(&pending[..aligned_len]).map_err(internal_error)?;
        analyzer.push_samples(&samples).map_err(internal_error)?;
        let remaining = pending.len() - aligned_len;
        pending.copy_within(aligned_len.., 0);
        pending.truncate(remaining);
    }
    let status = child.wait().await?;
    if !status.success() {
        return Ok(PcmDecodeEvidence {
            validity: DecodeValidity::Invalid,
            analysis: None,
            malformed_output: None,
        });
    }
    if !pending.is_empty() {
        return Ok(PcmDecodeEvidence {
            validity: DecodeValidity::Invalid,
            analysis: None,
            malformed_output: Some(format!(
                "decoded f32le PCM ended with {} byte(s) outside a complete frame",
                pending.len()
            )),
        });
    }
    Ok(PcmDecodeEvidence {
        validity: DecodeValidity::Valid,
        analysis: Some(analyzer.finish()),
        malformed_output: None,
    })
}

#[allow(clippy::too_many_lines)]
async fn analyze_media_file(
    path: &FilePath,
    record: &ArtifactRecord,
    rules: PolicyBehavior,
    sidecars: &SidecarPair,
) -> Result<MediaEvidence, ServiceError> {
    let planner = MediaQcPlanner::new(sidecars.clone());
    let plan = planner.plan(path);
    let probe = Command::new(&plan.metadata_probe.executable)
        .args(&plan.metadata_probe.arguments)
        .kill_on_drop(true)
        .output()
        .await?;
    let probe_json = String::from_utf8_lossy(&probe.stdout);
    let parsed = parse_ffprobe_metadata(&probe_json).ok();
    let primary = parsed
        .as_ref()
        .and_then(audiobookai_media::MediaFileMetadata::primary_audio_stream);
    let extension = path
        .extension()
        .and_then(std::ffi::OsStr::to_str)
        .unwrap_or_default()
        .to_ascii_lowercase();
    let expectations = file_expectations(rules, &extension, primary);
    let pcm_policy = PcmQcPolicy {
        silence_threshold_dbfs: -60.0,
        long_silence_min_milliseconds: 10_000,
        // Retailer rules currently use sample peak. Disabling sinc oversampling keeps full-book
        // reruns linear while the offline primitive remains available for targeted analysis.
        true_peak_oversample_factor: 1,
        rms_dbfs: rules.rms_dbfs,
        max_sample_peak_dbfs: rules.max_sample_peak_dbfs,
        max_estimated_true_peak_dbfs: None,
        leading_silence_milliseconds: rules.leading_silence_milliseconds,
        trailing_silence_milliseconds: rules.trailing_silence_milliseconds,
        ..PcmQcPolicy::default()
    };
    let pcm_layout = primary
        .and_then(|stream| stream.sample_rate_hz.zip(stream.channels))
        .map(|(sample_rate_hz, channels)| (sample_rate_hz, channels, pcm_policy));
    let PcmDecodeEvidence {
        validity: decode_validity,
        analysis: pcm_analysis,
        malformed_output,
    } = decode_pcm_stream(&plan.pcm_decode, pcm_layout).await?;
    let file_analysis = analyze_ffprobe_output(&probe_json, decode_validity, &expectations)
        .map_err(internal_error)?;
    let scope = format!("technical.file.{}", record.artifact.id);
    let mut findings = file_analysis
        .findings
        .into_iter()
        .map(|finding| media_finding(finding, &scope, rules))
        .collect::<Vec<_>>();
    let channels = file_analysis
        .metadata
        .as_ref()
        .and_then(audiobookai_media::MediaFileMetadata::primary_audio_stream)
        .and_then(|stream| stream.channels)
        .into_iter()
        .collect::<Vec<_>>();
    let duration_milliseconds = file_analysis.metadata.as_ref().and_then(|metadata| {
        metadata
            .primary_audio_stream()
            .and_then(|stream| stream.duration_milliseconds)
            .or(metadata.duration_milliseconds)
    });
    if let Some(channel_count) = channels.first()
        && !matches!(*channel_count, 1 | 2)
    {
        findings.push(quality_finding(
            "file_channel_count_unsupported",
            QualityFindingStatus::Fail,
            &scope,
            "audiobook audio must be mono or stereo",
            Some(serde_json::json!(channel_count)),
            Some(serde_json::json!([1, 2])),
            Some("re-export as mono or stereo"),
            false,
        ));
    }
    append_target_specific_file_findings(&mut findings, rules, &extension, primary, &scope);

    if let Some(error) = malformed_output {
        findings.push(quality_finding(
            "pcm_decode_alignment_invalid",
            QualityFindingStatus::Fail,
            &scope,
            "decoded PCM output is malformed",
            Some(serde_json::json!(error)),
            Some(serde_json::json!("aligned f32le PCM frames")),
            Some("re-export the audio and rerun quality control"),
            false,
        ));
    }
    if let Some(analysis) = pcm_analysis {
        findings.extend(
            analysis
                .findings
                .into_iter()
                .map(|finding| media_finding(finding, &scope, rules)),
        );
    } else if decode_validity == DecodeValidity::Valid && pcm_layout.is_none() {
        findings.push(quality_finding(
            "pcm_layout_unknown",
            QualityFindingStatus::Fail,
            &scope,
            "decoded PCM could not be measured because its sample rate or channel count is unknown",
            None,
            Some(serde_json::json!("known sample rate and channel count")),
            Some("re-export the audio in a standard supported format"),
            false,
        ));
    }

    if extension == "mp3" {
        let mp3 = analyze_mp3_bounded(
            path,
            Mp3QcExpectations {
                require_cbr: rules.require_cbr || rules.prefer_cbr,
                bitrate_kbps: mp3_bitrate_expectation(rules.target, primary),
                sample_rate_hz: (rules.target == DistributionTarget::Acx).then_some(44_100),
            },
        )
        .await?;
        match mp3 {
            BoundedMp3Analysis::TooLarge { size_bytes } => findings.push(quality_finding(
                "mp3_frame_scan_size_limit_exceeded",
                QualityFindingStatus::Fail,
                &scope,
                "MP3 is too large for the bounded in-memory frame scanner",
                Some(serde_json::json!(size_bytes)),
                Some(serde_json::json!({"maximumBytes": MAX_IN_MEMORY_MP3_SCAN_BYTES})),
                Some("split the MP3 into smaller retailer-compliant files and re-export"),
                false,
            )),
            BoundedMp3Analysis::Analyzed(mp3) => {
                findings.extend(
                    mp3.findings
                        .into_iter()
                        .map(|finding| media_finding(finding, &scope, rules)),
                );
                if rules.prefer_cbr && mp3.frames.status == Mp3CbrStatus::Constant {
                    findings.push(quality_finding(
                        "mp3_cbr_verified",
                        QualityFindingStatus::Pass,
                        &scope,
                        "MP3 bitrate is constant across all parsed frames",
                        Some(serde_json::json!(mp3.frames.bitrates_kbps)),
                        Some(serde_json::json!("constant bitrate")),
                        None::<String>,
                        true,
                    ));
                }
            }
        }
    }
    Ok(MediaEvidence {
        channels,
        duration_milliseconds,
        findings,
    })
}

fn file_expectations(
    rules: PolicyBehavior,
    extension: &str,
    primary: Option<&audiobookai_media::AudioStreamMetadata>,
) -> FileQcExpectations {
    let (containers, codecs) = match extension {
        "mp3" => (vec!["mp3".to_owned()], vec!["mp3".to_owned()]),
        "wav" => (
            vec!["wav".to_owned()],
            if matches!(
                rules.target,
                DistributionTarget::SpotifyForAuthors | DistributionTarget::GooglePlay
            ) {
                vec!["pcm_s16le".to_owned()]
            } else {
                vec!["pcm_s16le".to_owned(), "pcm_s24le".to_owned()]
            },
        ),
        "flac" => (vec!["flac".to_owned()], vec!["flac".to_owned()]),
        "m4a" | "m4b" => (
            vec![
                "mov".to_owned(),
                "mp4".to_owned(),
                "m4a".to_owned(),
                "3gp".to_owned(),
                "3g2".to_owned(),
                "mj2".to_owned(),
            ],
            vec!["aac".to_owned()],
        ),
        "aac" => (vec!["aac".to_owned()], vec!["aac".to_owned()]),
        _ => (Vec::new(), Vec::new()),
    };
    let bitrate_bps = match rules.target {
        DistributionTarget::Acx => Some(QcRangeU64 {
            min: Some(192_000),
            max: None,
        }),
        DistributionTarget::SpotifyForAuthors if extension == "mp3" => Some(QcRangeU64 {
            min: Some(192_000),
            max: None,
        }),
        DistributionTarget::GooglePlay if matches!(extension, "mp3" | "m4a" | "aac") => {
            let mut minimum: u64 = if primary.and_then(|stream| stream.channels) == Some(1) {
                128_000
            } else {
                256_000
            };
            if matches!(extension, "m4a" | "aac")
                && primary.and_then(|stream| stream.channels) == Some(1)
            {
                minimum = minimum.saturating_add(1);
            }
            Some(QcRangeU64 {
                min: Some(minimum),
                max: None,
            })
        }
        DistributionTarget::GenericM4b => Some(QcRangeU64 {
            min: Some(64_000),
            max: None,
        }),
        _ => None,
    };
    FileQcExpectations {
        allowed_containers: containers,
        allowed_audio_codecs: codecs,
        sample_rate_hz: (rules.target == DistributionTarget::Acx).then_some(44_100),
        channels: None,
        bitrate_bps,
        duration_milliseconds: rules.max_duration_milliseconds.map(|maximum| QcRangeU64 {
            min: Some(1),
            max: Some(maximum),
        }),
        require_single_audio_stream: true,
        require_decode_valid: true,
    }
}

fn mp3_bitrate_expectation(
    target: DistributionTarget,
    primary: Option<&audiobookai_media::AudioStreamMetadata>,
) -> Option<QcRangeU64> {
    match target {
        DistributionTarget::Acx | DistributionTarget::SpotifyForAuthors => Some(QcRangeU64 {
            min: Some(192),
            max: None,
        }),
        DistributionTarget::GooglePlay => Some(QcRangeU64 {
            min: Some(if primary.and_then(|stream| stream.channels) == Some(1) {
                128
            } else {
                256
            }),
            max: None,
        }),
        DistributionTarget::GenericM4b => None,
    }
}

fn append_target_specific_file_findings(
    findings: &mut Vec<QualityFinding>,
    rules: PolicyBehavior,
    extension: &str,
    primary: Option<&audiobookai_media::AudioStreamMetadata>,
    scope: &str,
) {
    if rules.target == DistributionTarget::SpotifyForAuthors
        && primary.and_then(|stream| stream.sample_rate_hz) != Some(44_100)
    {
        findings.push(quality_finding(
            "spotify_sample_rate_recommended",
            QualityFindingStatus::Warning,
            scope,
            "Spotify recommends a 44.1 kHz sample rate",
            Some(serde_json::json!(
                primary.and_then(|stream| stream.sample_rate_hz)
            )),
            Some(serde_json::json!(44_100)),
            Some("re-export at 44.1 kHz for the documented preferred profile"),
            false,
        ));
    }
    if rules.target == DistributionTarget::GooglePlay
        && matches!(extension, "wav" | "flac")
        && primary
            .and_then(|stream| stream.sample_rate_hz)
            .is_none_or(|rate| rate < 44_100)
    {
        findings.push(quality_finding(
            "google_lossless_sample_rate_too_low",
            QualityFindingStatus::Fail,
            scope,
            "Google lossless audio requires at least 44.1 kHz",
            Some(serde_json::json!(
                primary.and_then(|stream| stream.sample_rate_hz)
            )),
            Some(serde_json::json!({"minHertz": 44100})),
            Some("re-export the lossless file at 44.1 kHz or higher"),
            false,
        ));
    }
    if extension == "flac" && rules.target == DistributionTarget::GooglePlay {
        findings.push(quality_finding(
            "lossless_bit_depth_manual_review",
            QualityFindingStatus::Manual,
            format!(
                "manual.{}.bit_depth",
                scope.strip_prefix("technical.").unwrap_or(scope)
            ),
            "Google Play FLAC bit depth has not been independently verified",
            None,
            Some(serde_json::json!(16)),
            Some("confirm the FLAC is 16-bit before upload"),
            false,
        ));
    }
}

fn media_finding(finding: QcFinding, scope: &str, rules: PolicyBehavior) -> QualityFinding {
    let recommendation = (rules.target == DistributionTarget::SpotifyForAuthors
        && matches!(
            finding.code,
            QcFindingCode::FileBitrateOutOfRange
                | QcFindingCode::PcmRmsOutOfRange
                | QcFindingCode::PcmSamplePeakExceeded
                | QcFindingCode::PcmLeadingSilenceOutOfRange
                | QcFindingCode::PcmTrailingSilenceOutOfRange
        ))
        || (rules.prefer_cbr
            && matches!(
                finding.code,
                QcFindingCode::Mp3VariableBitrate | QcFindingCode::Mp3CbrUnverified
            ));
    let status = if recommendation
        || (rules.rms_is_recommendation && finding.code == QcFindingCode::PcmRmsOutOfRange)
        || (rules.boundary_silence_is_recommendation
            && matches!(
                finding.code,
                QcFindingCode::PcmLeadingSilenceOutOfRange
                    | QcFindingCode::PcmTrailingSilenceOutOfRange
            )) {
        QualityFindingStatus::Warning
    } else {
        match finding.severity {
            QcSeverity::Info => QualityFindingStatus::Pass,
            QcSeverity::Warning => QualityFindingStatus::Warning,
            QcSeverity::Error => QualityFindingStatus::Fail,
        }
    };
    QualityFinding {
        code: finding_code(finding.code),
        status,
        scope: scope.to_owned(),
        message: finding.message,
        actual: finding
            .actual
            .and_then(|value| serde_json::to_value(value).ok()),
        expected: finding
            .expected
            .and_then(|value| serde_json::to_value(value).ok()),
        start_ms: finding.start_milliseconds,
        end_ms: finding.end_milliseconds,
        remediation: remediation(finding.code).map(str::to_owned),
        acknowledged: status == QualityFindingStatus::Pass,
    }
}

fn finding_code(code: QcFindingCode) -> String {
    serde_json::to_value(code)
        .ok()
        .and_then(|value| value.as_str().map(str::to_owned))
        .unwrap_or_else(|| "media_qc_unknown".to_owned())
}

const fn remediation(code: QcFindingCode) -> Option<&'static str> {
    match code {
        QcFindingCode::DecodeInvalid
        | QcFindingCode::FileMetadataInvalid
        | QcFindingCode::Mp3Invalid => Some("re-export the file and rerun quality control"),
        QcFindingCode::FileContainerUnexpected
        | QcFindingCode::FileCodecUnexpected
        | QcFindingCode::FileSampleRateUnexpected
        | QcFindingCode::FileChannelCountUnexpected
        | QcFindingCode::FileBitrateOutOfRange
        | QcFindingCode::Mp3BitrateOutOfRange
        | QcFindingCode::Mp3SampleRateUnexpected
        | QcFindingCode::Mp3VariableBitrate
        | QcFindingCode::Mp3CbrUnverified => {
            Some("re-export using the selected retailer's encoding profile")
        }
        QcFindingCode::PcmClipping
        | QcFindingCode::PcmRmsOutOfRange
        | QcFindingCode::PcmSamplePeakExceeded
        | QcFindingCode::PcmTruePeakExceeded => {
            Some("remaster the audio levels, then re-export and rerun quality control")
        }
        QcFindingCode::PcmLeadingSilenceOutOfRange
        | QcFindingCode::PcmTrailingSilenceOutOfRange
        | QcFindingCode::PcmLongSilence
        | QcFindingCode::PcmAbruptJoin => {
            Some("review the timestamp, edit the affected boundary, and re-export")
        }
        _ => None,
    }
}

fn render_report_html(report: &QualityReport) -> String {
    let mut rows = String::new();
    for finding in &report.findings {
        rows.push_str("<tr><td>");
        rows.push_str(&escape_html(&finding.code));
        rows.push_str("</td><td>");
        rows.push_str(&escape_html(
            &format!("{:?}", finding.status).to_lowercase(),
        ));
        rows.push_str("</td><td>");
        rows.push_str(&escape_html(&finding.scope));
        rows.push_str("</td><td>");
        rows.push_str(&escape_html(&finding.message));
        rows.push_str("</td></tr>");
    }
    let provenance = serde_json::to_string_pretty(&serde_json::json!({
        "policyDigest": report.policy_digest,
        "policySnapshot": report.policy_snapshot,
        "metadataRevision": report.metadata_revision,
        "metadataDigest": report.metadata_digest,
        "metadataSnapshot": report.metadata_snapshot,
        "projectTitle": report.project_title,
        "packageDigest": report.package_digest,
        "packageSnapshot": report.package_snapshot,
        "exportManifestArtifactId": report.export_manifest_artifact_id,
        "segmentEvidence": report.segment_evidence,
        "analyzerVersion": report.analyzer_version,
        "ffmpegVersion": report.ffmpeg_version,
        "ffmpegBuildFingerprint": report.ffmpeg_build_fingerprint,
        "fileHashes": report.file_hashes,
        "inputDigest": report.input_digest,
    }))
    .unwrap_or_else(|_| "provenance unavailable".to_owned());
    format!(
        "<!doctype html><html lang=\"en\"><head><meta charset=\"utf-8\"><title>AudiobookAI quality report</title>\
         <style>body{{font-family:system-ui,sans-serif;max-width:1100px;margin:2rem auto;padding:0 1rem}}\
         table{{border-collapse:collapse;width:100%}}th,td{{border:1px solid #bbb;padding:.5rem;text-align:left;vertical-align:top}}\
         code{{overflow-wrap:anywhere}}</style></head><body><h1>Quality report</h1>\
         <p><strong>Report:</strong> <code>{}</code><br><strong>Policy:</strong> {}<br>\
         <strong>Technical ready:</strong> {}<br><strong>Submission ready:</strong> {}<br>\
         <strong>Generated:</strong> {}</p><table><thead><tr><th>Code</th><th>Status</th><th>Scope</th><th>Message</th></tr></thead>\
         <tbody>{rows}</tbody></table><details><summary>Reproducibility provenance</summary><pre>{}</pre></details></body></html>",
        escape_html(&report.id.to_string()),
        escape_html(&report.policy.policy_version),
        report.technical_ready,
        report.submission_ready,
        escape_html(&report.generated_at.to_rfc3339()),
        escape_html(&provenance),
    )
}

fn escape_html(value: &str) -> String {
    value
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&#39;")
}

#[allow(clippy::needless_pass_by_value)]
fn storage_error(error: sqlx::Error) -> ServiceError {
    ServiceError::Storage(error.to_string())
}

fn internal_error(error: impl std::fmt::Display) -> ServiceError {
    ServiceError::Internal(error.to_string())
}

fn invalid_stored_data(error: impl std::fmt::Display) -> ServiceError {
    ServiceError::Internal(format!("stored distribution data is invalid: {error}"))
}

#[cfg(test)]
mod tests {
    use audiobookai_core::{
        Book, BookId, BookMetadata, CloudConsent, FileFingerprint, Job, JobKind, JobState, JobUnit,
        JobUnitId, JobUnitKind, JobUnitState, PerformanceSettings, ProductionSegment,
        ProjectSettings, ProjectStatus, ProofingPlan, SegmentReviewState, SegmentSelection,
        SegmentTake, SegmentTakeId, Speaker, TimingSettings,
    };
    use tempfile::TempDir;

    use super::*;

    async fn fixture() -> (TempDir, Arc<AppState>, Project) {
        let directory = tempfile::tempdir().expect("temporary directory");
        let database = audiobookai_storage::Database::open_in(directory.path())
            .await
            .expect("database");
        let now = Utc::now();
        let book_id = BookId::new();
        let project = Project {
            id: ProjectId::new(),
            book_id,
            name: "Fixture Book".to_owned(),
            status: ProjectStatus::Ready,
            metadata: BookMetadata {
                title: "Fixture Book".to_owned(),
                authors: vec!["Fixture Author".to_owned()],
                narrator: Some("Fixture Narrator".to_owned()),
                publisher: Some("Fixture Publisher".to_owned()),
                description: Some("Fixture description".to_owned()),
                language: Some("en".to_owned()),
                identifier: Some("9780000000000".to_owned()),
                ..BookMetadata::default()
            },
            cloud_consent: CloudConsent::default(),
            settings: ProjectSettings::default(),
            character_reviewed_at: None,
            created_at: now,
            updated_at: now,
        };
        let book = Book {
            id: book_id,
            managed_epub_path: directory
                .path()
                .join("fixture.epub")
                .to_string_lossy()
                .into_owned(),
            original_filename: "fixture.epub".to_owned(),
            source_fingerprint: FileFingerprint {
                algorithm: "blake3".to_owned(),
                digest: "00".repeat(32),
                size_bytes: 1,
            },
            epub_version: Some("3".to_owned()),
            metadata: project.metadata.clone(),
            imported_at: now,
        };
        database
            .repositories()
            .projects
            .create_import(&book, &project, &[], &[])
            .await
            .expect("fixture project");
        let state = Arc::new(
            AppState::new(
                crate::ServiceConfig {
                    bind: "127.0.0.1:0".parse().expect("address"),
                    data_dir: directory.path().to_path_buf(),
                    bundled_sidecar_dir: None,
                    tls: None,
                    lan_hostnames: Vec::new(),
                    allow_insecure_lan: false,
                    desktop_bootstrap: false,
                },
                database,
            )
            .await
            .expect("application state"),
        );
        (directory, state, project)
    }

    async fn report_for_current_inputs(
        state: &AppState,
        package: &ExportPackage,
        project: &Project,
        metadata: &DistributionMetadataView,
    ) -> QualityReport {
        let inputs = current_report_inputs(state, package, &metadata.metadata)
            .await
            .expect("current report inputs");
        let policy = policy_view(package.target);
        QualityReport {
            id: QualityReportId::new(),
            package_id: package.id,
            policy: policy.policy_ref(),
            policy_digest: policy_digest(&policy).expect("policy digest"),
            policy_snapshot: Some(serde_json::to_value(policy).expect("policy snapshot")),
            metadata_revision: metadata.revision,
            metadata_digest: metadata_digest(project, &metadata.metadata).expect("metadata digest"),
            metadata_snapshot: Some(metadata.metadata.clone()),
            project_title: Some(project.metadata.title.clone()),
            package_digest: package_digest(package).expect("package digest"),
            package_snapshot: Some(package_input_snapshot(package)),
            export_manifest_artifact_id: inputs.export_manifest_artifact_id,
            segment_evidence: inputs.segment_evidence,
            technical_ready: false,
            submission_ready: false,
            findings: Vec::new(),
            analyzer_version: ANALYZER_VERSION.to_owned(),
            ffmpeg_version: "fixture".to_owned(),
            ffmpeg_build_fingerprint: "00".repeat(32),
            file_hashes: inputs.file_hashes,
            input_digest: inputs.input_digest,
            generated_at: Utc::now(),
        }
    }

    #[test]
    #[allow(clippy::too_many_lines)]
    fn exposes_versioned_machine_readable_policies() {
        let policies = [
            DistributionTarget::GenericM4b,
            DistributionTarget::Acx,
            DistributionTarget::SpotifyForAuthors,
            DistributionTarget::GooglePlay,
        ]
        .map(policy_view);
        assert_eq!(
            policies
                .iter()
                .map(|policy| policy.policy_version.as_str())
                .collect::<BTreeSet<_>>()
                .len(),
            4
        );
        assert!(policies.iter().all(|policy| {
            !policy.rules.is_empty()
                && policy
                    .source_urls
                    .iter()
                    .all(|source| source.starts_with("https://"))
        }));
        let acx = &policies[1];
        assert_eq!(acx.effective_date, date(2026, 4, 15));
        assert!(acx.rules.iter().any(|rule| {
            rule.code == "acx_human_narration_authorization"
                && matches!(rule.level, PolicyRuleLevel::ManualGate)
        }));
        let generic_format = policies[0]
            .rules
            .iter()
            .find(|rule| rule.code == "generic_m4b_container")
            .expect("generic format policy");
        assert_eq!(generic_format.expected["minimumBitrateBps"], 64_000);

        let acx_levels = acx
            .rules
            .iter()
            .find(|rule| rule.code == "acx_audio_levels")
            .expect("ACX levels policy");
        assert!(acx_levels.automated);
        assert!(acx_levels.expected.get("noiseFloorDbfsMaximum").is_none());
        assert!(acx.rules.iter().any(|rule| {
            rule.code == "acx_noise_floor_manual_review"
                && !rule.automated
                && matches!(rule.level, PolicyRuleLevel::ManualGate)
        }));

        let spotify_behavior = behavior(DistributionTarget::SpotifyForAuthors);
        assert_eq!(spotify_behavior.max_sample_peak_dbfs, None);
        assert!(spotify_behavior.require_consistent_channels);
        assert!(policies[2].rules.iter().any(|rule| {
            rule.code == "spotify_retail_sample"
                && matches!(rule.level, PolicyRuleLevel::Recommended)
        }));

        let google = &policies[3];
        assert!(
            google
                .source_urls
                .iter()
                .any(|url| url.contains("answer/3424254"))
        );
        let google_duration = google
            .rules
            .iter()
            .find(|rule| rule.code == "google_total_duration")
            .expect("Google duration policy");
        assert_eq!(
            google_duration.expected["maximumMilliseconds"],
            360_000_000_u64
        );
        let mono = audiobookai_media::AudioStreamMetadata {
            index: 0,
            codec_name: Some("aac".to_owned()),
            sample_rate_hz: Some(44_100),
            channels: Some(1),
            channel_layout: Some("mono".to_owned()),
            bitrate_bps: Some(128_001),
            duration_milliseconds: Some(300_000),
            is_default: true,
        };
        assert_eq!(
            file_expectations(behavior(DistributionTarget::GooglePlay), "mp3", Some(&mono))
                .bitrate_bps
                .and_then(|range| range.min),
            Some(128_000)
        );
        assert_eq!(
            file_expectations(behavior(DistributionTarget::GooglePlay), "m4a", Some(&mono))
                .bitrate_bps
                .and_then(|range| range.min),
            Some(128_001)
        );
        let stereo = audiobookai_media::AudioStreamMetadata {
            channels: Some(2),
            channel_layout: Some("stereo".to_owned()),
            bitrate_bps: Some(256_000),
            ..mono
        };
        assert_eq!(
            file_expectations(
                behavior(DistributionTarget::GooglePlay),
                "m4a",
                Some(&stereo)
            )
            .bitrate_bps
            .and_then(|range| range.min),
            Some(256_000)
        );
        assert!(behavior(DistributionTarget::GooglePlay).prefer_cbr);
    }

    #[test]
    fn separates_technical_and_submission_readiness() {
        let warning = quality_finding(
            "recommendation",
            QualityFindingStatus::Warning,
            "technical.file",
            "recommended improvement",
            None,
            None,
            None::<String>,
            false,
        );
        assert_eq!(report_readiness(&[warning]), (true, true));

        let manual = manual_gate(
            "authorization",
            "manual.authorization",
            "authorization required",
            false,
            "record authorization",
        );
        assert_eq!(report_readiness(&[manual]), (true, false));

        let metadata = quality_finding(
            "missing_metadata",
            QualityFindingStatus::Fail,
            "metadata",
            "metadata missing",
            None,
            None,
            None::<String>,
            false,
        );
        assert_eq!(report_readiness(&[metadata]), (true, false));

        let technical = quality_finding(
            "invalid_audio",
            QualityFindingStatus::Fail,
            "technical.file.1",
            "invalid audio",
            None,
            None,
            None::<String>,
            false,
        );
        assert_eq!(report_readiness(&[technical]), (false, false));
    }

    #[test]
    fn package_requires_the_complete_export_artifact_set() {
        let first = ArtifactId::new();
        let second = ArtifactId::new();
        let partial = BTreeSet::from([first.as_uuid()]);
        assert!(matches!(
            require_complete_export_set(&partial, &[first, second]),
            Err(ServiceError::InvalidRequest(_))
        ));
        let complete = BTreeSet::from([second.as_uuid(), first.as_uuid()]);
        require_complete_export_set(&complete, &[first, second]).expect("complete set");
    }

    #[tokio::test]
    async fn report_detects_artifact_changes_during_analysis() {
        let directory = tempfile::tempdir().expect("temporary directory");
        let path = directory.path().join("changing.m4b");
        tokio::fs::write(&path, b"before")
            .await
            .expect("initial artifact");
        let artifact_id = ArtifactId::new();
        let artifact = Artifact {
            id: artifact_id,
            kind: ArtifactKind::Export,
            path: path.to_string_lossy().into_owned(),
            fingerprint: FileFingerprint {
                algorithm: "blake3".to_owned(),
                digest: blake3::hash(b"before").to_hex().to_string(),
                size_bytes: 6,
            },
            media_type: Some("audio/mp4".to_owned()),
            duration_ms: Some(1_000),
            cache_key: None,
            pinned_by_job_id: None,
            created_at: Utc::now(),
            last_accessed_at: Utc::now(),
        };
        let mut records = BTreeMap::from([(
            artifact_id,
            ArtifactRecord {
                artifact,
                project_id: ProjectId::new(),
                job_id: None,
            },
        )]);
        let initial_hashes = BTreeMap::from([(
            artifact_id.to_string(),
            blake3::hash(b"before").to_hex().to_string(),
        )]);
        tokio::fs::write(&path, b"after")
            .await
            .expect("changed artifact");
        let mut findings = Vec::new();
        append_artifact_stability_findings(&records, &initial_hashes, &mut findings).await;
        assert!(findings.iter().any(|finding| {
            finding.code == "artifact_changed_during_quality_control"
                && finding.status == QualityFindingStatus::Fail
        }));
        records
            .get_mut(&artifact_id)
            .expect("artifact record")
            .artifact
            .fingerprint
            .algorithm = "sha256".to_owned();
        let mut fingerprint_findings = Vec::new();
        let _ = hash_report_artifacts(&records, &mut fingerprint_findings).await;
        assert!(fingerprint_findings.iter().any(|finding| {
            finding.code == "influencing_artifact_fingerprint_algorithm_unsupported"
                && finding.status == QualityFindingStatus::Fail
        }));
    }

    #[tokio::test]
    async fn oversized_mp3_is_rejected_before_file_allocation() {
        let directory = tempfile::tempdir().expect("temporary directory");
        let path = directory.path().join("oversized.mp3");
        let file = tokio::fs::File::create(&path)
            .await
            .expect("sparse MP3 fixture");
        file.set_len(MAX_IN_MEMORY_MP3_SCAN_BYTES.saturating_add(1))
            .await
            .expect("sparse MP3 length");
        let result = analyze_mp3_bounded(&path, Mp3QcExpectations::default())
            .await
            .expect("bounded MP3 admission");
        assert!(matches!(
            result,
            BoundedMp3Analysis::TooLarge { size_bytes }
                if size_bytes == MAX_IN_MEMORY_MP3_SCAN_BYTES.saturating_add(1)
        ));
    }

    #[tokio::test]
    async fn retailer_disclosure_and_authorization_are_explicit_manual_gates() {
        let (_directory, _state, project) = fixture().await;
        let metadata = default_metadata(&project);
        let acx_package = ExportPackage {
            id: ExportPackageId::new(),
            project_id: project.id,
            job_id: JobId::new(),
            target: DistributionTarget::Acx,
            output_directory: "/exports".to_owned(),
            upload_artifact_ids: Vec::new(),
            review_artifact_ids: Vec::new(),
            quality_report_id: None,
            created_at: Utc::now(),
        };
        let artifacts = ReportArtifactSet {
            records: BTreeMap::new(),
            invalid_ids: BTreeSet::new(),
            export_manifest_artifact_id: None,
            segment_evidence: Vec::new(),
        };
        let acx = metadata_findings(&acx_package, &project, &metadata, &artifacts)
            .await
            .expect("ACX findings");
        let authorization = acx
            .iter()
            .find(|finding| finding.code == "acx_external_authorization")
            .expect("authorization gate");
        assert_eq!(authorization.status, QualityFindingStatus::Manual);
        assert!(!authorization.acknowledged);

        let spotify_package = ExportPackage {
            target: DistributionTarget::SpotifyForAuthors,
            ..acx_package
        };
        let spotify = metadata_findings(&spotify_package, &project, &metadata, &artifacts)
            .await
            .expect("Spotify findings");
        let disclosure = spotify
            .iter()
            .find(|finding| finding.code == "spotify_digital_voice_disclosure")
            .expect("disclosure gate");
        assert_eq!(disclosure.status, QualityFindingStatus::Manual);
        assert!(!disclosure.acknowledged);
    }

    #[tokio::test]
    async fn metadata_put_is_normalized_and_optimistically_revisioned() {
        let (_directory, state, project) = fixture().await;
        let initial = get_metadata(State(Arc::clone(&state)), Path(project.id.as_uuid()))
            .await
            .expect("default metadata")
            .0;
        assert_eq!(initial.revision, 0);
        assert_eq!(initial.metadata.authors, ["Fixture Author"]);

        let mut metadata = initial.metadata;
        metadata.authors = vec![
            " Fixture Author ".to_owned(),
            "fixture author".to_owned(),
            "Second Author".to_owned(),
        ];
        metadata.subtitle = Some("  Subtitle  ".to_owned());
        let saved = put_metadata(
            State(Arc::clone(&state)),
            Path(project.id.as_uuid()),
            Json(PutDistributionMetadataInput {
                expected_revision: 0,
                metadata: metadata.clone(),
            }),
        )
        .await
        .expect("saved metadata")
        .0;
        assert_eq!(saved.revision, 1);
        assert_eq!(saved.metadata.subtitle.as_deref(), Some("Subtitle"));
        assert_eq!(saved.metadata.authors, ["Fixture Author", "Second Author"]);

        let stale_error = put_metadata(
            State(state),
            Path(project.id.as_uuid()),
            Json(PutDistributionMetadataInput {
                expected_revision: 0,
                metadata,
            }),
        )
        .await
        .expect_err("stale metadata must fail");
        assert!(matches!(
            stale_error,
            ServiceError::ConflictDetails {
                code: "stale_distribution_metadata",
                ..
            }
        ));
    }

    #[tokio::test]
    async fn concurrent_initial_metadata_writes_use_atomic_compare_and_swap() {
        let (_directory, state, project) = fixture().await;
        let first_state = Arc::clone(&state);
        let second_state = Arc::clone(&state);
        let first_metadata = default_metadata(&project);
        let mut second_metadata = default_metadata(&project);
        second_metadata.subtitle = Some("Concurrent alternative".to_owned());
        let (first, second) = tokio::join!(
            put_metadata(
                State(first_state),
                Path(project.id.as_uuid()),
                Json(PutDistributionMetadataInput {
                    expected_revision: 0,
                    metadata: first_metadata,
                }),
            ),
            put_metadata(
                State(second_state),
                Path(project.id.as_uuid()),
                Json(PutDistributionMetadataInput {
                    expected_revision: 0,
                    metadata: second_metadata,
                }),
            )
        );
        assert_eq!(usize::from(first.is_ok()) + usize::from(second.is_ok()), 1);
        let conflict = first.err().or_else(|| second.err()).expect("one conflict");
        assert!(matches!(
            conflict,
            ServiceError::ConflictDetails {
                code: "stale_distribution_metadata",
                ..
            }
        ));
        let stored = load_metadata_view(&state, &project)
            .await
            .expect("stored metadata");
        assert_eq!(stored.revision, 1);
    }

    #[tokio::test]
    async fn metadata_rejects_superseded_proofing_segment_references() {
        let (_directory, state, project) = fixture().await;
        let now = Utc::now();
        let segment = ProductionSegment {
            id: audiobookai_core::SegmentId::new(),
            project_id: project.id,
            chapter_id: None,
            paragraph_id: None,
            source: ProductionSegmentSource::OpeningCredit,
            stable_key: "opening-credit".to_owned(),
            ordinal: 0,
            source_content_hash: "source".to_owned(),
            byte_start: None,
            byte_end: None,
            speaker: Speaker::Narrator,
            original_text: "Fixture Book, written by Fixture Author".to_owned(),
            narration_text_override: None,
            effective_text: "Fixture Book, written by Fixture Author".to_owned(),
            context_before: None,
            context_after: None,
            performance_override: PerformanceSettings::default(),
            timing_override: TimingSettings::default(),
            expected_input_hash: "expected".to_owned(),
            review_state: SegmentReviewState::Approved,
            active: false,
            revision: 0,
            created_at: now,
            updated_at: now,
        };
        sqlx::query(
            "INSERT INTO production_segments \
             (id, project_id, chapter_id, paragraph_id, source_kind, stable_key, ordinal, \
              source_content_hash, byte_start, byte_end, speaker_key, expected_input_hash, active, \
              review_state, revision, created_at, updated_at, payload) \
             VALUES (?, ?, NULL, NULL, 'opening_credit', ?, 0, ?, NULL, NULL, ?, ?, 0, \
                     'approved', 0, ?, ?, ?)",
        )
        .bind(segment.id.to_string())
        .bind(project.id.to_string())
        .bind(&segment.stable_key)
        .bind(&segment.source_content_hash)
        .bind(serde_json::to_string(&segment.speaker).expect("speaker JSON"))
        .bind(&segment.expected_input_hash)
        .bind(now.to_rfc3339())
        .bind(now.to_rfc3339())
        .bind(serde_json::to_string(&segment).expect("segment JSON"))
        .execute(state.database.pool())
        .await
        .expect("superseded segment");
        let mut metadata = default_metadata(&project);
        metadata.opening_credit_segment_ids = vec![segment.id];
        let error = validate_metadata(&state, project.id, &metadata)
            .await
            .expect_err("superseded references must be rejected");
        assert!(
            matches!(error, ServiceError::InvalidRequest(detail) if detail.contains("superseded"))
        );
    }

    #[tokio::test]
    async fn package_job_admission_requires_completed_project_export() {
        let (_directory, state, project) = fixture().await;
        let now = Utc::now();
        let mut job = Job {
            id: JobId::new(),
            project_id: project.id,
            kind: JobKind::Export,
            state: JobState::Running,
            export_profile_id: None,
            reservation_id: None,
            progress_completed: 0,
            progress_total: 1,
            status_message: Some("running".to_owned()),
            allow_budget_override: false,
            created_at: now,
            started_at: Some(now),
            finished_at: None,
            updated_at: now,
            revision: 0,
        };
        state
            .database
            .repositories()
            .jobs
            .insert(&job)
            .await
            .expect("running export job");
        assert!(matches!(
            load_completed_distribution_job(&state, job.id, project.id).await,
            Err(ServiceError::Conflict(_))
        ));

        job.id = JobId::new();
        job.kind = JobKind::Preview;
        job.state = JobState::Completed;
        job.progress_completed = 1;
        job.finished_at = Some(now);
        state
            .database
            .repositories()
            .jobs
            .insert(&job)
            .await
            .expect("completed preview job");
        assert!(matches!(
            load_completed_distribution_job(&state, job.id, project.id).await,
            Err(ServiceError::InvalidRequest(_))
        ));
    }

    #[tokio::test]
    #[allow(clippy::too_many_lines)]
    async fn report_freshness_and_persistence_track_live_proof_revisions() {
        let (directory, state, project) = fixture().await;
        let now = Utc::now();
        let job = Job {
            id: JobId::new(),
            project_id: project.id,
            kind: JobKind::Conversion,
            state: JobState::Completed,
            export_profile_id: None,
            reservation_id: None,
            progress_completed: 1,
            progress_total: 1,
            status_message: Some("complete".to_owned()),
            allow_budget_override: false,
            created_at: now,
            started_at: Some(now),
            finished_at: Some(now),
            updated_at: now,
            revision: 0,
        };
        state
            .database
            .repositories()
            .jobs
            .insert(&job)
            .await
            .expect("proof source job");
        let segment = ProductionSegment {
            id: audiobookai_core::SegmentId::new(),
            project_id: project.id,
            chapter_id: None,
            paragraph_id: None,
            source: ProductionSegmentSource::EpubRange,
            stable_key: "proof-revision-fixture".to_owned(),
            ordinal: 0,
            source_content_hash: "source".to_owned(),
            byte_start: None,
            byte_end: None,
            speaker: Speaker::Narrator,
            original_text: "Proof revision fixture".to_owned(),
            narration_text_override: None,
            effective_text: "Proof revision fixture".to_owned(),
            context_before: None,
            context_after: None,
            performance_override: PerformanceSettings::default(),
            timing_override: TimingSettings::default(),
            expected_input_hash: "expected".to_owned(),
            review_state: SegmentReviewState::Approved,
            active: true,
            revision: 0,
            created_at: now,
            updated_at: now,
        };
        let unit = JobUnit {
            id: JobUnitId::new(),
            job_id: job.id,
            kind: JobUnitKind::SynthesisSegment,
            state: JobUnitState::Completed,
            chapter_id: None,
            segment_id: Some(segment.id),
            provider_profile_id: None,
            dependencies: Vec::new(),
            attempt_count: 1,
            next_attempt_at: None,
            output_artifact_id: None,
            payload: BTreeMap::new(),
            created_at: now,
            updated_at: now,
        };
        let plan = ProofingPlan {
            project_id: project.id,
            source_conversion_job_id: job.id,
            plan_revision: 0,
            plan_hash: "plan-0".to_owned(),
            status: ProofingPlanStatus::Ready,
            dirty_reasons: Vec::new(),
            created_at: now,
            updated_at: now,
        };
        state
            .database
            .repositories()
            .proofing
            .replace_plan_with_units(
                &plan,
                std::slice::from_ref(&segment),
                std::slice::from_ref(&unit),
            )
            .await
            .expect("proof plan");

        let take_path = directory.path().join("proof-take.flac");
        tokio::fs::write(&take_path, b"proof take")
            .await
            .expect("proof take file");
        let take_artifact = Artifact {
            id: ArtifactId::new(),
            kind: ArtifactKind::SegmentAudio,
            path: take_path.to_string_lossy().into_owned(),
            fingerprint: FileFingerprint {
                algorithm: "blake3".to_owned(),
                digest: blake3::hash(b"proof take").to_hex().to_string(),
                size_bytes: 10,
            },
            media_type: Some("audio/flac".to_owned()),
            duration_ms: Some(1_000),
            cache_key: None,
            pinned_by_job_id: Some(job.id),
            created_at: now,
            last_accessed_at: now,
        };
        sqlx::query(
            "INSERT INTO artifacts \
             (id, project_id, kind, path, cache_key, pinned_by_job_id, created_at, last_accessed_at, payload) \
             VALUES (?, ?, 'segment_audio', ?, NULL, ?, ?, ?, ?)",
        )
        .bind(take_artifact.id.to_string())
        .bind(project.id.to_string())
        .bind(&take_artifact.path)
        .bind(job.id.to_string())
        .bind(now.to_rfc3339())
        .bind(now.to_rfc3339())
        .bind(serde_json::to_string(&take_artifact).expect("take artifact JSON"))
        .execute(state.database.pool())
        .await
        .expect("take artifact");
        let take = SegmentTake {
            id: SegmentTakeId::new(),
            segment_id: segment.id,
            artifact_id: take_artifact.id,
            ordinal: 1,
            source_job_id: job.id,
            source_job_unit_id: unit.id,
            semantic_input_hash: segment.expected_input_hash.clone(),
            duration_ms: 1_000,
            provider_profile_id: None,
            model: None,
            voice_profile_id: None,
            dictionary_revision_hash: "dictionary".to_owned(),
            normalization_version: "fixture".to_owned(),
            synthesis_provenance: BTreeMap::new(),
            findings: Vec::new(),
            created_at: now,
        };
        let selection = SegmentSelection {
            segment_id: segment.id,
            take_id: take.id,
            selected_at: now,
            revision: 0,
        };
        state
            .database
            .repositories()
            .proofing
            .insert_take_and_select(&take, &selection)
            .await
            .expect("selected take");

        let mut metadata = load_metadata_view(&state, &project)
            .await
            .expect("distribution metadata");
        metadata.metadata.sample_segment_ids = vec![segment.id];
        let package = ExportPackage {
            id: ExportPackageId::new(),
            project_id: project.id,
            job_id: job.id,
            target: DistributionTarget::GenericM4b,
            output_directory: directory.path().to_string_lossy().into_owned(),
            upload_artifact_ids: Vec::new(),
            review_artifact_ids: Vec::new(),
            quality_report_id: None,
            created_at: now,
        };
        let report = report_for_current_inputs(&state, &package, &project, &metadata).await;
        assert_eq!(report.segment_evidence[0].segment_revision, Some(0));
        assert_eq!(
            report.segment_evidence[0].plan_source_conversion_job_id,
            Some(job.id)
        );
        assert_eq!(
            report.segment_evidence[0].plan_status,
            Some(ProofingPlanStatus::Ready)
        );
        assert_eq!(report.segment_evidence[0].selection_revision, Some(0));
        assert!(
            latest_report_is_current_locked(&state, &package, &project, &metadata, Some(&report),)
                .await
                .expect("initial proof currentness")
        );

        state
            .database
            .repositories()
            .proofing
            .select_take(&selection, 0)
            .await
            .expect("selection revision");
        assert!(
            !latest_report_is_current_locked(&state, &package, &project, &metadata, Some(&report),)
                .await
                .expect("selection currentness")
        );
        let persist_error =
            persist_quality_report(&state, package.clone(), &project, &metadata, &report)
                .await
                .expect_err("stale proof report must not persist");
        assert!(matches!(
            persist_error,
            ServiceError::ConflictDetails {
                code: "quality_report_inputs_changed",
                ..
            }
        ));

        let report = report_for_current_inputs(&state, &package, &project, &metadata).await;
        let mut changed_segment = segment.clone();
        changed_segment.review_state = SegmentReviewState::Flagged;
        changed_segment.updated_at = Utc::now();
        state
            .database
            .repositories()
            .proofing
            .update_segment(&changed_segment, 0)
            .await
            .expect("segment revision");
        assert!(
            !latest_report_is_current_locked(&state, &package, &project, &metadata, Some(&report),)
                .await
                .expect("segment currentness")
        );

        let report = report_for_current_inputs(&state, &package, &project, &metadata).await;
        let mut changed_plan = plan;
        changed_plan.plan_revision = 1;
        changed_plan.plan_hash = "plan-1".to_owned();
        changed_plan.updated_at = Utc::now();
        state
            .database
            .repositories()
            .proofing
            .update_plan(&changed_plan, 0)
            .await
            .expect("plan revision");
        assert!(
            latest_report_is_current_locked(&state, &package, &project, &metadata, Some(&report),)
                .await
                .expect("unrelated plan revision currentness")
        );
        changed_plan.plan_revision = 2;
        changed_plan.status = ProofingPlanStatus::Dirty;
        changed_plan.updated_at = Utc::now();
        state
            .database
            .repositories()
            .proofing
            .update_plan(&changed_plan, 1)
            .await
            .expect("plan status");
        assert!(
            !latest_report_is_current_locked(&state, &package, &project, &metadata, Some(&report),)
                .await
                .expect("plan status currentness")
        );
    }

    #[tokio::test]
    #[allow(clippy::too_many_lines)]
    async fn package_registration_is_idempotent_and_reports_are_durable() {
        let (directory, state, project) = fixture().await;
        let now = Utc::now();
        let job = Job {
            id: JobId::new(),
            project_id: project.id,
            kind: JobKind::Conversion,
            state: JobState::Completed,
            export_profile_id: None,
            reservation_id: None,
            progress_completed: 1,
            progress_total: 1,
            status_message: Some("complete".to_owned()),
            allow_budget_override: false,
            created_at: now,
            started_at: Some(now),
            finished_at: Some(now),
            updated_at: now,
            revision: 0,
        };
        state
            .database
            .repositories()
            .jobs
            .insert(&job)
            .await
            .expect("job");
        let export_path = directory.path().join("fixture.m4b");
        tokio::fs::write(&export_path, b"fixture export")
            .await
            .expect("export fixture");
        let artifact_id = ArtifactId::new();
        let digest = blake3::hash(b"fixture export").to_hex().to_string();
        let artifact = Artifact {
            id: artifact_id,
            kind: ArtifactKind::Export,
            path: export_path.to_string_lossy().into_owned(),
            fingerprint: FileFingerprint {
                algorithm: "blake3".to_owned(),
                digest,
                size_bytes: 14,
            },
            media_type: Some("audio/mp4".to_owned()),
            duration_ms: Some(1_000),
            cache_key: None,
            pinned_by_job_id: Some(job.id),
            created_at: now,
            last_accessed_at: now,
        };
        sqlx::query(
            "INSERT INTO artifacts \
             (id, project_id, kind, path, cache_key, pinned_by_job_id, created_at, last_accessed_at, payload) \
             VALUES (?, ?, 'export', ?, NULL, ?, ?, ?, ?)",
        )
        .bind(artifact.id.to_string())
        .bind(project.id.to_string())
        .bind(&artifact.path)
        .bind(job.id.to_string())
        .bind(now.to_rfc3339())
        .bind(now.to_rfc3339())
        .bind(serde_json::to_string(&artifact).expect("artifact JSON"))
        .execute(state.database.pool())
        .await
        .expect("artifact");
        let manifest_path = directory.path().join("export-manifest.json");
        let manifest_bytes = serde_json::to_vec(&serde_json::json!({
            "jobId": job.id,
            "outputFiles": [artifact.path.clone()]
        }))
        .expect("manifest JSON");
        tokio::fs::write(&manifest_path, &manifest_bytes)
            .await
            .expect("manifest fixture");
        let manifest = Artifact {
            id: ArtifactId::new(),
            kind: ArtifactKind::ExportManifest,
            path: manifest_path.to_string_lossy().into_owned(),
            fingerprint: FileFingerprint {
                algorithm: "blake3".to_owned(),
                digest: blake3::hash(&manifest_bytes).to_hex().to_string(),
                size_bytes: u64::try_from(manifest_bytes.len()).expect("manifest size"),
            },
            media_type: Some("application/json".to_owned()),
            duration_ms: None,
            cache_key: None,
            pinned_by_job_id: Some(job.id),
            created_at: now,
            last_accessed_at: now,
        };
        sqlx::query(
            "INSERT INTO artifacts \
             (id, project_id, kind, path, cache_key, pinned_by_job_id, created_at, last_accessed_at, payload) \
             VALUES (?, ?, 'export_manifest', ?, NULL, ?, ?, ?, ?)",
        )
        .bind(manifest.id.to_string())
        .bind(project.id.to_string())
        .bind(&manifest.path)
        .bind(job.id.to_string())
        .bind(now.to_rfc3339())
        .bind(now.to_rfc3339())
        .bind(serde_json::to_string(&manifest).expect("manifest artifact JSON"))
        .execute(state.database.pool())
        .await
        .expect("manifest artifact");

        let input = CreatePackageInput {
            target: DistributionTarget::GenericM4b,
            upload_artifact_ids: vec![artifact.id.as_uuid()],
            review_artifact_ids: Vec::new(),
        };
        let created = create_package(
            State(Arc::clone(&state)),
            Path(project.id.as_uuid()),
            Json(input),
        )
        .await
        .expect("package");
        assert_eq!(created.0, StatusCode::CREATED);
        let package = created.1.0.package;
        assert_eq!(package.review_artifact_ids, [manifest.id]);
        let repeated = create_package(
            State(Arc::clone(&state)),
            Path(project.id.as_uuid()),
            Json(CreatePackageInput {
                target: DistributionTarget::GenericM4b,
                upload_artifact_ids: vec![artifact.id.as_uuid()],
                review_artifact_ids: Vec::new(),
            }),
        )
        .await
        .expect("idempotent package");
        assert_eq!(repeated.0, StatusCode::OK);
        assert_eq!(repeated.1.0.package.id, package.id);

        let alternate_path = directory.path().join("review.mp3");
        tokio::fs::write(&alternate_path, b"review audio")
            .await
            .expect("review fixture");
        let alternate = Artifact {
            id: ArtifactId::new(),
            kind: ArtifactKind::Preview,
            path: alternate_path.to_string_lossy().into_owned(),
            fingerprint: FileFingerprint {
                algorithm: "blake3".to_owned(),
                digest: blake3::hash(b"review audio").to_hex().to_string(),
                size_bytes: 12,
            },
            pinned_by_job_id: None,
            ..artifact.clone()
        };
        sqlx::query(
            "INSERT INTO artifacts \
             (id, project_id, kind, path, cache_key, pinned_by_job_id, created_at, last_accessed_at, payload) \
             VALUES (?, ?, 'preview', ?, NULL, NULL, ?, ?, ?)",
        )
        .bind(alternate.id.to_string())
        .bind(project.id.to_string())
        .bind(&alternate.path)
        .bind(now.to_rfc3339())
        .bind(now.to_rfc3339())
        .bind(serde_json::to_string(&alternate).expect("alternate artifact JSON"))
        .execute(state.database.pool())
        .await
        .expect("alternate artifact");
        let conflicting = create_package(
            State(Arc::clone(&state)),
            Path(project.id.as_uuid()),
            Json(CreatePackageInput {
                target: DistributionTarget::GenericM4b,
                upload_artifact_ids: vec![artifact.id.as_uuid()],
                review_artifact_ids: vec![alternate.id.as_uuid()],
            }),
        )
        .await
        .expect_err("same job and target with different inputs must conflict");
        assert!(matches!(conflicting, ServiceError::Conflict(_)));

        let second_target = create_package(
            State(Arc::clone(&state)),
            Path(project.id.as_uuid()),
            Json(CreatePackageInput {
                target: DistributionTarget::Acx,
                upload_artifact_ids: vec![artifact.id.as_uuid()],
                review_artifact_ids: Vec::new(),
            }),
        )
        .await
        .expect("same export can be evaluated for another target");
        assert_eq!(second_target.0, StatusCode::CREATED);
        assert_ne!(second_target.1.0.package.id, package.id);

        let finding = quality_finding(
            "fixture_warning",
            QualityFindingStatus::Warning,
            "technical.file",
            "<script>not executable</script>",
            None,
            None,
            None::<String>,
            false,
        );
        let current_metadata = load_metadata_view(&state, &project)
            .await
            .expect("current metadata");
        let report_inputs = current_report_inputs(&state, &package, &current_metadata.metadata)
            .await
            .expect("current report inputs");
        let active_policy = policy_view(package.target);
        let report = QualityReport {
            id: QualityReportId::new(),
            package_id: package.id,
            policy: active_policy.policy_ref(),
            policy_digest: policy_digest(&active_policy).expect("policy digest"),
            policy_snapshot: Some(serde_json::to_value(active_policy).expect("policy snapshot")),
            metadata_revision: current_metadata.revision,
            metadata_digest: metadata_digest(&project, &current_metadata.metadata)
                .expect("metadata digest"),
            metadata_snapshot: Some(current_metadata.metadata.clone()),
            project_title: Some(project.metadata.title.clone()),
            package_digest: package_digest(&package).expect("package digest"),
            package_snapshot: Some(package_input_snapshot(&package)),
            export_manifest_artifact_id: report_inputs.export_manifest_artifact_id,
            segment_evidence: report_inputs.segment_evidence,
            technical_ready: true,
            submission_ready: true,
            findings: vec![finding],
            analyzer_version: ANALYZER_VERSION.to_owned(),
            ffmpeg_version: "fixture".to_owned(),
            ffmpeg_build_fingerprint: "00".repeat(32),
            file_hashes: report_inputs.file_hashes,
            input_digest: report_inputs.input_digest,
            generated_at: now,
        };
        let updated = persist_quality_report(&state, package, &project, &current_metadata, &report)
            .await
            .expect("report");
        assert_eq!(updated.quality_report_id, Some(report.id));
        assert!(
            latest_report_is_current_locked(
                &state,
                &updated,
                &project,
                &current_metadata,
                Some(&report),
            )
            .await
            .expect("current report comparison")
        );
        tokio::fs::write(&export_path, b"mutated export")
            .await
            .expect("mutate influencing artifact");
        assert!(
            !latest_report_is_current_locked(
                &state,
                &updated,
                &project,
                &current_metadata,
                Some(&report),
            )
            .await
            .expect("mutated artifact comparison")
        );
        tokio::fs::write(&export_path, b"fixture export")
            .await
            .expect("restore influencing artifact");
        let stale_metadata = DistributionMetadataView {
            revision: current_metadata.revision.saturating_add(1),
            ..current_metadata.clone()
        };
        assert!(
            !latest_report_is_current_locked(
                &state,
                &updated,
                &project,
                &stale_metadata,
                Some(&report),
            )
            .await
            .expect("stale report comparison")
        );
        let reports = load_reports_for_package(&state, updated.id)
            .await
            .expect("reports");
        assert_eq!(reports.as_slice(), std::slice::from_ref(&report));
        let html = render_report_html(&report);
        assert!(html.contains("&lt;script&gt;not executable&lt;/script&gt;"));
        assert!(!html.contains("<script>"));
    }
}
