# neuro-sync

`neuro-sync` is the local Scaling Neuro contribution client. A researcher enrolls a workstation
once, opens the application, chooses a completed DICOM folder with the operating system's native
folder picker, attests that the scans are approved under the displayed project policy, and leaves
the rest to the client. Enrollment grants institutionally pre-authorized project access; neither
enrollment nor this per-upload attestation collects or substitutes for participant consent.

The current beta is deliberately EPI-only. It never uploads source DICOMs. It converts eligible
functional EPI to minimally transformed acquisition-space NIfTI, preserves a strict set of useful
scanner/acquisition metadata, and uploads only bundles that pass the local default-deny gate.

## Researcher workflow

The release archive contains `neuro-sync` (`neuro-sync.exe` on Windows) and the pinned converter at
`libexec/dcm2niix`.

```text
# graphical flow: opens a private loopback UI and native folder chooser
neuro-sync

# headless/scanner-server flow
neuro-sync enroll ONE_TIME_INVITE
neuro-sync upload /path/to/completed-dicom-folder
neuro-sync status
neuro-sync resume
neuro-sync report
```

`run` is retained as an alias for `upload`. `upload --dry-run` performs discovery, conversion,
privacy filtering, and QC without contacting the ingest service or R2. The graphical flow always
shows the enrolled project and policy version before enabling upload.

## Local decision gate

The client recursively reads DICOM headers without reading pixel payloads, groups by Study and
Series Instance UID, and HMAC-pseudonymizes subject, session, series, protocol-group, and bundle
identifiers with the enrolled site's secret. Raw IDs, UIDs, dates, paths, protocol descriptions,
and patient fields never enter archive filenames, sidecars, manifests, or API requests.

Explicit structural, diffusion, ASL/perfusion, field-map, SBRef, localizer/scout, derived, and
secondary series are held locally. Otherwise-safe ambiguous MR may be converted locally so unusual
enhanced Philips/GE/Siemens exports are not rejected merely for missing a top-level vendor tag. It
still uploads only when converter and NIfTI evidence confirms a valid functional EPI time series.

The final gate requires:

- one verified 4D output, or a set of multi-echo outputs with unique explicit EchoNumber/TE;
- at least 10 volumes, TR in 0.1–20 seconds, and TE in (0, 2] seconds;
- no diffusion or ASL outputs/context;
- finite positive voxel sizes, plausible dimensions, a finite nondegenerate native affine, exact
  NIfTI payload size, supported datatype/bit depth, and finite nonconstant signal across the full payload;
- no NIfTI extensions and zeroed NIfTI text fields (`data_type`, `db_name`, `descrip`, `aux_file`,
  and `intent_name`).

Multi-echo runs create one bundle per echo with a shared pseudonymous series ID. Any ambiguous or
failed echo holds the entire source series.

## Archived data and metadata

Each immutable bundle contains a deterministic `.nii.gz` and same-basename `.json`. The sidecar is
not a DICOM dump or a raw dcm2niix sidecar. It follows
`../schemas/scan-sidecar-v1.schema.json` and the `scaling-neuro-epi-default-deny` policy.

Typed allowlisted metadata includes, when present and valid:

- scanner manufacturer/model/software, field strength, patient position, and receive/transmit coil;
- sequence name, standardized scanning/variant/options/image-type codes, acquisition type and
  series/acquisition/echo numbers;
- dimensions, voxel sizes, datatype, affine/orientation and volume count;
- TR, TE, inversion time, flip angle, slice timing/thickness/spacing, phase-encoding direction,
  echo spacing, readout/dwell time, bandwidth, matrices, multiband/parallel acceleration, partial
  Fourier, echo-train length, averages, imaging frequency, and nucleus.

Every copied string is length-bounded and restricted to the public schema's safe ASCII alphabet;
every numeric field is finite and range-filtered. Unknown keys, private tags, institution/station
and device identifiers, free-text descriptions/comments, demographics, dates/times, accession
numbers, and DICOM UIDs are dropped. The bundle records compressed and scrubbed-uncompressed
SHA-256 hashes plus deterministic converter/client/policy provenance.

## Resumability and integrity

Private state lives in the OS application-data directory (`ScalingNeuro/neuro-sync`) in SQLite WAL
mode. The database records prepared bundles, server-owned multipart IDs, and each uploaded part's
bare ETag. On restart, `resume` sends only locally uncheckpointed 64 MiB-class parts sequentially
bounded concurrency and resends a locally persisted completion request if the prior response was
interrupted. If R2 accepted a part immediately before a crash, re-PUT to the same multipart part
number safely replaces it.

On Unix, state directories are mode `0700` and secret-bearing files plus SQLite state/sidecars are
mode `0600`. On Windows, the default state is inside the current user's LocalAppData profile and
inherits its per-user ACL. Managed deployments that set the hidden `NEURO_SYNC_STATE_DIR` override
must provision that directory with an equivalent user-private ACL.

The Worker owns multipart creation/completion and embeds trusted `sha256` and `upload_id` object
metadata. For each exact part, the client declares the allocated key, part number, byte length, and
SHA-256; the Worker returns a short-lived UploadPart URL signed for precisely those values. The
client never receives a reusable R2 credential and cannot create, complete, read, list, or overwrite
archive objects. Upload transactions are grouped by pseudonymous subject and then split
sequentially at 32 bundles or 32 GiB, whichever comes first, so a Worker session never mixes
subjects while the whole folder remains one researcher-visible run. A
compressed NIfTI above the pilot service's 5 GiB per-object limit is held locally with an explicit
report code.

Only one client process may use a state directory at a time. Temporary DICOM staging is private,
removed after each conversion, and cleaned after an interrupted process on the next launch.
Prepared bundle bytes are removed after every archive chunk commits; hashes and the local report
remain for audit. Interrupted uploads and dry-run bundles remain local so they can be resumed or
inspected.

## Build and test

Rust 1.85 or newer is supported.

```bash
cargo build --release --manifest-path client/Cargo.toml
cargo fmt --check --manifest-path client/Cargo.toml --all
cargo clippy --manifest-path client/Cargo.toml --all-targets -- -D warnings
cargo test --manifest-path client/Cargo.toml
```

Tests include synthetic non-PHI DICOM Part 10 fixtures, classification exclusions, NIfTI
scrubbing/geometry/QC, deterministic gzip, published sidecar contract round-tripping, exact
per-part upload grants and multipart state, native-picker UI guards, and an end-to-end local dry run through a pinned fake
converter. `../schemas/validate.py` separately validates public examples and metadata-policy paths.

## Packaging contract

Release archives are named:

- `neuro-sync-vVERSION-macos-universal[-UNSIGNED-PILOT].zip`
- `neuro-sync-vVERSION-windows-x86_64[-UNSIGNED-PILOT].zip`
- `neuro-sync-vVERSION-linux-x86_64[-UNSIGNED-PILOT].tar.gz`

The binary searches for dcm2niix in this order: `NEURO_SYNC_DCM2NIIX`,
`<executable>/libexec/dcm2niix[.exe]`, beside the executable, then `PATH`. Production uploads
require exactly `v1.0.20260416`; `NEURO_SYNC_ALLOW_UNPINNED_DCM2NIIX=1` exists only for local dry-run
development. The default API is `https://scalingneuro.com`; `--server` supports a different
HTTPS deployment and loopback HTTP test servers.

This software is a research data-contribution tool, not a clinical device. Structural MRI remains
out of scope until the future local defacing and fail-closed face/brain-preservation QC route is
implemented and independently validated.
