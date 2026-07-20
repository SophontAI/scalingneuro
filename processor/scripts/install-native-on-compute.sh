#!/bin/sh
set -eu

umask 077
export PYTHONNOUSERSITE=1
export PYTHONDONTWRITEBYTECODE=1
export PYTHONSAFEPATH=1

PROCESSOR_VERSION=0.2.0
DCM2NIIX_VERSION=1.0.20260416
DCM2NIIX_SHA256=e88b40f6ebbcf9f47ebfdd7bb5f0127297cb7e8b06266a91a4642b5814031bd0
DCM2NIIX_URL=https://github.com/rordenlab/dcm2niix/releases/download/v1.0.20260416/dcm2niix_lnx.zip
ZSTD_VERSION=1.5.5
ZSTD_SHA256=7c5468b370f7c47eda07281e3437fafc568f95d10420051e3aa522709f9342c5
ZSTD_SYSTEM_BIN=/usr/bin/zstd

if [ "$#" -lt 1 ] || [ "$#" -gt 3 ]; then
    echo "usage: install-native-on-compute.sh PROCESSOR_SOURCE [INSTALL_ROOT [PYTHON]]" >&2
    exit 2
fi
if [ -z "${SLURM_JOB_ID:-}" ]; then
    echo "refusing to install outside a Slurm compute allocation" >&2
    exit 2
fi
if [ "$(uname -s)" != Linux ] || [ "$(uname -m)" != x86_64 ]; then
    echo "the pinned native release requires Linux x86_64" >&2
    exit 2
fi

processor_source=$1
install_root=${2:-/data/paul/scaling-neuro/native/releases/0.2.0}
python_bin=${3:-python3.12}

if [ ! -r "$processor_source/requirements.lock" ] || \
    [ ! -d "$processor_source/scaling_neuro_processor" ] || \
    [ ! -r "$processor_source/scripts/controller-source-sha256.py" ]; then
    echo "processor source directory is incomplete" >&2
    exit 2
fi
if ! command -v "$python_bin" >/dev/null 2>&1; then
    echo "Python 3.12 is required for the hash-locked native environment" >&2
    exit 2
fi
if [ ! -x "$ZSTD_SYSTEM_BIN" ]; then
    echo "the pinned system zstd is required on the compute node" >&2
    exit 2
fi
actual_zstd_sha256=$(sha256sum "$ZSTD_SYSTEM_BIN" | awk '{print $1}')
if [ "$actual_zstd_sha256" != "$ZSTD_SHA256" ] || \
    ! "$ZSTD_SYSTEM_BIN" --version | grep -F "v$ZSTD_VERSION" >/dev/null; then
    echo "system zstd does not match the pinned compute-node binary" >&2
    exit 2
fi

enroot_runtime_root="/tmp/scaling-neuro-enroot-install-$(id -u)-${SLURM_JOB_ID}-$$"
install -d -m 700 \
    "$enroot_runtime_root" \
    "$enroot_runtime_root/cache" \
    "$enroot_runtime_root/data" \
    "$enroot_runtime_root/runtime" \
    "$enroot_runtime_root/tmp"
staging=
cleanup() {
    status=$?
    trap - EXIT HUP INT TERM
    rm -rf "$enroot_runtime_root"
    if [ "$status" -ne 0 ] && [ -n "$staging" ] && [ -d "$staging" ]; then
        rm -rf "$staging"
    fi
    exit "$status"
}
trap cleanup EXIT HUP INT TERM
if [ ! -x /usr/bin/mksquashfs ] || [ ! -x /opt/slurm/bin/srun ]; then
    echo "mksquashfs and the cluster Slurm client are required" >&2
    exit 2
fi
if ! "$python_bin" -c 'import sys; raise SystemExit(sys.version_info[:2] != (3, 12))'; then
    echo "Python must be exactly the 3.12 minor series" >&2
    exit 2
fi

controller_source_sha256() {
    requirements_path=$1
    package_path=$2
    "$python_bin" "$processor_source/scripts/controller-source-sha256.py" \
        "$requirements_path" "$package_path"
}

source_controller_sha256=$(controller_source_sha256 \
    "$processor_source/requirements.lock" \
    "$processor_source/scaling_neuro_processor")

validate_release() {
    release=$1
    test -x "$release/venv/bin/python" || return 1
    test -x "$release/bin/dcm2niix" || return 1
    test -x "$release/bin/controller-source-sha256.py" || return 1
    test -r "$release/native-tools.sqsh" || return 1
    test -r "$release/RELEASE" || return 1
    test -r "$release/app/scaling_neuro_processor/__init__.py" || return 1
    grep -Fx "processor_version=$PROCESSOR_VERSION" "$release/RELEASE" >/dev/null || return 1
    grep -Fx "dcm2niix_version=v$DCM2NIIX_VERSION" "$release/RELEASE" >/dev/null || return 1
    grep -Fx "dcm2niix_archive_sha256=$DCM2NIIX_SHA256" "$release/RELEASE" >/dev/null || return 1
    grep -Fx "zstd_version=v$ZSTD_VERSION" "$release/RELEASE" >/dev/null || return 1
    grep -Fx "zstd_binary_sha256=$ZSTD_SHA256" "$release/RELEASE" >/dev/null || return 1
    grep -Fx "controller_source_sha256=$source_controller_sha256" "$release/RELEASE" >/dev/null || return 1
    PYTHONNOUSERSITE=1 PYTHONPATH="$release/app" "$release/venv/bin/python" -c \
        'import numpy, pydicom, scaling_neuro_processor as p; assert numpy.__version__ == "2.2.6"; assert pydicom.__version__ == "3.0.1"; assert p.__version__ == "0.2.0"' || return 1
    installed_controller_sha256=$(controller_source_sha256 \
        "$release/requirements.lock" \
        "$release/app/scaling_neuro_processor") || return 1
    test "$installed_controller_sha256" = "$source_controller_sha256" || return 1
    expected_image_sha256=$(sed -n 's/^native_tools_squashfs_sha256=//p' "$release/RELEASE")
    actual_image_sha256=$(sha256sum "$release/native-tools.sqsh" | awk '{print $1}')
    test -n "$expected_image_sha256" || return 1
    test "$actual_image_sha256" = "$expected_image_sha256" || return 1
    validate_sandbox_version \
        "$release" \
        dcm2niix \
        "/opt/scaling-neuro/dcm2niix" \
        "v$DCM2NIIX_VERSION" \
        3 || return 1
    validate_sandbox_version \
        "$release" \
        zstd \
        "/opt/scaling-neuro/zstd" \
        "v$ZSTD_VERSION" \
        0 || return 1
}

validate_sandbox_version() {
    release=$1
    tool_name=$2
    tool_path=$3
    expected_version=$4
    expected_status=$5
    output="$enroot_runtime_root/$tool_name-version.log"

    # The deliberately minimal environment must not make srun forget the parent
    # allocation. Without --jobid, Slurm creates a separate job for each probe.
    if env -i \
        ENROOT_RESTRICT_DEV=yes \
        ENROOT_CACHE_PATH="$enroot_runtime_root/cache" \
        ENROOT_DATA_PATH="$enroot_runtime_root/data" \
        ENROOT_RUNTIME_PATH="$enroot_runtime_root/runtime" \
        ENROOT_TEMP_PATH="$enroot_runtime_root/tmp" \
        HOME=/tmp \
        LANG=C \
        LC_ALL=C \
        PATH=/opt/slurm/bin:/usr/bin:/bin \
        TZ=UTC \
        XDG_RUNTIME_DIR="$enroot_runtime_root/runtime" \
        /opt/slurm/bin/srun \
        --jobid="$SLURM_JOB_ID" \
        --overlap \
        --nodes=1 \
        --ntasks=1 \
        --container-image="$release/native-tools.sqsh" \
        --container-readonly \
        --no-container-mount-home \
        --no-container-remap-root \
        --no-container-entrypoint \
        --container-workdir=/tmp \
        "$tool_path" --version >"$output" 2>&1; then
        actual_status=0
    else
        actual_status=$?
    fi
    if [ "$actual_status" -ne "$expected_status" ]; then
        echo "sandboxed $tool_name version probe returned $actual_status; expected $expected_status" >&2
        sed -n '1,40p' "$output" >&2
        return 1
    fi
    if ! grep -F "$expected_version" "$output" >/dev/null; then
        echo "sandboxed $tool_name did not report the pinned version $expected_version" >&2
        sed -n '1,40p' "$output" >&2
        return 1
    fi
    rm -f "$output"
}

if [ -e "$install_root" ]; then
    if validate_release "$install_root"; then
        echo "native processor release already verified at $install_root"
        exit 0
    fi
    echo "existing native release is incomplete or does not match; choose a new versioned INSTALL_ROOT" >&2
    exit 2
fi

install_parent=$(dirname "$install_root")
install -d -m 700 "$install_parent"
staging="$install_parent/.install-${PROCESSOR_VERSION}-${SLURM_JOB_ID}-$$"
if [ -e "$staging" ]; then
    echo "unique staging directory already exists" >&2
    exit 2
fi
install -d -m 700 \
    "$staging" \
    "$staging/app" \
    "$staging/app/scaling_neuro_processor" \
    "$staging/bin"

"$python_bin" -m venv "$staging/venv"
"$staging/venv/bin/python" -m pip install \
    --disable-pip-version-check \
    --no-cache-dir \
    --only-binary=:all: \
    --require-hashes \
    -r "$processor_source/requirements.lock"

echo "installed hash-locked Python controller dependencies"

"$python_bin" - \
    "$processor_source/scaling_neuro_processor" \
    "$staging/app/scaling_neuro_processor" <<'PY'
from pathlib import Path
import shutil
import sys

source = Path(sys.argv[1])
destination = Path(sys.argv[2])
files = sorted(path for path in source.rglob("*.py") if path.is_file())
if not files or any(path.is_symlink() for path in files):
    raise SystemExit(2)
for path in files:
    target = destination / path.relative_to(source)
    target.parent.mkdir(mode=0o700, parents=True, exist_ok=True)
    shutil.copyfile(path, target)
    target.chmod(0o444)
PY
install -m 0444 "$processor_source/requirements.lock" "$staging/requirements.lock"
install -m 0555 \
    "$processor_source/scripts/controller-source-sha256.py" \
    "$staging/bin/controller-source-sha256.py"
installed_controller_sha256=$(controller_source_sha256 \
    "$staging/requirements.lock" \
    "$staging/app/scaling_neuro_processor")
if [ "$installed_controller_sha256" != "$source_controller_sha256" ]; then
    echo "installed controller source digest mismatch" >&2
    exit 2
fi

"$python_bin" - "$DCM2NIIX_URL" "$staging/dcm2niix.zip" <<'PY'
from pathlib import Path
import sys
from urllib.request import urlopen

url, destination = sys.argv[1:]
with urlopen(url, timeout=120) as response, Path(destination).open("wb") as output:
    while chunk := response.read(1024 * 1024):
        output.write(chunk)
PY

actual_sha256=$("$python_bin" - "$staging/dcm2niix.zip" <<'PY'
from pathlib import Path
import hashlib
import sys

digest = hashlib.sha256()
with Path(sys.argv[1]).open("rb") as stream:
    while chunk := stream.read(1024 * 1024):
        digest.update(chunk)
print(digest.hexdigest())
PY
)
if [ "$actual_sha256" != "$DCM2NIIX_SHA256" ]; then
    echo "pinned dcm2niix release checksum mismatch" >&2
    exit 2
fi
"$python_bin" -m zipfile -e "$staging/dcm2niix.zip" "$staging/unpacked"
if [ ! -f "$staging/unpacked/dcm2niix" ]; then
    echo "pinned dcm2niix archive layout changed" >&2
    exit 2
fi
install -m 0555 "$staging/unpacked/dcm2niix" "$staging/bin/dcm2niix"
"$python_bin" - "$staging/dcm2niix.zip" "$staging/unpacked" <<'PY'
from pathlib import Path
import shutil
import sys

Path(sys.argv[1]).unlink()
shutil.rmtree(sys.argv[2])
PY

echo "downloaded and verified dcm2niix v$DCM2NIIX_VERSION"

sandbox_root="$staging/sandbox-root"
install -d -m 0755 \
    "$sandbox_root/bin" \
    "$sandbox_root/input" \
    "$sandbox_root/output" \
    "$sandbox_root/opt/scaling-neuro"
install -d -m 1777 "$sandbox_root/tmp"
install -m 0555 \
    "$staging/bin/dcm2niix" \
    "$sandbox_root/opt/scaling-neuro/dcm2niix"
install -m 0555 "$ZSTD_SYSTEM_BIN" "$sandbox_root/opt/scaling-neuro/zstd"
install -m 0555 /bin/dash "$sandbox_root/bin/dash"
ln -s dash "$sandbox_root/bin/sh"

ldd_output="$staging/dcm2niix.ldd"
if ! {
    ldd "$staging/bin/dcm2niix" && ldd "$ZSTD_SYSTEM_BIN" && ldd /bin/dash
} >"$ldd_output" 2>&1; then
    echo "could not resolve pinned converter runtime dependencies" >&2
    exit 2
fi
if grep -F "not found" "$ldd_output" >/dev/null; then
    echo "pinned dcm2niix has an unavailable runtime dependency" >&2
    exit 2
fi
awk '{ for (field = 1; field <= NF; field++) if ($field ~ /^\//) print $field }' \
    "$ldd_output" | LC_ALL=C sort -u | while IFS= read -r dependency; do
        case "$dependency" in
            /lib/*|/lib64/*|/usr/lib/*|/usr/lib64/*) ;;
            *)
                echo "refusing unexpected dcm2niix runtime dependency: $dependency" >&2
                exit 2
                ;;
        esac
        resolved=$(readlink -f "$dependency")
        if [ ! -f "$resolved" ]; then
            echo "dcm2niix runtime dependency is not a regular file" >&2
            exit 2
        fi
        install -D -m 0555 "$resolved" "$sandbox_root$dependency"
    done
rm -f "$ldd_output"

/usr/bin/mksquashfs \
    "$sandbox_root" \
    "$staging/native-tools.sqsh" \
    -comp zstd \
    -noappend \
    -all-root \
    -no-xattrs \
    -all-time 0 \
    -mkfs-time 0 \
    -no-progress >/dev/null
chmod 0444 "$staging/native-tools.sqsh"
image_sha256=$(sha256sum "$staging/native-tools.sqsh" | awk '{print $1}')
"$python_bin" - "$sandbox_root" <<'PY'
from pathlib import Path
import shutil
import sys

shutil.rmtree(Path(sys.argv[1]))
PY

echo "built native converter sandbox"

printf '%s\n' \
    "processor_version=$PROCESSOR_VERSION" \
    "dcm2niix_version=v$DCM2NIIX_VERSION" \
    "dcm2niix_archive_sha256=$DCM2NIIX_SHA256" \
    "zstd_version=v$ZSTD_VERSION" \
    "zstd_binary_sha256=$ZSTD_SHA256" \
    "native_tools_squashfs_sha256=$image_sha256" \
    "controller_source_sha256=$source_controller_sha256" \
    "python_series=3.12" \
    "numpy_version=2.2.6" \
    "pydicom_version=3.0.1" >"$staging/RELEASE"
chmod 0444 "$staging/RELEASE"

echo "validating native processor release"
validate_release "$staging"
mv "$staging" "$install_root"
validate_release "$install_root"
echo "installed and verified native processor release at $install_root"
