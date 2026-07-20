from __future__ import annotations

import gzip
import hashlib
import io
import json
from pathlib import Path
import struct
import subprocess
import tarfile
from typing import Any, Callable


ARCHIVE_ID = "a" * 24
SERIES_ID = "b" * 24
SUBJECT_ID = "c" * 24
SESSION_ID = "d" * 24
PROTOCOL_ID = "e" * 24
SOP_UID = "2.25.123456789012345678901234567890123456"
# Keep functional fixtures independent of hosted-runner disk size. Production
# retains the 20 GiB default; exact capacity boundaries are exercised with
# mocked filesystem values in test_hardening_contract.py.
TEST_DISK_RESERVE_BYTES = 1024**3


def siemens_csa_fixture() -> bytes:
    csa = bytearray(b"SV10\x04\x03\x02\x01")
    csa.extend(struct.pack("<II", 2, 77))
    name = b"NumberOfImagesInMosaic"
    csa.extend(name + b"\0" * (64 - len(name)))
    csa.extend(struct.pack("<i4siii", 1, b"US\0\0", 0, 1, 77))
    csa.extend(struct.pack("<iiii", 2, 2, 77, 0))
    csa.extend(b"4\0\0\0")
    name = b"SliceMeasurementDuration"
    csa.extend(name + b"\0" * (64 - len(name)))
    csa.extend(struct.pack("<i4siii", 1, b"DS\0\0", 0, 3, 77))
    csa.extend(struct.pack("<iiii", 4, 4, 77, 0))
    csa.extend(b"800\0")
    csa.extend(struct.pack("<iiii", 0, 0, 77, 0))
    csa.extend(struct.pack("<iiii", 0, 0, 77, 0))
    return bytes(csa)


def canonical_json(value: Any) -> bytes:
    return (json.dumps(value, sort_keys=True, separators=(",", ":")) + "\n").encode()


def _tar_octal_into(target: bytearray, start: int, length: int, value: int) -> None:
    encoded = format(value, "o").encode("ascii")
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


def _set_tar_checksum(header: bytearray) -> None:
    header[148:156] = b" " * 8
    _tar_octal_into(header, 148, 8, sum(header))


def _canonical_gnu_header(name: str, size: int) -> bytearray:
    encoded_name = name.encode("ascii")
    if not encoded_name or len(encoded_name) > 99:
        raise ValueError("test archive path does not fit the canonical GNU header")
    header = bytearray(512)
    header[: len(encoded_name)] = encoded_name
    _tar_octal_into(header, 100, 8, 0o600)
    _tar_number_into(header, 108, 8, 0)
    _tar_number_into(header, 116, 8, 0)
    _tar_number_into(header, 124, 12, size)
    _tar_number_into(header, 136, 12, 0)
    header[156] = ord("0")
    header[257:263] = b"ustar "
    header[263:265] = b" \0"
    _set_tar_checksum(header)
    return header


def _append_canonical_member(
    output: bytearray,
    name: str,
    payload: bytes,
    *,
    header_mutator: Callable[[bytearray], None] | None = None,
    padding_byte: int = 0,
) -> None:
    header = _canonical_gnu_header(name, len(payload))
    if header_mutator is not None:
        header_mutator(header)
        _set_tar_checksum(header)
    output.extend(header)
    output.extend(payload)
    output.extend(bytes([padding_byte]) * ((-len(payload)) % 512))


def make_dicom(path: Path, sop_uid: str = SOP_UID) -> bytes:
    from pydicom.dataset import FileDataset, FileMetaDataset
    from pydicom.tag import Tag
    from pydicom.uid import ExplicitVRLittleEndian, MRImageStorage, UID

    meta = FileMetaDataset()
    meta.FileMetaInformationVersion = b"\0\1"
    meta.MediaStorageSOPClassUID = MRImageStorage
    meta.MediaStorageSOPInstanceUID = UID(sop_uid)
    meta.TransferSyntaxUID = ExplicitVRLittleEndian
    meta.ImplementationClassUID = UID("2.25.323468694959424494117938985101850441847")
    meta.ImplementationVersionName = "NEUROSYNC_RAW_1"
    dataset = FileDataset(str(path), {}, file_meta=meta, preamble=b"\0" * 128)
    dataset.SOPClassUID = MRImageStorage
    dataset.SOPInstanceUID = sop_uid
    dataset.Modality = "MR"
    dataset.Manufacturer = "SIEMENS"
    dataset.ImageType = ["ORIGINAL", "PRIMARY", "M", "EPI", "BOLD", "MOSAIC"]
    dataset.ManufacturerModelName = "MAGNETOM Prisma_fit"
    dataset.ScanningSequence = "EP"
    dataset.SequenceVariant = "SK"
    dataset.ScanOptions = "FS"
    dataset.MRAcquisitionType = "2D"
    dataset.SequenceName = "ep2d_bold"
    dataset.RepetitionTime = "800"
    dataset.EchoTime = "30"
    dataset.SoftwareVersions = "Siemens E11"
    dataset.PatientPosition = "HFS"
    dataset.MagneticFieldStrength = "3.0"
    dataset.PatientName = SUBJECT_ID
    dataset.PatientID = SUBJECT_ID
    dataset.PatientBirthDate = ""
    dataset.PatientSex = ""
    dataset.PatientIdentityRemoved = "YES"
    dataset.DeidentificationMethod = (
        "Scaling Neuro scaling-neuro.dicom-deidentification 1.0.0"
    )
    dataset.LongitudinalTemporalInformationModified = "REMOVED"
    dataset.StudyInstanceUID = "2.25.100000000000000000000000000000000001"
    dataset.SeriesInstanceUID = "2.25.100000000000000000000000000000000002"
    dataset.FrameOfReferenceUID = "2.25.100000000000000000000000000000000003"
    dataset.StudyDate = ""
    dataset.AcquisitionDate = ""
    dataset.ContentDate = ""
    dataset.StudyTime = ""
    dataset.AcquisitionTime = ""
    dataset.ContentTime = ""
    dataset.AccessionNumber = ""
    dataset.ReferringPhysicianName = ""
    dataset.StudyID = ""
    dataset.PositionReferenceIndicator = ""
    dataset.SeriesNumber = "7"
    dataset.AcquisitionNumber = "1"
    dataset.InstanceNumber = "1"
    dataset.EchoTrainLength = "1"
    dataset.NumberOfFrames = "1"
    dataset.DeviceSerialNumber = "SN-0123456789abcdef01234567"
    dataset.NumberOfTemporalPositions = "12"
    dataset.BurnedInAnnotation = "NO"
    dataset.add_new(Tag(0x0029, 0x0010), "LO", "SIEMENS CSA HEADER")
    dataset.add_new(Tag(0x0029, 0x1010), "OB", siemens_csa_fixture())
    dataset.Rows = 8
    dataset.Columns = 8
    dataset.BitsAllocated = 16
    dataset.BitsStored = 16
    dataset.HighBit = 15
    dataset.PixelRepresentation = 0
    dataset.SamplesPerPixel = 1
    dataset.PhotometricInterpretation = "MONOCHROME2"
    dataset.PixelData = b"\0" * 128
    dataset.save_as(path, enforce_file_format=True)
    return path.read_bytes()


def make_structural_dicom(path: Path, sop_uid: str = SOP_UID) -> bytes:
    """Create a privacy-cleared, non-EPI MR Image Storage instance."""
    from pydicom import dcmread
    from pydicom.tag import Tag

    make_dicom(path, sop_uid)
    dataset = dcmread(path)
    dataset.Manufacturer = "GE MEDICAL SYSTEMS"
    dataset.ManufacturerModelName = "Discovery MR750"
    dataset.SoftwareVersions = "GE DV26"
    dataset.ImageType = ["ORIGINAL", "PRIMARY", "M", "T1"]
    dataset.ScanningSequence = "GR"
    dataset.MRAcquisitionType = "3D"
    dataset.SequenceName = "mprage"
    dataset.RepetitionTime = "2300"
    dataset.EchoTime = "2.5"
    dataset.DeidentificationMethod = (
        "Scaling Neuro scaling-neuro.dicom-deidentification 2.0.0"
    )
    for tag in (
        Tag(0x0020, 0x0105),
        Tag(0x0029, 0x0010),
        Tag(0x0029, 0x1010),
    ):
        if tag in dataset:
            del dataset[tag]
    dataset.save_as(path, enforce_file_format=True)
    return path.read_bytes()


def make_functional_dicom_v2(path: Path, sop_uid: str = SOP_UID) -> bytes:
    from pydicom import dcmread

    make_dicom(path, sop_uid)
    dataset = dcmread(path)
    dataset.DeidentificationMethod = (
        "Scaling Neuro scaling-neuro.dicom-deidentification 2.0.0"
    )
    dataset.save_as(path, enforce_file_format=True)
    return path.read_bytes()


def conform_enhanced_mr(dataset: Any, *, adjust_pixel_data: bool = True) -> None:
    """Add the bounded mandatory Enhanced MR modules used by sanitized fixtures."""
    from pydicom.dataset import Dataset
    from pydicom.sequence import Sequence
    from pydicom.tag import Tag
    from pydicom.uid import UID

    enhanced_uid = "1.2.840.10008.5.1.4.1.1.4.1"
    legacy_uid = "1.2.840.10008.5.1.4.1.1.4.4"
    sop_class = str(dataset.SOPClassUID)
    if sop_class not in {enhanced_uid, legacy_uid}:
        return

    dataset.ContentDate = "19000101"
    dataset.ContentTime = "000000"
    dataset.InstanceNumber = str(getattr(dataset, "InstanceNumber", "1") or "1")
    dataset.add_new(Tag(0x0008, 0x9205), "CS", "MONOCHROME")
    dataset.add_new(Tag(0x0008, 0x9206), "CS", "VOLUME")
    dataset.add_new(Tag(0x0008, 0x9207), "CS", "NONE")
    dataset.add_new(Tag(0x2050, 0x0020), "CS", "IDENTITY")
    dataset.add_new(Tag(0x0040, 0x0555), "SQ", Sequence([]))
    if sop_class == enhanced_uid:
        dataset.add_new(Tag(0x0008, 0x9208), "CS", "MAGNITUDE")
        dataset.add_new(Tag(0x0008, 0x9209), "CS", "UNKNOWN")
        dataset.add_new(Tag(0x0028, 0x2110), "CS", "00")
        dataset.add_new(Tag(0x0018, 0x9005), "SH", "ep2d")
        dataset.add_new(Tag(0x0018, 0x9008), "CS", "GRADIENT")
        for tag in (
            Tag(0x0018, 0x9012),
            Tag(0x0018, 0x9014),
            Tag(0x0018, 0x9015),
            Tag(0x0018, 0x9024),
        ):
            dataset.add_new(tag, "CS", "NO")
        dataset.add_new(Tag(0x0018, 0x9017), "CS", "NONE")
        dataset.add_new(Tag(0x0018, 0x9018), "CS", "YES")
        dataset.add_new(Tag(0x0018, 0x9025), "CS", "NONE")
        dataset.add_new(Tag(0x0018, 0x9029), "CS", "NONE")
        dataset.add_new(Tag(0x0018, 0x9032), "CS", "RECTILINEAR")
        dataset.add_new(Tag(0x0018, 0x9033), "CS", "SINGLE")
        dataset.add_new(Tag(0x0018, 0x9034), "CS", "LINEAR")
        dataset.add_new(Tag(0x0018, 0x9093), "US", 1)

    frames = int(getattr(dataset, "NumberOfFrames", 1) or 1)
    dataset.NumberOfFrames = str(frames)
    existing_shared = list(getattr(dataset, "SharedFunctionalGroupsSequence", []))
    shared = existing_shared[0] if len(existing_shared) == 1 else Dataset()
    pixel_measures = Dataset()
    pixel_measures.PixelSpacing = ["2", "2"]
    pixel_measures.SliceThickness = "3"
    shared.PixelMeasuresSequence = Sequence([pixel_measures])
    plane_position = Dataset()
    plane_position.ImagePositionPatient = ["0", "0", "0"]
    shared.PlanePositionSequence = Sequence([plane_position])
    plane_orientation = Dataset()
    plane_orientation.ImageOrientationPatient = ["1", "0", "0", "0", "1", "0"]
    shared.PlaneOrientationSequence = Sequence([plane_orientation])

    if sop_class == enhanced_uid:
        anatomy_code = Dataset()
        anatomy_code.CodeValue = "T-A0100"
        anatomy_code.CodingSchemeDesignator = "SRT"
        anatomy_code.CodeMeaning = "ANATOMY"
        frame_anatomy = Dataset()
        frame_anatomy.AnatomicRegionSequence = Sequence([anatomy_code])
        frame_anatomy.FrameLaterality = "U"
        shared.FrameAnatomySequence = Sequence([frame_anatomy])

        transform = Dataset()
        transform.RescaleIntercept = "0"
        transform.RescaleSlope = "1"
        transform.RescaleType = "US"
        shared.PixelValueTransformationSequence = Sequence([transform])

        timing = Dataset()
        timing.RepetitionTime = "800"
        timing.EchoTrainLength = "1"
        timing.FlipAngle = "70"
        timing.add_new(Tag(0x0018, 0x9240), "US", 1)
        timing.add_new(Tag(0x0018, 0x9241), "US", 1)
        shared.MRTimingAndRelatedParametersSequence = Sequence([timing])
        echo = Dataset()
        echo.EffectiveEchoTime = 30.0
        shared.MREchoSequence = Sequence([echo])
        modifier = Dataset()
        for tag, value in (
            (Tag(0x0018, 0x9009), "NO"),
            (Tag(0x0018, 0x9010), "NONE"),
            (Tag(0x0018, 0x9016), "NONE"),
            (Tag(0x0018, 0x9021), "NO"),
            (Tag(0x0018, 0x9026), "NONE"),
            (Tag(0x0018, 0x9027), "NONE"),
            (Tag(0x0018, 0x9077), "NO"),
            (Tag(0x0018, 0x9081), "NO"),
        ):
            modifier.add_new(tag, "CS", value)
        shared.MRModifierSequence = Sequence([modifier])
        imaging_modifier = Dataset()
        imaging_modifier.PixelBandwidth = "2000"
        imaging_modifier.add_new(Tag(0x0018, 0x9020), "CS", "NONE")
        imaging_modifier.add_new(Tag(0x0018, 0x9022), "CS", "NO")
        imaging_modifier.add_new(Tag(0x0018, 0x9028), "CS", "NONE")
        imaging_modifier.add_new(Tag(0x0018, 0x9098), "FD", 123.25)
        shared.MRImagingModifierSequence = Sequence([imaging_modifier])
        receive = Dataset()
        receive.ReceiveCoilName = "HEAD_32"
        receive.add_new(Tag(0x0018, 0x9041), "LO", "")
        receive.add_new(Tag(0x0018, 0x9043), "CS", "VOLUME")
        receive.add_new(Tag(0x0018, 0x9044), "CS", "YES")
        shared.MRReceiveCoilSequence = Sequence([receive])
        transmit = Dataset()
        transmit.TransmitCoilName = "BODY"
        transmit.add_new(Tag(0x0018, 0x9050), "LO", "")
        transmit.add_new(Tag(0x0018, 0x9051), "CS", "BODY")
        shared.MRTransmitCoilSequence = Sequence([transmit])
        averages = Dataset()
        averages.NumberOfAverages = "1"
        shared.MRAveragesSequence = Sequence([averages])
        fov = Dataset()
        fov.PercentSampling = "100"
        fov.PercentPhaseFieldOfView = "100"
        fov.InPlanePhaseEncodingDirection = "COLUMN"
        fov.add_new(Tag(0x0018, 0x9058), "US", 64)
        fov.add_new(Tag(0x0018, 0x9231), "US", 64)
        shared.MRFOVGeometrySequence = Sequence([fov])
    else:
        shared.add_new(Tag(0x0020, 0x9170), "SQ", Sequence([Dataset()]))
    dataset.SharedFunctionalGroupsSequence = Sequence([shared])

    organization_uid = UID("2.25.100000000000000000000000000000000004")
    organization = Dataset()
    organization.DimensionOrganizationUID = organization_uid
    dataset.DimensionOrganizationSequence = Sequence([organization])
    dimension = Dataset()
    dimension.DimensionOrganizationUID = organization_uid
    dimension.DimensionIndexPointer = Tag(0x0020, 0x9057)
    dimension.FunctionalGroupPointer = Tag(0x0020, 0x9111)
    dataset.DimensionIndexSequence = Sequence([dimension])

    existing_frames = list(getattr(dataset, "PerFrameFunctionalGroupsSequence", []))
    per_frame = []
    for index in range(frames):
        frame = existing_frames[index] if index < len(existing_frames) else Dataset()
        direct_frame_type = frame.get(Tag(0x0008, 0x9007))
        existing_frame_type = frame.get(Tag(0x0018, 0x9226))
        if (
            existing_frame_type is not None
            and len(existing_frame_type.value) == 1
            and Tag(0x0008, 0x9007) in existing_frame_type.value[0]
        ):
            frame_type_values = list(
                existing_frame_type.value[0][Tag(0x0008, 0x9007)].value
            )
        elif direct_frame_type is not None:
            frame_type_values = list(direct_frame_type.value)
        else:
            frame_type_values = list(dataset.ImageType)
        frame.pop(Tag(0x0008, 0x9007), None)
        frame_type = Dataset()
        frame_type.FrameType = frame_type_values
        frame_type.add_new(Tag(0x0008, 0x9205), "CS", "MONOCHROME")
        frame_type.add_new(Tag(0x0008, 0x9206), "CS", "VOLUME")
        frame_type.add_new(Tag(0x0008, 0x9207), "CS", "NONE")
        frame.MRImageFrameTypeSequence = Sequence([frame_type])
        frame_content = Dataset()
        frame_content.DimensionIndexValues = [index + 1]
        frame_content.InStackPositionNumber = index + 1
        if sop_class == enhanced_uid:
            frame_content.FrameAcquisitionDateTime = "19000101000000"
            frame_content.FrameReferenceDateTime = "19000101000000"
            frame_content.FrameAcquisitionDuration = 10.0
        frame.FrameContentSequence = Sequence([frame_content])
        if sop_class == legacy_uid:
            frame.add_new(Tag(0x0020, 0x9171), "SQ", Sequence([Dataset()]))
        per_frame.append(frame)
    dataset.PerFrameFunctionalGroupsSequence = Sequence(per_frame)

    if (
        adjust_pixel_data
        and not UID(str(dataset.file_meta.TransferSyntaxUID)).is_compressed
    ):
        expected = (
            int(dataset.Rows)
            * int(dataset.Columns)
            * int(dataset.SamplesPerPixel)
            * frames
            * (int(dataset.BitsAllocated) // 8)
        )
        dataset.PixelData = b"\0" * (expected + expected % 2)


def archive_manifest(dicom: bytes, *, archive_id: str = ARCHIVE_ID) -> dict[str, Any]:
    return {
        "schema_version": "1.0.0",
        "series_archive_id": archive_id,
        "series_id": SERIES_ID,
        "subject_id": SUBJECT_ID,
        "session_id": SESSION_ID,
        "protocol_group_id": PROTOCOL_ID,
        "modality": "functional_epi",
        "dicom_instance_count": 1,
        "client": {"name": "neuro-sync", "version": "0.3.0"},
        "deidentification": {
            "policy_id": "scaling-neuro.dicom-deidentification",
            "policy_version": "1.0.0",
            "method": "scaling-neuro-recursive-allowlist-v1",
            "recursive": True,
            "private_text_removed": True,
            "unknown_private_removed": True,
            "uids_remapped": True,
            "pixel_data_retained": True,
            "burned_in_annotation_status": "verified_no",
            "safe_private_exceptions": ["siemens_csa_image_header_numeric_v1"],
        },
        "source": {
            "dicom_count": 1,
            "manufacturer": "SIEMENS",
            "model": "MAGNETOM Prisma_fit",
            "software_versions": ["Siemens E11"],
            "magnetic_field_strength": 3.0,
            "scanning_sequence": ["EP"],
            "sequence_variant": ["SK"],
            "scan_options": ["FS"],
            "mr_acquisition_type": "2D",
            "image_type": [
                "ORIGINAL",
                "PRIMARY",
                "M",
                "EPI",
                "BOLD",
                "MOSAIC",
            ],
            "series_number": 7,
        },
        "classification": {
            "decision": "accepted",
            "kind": "functional_epi",
            "confidence": 0.99,
            "evidence": [
                {
                    "code": "echo_planar_scanning_sequence",
                    "source": "dicom_header",
                    "effect": "supports",
                }
            ],
        },
        "instances": [
            {
                "path": "dicom/000001.dcm",
                "size_bytes": len(dicom),
                "sha256": hashlib.sha256(dicom).hexdigest(),
                "sop_instance_uid": SOP_UID,
            }
        ],
    }


def archive_manifest_v2(
    dicom: bytes,
    *,
    archive_id: str = ARCHIVE_ID,
    series_kind: str = "structural_t1w",
    processing_route: str = "archive-verify-v1",
    evidence_code: str = "structural_t1w_detected",
    source: dict[str, Any] | None = None,
    safe_private_exceptions: list[str] | None = None,
) -> dict[str, Any]:
    deidentification: dict[str, Any] = {
        "policy_id": "scaling-neuro.dicom-deidentification",
        "policy_version": "2.0.0",
        "method": "scaling-neuro-recursive-allowlist-v2",
        "recursive": True,
        "private_text_removed": True,
        "unknown_private_removed": True,
        "uids_remapped": True,
        "pixel_data_retained": True,
        "burned_in_annotation_status": "verified_no",
        "defacing_performed": False,
        "recognizable_visual_features": "may_be_present",
    }
    if safe_private_exceptions:
        deidentification["safe_private_exceptions"] = safe_private_exceptions
    return {
        "schema_version": "2.0.0",
        "series_archive_id": archive_id,
        "series_id": SERIES_ID,
        "subject_id": SUBJECT_ID,
        "session_id": SESSION_ID,
        "protocol_group_id": PROTOCOL_ID,
        "modality": "mr",
        "series_kind": series_kind,
        "processing_route": processing_route,
        "pixel_data_policy": "scanner-native-not-defaced",
        "dicom_instance_count": 1,
        "writer_contract": {"name": "neuro-sync", "version": "2.0.0"},
        "deidentification": deidentification,
        "source": source
        or {
            "dicom_count": 1,
            "manufacturer": "GE MEDICAL SYSTEMS",
            "model": "Discovery MR750",
            "software_versions": ["GE DV26"],
            "scanning_sequence": ["GR"],
            "sequence_name": "mprage",
            "mr_acquisition_type": "3D",
            "image_type": ["ORIGINAL", "PRIMARY", "M", "T1"],
            "series_number": 7,
        },
        "classification": {
            "decision": "accepted",
            "kind": series_kind,
            "confidence": 0.98,
            "evidence": [
                {
                    "code": evidence_code,
                    "source": "dicom_header",
                    "effect": "supports",
                }
            ],
        },
        "instances": [
            {
                "path": "dicom/000001.dcm",
                "size_bytes": len(dicom),
                "sha256": hashlib.sha256(dicom).hexdigest(),
                "sop_instance_uid": SOP_UID,
            }
        ],
    }


def make_archive(
    path: Path,
    dicom: bytes,
    manifest: dict[str, Any],
    *,
    dicoms: list[bytes] | None = None,
    extra_member: tarfile.TarInfo | None = None,
    dicom_header_mutator: Callable[[bytearray], None] | None = None,
    dicom_padding_byte: int = 0,
) -> bytes:
    tar_path = path.with_suffix(".tar")
    archive = bytearray()
    for index, payload in enumerate(dicoms or [dicom], start=1):
        _append_canonical_member(
            archive,
            f"dicom/{index:06d}.dcm",
            payload,
            header_mutator=dicom_header_mutator,
            padding_byte=dicom_padding_byte,
        )
    if extra_member is not None:
        payload = b"x" * extra_member.size if extra_member.isfile() else b""
        if extra_member.isfile():
            _append_canonical_member(archive, extra_member.name, payload)
        else:
            archive.extend(extra_member.tobuf(format=tarfile.GNU_FORMAT))
            archive.extend(payload)
            archive.extend(b"\0" * ((-len(payload)) % 512))
    _append_canonical_member(archive, "manifest.json", canonical_json(manifest))
    archive.extend(b"\0" * 1024)
    tar_path.write_bytes(archive)
    subprocess.run(
        ["zstd", "-q", "-1", "-f", str(tar_path), "-o", str(path)], check=True
    )
    return path.read_bytes()


def nifti_bytes(*, volumes: int = 10, tr: float = 2.0) -> bytes:
    dimensions = (8, 8, 8, volumes)
    header = bytearray(352)
    struct.pack_into("<i", header, 0, 348)
    struct.pack_into("<8h", header, 40, 4, *dimensions, 1, 1, 1)
    struct.pack_into("<h", header, 70, 4)
    struct.pack_into("<h", header, 72, 16)
    struct.pack_into("<8f", header, 76, 1.0, 2.0, 2.0, 2.0, tr, 0.0, 0.0, 0.0)
    struct.pack_into("<f", header, 108, 352.0)
    header[123] = 2 | 8
    struct.pack_into("<h", header, 254, 1)
    struct.pack_into("<12f", header, 280, 2, 0, 0, 0, 0, 2, 0, 0, 0, 0, 2, 0)
    header[344:348] = b"n+1\0"
    voxel_count = 8 * 8 * 8 * volumes
    voxels = b"".join(struct.pack("<h", index % 101) for index in range(voxel_count))
    return bytes(header) + voxels


def gzip_bytes(raw: bytes) -> bytes:
    return gzip.compress(raw, compresslevel=6, mtime=0)


def fake_converter(path: Path) -> None:
    script = """#!/usr/bin/env python3
import json, pathlib, struct, sys
if "--version" in sys.argv:
    print("Chris Rorden's dcm2niix version v1.0.20260416")
    raise SystemExit(0)
out = pathlib.Path(sys.argv[sys.argv.index("-o") + 1])
out.mkdir(parents=True, exist_ok=True)
counter = pathlib.Path(sys.argv[0] + ".count")
counter.write_text(str((int(counter.read_text()) if counter.exists() else 0) + 1))
dims = (8, 8, 8, 10)
h = bytearray(352)
struct.pack_into("<i", h, 0, 348)
struct.pack_into("<8h", h, 40, 4, *dims, 1, 1, 1)
struct.pack_into("<h", h, 70, 4)
struct.pack_into("<h", h, 72, 16)
struct.pack_into("<8f", h, 76, 1.0, 2.0, 2.0, 2.0, 2.0, 0.0, 0.0, 0.0)
struct.pack_into("<f", h, 108, 352.0)
h[123] = 10
struct.pack_into("<h", h, 254, 1)
struct.pack_into("<12f", h, 280, 2,0,0,0, 0,2,0,0, 0,0,2,0)
h[344:348] = b"n+1\\0"
voxels = b"".join(struct.pack("<h", i % 101) for i in range(8*8*8*10))
(out / "series.nii").write_bytes(bytes(h) + voxels)
(out / "series.json").write_text(json.dumps({"RepetitionTime":2.0,"EchoTime":0.03,"PhaseEncodingDirection":"j-","SliceTiming":[0.0,1.0]}))
"""
    path.write_text(script)
    path.chmod(0o755)


def legacy_sidecar(raw_nifti: bytes, compressed: bytes) -> dict[str, Any]:
    from scaling_neuro_processor.converter import NORMALIZED_ARGUMENTS
    from scaling_neuro_processor.nifti import inspect_nifti_stream

    facts = inspect_nifti_stream(io.BytesIO(raw_nifti))
    return {
        "schema_version": "1.0.0",
        "bundle_id": ARCHIVE_ID,
        "subject_id": SUBJECT_ID,
        "session_id": SESSION_ID,
        "series_id": SERIES_ID,
        "protocol_group_id": PROTOCOL_ID,
        "modality": "bold",
        "source": {"dicom_count": 100},
        "image": {**facts.image_dict(), "te_seconds": 0.03},
        "files": {
            "nifti": {
                "filename": "legacy.nii.gz",
                "size_bytes": len(compressed),
                "sha256": hashlib.sha256(compressed).hexdigest(),
                "uncompressed_sha256": hashlib.sha256(raw_nifti).hexdigest(),
            }
        },
        "conversion": {
            "client_version": "0.2.4",
            "converter": "dcm2niix",
            "converter_version": "v1.0.20260416",
            "arguments": NORMALIZED_ARGUMENTS,
        },
        "classification": {
            "decision": "accepted",
            "kind": "functional_epi",
            "confidence": 0.99,
            "evidence": [
                {
                    "code": "dicom.epi",
                    "source": "dicom_header",
                    "effect": "supports",
                }
            ],
        },
        "qc": {
            "passed": True,
            "checks": [{"code": "nifti.header", "status": "pass"}],
            "warnings": [],
        },
        "metadata_policy": {
            "policy_id": "scaling-neuro-epi-default-deny",
            "policy_version": "1.1.0",
        },
    }
