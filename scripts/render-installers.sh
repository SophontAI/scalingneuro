#!/usr/bin/env bash
set -euo pipefail

if (( $# != 9 )); then
  echo "usage: $0 OUTPUT_DIR VERSION DOWNLOAD_BASE MACOS_PACKAGE MACOS_SHA256 LINUX_PACKAGE LINUX_SHA256 WINDOWS_PACKAGE WINDOWS_SHA256" >&2
  exit 2
fi

ROOT_DIR=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)
OUTPUT_DIR=$1
VERSION=$2
DOWNLOAD_BASE=${3%/}
MACOS_PACKAGE=$4
MACOS_SHA256=$5
LINUX_PACKAGE=$6
LINUX_SHA256=$7
WINDOWS_PACKAGE=$8
WINDOWS_SHA256=$9

[[ "$VERSION" =~ ^[0-9]+\.[0-9]+\.[0-9]+([+-][A-Za-z0-9.-]+)?$ ]] || { echo "invalid version" >&2; exit 1; }
[[ "$DOWNLOAD_BASE" =~ ^(https|file)://[^\|[:space:]]+$ ]] || { echo "invalid download base" >&2; exit 1; }
for name in "$MACOS_PACKAGE" "$LINUX_PACKAGE" "$WINDOWS_PACKAGE"; do
  [[ "$name" =~ ^[A-Za-z0-9][A-Za-z0-9._+-]*$ ]] || { echo "invalid package name: $name" >&2; exit 1; }
done
for digest in "$MACOS_SHA256" "$LINUX_SHA256" "$WINDOWS_SHA256"; do
  [[ "$digest" =~ ^[0-9a-f]{64}$ ]] || { echo "invalid SHA-256: $digest" >&2; exit 1; }
done

escape_sed() {
  sed 's/[&|\\]/\\&/g' <<< "$1"
}

mkdir -p "$OUTPUT_DIR"
version=$(escape_sed "$VERSION")
base=$(escape_sed "$DOWNLOAD_BASE")
macos_package=$(escape_sed "$MACOS_PACKAGE")
linux_package=$(escape_sed "$LINUX_PACKAGE")
windows_package=$(escape_sed "$WINDOWS_PACKAGE")

sed \
  -e "s|@VERSION@|$version|g" \
  -e "s|@DOWNLOAD_BASE@|$base|g" \
  -e "s|@MACOS_PACKAGE@|$macos_package|g" \
  -e "s|@MACOS_SHA256@|$MACOS_SHA256|g" \
  -e "s|@LINUX_PACKAGE@|$linux_package|g" \
  -e "s|@LINUX_SHA256@|$LINUX_SHA256|g" \
  "$ROOT_DIR/installers/install.sh.in" > "$OUTPUT_DIR/install.sh"

sed \
  -e "s|@VERSION@|$version|g" \
  -e "s|@DOWNLOAD_BASE@|$base|g" \
  -e "s|@WINDOWS_PACKAGE@|$windows_package|g" \
  -e "s|@WINDOWS_SHA256@|$WINDOWS_SHA256|g" \
  "$ROOT_DIR/installers/install.ps1.in" > "$OUTPUT_DIR/install.ps1"

chmod 0755 "$OUTPUT_DIR/install.sh"
chmod 0644 "$OUTPUT_DIR/install.ps1"

if grep -En '@[A-Z0-9_]+@' "$OUTPUT_DIR/install.sh" "$OUTPUT_DIR/install.ps1"; then
  echo "unrendered installer placeholder" >&2
  exit 1
fi
