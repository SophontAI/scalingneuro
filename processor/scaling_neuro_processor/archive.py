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
from threading import Event, Thread, Timer
from typing import Any, Mapping

from .config import Config
from .dicom_privacy import (
    DicomAudit,
    SAFE_PRIVATE_EXCEPTION_ORDER,
    SAFE_PRIVATE_EXCEPTIONS,
    audit_dicom,
    safe_scanner_text,
)
from .errors import CapacityFailure, ConverterFailure, InvalidArchive, LeaseLost
from .models import PIXEL_DATA_POLICY, PROCESSING_ROUTES, SERIES_KINDS
from . import sandbox


DICOM_PATH_RE = re.compile(r"^dicom/([0-9]{6})\.dcm$")
PSEUDONYM_RE = re.compile(r"^[0-9a-f]{24}$")
UID_RE = re.compile(r"^[0-9]+(?:\.[0-9]+)+$")
SEMVER_RE = re.compile(
    r"^(?:0|[1-9][0-9]*)\.(?:0|[1-9][0-9]*)\.(?:0|[1-9][0-9]*)(?:[-+][A-Za-z0-9.-]+)?$"
)
CANONICAL_COIL_RE = re.compile(
    r"^(?:MULTI_COIL|SURFACE|HEAD(?:_NECK)?|NECK|BODY|SPINE|KNEE|FLEX|BREAST|CARDIAC|FOOT|ANKLE|SHOULDER|WRIST)"
    r"(?:_(?:[1-9]|[1-9][0-9]|1[0-9]{2}|2[0-4][0-9]|25[0-6]))?$"
)
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
    "mprage",
    "flair",
    "bravo",
    "spgr",
    "space",
    "diffusion",
    "pcasl",
    "pasl",
    "fieldmap",
}
LEGACY_FUNCTIONAL_EVIDENCE = {
    "echo_planar_pulse_sequence",
    "echo_planar_scanning_sequence",
    "functional_image_type",
    "echo_planar_sequence",
    "functional_protocol_label",
    "functional_tr_range",
    "multiple_temporal_positions",
}
CLASSIFICATION_EVIDENCE_EFFECTS = {
    **{code: frozenset({"supports"}) for code in LEGACY_FUNCTIONAL_EVIDENCE},
    "functional_epi_confirmed": frozenset({"supports"}),
    "diffusion_detected": frozenset({"supports"}),
    "diffusion_scientific_metadata_contract_verified": frozenset({"supports"}),
    "asl_or_perfusion_detected": frozenset({"supports"}),
    "asl_scientific_metadata_contract_verified": frozenset({"supports"}),
    "perfusion_detected": frozenset({"supports"}),
    "fieldmap_detected": frozenset({"supports"}),
    "sbref_detected": frozenset({"supports"}),
    "localizer_detected": frozenset({"supports"}),
    "structural_t1w_detected": frozenset({"supports"}),
    "structural_t2w_detected": frozenset({"supports"}),
    "structural_detected": frozenset({"supports"}),
    "derived_or_secondary": frozenset({"supports"}),
    "supported_mr_image": frozenset({"supports"}),
    "missing_tr_in_series_instance": frozenset({"limits_processing"}),
    "missing_te_in_series_instance": frozenset({"limits_processing"}),
    "tr_out_of_range_in_series_instance": frozenset({"limits_processing"}),
    "te_out_of_range_in_series_instance": frozenset({"limits_processing"}),
    "tr_inconsistent_across_series_instances": frozenset({"limits_processing"}),
    "philips_private_metadata_dropped_public_pixel_scaling_retained": frozenset(
        {"limits_processing"}
    ),
}
CLASSIFICATION_EVIDENCE = frozenset(CLASSIFICATION_EVIDENCE_EFFECTS)
METADATA_TRANSFORMATION_ORDER = (
    "replaced_unknown_classic_image_type_components_with_other",
    "suppressed_redundant_philips_dynamic_trigger_time",
    "emptied_asl_technique_description",
    "redacted_asl_crusher_description",
    "emptied_asl_bolus_cutoff_technique",
)
PRIVATE_EXCEPTION_MANUFACTURERS = {
    "siemens_csa_image_header_numeric_v1": "SIEMENS",
    "dicom_ps3.15_siemens_mr_header_diffusion": "SIEMENS",
    "dicom_ps3.15_philips_diffusion": "Philips Medical Systems",
    "dicom_ps3.15_philips_phase_number": "Philips Medical Systems",
    "dicom_ps3.15_ge_diffusion_b_value": "GE MEDICAL SYSTEMS",
    "uih_image_private_header_grid_slice_count_numeric_v1": "United Imaging",
    "uih_image_private_header_diffusion_numeric_v1": "United Imaging",
    "philips_mr_imaging_dd_001_diffusion_gradient_vector_numeric_v1": "Philips Medical Systems",
    "philips_mr_imaging_dd_005_diffusion_indices_numeric_v1": "Philips Medical Systems",
    "philips_mr_imaging_dd_005_asl_label_code_v1": "Philips Medical Systems",
    "ge_gems_acqu_01_diffusion_gradient_vector_numeric_v1": "GE MEDICAL SYSTEMS",
    "ge_gems_parm_01_asl_technique_duration_v1": "GE MEDICAL SYSTEMS",
    "dicom_ps3.15_philips_scale_intercept_slope": "Philips Medical Systems",
    "dicom_ps3.15_philips_number_of_slices": "Philips Medical Systems",
    "dicom_ps3.15_philips_water_fat_shift": "Philips Medical Systems",
    "dicom_ps3.15_philips_per_frame_scale_slope": "Philips Medical Systems",
}


def _scanner_vendor_family(value: str) -> str | None:
    upper = " ".join(value.split()).upper()
    if upper == "SIEMENS" or upper.startswith("SIEMENS "):
        return "SIEMENS"
    if upper == "PHILIPS" or upper.startswith("PHILIPS "):
        return "Philips Medical Systems"
    if upper == "GE" or upper.startswith(("GE ", "GENERAL ELECTRIC")):
        return "GE MEDICAL SYSTEMS"
    if "CANON" in upper or "TOSHIBA" in upper:
        return "Canon/Toshiba"
    if upper == "UIH" or "UNITEDIMAGING" in upper or "UNITED IMAGING" in upper:
        return "United Imaging"
    if "BRUKER" in upper:
        return "Bruker"
    return None


MAX_MANIFEST_BYTES = 128 * 1024**2
MAX_DICOM_BYTES = 64 * 1024**3
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
    functional_epi_headers_confirmed: bool = False

    @property
    def dicom_count(self) -> int:
        return self.value["dicom_instance_count"]

    @property
    def series_kind(self) -> str:
        return self.value.get("series_kind", "functional_epi")

    @property
    def processing_route(self) -> str:
        return self.value.get("processing_route", "functional-epi-v1")

    @property
    def pixel_data_policy(self) -> str:
        return self.value.get("pixel_data_policy", PIXEL_DATA_POLICY)

    @property
    def deidentification_policy_version(self) -> str:
        return self.value["deidentification"]["policy_version"]


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
    required = {"dicom_count"}
    optional = {
        "manufacturer",
        "model",
        "software_versions",
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
                "DERIVED",
                "PRIMARY",
                "SECONDARY",
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
                "GRID",
                "VFRAME",
                "DIS2D",
                "FMRI",
                "BOLD",
                "EPI",
                "T1",
                "T1W",
                "T2",
                "T2W",
                "T2_STAR",
                "T2STAR",
                "FLAIR",
                "DIFFUSION",
                "DWI",
                "ADC",
                "TRACEW",
                "FA",
                "DTI",
                "ASL",
                "PERFUSION",
                "FIELD_MAP",
                "FIELDMAP",
                "PHASEDIFF",
                "SBREF",
                "LOCALIZER",
                "SCOUT",
                "SURVEY",
                "REF",
                "REFERENCE",
                "NONE",
            },
            48,
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
    if "software_versions" in source:
        versions = source["software_versions"]
        if (
            not isinstance(versions, list)
            or not 1 <= len(versions) <= 16
            or any(not isinstance(item, str) for item in versions)
            or len(versions) != len(set(versions))
            or any(not safe_scanner_text(item) for item in versions)
        ):
            raise InvalidArchive("ARCHIVE_MANIFEST_SCHEMA")
    if "manufacturer" in source and (
        not isinstance(source["manufacturer"], str)
        or not safe_scanner_text(source["manufacturer"])
    ):
        raise InvalidArchive("ARCHIVE_MANIFEST_SCHEMA")
    if "model" in source and (
        not isinstance(source["model"], str) or not safe_scanner_text(source["model"])
    ):
        raise InvalidArchive("ARCHIVE_MANIFEST_SCHEMA")
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
    expected_series_kind: str = "functional_epi",
    expected_processing_route: str = "functional-epi-v1",
    expected_pixel_data_policy: str = PIXEL_DATA_POLICY,
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
    schema_version = value.get("schema_version")
    required = {
        "schema_version",
        "series_archive_id",
        "series_id",
        "subject_id",
        "session_id",
        "protocol_group_id",
        "modality",
        "dicom_instance_count",
        "deidentification",
        "source",
        "classification",
        "instances",
    }
    if schema_version == "1.0.0":
        required.add("client")
    if schema_version == "2.0.0":
        required.update(
            {"series_kind", "processing_route", "pixel_data_policy", "writer_contract"}
        )
    _exact_keys(
        value,
        required,
    )
    if schema_version == "1.0.0":
        if (
            value["modality"] != "functional_epi"
            or expected_series_kind != "functional_epi"
            or expected_processing_route != "functional-epi-v1"
            or expected_pixel_data_policy != PIXEL_DATA_POLICY
        ):
            raise InvalidArchive("ARCHIVE_PROCESSING_ROUTE_MISMATCH")
        manifest_series_kind = "functional_epi"
        manifest_processing_route = "functional-epi-v1"
        manifest_pixel_data_policy = PIXEL_DATA_POLICY
    elif schema_version == "2.0.0":
        manifest_series_kind = value["series_kind"]
        manifest_processing_route = value["processing_route"]
        manifest_pixel_data_policy = value["pixel_data_policy"]
        if (
            value["modality"] != "mr"
            or manifest_series_kind not in SERIES_KINDS
            or manifest_processing_route not in PROCESSING_ROUTES
            or manifest_pixel_data_policy != PIXEL_DATA_POLICY
        ):
            raise InvalidArchive("ARCHIVE_MANIFEST_SCHEMA")
        if (
            manifest_series_kind != expected_series_kind
            or manifest_processing_route != expected_processing_route
            or manifest_pixel_data_policy != expected_pixel_data_policy
            or (manifest_series_kind == "functional_epi")
            != (manifest_processing_route == "functional-epi-v1")
        ):
            raise InvalidArchive("ARCHIVE_PROCESSING_ROUTE_MISMATCH")
    else:
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

    writer_contract = (
        value["client"] if schema_version == "1.0.0" else value["writer_contract"]
    )
    if not isinstance(writer_contract, dict):
        raise InvalidArchive("ARCHIVE_MANIFEST_SCHEMA")
    _exact_keys(writer_contract, {"name", "version"})
    if (
        writer_contract["name"] != "neuro-sync"
        or not isinstance(writer_contract["version"], str)
        or not SEMVER_RE.fullmatch(writer_contract["version"])
        or (schema_version == "2.0.0" and writer_contract["version"] != "2.0.0")
    ):
        raise InvalidArchive("ARCHIVE_MANIFEST_SCHEMA")

    deid = value["deidentification"]
    if not isinstance(deid, dict):
        raise InvalidArchive("ARCHIVE_MANIFEST_SCHEMA")
    deidentification_required = {
        "policy_id",
        "policy_version",
        "method",
        "recursive",
        "private_text_removed",
        "unknown_private_removed",
        "uids_remapped",
        "pixel_data_retained",
        "burned_in_annotation_status",
    }
    if schema_version == "2.0.0":
        deidentification_required.update(
            {"defacing_performed", "recognizable_visual_features"}
        )
    _exact_keys(
        deid,
        deidentification_required,
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
    expected_deidentification = (
        ("1.0.0", "scaling-neuro-recursive-allowlist-v1")
        if schema_version == "1.0.0"
        else ("2.0.0", "scaling-neuro-recursive-allowlist-v2")
    )
    if (
        deid["policy_id"] != "scaling-neuro.dicom-deidentification"
        or (deid["policy_version"], deid["method"]) != expected_deidentification
        or not isinstance(deid["burned_in_annotation_status"], str)
        or deid["burned_in_annotation_status"] not in {"verified_no", "not_declared"}
    ):
        raise InvalidArchive("ARCHIVE_DEIDENTIFICATION_POLICY_MISMATCH")
    if schema_version == "2.0.0" and (
        deid["defacing_performed"] is not False
        or deid["recognizable_visual_features"] != "may_be_present"
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
    if "metadata_transformations" in deid and (
        not isinstance(metadata_transformations, list)
        or not metadata_transformations
        or any(not isinstance(item, str) for item in metadata_transformations)
        or len(metadata_transformations) != len(set(metadata_transformations))
        or metadata_transformations
        != [
            item
            for item in METADATA_TRANSFORMATION_ORDER
            if item in metadata_transformations
        ]
    ):
        raise InvalidArchive("ARCHIVE_DEIDENTIFICATION_POLICY_MISMATCH")

    _validate_source(value["source"], count)
    classification = value["classification"]
    if not isinstance(classification, dict):
        raise InvalidArchive("ARCHIVE_MANIFEST_SCHEMA")
    _exact_keys(classification, {"decision", "kind", "confidence", "evidence"})
    if classification["decision"] != "accepted":
        raise InvalidArchive("ARCHIVE_MANIFEST_SCHEMA")
    if classification["kind"] != manifest_series_kind:
        raise InvalidArchive("ARCHIVE_PROCESSING_ROUTE_MISMATCH")
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
            or code
            not in (
                LEGACY_FUNCTIONAL_EVIDENCE
                if schema_version == "1.0.0"
                else CLASSIFICATION_EVIDENCE
            )
            or code in observed_evidence
        ):
            raise InvalidArchive("ARCHIVE_MANIFEST_SCHEMA")
        observed_evidence.add(code)
        expected_effects = (
            frozenset({"supports"})
            if schema_version == "1.0.0"
            else CLASSIFICATION_EVIDENCE_EFFECTS[code]
        )
        if item["source"] != "dicom_header" or item["effect"] not in expected_effects:
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
    acquisition_numbers: set[int] = set()
    temporal_positions: set[int] = set()
    declared_temporal_positions: list[int] = []
    image_positions: set[tuple[float, float, float]] = set()
    image_position_count = 0
    mosaic_instances = 0
    repetition_times: list[float] = []
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
            or audit.diffusion_semantic_evidence
            or audit.asl_metadata_present
            or audit.asl_technique_present
        ):
            return False
        if audit.repetition_time_ms is None or not (
            100.0 <= audit.repetition_time_ms <= 20_000.0
        ):
            return False
        if not audit.echo_times_ms or any(
            not 0 < value <= 2_000.0 for value in audit.echo_times_ms
        ):
            return False
        repetition_times.append(audit.repetition_time_ms)
        if audit.acquisition_number is not None:
            acquisition_numbers.add(audit.acquisition_number)
        if audit.temporal_position_identifier is not None:
            temporal_positions.add(audit.temporal_position_identifier)
        temporal_positions.update(audit.temporal_position_indices)
        if audit.number_of_temporal_positions is not None:
            declared_temporal_positions.append(audit.number_of_temporal_positions)
        image_positions.update(audit.image_positions)
        image_position_count += audit.image_position_count
        if "MOSAIC" in audit.image_type:
            mosaic_instances += 1
    if max(repetition_times) - min(repetition_times) > 0.001:
        return False
    temporal_structure = bool(
        any(value >= 2 for value in declared_temporal_positions)
        or len(temporal_positions) >= 2
        or len(acquisition_numbers) >= 2
        or mosaic_instances >= 2
        or image_positions
        and image_position_count // len(image_positions) >= 2
    )
    return temporal_structure


def _archive_extraction_contract_bytes(config: Config, archive_bytes: int) -> int:
    return min(
        config.max_archive_uncompressed_bytes,
        max(
            config.archive_expansion_floor_bytes,
            archive_bytes * config.archive_expansion_ratio,
        ),
    )


EXTRACTION_LEASE_POLL_SECONDS = 0.05


def _require_active_lease(lease_active: Event) -> None:
    if not lease_active.is_set():
        raise LeaseLost()


def _terminate_process(process: subprocess.Popen[bytes]) -> None:
    try:
        if process.poll() is None:
            process.kill()
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


def _extract_archive(
    config: Config,
    archive_path: Path,
    destination: Path,
    *,
    expected_series_archive_id: str,
    expected_series_id: str,
    expected_dicom_count: int,
    expected_series_kind: str,
    expected_processing_route: str,
    expected_pixel_data_policy: str,
    lease_active: Event,
) -> ArchiveManifest:
    _require_active_lease(lease_active)
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
    lease_cancelled = Event()
    lease_monitor_stop = Event()

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

    def terminate_after_lease_loss() -> None:
        while not lease_monitor_stop.is_set():
            if not lease_active.is_set():
                lease_cancelled.set()
                try:
                    process.kill()
                except OSError:
                    pass
                return
            if lease_monitor_stop.wait(EXTRACTION_LEASE_POLL_SECONDS):
                return

    lease_monitor = Thread(
        target=terminate_after_lease_loss,
        name="archive-lease-monitor",
        daemon=True,
    )
    lease_monitor.start()
    try:
        while True:
            _require_active_lease(lease_active)
            header = _read_exact(process.stdout, TAR_BLOCK_BYTES)
            _require_active_lease(lease_active)
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
                        _require_active_lease(lease_active)
                        chunk = _read_exact(process.stdout, min(8 * 1024**2, remaining))
                        _require_active_lease(lease_active)
                        remaining -= len(chunk)
                        digest.update(chunk)
                        output.write(chunk)
                extracted.append((name, member_size, digest.hexdigest(), path))
            padding = (-member_size) % TAR_BLOCK_BYTES
            if padding and any(_read_exact(process.stdout, padding)):
                raise InvalidArchive("ARCHIVE_TAR_PADDING_INVALID")
        _require_active_lease(lease_active)
        process.stdout.close()
        try:
            completed_returncode = process.wait(timeout=30)
        except subprocess.TimeoutExpired as exc:
            extraction_timer.cancel()
            _terminate_process(process)
            if lease_cancelled.is_set() or not lease_active.is_set():
                raise LeaseLost() from exc
            raise ConverterFailure("ZSTD_SANDBOX_TIMEOUT", retryable=True) from exc
        extraction_timer.cancel()
        _require_active_lease(lease_active)
        if extraction_timed_out.is_set():
            raise ConverterFailure("ARCHIVE_EXTRACTION_TIMEOUT", retryable=True)
        _check_zstd_returncode(config, completed_returncode)
    except InvalidArchive as error:
        extraction_timer.cancel()
        process.stdout.close()
        if lease_cancelled.is_set() or not lease_active.is_set():
            _terminate_process(process)
            raise LeaseLost() from error
        if extraction_timed_out.is_set():
            _terminate_process(process)
            raise ConverterFailure(
                "ARCHIVE_EXTRACTION_TIMEOUT", retryable=True
            ) from error
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
            _terminate_process(process)
        raise
    except BaseException:
        extraction_timer.cancel()
        process.stdout.close()
        _terminate_process(process)
        raise
    finally:
        lease_monitor_stop.set()
        lease_monitor.join(timeout=1)
    _require_active_lease(lease_active)
    if manifest_raw is None:
        raise InvalidArchive("ARCHIVE_MANIFEST_MISSING")
    manifest = validate_manifest(
        manifest_raw,
        expected_series_archive_id=expected_series_archive_id,
        expected_series_id=expected_series_id,
        expected_dicom_count=expected_dicom_count,
        expected_series_kind=expected_series_kind,
        expected_processing_route=expected_processing_route,
        expected_pixel_data_policy=expected_pixel_data_policy,
    )
    _require_active_lease(lease_active)
    if len(extracted) != manifest.dicom_count:
        raise InvalidArchive("ARCHIVE_DICOM_COUNT_MISMATCH")
    burned_in_declared: list[bool] = []
    dicom_audits: list[DicomAudit] = []
    observed_private_exceptions: set[str] = set()
    trigger_time_present = False
    asl_technique_descriptions_emptied = 0
    asl_crusher_descriptions_redacted = 0
    asl_bolus_cutoff_techniques_emptied = 0
    for extracted_item, declared in zip(
        extracted, manifest.value["instances"], strict=True
    ):
        _require_active_lease(lease_active)
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
            expected_deidentification_policy_version=(
                manifest.deidentification_policy_version
            ),
        )
        if audit.sop_instance_uid != declared["sop_instance_uid"]:
            raise InvalidArchive("ARCHIVE_SOP_UID_MISMATCH")
        source_manufacturer = manifest.value["source"].get("manufacturer")
        if (
            source_manufacturer is not None
            and audit.manufacturer is not None
            and audit.manufacturer != source_manufacturer
        ):
            raise InvalidArchive("ARCHIVE_MANUFACTURER_MISMATCH")
        source_model = manifest.value["source"].get("model")
        source_versions = manifest.value["source"].get("software_versions")
        if (
            source_model is not None
            and audit.model is not None
            and audit.model != source_model
            or source_versions is not None
            and audit.software_versions
            and audit.software_versions != frozenset(source_versions)
        ):
            raise InvalidArchive("ARCHIVE_SCANNER_METADATA_MISMATCH")
        source = manifest.value["source"]
        scalar_acquisition_fields = (
            ("patient_position", audit.patient_position),
            ("receive_coil_name", audit.receive_coil_name),
            ("transmit_coil_name", audit.transmit_coil_name),
            ("sequence_name", audit.sequence_name),
            ("mr_acquisition_type", audit.mr_acquisition_type),
            ("series_number", audit.series_number),
        )
        if any(
            source.get(name) is not None
            and observed is not None
            and source[name] != observed
            for name, observed in scalar_acquisition_fields
        ):
            raise InvalidArchive("ARCHIVE_ACQUISITION_METADATA_MISMATCH")
        source_field_strength = source.get("magnetic_field_strength")
        if (
            source_field_strength is not None
            and audit.magnetic_field_strength is not None
            and not math.isclose(
                float(source_field_strength),
                audit.magnetic_field_strength,
                rel_tol=1.0e-6,
                abs_tol=1.0e-6,
            )
        ):
            raise InvalidArchive("ARCHIVE_ACQUISITION_METADATA_MISMATCH")
        set_acquisition_fields = (
            ("scanning_sequence", audit.scanning_sequence),
            ("sequence_variant", audit.sequence_variant),
            ("scan_options", audit.scan_options),
        )
        if any(
            source.get(name) is not None
            and observed
            and frozenset(source[name]) != observed
            for name, observed in set_acquisition_fields
        ):
            raise InvalidArchive("ARCHIVE_ACQUISITION_METADATA_MISMATCH")
        if audit.sop_class_uid not in {
            "1.2.840.10008.5.1.4.1.1.4",
            "1.2.840.10008.5.1.4.1.1.4.1",
            "1.2.840.10008.5.1.4.1.1.4.4",
        }:
            raise InvalidArchive("ARCHIVE_UNSUPPORTED_DICOM_FORM")
        private_vendor_families = {
            PRIVATE_EXCEPTION_MANUFACTURERS.get(exception, "unsupported")
            for exception in audit.private_exceptions
        }
        if "unsupported" in private_vendor_families:
            raise InvalidArchive("ARCHIVE_VENDOR_METADATA_MISMATCH")
        audited_vendor_family = (
            _scanner_vendor_family(audit.manufacturer)
            if audit.manufacturer is not None
            else None
        )
        if audit.manufacturer is not None and (
            private_vendor_families
            and (
                audited_vendor_family is None
                or private_vendor_families - {audited_vendor_family}
            )
        ):
            raise InvalidArchive("ARCHIVE_VENDOR_METADATA_MISMATCH")
        if audit.manufacturer is None and len(private_vendor_families) > 1:
            raise InvalidArchive("ARCHIVE_VENDOR_METADATA_MISMATCH")
        if (
            "MOSAIC" in audit.image_type
            and "siemens_csa_image_header_numeric_v1" not in audit.private_exceptions
        ):
            raise InvalidArchive("ARCHIVE_MOSAIC_CSA_REQUIRED")
        if audit.image_type & {"GRID", "VFRAME"} and (
            "uih_image_private_header_grid_slice_count_numeric_v1"
            not in audit.private_exceptions
        ):
            raise InvalidArchive("ARCHIVE_GRID_SLICE_COUNT_REQUIRED")
        dicom_audits.append(audit)
        burned_in_declared.append(audit.burned_in_annotation_declared_no)
        observed_private_exceptions.update(audit.private_exceptions)
        trigger_time_present = trigger_time_present or audit.trigger_time_present
        asl_technique_descriptions_emptied += audit.asl_technique_descriptions_emptied
        asl_crusher_descriptions_redacted += audit.asl_crusher_descriptions_redacted
        asl_bolus_cutoff_techniques_emptied += audit.asl_bolus_cutoff_techniques_emptied
    _require_active_lease(lease_active)
    if (
        len({audit.study_instance_uid for audit in dicom_audits}) != 1
        or len({audit.series_instance_uid for audit in dicom_audits}) != 1
        or len({audit.sop_class_uid for audit in dicom_audits}) != 1
    ):
        raise InvalidArchive("ARCHIVE_SERIES_METADATA_MISMATCH")
    known_manufacturers = {
        audit.manufacturer for audit in dicom_audits if audit.manufacturer is not None
    }
    known_models = {audit.model for audit in dicom_audits if audit.model is not None}
    known_software_versions = {
        audit.software_versions for audit in dicom_audits if audit.software_versions
    }
    archive_private_vendor_families = {
        PRIVATE_EXCEPTION_MANUFACTURERS.get(exception, "unsupported")
        for audit in dicom_audits
        for exception in audit.private_exceptions
    }
    audited_vendor_families = {
        family
        for manufacturer in known_manufacturers
        if (family := _scanner_vendor_family(manufacturer)) is not None
    }
    if (
        len(known_manufacturers) > 1
        or len(known_models) > 1
        or len(known_software_versions) > 1
        or len(archive_private_vendor_families) > 1
        or (
            bool(known_manufacturers)
            and bool(archive_private_vendor_families - audited_vendor_families)
        )
    ):
        raise InvalidArchive("ARCHIVE_SCANNER_METADATA_MISMATCH")
    source = manifest.value["source"]
    observed_manufacturer = (
        next(iter(known_manufacturers), None)
        if all(audit.manufacturer is not None for audit in dicom_audits)
        else None
    )
    observed_model = (
        next(iter(known_models), None)
        if all(audit.model is not None for audit in dicom_audits)
        else None
    )
    observed_versions = (
        next(iter(known_software_versions), frozenset())
        if all(audit.software_versions for audit in dicom_audits)
        else frozenset()
    )
    if source.get("manufacturer") != observed_manufacturer:
        raise InvalidArchive("ARCHIVE_MANUFACTURER_MISMATCH")
    if source.get("model") != observed_model or (
        (source_versions := source.get("software_versions")) is None
        and observed_versions
        or source_versions is not None
        and frozenset(source_versions) != observed_versions
    ):
        raise InvalidArchive("ARCHIVE_SCANNER_METADATA_MISMATCH")
    known_acquisition_values = (
        {audit.patient_position for audit in dicom_audits if audit.patient_position},
        {
            audit.magnetic_field_strength
            for audit in dicom_audits
            if audit.magnetic_field_strength is not None
        },
        {audit.receive_coil_name for audit in dicom_audits if audit.receive_coil_name},
        {
            audit.transmit_coil_name
            for audit in dicom_audits
            if audit.transmit_coil_name
        },
        {audit.sequence_name for audit in dicom_audits if audit.sequence_name},
        {audit.scanning_sequence for audit in dicom_audits if audit.scanning_sequence},
        {audit.sequence_variant for audit in dicom_audits if audit.sequence_variant},
        {audit.scan_options for audit in dicom_audits if audit.scan_options},
        {
            audit.mr_acquisition_type
            for audit in dicom_audits
            if audit.mr_acquisition_type
        },
        {
            audit.series_number
            for audit in dicom_audits
            if audit.series_number is not None
        },
    )
    if any(len(values) > 1 for values in known_acquisition_values):
        raise InvalidArchive("ARCHIVE_ACQUISITION_METADATA_MISMATCH")
    functional_epi_headers_confirmed = _functional_epi_headers_confirmed(dicom_audits)
    evidence_codes = {
        item["code"] for item in manifest.value["classification"]["evidence"]
    }
    derived_headers = any(
        bool(audit.image_type & {"DERIVED", "SECONDARY"}) for audit in dicom_audits
    )
    if manifest.series_kind == "diffusion":
        if (
            derived_headers
            or not all(
                audit.diffusion_metadata_present
                and audit.diffusion_metadata_contract_verified
                for audit in dicom_audits
            )
            or not any(audit.diffusion_semantic_evidence for audit in dicom_audits)
            or not {
                "diffusion_detected",
                "diffusion_scientific_metadata_contract_verified",
            }.issubset(evidence_codes)
            or any(audit.asl_metadata_present for audit in dicom_audits)
            or "asl_scientific_metadata_contract_verified" in evidence_codes
        ):
            raise InvalidArchive("ARCHIVE_SCIENTIFIC_METADATA_UNVERIFIED")
    elif manifest.series_kind == "asl_perfusion":
        if (
            derived_headers
            or not all(
                audit.asl_metadata_present and audit.asl_metadata_contract_verified
                for audit in dicom_audits
            )
            or not {
                "asl_or_perfusion_detected",
                "asl_scientific_metadata_contract_verified",
            }.issubset(evidence_codes)
            or "diffusion_scientific_metadata_contract_verified" in evidence_codes
        ):
            raise InvalidArchive("ARCHIVE_SCIENTIFIC_METADATA_UNVERIFIED")
    elif evidence_codes & {
        "diffusion_scientific_metadata_contract_verified",
        "asl_scientific_metadata_contract_verified",
    }:
        raise InvalidArchive("ARCHIVE_SCIENTIFIC_METADATA_UNVERIFIED")
    elif manifest.series_kind != "derived_mr" and (
        any(audit.diffusion_semantic_evidence for audit in dicom_audits)
        or any(audit.asl_metadata_present for audit in dicom_audits)
    ):
        raise InvalidArchive("ARCHIVE_SCIENTIFIC_METADATA_UNVERIFIED")
    burned_in_status = manifest.value["deidentification"]["burned_in_annotation_status"]
    if (burned_in_status == "verified_no" and not all(burned_in_declared)) or (
        burned_in_status == "not_declared" and all(burned_in_declared)
    ):
        raise InvalidArchive("ARCHIVE_DEIDENTIFICATION_UNVERIFIED")
    if observed_private_exceptions != set(
        manifest.value["deidentification"].get("safe_private_exceptions", [])
    ):
        raise InvalidArchive("ARCHIVE_DEIDENTIFICATION_UNVERIFIED")
    metadata_transformations = manifest.value["deidentification"].get(
        "metadata_transformations", []
    )
    if "replaced_unknown_classic_image_type_components_with_other" in (
        metadata_transformations
    ) and (
        manifest.value["schema_version"] != "2.0.0"
        or not any(
            audit.sop_class_uid == "1.2.840.10008.5.1.4.1.1.4"
            and "OTHER" in audit.image_type
            for audit in dicom_audits
        )
    ):
        raise InvalidArchive("ARCHIVE_DEIDENTIFICATION_UNVERIFIED")
    if "suppressed_redundant_philips_dynamic_trigger_time" in (
        metadata_transformations
    ) and (
        manifest.value["source"].get("manufacturer") != "Philips Medical Systems"
        or manifest.series_kind != "functional_epi"
        or trigger_time_present
    ):
        raise InvalidArchive("ARCHIVE_DEIDENTIFICATION_UNVERIFIED")
    if ("emptied_asl_technique_description" in metadata_transformations) != (
        asl_technique_descriptions_emptied > 0
    ):
        raise InvalidArchive("ARCHIVE_DEIDENTIFICATION_UNVERIFIED")
    if ("redacted_asl_crusher_description" in metadata_transformations) != (
        asl_crusher_descriptions_redacted > 0
    ):
        raise InvalidArchive("ARCHIVE_DEIDENTIFICATION_UNVERIFIED")
    if ("emptied_asl_bolus_cutoff_technique" in metadata_transformations) != (
        asl_bolus_cutoff_techniques_emptied > 0
    ):
        raise InvalidArchive("ARCHIVE_DEIDENTIFICATION_UNVERIFIED")
    return ArchiveManifest(
        value=manifest.value,
        sha256=manifest.sha256,
        extracted_bytes=total_bytes,
        functional_epi_headers_confirmed=functional_epi_headers_confirmed,
    )


def extract_archive(
    config: Config,
    archive_path: Path,
    destination: Path,
    *,
    expected_series_archive_id: str,
    expected_series_id: str,
    expected_dicom_count: int,
    expected_series_kind: str = "functional_epi",
    expected_processing_route: str = "functional-epi-v1",
    expected_pixel_data_policy: str = PIXEL_DATA_POLICY,
    lease_active: Event | None = None,
) -> ArchiveManifest:
    """Extract one archive and leave no partial stage after a failed attempt."""
    if lease_active is None:
        lease_active = Event()
        lease_active.set()
    try:
        return _extract_archive(
            config,
            archive_path,
            destination,
            expected_series_archive_id=expected_series_archive_id,
            expected_series_id=expected_series_id,
            expected_dicom_count=expected_dicom_count,
            expected_series_kind=expected_series_kind,
            expected_processing_route=expected_processing_route,
            expected_pixel_data_policy=expected_pixel_data_policy,
            lease_active=lease_active,
        )
    except OSError as exc:
        shutil.rmtree(destination, ignore_errors=True)
        raise CapacityFailure("PROCESSOR_STORAGE_UNAVAILABLE") from exc
    except BaseException:
        shutil.rmtree(destination, ignore_errors=True)
        raise
