use std::path::PathBuf;

#[derive(Debug, thiserror::Error)]
pub enum MediaError {
    #[error("invalid media configuration: {0}")]
    Configuration(String),
    #[error("FFmpeg sidecar was not found; checked {checked:?}")]
    SidecarNotFound { checked: Vec<PathBuf> },
    #[error("sidecar checksum mismatch for {path}")]
    ChecksumMismatch { path: PathBuf },
    #[error("a production export requires two-pass loudness measurements")]
    LoudnessMeasurementRequired,
    #[error("cache key is invalid")]
    InvalidCacheKey,
    #[error("media I/O failed at {path}: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("media serialization failed: {0}")]
    Serialization(#[from] serde_json::Error),
}

impl MediaError {
    pub fn io(path: impl Into<PathBuf>, source: std::io::Error) -> Self {
        Self::Io {
            path: path.into(),
            source,
        }
    }
}

pub type Result<T> = std::result::Result<T, MediaError>;
