from __future__ import annotations

from dataclasses import dataclass
from datetime import datetime
import re
from typing import Any, Mapping

from .errors import InvalidJob


SHA256_RE = re.compile(r"^[0-9a-f]{64}$")
PSEUDONYM_RE = re.compile(r"^[0-9a-f]{24}$")
SAFE_ID_RE = re.compile(r"^[A-Za-z0-9][A-Za-z0-9._:-]{0,127}$")
HEADER_RE = re.compile(r"^[!#$%&'*+.^_`|~0-9A-Za-z-]{1,128}$")
MAX_OBJECT_BYTES = 256 * 1024**3
MAX_DICOM_INSTANCES = 500_000


def _mapping(value: Any) -> Mapping[str, Any]:
    if not isinstance(value, Mapping):
        raise InvalidJob()
    return value


def _string(value: Any, *, identifier: bool = False, maximum: int = 512) -> str:
    if not isinstance(value, str) or not value or len(value) > maximum:
        raise InvalidJob()
    if identifier and not SAFE_ID_RE.fullmatch(value):
        raise InvalidJob()
    return value


def _sha(value: Any) -> str:
    value = _string(value, maximum=64)
    if not SHA256_RE.fullmatch(value):
        raise InvalidJob()
    return value


def _size(value: Any) -> int:
    if (
        isinstance(value, bool)
        or not isinstance(value, int)
        or not (1 <= value <= MAX_OBJECT_BYTES)
    ):
        raise InvalidJob()
    return value


def _headers(value: Any) -> dict[str, str]:
    if value is None:
        return {}
    source = _mapping(value)
    if len(source) > 32:
        raise InvalidJob()
    result: dict[str, str] = {}
    lowered_names: set[str] = set()
    for raw_name, raw_value in source.items():
        if not isinstance(raw_name, str) or not HEADER_RE.fullmatch(raw_name):
            raise InvalidJob()
        if (
            not isinstance(raw_value, str)
            or len(raw_value) > 4096
            or "\r" in raw_value
            or "\n" in raw_value
        ):
            raise InvalidJob()
        lowered = raw_name.lower()
        if lowered in lowered_names or lowered in {
            "authorization",
            "cookie",
            "host",
            "proxy-authorization",
        }:
            raise InvalidJob()
        lowered_names.add(lowered)
        result[raw_name] = raw_value
    return result


@dataclass(frozen=True)
class Download:
    url: str
    size_bytes: int
    sha256: str
    headers: dict[str, str]
    uncompressed_sha256: str | None = None
    filename: str | None = None

    @classmethod
    def from_json(cls, value: Any) -> "Download":
        obj = _mapping(value)
        uncompressed = obj.get("uncompressed_sha256")
        filename = obj.get("filename")
        if filename is None and isinstance(obj.get("key"), str):
            filename = obj["key"].rsplit("/", 1)[-1]
        if filename is not None:
            filename = _string(filename, maximum=255)
            if filename in {".", ".."} or "/" in filename or "\\" in filename:
                raise InvalidJob()
        return cls(
            url=_string(obj.get("url"), maximum=8192),
            size_bytes=_size(obj.get("size_bytes")),
            sha256=_sha(obj.get("sha256")),
            headers=_headers(obj.get("headers")),
            uncompressed_sha256=_sha(uncompressed)
            if uncompressed is not None
            else None,
            filename=filename,
        )


@dataclass(frozen=True)
class DicomInput:
    archive: Download
    format: str
    dicom_count: int


@dataclass(frozen=True)
class NiftiInput:
    nifti: Download
    sidecar: Download


@dataclass(frozen=True)
class Job:
    job_id: str
    upload_id: str
    series_archive_id: str
    series_id: str
    attempt: int
    lease_token: str
    lease_expires_at: str
    input_format: str
    input: DicomInput | NiftiInput

    @classmethod
    def from_json(cls, value: Any) -> "Job":
        obj = _mapping(value)
        schema_version = obj.get("schema_version", "1.0.0")
        if schema_version not in (1, "1", "1.0.0"):
            raise InvalidJob()
        input_format = _string(obj.get("input_format"), maximum=32)
        raw_input = _mapping(obj.get("input"))
        if input_format == "dicom-series-v1":
            descriptor = raw_input.get("archive", raw_input)
            count = raw_input.get("dicom_count")
            if (
                isinstance(count, bool)
                or not isinstance(count, int)
                or not (1 <= count <= MAX_DICOM_INSTANCES)
            ):
                raise InvalidJob()
            archive_format = raw_input.get("format", "dicom-tar-zstd")
            if archive_format != "dicom-tar-zstd":
                raise InvalidJob()
            archive = Download.from_json(descriptor)
            if archive.size_bytes > 64 * 1024**3:
                raise InvalidJob()
            parsed_input: DicomInput | NiftiInput = DicomInput(
                archive=archive,
                format=archive_format,
                dicom_count=count,
            )
        elif input_format == "nifti-v1":
            nifti = Download.from_json(raw_input.get("nifti"))
            if nifti.uncompressed_sha256 is None:
                raise InvalidJob()
            sidecar = Download.from_json(raw_input.get("sidecar"))
            if nifti.size_bytes > 5 * 1024**3 or sidecar.size_bytes > 8 * 1024**2:
                raise InvalidJob()
            parsed_input = NiftiInput(
                nifti=nifti,
                sidecar=sidecar,
            )
        else:
            raise InvalidJob()

        attempt = obj.get("attempt")
        if (
            isinstance(attempt, bool)
            or not isinstance(attempt, int)
            or not (1 <= attempt <= 100)
        ):
            raise InvalidJob()
        lease_expires_at = _string(obj.get("lease_expires_at"), maximum=64)
        try:
            datetime.fromisoformat(lease_expires_at.replace("Z", "+00:00"))
        except ValueError as exc:
            raise InvalidJob() from exc
        archive_id_value = obj.get("series_archive_id", obj.get("bundle_id"))
        if (
            obj.get("series_archive_id") is not None
            and obj.get("bundle_id") is not None
            and obj["series_archive_id"] != obj["bundle_id"]
        ):
            raise InvalidJob()
        series_archive_id = _string(archive_id_value, maximum=24)
        if not PSEUDONYM_RE.fullmatch(series_archive_id):
            raise InvalidJob()
        series_id = _string(obj.get("series_id"), maximum=24)
        if not PSEUDONYM_RE.fullmatch(series_id):
            raise InvalidJob()
        return cls(
            job_id=_string(obj.get("job_id"), identifier=True, maximum=128),
            upload_id=_string(obj.get("upload_id"), identifier=True, maximum=128),
            series_archive_id=series_archive_id,
            series_id=series_id,
            attempt=attempt,
            lease_token=_string(obj.get("lease_token"), maximum=4096),
            lease_expires_at=lease_expires_at,
            input_format=input_format,
            input=parsed_input,
        )


@dataclass(frozen=True)
class OutputFile:
    kind: str
    path: str
    size_bytes: int
    sha256: str
    content_type: str
    uncompressed_sha256: str | None = None

    def descriptor(self) -> dict[str, Any]:
        result: dict[str, Any] = {
            "kind": self.kind,
            "size_bytes": self.size_bytes,
            "sha256": self.sha256,
            "content_type": self.content_type,
        }
        if self.uncompressed_sha256 is not None:
            result["uncompressed_sha256"] = self.uncompressed_sha256
        return result


@dataclass(frozen=True)
class PutGrant:
    kind: str
    url: str
    expires_at: str
    headers: dict[str, str]

    @classmethod
    def from_json(cls, value: Any) -> "PutGrant":
        obj = _mapping(value)
        kind = _string(obj.get("kind"), maximum=32)
        if kind not in {"nifti", "sidecar", "processing_manifest"}:
            raise InvalidJob()
        return cls(
            kind=kind,
            url=_string(obj.get("url"), maximum=8192),
            expires_at=_string(obj.get("expires_at"), maximum=64),
            headers=_headers(obj.get("headers")),
        )
