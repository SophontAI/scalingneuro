from __future__ import annotations

import hashlib
import os
from pathlib import Path
from urllib.error import HTTPError, URLError
from urllib.request import Request, build_opener

from .api import NoRedirects
from .config import Config
from .errors import ApiFailure, InvalidJob
from .models import Download, OutputFile, PutGrant


CHUNK_BYTES = 8 * 1024**2


def sha256_file(path: Path) -> tuple[int, str]:
    digest = hashlib.sha256()
    size = 0
    with path.open("rb") as stream:
        while chunk := stream.read(CHUNK_BYTES):
            size += len(chunk)
            digest.update(chunk)
    return size, digest.hexdigest()


class ObjectTransport:
    def __init__(self, config: Config) -> None:
        self.config = config
        self._opener = build_opener(NoRedirects())

    def _check_url(self, url: str) -> None:
        if not self.config.object_url_allowed(url):
            raise ApiFailure("OBJECT_URL_REJECTED")

    def download(self, descriptor: Download, destination: Path) -> None:
        self._check_url(descriptor.url)
        if destination.exists():
            size, cached_sha256 = sha256_file(destination)
            if size == descriptor.size_bytes and cached_sha256 == descriptor.sha256:
                return
        destination.parent.mkdir(parents=True, exist_ok=True, mode=0o700)
        partial = destination.with_suffix(destination.suffix + ".partial")
        stream_digest = hashlib.sha256()
        received = 0
        request = Request(
            descriptor.url,
            method="GET",
            headers={**descriptor.headers, "Accept": "application/octet-stream"},
        )
        try:
            with self._opener.open(
                request, timeout=self.config.object_transfer_timeout_seconds
            ) as response:
                if not 200 <= response.status < 300:
                    raise ApiFailure("OBJECT_DOWNLOAD_FAILED")
                with partial.open("wb") as output:
                    os.chmod(partial, 0o600)
                    while chunk := response.read(CHUNK_BYTES):
                        received += len(chunk)
                        if received > descriptor.size_bytes:
                            raise InvalidJob()
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
            raise InvalidJob()
        os.replace(partial, destination)

    def upload(self, output: OutputFile, grant: PutGrant) -> None:
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
                request = Request(grant.url, data=stream, method="PUT", headers=headers)
                with self._opener.open(
                    request, timeout=self.config.object_transfer_timeout_seconds
                ) as response:
                    if not 200 <= response.status < 300:
                        raise ApiFailure("OBJECT_UPLOAD_FAILED")
                    response.read(4096)
        except (HTTPError, URLError, TimeoutError, OSError) as exc:
            raise ApiFailure("OBJECT_UPLOAD_FAILED") from exc
