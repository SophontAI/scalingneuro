#!/usr/bin/env bash
set -euo pipefail

usage() {
  echo "Usage: retry-release-candidate.sh <registration|policy-upgrade|upload> <command> [args...]" >&2
  exit 2
}

[[ $# -ge 2 ]] || usage

operation=$1
shift
case "$operation" in
  registration|policy-upgrade|upload) ;;
  *) usage ;;
esac

attempt_limit=${SCALING_NEURO_RELEASE_RETRY_ATTEMPTS:-24}
retry_delay=${SCALING_NEURO_RELEASE_RETRY_DELAY_SECONDS:-5}
[[ "$attempt_limit" =~ ^[1-9][0-9]*$ ]] || {
  echo "SCALING_NEURO_RELEASE_RETRY_ATTEMPTS must be a positive integer" >&2
  exit 2
}
[[ "$retry_delay" =~ ^[0-9]+$ ]] || {
  echo "SCALING_NEURO_RELEASE_RETRY_DELAY_SECONDS must be a non-negative integer" >&2
  exit 2
}

stale_policy_get_error='--accept-policy-version names open-mri-1.0.0, but the server requires open-epi-1.0.0; review the current policy and pass that exact version'
stale_policy_post_error='ingest API request failed (CONSENT_POLICY_UPDATE_REQUIRED): Review and accept the current public contribution policy'
stale_device_policy_route_error='ingest API request failed (NOT_FOUND): Route was not found'
stale_upload_contract_error='ingest API request failed (INVALID_REQUEST): series[0] contains unknown field: series_kind'

is_stale_generation_error() {
  local output=$1
  [[ "$output" == *"$stale_policy_get_error"* ]] ||
    [[ "$output" == *"$stale_policy_post_error"* ]] ||
    [[ "$operation" == policy-upgrade &&
      "$output" == *"$stale_device_policy_route_error"* ]] ||
    [[ "$operation" == upload &&
      ("$output" == *"$stale_upload_contract_error"* ||
        "$output" == *"$stale_device_policy_route_error"*) ]]
}

for attempt in $(seq 1 "$attempt_limit"); do
  if output=$("$@" 2>&1); then
    printf '%s\n' "$output"
    exit 0
  else
    status=$?
  fi

  if ! is_stale_generation_error "$output"; then
    printf '%s\n' "$output" >&2
    exit "$status"
  fi
  if (( attempt == attempt_limit )); then
    printf '%s\n' "$output" >&2
    exit "$status"
  fi

  printf 'Candidate API generation is still propagating (attempt %s/%s); retrying the same idempotent operation.\n' \
    "$attempt" "$attempt_limit" >&2
  sleep "$retry_delay"
done
