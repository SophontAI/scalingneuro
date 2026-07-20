from __future__ import annotations

import hashlib
import json
import math
from pathlib import Path
import re
from typing import Any

from . import DCM2NIIX_VERSION, __version__
from .archive import (
    ArchiveManifest,
    CANONICAL_COIL_RE,
    PSEUDONYM_RE,
    SEMVER_RE,
)
from .converter import NORMALIZED_ARGUMENTS
from .dicom_privacy import safe_scanner_text
from .errors import InvalidNifti
from .models import OutputFile
from .nifti import NiftiFacts
from .transport import sha256_file


MAX_CONVERTER_JSON_BYTES = 16 * 1024**2
CODE_RE = re.compile(r"^[a-z][a-z0-9_.-]{2,63}$")
SHA256_RE = re.compile(r"^[a-f0-9]{64}$")
SAFE_FILENAME_RE = re.compile(r"^[^/\\\x00-\x1f]{1,180}$")
PUBLIC_SOURCE_KEYS = {
    "dicom_count",
    "manufacturer",
    "model",
    "software_versions",
    "magnetic_field_strength",
    "sequence_name",
    "scanning_sequence",
    "sequence_variant",
    "scan_options",
    "mr_acquisition_type",
    "image_type",
    "series_number",
    "acquisition_number",
    "receive_coil_name",
    "transmit_coil_name",
    "patient_position",
}
PUBLIC_IMAGE_KEYS = {
    "dimensions",
    "voxel_size_mm",
    "datatype",
    "bits_per_voxel",
    "affine",
    "orientation",
    "volume_count",
    "tr_seconds",
    "te_seconds",
    "inversion_time_seconds",
    "flip_angle_degrees",
    "echo_number",
    "slice_timing_seconds",
    "phase_encoding_direction",
    "effective_echo_spacing_seconds",
    "total_readout_time_seconds",
    "dwell_time_seconds",
    "multiband_acceleration_factor",
    "parallel_reduction_factor_in_plane",
    "partial_fourier",
    "echo_train_length",
    "number_of_averages",
    "imaging_frequency_mhz",
    "imaged_nucleus",
    "slice_thickness_mm",
    "spacing_between_slices_mm",
    "pixel_bandwidth_hz",
    "acquisition_matrix",
    "recon_matrix",
}


def canonical_json(value: Any) -> bytes:
    return (
        json.dumps(
            value,
            ensure_ascii=False,
            sort_keys=True,
            separators=(",", ":"),
            allow_nan=False,
        )
        + "\n"
    ).encode("utf-8")


def write_canonical_json(path: Path, value: Any) -> tuple[int, str]:
    raw = canonical_json(value)
    path.write_bytes(raw)
    path.chmod(0o600)
    return len(raw), hashlib.sha256(raw).hexdigest()


def _unique_object(pairs: list[tuple[str, Any]]) -> dict[str, Any]:
    result: dict[str, Any] = {}
    for key, value in pairs:
        if key in result:
            raise InvalidNifti("DCM2NIIX_SIDECAR_INVALID")
        result[key] = value
    return result


def read_converter_sidecar(path: Path) -> dict[str, Any]:
    try:
        raw = path.read_bytes()
        if len(raw) > MAX_CONVERTER_JSON_BYTES:
            raise InvalidNifti("DCM2NIIX_SIDECAR_TOO_LARGE")
        value = json.loads(
            raw.decode("utf-8"),
            object_pairs_hook=_unique_object,
            parse_constant=lambda _: (_ for _ in ()).throw(ValueError()),
        )
    except (OSError, UnicodeDecodeError, ValueError) as exc:
        raise InvalidNifti("DCM2NIIX_SIDECAR_INVALID") from exc
    if not isinstance(value, dict):
        raise InvalidNifti("DCM2NIIX_SIDECAR_INVALID")
    return value


def _number(
    value: Any, *, minimum: float, maximum: float, required: bool = False
) -> float | None:
    if value is None and not required:
        return None
    if (
        isinstance(value, bool)
        or not isinstance(value, (int, float))
        or not math.isfinite(value)
    ):
        raise InvalidNifti("DCM2NIIX_METADATA_INVALID")
    result = float(value)
    if not minimum <= result <= maximum:
        raise InvalidNifti("DCM2NIIX_METADATA_INVALID")
    return result


def _integer(value: Any, *, minimum: int, maximum: int) -> int | None:
    if value is None:
        return None
    if (
        isinstance(value, bool)
        or not isinstance(value, int)
        or not minimum <= value <= maximum
    ):
        raise InvalidNifti("DCM2NIIX_METADATA_INVALID")
    return value


def image_metadata(converter: dict[str, Any], facts: NiftiFacts) -> dict[str, Any]:
    repetition_time = _number(
        converter.get("RepetitionTime"), minimum=0.1, maximum=20, required=True
    )
    assert repetition_time is not None
    if not math.isclose(repetition_time, facts.tr_seconds, rel_tol=1e-5, abs_tol=1e-5):
        raise InvalidNifti("NIFTI_SIDECAR_TR_MISMATCH")
    echo_time = _number(
        converter.get("EchoTime"), minimum=1e-7, maximum=2, required=True
    )
    assert echo_time is not None
    result = facts.image_dict()
    result["te_seconds"] = echo_time
    mappings = {
        "InversionTime": ("inversion_time_seconds", 0.0, 30.0),
        "FlipAngle": ("flip_angle_degrees", 0.0, 360.0),
        "EffectiveEchoSpacing": ("effective_echo_spacing_seconds", 1e-12, 1.0),
        "TotalReadoutTime": ("total_readout_time_seconds", 0.0, 10.0),
        "DwellTime": ("dwell_time_seconds", 1e-12, 1.0),
        "MultibandAccelerationFactor": ("multiband_acceleration_factor", 1.0, 64.0),
        "ParallelReductionFactorInPlane": (
            "parallel_reduction_factor_in_plane",
            1.0,
            64.0,
        ),
        "PartialFourier": ("partial_fourier", 1e-12, 1.0),
        "NumberOfAverages": ("number_of_averages", 1e-12, 1_000_000.0),
        "ImagingFrequency": ("imaging_frequency_mhz", 1e-12, 10_000.0),
        "SliceThickness": ("slice_thickness_mm", 1e-12, 100.0),
        "SpacingBetweenSlices": ("spacing_between_slices_mm", 1e-12, 100.0),
        "PixelBandwidth": ("pixel_bandwidth_hz", 1e-12, 10_000_000.0),
    }
    for source, (destination, minimum, maximum) in mappings.items():
        parsed = _number(converter.get(source), minimum=minimum, maximum=maximum)
        if parsed is not None:
            result[destination] = parsed
    integer_mappings = {
        "EchoNumber": ("echo_number", 1, 1024),
        "EchoTrainLength": ("echo_train_length", 1, 1_000_000),
    }
    for source, (destination, minimum, maximum) in integer_mappings.items():
        parsed = _integer(converter.get(source), minimum=minimum, maximum=maximum)
        if parsed is not None:
            result[destination] = parsed
    phase = converter.get("PhaseEncodingDirection")
    if phase is not None:
        if phase not in {"i", "i-", "j", "j-", "k", "k-"}:
            raise InvalidNifti("DCM2NIIX_METADATA_INVALID")
        result["phase_encoding_direction"] = phase
    nucleus = converter.get("ImagedNucleus")
    if nucleus is not None:
        if nucleus not in {"1H", "13C", "17O", "19F", "23Na", "31P", "129Xe"}:
            raise InvalidNifti("DCM2NIIX_METADATA_INVALID")
        result["imaged_nucleus"] = nucleus
    slice_timing = converter.get("SliceTiming")
    if slice_timing is not None:
        if not isinstance(slice_timing, list) or len(slice_timing) > 4096:
            raise InvalidNifti("DCM2NIIX_METADATA_INVALID")
        parsed_slice_timing = [
            _number(item, minimum=0.0, maximum=60.0, required=True)
            for item in slice_timing
        ]
        result["slice_timing_seconds"] = parsed_slice_timing
    return result


def _canonical_manufacturer(value: Any) -> str | None:
    if not isinstance(value, str):
        return None
    folded = re.sub(r"[^a-z0-9]+", "", value.casefold())
    if "siemens" in folded:
        return "Siemens"
    if "philips" in folded:
        return "Philips"
    if folded in {"ge", "gehealthcare", "gemedicalsystems"}:
        return "GE"
    if "canon" in folded or "toshiba" in folded:
        return "Canon/Toshiba"
    if "unitedimaging" in folded or folded.startswith("uih"):
        return "United Imaging"
    if "bruker" in folded:
        return "Bruker"
    return None


def _canonical_sequence(value: Any) -> str | None:
    if not isinstance(value, str):
        return None
    folded = re.sub(r"[^A-Z0-9]+", "", value.upper())
    if "BOLD" in folded and "EPI" in folded:
        return "BOLD_EPI"
    if "EPFID" in folded:
        return "EPFID"
    if "EP2D" in folded:
        return "EP2D"
    if folded == "EPI" or "EPI" in folded:
        return "EPI"
    return None


def canonical_source_metadata(source: dict[str, Any]) -> dict[str, Any]:
    """Project the raw inventory onto the public default-deny sidecar.

    Vendor names are normalized to broad families when recognized. Safe model
    and software identity text is retained verbatim so new scanners do not
    require a server release before their provenance can be represented.
    """
    result: dict[str, Any] = {"dicom_count": source["dicom_count"]}
    manufacturer = _canonical_manufacturer(source.get("manufacturer"))
    if manufacturer is not None:
        result["manufacturer"] = manufacturer
    model = source.get("model")
    if isinstance(model, str) and safe_scanner_text(model):
        result["model"] = model
    versions = [
        value
        for value in source.get("software_versions", [])
        if isinstance(value, str) and safe_scanner_text(value)
    ]
    if versions:
        result["software_versions"] = sorted(set(versions))[:16]
    sequence_name = _canonical_sequence(source.get("sequence_name"))
    if sequence_name is not None:
        result["sequence_name"] = sequence_name
    for key in (
        "patient_position",
        "magnetic_field_strength",
        "receive_coil_name",
        "transmit_coil_name",
        "scanning_sequence",
        "sequence_variant",
        "scan_options",
        "mr_acquisition_type",
        "image_type",
        "series_number",
        "acquisition_number",
    ):
        if key in source and source[key] not in (None, []):
            result[key] = source[key]
    return result


def build_sidecar(
    manifest: ArchiveManifest,
    converter_sidecar: dict[str, Any],
    facts: NiftiFacts,
    *,
    client_version: str,
    nifti_filename: str,
    nifti_size: int,
    nifti_sha256: str,
) -> dict[str, Any]:
    source = manifest.value
    return {
        "$schema": "https://scalingneuro.com/schemas/scan-sidecar-v1.schema.json",
        "schema_version": "1.0.0",
        "bundle_id": source["series_archive_id"],
        "subject_id": source["subject_id"],
        "session_id": source["session_id"],
        "series_id": source["series_id"],
        "protocol_group_id": source["protocol_group_id"],
        "modality": "bold",
        "source": canonical_source_metadata(source["source"]),
        "image": image_metadata(converter_sidecar, facts),
        "files": {
            "nifti": {
                "filename": nifti_filename,
                "size_bytes": nifti_size,
                "sha256": nifti_sha256,
                "uncompressed_sha256": facts.uncompressed_sha256,
            }
        },
        "conversion": {
            "client_version": client_version,
            "converter": "dcm2niix",
            "converter_version": DCM2NIIX_VERSION,
            "arguments": NORMALIZED_ARGUMENTS,
        },
        "classification": source["classification"],
        "qc": {
            "passed": True,
            "checks": [
                {"code": "archive.instances_verified", "status": "pass"},
                {"code": "dicom.parse_succeeded", "status": "pass"},
                {"code": "nifti.functional_4d", "status": "pass"},
                {"code": "nifti.hash_verified", "status": "pass"},
                {"code": "nifti.signal_finite_nonconstant", "status": "pass"},
                {"code": "metadata.default_deny", "status": "pass"},
            ],
            "warnings": [],
        },
        "metadata_policy": {
            "policy_id": "scaling-neuro-epi-default-deny",
            "policy_version": "1.1.0",
        },
    }


def build_processing_manifest(
    manifest: ArchiveManifest,
    *,
    input_sha256: str,
    outputs: list[OutputFile],
    facts: NiftiFacts,
) -> dict[str, Any]:
    source = manifest.value
    return {
        "schema_version": "scaling-neuro.processing-manifest.v1",
        "series_archive_id": source["series_archive_id"],
        "series_id": source["series_id"],
        "subject_id": source["subject_id"],
        "session_id": source["session_id"],
        "protocol_group_id": source["protocol_group_id"],
        "input": {
            "format": "dicom-tar-zstd",
            "sha256": input_sha256,
            "dicom_count": source["dicom_instance_count"],
            "internal_manifest_sha256": manifest.sha256,
        },
        "processor": {
            "name": "scaling-neuro-processor",
            "version": __version__,
            "converter": "dcm2niix",
            "converter_version": DCM2NIIX_VERSION,
            "arguments": NORMALIZED_ARGUMENTS,
        },
        "outputs": [output.descriptor() for output in outputs],
        "validation": {
            "functional_epi": True,
            "dimensions": facts.dimensions,
            "volume_count": facts.volume_count,
            "tr_seconds": facts.tr_seconds,
        },
    }


def output_file(
    kind: str, path: Path, content_type: str, *, uncompressed_sha256: str | None = None
) -> OutputFile:
    size, digest = sha256_file(path)
    return OutputFile(
        kind=kind,
        path=str(path),
        size_bytes=size,
        sha256=digest,
        content_type=content_type,
        uncompressed_sha256=uncompressed_sha256,
    )


def _legacy_exact_keys(
    value: Any, required: set[str], optional: set[str] | frozenset[str] = frozenset()
) -> dict[str, Any]:
    if (
        not isinstance(value, dict)
        or not required.issubset(value)
        or set(value) - required - optional
    ):
        raise InvalidNifti("LEGACY_SIDECAR_INVALID")
    return value


def _legacy_number(
    value: Any,
    *,
    minimum: float,
    maximum: float,
    exclusive_minimum: bool = False,
) -> float:
    if (
        isinstance(value, bool)
        or not isinstance(value, (int, float))
        or not math.isfinite(value)
    ):
        raise InvalidNifti("LEGACY_SIDECAR_INVALID")
    result = float(value)
    if result > maximum or (
        result <= minimum if exclusive_minimum else result < minimum
    ):
        raise InvalidNifti("LEGACY_SIDECAR_INVALID")
    return result


def _legacy_integer(value: Any, *, minimum: int, maximum: int) -> int:
    if (
        isinstance(value, bool)
        or not isinstance(value, int)
        or not minimum <= value <= maximum
    ):
        raise InvalidNifti("LEGACY_SIDECAR_INVALID")
    return value


def _legacy_enum_list(value: Any, *, allowed: set[str], maximum: int) -> list[str]:
    if (
        not isinstance(value, list)
        or len(value) > maximum
        or any(not isinstance(item, str) or item not in allowed for item in value)
        or len(set(value)) != len(value)
    ):
        raise InvalidNifti("LEGACY_SIDECAR_INVALID")
    return value


def _validate_public_source(value: Any) -> int:
    source = _legacy_exact_keys(
        value, {"dicom_count"}, PUBLIC_SOURCE_KEYS - {"dicom_count"}
    )
    dicom_count = _legacy_integer(source["dicom_count"], minimum=1, maximum=2**31 - 1)
    if "manufacturer" in source and source["manufacturer"] not in {
        "Siemens",
        "Philips",
        "GE",
        "Canon/Toshiba",
        "United Imaging",
        "Bruker",
    }:
        raise InvalidNifti("LEGACY_SIDECAR_INVALID")
    if "model" in source and (
        not isinstance(source["model"], str) or not safe_scanner_text(source["model"])
    ):
        raise InvalidNifti("LEGACY_SIDECAR_INVALID")
    if "software_versions" in source:
        versions = source["software_versions"]
        if (
            not isinstance(versions, list)
            or not 1 <= len(versions) <= 16
            or any(
                not isinstance(item, str) or not safe_scanner_text(item)
                for item in versions
            )
            or len(set(versions)) != len(versions)
        ):
            raise InvalidNifti("LEGACY_SIDECAR_INVALID")
    if "magnetic_field_strength" in source:
        _legacy_number(
            source["magnetic_field_strength"],
            minimum=0,
            maximum=15,
            exclusive_minimum=True,
        )
    if "sequence_name" in source and source["sequence_name"] not in {
        "EPI",
        "EPFID",
        "EP2D",
        "BOLD_EPI",
    }:
        raise InvalidNifti("LEGACY_SIDECAR_INVALID")
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
                "OTHER",
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
            22,
        ),
    }
    for key, (allowed, maximum) in enum_lists.items():
        if key in source:
            _legacy_enum_list(source[key], allowed=allowed, maximum=maximum)
    if "mr_acquisition_type" in source and source["mr_acquisition_type"] not in {
        "2D",
        "3D",
        "UNKNOWN",
    }:
        raise InvalidNifti("LEGACY_SIDECAR_INVALID")
    if "patient_position" in source and source["patient_position"] not in {
        "HFP",
        "HFS",
        "HFDR",
        "HFDL",
        "FFDR",
        "FFDL",
        "FFP",
        "FFS",
        "UNKNOWN",
    }:
        raise InvalidNifti("LEGACY_SIDECAR_INVALID")
    for key in ("series_number", "acquisition_number"):
        if key in source:
            _legacy_integer(source[key], minimum=0, maximum=2**31 - 1)
    for key in ("receive_coil_name", "transmit_coil_name"):
        if key in source and (
            not isinstance(source[key], str)
            or not CANONICAL_COIL_RE.fullmatch(source[key])
        ):
            raise InvalidNifti("LEGACY_SIDECAR_INVALID")
    return dicom_count


def _legacy_value_matches(observed: Any, expected: Any) -> bool:
    if isinstance(expected, bool):
        return observed is expected
    if isinstance(expected, int):
        return (
            not isinstance(observed, bool)
            and isinstance(observed, int)
            and observed == expected
        )
    if isinstance(expected, float):
        return (
            not isinstance(observed, bool)
            and isinstance(observed, (int, float))
            and math.isfinite(observed)
            and math.isclose(float(observed), expected, rel_tol=1e-5, abs_tol=1e-5)
        )
    if isinstance(expected, list):
        return (
            isinstance(observed, list)
            and len(observed) == len(expected)
            and all(
                _legacy_value_matches(item, expected_item)
                for item, expected_item in zip(observed, expected, strict=True)
            )
        )
    return observed == expected


def _validate_public_image(value: Any, facts: NiftiFacts) -> None:
    expected_image = facts.image_dict()
    image = _legacy_exact_keys(
        value,
        set(expected_image) | {"te_seconds"},
        PUBLIC_IMAGE_KEYS - set(expected_image) - {"te_seconds"},
    )
    if any(
        not _legacy_value_matches(image.get(key), expected)
        for key, expected in expected_image.items()
    ):
        raise InvalidNifti("LEGACY_SIDECAR_IMAGE_MISMATCH")
    numeric_fields = {
        "te_seconds": (0.0, 2.0, True),
        "inversion_time_seconds": (0.0, 30.0, False),
        "flip_angle_degrees": (0.0, 360.0, False),
        "effective_echo_spacing_seconds": (0.0, 1.0, True),
        "total_readout_time_seconds": (0.0, 10.0, False),
        "dwell_time_seconds": (0.0, 1.0, True),
        "multiband_acceleration_factor": (1.0, 64.0, False),
        "parallel_reduction_factor_in_plane": (1.0, 64.0, False),
        "partial_fourier": (0.0, 1.0, True),
        "number_of_averages": (0.0, 1_000_000.0, True),
        "imaging_frequency_mhz": (0.0, 10_000.0, True),
        "slice_thickness_mm": (0.0, 100.0, True),
        "spacing_between_slices_mm": (0.0, 100.0, True),
        "pixel_bandwidth_hz": (0.0, 10_000_000.0, True),
    }
    for key, (minimum, maximum, exclusive) in numeric_fields.items():
        if key in image:
            _legacy_number(
                image[key],
                minimum=minimum,
                maximum=maximum,
                exclusive_minimum=exclusive,
            )
    for key, maximum in (("echo_number", 1024), ("echo_train_length", 1_000_000)):
        if key in image:
            _legacy_integer(image[key], minimum=1, maximum=maximum)
    if "slice_timing_seconds" in image:
        timings = image["slice_timing_seconds"]
        if not isinstance(timings, list) or len(timings) > 4096:
            raise InvalidNifti("LEGACY_SIDECAR_INVALID")
        for timing in timings:
            _legacy_number(timing, minimum=0, maximum=60)
    if "phase_encoding_direction" in image and image[
        "phase_encoding_direction"
    ] not in {
        "i",
        "i-",
        "j",
        "j-",
        "k",
        "k-",
    }:
        raise InvalidNifti("LEGACY_SIDECAR_INVALID")
    if "imaged_nucleus" in image and image["imaged_nucleus"] not in {
        "1H",
        "13C",
        "17O",
        "19F",
        "23Na",
        "31P",
        "129Xe",
    }:
        raise InvalidNifti("LEGACY_SIDECAR_INVALID")
    for key, minimum_items, maximum_items, minimum in (
        ("acquisition_matrix", 4, 4, 0),
        ("recon_matrix", 2, 3, 1),
    ):
        if key in image:
            matrix = image[key]
            if (
                not isinstance(matrix, list)
                or not minimum_items <= len(matrix) <= maximum_items
            ):
                raise InvalidNifti("LEGACY_SIDECAR_INVALID")
            for item in matrix:
                _legacy_integer(item, minimum=minimum, maximum=2**31 - 1)


def validate_legacy_sidecar(
    path: Path,
    facts: NiftiFacts,
    *,
    expected_bundle_id: str,
    expected_series_id: str,
    nifti_filename: str,
    nifti_size: int,
    nifti_sha256: str,
) -> int:
    value = read_converter_sidecar(path)
    required = {
        "schema_version",
        "bundle_id",
        "subject_id",
        "session_id",
        "series_id",
        "protocol_group_id",
        "modality",
        "source",
        "image",
        "files",
        "conversion",
        "classification",
        "qc",
        "metadata_policy",
    }
    _legacy_exact_keys(value, required, {"$schema"})
    if value.get("$schema") not in {
        None,
        "https://scalingneuro.com/schemas/scan-sidecar-v1.schema.json",
    } or (
        value.get("schema_version") != "1.0.0"
        or value.get("bundle_id") != expected_bundle_id
        or value.get("series_id") != expected_series_id
        or value.get("modality") != "bold"
    ):
        raise InvalidNifti("LEGACY_SIDECAR_INVALID")
    for key in (
        "bundle_id",
        "subject_id",
        "session_id",
        "series_id",
        "protocol_group_id",
    ):
        if not isinstance(value.get(key), str) or not PSEUDONYM_RE.fullmatch(
            value[key]
        ):
            raise InvalidNifti("LEGACY_SIDECAR_INVALID")
    dicom_count = _validate_public_source(value.get("source"))
    files = _legacy_exact_keys(value.get("files"), {"nifti"})
    nifti = _legacy_exact_keys(
        files.get("nifti"),
        {"filename", "size_bytes", "sha256", "uncompressed_sha256"},
    )
    if (
        not isinstance(nifti.get("filename"), str)
        or not SAFE_FILENAME_RE.fullmatch(nifti["filename"])
        or _legacy_integer(nifti.get("size_bytes"), minimum=1, maximum=5 * 1024**3)
        != nifti_size
        or not isinstance(nifti.get("sha256"), str)
        or not SHA256_RE.fullmatch(nifti["sha256"])
        or not isinstance(nifti.get("uncompressed_sha256"), str)
        or not SHA256_RE.fullmatch(nifti["uncompressed_sha256"])
        or nifti["filename"] != nifti_filename
        or nifti["sha256"] != nifti_sha256
        or nifti["uncompressed_sha256"] != facts.uncompressed_sha256
    ):
        raise InvalidNifti("LEGACY_SIDECAR_FILE_MISMATCH")
    _validate_public_image(value.get("image"), facts)
    conversion = _legacy_exact_keys(
        value.get("conversion"),
        {"client_version", "converter", "converter_version", "arguments"},
    )
    if (
        not isinstance(conversion.get("client_version"), str)
        or not SEMVER_RE.fullmatch(conversion["client_version"])
        or conversion.get("converter") != "dcm2niix"
        or conversion.get("converter_version") != DCM2NIIX_VERSION
        or conversion.get("arguments") != NORMALIZED_ARGUMENTS
    ):
        raise InvalidNifti("LEGACY_SIDECAR_INVALID")
    classification = _legacy_exact_keys(
        value.get("classification"), {"decision", "kind", "confidence", "evidence"}
    )
    confidence = classification.get("confidence")
    evidence = classification.get("evidence")
    if (
        classification.get("decision") != "accepted"
        or classification.get("kind") != "functional_epi"
        or isinstance(confidence, bool)
        or not isinstance(confidence, (int, float))
        or not math.isfinite(confidence)
        or not 0.9 <= float(confidence) <= 1.0
        or not isinstance(evidence, list)
        or not 1 <= len(evidence) <= 64
    ):
        raise InvalidNifti("LEGACY_SIDECAR_CLASSIFICATION_INVALID")
    for item in evidence:
        evidence_item = _legacy_exact_keys(item, {"code", "source", "effect"})
        if (
            not isinstance(evidence_item.get("code"), str)
            or not CODE_RE.fullmatch(evidence_item["code"])
            or evidence_item.get("source")
            not in {"dicom_header", "converter_sidecar", "nifti_header", "derived"}
            or evidence_item.get("effect")
            not in {"supports", "contradicts", "excludes"}
        ):
            raise InvalidNifti("LEGACY_SIDECAR_CLASSIFICATION_INVALID")
    qc = _legacy_exact_keys(value.get("qc"), {"passed", "checks", "warnings"})
    checks = qc.get("checks")
    warnings = qc.get("warnings")
    if (
        qc.get("passed") is not True
        or not isinstance(checks, list)
        or not 1 <= len(checks) <= 128
        or not isinstance(warnings, list)
        or len(warnings) > 64
        or any(
            not isinstance(item, str) or not CODE_RE.fullmatch(item)
            for item in warnings
        )
        or len(set(warnings)) != len(warnings)
    ):
        raise InvalidNifti("LEGACY_SIDECAR_QC_INVALID")
    for item in checks:
        check = _legacy_exact_keys(item, {"code", "status"})
        if (
            not isinstance(check.get("code"), str)
            or not CODE_RE.fullmatch(check["code"])
            or check.get("status") not in {"pass", "warn"}
        ):
            raise InvalidNifti("LEGACY_SIDECAR_QC_INVALID")
    policy = _legacy_exact_keys(
        value.get("metadata_policy"), {"policy_id", "policy_version"}
    )
    if policy != {
        "policy_id": "scaling-neuro-epi-default-deny",
        "policy_version": "1.1.0",
    }:
        raise InvalidNifti("LEGACY_SIDECAR_POLICY_INVALID")
    return dicom_count
