from __future__ import annotations

import os
from pathlib import Path
import stat

from .config import Config
from .errors import ConverterFailure


NATIVE_DCM2NIIX = "/opt/scaling-neuro/dcm2niix"
NATIVE_ZSTD = "/opt/scaling-neuro/zstd"


def enabled(config: Config) -> bool:
    return config.native_tools_slurm_image is not None


def _verified_runtime_directory(path: Path) -> None:
    try:
        path.mkdir(mode=0o700, exist_ok=True)
        status = path.lstat()
    except OSError as exc:
        raise ConverterFailure(
            "CONVERTER_SANDBOX_RUNTIME_UNAVAILABLE", retryable=True
        ) from exc
    if (
        not stat.S_ISDIR(status.st_mode)
        or status.st_uid != os.getuid()
        or stat.S_IMODE(status.st_mode) != 0o700
    ):
        raise ConverterFailure("CONVERTER_SANDBOX_RUNTIME_UNAVAILABLE", retryable=True)


def subprocess_environment(config: Config, home: Path) -> dict[str, str]:
    if not enabled(config):
        return {
            "HOME": str(home),
            "LANG": "C.UTF-8",
            "LC_ALL": "C.UTF-8",
            "PATH": os.environ.get("PATH", "/usr/local/bin:/usr/bin:/bin"),
            "TZ": "UTC",
        }
    runtime_root = config.enroot_runtime_root
    _verified_runtime_directory(runtime_root)
    paths = {
        "ENROOT_CACHE_PATH": runtime_root / "cache",
        "ENROOT_DATA_PATH": runtime_root / "data",
        "ENROOT_RUNTIME_PATH": runtime_root / "runtime",
        "ENROOT_TEMP_PATH": runtime_root / "tmp",
    }
    for path in paths.values():
        _verified_runtime_directory(path)
    return {
        **{key: str(value) for key, value in paths.items()},
        "ENROOT_RESTRICT_DEV": "yes",
        "HOME": "/tmp",
        "LANG": "C",
        "LC_ALL": "C",
        "PATH": "/opt/slurm/bin:/usr/bin:/bin",
        "TZ": "UTC",
        "XDG_RUNTIME_DIR": str(paths["ENROOT_RUNTIME_PATH"]),
    }


def _host_path(path: Path) -> Path:
    try:
        resolved = path.resolve(strict=True)
    except OSError as exc:
        raise ConverterFailure(
            "CONVERTER_SANDBOX_PATH_INVALID", retryable=False
        ) from exc
    if any(character in str(resolved) for character in ",:\r\n"):
        raise ConverterFailure("CONVERTER_SANDBOX_PATH_INVALID", retryable=False)
    return resolved


def command(
    config: Config,
    *,
    mounts: tuple[tuple[Path, str, str], ...] = (),
    workdir: str = "/tmp",
    executable: str,
    arguments: tuple[str, ...] = (),
) -> list[str]:
    image = config.native_tools_slurm_image
    if image is None:
        raise ValueError("native-tools sandbox is not configured")
    value = [
        config.slurm_srun_bin,
        "--overlap",
        "--nodes=1",
        "--ntasks=1",
        f"--container-image={image}",
        "--container-readonly",
        "--no-container-mount-home",
        "--no-container-remap-root",
        "--no-container-entrypoint",
    ]
    if mounts:
        mount_value = ",".join(
            f"{_host_path(source)}:{target}:{options}"
            for source, target, options in mounts
        )
        value.append(f"--container-mounts={mount_value}")
    value.extend((f"--container-workdir={workdir}", executable, *arguments))
    return value
