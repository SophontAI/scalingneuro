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
MAX_MULTI_COIL_ELEMENTS = 256
MAX_VALUE_MULTIPLICITY = 65_536
MAX_DICOM_INSTANCES = 500_000
IMPLEMENTATION_CLASS_UID = "2.25.323468694959424494117938985101850441847"
IMPLEMENTATION_VERSION_NAME = "NEUROSYNC_RAW_1"
EXTENDED_OFFSET_TABLE = Tag(0x7FE0, 0x0001)
EXTENDED_OFFSET_TABLE_LENGTHS = Tag(0x7FE0, 0x0002)
SOURCE_IMAGE_SEQUENCE = Tag(0x0008, 0x2112)
REFERENCED_IMAGE_SEQUENCE = Tag(0x0008, 0x1140)
REFERENCED_SOP_CLASS_UID = Tag(0x0008, 0x1150)
REFERENCED_SOP_INSTANCE_UID = Tag(0x0008, 0x1155)
REFERENCED_FRAME_NUMBER = Tag(0x0008, 0x1160)
PURPOSE_OF_REFERENCE_CODE_SEQUENCE = Tag(0x0040, 0xA170)
ANATOMY_CONTEXT_UID = "1.2.840.10008.6.1.307"
MR_METABOLITE_MAP_SEQUENCE = Tag(0x0018, 0x9152)
METABOLITE_MAP_DESCRIPTION = Tag(0x0018, 0x9080)
MR_RECEIVE_COIL_SEQUENCE = Tag(0x0018, 0x9042)
RECEIVE_COIL_NAME = Tag(0x0018, 0x1250)
RECEIVE_COIL_MANUFACTURER_NAME = Tag(0x0018, 0x9041)
RECEIVE_COIL_TYPE = Tag(0x0018, 0x9043)
QUADRATURE_RECEIVE_COIL = Tag(0x0018, 0x9044)
MULTI_COIL_DEFINITION_SEQUENCE = Tag(0x0018, 0x9045)
MULTI_COIL_CONFIGURATION = Tag(0x0018, 0x9046)
MULTI_COIL_ELEMENT_NAME = Tag(0x0018, 0x9047)
MULTI_COIL_ELEMENT_USED = Tag(0x0018, 0x9048)
MR_TRANSMIT_COIL_SEQUENCE = Tag(0x0018, 0x9049)
TRANSMIT_COIL_NAME = Tag(0x0018, 0x1251)
TRANSMIT_COIL_MANUFACTURER_NAME = Tag(0x0018, 0x9050)
TRANSMIT_COIL_TYPE = Tag(0x0018, 0x9051)
UNSUPPORTED_REFERENCE_SEMANTICS = frozenset(
    {
        Tag(0x0008, 0x9124),
        Tag(0x0008, 0x9215),
        PURPOSE_OF_REFERENCE_CODE_SEQUENCE,
    }
)
DEIDENTIFICATION_METHODS = {
    "1.0.0": "Scaling Neuro scaling-neuro.dicom-deidentification 1.0.0",
    "2.0.0": "Scaling Neuro scaling-neuro.dicom-deidentification 2.0.0",
}
# Kept as the public legacy constant for downstream imports and old fixtures.
DEIDENTIFICATION_METHOD = DEIDENTIFICATION_METHODS["1.0.0"]
REMAPPED_UID_RE = re.compile(r"^2\.25\.(?:0|[1-9][0-9]{0,38})$")
UID_RE = re.compile(r"^[0-9]+(?:\.[0-9]+)+$")
SUPPORTED_MR_SOP_CLASSES = {
    "1.2.840.10008.5.1.4.1.1.4",
    "1.2.840.10008.5.1.4.1.1.4.1",
    "1.2.840.10008.5.1.4.1.1.4.4",
}
CLASSIC_MR_IMAGE_STORAGE_UID = "1.2.840.10008.5.1.4.1.1.4"
ENHANCED_MR_IMAGE_STORAGE_UID = "1.2.840.10008.5.1.4.1.1.4.1"
ENHANCED_MR_IMAGE_STORAGE_UIDS = frozenset(
    {
        ENHANCED_MR_IMAGE_STORAGE_UID,
        "1.2.840.10008.5.1.4.1.1.4.4",
    }
)
LEGACY_CONVERTED_ENHANCED_MR_IMAGE_STORAGE_UID = "1.2.840.10008.5.1.4.1.1.4.4"
CANONICAL_COIL_RE = re.compile(
    r"^(?:MULTI_COIL|SURFACE|HEAD(?:_NECK)?|NECK|BODY|SPINE|KNEE|FLEX|BREAST|CARDIAC|FOOT|ANKLE|SHOULDER|WRIST)"
    r"(?:_(?:[1-9]|[1-9][0-9]|1[0-9]{2}|2[0-4][0-9]|25[0-6]))?$"
)

SIEMENS_CSA_CREATOR_TAG = Tag(0x0029, 0x0010)
SIEMENS_CSA_DATA_TAG = Tag(0x0029, 0x1010)
SIEMENS_CSA_CREATOR = "SIEMENS CSA HEADER"
SIEMENS_MR_HEADER_CREATOR = "SIEMENS MR HEADER"
UIH_IMAGE_HEADER_CREATOR = "Image Private Header"
GE_ACQU_CREATOR = "GEMS_ACQU_01"
GE_PARM_CREATOR = "GEMS_PARM_01"
PHILIPS_MR_CREATOR = "Philips MR Imaging DD 001"
PHILIPS_PER_FRAME_CREATOR = "Philips MR Imaging DD 005"
PHILIPS_IMAGING_CREATOR = "Philips Imaging DD 001"
SAFE_PRIVATE_EXCEPTION_ORDER = (
    "siemens_csa_image_header_numeric_v1",
    "dicom_ps3.15_siemens_mr_header_diffusion",
    "dicom_ps3.15_philips_diffusion",
    "dicom_ps3.15_philips_phase_number",
    "dicom_ps3.15_ge_diffusion_b_value",
    "uih_image_private_header_grid_slice_count_numeric_v1",
    "uih_image_private_header_diffusion_numeric_v1",
    "philips_mr_imaging_dd_001_diffusion_gradient_vector_numeric_v1",
    "philips_mr_imaging_dd_005_diffusion_indices_numeric_v1",
    "philips_mr_imaging_dd_005_asl_label_code_v1",
    "ge_gems_acqu_01_diffusion_gradient_vector_numeric_v1",
    "ge_gems_parm_01_asl_technique_duration_v1",
    "dicom_ps3.15_philips_scale_intercept_slope",
    "dicom_ps3.15_philips_number_of_slices",
    "dicom_ps3.15_philips_water_fat_shift",
    "dicom_ps3.15_philips_per_frame_scale_slope",
)
SAFE_PRIVATE_EXCEPTIONS = frozenset(SAFE_PRIVATE_EXCEPTION_ORDER)
SAFE_PRIVATE_CREATORS = {
    SIEMENS_CSA_CREATOR,
    SIEMENS_MR_HEADER_CREATOR,
    UIH_IMAGE_HEADER_CREATOR,
    GE_ACQU_CREATOR,
    GE_PARM_CREATOR,
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
    ("B_value", b"DS\0\0"),
    ("DiffusionGradientDirection", b"DS\0\0"),
    ("B_matrix", b"DS\0\0"),
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
# These names remain public compatibility exports for archive metadata tests.
# Header validation no longer depends on either vocabulary: a bounded, safe
# scanner string is retained so a newly released scanner is not rejected merely
# because the processor predates it.
CANONICAL_SEQUENCE_NAMES = {
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

CODE_VALUES = {
    Tag(0x0008, 0x0060): {"MR"},
    Tag(0x0008, 0x9205): {"COLOR", "MONOCHROME", "MIXED"},
    Tag(0x0008, 0x9206): {"VOLUME", "SAMPLED", "DISTORTED", "MIXED"},
    Tag(0x0008, 0x9207): {
        "MAX_IP",
        "MIN_IP",
        "VOLUME_RENDER",
        "SURFACE_RENDER",
        "MPR",
        "CURVED_MPR",
        "NONE",
        "MIXED",
    },
    Tag(0x0008, 0x9208): {"MAGNITUDE", "PHASE", "REAL", "IMAGINARY", "MIXED"},
    Tag(0x0008, 0x9209): {
        "UNKNOWN",
        "T1",
        "T2",
        "T2_STAR",
        "PROTON_DENSITY",
        "DIFFUSION",
        "FLOW_ENCODED",
        "FLUID_ATTENUATED",
        "PERFUSION",
        "STIR",
        "TAGGING",
        "TOF",
        "MIXED",
    },
    Tag(0x0018, 0x0020): {"SE", "IR", "GR", "EP", "RM"},
    Tag(0x0018, 0x0021): {"SK", "MTC", "SS", "TRSS", "SP", "MP", "OSP", "NONE"},
    Tag(0x0018, 0x0022): {"PER", "RG", "CG", "PPG", "FC", "PFF", "PFP", "SP", "FS"},
    Tag(0x0018, 0x0023): {"2D", "3D"},
    Tag(0x0018, 0x0025): {"Y", "N"},
    Tag(0x0018, 0x1312): {"ROW", "COL", "COLUMN", "OTHER"},
    Tag(0x0018, 0x5100): {"HFP", "HFS", "HFDR", "HFDL", "FFDR", "FFDL", "FFP", "FFS"},
    Tag(0x0018, 0x9008): {"SPIN", "GRADIENT", "BOTH"},
    Tag(0x0018, 0x9009): {"YES", "NO"},
    Tag(0x0018, 0x9010): {"ACCELERATION", "VELOCITY", "OTHER", "NONE"},
    Tag(0x0018, 0x9011): {"YES", "NO"},
    Tag(0x0018, 0x9012): {"YES", "NO"},
    Tag(0x0018, 0x9014): {"YES", "NO"},
    Tag(0x0018, 0x9015): {"YES", "NO"},
    Tag(0x0018, 0x9016): {"RF", "GRADIENT", "RF_AND_GRADIENT", "NONE"},
    Tag(0x0018, 0x9017): {
        "FREE_PRECESSION",
        "TRANSVERSE",
        "TIME_REVERSED",
        "LONGITUDINAL",
        "NONE",
    },
    Tag(0x0018, 0x9036): {"PHASE", "FREQUENCY", "SLICE", "COMBINATION"},
    Tag(0x0018, 0x9018): {"YES", "NO"},
    Tag(0x0018, 0x9020): {"ON_RESONANCE", "OFF_RESONANCE", "NONE"},
    Tag(0x0018, 0x9021): {"YES", "NO"},
    Tag(0x0018, 0x9022): {"YES", "NO"},
    Tag(0x0018, 0x9024): {"YES", "NO"},
    Tag(0x0018, 0x9025): {"FAT", "WATER", "FAT_AND_WATER", "SILICON_GEL", "NONE"},
    Tag(0x0018, 0x9026): {"WATER", "FAT", "NONE"},
    Tag(0x0018, 0x9027): {"SLAB", "NONE"},
    Tag(0x0018, 0x9028): {"GRID", "LINE", "NONE"},
    Tag(0x0018, 0x9029): {"2D", "3D", "2D_3D", "NONE"},
    Tag(0x0018, 0x9032): {"RECTILINEAR", "RADIAL", "SPIRAL"},
    Tag(0x0018, 0x9033): {"SINGLE", "PARTIAL", "FULL"},
    Tag(0x0018, 0x9034): {
        "LINEAR",
        "REVERSE_LINEAR",
        "CENTRIC",
        "REVERSE_CENTRIC",
        "SEGMENTED",
        "UNKNOWN",
    },
    Tag(0x0018, 0x9043): {"BODY", "VOLUME", "SURFACE", "MULTICOIL"},
    Tag(0x0018, 0x9044): {"YES", "NO"},
    Tag(0x0018, 0x9048): {"YES", "NO"},
    Tag(0x0018, 0x9051): {"BODY", "VOLUME", "SURFACE", "MULTICOIL"},
    Tag(0x0018, 0x9075): {"NONE", "ISOTROPIC", "DIRECTIONAL", "BMATRIX"},
    Tag(0x0018, 0x9077): {"YES", "NO"},
    Tag(0x0018, 0x9078): {"PILS", "SENSE", "GRAPPA", "ASSET", "SMASH", "OTHER", "NONE"},
    Tag(0x0018, 0x9081): {"YES", "NO"},
    Tag(0x0018, 0x9183): {
        "PHASE",
        "FREQUENCY",
        "SLICE_SELECT",
        "SLICE_AND_FREQ",
        "SLICE_FREQ_PHASE",
        "PHASE_AND_FREQ",
        "SLICE_AND_PHASE",
        "OTHER",
    },
    Tag(0x0018, 0x9250): {"CONTINUOUS", "PULSED", "PSEUDOCONTINUOUS"},
    Tag(0x0018, 0x9257): {"LABEL", "CONTROL", "M_ZERO_SCAN"},
    Tag(0x0018, 0x9259): {"YES", "NO"},
    Tag(0x0018, 0x925C): {"YES", "NO"},
    Tag(0x0018, 0x9624): {"YES", "NO"},
    Tag(0x0020, 0x9072): {"R", "L", "U", "B"},
    Tag(0x0028, 0x0004): {
        "MONOCHROME1",
        "MONOCHROME2",
        "PALETTE COLOR",
        "RGB",
        "YBR_FULL",
        "YBR_FULL_422",
        "YBR_ICT",
        "YBR_RCT",
        "YBR_PARTIAL_420",
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

CLASSIC_IMAGE_TYPE_TRAILING_VALUES = frozenset(
    {
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
        "FFE",
        "FFE_IP",
        "WATER",
        "FAT",
        "DENSITY MAP",
        "DIFFUSION MAP",
        "IMAGE ADDITION",
        "MODULUS SUBTRACT",
        "MPR",
        "PHASE MAP",
        "PHASE SUBTRACT",
        "PROJECTION IMAGE",
        "T1 MAP",
        "T2 MAP",
        "VELOCITY MAP",
    }
)
ENHANCED_ROOT_TYPE_VALUE_1 = frozenset({"ORIGINAL", "DERIVED", "MIXED"})
FRAME_TYPE_VALUE_1 = frozenset({"ORIGINAL", "DERIVED"})
FRAME_TYPE_VALUE_2 = frozenset({"PRIMARY"})
FRAME_TYPE_VALUE_3 = frozenset(
    {
        "ANGIO",
        "CARDIAC",
        "CARDIAC_GATED",
        "CARDRESP_GATED",
        "DYNAMIC",
        "FLUOROSCOPY",
        "LOCALIZER",
        "MOTION",
        "PERFUSION",
        "PRE_CONTRAST",
        "POST_CONTRAST",
        "RESP_GATED",
        "REST",
        "STATIC",
        "STRESS",
        "VOLUME",
        "NON_PARALLEL",
        "PARALLEL",
        "WHOLE_BODY",
        "ANGIO_TIME",
        "ASL",
        "CINE",
        "DIFFUSION",
        "DIXON",
        "FLOW_ENCODED",
        "FLUID_ATTENUATED",
        "FMRI",
        "MAX_IP",
        "MIN_IP",
        "M_MODE",
        "METABOLITE_MAP",
        "MULTIECHO",
        "PROTON_DENSITY",
        "REALTIME",
        "STIR",
        "TAGGING",
        "TEMPERATURE",
        "T1",
        "T2",
        "T2_STAR",
        "TOF",
        "VELOCITY",
    }
)
FRAME_TYPE_VALUE_4 = frozenset(
    {
        "ADDITION",
        "DIVISION",
        "MASKED",
        "MAXIMUM",
        "MEAN",
        "MINIMUM",
        "MULTIPLICATION",
        "RESAMPLED",
        "STD_DEVIATION",
        "SUBTRACTION",
        "NONE",
        "QUANTITY",
        # Retain the bounded MR Defined Terms emitted by deployed Enhanced and
        # Legacy Converted Enhanced MR implementations. These values are
        # scientific semantics, not arbitrary scanner text.
        "ADC",
        "DIFFUSION",
        "DIFFUSION_ANISO",
        "DIFFUSION_ATTNTD",
        "DIFFUSION_ISO",
        "ATTNTD",
        "FA",
        "TRACEW",
        "FAT",
        "FAT_FRACTION",
        "FIELD_MAP",
        "IN_PHASE",
        "METABOLITE_MAP",
        "NEI",
        "OUT_OF_PHASE",
        "PERFUSION_ASL",
        "R_COEFFICIENT",
        "R2_MAP",
        "R2_STAR_MAP",
        "RHO",
        "SCM",
        "SNR_MAP",
        "T1_MAP",
        "T2_STAR_MAP",
        "T2_MAP",
        "TCS",
        "TEMPERATURE",
        "VELOCITY",
        "WATER",
        "WATER_FRACTION",
    }
)

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
    Tag(0x0020, 0x000D),
    Tag(0x0020, 0x000E),
    Tag(0x0028, 0x0303),
}

ROWS = Tag(0x0028, 0x0010)
COLUMNS = Tag(0x0028, 0x0011)
SAMPLES_PER_PIXEL = Tag(0x0028, 0x0002)
PHOTOMETRIC_INTERPRETATION = Tag(0x0028, 0x0004)
NUMBER_OF_FRAMES = Tag(0x0028, 0x0008)
PLANAR_CONFIGURATION = Tag(0x0028, 0x0006)
BITS_ALLOCATED = Tag(0x0028, 0x0100)
BITS_STORED = Tag(0x0028, 0x0101)
HIGH_BIT = Tag(0x0028, 0x0102)
PIXEL_REPRESENTATION = Tag(0x0028, 0x0103)
ENHANCED_CONTENT_DATE_SENTINEL = "19000101"
ENHANCED_CONTENT_TIME_SENTINEL = "000000"
ENHANCED_FRAME_DATETIME_SENTINEL = "19000101000000"

RESCALE_INTERCEPT = Tag(0x0028, 0x1052)
RESCALE_SLOPE = Tag(0x0028, 0x1053)
RESCALE_TYPE = Tag(0x0028, 0x1054)
WINDOW_CENTER = Tag(0x0028, 0x1050)
WINDOW_WIDTH = Tag(0x0028, 0x1051)
PIXEL_VALUE_TRANSFORMATION_SEQUENCE = Tag(0x0028, 0x9145)
FRAME_VOI_LUT_SEQUENCE = Tag(0x0028, 0x9132)
SHARED_FUNCTIONAL_GROUPS_SEQUENCE = Tag(0x5200, 0x9229)
PER_FRAME_FUNCTIONAL_GROUPS_SEQUENCE = Tag(0x5200, 0x9230)
DIMENSION_ORGANIZATION_UID = Tag(0x0020, 0x9164)
DIMENSION_INDEX_POINTER = Tag(0x0020, 0x9165)
FUNCTIONAL_GROUP_POINTER = Tag(0x0020, 0x9167)
DIMENSION_INDEX_VALUES = Tag(0x0020, 0x9157)
FRAME_CONTENT_SEQUENCE = Tag(0x0020, 0x9111)
PIXEL_MEASURES_SEQUENCE = Tag(0x0028, 0x9110)
PLANE_POSITION_SEQUENCE = Tag(0x0020, 0x9113)
PLANE_ORIENTATION_SEQUENCE = Tag(0x0020, 0x9116)
FRAME_ANATOMY_SEQUENCE = Tag(0x0020, 0x9071)
MR_IMAGE_FRAME_TYPE_SEQUENCE = Tag(0x0018, 0x9226)
ACQUISITION_CONTEXT_SEQUENCE = Tag(0x0040, 0x0555)
UNASSIGNED_SHARED_CONVERTED_ATTRIBUTES_SEQUENCE = Tag(0x0020, 0x9170)
UNASSIGNED_PER_FRAME_CONVERTED_ATTRIBUTES_SEQUENCE = Tag(0x0020, 0x9171)
CONVERSION_SOURCE_ATTRIBUTES_SEQUENCE = Tag(0x0020, 0x9172)
CONCATENATION_ATTRIBUTES = frozenset(
    {
        Tag(0x0020, 0x0242),
        Tag(0x0020, 0x9161),
        Tag(0x0020, 0x9162),
        Tag(0x0020, 0x9163),
        Tag(0x0020, 0x9228),
    }
)
COMMON_FUNCTIONAL_GROUP_MACROS = frozenset(
    {
        PIXEL_MEASURES_SEQUENCE,
        PLANE_POSITION_SEQUENCE,
        PLANE_ORIENTATION_SEQUENCE,
        FRAME_ANATOMY_SEQUENCE,
        MR_IMAGE_FRAME_TYPE_SEQUENCE,
        FRAME_CONTENT_SEQUENCE,
        PIXEL_VALUE_TRANSFORMATION_SEQUENCE,
        FRAME_VOI_LUT_SEQUENCE,
    }
)
CURRENT_ENHANCED_FUNCTIONAL_GROUP_MACROS = frozenset(
    {
        Tag(0x0018, 0x9006),
        Tag(0x0018, 0x9042),
        Tag(0x0018, 0x9049),
        Tag(0x0018, 0x9112),
        Tag(0x0018, 0x9114),
        Tag(0x0018, 0x9115),
        Tag(0x0018, 0x9117),
        Tag(0x0018, 0x9119),
        Tag(0x0018, 0x9125),
        Tag(0x0018, 0x9251),
    }
)
ROOT_RESCALE_TAGS = frozenset({RESCALE_INTERCEPT, RESCALE_SLOPE, RESCALE_TYPE})
ROOT_WINDOW_TAGS = frozenset({WINDOW_CENTER, WINDOW_WIDTH})

# A transform must either be preserved atomically below or rejected.  In
# particular, retaining one descriptor while dropping its LUT data would leave
# a syntactically readable but scientifically unusable image.
UNSUPPORTED_PIXEL_TRANSFORM_TAGS = frozenset(
    {
        Tag(0x0028, 0x1055),  # Window Center & Width Explanation (free text)
        Tag(0x0028, 0x1056),  # VOI LUT Function
        Tag(0x0028, 0x1100),  # Palette Color LUT Descriptors and UIDs
        Tag(0x0028, 0x1101),
        Tag(0x0028, 0x1102),
        Tag(0x0028, 0x1103),
        Tag(0x0028, 0x1104),
        Tag(0x0028, 0x1111),
        Tag(0x0028, 0x1112),
        Tag(0x0028, 0x1113),
        Tag(0x0028, 0x1114),
        Tag(0x0028, 0x1199),
        Tag(0x0028, 0x1200),  # Palette Color LUT Data
        Tag(0x0028, 0x1201),
        Tag(0x0028, 0x1202),
        Tag(0x0028, 0x1203),
        Tag(0x0028, 0x1204),
        Tag(0x0028, 0x1211),
        Tag(0x0028, 0x1212),
        Tag(0x0028, 0x1213),
        Tag(0x0028, 0x1214),
        Tag(0x0028, 0x1221),  # Segmented Palette Color LUT Data
        Tag(0x0028, 0x1222),
        Tag(0x0028, 0x1223),
        Tag(0x0028, 0x1224),
        Tag(0x0028, 0x2000),  # ICC Profile
        Tag(0x0028, 0x3000),  # Modality LUT Sequence
        Tag(0x0028, 0x3002),  # LUT Descriptor
        Tag(0x0028, 0x3003),  # LUT Explanation
        Tag(0x0028, 0x3004),  # Modality LUT Type
        Tag(0x0028, 0x3006),  # LUT Data
        Tag(0x0028, 0x3010),  # VOI LUT Sequence
        Tag(0x0040, 0x9094),  # Referenced Image Real World Value Mapping Sequence
        Tag(0x0040, 0x9096),  # Real World Value Mapping Sequence
        Tag(0x0040, 0x9098),  # Pixel Value Mapping Code Sequence
        Tag(0x0040, 0x9210),  # LUT Label
        Tag(0x0040, 0x9211),  # Real World Value Last Value Mapped
        Tag(0x0040, 0x9212),  # Real World Value LUT Data
        Tag(0x0040, 0x9213),  # Double Float Real World Value Last Value Mapped
        Tag(0x0040, 0x9214),  # Double Float Real World Value First Value Mapped
        Tag(0x0040, 0x9216),  # Real World Value First Value Mapped
        Tag(0x0040, 0x9220),  # Quantity Definition Sequence
        Tag(0x0040, 0x9224),  # Real World Value Intercept
        Tag(0x0040, 0x9225),  # Real World Value Slope
    }
)

CORE_TYPE_1_ATTRIBUTES = {
    Tag(0x0008, 0x0008): "CS",  # Image Type
    Tag(0x0008, 0x0016): "UI",  # SOP Class UID
    Tag(0x0008, 0x0018): "UI",  # SOP Instance UID
    Tag(0x0008, 0x0060): "CS",  # Modality
    Tag(0x0010, 0x0010): "PN",  # de-identified Patient Name
    Tag(0x0010, 0x0020): "LO",  # de-identified Patient ID
    Tag(0x0020, 0x000D): "UI",  # Study Instance UID
    Tag(0x0020, 0x000E): "UI",  # Series Instance UID
}
PRIVACY_TYPE_2_EMPTY_ATTRIBUTES = {
    Tag(0x0008, 0x0020): "DA",  # Study Date
    Tag(0x0008, 0x0022): "DA",  # Acquisition Date
    Tag(0x0008, 0x0023): "DA",  # Content Date
    Tag(0x0008, 0x0030): "TM",  # Study Time
    Tag(0x0008, 0x0032): "TM",  # Acquisition Time
    Tag(0x0008, 0x0033): "TM",  # Content Time
    Tag(0x0008, 0x0050): "SH",  # Accession Number
    Tag(0x0008, 0x0090): "PN",  # Referring Physician's Name
    Tag(0x0010, 0x0030): "DA",  # Patient Birth Date
    Tag(0x0010, 0x0040): "CS",  # Patient Sex
    Tag(0x0020, 0x0010): "SH",  # Study ID
    Tag(0x0020, 0x1040): "LO",  # Position Reference Indicator
}
NUMERIC_TYPE_2_ATTRIBUTES = {
    Tag(0x0020, 0x0011): "IS",  # Series Number
    Tag(0x0020, 0x0012): "IS",  # Acquisition Number
    Tag(0x0020, 0x0013): "IS",  # Instance Number
}
MANUFACTURER = Tag(0x0008, 0x0070)
MANUFACTURER_MODEL_NAME = Tag(0x0008, 0x1090)
DEVICE_SERIAL_NUMBER = Tag(0x0018, 0x1000)
SOFTWARE_VERSIONS = Tag(0x0018, 0x1020)
FRAME_OF_REFERENCE_UID = Tag(0x0020, 0x0052)
CLASSIC_MR_TYPE_1_CODES = {
    Tag(0x0018, 0x0020): "CS",  # Scanning Sequence
    Tag(0x0018, 0x0021): "CS",  # Sequence Variant
}
CLASSIC_MR_TYPE_2_ATTRIBUTES = {
    Tag(0x0018, 0x0022): "CS",  # Scan Options
    Tag(0x0018, 0x0023): "CS",  # MR Acquisition Type
    Tag(0x0018, 0x0081): "DS",  # Echo Time
    Tag(0x0018, 0x0091): "IS",  # Echo Train Length
}
PSEUDONYMOUS_DEVICE_SERIAL_RE = re.compile(r"^SN-[a-f0-9]{24}$")
SCANNER_TEXT_PUNCTUATION = frozenset(" .,_-+&()/")
RESCALE_TYPE_PUNCTUATION = frozenset(" _-./%")
PHI_LIKE_RESCALE_TOKENS = re.compile(
    r"(?:PATIENT|SUBJECT|NAME|MRN|BIRTH|DOB|ACCESSION)", re.IGNORECASE
)
PHI_LIKE_SCANNER_TOKEN_PREFIXES = (
    "EMAIL",
    "NAME",
    "MRN",
    "PATIENT",
    "PARTICIPANT",
    "SUBJECT",
    "BIRTH",
    "DOB",
    "SSN",
    "ACCESSION",
)
ASL_CRUSHER_DESCRIPTION_SENTINEL = "REDACTED"


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
    asl_technique_descriptions_emptied: int = 0
    asl_crusher_descriptions_redacted: int = 0
    asl_bolus_cutoff_techniques_emptied: int = 0


@dataclass(frozen=True)
class DicomAudit:
    sop_instance_uid: str
    sop_class_uid: str
    study_instance_uid: str
    series_instance_uid: str
    manufacturer: str | None
    model: str | None
    software_versions: frozenset[str]
    patient_position: str | None
    magnetic_field_strength: float | None
    receive_coil_name: str | None
    transmit_coil_name: str | None
    series_number: int | None
    image_type: frozenset[str]
    scanning_sequence: frozenset[str]
    sequence_variant: frozenset[str]
    scan_options: frozenset[str]
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
    diffusion_metadata_present: bool
    diffusion_metadata_contract_verified: bool
    diffusion_semantic_evidence: bool
    asl_technique_present: bool
    asl_metadata_present: bool
    asl_metadata_contract_verified: bool
    asl_technique_descriptions_emptied: int
    asl_crusher_descriptions_redacted: int
    asl_bolus_cutoff_techniques_emptied: int
    burned_in_annotation_declared_no: bool
    private_exceptions: frozenset[str]
    philips_private_fields: frozenset[str]
    trigger_time_present: bool


@dataclass(frozen=True)
class _ScientificContract:
    present: bool
    valid: bool
    semantic: bool = False


@dataclass(frozen=True)
class _DiffusionValues:
    b_value: tuple[float, ...] | None = None
    gradient: tuple[float, ...] | None = None
    b_matrix: tuple[float, ...] | None = None


def _components(value: Any) -> list[Any]:
    if isinstance(value, (MultiValue, list, tuple)):
        values = list(value)
    else:
        values = [value]
    if not 1 <= len(values) <= MAX_VALUE_MULTIPLICITY:
        raise _PrivacyViolation()
    return values


def _text_components(element: DataElement, *, allow_empty: bool = False) -> list[str]:
    values = [str(value).strip(" \0") for value in _components(element.value)]
    if any((not value and not allow_empty) or len(value) > 96 for value in values):
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
    if element.is_empty:
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


def _safe_bounded_text(
    value: str,
    *,
    maximum: int,
    punctuation: frozenset[str] = SCANNER_TEXT_PUNCTUATION,
) -> bool:
    """Accept useful scanner text without accepting markup or control data."""
    return (
        1 <= len(value) <= maximum
        and value == " ".join(value.split())
        and value[0] != " "
        and value[-1] != " "
        and not value.startswith(("/", "\\"))
        and "\\" not in value
        and " / " not in value
        and ".." not in value
        and "://" not in value
        and "@" not in value
        and re.match(r"^[A-Za-z]:", value) is None
        and all(character.isascii() for character in value)
        and any(character.isalpha() for character in value)
        and all(character.isalnum() or character in punctuation for character in value)
    )


def safe_scanner_text(value: str) -> bool:
    """Public manifest/header policy for bounded scanner identity text."""
    if not _safe_bounded_text(value, maximum=64):
        return False
    tokens = tuple(
        token for token in re.split(r"[^A-Za-z0-9]+", value.upper()) if token
    )
    return not any(
        any(token.startswith(prefix) for prefix in PHI_LIKE_SCANNER_TOKEN_PREFIXES)
        or len(token) >= 7
        and token.isdigit()
        for token in tokens
    )


def _root_element(dataset: Dataset, tag: BaseTag, vr: str) -> DataElement:
    element = dataset.get(tag)
    if not isinstance(element, DataElement) or element.VR != vr:
        raise _PrivacyViolation()
    return element


def _single_unsigned_short(dataset: Dataset, tag: BaseTag) -> int:
    element = _root_element(dataset, tag, "US")
    values = _components(element.value)
    if (
        len(values) != 1
        or isinstance(values[0], bool)
        or not isinstance(values[0], Integral)
    ):
        raise _PrivacyViolation()
    value = int(values[0])
    if not 0 <= value <= 65_535:
        raise _PrivacyViolation()
    return value


def _single_integer_string(
    dataset: Dataset,
    tag: BaseTag,
    *,
    minimum: int,
    maximum: int,
) -> int:
    element = _root_element(dataset, tag, "IS")
    values = _text_components(element)
    if len(values) != 1:
        raise _PrivacyViolation()
    try:
        value = int(values[0])
    except (TypeError, ValueError, OverflowError) as exc:
        raise _PrivacyViolation() from exc
    if not minimum <= value <= maximum:
        raise _PrivacyViolation()
    return value


def _single_code(dataset: Dataset, tag: BaseTag) -> str:
    element = _root_element(dataset, tag, "CS")
    values = _text_components(element)
    if len(values) != 1:
        raise _PrivacyViolation()
    return values[0]


def _finite_decimal_values(
    element: DataElement, *, maximum_vm: int = 64
) -> list[float]:
    if element.VR != "DS":
        raise _PrivacyViolation()
    values = _text_components(element)
    if not 1 <= len(values) <= maximum_vm:
        raise _PrivacyViolation()
    try:
        parsed = [float(value) for value in values]
    except (TypeError, ValueError, OverflowError) as exc:
        raise _PrivacyViolation() from exc
    if any(not math.isfinite(value) for value in parsed):
        raise _PrivacyViolation()
    return parsed


def _audit_rescale_triplet(dataset: Dataset) -> None:
    if not ROOT_RESCALE_TAGS.issubset(dataset.keys()):
        raise _PrivacyViolation()
    intercept = _finite_decimal_values(
        _root_element(dataset, RESCALE_INTERCEPT, "DS"), maximum_vm=1
    )[0]
    slope = _finite_decimal_values(
        _root_element(dataset, RESCALE_SLOPE, "DS"), maximum_vm=1
    )[0]
    rescale_type_element = _root_element(dataset, RESCALE_TYPE, "LO")
    rescale_type_values = _text_components(rescale_type_element)
    if (
        abs(intercept) > 1.0e12
        or not 0.0 < abs(slope) <= 1.0e12
        or len(rescale_type_values) != 1
        or not _safe_bounded_text(
            rescale_type_values[0],
            maximum=16,
            punctuation=RESCALE_TYPE_PUNCTUATION,
        )
        or PHI_LIKE_RESCALE_TOKENS.search(rescale_type_values[0]) is not None
    ):
        raise _PrivacyViolation()


def _audit_window_pair(dataset: Dataset) -> None:
    if not ROOT_WINDOW_TAGS.issubset(dataset.keys()):
        raise _PrivacyViolation()
    centers = _finite_decimal_values(dataset[WINDOW_CENTER], maximum_vm=16)
    widths = _finite_decimal_values(dataset[WINDOW_WIDTH], maximum_vm=16)
    if (
        len(centers) != len(widths)
        or any(abs(center) > 1.0e12 for center in centers)
        or any(not 0.0 < width <= 1.0e12 for width in widths)
    ):
        raise _PrivacyViolation()


def _audit_pixel_transforms(
    dataset: Dataset,
    *,
    depth: int = 0,
    inside_pixel_value_transformation: bool = False,
    inside_frame_voi_lut: bool = False,
    frame_voi_lut_allowed_here: bool = False,
) -> None:
    if depth > MAX_SEQUENCE_DEPTH:
        raise _PrivacyViolation()
    tags = set(dataset.keys())
    if tags & UNSUPPORTED_PIXEL_TRANSFORM_TAGS:
        raise _PrivacyViolation()

    rescale_present = tags & ROOT_RESCALE_TAGS
    if rescale_present:
        if depth != 0 and not inside_pixel_value_transformation:
            raise _PrivacyViolation()
        _audit_rescale_triplet(dataset)

    window_present = tags & ROOT_WINDOW_TAGS
    if window_present:
        if depth != 0 and not inside_frame_voi_lut:
            raise _PrivacyViolation()
        _audit_window_pair(dataset)

    for element in dataset:
        if element.VR != "SQ":
            continue
        if not isinstance(element.value, Sequence):
            raise _PrivacyViolation()
        is_pixel_value_transformation = (
            element.tag == PIXEL_VALUE_TRANSFORMATION_SEQUENCE
        )
        if is_pixel_value_transformation and len(element.value) != 1:
            raise _PrivacyViolation()
        is_frame_voi_lut = element.tag == FRAME_VOI_LUT_SEQUENCE
        if is_frame_voi_lut:
            if not frame_voi_lut_allowed_here or len(element.value) != 1:
                raise _PrivacyViolation()
            item = element.value[0]
            if not isinstance(item, Dataset) or set(item.keys()) != ROOT_WINDOW_TAGS:
                raise _PrivacyViolation()
        functional_group_item = depth == 0 and element.tag in {
            SHARED_FUNCTIONAL_GROUPS_SEQUENCE,
            PER_FRAME_FUNCTIONAL_GROUPS_SEQUENCE,
        }
        for item in element.value:
            if not isinstance(item, Dataset):
                raise _PrivacyViolation()
            if is_pixel_value_transformation:
                # Exactness applies to the transformation attributes; standard
                # functional-group bookkeeping in the same item stays allowed.
                item_tags = set(item.keys())
                if item_tags & ROOT_RESCALE_TAGS != ROOT_RESCALE_TAGS:
                    raise _PrivacyViolation()
            _audit_pixel_transforms(
                item,
                depth=depth + 1,
                inside_pixel_value_transformation=is_pixel_value_transformation,
                inside_frame_voi_lut=is_frame_voi_lut,
                frame_voi_lut_allowed_here=functional_group_item,
            )


def _audit_pixel_module(dataset: Dataset, sop_class: str) -> int:
    rows = _single_unsigned_short(dataset, ROWS)
    columns = _single_unsigned_short(dataset, COLUMNS)
    samples_per_pixel = _single_unsigned_short(dataset, SAMPLES_PER_PIXEL)
    bits_allocated = _single_unsigned_short(dataset, BITS_ALLOCATED)
    bits_stored = _single_unsigned_short(dataset, BITS_STORED)
    high_bit = _single_unsigned_short(dataset, HIGH_BIT)
    pixel_representation = _single_unsigned_short(dataset, PIXEL_REPRESENTATION)
    photometric_interpretation = _single_code(dataset, PHOTOMETRIC_INTERPRETATION)

    if (
        rows == 0
        or columns == 0
        or samples_per_pixel != 1
        or PLANAR_CONFIGURATION in dataset
        or pixel_representation not in {0, 1}
        or high_bit != bits_stored - 1
    ):
        raise _PrivacyViolation()

    number_of_frames: int | None = None
    if NUMBER_OF_FRAMES in dataset:
        number_of_frames = _single_integer_string(
            dataset,
            NUMBER_OF_FRAMES,
            minimum=1,
            maximum=MAX_DICOM_INSTANCES,
        )

    if sop_class == CLASSIC_MR_IMAGE_STORAGE_UID:
        if (
            photometric_interpretation not in {"MONOCHROME1", "MONOCHROME2"}
            or bits_allocated != 16
            or not 1 <= bits_stored <= 16
            or number_of_frames not in {None, 1}
        ):
            raise _PrivacyViolation()
        frames = 1

    else:
        if (
            sop_class not in ENHANCED_MR_IMAGE_STORAGE_UIDS
            or number_of_frames is None
            or photometric_interpretation != "MONOCHROME2"
            or (bits_allocated, bits_stored) not in {(8, 8), (16, 12), (16, 16)}
        ):
            raise _PrivacyViolation()
        frames = number_of_frames

    raw_bytes = rows * columns * samples_per_pixel * frames * (bits_allocated // 8)
    return raw_bytes + raw_bytes % 2


def _audit_type_2_numeric_shell(element: DataElement, expected_vr: str) -> None:
    if element.VR != expected_vr:
        raise _PrivacyViolation()
    if element.is_empty:
        return
    values = _text_components(element)
    if len(values) != 1:
        raise _PrivacyViolation()
    try:
        value = int(values[0])
    except (TypeError, ValueError, OverflowError) as exc:
        raise _PrivacyViolation() from exc
    if not -(2**31) <= value < 2**31:
        raise _PrivacyViolation()


def _audit_classic_mr_type_2(element: DataElement, expected_vr: str) -> None:
    if element.VR != expected_vr:
        raise _PrivacyViolation()
    if element.is_empty:
        return
    if expected_vr == "CS":
        code_values = _text_components(element)
        allowed = CODE_VALUES.get(element.tag)
        if (
            allowed is None
            or any(value not in allowed for value in code_values)
            or len(set(code_values)) != len(code_values)
        ):
            raise _PrivacyViolation()
        return
    if expected_vr == "DS":
        decimal_values = _finite_decimal_values(element, maximum_vm=1)
        if not 0 <= decimal_values[0] <= 100_000_000:
            raise _PrivacyViolation()
        return
    if expected_vr == "IS":
        integer_values = _text_components(element)
        if len(integer_values) != 1:
            raise _PrivacyViolation()
        try:
            value = int(integer_values[0])
        except (TypeError, ValueError, OverflowError) as exc:
            raise _PrivacyViolation() from exc
        if not 0 <= value < 2**31:
            raise _PrivacyViolation()
        return
    raise _PrivacyViolation()


def _required_root_code(dataset: Dataset, tag: BaseTag) -> str:
    value = _single_code(dataset, tag)
    if value not in CODE_VALUES.get(tag, set()):
        raise _PrivacyViolation()
    return value


def _single_attribute_tag(element: DataElement) -> BaseTag:
    values = _components(element.value)
    if element.VR != "AT" or len(values) != 1 or not isinstance(values[0], BaseTag):
        raise _PrivacyViolation()
    return Tag(values[0])


def _functional_group_item(container: Dataset, group_tag: BaseTag) -> Dataset | None:
    element = container.get(group_tag)
    if element is None:
        return None
    if (
        not isinstance(element, DataElement)
        or element.VR != "SQ"
        or not isinstance(element.value, Sequence)
        or len(element.value) != 1
        or not isinstance(element.value[0], Dataset)
    ):
        raise _PrivacyViolation()
    return element.value[0]


def _functional_group_reference_valid(
    shared: Dataset,
    per_frame: Sequence,
    group_tag: BaseTag,
    target_tag: BaseTag | None,
) -> bool:
    shared_group = _functional_group_item(shared, group_tag)
    per_frame_groups = [_functional_group_item(frame, group_tag) for frame in per_frame]
    if shared_group is not None:
        if any(group is not None for group in per_frame_groups):
            return False
        groups = [shared_group]
    else:
        if not per_frame_groups or any(group is None for group in per_frame_groups):
            return False
        groups = [group for group in per_frame_groups if group is not None]
    return target_tag is None or all(
        isinstance(group.get(target_tag), DataElement) for group in groups
    )


def _dimension_index_item_valid(
    dataset: Dataset,
    shared: Dataset,
    per_frame: Sequence,
    dimension: Dataset,
    organization_uids: set[str],
) -> bool:
    if set(dimension.keys()) - {
        DIMENSION_ORGANIZATION_UID,
        DIMENSION_INDEX_POINTER,
        FUNCTIONAL_GROUP_POINTER,
    }:
        return False
    organization_reference = _text_components(
        _root_element(dimension, DIMENSION_ORGANIZATION_UID, "UI")
    )
    index_pointer = _single_attribute_tag(
        _root_element(dimension, DIMENSION_INDEX_POINTER, "AT")
    )
    if (
        len(organization_reference) != 1
        or organization_reference[0] not in organization_uids
        or index_pointer.group % 2 == 1
        or index_pointer in {FRAME_CONTENT_SEQUENCE, DIMENSION_INDEX_VALUES}
    ):
        return False

    group_element = dimension.get(FUNCTIONAL_GROUP_POINTER)
    if isinstance(group_element, DataElement):
        group_pointer = _single_attribute_tag(group_element)
        return group_pointer.group % 2 == 0 and _functional_group_reference_valid(
            shared, per_frame, group_pointer, index_pointer
        )
    if group_element is not None:
        return False

    root_target = dataset.get(index_pointer)
    return (
        _functional_group_reference_valid(shared, per_frame, index_pointer, None)
        or isinstance(root_target, DataElement)
        and root_target.VR != "SQ"
    )


def _required_functional_group_items(
    shared: Dataset, per_frame: Sequence, tag: BaseTag
) -> list[Dataset]:
    shared_item = _functional_group_item(shared, tag)
    per_frame_items = [_functional_group_item(frame, tag) for frame in per_frame]
    if shared_item is not None:
        if any(item is not None for item in per_frame_items):
            raise _PrivacyViolation()
        return [shared_item]
    if not per_frame_items or any(item is None for item in per_frame_items):
        raise _PrivacyViolation()
    return [item for item in per_frame_items if item is not None]


def _required_per_frame_functional_group_items(
    shared: Dataset, per_frame: Sequence, tag: BaseTag
) -> list[Dataset]:
    if tag in shared:
        raise _PrivacyViolation()
    items = [_functional_group_item(frame, tag) for frame in per_frame]
    if not items or any(item is None for item in items):
        raise _PrivacyViolation()
    return [item for item in items if item is not None]


def _audit_functional_group_surface(
    container: Dataset, *, sop_class: str, shared: bool
) -> None:
    legacy = sop_class == LEGACY_CONVERTED_ENHANCED_MR_IMAGE_STORAGE_UID
    allowed = set(COMMON_FUNCTIONAL_GROUP_MACROS)
    if legacy:
        allowed.update(
            {
                UNASSIGNED_SHARED_CONVERTED_ATTRIBUTES_SEQUENCE,
                UNASSIGNED_PER_FRAME_CONVERTED_ATTRIBUTES_SEQUENCE,
            }
        )
    else:
        allowed.update(CURRENT_ENHANCED_FUNCTIONAL_GROUP_MACROS)
        if shared:
            allowed.add(REFERENCED_IMAGE_SEQUENCE)
        else:
            allowed.add(MR_METABOLITE_MAP_SEQUENCE)
    for element in container:
        if (
            element.VR != "SQ"
            or not isinstance(element.value, Sequence)
            or element.tag not in allowed
        ):
            raise _PrivacyViolation()
        if element.tag == FRAME_CONTENT_SEQUENCE and shared:
            raise _PrivacyViolation()
        if (
            element.tag == UNASSIGNED_SHARED_CONVERTED_ATTRIBUTES_SEQUENCE
            and not shared
        ):
            raise _PrivacyViolation()
        if element.tag == UNASSIGNED_PER_FRAME_CONVERTED_ATTRIBUTES_SEQUENCE and shared:
            raise _PrivacyViolation()
        if element.tag == CONVERSION_SOURCE_ATTRIBUTES_SEQUENCE:
            raise _PrivacyViolation()


def _audit_source_image_sequence(element: DataElement) -> None:
    if (
        element.VR != "SQ"
        or not isinstance(element.value, Sequence)
        or not 1 <= len(element.value) <= MAX_SEQUENCE_ITEMS
    ):
        raise _PrivacyViolation()
    required = {Tag(0x0008, 0x1150), Tag(0x0008, 0x1155)}
    for item in element.value:
        if not isinstance(item, Dataset) or set(item.keys()) != required:
            raise _PrivacyViolation()
        sop_class = _text_components(_root_element(item, Tag(0x0008, 0x1150), "UI"))
        sop_instance = _text_components(_root_element(item, Tag(0x0008, 0x1155), "UI"))
        if (
            len(sop_class) != 1
            or not _valid_uid(sop_class[0])
            or not sop_class[0].startswith("1.2.840.10008.")
            or len(sop_instance) != 1
            or REMAPPED_UID_RE.fullmatch(sop_instance[0]) is None
        ):
            raise _PrivacyViolation()


def _audit_referenced_image_sequence(element: DataElement) -> None:
    if (
        element.VR != "SQ"
        or not isinstance(element.value, Sequence)
        or not 1 <= len(element.value) <= MAX_SEQUENCE_ITEMS
    ):
        raise _PrivacyViolation()
    required_reference = {
        REFERENCED_SOP_CLASS_UID,
        REFERENCED_SOP_INSTANCE_UID,
        REFERENCED_FRAME_NUMBER,
        PURPOSE_OF_REFERENCE_CODE_SEQUENCE,
    }
    required_code = {
        Tag(0x0008, 0x0100),
        Tag(0x0008, 0x0102),
        Tag(0x0008, 0x0104),
        Tag(0x0008, 0x0117),
    }
    expected_code = {
        Tag(0x0008, 0x0100): ("SH", "121311"),
        Tag(0x0008, 0x0102): ("SH", "DCM"),
        Tag(0x0008, 0x0104): ("LO", "Localizer"),
        Tag(0x0008, 0x0117): ("UI", "1.2.840.10008.6.1.508"),
    }
    for item in element.value:
        if not isinstance(item, Dataset) or set(item.keys()) != required_reference:
            raise _PrivacyViolation()
        sop_class = _text_components(
            _root_element(item, REFERENCED_SOP_CLASS_UID, "UI")
        )
        sop_instance = _text_components(
            _root_element(item, REFERENCED_SOP_INSTANCE_UID, "UI")
        )
        if (
            sop_class != [ENHANCED_MR_IMAGE_STORAGE_UID]
            or len(sop_instance) != 1
            or REMAPPED_UID_RE.fullmatch(sop_instance[0]) is None
        ):
            raise _PrivacyViolation()
        _single_integer_string(
            item,
            REFERENCED_FRAME_NUMBER,
            minimum=1,
            maximum=MAX_DICOM_INSTANCES,
        )
        purpose = _exact_sequence(item, PURPOSE_OF_REFERENCE_CODE_SEQUENCE)
        if (
            purpose is None
            or len(purpose) != 1
            or set(purpose[0].keys()) != required_code
        ):
            raise _PrivacyViolation()
        for tag, (vr, value) in expected_code.items():
            if _text_components(_root_element(purpose[0], tag, vr)) != [value]:
                raise _PrivacyViolation()


def _audit_mr_metabolite_map_sequence(element: DataElement) -> None:
    if (
        element.VR != "SQ"
        or not isinstance(element.value, Sequence)
        or len(element.value) != 1
        or not isinstance(element.value[0], Dataset)
        or set(element.value[0].keys()) != {METABOLITE_MAP_DESCRIPTION}
        or _text_components(
            _root_element(element.value[0], METABOLITE_MAP_DESCRIPTION, "ST")
        )
        != ["WATER"]
    ):
        raise _PrivacyViolation()


def _audit_empty_single_item_macro(container: Dataset, tag: BaseTag) -> None:
    items = _exact_sequence(container, tag)
    if items is None or len(items) != 1 or len(items[0]) != 0:
        raise _PrivacyViolation()


def _exact_decimal_values(dataset: Dataset, tag: BaseTag, count: int) -> list[float]:
    values = _finite_decimal_values(_root_element(dataset, tag, "DS"))
    if len(values) != count:
        raise _PrivacyViolation()
    return values


def _required_code(dataset: Dataset, tag: BaseTag) -> str:
    return _required_root_code(dataset, tag)


def _audit_pixel_measures_item(item: Dataset) -> None:
    spacing = _exact_decimal_values(item, Tag(0x0028, 0x0030), 2)
    thickness = _exact_decimal_values(item, Tag(0x0018, 0x0050), 1)[0]
    if (
        any(not 0.0 < value <= 1.0e6 for value in spacing)
        or not 0.0 < thickness <= 1.0e6
    ):
        raise _PrivacyViolation()


def _audit_plane_position_item(item: Dataset) -> None:
    if any(
        abs(value) > 1.0e9
        for value in _exact_decimal_values(item, Tag(0x0020, 0x0032), 3)
    ):
        raise _PrivacyViolation()


def _audit_plane_orientation_item(item: Dataset) -> None:
    values = _exact_decimal_values(item, Tag(0x0020, 0x0037), 6)
    first, second = values[:3], values[3:]
    first_norm = sum(value * value for value in first)
    second_norm = sum(value * value for value in second)
    dot = sum(left * right for left, right in zip(first, second, strict=True))
    if (
        abs(first_norm - 1.0) > 1.0e-3
        or abs(second_norm - 1.0) > 1.0e-3
        or abs(dot) > 1.0e-3
    ):
        raise _PrivacyViolation()


def _audit_frame_anatomy_item(item: Dataset) -> None:
    _required_code(item, Tag(0x0020, 0x9072))
    anatomy = _exact_sequence(item, Tag(0x0008, 0x2218))
    if anatomy is None or len(anatomy) != 1:
        raise _PrivacyViolation()
    code = anatomy[0]
    required = {
        Tag(0x0008, 0x0100),
        Tag(0x0008, 0x0102),
        Tag(0x0008, 0x0104),
    }
    context_uid = Tag(0x0008, 0x0117)
    if set(code.keys()) not in {
        frozenset(required),
        frozenset(required | {context_uid}),
    }:
        raise _PrivacyViolation()
    for tag in (Tag(0x0008, 0x0100), Tag(0x0008, 0x0102)):
        values = _text_components(_root_element(code, tag, "SH"))
        if len(values) != 1 or re.fullmatch(r"[A-Za-z0-9._-]{1,16}", values[0]) is None:
            raise _PrivacyViolation()
    if _text_components(_root_element(code, Tag(0x0008, 0x0104), "LO")) != ["ANATOMY"]:
        raise _PrivacyViolation()
    if context_uid in code and _text_components(
        _root_element(code, context_uid, "UI")
    ) != [ANATOMY_CONTEXT_UID]:
        raise _PrivacyViolation()


def _audit_frame_type_items(
    items: list[Dataset],
    *,
    root_image_type: list[str],
    root_pixel_presentation: str,
    root_volumetric_properties: str,
    root_volume_calculation: str,
    sop_class: str,
) -> None:
    legacy = sop_class == LEGACY_CONVERTED_ENHANCED_MR_IMAGE_STORAGE_UID
    origins: set[str] = set()
    for item in items:
        frame_type = _root_element(item, Tag(0x0008, 0x9007), "CS")
        _audit_positional_frame_type(frame_type, sop_class)
        values = _text_components(frame_type, allow_empty=True)
        origin = values[0]
        origins.add(origin)
        pixel = _required_code(item, Tag(0x0008, 0x9205))
        volumetric = _required_code(item, Tag(0x0008, 0x9206))
        calculation = _required_code(item, Tag(0x0008, 0x9207))
        if (
            root_pixel_presentation != "MIXED"
            and pixel != root_pixel_presentation
            or root_volumetric_properties != "MIXED"
            and volumetric != root_volumetric_properties
            or root_volume_calculation != "MIXED"
            and calculation != root_volume_calculation
            or origin == "ORIGINAL"
            and calculation != "NONE"
        ):
            raise _PrivacyViolation()
    root_origin = root_image_type[0]
    if root_origin == "ORIGINAL":
        valid = origins == {"ORIGINAL"}
    elif root_origin == "DERIVED":
        valid = origins == {"DERIVED"}
    elif root_origin == "MIXED" and legacy and origins == {"MIXED"}:
        valid = True
    elif root_origin == "MIXED":
        valid = {"ORIGINAL", "DERIVED"}.issubset(origins)
    else:
        valid = False
    if not valid:
        raise _PrivacyViolation()


def _audit_frame_content_item(item: Dataset, *, current_enhanced: bool) -> None:
    if not current_enhanced:
        return
    for tag in (Tag(0x0018, 0x9074), Tag(0x0018, 0x9151)):
        if _text_components(_root_element(item, tag, "DT")) != [
            ENHANCED_FRAME_DATETIME_SENTINEL
        ]:
            raise _PrivacyViolation()
    duration = _exact_numbers(item, Tag(0x0018, 0x9220), vr="FD", count=1)
    if duration is None or not 0.0 <= duration[0] <= 1.0e12:
        raise _PrivacyViolation()


def _audit_mr_pulse_sequence_module(dataset: Dataset) -> None:
    name = _text_components(_root_element(dataset, Tag(0x0018, 0x9005), "SH"))
    if len(name) != 1 or name[0] not in CANONICAL_SEQUENCE_NAMES | {"OTHER"}:
        raise _PrivacyViolation()
    _required_code(dataset, Tag(0x0018, 0x0023))
    echo = _required_code(dataset, Tag(0x0018, 0x9008))
    if echo in {"SPIN", "BOTH"}:
        _required_code(dataset, Tag(0x0018, 0x9011))
    for tag in (
        Tag(0x0018, 0x9012),
        Tag(0x0018, 0x9014),
        Tag(0x0018, 0x9015),
        Tag(0x0018, 0x9017),
        Tag(0x0018, 0x9018),
        Tag(0x0018, 0x9024),
        Tag(0x0018, 0x9025),
        Tag(0x0018, 0x9029),
        Tag(0x0018, 0x9032),
        Tag(0x0018, 0x9033),
    ):
        _required_code(dataset, tag)
    if _required_code(dataset, Tag(0x0018, 0x9032)) == "RECTILINEAR":
        _required_code(dataset, Tag(0x0018, 0x9034))
    _single_unsigned_short(dataset, Tag(0x0018, 0x9093))


def _audit_mr_timing_item(item: Dataset) -> None:
    repetition = _exact_decimal_values(item, Tag(0x0018, 0x0080), 1)[0]
    flip = _exact_decimal_values(item, Tag(0x0018, 0x1314), 1)[0]
    echo_train = _single_integer_string(
        item, Tag(0x0018, 0x0091), minimum=0, maximum=1_000_000
    )
    if not 0.0 < repetition <= 1.0e9 or not 0.0 <= flip <= 360.0 or echo_train < 0:
        raise _PrivacyViolation()
    _single_unsigned_short(item, Tag(0x0018, 0x9240))
    _single_unsigned_short(item, Tag(0x0018, 0x9241))


def _audit_mr_echo_item(item: Dataset) -> None:
    value = _exact_numbers(item, Tag(0x0018, 0x9082), vr="FD", count=1)
    if value is None or not 0.0 <= value[0] <= 1.0e9:
        raise _PrivacyViolation()


def _audit_mr_modifier_item(item: Dataset) -> None:
    for tag in (
        Tag(0x0018, 0x9009),
        Tag(0x0018, 0x9010),
        Tag(0x0018, 0x9016),
        Tag(0x0018, 0x9021),
        Tag(0x0018, 0x9026),
        Tag(0x0018, 0x9027),
        Tag(0x0018, 0x9077),
        Tag(0x0018, 0x9081),
    ):
        _required_code(item, tag)
    if _required_code(item, Tag(0x0018, 0x9077)) == "YES":
        _required_code(item, Tag(0x0018, 0x9078))
    if _required_code(item, Tag(0x0018, 0x9081)) == "YES":
        _required_code(item, Tag(0x0018, 0x9036))


def _audit_mr_imaging_modifier_item(item: Dataset) -> None:
    for tag in (Tag(0x0018, 0x9020), Tag(0x0018, 0x9022), Tag(0x0018, 0x9028)):
        _required_code(item, tag)
    transmitter = _exact_numbers(item, Tag(0x0018, 0x9098), vr="FD", count=1)
    bandwidth = _exact_decimal_values(item, Tag(0x0018, 0x0095), 1)[0]
    if transmitter is None or transmitter[0] <= 0.0 or bandwidth <= 0.0:
        raise _PrivacyViolation()


def _audit_mr_receive_coil_item(item: Dataset) -> None:
    names = _text_components(_root_element(item, RECEIVE_COIL_NAME, "SH"))
    manufacturer = _root_element(item, RECEIVE_COIL_MANUFACTURER_NAME, "LO")
    coil_type = _required_code(item, RECEIVE_COIL_TYPE)
    if (
        len(names) != 1
        or CANONICAL_COIL_RE.fullmatch(names[0]) is None
        or not manufacturer.is_empty
    ):
        raise _PrivacyViolation()
    _required_code(item, QUADRATURE_RECEIVE_COIL)
    if coil_type == "MULTICOIL":
        if names != ["MULTI_COIL"] or set(item.keys()) != {
            RECEIVE_COIL_NAME,
            RECEIVE_COIL_MANUFACTURER_NAME,
            RECEIVE_COIL_TYPE,
            QUADRATURE_RECEIVE_COIL,
            MULTI_COIL_DEFINITION_SEQUENCE,
        }:
            raise _PrivacyViolation()
        definitions = _exact_sequence(item, MULTI_COIL_DEFINITION_SEQUENCE)
        if definitions is None or not 1 <= len(definitions) <= MAX_MULTI_COIL_ELEMENTS:
            raise _PrivacyViolation()
        for definition in definitions:
            if set(definition.keys()) != {
                MULTI_COIL_ELEMENT_NAME,
                MULTI_COIL_ELEMENT_USED,
            }:
                raise _PrivacyViolation()
            element_names = _text_components(
                _root_element(definition, MULTI_COIL_ELEMENT_NAME, "SH")
            )
            if element_names != ["MULTI_ELEMENT"]:
                raise _PrivacyViolation()
            _required_code(definition, MULTI_COIL_ELEMENT_USED)
    elif (
        MULTI_COIL_DEFINITION_SEQUENCE in item or MULTI_COIL_CONFIGURATION in item
    ):
        raise _PrivacyViolation()


def _audit_mr_transmit_coil_item(item: Dataset) -> None:
    names = _text_components(_root_element(item, TRANSMIT_COIL_NAME, "SH"))
    manufacturer = _root_element(item, TRANSMIT_COIL_MANUFACTURER_NAME, "LO")
    if (
        len(names) != 1
        or CANONICAL_COIL_RE.fullmatch(names[0]) is None
        or not manufacturer.is_empty
    ):
        raise _PrivacyViolation()
    coil_type = _required_code(item, TRANSMIT_COIL_TYPE)
    if names == ["SURFACE"] and (
        coil_type != "SURFACE"
        or set(item.keys())
        != {
            TRANSMIT_COIL_NAME,
            TRANSMIT_COIL_MANUFACTURER_NAME,
            TRANSMIT_COIL_TYPE,
        }
    ):
        raise _PrivacyViolation()


def _audit_mr_averages_item(item: Dataset) -> None:
    value = _exact_decimal_values(item, Tag(0x0018, 0x0083), 1)[0]
    if not 0.0 < value <= 1.0e9:
        raise _PrivacyViolation()


def _audit_mr_fov_item(item: Dataset) -> None:
    direction = _required_code(item, Tag(0x0018, 0x1312))
    sampling = _exact_decimal_values(item, Tag(0x0018, 0x0093), 1)[0]
    phase_fov = _exact_decimal_values(item, Tag(0x0018, 0x0094), 1)[0]
    if (
        direction not in {"ROW", "COLUMN", "OTHER"}
        or not 0.0 < sampling <= 100.0
        or not 0.0 < phase_fov <= 100.0
    ):
        raise _PrivacyViolation()
    _single_unsigned_short(item, Tag(0x0018, 0x9058))
    _single_unsigned_short(item, Tag(0x0018, 0x9231))


def _audit_enhanced_mr_iod_contract(dataset: Dataset, sop_class: str) -> None:
    if _text_components(_root_element(dataset, Tag(0x0008, 0x0023), "DA")) != [
        ENHANCED_CONTENT_DATE_SENTINEL
    ] or _text_components(_root_element(dataset, Tag(0x0008, 0x0033), "TM")) != [
        ENHANCED_CONTENT_TIME_SENTINEL
    ]:
        raise _PrivacyViolation()
    _single_integer_string(
        dataset,
        Tag(0x0020, 0x0013),
        minimum=-(2**31),
        maximum=2**31 - 1,
    )

    pixel_presentation = _required_root_code(dataset, Tag(0x0008, 0x9205))
    if pixel_presentation != "MONOCHROME":
        raise _PrivacyViolation()
    volumetric_properties = _required_root_code(dataset, Tag(0x0008, 0x9206))
    volume_calculation = _required_root_code(dataset, Tag(0x0008, 0x9207))
    image_type = _text_components(
        _root_element(dataset, Tag(0x0008, 0x0008), "CS"), allow_empty=True
    )
    _audit_positional_enhanced_type_values(
        image_type,
        root=True,
        legacy=sop_class == LEGACY_CONVERTED_ENHANCED_MR_IMAGE_STORAGE_UID,
    )
    if image_type[0] == "ORIGINAL" and volume_calculation != "NONE":
        raise _PrivacyViolation()
    if _single_code(dataset, Tag(0x2050, 0x0020)) != "IDENTITY":
        raise _PrivacyViolation()

    frames = _single_integer_string(
        dataset,
        NUMBER_OF_FRAMES,
        minimum=1,
        maximum=MAX_DICOM_INSTANCES,
    )
    shared = _exact_sequence(dataset, Tag(0x5200, 0x9229))
    per_frame = _exact_sequence(dataset, Tag(0x5200, 0x9230))
    if (
        shared is None
        or len(shared) != 1
        or per_frame is None
        or len(per_frame) != frames
    ):
        raise _PrivacyViolation()
    context = _exact_sequence(dataset, ACQUISITION_CONTEXT_SEQUENCE)
    if context is None or len(context) != 0:
        raise _PrivacyViolation()
    if set(dataset.keys()) & CONCATENATION_ATTRIBUTES:
        raise _PrivacyViolation()
    _audit_functional_group_surface(shared[0], sop_class=sop_class, shared=True)
    for frame in per_frame:
        _audit_functional_group_surface(frame, sop_class=sop_class, shared=False)
    for item in _required_functional_group_items(
        shared[0], per_frame, PIXEL_MEASURES_SEQUENCE
    ):
        _audit_pixel_measures_item(item)
    for item in _required_functional_group_items(
        shared[0], per_frame, PLANE_POSITION_SEQUENCE
    ):
        _audit_plane_position_item(item)
    for item in _required_functional_group_items(
        shared[0], per_frame, PLANE_ORIENTATION_SEQUENCE
    ):
        _audit_plane_orientation_item(item)
    _audit_frame_type_items(
        _required_functional_group_items(
            shared[0], per_frame, MR_IMAGE_FRAME_TYPE_SEQUENCE
        ),
        root_image_type=image_type,
        root_pixel_presentation=pixel_presentation,
        root_volumetric_properties=volumetric_properties,
        root_volume_calculation=volume_calculation,
        sop_class=sop_class,
    )
    frame_contents = _required_per_frame_functional_group_items(
        shared[0], per_frame, FRAME_CONTENT_SEQUENCE
    )
    if sop_class == ENHANCED_MR_IMAGE_STORAGE_UID:
        for item in _required_functional_group_items(
            shared[0], per_frame, FRAME_ANATOMY_SEQUENCE
        ):
            _audit_frame_anatomy_item(item)
        _required_functional_group_items(
            shared[0], per_frame, PIXEL_VALUE_TRANSFORMATION_SEQUENCE
        )
    else:
        _audit_empty_single_item_macro(
            shared[0], UNASSIGNED_SHARED_CONVERTED_ATTRIBUTES_SEQUENCE
        )
        for frame in per_frame:
            _audit_empty_single_item_macro(
                frame, UNASSIGNED_PER_FRAME_CONVERTED_ATTRIBUTES_SEQUENCE
            )
    for item in frame_contents:
        _audit_frame_content_item(
            item, current_enhanced=sop_class == ENHANCED_MR_IMAGE_STORAGE_UID
        )

    if sop_class == ENHANCED_MR_IMAGE_STORAGE_UID and image_type[0] in {
        "ORIGINAL",
        "MIXED",
    }:
        _audit_mr_pulse_sequence_module(dataset)
        validators = (
            (Tag(0x0018, 0x9112), _audit_mr_timing_item),
            (Tag(0x0018, 0x9114), _audit_mr_echo_item),
            (Tag(0x0018, 0x9115), _audit_mr_modifier_item),
            (Tag(0x0018, 0x9006), _audit_mr_imaging_modifier_item),
            (Tag(0x0018, 0x9042), _audit_mr_receive_coil_item),
            (Tag(0x0018, 0x9049), _audit_mr_transmit_coil_item),
            (Tag(0x0018, 0x9119), _audit_mr_averages_item),
        )
        for macro, validator in validators:
            for item in _required_functional_group_items(shared[0], per_frame, macro):
                validator(item)
        if _required_code(dataset, Tag(0x0018, 0x9032)) == "RECTILINEAR":
            for item in _required_functional_group_items(
                shared[0], per_frame, Tag(0x0018, 0x9125)
            ):
                _audit_mr_fov_item(item)

    dimension_organizations = _exact_sequence(dataset, Tag(0x0020, 0x9221))
    dimension_indexes = _exact_sequence(dataset, Tag(0x0020, 0x9222))
    dimensions_required = sop_class == ENHANCED_MR_IMAGE_STORAGE_UID
    dimensions_present = (
        dimension_organizations is not None
        or dimension_indexes is not None
        or any(DIMENSION_INDEX_VALUES in item for item in frame_contents)
    )
    if not dimensions_required and not dimensions_present:
        dimension_organizations = None
        dimension_indexes = None
    elif not dimension_organizations or not dimension_indexes:
        raise _PrivacyViolation()

    if dimension_organizations is None or dimension_indexes is None:
        return
    organization_uids: set[str] = set()
    for organization_item in dimension_organizations:
        uid_values = _text_components(
            _root_element(organization_item, Tag(0x0020, 0x9164), "UI")
        )
        if len(uid_values) != 1 or REMAPPED_UID_RE.fullmatch(uid_values[0]) is None:
            raise _PrivacyViolation()
        organization_uids.add(uid_values[0])
    if len(organization_uids) != len(dimension_organizations):
        raise _PrivacyViolation()
    for dimension in dimension_indexes:
        if not _dimension_index_item_valid(
            dataset,
            shared[0],
            per_frame,
            dimension,
            organization_uids,
        ):
            raise _PrivacyViolation()
    for frame_content in frame_contents:
        dimension_values = frame_content.get(DIMENSION_INDEX_VALUES)
        values = (
            _components(dimension_values.value)
            if isinstance(dimension_values, DataElement)
            else []
        )
        if (
            not isinstance(dimension_values, DataElement)
            or dimension_values.VR != "UL"
            or len(values) != len(dimension_indexes)
            or any(
                isinstance(value, bool)
                or not isinstance(value, Integral)
                or int(value) <= 0
                for value in values
            )
        ):
            raise _PrivacyViolation()

    if sop_class == ENHANCED_MR_IMAGE_STORAGE_UID:
        if _single_code(dataset, Tag(0x0028, 0x0301)) != "NO":
            raise _PrivacyViolation()
        _required_root_code(dataset, Tag(0x0008, 0x9208))
        _required_root_code(dataset, Tag(0x0008, 0x9209))
        lossy = _required_root_code(dataset, Tag(0x0028, 0x2110))
        if lossy == "01":
            ratios = _finite_decimal_values(
                _root_element(dataset, Tag(0x0028, 0x2112), "DS")
            )
            methods = _text_components(
                _root_element(dataset, Tag(0x0028, 0x2114), "CS")
            )
            if (
                len(ratios) != len(methods)
                or any(ratio <= 0 for ratio in ratios)
                or any(
                    method not in CODE_VALUES[Tag(0x0028, 0x2114)] for method in methods
                )
            ):
                raise _PrivacyViolation()


def _audit_sop_conformance(
    dataset: Dataset,
    *,
    expected_subject_id: str,
    sop_class: str,
) -> int:
    for tag, vr in CORE_TYPE_1_ATTRIBUTES.items():
        element = _root_element(dataset, tag, vr)
        if element.is_empty:
            raise _PrivacyViolation()

    if _single_code(dataset, Tag(0x0008, 0x0060)) != "MR":
        raise _PrivacyViolation()
    if str(dataset[Tag(0x0008, 0x0016)].value).strip(" \0") != sop_class:
        raise _PrivacyViolation()
    for tag in (Tag(0x0010, 0x0010), Tag(0x0010, 0x0020)):
        values = _text_components(dataset[tag])
        if len(values) != 1 or values[0] != expected_subject_id:
            raise _PrivacyViolation()

    frame_of_reference = _root_element(dataset, FRAME_OF_REFERENCE_UID, "UI")
    frame_values = _text_components(frame_of_reference)
    if len(frame_values) != 1 or REMAPPED_UID_RE.fullmatch(frame_values[0]) is None:
        raise _PrivacyViolation()

    manufacturer = _root_element(dataset, MANUFACTURER, "LO")
    if not manufacturer.is_empty:
        values = _text_components(manufacturer)
        if len(values) != 1 or not safe_scanner_text(values[0]):
            raise _PrivacyViolation()

    for tag, vr in PRIVACY_TYPE_2_EMPTY_ATTRIBUTES.items():
        element = _root_element(dataset, tag, vr)
        if sop_class in ENHANCED_MR_IMAGE_STORAGE_UIDS and tag in {
            Tag(0x0008, 0x0023),
            Tag(0x0008, 0x0033),
        }:
            continue
        if not element.is_empty:
            raise _PrivacyViolation()
    for tag, vr in NUMERIC_TYPE_2_ATTRIBUTES.items():
        _audit_type_2_numeric_shell(_root_element(dataset, tag, vr), vr)

    if sop_class == CLASSIC_MR_IMAGE_STORAGE_UID:
        for tag, vr in CLASSIC_MR_TYPE_1_CODES.items():
            element = _root_element(dataset, tag, vr)
            values = _text_components(element)
            allowed = CODE_VALUES.get(tag)
            if (
                allowed is None
                or any(value not in allowed for value in values)
                or len(set(values)) != len(values)
            ):
                raise _PrivacyViolation()
        for tag, vr in CLASSIC_MR_TYPE_2_ATTRIBUTES.items():
            _audit_classic_mr_type_2(_root_element(dataset, tag, vr), vr)
    elif sop_class == ENHANCED_MR_IMAGE_STORAGE_UID:
        if manufacturer.is_empty:
            raise _PrivacyViolation()
        model = _root_element(dataset, MANUFACTURER_MODEL_NAME, "LO")
        versions = _root_element(dataset, SOFTWARE_VERSIONS, "LO")
        serial = _root_element(dataset, DEVICE_SERIAL_NUMBER, "LO")
        model_values = _text_components(model)
        version_values = _text_components(versions)
        serial_values = _text_components(serial)
        if (
            len(model_values) != 1
            or not safe_scanner_text(model_values[0])
            or not 1 <= len(version_values) <= 16
            or any(not safe_scanner_text(value) for value in version_values)
            or len(serial_values) != 1
            or PSEUDONYMOUS_DEVICE_SERIAL_RE.fullmatch(serial_values[0]) is None
        ):
            raise _PrivacyViolation()

    if sop_class in ENHANCED_MR_IMAGE_STORAGE_UIDS:
        _audit_enhanced_mr_iod_contract(dataset, sop_class)

    expected_pixel_value_length = _audit_pixel_module(dataset, sop_class)
    _audit_pixel_transforms(dataset)
    return expected_pixel_value_length


def _audit_philips_scaling_fallback(
    dataset: Dataset, state: _AuditState, sop_class: str
) -> None:
    fields = state.philips_private_fields
    scale_fields = fields & {"scale_intercept", "scale_slope"}
    if scale_fields and scale_fields != {"scale_intercept", "scale_slope"}:
        raise _PrivacyViolation()

    classic_conversion_fields = fields & {"number_of_slices", "water_fat_shift"}
    if (
        classic_conversion_fields
        and not scale_fields
        and not ROOT_RESCALE_TAGS.issubset(dataset.keys())
    ):
        if sop_class == ENHANCED_MR_IMAGE_STORAGE_UID:
            # The current Enhanced MR IOD audit has already required a complete
            # Pixel Value Transformation macro in the shared/per-frame
            # functional groups, and the recursive transform audit has proved
            # its exact public rescale triplet. It is the quantitative fallback
            # for Enhanced images; a duplicate root triplet is neither required
            # nor emitted by the workstation sanitizer.
            return
        # Philips private scaling is optional only when the canonical public
        # rescale triplet survived as an atomic quantitative fallback.
        raise _PrivacyViolation()


def _public_attribute_allowed(tag: BaseTag, vr: str) -> bool:
    group = tag.group
    element = tag.element
    if group == 0x0008:
        return element in {
            0x0005,
            0x0008,
            0x0100,
            0x0102,
            0x0104,
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
            0x2112,
            0x2218,
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
            0x1000,
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
            "SH",
            "LO",
            "DT",
        }
        return element in classic or element >= 0x9000 and vr in enhanced_vrs
    if group == 0x0020:
        classic = {
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
        }
        geometry_vrs = {
            0x0242: {"UI"},
            0x9056: {"SH"},
            0x9057: {"UL"},
            0x9071: {"SQ"},
            0x9072: {"CS"},
            0x9111: {"SQ"},
            0x9113: {"SQ"},
            0x9116: {"SQ"},
            0x9128: {"UL"},
            0x9153: {"FD"},
            0x9156: {"US"},
            0x9157: {"UL"},
            0x9161: {"UI"},
            0x9162: {"US"},
            0x9163: {"US"},
            0x9164: {"UI"},
            0x9165: {"AT"},
            0x9167: {"AT"},
            0x9170: {"SQ"},
            0x9171: {"SQ"},
            0x9172: {"SQ"},
            0x9221: {"SQ"},
            0x9222: {"SQ"},
            0x9228: {"UL"},
        }
        return element in classic or vr in geometry_vrs.get(element, set())
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
        return element in {0x0555, 0x9094, 0x9210, 0x9211, 0x9212, 0x9216}
    if group == 0x2050:
        return element == 0x0020
    if group == 0x5200:
        return element in {0x9229, 0x9230}
    if group == 0x7FE0:
        return element in {0x0001, 0x0002} and vr == "OV"
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


def _audit_positional_image_type(element: DataElement, sop_class: str) -> None:
    if element.VR != "CS":
        raise _PrivacyViolation()
    values = _text_components(element, allow_empty=True)
    if sop_class in ENHANCED_MR_IMAGE_STORAGE_UIDS:
        _audit_positional_enhanced_type_values(
            values,
            root=True,
            legacy=sop_class == LEGACY_CONVERTED_ENHANCED_MR_IMAGE_STORAGE_UID,
        )
        return
    if (
        not 2 <= len(values) <= 64
        or values[0] not in {"ORIGINAL", "DERIVED"}
        or values[1] not in {"PRIMARY", "SECONDARY"}
        or any(
            value and value not in CLASSIC_IMAGE_TYPE_TRAILING_VALUES
            for value in values[2:]
        )
    ):
        raise _PrivacyViolation()


def _audit_positional_enhanced_type_values(
    values: list[str], *, root: bool, legacy: bool
) -> None:
    value_1 = ENHANCED_ROOT_TYPE_VALUE_1 if root or legacy else FRAME_TYPE_VALUE_1
    value_4_valid = (
        (
            values[3] in FRAME_TYPE_VALUE_4
            or root
            and values[3] == "MIXED"
            or legacy
            and not values[3]
        )
        if len(values) == 4
        else False
    )
    if (
        len(values) != 4
        or values[0] not in value_1
        or values[1] not in FRAME_TYPE_VALUE_2
        or values[2] not in FRAME_TYPE_VALUE_3
        or not value_4_valid
        or values[0] == "ORIGINAL"
        and values[3] != "NONE"
        and not (legacy and not values[3])
    ):
        raise _PrivacyViolation()


def _audit_positional_frame_type(element: DataElement, sop_class: str) -> None:
    if element.VR != "CS":
        raise _PrivacyViolation()
    _audit_positional_enhanced_type_values(
        _text_components(element, allow_empty=True),
        root=False,
        legacy=sop_class == LEGACY_CONVERTED_ENHANCED_MR_IMAGE_STORAGE_UID,
    )


def _audit_geometry_value(element: DataElement) -> bool:
    tag = element.tag
    if tag == Tag(0x0020, 0x9056):
        values = _text_components(element)
        if (
            element.VR != "SH"
            or len(values) != 1
            or re.fullmatch(r"[a-f0-9]{16}", values[0]) is None
        ):
            raise _PrivacyViolation()
        return True
    if tag == Tag(0x0020, 0x9153):
        _audit_private_float(
            element,
            vr="FD",
            count=1,
            valid=lambda _values: True,
        )
        return True
    if tag == Tag(0x0020, 0x9157):
        values = _components(element.value)
        if (
            element.VR != "UL"
            or not 1 <= len(values) <= 64
            or any(
                isinstance(value, bool)
                or not isinstance(value, Integral)
                or int(value) <= 0
                for value in values
            )
        ):
            raise _PrivacyViolation()
        return True
    if tag == Tag(0x0020, 0x9057):
        values = _components(element.value)
        if (
            element.VR != "UL"
            or len(values) != 1
            or isinstance(values[0], bool)
            or not isinstance(values[0], Integral)
            or int(values[0]) <= 0
        ):
            raise _PrivacyViolation()
        return True
    if tag == Tag(0x0020, 0x9228):
        values = _components(element.value)
        if (
            element.VR != "UL"
            or len(values) != 1
            or isinstance(values[0], bool)
            or not isinstance(values[0], Integral)
        ):
            raise _PrivacyViolation()
        return True
    if tag in {Tag(0x0020, 0x9162), Tag(0x0020, 0x9163)}:
        values = _components(element.value)
        if (
            element.VR != "US"
            or len(values) != 1
            or isinstance(values[0], bool)
            or not isinstance(values[0], Integral)
            or int(values[0]) <= 0
        ):
            raise _PrivacyViolation()
        return True
    if tag in {Tag(0x0020, 0x9165), Tag(0x0020, 0x9167)}:
        values = _components(element.value)
        if element.VR != "AT" or len(values) != 1 or not isinstance(values[0], BaseTag):
            raise _PrivacyViolation()
        return True
    return False


def _audit_special_text(
    element: DataElement,
    expected_subject_id: str,
    expected_deidentification_method: str,
) -> bool:
    tag = element.tag
    if tag not in {
        Tag(0x0010, 0x0010),
        Tag(0x0010, 0x0020),
        Tag(0x0012, 0x0062),
        Tag(0x0012, 0x0063),
        Tag(0x0028, 0x0303),
        Tag(0x0028, 0x0301),
        Tag(0x0008, 0x0070),
        Tag(0x0008, 0x0100),
        Tag(0x0008, 0x0102),
        Tag(0x0008, 0x0104),
        Tag(0x0008, 0x1090),
        DEVICE_SERIAL_NUMBER,
        Tag(0x0018, 0x0024),
        Tag(0x0018, 0x9005),
        Tag(0x0018, 0x9041),
        Tag(0x0018, 0x9050),
        Tag(0x0018, 0x1020),
        Tag(0x0018, 0x1250),
        Tag(0x0018, 0x1251),
        Tag(0x0018, 0x0085),
    }:
        return False
    if tag == MANUFACTURER and element.is_empty:
        if element.VR != "LO":
            raise _PrivacyViolation()
        return True
    if tag in {Tag(0x0018, 0x9041), Tag(0x0018, 0x9050)}:
        if element.VR != "LO" or not element.is_empty:
            raise _PrivacyViolation()
        return True
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
        expected_vr, valid = (
            "LO",
            values == [expected_deidentification_method],
        )
    elif tag == Tag(0x0028, 0x0303):
        expected_vr, valid = "CS", values == ["REMOVED"]
    elif tag == Tag(0x0028, 0x0301):
        expected_vr, valid = "CS", values == ["NO"]
    elif tag == MANUFACTURER:
        expected_vr, valid = (
            "LO",
            len(values) == 1 and safe_scanner_text(values[0]),
        )
    elif tag in {Tag(0x0008, 0x0100), Tag(0x0008, 0x0102)}:
        expected_vr, valid = (
            "SH",
            len(values) == 1
            and re.fullmatch(r"[A-Za-z0-9._-]{1,16}", values[0]) is not None,
        )
    elif tag == Tag(0x0008, 0x0104):
        expected_vr, valid = "LO", values == ["ANATOMY"]
    elif tag == Tag(0x0008, 0x1090):
        expected_vr, valid = (
            "LO",
            len(values) == 1 and safe_scanner_text(values[0]),
        )
    elif tag == DEVICE_SERIAL_NUMBER:
        expected_vr, valid = (
            "LO",
            len(values) == 1
            and PSEUDONYMOUS_DEVICE_SERIAL_RE.fullmatch(values[0]) is not None,
        )
    elif tag == Tag(0x0018, 0x0024):
        expected_vr, valid = (
            "SH",
            len(values) == 1 and values[0] in CANONICAL_SEQUENCE_NAMES,
        )
    elif tag == Tag(0x0018, 0x9005):
        expected_vr, valid = (
            "SH",
            len(values) == 1 and values[0] in CANONICAL_SEQUENCE_NAMES | {"OTHER"},
        )
    elif tag == Tag(0x0018, 0x1020):
        expected_vr, valid = (
            "LO",
            len(values) <= 16 and all(safe_scanner_text(value) for value in values),
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


def _validate_canonical_siemens_csa(value: Any) -> dict[str, tuple[float, ...]]:
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
        elif name == "B_value":
            valid = len(numbers) == 1 and 0 <= numbers[0] <= 1.0e6
        elif name == "DiffusionGradientDirection":
            valid = len(numbers) == 3 and all(-1.1 <= item <= 1.1 for item in numbers)
        elif name == "B_matrix":
            valid = len(numbers) == 6 and all(
                -1.0e9 <= item <= 1.0e9 for item in numbers
            )
        else:
            valid = False
        if not valid:
            raise _PrivacyViolation()
    return {name: tuple(values) for name, values in observed.items()}


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


def _record_private_field(semantic_fields: set[str], field_name: str) -> None:
    if field_name in semantic_fields:
        raise _PrivacyViolation()
    semantic_fields.add(field_name)


def _audit_private_is(
    element: DataElement,
    *,
    count: int,
    valid: Any,
) -> tuple[int, ...]:
    if element.VR != "IS":
        raise _PrivacyViolation()
    raw = _text_components(element)
    if len(raw) != count:
        raise _PrivacyViolation()
    try:
        values = tuple(int(value) for value in raw)
    except (TypeError, ValueError, OverflowError) as exc:
        raise _PrivacyViolation() from exc
    if any(str(value) != source for value, source in zip(values, raw)) or not valid(
        values
    ):
        raise _PrivacyViolation()
    return values


def _audit_private_float(
    element: DataElement,
    *,
    vr: str,
    count: int,
    valid: Any,
) -> tuple[float, ...]:
    if element.VR != vr:
        raise _PrivacyViolation()
    values = _components(element.value)
    if len(values) != count:
        raise _PrivacyViolation()
    try:
        parsed = tuple(float(value) for value in values)
    except (TypeError, ValueError, OverflowError) as exc:
        raise _PrivacyViolation() from exc
    if any(not math.isfinite(value) for value in parsed) or not valid(parsed):
        raise _PrivacyViolation()
    return parsed


def _audit_private_cs(element: DataElement, allowed: set[str] | None = None) -> str:
    if element.VR != "CS":
        raise _PrivacyViolation()
    values = _text_components(element)
    if len(values) != 1:
        raise _PrivacyViolation()
    value = values[0]
    if (
        len(value) > 16
        or not re.fullmatch(r"[A-Z0-9_ ]+", value)
        or (allowed is not None and value not in allowed)
    ):
        raise _PrivacyViolation()
    return value


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
    low = tag.element & 0x00FF
    if tag.group == 0x0019 and creator == SIEMENS_MR_HEADER_CREATOR:
        field = {
            0x0C: "siemens_b_value",
            0x0D: "siemens_directionality",
            0x0E: "siemens_diffusion_gradient",
            0x27: "siemens_b_matrix",
        }.get(low)
        if field is None:
            raise _PrivacyViolation()
        if low == 0x0C:
            _audit_private_is(
                element,
                count=1,
                valid=lambda values: 0 <= values[0] <= 1_000_000,
            )
        elif low == 0x0D:
            _audit_private_cs(element, {"NONE", "ISOTROPIC", "DIRECTIONAL", "BMATRIX"})
        elif low == 0x0E:
            _audit_private_float(
                element,
                vr="FD",
                count=3,
                valid=lambda values: all(-1.1 <= value <= 1.1 for value in values),
            )
        else:
            _audit_private_float(
                element,
                vr="FD",
                count=6,
                valid=lambda values: all(-1.0e9 <= value <= 1.0e9 for value in values),
            )
        _record_private_field(semantic_fields, field)
        return creator_tag, "dicom_ps3.15_siemens_mr_header_diffusion"
    if (
        tag.group == 0x2001
        and creator == PHILIPS_IMAGING_CREATOR
        and low
        in {
            0x03,
            0x04,
            0x08,
        }
    ):
        if low == 0x03:
            _audit_private_float(
                element,
                vr="FL",
                count=1,
                valid=lambda values: 0.0 <= values[0] <= 1_000_000.0,
            )
            field = "philips_diffusion_b_factor"
            exception = "dicom_ps3.15_philips_diffusion"
        elif low == 0x04:
            _audit_private_cs(
                element,
                {"AP", "FH", "RL", "NONE", "ISOTROPIC", "DIRECTIONAL"},
            )
            field = "philips_diffusion_direction"
            exception = "dicom_ps3.15_philips_diffusion"
        else:
            _audit_private_is(
                element,
                count=1,
                valid=lambda values: 0 <= values[0] <= 1_000_000,
            )
            field = "philips_phase_number"
            exception = "dicom_ps3.15_philips_phase_number"
        _record_private_field(semantic_fields, field)
        return creator_tag, exception
    if tag.group == 0x0043 and creator == GE_PARM_CREATOR and low == 0x39:
        _audit_private_is(
            element,
            count=4,
            valid=lambda values: (
                0 <= values[0] <= 1_000_000
                and all(
                    -1_000_000_000 <= value <= 1_000_000_000 for value in values[1:]
                )
            ),
        )
        _record_private_field(semantic_fields, "ge_diffusion_b_value")
        return creator_tag, "dicom_ps3.15_ge_diffusion_b_value"
    if tag.group == 0x0065 and creator == UIH_IMAGE_HEADER_CREATOR:
        if low == 0x50:
            _audit_private_float(
                element,
                vr="DS",
                count=1,
                valid=lambda values: values[0].is_integer() and 1 <= values[0] <= 4096,
            )
            field = "uih_grid_slice_count"
            exception = "uih_image_private_header_grid_slice_count_numeric_v1"
        elif low == 0x09:
            _audit_private_float(
                element,
                vr="FD",
                count=1,
                valid=lambda values: 0 <= values[0] <= 1_000_000,
            )
            field = "uih_diffusion_b_value"
            exception = "uih_image_private_header_diffusion_numeric_v1"
        elif low == 0x37:
            _audit_private_float(
                element,
                vr="FD",
                count=3,
                valid=lambda values: all(-1.1 <= value <= 1.1 for value in values),
            )
            field = "uih_diffusion_gradient"
            exception = "uih_image_private_header_diffusion_numeric_v1"
        else:
            raise _PrivacyViolation()
        _record_private_field(semantic_fields, field)
        return creator_tag, exception
    if (
        tag.group == 0x2005
        and creator == PHILIPS_MR_CREATOR
        and low
        in {
            0xB0,
            0xB1,
            0xB2,
        }
    ):
        _audit_private_float(
            element,
            vr="FL",
            count=1,
            valid=lambda values: -1.1 <= values[0] <= 1.1,
        )
        _record_private_field(
            semantic_fields, f"philips_diffusion_gradient_{low - 0xB0}"
        )
        return (
            creator_tag,
            "philips_mr_imaging_dd_001_diffusion_gradient_vector_numeric_v1",
        )
    if tag.group == 0x2005 and creator == PHILIPS_PER_FRAME_CREATOR:
        if low in {0x12, 0x13}:
            _audit_private_is(
                element,
                count=1,
                valid=lambda values: 0 <= values[0] <= 1_000_000,
            )
            _record_private_field(
                semantic_fields, f"philips_diffusion_index_{low - 0x12}"
            )
            return (
                creator_tag,
                "philips_mr_imaging_dd_005_diffusion_indices_numeric_v1",
            )
        if low == 0x29:
            _audit_private_cs(element, {"LABEL", "CONTROL", "M_ZERO_SCAN"})
            _record_private_field(semantic_fields, "philips_asl_label")
            return creator_tag, "philips_mr_imaging_dd_005_asl_label_code_v1"
    if (
        tag.group == 0x0019
        and creator == GE_ACQU_CREATOR
        and low
        in {
            0xBB,
            0xBC,
            0xBD,
        }
    ):
        _audit_private_float(
            element,
            vr="DS",
            count=1,
            valid=lambda values: -1.1 <= values[0] <= 1.1,
        )
        _record_private_field(semantic_fields, f"ge_diffusion_gradient_{low - 0xBB}")
        return (
            creator_tag,
            "ge_gems_acqu_01_diffusion_gradient_vector_numeric_v1",
        )
    if tag.group == 0x0043 and creator == GE_PARM_CREATOR and low in {0xA3, 0xA5}:
        if low == 0xA3:
            _audit_private_cs(element, {"CONTINUOUS", "PULSED", "PSEUDOCONTINUOUS"})
            field = "ge_asl_technique"
        else:
            _audit_private_is(
                element,
                count=1,
                valid=lambda values: 0 <= values[0] <= 100_000_000,
            )
            field = "ge_asl_duration"
        _record_private_field(semantic_fields, field)
        return creator_tag, "ge_gems_parm_01_asl_technique_duration_v1"
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
    expected_deidentification_method: str,
    state: _AuditState,
    depth: int,
    sop_class: str,
    sequence_path: tuple[BaseTag, ...] = (),
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
        if tag == MR_TRANSMIT_COIL_SEQUENCE and sequence_path not in {
            (SHARED_FUNCTIONAL_GROUPS_SEQUENCE,),
            (PER_FRAME_FUNCTIONAL_GROUPS_SEQUENCE,),
        }:
            raise _PrivacyViolation()
        if tag == SOURCE_IMAGE_SEQUENCE:
            _audit_source_image_sequence(element)
        if tag == REFERENCED_IMAGE_SEQUENCE:
            if sop_class == CLASSIC_MR_IMAGE_STORAGE_UID and not sequence_path:
                _audit_source_image_sequence(element)
            elif sop_class == ENHANCED_MR_IMAGE_STORAGE_UID and sequence_path == (
                SHARED_FUNCTIONAL_GROUPS_SEQUENCE,
            ):
                _audit_referenced_image_sequence(element)
            else:
                raise _PrivacyViolation()
        if tag == MR_METABOLITE_MAP_SEQUENCE:
            if sop_class != ENHANCED_MR_IMAGE_STORAGE_UID or sequence_path != (
                PER_FRAME_FUNCTIONAL_GROUPS_SEQUENCE,
            ):
                raise _PrivacyViolation()
            _audit_mr_metabolite_map_sequence(element)
        purpose_code_allowed = (
            tag == PURPOSE_OF_REFERENCE_CODE_SEQUENCE
            and sop_class == ENHANCED_MR_IMAGE_STORAGE_UID
            and sequence_path
            == (SHARED_FUNCTIONAL_GROUPS_SEQUENCE, REFERENCED_IMAGE_SEQUENCE)
        )
        if tag in UNSUPPORTED_REFERENCE_SEMANTICS and not purpose_code_allowed:
            raise _PrivacyViolation()
        if (
            0x5000 <= tag.group <= 0x501E
            or 0x6000 <= tag.group <= 0x601E
            or tag.group == 0x0070
        ):
            raise _PrivacyViolation()
        if tag in PRIVACY_TYPE_2_EMPTY_ATTRIBUTES:
            expected_vr = PRIVACY_TYPE_2_EMPTY_ATTRIBUTES[tag]
            if (
                depth == 0
                and sop_class in ENHANCED_MR_IMAGE_STORAGE_UIDS
                and tag in {Tag(0x0008, 0x0023), Tag(0x0008, 0x0033)}
            ):
                expected = (
                    ENHANCED_CONTENT_DATE_SENTINEL
                    if tag == Tag(0x0008, 0x0023)
                    else ENHANCED_CONTENT_TIME_SENTINEL
                )
                if element.VR != expected_vr or _text_components(element) != [expected]:
                    raise _PrivacyViolation()
                continue
            if element.VR != expected_vr or not element.is_empty:
                raise _PrivacyViolation()
            continue
        if tag in NUMERIC_TYPE_2_ATTRIBUTES:
            _audit_type_2_numeric_shell(element, NUMERIC_TYPE_2_ATTRIBUTES[tag])
            continue
        if (
            sop_class == CLASSIC_MR_IMAGE_STORAGE_UID
            and tag in CLASSIC_MR_TYPE_2_ATTRIBUTES
        ):
            _audit_classic_mr_type_2(element, CLASSIC_MR_TYPE_2_ATTRIBUTES[tag])
            continue
        if (
            sop_class in ENHANCED_MR_IMAGE_STORAGE_UIDS
            and element.VR == "DT"
            and tag in {Tag(0x0018, 0x9074), Tag(0x0018, 0x9151)}
            and _text_components(element) == [ENHANCED_FRAME_DATETIME_SENTINEL]
        ):
            continue
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
        if sequence_path == (
            SHARED_FUNCTIONAL_GROUPS_SEQUENCE,
            REFERENCED_IMAGE_SEQUENCE,
            PURPOSE_OF_REFERENCE_CODE_SEQUENCE,
        ) and tag in {
            Tag(0x0008, 0x0100),
            Tag(0x0008, 0x0102),
            Tag(0x0008, 0x0104),
            Tag(0x0008, 0x0117),
        }:
            # The enclosing Referenced Image Sequence was validated atomically.
            # Do not reinterpret this fixed standard code through the broader
            # scanner-text allowlist (whose Code Meaning contract is ANATOMY).
            continue
        if sequence_path in {
            (
                SHARED_FUNCTIONAL_GROUPS_SEQUENCE,
                FRAME_ANATOMY_SEQUENCE,
                Tag(0x0008, 0x2218),
            ),
            (
                PER_FRAME_FUNCTIONAL_GROUPS_SEQUENCE,
                FRAME_ANATOMY_SEQUENCE,
                Tag(0x0008, 0x2218),
            ),
        } and tag == Tag(0x0008, 0x0117):
            # The enclosing Frame Anatomy macro proved the exact optional
            # standard context UID before this path-specific bypass.
            if element.VR != "UI" or _text_components(element) != [
                ANATOMY_CONTEXT_UID
            ]:
                raise _PrivacyViolation()
            continue
        if (
            tag == METABOLITE_MAP_DESCRIPTION
            and sop_class == ENHANCED_MR_IMAGE_STORAGE_UID
            and sequence_path
            == (PER_FRAME_FUNCTIONAL_GROUPS_SEQUENCE, MR_METABOLITE_MAP_SEQUENCE)
        ):
            # The enclosing per-frame macro already proved this is exact ST
            # `WATER`; arbitrary spectroscopy description text stays denied.
            continue
        if tag == MULTI_COIL_ELEMENT_NAME and sequence_path in {
            (
                SHARED_FUNCTIONAL_GROUPS_SEQUENCE,
                MR_RECEIVE_COIL_SEQUENCE,
                MULTI_COIL_DEFINITION_SEQUENCE,
            ),
            (
                PER_FRAME_FUNCTIONAL_GROUPS_SEQUENCE,
                MR_RECEIVE_COIL_SEQUENCE,
                MULTI_COIL_DEFINITION_SEQUENCE,
            ),
        }:
            # The enclosing receive-coil macro has already proved the exact
            # two-field element surface. Permit only the fixed non-source label
            # emitted by the workstation sanitizer.
            if element.VR != "SH" or _text_components(element) != ["MULTI_ELEMENT"]:
                raise _PrivacyViolation()
            continue
        if tag in IDENTITY_TAGS or tag in {
            Tag(0x0028, 0x0301),
            Tag(0x0028, 0x0303),
        }:
            _audit_special_text(
                element, expected_subject_id, expected_deidentification_method
            )
            continue
        if tag == Tag(0x0018, 0x9252):
            if element.VR != "LO" or not element.is_empty:
                raise _PrivacyViolation()
            state.asl_technique_descriptions_emptied += 1
            continue
        if tag == Tag(0x0018, 0x925B):
            if element.VR != "LO" or _text_components(element) != [
                ASL_CRUSHER_DESCRIPTION_SENTINEL
            ]:
                raise _PrivacyViolation()
            state.asl_crusher_descriptions_redacted += 1
            continue
        if tag == Tag(0x0018, 0x925E):
            if element.VR != "LO" or not element.is_empty:
                raise _PrivacyViolation()
            state.asl_bolus_cutoff_techniques_emptied += 1
            continue
        if tag in ROOT_RESCALE_TAGS:
            # Atomic root/PVT validation is performed before this recursive
            # allowlist pass. Rescale Type is deliberate bounded LO metadata.
            continue
        if not purpose_code_allowed and not _public_attribute_allowed(tag, element.VR):
            raise _PrivacyViolation()
        if tag in {EXTENDED_OFFSET_TABLE, EXTENDED_OFFSET_TABLE_LENGTHS}:
            if depth != 0:
                raise _PrivacyViolation()
            _extended_offset_values(element)
            continue
        if _audit_special_text(
            element, expected_subject_id, expected_deidentification_method
        ):
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
                _audit_dataset(
                    item,
                    expected_subject_id,
                    expected_deidentification_method,
                    state,
                    depth + 1,
                    sop_class,
                    sequence_path + (tag,),
                )
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
            if tag == Tag(0x0008, 0x0008):
                if depth != 0:
                    raise _PrivacyViolation()
                _audit_positional_image_type(element, sop_class)
            elif tag == Tag(0x0008, 0x9007):
                _audit_positional_frame_type(element, sop_class)
            else:
                values = _text_components(element)
                allowed = CODE_VALUES.get(tag)
                if (
                    allowed is None
                    or any(value not in allowed for value in values)
                    or len(set(values)) != len(values)
                ):
                    raise _PrivacyViolation()
        else:
            if not _audit_geometry_value(element):
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


def _extended_offset_values(element: DataElement) -> tuple[int, ...]:
    if element.VR != "OV" or not isinstance(element.value, (bytes, bytearray)):
        raise _PrivacyViolation()
    raw = bytes(element.value)
    if not raw or len(raw) % 8 or len(raw) // 8 > MAX_DICOM_INSTANCES:
        raise _PrivacyViolation()
    return struct.unpack(f"<{len(raw) // 8}Q", raw)


def _audit_extended_offset_table(
    dataset: Dataset,
    path: Path,
    pixel_offset: int,
    transfer_syntax: UID,
) -> None:
    offset_element = dataset.get(EXTENDED_OFFSET_TABLE)
    length_element = dataset.get(EXTENDED_OFFSET_TABLE_LENGTHS)
    if offset_element is None and length_element is None:
        return
    if not isinstance(offset_element, DataElement) or not isinstance(
        length_element, DataElement
    ):
        raise _PrivacyViolation()
    offsets = _extended_offset_values(offset_element)
    lengths = _extended_offset_values(length_element)
    if (
        len(offsets) != len(lengths)
        or offsets[0] != 0
        or any(left >= right for left, right in zip(offsets, offsets[1:]))
        or any(length == 0 for length in lengths)
        or not transfer_syntax.is_encapsulated
        or not transfer_syntax.is_little_endian
        or transfer_syntax.is_implicit_VR
    ):
        raise _PrivacyViolation()
    frames = _optional_int(dataset, Tag(0x0028, 0x0008))
    if frames is None or frames != len(offsets):
        raise _PrivacyViolation()

    size = path.stat().st_size
    with path.open("rb") as stream:
        stream.seek(pixel_offset)
        if _read_exact(stream, 4) != b"\xe0\x7f\x10\x00":
            raise _PrivacyViolation()
        if (
            _read_exact(stream, 2) != b"OB"
            or _read_exact(stream, 2) != b"\0\0"
            or struct.unpack("<I", _read_exact(stream, 4))[0] != 0xFFFFFFFF
        ):
            raise _PrivacyViolation()
        if _read_exact(stream, 4) != b"\xfe\xff\x00\xe0":
            raise _PrivacyViolation()
        if struct.unpack("<I", _read_exact(stream, 4))[0] != 0:
            raise _PrivacyViolation()
        first_fragment = stream.tell()
        for index, (declared_offset, declared_length) in enumerate(
            zip(offsets, lengths)
        ):
            item_start = stream.tell()
            if item_start - first_fragment != declared_offset:
                raise _PrivacyViolation()
            if _read_exact(stream, 4) != b"\xfe\xff\x00\xe0":
                raise _PrivacyViolation()
            item_length = struct.unpack("<I", _read_exact(stream, 4))[0]
            if (
                item_length < 2
                or item_length % 2
                or declared_length + declared_length % 2 != item_length
                or stream.tell() + item_length > size
            ):
                raise _PrivacyViolation()
            stream.seek(item_length, io.SEEK_CUR)
            if (
                index + 1 < len(offsets)
                and stream.tell() - first_fragment != offsets[index + 1]
            ):
                raise _PrivacyViolation()
        if (
            _read_exact(stream, 4) != b"\xfe\xff\xdd\xe0"
            or struct.unpack("<I", _read_exact(stream, 4))[0] != 0
            or stream.tell() != size
        ):
            raise _PrivacyViolation()


def _audit_pixel_boundary(
    path: Path,
    offset: int,
    transfer_syntax: UID,
    expected_native_value_length: int,
) -> None:
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
            if (
                transfer_syntax.is_compressed
                or length < 2
                or length % 2
                or length != expected_native_value_length
            ):
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


def _exact_numbers(
    dataset: Dataset, tag: BaseTag, *, vr: str, count: int
) -> tuple[float, ...] | None:
    element = dataset.get(tag)
    if not isinstance(element, DataElement) or element.VR != vr:
        return None
    values = _components(element.value)
    if len(values) != count:
        return None
    try:
        parsed = tuple(float(value) for value in values)
    except (TypeError, ValueError, OverflowError):
        return None
    return parsed if all(math.isfinite(value) for value in parsed) else None


def _exact_code(dataset: Dataset, tag: BaseTag, allowed: set[str]) -> str | None:
    element = dataset.get(tag)
    if not isinstance(element, DataElement) or element.VR != "CS":
        return None
    try:
        values = _text_components(element)
    except _PrivacyViolation:
        return None
    return values[0] if len(values) == 1 and values[0] in allowed else None


def _exact_unsigned_integer(dataset: Dataset, tag: BaseTag, vr: str) -> int | None:
    element = dataset.get(tag)
    if not isinstance(element, DataElement) or element.VR != vr:
        return None
    values = _components(element.value)
    if (
        len(values) != 1
        or isinstance(values[0], bool)
        or not isinstance(values[0], Integral)
        or int(values[0]) < 0
    ):
        return None
    return int(values[0])


def _exact_sequence(dataset: Dataset, tag: BaseTag) -> Sequence | None:
    element = dataset.get(tag)
    if (
        not isinstance(element, DataElement)
        or element.VR != "SQ"
        or not isinstance(element.value, Sequence)
        or any(not isinstance(item, Dataset) for item in element.value)
    ):
        return None
    return element.value


def _optional_sequence_contract(
    dataset: Dataset,
    tag: BaseTag,
    validator: Any,
    *,
    minimum_items: int,
    maximum_items: int | None = None,
) -> tuple[str, bool]:
    if tag not in dataset:
        return "absent", False
    items = _exact_sequence(dataset, tag)
    if items is None or len(items) < minimum_items:
        return "invalid", False
    if maximum_items is not None and len(items) > maximum_items:
        return "invalid", False
    results = [validator(item) for item in items]
    if not all(result[0] for result in results):
        return "invalid", False
    return "valid", any(result[1] for result in results)


def _valid_public_diffusion_gradient(item: Dataset) -> tuple[bool, bool]:
    values = _exact_numbers(item, Tag(0x0018, 0x9089), vr="FD", count=3)
    if values is None:
        return False, False
    norm_squared = sum(value * value for value in values)
    valid = all(-1.1 <= value <= 1.1 for value in values) and 0.5 <= norm_squared <= 1.5
    return valid, False


def _valid_public_diffusion_b_matrix(item: Dataset) -> tuple[bool, bool]:
    values = [
        _exact_numbers(item, Tag(0x0018, element), vr="FD", count=1)
        for element in range(0x9602, 0x9608)
    ]
    valid = all(value is not None and -1.0e9 <= value[0] <= 1.0e9 for value in values)
    return valid, False


def _valid_public_diffusion_item(item: Dataset) -> tuple[bool, bool]:
    b_values = _exact_numbers(item, Tag(0x0018, 0x9087), vr="FD", count=1)
    directionality = _exact_code(
        item,
        Tag(0x0018, 0x9075),
        {"NONE", "ISOTROPIC", "DIRECTIONAL", "BMATRIX"},
    )
    if b_values is None or not 0 <= b_values[0] <= 1.0e6 or directionality is None:
        return False, False
    gradient_state, _ = _optional_sequence_contract(
        item,
        Tag(0x0018, 0x9076),
        _valid_public_diffusion_gradient,
        minimum_items=1,
        maximum_items=1,
    )
    matrix_state, _ = _optional_sequence_contract(
        item,
        Tag(0x0018, 0x9601),
        _valid_public_diffusion_b_matrix,
        minimum_items=1,
        maximum_items=1,
    )
    b_value = b_values[0]
    if directionality == "NONE":
        valid = b_value <= 1 and gradient_state == "absent" and matrix_state == "absent"
    elif directionality == "ISOTROPIC":
        valid = b_value > 1 and gradient_state == "absent" and matrix_state == "absent"
    elif directionality == "DIRECTIONAL":
        valid = b_value > 1 and gradient_state == "valid" and matrix_state == "absent"
    else:
        valid = b_value > 1 and matrix_state == "valid" and gradient_state == "absent"
    semantic = valid and (b_value > 1 or directionality in {"DIRECTIONAL", "BMATRIX"})
    return valid, semantic


def _direct_numeric_state(
    dataset: Dataset,
    tag: BaseTag,
    *,
    vr: str,
    count: int,
    validator: Any,
) -> tuple[str, tuple[float, ...] | None]:
    if tag not in dataset:
        return "absent", None
    values = _exact_numbers(dataset, tag, vr=vr, count=count)
    if values is None or not validator(values):
        return "invalid", None
    return "valid", values


def _exclusive_state(left: str, right: str) -> str:
    if left == "absent" and right == "absent":
        return "absent"
    if {left, right} == {"absent", "valid"}:
        return "valid"
    return "invalid"


def _direct_b_matrix_state(dataset: Dataset) -> str:
    states = [
        _direct_numeric_state(
            dataset,
            Tag(0x0018, element),
            vr="FD",
            count=1,
            validator=lambda values: -1.0e9 <= values[0] <= 1.0e9,
        )[0]
        for element in range(0x9602, 0x9608)
    ]
    if all(state == "absent" for state in states):
        return "absent"
    if all(state == "valid" for state in states):
        return "valid"
    return "invalid"


def _classic_public_diffusion_contract(dataset: Dataset) -> tuple[bool, bool]:
    if Tag(0x0018, 0x9117) in dataset:
        loose_tags = {
            Tag(0x0018, 0x9087),
            Tag(0x0018, 0x9075),
            Tag(0x0018, 0x9076),
            Tag(0x0018, 0x9089),
            Tag(0x0018, 0x9601),
            *(Tag(0x0018, element) for element in range(0x9602, 0x9608)),
        }
        if any(tag in dataset for tag in loose_tags):
            return False, False
        state, semantic = _optional_sequence_contract(
            dataset,
            Tag(0x0018, 0x9117),
            _valid_public_diffusion_item,
            minimum_items=1,
            maximum_items=1,
        )
        return state == "valid", semantic

    b_state, b_values = _direct_numeric_state(
        dataset,
        Tag(0x0018, 0x9087),
        vr="FD",
        count=1,
        validator=lambda values: 0 <= values[0] <= 1.0e6,
    )
    directionality = _exact_code(
        dataset,
        Tag(0x0018, 0x9075),
        {"NONE", "ISOTROPIC", "DIRECTIONAL", "BMATRIX"},
    )
    if b_state != "valid" or directionality is None or b_values is None:
        return False, False

    direct_gradient = _direct_numeric_state(
        dataset,
        Tag(0x0018, 0x9089),
        vr="FD",
        count=3,
        validator=lambda values: (
            all(-1.1 <= value <= 1.1 for value in values)
            and 0.5 <= sum(value * value for value in values) <= 1.5
        ),
    )[0]
    sequence_gradient = _optional_sequence_contract(
        dataset,
        Tag(0x0018, 0x9076),
        _valid_public_diffusion_gradient,
        minimum_items=1,
        maximum_items=1,
    )[0]
    gradient = _exclusive_state(direct_gradient, sequence_gradient)

    direct_matrix = _direct_b_matrix_state(dataset)
    sequence_matrix = _optional_sequence_contract(
        dataset,
        Tag(0x0018, 0x9601),
        _valid_public_diffusion_b_matrix,
        minimum_items=1,
        maximum_items=1,
    )[0]
    matrix = _exclusive_state(direct_matrix, sequence_matrix)
    b_value = b_values[0]
    if directionality == "NONE":
        valid = b_value <= 1 and gradient == "absent" and matrix == "absent"
    elif directionality == "ISOTROPIC":
        valid = b_value > 1 and gradient == "absent" and matrix == "absent"
    elif directionality == "DIRECTIONAL":
        valid = b_value > 1 and gradient == "valid" and matrix == "absent"
    else:
        valid = b_value > 1 and matrix == "valid" and gradient == "absent"
    semantic = valid and (
        b_value > 1 or directionality in {"ISOTROPIC", "DIRECTIONAL", "BMATRIX"}
    )
    return valid, semantic


def _direct_frame_origin_state(
    functional_group_item: Dataset, sop_class: str
) -> tuple[str, str | None]:
    tag = Tag(0x0018, 0x9226)
    if tag not in functional_group_item:
        return "absent", None
    items = _exact_sequence(functional_group_item, tag)
    if items is None or len(items) != 1:
        return "invalid", None
    element = items[0].get(Tag(0x0008, 0x9007))
    if not isinstance(element, DataElement) or element.VR != "CS":
        return "invalid", None
    try:
        values = _text_components(element, allow_empty=True)
        _audit_positional_enhanced_type_values(
            values,
            root=False,
            legacy=sop_class == LEGACY_CONVERTED_ENHANCED_MR_IMAGE_STORAGE_UID,
        )
    except _PrivacyViolation:
        return "invalid", None
    return "valid", values[0]


def _enhanced_frame_origins(
    dataset: Dataset, sop_class: str
) -> tuple[list[str], Dataset | None, Sequence] | None:
    frame_count = _optional_int(dataset, Tag(0x0028, 0x0008))
    if frame_count is None or frame_count <= 0:
        return None

    shared_items: Sequence = Sequence()
    if Tag(0x5200, 0x9229) in dataset:
        parsed = _exact_sequence(dataset, Tag(0x5200, 0x9229))
        if parsed is None or len(parsed) != 1:
            return None
        shared_items = parsed

    per_frame_items: Sequence = Sequence()
    if Tag(0x5200, 0x9230) in dataset:
        parsed = _exact_sequence(dataset, Tag(0x5200, 0x9230))
        if parsed is None or len(parsed) != frame_count:
            return None
        per_frame_items = parsed

    shared_item = shared_items[0] if shared_items else None
    shared_origin_state, shared_origin = (
        _direct_frame_origin_state(shared_item, sop_class)
        if shared_item is not None
        else ("absent", None)
    )
    if shared_origin_state == "invalid":
        return None
    if shared_origin_state == "valid":
        if any(
            _direct_frame_origin_state(item, sop_class)[0] != "absent"
            for item in per_frame_items
        ):
            return None
        origins = [str(shared_origin)] * frame_count
    else:
        if not per_frame_items:
            return None
        states = [
            _direct_frame_origin_state(item, sop_class) for item in per_frame_items
        ]
        if any(state != "valid" or origin is None for state, origin in states):
            return None
        origins = [str(origin) for _, origin in states]

    root = dataset.get(Tag(0x0008, 0x0008))
    if not isinstance(root, DataElement) or root.VR != "CS":
        return None
    try:
        root_values = _text_components(root, allow_empty=True)
        _audit_positional_enhanced_type_values(
            root_values,
            root=True,
            legacy=sop_class == LEGACY_CONVERTED_ENHANCED_MR_IMAGE_STORAGE_UID,
        )
    except _PrivacyViolation:
        return None
    has_original = "ORIGINAL" in origins
    has_derived = "DERIVED" in origins
    summary_matches = (
        root_values[0] == "ORIGINAL"
        and has_original
        and not has_derived
        or root_values[0] == "DERIVED"
        and has_derived
        and not has_original
        or root_values[0] == "MIXED"
        and has_original
        and has_derived
    )
    if not summary_matches:
        return None
    return origins, shared_item, per_frame_items


def _enhanced_public_diffusion_contract(
    dataset: Dataset, sop_class: str
) -> tuple[bool, bool]:
    frame_contract = _enhanced_frame_origins(dataset, sop_class)
    if frame_contract is None:
        return False, False
    origins, shared_item, per_frame_items = frame_contract
    if shared_item is None:
        shared_state, shared_semantic = "absent", False
    else:
        shared_state, shared_semantic = _optional_sequence_contract(
            shared_item,
            Tag(0x0018, 0x9117),
            _valid_public_diffusion_item,
            minimum_items=1,
            maximum_items=1,
        )
    if shared_state == "invalid":
        return False, False
    if shared_state == "valid":
        valid = all(origin == "ORIGINAL" for origin in origins) and all(
            _optional_sequence_contract(
                item,
                Tag(0x0018, 0x9117),
                _valid_public_diffusion_item,
                minimum_items=1,
                maximum_items=1,
            )[0]
            == "absent"
            for item in per_frame_items
        )
        return valid, shared_semantic if valid else False
    if len(per_frame_items) != len(origins):
        return False, False
    results = [
        _optional_sequence_contract(
            item,
            Tag(0x0018, 0x9117),
            _valid_public_diffusion_item,
            minimum_items=1,
            maximum_items=1,
        )
        for item in per_frame_items
    ]
    valid = all(
        state == ("valid" if origin == "ORIGINAL" else "absent")
        for origin, (state, _) in zip(origins, results)
    )
    return valid, valid and any(semantic for _, semantic in results)


def _valid_public_asl_slab(item: Dataset) -> tuple[bool, bool]:
    slab_number = _exact_unsigned_integer(item, Tag(0x0018, 0x9253), "US")
    thickness = _exact_numbers(item, Tag(0x0018, 0x9254), vr="FD", count=1)
    orientation = _exact_numbers(item, Tag(0x0018, 0x9255), vr="FD", count=3)
    position = _exact_numbers(item, Tag(0x0018, 0x9256), vr="FD", count=3)
    pulse_duration = _exact_unsigned_integer(item, Tag(0x0018, 0x9258), "UL")
    orientation_norm = (
        sum(value * value for value in orientation) if orientation is not None else 0.0
    )
    valid = (
        slab_number is not None
        and 1 <= slab_number <= 4096
        and thickness is not None
        and 0 < thickness[0] <= 1.0e6
        and orientation is not None
        and all(-1.1 <= value <= 1.1 for value in orientation)
        and 0.5 <= orientation_norm <= 1.5
        and position is not None
        and all(abs(value) <= 1.0e6 for value in position)
        and pulse_duration is not None
        and pulse_duration <= 100_000_000
    )
    return valid, False


def _valid_public_asl_item(item: Dataset) -> tuple[bool, bool]:
    context = _exact_code(
        item, Tag(0x0018, 0x9257), {"LABEL", "CONTROL", "M_ZERO_SCAN"}
    )
    crusher = _exact_code(item, Tag(0x0018, 0x9259), {"YES", "NO"})
    bolus = _exact_code(item, Tag(0x0018, 0x925C), {"YES", "NO"})
    technique_description = item.get(Tag(0x0018, 0x9252))
    if (
        context is None
        or crusher is None
        or bolus is None
        or not isinstance(technique_description, DataElement)
        or technique_description.VR != "LO"
        or not technique_description.is_empty
    ):
        return False, False

    crusher_flow = _exact_numbers(item, Tag(0x0018, 0x925A), vr="FD", count=1)
    crusher_description = item.get(Tag(0x0018, 0x925B))
    if crusher == "YES":
        crusher_valid = bool(
            crusher_flow is not None
            and 0 <= crusher_flow[0] <= 1.0e6
            and isinstance(crusher_description, DataElement)
            and crusher_description.VR == "LO"
            and _text_components(crusher_description)
            == [ASL_CRUSHER_DESCRIPTION_SENTINEL]
        )
    else:
        crusher_valid = (
            Tag(0x0018, 0x925A) not in item and Tag(0x0018, 0x925B) not in item
        )
    if not crusher_valid:
        return False, False

    bolus_timing = item.get(Tag(0x0018, 0x925D))
    if bolus == "YES":
        if (
            not isinstance(bolus_timing, DataElement)
            or bolus_timing.VR != "SQ"
            or not isinstance(bolus_timing.value, Sequence)
            or len(bolus_timing.value) != 1
        ):
            return False, False
        timing_item = bolus_timing.value[0]
        if not isinstance(timing_item, Dataset) or set(timing_item.keys()) != {
            Tag(0x0018, 0x925E),
            Tag(0x0018, 0x925F),
        }:
            return False, False
        technique = timing_item.get(Tag(0x0018, 0x925E))
        delay = _exact_unsigned_integer(timing_item, Tag(0x0018, 0x925F), "UL")
        bolus_valid = bool(
            isinstance(technique, DataElement)
            and technique.VR == "LO"
            and technique.is_empty
            and delay is not None
            and delay <= 100_000_000
        )
    else:
        bolus_valid = Tag(0x0018, 0x925D) not in item
    if not bolus_valid:
        return False, False

    if context == "M_ZERO_SCAN":
        return True, True
    slabs = _exact_sequence(item, Tag(0x0018, 0x9260))
    valid = bool(slabs) and all(_valid_public_asl_slab(slab)[0] for slab in slabs or ())
    return valid, valid


def _functional_group_contract(
    dataset: Dataset,
    macro_tag: BaseTag,
    validator: Any,
    *,
    macro_minimum_items: int,
    macro_maximum_items: int | None,
) -> tuple[bool, bool]:
    shared_state = "absent"
    shared_semantic = False
    if Tag(0x5200, 0x9229) in dataset:
        shared_items = _exact_sequence(dataset, Tag(0x5200, 0x9229))
        if shared_items is None or len(shared_items) != 1:
            return False, False
        shared_state, shared_semantic = _optional_sequence_contract(
            shared_items[0],
            macro_tag,
            validator,
            minimum_items=macro_minimum_items,
            maximum_items=macro_maximum_items,
        )
        if shared_state == "invalid":
            return False, False

    per_frame_items: Sequence | None = None
    if Tag(0x5200, 0x9230) in dataset:
        per_frame_items = _exact_sequence(dataset, Tag(0x5200, 0x9230))
        frame_count = _optional_int(dataset, Tag(0x0028, 0x0008))
        if (
            per_frame_items is None
            or frame_count is None
            or frame_count <= 0
            or len(per_frame_items) != frame_count
        ):
            return False, False

    if shared_state == "valid":
        if per_frame_items is None:
            return True, shared_semantic
        states = [
            _optional_sequence_contract(
                item,
                macro_tag,
                validator,
                minimum_items=macro_minimum_items,
                maximum_items=macro_maximum_items,
            )[0]
            for item in per_frame_items
        ]
        return all(state == "absent" for state in states), shared_semantic

    if per_frame_items is None or not per_frame_items:
        return False, False
    results = [
        _optional_sequence_contract(
            item,
            macro_tag,
            validator,
            minimum_items=macro_minimum_items,
            maximum_items=macro_maximum_items,
        )
        for item in per_frame_items
    ]
    return all(state == "valid" for state, _ in results), any(
        semantic for _, semantic in results
    )


def _public_diffusion_contract(dataset: Dataset, sop_class: str) -> _ScientificContract:
    relevant_tags = {
        Tag(0x0018, 0x9117),
        Tag(0x0018, 0x9087),
        Tag(0x0018, 0x9075),
        Tag(0x0018, 0x9076),
        Tag(0x0018, 0x9089),
        Tag(0x0018, 0x9601),
        *(Tag(0x0018, element) for element in range(0x9602, 0x9608)),
    }
    direct_root_present = any(tag in dataset for tag in relevant_tags)
    recursive_present = any(_recursive_elements(dataset, tag) for tag in relevant_tags)
    if sop_class == CLASSIC_MR_IMAGE_STORAGE_UID:
        if not direct_root_present:
            return _ScientificContract(False, False)
        valid, semantic = _classic_public_diffusion_contract(dataset)
        return _ScientificContract(True, valid, semantic)
    if sop_class in ENHANCED_MR_IMAGE_STORAGE_UIDS:
        # The Enhanced MR diffusion contract is a Functional Groups contract;
        # reviewed private values cannot substitute for an omitted mandatory
        # public frame macro.
        valid, semantic = _enhanced_public_diffusion_contract(dataset, sop_class)
        return _ScientificContract(True, valid, semantic)
    if not recursive_present:
        return _ScientificContract(False, False)
    valid, semantic = False, False
    return _ScientificContract(True, valid, semantic)


def _public_asl_contract(dataset: Dataset, sop_class: str) -> _ScientificContract:
    technique = _exact_code(
        dataset,
        Tag(0x0018, 0x9250),
        {"CONTINUOUS", "PULSED", "PSEUDOCONTINUOUS"},
    )
    recursive_macro_present = bool(
        _recursive_elements(dataset, Tag(0x0018, 0x9251))
        or _recursive_elements(dataset, Tag(0x0018, 0x9257))
    )
    direct_macro_present = (
        Tag(0x0018, 0x9251) in dataset or Tag(0x0018, 0x9257) in dataset
    )
    if sop_class == CLASSIC_MR_IMAGE_STORAGE_UID:
        if not direct_macro_present:
            return _ScientificContract(False, False)
        state, semantic = _optional_sequence_contract(
            dataset,
            Tag(0x0018, 0x9251),
            _valid_public_asl_item,
            minimum_items=1,
            maximum_items=None,
        )
        valid = technique is not None and state == "valid"
        return _ScientificContract(True, valid, valid and semantic)
    if sop_class not in ENHANCED_MR_IMAGE_STORAGE_UIDS:
        return _ScientificContract(recursive_macro_present, False)
    present = technique is not None or recursive_macro_present
    if not present:
        return _ScientificContract(False, False)
    valid, semantic = _functional_group_contract(
        dataset,
        Tag(0x0018, 0x9251),
        _valid_public_asl_item,
        macro_minimum_items=1,
        macro_maximum_items=None,
    )
    valid = technique is not None and valid
    return _ScientificContract(True, valid, valid and semantic)


def _root_private_creators(dataset: Dataset) -> dict[BaseTag, str]:
    creators: dict[BaseTag, str] = {}
    for element in dataset:
        if element.tag.group % 2 == 1 and 0x0010 <= element.tag.element <= 0x00FF:
            values = _text_components(element)
            if len(values) == 1:
                creators[element.tag] = values[0]
    return creators


def _root_private_element(
    dataset: Dataset,
    creators: dict[BaseTag, str],
    *,
    group: int,
    low: int,
    creator: str,
) -> DataElement | None:
    matches = [
        element
        for element in dataset
        if element.tag.group == group
        and element.tag.element & 0x00FF == low
        and creators.get(_private_creator_tag(element.tag)) == creator
    ]
    return matches[0] if len(matches) == 1 else None


def _private_numbers(
    dataset: Dataset,
    creators: dict[BaseTag, str],
    *,
    group: int,
    low: int,
    creator: str,
    vr: str,
    count: int,
) -> tuple[float, ...] | None:
    element = _root_private_element(
        dataset, creators, group=group, low=low, creator=creator
    )
    if element is None or element.VR != vr:
        return None
    values = _components(element.value)
    if len(values) != count:
        return None
    try:
        parsed = tuple(float(value) for value in values)
    except (TypeError, ValueError, OverflowError):
        return None
    return parsed if all(math.isfinite(value) for value in parsed) else None


def _private_code(
    dataset: Dataset,
    creators: dict[BaseTag, str],
    *,
    group: int,
    low: int,
    creator: str,
    allowed: set[str],
) -> str | None:
    element = _root_private_element(
        dataset, creators, group=group, low=low, creator=creator
    )
    if element is None:
        return None
    try:
        return _audit_private_cs(element, allowed)
    except _PrivacyViolation:
        return None


def _gradient_valid(values: tuple[float, ...] | None) -> bool:
    return (
        values is not None
        and len(values) == 3
        and all(-1.1 <= value <= 1.1 for value in values)
        and 0.5 <= sum(value * value for value in values) <= 1.5
    )


def _numeric_values_zero(values: tuple[float, ...] | None) -> bool:
    return values is not None and all(abs(value) <= 1.0e-6 for value in values)


def _diffusion_values_from_public_item(dataset: Dataset) -> _DiffusionValues:
    b_value = _exact_numbers(dataset, Tag(0x0018, 0x9087), vr="FD", count=1)
    gradient = _exact_numbers(dataset, Tag(0x0018, 0x9089), vr="FD", count=3)
    if gradient is None:
        gradient_items = _exact_sequence(dataset, Tag(0x0018, 0x9076))
        if gradient_items is not None and len(gradient_items) == 1:
            gradient = _exact_numbers(
                gradient_items[0], Tag(0x0018, 0x9089), vr="FD", count=3
            )

    matrix_parts = [
        _exact_numbers(dataset, Tag(0x0018, element), vr="FD", count=1)
        for element in range(0x9602, 0x9608)
    ]
    b_matrix = (
        tuple(value[0] for value in matrix_parts if value is not None)
        if all(value is not None for value in matrix_parts)
        else None
    )
    if b_matrix is None:
        matrix_items = _exact_sequence(dataset, Tag(0x0018, 0x9601))
        if matrix_items is not None and len(matrix_items) == 1:
            matrix_parts = [
                _exact_numbers(matrix_items[0], Tag(0x0018, element), vr="FD", count=1)
                for element in range(0x9602, 0x9608)
            ]
            if all(value is not None for value in matrix_parts):
                b_matrix = tuple(
                    value[0] for value in matrix_parts if value is not None
                )
    return _DiffusionValues(b_value, gradient, b_matrix)


def _public_diffusion_value_sets(dataset: Dataset) -> list[_DiffusionValues]:
    values: list[_DiffusionValues] = []

    def visit(current: Dataset, depth: int) -> None:
        if depth > MAX_SEQUENCE_DEPTH:
            raise _PrivacyViolation()
        for element in current:
            if element.VR != "SQ":
                continue
            if not isinstance(element.value, Sequence):
                raise _PrivacyViolation()
            if element.tag == Tag(0x0018, 0x9117):
                values.extend(
                    _diffusion_values_from_public_item(item)
                    for item in element.value
                    if isinstance(item, Dataset)
                )
                continue
            for item in element.value:
                if not isinstance(item, Dataset):
                    raise _PrivacyViolation()
                visit(item, depth + 1)

    visit(dataset, 0)
    if not values and any(
        tag in dataset
        for tag in (
            Tag(0x0018, 0x9087),
            Tag(0x0018, 0x9089),
            Tag(0x0018, 0x9601),
            *(Tag(0x0018, element) for element in range(0x9602, 0x9608)),
        )
    ):
        values.append(_diffusion_values_from_public_item(dataset))
    return values


def _private_diffusion_value_sources(
    dataset: Dataset,
) -> dict[str, _DiffusionValues]:
    creators = _root_private_creators(dataset)
    sources: dict[str, _DiffusionValues] = {}

    siemens = _DiffusionValues(
        _private_numbers(
            dataset,
            creators,
            group=0x0019,
            low=0x0C,
            creator=SIEMENS_MR_HEADER_CREATOR,
            vr="IS",
            count=1,
        ),
        _private_numbers(
            dataset,
            creators,
            group=0x0019,
            low=0x0E,
            creator=SIEMENS_MR_HEADER_CREATOR,
            vr="FD",
            count=3,
        ),
        _private_numbers(
            dataset,
            creators,
            group=0x0019,
            low=0x27,
            creator=SIEMENS_MR_HEADER_CREATOR,
            vr="FD",
            count=6,
        ),
    )
    if any(value is not None for value in vars(siemens).values()):
        sources["siemens_mr_header"] = siemens

    csa_element = dataset.get(SIEMENS_CSA_DATA_TAG)
    if isinstance(csa_element, DataElement) and csa_element.VR == "OB":
        fields = _validate_canonical_siemens_csa(csa_element.value)
        csa = _DiffusionValues(
            fields.get("B_value"),
            fields.get("DiffusionGradientDirection"),
            fields.get("B_matrix"),
        )
        if any(value is not None for value in vars(csa).values()):
            sources["siemens_csa"] = csa

    philips_parts = tuple(
        _private_numbers(
            dataset,
            creators,
            group=0x2005,
            low=low,
            creator=PHILIPS_MR_CREATOR,
            vr="FL",
            count=1,
        )
        for low in (0xB0, 0xB1, 0xB2)
    )
    philips = _DiffusionValues(
        _private_numbers(
            dataset,
            creators,
            group=0x2001,
            low=0x03,
            creator=PHILIPS_IMAGING_CREATOR,
            vr="FL",
            count=1,
        ),
        (
            tuple(value[0] for value in philips_parts if value is not None)
            if all(value is not None for value in philips_parts)
            else None
        ),
    )
    if any(value is not None for value in vars(philips).values()):
        sources["philips"] = philips

    ge_parts = tuple(
        _private_numbers(
            dataset,
            creators,
            group=0x0019,
            low=low,
            creator=GE_ACQU_CREATOR,
            vr="DS",
            count=1,
        )
        for low in (0xBB, 0xBC, 0xBD)
    )
    ge_b_values = _private_numbers(
        dataset,
        creators,
        group=0x0043,
        low=0x39,
        creator=GE_PARM_CREATOR,
        vr="IS",
        count=4,
    )
    ge = _DiffusionValues(
        (ge_b_values[0],) if ge_b_values is not None else None,
        (
            tuple(value[0] for value in ge_parts if value is not None)
            if all(value is not None for value in ge_parts)
            else None
        ),
    )
    if any(value is not None for value in vars(ge).values()):
        sources["ge"] = ge

    uih = _DiffusionValues(
        _private_numbers(
            dataset,
            creators,
            group=0x0065,
            low=0x09,
            creator=UIH_IMAGE_HEADER_CREATOR,
            vr="FD",
            count=1,
        ),
        _private_numbers(
            dataset,
            creators,
            group=0x0065,
            low=0x37,
            creator=UIH_IMAGE_HEADER_CREATOR,
            vr="FD",
            count=3,
        ),
    )
    if any(value is not None for value in vars(uih).values()):
        sources["uih"] = uih
    return sources


def _numeric_tuples_match(left: tuple[float, ...], right: tuple[float, ...]) -> bool:
    return len(left) == len(right) and all(
        math.isclose(a, b, rel_tol=1.0e-6, abs_tol=1.0e-6) for a, b in zip(left, right)
    )


def _diffusion_values_consistent(
    left: _DiffusionValues, right: _DiffusionValues
) -> bool:
    return all(
        left_value is None
        or right_value is None
        or _numeric_tuples_match(left_value, right_value)
        for left_value, right_value in (
            (left.b_value, right.b_value),
            (left.gradient, right.gradient),
            (left.b_matrix, right.b_matrix),
        )
    )


def _diffusion_sources_consistent(dataset: Dataset) -> bool:
    private_sources = _private_diffusion_value_sources(dataset)
    siemens_header = private_sources.get("siemens_mr_header")
    siemens_csa = private_sources.get("siemens_csa")
    if (
        siemens_header is not None
        and siemens_csa is not None
        and not _diffusion_values_consistent(siemens_header, siemens_csa)
    ):
        return False
    return all(
        _diffusion_values_consistent(public, private)
        for public in _public_diffusion_value_sets(dataset)
        for private in private_sources.values()
    )


def _private_diffusion_contract(dataset: Dataset) -> _ScientificContract:
    creators = _root_private_creators(dataset)

    siemens_b = _private_numbers(
        dataset,
        creators,
        group=0x0019,
        low=0x0C,
        creator=SIEMENS_MR_HEADER_CREATOR,
        vr="IS",
        count=1,
    )
    siemens_direction = _private_code(
        dataset,
        creators,
        group=0x0019,
        low=0x0D,
        creator=SIEMENS_MR_HEADER_CREATOR,
        allowed={"NONE", "ISOTROPIC", "DIRECTIONAL", "BMATRIX"},
    )
    siemens_gradient = _private_numbers(
        dataset,
        creators,
        group=0x0019,
        low=0x0E,
        creator=SIEMENS_MR_HEADER_CREATOR,
        vr="FD",
        count=3,
    )
    siemens_matrix = _private_numbers(
        dataset,
        creators,
        group=0x0019,
        low=0x27,
        creator=SIEMENS_MR_HEADER_CREATOR,
        vr="FD",
        count=6,
    )
    siemens_present = any(
        value is not None
        for value in (siemens_b, siemens_direction, siemens_gradient, siemens_matrix)
    )
    siemens_b_value = siemens_b[0] if siemens_b is not None else None
    siemens_tag_valid = bool(
        siemens_b_value is not None
        and 0 <= siemens_b_value <= 1.0e6
        and (
            siemens_b_value <= 1
            and siemens_direction in {None, "NONE"}
            and (siemens_gradient is None or _numeric_values_zero(siemens_gradient))
            and (siemens_matrix is None or _numeric_values_zero(siemens_matrix))
            or siemens_b_value > 1
            and siemens_direction == "DIRECTIONAL"
            and _gradient_valid(siemens_gradient)
            and (siemens_matrix is None or _numeric_values_zero(siemens_matrix))
            or siemens_b_value > 1
            and siemens_direction == "BMATRIX"
            and siemens_matrix is not None
            and all(-1.0e9 <= value <= 1.0e9 for value in siemens_matrix)
            and (siemens_gradient is None or _numeric_values_zero(siemens_gradient))
            or siemens_b_value > 1
            and siemens_direction == "ISOTROPIC"
            and siemens_gradient is None
            and siemens_matrix is None
        )
    )
    siemens_tag_semantic = bool(
        siemens_b_value is not None
        and siemens_b_value > 1
        or siemens_direction in {"ISOTROPIC", "DIRECTIONAL", "BMATRIX"}
    )

    csa_present = False
    csa_valid = False
    csa_semantic = False
    csa_element = dataset.get(SIEMENS_CSA_DATA_TAG)
    if isinstance(csa_element, DataElement) and csa_element.VR == "OB":
        fields = _validate_canonical_siemens_csa(csa_element.value)
        csa_present = any(
            name in fields
            for name in ("B_value", "DiffusionGradientDirection", "B_matrix")
        )
        if csa_present:
            b_values = fields.get("B_value")
            b_value = (
                b_values[0] if b_values is not None and len(b_values) == 1 else None
            )
            gradient = fields.get("DiffusionGradientDirection")
            gradient_present = gradient is not None
            matrix = fields.get("B_matrix")
            matrix_present = matrix is not None
            csa_valid = bool(
                b_value is not None
                and 0 <= b_value <= 1.0e6
                and (
                    b_value <= 1
                    and (not gradient_present or _numeric_values_zero(gradient))
                    and (not matrix_present or _numeric_values_zero(matrix))
                    or b_value > 1
                    and gradient_present
                    and _gradient_valid(gradient)
                    and not matrix_present
                    or b_value > 1
                    and matrix_present
                    and matrix is not None
                    and len(matrix) == 6
                    and all(-1.0e9 <= value <= 1.0e9 for value in matrix)
                    and not gradient_present
                )
            )
            csa_semantic = bool(
                b_value is not None
                and b_value > 1
                or gradient_present
                or matrix_present
            )
    siemens_contract_present = siemens_present or csa_present
    siemens_valid = bool(
        siemens_contract_present
        and (not siemens_present or siemens_tag_valid)
        and (not csa_present or csa_valid)
        and (
            not (siemens_present and csa_present)
            or siemens_tag_semantic == csa_semantic
        )
    )
    siemens_semantic = siemens_tag_semantic or csa_semantic

    philips_b = _private_numbers(
        dataset,
        creators,
        group=0x2001,
        low=0x03,
        creator=PHILIPS_IMAGING_CREATOR,
        vr="FL",
        count=1,
    )
    philips_direction = _private_code(
        dataset,
        creators,
        group=0x2001,
        low=0x04,
        creator=PHILIPS_IMAGING_CREATOR,
        allowed={"AP", "FH", "RL", "NONE", "ISOTROPIC", "DIRECTIONAL"},
    )
    philips_gradient_parts = tuple(
        _private_numbers(
            dataset,
            creators,
            group=0x2005,
            low=low,
            creator=PHILIPS_MR_CREATOR,
            vr="FL",
            count=1,
        )
        for low in (0xB0, 0xB1, 0xB2)
    )
    philips_present = (
        philips_b is not None
        or philips_direction is not None
        or any(part is not None for part in philips_gradient_parts)
    )
    philips_b_value = philips_b[0] if philips_b is not None else None
    philips_gradient = (
        tuple(part[0] for part in philips_gradient_parts if part is not None)
        if all(part is not None for part in philips_gradient_parts)
        else None
    )
    philips_valid = bool(
        philips_b_value is not None
        and 0 <= philips_b_value <= 1.0e6
        and (
            philips_b_value <= 1
            and philips_direction in {None, "NONE"}
            and (philips_gradient is None or _numeric_values_zero(philips_gradient))
            or philips_b_value > 1
            and philips_direction == "ISOTROPIC"
            and philips_gradient is None
            or philips_b_value > 1
            and philips_direction in {"AP", "FH", "RL", "DIRECTIONAL"}
            and _gradient_valid(philips_gradient)
        )
    )
    philips_semantic = bool(
        philips_b_value is not None
        and philips_b_value > 1
        or philips_direction is not None
        and philips_direction != "NONE"
    )

    ge_b_values = _private_numbers(
        dataset,
        creators,
        group=0x0043,
        low=0x39,
        creator=GE_PARM_CREATOR,
        vr="IS",
        count=4,
    )
    ge_parts = tuple(
        _private_numbers(
            dataset,
            creators,
            group=0x0019,
            low=low,
            creator=GE_ACQU_CREATOR,
            vr="DS",
            count=1,
        )
        for low in (0xBB, 0xBC, 0xBD)
    )
    ge_present = ge_b_values is not None or any(part is not None for part in ge_parts)
    ge_b_value = ge_b_values[0] if ge_b_values is not None else None
    ge_gradient = (
        tuple(part[0] for part in ge_parts if part is not None)
        if all(part is not None for part in ge_parts)
        else None
    )
    ge_valid = bool(
        ge_b_value is not None
        and 0 <= ge_b_value <= 1.0e6
        and ge_b_values is not None
        and all(-1.0e9 <= value <= 1.0e9 for value in ge_b_values[1:])
        and (
            ge_b_value <= 1
            and (ge_gradient is None or _numeric_values_zero(ge_gradient))
            or ge_b_value > 1
            and _gradient_valid(ge_gradient)
        )
    )
    ge_semantic = bool(ge_b_value is not None and ge_b_value > 1)

    uih_b_values = _private_numbers(
        dataset,
        creators,
        group=0x0065,
        low=0x09,
        creator=UIH_IMAGE_HEADER_CREATOR,
        vr="FD",
        count=1,
    )
    uih_gradient = _private_numbers(
        dataset,
        creators,
        group=0x0065,
        low=0x37,
        creator=UIH_IMAGE_HEADER_CREATOR,
        vr="FD",
        count=3,
    )
    uih_present = uih_b_values is not None or uih_gradient is not None
    uih_b_value = uih_b_values[0] if uih_b_values is not None else None
    uih_valid = bool(
        uih_b_value is not None
        and 0 <= uih_b_value <= 1.0e6
        and (
            uih_b_value <= 1
            and (uih_gradient is None or _numeric_values_zero(uih_gradient))
            or uih_b_value > 1
            and _gradient_valid(uih_gradient)
        )
    )
    uih_semantic = bool(uih_b_value is not None and uih_b_value > 1)

    present_contracts = [
        siemens_contract_present,
        philips_present,
        ge_present,
        uih_present,
    ]
    if not any(present_contracts):
        return _ScientificContract(False, False)
    return _ScientificContract(
        True,
        sum(present_contracts) == 1
        and (siemens_valid or philips_valid or ge_valid or uih_valid),
        siemens_semantic or philips_semantic or ge_semantic or uih_semantic,
    )


def _public_asl_contexts(dataset: Dataset) -> frozenset[str]:
    contexts: set[str] = set()
    for element in _recursive_elements(dataset, Tag(0x0018, 0x9257)):
        if element.VR != "CS":
            raise _PrivacyViolation()
        values = _text_components(element)
        if len(values) != 1 or values[0] not in {"LABEL", "CONTROL", "M_ZERO_SCAN"}:
            raise _PrivacyViolation()
        contexts.add(values[0])
    return frozenset(contexts)


def _private_asl_contract(dataset: Dataset) -> _ScientificContract:
    creators = _root_private_creators(dataset)
    label = _private_code(
        dataset,
        creators,
        group=0x2005,
        low=0x29,
        creator=PHILIPS_PER_FRAME_CREATOR,
        allowed={"LABEL", "CONTROL", "M_ZERO_SCAN"},
    )
    philips_present = label is not None
    if not philips_present:
        return _ScientificContract(False, False)
    technique = _exact_code(
        dataset,
        Tag(0x0018, 0x9250),
        {"CONTINUOUS", "PULSED", "PSEUDOCONTINUOUS"},
    )
    timing = _exact_numbers(dataset, Tag(0x0018, 0x0082), vr="DS", count=1)
    if timing is None:
        timing = _exact_numbers(dataset, Tag(0x0018, 0x1060), vr="DS", count=1)
    contexts = _public_asl_contexts(dataset)
    philips_valid = (
        philips_present
        and technique is not None
        and timing is not None
        and 0 <= timing[0] <= 100_000_000
        and (not contexts or contexts == {label})
    )
    return _ScientificContract(True, philips_valid, philips_valid)


def _ge_asl_supplemental_contract(dataset: Dataset) -> tuple[bool, bool]:
    creators = _root_private_creators(dataset)
    ge_technique = _private_code(
        dataset,
        creators,
        group=0x0043,
        low=0xA3,
        creator=GE_PARM_CREATOR,
        allowed={"CONTINUOUS", "PULSED", "PSEUDOCONTINUOUS"},
    )
    ge_duration = _private_numbers(
        dataset,
        creators,
        group=0x0043,
        low=0xA5,
        creator=GE_PARM_CREATOR,
        vr="IS",
        count=1,
    )
    present = ge_technique is not None or ge_duration is not None
    public_technique = _exact_code(
        dataset,
        Tag(0x0018, 0x9250),
        {"CONTINUOUS", "PULSED", "PSEUDOCONTINUOUS"},
    )
    valid = bool(
        present
        and (ge_duration is None or 0 <= ge_duration[0] <= 100_000_000)
        and (
            ge_technique is None
            or public_technique is None
            or ge_technique == public_technique
        )
    )
    return present, valid


def _combine_scientific_contracts(
    *contracts: _ScientificContract,
) -> _ScientificContract:
    present = [contract for contract in contracts if contract.present]
    if not present:
        return _ScientificContract(False, False)
    return _ScientificContract(
        True,
        all(contract.valid for contract in present),
        any(contract.semantic for contract in present),
    )


def audit_dicom(
    path: Path,
    *,
    expected_subject_id: str,
    expected_deidentification_policy_version: str = "1.0.0",
) -> DicomAudit:
    """Fail closed unless a rewritten DICOM satisfies the server privacy policy."""
    try:
        if not re.fullmatch(r"[a-f0-9]{24}", expected_subject_id):
            raise _PrivacyViolation()
        expected_deidentification_method = DEIDENTIFICATION_METHODS.get(
            expected_deidentification_policy_version
        )
        if expected_deidentification_method is None:
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
        expected_native_value_length = _audit_sop_conformance(
            dataset,
            expected_subject_id=expected_subject_id,
            sop_class=sop_class,
        )
        burned_in_declared = Tag(0x0028, 0x0301) in dataset
        if not burned_in_declared:
            if sop_class in ENHANCED_MR_IMAGE_STORAGE_UIDS:
                raise _PrivacyViolation()
            image_type = dataset.get(Tag(0x0008, 0x0008))
            if not isinstance(image_type, DataElement):
                raise _PrivacyViolation()
            values = set(_text_components(image_type, allow_empty=True))
            if not {"ORIGINAL", "PRIMARY"}.issubset(values) or values & {
                "DERIVED",
                "SECONDARY",
            }:
                raise _PrivacyViolation()
        state = _AuditState()
        _audit_dataset(
            dataset,
            expected_subject_id,
            expected_deidentification_method,
            state,
            0,
            sop_class,
        )
        _audit_philips_scaling_fallback(dataset, state, sop_class)
        public_diffusion_contract = _public_diffusion_contract(dataset, sop_class)
        private_diffusion_contract = _private_diffusion_contract(dataset)
        diffusion_contract = _combine_scientific_contracts(
            public_diffusion_contract,
            private_diffusion_contract,
        )
        if diffusion_contract.valid and (
            not _diffusion_sources_consistent(dataset)
            or public_diffusion_contract.present
            and private_diffusion_contract.present
            and public_diffusion_contract.semantic
            != private_diffusion_contract.semantic
        ):
            diffusion_contract = _ScientificContract(True, False)
        asl_contract = _combine_scientific_contracts(
            _public_asl_contract(dataset, sop_class),
            _private_asl_contract(dataset),
        )
        ge_asl_present, ge_asl_valid = _ge_asl_supplemental_contract(dataset)
        if ge_asl_present and (not ge_asl_valid or not asl_contract.present):
            asl_contract = _ScientificContract(True, False)
        transfer_syntax = _audit_file_meta(dataset.file_meta, dataset)
        _audit_extended_offset_table(dataset, path, pixel_offset, transfer_syntax)
        _audit_pixel_boundary(
            path,
            pixel_offset,
            transfer_syntax,
            expected_native_value_length,
        )
        temporal_position_indices = _recursive_integers(dataset, Tag(0x0020, 0x9128))
        number_of_temporal_positions = _optional_int(dataset, Tag(0x0020, 0x0105))
        if number_of_temporal_positions is None and len(temporal_position_indices) >= 2:
            number_of_temporal_positions = len(temporal_position_indices)
        image_positions, image_position_count = _recursive_image_positions(dataset)
        return DicomAudit(
            sop_instance_uid=str(dataset[Tag(0x0008, 0x0018)].value).strip(" \0"),
            sop_class_uid=sop_class,
            study_instance_uid=str(dataset[Tag(0x0020, 0x000D)].value).strip(" \0"),
            series_instance_uid=str(dataset[Tag(0x0020, 0x000E)].value).strip(" \0"),
            manufacturer=_optional_single_text(dataset, Tag(0x0008, 0x0070)),
            model=_optional_single_text(dataset, Tag(0x0008, 0x1090)),
            software_versions=frozenset(_optional_text(dataset, Tag(0x0018, 0x1020))),
            patient_position=_optional_single_text(dataset, Tag(0x0018, 0x5100)),
            magnetic_field_strength=_optional_float(dataset, Tag(0x0018, 0x0087)),
            receive_coil_name=_optional_single_text(dataset, Tag(0x0018, 0x1250)),
            transmit_coil_name=_optional_single_text(dataset, Tag(0x0018, 0x1251)),
            series_number=_optional_int(dataset, Tag(0x0020, 0x0011)),
            image_type=frozenset(
                _text_components(dataset[Tag(0x0008, 0x0008)], allow_empty=True)
            ),
            scanning_sequence=frozenset(_optional_text(dataset, Tag(0x0018, 0x0020))),
            sequence_variant=frozenset(_optional_text(dataset, Tag(0x0018, 0x0021))),
            scan_options=frozenset(_optional_text(dataset, Tag(0x0018, 0x0022))),
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
            diffusion_metadata_present=diffusion_contract.present,
            diffusion_metadata_contract_verified=diffusion_contract.valid,
            diffusion_semantic_evidence=diffusion_contract.semantic,
            asl_technique_present=Tag(0x0018, 0x9250) in dataset,
            asl_metadata_present=asl_contract.present,
            asl_metadata_contract_verified=asl_contract.valid,
            asl_technique_descriptions_emptied=(
                state.asl_technique_descriptions_emptied
            ),
            asl_crusher_descriptions_redacted=(state.asl_crusher_descriptions_redacted),
            asl_bolus_cutoff_techniques_emptied=(
                state.asl_bolus_cutoff_techniques_emptied
            ),
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
