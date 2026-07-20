# MR DICOM ingestion contract

## Scope

`neuro-sync 0.4` treats the selected folder as a completed MR export, not as an EPI-selection prompt. It uploads every readable series that satisfies all of these invariants:

- `Modality = MR`;
- a supported MR Image Storage SOP Class;
- stable study, series, subject, instance, and pixel-decoding identity;
- unique nonempty SOP Instance UIDs and bounded file/series sizes;
- a Pixel Data element the streaming writer can preserve exactly;
- no overlay, curve, or graphic content;
- `BurnedInAnnotation = NO`, or an absent value only when every instance declares `ORIGINAL` and `PRIMARY`; and
- successful recursive metadata rewriting and post-write audit.

The supported purpose categories are `functional_epi`, `structural_t1w`, `structural_t2w`, `structural_other`, `diffusion`, `asl_perfusion`, `perfusion`, `fieldmap`, `sbref`, `localizer`, `derived_mr`, and `other_mr`. ASL is kept distinct because it has label/control metadata requirements; DSC, DCE, and other non-ASL perfusion remain archive-only without being forced through that ASL contract. Purpose classification controls downstream processing; it is not a scanner-vendor allowlist.

Secondary Capture, presentation states, reports, segmentation objects, encapsulated documents, raw-data and spectroscopy objects, non-MR DICOM, malformed series, and privacy-unsafe images remain local with stable report codes. “All DICOMs” in the product means all supported MR Image DICOMs that can pass this executable contract, not arbitrary DICOM information objects.

The `0.4.0` supported storage classes are classic MR Image Storage, Enhanced MR Image Storage, and Legacy Converted Enhanced MR Image Storage. Enhanced MR is admitted only when its mandatory root modules and functional-group macros remain complete after privacy rewriting. Legacy Converted Enhanced MR uses its separate A.71 contract: dimensions are optional but atomic when present, converted-attribute macro shells must be empty and correctly placed, and A.36-only MR functional groups are not misinterpreted as Legacy metadata.

The current fail-closed compatibility boundary is deliberate. Enhanced MR Color remains local until its mandatory ICC Profile has a reviewed privacy-preserving binary contract. Real World Value Mapping, opaque modality/VOI/palette LUT data, VOI LUT Function, nonempty Acquisition Context, nonempty Legacy unassigned-converted attributes, Conversion Source references, richer derivation/purpose provenance, and concatenation members also remain local rather than losing quantitative, temporal, display, or referential semantics during rewriting. The bounded `SourceImageSequence` form emitted by major Siemens classic mosaics is preserved only when every item contains exactly a standard Referenced SOP Class UID and a Referenced SOP Instance UID that can be pseudonymized. Current Enhanced MR supports its mandatory ORIGINAL/MIXED pulse-sequence, geometry, timing, echo, modifier, coil, averages, PVT, diffusion, and ASL surfaces. Optional cardiac, respiratory, contrast, functional-MR, metabolite, velocity-encoding, multi-coil-element, and other unreviewed conditional macros remain local until paired client/server contracts and scanner fixtures exist.

## One-folder workflow

1. Canonicalize the folder and bind it to a private local checkpoint.
2. Inventory regular files recursively without following symlinks, then read bounded DICOM headers with visible progress.
3. Group instances into series, enforce the invariant gates above, and assign an informational MR purpose.
4. Write a new DICOM Part 10 object for every accepted instance under `scaling-neuro.dicom-deidentification` `2.0.0`. Pixel Data remains byte-for-byte unchanged; metadata is recursively minimized, pseudonymized, and audited.
5. Stream each series into a deterministic `dicom.tar.zst` with a canonical `2.0.0` manifest, ordered member inventory, policy/routing declarations, and SHA-256 hashes.
6. Upload missing multipart bytes through short-lived, key/part/length/checksum-bound capabilities. Checkpoint every accepted ETag.
7. Complete and `HEAD`-verify each series object in R2, with a 90-day provisional retention window, before deleting its local staging archive. This creates no scientific receipt or processing job.
8. Re-inventory the complete folder, hash every DICOM byte again, and confirm a second quiet snapshot. Until this whole-folder gate passes, the R2 objects remain provisional.
9. Commit one authoritative R2 receipt per series only after the folder is stable and each object length, metadata, and checkpoint state match its declaration.
10. Queue one server-verification job per series. The workstation is finished and may go offline.
11. On Sophont, verify the archive hash, tar boundary, every member hash, DICOM parse, metadata-privacy contract, and declared processing route. Confirmed functional EPI additionally receives pinned conversion and scientific QC; every other MR category completes as a verified source archive with no EPI-derived outputs.

Rerunning `neuro-sync <same-folder>` reuses compatible local archives, multipart parts, completed provisional R2 objects, and durable receipts. If a scanner or PACS changes the folder during transfer, the run stops before creating any new durable receipt; the same command resumes after the export becomes quiet. A selection, archive, privacy, or consent-policy version change forces the appropriate re-evaluation instead of falsely treating an older, narrower run as complete.

## Source artifact and routing

One accepted series becomes one immutable `dicom.tar.zst`. Its manifest records:

- site-scoped pseudonymous subject, session, series, protocol-group, and archive identities;
- `modality = mr`, the MR purpose category, and either `functional-epi-v1` or `archive-verify-v1`;
- client, archive-schema, and metadata-de-identification versions;
- normalized safe scanner/acquisition provenance and local classification evidence;
- an ordered instance path, size, SOP Instance UID, and SHA-256 inventory; and
- the native-pixel disclosure: Pixel Data was retained, defacing was not performed, and recognizable visual features may be present.

Raw paths, filenames, original UIDs, protocol descriptions, and arbitrary DICOM values are never copied into the manifest. Every routing and pixel-risk declaration participates in archive identity and the upload request hash, so the same ID cannot silently change scientific purpose or governance.

`functional-epi-v1` is narrow: the client and server must independently confirm original/primary functional echo-planar time-series evidence before pinned `dcm2niix` conversion. `archive-verify-v1` never enters that converter. It is still independently downloaded, parsed, and privacy-audited before reaching the terminal `archive verified` state.

Purpose is independently checked rather than trusted as a destructive gate. If a client-proposed functional series passes archive and privacy verification but fails the server's functional-EPI header test, it is atomically reclassified as `other_mr`, completes as `archive-verify-v1`, and remains preserved. A purpose disagreement alone never purges scanner-native DICOM.

## Metadata and native pixels

The writer preserves the supported standard DICOM fields needed to decode and reinterpret the image: SOP/transfer syntax, pixel representation, geometry, frame organization, MR acquisition parameters, safe timing values, and normalized scanner provenance. It removes direct identifiers, calendar dates and times, clinical/administrative free text, institution/station/operator/device identifiers, source filenames and paths, presentation content, unknown private blocks, and unsafe private text or binary values. Required Enhanced frame DateTimes are replaced by a fixed non-source sentinel while Frame Acquisition Duration and temporal indices remain. Required Enhanced `PulseSequenceName` values outside the bounded canonical vocabulary become the fixed `OTHER` sentinel, so a valid scanner-specific name does not block intake or leak arbitrary text. Supported referential UIDs are deterministically remapped within the site privacy domain; a reference structure without a complete reviewed macro is rejected rather than partially retained.

Private metadata is default-deny. The only retained exceptions are bounded, rebuilt scientific values documented in [the DICOM metadata policy](dicom-deidentification-policy.md). A supported scanner does not become ineligible merely because its model or software is unfamiliar; unknown provenance is omitted rather than uploaded as unchecked text.

Pixel Data is copied exactly and is not inspected, defaced, cropped, masked, resampled, or quantized. Header de-identification therefore does **not** make high-resolution head or neck images anonymous. Native structural and localizer pixels may contain recognizable facial anatomy. The public terminal policy requires explicit institutional authorization for this governed, potentially identifiable source storage. No scanner-native archive is automatically a public-release artifact; release and face-privacy treatment are separate governed decisions.

## Receipt, verification, and failure states

Receipt and server verification are separate:

- `received` means R2 durably holds the declared bytes;
- `verification queued` or `verifying` means the server privacy/archive audit is pending;
- `archive verified` means a non-EPI source passed the independent server audit;
- `processed` means a functional EPI source also passed conversion and scientific QC;
- `failed, source retained` means a retry-exhausted operational/conversion compatibility failure requires review; and
- `failed, source purged` means an intrinsic privacy/archive-integrity violation caused deletion and a non-identifying tombstone, except that a stored-object hash mismatch proven across five full downloads exposes one exact deterministic singleton repair before becoming permanently tombstoned.

The raw archive remains the canonical governed source. Functional NIfTI, minimized sidecar, processing manifest, later defaced anatomy, and training caches are derived artifacts and never replace its provenance.

## Scale and storage

Series archives may be up to 64 GiB. `neuro-sync` creates one durable receipt per series, so a retry or independently proven object-integrity repair can resume or replace exactly that series without coupling it to its neighbors. Multipart transfer never requires the Worker to proxy scientific bytes. The control-plane API retains a bounded multi-series request shape for compatible clients, but the production terminal client deliberately uses singleton receipts. Server verification is one-series-at-a-time under exclusive, renewable leases and bounded extraction contracts, so a multi-terabyte contribution is streamed through the queue rather than copied wholesale onto Sophont storage. Successful jobs remove their temporary cluster workspace. Canonical source archives remain in R2 until governed withdrawal or retention policy removes them.
