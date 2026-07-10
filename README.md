# Scaling Neuro — landing site

A single-page site outlining the open neuroimaging scaling initiative described in
[*Scaling Up Neuroimaging Data for Foundation Models*](./Scaling%20Up%20Neuroimaging%20Data%20for%20Foundation%20Models.md).

It is a static concept prototype—no build step or package install—and now includes three
switchable visual directions:

- **Atlas** — aubergine, parchment, and sage; editorial/scientific-manuscript typography.
- **Whiteboard** — warm dry-erase white, blue and red marker, and yellow highlighter; an academic scratch-space direction with handwritten display type, graph lines, and lightly imperfect diagram frames.
- **Fieldnote** — bottle green, unbleached paper, and safety orange; a tactile academic-notebook direction with graph grids, registration marks, and offset print-like frames.

The selected theme is saved in local browser storage so the three directions can be compared
without maintaining three separate site builds.

The landing page is intentionally reduced to four surfaces: the scaling argument, contribution
flow, scan explorer, and one-folder script. The hero pairs the original two-bar capacity
comparison—today's flagship public sources versus one year across six centers—with a second,
CortexMAE-inspired scaling-law plot. The solid segment ends at the public-data frontier available
today; a dotted segment shows the conceptual frontier that coordinated institutional sharing
could unlock. The dotted extension is explicitly described as explanatory rather than a measured
performance projection.

## Run locally

```bash
python3 -m http.server 4173
# open http://localhost:4173
```

(Three.js loads from a CDN at runtime, so the 3D viewer needs a network connection.)

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
| `app.js` | Theme switching, archive filtering, browser-side NIfTI-1 decoding, and the 3D viewer (Three.js) |
| `sub-1001_T1w.nii.gz` | Optional, ignored local T1w input used to prove the viewer path; never deployed |
| `downloads/neuro-sync-preview.sh` | Safe, non-functional workflow preview; reads and uploads nothing |

## What's real vs. placeholder

The site is still a **front-end concept prototype**. Local development has one real input path;
the public deployment exposes synthetic previews only.
Features that depend on infrastructure we have not built are marked in the UI:

- **The production `neuro-sync` CLI** — the production command and output are illustrative.
  The downloadable shell file is a real, safe preview that only explains the intended workflow;
  it never opens or transmits scan files. The proposed production contract is a portable,
  CPU-only launcher that processes one series at a time, resumes safely, and downloads a
  versioned privacy pack only when structural scans require it.
- **S3 archive** (`s3://scaling-neuro/`) — the file tree and access model are representative; no live bucket is connected.
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
the proposed S3 path.
