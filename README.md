# AudiobookAI

AudiobookAI is a GPLv3 desktop application that turns DRM-free EPUB 2/3 books
into multi-voice audiobooks. The primary distribution is a native Tauri desktop
application for Windows, macOS, and Linux. Native installers are the only
supported application distributions; Python packaging and an end-user CLI are
out of scope.

> **Development status:** the 1.0 implementation is under active development.
> Do not use real paid-provider credentials until the opt-in live contract gates
> for the selected provider have been completed and reviewed.

## Features

- Chapter-aware EPUB import, metadata and cover extraction, and DRM detection
- Character/dialogue detection with editable speaker review
- Per-character providers, models, catalog voices, reference audio, and clones
- Typed voice direction and side-by-side, explicitly billable voice auditions
- ElevenLabs, MLX-audio, LocalAI, AllTalk V2, native OS TTS, OpenAI, Anthropic,
  Gemini, Qwen, Kimi/Moonshot, LM Studio, and Ollama adapter families
- In-app MLX-audio installation plus capability-gated local model management
- Preview, estimate, dry-run, budgets, reservations, and provenance-led usage
- Durable resumable jobs, per-provider concurrency, retries, and a content cache
- A durable proofing workbench with text overrides, review states, take history,
  selective regeneration, and provider-free re-export from approved takes
- MP3, WAV/RF64, M4A, and M4B export with metadata, cover, and chapter markers
- Versioned retailer QC and reproducible delivery packages with manual safety gates
- Progressive playback, loudness normalization, and optional user-supplied music
- OS-keychain/AES-GCM secrets and opt-in authenticated TLS LAN mode
- Detailed, rotating, privacy-sanitized diagnostics in the authenticated UI
- Complete English and German desktop dashboard

## Repository layout

```text
apps/desktop/       Tauri desktop host and native packaging
crates/core/        Domain types and validation
crates/storage/     SQLite migrations and repositories
crates/providers/   TTS, character-AI, voice, and lifecycle adapters
crates/media/       FFmpeg discovery, cache, mixing, and export planning
crates/service/     Axum API, job runtime, auth, and embedded dashboard
web/                React/TypeScript dashboard
```

## Development

Requirements are Rust 1.96, Node.js 26, and pnpm 11. FFmpeg/ffprobe are needed
for media integration tests; production installers carry pinned native
sidecars. No provider key is required for the normal unit or contract-test
suite.

```bash
pnpm --dir web install
pnpm --dir web build
cargo test --workspace
pnpm --dir web tauri dev
```

To produce a fresh native executable and non-release-signed host package from the current
checkout, run:

```bash
make native-local
```

`make desktop` is retained as an alias for the same packaging path.
The command uses the locked Rust and pnpm dependency graph, builds the embedded
dashboard and an optimized debug Tauri host, and publishes a stable current view under
`artifacts/local-native/current/<rust-host-target>/`. That directory contains
the executable, a host package, `manifest.json`, and `SHA256SUMS`. Its matching
content-addressed snapshot remains under `artifacts/local-native/builds/`; older
snapshots for that host are pruned only after the new snapshot and current view
both pass checksum verification.
These local artifacts are for development and deliberately use no release
identity, so they do not satisfy the public release procedure. On Apple Silicon,
the linker can still apply an ad-hoc Mach-O signature; that is not project
distribution signing or notarization.
The development profile accepts system FFmpeg and ffprobe from `PATH`; those
tools must be installed for conversion, quality-control, and export features,
and Linux native speech additionally requires eSpeak NG.

A successful cross-platform `Native CI` run for a push to `main` starts the
GitHub-hosted rolling development snapshot workflow. It publishes a prerelease
only after the Windows x64, macOS universal, and Linux x64 packaging jobs all
succeed and the aggregate job verifies the complete set and its checksums.
Snapshot installers are debug builds with a separate package identifier.
Windows and Linux are unsigned; macOS uses only an untrusted ad-hoc signature
and is not notarized. All are updater-disabled and omit the audited release
sidecars. They may share AudiobookAI application data, so use them only with
disposable test data and backups.

The service defaults to loopback. LAN binding is deliberately unavailable until
owner authentication and either a TLS identity or the separately confirmed
insecure-LAN override are configured. Provider setup is optional during first
run and can be completed later in the configuration UI. Credentials are
referenced by opaque secret IDs; they are never stored in project JSON, logs,
diagnostic exports, build artifacts, or Git history.

## Packaging

- Desktop: Tauri NSIS (Windows), notarized universal DMG (macOS), AppImage and
  `.deb` (Linux)
- Base desktop: no separately installed Node.js, Python, Docker, or system
  FFmpeg; an explicitly requested MLX-audio install creates its own isolated,
  app-managed Python/tool environment

The signed desktop installers are the only supported public application
distributions; SBOMs, notices, checksums, and corresponding source accompany
them as release materials.

A qualifying annotated stable tag automatically starts the full signed release
pipeline. The tag, Cargo workspace, Tauri app, and dashboard versions must agree;
all protected-environment approvals and every native/signing gate must pass.
Only then is the byte-verified draft made public, without a second manual
dispatch. The `stable-release` and `stable-publish` environments remain approval
boundaries when required reviewers are configured.

See [the packaging inputs](packaging/README.md) for the distinction between the
non-release-signed local native command and signed release packaging.

See [the architecture](docs/architecture.md), [security model](docs/security.md),
and [provider contract](docs/providers.md) for implementation details.

## License

AudiobookAI is licensed under [GPL-3.0-only](LICENSE). Bundled media and native
runtime components retain their own licenses and corresponding-source notices.
