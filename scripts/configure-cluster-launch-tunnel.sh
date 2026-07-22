#!/usr/bin/env bash
set -euo pipefail

: "${CLOUDFLARE_API_TOKEN:?CLOUDFLARE_API_TOKEN is required}"
: "${CLOUDFLARE_ACCOUNT_ID:?CLOUDFLARE_ACCOUNT_ID is required}"

readonly tunnel_id=${SCALING_NEURO_CLUSTER_TUNNEL_ID:-91bc594a-1c3d-40ae-a4de-6fcdaec10ac6}
readonly hostname=${SCALING_NEURO_CLUSTER_HOSTNAME:-cluster-launch.scalingneuro.com}
readonly zone_name=${SCALING_NEURO_ZONE_NAME:-scalingneuro.com}
readonly api=https://api.cloudflare.com/client/v4
readonly authorization="Authorization: Bearer ${CLOUDFLARE_API_TOKEN}"

api_request() {
  curl --fail --silent --show-error \
    --header "$authorization" \
    --header 'content-type: application/json' \
    "$@"
}

zone_response=$(api_request --get --data-urlencode "name=$zone_name" "$api/zones")
zone_id=$(jq -er '.result | select(length == 1) | .[0].id' <<<"$zone_response")

configuration=$(jq --null-input --compact-output \
  --arg hostname "$hostname" '
  {config:{ingress:[
    {hostname:$hostname,service:"http://127.0.0.1:8788"},
    {service:"http_status:404"}
  ]}}')
configure_response=$(api_request \
  --request PUT \
  --data "$configuration" \
  "$api/accounts/$CLOUDFLARE_ACCOUNT_ID/cfd_tunnel/$tunnel_id/configurations")
jq --exit-status \
  --arg hostname "$hostname" '
  .success == true and
  .result.config.ingress[0].hostname == $hostname and
  .result.config.ingress[0].service == "http://127.0.0.1:8788"
' <<<"$configure_response" >/dev/null

records_response=$(api_request --get --data-urlencode "name=$hostname" "$api/zones/$zone_id/dns_records")
record_count=$(jq -er '.result | length' <<<"$records_response")
target="${tunnel_id}.cfargotunnel.com"
record_payload=$(jq --null-input --compact-output \
  --arg name "$hostname" \
  --arg content "$target" '
  {type:"CNAME",name:$name,content:$content,proxied:true,ttl:1}')

case "$record_count" in
  0)
    record_response=$(api_request --request POST --data "$record_payload" "$api/zones/$zone_id/dns_records")
    ;;
  1)
    record_id=$(jq -er '.result[0].id' <<<"$records_response")
    record_response=$(api_request --request PUT --data "$record_payload" "$api/zones/$zone_id/dns_records/$record_id")
    ;;
  *)
    echo "Expected at most one DNS record for $hostname" >&2
    exit 1
    ;;
esac

jq --exit-status \
  --arg name "$hostname" \
  --arg content "$target" '
  .success == true and .result.type == "CNAME" and
  .result.name == $name and .result.content == $content and
  .result.proxied == true
' <<<"$record_response" >/dev/null

echo "Cluster launch tunnel route is configured."
