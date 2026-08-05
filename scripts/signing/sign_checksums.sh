#!/usr/bin/env bash
set -euo pipefail

if [[ $# -ne 1 ]]; then
  echo "usage: sign_checksums.sh PATH_TO_SHA256SUMS" >&2
  exit 2
fi
for variable_name in RELEASE_GPG_KEY_ID; do
  if [[ -z "${!variable_name:-}" ]]; then
    echo "checksum signing failed: missing ${variable_name}" >&2
    exit 2
  fi
done

checksums_path="$1"
if [[ ! -f "${checksums_path}" ]]; then
  echo "checksum signing failed: file does not exist: ${checksums_path}" >&2
  exit 2
fi

if ! gpg --batch --list-secret-keys "${RELEASE_GPG_KEY_ID}" >/dev/null 2>&1; then
  echo "checksum signing failed: configured key is absent from the local GPG keyring" >&2
  exit 2
fi

gpg --batch --yes --armor \
  --local-user "${RELEASE_GPG_KEY_ID}" \
  --output "${checksums_path}.asc" \
  --detach-sign "${checksums_path}"
gpg --batch --armor \
  --local-user "${RELEASE_GPG_KEY_ID}" \
  --output "$(dirname "${checksums_path}")/RELEASE_KEY.asc" \
  --export "${RELEASE_GPG_KEY_ID}"
echo "checksum manifest signed with the locally provisioned release key"
