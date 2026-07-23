#!/usr/bin/env bash
set -euo pipefail

: "${CLOUDFLARE_API_TOKEN:?CLOUDFLARE_API_TOKEN is required}"
: "${CLOUDFLARE_ACCOUNT_ID:?CLOUDFLARE_ACCOUNT_ID is required}"

PROJECT_NAME=${SCALING_NEURO_PAGES_PROJECT:-scalingneuro}
repository_root=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)
wrangler_config=${SCALING_NEURO_WRANGLER_CONFIG:-"$repository_root/worker/wrangler.jsonc"}
endpoint="https://api.cloudflare.com/client/v4/accounts/${CLOUDFLARE_ACCOUNT_ID}/pages/projects/${PROJECT_NAME}"
authorization="Authorization: Bearer ${CLOUDFLARE_API_TOKEN}"
required_secrets='[
  "R2_PARENT_SECRET_ACCESS_KEY",
  "SITE_KEY_ENCRYPTION_KEY_B64"
]'

jq --exit-status '
  .compatibility_date == "2026-07-23" and
  .compatibility_flags == ["nodejs_compat"] and
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

d1_id=$(jq --raw-output '.d1_databases[] | select(.binding == "DB") | .database_id' "$wrangler_config")
r2_bucket=$(jq --raw-output '.r2_buckets[] | select(.binding == "ARCHIVE") | .bucket_name' "$wrangler_config")
r2_account=$(jq --raw-output '.vars.R2_ACCOUNT_ID' "$wrangler_config")
r2_access_key=$(jq --raw-output '.vars.R2_PARENT_ACCESS_KEY_ID' "$wrangler_config")
r2_bucket_var=$(jq --raw-output '.vars.R2_BUCKET_NAME' "$wrangler_config")
credential_ttl=$(jq --raw-output '.vars.CREDENTIAL_TTL_SECONDS' "$wrangler_config")
upload_ttl=$(jq --raw-output '.vars.UPLOAD_TTL_SECONDS' "$wrangler_config")
compatibility_date=$(jq --raw-output '.compatibility_date' "$wrangler_config")
compatibility_flags=$(jq --compact-output '.compatibility_flags' "$wrangler_config")

if [[ "$r2_account" != "$CLOUDFLARE_ACCOUNT_ID" ]]; then
  echo "R2_ACCOUNT_ID does not match CLOUDFLARE_ACCOUNT_ID" >&2
  exit 1
fi
if [[ "$r2_bucket" != "$r2_bucket_var" ]]; then
  echo "R2 binding and R2_BUCKET_NAME disagree" >&2
  exit 1
fi

get_project() {
  curl --fail --silent --show-error --header "$authorization" "$endpoint"
}

current=$(get_project)
jq --exit-status '.success == true and (.result | type == "object")' \
  <<<"$current" >/dev/null
jq --exit-status --argjson required "$required_secrets" '
  (.result.deployment_configs.production.env_vars // {}) as $vars |
  $required | all(. as $name | $vars[$name].type == "secret_text")
' <<<"$current" >/dev/null || {
  echo "Required production secrets are missing or not secret_text" >&2
  exit 1
}

production_hash=$(jq --raw-output '.result.deployment_configs.production.wrangler_config_hash // empty' <<<"$current")
preview_hash=$(jq --raw-output '.result.deployment_configs.preview.wrangler_config_hash // empty' <<<"$current")

payload=$(jq --compact-output \
  --arg d1_id "$d1_id" \
  --arg r2_bucket "$r2_bucket" \
  --arg r2_account "$r2_account" \
  --arg r2_access_key "$r2_access_key" \
  --arg credential_ttl "$credential_ttl" \
  --arg upload_ttl "$upload_ttl" \
  --arg compatibility_date "$compatibility_date" \
  --argjson compatibility_flags "$compatibility_flags" \
  --argjson required "$required_secrets" '
  def null_all($object):
    (($object // {}) | with_entries(.value = null));
  def null_except($object; $excluded):
    (($object // {}) | to_entries
      | map(select(.key as $name | ($excluded | index($name)) == null)
        | {key: .key, value: null})
      | from_entries);
  def keep_hash($environment):
    ($environment.wrangler_config_hash // "") as $hash |
    if ($hash | length) > 0 then {wrangler_config_hash: $hash} else {} end;
  .result.deployment_configs as $configs |
  ($configs.production // {}) as $production |
  ($configs.preview // {}) as $preview |
  {
    deployment_configs: {
      production: ({
        fail_open: false,
        compatibility_date: $compatibility_date,
        compatibility_flags: $compatibility_flags,
        limits: {cpu_ms: 300000},
        d1_databases: (
          null_all($production.d1_databases) + {DB: {id: $d1_id}}
        ),
        r2_buckets: (
          null_all($production.r2_buckets) + {ARCHIVE: {name: $r2_bucket}}
        ),
        env_vars: (
          null_except($production.env_vars; $required) + {
            R2_ACCOUNT_ID: {type: "plain_text", value: $r2_account},
            R2_PARENT_ACCESS_KEY_ID: {
              type: "plain_text", value: $r2_access_key
            },
            R2_BUCKET_NAME: {type: "plain_text", value: $r2_bucket},
            CREDENTIAL_TTL_SECONDS: {
              type: "plain_text", value: $credential_ttl
            },
            UPLOAD_TTL_SECONDS: {type: "plain_text", value: $upload_ttl}
          }
        )
      } + keep_hash($production)),
      preview: ({
        fail_open: false,
        compatibility_date: $compatibility_date,
        compatibility_flags: $compatibility_flags,
        env_vars: null_all($preview.env_vars),
        d1_databases: null_all($preview.d1_databases),
        r2_buckets: null_all($preview.r2_buckets)
      } + keep_hash($preview))
    }
  }
' <<<"$current")

response=$(curl --fail --silent --show-error --request PATCH \
  --header "$authorization" \
  --header 'content-type: application/json' \
  --data "$payload" \
  "$endpoint")
jq --exit-status '.success == true' <<<"$response" >/dev/null

verified=$(get_project)
jq --exit-status \
  --arg d1_id "$d1_id" \
  --arg r2_bucket "$r2_bucket" \
  --arg r2_account "$r2_account" \
  --arg r2_access_key "$r2_access_key" \
  --arg credential_ttl "$credential_ttl" \
  --arg upload_ttl "$upload_ttl" \
  --arg compatibility_date "$compatibility_date" \
  --argjson compatibility_flags "$compatibility_flags" \
  --arg production_hash "$production_hash" \
  --arg preview_hash "$preview_hash" '
  .success == true and
  .result.deployment_configs.production as $production |
  .result.deployment_configs.preview as $preview |
  $production.fail_open == false and
  $production.limits.cpu_ms == 300000 and
  ($production.d1_databases | keys) == ["DB"] and
  $production.d1_databases.DB.id == $d1_id and
  ($production.r2_buckets | keys) == ["ARCHIVE"] and
  $production.r2_buckets.ARCHIVE.name == $r2_bucket and
  ($production.env_vars | keys | sort) == [
    "CREDENTIAL_TTL_SECONDS",
    "R2_ACCOUNT_ID",
    "R2_BUCKET_NAME",
    "R2_PARENT_ACCESS_KEY_ID",
    "R2_PARENT_SECRET_ACCESS_KEY",
    "SITE_KEY_ENCRYPTION_KEY_B64",
    "UPLOAD_TTL_SECONDS"
  ] and
  $production.env_vars.R2_ACCOUNT_ID == {
    type: "plain_text", value: $r2_account
  } and
  $production.env_vars.R2_PARENT_ACCESS_KEY_ID == {
    type: "plain_text", value: $r2_access_key
  } and
  $production.env_vars.R2_BUCKET_NAME == {
    type: "plain_text", value: $r2_bucket
  } and
  $production.env_vars.CREDENTIAL_TTL_SECONDS == {
    type: "plain_text", value: $credential_ttl
  } and
  $production.env_vars.UPLOAD_TTL_SECONDS == {
    type: "plain_text", value: $upload_ttl
  } and
  $production.env_vars.R2_PARENT_SECRET_ACCESS_KEY.type == "secret_text" and
  $production.env_vars.SITE_KEY_ENCRYPTION_KEY_B64.type == "secret_text" and
  $preview.fail_open == false and
  $production.compatibility_date == $compatibility_date and
  $production.compatibility_flags == $compatibility_flags and
  $preview.compatibility_date == $compatibility_date and
  $preview.compatibility_flags == $compatibility_flags and
  ($preview.env_vars | length) == 0 and
  ($preview.d1_databases | length) == 0 and
  ($preview.r2_buckets | length) == 0 and
  ($production_hash == "" or
    $production.wrangler_config_hash == $production_hash) and
  ($preview_hash == "" or $preview.wrangler_config_hash == $preview_hash)
' <<<"$verified" >/dev/null

echo "Reconciled the production D1, R2, and minimal EPI archive secrets."
