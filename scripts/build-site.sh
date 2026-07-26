#!/usr/bin/env bash
set -euo pipefail

ROOT_DIR=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)
DIST_DIR="$ROOT_DIR/dist"
COMMIT_SHA=${GITHUB_SHA:-$(git -C "$ROOT_DIR" rev-parse HEAD)}

rm -rf "$DIST_DIR"
mkdir -p "$DIST_DIR/downloads" "$DIST_DIR/docs" "$DIST_DIR/schemas/examples"

install -m 0644 \
  "$ROOT_DIR/index.html" \
  "$ROOT_DIR/404.html" \
  "$ROOT_DIR/favicon.svg" \
  "$ROOT_DIR/og.png" \
  "$ROOT_DIR/styles.css" \
  "$ROOT_DIR/app.js" \
  "$DIST_DIR/"
install -m 0644 "$ROOT_DIR/downloads/index.html" "$DIST_DIR/downloads/"
install -m 0644 \
  "$ROOT_DIR/docs/contribution-policy.html" \
  "$ROOT_DIR/docs/de-identification-procedure.html" \
  "$ROOT_DIR/docs/dicom-deidentification-policy.md" \
  "$DIST_DIR/docs/"

# Installers are release artifacts, never source-tree fallbacks. The production
# deployment restores only a release whose version matches this source; the
# release workflow writes the newly verified installers into this directory.

for schema in \
  common-v1.schema.json \
  archive-access-request-v1.schema.json \
  archive-access-response-v1.schema.json \
  archive-list-v1.schema.json \
  contribution-info-v1.schema.json \
  registration-request-v1.schema.json \
  registration-response-v1.schema.json \
  device-policy-v1.schema.json \
  dicom-archive-manifest-v2.schema.json \
  dicom-upload-init-v1.schema.json \
  dicom-upload-session-v1.schema.json \
  dicom-upload-status-v1.schema.json \
  local-manifest-v1.schema.json \
  upload-complete-v1.schema.json \
  upload-part-request-v1.schema.json \
  upload-part-response-v1.schema.json \
  api-error-v1.schema.json; do
  install -m 0644 "$ROOT_DIR/schemas/$schema" "$DIST_DIR/schemas/"
done
for example in \
  archive-access-request-v1.example.json \
  archive-access-response-v1.example.json \
  archive-list-v1.example.json \
  registration-request-v1.example.json \
  registration-response-v1.example.json \
  contribution-info-v1.example.json \
  dicom-archive-manifest-v2.example.json \
  dicom-upload-init-v1.example.json \
  dicom-upload-session-v1.example.json \
  dicom-upload-session-checkpointed-v1.example.json \
  dicom-upload-status-v1.example.json \
  dicom-upload-status-already-received-v1.example.json \
  local-manifest-v1.example.json \
  upload-complete-v1.example.json \
  upload-part-request-v1.example.json \
  upload-part-response-v1.example.json \
  api-error-v1.example.json; do
  install -m 0644 "$ROOT_DIR/schemas/examples/$example" "$DIST_DIR/schemas/examples/"
done

install -m 0644 \
  "$ROOT_DIR/docs/epi-ingestion-contract.md" \
  "$ROOT_DIR/docs/artifact-and-api-contracts.md" \
  "$ROOT_DIR/docs/collaborator-onboarding.md" \
  "$ROOT_DIR/docs/client-release.md" \
  "$DIST_DIR/docs/"

if [[ ! -x "$ROOT_DIR/worker/node_modules/.bin/esbuild" ]]; then
  echo "worker dependencies are missing; run: npm ci --prefix worker" >&2
  exit 1
fi
npm run build:pages --prefix "$ROOT_DIR/worker"

printf '{"commit":"%s"}\n' "$COMMIT_SHA" > "$DIST_DIR/version.json"
