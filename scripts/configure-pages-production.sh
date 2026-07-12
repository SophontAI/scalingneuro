#!/usr/bin/env bash
set -euo pipefail

: "${CLOUDFLARE_API_TOKEN:?CLOUDFLARE_API_TOKEN is required}"
: "${CLOUDFLARE_ACCOUNT_ID:?CLOUDFLARE_ACCOUNT_ID is required}"

PROJECT_NAME=${SCALING_NEURO_PAGES_PROJECT:-scalingneuro}
endpoint="https://api.cloudflare.com/client/v4/accounts/${CLOUDFLARE_ACCOUNT_ID}/pages/projects/${PROJECT_NAME}"
authorization="Authorization: Bearer ${CLOUDFLARE_API_TOKEN}"
payload='{"deployment_configs":{"production":{"fail_open":false,"limits":{"cpu_ms":300000}},"preview":{"fail_open":false}}}'

curl --fail --silent --show-error --request PATCH \
  --header "$authorization" \
  --header 'content-type: application/json' \
  --data "$payload" \
  "$endpoint" >/dev/null

project=$(curl --fail --silent --show-error --header "$authorization" "$endpoint")
jq --exit-status '
  .success == true and
  .result.deployment_configs.production.fail_open == false and
  .result.deployment_configs.production.limits.cpu_ms == 300000 and
  .result.deployment_configs.production.d1_databases.DB.id != null and
  .result.deployment_configs.production.r2_buckets.ARCHIVE.name != null and
  .result.deployment_configs.production.env_vars.ADMIN_API_TOKEN.type == "secret_text" and
  .result.deployment_configs.production.env_vars.SITE_KEY_ENCRYPTION_KEY_B64.type == "secret_text" and
  .result.deployment_configs.production.env_vars.R2_PARENT_SECRET_ACCESS_KEY.type == "secret_text" and
  .result.deployment_configs.preview.fail_open == false and
  ((.result.deployment_configs.preview.env_vars // {}) | length) == 0 and
  ((.result.deployment_configs.preview.d1_databases // {}) | length) == 0 and
  ((.result.deployment_configs.preview.r2_buckets // {}) | length) == 0
' <<<"$project" >/dev/null

echo "Verified fail-closed production Pages bindings, 300 s CPU budget, and isolated previews."
