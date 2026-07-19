#!/usr/bin/env bash
set -euo pipefail

repository_root=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)
gate="$repository_root/scripts/configure-pages-production.sh"
config="$repository_root/worker/wrangler.jsonc"
tmp=$(mktemp -d)
trap 'rm -rf "$tmp"' EXIT

mkdir -p "$tmp/bin"
cat >"$tmp/bin/curl" <<'MOCK_CURL'
#!/usr/bin/env bash
set -euo pipefail
if [[ " $* " == *" --request PATCH "* ]]; then
  printf '{"success":true}\n'
else
  cat "$MOCK_PROJECT_RESPONSE"
fi
MOCK_CURL
chmod 0755 "$tmp/bin/curl"

d1_id=$(jq --raw-output '.d1_databases[] | select(.binding == "DB") | .database_id' "$config")
r2_bucket=$(jq --raw-output '.r2_buckets[] | select(.binding == "ARCHIVE") | .bucket_name' "$config")
r2_account=$(jq --raw-output '.vars.R2_ACCOUNT_ID' "$config")
r2_access_key=$(jq --raw-output '.vars.R2_PARENT_ACCESS_KEY_ID' "$config")
credential_ttl=$(jq --raw-output '.vars.CREDENTIAL_TTL_SECONDS' "$config")
upload_ttl=$(jq --raw-output '.vars.UPLOAD_TTL_SECONDS' "$config")

jq --null-input \
  --arg d1_id "$d1_id" \
  --arg r2_bucket "$r2_bucket" \
  --arg r2_account "$r2_account" \
  --arg r2_access_key "$r2_access_key" \
  --arg credential_ttl "$credential_ttl" \
  --arg upload_ttl "$upload_ttl" '
  {
    success: true,
    result: {
      deployment_configs: {
        production: {
          fail_open: false,
          limits: {cpu_ms: 300000},
          d1_databases: {DB: {id: $d1_id}},
          r2_buckets: {ARCHIVE: {name: $r2_bucket}},
          env_vars: {
            R2_ACCOUNT_ID: {type: "plain_text", value: $r2_account},
            R2_PARENT_ACCESS_KEY_ID: {type: "plain_text", value: $r2_access_key},
            R2_BUCKET_NAME: {type: "plain_text", value: $r2_bucket},
            CREDENTIAL_TTL_SECONDS: {type: "plain_text", value: $credential_ttl},
            UPLOAD_TTL_SECONDS: {type: "plain_text", value: $upload_ttl},
            ADMIN_API_TOKEN: {type: "secret_text", value: ""},
            PROCESSOR_API_TOKEN: {type: "secret_text", value: ""},
            SITE_KEY_ENCRYPTION_KEY_B64: {type: "secret_text", value: ""},
            R2_PARENT_SECRET_ACCESS_KEY: {type: "secret_text", value: ""}
          }
        },
        preview: {
          fail_open: false,
          env_vars: {},
          d1_databases: {},
          r2_buckets: {}
        }
      }
    }
  }
' >"$tmp/valid.json"

run_gate() {
  MOCK_PROJECT_RESPONSE=$1 \
    PATH="$tmp/bin:$PATH" \
    CLOUDFLARE_API_TOKEN=test-token \
    CLOUDFLARE_ACCOUNT_ID="$r2_account" \
    SCALING_NEURO_WRANGLER_CONFIG="$config" \
    "$gate"
}

run_gate "$tmp/valid.json" >/dev/null

expect_rejection() {
  local name=$1
  local filter=$2
  local response="$tmp/$name.json"
  jq "$filter" "$tmp/valid.json" >"$response"
  if run_gate "$response" >/dev/null 2>&1; then
    echo "production configuration gate accepted $name" >&2
    exit 1
  fi
}

expect_rejection wrong-d1 \
  '.result.deployment_configs.production.d1_databases.DB.id = "wrong"'
expect_rejection wrong-r2 \
  '.result.deployment_configs.production.r2_buckets.ARCHIVE.name = "wrong"'
expect_rejection missing-account \
  'del(.result.deployment_configs.production.env_vars.R2_ACCOUNT_ID)'
expect_rejection wrong-access-key \
  '.result.deployment_configs.production.env_vars.R2_PARENT_ACCESS_KEY_ID.value = "wrong"'
expect_rejection wrong-upload-ttl \
  '.result.deployment_configs.production.env_vars.UPLOAD_TTL_SECONDS.value = "3600"'
expect_rejection plaintext-secret \
  '.result.deployment_configs.production.env_vars.PROCESSOR_API_TOKEN.type = "plain_text"'
expect_rejection preview-binding \
  '.result.deployment_configs.preview.d1_databases.DB = {id: "preview"}'
expect_rejection fail-open \
  '.result.deployment_configs.production.fail_open = true'

echo "production Pages configuration gate tests passed"
