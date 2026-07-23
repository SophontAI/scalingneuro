# DICOM deidentification policy

Policy ID: `scaling-neuro.dicom-deidentification`

Policy version: `2.0.0`

This policy applies only to functional EPI series selected by the local
classifier. A series that is not confirmed functional EPI never reaches archive
creation.

## Default deny

The client rewrites every selected DICOM Part 10 file into a new object. It walks
nested sequences recursively and retains only standard elements required for:

- Pixel Data decoding and transfer syntax;
- image dimensions, geometry, orientation, and frame structure;
- MR acquisition timing and echo-planar interpretation;
- bounded scanner make, model, software, field strength, and coil provenance;
- required reference relationships after UID remapping; and
- narrowly validated vendor fields needed to interpret packed EPI geometry.

Unknown standard fields and unknown private fields are removed. Private fields
are retained only when their semantic, VR, VM, size, and structural predicates
are explicitly implemented.

## Removed data

The policy removes:

- patient names, IDs, demographics, accession values, and clinical identifiers;
- calendar dates, times, ages, addresses, telephone numbers, and free text;
- institution, department, station, device serial, operator, and physician data;
- study, series, protocol, image, and comment descriptions;
- source paths, filenames, and media identifiers;
- overlays, graphics, presentation state, and unsafe binary payloads; and
- private creators and blocks that are not explicitly reconstructed.

Patient, study, series, SOP instance, frame-of-reference, dimension-organization,
and referenced SOP instance UIDs are deterministically remapped within the local
site privacy domain.

## Pixel and image gates

Pixel Data is preserved in the source transfer syntax. The client does not
convert, resample, crop, mask, or otherwise preprocess images.

Declared burned-in annotation, overlay or graphic content, Secondary Capture,
unsupported image SOP classes, malformed Pixel Data boundaries, inconsistent
series identity, and unsupported enhanced or packed-image geometry fail closed.
Those series stay local.

Because the upload scope is functional EPI only, the client does not upload
high-resolution structural or localizer images and does not implement a defacing
workflow.

## Audit

Before archive creation, every rewritten object is reopened and checked against
the same recursive default-deny policy. The archive manifest records the policy
ID and version, pseudonymous identities, classifier evidence, ordered instance
hashes, and deterministic archive identity. It contains no source paths,
filenames, source UIDs, or arbitrary DICOM values.
