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

method=GET
data=
while (($# > 0)); do
  case "$1" in
    --request)
      method=$2
      shift 2
      ;;
    --data)
      data=$2
      shift 2
      ;;
    --header)
      shift 2
      ;;
    --fail|--silent|--show-error)
      shift
      ;;
    *)
      shift
      ;;
  esac
done

if [[ "$method" == PATCH ]]; then
  patch_count=$(cat "$MOCK_PATCH_COUNT")
  printf '%s\n' "$((patch_count + 1))" >"$MOCK_PATCH_COUNT"
  printf '%s\n' "$data" >"$MOCK_PATCH_PAYLOAD"
  cat "$MOCK_PATCH_RESPONSE"
else
  get_count=$(cat "$MOCK_GET_COUNT")
  get_count=$((get_count + 1))
  printf '%s\n' "$get_count" >"$MOCK_GET_COUNT"
  if ((get_count == 1)); then
    cat "$MOCK_BEFORE_RESPONSE"
  else
    cat "$MOCK_AFTER_RESPONSE"
  fi
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
          wrangler_config_hash: "production-hash",
          d1_databases: {DB: {id: $d1_id}},
          r2_buckets: {ARCHIVE: {name: $r2_bucket}},
          env_vars: {
            R2_ACCOUNT_ID: {type: "plain_text", value: $r2_account},
            R2_PARENT_ACCESS_KEY_ID: {type: "plain_text", value: $r2_access_key},
            R2_BUCKET_NAME: {type: "plain_text", value: $r2_bucket},
            CREDENTIAL_TTL_SECONDS: {type: "plain_text", value: $credential_ttl},
            UPLOAD_TTL_SECONDS: {type: "plain_text", value: $upload_ttl},
            ADMIN_API_TOKEN: {type: "secret_text", value: "ADMIN_SECRET_SENTINEL"},
            PROCESSOR_API_TOKEN: {type: "secret_text", value: "PROCESSOR_SECRET_SENTINEL"},
            SITE_KEY_ENCRYPTION_KEY_B64: {type: "secret_text", value: "SITE_SECRET_SENTINEL"},
            R2_PARENT_SECRET_ACCESS_KEY: {type: "secret_text", value: "R2_SECRET_SENTINEL"}
          }
        },
        preview: {
          fail_open: false,
          wrangler_config_hash: "preview-hash",
          env_vars: {},
          d1_databases: {},
          r2_buckets: {}
        }
      }
    }
  }
' >"$tmp/valid.json"

jq '
  .result.deployment_configs.production.fail_open = true |
  .result.deployment_configs.production.limits.cpu_ms = 50 |
  .result.deployment_configs.production.d1_databases = {
    DB: {id: "DRIFT_D1_SENTINEL"},
    LEGACY_DB: {id: "LEGACY_D1_SENTINEL"}
  } |
  .result.deployment_configs.production.r2_buckets = {
    ARCHIVE: {name: "DRIFT_R2_SENTINEL"},
    LEGACY_ARCHIVE: {name: "LEGACY_R2_SENTINEL"}
  } |
  .result.deployment_configs.production.env_vars.R2_ACCOUNT_ID.value = "DRIFT_ACCOUNT_SENTINEL" |
  .result.deployment_configs.production.env_vars.LEGACY = {
    type: "plain_text", value: "LEGACY_VALUE_SENTINEL"
  } |
  .result.deployment_configs.preview = {
    fail_open: true,
    wrangler_config_hash: "preview-hash",
    env_vars: {
      PREVIEW_PLAIN: {type: "plain_text", value: "PREVIEW_VALUE_SENTINEL"},
      PREVIEW_SECRET: {type: "secret_text", value: "PREVIEW_SECRET_SENTINEL"}
    },
    d1_databases: {DB: {id: "PREVIEW_D1_SENTINEL"}},
    r2_buckets: {ARCHIVE: {name: "PREVIEW_R2_SENTINEL"}}
  }
' "$tmp/valid.json" >"$tmp/drift.json"

printf '{"success":true}\n' >"$tmp/patch-success.json"

invoke_gate() {
  local before=$1
  local after=$2
  local patch_response=$3

  printf '0\n' >"$tmp/get-count"
  printf '0\n' >"$tmp/patch-count"
  rm -f "$tmp/patch-payload"
  set +e
  MOCK_BEFORE_RESPONSE="$before" \
    MOCK_AFTER_RESPONSE="$after" \
    MOCK_PATCH_RESPONSE="$patch_response" \
    MOCK_GET_COUNT="$tmp/get-count" \
    MOCK_PATCH_COUNT="$tmp/patch-count" \
    MOCK_PATCH_PAYLOAD="$tmp/patch-payload" \
    PATH="$tmp/bin:$PATH" \
    CLOUDFLARE_API_TOKEN=test-token \
    CLOUDFLARE_ACCOUNT_ID="$r2_account" \
    SCALING_NEURO_WRANGLER_CONFIG="$config" \
    "$gate" >"$tmp/stdout" 2>"$tmp/stderr"
  gate_status=$?
  set -e
}

fail_test() {
  echo "$1" >&2
  if [[ -s "$tmp/stderr" ]]; then
    sed -n '1,120p' "$tmp/stderr" >&2
  fi
  exit 1
}

invoke_gate "$tmp/drift.json" "$tmp/valid.json" "$tmp/patch-success.json"
[[ $gate_status -eq 0 ]] || fail_test "configuration drift was not repaired"
[[ $(cat "$tmp/get-count") == 2 ]] || fail_test "gate did not GET before and after PATCH"
[[ $(cat "$tmp/patch-count") == 1 ]] || fail_test "gate did not issue exactly one PATCH"

jq --exit-status \
  --arg d1_id "$d1_id" \
  --arg r2_bucket "$r2_bucket" \
  --arg r2_account "$r2_account" \
  --arg r2_access_key "$r2_access_key" \
  --arg credential_ttl "$credential_ttl" \
  --arg upload_ttl "$upload_ttl" '
  .deployment_configs.production as $production |
  .deployment_configs.preview as $preview |
  $production.fail_open == false and
  $production.limits.cpu_ms == 300000 and
  $production.wrangler_config_hash == "production-hash" and
  $production.d1_databases.DB == {id: $d1_id} and
  $production.d1_databases.LEGACY_DB == null and
  $production.r2_buckets.ARCHIVE == {name: $r2_bucket} and
  $production.r2_buckets.LEGACY_ARCHIVE == null and
  $production.env_vars.R2_ACCOUNT_ID == {type: "plain_text", value: $r2_account} and
  $production.env_vars.R2_PARENT_ACCESS_KEY_ID == {type: "plain_text", value: $r2_access_key} and
  $production.env_vars.R2_BUCKET_NAME == {type: "plain_text", value: $r2_bucket} and
  $production.env_vars.CREDENTIAL_TTL_SECONDS == {type: "plain_text", value: $credential_ttl} and
  $production.env_vars.UPLOAD_TTL_SECONDS == {type: "plain_text", value: $upload_ttl} and
  $production.env_vars.LEGACY == null and
  ($production.env_vars | has("ADMIN_API_TOKEN") | not) and
  ($production.env_vars | has("PROCESSOR_API_TOKEN") | not) and
  ($production.env_vars | has("SITE_KEY_ENCRYPTION_KEY_B64") | not) and
  ($production.env_vars | has("R2_PARENT_SECRET_ACCESS_KEY") | not) and
  $preview.fail_open == false and
  $preview.wrangler_config_hash == "preview-hash" and
  ($preview.env_vars | keys) == ["PREVIEW_PLAIN", "PREVIEW_SECRET"] and
  ($preview.env_vars | all(. == null)) and
  $preview.d1_databases == {DB: null} and
  $preview.r2_buckets == {ARCHIVE: null}
' "$tmp/patch-payload" >/dev/null || fail_test "PATCH payload did not exactly reconcile configuration drift"

if rg --quiet 'ADMIN_SECRET_SENTINEL|PROCESSOR_SECRET_SENTINEL|SITE_SECRET_SENTINEL|R2_SECRET_SENTINEL|PREVIEW_VALUE_SENTINEL|PREVIEW_SECRET_SENTINEL' "$tmp/patch-payload"; then
  fail_test "PATCH payload leaked a secret or stale preview value"
fi

jq 'del(.result.deployment_configs.production.env_vars.PROCESSOR_API_TOKEN)' \
  "$tmp/drift.json" >"$tmp/missing-secret.json"
invoke_gate "$tmp/missing-secret.json" "$tmp/valid.json" "$tmp/patch-success.json"
[[ $gate_status -ne 0 ]] || fail_test "missing required secret was accepted"
[[ $(cat "$tmp/patch-count") == 0 ]] || fail_test "gate PATCHed after finding a missing secret"

jq '.result.deployment_configs.production.env_vars.PROCESSOR_API_TOKEN.type = "plain_text"' \
  "$tmp/drift.json" >"$tmp/plaintext-secret.json"
invoke_gate "$tmp/plaintext-secret.json" "$tmp/valid.json" "$tmp/patch-success.json"
[[ $gate_status -ne 0 ]] || fail_test "plaintext required secret was accepted"
[[ $(cat "$tmp/patch-count") == 0 ]] || fail_test "gate PATCHed after finding a plaintext secret"

printf '{"success":false,"errors":[{"message":"GET_API_VALUE_SENTINEL"}]}\n' \
  >"$tmp/get-api-failure.json"
invoke_gate "$tmp/get-api-failure.json" "$tmp/valid.json" "$tmp/patch-success.json"
[[ $gate_status -ne 0 ]] || fail_test "unsuccessful initial API response was accepted"
[[ $(cat "$tmp/patch-count") == 0 ]] || fail_test "gate PATCHed after unsuccessful initial API response"
if rg --quiet 'GET_API_VALUE_SENTINEL' "$tmp/stdout" "$tmp/stderr"; then
  fail_test "initial API failure leaked a response value"
fi

printf '{"success":false,"errors":[{"message":"PATCH_API_VALUE_SENTINEL"}]}\n' \
  >"$tmp/patch-api-failure.json"
invoke_gate "$tmp/drift.json" "$tmp/valid.json" "$tmp/patch-api-failure.json"
[[ $gate_status -ne 0 ]] || fail_test "unsuccessful PATCH API response was accepted"
[[ $(cat "$tmp/patch-count") == 1 ]] || fail_test "PATCH API failure test did not issue one PATCH"
[[ $(cat "$tmp/get-count") == 1 ]] || fail_test "gate re-read configuration after unsuccessful PATCH"
if rg --quiet 'PATCH_API_VALUE_SENTINEL' "$tmp/stdout" "$tmp/stderr"; then
  fail_test "PATCH API failure leaked a response value"
fi

jq '
  .result.deployment_configs.production.env_vars.R2_ACCOUNT_ID.value = "POST_VALUE_SENTINEL" |
  .result.deployment_configs.production.env_vars.PROCESSOR_API_TOKEN.value = "POST_SECRET_SENTINEL"
' "$tmp/valid.json" >"$tmp/post-mismatch.json"
invoke_gate "$tmp/drift.json" "$tmp/post-mismatch.json" "$tmp/patch-success.json"
[[ $gate_status -ne 0 ]] || fail_test "post-reconciliation mismatch was accepted"
[[ $(cat "$tmp/patch-count") == 1 ]] || fail_test "post-reconciliation mismatch did not follow PATCH"
if rg --quiet 'POST_VALUE_SENTINEL|POST_SECRET_SENTINEL' "$tmp/stdout" "$tmp/stderr"; then
  fail_test "post-reconciliation mismatch leaked a variable value"
fi
rg --quiet '"name":"PROCESSOR_API_TOKEN","type":"secret_text"' "$tmp/stderr" || \
  fail_test "sanitized report omitted safe variable name/type evidence"

echo "production Pages configuration reconciliation tests passed"
