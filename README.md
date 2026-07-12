# Scaling Neuro — landing site

A single-page site outlining the open neuroimaging scaling initiative described in
[*Scaling Up Neuroimaging Data for Foundation Models*](./Scaling%20Up%20Neuroimaging%20Data%20for%20Foundation%20Models.md)
and [*Creating a web-scale neuroimaging database*](./Creating%20a%20web-scale%20neuroimaging%20database.md).

It is a static concept prototype with no framework compilation or package install. Production
uses a small allowlist-packaging script. The fixed visual system uses muted lavender-gray,
softened navy, and dusty coral; low-glare editorial surfaces with a faint graph-paper texture,
plus deep-navy imaging and terminal workspaces for technical focus.

The landing page is intentionally reduced to four surfaces: the scaling argument, contribution
flow, scan explorer, and one-folder script. The hero compares the estimated 32.2k hours of fMRI
in freely accessible archives with a transparent planning scenario: 25 centers acquiring 25
hours per week would produce 32.5k hours in one year, before consent and privacy filtering. A
second, CortexMAE-inspired plot shows the public-data frontier and the conceptual frontier that
coordinated institutional sharing could unlock. The contribution flow also makes the exchange
explicit: labs share approved scans without unpublished study annotations and receive access to
the resulting commons.

## Run locally

```bash
python3 -m http.server 4173
# open http://localhost:4173
```

(Three.js loads from a CDN at runtime, so the 3D viewer needs a network connection.)

## Production deployment

Every push to `main` builds an explicit allowlist of public assets and deploys it to the
`scalingneuro` Cloudflare Pages project through GitHub Actions. The production URL is
<https://scalingneuro.com>; requests to `scalingneuro.pages.dev` redirect to the canonical
domain. `version.json` records the deployed Git commit for release verification.

The GitHub repository must provide the Actions secrets `CLOUDFLARE_API_TOKEN` and
`CLOUDFLARE_ACCOUNT_ID`. The API token needs Cloudflare Pages edit access for the Sophont
Cloudflare account.

## Deployment safety

Raw NIfTI inputs are local-only and ignored by Git. The public deployment excludes
`sub-1001_T1w.nii.gz` because its provenance, consent, and defacing status have not been
established. On public hosts, the archive automatically hides that row and serves synthetic
previews only.

## Files

| File | Purpose |
|------|---------|
| `index.html` | Page structure, hero bar comparison, and SVG sprite |
| `styles.css` | All styling and the design system (CSS variables at the top) |
| `app.js` | Archive filtering, browser-side NIfTI-1 decoding, and the 3D viewer (Three.js) |
| `404.html` | Standalone not-found page matching the production visual system |
| `_worker.js` | Canonical redirect from `scalingneuro.pages.dev` to `scalingneuro.com` |
| `scripts/build-site.sh` | Builds the explicit, safe production asset bundle |
| `.github/workflows/deploy-production.yml` | Deploys every push to `main` through GitHub Actions |
| `version.json` *(generated)* | Records the deployed Git commit for verification |
| `sub-1001_T1w.nii.gz` | Optional, ignored local T1w input used to prove the viewer path; never deployed |
| `downloads/neuro-sync-preview.sh` | Safe, non-functional workflow preview; reads and uploads nothing |
| `Scaling Up Neuroimaging Data for Foundation Models.md` | Initial initiative brief |
| `Creating a web-scale neuroimaging database.md` | Expanded strategy and Q&A source note |
| `scaling_neuro_mockup.mhtml` | Saved visual reference used for the original design translation |

## What's real vs. placeholder

The site is still a **front-end concept prototype**. Local development has one real input path;
the public deployment exposes synthetic previews only.
Features that depend on infrastructure we have not built are marked in the UI:

- **The production `neuro-sync` CLI** — the production command and output are illustrative.
  The downloadable shell file is a real, safe preview that only explains the intended workflow;
  it never opens or transmits scan files. The proposed production contract is a portable,
  CPU-only launcher that processes one series at a time, resumes safely, and downloads a
  versioned privacy pack only when structural scans require it.
- **R2 archive** (`s3://scaling-neuro/` via R2's S3-compatible API) — the file tree and
  access model are representative; no live bucket is connected.
- **3D viewer** — on localhost, `sub-1001_T1w.nii.gz` is fetched from the same local site, decompressed,
  parsed as NIfTI-1, intensity-normalized, and resampled to a 96³ interactive grid entirely
  in the browser. Public hosts hide that local row. The other archive rows use procedural synthetic volumes so modality,
  participant, session, filtering, and switching behavior can be explored before bucket integration.

The name **“Scaling Neuro”** is a working title — rename via the brand text in `index.html`.

## Design note: T1w in the archive

Structural scans now enter a fail-closed privacy gate rather than being excluded. A proposed
production client would classify the field of view, locally privacy-process scans containing
facial anatomy, verify face removal and brain preservation, and quarantine uncertain outputs.
Known no-face fields of view may pass unchanged. The local T1w entry remains labeled **real
NIfTI** and **local example** because its privacy status has not been established here, and it
is hidden outside localhost. The other T1w rows are synthetic **privacy-cleared concepts** for
the proposed R2 path.
