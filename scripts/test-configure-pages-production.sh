#!/usr/bin/env bash
set -euo pipefail

root=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)
gate="$root/scripts/configure-pages-production.sh"
config="$root/worker/wrangler.jsonc"
scratch=$(mktemp -d)
trap 'rm -rf "$scratch"' EXIT

mkdir -p "$scratch/bin"
cat >"$scratch/bin/curl" <<'MOCK_CURL'
#!/usr/bin/env bash
set -euo pipefail
method=GET
data=
while (($# > 0)); do
  case "$1" in
    --request) method=$2; shift 2 ;;
    --data) data=$2; shift 2 ;;
    --header) shift 2 ;;
    --fail|--silent|--show-error) shift ;;
    *) shift ;;
  esac
done
if [[ "$method" == PATCH ]]; then
  printf '%s\n' "$data" >"$MOCK_PATCH_PAYLOAD"
  cat "$MOCK_PATCH_RESPONSE"
else
  count=$(<"$MOCK_GET_COUNT")
  count=$((count + 1))
  printf '%s\n' "$count" >"$MOCK_GET_COUNT"
  if ((count == 1)); then
    cat "$MOCK_BEFORE_RESPONSE"
  else
    cat "$MOCK_AFTER_RESPONSE"
  fi
fi
MOCK_CURL
chmod 0755 "$scratch/bin/curl"

d1_id=$(jq --raw-output '.d1_databases[0].database_id' "$config")
r2_bucket=$(jq --raw-output '.r2_buckets[0].bucket_name' "$config")
r2_account=$(jq --raw-output '.vars.R2_ACCOUNT_ID' "$config")
r2_key=$(jq --raw-output '.vars.R2_PARENT_ACCESS_KEY_ID' "$config")
credential_ttl=$(jq --raw-output '.vars.CREDENTIAL_TTL_SECONDS' "$config")
upload_ttl=$(jq --raw-output '.vars.UPLOAD_TTL_SECONDS' "$config")
compatibility_date=$(jq --raw-output '.compatibility_date' "$config")
compatibility_flags=$(jq --compact-output '.compatibility_flags' "$config")

jq --null-input \
  --arg d1 "$d1_id" \
  --arg bucket "$r2_bucket" \
  --arg account "$r2_account" \
  --arg key "$r2_key" \
  --arg credential_ttl "$credential_ttl" \
  --arg upload_ttl "$upload_ttl" \
  --arg compatibility_date "$compatibility_date" \
  --argjson compatibility_flags "$compatibility_flags" '
  {
    success: true,
    result: {
      deployment_configs: {
        production: {
          fail_open: false,
          compatibility_date: $compatibility_date,
          compatibility_flags: $compatibility_flags,
          limits: {cpu_ms: 300000},
          wrangler_config_hash: "production-hash",
          d1_databases: {DB: {id: $d1}},
          r2_buckets: {ARCHIVE: {name: $bucket}},
          env_vars: {
            R2_ACCOUNT_ID: {type: "plain_text", value: $account},
            R2_PARENT_ACCESS_KEY_ID: {type: "plain_text", value: $key},
            R2_BUCKET_NAME: {type: "plain_text", value: $bucket},
            CREDENTIAL_TTL_SECONDS: {
              type: "plain_text", value: $credential_ttl
            },
            UPLOAD_TTL_SECONDS: {type: "plain_text", value: $upload_ttl},
            R2_PARENT_SECRET_ACCESS_KEY: {
              type: "secret_text", value: "R2_SECRET_SENTINEL"
            },
            SITE_KEY_ENCRYPTION_KEY_B64: {
              type: "secret_text", value: "SITE_SECRET_SENTINEL"
            }
          }
        },
        preview: {
          fail_open: false,
          compatibility_date: $compatibility_date,
          compatibility_flags: $compatibility_flags,
          wrangler_config_hash: "preview-hash",
          env_vars: {},
          d1_databases: {},
          r2_buckets: {}
        }
      }
    }
  }
' >"$scratch/valid.json"

jq '
  .result.deployment_configs.production.fail_open = true |
  .result.deployment_configs.production.d1_databases.LEGACY = {id: "old"} |
  .result.deployment_configs.production.env_vars.LEGACY = {
    type: "plain_text", value: "old"
  } |
  .result.deployment_configs.preview.env_vars.OLD = {
    type: "plain_text", value: "old"
  }
' "$scratch/valid.json" >"$scratch/drift.json"
printf '{"success":true}\n' >"$scratch/patch.json"

run_gate() {
  printf '0\n' >"$scratch/get-count"
  MOCK_BEFORE_RESPONSE="$1" \
    MOCK_AFTER_RESPONSE="$2" \
    MOCK_PATCH_RESPONSE="$3" \
    MOCK_GET_COUNT="$scratch/get-count" \
    MOCK_PATCH_PAYLOAD="$scratch/payload" \
    PATH="$scratch/bin:$PATH" \
    CLOUDFLARE_API_TOKEN=test-token \
    CLOUDFLARE_ACCOUNT_ID="$r2_account" \
    SCALING_NEURO_WRANGLER_CONFIG="$config" \
    "$gate"
}

run_gate "$scratch/drift.json" "$scratch/valid.json" "$scratch/patch.json"
if grep -Eq 'R2_SECRET_SENTINEL|SITE_SECRET_SENTINEL' "$scratch/payload"; then
  echo "Configuration payload leaked a secret" >&2
  exit 1
fi
jq --exit-status '
  .deployment_configs.production as $production |
  .deployment_configs.preview as $preview |
  $production.fail_open == false and
  $production.compatibility_date == "2026-07-23" and
  $production.compatibility_flags == ["nodejs_compat"] and
  $preview.compatibility_date == "2026-07-23" and
  $preview.compatibility_flags == ["nodejs_compat"] and
  $production.d1_databases.LEGACY == null and
  $production.env_vars.LEGACY == null and
  ($production.env_vars | has("R2_PARENT_SECRET_ACCESS_KEY") | not) and
  ($production.env_vars | has("SITE_KEY_ENCRYPTION_KEY_B64") | not) and
  .deployment_configs.preview.env_vars.OLD == null
' "$scratch/payload" >/dev/null

jq 'del(.result.deployment_configs.production.env_vars.SITE_KEY_ENCRYPTION_KEY_B64)' \
  "$scratch/drift.json" >"$scratch/missing.json"
if run_gate "$scratch/missing.json" "$scratch/valid.json" "$scratch/patch.json"; then
  echo "Missing production secret was accepted" >&2
  exit 1
fi

echo "Production Pages configuration gate tests passed."
