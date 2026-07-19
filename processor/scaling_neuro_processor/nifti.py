from __future__ import annotations

from dataclasses import asdict, dataclass
import gzip
import hashlib
import math
from pathlib import Path
import struct
from typing import Any

import numpy as np

from .errors import InvalidNifti
from .transport import sha256_file


HEADER_BYTES = 352
MAX_UNCOMPRESSED_BYTES = 64 * 1024**3
TEXT_RANGES = ((4, 32), (148, 252), (328, 344))
DATATYPES = {
    2: ("uint8", 8, "u1"),
    4: ("int16", 16, "i2"),
    8: ("int32", 32, "i4"),
    16: ("float32", 32, "f4"),
    64: ("float64", 64, "f8"),
    256: ("int8", 8, "i1"),
    512: ("uint16", 16, "u2"),
    768: ("uint32", 32, "u4"),
    1024: ("int64", 64, "i8"),
    1280: ("uint64", 64, "u8"),
}


@dataclass(frozen=True)
class NiftiFacts:
    dimensions: list[int]
    voxel_size_mm: list[float]
    datatype: str
    bits_per_voxel: int
    affine: list[list[float]]
    orientation: str
    volume_count: int
    tr_seconds: float
    uncompressed_sha256: str
    uncompressed_size: int

    def image_dict(self) -> dict:
        value = asdict(self)
        value.pop("uncompressed_sha256")
        value.pop("uncompressed_size")
        return value


def _endian(header: bytes) -> str:
    if struct.unpack_from("<i", header, 0)[0] == 348:
        return "<"
    if struct.unpack_from(">i", header, 0)[0] == 348:
        return ">"
    raise InvalidNifti("NIFTI_HEADER_INVALID")


def _orientation(affine: list[list[float]]) -> str:
    used: set[int] = set()
    result = ""
    positive = ("R", "A", "S")
    negative = ("L", "P", "I")
    for column in range(3):
        candidates = [
            (abs(affine[axis][column]), axis, affine[axis][column])
            for axis in range(3)
            if axis not in used
        ]
        magnitude, axis, signed = max(candidates)
        if magnitude < 1e-8:
            raise InvalidNifti("NIFTI_GEOMETRY_INVALID")
        used.add(axis)
        result += positive[axis] if signed >= 0 else negative[axis]
    return result


def _affine(header: bytes, endian: str, pixdim: list[float]) -> list[list[float]]:
    qform = struct.unpack_from(f"{endian}h", header, 252)[0]
    sform = struct.unpack_from(f"{endian}h", header, 254)[0]
    if sform > 0:
        affine = [[0.0] * 4 for _ in range(4)]
        values = struct.unpack_from(f"{endian}12f", header, 280)
        for row in range(3):
            affine[row] = [float(item) for item in values[row * 4 : row * 4 + 4]]
        affine[3][3] = 1.0
        return affine
    if qform <= 0:
        raise InvalidNifti("NIFTI_GEOMETRY_MISSING")
    b, c, d = struct.unpack_from(f"{endian}3f", header, 256)
    a_squared = 1.0 - b * b - c * c - d * d
    a = math.sqrt(a_squared) if a_squared > 1e-7 else 0.0
    rotation = [
        [a * a + b * b - c * c - d * d, 2 * (b * c - a * d), 2 * (b * d + a * c)],
        [2 * (b * c + a * d), a * a + c * c - b * b - d * d, 2 * (c * d - a * b)],
        [2 * (b * d - a * c), 2 * (c * d + a * b), a * a + d * d - c * c - b * b],
    ]
    scales = [
        abs(pixdim[1]),
        abs(pixdim[2]),
        abs(pixdim[3]) * (-1 if pixdim[0] < 0 else 1),
    ]
    offsets = struct.unpack_from(f"{endian}3f", header, 268)
    affine = [[0.0] * 4 for _ in range(4)]
    for row in range(3):
        for column in range(3):
            affine[row][column] = rotation[row][column] * scales[column]
        affine[row][3] = float(offsets[row])
    affine[3][3] = 1.0
    return affine


def parse_header(
    header: bytes, *, require_sanitized: bool = True
) -> tuple[dict, int, np.dtype, float, float]:
    if len(header) != HEADER_BYTES:
        raise InvalidNifti("NIFTI_TRUNCATED")
    endian = _endian(header)
    if header[344:348] != b"n+1\0":
        raise InvalidNifti("NIFTI_MAGIC_INVALID")
    if header[348:352] != b"\0\0\0\0":
        raise InvalidNifti("NIFTI_EXTENSIONS_FORBIDDEN")
    if require_sanitized and any(any(header[start:end]) for start, end in TEXT_RANGES):
        raise InvalidNifti("NIFTI_TEXT_NOT_SANITIZED")
    if struct.unpack_from(f"{endian}h", header, 40)[0] != 4:
        raise InvalidNifti("NIFTI_NOT_4D")
    dimensions = list(struct.unpack_from(f"{endian}4h", header, 42))
    if (
        any(value < 8 or value > 4096 for value in dimensions[:3])
        or not 10 <= dimensions[3] <= 10_000_000
    ):
        raise InvalidNifti("NIFTI_DIMENSIONS_INVALID")
    pixdim = list(struct.unpack_from(f"{endian}8f", header, 76))
    voxel_size = [abs(value) for value in pixdim[1:4]]
    if any(not math.isfinite(value) or not 0 < value <= 100 for value in voxel_size):
        raise InvalidNifti("NIFTI_VOXEL_SIZE_INVALID")
    datatype_code = struct.unpack_from(f"{endian}h", header, 70)[0]
    datatype = DATATYPES.get(datatype_code)
    if datatype is None:
        raise InvalidNifti("NIFTI_DATATYPE_UNSUPPORTED")
    bits = struct.unpack_from(f"{endian}h", header, 72)[0]
    if bits != datatype[1]:
        raise InvalidNifti("NIFTI_DATATYPE_MISMATCH")
    voxel_offset = struct.unpack_from(f"{endian}f", header, 108)[0]
    if not math.isfinite(voxel_offset) or voxel_offset != HEADER_BYTES:
        raise InvalidNifti("NIFTI_VOXEL_OFFSET_INVALID")
    slope, intercept = struct.unpack_from(f"{endian}2f", header, 112)
    if not math.isfinite(slope) or not math.isfinite(intercept):
        raise InvalidNifti("NIFTI_SCALING_INVALID")
    affine = _affine(header, endian, pixdim)
    if any(not math.isfinite(item) for row in affine for item in row):
        raise InvalidNifti("NIFTI_GEOMETRY_INVALID")
    determinant = (
        affine[0][0] * (affine[1][1] * affine[2][2] - affine[1][2] * affine[2][1])
        - affine[0][1] * (affine[1][0] * affine[2][2] - affine[1][2] * affine[2][0])
        + affine[0][2] * (affine[1][0] * affine[2][1] - affine[1][1] * affine[2][0])
    )
    if not math.isfinite(determinant) or abs(determinant) <= 1e-8:
        raise InvalidNifti("NIFTI_GEOMETRY_INVALID")
    units = header[123]
    if units & 0x07 != 2 or units & 0x38 not in {8, 16, 24}:
        raise InvalidNifti("NIFTI_UNITS_INVALID")
    raw_tr = abs(pixdim[4])
    temporal = units & 0x38
    tr_seconds = (
        raw_tr / 1000
        if temporal == 16
        else raw_tr / 1_000_000
        if temporal == 24
        else raw_tr
    )
    if not math.isfinite(tr_seconds) or not 0.1 <= tr_seconds <= 20:
        raise InvalidNifti("NIFTI_TR_INVALID")
    expected_size = HEADER_BYTES + math.prod(dimensions) * bits // 8
    if expected_size > MAX_UNCOMPRESSED_BYTES:
        raise InvalidNifti("NIFTI_TOO_LARGE")
    dtype = np.dtype(endian + datatype[2])
    # NIfTI defines a zero slope as "no scaling", in which case intercept is
    # ignored as well.
    effective_slope = float(slope) if slope != 0 else 1.0
    effective_intercept = float(intercept) if slope != 0 else 0.0
    return (
        {
            "dimensions": dimensions,
            "voxel_size_mm": voxel_size,
            "datatype": datatype[0],
            "bits_per_voxel": bits,
            "affine": affine,
            "orientation": _orientation(affine),
            "volume_count": dimensions[3],
            "tr_seconds": tr_seconds,
        },
        expected_size,
        dtype,
        effective_slope,
        effective_intercept,
    )


def sanitize_nifti(path: Path) -> None:
    try:
        with path.open("r+b") as stream:
            header = bytearray(stream.read(HEADER_BYTES))
            if len(header) != HEADER_BYTES:
                raise InvalidNifti("NIFTI_TRUNCATED")
            # Reject hidden extensions before clearing the extension flag.
            if header[348:352] != b"\0\0\0\0":
                raise InvalidNifti("NIFTI_EXTENSIONS_FORBIDDEN")
            for start, end in TEXT_RANGES:
                header[start:end] = b"\0" * (end - start)
            stream.seek(0)
            stream.write(header)
            stream.flush()
    except OSError as exc:
        raise InvalidNifti("NIFTI_IO_FAILED") from exc


def inspect_nifti_stream(
    stream: Any, *, expected_uncompressed_sha256: str | None = None
) -> NiftiFacts:
    digest = hashlib.sha256()
    header = stream.read(HEADER_BYTES)
    digest.update(header)
    facts, expected_size, dtype, slope, intercept = parse_header(header)
    total = len(header)
    remainder = b""
    signal_min: float | int | None = None
    signal_max: float | int | None = None
    while chunk := stream.read(8 * 1024**2):
        total += len(chunk)
        if total > expected_size:
            raise InvalidNifti("NIFTI_SIZE_MISMATCH")
        digest.update(chunk)
        payload = remainder + chunk
        usable = len(payload) - (len(payload) % dtype.itemsize)
        if usable:
            values = np.frombuffer(memoryview(payload)[:usable], dtype=dtype)
            if np.issubdtype(dtype, np.floating) and not bool(
                np.isfinite(values).all()
            ):
                raise InvalidNifti("NIFTI_SIGNAL_NONFINITE")
            chunk_min = values.min().item()
            chunk_max = values.max().item()
            if signal_min is None or chunk_min < signal_min:
                signal_min = chunk_min
            if signal_max is None or chunk_max > signal_max:
                signal_max = chunk_max
        remainder = payload[usable:]
    if total != expected_size:
        raise InvalidNifti("NIFTI_SIZE_MISMATCH")
    if remainder or signal_min is None or signal_max is None:
        raise InvalidNifti("NIFTI_SIZE_MISMATCH")
    scaled_min = float(signal_min) * slope + intercept
    scaled_max = float(signal_max) * slope + intercept
    if not math.isfinite(scaled_min) or not math.isfinite(scaled_max):
        raise InvalidNifti("NIFTI_SIGNAL_NONFINITE")
    if signal_min == signal_max:
        raise InvalidNifti("NIFTI_SIGNAL_CONSTANT")
    result = digest.hexdigest()
    if (
        expected_uncompressed_sha256 is not None
        and result != expected_uncompressed_sha256
    ):
        raise InvalidNifti("NIFTI_UNCOMPRESSED_HASH_MISMATCH")
    return NiftiFacts(**facts, uncompressed_sha256=result, uncompressed_size=total)


def inspect_nifti_file(path: Path) -> NiftiFacts:
    try:
        with path.open("rb") as stream:
            return inspect_nifti_stream(stream)
    except OSError as exc:
        raise InvalidNifti("NIFTI_IO_FAILED") from exc


def inspect_gzip_nifti(path: Path, expected_uncompressed_sha256: str) -> NiftiFacts:
    try:
        with gzip.open(path, "rb") as stream:
            return inspect_nifti_stream(
                stream, expected_uncompressed_sha256=expected_uncompressed_sha256
            )
    except (gzip.BadGzipFile, EOFError, OSError) as exc:
        raise InvalidNifti("NIFTI_GZIP_INVALID") from exc


def deterministic_gzip(source: Path, destination: Path) -> tuple[NiftiFacts, int, str]:
    facts = inspect_nifti_file(source)
    partial = destination.with_suffix(destination.suffix + ".partial")
    try:
        with source.open("rb") as input_stream, partial.open("wb") as raw_output:
            partial.chmod(0o600)
            with gzip.GzipFile(
                filename="", mode="wb", fileobj=raw_output, compresslevel=6, mtime=0
            ) as output:
                while chunk := input_stream.read(8 * 1024**2):
                    output.write(chunk)
        partial.replace(destination)
    except OSError as exc:
        raise InvalidNifti("NIFTI_GZIP_FAILED") from exc
    compressed_size, compressed_sha = sha256_file(destination)
    return facts, compressed_size, compressed_sha
