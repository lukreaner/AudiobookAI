# Native desktop release procedure

AudiobookAI's supported public distribution is the signed Tauri desktop application. Release
automation does not build command-line or Python artifacts.

## Non-negotiable secret boundary

No private key, private certificate bundle, provider key, API token, password, passphrase, session
credential, or decrypted application secret may be committed to Git, configured as a GitHub
Actions/Dependabot secret, entered as a workflow input, cached by GitHub, attached to a workflow,
attested, or published in a GitHub Release. This includes GitHub encrypted repository,
organization, and environment secrets.

GitHub environments are approval gates only. Both `stable-release` and `stable-publish` must have
required reviewers and **zero configured secrets or variables**. The workflow policy test rejects
all `secrets.*` expressions, legacy certificate/key inputs, private workflow-dispatch fields, and an
upload, attestation, or release step that is not immediately preceded by the repository scanner.

GitHub creates its own short-lived job token for checkout and the final GitHub Release API call.
That platform token originates and remains inside GitHub; no personal access token or other
operator token is configured or uploaded. `persist-credentials: false` prevents checkout from
writing the job token into the repository configuration.

Public verification keys, public certificates embedded in code signatures, detached signatures,
and certificate thumbprints are intentionally distributable. Their corresponding private material
is not.

## Artifact matrix and trust split

Credential-free quality tests and public sidecar verification run on GitHub-hosted runners. Every
operation that can access signing or notarization material runs only on dedicated, owner-controlled
self-hosted native runners selected by all of these labels:

- `self-hosted` and `audiobookai-release`;
- `windows`/`x64`, `macos`/`arm64`, or `linux`/`x64` as applicable.

| Target | Owner-controlled runner | Supported artifact | Local signing operation |
| --- | --- | --- | --- |
| Windows 10/11 x64 | `windows`, `x64` | NSIS `.exe` with offline WebView2 | local certificate store + updater signer |
| macOS 13+ universal | `macos`, `arm64` | universal arm64/x86_64 `.dmg` | local Keychain, notary profile + updater signer |
| Linux x64 | `linux`, `x64` | `.AppImage` and Debian-compatible `.deb` | updater signer + local GPG keyring |

The self-hosted runners must be dedicated to this repository, denied to forks and pull requests,
kept out of shared runner groups, patched, monitored, and cleaned between releases. Protect the
`audiobookai-release` label and the release environments with owner approval. Restrict outbound
network access to the public dependency/sidecar endpoints, Apple notarization on macOS, and GitHub
artifact/publication endpoints. Never enable Actions debug tracing on a release runner.

Each candidate contains `latest.json`, a CycloneDX 1.6 SBOM, generated notices, the public sidecar
lockfile, detached checksum signature, GitHub build-provenance attestation, and the exact
corresponding-source archive.

## Local runner provisioning

Provision private material directly on each owner-controlled machine using an offline or otherwise
owner-controlled channel. Never copy it through an issue, pull request, repository file, workflow
dispatch field, Actions secret, Actions artifact, build log, cloud drive, chat, or email.

All native runners require:

- `TAURI_SIGNING_PRIVATE_KEY`: an absolute path to the local updater signing key, outside the
  checkout. Raw key content is rejected. On Unix the file must be owner-accessible only.
- `TAURI_SIGNING_PRIVATE_KEY_PASSWORD`: available only in the local runner process environment.
- `TAURI_SIGNING_PUBLIC_KEY`: the matching public updater key.
- `RELEASE_TAG_SIGNING_FINGERPRINT`: the expected public OpenPGP fingerprint; its public key must
  already be in the local release verifier's GPG keyring.

The runner service must not print or persist its environment. The workflow checks only presence and
key isolation, never values.

Windows additionally requires a valid code-signing certificate with its non-exportable private key
preinstalled in `Cert:\CurrentUser\My` for the runner account, plus
`WINDOWS_CERTIFICATE_THUMBPRINT` identifying it. The workflow never accepts, creates, decodes,
imports, or uploads a `.pfx`/`.p12` bundle or its password.

macOS additionally requires a Developer ID signing identity preinstalled in the runner account's
Keychain, `APPLE_SIGNING_IDENTITY` containing only its public name, and `APPLE_NOTARY_PROFILE`
naming credentials previously installed locally with `xcrun notarytool store-credentials`. The
workflow submits only the already built disk image for notarization. It never accepts Apple account
credentials, API-key material, certificate bundles, or keychain passwords.

The Linux release/aggregation runner additionally requires `RELEASE_GPG_KEY_ID`. The matching
checksum-signing secret key must already exist in its local GPG keyring or attached HSM. The signing
script never imports a private key and exports only the public verification key.

## Public sidecar inputs

Every sidecar archive and detached signature must be anonymously downloadable from a stable public
HTTPS URL recorded in `packaging/sidecars.lock.json`. URLs containing user information, query
parameters, or fragments are rejected. The fetcher has no authorization-header or token support.
It verifies the pinned archive/signature SHA-256 values, the detached OpenPGP signature against the
committed public key, safe extraction, and each critical-file hash.

Do not use private releases, pre-signed object-store links, authenticated endpoints, or expiring
URLs for sidecars.

## Workflow separation and publication boundaries

- `ci.yml` runs normal locked builds and tests on GitHub-hosted native runners. It receives no
  provider or release credential and never accesses paid provider APIs. A successful run caused
  by a push to `main` is the only event that can trigger `snapshot.yml`.
- `snapshot.yml` checks out that successful CI run's immutable commit, builds the complete
  GitHub-hosted Windows, macOS, and Linux development matrix, verifies the combined checksums and
  uploaded bytes, and publishes a clearly marked untrusted prerelease. Pull-request, fork,
  manually dispatched, cancelled, and failed CI runs cannot publish a snapshot.
- `release.yml` first runs credential-free quality gates, then locally verifies the tag signature
  on the owner-controlled Linux release runner before any signing job can start. A qualifying
  annotated stable tag automatically enters this pipeline.
- Native package and aggregate jobs use only locally provisioned signers. They upload signatures
  and signed artifacts, never the signing material.
- After the complete stable aggregate passes, the tag-triggered run automatically enters the
  `stable-publish` job. Required reviewers on that empty environment can still pause final
  publication; no second workflow-dispatch run is required. Manual dispatch remains a recovery
  path for an existing tag, and its explicit `publish` choice never bypasses an environment gate.

Every third-party GitHub Action is pinned to a full commit SHA. Before each
`actions/upload-artifact`, provenance attestation, and `gh release create` step, the exact candidate
path is scanned. The publish job checks out the immutable tag first, downloads the candidate,
re-scans it, verifies the detached checksum signature, and scans it again immediately before the
GitHub Release upload. Matched values are never printed.

Run the policy and scanner locally before release work:

```text
python3 scripts/release/validate_workflow.py
python3 scripts/security/check_no_secrets.py --current --history
python3 -m unittest discover -s scripts/release/tests
```

If any scan reports a possible credential, stop. Revoke the affected value, remove it from every
reachable Git object and local artifact, and rerun the full-history scan. A later deletion commit
does not make an earlier secret-bearing commit safe to push.

## Release preparation

1. Complete every feature, provider contract, security, media, accessibility, localization, and
   clean-machine gate. A passing build is not evidence that those product gates passed.
2. Audit native sidecar builds as described in [sidecars.md](sidecars.md), replace every placeholder
   public URL/hash, collect detached signatures, and set `releaseReady` to `true`.
3. Set the same stable version in the Cargo workspace, Tauri config, and dashboard package. The
   release tag must be exactly `v<version>` and the major version must be at least 1.
4. From a credential-free development checkout, run the history scanner, commit clean lockfiles,
   create an annotated OpenPGP-signed tag, scan again, and push only after the installed pre-push
   hook has scanned the exact object IDs Git is about to send. That push starts `Native desktop
   release` automatically.
5. Approve `stable-release` only after confirming the immutable tag and dedicated runners. Review
   code-signing output, notarization/stapling, updater signatures, SBOM, source bundle, checksums,
   provenance, and every scanner result as the jobs complete.
6. Install the staged workflow artifacts on disposable clean machines and complete the matrix
   below before approving `stable-publish`. Configure that environment with required reviewers so
   this acceptance gate cannot be skipped for a stable release.
7. Approval of the empty `stable-publish` environment lets the same run create an unpublished
   draft, upload and download every asset for byte verification, and then make it public. Do not
   rerun or recreate the tag.

The workflow fails closed for a mismatched/untrusted tag, pre-1.0 version, unresolved sidecar
manifest, absent local signer, updater key inside the checkout, absent updater signature, failed
notarization, scan finding, or incomplete artifact matrix.

GitHub secret scanning with push protection and protected-branch rules are mandatory repository
settings. CI runs only after source has reached GitHub, so a passing CI history scan is detection,
not proof that the remote push boundary was protected.

## Clean-machine acceptance gate

For every supported OS, verify at minimum:

- installer signature, notarization/stapling where applicable, double-click launch, and uninstall;
- launch with an empty `PATH` and without Node.js, Python, Docker, FFmpeg, or eSpeak installed;
- bundled dashboard, FFmpeg/ffprobe, Linux eSpeak voices, and offline WebView2 operation;
- EPUB association is optional and opening a file creates a draft without modifying the source;
- keychain/passphrase setup, provider configuration wizard, preview, estimate, dry run, conversion,
  progressive playback, every export format, metadata/cover/chapter markers, pause/crash/resume;
- tray continuation and explicit Quit behavior for active jobs and app-owned provider children;
- authenticated TLS LAN setup, revocation, CSRF/Origin enforcement, and loopback defaults;
- update approval, active-job deferral, data preservation, rollback/recovery documentation; and
- uninstall behavior leaves user data only when explicitly selected.

Record machine image, OS build, hashes, exact result, tester, and timestamp in signed release
evidence. Evidence must contain no environment dump, command history, credential-store path, user
home listing, or raw signing/notarization response.

## Known scaffold blockers

The committed sidecar lock intentionally contains unresolved hashes and `releaseReady: false`, and
the application version remains pre-1.0. The release workflow must therefore fail today. The three
dedicated native release runners, local signing identities, local notarization profile, public
sidecar bundles, verified GitHub push-protection settings, and clean-machine evidence are operator
prerequisites that are not present in the repository. Before enabling the MLX-audio installer in a
release, its exact managed Python distribution and complete transitive wheel set must also be locked
and hash-verified from reviewed public sources, staged in the fixed offline bundle layout, covered by
SBOM/notices, and exercised by a network-blocked clean-machine installation. The runtime already
refuses missing, corrupt, symlinked, wrong-version, or incomplete payloads; the unresolved release
hashes intentionally keep packaging blocked. Pinning only `mlx-audio==0.4.6` is not a reproducible
supply-chain boundary. Provider-native model-download operations also still require a durable
restart journal, and reasoning controls require model-specific capability discovery before the 1.0
contract can be claimed. These are safety gates, not optional warnings.
