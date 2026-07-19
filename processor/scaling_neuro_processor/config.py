from __future__ import annotations

from dataclasses import dataclass, field
import hashlib
import os
from pathlib import Path
import re
import secrets
import socket
import stat as stat_module
from urllib.parse import urlsplit

from .errors import ProcessorError


PROCESSOR_ID_RE = re.compile(r"[^A-Za-z0-9._:-]+")


def default_processor_id() -> str:
    pieces = [socket.gethostname()]
    if os.environ.get("SLURM_JOB_ID"):
        pieces.append(os.environ["SLURM_JOB_ID"])
    if os.environ.get("SLURM_ARRAY_TASK_ID"):
        pieces.append(os.environ["SLURM_ARRAY_TASK_ID"])
    value = PROCESSOR_ID_RE.sub("-", "-".join(pieces)).strip("-")
    return value[:96] or f"processor-{os.getpid()}"


def default_enroot_runtime_root() -> Path:
    suffix = secrets.token_hex(8)
    return Path("/tmp") / f"scaling-neuro-enroot-{os.getuid()}-{os.getpid()}-{suffix}"


def read_token(path: Path) -> str:
    try:
        stat = path.stat()
        token = path.read_text(encoding="utf-8").strip()
    except OSError as exc:
        raise ProcessorError("PROCESSOR_TOKEN_UNAVAILABLE", retryable=True) from exc
    if not stat_module.S_ISREG(stat.st_mode):
        raise ProcessorError("PROCESSOR_TOKEN_INVALID", retryable=False)
    if stat.st_mode & 0o077:
        raise ProcessorError("PROCESSOR_TOKEN_PERMISSIONS", retryable=False)
    if not token or len(token) > 4096 or "\n" in token or "\r" in token:
        raise ProcessorError("PROCESSOR_TOKEN_INVALID", retryable=False)
    return token


@dataclass(frozen=True)
class Config:
    api_url: str
    token: str
    work_root: Path
    processor_id: str
    dcm2niix_bin: str = "dcm2niix"
    native_tools_slurm_image: Path | None = None
    slurm_srun_bin: str = "/opt/slurm/bin/srun"
    enroot_runtime_root: Path = field(default_factory=default_enroot_runtime_root)
    zstd_bin: str = "zstd"
    lease_seconds: int = 900
    heartbeat_seconds: int = 60
    request_timeout_seconds: int = 120
    object_transfer_timeout_seconds: int = 3600
    conversion_timeout_seconds: int = 7200
    idle_seconds: float = 15.0
    idle_exit_after_seconds: int = 300
    max_archive_uncompressed_bytes: int = 64 * 1024**3
    archive_expansion_floor_bytes: int = 64 * 1024**2
    archive_expansion_ratio: int = 20
    disk_reserve_bytes: int = 20 * 1024**3
    inode_reserve: int = 1024
    max_jobs: int = 0
    allow_insecure_http: bool = False
    allowed_object_hosts: tuple[str, ...] = (".r2.cloudflarestorage.com",)

    def __post_init__(self) -> None:
        parsed = urlsplit(self.api_url)
        if parsed.scheme not in (
            {"https", "http"} if self.allow_insecure_http else {"https"}
        ):
            raise ProcessorError("API_URL_INVALID", retryable=False)
        if (
            not parsed.hostname
            or parsed.username
            or parsed.password
            or parsed.query
            or parsed.fragment
        ):
            raise ProcessorError("API_URL_INVALID", retryable=False)
        if len(self.processor_id) > 96 or not re.fullmatch(
            r"[A-Za-z0-9][A-Za-z0-9._:-]{0,95}", self.processor_id
        ):
            raise ProcessorError("PROCESSOR_ID_INVALID", retryable=False)
        if not (300 <= self.lease_seconds <= 3600):
            raise ProcessorError("LEASE_CONFIGURATION_INVALID", retryable=False)
        if not (10 <= self.heartbeat_seconds < self.lease_seconds // 2):
            raise ProcessorError("HEARTBEAT_CONFIGURATION_INVALID", retryable=False)
        if not (300 <= self.object_transfer_timeout_seconds <= 86_400):
            raise ProcessorError("OBJECT_TRANSFER_TIMEOUT_INVALID", retryable=False)
        if (
            self.max_archive_uncompressed_bytes < 1024**3
            or not 64 * 1024**2
            <= self.archive_expansion_floor_bytes
            <= self.max_archive_uncompressed_bytes
            or not 2 <= self.archive_expansion_ratio <= 64
            or self.disk_reserve_bytes < 1024**3
            or self.inode_reserve < 128
        ):
            raise ProcessorError("ARCHIVE_LIMIT_INVALID", retryable=False)
        if self.native_tools_slurm_image is not None and (
            not self.native_tools_slurm_image.is_absolute()
            or not Path(self.slurm_srun_bin).is_absolute()
            or not self.enroot_runtime_root.is_absolute()
            or any(
                character in str(self.native_tools_slurm_image)
                for character in ",:\r\n"
            )
        ):
            raise ProcessorError(
                "CONVERTER_SANDBOX_CONFIGURATION_INVALID", retryable=False
            )

    @property
    def api_origin_host(self) -> str:
        hostname = urlsplit(self.api_url).hostname
        assert hostname is not None
        return hostname.lower()

    def job_root(self, job_id: str) -> Path:
        digest = hashlib.sha256(job_id.encode("utf-8")).hexdigest()[:32]
        return self.work_root / "jobs" / digest

    def object_url_allowed(self, url: str) -> bool:
        parsed = urlsplit(url)
        schemes = {"https", "http"} if self.allow_insecure_http else {"https"}
        if (
            parsed.scheme not in schemes
            or not parsed.hostname
            or parsed.username
            or parsed.password
        ):
            return False
        host = parsed.hostname.lower()
        if host == self.api_origin_host:
            return True
        return any(
            host == allowed or (allowed.startswith(".") and host.endswith(allowed))
            for allowed in self.allowed_object_hosts
        )
