# Scaling Neuro

Scaling Neuro is a terminal-first path from a scanner export to a reusable MR research archive. In the `0.4.0` beta, a researcher points one command at a completed DICOM folder; `neuro-sync` archives every supported MR Image series that passes local integrity and metadata-deidentification gates, uploads resumable per-series objects to Cloudflare R2, and returns a durable receipt. Sophont then independently verifies every archive. Confirmed functional EPI additionally receives pinned conversion and scientific QC; structural, diffusion, field-map, reference, localizer, and other MR remain verified scanner-native source archives.

It is open to any lab. Registration records the lab and creates a revocable identity for one workstation; the same lab may register any number of workstations. Scaling Neuro is a research transfer system, not a clinical device or a substitute for consent, IRB, or data-use review.

## Researcher experience

Install one executable—no Python, Docker, AWS CLI, Cloudflare key, local converter, browser, or administrator access:

```sh
# macOS or Linux
curl -fsSL https://scalingneuro.com/install.sh | sh
```

```powershell
# Windows PowerShell
irm https://scalingneuro.com/install.ps1 | iex
```

The installer returns to the shell. After finding the export path, run:

```sh
neuro-sync /path/to/dicom-export
```

On first use, registration and the authorization confirmation happen in that terminal. During discovery, privacy rewriting, archive creation, and transfer, the client shows bytes, speed, percentage, and ETA. Once R2 confirms the exact object length and multipart receipt, the workstation is done; cluster processing does not keep it connected.

If the process or network stops, rerun the identical folder command. The folder selects the compatible local checkpoint automatically, so completed archives and multipart parts are reused. There is no separate `resume` command.

## What is preserved

The canonical ingest object is one deterministic `dicom.tar.zst` per accepted MR Image series. It contains newly written DICOM Part 10 instances plus a canonical manifest. The source directory is read-only and unchanged.

For each uploaded instance, `neuro-sync`:

- copies Pixel Data byte-for-byte in the original transfer syntax;
- recursively rewrites nested sequences, not only the top-level header;
- replaces patient identity and all referential UIDs with consistent site-scoped pseudonyms;
- removes calendar dates/times, administrative/clinical identifiers, institutions, stations, operators, descriptions, comments, overlays, graphics, unknown private data, and unsafe private text or binary values;
- preserves a conservative scientific allowlist covering pixel decoding, geometry, timing, MR acquisition, scanner make/model/software, coils, acceleration, and referenced-image structure; and
- reopens and audits the result before it can enter an upload archive.

The archive manifest records pseudonymous subject/session/series/protocol identities, classifier evidence, policy/version provenance, an ordered instance inventory, and SHA-256 hashes. It never contains source paths, filenames, source UIDs, or free-text descriptions.

This is not a claim that scanner-native pixels are anonymous. Header de-identification does not remove recognizable facial anatomy from high-resolution head images: the current client does not deface, crop, mask, or alter Pixel Data. The terminal policy requires explicit institutional authorization for governed storage of native MR pixels, and every manifest records that recognizable visual features may be present. The executable behavior is documented in [the DICOM metadata policy](docs/dicom-deidentification-policy.md), informed by DICOM PS3.15.

## All-MR intake and EPI routing

Intake is evidence-based and independent of scanner manufacturer, model, or software version. Supported classic, Enhanced, and Legacy Converted Enhanced MR Image series are uploaded when their identities, Pixel Data boundaries, sizes, and metadata-privacy gates pass. Functional EPI, structural T1/T2, diffusion, ASL/perfusion, field maps, SBRefs, localizers, derived MR, and other MR receive explicit purpose codes. Secondary Capture, non-image DICOM documents, non-MR objects, malformed series, overlays/graphics, declared burned-in annotation, and unsupported privacy conditions stay local with code-only reasons.

Enhanced multi-frame support is standards-bounded rather than a blanket sequence allowlist. Current Enhanced MR and Legacy Converted Enhanced MR have separate mandatory functional-group contracts; both require explicit `BurnedInAnnotation = NO`. A bounded `SourceImageSequence` containing only standard SOP Class UIDs and pseudonymized SOP Instance UIDs is preserved. Concatenations, nonempty Acquisition Context or Legacy converted-attribute containers, richer conversion/derivation provenance, Real World Value Mapping/LUT semantics, and unreviewed optional functional-group macros currently fail closed instead of being partially rewritten.

Purpose controls processing, not receipt. Only a series with strong standard echo-planar and temporal evidence enters `functional-epi-v1`; the server repeats that classification before conversion. All other accepted MR enters `archive-verify-v1` and never reaches the EPI converter. Safe, bounded equipment provenance is retained without a manufacturer/model allowlist; missing or privacy-unsafe provenance is omitted rather than becoming an eligibility failure. Known safe standard acquisition metadata is retained; unknown and malformed private metadata is dropped. Siemens mosaic Pixel Data still requires a successfully rebuilt numeric-only CSA image header because that geometry is needed to interpret the mosaic. The exact measured conversion matrix remains in [Vendor QA](docs/vendor-qa.md).

## Architecture

```mermaid
flowchart LR
  A["neuro-sync folder"] --> B["Discover every supported MR Image series"]
  B --> C["Rewrite and audit DICOM metadata locally"]
  C --> D["Deterministic series archive"]
  D --> E["Checksum-bound resumable upload"]
  E --> F["Durable R2 receipt"]
  F --> G["One verification job per series"]
  G --> H["Sophont archive and privacy audit"]
  H --> I{ "Functional EPI?" }
  I -->|yes| J["Pinned dcm2niix and scientific QC"]
  I -->|no| K["Verified scanner-native source archive"]
  J --> L["Derived NIfTI, sidecar, processing manifest"]
```

The client never receives a reusable R2 credential. The Worker creates a multipart object and issues a short-lived `UploadPart` URL bound to an exact key, multipart ID, part number, content length, and SHA-256 header. Returned ETags are checkpointed locally. After each series transfer, the Worker completes that multipart object and verifies its authoritative R2 `HEAD` before the client removes its local staging archive. This provisional object creates no receipt or processing job. Only after the final whole-folder stability check does a second metadata-only call atomically create the scientific receipt and queue work. Provisional objects are retained for 90 days, so a multi-terabyte folder is not coupled to R2's seven-day multipart lifetime.

Receipt and server verification are separate states. A successful upload means the locally metadata-deidentified DICOM archive is durable and queued. A cluster consumer later receives a scoped GET capability, verifies the whole archive and every member, repeats the metadata-privacy and routing audit, and completes non-EPI work as `archive verified`. For functional EPI it additionally runs pinned `dcm2niix`, validates a native-space 4D NIfTI and minimized sidecar, publishes deterministic derived outputs, and commits the catalog under a lease. A stale processor cannot publish after losing that lease. A terminal server finding that the input violates privacy, archive boundaries, hashes, or routing deletes that source object and leaves an auditable tombstone; converter/scientific compatibility failures retain the governed source for review.

Stable site/project/series-archive identity makes creation and completion idempotent. Raw series archives may be up to 64 GiB; the production client uses one durable receipt per series, so an interrupted upload or independently proven integrity repair resumes exactly that scientific unit without coupling it to neighboring series. Larger folders are streamed through those singleton receipts one series at a time. If two authenticated devices in the same managed site/project upload the same eligible series concurrently, the exact winner is reused and the losing prefix is purged; a semantic mismatch or withdrawal tombstone is never treated as a duplicate success. Open self-service workstation registrations are intentionally independent privacy domains, so any number of machines from one lab can register without sharing pseudonym keys or trusting self-asserted lab names. Public workstations have no cumulative upload allowance; bounded object sizes and multipart requests remain enforced as operational safety limits.

## Command line

```sh
# Normal path; first use registers in the terminal.
neuro-sync /path/to/dicom-export

# Optional explicit setup for managed workstations.
neuro-sync register --email researcher@example.edu --name "Researcher Name" \
  --institution "Example University" --lab "Example Neuroimaging Lab" \
  --accept-policy-version open-mri-1.0.0

# Explicit form and non-interactive authorization. Both flags are required
# because 0.4 uploads scanner-native, non-defaced MR pixels.
neuro-sync upload /path/to/dicom-export
neuro-sync upload /path/to/dicom-export --confirm-authorized \
  --confirm-native-pixels-authorized

# Required only when accepting a newly advertised policy non-interactively:
neuro-sync upload /path/to/dicom-export --confirm-authorized \
  --confirm-native-pixels-authorized \
  --accept-policy-version open-mri-1.0.0

# Classify, rewrite, audit, and archive locally without contacting the service.
neuro-sync upload /path/to/dicom-export --dry-run

neuro-sync status --json
neuro-sync report RUN_ID --json
```

Running `neuro-sync` without arguments remains a guided terminal fallback. `neuro-sync run` remains an alias for `upload`. Reports contain only pseudonymous identifiers, counts, hashes, and stable status/QC codes—never local paths or arbitrary DICOM values.

## Repository

| Path | Role |
|---|---|
| `client/` | Rust terminal client: registration, DICOM selection/rewrite, deterministic archive, checkpoints, progress/ETA, multipart upload |
| `worker/` | Cloudflare control plane: D1 state, R2 receipt, idempotency, queue leases, scoped processor capabilities |
| `worker/migrations/` | Ordered production D1 migrations and legacy-upload backfill |
| `processor/` | Pinned cluster consumer, DICOM/archive validation, conversion, derived-artifact validation, Slurm/Enroot deployment |
| `schemas/` | Versioned public request, artifact, status, and error contracts |
| `docs/` | De-identification, ingest, onboarding, API, release, and vendor-QA contracts |
| `installers/` | Dependency-free terminal installer templates |
| `scripts/` | Installer rendering/tests and explicit Pages build allowlist |
| `.github/workflows/` | Client, Worker, processor, schema, release, migration, and deployment gates |

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
python3 -m pip install --require-hashes -r scripts/vendor-dicom-qa-requirements.txt
python3 scripts/test_vendor_dicom_qa.py
python3 scripts/vendor_dicom_qa.py --self-test

python3 -m venv processor/.venv
processor/.venv/bin/pip install --require-hashes -r processor/requirements.lock
PYTHONPATH=processor processor/.venv/bin/python -m unittest discover -v -s processor/tests

./scripts/test-installers.sh
./scripts/build-site.sh
node --check dist/_worker.js
```

Never place participant scans or populated secret files in this repository. DICOM/NIfTI data, `.env*`, `.dev.vars`, processor work directories, build output, and local Cloudflare state are ignored.

## Production

Production is published as one source-aligned unit. A `main` push deploys only when the latest non-prerelease client tag points to that exact commit and its version matches the client source. A new release stays a private GitHub draft while the workflow builds every platform package and applies migrations. It then deploys the new Worker/site with byte-for-byte verified copies of the currently public downloads, proves that exact preserved client can still register, proves the newly built candidate client and non-PHI fixture through production and Sophont, and only afterward cuts over the new index, installers, and packages. Because D1 migrations and accepted all-MR state are forward-only, any post-phase-one failure redeploys the forward-compatible candidate backend with the byte-for-byte preserved public downloads; it never restores a pre-v2 Worker over migrated state. A failure before the public-download capture or first deployment leaves production untouched. Only a fully verified release is made public. Production requires D1 `DB`, R2 `ARCHIVE`, and these secrets:

- `ADMIN_API_TOKEN`
- `SITE_KEY_ENCRYPTION_KEY_B64`
- `R2_PARENT_SECRET_ACCESS_KEY`
- `PROCESSOR_API_TOKEN`

Before either release path deploys, the production gate compares the Pages dashboard with the exact D1 database ID, R2 bucket, R2 account/access-key IDs, and TTL values committed in `worker/wrangler.jsonc`; it also requires all four secrets to remain encrypted and preview deployments to have no production bindings. CI exercises both acceptance and fail-closed mismatch cases. The tracked hourly cleanup workflow calls the authenticated admin route so expired, withdrawn, and rejected temporary inputs continue to be purged even though Pages itself has no Cron Trigger.

The R2 parent token must be dedicated to Object Read & Write on only the Scaling Neuro bucket. The processor token is stored separately as a mode-`0600` file on shared Sophont storage; it receives only short-lived object-scoped capabilities. See [processor/README.md](processor/README.md) for the pinned container and bottom-priority Slurm deployment.

Client packages are built from the current `main` commit for universal macOS, Windows x64, and one fully static Linux x64 target with no distribution-library choice. Each is a single client executable plus licenses, release metadata, and SBOMs. Ordinary CI and the release workflow verify the executable and static Linux linkage; release gates also verify signing state, package hash, installer behavior, tamper rejection, and protected Windows private-state ACLs. See [the release contract](docs/client-release.md).

## Remaining broad-adoption gates

- Expand fixture-certified conversion/QC across GE, Canon/Toshiba, United Imaging, Bruker, additional Siemens/Philips families, PACS rewrites, and transfer syntaxes without reintroducing an intake whitelist.
- Add narrowly rebuilt vendor-private scientific fields only after hostile/property tests demonstrate that no opaque private block or unbounded value can enter an archive.
- Extend Enhanced MR fixture coverage across shared/per-frame dimension-index layouts and compressed multi-frame exports.
- Run clean-machine installation, interruption, automatic continuation, and receipt tests on each promised OS and representative institution-managed workstations.
- Independently inspect every release’s stored rewritten DICOM, derived sidecar/manifest, metadata retention, PHI absence, native geometry, hashes, withdrawal, and cleanup.
- Add governed discovery/access, compatibility dashboards, and downstream training caches without weakening the immutable source archive.
- Validate a separate derived structural defacing route with quantitative brain-preservation and face-reidentification QC before any anatomy is eligible for broader release.

## Contracts

- [MR ingest and identity](docs/mr-ingestion-contract.md)
- [Functional EPI processing](docs/epi-ingestion-contract.md)
- [DICOM de-identification](docs/dicom-deidentification-policy.md)
- [Artifacts and APIs](docs/artifact-and-api-contracts.md)
- [Terminal onboarding](docs/collaborator-onboarding.md)
- [Client release](docs/client-release.md)
- [Vendor QA](docs/vendor-qa.md)
- [Scaling Neuro initiative brief](Scaling%20Up%20Neuroimaging%20Data%20for%20Foundation%20Models.md)
- [Web-scale archive strategy](Creating%20web-scale%20neuroimaging%20database.md)

## License

Scaling Neuro is available under either [Apache License 2.0](LICENSE-APACHE) or [MIT](LICENSE-MIT). Server-side third-party components retain their own notices.
