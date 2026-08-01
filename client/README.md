# neuro-sync

`neuro-sync` is the Scaling Neuro workstation client. Give it one completed
DICOM export folder. It finds functional EPI time series and deidentifies their
DICOM metadata locally. It can write deterministic per-series archives and sync
immediately, or first expose the deidentified DICOMs in an editable local review
folder.

It leaves structural, diffusion, perfusion, field-map, reference, localizer,
derived, secondary-capture, malformed, and ambiguous series on the workstation.
It does not convert DICOMs or run preprocessing.

```sh
curl -fsSL https://scalingneuro.org/install.sh | sh
neuro-sync /path/to/dicom-export
```

```powershell
irm https://scalingneuro.org/install.ps1 | iex
neuro-sync "C:\path\to\dicom-export"
```

Rerun the same folder command after any interruption. The folder and current
privacy contract select the existing private checkpoint.

Review before uploading:

```sh
neuro-sync prepare /path/to/dicoms
# Inspect or edit ./dicoms-review/series.
neuro-sync upload ./dicoms-review
```

Preparation uploads nothing. Its default output is `<source-folder>-review` in
the current working directory; pass `--output` to choose another local location.
The later upload uses the current contents of the review folder, reruns EPI
classification and deidentification, and creates fresh archives from those
files. `preparation-report.json` is an initial snapshot and is not updated after
researcher edits.

Useful explicit commands:

```sh
neuro-sync help
neuro-sync register --email researcher@example.edu --name "Researcher Name" \
  --institution "Example University" --lab "Example Lab" \
  --accept-policy-version open-epi-4.0.0
neuro-sync upload /path/to/dicoms --confirm-authorized
neuro-sync upload /path/to/dicoms --dry-run
neuro-sync status --json
neuro-sync report RUN_ID --json
```

## Local boundary

The source folder is unchanged. The client recursively rewrites selected DICOMs,
pseudonymizes identity and UIDs, removes dates, administrative and clinical
fields, institution and operator identity, free text, paths, overlays, graphics,
and unreviewed private data, then reopens and audits the result.

Pixel Data stays in the original transfer syntax. The client retains a
conservative allowlist for image decoding, geometry, MR timing, EPI acquisition,
and bounded scanner provenance. Declared burned-in annotation, overlays,
graphics, unsupported SOP classes, malformed Pixel Data, and unsafe metadata
fail closed.

## Transfer

One `dicom.tar.zst` is created per functional EPI series. Multipart grants are
short-lived and bound to the exact key, part number, length, and SHA-256. The
client stores bare ETags in owner-only local SQLite state and never receives a
reusable R2 credential.

A successful run means every selected EPI series has a durable R2 receipt.
Each successfully received archive is restricted for seven days and then
irrevocably dedicated under CC0 1.0 unless cancelled during staging. Use
`--confirm-authorized` only when the specific data permits irrevocable sharing,
commercial reuse by any party, and public-domain redistribution without
conditions.

## Development

```sh
cargo +1.85.0 fmt --manifest-path Cargo.toml --all -- --check
cargo +1.85.0 clippy --locked --manifest-path Cargo.toml --all-targets --all-features -- -D warnings
cargo +1.85.0 test --locked --manifest-path Cargo.toml --all-features
```
