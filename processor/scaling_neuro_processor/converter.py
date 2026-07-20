from __future__ import annotations

from dataclasses import dataclass
from pathlib import Path
import subprocess
from threading import Event
from time import monotonic, sleep

from . import DCM2NIIX_VERSION
from . import sandbox
from .config import Config
from .errors import ConverterFailure, LeaseLost


NORMALIZED_ARGUMENTS = [
    "-b",
    "y",
    "-ba",
    "y",
    "-g",
    "i",
    "-i",
    "n",
    "-l",
    "o",
    "-m",
    "2",
    "-p",
    "y",
    "-t",
    "n",
    "-x",
    "i",
    "-z",
    "n",
    "-f",
    "series",
]
SANDBOX_DCM2NIIX = sandbox.NATIVE_DCM2NIIX
CONVERSION_POLL_SECONDS = 0.25


@dataclass(frozen=True)
class ConversionResult:
    nifti: Path
    sidecar: Path
    version: str


def version_command(config: Config) -> list[str]:
    if not sandbox.enabled(config):
        return [config.dcm2niix_bin, "--version"]
    return sandbox.command(
        config,
        executable=SANDBOX_DCM2NIIX,
        arguments=("--version",),
    )


def conversion_command(config: Config, dicom_dir: Path, output_dir: Path) -> list[str]:
    if not sandbox.enabled(config):
        return [
            config.dcm2niix_bin,
            *NORMALIZED_ARGUMENTS,
            "-o",
            str(output_dir),
            str(dicom_dir),
        ]
    return sandbox.command(
        config,
        mounts=(
            (dicom_dir, "/input", "ro+rprivate"),
            (output_dir, "/output", "rw+rprivate"),
        ),
        workdir="/input",
        executable=SANDBOX_DCM2NIIX,
        arguments=(*NORMALIZED_ARGUMENTS, "-o", "/output", "/input"),
    )


def _subprocess_environment(config: Config, home: Path) -> dict[str, str]:
    return sandbox.subprocess_environment(config, home)


def _require_active(lease_active: Event | None) -> None:
    if lease_active is not None and not lease_active.is_set():
        raise LeaseLost()


def _terminate_process(process: subprocess.Popen[bytes]) -> None:
    try:
        if process.poll() is None:
            process.terminate()
    except OSError:
        pass
    try:
        process.wait(timeout=5)
    except subprocess.TimeoutExpired:
        try:
            process.kill()
        except OSError:
            pass
        try:
            process.wait(timeout=5)
        except (OSError, subprocess.TimeoutExpired):
            pass
    except OSError:
        pass


def check_version(config: Config, lease_active: Event | None = None) -> str:
    _require_active(lease_active)
    try:
        result = subprocess.run(
            version_command(config),
            stdin=subprocess.DEVNULL,
            stdout=subprocess.PIPE,
            stderr=subprocess.STDOUT,
            timeout=30,
            check=False,
            env=_subprocess_environment(config, config.work_root),
        )
    except FileNotFoundError as exc:
        raise ConverterFailure("DCM2NIIX_UNAVAILABLE", retryable=True) from exc
    except (OSError, subprocess.TimeoutExpired) as exc:
        raise ConverterFailure("DCM2NIIX_VERSION_FAILED", retryable=True) from exc
    _require_active(lease_active)
    output = result.stdout.decode("utf-8", errors="replace")[:4096]
    # The official Linux release reports a successful --version probe with
    # status 3. Builds used by the test harness and some distributions use 0.
    if result.returncode not in {0, 3} or DCM2NIIX_VERSION not in output:
        raise ConverterFailure("DCM2NIIX_VERSION_MISMATCH", retryable=True)
    return DCM2NIIX_VERSION


def convert(
    config: Config,
    dicom_dir: Path,
    output_dir: Path,
    lease_active: Event,
) -> ConversionResult:
    version = check_version(config, lease_active)
    _require_active(lease_active)
    output_dir.mkdir(parents=True, exist_ok=False, mode=0o700)
    command = conversion_command(config, dicom_dir, output_dir)
    try:
        process = subprocess.Popen(
            command,
            stdin=subprocess.DEVNULL,
            stdout=subprocess.DEVNULL,
            stderr=subprocess.DEVNULL,
            env=_subprocess_environment(config, output_dir),
        )
    except FileNotFoundError as exc:
        raise ConverterFailure("DCM2NIIX_UNAVAILABLE", retryable=True) from exc
    except OSError as exc:
        raise ConverterFailure("DCM2NIIX_EXECUTION_FAILED", retryable=True) from exc
    deadline = monotonic() + config.conversion_timeout_seconds
    try:
        while True:
            _require_active(lease_active)
            returncode = process.poll()
            if returncode is not None:
                break
            if monotonic() > deadline:
                raise ConverterFailure("DCM2NIIX_TIMEOUT", retryable=True)
            sleep(CONVERSION_POLL_SECONDS)
        _require_active(lease_active)
    except BaseException:
        _terminate_process(process)
        raise
    if returncode != 0:
        raise ConverterFailure()
    regular = sorted(path for path in output_dir.iterdir() if path.is_file())
    nifti = [path for path in regular if path.suffix == ".nii"]
    sidecars = [path for path in regular if path.suffix == ".json"]
    unexpected = [path for path in regular if path.suffix not in {".nii", ".json"}]
    if len(nifti) != 1 or len(sidecars) != 1 or unexpected:
        raise ConverterFailure("DCM2NIIX_OUTPUT_AMBIGUOUS")
    if nifti[0].stem != sidecars[0].stem:
        raise ConverterFailure("DCM2NIIX_OUTPUT_MISMATCH")
    return ConversionResult(nifti=nifti[0], sidecar=sidecars[0], version=version)
