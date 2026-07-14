# Contribute an EPI folder

Scaling Neuro’s open-beta flow is deliberately one-folder: register the lab once, select the folder exported by the scanner, and leave the client running. You do not need to rename files, arrange BIDS folders, enter task labels, install Python, configure AWS, or understand DICOM tags.

## Before you start

You need:

- your name, work email, institution, and lab or research-group name for the one-time registration;
- a folder containing a completed DICOM export;
- enough local free space for one converted EPI series plus resumable staging; and
- institutional authorization to contribute the scans under the project’s displayed consent/data-use policy.

Registration creates a private, revocable upload identity for one workstation and lab; it is not evidence of participant consent and does not make an otherwise unauthorized scan upload permissible. The uploader remains responsible for confirming that the selected scans are covered by the institution’s IRB, consent, and data-use approvals. The client is a research data-transfer tool, not a clinical device or a substitute for those reviews.

Pilot releases support Apple Silicon and Intel macOS through one universal package, Windows x64, and Linux x64. The Linux package requires glibc 2.28 or newer (for example, Ubuntu 20.04+, Debian 10+, or RHEL 8+), `libwayland-client.so.0`, and a working `xdg-desktop-portal` backend for the folder picker. Scanner compatibility comes from the pinned multi-vendor dcm2niix converter and fail-closed validation; no software can honestly guarantee every historical or malformed scanner export. Unsupported series stay local and appear in the report.

## Graphical flow

1. Download the release for your operating system and verify it using the adjacent `SHA256SUMS` file. Prefer signed builds. A beta file containing `UNSIGNED-PILOT` in its name may trigger operating-system warnings.
2. Open `neuro-sync`. On first launch, complete the short lab form and review the functional-EPI contribution policy shown by the app.
3. Choose **Choose folder…**, select the top-level export folder, confirm that the scans are approved for the displayed project, and choose **Validate and upload**.
4. Leave the app open or close it normally. Relaunching resumes from a compatible local checkpoint; it does not restart completed transfers. If a release tightened the privacy rules, choose **Revalidate with current privacy rules**: the same private source is converted again locally and must reproduce the original scan identities before upload. Large or multi-subject folders are split automatically into sequential, independently committed one-subject sessions.
5. Wait for **Committed**. Save the run report for the study record. It contains pseudonymous IDs, counts, hashes, QC codes, and held/excluded reasons—never patient names or raw DICOM values.

The source folder is read-only. DICOMs are neither modified nor uploaded. Only accepted functional EPI NIfTI/JSON bundles leave the machine. Structural scans, DWI, ASL, fieldmaps, SBRefs, localizers, derived images, and uncertain series stay local.

## Command-line flow

The graphical flow and CLI use the same state database and resume logic.

```bash
neuro-sync register --email researcher@example.edu --name "Researcher Name" \
  --institution "Example University" --lab "Example Neuroimaging Lab"
neuro-sync upload /path/to/dicom-export
neuro-sync status
neuro-sync report
```

Useful variants:

```bash
# Inspect classification/conversion output without transmitting data.
neuro-sync upload /path/to/dicom-export --dry-run

# Resume all unfinished work, or one known local run.
neuro-sync resume
neuro-sync resume RUN_ID

# Machine-readable status/report for lab automation.
neuro-sync status --json
neuro-sync report RUN_ID --json
```

Running `neuro-sync` with no arguments opens the loopback-only interface and native folder picker. The local interface binds only to `127.0.0.1`; it is not exposed to the lab network.

## What success looks like

A successful run shows:

- source files and DICOM series discovered;
- functional EPI series accepted, with held and excluded counts separated;
- local conversion and QC complete;
- bytes uploaded or resumed;
- an immutable manifest key and SHA-256; and
- final status `committed`.

`created` or `uploading` means the archive is not committed yet. `held` is a safe outcome, not a partial upload. Do not copy source DICOM headers into a support ticket; share the code-only report and client version.

## Recovery and support

- **Network interruption or expired 15-minute part URL:** choose Resume. Checkpointed multipart pieces are reused and the client requests a new checksum-bound URL only for the next missing part. A crash in the instant before an accepted ETag was saved may safely resend that same part number.
- **Registration timed out or the app closed before confirmation:** reopen the client and submit the same details. The owner-only pending operation is replayed with the same client-bound token, so a lost response cannot create duplicate lab or device records.
- **The app was closed or the computer restarted:** reopen the same installation and choose Resume.
- **The app says Privacy update required:** choose **Revalidate with current privacy rules**. The old prepared bytes are never uploaded; the source path stays private, and a changed or missing source fails locally so you can select the folder again.
- **Resume says the registration context changed:** do not bypass it. The prepared run’s site, project, or contribution-policy version no longer matches the current device; review the policy and prepare a new authorized run.
- **A series is held:** keep the source folder unchanged and share the report. Scaling Neuro can add a compatibility fixture or classifier rule without receiving PHI.
- **Consent policy update required:** review and accept the displayed new policy before another upload. Existing committed data is not silently relabeled.
- **Duplicate bundle:** an exact active archive match under the current metadata privacy policy is recorded as **already archived** and is not retransmitted. If the selected folder also contains new EPI series, those continue normally. If two workstations finish the same scan at once, the losing transfer is purged and reconciled automatically. A withdrawn tombstone, stale metadata-policy version, or any mismatch in subject/session/series/protocol identity or uncompressed NIfTI hash stops the run instead of being treated as a duplicate.
- **Object mismatch:** do not retry by manually changing files. Resume from the client so it can re-verify local hashes.

For a beta issue, provide the operating system, `neuro-sync` version, run/upload ID, scanner manufacturer/model if permitted, and the report’s stable error/QC codes. Never provide names, MRNs, accession numbers, DICOM UIDs, raw descriptions, source paths, or source files unless a separate approved secure process has been arranged.
