from __future__ import annotations

from dataclasses import dataclass, field
import io
import math
from numbers import Integral
from pathlib import Path
import re
import struct
from typing import Any, BinaryIO

from pydicom import config as pydicom_config
from pydicom import dcmread
from pydicom.dataelem import DataElement
from pydicom.dataset import Dataset, FileMetaDataset
from pydicom.errors import InvalidDicomError
from pydicom.multival import MultiValue
from pydicom.sequence import Sequence
from pydicom.tag import BaseTag, Tag
from pydicom.uid import UID

from .errors import InvalidArchive


PRIVACY_ERROR = "DICOM_PRIVACY_AUDIT_FAILED"
MAX_SEQUENCE_DEPTH = 32
MAX_METADATA_BYTES = 256 * 1024**2
MAX_ELEMENTS = 250_000
MAX_SEQUENCE_ITEMS = 100_000
MAX_VALUE_MULTIPLICITY = 65_536
IMPLEMENTATION_CLASS_UID = "2.25.323468694959424494117938985101850441847"
IMPLEMENTATION_VERSION_NAME = "NEUROSYNC_RAW_1"
DEIDENTIFICATION_METHOD = "Scaling Neuro scaling-neuro.dicom-deidentification 1.0.0"
REMAPPED_UID_RE = re.compile(r"^2\.25\.(?:0|[1-9][0-9]{0,38})$")
UID_RE = re.compile(r"^[0-9]+(?:\.[0-9]+)+$")
SUPPORTED_MR_SOP_CLASSES = {
    "1.2.840.10008.5.1.4.1.1.4",
    "1.2.840.10008.5.1.4.1.1.4.1",
    "1.2.840.10008.5.1.4.1.1.4.4",
}
SOFTWARE_VERSION_RE = re.compile(
    r"^(?:Siemens (?:[A-E][0-9]{2}[A-Z]?|V[A-E][0-9]{2}[A-Z]?|X[AB][0-9]{2}[A-Z]?)|"
    r"(?:Philips|Canon/Toshiba|United Imaging|Bruker) [1-9][0-9]?(?:\.[0-9]{1,2}){1,3}|"
    r"GE (?:DV[0-9]{1,2}(?:\.[0-9]{1,2})?|[1-9][0-9]?(?:\.[0-9]{1,2}){1,3}))$"
)
CANONICAL_COIL_RE = re.compile(
    r"^(?:HEAD(?:_NECK)?|NECK|BODY|SPINE|KNEE|FLEX|BREAST|CARDIAC|FOOT|ANKLE|SHOULDER|WRIST)"
    r"(?:_(?:[1-9]|[1-9][0-9]|1[0-9]{2}|2[0-4][0-9]|25[0-6]))?$"
)

SIEMENS_CSA_CREATOR_TAG = Tag(0x0029, 0x0010)
SIEMENS_CSA_DATA_TAG = Tag(0x0029, 0x1010)
SIEMENS_CSA_CREATOR = "SIEMENS CSA HEADER"
PHILIPS_MR_CREATOR = "Philips MR Imaging DD 001"
PHILIPS_PER_FRAME_CREATOR = "Philips MR Imaging DD 005"
PHILIPS_IMAGING_CREATOR = "Philips Imaging DD 001"
SAFE_PRIVATE_EXCEPTION_ORDER = (
    "siemens_csa_image_header_numeric_v1",
    "dicom_ps3.15_philips_scale_intercept_slope",
    "dicom_ps3.15_philips_number_of_slices",
    "dicom_ps3.15_philips_water_fat_shift",
    "dicom_ps3.15_philips_per_frame_scale_slope",
)
SAFE_PRIVATE_EXCEPTIONS = frozenset(SAFE_PRIVATE_EXCEPTION_ORDER)
SAFE_PRIVATE_CREATORS = {
    SIEMENS_CSA_CREATOR,
    PHILIPS_MR_CREATOR,
    PHILIPS_PER_FRAME_CREATOR,
    PHILIPS_IMAGING_CREATOR,
}
SIEMENS_CSA_FIELDS = (
    ("NumberOfImagesInMosaic", b"US\0\0"),
    ("SliceNormalVector", b"DS\0\0"),
    ("SliceMeasurementDuration", b"DS\0\0"),
    ("BandwidthPerPixelPhaseEncode", b"DS\0\0"),
    ("MosaicRefAcqTimes", b"DS\0\0"),
    ("ProtocolSliceNumber", b"IS\0\0"),
    ("PhaseEncodingDirectionPositive", b"IS\0\0"),
)
CANONICAL_MANUFACTURERS = {
    "SIEMENS",
    "Philips Medical Systems",
    "GE MEDICAL SYSTEMS",
    "Canon/Toshiba",
    "United Imaging",
    "Bruker",
}
CANONICAL_MODELS = {
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
CANONICAL_SEQUENCE_NAMES = {
    "ep2d_bold",
    "epfid_bold",
    "bold",
    "fmri",
    "ep2d",
    "epfid",
    "epi",
}

CODE_VALUES = {
    Tag(0x0008, 0x0008): {
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
    Tag(0x0008, 0x0060): {"MR"},
    Tag(0x0008, 0x9205): {"COLOR", "MONOCHROME", "MIXED"},
    Tag(0x0008, 0x9206): {"VOLUME", "SAMPLED", "DISTORTED", "MIXED"},
    Tag(0x0008, 0x9207): {
        "NONE",
        "RECON_TOMOGRAPHIC",
        "RECON_PROJECTION",
        "RECON_PLANAR",
    },
    Tag(0x0008, 0x9208): {"MAGNITUDE", "PHASE", "REAL", "IMAGINARY", "MIXED"},
    Tag(0x0008, 0x9209): {
        "UNKNOWN",
        "NONE",
        "T1",
        "T2",
        "T2_STAR",
        "PROTON_DENSITY",
        "DIFFUSION",
        "FLOW_ENCODED",
        "FLUID_ATTENUATED",
        "PERFUSION",
    },
    Tag(0x0018, 0x0020): {"SE", "IR", "GR", "EP", "RM"},
    Tag(0x0018, 0x0021): {"SK", "MTC", "SS", "TRSS", "SP", "MP", "OSP", "NONE"},
    Tag(0x0018, 0x0022): {"PER", "RG", "CG", "PPG", "FC", "PFF", "PFP", "SP", "FS"},
    Tag(0x0018, 0x0023): {"2D", "3D"},
    Tag(0x0018, 0x0025): {"Y", "N"},
    Tag(0x0018, 0x1312): {"ROW", "COL"},
    Tag(0x0018, 0x5100): {"HFP", "HFS", "HFDR", "HFDL", "FFDR", "FFDL", "FFP", "FFS"},
    Tag(0x0018, 0x9036): {"PHASE", "FREQUENCY", "SLICE", "COMBINATION"},
    Tag(0x0018, 0x9018): {"YES", "NO"},
    Tag(0x0018, 0x9034): {"LINEAR", "REVERSE_LINEAR", "CENTRIC", "REVERSE_CENTRIC"},
    Tag(0x0018, 0x9078): {"SENSE", "GRAPPA", "ASSET", "SMASH", "OTHER", "NONE"},
    Tag(0x0028, 0x0004): {
        "MONOCHROME1",
        "MONOCHROME2",
        "PALETTE COLOR",
        "RGB",
        "YBR_FULL",
        "YBR_FULL_422",
    },
    Tag(0x0028, 0x2110): {"00", "01"},
    Tag(0x0028, 0x2114): {
        "ISO_10918_1",
        "ISO_14495_1",
        "ISO_15444_1",
        "ISO_15444_2",
        "ISO_13818_2",
        "ISO_14496_10",
    },
    Tag(0x2050, 0x0020): {"IDENTITY", "INVERSE", "LIN OD"},
}

IDENTITY_TAGS = {
    Tag(0x0010, 0x0010),
    Tag(0x0010, 0x0020),
    Tag(0x0012, 0x0062),
    Tag(0x0012, 0x0063),
    Tag(0x0028, 0x0303),
}
SEMANTIC_UID_TAGS = {
    Tag(0x0008, 0x0016),
    Tag(0x0008, 0x001A),
    Tag(0x0008, 0x001B),
    Tag(0x0008, 0x010C),
    Tag(0x0008, 0x1150),
}
REQUIRED_TAGS = {
    Tag(0x0008, 0x0008),
    Tag(0x0008, 0x0016),
    Tag(0x0008, 0x0018),
    Tag(0x0008, 0x0060),
    Tag(0x0010, 0x0010),
    Tag(0x0010, 0x0020),
    Tag(0x0012, 0x0062),
    Tag(0x0012, 0x0063),
    Tag(0x0028, 0x0303),
}


class _PrivacyViolation(Exception):
    pass


class _BoundedReader:
    def __init__(self, stream: BinaryIO, maximum: int):
        self.stream = stream
        self.maximum = maximum

    def read(self, size: int = -1) -> bytes:
        remaining = self.maximum - self.stream.tell()
        if remaining < 0 or size < 0 or size > remaining:
            raise _PrivacyViolation()
        return self.stream.read(size)

    def seek(self, offset: int, whence: int = io.SEEK_SET) -> int:
        current = self.stream.tell()
        if whence == io.SEEK_SET:
            target = offset
        elif whence == io.SEEK_CUR:
            target = current + offset
        elif whence == io.SEEK_END:
            target = self.stream.seek(offset, whence)
            if target > self.maximum:
                raise _PrivacyViolation()
            return target
        else:
            raise _PrivacyViolation()
        if not 0 <= target <= self.maximum:
            raise _PrivacyViolation()
        return self.stream.seek(offset, whence)

    def tell(self) -> int:
        return self.stream.tell()

    def readable(self) -> bool:
        return True

    def seekable(self) -> bool:
        return True


@dataclass
class _AuditState:
    elements: int = 0
    sequence_items: int = 0
    private_exceptions: set[str] = field(default_factory=set)
    philips_private_fields: set[str] = field(default_factory=set)
    trigger_time_present: bool = False


@dataclass(frozen=True)
class DicomAudit:
    sop_instance_uid: str
    sop_class_uid: str
    manufacturer: str | None
    model: str | None
    software_versions: frozenset[str]
    image_type: frozenset[str]
    scanning_sequence: frozenset[str]
    sequence_name: str | None
    mr_acquisition_type: str | None
    echo_planar_pulse_sequence: str | None
    repetition_time_ms: float | None
    echo_times_ms: frozenset[float]
    acquisition_number: int | None
    temporal_position_identifier: int | None
    temporal_position_indices: frozenset[int]
    number_of_temporal_positions: int | None
    image_positions: frozenset[tuple[float, float, float]]
    image_position_count: int
    acquisition_contrast: frozenset[str]
    diffusion_b_value: float | None
    asl_technique_present: bool
    burned_in_annotation_declared_no: bool
    private_exceptions: frozenset[str]
    philips_private_fields: frozenset[str]
    trigger_time_present: bool


def _components(value: Any) -> list[Any]:
    if isinstance(value, (MultiValue, list, tuple)):
        values = list(value)
    else:
        values = [value]
    if not 1 <= len(values) <= MAX_VALUE_MULTIPLICITY:
        raise _PrivacyViolation()
    return values


def _text_components(element: DataElement) -> list[str]:
    values = [str(value).strip(" \0") for value in _components(element.value)]
    if any(not value or len(value) > 96 for value in values):
        raise _PrivacyViolation()
    return values


def _valid_uid(value: str) -> bool:
    return (
        1 <= len(value) <= 64
        and UID_RE.fullmatch(value) is not None
        and not value.startswith(".")
        and not value.endswith(".")
        and ".." not in value
    )


def _optional_text(dataset: Dataset, tag: BaseTag) -> list[str]:
    element = dataset.get(tag)
    if not isinstance(element, DataElement):
        return []
    return _text_components(element)


def _optional_single_text(dataset: Dataset, tag: BaseTag) -> str | None:
    values = _optional_text(dataset, tag)
    if not values:
        return None
    if len(values) != 1:
        raise _PrivacyViolation()
    return values[0]


def _recursive_elements(
    dataset: Dataset, tag: BaseTag, depth: int = 0
) -> list[DataElement]:
    if depth > MAX_SEQUENCE_DEPTH:
        raise _PrivacyViolation()
    output: list[DataElement] = []
    for element in dataset:
        if element.tag == tag:
            output.append(element)
        if element.VR == "SQ":
            if not isinstance(element.value, Sequence):
                raise _PrivacyViolation()
            for item in element.value:
                if not isinstance(item, Dataset):
                    raise _PrivacyViolation()
                output.extend(_recursive_elements(item, tag, depth + 1))
    return output


def _recursive_single_text(dataset: Dataset, tag: BaseTag) -> str | None:
    values = [
        value
        for element in _recursive_elements(dataset, tag)
        for value in _text_components(element)
    ]
    if not values:
        return None
    if any(value != values[0] for value in values[1:]):
        raise _PrivacyViolation()
    return values[0]


def _recursive_float(dataset: Dataset, tag: BaseTag) -> float | None:
    value = _recursive_single_text(dataset, tag)
    if value is None:
        return None
    try:
        parsed = float(value)
    except (TypeError, ValueError, OverflowError) as exc:
        raise _PrivacyViolation() from exc
    if not math.isfinite(parsed):
        raise _PrivacyViolation()
    return parsed


def _recursive_floats(dataset: Dataset, tag: BaseTag) -> frozenset[float]:
    output: set[float] = set()
    for element in _recursive_elements(dataset, tag):
        for value in _text_components(element):
            try:
                parsed = float(value)
            except (TypeError, ValueError, OverflowError) as exc:
                raise _PrivacyViolation() from exc
            if not math.isfinite(parsed):
                raise _PrivacyViolation()
            output.add(parsed)
    return frozenset(output)


def _recursive_integers(dataset: Dataset, tag: BaseTag) -> frozenset[int]:
    output: set[int] = set()
    for element in _recursive_elements(dataset, tag):
        for value in _text_components(element):
            try:
                output.add(int(value))
            except (TypeError, ValueError, OverflowError) as exc:
                raise _PrivacyViolation() from exc
    return frozenset(output)


def _recursive_image_positions(
    dataset: Dataset,
) -> tuple[frozenset[tuple[float, float, float]], int]:
    output: set[tuple[float, float, float]] = set()
    count = 0
    for element in _recursive_elements(dataset, Tag(0x0020, 0x0032)):
        values = _text_components(element)
        if len(values) != 3:
            raise _PrivacyViolation()
        try:
            position = tuple(float(value) for value in values)
        except (TypeError, ValueError, OverflowError) as exc:
            raise _PrivacyViolation() from exc
        if len(position) != 3 or any(not math.isfinite(value) for value in position):
            raise _PrivacyViolation()
        output.add((position[0], position[1], position[2]))
        count += 1
    return frozenset(output), count


def _optional_float(dataset: Dataset, tag: BaseTag) -> float | None:
    values = _optional_text(dataset, tag)
    if not values:
        return None
    if len(values) != 1:
        raise _PrivacyViolation()
    try:
        value = float(values[0])
    except (TypeError, ValueError, OverflowError) as exc:
        raise _PrivacyViolation() from exc
    if not math.isfinite(value):
        raise _PrivacyViolation()
    return value


def _optional_int(dataset: Dataset, tag: BaseTag) -> int | None:
    values = _optional_text(dataset, tag)
    if not values:
        return None
    if len(values) != 1:
        raise _PrivacyViolation()
    try:
        return int(values[0])
    except (TypeError, ValueError, OverflowError) as exc:
        raise _PrivacyViolation() from exc


def _public_attribute_allowed(tag: BaseTag, vr: str) -> bool:
    group = tag.group
    element = tag.element
    if group == 0x0008:
        return element in {
            0x0005,
            0x0008,
            0x0016,
            0x0018,
            0x001A,
            0x001B,
            0x0060,
            0x0070,
            0x1090,
            0x1115,
            0x1140,
            0x1150,
            0x1155,
            0x1160,
            0x9007,
            0x9205,
            0x9206,
            0x9207,
            0x9208,
            0x9209,
        }
    if group == 0x0018:
        classic = {
            0x0020,
            0x0021,
            0x0022,
            0x0023,
            0x0024,
            0x0025,
            0x0050,
            0x0080,
            0x0081,
            0x0082,
            0x0083,
            0x0084,
            0x0085,
            0x0086,
            0x0087,
            0x0088,
            0x0089,
            0x0091,
            0x0093,
            0x0094,
            0x0095,
            0x1020,
            0x1060,
            0x1062,
            0x1250,
            0x1251,
            0x1310,
            0x1312,
            0x1314,
            0x1315,
            0x5100,
        }
        enhanced_vrs = {
            "SQ",
            "CS",
            "UI",
            "AT",
            "US",
            "SS",
            "UL",
            "SL",
            "UV",
            "SV",
            "FL",
            "FD",
            "IS",
            "DS",
        }
        return element in classic or element >= 0x9000 and vr in enhanced_vrs
    if group == 0x0020:
        return element in {
            0x000D,
            0x000E,
            0x0011,
            0x0012,
            0x0013,
            0x0032,
            0x0037,
            0x0052,
            0x0100,
            0x0105,
            0x1002,
            0x1041,
            0x9056,
            0x9057,
            0x9111,
            0x9113,
            0x9116,
            0x9128,
            0x9156,
            0x9157,
            0x9161,
            0x9164,
            0x9165,
            0x9221,
            0x9222,
        }
    if group == 0x0028:
        if element in {0x0300, 0x0302}:
            return False
        return (
            0x0002 <= element <= 0x0009
            or 0x0010 <= element <= 0x0014
            or element in {0x0030, 0x0031, 0x0034, 0x0301, 0x0303}
            or 0x0100 <= element <= 0x0103
            or 0x0106 <= element <= 0x0121
            or 0x1050 <= element <= 0x1055
            or 0x1101 <= element <= 0x1223
            or 0x2000 <= element <= 0x3010
            or element in {0x9110, 0x9132, 0x9145}
            or vr == "SQ"
            and element in {0x3000, 0x3010}
        )
    if group == 0x0040:
        return element in {0x9094, 0x9210, 0x9211, 0x9212, 0x9216}
    if group == 0x2050:
        return element == 0x0020
    if group == 0x5200:
        return element in {0x9229, 0x9230}
    return False


def _audit_numeric(element: DataElement) -> None:
    values = _components(element.value)
    if element.VR in {"US", "SS", "UL", "SL", "UV", "SV"}:
        if any(
            isinstance(value, bool) or not isinstance(value, Integral)
            for value in values
        ):
            raise _PrivacyViolation()
    elif element.VR in {"FL", "FD", "DS"}:
        for value in values:
            try:
                parsed = float(value)
            except (TypeError, ValueError, OverflowError) as exc:
                raise _PrivacyViolation() from exc
            if not math.isfinite(parsed):
                raise _PrivacyViolation()
    elif element.VR == "IS":
        for value in values:
            if isinstance(value, bool):
                raise _PrivacyViolation()
            try:
                int(str(value))
            except (TypeError, ValueError, OverflowError) as exc:
                raise _PrivacyViolation() from exc
    elif element.VR == "AT":
        if any(not isinstance(value, BaseTag) for value in values):
            raise _PrivacyViolation()
    else:
        raise _PrivacyViolation()


def _audit_special_text(element: DataElement, expected_subject_id: str) -> bool:
    tag = element.tag
    if tag not in {
        Tag(0x0010, 0x0010),
        Tag(0x0010, 0x0020),
        Tag(0x0012, 0x0062),
        Tag(0x0012, 0x0063),
        Tag(0x0028, 0x0303),
        Tag(0x0028, 0x0301),
        Tag(0x0008, 0x0070),
        Tag(0x0008, 0x1090),
        Tag(0x0018, 0x0024),
        Tag(0x0018, 0x1020),
        Tag(0x0018, 0x1250),
        Tag(0x0018, 0x1251),
        Tag(0x0018, 0x0085),
    }:
        return False
    values = _text_components(element)
    expected_vr: str | None = None
    valid = False
    if tag == Tag(0x0010, 0x0010):
        expected_vr, valid = "PN", values == [expected_subject_id]
    elif tag == Tag(0x0010, 0x0020):
        expected_vr, valid = "LO", values == [expected_subject_id]
    elif tag == Tag(0x0012, 0x0062):
        expected_vr, valid = "CS", values == ["YES"]
    elif tag == Tag(0x0012, 0x0063):
        expected_vr, valid = "LO", values == [DEIDENTIFICATION_METHOD]
    elif tag == Tag(0x0028, 0x0303):
        expected_vr, valid = "CS", values == ["REMOVED"]
    elif tag == Tag(0x0028, 0x0301):
        expected_vr, valid = "CS", values == ["NO"]
    elif tag == Tag(0x0008, 0x0070):
        expected_vr, valid = (
            "LO",
            len(values) == 1 and values[0] in CANONICAL_MANUFACTURERS,
        )
    elif tag == Tag(0x0008, 0x1090):
        expected_vr, valid = "LO", len(values) == 1 and values[0] in CANONICAL_MODELS
    elif tag == Tag(0x0018, 0x0024):
        expected_vr, valid = (
            "SH",
            len(values) == 1 and values[0] in CANONICAL_SEQUENCE_NAMES,
        )
    elif tag == Tag(0x0018, 0x1020):
        expected_vr, valid = (
            "LO",
            len(values) <= 16
            and all(SOFTWARE_VERSION_RE.fullmatch(value) for value in values),
        )
    elif tag in {Tag(0x0018, 0x1250), Tag(0x0018, 0x1251)}:
        expected_vr, valid = (
            "SH",
            len(values) == 1 and CANONICAL_COIL_RE.fullmatch(values[0]) is not None,
        )
    elif tag == Tag(0x0018, 0x0085):
        expected_vr, valid = (
            "SH",
            values
            in [[item] for item in ("1H", "13C", "17O", "19F", "23Na", "31P", "129Xe")],
        )
    else:
        return False
    if element.VR != expected_vr or not valid:
        raise _PrivacyViolation()
    return True


def _validate_canonical_siemens_csa(value: Any) -> None:
    if not isinstance(value, bytes) or not 36 <= len(value) <= 16 * 1024**2:
        raise _PrivacyViolation()
    if value[:8] != b"SV10\x04\x03\x02\x01":
        raise _PrivacyViolation()
    tag_count, marker = struct.unpack_from("<II", value, 8)
    if not 1 <= tag_count <= len(SIEMENS_CSA_FIELDS) or marker != 77:
        raise _PrivacyViolation()
    cursor = 16
    observed: dict[str, list[float]] = {}
    field_order = {name: index for index, (name, _) in enumerate(SIEMENS_CSA_FIELDS)}
    previous_order = -1
    for _ in range(tag_count):
        if cursor + 84 > len(value):
            raise _PrivacyViolation()
        header = value[cursor : cursor + 84]
        cursor += 84
        try:
            name_end = header[:64].index(0)
            name = header[:name_end].decode("ascii")
        except (UnicodeDecodeError, ValueError) as exc:
            raise _PrivacyViolation() from exc
        if (
            not name
            or any(header[name_end + 1 : 64])
            or name not in field_order
            or field_order[name] <= previous_order
            or name in observed
        ):
            raise _PrivacyViolation()
        previous_order = field_order[name]
        expected_vr = dict(SIEMENS_CSA_FIELDS)[name]
        vm, vr, syngodt, item_count, header_marker = struct.unpack_from(
            "<i4siii", header, 64
        )
        expected_item_count = (
            3
            if name in {"SliceMeasurementDuration", "BandwidthPerPixelPhaseEncode"}
            else vm
        )
        if (
            vr != expected_vr
            or not 1 <= vm <= 4096
            or item_count != expected_item_count
            or not 1 <= item_count <= 4096
            or header_marker != 77
            or syngodt != 0
        ):
            raise _PrivacyViolation()
        numbers: list[float] = []
        for item_index in range(item_count):
            if cursor + 16 > len(value):
                raise _PrivacyViolation()
            first_length, length, item_marker, reserved = struct.unpack_from(
                "<iiii", value, cursor
            )
            cursor += 16
            if item_index >= vm:
                if (first_length, length, item_marker, reserved) != (0, 0, 77, 0):
                    raise _PrivacyViolation()
                continue
            if (
                first_length != length
                or not 2 <= length <= 64
                or item_marker != 77
                or reserved != 0
                or cursor + length > len(value)
            ):
                raise _PrivacyViolation()
            raw = value[cursor : cursor + length]
            cursor += length
            padding = (-length) % 4
            if cursor + padding > len(value) or any(value[cursor : cursor + padding]):
                raise _PrivacyViolation()
            cursor += padding
            if raw[-1:] != b"\0" or b"\0" in raw[:-1]:
                raise _PrivacyViolation()
            try:
                text = raw[:-1].decode("ascii")
            except UnicodeDecodeError as exc:
                raise _PrivacyViolation() from exc
            if not text or any(
                character not in "0123456789+-.eE" for character in text
            ):
                raise _PrivacyViolation()
            try:
                number = float(text)
            except ValueError as exc:
                raise _PrivacyViolation() from exc
            if not math.isfinite(number):
                raise _PrivacyViolation()
            numbers.append(number)
        if len(numbers) != vm:
            raise _PrivacyViolation()
        observed[name] = numbers
    if cursor != len(value) or "NumberOfImagesInMosaic" not in observed:
        raise _PrivacyViolation()

    def integers(items: list[float], minimum: int, maximum: int) -> bool:
        return all(item.is_integer() and minimum <= item <= maximum for item in items)

    for name, numbers in observed.items():
        if name == "NumberOfImagesInMosaic":
            valid = len(numbers) == 1 and integers(numbers, 2, 4096)
        elif name == "SliceNormalVector":
            valid = len(numbers) == 3 and all(-1.1 <= item <= 1.1 for item in numbers)
        elif name in {
            "SliceMeasurementDuration",
            "BandwidthPerPixelPhaseEncode",
        }:
            valid = 1 <= len(numbers) <= 3 and all(
                0 <= item <= 1.0e12 for item in numbers
            )
        elif name == "MosaicRefAcqTimes":
            valid = 4 <= len(numbers) <= 4096 and all(
                -1.0e9 <= item <= 1.0e9 for item in numbers
            )
        elif name == "ProtocolSliceNumber":
            valid = len(numbers) == 1 and integers(numbers, 0, 4096)
        elif name == "PhaseEncodingDirectionPositive":
            valid = len(numbers) == 1 and numbers[0] in {0.0, 1.0}
        else:
            valid = False
        if not valid:
            raise _PrivacyViolation()


def _private_creator_tag(tag: BaseTag) -> BaseTag:
    block = tag.element >> 8
    if not 0x10 <= block <= 0xFF:
        raise _PrivacyViolation()
    return Tag(tag.group, block)


def _audit_finite_float32_vm1(element: DataElement) -> float:
    values = _components(element.value)
    if element.VR != "FL" or len(values) != 1:
        raise _PrivacyViolation()
    try:
        value = float(values[0])
    except (TypeError, ValueError, OverflowError) as exc:
        raise _PrivacyViolation() from exc
    if not math.isfinite(value):
        raise _PrivacyViolation()
    return value


def _audit_positive_i32_vm1(element: DataElement) -> None:
    values = _components(element.value)
    if (
        element.VR != "SL"
        or len(values) != 1
        or isinstance(values[0], bool)
        or not isinstance(values[0], Integral)
    ):
        raise _PrivacyViolation()
    if not 1 <= int(values[0]) <= 4096:
        raise _PrivacyViolation()


def _record_philips_private_field(
    state: _AuditState, semantic_fields: set[str], field_name: str
) -> None:
    # Enhanced MR legitimately repeats a narrowly allowed scale attribute in
    # each per-frame Dataset. Reject duplicate semantic fields within one
    # Dataset, while retaining a file-level inventory for manifest attestation.
    if field_name in semantic_fields:
        raise _PrivacyViolation()
    semantic_fields.add(field_name)
    state.philips_private_fields.add(field_name)


def _audit_philips_per_frame_scale_sequence(
    element: DataElement,
    state: _AuditState,
    depth: int,
) -> None:
    if (
        element.VR != "SQ"
        or not isinstance(element.value, Sequence)
        or not element.value
        or depth >= MAX_SEQUENCE_DEPTH
    ):
        raise _PrivacyViolation()
    state.sequence_items += len(element.value)
    if state.sequence_items > MAX_SEQUENCE_ITEMS:
        raise _PrivacyViolation()
    for item in element.value:
        if not isinstance(item, Dataset) or len(item) != 2:
            raise _PrivacyViolation()
        state.elements += len(item)
        if state.elements > MAX_ELEMENTS:
            raise _PrivacyViolation()
        creators = [
            candidate
            for candidate in item
            if candidate.tag.group == 0x2005
            and 0x0010 <= candidate.tag.element <= 0x00FF
        ]
        if len(creators) != 1:
            raise _PrivacyViolation()
        creator = creators[0]
        if creator.VR != "LO" or _text_components(creator) != [PHILIPS_MR_CREATOR]:
            raise _PrivacyViolation()
        expected_creator_tag = creator.tag
        scales = [candidate for candidate in item if candidate.tag != creator.tag]
        if len(scales) != 1:
            raise _PrivacyViolation()
        scale = scales[0]
        if (
            scale.tag.group != 0x2005
            or scale.tag.element & 0x00FF != 0x000E
            or _private_creator_tag(scale.tag) != expected_creator_tag
        ):
            raise _PrivacyViolation()
        slope = _audit_finite_float32_vm1(scale)
        if not 0.0 < slope <= 1.0e9:
            raise _PrivacyViolation()


def _audit_private(
    element: DataElement,
    creators: dict[BaseTag, str],
    state: _AuditState,
    semantic_fields: set[str],
    depth: int,
) -> tuple[BaseTag, str]:
    tag = element.tag
    creator_tag = _private_creator_tag(tag)
    creator = creators.get(creator_tag)
    if tag == SIEMENS_CSA_DATA_TAG and creator_tag == SIEMENS_CSA_CREATOR_TAG:
        if element.VR != "OB" or creator != SIEMENS_CSA_CREATOR:
            raise _PrivacyViolation()
        _validate_canonical_siemens_csa(element.value)
        return creator_tag, "siemens_csa_image_header_numeric_v1"
    if (
        tag.group == 0x2005
        and tag.element & 0x00FF in {0x000D, 0x000E}
        and creator == PHILIPS_MR_CREATOR
    ):
        value = _audit_finite_float32_vm1(element)
        if tag.element & 0x00FF == 0x000D:
            field_name = "scale_intercept"
            if abs(value) > 1.0e9:
                raise _PrivacyViolation()
        else:
            field_name = "scale_slope"
            if not 0.0 < value <= 1.0e9:
                raise _PrivacyViolation()
        _record_philips_private_field(state, semantic_fields, field_name)
        return creator_tag, "dicom_ps3.15_philips_scale_intercept_slope"
    if (
        tag.group == 0x2001
        and tag.element & 0x00FF == 0x0018
        and creator == PHILIPS_IMAGING_CREATOR
    ):
        _audit_positive_i32_vm1(element)
        _record_philips_private_field(state, semantic_fields, "number_of_slices")
        return creator_tag, "dicom_ps3.15_philips_number_of_slices"
    if (
        tag.group == 0x2001
        and tag.element & 0x00FF == 0x0022
        and creator == PHILIPS_IMAGING_CREATOR
    ):
        value = _audit_finite_float32_vm1(element)
        if not 0.0 <= value <= 1.0e6:
            raise _PrivacyViolation()
        _record_philips_private_field(state, semantic_fields, "water_fat_shift")
        return creator_tag, "dicom_ps3.15_philips_water_fat_shift"
    if (
        tag.group == 0x2005
        and tag.element & 0x00FF == 0x000F
        and creator == PHILIPS_PER_FRAME_CREATOR
    ):
        _audit_philips_per_frame_scale_sequence(element, state, depth)
        _record_philips_private_field(state, semantic_fields, "per_frame_scale_slope")
        return creator_tag, "dicom_ps3.15_philips_per_frame_scale_slope"
    raise _PrivacyViolation()


def _audit_dataset(
    dataset: Dataset,
    expected_subject_id: str,
    state: _AuditState,
    depth: int,
) -> None:
    if depth > MAX_SEQUENCE_DEPTH:
        raise _PrivacyViolation()
    state.elements += len(dataset)
    if state.elements > MAX_ELEMENTS:
        raise _PrivacyViolation()
    creators: dict[BaseTag, str] = {}
    for element in dataset:
        tag = element.tag
        if tag.group % 2 == 1 and 0x0010 <= tag.element <= 0x00FF:
            values = _text_components(element)
            if (
                element.VR != "LO"
                or len(values) != 1
                or values[0] not in SAFE_PRIVATE_CREATORS
            ):
                raise _PrivacyViolation()
            creators[tag] = values[0]
    used_creators: set[BaseTag] = set()
    semantic_private_fields: set[str] = set()
    for element in dataset:
        tag = element.tag
        if (
            0x5000 <= tag.group <= 0x501E
            or 0x6000 <= tag.group <= 0x601E
            or tag.group == 0x0070
        ):
            raise _PrivacyViolation()
        if element.VR in {"DA", "DT", "TM"}:
            raise _PrivacyViolation()
        if tag == Tag(0x0018, 0x1060):
            state.trigger_time_present = True
        if tag.group % 2 == 1:
            if tag in creators:
                continue
            creator_tag, exception = _audit_private(
                element,
                creators,
                state,
                semantic_private_fields,
                depth,
            )
            used_creators.add(creator_tag)
            state.private_exceptions.add(exception)
            continue
        if tag in IDENTITY_TAGS or tag in {
            Tag(0x0028, 0x0301),
            Tag(0x0028, 0x0303),
        }:
            _audit_special_text(element, expected_subject_id)
            continue
        if not _public_attribute_allowed(tag, element.VR):
            raise _PrivacyViolation()
        if _audit_special_text(element, expected_subject_id):
            continue
        if element.VR == "SQ":
            if not isinstance(element.value, Sequence):
                raise _PrivacyViolation()
            state.sequence_items += len(element.value)
            if state.sequence_items > MAX_SEQUENCE_ITEMS:
                raise _PrivacyViolation()
            for item in element.value:
                if not isinstance(item, Dataset):
                    raise _PrivacyViolation()
                _audit_dataset(item, expected_subject_id, state, depth + 1)
        elif element.VR == "UI":
            values = _text_components(element)
            if any(not _valid_uid(value) for value in values):
                raise _PrivacyViolation()
            if tag in SEMANTIC_UID_TAGS and any(
                not value.startswith("1.2.840.10008.") for value in values
            ):
                raise _PrivacyViolation()
            if tag not in SEMANTIC_UID_TAGS and any(
                REMAPPED_UID_RE.fullmatch(value) is None for value in values
            ):
                raise _PrivacyViolation()
        elif element.VR == "CS":
            values = _text_components(element)
            allowed = CODE_VALUES.get(tag)
            if (
                allowed is None
                or any(value not in allowed for value in values)
                or len(set(values)) != len(values)
            ):
                raise _PrivacyViolation()
        else:
            _audit_numeric(element)
    if set(creators) != used_creators:
        raise _PrivacyViolation()


def _audit_file_meta(
    file_meta: FileMetaDataset,
    dataset: Dataset,
) -> UID:
    allowed = {
        Tag(0x0002, 0x0000),
        Tag(0x0002, 0x0001),
        Tag(0x0002, 0x0002),
        Tag(0x0002, 0x0003),
        Tag(0x0002, 0x0010),
        Tag(0x0002, 0x0012),
        Tag(0x0002, 0x0013),
    }
    required = allowed - {Tag(0x0002, 0x0000)}
    if not required.issubset(file_meta.keys()) or set(file_meta.keys()) - allowed:
        raise _PrivacyViolation()
    if bytes(file_meta.FileMetaInformationVersion) != b"\x00\x01":
        raise _PrivacyViolation()
    sop_class = str(dataset[Tag(0x0008, 0x0016)].value).strip(" \0")
    sop_instance = str(dataset[Tag(0x0008, 0x0018)].value).strip(" \0")
    if (
        not _valid_uid(sop_class)
        or REMAPPED_UID_RE.fullmatch(sop_instance) is None
        or str(file_meta.MediaStorageSOPClassUID) != sop_class
        or str(file_meta.MediaStorageSOPInstanceUID) != sop_instance
        or str(file_meta.ImplementationClassUID) != IMPLEMENTATION_CLASS_UID
        or str(file_meta.ImplementationVersionName) != IMPLEMENTATION_VERSION_NAME
    ):
        raise _PrivacyViolation()
    transfer_syntax = UID(str(file_meta.TransferSyntaxUID))
    if (
        not transfer_syntax.is_transfer_syntax
        or str(transfer_syntax) == "1.2.840.10008.1.2.1.99"
    ):
        raise _PrivacyViolation()
    return transfer_syntax


def _read_exact(stream: BinaryIO, size: int) -> bytes:
    value = stream.read(size)
    if len(value) != size:
        raise _PrivacyViolation()
    return value


def _audit_pixel_boundary(path: Path, offset: int, transfer_syntax: UID) -> None:
    size = path.stat().st_size
    if not 132 <= offset < size:
        raise _PrivacyViolation()
    endian = "<" if transfer_syntax.is_little_endian else ">"
    with path.open("rb") as stream:
        stream.seek(offset)
        group, element = struct.unpack(f"{endian}HH", _read_exact(stream, 4))
        if (group, element) != (0x7FE0, 0x0010):
            raise _PrivacyViolation()
        if transfer_syntax.is_implicit_VR:
            length = struct.unpack(f"{endian}I", _read_exact(stream, 4))[0]
        else:
            vr = _read_exact(stream, 2)
            if vr not in {b"OB", b"OW"} or _read_exact(stream, 2) != b"\0\0":
                raise _PrivacyViolation()
            length = struct.unpack(f"{endian}I", _read_exact(stream, 4))[0]
        if length != 0xFFFFFFFF:
            if transfer_syntax.is_compressed or length < 2 or length % 2:
                raise _PrivacyViolation()
            if stream.tell() + length != size:
                raise _PrivacyViolation()
            return
        if not transfer_syntax.is_compressed or not transfer_syntax.is_little_endian:
            raise _PrivacyViolation()
        item_count = 0
        fragment_bytes = 0
        while stream.tell() < size:
            item_group, item_element, item_length = struct.unpack(
                "<HHI", _read_exact(stream, 8)
            )
            if (item_group, item_element) == (0xFFFE, 0xE0DD):
                if (
                    item_length != 0
                    or stream.tell() != size
                    or item_count < 2
                    or fragment_bytes < 2
                ):
                    raise _PrivacyViolation()
                return
            if (item_group, item_element) != (0xFFFE, 0xE000) or item_length % 2:
                raise _PrivacyViolation()
            item_count += 1
            if item_count > 10_000_000 or stream.tell() + item_length > size:
                raise _PrivacyViolation()
            if item_count > 1:
                fragment_bytes += item_length
            stream.seek(item_length, io.SEEK_CUR)
    raise _PrivacyViolation()


def audit_dicom(path: Path, *, expected_subject_id: str) -> DicomAudit:
    """Fail closed unless a rewritten DICOM satisfies the server privacy policy."""
    try:
        if not re.fullmatch(r"[a-f0-9]{24}", expected_subject_id):
            raise _PrivacyViolation()
        with path.open("rb") as raw:
            if _read_exact(raw, 128) != b"\0" * 128 or _read_exact(raw, 4) != b"DICM":
                raise _PrivacyViolation()
            raw.seek(0)
            reader = _BoundedReader(raw, min(path.stat().st_size, MAX_METADATA_BYTES))
            previous_mode = pydicom_config.settings.reading_validation_mode
            pydicom_config.settings.reading_validation_mode = pydicom_config.RAISE
            try:
                dataset = dcmread(reader, stop_before_pixels=True, force=False)
            finally:
                pydicom_config.settings.reading_validation_mode = previous_mode
            pixel_offset = reader.tell()
        if not REQUIRED_TAGS.issubset(dataset.keys()):
            raise _PrivacyViolation()
        sop_class = str(dataset[Tag(0x0008, 0x0016)].value).strip(" \0")
        if sop_class not in SUPPORTED_MR_SOP_CLASSES:
            raise _PrivacyViolation()
        burned_in_declared = Tag(0x0028, 0x0301) in dataset
        if not burned_in_declared:
            image_type = dataset.get(Tag(0x0008, 0x0008))
            if not isinstance(image_type, DataElement):
                raise _PrivacyViolation()
            values = set(_text_components(image_type))
            if not {"ORIGINAL", "PRIMARY"}.issubset(values) or values & {
                "DERIVED",
                "SECONDARY",
            }:
                raise _PrivacyViolation()
        state = _AuditState()
        _audit_dataset(dataset, expected_subject_id, state, 0)
        transfer_syntax = _audit_file_meta(dataset.file_meta, dataset)
        _audit_pixel_boundary(path, pixel_offset, transfer_syntax)
        temporal_position_indices = _recursive_integers(dataset, Tag(0x0020, 0x9128))
        number_of_temporal_positions = _optional_int(dataset, Tag(0x0020, 0x0105))
        if number_of_temporal_positions is None and len(temporal_position_indices) >= 2:
            number_of_temporal_positions = len(temporal_position_indices)
        image_positions, image_position_count = _recursive_image_positions(dataset)
        return DicomAudit(
            sop_instance_uid=str(dataset[Tag(0x0008, 0x0018)].value).strip(" \0"),
            sop_class_uid=sop_class,
            manufacturer=_optional_single_text(dataset, Tag(0x0008, 0x0070)),
            model=_optional_single_text(dataset, Tag(0x0008, 0x1090)),
            software_versions=frozenset(_optional_text(dataset, Tag(0x0018, 0x1020))),
            image_type=frozenset(_text_components(dataset[Tag(0x0008, 0x0008)])),
            scanning_sequence=frozenset(_optional_text(dataset, Tag(0x0018, 0x0020))),
            sequence_name=_optional_single_text(dataset, Tag(0x0018, 0x0024)),
            mr_acquisition_type=_optional_single_text(dataset, Tag(0x0018, 0x0023)),
            echo_planar_pulse_sequence=_optional_single_text(
                dataset, Tag(0x0018, 0x9018)
            )
            or _recursive_single_text(dataset, Tag(0x0018, 0x9018)),
            repetition_time_ms=_optional_float(dataset, Tag(0x0018, 0x0080))
            or _recursive_float(dataset, Tag(0x0018, 0x0080)),
            echo_times_ms=(
                frozenset([root_echo_time])
                if (root_echo_time := _optional_float(dataset, Tag(0x0018, 0x0081)))
                is not None
                else _recursive_floats(dataset, Tag(0x0018, 0x9082))
            ),
            acquisition_number=_optional_int(dataset, Tag(0x0020, 0x0012)),
            temporal_position_identifier=_optional_int(dataset, Tag(0x0020, 0x0100)),
            temporal_position_indices=temporal_position_indices,
            number_of_temporal_positions=number_of_temporal_positions,
            image_positions=image_positions,
            image_position_count=image_position_count,
            acquisition_contrast=frozenset(
                _optional_text(dataset, Tag(0x0008, 0x9209))
            ),
            diffusion_b_value=_optional_float(dataset, Tag(0x0018, 0x9087)),
            asl_technique_present=Tag(0x0018, 0x9250) in dataset,
            burned_in_annotation_declared_no=burned_in_declared,
            private_exceptions=frozenset(state.private_exceptions),
            philips_private_fields=frozenset(state.philips_private_fields),
            trigger_time_present=state.trigger_time_present,
        )
    except InvalidArchive:
        raise
    except (
        AttributeError,
        InvalidDicomError,
        KeyError,
        OSError,
        OverflowError,
        RecursionError,
        TypeError,
        ValueError,
        _PrivacyViolation,
    ) as exc:
        raise InvalidArchive(PRIVACY_ERROR) from exc
