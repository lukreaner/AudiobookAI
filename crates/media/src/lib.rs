//! Media planning, sidecar discovery and content-addressed cache support.
//!
//! This crate builds argument vectors instead of shell command strings. Callers can inspect and
//! execute the plans without exposing user paths to shell parsing.
#![allow(
    clippy::default_trait_access,
    clippy::format_push_string,
    clippy::missing_errors_doc
)]

pub mod cache;
pub mod error;
pub mod export;
pub mod sidecar;

pub use cache::{CacheFingerprint, CacheKey, CachePruneReport, ContentAddressedCache};
pub use error::{MediaError, Result};
pub use export::*;
pub use sidecar::{SidecarChecksums, SidecarPair, SidecarResolver};
