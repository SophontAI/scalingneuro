#!/usr/bin/env bash
set -euo pipefail

: "${CLOUDFLARE_API_TOKEN:?CLOUDFLARE_API_TOKEN is required}"
: "${CLOUDFLARE_ACCOUNT_ID:?CLOUDFLARE_ACCOUNT_ID is required}"

PROJECT_NAME=${SCALING_NEURO_PAGES_PROJECT:-scalingneuro}
repository_root=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)
wrangler_config=${SCALING_NEURO_WRANGLER_CONFIG:-"$repository_root/worker/wrangler.jsonc"}
endpoint="https://api.cloudflare.com/client/v4/accounts/${CLOUDFLARE_ACCOUNT_ID}/pages/projects/${PROJECT_NAME}"
authorization="Authorization: Bearer ${CLOUDFLARE_API_TOKEN}"
payload='{"deployment_configs":{"production":{"fail_open":false,"limits":{"cpu_ms":300000}},"preview":{"fail_open":false}}}'

# The Pages deployment does not consume worker/wrangler.jsonc. Treat the
# version-controlled Worker configuration as the production target and refuse
# to deploy when the dashboard points at a different account, database,
# bucket, or signing/TTL value.
jq --exit-status '
  ([.d1_databases[] | select(.binding == "DB")] | length) == 1 and
  ([.r2_buckets[] | select(.binding == "ARCHIVE")] | length) == 1 and
  (.vars | keys | sort) == [
    "CREDENTIAL_TTL_SECONDS",
    "R2_ACCOUNT_ID",
    "R2_BUCKET_NAME",
    "R2_PARENT_ACCESS_KEY_ID",
    "UPLOAD_TTL_SECONDS"
  ]
' "$wrangler_config" >/dev/null

expected_d1_id=$(jq --raw-output '.d1_databases[] | select(.binding == "DB") | .database_id' "$wrangler_config")
expected_r2_bucket=$(jq --raw-output '.r2_buckets[] | select(.binding == "ARCHIVE") | .bucket_name' "$wrangler_config")
expected_r2_account=$(jq --raw-output '.vars.R2_ACCOUNT_ID' "$wrangler_config")
expected_r2_access_key=$(jq --raw-output '.vars.R2_PARENT_ACCESS_KEY_ID' "$wrangler_config")
expected_r2_bucket_var=$(jq --raw-output '.vars.R2_BUCKET_NAME' "$wrangler_config")
expected_credential_ttl=$(jq --raw-output '.vars.CREDENTIAL_TTL_SECONDS' "$wrangler_config")
expected_upload_ttl=$(jq --raw-output '.vars.UPLOAD_TTL_SECONDS' "$wrangler_config")

if [[ "$expected_r2_account" != "$CLOUDFLARE_ACCOUNT_ID" ]]; then
  echo "R2_ACCOUNT_ID in $wrangler_config does not match CLOUDFLARE_ACCOUNT_ID" >&2
  exit 1
fi
if [[ "$expected_r2_bucket" != "$expected_r2_bucket_var" ]]; then
  echo "R2 binding and R2_BUCKET_NAME disagree in $wrangler_config" >&2
  exit 1
fi

curl --fail --silent --show-error --request PATCH \
  --header "$authorization" \
  --header 'content-type: application/json' \
  --data "$payload" \
  "$endpoint" >/dev/null

project=$(curl --fail --silent --show-error --header "$authorization" "$endpoint")
if ! jq --exit-status \
  --arg d1_id "$expected_d1_id" \
  --arg r2_bucket "$expected_r2_bucket" \
  --arg r2_account "$expected_r2_account" \
  --arg r2_access_key "$expected_r2_access_key" \
  --arg credential_ttl "$expected_credential_ttl" \
  --arg upload_ttl "$expected_upload_ttl" '
  .success == true and
  .result.deployment_configs.production.fail_open == false and
  .result.deployment_configs.production.limits.cpu_ms == 300000 and
  .result.deployment_configs.production.d1_databases.DB.id == $d1_id and
  .result.deployment_configs.production.r2_buckets.ARCHIVE.name == $r2_bucket and
  .result.deployment_configs.production.env_vars.R2_ACCOUNT_ID == {
    type: "plain_text", value: $r2_account
  } and
  .result.deployment_configs.production.env_vars.R2_PARENT_ACCESS_KEY_ID == {
    type: "plain_text", value: $r2_access_key
  } and
  .result.deployment_configs.production.env_vars.R2_BUCKET_NAME == {
    type: "plain_text", value: $r2_bucket
  } and
  .result.deployment_configs.production.env_vars.CREDENTIAL_TTL_SECONDS == {
    type: "plain_text", value: $credential_ttl
  } and
  .result.deployment_configs.production.env_vars.UPLOAD_TTL_SECONDS == {
    type: "plain_text", value: $upload_ttl
  } and
  .result.deployment_configs.production.env_vars.ADMIN_API_TOKEN.type == "secret_text" and
  .result.deployment_configs.production.env_vars.PROCESSOR_API_TOKEN.type == "secret_text" and
  .result.deployment_configs.production.env_vars.SITE_KEY_ENCRYPTION_KEY_B64.type == "secret_text" and
  .result.deployment_configs.production.env_vars.R2_PARENT_SECRET_ACCESS_KEY.type == "secret_text" and
  .result.deployment_configs.preview.fail_open == false and
  ((.result.deployment_configs.preview.env_vars // {}) | length) == 0 and
  ((.result.deployment_configs.preview.d1_databases // {}) | length) == 0 and
  ((.result.deployment_configs.preview.r2_buckets // {}) | length) == 0
' <<<"$project" >/dev/null; then
  echo "Production Pages bindings or variables do not match $wrangler_config" >&2
  exit 1
fi

echo "Verified exact production D1/R2/variable bindings, 300 s CPU budget, and isolated previews."
