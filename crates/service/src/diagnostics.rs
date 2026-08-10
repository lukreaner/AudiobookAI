//! Privacy-preserving application diagnostics.
//!
//! The diagnostics boundary is deliberately stricter than ordinary logging:
//! event messages must be explicitly approved and structured fields are kept
//! only when both their names and value shapes are allowlisted. Request and
//! response bodies, book text, reference audio, credentials, headers, cookies,
//! URLs, and provider payloads never enter this subsystem.

use std::{
    collections::{BTreeMap, VecDeque},
    fs::{self, File, OpenOptions},
    io::{BufRead, BufReader, Write},
    path::{Path, PathBuf},
    sync::{Mutex, OnceLock},
    time::Instant,
};

use axum::{body::Body, extract::MatchedPath, http::Request, middleware::Next, response::Response};
use chrono::{DateTime, Utc};
use regex::Regex;
use serde::{Deserialize, Serialize};
use tracing::{Event, Subscriber, field::Visit};
use tracing_subscriber::{Layer, layer::Context};

const DEFAULT_CAPACITY: usize = 2_000;
const DEFAULT_FILE_BYTES: u64 = 2 * 1024 * 1024;
const DEFAULT_FILE_GENERATIONS: usize = 5;
const DEFAULT_QUERY_LIMIT: usize = 250;
const MAX_QUERY_LIMIT: usize = 1_000;
const MAX_EXPORT_ENTRIES: usize = 10_000;
const FALLBACK_MESSAGE: &str = "Application diagnostic event";
const REDACTED: &str = "[REDACTED]";

/// Messages must be compile-time, non-content summaries from this list.
/// Unknown tracing messages are replaced rather than heuristically retained.
const SAFE_MESSAGES: &[&str] = &[
    FALLBACK_MESSAGE,
    "HTTP request completed",
    "Service event published",
    "AudiobookAI diagnostics initialized",
    "Diagnostic file persistence is unavailable",
    "opened AudiobookAI database",
    "created pre-migration database backup",
    "desktop service did not shut down cleanly",
    "using software rendering for Linux AppImage compatibility",
    "persisted LAN settings were rejected; using loopback-only recovery mode",
    "persisted LAN listener failed to start; using loopback-only recovery mode",
    "some owned provider children did not stop cleanly",
    "active jobs were checkpointed for shutdown",
    "one or more active jobs could not be checkpointed before shutdown",
    "an app-owned MLX-audio operation did not stop before the shutdown deadline",
    "some app-owned provider model operations did not stop before the shutdown deadline",
    "the local service exceeded its graceful shutdown deadline",
    "character detection failed",
    "service request failed",
    "skipping corrupt optional persisted record",
    "skipping inconsistent optional persisted record",
    "provider was deleted but its orphaned secret reference could not be removed",
    "rotated provider credential but could not remove the old encrypted secret",
    "provider model download failed",
    "provider model download status failed",
    "conversion job failed",
    "could not reconcile conversion budget reservation",
    "could not release completed job cache pins",
    "could not enforce the cache limit after conversion",
    "could not start progressive playback decoder",
    "progressive provider audio decoder stopped",
    "could not release preview cache pins",
    "could not enforce the cache limit after preview",
    "MLX-audio installation completed but its managed profile needs review",
    "MLX-audio management operation failed",
];

const SAFE_EVENT_CODES: &[(&str, &str)] = &[
    ("diagnostics.ready", "AudiobookAI diagnostics initialized"),
    (
        "diagnostics.persistence.unavailable",
        "Diagnostic file persistence is unavailable",
    ),
    ("http.request.completed", "HTTP request completed"),
    ("service.event.published", "Service event published"),
    ("storage.database.opened", "opened AudiobookAI database"),
    (
        "storage.database.backup.created",
        "created pre-migration database backup",
    ),
    (
        "desktop.service.shutdown.failed",
        "desktop service did not shut down cleanly",
    ),
    (
        "desktop.renderer.software",
        "using software rendering for Linux AppImage compatibility",
    ),
    (
        "desktop.lan.settings.rejected",
        "persisted LAN settings were rejected; using loopback-only recovery mode",
    ),
    (
        "desktop.lan.listener.failed",
        "persisted LAN listener failed to start; using loopback-only recovery mode",
    ),
    (
        "provider.shutdown.partial",
        "some owned provider children did not stop cleanly",
    ),
    (
        "jobs.shutdown.checkpointed",
        "active jobs were checkpointed for shutdown",
    ),
    (
        "jobs.shutdown.checkpoint_failed",
        "one or more active jobs could not be checkpointed before shutdown",
    ),
    (
        "mlx.management.shutdown.timeout",
        "an app-owned MLX-audio operation did not stop before the shutdown deadline",
    ),
    (
        "provider.model.shutdown.timeout",
        "some app-owned provider model operations did not stop before the shutdown deadline",
    ),
    (
        "service.shutdown.timeout",
        "the local service exceeded its graceful shutdown deadline",
    ),
    ("detection.failed", "character detection failed"),
    ("service.request.failed", "service request failed"),
    (
        "storage.record.corrupt",
        "skipping corrupt optional persisted record",
    ),
    (
        "storage.record.inconsistent",
        "skipping inconsistent optional persisted record",
    ),
    (
        "provider.secret.cleanup.failed",
        "provider was deleted but its orphaned secret reference could not be removed",
    ),
    (
        "provider.secret.rotation_cleanup.failed",
        "rotated provider credential but could not remove the old encrypted secret",
    ),
    (
        "provider.model.download.failed",
        "provider model download failed",
    ),
    (
        "provider.model.download_status.failed",
        "provider model download status failed",
    ),
    ("conversion.failed", "conversion job failed"),
    (
        "conversion.budget.reconcile.failed",
        "could not reconcile conversion budget reservation",
    ),
    (
        "conversion.cache.unpin.failed",
        "could not release completed job cache pins",
    ),
    (
        "conversion.cache.prune.failed",
        "could not enforce the cache limit after conversion",
    ),
    (
        "playback.decoder.start.failed",
        "could not start progressive playback decoder",
    ),
    (
        "playback.decoder.stopped",
        "progressive provider audio decoder stopped",
    ),
    (
        "preview.cache.unpin.failed",
        "could not release preview cache pins",
    ),
    (
        "preview.cache.prune.failed",
        "could not enforce the cache limit after preview",
    ),
    (
        "mlx.profile.action_required",
        "MLX-audio installation completed but its managed profile needs review",
    ),
    (
        "mlx.management.operation.failed",
        "MLX-audio management operation failed",
    ),
];

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum DiagnosticLevel {
    Trace,
    Debug,
    Info,
    Warn,
    Error,
}

impl DiagnosticLevel {
    const fn priority(self) -> u8 {
        match self {
            Self::Trace => 0,
            Self::Debug => 1,
            Self::Info => 2,
            Self::Warn => 3,
            Self::Error => 4,
        }
    }
}

impl From<&tracing::Level> for DiagnosticLevel {
    fn from(level: &tracing::Level) -> Self {
        match *level {
            tracing::Level::TRACE => Self::Trace,
            tracing::Level::DEBUG => Self::Debug,
            tracing::Level::INFO => Self::Info,
            tracing::Level::WARN => Self::Warn,
            tracing::Level::ERROR => Self::Error,
        }
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DiagnosticEntry {
    pub sequence: u64,
    pub timestamp: DateTime<Utc>,
    pub level: DiagnosticLevel,
    pub target: String,
    pub message: String,
    pub fields: BTreeMap<String, serde_json::Value>,
}

#[derive(Clone, Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DiagnosticQuery {
    /// Minimum severity to include.
    pub level: Option<DiagnosticLevel>,
    pub target: Option<String>,
    pub search: Option<String>,
    pub after: Option<u64>,
    pub limit: Option<usize>,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DiagnosticPage {
    pub items: Vec<DiagnosticEntry>,
    pub total: usize,
    pub latest_sequence: u64,
}

#[derive(Debug)]
struct StoreInner {
    entries: VecDeque<DiagnosticEntry>,
    next_sequence: u64,
    sink: Option<RotatingJsonl>,
}

/// A bounded, thread-safe diagnostic store.
#[derive(Debug)]
pub struct DiagnosticsStore {
    capacity: usize,
    inner: Mutex<StoreInner>,
}

impl DiagnosticsStore {
    pub fn new(capacity: usize) -> Self {
        Self {
            capacity: capacity.max(1),
            inner: Mutex::new(StoreInner {
                entries: VecDeque::with_capacity(capacity.max(1)),
                next_sequence: 1,
                sink: None,
            }),
        }
    }

    /// Enables private, rotating JSONL persistence and re-loads only records
    /// that still pass the current sanitizer.
    ///
    /// # Errors
    ///
    /// Returns an I/O error when the private log directory or current log file
    /// cannot be created. In-memory diagnostics remain available.
    pub fn configure_persistence(&self, data_dir: &Path) -> std::io::Result<()> {
        let log_dir = data_dir.join("logs");
        let mut sink = RotatingJsonl::new(log_dir, DEFAULT_FILE_BYTES, DEFAULT_FILE_GENERATIONS)?;
        let persisted = sink.load_sanitized();
        let mut inner = self
            .inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);

        if inner
            .sink
            .as_ref()
            .is_some_and(|current| current.directory == sink.directory)
        {
            return Ok(());
        }

        let early_entries = inner.entries.iter().cloned().collect::<Vec<_>>();
        inner.entries.clear();
        inner.next_sequence = 1;
        for entry in persisted.into_iter().chain(early_entries.iter().cloned()) {
            let mut entry = sanitize_stored_entry(entry);
            entry.sequence = inner.next_sequence;
            inner.next_sequence = inner.next_sequence.saturating_add(1);
            push_bounded(&mut inner.entries, entry, self.capacity);
        }
        for entry in &early_entries {
            sink.append(entry)?;
        }
        inner.sink = Some(sink);
        Ok(())
    }

    fn record(
        &self,
        level: DiagnosticLevel,
        target: &str,
        message: &str,
        fields: BTreeMap<String, serde_json::Value>,
    ) {
        let mut inner = self
            .inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let entry = DiagnosticEntry {
            sequence: inner.next_sequence,
            timestamp: Utc::now(),
            level,
            target: sanitize_target(target),
            message: approve_message(message).to_owned(),
            fields: sanitize_fields(fields),
        };
        inner.next_sequence = inner.next_sequence.saturating_add(1);
        push_bounded(&mut inner.entries, entry.clone(), self.capacity);
        if let Some(sink) = inner.sink.as_mut() {
            // Diagnostics must never prevent the application from operating.
            // A failed append disables disk logging while the in-memory ring
            // remains available through the authenticated UI.
            if sink.append(&entry).is_err() {
                inner.sink = None;
            }
        }
    }

    pub fn query(&self, query: &DiagnosticQuery) -> DiagnosticPage {
        let inner = self
            .inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let mut items = filtered_entries(&inner.entries, query);
        let total = items.len();
        items.reverse();
        items.truncate(
            query
                .limit
                .unwrap_or(DEFAULT_QUERY_LIMIT)
                .clamp(1, MAX_QUERY_LIMIT),
        );
        DiagnosticPage {
            items,
            total,
            latest_sequence: inner.next_sequence.saturating_sub(1),
        }
    }

    /// Produces a secondarily sanitized, bounded export from memory. Disk log
    /// files are intentionally never served directly.
    pub fn export_jsonl(&self, query: &DiagnosticQuery) -> Vec<u8> {
        let inner = self
            .inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let mut output = Vec::new();
        for entry in filtered_entries(&inner.entries, query)
            .into_iter()
            .rev()
            .take(MAX_EXPORT_ENTRIES)
            .rev()
        {
            let safe = sanitize_stored_entry(entry);
            if serde_json::to_writer(&mut output, &safe).is_ok() {
                output.push(b'\n');
            }
        }
        output
    }
}

fn push_bounded(entries: &mut VecDeque<DiagnosticEntry>, entry: DiagnosticEntry, capacity: usize) {
    if entries.len() == capacity {
        entries.pop_front();
    }
    entries.push_back(entry);
}

fn filtered_entries(
    entries: &VecDeque<DiagnosticEntry>,
    query: &DiagnosticQuery,
) -> Vec<DiagnosticEntry> {
    let target = query
        .target
        .as_deref()
        .map(|value| value.trim().to_ascii_lowercase())
        .filter(|value| !value.is_empty());
    let search = query
        .search
        .as_deref()
        .map(|value| sanitize_search(value).to_ascii_lowercase())
        .filter(|value| !value.is_empty());
    entries
        .iter()
        .filter(|entry| query.after.is_none_or(|after| entry.sequence > after))
        .filter(|entry| {
            query
                .level
                .is_none_or(|minimum| entry.level.priority() >= minimum.priority())
        })
        .filter(|entry| {
            target
                .as_deref()
                .is_none_or(|needle| entry.target.to_ascii_lowercase().contains(needle))
        })
        .filter(|entry| {
            search.as_deref().is_none_or(|needle| {
                entry.message.to_ascii_lowercase().contains(needle)
                    || entry.target.to_ascii_lowercase().contains(needle)
                    || entry.fields.iter().any(|(key, value)| {
                        key.to_ascii_lowercase().contains(needle)
                            || safe_value_display(value)
                                .to_ascii_lowercase()
                                .contains(needle)
                    })
            })
        })
        .cloned()
        .collect()
}

fn sanitize_stored_entry(mut entry: DiagnosticEntry) -> DiagnosticEntry {
    entry.target = sanitize_target(&entry.target);
    entry.fields = sanitize_fields(entry.fields);
    entry.message = entry
        .fields
        .get("diagnosticCode")
        .and_then(serde_json::Value::as_str)
        .and_then(message_for_code)
        .unwrap_or(FALLBACK_MESSAGE)
        .to_owned();
    entry
}

fn sanitize_target(value: &str) -> String {
    static TARGET: OnceLock<Regex> = OnceLock::new();
    let matcher = TARGET.get_or_init(|| Regex::new(r"^[A-Za-z0-9_.:-]{1,128}$").expect("regex"));
    if matcher.is_match(value) {
        value.to_owned()
    } else {
        "audiobookai".to_owned()
    }
}

fn approve_message(value: &str) -> &'static str {
    SAFE_MESSAGES
        .iter()
        .copied()
        .find(|candidate| *candidate == value)
        .unwrap_or(FALLBACK_MESSAGE)
}

fn message_for_code(value: &str) -> Option<&'static str> {
    SAFE_EVENT_CODES
        .iter()
        .find_map(|(code, message)| (*code == value).then_some(*message))
}

fn sanitize_search(value: &str) -> String {
    value.chars().take(120).collect::<String>()
}

fn safe_value_display(value: &serde_json::Value) -> String {
    match value {
        serde_json::Value::String(value) => value.clone(),
        serde_json::Value::Bool(value) => value.to_string(),
        serde_json::Value::Number(value) => value.to_string(),
        _ => String::new(),
    }
}

fn sanitize_fields(
    fields: BTreeMap<String, serde_json::Value>,
) -> BTreeMap<String, serde_json::Value> {
    let mut safe = BTreeMap::new();
    let mut removed = 0_u64;
    for (name, value) in fields {
        match sanitize_field(&name, &value) {
            Some((name, value)) => {
                safe.insert(name, value);
            }
            None => removed = removed.saturating_add(1),
        }
    }
    if removed > 0 {
        safe.insert("redactedFieldCount".to_owned(), removed.into());
    }
    safe
}

fn sanitize_field(name: &str, value: &serde_json::Value) -> Option<(String, serde_json::Value)> {
    let canonical = match name {
        "diagnostic_code" | "diagnosticCode" => "diagnosticCode",
        "operation" => "operation",
        "action" => "action",
        "status" => "status",
        "stage" => "stage",
        "entity" => "entity",
        "provider_kind" | "providerKind" => "providerKind",
        "job_id" | "jobId" => "jobId",
        "project_id" | "projectId" => "projectId",
        "provider_id" | "providerId" => "providerId",
        "operation_id" | "operationId" => "operationId",
        "request_id" | "requestId" => "requestId",
        "method" => "method",
        "route" => "route",
        "attempt" => "attempt",
        "count" => "count",
        "duration_ms" | "durationMs" => "durationMs",
        "port" => "port",
        "scheme" => "scheme",
        "version" => "version",
        "source" => "source",
        "redactedFieldCount" => "redactedFieldCount",
        _ => return None,
    };

    let value = match canonical {
        "attempt" | "count" | "durationMs" | "port" | "redactedFieldCount" => numeric_value(value)?,
        "status" if value.is_number() => numeric_value(value)?,
        "jobId" | "projectId" | "providerId" | "operationId" | "requestId" => {
            let value = string_value(value)?;
            if uuid::Uuid::parse_str(&value).is_err() {
                return None;
            }
            value.into()
        }
        "method" => {
            let value = string_value(value)?.to_ascii_uppercase();
            if !matches!(
                value.as_str(),
                "GET" | "HEAD" | "POST" | "PUT" | "PATCH" | "DELETE" | "OPTIONS"
            ) {
                return None;
            }
            value.into()
        }
        "route" => {
            static ROUTE: OnceLock<Regex> = OnceLock::new();
            let matcher = ROUTE
                .get_or_init(|| Regex::new(r"^/api/v1/[A-Za-z0-9_/{}/.-]{0,160}$").expect("regex"));
            let value = string_value(value)?;
            if !matcher.is_match(&value) || value.contains('?') {
                return None;
            }
            value.into()
        }
        "scheme" => {
            let value = string_value(value)?.to_ascii_lowercase();
            if !matches!(value.as_str(), "http" | "https") {
                return None;
            }
            value.into()
        }
        "diagnosticCode" => {
            let value = string_value(value)?;
            message_for_code(&value)?;
            value.into()
        }
        _ => {
            static SYMBOLIC: OnceLock<Regex> = OnceLock::new();
            let matcher =
                SYMBOLIC.get_or_init(|| Regex::new(r"^[A-Za-z0-9_.:/-]{1,96}$").expect("regex"));
            let value = string_value(value)?;
            let sanitized = redact_token_patterns(&value);
            if sanitized.contains(REDACTED) || !matcher.is_match(&sanitized) {
                return None;
            }
            sanitized.into()
        }
    };
    Some((canonical.to_owned(), value))
}

fn string_value(value: &serde_json::Value) -> Option<String> {
    value.as_str().map(ToOwned::to_owned)
}

fn numeric_value(value: &serde_json::Value) -> Option<serde_json::Value> {
    value
        .as_u64()
        .filter(|number| *number <= 1_000_000_000_000)
        .map(Into::into)
}

fn redact_token_patterns(value: &str) -> String {
    static PATTERNS: OnceLock<Vec<Regex>> = OnceLock::new();
    let patterns = PATTERNS.get_or_init(|| {
        [
            r"(?i)\bBearer\s+[A-Za-z0-9._~+/=-]{8,}",
            r"(?i)\b(?:api[_-]?key|access[_-]?token|refresh[_-]?token|password|secret|authorization|cookie)\s*[:=]\s*[^\s&,;]+",
            r"(?i)(?:[?&](?:api[_-]?key|key|token|access[_-]?token|password|secret)=)[^&\s]+",
            r"\bsk-[A-Za-z0-9_-]{12,}\b",
            r"\bgh[pousr]_[A-Za-z0-9]{20,}\b",
            r"-----BEGIN [A-Z ]*PRIVATE KEY-----",
        ]
        .into_iter()
        .map(|pattern| Regex::new(pattern).expect("redaction regex"))
        .collect()
    });
    patterns.iter().fold(value.to_owned(), |current, pattern| {
        pattern.replace_all(&current, REDACTED).into_owned()
    })
}

#[derive(Debug)]
struct SafeEventVisitor {
    fields: BTreeMap<String, serde_json::Value>,
    dropped: u64,
}

impl SafeEventVisitor {
    fn new() -> Self {
        Self {
            fields: BTreeMap::new(),
            dropped: 0,
        }
    }

    fn record_value(&mut self, field: &tracing::field::Field, value: &serde_json::Value) {
        if field.name() == "message" {
            return;
        }
        match sanitize_field(field.name(), value) {
            Some((name, value)) => {
                self.fields.insert(name, value);
            }
            None => self.dropped = self.dropped.saturating_add(1),
        }
    }

    fn finish(mut self) -> (String, BTreeMap<String, serde_json::Value>) {
        if self.dropped > 0 {
            self.fields
                .insert("redactedFieldCount".to_owned(), self.dropped.into());
        }
        let message = self
            .fields
            .get("diagnosticCode")
            .and_then(serde_json::Value::as_str)
            .and_then(message_for_code)
            .unwrap_or(FALLBACK_MESSAGE)
            .to_owned();
        (message, self.fields)
    }
}

impl Visit for SafeEventVisitor {
    fn record_bool(&mut self, field: &tracing::field::Field, value: bool) {
        self.record_value(field, &value.into());
    }

    fn record_i64(&mut self, field: &tracing::field::Field, value: i64) {
        self.record_value(field, &value.into());
    }

    fn record_u64(&mut self, field: &tracing::field::Field, value: u64) {
        self.record_value(field, &value.into());
    }

    fn record_str(&mut self, field: &tracing::field::Field, value: &str) {
        self.record_value(field, &value.into());
    }

    fn record_debug(&mut self, field: &tracing::field::Field, value: &dyn std::fmt::Debug) {
        if field.name() == "message" {
            // The raw formatted message is intentionally never materialized.
            // Approved summaries are selected only by `diagnostic_code`.
            let _ = value;
        } else if matches!(
            field.name(),
            "job_id"
                | "jobId"
                | "project_id"
                | "projectId"
                | "provider_id"
                | "providerId"
                | "operation_id"
                | "operationId"
        ) {
            // Identifier fields may use tracing's `%` formatter. Their output
            // is retained only if it parses as a UUID in `sanitize_field`.
            self.record_value(field, &format!("{value:?}").into());
        } else {
            // Debug values may contain provider bodies, source text, paths, or
            // secret-bearing errors. They are never formatted into storage.
            self.dropped = self.dropped.saturating_add(1);
        }
    }
}

/// A tracing layer that forwards only safe event metadata into diagnostics.
#[derive(Clone, Copy, Debug, Default)]
pub struct DiagnosticsLayer;

impl<S> Layer<S> for DiagnosticsLayer
where
    S: Subscriber,
{
    fn on_event(&self, event: &Event<'_>, _context: Context<'_, S>) {
        let metadata = event.metadata();
        let mut visitor = SafeEventVisitor::new();
        event.record(&mut visitor);
        let (message, mut fields) = visitor.finish();
        if let (Some(file), Some(line)) = (metadata.file(), metadata.line())
            && let Some(file) = Path::new(file).file_name().and_then(|name| name.to_str())
        {
            fields.insert("source".to_owned(), format!("{file}:{line}").into());
        }
        global().record(
            DiagnosticLevel::from(metadata.level()),
            metadata.target(),
            &message,
            fields,
        );
    }
}

static GLOBAL: OnceLock<DiagnosticsStore> = OnceLock::new();

pub fn global() -> &'static DiagnosticsStore {
    GLOBAL.get_or_init(|| DiagnosticsStore::new(DEFAULT_CAPACITY))
}

pub(crate) fn configure_global(data_dir: &Path) {
    let (level, code, message) = if global().configure_persistence(data_dir).is_ok() {
        (
            DiagnosticLevel::Info,
            "diagnostics.ready",
            "AudiobookAI diagnostics initialized",
        )
    } else {
        (
            DiagnosticLevel::Warn,
            "diagnostics.persistence.unavailable",
            "Diagnostic file persistence is unavailable",
        )
    };
    global().record(
        level,
        "audiobookai_service::diagnostics",
        message,
        BTreeMap::from([(
            "diagnosticCode".to_owned(),
            serde_json::Value::String(code.to_owned()),
        )]),
    );
}

/// Records method, matched route pattern, status, and duration only. The raw
/// URI, query string, headers, request body, and response body are never read.
pub(crate) async fn request_diagnostics(request: Request<Body>, next: Next) -> Response {
    let method = request.method().as_str().to_owned();
    let route = request
        .extensions()
        .get::<MatchedPath>()
        .map(|path| path.as_str().to_owned());
    let started = Instant::now();
    let response = next.run(request).await;
    let status = response.status();
    if let Some(route) = route {
        let level = if status.is_server_error() {
            DiagnosticLevel::Error
        } else if status.is_client_error() {
            DiagnosticLevel::Warn
        } else {
            DiagnosticLevel::Info
        };
        global().record(
            level,
            "audiobookai_service::http",
            "HTTP request completed",
            BTreeMap::from([
                ("diagnosticCode".to_owned(), "http.request.completed".into()),
                ("method".to_owned(), method.into()),
                ("route".to_owned(), route.into()),
                ("status".to_owned(), u64::from(status.as_u16()).into()),
                (
                    "durationMs".to_owned(),
                    u64::try_from(started.elapsed().as_millis())
                        .unwrap_or(u64::MAX)
                        .into(),
                ),
            ]),
        );
    }
    response
}

#[derive(Debug)]
struct RotatingJsonl {
    directory: PathBuf,
    max_bytes: u64,
    generations: usize,
}

impl RotatingJsonl {
    fn new(directory: PathBuf, max_bytes: u64, generations: usize) -> std::io::Result<Self> {
        create_private_directory(&directory)?;
        Ok(Self {
            directory,
            max_bytes: max_bytes.max(1024),
            generations: generations.max(1),
        })
    }

    fn current_path(&self) -> PathBuf {
        self.directory.join("diagnostics.jsonl")
    }

    fn rotated_path(&self, generation: usize) -> PathBuf {
        self.directory
            .join(format!("diagnostics.{generation}.jsonl"))
    }

    fn append(&mut self, entry: &DiagnosticEntry) -> std::io::Result<()> {
        let safe = sanitize_stored_entry(entry.clone());
        let mut line = serde_json::to_vec(&safe).map_err(std::io::Error::other)?;
        line.push(b'\n');
        let current = self.current_path();
        let size = fs::metadata(&current).map_or(0, |metadata| metadata.len());
        if size.saturating_add(u64::try_from(line.len()).unwrap_or(u64::MAX)) > self.max_bytes {
            self.rotate()?;
        }
        let mut file = open_private_append(&current)?;
        file.write_all(&line)?;
        file.flush()
    }

    fn rotate(&self) -> std::io::Result<()> {
        let oldest = self.rotated_path(self.generations);
        if oldest.exists() {
            fs::remove_file(oldest)?;
        }
        for generation in (1..self.generations).rev() {
            let source = self.rotated_path(generation);
            if source.exists() {
                fs::rename(source, self.rotated_path(generation + 1))?;
            }
        }
        let current = self.current_path();
        if current.exists() {
            fs::rename(current, self.rotated_path(1))?;
        }
        Ok(())
    }

    fn load_sanitized(&self) -> Vec<DiagnosticEntry> {
        let mut entries = Vec::new();
        for path in (1..=self.generations)
            .rev()
            .map(|generation| self.rotated_path(generation))
            .chain(std::iter::once(self.current_path()))
        {
            let Ok(file) = File::open(path) else {
                continue;
            };
            for line in BufReader::new(file).lines().map_while(Result::ok) {
                if line.len() > 32 * 1024 {
                    continue;
                }
                if let Ok(entry) = serde_json::from_str::<DiagnosticEntry>(&line) {
                    entries.push(sanitize_stored_entry(entry));
                }
            }
        }
        entries
    }
}

fn create_private_directory(path: &Path) -> std::io::Result<()> {
    fs::create_dir_all(path)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(path, fs::Permissions::from_mode(0o700))?;
    }
    Ok(())
}

fn open_private_append(path: &Path) -> std::io::Result<File> {
    let mut options = OpenOptions::new();
    options.create(true).append(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let file = options.open(path)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        file.set_permissions(fs::Permissions::from_mode(0o600))?;
    }
    Ok(file)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fields(values: &[(&str, serde_json::Value)]) -> BTreeMap<String, serde_json::Value> {
        values
            .iter()
            .map(|(key, value)| ((*key).to_owned(), value.clone()))
            .collect()
    }

    #[test]
    fn removes_secrets_bodies_urls_and_unapproved_messages() {
        let store = DiagnosticsStore::new(10);
        store.record(
            DiagnosticLevel::Error,
            "audiobookai_service::test",
            "A line copied from a DRM-free novel",
            fields(&[
                ("password", "do-not-store-this".into()),
                ("authorization", "Bearer do-not-store-this".into()),
                ("body", "A line copied from a DRM-free novel".into()),
                (
                    "endpoint",
                    "https://example.invalid?token=do-not-store-this".into(),
                ),
                ("action", "start".into()),
            ]),
        );

        let exported = String::from_utf8(store.export_jsonl(&DiagnosticQuery::default()))
            .expect("UTF-8 export");
        assert!(!exported.contains("do-not-store-this"));
        assert!(!exported.contains("copied from"));
        assert!(!exported.contains("example.invalid"));
        assert!(exported.contains(FALLBACK_MESSAGE));
        assert!(exported.contains("start"));
        assert!(exported.contains("redactedFieldCount"));
    }

    #[test]
    fn redacts_token_and_query_patterns_defensively() {
        let fixtures = [
            ["Bearer ", "abcdefghijklmnop"].concat(),
            ["api_", "key", "=", "abcdefghijklmnop"].concat(),
            [
                "https://example.invalid/path?",
                "token",
                "=",
                "abcdefghijklmnop",
            ]
            .concat(),
            ["sk", "-", "abcdefghijklmnop"].concat(),
            ["gh", "p_", "abcdefghijklmnopqrstuvwxyz"].concat(),
            ["-----BEGIN ", "PRIVATE", " ", "KEY", "-----"].concat(),
        ];
        for value in fixtures {
            let sanitized = redact_token_patterns(&value);
            assert!(sanitized.contains(REDACTED), "pattern was not redacted");
            assert!(!sanitized.contains("abcdefghijklmnop"));
        }
    }

    #[test]
    fn ring_is_bounded_and_filters_without_mutating_records() {
        let store = DiagnosticsStore::new(2);
        for (index, level) in [
            DiagnosticLevel::Info,
            DiagnosticLevel::Warn,
            DiagnosticLevel::Error,
        ]
        .into_iter()
        .enumerate()
        {
            store.record(
                level,
                "audiobookai_service::test",
                "HTTP request completed",
                fields(&[("count", u64::try_from(index).unwrap().into())]),
            );
        }
        let all = store.query(&DiagnosticQuery::default());
        assert_eq!(all.items.len(), 2);
        assert_eq!(all.items[0].level, DiagnosticLevel::Error);
        assert_eq!(all.items[1].level, DiagnosticLevel::Warn);

        let errors = store.query(&DiagnosticQuery {
            level: Some(DiagnosticLevel::Error),
            ..DiagnosticQuery::default()
        });
        assert_eq!(errors.items.len(), 1);
    }

    #[test]
    fn persisted_records_are_private_rotated_and_resanitized() {
        let directory = tempfile::tempdir().expect("tempdir");
        let store = DiagnosticsStore::new(10);
        store
            .configure_persistence(directory.path())
            .expect("persistence");
        store.record(
            DiagnosticLevel::Info,
            "audiobookai_service::test",
            "HTTP request completed",
            fields(&[("route", "/api/v1/health".into())]),
        );
        let path = directory.path().join("logs/diagnostics.jsonl");
        let persisted = fs::read_to_string(&path).expect("persisted log");
        assert!(persisted.contains("/api/v1/health"));
        assert!(!persisted.contains("token"));

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                fs::metadata(path).unwrap().permissions().mode() & 0o777,
                0o600
            );
        }
    }

    #[test]
    fn tracing_layer_keeps_approved_summary_and_omits_error_detail() {
        use tracing_subscriber::prelude::*;

        let after = global().query(&DiagnosticQuery::default()).latest_sequence;
        tracing::subscriber::with_default(
            tracing_subscriber::registry().with(DiagnosticsLayer),
            || {
                tracing::warn!(
                    target: "audiobookai_service::diagnostics_test",
                    diagnostic_code = "conversion.failed",
                    action = "restart",
                    error = "opaque provider detail",
                    "conversion job failed"
                );
            },
        );
        let page = global().query(&DiagnosticQuery {
            after: Some(after),
            target: Some("audiobookai_service::diagnostics_test".to_owned()),
            ..DiagnosticQuery::default()
        });
        assert_eq!(page.items.len(), 1);
        assert_eq!(page.items[0].message, "conversion job failed");
        assert_eq!(page.items[0].fields.get("action"), Some(&"restart".into()));
        assert!(
            !page.items[0]
                .fields
                .values()
                .any(|value| { value.as_str().is_some_and(|value| value.contains("opaque")) })
        );
    }

    #[test]
    fn mlx_failure_diagnostic_keeps_only_safe_operation_provenance() {
        use tracing_subscriber::prelude::*;

        let operation_id = uuid::Uuid::new_v4();
        let after = global().query(&DiagnosticQuery::default()).latest_sequence;
        tracing::subscriber::with_default(
            tracing_subscriber::registry().with(DiagnosticsLayer),
            || {
                tracing::warn!(
                    target: "audiobookai_service::mlx_diagnostics_test",
                    diagnostic_code = "mlx.management.operation.failed",
                    operation_id = %operation_id,
                    action = "download_model",
                    tool_output = "must never be retained",
                    "MLX-audio management operation failed"
                );
            },
        );
        let page = global().query(&DiagnosticQuery {
            after: Some(after),
            target: Some("audiobookai_service::mlx_diagnostics_test".to_owned()),
            ..DiagnosticQuery::default()
        });
        assert_eq!(page.items.len(), 1);
        assert_eq!(
            page.items[0].message,
            "MLX-audio management operation failed"
        );
        assert_eq!(
            page.items[0].fields.get("operationId"),
            Some(&operation_id.to_string().into())
        );
        assert_eq!(
            page.items[0].fields.get("action"),
            Some(&"download_model".into())
        );
        assert!(!page.items[0].fields.contains_key("tool_output"));
    }

    #[test]
    fn provider_model_failure_diagnostic_keeps_operation_id_without_error_detail() {
        use tracing_subscriber::prelude::*;

        let operation_id = uuid::Uuid::new_v4();
        let after = global().query(&DiagnosticQuery::default()).latest_sequence;
        tracing::subscriber::with_default(
            tracing_subscriber::registry().with(DiagnosticsLayer),
            || {
                tracing::warn!(
                    target: "audiobookai_service::provider_model_diagnostics_test",
                    diagnostic_code = "provider.model.download_status.failed",
                    operation_id = %operation_id,
                    error = "raw provider response must never be retained",
                    "Provider model download status failed"
                );
            },
        );
        let page = global().query(&DiagnosticQuery {
            after: Some(after),
            target: Some("audiobookai_service::provider_model_diagnostics_test".to_owned()),
            ..DiagnosticQuery::default()
        });
        assert_eq!(page.items.len(), 1);
        assert_eq!(
            page.items[0].message,
            "provider model download status failed"
        );
        assert_eq!(
            page.items[0].fields.get("operationId"),
            Some(&operation_id.to_string().into())
        );
        assert!(!page.items[0].fields.contains_key("error"));
        assert!(
            !serde_json::to_string(&page.items[0])
                .unwrap()
                .contains("raw provider response")
        );
    }
}
