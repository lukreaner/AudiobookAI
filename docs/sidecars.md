# Bundled FFmpeg and eSpeak NG

Packaged AudiobookAI releases must not discover an arbitrary system FFmpeg. They carry an audited
FFmpeg/ffprobe pair for the release target, while Linux additionally carries the eSpeak NG binary
and voice data. The macOS bundle additionally carries uv 0.12.1 and the complete audited offline
MLX-audio installer payload solely for the explicit, user-initiated installation flow. macOS and
Windows use their native speech systems instead of eSpeak NG.

The optional Piper TTS runtime is deliberately outside these release bundles. On Linux x86_64 it
is an explicit, online, app-managed install into the application data directory; it is never copied
into `packaging/runtime/` or advertised as available offline.

## Pinned sources and build contract

The authoritative machine-readable record is
[`packaging/sidecars.lock.json`](../packaging/sidecars.lock.json). FFmpeg is pinned to 8.1.1 and its
official source archive SHA-256, libmp3lame is pinned to 3.100 and its source SHA-256, and eSpeak NG
is pinned to the full 1.52.0 Git commit. Release builds must use clean native hosts; containers and
Docker-derived binaries are not accepted.

FFmpeg must be configured with every flag in the lockfile. In particular, GPL and nonfree FFmpeg
options stay disabled, network protocols and FFplay stay disabled, and the build uses native AAC,
FLAC/PCM, and libmp3lame. Before a bundle is approved, run and archive:

```text
ffmpeg -hide_banner -version
ffmpeg -hide_banner -buildconf
ffmpeg -hide_banner -encoders
ffmpeg -hide_banner -filters
ffmpeg -hide_banner -muxers
ffprobe -hide_banner -version
```

The captured output must prove every `requiredFeatures` entry in the lockfile and must show no
`--enable-gpl` or `--enable-nonfree`. Linux evidence must invoke eSpeak with the bundled
`share/espeak-ng-data/` path and successfully list its voices. Record the compiler, linker, SDK,
deployment target, source date epoch, build-host image, static-library inputs, and all patches.
macOS binaries must contain both arm64 and x86_64 slices. Windows binaries must target x64. Linux
binaries must run on a clean Ubuntu 22.04 installation without an installed FFmpeg or eSpeak.
The pinned uv binary must pass `uv --version`, must never be downloaded at runtime, and is included
only after its source and binary hashes are independently audited.

`uv` is only the installer executable, not a dependency lock. The fixed bundle layout is
`share/mlx-audio-installer/`, containing `python/bin/python3`, `installer.lock.json`,
`requirements.lock`, and a flat `wheelhouse/`. The artifact lock must select exactly one binary wheel
per normalized distribution, include the exact MLX-audio 0.4.6 wheel, attest the complete target- and
Python-specific closure, and match every requirement and wheel hash. The runtime checks the lock,
requirements, wheel names, wheel bytes, canonical containment, and installed package metadata before
marking installation complete. It never resolves or downloads Python or packages. The managed
feature remains release-blocked until the exact CPython distribution and complete transitive wheel
set have independently reviewed hashes; `releaseReady` must remain false until that work is done.

## App-managed online Piper input

The `piperInstaller` section of the lockfile records the separate runtime-download contract. It
pins the official Piper 1.2.0 `piper_amd64.tar.gz` release archive to 25,916,047 bytes and SHA-256
`467c17935d2a22dcce9dc9e08ba07485e29be813097e7cf08c5627aa09d32e42`. The app offers this only on
Linux x86_64 and downloads it only after an explicit install action. The runtime installer verifies
the byte count and checksum before safe extraction into its marker-owned app-data root. The release
sidecar fetch/extract command validates this metadata but does not fetch Piper or add it to a desktop
package.

Voice files are a separate explicit download. The curated catalog is pinned to the full
`rhasspy/piper-voices` commit `f5a6e9094787fd865d65cb024472f977f9c542b5`; each model, config,
and model card has its own exact path, byte count, and SHA-256. The initial catalog exposes only the
single-speaker `de_DE-thorsten-medium` voice. The pinned card declares its source dataset license as
CC0, which is recorded with that narrow scope; it is not an unqualified license assertion for every
artifact in the voice repository. The UI must show the pinned card, declared license, and provenance
and obtain per-voice confirmation before downloading. Voice provenance and licensing remain
independent from the MIT-licensed Piper engine.

Engine uninstall retains voices. Removing a voice requires a separate confirmation and must fail
closed if any profile, assignment, or active job still uses it. Adding a curated voice requires the
same per-file locking and license/provenance review; a branch name or mutable `main` URL is not an
acceptable catalog pin.

## Bundle approval

1. Build independently on the matching native host and run the media acceptance suite.
2. Produce a deterministic ZIP or `tar.gz` rooted at `bin/` and `share/`; Linux includes
   `share/espeak-ng-data/`, while Apple includes `share/mlx-audio-installer/`.
3. Compute the archive and critical-file SHA-256 values twice on independent machines.
4. Sign the archive with the project release key and retain the detached signature beside it.
5. Publish the archive and detached signature at stable, anonymously downloadable HTTPS URLs.
   Authenticated/private releases, bearer tokens, expiring links, URL credentials, and query-based
   signatures are not supported.
6. Update the public URLs and hashes in the lockfile, then set `releaseReady` only after all target
   bundles have completed review.
7. Run `python3 scripts/release/sidecars.py manifest` and fetch each target through the same script.

The fetcher verifies the archive before extraction, rejects unsafe archive entries, then verifies
each critical file. It never sends an authorization header and never treats TLS alone as sufficient
provenance.

This bundle approval procedure covers shipped FFmpeg, eSpeak NG, uv, and MLX installer inputs. The
separate Piper runtime and voice downloads follow the app-managed contract above and are never
introduced into the bundle by this procedure.

The Tauri bundle preserves that verified tree below its resource directory as `sidecars/bin/` and
`sidecars/share/`. The desktop host passes the installed `sidecars/bin/` directory to the service;
the MLX manager derives only its canonical sibling `share/mlx-audio-installer/` path. Packaged media,
Linux speech, Python, requirements, and wheels must never resolve from a development directory,
user-data directory, ambient `PATH`, package index, or network endpoint.

## Licensing and corresponding source

Every binary release must carry:

- the exact FFmpeg, eSpeak NG, and bundled uv source used to build it;
- AudiobookAI's patches, build scripts, configuration logs, and toolchain description;
- FFmpeg, libmp3lame, eSpeak NG, and uv copyright/license notices;
- the generated CycloneDX SBOM and `THIRD_PARTY_NOTICES.md`;
- the lockfile and bundle checksums; and
- a durable written source offer where required by the applicable license.

The source archive published beside a release is the compliance source of record. A link to an
upstream repository alone is not a substitute for the exact source and build material shipped for
that release. Legal review must approve the static-linking obligations and notices before the first
public 1.0 package.
