# neuro-sync

`neuro-sync` is the single-executable Scaling Neuro workstation client. Give it a completed scanner-export folder and it finds every supported MR Image series, writes privacy-cleared DICOM copies into deterministic per-series archives, and uploads them with automatic continuation, progress, speed, and ETA. It does not convert scans to NIfTI locally.

## Install and run

```sh
curl -fsSL https://scalingneuro.com/install.sh | sh
neuro-sync /path/to/completed-dicom-folder
```

On Windows PowerShell:

```powershell
irm https://scalingneuro.com/install.ps1 | iex
neuro-sync "C:\path\to\completed-dicom-folder"
```

The installer downloads one SHA-256-pinned package and returns to the shell. First-use registration, lab details, the policy summary, and authorization confirmation all stay in the terminal. Multiple workstations from the same lab register independently.

Rerun the same folder command after any interruption. Its canonical path and privacy context select the unfinished local checkpoint; existing archives, uploaded parts, and completed receipts are reused. There is no separate resume operation.

Useful explicit forms:

```sh
neuro-sync register --email researcher@example.edu --name "Researcher Name" \
  --institution "Example University" --lab "Example Lab" \
  --accept-policy-version open-mri-1.0.0
neuro-sync upload /path/to/dicoms
neuro-sync upload /path/to/dicoms --confirm-authorized \
  --confirm-native-pixels-authorized
# If the server reports a newer policy, automation must review and name it:
neuro-sync upload /path/to/dicoms --confirm-authorized \
  --confirm-native-pixels-authorized \
  --accept-policy-version open-mri-1.0.0
neuro-sync upload /path/to/dicoms --dry-run
neuro-sync status --json
neuro-sync report RUN_ID --json
```

The two confirmation flags are for non-interactive automation. They attest both institutional authorization for the selected scans and specific authorization to transfer scanner-native pixels without defacing. Interactive use asks for the same confirmation in the terminal.

## Local privacy boundary

The source folder is never modified. Before any copy leaves the workstation, the client:

- accepts privacy-clearable classic MR Image, Enhanced MR Image, and Legacy Converted Enhanced MR Image series from any scanner vendor;
- classifies accepted series as functional EPI, T1w/T2w/other structural, diffusion, ASL/perfusion, field map, SBRef, localizer, derived MR, or other MR so downstream processing is explicit rather than inferred from filenames;
- keeps malformed objects, non-MR objects, unsupported SOP classes such as spectroscopy and secondary capture, overlays/graphics, declared burned-in annotations, unsafe undeclared annotations, and objects that exceed bounded resource limits local;
- recursively allowlists DICOM attributes, pseudonymizes identity and UIDs consistently, removes dates/times and unsafe text/private data, rebuilds Part 10 metadata, and audits the rewritten object;
- byte-copies Pixel Data in its original transfer syntax without decoding or recompression; and
- writes one deterministic `dicom.tar.zst` containing ordinal DICOM filenames plus `manifest.json` and hashes.

Scanner-native Pixel Data is intentionally not defaced, cropped, masked, or otherwise altered. Structural and other head MR images may therefore contain recognizable facial anatomy even after DICOM headers are de-identified. Upload requires explicit institutional authorization for that boundary; `neuro-sync` does not treat header de-identification as a substitute for participant consent, IRB review, data-use agreements, or local governance.

Each archive manifest records `modality: "mr"`, its `series_kind`, its downstream `processing_route`, `pixel_data_policy: "scanner-native-not-defaced"`, instance hashes, retained scanner/acquisition provenance, an explicit `writer_contract`, and a de-identification audit. The current DICOM manifest and recursive allowlist policy versions are both `2.0.0`. Deterministic bytes and folder completion are keyed to explicit archive (`2.0.0`) and classifier (`2.0.0`) contracts, not the `neuro-sync` binary patch number, so a routine client update cannot re-upload an unchanged multi-terabyte export. The exact binary version remains outer receipt provenance.

The `0.4.0` intake boundary is vendor-neutral:

- manufacturer, model, software, and prior fixture status are provenance—not upload eligibility;
- all supported MR Image storage classes use the same local privacy and integrity predicates;
- recognized safe make/model/software fields and bounded standard acquisition metadata are retained, while unknown/malformed private metadata is default-dropped;
- a top-level Extended Offset Table and Extended Offset Table Lengths pair is retained only when both are numeric `OV`, match `NumberOfFrames`, use an empty Basic Offset Table, and exactly index one in-bounds encapsulated fragment per frame; malformed or orphan tables are held locally; and
- a Siemens mosaic is held only when its necessary CSA image geometry cannot be rebuilt as the reviewed numeric-only form.

Privacy-unsafe objects are never uploaded speculatively. Classification affects downstream routing, not whether a privacy-clearable MR series is preserved: functional EPI is queued for conversion, while every other accepted MR kind is queued for archive verification. Scanner-specific processing improvements do not make the workstation re-upload the privacy-cleared source.

Enhanced MR Color is currently held locally rather than rewritten incorrectly: its mandatory ICC color profile needs a separately reviewed binary de-identification contract. Real World Value Mapping and opaque LUT/palette transforms are likewise held until their quantitative/display semantics can be retained atomically. A `SourceImageSequence` is retained only when the sequence is nonempty and every item is exactly a standard Referenced SOP Class UID plus a pseudonymized Referenced SOP Instance UID. The fail-closed rule still applies to richer conversion/derivation references, concatenations, nonempty Acquisition Context, nonempty Legacy converted-attribute containers, multi-coil element details, and unreviewed optional cardiac, respiratory, contrast, functional-MR, metabolite, or velocity-encoding macros. Current Enhanced and Legacy Converted Enhanced MR use separate mandatory functional-group and dimension contracts, and both require an explicit `BurnedInAnnotation = NO`. These are explicit reportable boundaries, not silent metadata deletion.

## Transfer boundary

The Worker allocates each archive object. Raw series archives are admitted up to the server’s 64 GiB object limit, and a single Enhanced MR multi-frame DICOM may use that full per-series boundary. There is one series per durable receipt. The client prepares and uploads one series at a time, then asks the Worker to complete and `HEAD`-verify its R2 object before deleting the local archive. That object remains provisional—with no scientific receipt or processing job—until the client re-verifies the entire folder and commits every series receipt. Provisional objects have a 90-day retention window, so total folder duration is independent of R2's seven-day multipart lifetime. Peak scratch therefore depends on the largest individual series—not on whether the selected export is 5 GB or several TB. Before writing, the client requires enough free scratch for the in-progress archive, the current immutable staged source instance, the sanitized instance, and 64 MiB of filesystem headroom; the conservative bound is the series source-byte total plus twice its largest instance plus that headroom. For every multipart part, the client computes a SHA-256 while reading that part and requests a short-lived signed URL bound to its exact key, part number, byte length, and hash. Bare ETags are checkpointed in owner-only SQLite state. Neither checkpoint nor receipt performs synchronous conversion.

A successful client run means all accepted, de-identified archives are durable and queued. The Sophont processor separately verifies every source archive. Functional EPI follows the `functional-epi-v1` conversion route; other MR kinds follow the `archive-verify-v1` preservation route. `neuro-sync status` can report that asynchronous processing state, but the workstation need not remain online.

The client reads each DICOM candidate once to compute the folder’s content-bound sync identity before any upload, once more while creating its series archive, and reads that archive once while uploading it. A fresh archive is not redundantly rehashed before upload because its streaming creator already recorded the exact digest and completed the post-write audit. A rerun reads DICOM headers and hashes candidate contents once to prove the completed or partial folder is still the same; README, log, `.DS_Store`, and other clearly non-DICOM files do not affect that identity. Successfully parsed DICOM plus DICOM-like unreadable files are included, so an incomplete scan export still fails closed. Memory use is bounded by one sanitized DICOM header plus streaming buffers; multi-frame Pixel Data is not loaded wholesale.

## Private state

State lives in the OS application-data directory (`ScalingNeuro/neuro-sync`) using SQLite WAL plus owner-only manifests and the current one-series archive staging area. If the default filesystem is small, choose scratch explicitly with `neuro-sync --state-dir /large/private/scratch /path/to/dicoms` or set `NEURO_SYNC_STATE_DIR`; reruns must use the same state directory to find their checkpoint. A conservative free-space check runs before each series, and an actual disk-full or quota error stops clearly as retryable instead of silently holding an eligible scan. Unix state is forced to owner-only file/directory modes. Windows state receives a protected current-account-only ACL, including a recursive repair of pre-existing descendants; a custom path on storage that cannot retain those ACLs fails clearly before use. Reports omit source paths, credentials, arbitrary DICOM values, and signed URLs. Device secrets are stored locally; the control plane stores only token hashes. The client never receives a reusable R2 credential.

The folder checkpoint is bound to site, project, consent policy, DICOM privacy policy, and client compatibility. A policy change forces safe re-preparation rather than continuing old bytes. Exact series identity supports duplicate reconciliation between authenticated devices that share a managed site/project; identity mismatches fail closed. Independently registered public workstations remain separate privacy domains and never conflict merely because their users entered the same lab name.

## Development

```sh
cargo +1.85.0 fmt --manifest-path Cargo.toml --all -- --check
cargo +1.85.0 clippy --locked --manifest-path Cargo.toml --all-targets --all-features -- -D warnings
cargo +1.85.0 test --locked --manifest-path Cargo.toml --all-features
```

Tests use synthetic non-PHI Part 10 fixtures and local HTTP/R2 simulations. They cover all accepted MR classifications, recursive rewriting, exclusions, deterministic archives, exact scanner-native Pixel Data preservation for functional and structural images, bounded streaming, folder-keyed continuation, multipart retry, lost responses, policy refresh, duplicate reconciliation, and receipt semantics.

Release archives contain `neuro-sync[.exe]`, licenses, onboarding, release metadata, and SPDX/CycloneDX SBOMs. Linux researchers receive one fully static x86-64 package rather than a distribution-specific choice. No local DICOM converter, GUI framework, Python runtime, or browser integration is packaged.
