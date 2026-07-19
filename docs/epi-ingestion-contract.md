# Functional EPI ingestion contract

## Canonical source and derived artifacts

The beta’s canonical ingest unit is one confidently identified functional-EPI DICOM series. It is stored as an immutable, deterministic `dicom.tar.zst` containing:

- newly written, recursively de-identified DICOM Part 10 instances with scanner-native Pixel Data; and
- `manifest.json`, containing only pseudonymous identity, normalized scanner/acquisition context, classifier and privacy-policy evidence, an ordered instance inventory, and cryptographic hashes.

The exact scanner/PACS export is never uploaded. The original remains local and unchanged. “Raw” in this project means the earliest privacy-cleared scanner-native source artifact: pixels and essential acquisition semantics are preserved, but identifying and unsafe metadata is removed or pseudonymized.

Cluster-created NIfTI, minimized sidecar, and processing manifest are deterministic derived artifacts. They are scientifically useful and independently verified, but they do not replace the canonical source archive.

## One-folder path

1. Resolve and checkpoint the canonical source-folder path. Inventory regular files recursively without following symlinks.
2. Read bounded DICOM headers, group by Series Instance UID, and reject inconsistent identities, modalities, transfer syntax, geometry, or duplicate SOP Instance UIDs.
3. Exclude definite non-MR, structural, diffusion, ASL, field-map, SBRef, localizer, secondary-capture, derived, and presentation series. Hold uncertain or unsupported formats.
4. Accept only original/primary MR with standard echo-planar evidence, repeated temporal structure, consistent plausible TR/TE, explicit privacy eligibility, and classifier confidence at least `0.90`.
5. For each accepted series, write new Part 10 headers under `scaling-neuro.dicom-deidentification` version `1.0.0`, copy Pixel Data byte-for-byte, recursively audit the result, and stream it into a deterministic zstd-compressed tar archive.
6. Initialize an idempotent DICOM upload, upload missing multipart parts through checksum-and-length-bound capabilities, and checkpoint every accepted ETag.
7. Complete the multipart objects. The Worker performs authoritative R2 `HEAD` checks, records the receipt atomically, and queues one processing job per series.
8. Return success to the workstation. The Sophont consumer later verifies and extracts the archive, runs pinned conversion, validates the functional NIfTI and sidecar, publishes derived objects, and commits processing state under an exclusive lease.

Discovery and archive generation expose byte progress, rate, and ETA. Archive generation streams each DICOM and never stages a second rewritten DICOM tree. Upload reads the prepared archive once. Receipt never waits for conversion or reads the archive through a Worker request.

## Selection semantics

Exclusion evidence always wins. Inclusion does not use vendor names or unrestricted description text. Evidence may include standard DICOM fields such as:

- `Modality = MR`;
- original/primary Image Type;
- EPI scanning/sequence encodings;
- functional Image Type terms from standardized values;
- repetition/temporal-position/frame organization consistent with a time series; and
- multi-frame temporal structure for Enhanced MR.

The client does not infer a task label, BIDS entity, diagnosis, demographics, or behavioral context. A generic single-volume EPI, ambiguous echo-planar image, or series lacking strong temporal/functional evidence is held rather than uploaded.

An accepted archive must also satisfy:

- consistent patient/study/series linkage within the group;
- unique nonempty SOP Instance UIDs;
- supported explicit Pixel Data boundaries and transfer syntax;
- no overlays, curves, graphic annotations, or disallowed presentation content;
- `BurnedInAnnotation` compatible with the active fail-closed policy for every instance; and
- successful recursive de-identification and post-write audit for every instance.

The `0.3.1` intake boundary is:

- scanner manufacturer, model, software, and prior conversion-fixture status are provenance rather than eligibility;
- classic, Enhanced, and Legacy Converted Enhanced MR use the same standard-DICOM purpose, timing, and privacy checks;
- known bounded vendor-private scientific fields may be retained, while unknown or malformed private metadata is removed rather than blocking otherwise complete standard DICOM;
- Extended Offset Table metadata is removed while the complete Pixel Data element is copied and byte-audited; and
- Siemens mosaic images require the numeric-only rebuilt CSA image geometry needed to interpret the mosaic.

This is a universal standards-based intake claim, not a claim that every scanner/export has already produced an equivalent NIfTI under the current converter. Fixture-certified conversion status is downstream QC and is recorded separately from the durable source receipt.

## Metadata and pixels

The client preserves enough standard metadata to decode and reinterpret a supported acquisition: transfer syntax and SOP class; manufacturer/model/software; MR sequence/acquisition codes; field strength and coils; numeric TR/TE/TI/flip angle/bandwidth/acceleration; matrix and bit-depth semantics; pixel spacing; orientation/position and frame of reference; and required references. Private retention is an exact creator/tag/VR/cardinality/value-shape allowlist, not a general numeric-private policy. Any intentional public-tag suppression or private-block rebuild is recorded in transformation provenance.

It removes DICOM dates/times, people, identifiers, accessions, clinical/admin text, institution/station/operator/device identity, descriptions/comments, source paths and filenames, original UIDs, presentation graphics, unknown private data, private text, and opaque private binary data. The complete executable claim boundary is [dicom-deidentification-policy.md](dicom-deidentification-policy.md).

Pixel Data is copied exactly in the original transfer syntax. It is not decoded, recompressed, rescaled, reoriented, masked, cropped, normalized, registered, motion-corrected, slice-time-corrected, filtered, smoothed, or quantized locally.

## Determinism and identity

Site-scoped HMAC pseudonyms define subject, session, series, protocol group, and series-archive identities. Source values and the site key never cross the API. All source UIDs—including references inside sequences—map consistently under the site key.

Archive entries are ordered, ordinally named, and written with fixed tar ownership/mode/timestamps. The manifest JSON is canonical and the zstd frame includes a checksum. Archive identity is based on the ordered rewritten-instance hashes and sizes. The same unchanged series under the same site and policy therefore produces the same identity.

Transport identity uses the complete compressed archive SHA-256 and exact byte length. Scientific processing records the verified source archive hash plus derived compressed/uncompressed NIfTI and sidecar hashes. Multipart ETags are transport receipts, never scientific identity.

## Continuation and concurrent devices

The canonical folder path is the local recovery key. Before rediscovery, the client searches for a compatible unfinished run bound to the same site, project, contribution-policy version, DICOM de-identification policy, and client compatibility. A rerun reuses completed archives, upload allocation, multipart ETags, and received series.

If the folder is unchanged and its receipt is complete, the command is a local no-op except for an optional processing-status refresh. If content or a privacy binding changed, the client re-evaluates it rather than appending bytes to the old attempt.

The Worker treats creation and completion as idempotent. Authenticated devices that share a managed site/project may race on the same series: an exact match is success-by-reconciliation and the losing uncommitted R2 prefix is purged. Public workstation registrations are separate site-scoped privacy domains, even when contributors enter the same lab name, so they do not collide or share pseudonym keys. A withdrawn tombstone, policy mismatch, pseudonymous-identity mismatch, or hash mismatch fails closed.

## Cluster validation

For each DICOM job, the processor:

- verifies the archive byte length and SHA-256 before trusting it;
- stream-extracts a strict tar layout with path, type, count, and size bounds;
- validates the manifest schema, every member hash/size, every SOP UID, and all de-identification/classification attestations;
- runs `dcm2niix v1.0.20260416` in the pinned processor container;
- requires exactly one finite, nonconstant, numeric 4D NIfTI with at least 10 volumes, valid dimensions/voxel sizes/affine/orientation, and plausible TR/TE consistent with the minimized sidecar;
- strips NIfTI text fields, prohibits extensions, and deterministically gzips with `mtime=0`; and
- publishes the NIfTI, sidecar, and processing manifest only while its job lease is current.

Permanent failures remain visible as stable processing codes. A server-side privacy, archive-boundary, hash, or functional-EPI purpose rejection deletes the rejected source object and tombstones its stable identity so it cannot be silently reintroduced. A converter or scientific-compatibility failure retains the de-identified source archive for review. In either case the workstation receipt remains a faithful record that transfer completed; processing status separately reports whether the source was processed, retained after failure, or purged after rejection.

## Scope

This route uploads only functional EPI. Structural T1w/T2w, diffusion, ASL, field maps, reference images, localizers, derived data, events/behavioral data, and uncertain series stay local. Structural scans require a separate face-privacy design with validated defacing and brain-preservation QC.
