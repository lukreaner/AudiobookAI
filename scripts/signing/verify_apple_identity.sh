#!/usr/bin/env bash
set -euo pipefail

for variable_name in APPLE_SIGNING_IDENTITY APPLE_NOTARY_PROFILE; do
  if [[ -z "${!variable_name:-}" ]]; then
    echo "Apple signing setup failed: missing local ${variable_name} configuration" >&2
    exit 2
  fi
done

if ! security find-identity -v -p codesigning 2>/dev/null \
  | grep -Fq "${APPLE_SIGNING_IDENTITY}"; then
  echo "Apple signing setup failed: configured identity is absent from the local keychain" >&2
  exit 2
fi

# The profile remains in the macOS Keychain. notarytool authenticates without exposing or copying
# its Apple credentials into the repository, workflow definition, command line, or artifact tree.
if ! xcrun notarytool history \
  --keychain-profile "${APPLE_NOTARY_PROFILE}" \
  --output-format json >/dev/null 2>&1; then
  echo "Apple signing setup failed: local notary profile is unavailable" >&2
  exit 2
fi

echo "Local Apple signing identity and notary profile are available"
