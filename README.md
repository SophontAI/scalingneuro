# Scaling Neuro

Scaling Neuro is a shared archive for functional MRI DICOMs. A researcher points
`neuro-sync` at one scanner export folder. The client identifies functional
echo-planar imaging time series from DICOM metadata, removes identifying metadata
locally, and uploads one deidentified DICOM archive per EPI series to Cloudflare R2.

That is the complete data path. Scaling Neuro does not upload structural,
diffusion, field-map, localizer, secondary-capture, or ambiguous series. It does
not convert DICOMs to NIfTI, run preprocessing, or use Sophont compute.

Production is [scalingneuro.org](https://scalingneuro.org).
`scalingneuro.com` and `scalingneuro.pages.dev` are legacy hostnames that
redirect each request to the same path on the canonical domain.

## Use neuro-sync

```sh
# macOS or Linux
curl -fsSL https://scalingneuro.org/install.sh | sh
neuro-sync /path/to/dicom-export
```

```powershell
# Windows PowerShell
irm https://scalingneuro.org/install.ps1 | iex
neuro-sync "C:\path\to\dicom-export"
```

The first run registers the workstation and asks the researcher to confirm that
the selected functional MRI data are institutionally authorized for sharing.
Registration, selection, deidentification, progress, and the final receipt all
stay in the terminal.

Interrupted work is resumed by rerunning the same folder command. There is no
separate resume operation.

## Selection

A series is uploaded only when the local classifier finds:

- a supported MR Image SOP class;
- strong standard echo-planar evidence such as `ScanningSequence = EP`, an
  echo-planar pulse-sequence declaration, or EPI/BOLD values in Image Type or
  Sequence Name;
- repeated temporal structure from temporal positions, acquisition numbers,
  repeated slice positions, or multiple mosaic instances;
- plausible, consistent repetition and echo timing; and
- a complete local integrity and metadata-privacy contract.

Free-text protocol labels can support inspection but cannot select a series by
themselves. Scanner manufacturer, model, and software are provenance, not an
allowlist. Everything that is not a confirmed functional EPI time series stays
on the workstation.

## Deidentification and archive

The source folder is read-only and unchanged. For every selected EPI instance,
the client:

- preserves Pixel Data in the original transfer syntax;
- recursively rewrites nested DICOM sequences;
- replaces patient identity and referential UIDs with site-scoped pseudonyms;
- removes dates, direct identifiers, institutions, stations, operators,
  descriptions, comments, paths, overlays, graphics, and unsafe private data;
- retains a conservative acquisition, geometry, timing, and scanner-provenance
  allowlist; and
- reopens and audits the rewritten object before it can enter an archive.

Each accepted series becomes a deterministic `dicom.tar.zst` with a canonical
manifest and SHA-256 hashes. The client uploads through short-lived,
object-scoped multipart URLs and never receives reusable R2 credentials.

## Shared access

Researchers request access at
[scalingneuro.org/#access](https://scalingneuro.org/#access). A lab provides a
work email, institution, lab name, and a commitment to participate in the shared
functional MRI effort. The service returns a personal bearer token.

```sh
curl -H "Authorization: Bearer $SCALING_NEURO_ACCESS_TOKEN" \
  https://scalingneuro.org/v1/archive
```

The archive response lists available series and authenticated download routes.
Each download route redirects to a short-lived R2 GET URL:

```sh
curl -L -H "Authorization: Bearer $SCALING_NEURO_ACCESS_TOKEN" \
  "$(curl -fsS -H "Authorization: Bearer $SCALING_NEURO_ACCESS_TOKEN" \
    https://scalingneuro.org/v1/archive | jq -r '.series[0].download_url')" \
  -o series.dicom.tar.zst
```

Researchers verify the listed SHA-256, unpack the DICOM archive, and preprocess
it with their own tools and compute.

## Repository

| Path | Role |
|---|---|
| `client/` | Rust CLI, EPI selection, local DICOM rewriting, deterministic archives, checkpoints, multipart sync |
| `worker/` | Cloudflare Pages Worker, registration, upload receipts, archive access, D1 state, R2 URL signing |
| `worker/migrations/` | Ordered production D1 migrations |
| `schemas/` | Public request, archive, status, and error contracts |
| `docs/` | Contribution, deidentification, sync, API, onboarding, and release contracts |
| `installers/` | Dependency-free installer templates |
| `.github/workflows/` | Validation plus Cloudflare deployment, and manual client release |

## Local verification

Requirements are Rust 1.85, Node.js 22, and Python 3.13.

```sh
python3 -m pip install --requirement schemas/requirements.txt
python3 schemas/validate.py
node schemas/validate-ajv.mjs

npm ci --prefix worker
npm run check --prefix worker

cargo +1.85.0 fmt --manifest-path client/Cargo.toml --all -- --check
cargo +1.85.0 clippy --locked --manifest-path client/Cargo.toml --all-targets --all-features -- -D warnings
cargo +1.85.0 test --locked --manifest-path client/Cargo.toml --all-features

./scripts/test-installers.sh
./scripts/build-site.sh
node --check dist/_worker.js
```

Never place participant scans or populated secret files in this repository.
DICOM data, `.env*`, `.dev.vars`, build output, and local Cloudflare state are
ignored.

## Production bindings

The Pages Worker requires:

- D1 binding `DB`
- R2 binding `ARCHIVE`
- `SITE_KEY_ENCRYPTION_KEY_B64`
- `R2_ACCOUNT_ID`
- `R2_PARENT_ACCESS_KEY_ID`
- `R2_BUCKET_NAME`
- `R2_PARENT_SECRET_ACCESS_KEY`

The R2 token must be limited to Object Read & Write on the Scaling Neuro bucket.
Production configuration is checked before migrations and deployment.

## Contracts

- [EPI sync contract](docs/epi-ingestion-contract.md)
- [DICOM deidentification](docs/dicom-deidentification-policy.md)
- [Artifacts and APIs](docs/artifact-and-api-contracts.md)
- [Terminal onboarding](docs/collaborator-onboarding.md)
- [Client release](docs/client-release.md)

## License

Scaling Neuro is available under either
[Apache License 2.0](LICENSE-APACHE) or [MIT](LICENSE-MIT).
