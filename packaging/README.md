# Packaging inputs

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

See [the sidecar compliance procedure](../docs/sidecars.md) before updating any hash.
