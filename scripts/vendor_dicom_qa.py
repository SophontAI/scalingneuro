#!/usr/bin/env python3
"""Reproducible public-fixture QA for the Scaling Neuro DICOM ingest path.

The harness deliberately exercises the released boundary instead of importing
client internals:

1. fetch pinned, public scanner fixtures;
2. build a SHA-256-pinned dcm2niix release (unless an equivalent binary is
   supplied explicitly);
3. deterministically expand the short Siemens and Philips fixtures to ten
   temporal positions with synthetic, non-PHI identity and timing;
4. run the current neuro-sync executable in dry-run mode;
5. require the current cluster processor to extract and audit the exact client archive;
6. convert both source and privacy-cleared DICOM with pinned dcm2niix; and
7. compare voxel bytes, non-text NIfTI headers, and critical acquisition
   metadata while auditing the retained private-tag surface.

GE classic and Philips Enhanced are negative controls. They must remain local
with their exact compatibility reason until their private-metadata contracts
are independently implemented and validated.
"""

from __future__ import annotations

import argparse
import base64
from collections import Counter
import copy
import hashlib
import json
import os
import shutil
import struct
import subprocess
import sys
import tarfile
import tempfile
import urllib.request
from dataclasses import dataclass
from pathlib import Path
from typing import Any, Iterable, Sequence


DCM2NIIX_VERSION = "1.0.20260416"
DCM2NIIX_URL = (
    "https://github.com/rordenlab/dcm2niix/archive/refs/tags/"
    f"v{DCM2NIIX_VERSION}.tar.gz"
)
DCM2NIIX_SOURCE_SHA256 = (
    "dc87a34b8284df2700a5aee433c4ba7ea56b999ac774fcf684962de5e898670d"
)
PYDICOM_VERSION = "3.0.1"
DRY_RUN_KEY = base64.b64encode(bytes(range(32))).decode("ascii")


@dataclass(frozen=True)
class PublicFixture:
    name: str
    repository: str
    commit: str
    relative_path: str
    dicom_tree_sha256: str


PUBLIC_FIXTURES = {
    "siemens": PublicFixture(
        name="Siemens Prisma E11 classic mosaic",
        repository="https://github.com/neurolabusc/dcm_qa_stc.git",
        commit="a3c74322d4e8deee7faaaaddc9cadd06c3b7de0b",
        relative_path="In/Siemens/E11/7_Rest_fMRI_AP",
        dicom_tree_sha256=(
            "73e7e8dfc23a73473b8c54b74813c903dffc5822d5df3a45d4b25551b58d70a7"
        ),
    ),
    "philips": PublicFixture(
        name="Philips 5.1.1 classic EPI",
        repository="https://github.com/neurolabusc/dcm_qa_philips.git",
        commit="74efdbc01eb62540fbb702787c3a7a2c0e22f9eb",
        relative_path="In/Magdeburg_2014/fmri",
        dicom_tree_sha256=(
            "14d823611cb45f45f377cb3cb742c3d062e2ad60245980173874e9c559f6dc99"
        ),
    ),
    "ge": PublicFixture(
        name="GE Discovery MR750 classic EPI",
        repository="https://github.com/neurolabusc/dcm_qa_nih.git",
        commit="6a11dc671ac6a0631585d59c840e7ff364494943",
        relative_path="In/20180918GE/mr_0006",
        dicom_tree_sha256=(
            "96619e172a18342b6b5078d44f8c6662d27f577e0a673102e6d3fd5a560f7c45"
        ),
    ),
    "enhanced": PublicFixture(
        name="Philips Enhanced multi-frame fMRI",
        repository="https://github.com/neurolabusc/dcm_qa_enh.git",
        commit="58953e7b0150c6b866b2cc2fc2b40366625672dc",
        relative_path="In/Philips/IM_0035_fMRI.dcm",
        dicom_tree_sha256=(
            "cc69868f33a92218317d2987821c0e00377f638c9c3f48735efe41506ad29b6b"
        ),
    ),
}


# These are hashes of the deterministic pydicom 3.0.1 expansions, not hashes
# of the upstream directories. A mismatch means the derivation itself drifted.
DERIVED_FIXTURE_TREE_SHA256 = {
    "siemens": "69e04c568c4476f673a1019a5951e7a0fa80035e73655a95623cf106ba4efb53",
    "philips": "0f6ebfe419d41079dee7948abb8ea09bb37b75282ec372df584ca3590713a7cc",
}


CRITICAL_METADATA_KEYS = {
    "siemens": (
        "EchoTime",
        "RepetitionTime",
        "MultibandAccelerationFactor",
        "BandwidthPerPixelPhaseEncode",
        "EffectiveEchoSpacing",
        "TotalReadoutTime",
        "PhaseEncodingDirection",
        "SliceTiming",
        "ImageOrientationPatientDICOM",
        "InPlanePhaseEncodingDirectionDICOM",
    ),
    "philips": (
        "EchoTime",
        "RepetitionTime",
        "PhilipsRescaleSlope",
        "PhilipsRescaleIntercept",
        "PhilipsScaleSlope",
        "UsePhilipsFloatNotDisplayScaling",
        "EchoTrainLength",
        "PhaseEncodingSteps",
        "AcquisitionMatrixPE",
        "ReconMatrixPE",
        "WaterFatShift",
        "AcquisitionDuration",
        "PhaseEncodingAxis",
        "SliceTiming",
        "TriggerDelayTime",
        "ImageOrientationPatientDICOM",
        "InPlanePhaseEncodingDirectionDICOM",
    ),
}


EXPECTED_CONVERSION = {
    "siemens": {
        "derived_tree_sha256": DERIVED_FIXTURE_TREE_SHA256["siemens"],
        "nontext_header_sha256": (
            "41bf7e24184df51a5cc495ea09b48dde72805c9b21312d9bb458c6a65c6b1b37"
        ),
        "voxel_sha256": (
            "7934115b9a6bba2d72f4f60bcfadc3772c3d6de8a286bb542eedb1d322c89c85"
        ),
        "voxel_bytes": 7_543_920,
        "dimensions": (4, 86, 86, 51, 10, 1, 1, 1),
        "critical_json_sha256": (
            "711c86c7454ade3c5f2902d83f24161f182eec557a64a919be844a9297d3da72"
        ),
        "tr_seconds": 1.3,
        "te_seconds": 0.035,
    },
    "philips": {
        "derived_tree_sha256": DERIVED_FIXTURE_TREE_SHA256["philips"],
        "nontext_header_sha256": (
            "35041122e1b55002377adb6b3e56706744f66e546e04f522dbb5734eed8a7718"
        ),
        "voxel_sha256": (
            "13eab53cb50d0dfa00d011b8106a9cc9123f0596330454b307bda0d1fb5fc429"
        ),
        "voxel_bytes": 737_280,
        "dimensions": (4, 64, 64, 9, 10, 1, 1, 1),
        "critical_json_sha256": (
            "73aa9ddc07cd84a3e01d468b99c6ffec715157bdfe1ae827873f87e26eb61b8e"
        ),
        "tr_seconds": 2.0,
        "te_seconds": 0.030001,
    },
}


EXPECTED_PRIVATE_TAGS = {
    "siemens": {(0x0029, 0x0010), (0x0029, 0x1010)},
    "philips": {
        (0x2001, 0x0010),
        (0x2001, 0x1018),
        (0x2001, 0x1022),
        (0x2005, 0x0010),
        (0x2005, 0x100D),
        (0x2005, 0x100E),
    },
}


DIRECT_IDENTITY_TAGS = {
    (0x0008, 0x0050),  # Accession Number
    (0x0008, 0x0080),  # Institution Name
    (0x0008, 0x0081),  # Institution Address
    (0x0008, 0x0090),  # Referring Physician Name
    (0x0008, 0x1010),  # Station Name
    (0x0008, 0x1040),  # Institutional Department Name
    (0x0008, 0x1048),  # Physicians of Record
    (0x0008, 0x1050),  # Performing Physician Name
    (0x0008, 0x1060),  # Name of Physician Reading Study
    (0x0008, 0x1070),  # Operators' Name
    (0x0010, 0x0030),  # Patient Birth Date
    (0x0010, 0x0032),  # Patient Birth Time
    (0x0010, 0x1000),  # Other Patient IDs
    (0x0010, 0x1001),  # Other Patient Names
    (0x0010, 0x1040),  # Patient Address
    (0x0010, 0x2154),  # Patient Telephone Numbers
}


REMOVED_SENSITIVE_TAGS = DIRECT_IDENTITY_TAGS | {
    (0x0008, 0x1030),  # Study Description
    (0x0008, 0x103E),  # Series Description
    (0x0010, 0x0040),  # Patient Sex
    (0x0010, 0x1010),  # Patient Age
    (0x0010, 0x1020),  # Patient Size
    (0x0010, 0x1030),  # Patient Weight
    (0x0010, 0x2160),  # Ethnic Group
    (0x0010, 0x2180),  # Occupation
    (0x0010, 0x21B0),  # Additional Patient History
    (0x0010, 0x21C0),  # Pregnancy Status
    (0x0010, 0x4000),  # Patient Comments
    (0x0018, 0x1000),  # Device Serial Number
}


# Public-fixture scanner and study labels deliberately survive in the local
# pre-sanitization input. Requiring their absence from every prepared object
# catches leaks from both standard elements and retained private payloads.
FORBIDDEN_SANITIZED_MARKERS = {
    "siemens": (
        b"VENDOR_QA",
        b"Rorden^ABC_UofSC",
        b"MRC35131",
    ),
    "philips": (
        b"VENDOR_QA",
        b"Konvertertest",
        b"Leibniz Institut Magdeburg",
        b"3T-PHILIPSMR",
    ),
}


class QaFailure(RuntimeError):
    pass


def sha256_bytes(value: bytes) -> str:
    return hashlib.sha256(value).hexdigest()


def sha256_file(path: Path) -> str:
    digest = hashlib.sha256()
    with path.open("rb") as stream:
        while chunk := stream.read(1024 * 1024):
            digest.update(chunk)
    return digest.hexdigest()


def dicom_tree_hash(root: Path, files: Iterable[Path] | None = None) -> str:
    """Hash the stable `shasum` manifest used by the original fixture audit."""
    selected = list(files if files is not None else root.rglob("*.dcm"))
    selected.sort(key=lambda item: item.relative_to(root).as_posix())
    manifest = bytearray()
    for path in selected:
        relative = path.relative_to(root).as_posix()
        manifest.extend(sha256_file(path).encode("ascii"))
        manifest.extend(b"  ./")
        manifest.extend(relative.encode("utf-8"))
        manifest.extend(b"\n")
    if not selected:
        raise QaFailure(f"no DICOM files found under {root}")
    return sha256_bytes(bytes(manifest))


def canonical_json_hash(value: dict[str, Any], keys: Sequence[str]) -> str:
    selected = {key: value.get(key) for key in keys}
    encoded = json.dumps(
        selected,
        sort_keys=True,
        separators=(",", ":"),
        ensure_ascii=False,
    ).encode("utf-8")
    return sha256_bytes(encoded)


def normalized_nifti_signature(path: Path) -> dict[str, Any]:
    data = path.read_bytes()
    if len(data) < 352:
        raise QaFailure(f"NIfTI is too short: {path}")
    if struct.unpack("<I", data[:4])[0] == 348:
        endian = "<"
    elif struct.unpack(">I", data[:4])[0] == 348:
        endian = ">"
    else:
        raise QaFailure(f"not a NIfTI-1 file: {path}")
    voxel_offset = int(struct.unpack(endian + "f", data[108:112])[0])
    if voxel_offset < 348 or voxel_offset > len(data):
        raise QaFailure(f"invalid NIfTI voxel offset in {path}: {voxel_offset}")
    header = bytearray(data[:voxel_offset])
    # NIfTI text ranges may intentionally differ after de-identification.
    for start, end in ((4, 32), (148, 252), (328, 344)):
        header[start:end] = b"\0" * (end - start)
    payload = data[voxel_offset:]
    return {
        "nontext_header_sha256": sha256_bytes(bytes(header)),
        "voxel_sha256": sha256_bytes(payload),
        "voxel_bytes": len(payload),
        "dimensions": struct.unpack(endian + "8h", data[40:56]),
        "pixdim": struct.unpack(endian + "8f", data[76:108]),
    }


def deterministic_uid(label: str) -> str:
    digest = hashlib.sha256(f"scaling-neuro-vendor-qa:{label}".encode("utf-8")).digest()
    return f"2.25.{int.from_bytes(digest[:16], 'big')}"


def synthetic_time(seconds_after_noon: float) -> str:
    if not 0 <= seconds_after_noon < 60:
        raise QaFailure("fixture acquisition time offset is outside the first minute")
    return f"1200{seconds_after_noon:09.6f}"


def command_text(command: Sequence[os.PathLike[str] | str]) -> str:
    import shlex

    return " ".join(shlex.quote(os.fspath(value)) for value in command)


def run(
    command: Sequence[os.PathLike[str] | str],
    *,
    cwd: Path | None = None,
    env: dict[str, str] | None = None,
    capture: bool = False,
    allowed_returncodes: tuple[int, ...] = (0,),
) -> subprocess.CompletedProcess[str]:
    print(f"+ {command_text(command)}", flush=True)
    result = subprocess.run(
        [os.fspath(value) for value in command],
        cwd=cwd,
        env=env,
        text=True,
        stdout=subprocess.PIPE if capture else None,
        stderr=subprocess.STDOUT if capture else None,
        check=False,
    )
    if result.returncode not in allowed_returncodes:
        if capture and result.stdout:
            print(result.stdout, file=sys.stderr)
        raise QaFailure(
            f"command failed with exit {result.returncode}: {command_text(command)}"
        )
    return result


def require_tools(names: Iterable[str]) -> None:
    missing = [name for name in names if shutil.which(name) is None]
    if missing:
        raise QaFailure(f"missing required command(s): {', '.join(missing)}")


def clone_sparse(fixture: PublicFixture, destination: Path) -> Path:
    destination.mkdir(parents=True)
    run(["git", "init", "-q", destination])
    run(["git", "-C", destination, "remote", "add", "origin", fixture.repository])
    run(["git", "-C", destination, "config", "core.sparseCheckout", "true"])
    sparse = destination / ".git" / "info" / "sparse-checkout"
    sparse.write_text(f"/{fixture.relative_path}\n", encoding="utf-8")
    run(
        [
            "git",
            "-C",
            destination,
            "fetch",
            "--depth=1",
            "--filter=blob:none",
            "origin",
            fixture.commit,
        ]
    )
    run(["git", "-C", destination, "checkout", "--detach", "FETCH_HEAD"])
    actual_commit = run(
        ["git", "-C", destination, "rev-parse", "HEAD"], capture=True
    ).stdout.strip()
    if actual_commit != fixture.commit:
        raise QaFailure(
            f"{fixture.name} commit mismatch: {actual_commit} != {fixture.commit}"
        )
    source = destination / fixture.relative_path
    if source.is_file():
        actual_tree = dicom_tree_hash(source.parent, [source])
    else:
        actual_tree = dicom_tree_hash(source)
    if actual_tree != fixture.dicom_tree_sha256:
        raise QaFailure(
            f"{fixture.name} tree hash mismatch: "
            f"{actual_tree} != {fixture.dicom_tree_sha256}"
        )
    return source


def download(url: str, destination: Path) -> None:
    request = urllib.request.Request(
        url, headers={"User-Agent": "scaling-neuro-vendor-qa"}
    )
    with urllib.request.urlopen(request, timeout=120) as response:
        with destination.open("wb") as output:
            shutil.copyfileobj(response, output, length=1024 * 1024)


def safe_extract_tar(archive: Path, destination: Path) -> Path:
    destination.mkdir(parents=True)
    base = destination.resolve()
    with tarfile.open(archive, "r:gz") as stream:
        members = stream.getmembers()
        for member in members:
            target = (destination / member.name).resolve()
            if target != base and base not in target.parents:
                raise QaFailure(f"unsafe archive member: {member.name}")
            if member.issym() or member.islnk():
                raise QaFailure(
                    f"links are not allowed in source archive: {member.name}"
                )
        try:
            # Python 3.12+ rejects unsafe metadata as an additional defense.
            stream.extractall(destination, members=members, filter="data")
        except TypeError:
            # Python 3.10/3.11 do not expose the filter argument. The explicit
            # traversal and link checks above provide the same safety boundary.
            stream.extractall(destination, members=members)
    roots = [path for path in destination.iterdir() if path.is_dir()]
    if len(roots) != 1:
        raise QaFailure("dcm2niix source archive did not contain one root directory")
    return roots[0]


def verify_dcm2niix(binary: Path) -> None:
    # dcm2niix intentionally exits with code 3 for its version-only path.
    output = run([binary, "--version"], capture=True, allowed_returncodes=(0, 3)).stdout
    if f"v{DCM2NIIX_VERSION}" not in output:
        raise QaFailure(
            f"unexpected dcm2niix version; expected v{DCM2NIIX_VERSION}: {output.strip()}"
        )


def build_dcm2niix(work: Path, supplied: Path | None) -> Path:
    if supplied is not None:
        binary = supplied.resolve()
        if not binary.is_file():
            raise QaFailure(f"dcm2niix binary does not exist: {binary}")
        verify_dcm2niix(binary)
        return binary
    archive = work / "dcm2niix-source.tar.gz"
    download(DCM2NIIX_URL, archive)
    actual = sha256_file(archive)
    if actual != DCM2NIIX_SOURCE_SHA256:
        raise QaFailure(
            f"dcm2niix source hash mismatch: {actual} != {DCM2NIIX_SOURCE_SHA256}"
        )
    source = safe_extract_tar(archive, work / "dcm2niix-source")
    if shutil.which("cmake") is not None:
        build = work / "dcm2niix-build"
        run(
            [
                "cmake",
                "-S",
                source,
                "-B",
                build,
                "-DCMAKE_BUILD_TYPE=Release",
                "-DUSE_OPENJPEG=OFF",
                "-DUSE_JPEGLS=OFF",
                "-DUSE_JNIFTI=OFF",
                "-DZLIB_IMPLEMENTATION=Miniz",
            ]
        )
        run(["cmake", "--build", build, "--parallel"])
        candidates = [build / "bin" / "dcm2niix", build / "bin" / "dcm2niix.exe"]
    else:
        # The upstream release ships a small, dependency-free Makefile for
        # exactly this codec profile. This keeps the QA runnable on stock macOS
        # command-line environments where CMake is not installed.
        run(["make", "-C", source / "console", "JNIfTI=0"])
        candidates = [source / "console" / "dcm2niix"]
    binary = next((path for path in candidates if path.is_file()), None)
    if binary is None:
        raise QaFailure("dcm2niix build did not produce an executable")
    verify_dcm2niix(binary)
    return binary


def build_client(repo_root: Path, work: Path, supplied: Path | None) -> Path:
    if supplied is not None:
        binary = supplied.resolve()
        if not binary.is_file():
            raise QaFailure(f"neuro-sync binary does not exist: {binary}")
        return binary
    target = work / "client-target"
    env = os.environ.copy()
    env["CARGO_TARGET_DIR"] = os.fspath(target)
    run(
        [
            "cargo",
            "build",
            "--locked",
            "--manifest-path",
            repo_root / "client" / "Cargo.toml",
            "--bin",
            "neuro-sync",
        ],
        env=env,
    )
    suffix = ".exe" if os.name == "nt" else ""
    binary = target / "debug" / f"neuro-sync{suffix}"
    if not binary.is_file():
        raise QaFailure(f"client build did not produce {binary}")
    return binary


def require_pydicom() -> Any:
    try:
        import pydicom
    except ImportError as error:
        raise QaFailure(
            "pydicom is required; install scripts/vendor-dicom-qa-requirements.txt "
            "with --require-hashes"
        ) from error
    if pydicom.__version__ != PYDICOM_VERSION:
        raise QaFailure(
            f"pydicom {PYDICOM_VERSION} is required for byte-stable fixture generation; "
            f"found {pydicom.__version__}"
        )
    return pydicom


def scrub_fixture_identity(dataset: Any, vendor: str, ordinal: int) -> Any:
    from pydicom.tag import Tag

    for group, element in DIRECT_IDENTITY_TAGS:
        tag = Tag(group, element)
        if tag in dataset:
            del dataset[tag]
    for element in list(dataset.iterall()):
        if element.VR == "PN":
            element.value = "VENDOR_QA^PUBLIC"
        elif element.VR == "DA":
            element.value = "20000101"
        elif element.VR == "DT":
            element.value = "20000101120000.000000"
        elif element.VR == "TM":
            element.value = "120000.000000"
    dataset.PatientName = "VENDOR_QA^PUBLIC"
    dataset.PatientID = "VENDOR_QA_PUBLIC"
    dataset.StudyInstanceUID = deterministic_uid(f"{vendor}:study")
    dataset.SeriesInstanceUID = deterministic_uid(f"{vendor}:series")
    dataset.FrameOfReferenceUID = deterministic_uid(f"{vendor}:frame")
    dataset.SOPInstanceUID = deterministic_uid(f"{vendor}:instance:{ordinal:06d}")
    dataset.file_meta.MediaStorageSOPInstanceUID = dataset.SOPInstanceUID
    return dataset


def generate_siemens(source: Path, destination: Path) -> None:
    pydicom = require_pydicom()
    destination.mkdir(parents=True)
    sources = [pydicom.dcmread(path) for path in sorted(source.glob("*.dcm"))]
    if len(sources) != 2:
        raise QaFailure(f"expected two Siemens source mosaics, found {len(sources)}")
    for temporal in range(1, 11):
        dataset = scrub_fixture_identity(
            copy.deepcopy(sources[(temporal - 1) % len(sources)]),
            "siemens",
            temporal,
        )
        dataset.InstanceNumber = temporal
        dataset.AcquisitionNumber = temporal
        dataset.TemporalPositionIdentifier = temporal
        dataset.NumberOfTemporalPositions = 10
        dataset.AcquisitionTime = synthetic_time((temporal - 1) * 1.3)
        dataset.save_as(
            destination / f"volume-{temporal:03d}.dcm",
            enforce_file_format=True,
        )


def generate_philips(source: Path, destination: Path) -> None:
    pydicom = require_pydicom()
    destination.mkdir(parents=True)
    groups: dict[int, list[Any]] = {1: [], 2: [], 3: []}
    for path in source.glob("*.dcm"):
        dataset = pydicom.dcmread(path)
        temporal = int(dataset.TemporalPositionIdentifier)
        if temporal not in groups:
            raise QaFailure(f"unexpected Philips source temporal position: {temporal}")
        groups[temporal].append(dataset)
    for temporal, items in groups.items():
        items.sort(
            key=lambda item: tuple(float(value) for value in item.ImagePositionPatient)
        )
        if len(items) != 9:
            raise QaFailure(
                f"expected nine Philips slices at temporal {temporal}, found {len(items)}"
            )
    ordinal = 0
    for temporal in range(1, 11):
        source_temporal = ((temporal - 1) % 3) + 1
        for slice_number, original in enumerate(groups[source_temporal], start=1):
            ordinal += 1
            dataset = scrub_fixture_identity(
                copy.deepcopy(original), "philips", ordinal
            )
            dataset.InstanceNumber = ordinal
            dataset.AcquisitionNumber = temporal
            dataset.TemporalPositionIdentifier = temporal
            dataset.NumberOfTemporalPositions = 10
            dataset.AcquisitionTime = synthetic_time((temporal - 1) * 2.0)
            dataset[(0x2005, 0x10A0)].value = float((temporal - 1) * 2.0)
            # Exercise the real Philips behavior: public TriggerTime cycles while
            # private dynamic-scan time is cumulative. The client must prove the
            # whole-series contract before suppressing every TriggerTime.
            dataset.TriggerTime = float(((temporal - 1) % 3) * 2_000)
            image_type = [str(value) for value in dataset.ImageType]
            for value in ("EPI", "BOLD", "MAGNITUDE"):
                if value not in image_type:
                    image_type.append(value)
            dataset.ImageType = image_type
            dataset.ProtocolName = "BOLD"
            dataset.SeriesDescription = "BOLD"
            dataset.save_as(
                destination / f"volume-{temporal:03d}-slice-{slice_number:03d}.dcm",
                enforce_file_format=True,
            )


def assert_derived_fixture(name: str, root: Path) -> str:
    actual = dicom_tree_hash(root)
    expected = DERIVED_FIXTURE_TREE_SHA256[name]
    if actual != expected:
        raise QaFailure(
            f"{name} derived fixture drifted: {actual} != {expected}; "
            "do not refresh the baseline without reviewing the DICOM diff"
        )
    return actual


def initialize_state(path: Path) -> None:
    path.mkdir(parents=True)
    key = path / "dry-run-pseudonym.key"
    key.write_text(f"{DRY_RUN_KEY}\n", encoding="ascii")
    if os.name != "nt":
        key.chmod(0o600)


def load_single_report(state: Path) -> dict[str, Any]:
    reports = [
        path
        for path in (state / "reports").glob("*.json")
        if not path.name.endswith(".manifest.json")
    ]
    if len(reports) != 1:
        raise QaFailure(f"expected one client report in {state}, found {len(reports)}")
    with reports[0].open(encoding="utf-8") as stream:
        return json.load(stream)


def run_client(client: Path, source: Path, state: Path) -> dict[str, Any]:
    initialize_state(state)
    env = os.environ.copy()
    env["NEURO_SYNC_STATE_DIR"] = os.fspath(state)
    run(
        [
            client,
            "upload",
            source,
            "--dry-run",
            "--confirm-authorized",
        ],
        env=env,
    )
    return load_single_report(state)


def accepted_archive(report: dict[str, Any], state: Path, count: int) -> Path:
    summary = report.get("source_summary", {})
    if (
        summary.get("accepted") != 1
        or summary.get("held") != 0
        or summary.get("excluded") != 0
        or len(report.get("bundles", [])) != 1
    ):
        raise QaFailure(f"expected one accepted series, got {summary}")
    bundle = report["bundles"][0]
    archive = bundle.get("archive", {})
    if archive.get("dicom_instance_count") != count:
        raise QaFailure(
            f"archive DICOM count mismatch: {archive.get('dicom_instance_count')} != {count}"
        )
    path = state / "bundles" / report["run_id"] / archive["relative_key"]
    if not path.is_file():
        raise QaFailure(f"prepared archive is missing: {path}")
    if sha256_file(path) != archive["sha256"]:
        raise QaFailure(f"prepared archive hash mismatch: {path}")
    return path


def assert_hold(
    report: dict[str, Any], *, reason: str, dicom_count: int, series_count: int = 1
) -> None:
    summary = report.get("source_summary", {})
    if (
        summary.get("accepted") != 0
        or summary.get("held") != series_count
        or summary.get("excluded") != 0
        or report.get("bundles")
    ):
        raise QaFailure(f"unexpected held-series summary: {summary}")
    held = report.get("held_series", [])
    if len(held) != series_count:
        raise QaFailure(f"expected {series_count} held series, found {len(held)}")
    if series_count == 1:
        if held[0].get("reason_code") != reason:
            raise QaFailure(
                f"hold reason changed: {held[0].get('reason_code')} != {reason}"
            )
        if held[0].get("dicom_count") != dicom_count:
            raise QaFailure(
                f"held DICOM count changed: {held[0].get('dicom_count')} != {dicom_count}"
            )


def processor_archive_expectations(
    bundle: dict[str, Any], expected_count: int
) -> tuple[str, str, int]:
    archive_metadata = bundle.get("archive")
    series_archive_id = bundle.get("bundle_id")
    series_id = bundle.get("series_id")
    if (
        not isinstance(archive_metadata, dict)
        or not isinstance(series_archive_id, str)
        or len(series_archive_id) != 24
        or any(character not in "0123456789abcdef" for character in series_archive_id)
        or not isinstance(series_id, str)
        or len(series_id) != 24
        or any(character not in "0123456789abcdef" for character in series_id)
        or archive_metadata.get("dicom_instance_count") != expected_count
        or bundle.get("source_dicom_count") != expected_count
    ):
        raise QaFailure("client report cannot bind the processor archive audit")
    return series_archive_id, series_id, expected_count


def validate_processor_boundary(
    repo_root: Path,
    archive: Path,
    bundle: dict[str, Any],
    expected_count: int,
    destination: Path,
) -> tuple[Path, dict[str, Any]]:
    processor_root = repo_root / "processor"
    if not (processor_root / "scaling_neuro_processor" / "archive.py").is_file():
        raise QaFailure(f"processor implementation is missing: {processor_root}")
    zstd = shutil.which("zstd")
    if zstd is None:
        raise QaFailure("zstd is required for the processor archive boundary check")
    processor_path = os.fspath(processor_root)
    if processor_path not in sys.path:
        sys.path.insert(0, processor_path)
    try:
        from scaling_neuro_processor import PIPELINE_VERSION, __version__
        from scaling_neuro_processor.archive import (
            extract_archive as processor_extract_archive,
        )
        from scaling_neuro_processor.config import Config
        from scaling_neuro_processor.errors import ProcessorError
    except ImportError as error:
        raise QaFailure(
            "current processor imports failed; install its pinned Python dependencies"
        ) from error

    series_archive_id, series_id, dicom_count = processor_archive_expectations(
        bundle, expected_count
    )
    config = Config(
        api_url="http://127.0.0.1",
        token="vendor-qa",
        work_root=destination.parent / "processor-work",
        processor_id="vendor-dicom-qa",
        zstd_bin=zstd,
        allow_insecure_http=True,
        allowed_object_hosts=("127.0.0.1",),
    )
    try:
        manifest = processor_extract_archive(
            config,
            archive,
            destination,
            expected_series_archive_id=series_archive_id,
            expected_series_id=series_id,
            expected_dicom_count=dicom_count,
        )
    except ProcessorError as error:
        raise QaFailure(
            f"current processor rejected client archive {series_archive_id}: {error.code}"
        ) from error
    dicom = destination / "dicom"
    if len(list(dicom.glob("*.dcm"))) != dicom_count:
        raise QaFailure("processor extraction did not produce the bound DICOM count")
    return dicom, {
        "verified": True,
        "processor_version": __version__,
        "pipeline_version": PIPELINE_VERSION,
        "series_archive_id": manifest.value["series_archive_id"],
        "series_id": manifest.value["series_id"],
        "archive_manifest_sha256": manifest.sha256,
        "dicom_count": manifest.dicom_count,
        "dicom_parse_succeeded": True,
        "privacy_audit_passed": True,
        "functional_epi_confirmed": True,
    }


def recursive_private_tags(dataset: Any) -> set[tuple[int, int]]:
    tags: set[tuple[int, int]] = set()
    for element in dataset:
        if element.tag.group % 2:
            tags.add((element.tag.group, element.tag.element))
        if element.VR == "SQ":
            for item in element.value:
                tags.update(recursive_private_tags(item))
    return tags


def recursive_date_time_count(dataset: Any) -> int:
    count = 0
    for element in dataset:
        if element.VR in {"DA", "DT", "TM"}:
            count += 1
        if element.VR == "SQ":
            for item in element.value:
                count += recursive_date_time_count(item)
    return count


def pixel_inventory(datasets: Iterable[Any]) -> Counter[tuple[str, str]]:
    return Counter(
        (
            sha256_bytes(bytes(dataset.PixelData)),
            str(dataset.file_meta.TransferSyntaxUID),
        )
        for dataset in datasets
    )


def audit_sanitized_dicom(name: str, raw: Path, sanitized: Path) -> None:
    pydicom = require_pydicom()
    raw_datasets = [pydicom.dcmread(path) for path in raw.glob("*.dcm")]
    sanitized_paths = sorted(sanitized.glob("*.dcm"))
    if len(raw_datasets) != len(sanitized_paths):
        raise QaFailure(f"{name} source/sanitized DICOM count differs")
    sanitized_datasets = [pydicom.dcmread(path) for path in sanitized_paths]

    if pixel_inventory(raw_datasets) != pixel_inventory(sanitized_datasets):
        raise QaFailure(f"{name} PixelData or transfer syntax changed")
    private: set[tuple[int, int]] = set()
    for path, cleaned in zip(sanitized_paths, sanitized_datasets, strict=True):
        if recursive_date_time_count(cleaned) != 0:
            raise QaFailure(f"{name} sanitized DICOM retained DA/DT/TM in {path.name}")
        retained_sensitive = sorted(
            tag for tag in REMOVED_SENSITIVE_TAGS if tag in cleaned
        )
        if retained_sensitive:
            raise QaFailure(
                f"{name} sanitized DICOM retained sensitive standard tags in "
                f"{path.name}: {retained_sensitive}"
            )
        private.update(recursive_private_tags(cleaned))
        data = path.read_bytes()
        leaked = [
            value.decode("ascii")
            for value in FORBIDDEN_SANITIZED_MARKERS[name]
            if value in data
        ]
        if leaked:
            raise QaFailure(
                f"{name} source identity/scanner labels survived in {path.name}: "
                f"{leaked}"
            )
    if private != EXPECTED_PRIVATE_TAGS[name]:
        raise QaFailure(
            f"{name} private-tag surface changed: {sorted(private)} != "
            f"{sorted(EXPECTED_PRIVATE_TAGS[name])}"
        )


def run_dcm2niix(binary: Path, source: Path, output: Path) -> tuple[Path, Path]:
    output.mkdir(parents=True)
    run(
        [
            binary,
            "-b",
            "y",
            "-ba",
            "n",
            "-z",
            "n",
            "-p",
            "y",
            "-f",
            "qa",
            "-o",
            output,
            source,
        ]
    )
    images = list(output.glob("*.nii"))
    sidecars = list(output.glob("*.json"))
    if len(images) != 1 or len(sidecars) != 1:
        raise QaFailure(
            f"expected one 4D NIfTI/JSON pair from {source}; "
            f"found {len(images)} NIfTI and {len(sidecars)} JSON"
        )
    return images[0], sidecars[0]


def validate_conversion(
    name: str,
    dcm2niix: Path,
    raw_dicom: Path,
    sanitized_dicom: Path,
    work: Path,
) -> dict[str, Any]:
    raw_image, raw_json = run_dcm2niix(
        dcm2niix, raw_dicom, work / f"{name}-raw-conversion"
    )
    clean_image, clean_json = run_dcm2niix(
        dcm2niix, sanitized_dicom, work / f"{name}-sanitized-conversion"
    )
    raw_signature = normalized_nifti_signature(raw_image)
    clean_signature = normalized_nifti_signature(clean_image)
    if raw_signature != clean_signature:
        raise QaFailure(
            f"{name} raw/sanitized NIfTI differs:\n"
            f"raw={raw_signature}\nsanitized={clean_signature}"
        )
    expected = EXPECTED_CONVERSION[name]
    for key in (
        "nontext_header_sha256",
        "voxel_sha256",
        "voxel_bytes",
        "dimensions",
    ):
        if raw_signature[key] != expected[key]:
            raise QaFailure(
                f"{name} {key} baseline changed: "
                f"{raw_signature[key]} != {expected[key]}"
            )
    with raw_json.open(encoding="utf-8") as stream:
        raw_metadata = json.load(stream)
    with clean_json.open(encoding="utf-8") as stream:
        clean_metadata = json.load(stream)
    keys = CRITICAL_METADATA_KEYS[name]
    raw_critical = {key: raw_metadata.get(key) for key in keys}
    clean_critical = {key: clean_metadata.get(key) for key in keys}
    if raw_critical != clean_critical:
        raise QaFailure(
            f"{name} critical metadata differs:\n"
            f"raw={raw_critical}\nsanitized={clean_critical}"
        )
    critical_hash = canonical_json_hash(raw_metadata, keys)
    if critical_hash != expected["critical_json_sha256"]:
        raise QaFailure(
            f"{name} critical metadata baseline changed: "
            f"{critical_hash} != {expected['critical_json_sha256']}"
        )
    if abs(float(raw_metadata["RepetitionTime"]) - expected["tr_seconds"]) > 1e-9:
        raise QaFailure(f"{name} TR changed")
    if abs(float(raw_metadata["EchoTime"]) - expected["te_seconds"]) > 1e-9:
        raise QaFailure(f"{name} TE changed")
    return {
        "nifti": raw_signature,
        "critical_json_sha256": critical_hash,
        "critical_metadata": raw_critical,
    }


def run_positive_fixture(
    name: str,
    fixture: Path,
    count: int,
    client: Path,
    dcm2niix: Path,
    repo_root: Path,
    work: Path,
) -> dict[str, Any]:
    state = work / f"state-{name}"
    report = run_client(client, fixture, state)
    archive = accepted_archive(report, state, count)
    extracted, processor = validate_processor_boundary(
        repo_root,
        archive,
        report["bundles"][0],
        count,
        work / f"processor-archive-{name}",
    )
    audit_sanitized_dicom(name, fixture, extracted)
    conversion = validate_conversion(name, dcm2niix, fixture, extracted, work)
    return {
        "derived_fixture_tree_sha256": dicom_tree_hash(fixture),
        "archive_sha256": sha256_file(archive),
        "archive_bytes": archive.stat().st_size,
        "processor_boundary": processor,
        "conversion": conversion,
    }


def self_test() -> None:
    with tempfile.TemporaryDirectory(prefix="scaling-neuro-vendor-qa-unit-") as value:
        root = Path(value)
        (root / "b.dcm").write_bytes(b"b")
        (root / "a.dcm").write_bytes(b"a")
        first = dicom_tree_hash(root)
        second = dicom_tree_hash(root)
        if first != second:
            raise QaFailure("tree hash is not deterministic")
        if deterministic_uid("x") != deterministic_uid("x"):
            raise QaFailure("UID derivation is not deterministic")
        nifti_a = bytearray(356)
        nifti_b = bytearray(356)
        for nifti in (nifti_a, nifti_b):
            struct.pack_into("<I", nifti, 0, 348)
            struct.pack_into("<f", nifti, 108, 352.0)
            struct.pack_into("<8h", nifti, 40, 4, 1, 1, 1, 1, 1, 1, 1)
            nifti[352:] = b"DATA"
        nifti_a[4:8] = b"raw!"
        nifti_b[4:8] = b"safe"
        a = root / "a.nii"
        b = root / "b.nii"
        a.write_bytes(nifti_a)
        b.write_bytes(nifti_b)
        if normalized_nifti_signature(a) != normalized_nifti_signature(b):
            raise QaFailure("NIfTI text normalization failed")
    print("vendor DICOM QA self-test passed")


def execute(args: argparse.Namespace, work: Path) -> dict[str, Any]:
    require_tools(["git", "tar"])
    if args.dcm2niix_bin is None:
        if shutil.which("cmake") is None:
            require_tools(["make", "g++"])
    if args.client_bin is None:
        require_tools(["cargo"])
    require_pydicom()

    sources: dict[str, Path] = {}
    repositories = work / "repositories"
    repositories.mkdir()
    for key, fixture in PUBLIC_FIXTURES.items():
        print(f"\nFetching {fixture.name}", flush=True)
        sources[key] = clone_sparse(fixture, repositories / key)

    derived = work / "derived-fixtures"
    siemens = derived / "siemens"
    philips = derived / "philips"
    generate_siemens(sources["siemens"], siemens)
    generate_philips(sources["philips"], philips)
    siemens_tree = assert_derived_fixture("siemens", siemens)
    philips_tree = assert_derived_fixture("philips", philips)

    dcm2niix = build_dcm2niix(work, args.dcm2niix_bin)
    client = build_client(args.repo_root, work, args.client_bin)

    print("\nValidating Siemens classic mosaic", flush=True)
    siemens_result = run_positive_fixture(
        "siemens", siemens, 10, client, dcm2niix, args.repo_root, work
    )
    print("\nValidating Philips classic EPI", flush=True)
    philips_result = run_positive_fixture(
        "philips", philips, 90, client, dcm2niix, args.repo_root, work
    )

    print("\nValidating GE classic hold", flush=True)
    ge_report = run_client(client, sources["ge"], work / "state-ge")
    assert_hold(
        ge_report,
        reason="ge_classic_requires_verified_private_metadata_reconstruction",
        dicom_count=150,
    )

    print("\nValidating Enhanced MR hold", flush=True)
    enhanced_folder = work / "enhanced-negative"
    enhanced_folder.mkdir()
    enhanced_source = sources["enhanced"]
    shutil.copyfile(enhanced_source, enhanced_folder / enhanced_source.name)
    enhanced_report = run_client(client, enhanced_folder, work / "state-enhanced")
    assert_hold(
        enhanced_report,
        reason="enhanced_mr_pending_verified_metadata_contract",
        dicom_count=1,
    )

    return {
        "schema_version": "1.0.0",
        "dcm2niix": {
            "version": DCM2NIIX_VERSION,
            "source_url": DCM2NIIX_URL,
            "source_sha256": DCM2NIIX_SOURCE_SHA256,
        },
        "public_fixtures": {
            key: {
                "repository": fixture.repository,
                "commit": fixture.commit,
                "relative_path": fixture.relative_path,
                "dicom_tree_sha256": fixture.dicom_tree_sha256,
            }
            for key, fixture in PUBLIC_FIXTURES.items()
        },
        "derived_fixtures": {
            "siemens": siemens_tree,
            "philips": philips_tree,
        },
        "positive": {
            "siemens": siemens_result,
            "philips": philips_result,
        },
        "negative": {
            "ge": "ge_classic_requires_verified_private_metadata_reconstruction",
            "enhanced": "enhanced_mr_pending_verified_metadata_contract",
        },
    }


def parser() -> argparse.ArgumentParser:
    result = argparse.ArgumentParser(
        description=(
            "Run pinned public Siemens/Philips/GE/Enhanced DICOM compatibility QA "
            "against the current neuro-sync client."
        )
    )
    result.add_argument(
        "--repo-root",
        type=Path,
        default=Path(__file__).resolve().parents[1],
        help="Scaling Neuro checkout containing client/Cargo.toml (default: script parent)",
    )
    result.add_argument(
        "--work-dir",
        type=Path,
        help="empty directory for fetched fixtures, builds, state, and QA outputs",
    )
    result.add_argument(
        "--client-bin",
        type=Path,
        help="use this current neuro-sync binary instead of building client/",
    )
    result.add_argument(
        "--dcm2niix-bin",
        type=Path,
        help=(
            f"use this verified v{DCM2NIIX_VERSION} binary instead of building the "
            "pinned source archive"
        ),
    )
    result.add_argument(
        "--self-test",
        action="store_true",
        help="run fast deterministic helper tests without network access",
    )
    return result


def main() -> int:
    args = parser().parse_args()
    if args.self_test:
        self_test()
        return 0
    args.repo_root = args.repo_root.resolve()
    if not (args.repo_root / "client" / "Cargo.toml").is_file():
        raise QaFailure(f"not a Scaling Neuro checkout: {args.repo_root}")

    if args.work_dir is not None:
        work = args.work_dir.resolve()
        work.mkdir(parents=True, exist_ok=True)
        if any(work.iterdir()):
            raise QaFailure(f"--work-dir must be empty: {work}")
        cleanup = None
    else:
        cleanup = tempfile.TemporaryDirectory(prefix="scaling-neuro-vendor-qa-")
        work = Path(cleanup.name)
    print(f"QA workspace: {work}", flush=True)
    try:
        result = execute(args, work)
        output = work / "vendor-dicom-qa-result.json"
        output.write_text(
            json.dumps(result, indent=2, sort_keys=True) + "\n", encoding="utf-8"
        )
        print(f"\nVendor DICOM QA passed: {output}", flush=True)
        if cleanup is not None:
            print("Use --work-dir to retain fetched fixtures and QA artifacts.")
        return 0
    finally:
        if cleanup is not None:
            cleanup.cleanup()


if __name__ == "__main__":
    try:
        raise SystemExit(main())
    except QaFailure as error:
        print(f"vendor DICOM QA failed: {error}", file=sys.stderr)
        raise SystemExit(1)
