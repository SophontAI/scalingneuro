#!/usr/bin/env bash
set -euo pipefail

ROOT_DIR=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)
TEST_ROOT=$(mktemp -d "${TMPDIR:-/tmp}/neuro-sync-installer-test.XXXXXX")
trap 'rm -rf "$TEST_ROOT"' EXIT

VERSION=9.8.7
ASSETS="$TEST_ROOT/assets"
STAGE="$TEST_ROOT/stage"
OUTPUT="$TEST_ROOT/rendered"
mkdir -p "$ASSETS" "$STAGE"

make_package() {
  local name=$1
  local kind=$2
  local root="$STAGE/$name"
  mkdir -p "$root/libexec"
  cat > "$root/neuro-sync" <<'EOF'
#!/bin/sh
test "${1:-}" = "--version" || {
  printf 'installer launched neuro-sync unexpectedly\n' >&2
  exit 91
}
test -x "${NEURO_SYNC_DCM2NIIX:-}" || exit 7
printf 'neuro-sync 9.8.7\n'
EOF
  cat > "$root/libexec/dcm2niix" <<'EOF'
#!/bin/sh
printf 'dcm2niix test fixture\n'
EOF
  chmod 0755 "$root/neuro-sync" "$root/libexec/dcm2niix"
  if [[ "$kind" == zip ]]; then
    (cd "$STAGE" && zip -qr "$ASSETS/$name.zip" "$name")
  else
    tar -C "$STAGE" -czf "$ASSETS/$name.tar.gz" "$name"
  fi
}

MACOS_NAME="neuro-sync-v$VERSION-macos-universal-UNSIGNED-PILOT"
LINUX_NAME="neuro-sync-v$VERSION-linux-x86_64"
WINDOWS_NAME="neuro-sync-v$VERSION-windows-x86_64-UNSIGNED-PILOT.zip"
make_package "$MACOS_NAME" zip
make_package "$LINUX_NAME" tar
printf 'unused Windows fixture\n' > "$ASSETS/$WINDOWS_NAME"

digest() {
  if command -v sha256sum >/dev/null 2>&1; then
    sha256sum "$1" | awk '{print $1}'
  else
    shasum -a 256 "$1" | awk '{print $1}'
  fi
}

MACOS_PACKAGE="$MACOS_NAME.zip"
LINUX_PACKAGE="$LINUX_NAME.tar.gz"
"$ROOT_DIR/scripts/render-installers.sh" \
  "$OUTPUT" "$VERSION" "file://$ASSETS" \
  "$MACOS_PACKAGE" "$(digest "$ASSETS/$MACOS_PACKAGE")" \
  "$LINUX_PACKAGE" "$(digest "$ASSETS/$LINUX_PACKAGE")" \
  "$WINDOWS_NAME" "$(digest "$ASSETS/$WINDOWS_NAME")"

grep -F 'Installation complete. Find or copy your DICOM folder path, then run:' "$OUTPUT/install.sh" >/dev/null
if grep -E 'NEURO_SYNC_NO_LAUNCH|Starting terminal setup|"\$bin_dir/neuro-sync" </dev/tty' "$OUTPUT/install.sh" >/dev/null; then
  echo "installer still launches neuro-sync automatically" >&2
  exit 1
fi
if grep -F 'Opening the private local setup' "$OUTPUT/install.sh" >/dev/null; then
  echo "installer still contains the retired browser-first launch copy" >&2
  exit 1
fi

HOME_DIR="$TEST_ROOT/home"
mkdir -p "$HOME_DIR"
install_output=$(HOME="$HOME_DIR" SHELL=/bin/bash PATH=/usr/bin:/bin sh "$OUTPUT/install.sh")
printf '%s\n' "$install_output" | grep -F 'Installation complete. Find or copy your DICOM folder path, then run:' >/dev/null
printf '%s\n' "$install_output" | grep -F "  $HOME_DIR/.local/bin/neuro-sync" >/dev/null

test -x "$HOME_DIR/.local/bin/neuro-sync"
test -x "$HOME_DIR/.local/share/neuro-sync/versions/$VERSION/neuro-sync"
test -x "$HOME_DIR/.local/share/neuro-sync/versions/$VERSION/libexec/dcm2niix"
test "$(HOME="$HOME_DIR" "$HOME_DIR/.local/bin/neuro-sync" --version)" = "neuro-sync $VERSION"

HOME="$HOME_DIR" \
SHELL=/bin/bash \
PATH=/usr/bin:/bin \
sh "$OUTPUT/install.sh"
if [[ "$(uname -s)" == Darwin ]]; then profile="$HOME_DIR/.bash_profile"; else profile="$HOME_DIR/.bashrc"; fi
test "$(grep -Fc 'export PATH="$HOME/.local/bin:$PATH"' "$profile")" = 1

case "$(uname -s)" in
  Darwin) selected_archive="$ASSETS/$MACOS_PACKAGE" ;;
  Linux) selected_archive="$ASSETS/$LINUX_PACKAGE" ;;
  *) exit 0 ;;
esac
printf 'tampered\n' >> "$selected_archive"
BAD_HOME="$TEST_ROOT/bad-home"
mkdir -p "$BAD_HOME"
if HOME="$BAD_HOME" SHELL=/bin/bash PATH=/usr/bin:/bin sh "$OUTPUT/install.sh"; then
  echo "tampered package was accepted" >&2
  exit 1
fi
test ! -e "$BAD_HOME/.local/bin/neuro-sync"

printf 'terminal installer smoke test passed on %s\n' "$(uname -s)"
