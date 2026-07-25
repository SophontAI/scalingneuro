# neuro-sync

`neuro-sync` is the Scaling Neuro workstation client. Give it one completed
DICOM export folder. It finds functional EPI time series, deidentifies their
DICOM metadata locally, writes deterministic per-series archives, and syncs them
to the shared R2 archive.

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

Useful explicit commands:

```sh
neuro-sync register --email researcher@example.edu --name "Researcher Name" \
  --institution "Example University" --lab "Example Lab" \
  --accept-policy-version open-epi-2.0.0
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

## Development

```sh
cargo +1.85.0 fmt --manifest-path Cargo.toml --all -- --check
cargo +1.85.0 clippy --locked --manifest-path Cargo.toml --all-targets --all-features -- -D warnings
cargo +1.85.0 test --locked --manifest-path Cargo.toml --all-features
```
