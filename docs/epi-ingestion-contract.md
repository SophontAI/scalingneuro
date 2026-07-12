# EPI ingestion contract

## The unit Scaling Neuro preserves

The beta archives one **scan bundle** per confidently identified functional EPI time series:

- an immutable, acquisition-space `.nii.gz` containing the converted voxel array and native geometry; and
- a same-basename `.json` containing privacy-filtered DICOM acquisition metadata, conversion provenance, classifier evidence, QC results, and cryptographic hashes.

This is intentionally narrower than BIDS and more useful than a pile of DICOM files. It does not invent task labels, require a heuristic file, or upload events, behavioral data, subject demographics, structural scans, DWI, ASL, fieldmaps, SBRefs, or derived images. It also does not call NIfTI byte-for-byte DICOM “raw”: the canonical beta artifact is a minimally converted acquisition-space scan whose scientific content and relevant acquisition context are preserved.

The source DICOMs stay local and unmodified. Scaling Neuro never uploads a raw DICOM header dump or an unfiltered dcm2niix sidecar.

## One-folder pipeline

1. Recursively inventory regular files without following directory symlinks. Read only the DICOM fields required to group, classify, pseudonymize, and enforce privacy.
2. Group instances by Series Instance UID in local memory. Conflicting modality, geometry, timing, subject linkage, or series identity holds the group locally.
3. Apply cheap local exclusion gates before conversion. Definite non-MR, privacy-unsafe, structural, DWI, ASL, fieldmap, SBRef, localizer, or derived series stay local. Otherwise-safe MR—including ambiguous series—is eligible for local conversion so vendor differences do not cause false rejection.
4. Convert one candidate series at a time with the bundled `dcm2niix v1.0.20260416`, including `-x i` so it neither crops nor rotates to canonical space. Preserve native sampling and geometry; do not register, normalize, crop, reorient, mask, smooth, filter, motion-correct, slice-time-correct, or quantize.
5. Re-read the NIfTI and the anonymized converter output. Serialize a new sidecar from the explicit metadata policy, sanitize NIfTI text fields, prohibit NIfTI extensions, and compute SHA-256 for compressed and uncompressed NIfTI bytes.
6. Pass local QC. A failed or ambiguous series remains local with a code-only report.
7. Partition accepted bundles into one-subject upload sessions, initialize each upload, request one checksum-and-length-bound presigned URL per multipart part, send only the accepted NIfTI/sidecar bytes, and ask the Worker to commit them.
8. Consider the run complete only after the Worker verifies every object and writes an immutable archive manifest.

The local state database makes the pipeline idempotent. Closing the UI, losing the network, or expiring a part URL resumes the existing run rather than reconverting or restarting an object. The owner-only preparation manifest binds each run to its enrolled site, project, and displayed contribution-policy version; resume fails closed if any binding differs. Parts whose ETags reached the local checkpoint are reused. In the narrow crash window after R2 accepted a part but before its ETag was checkpointed, the client safely replaces that same part number instead of requiring bucket-list access.

The normalized converter provenance is pinned to `-b y -ba y -g i -i n -l o -m 2 -p y -t n -x i -z n -f series`. In particular, `-l o` retains original datatype/scaling, `-p y` uses Philips precise scaling, `-x i` preserves native space, and compression happens only after local header scrubbing. Private input/output path operands are deliberately absent from the sidecar.

## Functional EPI classifier

Classification is fail-closed and evidence-based. Raw `SeriesDescription` and `ProtocolName` may be inspected locally as weak evidence, but are never copied to an uploaded artifact.

An accepted series must have all of the following:

- MR modality and original/primary image semantics;
- consistent series identity, dimensions, orientation, and timing;
- positive EPI evidence from standard DICOM attributes or normalized converter metadata;
- exactly one valid 4D NIfTI with at least 10 volumes, TR in `0.1–20` seconds, and TE in `(0, 2]` seconds; and
- confidence of at least `0.90`, with no exclusion or privacy signal.

The following always prevent upload:

- structural, diffusion, ASL/perfusion, fieldmap, SBRef, localizer, secondary-capture, derived, segmentation, or non-MR classification;
- diffusion gradient outputs, ASL context, fieldmap-only semantics, or single-volume EPI ambiguity;
- `BurnedInAnnotation=YES`, secondary-capture semantics, or derived-image semantics;
- inconsistent or incomplete series, invalid timing/geometry, unsupported transfer syntax, converter failure, or privacy-policy failure; and
- any classifier disagreement that lowers the result below the acceptance threshold.

Multi-echo functional EPI is represented as one scan bundle per converted echo, retaining `echo_number` and `te_seconds`. Definite non-target series are `excluded`; potentially valuable but uncertain series are `held`. Neither state uploads bytes.

Classifier evidence is code-only: `{code, source, effect}`. It must never include observed free text, source paths, raw tag values, or DICOM identifiers.

## QC contract

Every uploaded bundle has `qc.passed=true`. At minimum the client verifies:

- the NIfTI header parses, declares a supported numeric datatype, has four dimensions, and agrees with the volume count;
- spatial dimensions, voxel sizes, affine entries, TR, and TE are finite and plausible;
- the affine is usable and native geometry is retained;
- the voxel stream decodes fully, has finite non-constant signal, and matches the declared uncompressed hash;
- compressed output is valid deterministic gzip and matches the declared file hash and size;
- text header fields are sanitized and no NIfTI extensions remain;
- DICOM instance/volume accounting and converter outputs are internally consistent; and
- the sidecar validates against `scan-sidecar-v1.schema.json` and the default-deny metadata policy.

QC reports use stable codes, not free-text values. This keeps cloud metadata queryable without creating another path for identifiers.

## DICOM metadata retention and privacy

The sidecar retains the acquisition context researchers need to interpret, stratify, and reverse-index scans:

| Category | Retained examples |
|---|---|
| Scanner | manufacturer, model, software versions, field strength, receive/transmit coil |
| Sequence | sequence name, scanning sequence/variant/options, acquisition type, image type |
| Timing | TR, TE, inversion time, flip angle, slice timing, echo number |
| Readout | phase-encoding direction, effective echo spacing, total readout time, dwell time, pixel bandwidth |
| Acceleration | multiband factor, in-plane parallel factor, partial Fourier, echo-train length |
| Sampling | dimensions, voxel sizes, acquisition/reconstruction matrices, slice thickness/spacing, averages |
| Geometry | full affine, orientation, volume count, datatype, bits per voxel |
| Provenance | source DICOM count, pinned converter/version/options, client version, content hashes |

`schemas/metadata-policy-v1.json` is executable documentation for every allowed output path and its source. The default action for everything not named there is `drop`. Vendor-private information may cross the boundary only when the pinned converter emits a named, normalized field that is explicitly allowlisted; private tags themselves never do.

Text normalization is deliberately mechanical: trim DICOM padding, require printable ASCII matching the destination schema, cap each value at 128 characters, and omit the field if it fails. Do not transliterate or redact-and-keep suspicious input. DICOM code lists are split, normalized to safe code tokens, deduplicated, and bounded; numeric fields must parse to finite values inside their schema ranges. This makes `normalize_safe_ascii_128`, `normalize_safe_ascii_list_128`, and the numeric/code transforms in the policy executable rules rather than permission to copy raw strings.

Patient identifiers, demographics, dates/times from the source acquisition, accession numbers, all raw UIDs, institution/address/station/device identifiers, operators, free-text descriptions/comments, unknown tags, private tags, source filenames, and source paths are prohibited. Patient ID, issuer, study UID, series UID, and protocol strings are used only locally to produce domain-separated, site-scoped HMAC identifiers, then discarded. Every scientific pseudonym field is exactly 24 lowercase hexadecimal characters (96 HMAC bits); semantic prefixes such as `sub-` appear only in filenames, never inside the identifier value. Converter provenance contains the exact normalized behavior options but omits the private output/input path operands. Operational upload timestamps live in the control plane; volatile conversion timestamps stay in the local report so identical input produces identical sidecar bytes.

## Cloud commit and archive invariants

The create-upload request declares every pseudonymous series, subject, session, and protocol-group ID plus every relative object key, size, and SHA-256. The Worker requires every bundle in one upload session to have the same `subject_id`, validates a deterministic flat `{bundle_id}/{same-basename}.{nii.gz|json}` layout, creates each R2 multipart upload itself, and attaches trusted `sha256` and `upload_id` object metadata. One device has at most one active upload; a session holds at most 32 bundles/32 GiB and each compressed NIfTI at most 5 GiB, so larger or multi-subject folders are committed as automatic sequential sessions.

The Worker returns only the exact full key, unguessable R2 multipart upload ID, and allocated part size for each object—never an R2 access key, secret, or session token. Before each transfer the authenticated client requests a capability for `{key, part_number, size, sha256}`. The Worker proves that the key and part belong to the active upload, that the part number/range and size match the allocated object, and then returns a 15-minute presigned `UploadPart` URL plus the exact signed `content-length` and `x-amz-content-sha256` headers.

Each URL can write only that one payload to one part of one pre-created multipart upload. It cannot create or complete an object, write a different key/part/length/hash, read/copy/delete data, or list the bucket. Presigned URLs are bearer capabilities until expiry and must never enter logs or reports. Resume uses locally checkpointed ETags; it intentionally has no broad `ListParts` credential.

Completion lists exactly all initialized full keys, sizes, hashes, and consecutive `{part_number, etag}` receipts. The Worker—not the client—completes multipart uploads through its R2 binding, then performs a fresh authoritative HEAD of each persisted object before checking identity/size/metadata. It streams the stored bytes through SHA-256 and strictly validates the sidecar before catalog commit. The immediate multipart-completion return object is never treated as authoritative because live bindings may omit its custom metadata. An ETag is transport evidence, never a content hash. A verification failure expires and purges the uncommitted session; scheduled cleanup and withdrawal abort incomplete multipart uploads server-side.

The Worker then stores canonical key-sorted UTF-8 JSON plus a trailing LF at `manifests/v1/{site_id}/{project_id}/{upload_id}.json`. The reported manifest SHA-256 covers those exact stored bytes.

`bundle_hash` is the SHA-256 of canonical JSON containing:

```json
{
  "series_id": "...",
  "subject_id": "...",
  "session_id": "...",
  "nii": { "uncompressed_sha256": "..." }
}
```

Compressed NIfTI size/hash, sidecar hash, keys, ETags, and local bundle IDs are deliberately excluded from that digest. In particular, the sidecar contains compressed transport fields, so including its byte hash would make scientifically identical recompressions look different. The uncompressed NIfTI hash is scientific-content identity; compressed and sidecar hashes remain byte-integrity checks in the manifest.

The stable deduplication and tombstone key is `(site_id, project_id, bundle_id)`. `bundle_id` is a site-scoped HMAC over the source-series/content/echo identity, so retries, client-version changes, metadata enrichment, and deterministic recompression converge without conflating institutions or multi-echo outputs. A withdrawn bundle remains tombstoned under that identity and cannot be silently recreated by uploading different transport bytes. `protocol_group_id` is carried into the manifest/catalog so related acquisitions can be grouped without revealing raw protocol text or fetching every sidecar.

## Boundary for later modalities

Structural MRI is not accepted by this beta. A future structural route may reuse the bundle, upload, and manifest contracts only after adding a separate local face-privacy decision, `mri_reface` provenance, quantitative brain-preservation QC, and a fail-closed review state. Derived training formats belong under a separate cache namespace and never replace the canonical acquisition-space bundle.

## Pilot acceptance and compatibility evidence

“Works with any scanner” is not an honest acceptance criterion. The pilot instead publishes evidence: clean-machine macOS/Windows/Linux results plus a growing, PHI-free compatibility matrix for Siemens classic/enhanced/XA, Philips classic/enhanced, and GE classic/enhanced functional EPI. Each entry records scanner family/software, transfer syntax, converter/client versions, fixture provenance, conversion/QC outcome, native geometry/volume agreement, and resume/commit result.

A release is collaborator-ready only when golden fixtures prove voxel/affine stability and metadata retention, adversarial fixtures prove PHI/default-deny behavior, non-target modalities never upload, interrupted multipart transfers reuse checkpointed parts (and safely replace only the narrow uncheckpointed crash window), sidecar/manifest hashes verify from downloaded R2 bytes, and one institution-approved fresh scanner export completes through the no-arguments folder-picker flow. Unsupported data must yield a code-only held report that can improve the next compatibility release without asking a lab to email DICOMs.
