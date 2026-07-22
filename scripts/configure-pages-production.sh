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
  "ADMIN_API_TOKEN",
  "CLUSTER_LAUNCH_HMAC_KEY",
  "PROCESSOR_API_TOKEN",
  "SITE_KEY_ENCRYPTION_KEY_B64",
  "R2_PARENT_SECRET_ACCESS_KEY"
]'

# Pages does not consume worker/wrangler.jsonc. Reconcile the Pages project to
# this version-controlled contract without ever reading or rewriting the values
# of its production secrets.
jq --exit-status '
  ([.d1_databases[] | select(.binding == "DB")] | length) == 1 and
  ([.r2_buckets[] | select(.binding == "ARCHIVE")] | length) == 1 and
  (.vars | keys | sort) == [
    "CLUSTER_LAUNCH_URL",
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
expected_cluster_launch_url=$(jq --raw-output '.vars.CLUSTER_LAUNCH_URL' "$wrangler_config")

if [[ "$expected_r2_account" != "$CLOUDFLARE_ACCOUNT_ID" ]]; then
  echo "R2_ACCOUNT_ID in $wrangler_config does not match CLOUDFLARE_ACCOUNT_ID" >&2
  exit 1
fi
if [[ "$expected_r2_bucket" != "$expected_r2_bucket_var" ]]; then
  echo "R2 binding and R2_BUCKET_NAME disagree in $wrangler_config" >&2
  exit 1
fi

sanitized_report() {
  local stage=$1
  local response=$2

  echo "Sanitized Pages configuration report ($stage):" >&2
  if ! jq --exit-status 'type == "object"' <<<"$response" >/dev/null 2>&1; then
    echo '{"api_success":false,"response_type":"invalid_json"}' >&2
    return
  fi

  jq --compact-output '
    def env_summary($environment):
      (($environment.env_vars // {}) | to_entries
        | map({name: .key, type: (if .value == null then "null" else (.value.type // "unknown") end)})
        | sort_by(.name));
    def binding_names($bindings): (($bindings // {}) | keys | sort);
    (.result.deployment_configs.production // {}) as $production |
    (.result.deployment_configs.preview // {}) as $preview |
    {
      api_success: (.success == true),
      production: {
        fail_open: $production.fail_open,
        cpu_ms: $production.limits.cpu_ms,
        env_vars: env_summary($production),
        d1_bindings: binding_names($production.d1_databases),
        r2_bindings: binding_names($production.r2_buckets)
      },
      preview: {
        fail_open: $preview.fail_open,
        cpu_ms: $preview.limits.cpu_ms,
        env_vars: env_summary($preview),
        d1_bindings: binding_names($preview.d1_databases),
        r2_bindings: binding_names($preview.r2_buckets)
      }
    }
  ' <<<"$response" >&2
}

get_project() {
  curl --fail --silent --show-error --header "$authorization" "$endpoint"
}

if ! current_project=$(get_project); then
  echo "Unable to read the current Pages project configuration." >&2
  exit 1
fi
if ! jq --exit-status '.success == true and (.result | type == "object")' \
  <<<"$current_project" >/dev/null 2>&1; then
  sanitized_report "before reconciliation" "$current_project"
  echo "Cloudflare did not return a successful Pages project response." >&2
  exit 1
fi

if ! jq --exit-status --argjson required "$required_secrets" '
  (.result.deployment_configs.production.env_vars // {}) as $env_vars |
  $required | all(. as $name | $env_vars[$name].type == "secret_text")
' <<<"$current_project" >/dev/null; then
  sanitized_report "before reconciliation" "$current_project"
  echo "Required production secrets are missing or are not secret_text; refusing to mutate Pages configuration." >&2
  exit 1
fi

production_hash=$(jq --raw-output '.result.deployment_configs.production.wrangler_config_hash // empty' <<<"$current_project")
preview_hash=$(jq --raw-output '.result.deployment_configs.preview.wrangler_config_hash // empty' <<<"$current_project")

payload=$(jq --compact-output \
  --arg d1_id "$expected_d1_id" \
  --arg r2_bucket "$expected_r2_bucket" \
  --arg r2_account "$expected_r2_account" \
  --arg r2_access_key "$expected_r2_access_key" \
  --arg credential_ttl "$expected_credential_ttl" \
  --arg upload_ttl "$expected_upload_ttl" \
  --arg cluster_launch_url "$expected_cluster_launch_url" \
  --argjson required "$required_secrets" '
  def null_entries_except($object; $excluded):
    (($object // {}) | to_entries
      | map(select(.key as $name | ($excluded | index($name)) == null)
        | {key: .key, value: null})
      | from_entries);
  def null_entries($object):
    (($object // {}) | with_entries(.value = null));
  def preserved_hash($environment):
    ($environment.wrangler_config_hash // "") as $hash |
    if (($hash | type) == "string" and ($hash | length) > 0)
    then {wrangler_config_hash: $hash}
    else {}
    end;
  .result.deployment_configs as $current |
  ($current.production // {}) as $production |
  ($current.preview // {}) as $preview |
  {
    deployment_configs: {
      production: ({
        fail_open: false,
        limits: {cpu_ms: 300000},
        d1_databases: (
          null_entries($production.d1_databases) +
          {DB: {id: $d1_id}}
        ),
        r2_buckets: (
          null_entries($production.r2_buckets) +
          {ARCHIVE: {name: $r2_bucket}}
        ),
        env_vars: (
          null_entries_except($production.env_vars; $required) +
          {
            R2_ACCOUNT_ID: {type: "plain_text", value: $r2_account},
            R2_PARENT_ACCESS_KEY_ID: {type: "plain_text", value: $r2_access_key},
            R2_BUCKET_NAME: {type: "plain_text", value: $r2_bucket},
            CLUSTER_LAUNCH_URL: {type: "plain_text", value: $cluster_launch_url},
            CREDENTIAL_TTL_SECONDS: {type: "plain_text", value: $credential_ttl},
            UPLOAD_TTL_SECONDS: {type: "plain_text", value: $upload_ttl}
          }
        )
      } + preserved_hash($production)),
      preview: ({
        fail_open: false,
        env_vars: null_entries($preview.env_vars),
        d1_databases: null_entries($preview.d1_databases),
        r2_buckets: null_entries($preview.r2_buckets)
      } + preserved_hash($preview))
    }
  }
' <<<"$current_project")

if ! patch_response=$(curl --fail --silent --show-error --request PATCH \
  --header "$authorization" \
  --header 'content-type: application/json' \
  --data "$payload" \
  "$endpoint"); then
  echo "Unable to update the Pages project configuration." >&2
  exit 1
fi
if ! jq --exit-status '.success == true' <<<"$patch_response" >/dev/null 2>&1; then
  sanitized_report "PATCH response" "$patch_response"
  echo "Cloudflare rejected the Pages project reconciliation." >&2
  exit 1
fi

if ! reconciled_project=$(get_project); then
  echo "Unable to verify the reconciled Pages project configuration." >&2
  exit 1
fi
if ! jq --exit-status \
  --arg d1_id "$expected_d1_id" \
  --arg r2_bucket "$expected_r2_bucket" \
  --arg r2_account "$expected_r2_account" \
  --arg r2_access_key "$expected_r2_access_key" \
  --arg credential_ttl "$expected_credential_ttl" \
  --arg upload_ttl "$expected_upload_ttl" \
  --arg cluster_launch_url "$expected_cluster_launch_url" \
  --arg production_hash "$production_hash" \
  --arg preview_hash "$preview_hash" \
  --argjson required "$required_secrets" '
  .success == true and
  (.result.deployment_configs.production // {}) as $production |
  (.result.deployment_configs.preview // {}) as $preview |
  ($production.env_vars // {}) as $production_env |
  $production.fail_open == false and
  $production.limits.cpu_ms == 300000 and
  (($production.d1_databases // {}) | keys) == ["DB"] and
  $production.d1_databases.DB.id == $d1_id and
  (($production.r2_buckets // {}) | keys) == ["ARCHIVE"] and
  $production.r2_buckets.ARCHIVE.name == $r2_bucket and
  ($production_env | keys | sort) == [
    "ADMIN_API_TOKEN",
    "CLUSTER_LAUNCH_HMAC_KEY",
    "CLUSTER_LAUNCH_URL",
    "CREDENTIAL_TTL_SECONDS",
    "PROCESSOR_API_TOKEN",
    "R2_ACCOUNT_ID",
    "R2_BUCKET_NAME",
    "R2_PARENT_ACCESS_KEY_ID",
    "R2_PARENT_SECRET_ACCESS_KEY",
    "SITE_KEY_ENCRYPTION_KEY_B64",
    "UPLOAD_TTL_SECONDS"
  ] and
  $production_env.R2_ACCOUNT_ID == {type: "plain_text", value: $r2_account} and
  $production_env.R2_PARENT_ACCESS_KEY_ID == {type: "plain_text", value: $r2_access_key} and
  $production_env.R2_BUCKET_NAME == {type: "plain_text", value: $r2_bucket} and
  $production_env.CLUSTER_LAUNCH_URL == {type: "plain_text", value: $cluster_launch_url} and
  $production_env.CREDENTIAL_TTL_SECONDS == {type: "plain_text", value: $credential_ttl} and
  $production_env.UPLOAD_TTL_SECONDS == {type: "plain_text", value: $upload_ttl} and
  ($required | all(. as $name | $production_env[$name].type == "secret_text")) and
  $preview.fail_open == false and
  (($preview.env_vars // {}) | length) == 0 and
  (($preview.d1_databases // {}) | length) == 0 and
  (($preview.r2_buckets // {}) | length) == 0 and
  ($production_hash == "" or $production.wrangler_config_hash == $production_hash) and
  ($preview_hash == "" or $preview.wrangler_config_hash == $preview_hash)
' <<<"$reconciled_project" >/dev/null; then
  sanitized_report "after reconciliation" "$reconciled_project"
  echo "Pages configuration does not match the version-controlled production contract after reconciliation." >&2
  exit 1
fi

echo "Reconciled exact production D1/R2/plaintext bindings, preserved production secrets, set a 300 s CPU budget, and isolated previews."
