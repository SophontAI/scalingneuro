# Contribute an MR DICOM folder

Scaling Neuro’s beta path is one folder and one command. Every supported MR Image series in that folder is considered: functional, structural, diffusion, ASL/perfusion, field-map, reference, localizer, and derived MR. You do not need to rename files, arrange BIDS, enter task labels, install Python or a DICOM converter, configure cloud credentials, or open a browser.

## Before starting

You need:

- your name, work email, institution, and lab/group name for one-time terminal registration;
- a completed DICOM export folder;
- temporary local space for the compressed metadata-deidentified MR archives; and
- authorization to contribute every eligible series, including scanner-native non-defaced structural/head pixels, under the displayed policy.

Registration creates a revocable identity for one workstation. The same lab may register additional workstations with the same contact information. Registration does not itself establish participant consent or data-use permission.

## Install once

```sh
# macOS or Linux
curl -fsSL https://scalingneuro.com/install.sh | sh
```

```powershell
# Windows PowerShell
irm https://scalingneuro.com/install.ps1 | iex
```

The readable installer downloads one SHA-256-verified package into your user account, adds `neuro-sync` to your user PATH, prints the next command, and returns to the shell. It does not launch setup. The package contains one executable; no administrator access, browser, Python runtime, Docker, cloud CLI, or local `dcm2niix` is required.

Current packages cover universal Intel/Apple-silicon macOS, Windows x64, and one fully static Linux x64 build with no glibc or other distribution-library requirement. They are terminal-only and have no X11, Wayland, GTK, or desktop-portal dependency. Institution-managed machines may still require local software approval.

## Run when the path is ready

```sh
neuro-sync /path/to/dicom-export
```

On first use, answer the short registration questions, read the policy summary, and confirm that you are authorized to contribute eligible scans from the folder already supplied. Everything happens in the terminal.

The client then:

1. inventories regular files without following symlinks, then discovers DICOMs with a bounded progress bar, speed, and ETA and groups them into series;
2. applies fail-closed format, integrity, and metadata-privacy gates to every MR Image series;
3. writes and audits metadata-deidentified DICOM copies while retaining scanner-native Pixel Data and useful acquisition metadata;
4. creates one compressed archive per accepted series; and
5. uploads missing bytes with a live percentage, speed, and ETA.

Success means Cloudflare R2 has durably received the exact archives and one independent server-verification job per series is queued. Sophont checks every archive, member hash, DICOM, metadata-privacy rule, and route. Confirmed functional EPI then receives pinned conversion and scientific validation; other MR finishes as a verified source archive. You may close the terminal and the workstation can go offline.

## Privacy and scientific content

The source folder is read-only and unchanged. The files uploaded are not the scanner’s untouched headers. Before upload, `neuro-sync` recursively pseudonymizes patient identity and DICOM UIDs; removes dates/times, accessions, clinical/admin text, institution/station/operator fields, descriptions/comments, source paths, overlays/graphics, unknown private tags, and unsafe private text/binary data; and reopens the results for a default-deny audit.

Pixel Data is copied byte-for-byte in its original transfer syntax. Standard scientific metadata required for decoding, geometry, MR timing/acquisition, scanner make/model/software, coils, matrices, and acceleration is retained. Pixel Data is not defaced, cropped, masked, resampled, or inspected for text. Structural and other high-resolution head images may contain recognizable facial anatomy and must be authorized and governed accordingly. See [the exact policy](dicom-deidentification-policy.md).

Secondary Capture, presentation objects, non-image DICOM, non-MR objects, malformed series, overlays/graphics, declared burned-in annotation, and unsupported privacy conditions stay local. Scanner manufacturer, model, software, and prior fixture status do not determine eligibility. Classic, Enhanced, and Legacy Converted Enhanced MR Image objects from any vendor use the same integrity and metadata-privacy gates. Unknown or malformed private metadata is removed; a Siemens mosaic is held only if the numeric-only CSA geometry needed to interpret its Pixel Data cannot be rebuilt safely. Functional conversion compatibility is reported later by the cluster and does not invalidate a durable source receipt.

## Interruption and continuation

If anything stops, use the same command again:

```sh
neuro-sync /the/same/dicom-export
```

There is no `resume` command. The folder path selects its compatible private checkpoint before rediscovery. Existing series archives, completed multipart parts, 90-day provisional R2 objects, and durable receipts are reused. A crash after R2 accepted a part but before its ETag was saved may resend only that same part number safely; a series already completed and `HEAD`-verified in R2 is never rebuilt or re-uploaded merely because the whole folder is still transferring.

Two or more workstations from one lab may register and upload independently; a repeated lab name or email never causes a conflict. Public registrations intentionally receive separate site-scoped pseudonym keys. When a managed lab explicitly enrolls multiple devices into one authenticated site/project, exact same-series races reconcile to the existing receipt and the losing temporary R2 prefix is purged. A different hash, pseudonymous identity, policy version, or withdrawal tombstone is never silently treated as a duplicate.

## Useful commands

```sh
# Guided fallback if no folder was supplied yet.
neuro-sync

# Optional explicit registration and upload forms.
neuro-sync register --email researcher@example.edu --name "Researcher Name" \
  --institution "Example University" --lab "Example Lab" \
  --accept-policy-version open-mri-1.0.0
neuro-sync upload /path/to/dicom-export

# For automation after both authorization decisions were confirmed out of band.
neuro-sync upload /path/to/dicom-export --confirm-authorized \
  --confirm-native-pixels-authorized

# Required only when accepting a newly advertised policy non-interactively:
neuro-sync upload /path/to/dicom-export --confirm-authorized \
  --confirm-native-pixels-authorized \
  --accept-policy-version open-mri-1.0.0

# Run discovery, privacy rewriting, audit, and archive generation without upload.
neuro-sync upload /path/to/dicom-export --dry-run

neuro-sync status
neuro-sync status --json
neuro-sync report RUN_ID --json
```

Reports contain pseudonymous IDs, counts, hashes, and stable accepted/held/excluded/processing codes. They omit source paths, source UIDs, arbitrary DICOM values, tokens, and signed URLs.

## Status meanings

- **received / queued:** every selected source archive is durable; workstation work is complete.
- **verifying / processing:** the cluster is independently auditing or converting one or more series.
- **archive verified:** a non-EPI source passed archive, DICOM, routing, and metadata-privacy verification; no EPI-derived output was requested.
- **processed:** a functional EPI source also produced validated NIfTI, minimized metadata, and a processing manifest.
- **processing failed, source retained:** a stable converter/scientific-compatibility error needs review; do not retransmit unless instructed.
- **processing failed, source purged:** the server found a privacy, archive-boundary, hash, or routing violation; the object was deleted and its identity tombstoned.
- **held:** the series never left the workstation because its DICOM form, integrity, or metadata-privacy condition was unsafe or unsupported.
- **already received:** the same series was archived earlier and no duplicate bytes were retained.

## Support

Share the operating system, `neuro-sync --version`, run/upload ID, scanner manufacturer/model if permitted, and stable report codes. Never paste names, MRNs, accessions, DICOM UIDs, descriptions, source paths, signed URLs, or source files into a support ticket.
