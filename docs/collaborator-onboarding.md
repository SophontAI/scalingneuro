# Share a functional MRI DICOM folder

You need a completed scanner export folder, temporary local space for one
compressed EPI series, and institutional authorization to contribute the
functional MRI data to the shared Scaling Neuro archive. The optional
review-first workflow also needs space for the deidentified DICOM copies.

Transient DICOM preparation uses operating-system local temporary storage by
default, while resumable checkpoints remain in the user data directory. On
clusters, `NEURO_SYNC_STAGING_DIR` can select another node-local scratch base.

## Install

```sh
curl -fsSL https://scalingneuro.org/install.sh | sh
```

```powershell
irm https://scalingneuro.org/install.ps1 | iex
```

The installer downloads one SHA-256-verified package into your user account and
returns to the shell. It does not require Python, Docker, a DICOM converter, a
cloud CLI, reusable cloud credentials, a browser, or administrator access.

## Run

```sh
neuro-sync /path/to/dicom-export
```

First use asks for your name, work email, institution, lab, policy acceptance,
and authorization in the terminal. The client then:

1. walks the folder recursively;
2. identifies functional EPI time series from DICOM metadata;
3. leaves every other series local;
4. rewrites and audits identifying metadata locally;
5. creates one compressed DICOM archive per selected series; and
6. uploads each archive with resumable, checksum-bound multipart transfers.

Success means the deidentified EPI DICOM archive is durably stored in R2. There
is no conversion or preprocessing phase after the receipt.

## Review before upload

```sh
neuro-sync prepare /path/to/dicom-export
```

This performs local selection, rewriting, and audit, then writes normal `.dcm`
files under `./dicom-export-review/series`. Nothing is uploaded and the scanner
export stays unchanged. Inspect or edit those files with the lab's usual DICOM
tools. The default output is `<source-folder>-review` in the current working
directory; pass `--output` to choose another local location.

Start with `series-index.tsv`, which maps each opaque series folder to its DICOM
Series Number, file count, core acquisition fields, classifier evidence, and QC
warnings. A `burned_in_annotation_not_declared` warning requires visual review
because the scanner did not explicitly assert `BurnedInAnnotation=NO`. To omit a
prepared series, move its complete series directory outside the review folder.

When the folder is approved:

```sh
neuro-sync upload ./dicom-export-review
```

The second command uses the reviewed DICOMs as they exist at that point. It
reruns functional EPI eligibility and the local privacy procedure, builds fresh
per-series archives, and then syncs them. Researcher edits are not rejected
merely because they differ from the initially prepared copies.
`series-index.tsv` and `preparation-report.json` describe the initial
preparation and are not updated after edits.

## Interruption

Rerun the same command:

```sh
neuro-sync /the/same/dicom-export
```

The folder and privacy context select the private checkpoint. Completed archives,
multipart parts, and receipts are reused.

## Useful explicit forms

```sh
neuro-sync register --email researcher@example.edu --name "Researcher Name" \
  --institution "Example University" --lab "Example Lab" \
  --accept-policy-version open-epi-3.0.0

neuro-sync upload /path/to/dicom-export --confirm-authorized
neuro-sync upload /path/to/dicom-export --dry-run
neuro-sync prepare /path/to/dicom-export
neuro-sync status --json
neuro-sync report RUN_ID --json
```

`--confirm-authorized` is for non-interactive use only after confirming that the
data owner and institution authorize public sharing under CC0 1.0.

## Archive access

Researchers can join the shared effort at
`https://scalingneuro.org/#access`. The form returns a personal archive token.
The archive contains deidentified functional EPI DICOMs. Each lab downloads and
preprocesses them independently.

## Support

Share the operating system, `neuro-sync --version`, pseudonymous run/upload ID,
and stable report codes. Never paste participant identity, accessions, source
DICOM UIDs, paths, source files, bearer tokens, or signed URLs into a support
request.
