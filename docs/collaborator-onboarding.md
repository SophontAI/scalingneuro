# Contribute an EPI folder

Scaling Neuro’s pilot flow is deliberately one-folder: enroll once, select the folder exported by the scanner, and leave the client running. You do not need to rename files, arrange BIDS folders, enter task labels, install Python, configure AWS, or understand DICOM tags.

## Before you start

You need:

- an invite code for the approved Scaling Neuro project;
- a folder containing a completed DICOM export;
- enough local free space for one converted EPI series plus resumable staging; and
- institutional authorization to contribute the scans under the project’s displayed consent/data-use policy.

An invite authorizes one workstation to access an institutionally pre-approved project; it is not evidence of participant consent and does not make an otherwise unauthorized scan upload permissible. The uploader remains responsible for confirming that the selected scans are covered by the institution’s IRB, consent, and data-use approvals. The client is a research data-transfer tool, not a clinical device or a substitute for those reviews.

Pilot releases support Apple Silicon and Intel macOS through one universal package, Windows x64, and Linux x64. Scanner compatibility comes from the pinned multi-vendor dcm2niix converter and fail-closed validation; no software can honestly guarantee every historical or malformed scanner export. Unsupported series stay local and appear in the report.

## Graphical flow

1. Download the release for your operating system and verify it using the adjacent `SHA256SUMS` file. Prefer signed builds. A file containing `UNSIGNED-PILOT` in its name is only for a named pilot and may trigger operating-system warnings.
2. Open `neuro-sync`. On first launch, paste the invite code and confirm the project and institutional authorization policy shown by the app.
3. Choose **Choose folder…**, select the top-level export folder, confirm that the scans are approved for the displayed project, and choose **Validate and upload**.
4. Leave the app open or close it normally. Relaunching resumes from the local checkpoint; it does not restart completed transfers. Large or multi-subject folders are split automatically into sequential, independently committed one-subject sessions.
5. Wait for **Committed**. Save the run report for the study record. It contains pseudonymous IDs, counts, hashes, QC codes, and held/excluded reasons—never patient names or raw DICOM values.

The source folder is read-only. DICOMs are neither modified nor uploaded. Only accepted functional EPI NIfTI/JSON bundles leave the machine. Structural scans, DWI, ASL, fieldmaps, SBRefs, localizers, derived images, and uncertain series stay local.

## Command-line flow

The graphical flow and CLI use the same state database and resume logic.

```bash
neuro-sync enroll YOUR_INVITE_CODE --server https://scalingneuro.com
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
- **Enrollment timed out or the app closed before confirmation:** reopen the client and submit the same invite. The owner-only pending operation is replayed with the same client-bound token, so a lost response does not spend the invite twice. Do not switch invites until the original result is recovered or an administrator revokes it.
- **The app was closed or the computer restarted:** reopen the same installation and choose Resume.
- **Resume says the enrollment context changed:** do not bypass it. The prepared run’s site, project, or contribution-policy version no longer matches the current enrollment; review the project policy and prepare a new authorized run.
- **A series is held:** keep the source folder unchanged and share the report. Scaling Neuro can add a compatibility fixture or classifier rule without receiving PHI.
- **Consent policy update required:** review and accept the displayed new policy before another upload. Existing committed data is not silently relabeled.
- **Duplicate bundle:** the archive already contains the same site/project/series content. The client should report the existing committed upload rather than send it again.
- **Object mismatch:** do not retry by manually changing files. Resume from the client so it can re-verify local hashes.

For a pilot issue, provide the operating system, `neuro-sync` version, run/upload ID, scanner manufacturer/model if permitted, and the report’s stable error/QC codes. Never provide names, MRNs, accession numbers, DICOM UIDs, raw descriptions, source paths, or source files unless a separate approved secure process has been arranged.
