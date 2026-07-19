from __future__ import annotations

from dataclasses import dataclass
import hashlib
import json
import math
import os
from pathlib import Path
import re
import shutil
import subprocess
from threading import Event, Timer
from typing import Any, Mapping

from .config import Config
from .dicom_privacy import (
    DicomAudit,
    SAFE_PRIVATE_EXCEPTION_ORDER,
    SAFE_PRIVATE_EXCEPTIONS,
    audit_dicom,
)
from .errors import CapacityFailure, ConverterFailure, InvalidArchive
from . import sandbox


DICOM_PATH_RE = re.compile(r"^dicom/([0-9]{6})\.dcm$")
PSEUDONYM_RE = re.compile(r"^[0-9a-f]{24}$")
UID_RE = re.compile(r"^[0-9]+(?:\.[0-9]+)+$")
SEMVER_RE = re.compile(
    r"^(?:0|[1-9][0-9]*)\.(?:0|[1-9][0-9]*)\.(?:0|[1-9][0-9]*)(?:[-+][A-Za-z0-9.-]+)?$"
)
SOFTWARE_VERSION_RE = re.compile(
    r"^(?:Siemens (?:[A-E][0-9]{2}[A-Z]?|V[A-E][0-9]{2}[A-Z]?|X[AB][0-9]{2}[A-Z]?)|"
    r"(?:Philips|Canon/Toshiba|United Imaging|Bruker) [1-9][0-9]?(?:\.[0-9]{1,2}){1,3}|"
    r"GE (?:DV[0-9]{1,2}(?:\.[0-9]{1,2})?|[1-9][0-9]?(?:\.[0-9]{1,2}){1,3}))$"
)
CANONICAL_COIL_RE = re.compile(
    r"^(?:HEAD(?:_NECK)?|NECK|BODY|SPINE|KNEE|FLEX|BREAST|CARDIAC|FOOT|ANKLE|SHOULDER|WRIST)"
    r"(?:_(?:[1-9]|[1-9][0-9]|1[0-9]{2}|2[0-4][0-9]|25[0-6]))?$"
)
SCANNER_MODELS = {
    "MAGNETOM Prisma_fit",
    "MAGNETOM Prisma",
    "MAGNETOM Skyra",
    "MAGNETOM TrioTim",
    "MAGNETOM Trio",
    "MAGNETOM Vida",
    "MAGNETOM Verio",
    "MAGNETOM Terra",
    "MAGNETOM Cima.X",
    "MAGNETOM Connectom",
    "MAGNETOM Sola",
    "MAGNETOM Aera",
    "MAGNETOM Avanto",
    "MAGNETOM Allegra",
    "MAGNETOM Espree",
    "Biograph mMR",
    "Ingenia Elition X",
    "Ingenia Ambition X",
    "Ingenia CX",
    "Ingenia",
    "Achieva dStream",
    "Achieva",
    "Intera",
    "MR 7700",
    "Discovery MR750w",
    "Discovery MR750",
    "Optima MR450w",
    "SIGNA Premier",
    "SIGNA Architect",
    "SIGNA PET/MR",
    "SIGNA HDxt",
    "SIGNA Voyager",
    "SIGNA Artist",
    "SIGNA Hero",
    "Vantage Galan",
    "Vantage Titan",
    "Vantage Orian",
    "Vantage Elan",
    "uMR Jupiter",
    "uMR Omega",
    "uMR 790",
    "uMR 780",
    "uMR 770",
    "uMR 670",
    "uMR 570",
    "uMR 560",
    "BioSpec",
    "PharmaScan",
}
RELEASE_MANUFACTURERS = {"SIEMENS", "Philips Medical Systems"}
PHILIPS_REQUIRED_PRIVATE_FIELDS = frozenset(
    {"scale_intercept", "scale_slope", "number_of_slices", "water_fat_shift"}
)
SEQUENCE_NAMES = {
    "ep2d_bold",
    "epfid_bold",
    "bold",
    "fmri",
    "ep2d",
    "epfid",
    "epi",
}
CLASSIFICATION_EVIDENCE = {
    "echo_planar_pulse_sequence",
    "echo_planar_scanning_sequence",
    "functional_image_type",
    "echo_planar_sequence",
    "functional_protocol_label",
    "functional_tr_range",
    "multiple_temporal_positions",
}
MAX_MANIFEST_BYTES = 128 * 1024**2
MAX_DICOM_BYTES = 256 * 1024**2
MAX_DICOM_INSTANCES = 500_000
MAX_COMPRESSED_ARCHIVE_BYTES = 64 * 1024**3
TAR_BLOCK_BYTES = 512
SANDBOX_ZSTD_INVALID_EXIT = 42
MIN_EXTRACTION_SECONDS = 10 * 60
MAX_EXTRACTION_SECONDS = 60 * 60
MIN_EXTRACTION_BYTES_PER_SECOND = 20 * 1024**2


@dataclass(frozen=True)
class ArchiveManifest:
    value: dict[str, Any]
    sha256: str
    extracted_bytes: int = 0

    @property
    def dicom_count(self) -> int:
        return self.value["dicom_instance_count"]


def _unique_object(pairs: list[tuple[str, Any]]) -> dict[str, Any]:
    value: dict[str, Any] = {}
    for key, item in pairs:
        if key in value:
            raise InvalidArchive("ARCHIVE_MANIFEST_DUPLICATE_KEY")
        value[key] = item
    return value


def _exact_keys(
    value: Mapping[str, Any],
    required: set[str],
    optional: set[str] | frozenset[str] = frozenset(),
) -> None:
    if set(value) - required - optional or not required.issubset(value):
        raise InvalidArchive("ARCHIVE_MANIFEST_SCHEMA")


def _pseudonym(value: Any) -> str:
    if not isinstance(value, str) or not PSEUDONYM_RE.fullmatch(value):
        raise InvalidArchive("ARCHIVE_MANIFEST_SCHEMA")
    return value


def _finite_number(value: Any, minimum: float, maximum: float) -> None:
    if isinstance(value, bool) or not isinstance(value, (int, float)):
        raise InvalidArchive("ARCHIVE_MANIFEST_SCHEMA")
    if not math.isfinite(value) or not minimum <= float(value) <= maximum:
        raise InvalidArchive("ARCHIVE_MANIFEST_SCHEMA")


def _validate_source(source: Any, count: int) -> None:
    if not isinstance(source, dict):
        raise InvalidArchive("ARCHIVE_MANIFEST_SCHEMA")
    required = {"dicom_count", "manufacturer", "model", "software_versions"}
    optional = {
        "patient_position",
        "magnetic_field_strength",
        "receive_coil_name",
        "transmit_coil_name",
        "sequence_name",
        "scanning_sequence",
        "sequence_variant",
        "scan_options",
        "mr_acquisition_type",
        "image_type",
        "series_number",
        "acquisition_number",
    }
    _exact_keys(source, required, optional)
    if (
        isinstance(source["dicom_count"], bool)
        or not isinstance(source["dicom_count"], int)
        or source["dicom_count"] != count
    ):
        raise InvalidArchive("ARCHIVE_DICOM_COUNT_MISMATCH")
    enum_lists = {
        "scanning_sequence": ({"SE", "IR", "GR", "EP", "RM"}, 5),
        "sequence_variant": (
            {"SK", "MTC", "SS", "TRSS", "SP", "MP", "OSP", "NONE"},
            8,
        ),
        "scan_options": (
            {"PER", "RG", "CG", "PPG", "FC", "PFF", "PFP", "SP", "FS"},
            9,
        ),
        "image_type": (
            {
                "ORIGINAL",
                "PRIMARY",
                "M",
                "MAGNITUDE",
                "P",
                "PHASE",
                "R",
                "REAL",
                "I",
                "IMAGINARY",
                "MIXED",
                "ND",
                "NORM",
                "MOSAIC",
                "DIS2D",
                "FMRI",
                "BOLD",
                "EPI",
                "NONE",
            },
            19,
        ),
    }
    for key, (allowed, maximum) in enum_lists.items():
        if key not in source:
            continue
        values = source[key]
        if (
            not isinstance(values, list)
            or not 1 <= len(values) <= maximum
            or any(not isinstance(item, str) for item in values)
            or len(values) != len(set(values))
        ):
            raise InvalidArchive("ARCHIVE_MANIFEST_SCHEMA")
        if any(item not in allowed for item in values):
            raise InvalidArchive("ARCHIVE_MANIFEST_SCHEMA")
    versions = source["software_versions"]
    if (
        not isinstance(versions, list)
        or not 1 <= len(versions) <= 16
        or any(not isinstance(item, str) for item in versions)
        or len(versions) != len(set(versions))
        or any(SOFTWARE_VERSION_RE.fullmatch(item) is None for item in versions)
    ):
        raise InvalidArchive("ARCHIVE_MANIFEST_SCHEMA")
    if (
        not isinstance(source["manufacturer"], str)
        or source["manufacturer"] not in RELEASE_MANUFACTURERS
    ):
        raise InvalidArchive("ARCHIVE_MANIFEST_SCHEMA")
    if not isinstance(source["model"], str) or source["model"] not in SCANNER_MODELS:
        raise InvalidArchive("ARCHIVE_MANIFEST_SCHEMA")
    if source["manufacturer"] == "SIEMENS" and (
        source["model"] != "MAGNETOM Prisma_fit" or versions != ["Siemens E11"]
    ):
        raise InvalidArchive("ARCHIVE_UNVERIFIED_SCANNER_FAMILY")
    if source["manufacturer"] == "Philips Medical Systems" and (
        source["model"] != "Achieva dStream"
        or "Philips 5.1.1" not in versions
        or any(
            version not in {"Philips 5.1.1", "Philips 5.1.1.0"} for version in versions
        )
    ):
        raise InvalidArchive("ARCHIVE_UNVERIFIED_SCANNER_FAMILY")
    for key in ("receive_coil_name", "transmit_coil_name"):
        if key in source and (
            not isinstance(source[key], str)
            or not CANONICAL_COIL_RE.fullmatch(source[key])
        ):
            raise InvalidArchive("ARCHIVE_MANIFEST_SCHEMA")
    if "sequence_name" in source and (
        not isinstance(source["sequence_name"], str)
        or source["sequence_name"] not in SEQUENCE_NAMES
    ):
        raise InvalidArchive("ARCHIVE_MANIFEST_SCHEMA")
    if "patient_position" in source and (
        not isinstance(source["patient_position"], str)
        or source["patient_position"]
        not in {"HFP", "HFS", "HFDR", "HFDL", "FFDR", "FFDL", "FFP", "FFS"}
    ):
        raise InvalidArchive("ARCHIVE_MANIFEST_SCHEMA")
    if "mr_acquisition_type" in source and (
        not isinstance(source["mr_acquisition_type"], str)
        or source["mr_acquisition_type"] not in {"2D", "3D"}
    ):
        raise InvalidArchive("ARCHIVE_MANIFEST_SCHEMA")
    if "magnetic_field_strength" in source:
        _finite_number(source["magnetic_field_strength"], 0.01, 15)
    for key in ("series_number", "acquisition_number"):
        if key in source and (
            isinstance(source[key], bool)
            or not isinstance(source[key], int)
            or not 0 <= source[key] <= 2**31 - 1
        ):
            raise InvalidArchive("ARCHIVE_MANIFEST_SCHEMA")


def validate_manifest(
    raw: bytes,
    *,
    expected_series_archive_id: str,
    expected_series_id: str,
    expected_dicom_count: int,
) -> ArchiveManifest:
    try:
        value = json.loads(
            raw.decode("utf-8"),
            object_pairs_hook=_unique_object,
            parse_constant=lambda _: (_ for _ in ()).throw(ValueError()),
        )
    except (UnicodeDecodeError, ValueError, InvalidArchive) as exc:
        if isinstance(exc, InvalidArchive):
            raise
        raise InvalidArchive("ARCHIVE_MANIFEST_JSON") from exc
    if not isinstance(value, dict):
        raise InvalidArchive("ARCHIVE_MANIFEST_SCHEMA")
    _exact_keys(
        value,
        {
            "schema_version",
            "series_archive_id",
            "series_id",
            "subject_id",
            "session_id",
            "protocol_group_id",
            "modality",
            "dicom_instance_count",
            "client",
            "deidentification",
            "source",
            "classification",
            "instances",
        },
    )
    if value["schema_version"] != "1.0.0" or value["modality"] != "functional_epi":
        raise InvalidArchive("ARCHIVE_MANIFEST_SCHEMA")
    archive_id = _pseudonym(value["series_archive_id"])
    if archive_id != expected_series_archive_id:
        raise InvalidArchive("ARCHIVE_SERIES_MISMATCH")
    for key in ("series_id", "subject_id", "session_id", "protocol_group_id"):
        _pseudonym(value[key])
    if value["series_id"] != expected_series_id:
        raise InvalidArchive("ARCHIVE_SERIES_MISMATCH")
    count = value["dicom_instance_count"]
    if (
        isinstance(count, bool)
        or not isinstance(count, int)
        or not 1 <= count <= MAX_DICOM_INSTANCES
        or count != expected_dicom_count
    ):
        raise InvalidArchive("ARCHIVE_DICOM_COUNT_MISMATCH")

    client = value["client"]
    if not isinstance(client, dict):
        raise InvalidArchive("ARCHIVE_MANIFEST_SCHEMA")
    _exact_keys(client, {"name", "version"})
    if (
        client["name"] != "neuro-sync"
        or not isinstance(client["version"], str)
        or not SEMVER_RE.fullmatch(client["version"])
    ):
        raise InvalidArchive("ARCHIVE_MANIFEST_SCHEMA")

    deid = value["deidentification"]
    if not isinstance(deid, dict):
        raise InvalidArchive("ARCHIVE_MANIFEST_SCHEMA")
    _exact_keys(
        deid,
        {
            "policy_id",
            "policy_version",
            "method",
            "recursive",
            "private_text_removed",
            "unknown_private_removed",
            "uids_remapped",
            "pixel_data_retained",
            "burned_in_annotation_status",
        },
        {"safe_private_exceptions", "metadata_transformations"},
    )
    for key in (
        "recursive",
        "private_text_removed",
        "unknown_private_removed",
        "uids_remapped",
        "pixel_data_retained",
    ):
        if deid[key] is not True:
            raise InvalidArchive("ARCHIVE_DEIDENTIFICATION_UNVERIFIED")
    if (
        deid["policy_id"] != "scaling-neuro.dicom-deidentification"
        or deid["policy_version"] != "1.0.0"
        or deid["method"] != "scaling-neuro-recursive-allowlist-v1"
        or not isinstance(deid["burned_in_annotation_status"], str)
        or deid["burned_in_annotation_status"] not in {"verified_no", "not_declared"}
    ):
        raise InvalidArchive("ARCHIVE_DEIDENTIFICATION_POLICY_MISMATCH")
    declared_private_exceptions = deid.get("safe_private_exceptions", [])
    if "safe_private_exceptions" in deid and (
        not isinstance(declared_private_exceptions, list)
        or not declared_private_exceptions
        or any(not isinstance(item, str) for item in declared_private_exceptions)
        or len(declared_private_exceptions) != len(set(declared_private_exceptions))
        or not set(declared_private_exceptions).issubset(SAFE_PRIVATE_EXCEPTIONS)
        or declared_private_exceptions
        != [
            item
            for item in SAFE_PRIVATE_EXCEPTION_ORDER
            if item in declared_private_exceptions
        ]
    ):
        raise InvalidArchive("ARCHIVE_DEIDENTIFICATION_POLICY_MISMATCH")
    metadata_transformations = deid.get("metadata_transformations", [])
    if "metadata_transformations" in deid and metadata_transformations != [
        "suppressed_redundant_philips_dynamic_trigger_time"
    ]:
        raise InvalidArchive("ARCHIVE_DEIDENTIFICATION_POLICY_MISMATCH")

    _validate_source(value["source"], count)
    classification = value["classification"]
    if not isinstance(classification, dict):
        raise InvalidArchive("ARCHIVE_MANIFEST_SCHEMA")
    _exact_keys(classification, {"decision", "kind", "confidence", "evidence"})
    if (
        classification["decision"] != "accepted"
        or classification["kind"] != "functional_epi"
    ):
        raise InvalidArchive("ARCHIVE_NOT_FUNCTIONAL_EPI")
    _finite_number(classification["confidence"], 0.9, 1.0)
    evidence = classification["evidence"]
    if not isinstance(evidence, list) or not 1 <= len(evidence) <= 64:
        raise InvalidArchive("ARCHIVE_MANIFEST_SCHEMA")
    observed_evidence: set[str] = set()
    for item in evidence:
        if not isinstance(item, dict):
            raise InvalidArchive("ARCHIVE_MANIFEST_SCHEMA")
        _exact_keys(item, {"code", "source", "effect"})
        code = item["code"]
        if (
            not isinstance(code, str)
            or code not in CLASSIFICATION_EVIDENCE
            or code in observed_evidence
        ):
            raise InvalidArchive("ARCHIVE_MANIFEST_SCHEMA")
        observed_evidence.add(code)
        if item["source"] != "dicom_header" or item["effect"] != "supports":
            raise InvalidArchive("ARCHIVE_MANIFEST_SCHEMA")

    instances = value["instances"]
    if not isinstance(instances, list) or len(instances) != count:
        raise InvalidArchive("ARCHIVE_DICOM_COUNT_MISMATCH")
    for index, item in enumerate(instances, 1):
        if not isinstance(item, dict):
            raise InvalidArchive("ARCHIVE_MANIFEST_SCHEMA")
        _exact_keys(item, {"path", "size_bytes", "sha256", "sop_instance_uid"})
        expected_path = f"dicom/{index:06d}.dcm"
        if item["path"] != expected_path:
            raise InvalidArchive("ARCHIVE_INSTANCE_ORDER")
        if (
            isinstance(item["size_bytes"], bool)
            or not isinstance(item["size_bytes"], int)
            or not 1 <= item["size_bytes"] <= MAX_DICOM_BYTES
        ):
            raise InvalidArchive("ARCHIVE_MANIFEST_SCHEMA")
        if not isinstance(item["sha256"], str) or not re.fullmatch(
            r"[0-9a-f]{64}", item["sha256"]
        ):
            raise InvalidArchive("ARCHIVE_MANIFEST_SCHEMA")
        uid = item["sop_instance_uid"]
        if not isinstance(uid, str) or len(uid) > 64 or not UID_RE.fullmatch(uid):
            raise InvalidArchive("ARCHIVE_MANIFEST_SCHEMA")
    return ArchiveManifest(value=value, sha256=hashlib.sha256(raw).hexdigest())


def _tar_octal_into(target: bytearray, start: int, length: int, value: int) -> None:
    encoded = format(value, "o").encode("ascii")
    if len(encoded) + 1 > length:
        raise InvalidArchive("ARCHIVE_TAR_HEADER_INVALID")
    target[start : start + length] = (
        b"0" * (length - len(encoded) - 1) + encoded + b"\0"
    )


def _tar_number_into(target: bytearray, start: int, length: int, value: int) -> None:
    if value >= 8_589_934_592 or (length == 8 and value >= 2_097_152):
        encoded = bytearray(length)
        encoded[-8:] = value.to_bytes(8, "big")
        encoded[0] |= 0x80
        target[start : start + length] = encoded
    else:
        _tar_octal_into(target, start, length, value)


def _canonical_tar_header(name: str, size: int) -> bytes:
    try:
        encoded_name = name.encode("ascii")
    except UnicodeEncodeError as exc:
        raise InvalidArchive("ARCHIVE_TAR_HEADER_INVALID") from exc
    if not encoded_name or len(encoded_name) > 99:
        raise InvalidArchive("ARCHIVE_TAR_HEADER_INVALID")
    header = bytearray(TAR_BLOCK_BYTES)
    header[: len(encoded_name)] = encoded_name
    _tar_octal_into(header, 100, 8, 0o600)
    _tar_number_into(header, 108, 8, 0)
    _tar_number_into(header, 116, 8, 0)
    _tar_number_into(header, 124, 12, size)
    _tar_number_into(header, 136, 12, 0)
    header[156] = ord("0")
    header[257:263] = b"ustar "
    header[263:265] = b" \0"
    checksum = sum(header[:148]) + 8 * ord(" ") + sum(header[156:])
    _tar_octal_into(header, 148, 8, checksum)
    return bytes(header)


def _tar_number(value: bytes) -> int:
    if not value:
        raise InvalidArchive("ARCHIVE_TAR_HEADER_INVALID")
    if value[0] & 0x80:
        mutable = bytearray(value)
        mutable[0] &= 0x7F
        return int.from_bytes(mutable, "big")
    try:
        text = value.rstrip(b"\0 ").lstrip(b" ")
        if not text or any(byte not in b"01234567" for byte in text):
            raise ValueError
        return int(text, 8)
    except ValueError as exc:
        raise InvalidArchive("ARCHIVE_TAR_HEADER_INVALID") from exc


def _parse_canonical_tar_header(header: bytes) -> tuple[str, int]:
    if len(header) != TAR_BLOCK_BYTES:
        raise InvalidArchive("ARCHIVE_TRUNCATED")
    try:
        name_end = header[:100].index(0)
        name = header[:name_end].decode("ascii")
    except (UnicodeDecodeError, ValueError) as exc:
        raise InvalidArchive("ARCHIVE_TAR_HEADER_INVALID") from exc
    size = _tar_number(header[124:136])
    if header != _canonical_tar_header(name, size):
        raise InvalidArchive("ARCHIVE_TAR_HEADER_INVALID")
    return name, size


def _read_exact(stream: Any, size: int) -> bytes:
    output = bytearray()
    while len(output) < size:
        chunk = stream.read(size - len(output))
        if not chunk:
            raise InvalidArchive("ARCHIVE_TRUNCATED")
        output.extend(chunk)
    return bytes(output)


def zstd_decompression_command(config: Config, archive_path: Path) -> list[str]:
    if not sandbox.enabled(config):
        return [config.zstd_bin, "-q", "-d", "--stdout", "--", str(archive_path)]
    return sandbox.command(
        config,
        mounts=((archive_path, "/input/archive.zst", "ro+rprivate"),),
        workdir="/input",
        executable="/bin/sh",
        arguments=(
            "-c",
            (
                f"{sandbox.NATIVE_ZSTD} -q -d --stdout -- /input/archive.zst; "
                "status=$?; "
                f'if [ "$status" -eq 1 ]; then exit {SANDBOX_ZSTD_INVALID_EXIT}; '
                'fi; exit "$status"'
            ),
        ),
    )


def _check_zstd_returncode(config: Config, returncode: int) -> None:
    if returncode == 0:
        return
    if not sandbox.enabled(config) or returncode == SANDBOX_ZSTD_INVALID_EXIT:
        raise InvalidArchive("ARCHIVE_ZSTD_INVALID")
    raise ConverterFailure("ZSTD_SANDBOX_FAILED", retryable=True)


def _functional_epi_headers_confirmed(audits: list[DicomAudit]) -> bool:
    if not audits:
        return False
    strong_sequence_names = {"ep2d_bold", "epfid_bold", "bold", "fmri"}
    echo_planar_sequence_names = {
        "ep2d_bold",
        "epfid_bold",
        "ep2d",
        "epfid",
        "epi",
    }
    negative_contrasts = {
        "T1",
        "T2",
        "DIFFUSION",
        "FLOW_ENCODED",
        "FLUID_ATTENUATED",
        "PERFUSION",
    }
    strong_functional = False
    acquisition_numbers: set[int] = set()
    temporal_positions: set[int] = set()
    declared_temporal_positions: list[int] = []
    repetition_times: list[float] = []
    echo_times: list[float] = []
    for audit in audits:
        if not {"ORIGINAL", "PRIMARY"}.issubset(
            audit.image_type
        ) or audit.image_type & {
            "DERIVED",
            "SECONDARY",
        }:
            return False
        epi_evidence = bool(
            "EP" in audit.scanning_sequence
            or audit.echo_planar_pulse_sequence == "YES"
            or "EPI" in audit.image_type
            or audit.sequence_name in echo_planar_sequence_names
        )
        if not epi_evidence or audit.mr_acquisition_type == "3D":
            return False
        if (
            audit.acquisition_contrast & negative_contrasts
            or (audit.diffusion_b_value is not None and audit.diffusion_b_value > 1.0)
            or audit.asl_technique_present
        ):
            return False
        siemens_measured_functional = bool(
            audit.manufacturer == "SIEMENS"
            and audit.model == "MAGNETOM Prisma_fit"
            and audit.software_versions == {"Siemens E11"}
            and "MOSAIC" in audit.image_type
            and "EP" in audit.scanning_sequence
            and audit.sequence_name == "epfid"
            and audit.mr_acquisition_type == "2D"
            and "siemens_csa_image_header_numeric_v1" in audit.private_exceptions
        )
        strong_functional = strong_functional or bool(
            audit.image_type & {"BOLD", "FMRI"}
            or audit.sequence_name in strong_sequence_names
            or siemens_measured_functional
        )
        if audit.repetition_time_ms is None or not (
            100.0 <= audit.repetition_time_ms <= 20_000.0
        ):
            return False
        if audit.echo_time_ms is None or not 0 < audit.echo_time_ms <= 2_000.0:
            return False
        repetition_times.append(audit.repetition_time_ms)
        echo_times.append(audit.echo_time_ms)
        if audit.acquisition_number is not None:
            acquisition_numbers.add(audit.acquisition_number)
        if audit.temporal_position_identifier is not None:
            temporal_positions.add(audit.temporal_position_identifier)
        if audit.number_of_temporal_positions is not None:
            declared_temporal_positions.append(audit.number_of_temporal_positions)
    if max(repetition_times) - min(repetition_times) > 0.001:
        return False
    if max(echo_times) - min(echo_times) > 0.001:
        return False
    temporal_structure = bool(
        any(value >= 10 for value in declared_temporal_positions)
        or len(temporal_positions) >= 10
        or len(acquisition_numbers) >= 10
    )
    return strong_functional and temporal_structure


def _archive_extraction_contract_bytes(config: Config, archive_bytes: int) -> int:
    return min(
        config.max_archive_uncompressed_bytes,
        max(
            config.archive_expansion_floor_bytes,
            archive_bytes * config.archive_expansion_ratio,
        ),
    )


def _extract_archive(
    config: Config,
    archive_path: Path,
    destination: Path,
    *,
    expected_series_archive_id: str,
    expected_series_id: str,
    expected_dicom_count: int,
) -> ArchiveManifest:
    if not 1 <= expected_dicom_count <= MAX_DICOM_INSTANCES:
        raise InvalidArchive("ARCHIVE_DICOM_COUNT_MISMATCH")
    destination.mkdir(parents=True, exist_ok=True, mode=0o700)
    archive_bytes = archive_path.stat().st_size
    if not 32 <= archive_bytes <= MAX_COMPRESSED_ARCHIVE_BYTES:
        raise InvalidArchive("ARCHIVE_SIZE_INVALID")
    archive_contract_bytes = _archive_extraction_contract_bytes(config, archive_bytes)
    free_bytes = shutil.disk_usage(destination).free
    if free_bytes <= config.disk_reserve_bytes:
        raise CapacityFailure()
    try:
        free_inodes = os.statvfs(destination).f_favail
    except OSError as exc:
        raise CapacityFailure("PROCESSOR_STORAGE_UNAVAILABLE") from exc
    if free_inodes < expected_dicom_count + config.inode_reserve:
        raise CapacityFailure("LOW_INODE_SPACE")
    extraction_capacity_bytes = free_bytes - config.disk_reserve_bytes
    dicom_dir = destination / "dicom"
    dicom_dir.mkdir(mode=0o700)
    extracted: list[tuple[str, int, str, Path]] = []
    total_bytes = 0
    manifest_raw: bytes | None = None
    try:
        process = subprocess.Popen(
            zstd_decompression_command(config, archive_path),
            stdin=subprocess.DEVNULL,
            stdout=subprocess.PIPE,
            stderr=subprocess.DEVNULL,
            env=sandbox.subprocess_environment(config, destination),
        )
    except (FileNotFoundError, OSError) as exc:
        raise ConverterFailure("ZSTD_UNAVAILABLE", retryable=True) from exc
    assert process.stdout is not None
    extraction_timed_out = Event()

    def terminate_slow_extraction() -> None:
        extraction_timed_out.set()
        try:
            process.kill()
        except OSError:
            pass

    extraction_seconds = min(
        MAX_EXTRACTION_SECONDS,
        max(
            MIN_EXTRACTION_SECONDS,
            archive_contract_bytes // MIN_EXTRACTION_BYTES_PER_SECOND + 5 * 60,
        ),
    )
    extraction_timer = Timer(extraction_seconds, terminate_slow_extraction)
    extraction_timer.daemon = True
    extraction_timer.start()
    try:
        while True:
            header = _read_exact(process.stdout, TAR_BLOCK_BYTES)
            if header == b"\0" * TAR_BLOCK_BYTES:
                if (
                    _read_exact(process.stdout, TAR_BLOCK_BYTES)
                    != b"\0" * TAR_BLOCK_BYTES
                ):
                    raise InvalidArchive("ARCHIVE_TRAILING_DATA")
                if process.stdout.read(1):
                    raise InvalidArchive("ARCHIVE_TRAILING_DATA")
                break
            if manifest_raw is not None:
                raise InvalidArchive("ARCHIVE_MANIFEST_NOT_LAST")
            name, member_size = _parse_canonical_tar_header(header)
            if name == "manifest.json":
                if member_size > MAX_MANIFEST_BYTES:
                    raise InvalidArchive("ARCHIVE_MANIFEST_TOO_LARGE")
                manifest_raw = _read_exact(process.stdout, member_size)
            else:
                match = DICOM_PATH_RE.fullmatch(name)
                expected_index = len(extracted) + 1
                if not match or int(match.group(1)) != expected_index:
                    raise InvalidArchive("ARCHIVE_INSTANCE_ORDER")
                if expected_index > expected_dicom_count:
                    raise InvalidArchive("ARCHIVE_DICOM_COUNT_MISMATCH")
                if member_size < 1 or member_size > MAX_DICOM_BYTES:
                    raise InvalidArchive("ARCHIVE_DICOM_SIZE_INVALID")
                total_bytes += member_size
                if total_bytes > archive_contract_bytes:
                    raise InvalidArchive("ARCHIVE_UNCOMPRESSED_LIMIT")
                if total_bytes > extraction_capacity_bytes:
                    raise CapacityFailure()
                path = dicom_dir / f"{expected_index:06d}.dcm"
                digest = hashlib.sha256()
                remaining = member_size
                with path.open("xb") as output:
                    path.chmod(0o600)
                    while remaining:
                        chunk = _read_exact(process.stdout, min(8 * 1024**2, remaining))
                        remaining -= len(chunk)
                        digest.update(chunk)
                        output.write(chunk)
                extracted.append((name, member_size, digest.hexdigest(), path))
            padding = (-member_size) % TAR_BLOCK_BYTES
            if padding and any(_read_exact(process.stdout, padding)):
                raise InvalidArchive("ARCHIVE_TAR_PADDING_INVALID")
        process.stdout.close()
        try:
            completed_returncode = process.wait(timeout=30)
        except subprocess.TimeoutExpired as exc:
            extraction_timer.cancel()
            process.kill()
            process.wait()
            raise ConverterFailure("ZSTD_SANDBOX_TIMEOUT", retryable=True) from exc
        extraction_timer.cancel()
        if extraction_timed_out.is_set():
            raise InvalidArchive("ARCHIVE_EXTRACTION_TIMEOUT")
        _check_zstd_returncode(config, completed_returncode)
    except InvalidArchive as error:
        extraction_timer.cancel()
        process.stdout.close()
        if extraction_timed_out.is_set():
            process.kill()
            process.wait()
            raise InvalidArchive("ARCHIVE_EXTRACTION_TIMEOUT") from error
        if error.code == "ARCHIVE_TRUNCATED":
            try:
                polled_returncode = process.wait(timeout=5)
            except subprocess.TimeoutExpired:
                polled_returncode = None
        else:
            polled_returncode = process.poll()
        if polled_returncode is not None:
            try:
                _check_zstd_returncode(config, polled_returncode)
            except (InvalidArchive, ConverterFailure) as process_error:
                raise process_error from error
        else:
            process.kill()
            process.wait()
        raise
    except BaseException:
        extraction_timer.cancel()
        process.stdout.close()
        process.kill()
        process.wait()
        raise
    if manifest_raw is None:
        raise InvalidArchive("ARCHIVE_MANIFEST_MISSING")
    manifest = validate_manifest(
        manifest_raw,
        expected_series_archive_id=expected_series_archive_id,
        expected_series_id=expected_series_id,
        expected_dicom_count=expected_dicom_count,
    )
    if len(extracted) != manifest.dicom_count:
        raise InvalidArchive("ARCHIVE_DICOM_COUNT_MISMATCH")
    burned_in_declared: list[bool] = []
    dicom_audits: list[DicomAudit] = []
    observed_private_exceptions: set[str] = set()
    trigger_time_present = False
    for extracted_item, declared in zip(
        extracted, manifest.value["instances"], strict=True
    ):
        name, size, file_sha256, path = extracted_item
        if (
            name != declared["path"]
            or size != declared["size_bytes"]
            or file_sha256 != declared["sha256"]
        ):
            raise InvalidArchive("ARCHIVE_INSTANCE_MISMATCH")
        audit = audit_dicom(
            path,
            expected_subject_id=manifest.value["subject_id"],
        )
        if audit.sop_instance_uid != declared["sop_instance_uid"]:
            raise InvalidArchive("ARCHIVE_SOP_UID_MISMATCH")
        source_manufacturer = manifest.value["source"]["manufacturer"]
        if audit.manufacturer != source_manufacturer:
            raise InvalidArchive("ARCHIVE_MANUFACTURER_MISMATCH")
        if audit.model != manifest.value["source"][
            "model"
        ] or audit.software_versions != frozenset(
            manifest.value["source"]["software_versions"]
        ):
            raise InvalidArchive("ARCHIVE_SCANNER_METADATA_MISMATCH")
        if audit.sop_class_uid != "1.2.840.10008.5.1.4.1.1.4":
            raise InvalidArchive("ARCHIVE_UNSUPPORTED_DICOM_FORM")
        if audit.manufacturer == "SIEMENS":
            if audit.model != "MAGNETOM Prisma_fit" or audit.software_versions != {
                "Siemens E11"
            }:
                raise InvalidArchive("ARCHIVE_UNVERIFIED_SCANNER_FAMILY")
            if any(
                exception.startswith("dicom_ps3.15_philips_")
                for exception in audit.private_exceptions
            ):
                raise InvalidArchive("ARCHIVE_VENDOR_METADATA_MISMATCH")
            if (
                "MOSAIC" not in audit.image_type
                or "siemens_csa_image_header_numeric_v1" not in audit.private_exceptions
            ):
                raise InvalidArchive("ARCHIVE_SIEMENS_CSA_REQUIRED")
        else:
            if (
                audit.model != "Achieva dStream"
                or "Philips 5.1.1" not in audit.software_versions
                or any(
                    version not in {"Philips 5.1.1", "Philips 5.1.1.0"}
                    for version in audit.software_versions
                )
            ):
                raise InvalidArchive("ARCHIVE_UNVERIFIED_SCANNER_FAMILY")
            if any(
                exception == "siemens_csa_image_header_numeric_v1"
                for exception in audit.private_exceptions
            ):
                raise InvalidArchive("ARCHIVE_VENDOR_METADATA_MISMATCH")
            if not PHILIPS_REQUIRED_PRIVATE_FIELDS.issubset(
                audit.philips_private_fields
            ):
                raise InvalidArchive("ARCHIVE_PHILIPS_PRIVATE_METADATA_REQUIRED")
        dicom_audits.append(audit)
        burned_in_declared.append(audit.burned_in_annotation_declared_no)
        observed_private_exceptions.update(audit.private_exceptions)
        trigger_time_present = trigger_time_present or audit.trigger_time_present
    if not _functional_epi_headers_confirmed(dicom_audits):
        raise InvalidArchive("FUNCTIONAL_EPI_NOT_CONFIRMED")
    burned_in_status = manifest.value["deidentification"]["burned_in_annotation_status"]
    if (burned_in_status == "verified_no" and not all(burned_in_declared)) or (
        burned_in_status == "not_declared" and all(burned_in_declared)
    ):
        raise InvalidArchive("ARCHIVE_DEIDENTIFICATION_UNVERIFIED")
    if observed_private_exceptions != set(
        manifest.value["deidentification"].get("safe_private_exceptions", [])
    ):
        raise InvalidArchive("ARCHIVE_DEIDENTIFICATION_UNVERIFIED")
    if "suppressed_redundant_philips_dynamic_trigger_time" in manifest.value[
        "deidentification"
    ].get("metadata_transformations", []) and (
        manifest.value["source"]["manufacturer"] != "Philips Medical Systems"
        or trigger_time_present
    ):
        raise InvalidArchive("ARCHIVE_DEIDENTIFICATION_UNVERIFIED")
    return ArchiveManifest(
        value=manifest.value,
        sha256=manifest.sha256,
        extracted_bytes=total_bytes,
    )


def extract_archive(
    config: Config,
    archive_path: Path,
    destination: Path,
    *,
    expected_series_archive_id: str,
    expected_series_id: str,
    expected_dicom_count: int,
) -> ArchiveManifest:
    """Extract one archive and leave no partial stage after a failed attempt."""
    try:
        return _extract_archive(
            config,
            archive_path,
            destination,
            expected_series_archive_id=expected_series_archive_id,
            expected_series_id=expected_series_id,
            expected_dicom_count=expected_dicom_count,
        )
    except OSError as exc:
        shutil.rmtree(destination, ignore_errors=True)
        raise CapacityFailure("PROCESSOR_STORAGE_UNAVAILABLE") from exc
    except BaseException:
        shutil.rmtree(destination, ignore_errors=True)
        raise
