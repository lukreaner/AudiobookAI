//! Durable `SQLite` persistence for `AudiobookAI`.

#![allow(clippy::missing_errors_doc)]

mod database;
mod error;
mod paths;
pub mod repositories;

pub use database::Database;
pub use error::{Result, StorageError};
pub use paths::{AppPaths, harden_private_file};
