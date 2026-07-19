#!/usr/bin/env bash
set -euo pipefail

if [[ $# -ne 2 ]]; then
  echo "Usage: rollback-pages-production.sh <deployment-id> <commit-hash-or-empty>" >&2
  exit 2
fi

DEPLOYMENT_ID=$1
COMMIT_HASH=$2
CLOUDFLARE_API_ORIGIN=${CLOUDFLARE_API_ORIGIN:-https://api.cloudflare.com/client/v4}
SCALING_NEURO_ORIGIN=${SCALING_NEURO_ORIGIN:-https://scalingneuro.com}
CURL_BIN=${CURL_BIN:-curl}
POLL_ATTEMPTS=${PAGES_ROLLBACK_ATTEMPTS:-20}
POLL_INTERVAL_SECONDS=${PAGES_ROLLBACK_INTERVAL_SECONDS:-5}

: "${CLOUDFLARE_API_TOKEN:?CLOUDFLARE_API_TOKEN is required}"
: "${CLOUDFLARE_ACCOUNT_ID:?CLOUDFLARE_ACCOUNT_ID is required}"
[[ "$DEPLOYMENT_ID" =~ ^[0-9a-f]{32}$ ||
   "$DEPLOYMENT_ID" =~ ^[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}$ ]] || {
  echo "Unsafe Cloudflare Pages deployment ID" >&2
  exit 2
}
[[ -z "$COMMIT_HASH" || "$COMMIT_HASH" =~ ^[0-9a-f]{40}$ ]] || {
  echo "Unsafe Cloudflare Pages commit hash" >&2
  exit 2
}
if [[ ! "$POLL_ATTEMPTS" =~ ^[0-9]+$ ]] ||
   (( POLL_ATTEMPTS < 1 || POLL_ATTEMPTS > 120 )); then
  echo "PAGES_ROLLBACK_ATTEMPTS must be between 1 and 120" >&2
  exit 2
fi
if [[ ! "$POLL_INTERVAL_SECONDS" =~ ^[0-9]+$ ]] ||
   (( POLL_INTERVAL_SECONDS > 60 )); then
  echo "PAGES_ROLLBACK_INTERVAL_SECONDS must be between 0 and 60" >&2
  exit 2
fi

api_base="${CLOUDFLARE_API_ORIGIN%/}/accounts/${CLOUDFLARE_ACCOUNT_ID}/pages/projects/scalingneuro"
rollback=$(
  "$CURL_BIN" --fail --silent --show-error \
    --request POST \
    --header "Authorization: Bearer $CLOUDFLARE_API_TOKEN" \
    --header "Content-Type: application/json" \
    --data '{}' \
    "$api_base/deployments/${DEPLOYMENT_ID}/rollback"
)
jq --exit-status --arg id "$DEPLOYMENT_ID" '
  .success == true and .result.id == $id
' <<<"$rollback" >/dev/null

for attempt in $(seq 1 "$POLL_ATTEMPTS"); do
  project=$(
    "$CURL_BIN" --fail --silent --show-error \
      --header "Authorization: Bearer $CLOUDFLARE_API_TOKEN" \
      "$api_base" || true
  )
  canonical_id=$(jq -r '.result.canonical_deployment.id // ""' <<<"$project" 2>/dev/null || true)
  version=$(
    "$CURL_BIN" --fail --silent --show-error --max-time 15 \
      "${SCALING_NEURO_ORIGIN%/}/version.json?rollback=$DEPLOYMENT_ID&attempt=$attempt" || true
  )
  if [[ "$canonical_id" == "$DEPLOYMENT_ID" ]] &&
     { [[ -z "$COMMIT_HASH" ]] ||
       jq --exit-status --arg sha "$COMMIT_HASH" '.commit == $sha' <<<"$version" >/dev/null 2>&1; }; then
    echo "Restored production deployment $DEPLOYMENT_ID."
    exit 0
  fi
  sleep "$POLL_INTERVAL_SECONDS"
done

echo "Cloudflare Pages did not converge to saved deployment $DEPLOYMENT_ID" >&2
exit 1
