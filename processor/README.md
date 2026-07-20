# Scaling Neuro cluster processor

This directory is the server-side verification and scientific-processing boundary for Scaling Neuro. `neuro-sync` uploads each accepted MR Image Storage series as a separately resumable DICOM archive and finishes once the control plane has durably received it. This processor independently verifies every archive, member, parsed DICOM header, and privacy invariant. Functional EPI series then follow `functional-epi-v1` and produce pinned, validated NIfTI derivatives; every other accepted MR series follows `archive-verify-v1`, produces no derivative, and completes after archive/privacy verification.

The processor is intentionally independent of Cloudflare credentials. It holds one processor API token and receives short-lived, object-scoped GET or PUT capabilities. A capability is never logged, persisted in local state, forwarded to `dcm2niix`, or sent to another host.

## Data flow

For `input_format: "dicom-series-v1"`, one claimed job performs:

1. Stream the `dicom-tar-zstd` archive into owner-only shared scratch while checking its declared length and SHA-256.
2. Decompress with the pinned `zstd` inside the same tokenless nested Pyxis boundary used for conversion, mounting only the received archive read-only and streaming decompressed bytes over stdout. Parse only the byte-exact deterministic GNU-tar encoding emitted by the client. Noncanonical identity fields, PAX/GNU extensions, links, sparse metadata, directories, duplicate/out-of-order paths, traversal, unexpected files, nonzero padding, oversized members, and trailing bytes are rejected. A series is bounded to 64 GiB compressed and extracted, 500,000 instances, up to the full 64 GiB series boundary for one multi-frame DICOM, a 128 MiB manifest, and at most 20:1 expansion (with a 64 MiB floor for small archives). Before extraction the processor preserves 20 GiB of free space and 1,024 spare inodes beyond the declared instance count; only the functional route requires two additional extracted-series conversion working sets. Decompression is terminated after a size-derived 10-to-60-minute deadline.
3. Validate the final `manifest.json` against canonical vocabularies and verify each DICOM byte length and SHA-256, then independently re-audit every DICOM rather than trusting the client attestation. This audit is bounded, default-deny, pure-Python `pydicom` controller code; it does not launch another native parser while the token is present. It enforces the public-tag allowlist at every sequence depth, pseudonymous patient fields, remapped non-semantic UIDs, standard SOP-class UIDs, constrained file meta, no date/time VRs, `LongitudinalTemporalInformationModified (0028,0303)=REMOVED`, no overlays/curves/graphics, and a readable Pixel Data boundary without loading pixels into memory. It rejects all private content except exact, independently observed, manifest-attested numeric contracts for Siemens mosaic/diffusion, Philips scaling/diffusion/phase/ASL, GE diffusion/ASL, and United Imaging grid/diffusion metadata. Every creator, tag, VR, multiplicity, numeric bound, code value, block relationship, and nested child set is exact; all neighboring private elements still fail closed. The complete allowlist is specified in [the metadata policy](../docs/dicom-deidentification-policy.md).
4. Match the manifest's bounded `series_kind`, `processing_route`, and `pixel_data_policy` to the immutable job contract. A v2 archive has `modality: "mr"`, uses deidentification policy v2, and explicitly declares `scanner-native-not-defaced`. This means DICOM pixel data is retained exactly as supplied by the scanner, defacing was not performed, and recognizable visual features may be present. The claim is disclosure, not a defacing guarantee.
5. For `archive-verify-v1`, persist a zero-output checkpoint and complete with archive hash, DICOM count, parse, and privacy-audit success while reporting `functional_epi_confirmed: false`. No converter or output grant is invoked. If the client proposed `functional-epi-v1` but the independent header audit does not confirm that purpose, use the same zero-output completion; the control plane safely downgrades the received series to `other_mr` / `archive-verify-v1` instead of deleting or repeatedly re-leasing a privacy-valid source archive.
6. For a server-confirmed `functional-epi-v1` series only, run exactly `dcm2niix v1.0.20260416` with normalized native-space arguments. In the production native-controller deployment, every conversion is a nested Slurm/Pyxis step using a checksum-recorded minimal SquashFS image. The image is read-only, does not mount the controller home, token, source, or work tree, and receives only that series' DICOM directory read-only plus a fresh output directory writable. Converter stdout/stderr and private paths never enter the job report.
7. Remove NIfTI text fields, prohibit extensions, require a single-file 4D numeric NIfTI with at least 10 volumes, stream every voxel to reject non-finite or constant signal, check its size, affine, orientation, voxel sizes, TR, and cross-check TR/TE against the converter sidecar.
8. Deterministically gzip the NIfTI (`mtime=0`), serialize a default-deny canonical sidecar, and serialize a canonical processing manifest. JSON is key-sorted UTF-8 with a trailing LF and contains no job ID or timestamp.
9. Ask for exact checksum-and-length-bound PUT grants, upload the three functional outputs, and mark the job complete only after the Worker confirms persisted object receipts.

Control-plane JSON calls retain a 120-second timeout. Each large object GET or PUT has a separate one-hour total wall-clock deadline, configurable with `--object-transfer-timeout-seconds` from 5 minutes through 24 hours. Individual socket operations are capped at five minutes so a continuously progressing slow stream still returns to the total-deadline check. This prevents a healthy multi-gigabyte transfer from inheriting the short API timeout while keeping withdrawal settlement and stalled-transfer behavior bounded.

`BurnedInAnnotation=NO` is retained when the scanner declared it. For any scanner, a missing declaration may also pass only when the image type is `ORIGINAL` and `PRIMARY` and neither `DERIVED` nor `SECONDARY`; the series manifest must distinguish `verified_no` from `not_declared`. This is a bounded direct-acquisition heuristic, not proof about a PACS export. Any positive/unknown declaration, presentation content, or mismatch fails terminally.

The local result record is keyed by the stable job ID, exact attempt and lease,
input SHA-256, processor/pipeline versions, series kind, route, and pixel policy
(plus `dcm2niix` version on the functional route). A replay of the same live
lease reuses its cryptographically bound checkpoint. A re-leased attempt uses
a disjoint workspace, so an old download, extraction, converter, or cleanup
cannot mutate the new owner's files. Zero-output archive-verification results
are checkpointed the same way as functional derivatives. Heartbeats fail
closed: once lease ownership is uncertain, no further output is uploaded or
committed. After successful completion, the processor deletes its downloaded
archive and extracted DICOM scratch; the durably received privacy-cleared
archive remains in object storage under the control plane's retention policy.

For `input_format: "nifti-v1"`, used to migrate the existing pilot upload, the job receives scoped GETs for the existing `.nii.gz` and sidecar. It validates compressed and uncompressed hashes, gzip/NIfTI structure, functional geometry, and cross-file sidecar metadata, then marks that series processed without conversion or duplicate output uploads.

## Control-plane contract

Every API request uses `Authorization: Bearer PROCESSOR_API_TOKEN`. Object GET/PUT requests deliberately omit it.

`POST /v1/processor/jobs/claim`

```json
{
  "processor_id": "slurm-12345-0",
  "lease_seconds": 900,
  "claim_input_format": "dicom-series-v1"
}
```

`claim_input_format` is optional for backward compatibility and accepts only
the exact values `dicom-series-v1` or `nifti-v1`. When present, no eligible job
of that format returns `204` even if another format is queued. A processor ID
can hold only one active lease: changing its filter cannot acquire a second job,
and an exact same-filter retry replays the first lease. A DICOM claim returns:

```json
{
  "schema_version": "1.0.0",
  "job_id": "...",
  "upload_id": "...",
  "series_archive_id": "24-lowercase-hex-characters",
  "series_id": "24-lowercase-hex-characters",
  "series_kind": "structural_t1w",
  "processing_route": "archive-verify-v1",
  "pixel_data_policy": "scanner-native-not-defaced",
  "attempt": 1,
  "lease_token": "...",
  "lease_expires_at": "2026-07-19T12:00:00Z",
  "input_format": "dicom-series-v1",
  "input": {
    "format": "dicom-tar-zstd",
    "dicom_count": 4153,
    "key": "private server object key",
    "url": "short-lived scoped GET",
    "expires_at": "2026-07-19T12:15:00Z",
    "headers": {},
    "size_bytes": 123456789,
    "sha256": "64-lowercase-hex-characters"
  }
}
```

A legacy claim replaces `input` with `nifti` and `sidecar` descriptors. The NIfTI descriptor also declares `uncompressed_sha256` and a safe basename (or object `key`).

While processing, the consumer calls:

- `POST /v1/processor/jobs/{job_id}/heartbeat` with `{lease_token, lease_seconds}`.
- `POST /v1/processor/jobs/{job_id}/outputs` only for the functional route, with the output kinds, byte lengths, SHA-256 hashes, content types, and NIfTI uncompressed hash. The response is `{outputs:[{kind,url,expires_at,headers}]}`.
- `POST /v1/processor/jobs/{job_id}/complete` with the same output descriptors, pinned versions when conversion ran, and exactly `{archive_sha256_verified,dicom_count,dicom_parse_succeeded,dicom_privacy_audit_succeeded,functional_epi_confirmed}` for DICOM jobs. Confirmed functional EPI sends three outputs and `functional_epi_confirmed: true`; archive verification or a safe purpose downgrade sends zero outputs, omits `dcm2niix_version`, and reports `false`. Legacy jobs instead report `{nifti_sha256_verified,nifti_uncompressed_sha256_verified,sidecar_sha256_verified,nifti_header_valid,sidecar_valid,nifti_sidecar_consistent}`.
- `POST /v1/processor/jobs/{job_id}/fail` with a stable code and retryability. It never sends exception text, DICOM values, paths, or URLs.

`409 LEASE_LOST` stops the job locally without a stale completion. Transient API failures are retried with bounded backoff. Retryable job failures return to the queue under the Worker’s bounded-attempt policy; archive, privacy, and scientific-contract failures are terminal. Explicit intrinsic archive/privacy violations are purge-eligible. A processor reports each full-object digest mismatch only as retryable `OBJECT_DOWNLOAD_INTEGRITY_MISMATCH`; after five independent attempts the Worker alone may conclude that the stored object is corrupt, purge it, and permit one exact deterministic singleton replacement. Extraction timeout, converter, capacity, lease, network, configuration, and internal failures preserve the source and retry or remain reviewable. A functional-purpose disagreement is not a privacy or archive failure: it completes through the archive-only downgrade above.

## Container

Build from the repository root so the Dockerfile can copy this directory:

```sh
docker build --pull -f processor/Dockerfile -t scaling-neuro-processor:0.2.0 .
```

The build downloads the official `dcm2niix v1.0.20260416` source tarball and verifies its pinned SHA-256 before compilation. JPEG-LS and JPEG 2000 support are enabled for cross-vendor scanner compatibility. `pydicom 3.0.1` and NumPy `2.2.6` are installed from hash-pinned wheels; NumPy performs bounded, vectorized full-voxel finite/nonconstant validation. The runtime is non-root and contains no secret or R2 credential.

Run a single local job with a mode-`0600` token file:

```sh
docker run --rm \
  --user "$(id -u):$(id -g)" \
  --mount type=bind,src="$PWD/processor-token",dst=/run/secrets/scaling-neuro-processor-token,readonly \
  --mount type=bind,src="$PWD/processor-work",dst=/data/scaling-neuro/processor \
  scaling-neuro-processor:0.2.0 \
  --api-url https://scalingneuro.com --max-jobs 1
```

## Sophont Slurm deployment

The supplied jobs target the current cluster layout: Slurm commands live under `/opt/slurm/bin`, Enroot is `/usr/bin/enroot`, shared storage is under `/data/paul`, jobs run under account `sophont` on partition `c` at `--qos=bottom`, and consumers are requeueable. No secret is exported through Slurm.

On `login-1.sophont-n.cgen`, prepare protected shared paths once:

```sh
install -d -m 700 /data/paul/scaling-neuro/{images,logs,processor,secrets}
install -m 600 /dev/stdin /data/paul/scaling-neuro/secrets/processor-token
```

Type the processor token on stdin and press Control-D; do not place it in shell history.

### Preferred now: native controller with an isolated converter step

The currently deployable security boundary keeps the network/API controller in a hash-locked Python environment and launches each native parser (`zstd` and `dcm2niix`) as a separate nested Pyxis step. This matters for open ingestion: both process contributor-controlled bytes, but their environment contains no processor token and their container is not given direct mounts for the token, controller source, home directory, shared work tree, or another series. The image is read-only, runs as the submitting user rather than remapped root, gets a restricted `/dev`, and receives only the minimum per-invocation mounts. `zstd` gets one archive read-only and stdout; `dcm2niix` gets `/input` read-only and a fresh `/output` writable. Owner-only, per-controller Enroot runtime/cache directories prevent collisions with the cluster's shared default `/tmp/enroot` path.

Pyxis on the current cluster shares the node network namespace. The isolated converter therefore still has network access, although it receives no credential or unrelated local data. Treat outbound-network denial as a cluster-policy hardening item; it is not falsely claimed by this implementation.

Install the pinned release from a repository checkout visible to compute nodes. The installer must itself run in a compute allocation. It creates the hash-locked Python controller; records a deterministic SHA-256 over every installed controller `.py` file plus `requirements.lock`; downloads and verifies the official Linux x86-64 `dcm2niix v1.0.20260416` archive (SHA-256 `e88b40f6ebbcf9f47ebfdd7bb5f0127297cb7e8b06266a91a4642b5814031bd0`); verifies the compute-node `zstd v1.5.5` binary (SHA-256 `7c5468b370f7c47eda07281e3437fafc568f95d10420051e3aa522709f9342c5`); and builds a minimal SquashFS containing those two tools, their dynamic libraries, and `/bin/sh`. It records the image hash and validates both tools through the same nested Pyxis path used in production.

```sh
cd /data/paul/scaling-neuro/source
/opt/slurm/bin/sbatch \
  processor/slurm/install-native.sbatch \
  "$PWD/processor" \
  /data/paul/scaling-neuro/native/releases/0.2.0 \
  /usr/bin/python3.12
```

The compute node must expose Python 3.12, the exact pinned system `zstd`, `mksquashfs`, Slurm, and Pyxis. The installer builds in a unique staging directory, verifies dependency versions, controller-source and SquashFS checksums, and both native-tool versions inside Pyxis, then atomically publishes the versioned directory. Reusing an existing release re-hashes the current source tree, installed controller, and tool image; any mismatch is rejected rather than silently running stale code.

After the install job reports success, start the persistent consumer:

```sh
processor/scripts/submit-native-consumer.sh \
  /data/paul/scaling-neuro/native/releases/0.2.0 \
  https://scalingneuro.com \
  /data/paul/scaling-neuro/processor \
  /data/paul/scaling-neuro/secrets/processor-token
```

The job polls continuously (`--idle-exit-after 0`), requests the `c` partition's infinite time limit (`--time=0`), receives a preemption warning two minutes early, and relies on Slurm `--requeue`. The production native launch is deliberately pinned to `--claim-input-format dicom-series-v1`, so the release and public raw-DICOM path cannot be captured by the one-time legacy NIfTI backlog. Multiple identical consumers are safe: the Worker's lease token makes claims exclusive. Scale with additional jobs only when queue depth warrants it.

### Whole-processor image: reproducible packaging, not parser isolation

Build the image in CI or in a compute/build environment, never on a shared login node. Once an immutable image is published, import it into shared Enroot storage:

```sh
/usr/bin/enroot import \
  -o /data/paul/scaling-neuro/images/processor-0.2.0.sqsh \
  docker://ghcr.io/sophontai/scaling-neuro-processor:0.2.0
```

Submit the persistent, bottom-priority consumer:

```sh
processor/scripts/submit-consumer.sh \
  /data/paul/scaling-neuro/images/processor-0.2.0.sqsh \
  https://scalingneuro.com \
  /data/paul/scaling-neuro/processor \
  /data/paul/scaling-neuro/secrets/processor-token
```

This mode is useful for reproducible packaging and is launched read-only without home mounting or remapped root. It is **not** the preferred open-ingest boundary on the current cluster: the converter shares the whole processor container's token and work mounts. Do not use it for untrusted public uploads until it adds an equivalent inner converter boundary. The repository also does not yet contain a release workflow that publishes this image to GHCR, so the import command is a deployment specification rather than a currently available artifact.

## Tests

The suite uses a real local HTTP control plane/object server, real zstd archives, synthetic functional and structural DICOM/NIfTI fixtures, and a fake pinned converter. It verifies both v2 routes, legacy v1 compatibility, bounded MR kinds/metadata, route mismatch rejection, zero-output resume, bearer-token separation, checksum-bound output grants, traversal/symlink rejection, SOP/hash mismatches, NIfTI contract failures, and deterministic gzip.

```sh
cd processor
python3 -m venv .venv
.venv/bin/pip install --require-hashes -r requirements.lock
PYTHONPATH=. .venv/bin/python -m unittest discover -v
```

Container validation should additionally assert:

```sh
docker run --rm --entrypoint dcm2niix scaling-neuro-processor:0.2.0 --version
```

The reported version must contain `v1.0.20260416`.
