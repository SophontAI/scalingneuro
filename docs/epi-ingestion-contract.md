# Functional EPI sync contract

## Scope

`neuro-sync` accepts one completed DICOM export folder and uploads only confirmed
functional echo-planar imaging time series. The source directory is read-only.
Structural, diffusion, perfusion, field-map, reference, localizer, derived,
secondary-capture, non-image, non-MR, malformed, and ambiguous series stay local.

The tool does not convert, deface, preprocess, visualize, or analyze scans.

## Local selection

A series must have a supported MR Image SOP class, coherent study/series/instance
identity, valid Pixel Data boundaries, and no declared burned-in annotation,
overlay, or graphic content.

Functional selection requires both:

1. strong EPI evidence from standard DICOM fields; and
2. repeated temporal evidence from temporal-position identifiers, acquisition
   numbers, repeated slice positions, or multiple classic mosaic instances.

The client also requires plausible and series-consistent repetition and echo
timing. Scanner vendor and free-text protocol labels never select a series alone.

## Local deidentification

Selected instances are rewritten before network access. The client removes
identity, dates, clinical and administrative fields, institution and workstation
identity, free text, paths, overlays, graphics, and unreviewed private data. It
remaps referential UIDs and retains only bounded fields needed to decode and
interpret the functional acquisition.

The rewritten file is reopened and recursively audited. Any failed privacy or
integrity check keeps that series local.

## Archive and transfer

Each selected series becomes one deterministic `dicom.tar.zst`. The archive
contains rewritten DICOM Part 10 files followed by a canonical manifest with
pseudonymous identities, classifier evidence, policy versions, ordered instance
hashes, and the whole-archive hash.

The client requests an R2 multipart allocation, uploads checksum-bound parts,
and checkpoints ETags locally. After R2 confirms the completed object and the
client confirms the source folder is unchanged, the Worker records a durable
receipt. No background job is created.

## Shared use

A participation form creates a pending archive access request. It does not
issue credentials. After the work email, institution, and lab information are
reviewed, an approved researcher receives a personal access token by email.
The archive API lists committed, non-withdrawn functional EPI series and issues
short-lived download URLs. The downloaded artifact is the deidentified DICOM
archive. Each lab chooses its own conversion, preprocessing, compute, and
analysis stack.
