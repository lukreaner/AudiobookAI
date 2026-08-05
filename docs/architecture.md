# Architecture

AudiobookAI embeds its application-services layer in the desktop process:

```text
Tauri desktop ─> in-process Axum application services
               ├─> SQLite / cache
               ├─> TTS and character providers
               └─> bundled FFmpeg / native speech
```

The Tauri process owns the Axum runtime and job scheduler. Closing the window
keeps that process in the tray; explicit quit checkpoints jobs before stopping
children that AudiobookAI itself launched. The dashboard is embedded into the
Rust service and is also the authenticated UI exposed when the owner enables LAN access.

## Data and consistency

SQLite is authoritative. WAL, foreign keys, a busy timeout, embedded migrations,
and an application lock prevent multiple local writers. Large immutable files
live in a BLAKE3-addressed object store; SQLite records ownership, cache request
fingerprints, pinning, and provenance. Files are streamed to a same-filesystem
temporary path, validated and synced, atomically renamed, and only then linked
from a committed database transaction.

Imported EPUBs are copied into the managed library. Text is represented as
stable paragraph IDs and offsets; AI suggestions and manual speaker overrides
are separate records so rerunning detection cannot erase reviewed work.

The desktop installation's application-data directory is authoritative for the
managed library and content cache. Their resolved paths are visible but
read-only in setup and settings; copying settings from another machine cannot
redirect existing data. A future relocation feature must checkpoint jobs, move
and verify every managed artifact, and atomically switch the database and file
roots before these paths can become editable.

Application defaults are durable owner settings. Loudness and true-peak values
are applied when a new export profile is created, while chapter concurrency and
retry counts are copied into newly imported projects. Existing projects retain
their stored policy. The cache limit is enforced with least-recently-used
cleanup after completed work and whenever the owner lowers the limit; cache
objects referenced by active jobs remain protected.

## Service contract

Commands use `/api/v1` JSON resources. Long-running operations return durable
job IDs. SSE carries live invalidations backed by durable job records and polling
recovery, WebSockets carry framed PCM only, and completed artifacts support HTTP Range. Mutations accept idempotency keys and
revision checks. Rust request/response models are authoritative; the typed dashboard
client is kept compatible through service and interaction tests.

## Scheduling

A job is a persisted dependency graph of detection, synthesis, chapter assembly,
mixing, normalization, and export units. Global and provider-specific semaphores
bound concurrency. Attempts record dispatch and billing uncertainty separately
from success/failure, allowing safe pause, retry, cancellation, and crash resume.
