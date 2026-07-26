#!/usr/bin/env bash
set -euo pipefail

usage() {
  cat <<'USAGE'
Usage:
  scripts/archive-access-admin.sh list [pending|approved|rejected|all]
  scripts/archive-access-admin.sh approve REQUEST_ID
  scripts/archive-access-admin.sh reject REQUEST_ID

The admin credential is read from ARCHIVE_ACCESS_ADMIN_TOKEN or from the
macOS Keychain service scalingneuro-archive-access-admin.
USAGE
}

command_name=${1:-}
argument=${2:-}
api_base=${SCALING_NEURO_API_BASE:-https://scalingneuro.org}
admin_token=${ARCHIVE_ACCESS_ADMIN_TOKEN:-}

if [[ -z "$admin_token" ]] && command -v security >/dev/null 2>&1; then
  admin_token=$(security find-generic-password \
    -a "$USER" \
    -s scalingneuro-archive-access-admin \
    -w 2>/dev/null || true)
fi
if [[ -z "$admin_token" ]]; then
  echo "Archive access admin credential was not found." >&2
  echo "Set ARCHIVE_ACCESS_ADMIN_TOKEN or add the macOS Keychain item." >&2
  exit 1
fi

request() {
  curl --fail-with-body --silent --show-error \
    --header "Authorization: Bearer ${admin_token}" \
    "$@"
}

case "$command_name" in
  list)
    status=${argument:-pending}
    case "$status" in
      pending|approved|rejected|all) ;;
      *)
        usage >&2
        exit 2
        ;;
    esac
    response=$(request --get \
      --data-urlencode "status=${status}" \
      "${api_base}/v1/admin/archive-access-requests")
    count=$(jq '.requests | length' <<<"$response")
    if [[ "$count" == "0" ]]; then
      echo "No ${status} archive access requests."
      exit 0
    fi
    jq -r '
      ["REQUEST ID", "STATUS", "SUBMITTED", "NAME", "EMAIL", "INSTITUTION", "LAB"],
      (.requests[] | [
        .request_id,
        .status,
        .submitted_at,
        .contact_name,
        .contact_email,
        .institution_name,
        .lab_name
      ]) | @tsv
    ' <<<"$response"
    ;;
  approve)
    if [[ ! "$argument" =~ ^[0-9a-f]{8}-[0-9a-f]{4}-[1-8][0-9a-f]{3}-[89ab][0-9a-f]{3}-[0-9a-f]{12}$ ]]; then
      usage >&2
      exit 2
    fi
    response=$(request --request POST \
      "${api_base}/v1/admin/archive-access-requests/${argument}/approve")
    jq -r '
      "Approved request \(.request_id).\n\n" +
      "Suggested email\n\n" +
      "Hello \(.contact_name),\n\n" +
      "Your Scaling Neuro archive access request has been approved.\n\n" +
      "Personal access token:\n\(.access_token)\n\n" +
      "Treat this token like a password and do not include it in support requests.\n\n" +
      "List the archive:\n" +
      "export SCALING_NEURO_ACCESS_TOKEN='\''\(.access_token)'\''\n" +
      "curl -H \"Authorization: Bearer $SCALING_NEURO_ACCESS_TOKEN\" " +
      "\(.archive_url)\n\n" +
      "This credential is personal to you and can be revoked if needed."
    ' <<<"$response"
    ;;
  reject)
    if [[ ! "$argument" =~ ^[0-9a-f]{8}-[0-9a-f]{4}-[1-8][0-9a-f]{3}-[89ab][0-9a-f]{3}-[0-9a-f]{12}$ ]]; then
      usage >&2
      exit 2
    fi
    response=$(request --request POST \
      "${api_base}/v1/admin/archive-access-requests/${argument}/reject")
    jq -r '"Rejected request \(.request_id)."' <<<"$response"
    ;;
  *)
    usage >&2
    exit 2
    ;;
esac
