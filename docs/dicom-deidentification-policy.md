# Functional EPI DICOM de-identification policy

Policy ID: `scaling-neuro.dicom-deidentification`
Policy version: `1.0.0`

## Purpose and claim boundary

`neuro-sync` preserves scanner-native functional EPI pixels and enough acquisition metadata to reinterpret and reprocess a series later. It does not upload the files exactly as exported by a scanner or PACS. It writes a new DICOM Part 10 object for every accepted instance, audits the new object recursively, and packages only those rewritten objects.

The implementation is informed by the DICOM PS3.15 [Basic Application Level Confidentiality Profile](https://dicom.nema.org/medical/dicom/current/output/chtml/part15/chapter_E.html) and its [Retain Safe Private Option](https://dicom.nema.org/medical/dicom/current/output/chtml/part15/sect_e.3.10.html). This document is an application conformance statement, not a claim that the current beta implements every optional PS3.15 confidentiality profile for every DICOM information object. Unsupported privacy conditions fail closed.

## Invariants

For every uploaded DICOM instance:

1. Pixel Data is copied byte-for-byte, including its original transfer syntax. It is never decoded, rescaled, reoriented, cropped, masked, or recompressed on the workstation.
2. The dataset is traversed recursively, including every item in nested sequences. The same retention and removal rules apply at every depth.
3. Patient Name and Patient ID are replaced by the same site-scoped pseudonym. Study, series, SOP-instance, frame-of-reference, and referenced-instance UIDs are deterministically remapped under the site key so references remain internally consistent without retaining source UIDs.
4. `PatientIdentityRemoved (0012,0062)` is written as `YES`; `DeidentificationMethod (0012,0063)` records this policy ID and version.
5. The original preamble and File Meta Information are not copied. New File Meta Information is written with the remapped SOP Instance UID, original SOP Class and transfer syntax, and the `neuro-sync` implementation identity.
6. DICOM date, time, and date-time value representations are removed, and `LongitudinalTemporalInformationModified (0028,0303)` is set to `REMOVED`. Administrative, clinical, institution, station, operator, person, accession, device-identity, protocol-description, comments, and other free-text attributes are not allowlisted.
7. Overlay, curve, graphic-annotation, and other presentation-graphic content causes the series to remain local. A series declaring burned-in annotation also remains local.
8. Private data is default-deny. The only retained private elements are the exact creator/tag/VR/cardinality/value-shape exceptions documented below, after bounded parsing and canonical rebuilding where required. A known creator or numeric VR is not, by itself, evidence that a value is safe.
9. The rewritten file is opened again and audited before it can enter an archive. A non-allowlisted public tag, unsafe private value, source date/time, inconsistent pseudonym, or unreadable Pixel Data boundary fails the instance and holds the whole series locally.

## Retained scientific metadata

The rewritten DICOMs retain standard attributes needed to decode pixels and understand an MR acquisition, including:

- SOP class, transfer syntax, modality, manufacturer, model, and software version;
- image type and original/primary derivation evidence;
- field strength, coils, MR acquisition type, scanning sequence, sequence variant, scan options, and sequence name;
- repetition, echo, inversion, acquisition, frame, and trigger timing where represented as non-calendar numeric values;
- flip angle, bandwidth, echo-train length, acceleration and sampling factors, acquisition matrix, phase-encoding direction, and related numeric acquisition parameters;
- rows, columns, frames, samples, bit depth, pixel representation, photometric interpretation, rescale/window parameters, and pixel spacing;
- image position/orientation, slice geometry, frame-of-reference linkage, and required referenced SOP linkage; and
- only the exact vendor exceptions below when their complete semantic and structural predicates pass.

The archive-level `manifest.json` adds normalized, default-deny scanner context; pseudonymous subject/session/series/protocol identities; local classifier evidence; the policy audit; an ordered instance inventory; and SHA-256 for every rewritten DICOM and the complete archive. Raw source paths, filenames, descriptions, UIDs, and local protocol text are not copied into this manifest.

## Private metadata and vendor behavior

DICOM's own safe-private guidance warns that `OB` data may contain proprietary text, XML, or whole headers with patient information. `neuro-sync` therefore drops all private creators and values by default, including numeric values, then constructs only these reviewed exceptions:

- **Siemens classic mosaic:** `(0029,1010)` is never copied wholesale. Under the canonical `SIEMENS CSA HEADER` creator, a bounded CSA parser accepts the expected `SV10` structure and rebuilds a new `OB` value containing only `NumberOfImagesInMosaic`, `SliceNormalVector`, `SliceMeasurementDuration`, `BandwidthPerPixelPhaseEncode`, `MosaicRefAcqTimes`, `ProtocolSliceNumber`, and `PhaseEncodingDirectionPositive`. Each field has a fixed numeric/vector shape and range. Phoenix protocol text, unknown CSA entries, malformed values, and the original binary blob are prohibited. A mosaic whose every instance cannot be rebuilt is held as `siemens_classic_mosaic_requires_safe_csa`.
- **Philips classic:** the client may retain only bounded VM1 physical values for number of slices `(2001,xx18)`, water-fat shift `(2001,xx22)`, and scale intercept/slope `(2005,xx0D/xx0E)` under their exact canonical creators. No other value in those blocks is retained. The private dynamic-scan-begin value `(2005,xxA0)` is local-only evidence and is never serialized. Public `TriggerTime (0018,1060)` is suppressed only when every expected instance proves a zero-based, TR-multiple dynamic sequence under the complete series-level timing contract; otherwise the public timing remains and the private value is dropped. The archive manifest records any suppression.
- **Philips Enhanced:** the same exceptions apply recursively. A per-frame scale sequence is rebuilt as only its canonical creator plus bounded VM1 slope; malformed candidates are dropped.
- **GE, Canon/Toshiba, United Imaging, Bruker, and other scanners:** private values are default-dropped. Their standard DICOM acquisition metadata and Pixel Data remain eligible for intake; conversion can later report scanner-specific compatibility without weakening privacy.

Extended Offset Table elements are metadata and are removed; the complete encapsulated Pixel Data element is copied as one exact, final-byte-audited span. The cluster conversion result is a derived validation artifact and never retroactively makes an unsafe local archive acceptable.

## Burned-in pixels

This EPI-only beta rejects Secondary Capture, derived presentation images, overlays, and graphics before packaging. An explicit `BurnedInAnnotation (0028,0301)` value other than `NO` is rejected. For any scanner, an absent attribute may proceed only when every otherwise eligible image is `ORIGINAL` and `PRIMARY`; the client does not synthesize `NO`, and the manifest records `not_declared`. This is a bounded direct-acquisition heuristic, not proof that a PACS never changed the pixels. The client does not inspect or alter brain-image pixels for text. Structural imaging remains out of scope until a separate face-privacy path is implemented and validated.

## Determinism and audit

Each accepted series becomes one deterministic `dicom.tar.zst` object. Instance names are pseudonymous ordinals; tar ownership, permissions, and timestamps are fixed; entries are ordered; and the archive carries a checksum. Re-running the same unchanged folder under the same site and policy reproduces the same series identity and resumes the existing transfer. Changing the policy version forces local re-preparation rather than silently continuing older bytes.

This policy should be reviewed against every newly supported vendor fixture and each new DICOM standard edition. Compatibility evidence belongs in [vendor-qa.md](vendor-qa.md); a scanner-family claim requires a privacy audit and successful conversion of the rewritten DICOMs, not merely successful parsing of the originals.
