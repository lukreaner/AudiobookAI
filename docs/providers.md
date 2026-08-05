# Provider contract

Provider configuration and capability discovery are separate from credentials.
A profile stores an opaque `SecretRef`, never the secret value.

Capabilities cover modality, model and voice discovery, native streaming,
concurrency, cancellation, structured output, usage reporting, pronunciation,
voice cloning, reasoning controls, and lifecycle/model controls. A cached
capability snapshot is bound to the configured endpoint and model, records its
adapter provenance and observation time, expires after 24 hours, and is replaced
after configuration or model changes. Provider version information remains
nullable when the provider's health contract does not report a trustworthy
version. Expired or mismatched snapshots fail closed for paid and destructive
operations.

Temperature has three states: omitted, explicit null, and numeric value. The
provider adapter serializes only a state the selected model supports. Reasoning
is likewise represented as inherit, disabled, effort, adaptive, or token budget;
preflight rejects unsupported settings.

Local processes are classified as external endpoints or app-owned children.
AudiobookAI may observe either, but may stop/restart only an app-owned child.
Managed-child profiles store an absolute executable, an optional absolute
working directory, and a bounded argv list. Each argument is passed literally
to the executable without a shell; argv is ordinary configuration and must not
contain secrets. Plaintext environment values are not part of the
provider-profile API; provider credentials use the encrypted secret store.
Deleting a profile never terminates an external process or deletes provider-side
models and voices, and a profile with a running app-owned child must be stopped
before it can be deleted. Model loading, unloading, downloading, and deletion
are separate capability-gated operations. Destructive model actions require an
explicit confirmation and are rejected while the model is selected, assigned,
used by an active job, or still loaded when the provider exposes that state.

MLX-audio is the first provider AudiobookAI can install. On Apple Silicon, an
explicit UI action first validates the canonical, non-symlink bundled installer
tree: uv 0.12.1, CPython, the complete artifact lock, a hash-locked requirements
file, and the exact wheelhouse for MLX-audio 0.4.6. Installation creates a venv
with that explicit Python and runs `uv pip` with offline, no-index, no-sources,
no-Python-download, require-hashes, and binary-wheel-only constraints. The child
environment is cleared and rebuilt from a small allowlist, so ambient Python,
user proxy/package-index settings, Hugging Face credentials, and provider
credentials are not inherited. Installed dist-info name/version and entry points
must pass validation before the runtime is marked complete. Bounded tool output
is reduced to allowlisted phase/exit diagnostics before it reaches the UI; raw
output, paths, argv, environment values, and credential-shaped text are never
retained. Public Hugging Face `owner/repository` model downloads remain separate,
explicit user actions. Runtime uninstall retains models; model removal checks an
ownership marker and never deletes an arbitrary path.

Provider-native model controls follow each provider's documented API instead of
assuming a shared OpenAI-compatible contract. Ollama supports installed-model
listing, streamed pulls, load/unload, and confirmed deletion after a loaded-model
check. LM Studio supports listing, asynchronous download/status reporting, load,
and exact-instance unload; deletion is not advertised because its current public
REST API has no deletion endpoint. LocalAI supports installed-model listing,
bounded gallery installs, load/unload, and confirmed deletion. LocalAI deletion
first reads its authenticated `/system` loaded-model view and fails closed on an
HTTP, schema, or identifier error; AudiobookAI never deletes LocalAI files
directly. Providers without a stable documented model-management API expose no
controls.

Provider-native download progress is visible and cancellable for the current app
session, but its operation journal is not yet durable across a service restart.
Restart recovery for these provider-native downloads and model-specific reasoning
capability discovery remain 1.0 release gates. The shipped pre-1.0 build therefore
does not claim those gates have passed.

Normal tests use mock transports and fixture audio and never require credentials.
The opt-in live contract matrix is a separate 1.0 release gate and must receive
credentials through the runtime secret store; it is not part of ordinary CI.
