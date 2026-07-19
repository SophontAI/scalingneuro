from __future__ import annotations

import json
import os
from pathlib import Path
import shutil
from threading import Event
from typing import Any

from . import DCM2NIIX_VERSION, PIPELINE_VERSION, __version__
from .api import ControlPlane
from .archive import extract_archive
from .config import Config
from .converter import NORMALIZED_ARGUMENTS, convert
from .errors import CapacityFailure, InvalidArchive, InvalidJob, InvalidNifti, LeaseLost
from .metadata import (
    build_processing_manifest,
    build_sidecar,
    output_file,
    read_converter_sidecar,
    validate_legacy_sidecar,
    write_canonical_json,
)
from .models import DicomInput, Job, NiftiInput, OutputFile
from .nifti import deterministic_gzip, inspect_gzip_nifti, sanitize_nifti
from .transport import ObjectTransport, sha256_file


RESULT_SCHEMA = "scaling-neuro.local-result.v2"
CONVERSION_WORKING_SET_FACTOR = 2


def _assert_lease(active: Event) -> None:
    if not active.is_set():
        raise LeaseLost()


def _clean(path: Path) -> None:
    if path.exists():
        shutil.rmtree(path)


def _require_free_space(path: Path, required_bytes: int, reserve_bytes: int) -> None:
    if shutil.disk_usage(path).free < required_bytes + reserve_bytes:
        raise CapacityFailure()


def _save_result(
    job_root: Path,
    job: Job,
    input_sha256: str,
    outputs: list[OutputFile],
    validation: dict[str, Any],
) -> None:
    value = {
        "schema_version": RESULT_SCHEMA,
        "processor_version": __version__,
        "pipeline_version": PIPELINE_VERSION,
        "dcm2niix_version": DCM2NIIX_VERSION,
        "series_archive_id": job.series_archive_id,
        "series_id": job.series_id,
        "input_sha256": input_sha256,
        "outputs": [
            {
                **output.descriptor(),
                "path": str(Path(output.path).relative_to(job_root)),
            }
            for output in outputs
        ],
        "validation": validation,
    }
    temporary = job_root / "result.json.partial"
    write_canonical_json(temporary, value)
    os.replace(temporary, job_root / "result.json")


def _load_result(
    job_root: Path, job: Job, input_sha256: str, expected_dicom_count: int
) -> tuple[list[OutputFile], dict[str, Any]] | None:
    path = job_root / "result.json"
    if not path.exists():
        return None
    try:
        value = json.loads(path.read_text(encoding="utf-8"))
        if set(value) != {
            "schema_version",
            "processor_version",
            "pipeline_version",
            "dcm2niix_version",
            "series_archive_id",
            "series_id",
            "input_sha256",
            "outputs",
            "validation",
        } or any(
            (
                value.get("schema_version") != RESULT_SCHEMA,
                value.get("processor_version") != __version__,
                value.get("pipeline_version") != PIPELINE_VERSION,
                value.get("dcm2niix_version") != DCM2NIIX_VERSION,
                value.get("series_archive_id") != job.series_archive_id,
                value.get("series_id") != job.series_id,
                value.get("input_sha256") != input_sha256,
            )
        ):
            return None
        raw_outputs = value.get("outputs")
        validation = value.get("validation")
        if (
            not isinstance(raw_outputs, list)
            or not isinstance(validation, dict)
            or set(validation)
            != {
                "archive_sha256_verified",
                "dicom_count",
                "dicom_parse_succeeded",
                "functional_epi_confirmed",
            }
            or validation
            != {
                "archive_sha256_verified": True,
                "dicom_count": expected_dicom_count,
                "dicom_parse_succeeded": True,
                "functional_epi_confirmed": True,
            }
        ):
            return None
        outputs: list[OutputFile] = []
        expected_kinds = ["nifti", "sidecar", "processing_manifest"]
        expected_paths = {
            "nifti": Path("outputs/bold.nii.gz"),
            "sidecar": Path("outputs/bold.json"),
            "processing_manifest": Path("outputs/processing-manifest.json"),
        }
        expected_content_types = {
            "nifti": "application/gzip",
            "sidecar": "application/json",
            "processing_manifest": "application/json",
        }
        for raw, expected_kind in zip(raw_outputs, expected_kinds, strict=True):
            expected_keys = {"kind", "size_bytes", "sha256", "content_type", "path"}
            if expected_kind == "nifti":
                expected_keys.add("uncompressed_sha256")
            if (
                not isinstance(raw, dict)
                or set(raw) != expected_keys
                or raw.get("kind") != expected_kind
                or raw.get("content_type") != expected_content_types[expected_kind]
            ):
                return None
            relative = Path(raw["path"])
            if relative != expected_paths[expected_kind]:
                return None
            output_path = job_root / relative
            size, digest = sha256_file(output_path)
            if size != raw.get("size_bytes") or digest != raw.get("sha256"):
                return None
            outputs.append(
                OutputFile(
                    kind=expected_kind,
                    path=str(output_path),
                    size_bytes=size,
                    sha256=digest,
                    content_type=raw["content_type"],
                    uncompressed_sha256=raw.get("uncompressed_sha256"),
                )
            )
        nifti_output, sidecar_output, processing_output = outputs
        if nifti_output.uncompressed_sha256 is None:
            return None
        facts = inspect_gzip_nifti(
            Path(nifti_output.path), nifti_output.uncompressed_sha256
        )
        validate_legacy_sidecar(
            Path(sidecar_output.path),
            facts,
            expected_bundle_id=job.series_archive_id,
            expected_series_id=job.series_id,
            nifti_filename=Path(nifti_output.path).name,
            nifti_size=nifti_output.size_bytes,
            nifti_sha256=nifti_output.sha256,
        )
        processing = json.loads(
            Path(processing_output.path).read_text(encoding="utf-8")
        )
        if (
            not isinstance(processing, dict)
            or set(processing)
            != {
                "schema_version",
                "series_archive_id",
                "series_id",
                "subject_id",
                "session_id",
                "protocol_group_id",
                "input",
                "processor",
                "outputs",
                "validation",
            }
            or processing.get("schema_version")
            != "scaling-neuro.processing-manifest.v1"
            or processing.get("series_archive_id") != job.series_archive_id
            or processing.get("series_id") != job.series_id
            or not isinstance(processing.get("input"), dict)
            or set(processing["input"])
            != {"format", "sha256", "dicom_count", "internal_manifest_sha256"}
            or processing["input"].get("format") != "dicom-tar-zstd"
            or processing["input"].get("sha256") != input_sha256
            or processing["input"].get("dicom_count") != expected_dicom_count
            or not isinstance(processing.get("processor"), dict)
            or processing["processor"]
            != {
                "name": "scaling-neuro-processor",
                "version": __version__,
                "converter": "dcm2niix",
                "converter_version": DCM2NIIX_VERSION,
                "arguments": NORMALIZED_ARGUMENTS,
            }
            or processing.get("outputs")
            != [nifti_output.descriptor(), sidecar_output.descriptor()]
            or processing.get("validation")
            != {
                "functional_epi": True,
                "dimensions": facts.dimensions,
                "volume_count": facts.volume_count,
                "tr_seconds": facts.tr_seconds,
            }
        ):
            return None
        return outputs, validation
    except (InvalidNifti, OSError, ValueError, KeyError, TypeError):
        return None


def prepare_dicom_job(
    config: Config,
    job: Job,
    descriptor: DicomInput,
    transport: ObjectTransport,
    lease_active: Event,
) -> tuple[list[OutputFile], dict[str, Any]]:
    job_root = config.job_root(job.job_id)
    job_root.mkdir(parents=True, exist_ok=True, mode=0o700)
    cached = _load_result(
        job_root, job, descriptor.archive.sha256, descriptor.dicom_count
    )
    if cached is not None:
        return cached
    archive_path = job_root / "input.tar.zst"
    _require_free_space(
        job_root,
        descriptor.archive.size_bytes,
        config.disk_reserve_bytes,
    )
    try:
        transport.download(descriptor.archive, archive_path)
    except InvalidJob as exc:
        raise InvalidArchive("ARCHIVE_DOWNLOAD_INTEGRITY_MISMATCH") from exc
    _assert_lease(lease_active)

    stage = job_root / "stage"
    outputs_dir = job_root / "outputs"
    _clean(stage)
    _clean(outputs_dir)
    stage.mkdir(mode=0o700)
    outputs_dir.mkdir(mode=0o700)
    manifest = extract_archive(
        config,
        archive_path,
        stage / "input",
        expected_series_archive_id=job.series_archive_id,
        expected_series_id=job.series_id,
        expected_dicom_count=descriptor.dicom_count,
    )
    _assert_lease(lease_active)
    _require_free_space(
        job_root,
        manifest.extracted_bytes * CONVERSION_WORKING_SET_FACTOR,
        config.disk_reserve_bytes,
    )
    conversion = convert(config, stage / "input" / "dicom", stage / "converted")
    sanitize_nifti(conversion.nifti)
    nifti_path = outputs_dir / "bold.nii.gz"
    facts, nifti_size, nifti_sha = deterministic_gzip(conversion.nifti, nifti_path)
    converter_sidecar = read_converter_sidecar(conversion.sidecar)
    sidecar_value = build_sidecar(
        manifest,
        converter_sidecar,
        facts,
        nifti_filename=nifti_path.name,
        nifti_size=nifti_size,
        nifti_sha256=nifti_sha,
    )
    image = sidecar_value["image"]
    functional_epi_confirmed = bool(
        manifest.value["classification"]["decision"] == "accepted"
        and manifest.value["classification"]["kind"] == "functional_epi"
        and manifest.value["classification"]["confidence"] >= 0.9
        and facts.dimensions[3] == facts.volume_count
        and facts.volume_count >= 10
        and 0.1 <= facts.tr_seconds <= 20
        and 0 < image["te_seconds"] <= 2
    )
    if not functional_epi_confirmed:
        raise InvalidNifti("FUNCTIONAL_EPI_NOT_CONFIRMED")
    sidecar_path = outputs_dir / "bold.json"
    write_canonical_json(sidecar_path, sidecar_value)
    nifti_output = output_file(
        "nifti",
        nifti_path,
        "application/gzip",
        uncompressed_sha256=facts.uncompressed_sha256,
    )
    sidecar_output = output_file("sidecar", sidecar_path, "application/json")
    processing_value = build_processing_manifest(
        manifest,
        input_sha256=descriptor.archive.sha256,
        outputs=[nifti_output, sidecar_output],
        facts=facts,
    )
    processing_path = outputs_dir / "processing-manifest.json"
    write_canonical_json(processing_path, processing_value)
    processing_output = output_file(
        "processing_manifest", processing_path, "application/json"
    )
    outputs = [nifti_output, sidecar_output, processing_output]
    validation = {
        "archive_sha256_verified": True,
        "dicom_count": manifest.dicom_count,
        "dicom_parse_succeeded": True,
        "functional_epi_confirmed": functional_epi_confirmed,
    }
    _save_result(job_root, job, descriptor.archive.sha256, outputs, validation)
    return outputs, validation


def process_legacy_job(
    config: Config,
    job: Job,
    descriptor: NiftiInput,
    transport: ObjectTransport,
    lease_active: Event,
) -> dict[str, Any]:
    job_root = config.job_root(job.job_id)
    job_root.mkdir(parents=True, exist_ok=True, mode=0o700)
    nifti_path = job_root / "legacy.nii.gz"
    sidecar_path = job_root / "legacy.json"
    transport.download(descriptor.nifti, nifti_path)
    transport.download(descriptor.sidecar, sidecar_path)
    _assert_lease(lease_active)
    assert descriptor.nifti.uncompressed_sha256 is not None
    facts = inspect_gzip_nifti(nifti_path, descriptor.nifti.uncompressed_sha256)
    filename = descriptor.nifti.filename
    if filename is None or not filename.endswith(".nii.gz"):
        # Older grants may omit the key; use the immutable sidecar declaration.
        try:
            raw = json.loads(sidecar_path.read_text(encoding="utf-8"))
            filename = raw["files"]["nifti"]["filename"]
        except (OSError, ValueError, KeyError, TypeError) as exc:
            raise InvalidJob() from exc
    validate_legacy_sidecar(
        sidecar_path,
        facts,
        expected_bundle_id=job.series_archive_id,
        expected_series_id=job.series_id,
        nifti_filename=filename,
        nifti_size=descriptor.nifti.size_bytes,
        nifti_sha256=descriptor.nifti.sha256,
    )
    return {
        "nifti_sha256_verified": True,
        "nifti_uncompressed_sha256_verified": True,
        "sidecar_sha256_verified": True,
        "nifti_header_valid": True,
        "sidecar_valid": True,
        "nifti_sidecar_consistent": True,
    }


def process_job(
    config: Config,
    api: ControlPlane,
    job: Job,
    lease_active: Event,
) -> None:
    transport = ObjectTransport(config)
    if isinstance(job.input, DicomInput):
        outputs, validation = prepare_dicom_job(
            config, job, job.input, transport, lease_active
        )
        _assert_lease(lease_active)
        grants = api.output_grants(job, outputs)
        for output in outputs:
            _assert_lease(lease_active)
            transport.upload(output, grants[output.kind])
        _assert_lease(lease_active)
        api.complete(job, outputs, validation, dcm2niix_version=DCM2NIIX_VERSION)
    elif isinstance(job.input, NiftiInput):
        validation = process_legacy_job(config, job, job.input, transport, lease_active)
        _assert_lease(lease_active)
        api.complete(job, [], validation, dcm2niix_version=DCM2NIIX_VERSION)
    else:
        raise InvalidJob()
    shutil.rmtree(config.job_root(job.job_id), ignore_errors=True)
