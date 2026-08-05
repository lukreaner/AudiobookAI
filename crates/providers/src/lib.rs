//! Capability-driven provider integrations.
//!
//! Provider adapters deliberately depend on an abstract HTTP transport. The service can
//! therefore add retries, tracing and rate limiting without putting credentials into tests.
#![allow(
    clippy::default_trait_access,
    clippy::duration_suboptimal_units,
    clippy::fn_params_excessive_bools,
    clippy::missing_errors_doc,
    clippy::return_self_not_must_use,
    clippy::struct_excessive_bools
)]

pub mod adapters;
pub mod error;
pub mod http;
mod model_library;
pub mod process;
pub mod registry;
pub mod traits;
pub mod types;

pub use error::{ProviderError, Result};
pub use http::{
    HttpByteStream, HttpMethod, HttpRequest, HttpResponse, HttpStreamResponse, HttpTransport,
    ReqwestTransport,
};
pub use model_library::{
    ModelControlProtocol, local_ai_model_identifiers_equal, ollama_model_identifiers_equal,
};
pub use process::{
    ManagedProcessController, ManagedProcessSupervisor, contains_secret_shaped_value,
    validate_managed_process_arguments,
};
pub use registry::ProviderRegistry;
pub use traits::{
    AudioChunkSink, CharacterProvider, ModelDownloadProgressSink, ProviderControl, TtsProvider,
    VoiceCloneProvider,
};
pub use types::*;
