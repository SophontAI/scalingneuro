#!/usr/bin/env bash
set -euo pipefail

ROOT_DIR=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)
WORK_DIR=$(mktemp -d)
trap 'rm -rf "$WORK_DIR"' EXIT

DEPLOYMENT_ID=01234567-89ab-cdef-0123-456789abcdef
LEGACY_DEPLOYMENT_ID=0123456789abcdef0123456789abcdef
COMMIT_HASH=0123456789abcdef0123456789abcdef01234567
MOCK_CURL=$WORK_DIR/curl

cat > "$MOCK_CURL" <<'EOF'
#!/usr/bin/env bash
set -euo pipefail
url=${!#}
if [[ "$url" == */rollback ]]; then
  printf '{"success":true,"result":{"id":"%s"}}\n' "$MOCK_DEPLOYMENT_ID"
elif [[ "$url" == */pages/projects/scalingneuro ]]; then
  if [[ "${MOCK_ROLLBACK_STUCK:-false}" == true ]]; then
    printf '{"success":true,"result":{"canonical_deployment":{"id":"ffffffff-ffff-ffff-ffff-ffffffffffff"}}}\n'
  else
    printf '{"success":true,"result":{"canonical_deployment":{"id":"%s"}}}\n' "$MOCK_DEPLOYMENT_ID"
  fi
elif [[ "$url" == */version.json* ]]; then
  printf '{"commit":"%s"}\n' "$MOCK_COMMIT_HASH"
else
  echo "Unexpected mock curl URL: $url" >&2
  exit 1
fi
EOF
chmod +x "$MOCK_CURL"

export CLOUDFLARE_API_TOKEN=test-token
export CLOUDFLARE_ACCOUNT_ID=abcdef0123456789abcdef0123456789
export CLOUDFLARE_API_ORIGIN=https://mock-cloudflare.invalid/client/v4
export SCALING_NEURO_ORIGIN=https://mock-scaling-neuro.invalid
export CURL_BIN=$MOCK_CURL
export MOCK_DEPLOYMENT_ID=$DEPLOYMENT_ID
export MOCK_COMMIT_HASH=$COMMIT_HASH
export PAGES_ROLLBACK_ATTEMPTS=2
export PAGES_ROLLBACK_INTERVAL_SECONDS=0

"$ROOT_DIR/scripts/rollback-pages-production.sh" "$DEPLOYMENT_ID" "$COMMIT_HASH" >/dev/null

MOCK_DEPLOYMENT_ID=$LEGACY_DEPLOYMENT_ID \
  "$ROOT_DIR/scripts/rollback-pages-production.sh" "$LEGACY_DEPLOYMENT_ID" "$COMMIT_HASH" >/dev/null

if MOCK_ROLLBACK_STUCK=true \
  "$ROOT_DIR/scripts/rollback-pages-production.sh" "$DEPLOYMENT_ID" "$COMMIT_HASH" >/dev/null 2>&1; then
  echo "A rollback that never became canonical was not rejected" >&2
  exit 1
fi

if "$ROOT_DIR/scripts/rollback-pages-production.sh" ../unsafe "$COMMIT_HASH" >/dev/null 2>&1; then
  echo "An unsafe rollback deployment ID was not rejected" >&2
  exit 1
fi

if "$ROOT_DIR/scripts/rollback-pages-production.sh" 01234567-89ab-cdef-0123-456789abcde "$COMMIT_HASH" >/dev/null 2>&1; then
  echo "A truncated rollback deployment UUID was not rejected" >&2
  exit 1
fi

echo "Pages rollback tests passed"
