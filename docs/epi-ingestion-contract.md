# Functional EPI processing route

The repository’s primary intake contract is now [MR DICOM ingestion](mr-ingestion-contract.md). This document defines the narrower `functional-epi-v1` processing route applied after an MR series has passed the common local archive and metadata-privacy gates.

## Routing evidence

An MR series enters `functional-epi-v1` only when standard DICOM evidence supports both echo-planar acquisition and repeated temporal structure. Evidence may include:

- `ScanningSequence = EP` or the Enhanced MR echo-planar pulse-sequence declaration;
- canonical EPI/BOLD values in Image Type or Sequence Name;
- two or more temporal-position or acquisition identifiers;
- repeated slice positions across time, or multiple classic mosaic instances; and
- plausible, series-consistent repetition and echo timing.

Scanner manufacturer, model, software version, local protocol name, and prior fixture status are provenance rather than inclusion gates. Free-text descriptions may assist local categorization but cannot be the sole basis for functional routing because they are removed from the uploaded archive.

Diffusion, ASL/perfusion, field-map, SBRef, structural, localizer, derived, single-volume, or ambiguous MR remains uploadable under the common contract but is assigned `archive-verify-v1`. It is never sent into the functional converter merely because it is echo planar.

## Independent confirmation

The Sophont consumer must independently establish all of the following from the exact received bytes:

1. whole-archive and member hashes match the receipt and manifest;
2. the tar boundary, ordering, sizes, and extraction limits are canonical;
3. every member parses as a supported MR Image DICOM and passes the recursive metadata-privacy audit;
4. the manifest’s purpose and processing route match the job declaration;
5. retained DICOM headers independently support functional EPI; and
6. pinned conversion produces one native-space 4D NIfTI with at least ten volumes, plausible TR/TE, valid geometry/datatype, finite nonconstant signal, and a contract-valid minimized sidecar.

Only then may the processor publish `bold.nii.gz`, `bold.json`, and `processing-manifest.json` and mark the functional series processed. If independently audited headers do not confirm functional EPI, the privacy-valid series is downgraded to archive verification without conversion or deletion. Intrinsic archive/privacy violations purge the unverified raw object and tombstone its identity. Transport, timeout, converter, and scientific-compatibility failures retain the governed source archive for retry or review.

## Vendor compatibility

Standards-based intake and fixture-certified conversion are separate claims. Classic MR, Enhanced MR, Legacy Converted Enhanced MR, and supported compressed transfer syntaxes use the same route declarations, but a scanner/export family earns a conversion certification only through the reproducible evidence in [Vendor QA](vendor-qa.md). Lack of a certified fixture does not prevent safe source receipt; it remains visible as downstream compatibility state.
