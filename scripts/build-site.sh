#!/usr/bin/env bash
set -euo pipefail

ROOT_DIR=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)
DIST_DIR="$ROOT_DIR/dist"
COMMIT_SHA=${GITHUB_SHA:-$(git -C "$ROOT_DIR" rev-parse HEAD)}

rm -rf "$DIST_DIR"
mkdir -p "$DIST_DIR/downloads"

install -m 0644 \
  "$ROOT_DIR/index.html" \
  "$ROOT_DIR/404.html" \
  "$ROOT_DIR/styles.css" \
  "$ROOT_DIR/app.js" \
  "$ROOT_DIR/_worker.js" \
  "$DIST_DIR/"
install -m 0755 "$ROOT_DIR/downloads/neuro-sync-preview.sh" "$DIST_DIR/downloads/"

printf '{"commit":"%s"}\n' "$COMMIT_SHA" > "$DIST_DIR/version.json"
