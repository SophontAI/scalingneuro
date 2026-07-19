# neuro-sync

`neuro-sync` is the single-executable Scaling Neuro workstation client. It accepts a completed DICOM export folder, identifies functional EPI conservatively, writes privacy-cleared DICOM copies into deterministic per-series archives, and uploads them with automatic continuation, progress, speed, and ETA. It does not convert scans to NIfTI locally.

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
  --institution "Example University" --lab "Example Lab" --accept-policy
neuro-sync upload /path/to/dicoms
neuro-sync upload /path/to/dicoms --confirm-authorized
neuro-sync upload /path/to/dicoms --dry-run
neuro-sync status --json
neuro-sync report RUN_ID --json
```

## Local privacy boundary

The source folder is never modified. Before any copy leaves the workstation, the client:

- requires strong functional-EPI evidence and rejects/holds structural, diffusion, field-map, SBRef, derived, secondary-capture, ambiguous, or privacy-unsafe series;
- recursively allowlists DICOM attributes, pseudonymizes identity and UIDs consistently, removes dates/times and unsafe text/private data, rebuilds Part 10 metadata, and audits the rewritten object;
- byte-copies Pixel Data in its original transfer syntax without decoding or recompression; and
- writes one deterministic `dicom.tar.zst` containing ordinal DICOM filenames plus `manifest.json` and hashes.

The policy is [../docs/dicom-deidentification-policy.md](../docs/dicom-deidentification-policy.md).

The `0.3.1` intake boundary is vendor-neutral:

- manufacturer, model, software, and prior fixture status are provenance—not upload eligibility;
- classic, Enhanced, and Legacy Converted Enhanced MR use the same standard-DICOM EPI, temporal, timing, and privacy predicates;
- recognized safe make/model/software fields and bounded standard acquisition metadata are retained, while unknown/malformed private metadata is default-dropped;
- Extended Offset Table metadata may be dropped because Pixel Data is copied and audited as one exact byte span; and
- a Siemens mosaic is held only when its necessary CSA image geometry cannot be rebuilt as the reviewed numeric-only form.

Privacy-unsafe objects are never uploaded speculatively. Scanner-specific conversion fidelity is evaluated asynchronously on the cluster and does not make the workstation re-upload data; the privacy-cleared source remains available for improved processors.

Release-equivalence checks convert raw and privacy-cleared validation fixtures with the same pinned converter. The Siemens native/RLE paths produced the same voxel SHA-256 `7934115b9a6bba2d72f4f60bcfadc3772c3d6de8a286bb542eedb1d322c89c85`; the Philips classic path produced `13eab53cb50d0dfa00d011b8106a9cc9123f0596330454b307bda0d1fb5fc429`. Dimensions, affine, datatype, scaling, TR/TE, and the vendor-specific preprocessing metadata listed above were compared separately rather than inferred from the voxel hash.

## Transfer boundary

The Worker allocates each archive object. Raw series archives are admitted up to the server’s 64 GiB object limit and transparently grouped into same-subject receipts of at most eight series and 250 GiB; the legacy NIfTI route retains its separate 32 GiB transaction limit. For every multipart part, the client computes a SHA-256 while reading that part and requests a short-lived signed URL bound to its exact key, part number, byte length, and hash. Bare ETags are checkpointed in owner-only SQLite state. Completion is multipart completion plus authoritative R2 metadata receipt—not synchronous conversion.

A successful client run means all selected, de-identified archives are durable and queued. The Sophont processor separately verifies source hashes, runs pinned `dcm2niix`, validates functional NIfTI/metadata, and publishes derived artifacts. `neuro-sync status` can report that asynchronous processing state, but the workstation need not remain online.

The expected data-read path for a new series is one source pixel pass while creating the archive and one archive pass while uploading it. A fresh archive is not redundantly rehashed before that upload because its streaming creator already recorded the exact digest and completed the post-write audit. A checkpoint reused by a later process is rehashed once before any resumed transfer. Discovery reads headers only. Memory use is bounded by one sanitized DICOM header plus streaming buffers; multi-frame Pixel Data is not loaded wholesale.

## Private state

State lives in the OS application-data directory (`ScalingNeuro/neuro-sync`) using SQLite WAL plus owner-only manifests/archive staging. Unix state is forced to owner-only file/directory modes. Windows state receives a protected current-account-only ACL, including a recursive repair of pre-existing descendants; a custom path on storage that cannot retain those ACLs fails clearly before use. Reports omit source paths, credentials, arbitrary DICOM values, and signed URLs. Device secrets are stored locally; the control plane stores only token hashes. The client never receives a reusable R2 credential.

The folder checkpoint is bound to site, project, consent policy, DICOM privacy policy, and client compatibility. A policy change forces safe re-preparation rather than continuing old bytes. Exact series identity supports duplicate reconciliation between authenticated devices that share a managed site/project; identity mismatches fail closed. Independently registered public workstations remain separate privacy domains and never conflict merely because their users entered the same lab name.

## Development

```sh
cargo +1.85.0 fmt --manifest-path Cargo.toml --all -- --check
cargo +1.85.0 clippy --locked --manifest-path Cargo.toml --all-targets --all-features -- -D warnings
cargo +1.85.0 test --locked --manifest-path Cargo.toml --all-features
```

Tests use synthetic non-PHI Part 10 fixtures and local HTTP/R2 simulations. They cover recursive rewriting, exclusions, deterministic archives, exact Pixel Data preservation, bounded streaming, folder-keyed continuation, multipart retry, lost responses, duplicate reconciliation, and receipt semantics.

Release archives contain `neuro-sync[.exe]`, licenses, onboarding, release metadata, and SPDX/CycloneDX SBOMs. Linux researchers receive one fully static x86-64 package rather than a distribution-specific choice. No local DICOM converter, GUI framework, Python runtime, or browser integration is packaged.
