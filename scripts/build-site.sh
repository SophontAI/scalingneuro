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
  "$ROOT_DIR/styles.css" \
  "$ROOT_DIR/app.js" \
  "$DIST_DIR/"
install -m 0644 "$ROOT_DIR/downloads/index.html" "$DIST_DIR/downloads/"
install -m 0644 "$ROOT_DIR/docs/contribution-policy.html" "$DIST_DIR/docs/"

# Keep the terminal-first route usable during the first source deployment. A
# published client release replaces these bootstrap scripts with release-matched
# copies after verifying the same package hashes.
"$ROOT_DIR/scripts/render-installers.sh" \
  "$DIST_DIR" \
  "0.2.0" \
  "https://scalingneuro.com/downloads" \
  "neuro-sync-v0.2.0-macos-universal-UNSIGNED-PILOT.zip" \
  "cdd1d618946d17ebd861430c3427738a6a54443db00c93112f1cb8d69845fd25" \
  "neuro-sync-v0.2.0-linux-x86_64.tar.gz" \
  "ab34b5343bca8900a7491177385c71798228d9dfe1e23900d8ee60f44d64ba63" \
  "neuro-sync-v0.2.0-windows-x86_64-UNSIGNED-PILOT.zip" \
  "4db12034cfa8c69e6aab77ee3baf17cb4b43b4d78ac88c8ec9ccc9199e56f0d1"

for schema in \
  common-v1.schema.json \
  scan-sidecar-v1.schema.json \
  metadata-policy-v1.schema.json \
  metadata-policy-v1.json \
  contribution-info-v1.schema.json \
  registration-request-v1.schema.json \
  enrollment-request-v1.schema.json \
  enrollment-response-v1.schema.json \
  local-manifest-v1.schema.json \
  upload-init-v1.schema.json \
  upload-session-v1.schema.json \
  upload-complete-v1.schema.json \
  archive-manifest-v1.schema.json \
  upload-status-v1.schema.json \
  upload-part-request-v1.schema.json \
  upload-part-response-v1.schema.json \
  api-error-v1.schema.json; do
  install -m 0644 "$ROOT_DIR/schemas/$schema" "$DIST_DIR/schemas/"
done
install -m 0644 "$ROOT_DIR"/schemas/examples/*.json "$DIST_DIR/schemas/examples/"

install -m 0644 \
  "$ROOT_DIR/docs/epi-ingestion-contract.md" \
  "$ROOT_DIR/docs/artifact-and-api-contracts.md" \
  "$ROOT_DIR/docs/collaborator-onboarding.md" \
  "$ROOT_DIR/docs/client-release.md" \
  "$ROOT_DIR/docs/vendor-qa.md" \
  "$DIST_DIR/docs/"

if [[ ! -x "$ROOT_DIR/worker/node_modules/.bin/esbuild" ]]; then
  echo "worker dependencies are missing; run: npm ci --prefix worker" >&2
  exit 1
fi
npm run build:pages --prefix "$ROOT_DIR/worker"

printf '{"commit":"%s"}\n' "$COMMIT_SHA" > "$DIST_DIR/version.json"
