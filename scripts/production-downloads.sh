#!/usr/bin/env bash
set -euo pipefail

usage() {
  cat >&2 <<'EOF'
Usage: production-downloads.sh capture <site-dist> [origin]
       production-downloads.sh verify  <site-dist> [origin]

Capture copies the exact currently published, checksum-verified release files
into a freshly built Pages directory. Verify proves that the canonical origin
still serves those exact files from both /downloads and the installer routes.
EOF
  exit 2
}

[[ $# -ge 2 && $# -le 3 ]] || usage

MODE=$1
DIST_DIR=$2
ORIGIN=${3:-https://scalingneuro.com}
ORIGIN=${ORIGIN%/}
DOWNLOADS_DIR=$DIST_DIR/downloads

case "$MODE" in
  capture|verify) ;;
  *) usage ;;
esac

command -v curl >/dev/null
command -v jq >/dev/null
mkdir -p "$DOWNLOADS_DIR"

WORK_DIR=$(mktemp -d)
trap 'rm -rf "$WORK_DIR"' EXIT

hash_file() {
  if command -v sha256sum >/dev/null 2>&1; then
    sha256sum "$1" | awk '{print $1}'
  else
    shasum -a 256 "$1" | awk '{print $1}'
  fi
}

fetch() {
  local url=$1
  local output=$2
  curl --fail --location --silent --show-error \
    --connect-timeout 15 --max-time 300 \
    --retry 5 --retry-all-errors \
    --output "$output" "$url"
}

safe_release_name() {
  [[ "$1" =~ ^[A-Za-z0-9][A-Za-z0-9._+-]*$ ]]
}

normalize_sums() {
  local sums=$1
  local output=$2
  awk '
    NF != 2 ||
    $1 !~ /^[0-9a-f]{64}$/ ||
    $2 !~ /^[A-Za-z0-9][A-Za-z0-9._+-]*$/ { exit 1 }
    { print $1 "\t" $2 }
  ' "$sums" > "$output"
  [[ -s "$output" ]]

  local duplicates
  duplicates=$(cut -f2 "$output" | LC_ALL=C sort | uniq -d)
  if [[ -n "$duplicates" ]]; then
    echo "Duplicate files in production SHA256SUMS:" >&2
    echo "$duplicates" >&2
    return 1
  fi
}

write_expected_inventory() {
  local latest=$1
  local output=$2

  jq --exit-status '
    .schema_version == "1.0.0" and
    (.version | type == "string" and test("^[0-9]+\\.[0-9]+\\.[0-9]+([+-][A-Za-z0-9.-]+)?$")) and
    (.channel == "pilot" or .channel == "open-beta") and
    .checksums_url == "/downloads/SHA256SUMS" and
    (.artifacts | type == "object" and length > 0) and
    (all(.artifacts[];
      (.url | type == "string" and test("^/downloads/[A-Za-z0-9][A-Za-z0-9._+-]*$")) and
      (.sha256 | type == "string" and test("^[0-9a-f]{64}$")) and
      (.sbom_spdx_url | type == "string" and test("^/downloads/[A-Za-z0-9][A-Za-z0-9._+-]*\\.spdx\\.json$")) and
      (.sbom_cyclonedx_url | type == "string" and test("^/downloads/[A-Za-z0-9][A-Za-z0-9._+-]*\\.cdx\\.json$"))
    )) and
    ((.installers // {}) | type == "object") and
    (all((.installers // {})[];
      (.url == "/install.sh" or .url == "/install.ps1") and
      (.sha256 | type == "string" and test("^[0-9a-f]{64}$"))
    ))
  ' "$latest" >/dev/null

  {
    printf '%s\n' latest.json
    jq -r '
      [
        .artifacts[] |
        .url, .sbom_spdx_url, .sbom_cyclonedx_url
      ] + [
        (.installers // {})[] | .url
      ] | .[] | split("/")[-1]
    ' "$latest"
  } | LC_ALL=C sort -u > "$output"

  while IFS= read -r name; do
    safe_release_name "$name" || {
      echo "Unsafe release filename in latest.json: $name" >&2
      return 1
    }
  done < "$output"
}

validate_bundle() {
  local release_dir=$1
  local normalized=$WORK_DIR/normalized-sums
  local expected=$WORK_DIR/expected-inventory
  local actual=$WORK_DIR/actual-inventory
  local artifact_digests=$WORK_DIR/artifact-digests
  local installer_digests=$WORK_DIR/installer-digests

  [[ -f "$release_dir/latest.json" && -f "$release_dir/SHA256SUMS" ]]
  normalize_sums "$release_dir/SHA256SUMS" "$normalized"
  write_expected_inventory "$release_dir/latest.json" "$expected"
  cut -f2 "$normalized" | LC_ALL=C sort > "$actual"
  if ! diff -u "$expected" "$actual"; then
    echo "Production release index and checksum inventory disagree" >&2
    return 1
  fi

  while IFS=$'\t' read -r expected_sha name; do
    [[ -f "$release_dir/$name" && ! -L "$release_dir/$name" ]] || {
      echo "Missing regular production release file: $name" >&2
      return 1
    }
    local actual_sha
    actual_sha=$(hash_file "$release_dir/$name")
    if [[ "$actual_sha" != "$expected_sha" ]]; then
      echo "Production release checksum mismatch: $name" >&2
      return 1
    fi
  done < "$normalized"

  jq -r '.artifacts[] | [(.url | split("/")[-1]), .sha256] | @tsv' \
    "$release_dir/latest.json" > "$artifact_digests"
  while IFS=$'\t' read -r name expected_sha; do
    local checksummed_sha
    checksummed_sha=$(awk -F '\t' -v name="$name" '$2 == name { print $1 }' "$normalized")
    if [[ "$checksummed_sha" != "$expected_sha" ]]; then
      echo "Production artifact digest disagrees with SHA256SUMS: $name" >&2
      return 1
    fi
  done < "$artifact_digests"

  jq -r '(.installers // {})[] | [(.url | split("/")[-1]), .sha256] | @tsv' \
    "$release_dir/latest.json" > "$installer_digests"
  while IFS=$'\t' read -r name expected_sha; do
    local checksummed_sha
    checksummed_sha=$(awk -F '\t' -v name="$name" '$2 == name { print $1 }' "$normalized")
    if [[ "$checksummed_sha" != "$expected_sha" ]]; then
      echo "Production installer digest disagrees with SHA256SUMS: $name" >&2
      return 1
    fi
  done < "$installer_digests"
}

capture() {
  local staged=$WORK_DIR/captured
  local normalized=$WORK_DIR/capture-sums
  mkdir -p "$staged"

  fetch "$ORIGIN/downloads/latest.json" "$staged/latest.json"
  fetch "$ORIGIN/downloads/SHA256SUMS" "$staged/SHA256SUMS"
  normalize_sums "$staged/SHA256SUMS" "$normalized"

  while IFS=$'\t' read -r _ name; do
    [[ "$name" == latest.json ]] && continue
    fetch "$ORIGIN/downloads/$name" "$staged/$name"
  done < "$normalized"

  validate_bundle "$staged"
  while IFS= read -r url; do
    local name=${url##*/}
    fetch "$ORIGIN$url" "$WORK_DIR/current-root-$name"
    if ! cmp "$staged/$name" "$WORK_DIR/current-root-$name"; then
      echo "Canonical installer and checksummed download differ: $url" >&2
      return 1
    fi
  done < <(jq -r '(.installers // {})[].url' "$staged/latest.json")

  while IFS= read -r file; do
    install -m 0644 "$file" "$DOWNLOADS_DIR/$(basename "$file")"
  done < <(find "$staged" -mindepth 1 -maxdepth 1 -type f | LC_ALL=C sort)

  while IFS= read -r url; do
    local name=${url##*/}
    if [[ "$name" == install.sh ]]; then
      install -m 0755 "$staged/$name" "$DIST_DIR/$name"
    else
      install -m 0644 "$staged/$name" "$DIST_DIR/$name"
    fi
  done < <(jq -r '(.installers // {})[].url' "$staged/latest.json")
}

verify() {
  validate_bundle "$DOWNLOADS_DIR"

  local remote=$WORK_DIR/remote
  local normalized=$WORK_DIR/verify-sums
  mkdir -p "$remote"
  fetch "$ORIGIN/downloads/SHA256SUMS" "$remote/SHA256SUMS"
  cmp "$DOWNLOADS_DIR/SHA256SUMS" "$remote/SHA256SUMS"
  normalize_sums "$DOWNLOADS_DIR/SHA256SUMS" "$normalized"

  while IFS=$'\t' read -r expected_sha name; do
    fetch "$ORIGIN/downloads/$name" "$remote/$name"
    local actual_sha
    actual_sha=$(hash_file "$remote/$name")
    if [[ "$actual_sha" != "$expected_sha" ]]; then
      echo "Published production release checksum mismatch: $name" >&2
      return 1
    fi
  done < "$normalized"

  while IFS= read -r url; do
    local name=${url##*/}
    fetch "$ORIGIN$url" "$remote/root-$name"
    cmp "$DIST_DIR/$name" "$remote/root-$name"
  done < <(jq -r '(.installers // {})[].url' "$DOWNLOADS_DIR/latest.json")
}

"$MODE"
