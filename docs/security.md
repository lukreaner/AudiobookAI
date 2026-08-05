# Security model

AudiobookAI is single-owner software. It is not a multi-tenant server.

- The desktop listener binds loopback over HTTP by default. It does not read or
  require certificate files in this mode.
- A one-time launch nonce creates an HttpOnly local session; host and origin are
  exact-match validated and wildcard CORS is never enabled.
- LAN mode is an explicit startup configuration and requires an owner password
  or API token plus TLS. Rustls reads a configured PEM certificate chain and a
  separate PEM private-key file. HTTPS session and CSRF cookies carry `Secure`.
  An insecure HTTP override is separately confirmed and prominently displayed;
  it never disables TLS when a TLS identity is present.
- LAN requests must use an exact configured hostname or concrete bind IP, the
  active listener port, and the active `https` scheme. Wildcard binds such as
  `0.0.0.0` therefore require an advertised hostname/IP; the wildcard itself is
  never accepted as a Host authority. Origins on a different alias, scheme, or
  port are rejected even if that alias is otherwise configured.
- Provider endpoints started by AudiobookAI remain loopback-only. Remote clients
  reach them solely through capability-checked application commands.
- Process control retains the child handle and ownership token; a matching port
  or PID alone is never authority to terminate a process.
- A provider credential is decrypted only for an intentional request to the
  exact endpoint configured by the owner. It is attached as the provider's
  authentication header, never placed in a URL or request body, and HTTP
  redirects and inherited operating-system/environment proxies are disabled so
  it cannot be forwarded to another host or an ambient proxy. This
  necessary authentication exchange is distinct from uploading or exporting a
  credential; no diagnostic, project, cache, manifest, or release path accepts
  secret material.

Provider secrets are encrypted using AES-256-GCM with a unique nonce and record
identity as additional authenticated data. The random master key is stored in
the platform keychain. If the keychain is unavailable, the desktop setup
requires an explicit Argon2id passphrase; plaintext fallback is not supported.
Logs and error payloads redact authorization headers, query credentials,
cookies, and known secret values.

On Unix platforms, AudiobookAI tightens its entire managed data tree to mode
`0700` on every startup and its SQLite database, writer lock, migration
backups, and passphrase salt to mode `0600`. This also repairs permissive modes
left by an earlier application version. Windows data remains scoped to the
signed-in user's platform application-data directory; installer ACL behavior is
part of the native clean-machine security acceptance gate. User-selected export
destinations retain the permissions the owner selected.

## Diagnostics privacy boundary

Application diagnostics are local and opt-in to share. AudiobookAI has no
automatic crash-reporting or log-upload path. The authenticated Diagnostics UI
reads a bounded in-memory ring; the service also writes size-bounded rotating
JSONL files under the managed data directory for local crash investigation.
On Unix platforms that directory is owner-only and each file is owner-readable
and owner-writable only.

The diagnostic tracing layer is an allowlist, not a best-effort scrubber. It
keeps only approved static event summaries and narrowly shaped scalar metadata
such as a matched API route template, status, duration, and opaque UUID. It
never reads or records raw URLs or query strings, request/response bodies,
headers, cookies, provider payloads, EPUB/book text, reference audio, or
credential/error values. Unknown messages and fields are replaced or omitted.
Exports are regenerated from the sanitized memory records and sanitized again;
the on-disk log files are never served directly. App-managed provider child
stdout/stderr remains a separate, explicitly opened provider-log view.

## Git and release secret boundary

Private keys, provider credentials, API tokens, passphrases, session cookies,
and decrypted secret-store values are never release inputs or repository data.
Local environment files and common private-key containers are ignored. The
repository's pre-commit hook scans the exact staged blobs without printing a
matched value. The pre-push hook then scans the complete working tree, every
reachable Git blob, and each exact object ID Git reports it is about to send,
including a detached commit pushed directly by hash. Native CI
and the release workflow repeat those checks before any build or publication
step. Install both committed hooks with `make install-hooks`. Public
signing-verification keys and public certificates are not secrets, but their
corresponding private material remains only on owner-controlled release hosts.

Local hooks protect only checkouts where they are installed and can be bypassed
deliberately, so the public repository must also enable GitHub secret scanning
with push protection and reject direct pushes to protected branches. The CI
history scan detects a bad contribution after GitHub has received it and is
therefore not a substitute for remote push protection. AudiobookAI's 1.0
release gate remains blocked until those repository settings are independently
verified. No automated detector can recognize every private value, so real
credentials and signing material must never be copied into the checkout in the
first place.
GitHub Actions encrypted repository, organization, and environment secrets are
explicitly forbidden, as are credential-bearing workflow inputs and artifacts.
The protected GitHub release environments are approval gates with no configured
secrets or variables. Windows uses a locally installed non-exportable
certificate key, macOS uses a local Keychain identity and notary profile, and
checksum signing uses a local GPG keyring/HSM. The updater signing environment
references an owner-only file outside the checkout; raw key content is rejected.

If a scanner ever reports a credential, do not merely delete it in a later
commit: revoke it first, purge it from Git history, and rerun the full-history
gate before any remote push.

Release sidecars must be anonymously downloadable from public, checksum-pinned
HTTPS URLs. The fetcher has no token or authorization-header support and rejects
credential-bearing URLs. Every sidecar tree is scanned immediately before it
can become a GitHub artifact.

Cloud processing is consented per project. The preflight screen identifies the
provider and whether book text, audio, or voice references leave the computer.

## LAN lifecycle

LAN bind, TLS identity, and advertised-host changes do not hot-rebind the active
listener. They are pending until the desktop process restarts and constructs its
startup `ServiceConfig` from the completed LAN configuration. Startup fails
closed if TLS is selected but either PEM file is absent or invalid; it does not
fall back to plaintext. A supplied private-key file must be owner-readable and
stored outside exported project data. AudiobookAI does not generate or install
trusted certificates; the configuration wizard records the certificate and key
paths selected by the owner.
