from __future__ import annotations

import hashlib
import os
from pathlib import Path
from threading import Event
from time import monotonic
from typing import BinaryIO
from urllib.error import HTTPError, URLError
from urllib.request import Request, build_opener

from .api import NoRedirects
from .config import Config
from .errors import ApiFailure, LeaseLost
from .models import Download, OutputFile, PutGrant


CHUNK_BYTES = 8 * 1024**2
# A socket operation is allowed to make progress for at most five minutes
# without returning control to the total wall-clock deadline check below.
SOCKET_OPERATION_TIMEOUT_SECONDS = 300


class _DeadlineReader:
    def __init__(self, stream: BinaryIO, deadline: float, lease_active: Event) -> None:
        self._stream = stream
        self._deadline = deadline
        self._lease_active = lease_active

    def _check_active(self) -> None:
        if not self._lease_active.is_set():
            raise LeaseLost()
        if monotonic() > self._deadline:
            raise TimeoutError("object transfer wall-clock deadline exceeded")

    def read(self, size: int = -1) -> bytes:
        self._check_active()
        data = self._stream.read(size)
        self._check_active()
        return data


def _require_active(lease_active: Event) -> None:
    if not lease_active.is_set():
        raise LeaseLost()


def sha256_file(path: Path, lease_active: Event | None = None) -> tuple[int, str]:
    digest = hashlib.sha256()
    size = 0
    with path.open("rb") as stream:
        while chunk := stream.read(CHUNK_BYTES):
            if lease_active is not None:
                _require_active(lease_active)
            size += len(chunk)
            digest.update(chunk)
    if lease_active is not None:
        _require_active(lease_active)
    return size, digest.hexdigest()


class ObjectTransport:
    def __init__(self, config: Config) -> None:
        self.config = config
        self._opener = build_opener(NoRedirects())

    def _check_url(self, url: str) -> None:
        if not self.config.object_url_allowed(url):
            raise ApiFailure("OBJECT_URL_REJECTED")

    def download(
        self, descriptor: Download, destination: Path, lease_active: Event
    ) -> None:
        _require_active(lease_active)
        self._check_url(descriptor.url)
        if destination.exists():
            size, cached_sha256 = sha256_file(destination, lease_active)
            if size == descriptor.size_bytes and cached_sha256 == descriptor.sha256:
                return
        destination.parent.mkdir(parents=True, exist_ok=True, mode=0o700)
        partial = destination.with_suffix(destination.suffix + ".partial")
        stream_digest = hashlib.sha256()
        received = 0
        deadline = monotonic() + self.config.object_transfer_timeout_seconds
        request = Request(
            descriptor.url,
            method="GET",
            headers={**descriptor.headers, "Accept": "application/octet-stream"},
        )
        try:
            with self._opener.open(
                request,
                timeout=min(
                    self.config.object_transfer_timeout_seconds,
                    SOCKET_OPERATION_TIMEOUT_SECONDS,
                ),
            ) as response:
                if not 200 <= response.status < 300:
                    raise ApiFailure("OBJECT_DOWNLOAD_FAILED")
                with partial.open("wb") as output:
                    os.chmod(partial, 0o600)
                    read_once = getattr(response, "read1", response.read)
                    while True:
                        _require_active(lease_active)
                        if monotonic() > deadline:
                            raise TimeoutError(
                                "object transfer wall-clock deadline exceeded"
                            )
                        # HTTPResponse.read(n) may internally aggregate many
                        # socket reads before returning. read1() returns after
                        # at most one buffered/raw read so the wall-clock check
                        # runs even for a continuously progressing slow stream.
                        chunk = read_once(CHUNK_BYTES)
                        _require_active(lease_active)
                        if monotonic() > deadline:
                            raise TimeoutError(
                                "object transfer wall-clock deadline exceeded"
                            )
                        if not chunk:
                            break
                        received += len(chunk)
                        if received > descriptor.size_bytes:
                            raise ApiFailure("OBJECT_DOWNLOAD_INTEGRITY_MISMATCH")
                        stream_digest.update(chunk)
                        output.write(chunk)
                    output.flush()
                    os.fsync(output.fileno())
        except (HTTPError, URLError, TimeoutError, OSError) as exc:
            raise ApiFailure("OBJECT_DOWNLOAD_FAILED") from exc
        if (
            received != descriptor.size_bytes
            or stream_digest.hexdigest() != descriptor.sha256
        ):
            raise ApiFailure("OBJECT_DOWNLOAD_INTEGRITY_MISMATCH")
        _require_active(lease_active)
        os.replace(partial, destination)

    def upload(self, output: OutputFile, grant: PutGrant, lease_active: Event) -> None:
        _require_active(lease_active)
        self._check_url(grant.url)
        path = Path(output.path)
        try:
            size = path.stat().st_size
        except OSError as exc:
            raise ApiFailure("OUTPUT_UNAVAILABLE") from exc
        if size != output.size_bytes:
            raise ApiFailure("OUTPUT_CHANGED")
        headers = dict(grant.headers)
        for name, value in headers.items():
            lowered = name.lower()
            if lowered == "content-length" and value != str(output.size_bytes):
                raise ApiFailure("OUTPUT_GRANT_INVALID")
            if lowered == "content-type" and value != output.content_type:
                raise ApiFailure("OUTPUT_GRANT_INVALID")
        lowered_headers = {name.lower() for name in headers}
        if "content-length" not in lowered_headers:
            headers["Content-Length"] = str(output.size_bytes)
        if "content-type" not in lowered_headers:
            headers["Content-Type"] = output.content_type
        try:
            with path.open("rb") as stream:
                deadline = monotonic() + self.config.object_transfer_timeout_seconds
                request = Request(
                    grant.url,
                    data=_DeadlineReader(stream, deadline, lease_active),
                    method="PUT",
                    headers=headers,
                )
                with self._opener.open(
                    request,
                    timeout=min(
                        self.config.object_transfer_timeout_seconds,
                        SOCKET_OPERATION_TIMEOUT_SECONDS,
                    ),
                ) as response:
                    if not 200 <= response.status < 300:
                        raise ApiFailure("OBJECT_UPLOAD_FAILED")
                    _require_active(lease_active)
                    response.read(4096)
                    _require_active(lease_active)
        except (HTTPError, URLError, TimeoutError, OSError) as exc:
            raise ApiFailure("OBJECT_UPLOAD_FAILED") from exc
