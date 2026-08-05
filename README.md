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
- ElevenLabs, MLX-audio, LocalAI, AllTalk V2, native OS TTS, OpenAI, Anthropic,
  Gemini, Qwen, Kimi/Moonshot, LM Studio, and Ollama adapter families
- In-app MLX-audio installation plus capability-gated local model management
- Preview, estimate, dry-run, budgets, reservations, and provenance-led usage
- Durable resumable jobs, per-provider concurrency, retries, and a content cache
- MP3, WAV/RF64, M4A, and M4B export with metadata, cover, and chapter markers
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

See [the architecture](docs/architecture.md), [security model](docs/security.md),
and [provider contract](docs/providers.md) for implementation details.

## License

AudiobookAI is licensed under [GPL-3.0-only](LICENSE). Bundled media and native
runtime components retain their own licenses and corresponding-source notices.
