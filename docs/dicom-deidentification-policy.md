# MR DICOM metadata-deidentification policy

Policy ID: `scaling-neuro.dicom-deidentification`
Policy version: `2.0.0`

## Purpose and claim boundary

`neuro-sync` preserves scanner-native MR Pixel Data and enough acquisition metadata to reinterpret a supported MR Image series later. It does not upload the files exactly as exported by a scanner or PACS. It writes a new DICOM Part 10 object for every accepted instance, audits the new object recursively, and packages only those rewritten objects.

The implementation is informed by the DICOM PS3.15 [Basic Application Level Confidentiality Profile](https://dicom.nema.org/medical/dicom/current/output/chtml/part15/chapter_E.html) and its [Retain Safe Private Option](https://dicom.nema.org/medical/dicom/current/output/chtml/part15/sect_e.3.10.html). PS3.15 explicitly separates metadata confidentiality from Pixel Data and recognizable-visual-feature treatment. This document is an application conformance statement for the supported MR Image boundary, not a claim that the beta implements every optional PS3.15 profile for every DICOM information object. Unsupported metadata-privacy conditions fail closed; native pixels remain governed as described below.

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

Required `PulseSequenceName (0018,9005)` in current Enhanced MR is privacy-normalized without imposing a vendor vocabulary: recognized scientific names use the bounded canonical value, and any other nonempty VM1 name becomes the fixed `OTHER` sentinel. An empty or multi-valued source remains invalid. Optional root `SequenceName (0018,0024)` keeps the stricter default-deny behavior and is omitted when it cannot be safely canonicalized.

`SourceImageSequence (0008,2112)` is retained only as a nonempty bounded sequence whose items contain exactly `ReferencedSOPClassUID (0008,1150)` and `ReferencedSOPInstanceUID (0008,1155)`. The SOP Class UID must be a standard DICOM UID and is preserved; every SOP Instance UID is pseudonymized with the same referential UID mapping used elsewhere. Referenced-frame, purpose-code, derivation-code, conversion-source, or other child semantics remain local until their complete macros are supported atomically.

## Private metadata and vendor behavior

DICOM's own safe-private guidance warns that `OB` data may contain proprietary text, XML, or whole headers with patient information. `neuro-sync` therefore drops every private creator and value by default—including numeric values—and reconstructs only the following reviewed exceptions. `xx` means the private block selected by the exact named creator; the block number is not assumed to be `10`.

- **Siemens CSA image header:** `(0029,xx10)` under `SIEMENS CSA HEADER` is never copied wholesale. A bounded `SV10` parser rebuilds a new `OB` value containing only `NumberOfImagesInMosaic` (one integer, 2–4096), `SliceNormalVector` (three finite values, each −1.1–1.1), `SliceMeasurementDuration` and `BandwidthPerPixelPhaseEncode` (one to three finite values, 0–10¹²), `MosaicRefAcqTimes` (4–4096 finite values, each −10⁹–10⁹), `ProtocolSliceNumber` (one integer, 0–4096), `PhaseEncodingDirectionPositive` (0 or 1), and the diffusion fields `B_value` (one value, 0–10⁶), `DiffusionGradientDirection` (three values, each −1.1–1.1), and `B_matrix` (six values, each −10⁹–10⁹). Phoenix protocol text, unknown CSA entries, malformed values, and the source blob are prohibited. A mosaic whose geometry cannot be rebuilt is held locally.
- **Siemens MR header diffusion:** under `SIEMENS MR HEADER`, only b value `(0019,xx0C)` as canonical VM1 `IS`, directionality `(0019,xx0D)` as one of `NONE`, `ISOTROPIC`, `DIRECTIONAL`, or `BMATRIX`, gradient `(0019,xx0E)` as VM3 `FD`, and b-matrix `(0019,xx27)` as VM6 `FD` may survive. Acquired diffusion additionally requires a complete b-value/direction/vector-or-matrix relationship; isolated or contradictory fields are not accepted as a diffusion series.
- **Philips diffusion and phase:** under `Philips Imaging DD 001`, `(2001,xx03)` is a VM1 `FL` b factor from 0–10⁶, `(2001,xx04)` is restricted to the reviewed direction codes `AP`, `FH`, `RL`, `NONE`, `ISOTROPIC`, or `DIRECTIONAL`, and `(2001,xx08)` is a VM1 non-negative `IS` phase number. Under `Philips MR Imaging DD 001`, `(2005,xxB0–xxB2)` may retain three VM1 finite `FL` gradient components in −1.1–1.1. Under `Philips MR Imaging DD 005`, `(2005,xx12/xx13)` may retain bounded VM1 non-negative `IS` diffusion indices.
- **Philips and public ASL:** `(2005,xx29)` under `Philips MR Imaging DD 005` is canonicalized to only `LABEL`, `CONTROL`, or `M_ZERO_SCAN`. It is sufficient for intake only with a valid public ASL technique and retained public inversion/trigger timing. Public ASL Technique Description `(0018,9252)` and Bolus Cut-off Technique `(0018,925E)` are Type 2 and are preserved as empty `LO` values. When the Crusher Flag is `YES`, the numeric Flow Limit `(0018,925A)` is retained and the required free-text Crusher Description `(0018,925B)` is replaced with the fixed value `REDACTED`. When the Bolus Cut-off Flag is `YES`, the one-item timing sequence and its numeric Delay Time `(0018,925F)` are retained. Each text transformation is recorded in the archive manifest; incomplete or contradictory conditional groups remain local.
- **Philips pixel interpretation:** the client may retain only bounded VM1 physical values for number of slices `(2001,xx18)`, water-fat shift `(2001,xx22)`, and scale intercept/slope `(2005,xx0D/xx0E)` under their exact canonical creators. The private dynamic-scan-begin value `(2005,xxA0)` is local-only evidence and is never serialized. Public `TriggerTime (0018,1060)` is suppressed only when every expected functional instance proves a zero-based, TR-multiple sequence; otherwise it remains. In Enhanced objects, a per-frame scale sequence may be rebuilt as only its canonical creator and bounded VM1 slope.
- **GE diffusion and ASL:** under `GEMS_PARM_01`, `(0043,xx39)` is an exact VM4 `IS` tuple whose first component is a b value from 0–10⁶; under `GEMS_ACQU_01`, `(0019,xxBB–xxBD)` may retain three VM1 `DS` gradient components in −1.1–1.1. The gradient is required for acquired nonzero-b diffusion. GE ASL technique `(0043,xxA3)` is restricted to `CONTINUOUS`, `PULSED`, or `PSEUDOCONTINUOUS`, and duration `(0043,xxA5)` is a VM1 non-negative `IS` no greater than 10⁸. These ASL fields are supplemental: they do not replace per-image label/control context, and free-text `(0043,xxA4)` is always dropped.
- **United Imaging:** under `Image Private Header`, GRID/VFRAME slice count `(0065,xx50)` is a VM1 integer-valued `DS` from 1–4096, diffusion b value `(0065,xx09)` is VM1 `FD` from 0–10⁶, and gradient `(0065,xx37)` is VM3 `FD` in −1.1–1.1. GRID/VFRAME requires a valid slice count; acquired nonzero-b diffusion requires a complete gradient.
- **Canon/Toshiba, Bruker, and unreviewed forms:** no private value is retained. Standard public MR acquisition metadata and Pixel Data remain eligible. A scan that depends on an unreviewed private field for safe scientific interpretation is held with a stable reason instead of weakening the privacy profile.

The server repeats the creator, tag, VR, VM, range, canonical-value, vendor-family, and complete scientific-contract checks on every rewritten instance. A manifest cannot make a partial or spoofed private contract valid.

The top-level Extended Offset Table `(7FE0,0001)` and Extended Offset Table Lengths `(7FE0,0002)` pair is retained only after structural validation: both attributes must be non-empty `OV` arrays of equal length, match `NumberOfFrames`, index an encapsulated explicit-VR little-endian Pixel Data sequence with an empty Basic Offset Table, and point to exactly one bounded fragment per frame with matching padded lengths. The complete encapsulated Pixel Data element is then copied as one exact, final-byte-audited span, and the server independently repeats those checks. A missing pair is valid; a partial, malformed, nested, or inconsistent pair is held locally. The cluster result is a derived validation artifact and never retroactively makes an unsafe local archive acceptable.

## Native-pixel and recognizable-feature boundary

The client rejects Secondary Capture, presentation objects, overlays, and graphics before packaging. An explicit `BurnedInAnnotation (0028,0301)` value other than `NO` is rejected. Current Enhanced MR and Legacy Converted Enhanced MR require an explicit `NO`; it is never synthesized. For classic MR, an absent attribute may proceed only when every otherwise eligible image is `ORIGINAL` and `PRIMARY`, and the manifest records `not_declared`. This is a bounded direct-acquisition heuristic, not proof that a PACS never changed the pixels. The client does not inspect image pixels for text.

Enhanced multi-frame metadata is SOP-class-specific and fail-closed. Current Enhanced MR must retain complete mandatory common and ORIGINAL/MIXED MR functional-group macros after rewriting, with required frame DateTimes replaced by a fixed non-source sentinel and quantitative duration/temporal fields retained. Legacy Converted Enhanced MR follows its separate A.71 macro and optional-dimension rules. Nonempty Acquisition Context, opaque/nonempty converted-attribute containers, conversion-source or richer derivation provenance beyond the exact bounded `SourceImageSequence`, concatenations, Real World Value Mapping, unsupported LUT semantics, and unreviewed optional functional-group macros are held locally rather than partially copied.

The client also does not apply DICOM's Clean Pixel Data or Clean Recognizable Visual Features options. It does not deface, crop, mask, distort, resample, or otherwise modify Pixel Data. High-resolution structural or localizer images may therefore retain recognizable facial anatomy. The archive manifest declares `scanner-native-not-defaced`, that defacing was not performed, and that recognizable visual features may be present. `PatientIdentityRemoved = YES` describes the rewritten metadata; it must not be interpreted as a claim that native pixels are anonymous. These source objects require authorization and governed storage appropriate for potentially identifiable research data, and they are not automatically public-release artifacts.

After receipt, the Sophont processor independently checks the archive boundary, member hashes, DICOM parsing, recursive metadata policy, and routing declaration for every series. Only functional EPI continues to conversion; a non-EPI archive can reach `archive verified` without creating a NIfTI derivative.

## Determinism and audit

Each accepted series becomes one deterministic `dicom.tar.zst` object. Instance names are pseudonymous ordinals; tar ownership, permissions, and timestamps are fixed; entries are ordered; and the archive carries a checksum. Re-running the same unchanged folder under the same site and policy reproduces the same series identity and resumes the existing transfer. Changing the policy version forces local re-preparation rather than silently continuing older bytes.

This policy should be reviewed against every newly supported vendor fixture and each new DICOM standard edition. Compatibility evidence belongs in [vendor-qa.md](vendor-qa.md). Every supported route requires local and server-side privacy audits of the rewritten DICOMs; a functional conversion claim additionally requires reproducible conversion and scientific-QC evidence.
