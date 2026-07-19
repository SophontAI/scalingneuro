#!/usr/bin/env bash
set -euo pipefail

ROOT_DIR=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)
WORK_DIR=$(mktemp -d)
trap 'rm -rf "$WORK_DIR"' EXIT

ORIGIN_DIR=$WORK_DIR/origin
SOURCE_DIR=$ORIGIN_DIR/downloads
DIST_DIR=$WORK_DIR/dist
mkdir -p "$SOURCE_DIR" "$DIST_DIR/downloads"
printf '<!doctype html><title>Downloads</title>\n' > "$DIST_DIR/downloads/index.html"

hash_file() {
  if command -v sha256sum >/dev/null 2>&1; then
    sha256sum "$1" | awk '{print $1}'
  else
    shasum -a 256 "$1" | awk '{print $1}'
  fi
}

printf '#!/bin/sh\necho installer\n' > "$SOURCE_DIR/install.sh"
printf 'Write-Output installer\n' > "$SOURCE_DIR/install.ps1"
for name in \
  neuro-sync-v9.8.7-linux-x86_64-musl-static.tar.gz \
  neuro-sync-v9.8.7-macos-universal.zip \
  neuro-sync-v9.8.7-windows-x86_64.zip; do
  printf 'package:%s\n' "$name" > "$SOURCE_DIR/$name"
  printf 'spdx:%s\n' "$name" > "$SOURCE_DIR/${name%.*}.spdx.json"
  printf 'cyclonedx:%s\n' "$name" > "$SOURCE_DIR/${name%.*}.cdx.json"
done

linux_name=neuro-sync-v9.8.7-linux-x86_64-musl-static.tar.gz
macos_name=neuro-sync-v9.8.7-macos-universal.zip
windows_name=neuro-sync-v9.8.7-windows-x86_64.zip

jq -n \
  --arg linux_sha "$(hash_file "$SOURCE_DIR/$linux_name")" \
  --arg macos_sha "$(hash_file "$SOURCE_DIR/$macos_name")" \
  --arg windows_sha "$(hash_file "$SOURCE_DIR/$windows_name")" \
  --arg unix_sha "$(hash_file "$SOURCE_DIR/install.sh")" \
  --arg ps_sha "$(hash_file "$SOURCE_DIR/install.ps1")" \
  '{
    schema_version:"1.0.0", version:"9.8.7", channel:"open-beta",
    checksums_url:"/downloads/SHA256SUMS",
    artifacts:{
      linux:{url:"/downloads/neuro-sync-v9.8.7-linux-x86_64-musl-static.tar.gz",sha256:$linux_sha,sbom_spdx_url:"/downloads/neuro-sync-v9.8.7-linux-x86_64-musl-static.tar.spdx.json",sbom_cyclonedx_url:"/downloads/neuro-sync-v9.8.7-linux-x86_64-musl-static.tar.cdx.json"},
      macos:{url:"/downloads/neuro-sync-v9.8.7-macos-universal.zip",sha256:$macos_sha,sbom_spdx_url:"/downloads/neuro-sync-v9.8.7-macos-universal.spdx.json",sbom_cyclonedx_url:"/downloads/neuro-sync-v9.8.7-macos-universal.cdx.json"},
      windows:{url:"/downloads/neuro-sync-v9.8.7-windows-x86_64.zip",sha256:$windows_sha,sbom_spdx_url:"/downloads/neuro-sync-v9.8.7-windows-x86_64.spdx.json",sbom_cyclonedx_url:"/downloads/neuro-sync-v9.8.7-windows-x86_64.cdx.json"}
    },
    installers:{
      unix:{url:"/install.sh",sha256:$unix_sha},
      windows:{url:"/install.ps1",sha256:$ps_sha}
    }
  }' > "$SOURCE_DIR/latest.json"

(
  cd "$SOURCE_DIR"
  sums=$(mktemp)
  for file in *; do
    printf '%s  %s\n' "$(hash_file "$file")" "$file"
  done | LC_ALL=C sort -k2 > "$sums"
  mv "$sums" SHA256SUMS
)
cp "$SOURCE_DIR/install.sh" "$ORIGIN_DIR/install.sh"
cp "$SOURCE_DIR/install.ps1" "$ORIGIN_DIR/install.ps1"

origin="file://$ORIGIN_DIR"
"$ROOT_DIR/scripts/production-downloads.sh" capture "$DIST_DIR" "$origin"
"$ROOT_DIR/scripts/production-downloads.sh" verify "$DIST_DIR" "$origin"
cmp "$SOURCE_DIR/latest.json" "$DIST_DIR/downloads/latest.json"
cmp "$SOURCE_DIR/$linux_name" "$DIST_DIR/downloads/$linux_name"
cmp "$SOURCE_DIR/install.sh" "$DIST_DIR/install.sh"
cmp "$SOURCE_DIR/install.ps1" "$DIST_DIR/install.ps1"

printf 'tampered\n' >> "$SOURCE_DIR/$linux_name"
if "$ROOT_DIR/scripts/production-downloads.sh" verify "$DIST_DIR" "$origin" >/dev/null 2>&1; then
  echo "Remote artifact tampering was not rejected" >&2
  exit 1
fi

cp "$DIST_DIR/downloads/$linux_name" "$SOURCE_DIR/$linux_name"
printf 'root-only tamper\n' >> "$ORIGIN_DIR/install.sh"
if "$ROOT_DIR/scripts/production-downloads.sh" capture "$WORK_DIR/root-mismatch" "$origin" >/dev/null 2>&1; then
  echo "A root installer that differed from its checksummed copy was not rejected" >&2
  exit 1
fi
cp "$SOURCE_DIR/install.sh" "$ORIGIN_DIR/install.sh"

printf '%064d  ../escape\n' 0 >> "$SOURCE_DIR/SHA256SUMS"
if "$ROOT_DIR/scripts/production-downloads.sh" capture "$WORK_DIR/rejected" "$origin" >/dev/null 2>&1; then
  echo "Unsafe checksum inventory was not rejected" >&2
  exit 1
fi

cp "$DIST_DIR/downloads/SHA256SUMS" "$SOURCE_DIR/SHA256SUMS"
cp "$DIST_DIR/downloads/latest.json" "$SOURCE_DIR/latest.json"
latest_tmp=$(mktemp)
jq '.artifacts.linux.sha256 = "0000000000000000000000000000000000000000000000000000000000000000"' \
  "$SOURCE_DIR/latest.json" > "$latest_tmp"
mv "$latest_tmp" "$SOURCE_DIR/latest.json"
latest_sha=$(hash_file "$SOURCE_DIR/latest.json")
sums_tmp=$(mktemp)
awk -v sha="$latest_sha" '
  $2 == "latest.json" { print sha "  " $2; next }
  { print }
' "$SOURCE_DIR/SHA256SUMS" > "$sums_tmp"
mv "$sums_tmp" "$SOURCE_DIR/SHA256SUMS"
if "$ROOT_DIR/scripts/production-downloads.sh" capture "$WORK_DIR/index-mismatch" "$origin" >/dev/null 2>&1; then
  echo "A latest.json digest that disagreed with SHA256SUMS was not rejected" >&2
  exit 1
fi

echo "production download preservation tests passed"
