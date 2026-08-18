# Packaging inputs

## Local native development build

`make native-local` is the deterministic, host-native developer packaging
entrypoint. It invokes Tauri in an optimized debug profile with dependency
locking and release-identity signing disabled, then publishes only generated files beneath the gitignored
`artifacts/local-native/` tree:

- `current/<rust-host-target>/` is the stable, discoverable view of the latest
  successful build. It contains the native executable, the host package,
  `manifest.json`, `SHA256SUMS`, and a `CURRENT` build identifier.
- `builds/<rust-host-target>/<build-id>/` is the immutable snapshot referenced
  by the manifest. After the new snapshot and current view both pass checksum
  verification, earlier recognized snapshots for that host are pruned. Unsafe
  or unrecognized entries fail closed and are never deleted.

The build ID incorporates the current tracked diff, relevant untracked build
inputs, and the produced artifact hashes. The script rechecks that source state
after Tauri exits, normalizes macOS app-archive metadata, writes sorted JSON and
checksums, and holds a per-target publication lock while it publishes, verifies,
and prunes. A concurrent publication or stale lock fails closed. The script
verifies both the immutable and current copies before reporting success. On
macOS, extract `AudiobookAI.app.tar.gz` to recover the development app
bundle, or run the standalone `AudiobookAI` executable directly. Apple Silicon's
linker may apply an ad-hoc signature to the Mach-O executable even with Tauri
signing disabled; the manifest records that possibility without treating it as
distribution signing.

The debug profile intentionally permits FFmpeg and ffprobe from `PATH`, so the
host must provide both tools for conversion, quality-control, and export use.
Linux native speech additionally requires eSpeak NG. The build does not copy
those host tools into the artifact.

This path intentionally does not consume release sidecars, signing keys,
notarization credentials, or the updater key. It is not a supported public
installer and must never be uploaded as a release artifact.

## Automated GitHub builds

A successful `Native CI` run for a push to `main` triggers
`.github/workflows/snapshot.yml` with that run's immutable commit SHA. A red,
cancelled, pull-request, fork, non-`main`, or manually dispatched CI run cannot
publish a snapshot. Packaging then runs entirely on GitHub-hosted Windows,
macOS, and Linux runners and produces this exact rolling development prerelease
asset set:

- `AudiobookAI-development-windows-x86_64.exe`
- `AudiobookAI-development-macos-universal.dmg`
- `AudiobookAI-development-linux-x86_64.AppImage`
- `AudiobookAI-development-linux-x86_64.deb`
- `DEVELOPMENT-BUILD.txt`, `LICENSE`, and `SHA256SUMS`

Checksums and the combined candidate are generated only after every native
target succeeds. Publication creates an unpublished prerelease draft, uploads
the complete set, downloads it again, compares every name and byte, and only
then makes it public. A newly verified snapshot is published before best-effort
pruning of older workflow-owned snapshots, so pruning cannot create an
availability gap.

These are optimized debug builds with a separate package identifier. Windows
and Linux are unsigned. The macOS DMG uses Tauri's literal `-` ad-hoc identity
to keep the downloaded app structurally signed, but that is not a trusted
Developer ID signature and the app is not notarized. No snapshot is
updater-enabled or a supported stable release, and snapshots intentionally omit
the unresolved audited sidecar bundles. Development media use therefore
requires system FFmpeg/ffprobe and, on Linux, eSpeak NG. Windows SmartScreen and
macOS Gatekeeper may warn or block them. The desktop service may still share
application data with stable builds; use disposable test data and keep backups.

`.github/workflows/release.yml` is the independent stable path. Pushing a
qualifying annotated `vMAJOR.MINOR.PATCH` tag starts it automatically. The tag
must match every committed version, resolve to the immutable quality-gated
commit, be signed by the locally trusted release identity, and pass the
finalized-sidecar and full signed native matrix. The aggregate job creates and
signs checksums only after all targets succeed. Publication then stages an
unpublished draft, uploads and byte-verifies every asset, and makes it public
without a second workflow-dispatch run. The `stable-release` and
`stable-publish` environments still pause their jobs for required-reviewer
approval when that protection is configured; the tag is the release intent,
not a bypass around those controls.

## Signed release inputs

`sidecars.lock.json` is the signed-release allowlist for native media tools. It pins the
upstream source, build contract, bundle archive, and critical files independently. The committed
manifest intentionally has `releaseReady: false` and unresolved bundle hashes: no release may be
published until audited native builds replace every placeholder and the flag is changed to `true`.

The release workflow extracts verified bundles into `packaging/runtime/<target>/`. That directory
is generated, never committed, and is included in the matching desktop package without flattening:
`bin/` becomes the installed `sidecars/bin/` resource directory and `share/` becomes
`sidecars/share/`. An archive must contain paths exactly as listed in the manifest and may not
contain links, absolute paths, device nodes, or traversal components.

Every archive and detached-signature URL must be a stable, anonymous public HTTPS URL. The manifest
must never contain credentials, access tokens, query parameters, fragments, private-release URLs,
pre-signed object-storage links, or any other access grant. Release automation intentionally has no
authenticated sidecar-download mode.

Supported target keys are:

- `x86_64-pc-windows-msvc`
- `universal-apple-darwin`
- `x86_64-unknown-linux-gnu`

The Apple bundle also requires `bin/uv` at the pinned version recorded in the lockfile. Its
currently unresolved source and binary hashes intentionally keep `releaseReady` false. Packaged
MLX-audio installation must use this verified absolute sidecar path and may never fetch or trust a
replacement uv executable at runtime.

The `mlxAudioInstaller` section is a second, independent release gate. Stable packages remain
blocked until it records an exact managed CPython artifact and a checksum-verified
`packaging/mlx-audio-installer.lock.json` containing the complete hashed transitive closure for
`mlx-audio[tts,server]==0.4.6`. `--allow-unresolved` is for local validation only.

The `piperInstaller` section is different: it is the allowlist for an optional app-managed online
install on Linux x86_64, not material for a release bundle. Piper must not appear in any target's
`requiredFiles` or below `packaging/runtime/`. The section pins the official Piper 1.2.0 archive URL,
byte count, and SHA-256, plus a curated voice catalog at one full `rhasspy/piper-voices` commit. Each
voice model, config, and model card is locked separately. The only initially approved voice is the
single-speaker `de_DE-thorsten-medium`; its pinned model-card dataset-license declaration and
provenance must be shown and confirmed before download. The literal `download=true` query recorded
for that runtime voice endpoint is not an exception to the no-query rule for bundled sidecar URLs.
The committed `releaseReady: false` value remains unchanged.

See [the sidecar compliance procedure](../docs/sidecars.md) before updating any hash.
