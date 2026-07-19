# Scanner and vendor QA

Scanner-neutral intake and fixture-certified conversion are separate claims. Any vendor’s standards-conformant functional MR may be ingested after the same purpose and privacy gates. A family earns a certified conversion row only when a non-PHI or explicitly cleared fixture also passes exact pixel comparison, cluster conversion, scientific QC, receipt/recovery, and withdrawal tests.

## Required evidence per route

Record:

- institution-approved phantom/public fixture provenance;
- manufacturer, model, software, transfer syntax, and DICOM form (classic files, classic mosaic, or Enhanced multi-frame);
- whether temporal structure is files, frames, or both;
- client, DICOM policy, processor, and `dcm2niix` versions;
- accepted/held/excluded outcome and stable reasons;
- recursive before/after tag audit, including nested sequences and private blocks;
- byte-for-byte Pixel Data equality for every rewritten instance;
- retained scientific fields and absence of direct identifiers, source UIDs, dates/times, free text, overlays/graphics, and unsafe private content;
- archive/member hashes and deterministic rerun result;
- cluster dimensions, volume count, voxel sizes, affine/orientation, TR/TE, datatype, finite/nonconstant signal, and output hashes;
- interrupted upload followed by the same folder command, including multipart reuse;
- two-device exact-duplicate reconciliation; and
- withdrawal/tombstone and verified R2 cleanup.

Do not upload participant scans merely to build the matrix. Use public test data, a phantom acquisition, or data explicitly cleared for this validation.

## Current `0.3.1` matrix

| Route | Workstation archive evidence | Pinned conversion evidence | Intake/processing status |
|---|---|---|---|
| Siemens `MAGNETOM Prisma_fit`, `syngo MR E11`, classic mosaic, native Explicit VR Little Endian | every CSA image header is boundedly parsed and rebuilt from the reviewed numeric/vector allowlist; Pixel Data and the private inventory were audited | exact 86x86x51x10 dimensions, affine, datatype, voxels, TR/TE, multiband factor, phase encoding, effective echo spacing, total readout time, and all 51 slice times; voxel SHA-256 `7934115b9a6bba2d72f4f60bcfadc3772c3d6de8a286bb542eedb1d322c89c85` | intake accepted when functional/privacy gates and required mosaic CSA pass; conversion certified |
| Same Siemens fixture, RLE Lossless Pixel Data | exact encapsulated Pixel Data element retained; same narrow CSA output | same conversion fields and voxel SHA-256 as the native fixture | intake accepted; conversion certified |
| Philips `Achieva dStream`, software `5.1.1`/`5.1.1.0`, classic single-frame | reviewed PS3.15 scale/slice/water-fat values survive when valid; local-only dynamic begin time is removed; redundant public `TriggerTime` is suppressed only after the complete series contract passes | exact 64x64x9x10 dimensions, affine, datatype, scaling, voxels, TR/TE, echo-train/water-fat/phase-encoding/acquisition-duration metadata; voxel SHA-256 `13eab53cb50d0dfa00d011b8106a9cc9123f0596330454b307bda0d1fb5fc429` | intake accepted by standard gates; conversion certified for this fixture |
| GE `Discovery MR750`, DV26 classic | standard metadata retained and unknown private values removed; synthetic archive/server regressions pass | prior prototype recovered exact voxels, geometry, TR/TE, phase encoding, echo spacing/readout duration, acquisition duration, and 15 slice times | intake accepted; full public fixture certification pending |
| Enhanced or Legacy Converted Enhanced MR | recursive shared/per-frame sanitizer and nested timing extraction are covered by synthetic client/server regressions | public multi-vendor conversion matrix pending | intake accepted; processing status reports conversion/QC |
| Other or missing manufacturer/model/software | safe recognized provenance is retained when present; unknown free text and private data are removed | no family-specific equivalence inferred | intake accepted by standard functional/privacy gates; processing status reports conversion/QC |

The prior `0.3.0` deterministic fixture baselines were:

- Siemens series archive ID `ec769f0fc957699701c228e6`, `dicom.tar.zst` SHA-256 `eabb2cd95627e770bfd503f17a4acd5ad84eccf847bb39d99477428c6d063951`;
- Philips series archive ID `ae353951a7e5ffb4f73fb745`, `dicom.tar.zst` SHA-256 `9ed7bb8978202018d2740815b978bc979819ddd7bdc308e3a924df48ed55e5c8`.

These identities include client version, the complete manifest, canonical scanner metadata, every rewritten instance hash, and the de-identification audit. Changing any of those inputs must change the series archive ID. The fixtures are fetched by `scripts/vendor_dicom_qa.py` from checksum-bound public repository commits; derived fixture trees, the current client, and pinned `dcm2niix` source are independently hash-verified. Fixture bytes are not copied into this repository.

Opaque private `OB`/`UN` and private text remain prohibited. Siemens CSA is never retained wholesale: the client emits a newly serialized, numeric-only image-header block. Philips private exceptions are exact owner/tag/VR/VM physical values, not a generic private-numeric policy. Manufacturer/model/software never substitutes for standard functional evidence or a fixture-certified conversion claim.

## Historical `0.2.x` evidence

The prior client-side-NIfTI path completed a public Siemens fixture through R2 and independently reproduced compressed/uncompressed NIfTI, sidecar, and manifest hashes. It also exercised authoritative R2 receipt, replay-safe enrollment, tamper/wrong-part rejection, and verified withdrawal cleanup. That evidence remains useful for transport and legacy migration, but it does not establish the new DICOM de-identification/archive contract.

The `0.3.x` release gate must therefore produce new evidence at both boundaries:

1. workstation archive privacy and scanner-native preservation; and
2. asynchronous cluster derivation from those exact rewritten DICOM bytes.

## Fixture design

Synthetic fixtures should cover at least:

- explicit and implicit VR little endian;
- encapsulated/compressed and native Pixel Data boundaries;
- Enhanced MR shared/per-frame functional groups;
- nested referenced SOP/series/frame-of-reference UIDs;
- multiple temporal positions/echoes and multi-frame temporal indices;
- Philips and GE standard acquisition values;
- recognized numeric private values and hostile private text/OB/UN;
- overlays, curves, graphic annotations, Secondary Capture, and positive/unknown burned-in annotation;
- structural, DWI, ASL, field-map, SBRef, localizer, derived, generic single-volume EPI, and ambiguous negatives; and
- malformed lengths, duplicate SOP UIDs, conflicting identities, incomplete series, symlinks, traversal names, and archive bombs.

Fixtures must assert peak memory remains bounded independently of Pixel Data size and that progress advances by streamed bytes rather than only at file boundaries.

## Promotion rule

A route may move from “intake accepted” to “conversion certified” only after the evidence above is reviewed and linked to a reproducible fixture/hash. One successful scanner does not establish conversion equivalence for every model, software release, export mode, PACS rewrite, or historical private-tag layout from the same vendor; it also must never become an intake whitelist.
