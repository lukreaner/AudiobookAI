//! Stable domain types shared by every `AudiobookAI` interface.
//!
//! This crate deliberately contains no I/O. Provider adapters, the HTTP service,
//! desktop host, service, and persistence layer all communicate through these types.

pub mod accounting;
pub mod common;
pub mod error;
pub mod export;
pub mod ids;
pub mod jobs;
pub mod library;
pub mod providers;
pub mod security;
pub mod speech;

pub use accounting::*;
pub use common::*;
pub use error::*;
pub use export::*;
pub use ids::*;
pub use jobs::*;
pub use library::*;
pub use providers::*;
pub use security::*;
pub use speech::*;
