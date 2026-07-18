# Contribute an EPI folder

Scaling Neuro’s open-beta flow is deliberately one-folder: register the lab once, select the folder exported by the scanner, and leave the client running. You do not need to rename files, arrange BIDS folders, enter task labels, install Python, configure AWS, or understand DICOM tags.

## Before you start

You need:

- your name, work email, institution, and lab or research-group name for the one-time registration;
- a folder containing a completed DICOM export;
- enough local free space for one converted EPI series plus resumable staging; and
- institutional authorization to contribute the scans under the project’s displayed consent/data-use policy.

Registration creates a private, revocable upload identity for one workstation and lab; the same lab and contact may register each additional workstation separately. Registration is not evidence of participant consent and does not make an otherwise unauthorized scan upload permissible. The uploader remains responsible for confirming that the selected scans are covered by the institution’s IRB, consent, and data-use approvals. The client is a research data-transfer tool, not a clinical device or a substitute for those reviews.

Pilot releases support Apple Silicon and Intel macOS through one universal package, Windows x64, and Linux x64. The Linux package requires glibc 2.28 or newer (for example, Ubuntu 20.04+, Debian 10+, or RHEL 8+); it does not require X11, Wayland, a desktop portal, or a browser. Scanner compatibility comes from the pinned multi-vendor dcm2niix converter and fail-closed validation; no software can honestly guarantee every historical or malformed scanner export. Unsupported series stay local and appear in the report.

## Install once

Paste the command for your computer. The installer runs without administrator access, downloads the release bundle and pinned converter, verifies the package SHA-256 before installing, adds `neuro-sync` to your user PATH, and returns control to the shell. It prints the exact command to run when your DICOM folder path is ready.

```bash
# macOS or Linux
curl -fsSL https://scalingneuro.com/install.sh | sh
```

```powershell
# Windows PowerShell
irm https://scalingneuro.com/install.ps1 | iex
```

Both scripts are readable at those URLs before use and do not send installer telemetry. Terminal installation does not override institutional endpoint-security rules; those environments may still require locally approved software.

## Guided terminal flow

1. After installation, find or copy the top-level DICOM export-folder path. The installer has returned control to the shell and has not started setup.
2. Run the exact command printed by the installer. On first launch, answer the short lab-registration prompts and review the functional-EPI contribution policy summary shown in the terminal. No browser or web form is opened.
3. Type, paste, or drag the export-folder path into the terminal and confirm that the scans are approved for the displayed project and policy.
4. Leave the command running. Large or multi-subject folders are split automatically into sequential, independently committed one-subject sessions. If the connection or process is interrupted, run `neuro-sync resume`; compatible prepared work is checkpointed locally.
5. Wait for status `committed`. Save the run report for the study record. It contains pseudonymous IDs, counts, hashes, QC codes, and held/excluded reasons—never patient names or raw DICOM values.

The source folder is read-only. DICOMs are neither modified nor uploaded. Only accepted functional EPI NIfTI/JSON bundles leave the machine. Structural scans, DWI, ASL, fieldmaps, SBRefs, localizers, derived images, and uncertain series stay local.

## Explicit command flow

The guided and explicit terminal flows use the same private state database and resume logic.

```bash
neuro-sync register --email researcher@example.edu --name "Researcher Name" \
  --institution "Example University" --lab "Example Neuroimaging Lab" --accept-policy
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

# Non-interactive transmission after authorization was confirmed out of band.
neuro-sync upload /path/to/dicom-export --confirm-authorized
```

Running `neuro-sync` with no arguments starts the complete guided flow in the current terminal. It does not start a local web server or require a graphical session.

During a large or network-mounted export, the client prints live file, DICOM, series, conversion, multipart-transfer, and server archive-verification progress. Final verification is checkpointed per NIfTI/sidecar pair; an interruption resumes from the last verified pair without retransmitting completed files. `Ctrl+C` is safe during local validation: no data reaches R2 until a privacy-checked bundle has been prepared, and interrupted work can be continued with `neuro-sync resume`.

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

- **Network interruption or expired 15-minute part URL:** run `neuro-sync resume`. Checkpointed multipart pieces are reused and the client requests a new checksum-bound URL only for the next missing part. A crash in the instant before an accepted ETag was saved may safely resend that same part number.
- **Final verification is still running or reports `CONFLICT`:** install the current client and run `neuro-sync resume`. Uploaded parts and verified NIfTI/sidecar pairs are reused; do not select and upload the folder again.
- **Registration timed out or the command stopped before confirmation:** run `neuro-sync` and submit the same details. The owner-only pending operation is replayed with the same client-bound token, so a lost response cannot create duplicate lab or device records.
- **The client was closed or the computer restarted:** run `neuro-sync resume`.
- **Privacy rules changed:** prepare the unchanged private source again with the current client. Old prepared bytes are never uploaded, and a changed or missing source fails locally.
- **Resume says the registration context changed:** do not bypass it. The prepared run’s site, project, or contribution-policy version no longer matches the current device; review the policy and prepare a new authorized run.
- **A series is held:** keep the source folder unchanged and share the report. Scaling Neuro can add a compatibility fixture or classifier rule without receiving PHI.
- **Consent policy update required:** review and accept the displayed new policy before another upload. Existing committed data is not silently relabeled.
- **Duplicate bundle:** an exact active archive match under the current metadata privacy policy is recorded as **already archived** and is not retransmitted. If the selected folder also contains new EPI series, those continue normally. If two workstations finish the same scan at once, the losing transfer is purged and reconciled automatically. A withdrawn tombstone, stale metadata-policy version, or any mismatch in subject/session/series/protocol identity or uncompressed NIfTI hash stops the run instead of being treated as a duplicate.
- **Object mismatch:** do not retry by manually changing files. Resume from the client so it can re-verify local hashes.

For a beta issue, provide the operating system, `neuro-sync` version, run/upload ID, scanner manufacturer/model if permitted, and the report’s stable error/QC codes. Never provide names, MRNs, accession numbers, DICOM UIDs, raw descriptions, source paths, or source files unless a separate approved secure process has been arranged.
