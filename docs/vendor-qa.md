# Vendor compatibility and production QA

This is an evidence log, not a claim that every DICOM ever emitted by a named vendor is interchangeable. Scaling Neuro accepts only scans that pass the same modality, privacy, geometry, time-series, metadata, and signal checks regardless of vendor. Unknown or incomplete inputs remain local.

## Public regression fixtures

All results below were produced on macOS arm64 with `neuro-sync 0.1.0` and the bundled `dcm2niix v1.0.20260416`. The converter release archives are hash-pinned by platform in the release workflow.

| Vendor fixture | Pinned public source | DICOM files | Native conversion | Client decision | Meaning |
|---|---|---:|---|---|---|
| GE SliceTiming `10_Ax_fMRI_HB8_80sl_int_des` | [`neurolabusc/dcm_qa_ge@e92133a`](https://github.com/neurolabusc/dcm_qa_ge/tree/e92133ae414e2b23e8c4d07a4030ab4dbb41518e/In/SliceTiming/10_Ax_fMRI_HB8_80sl_int_des) | 240 | `64×64×80×3` | held locally | GE discovery and conversion succeed; three volumes are intentionally below the ten-volume functional archive floor. |
| Philips Magdeburg `201_EPI_asc_CLEAR` | [`neurolabusc/dcm_qa_philips@74efdbc`](https://github.com/neurolabusc/dcm_qa_philips/tree/74efdbc01eb62540fbb702787c3a7a2c0e22f9eb/In/Magdeburg_2014/fmri) | 27 | `64×64×9×3` | held locally | Philips discovery and precise-scaling conversion succeed; three volumes are intentionally below the functional archive floor. |
| Siemens XA30 `7_func-bold_task-fa_run-1` | [`neurolabusc/dcm_qa_xa30@54a9f42`](https://github.com/neurolabusc/dcm_qa_xa30/tree/54a9f42222e2ebef6f24e01f9c618fe85ff63b2b/In/7_func-bold_task-fa_run-1) | 20 | `64×64×33×20` | accepted | The complete EPI, privacy, signal, geometry, and metadata gates pass. |

The Siemens sidecar retained manufacturer/model/software, field strength, receive coil, sequence/image codes, acquisition and series numbers, patient position, TR/TE, slice timing, phase encoding, echo/readout/dwell timing, multiband and partial-Fourier factors, matrix, voxel/affine/orientation, datatype, volume count, conversion provenance, QC, and the site-scoped protocol group. It contained no source UID, filename/path, person identifier, date/time, institution/station/operator field, protocol or series free text, or private-tag dump. Both the sidecar and NIfTI were validated again after storage.

## Production transport exercise

The accepted Siemens fixture was sent through the production `scalingneuro.com` API and private R2 archive. The exercise checked:

- one exact 15-minute signed URL per allocated part, with wrong hash, wrong part number, expired grant, and same-length wrong bytes rejected;
- exact compressed-object SHA-256 and uncompressed NIfTI SHA-256 after R2 round trip;
- server-side gzip, NIfTI header/geometry, sidecar schema, cross-object metadata, and protocol-group validation;
- an immutable Worker-authored archive manifest and matching D1 catalog row;
- no source path or forbidden PHI/UID marker in either stored JSON document;
- withdrawal deletion of both bundle objects and the manifest, retention of the catalog tombstone, and rejection of replay.

For the 3.1 MB fixture, the full completion request took about 3.9 seconds wall time and 254 ms CPU in Cloudflare's production trace. The Pages function is fail-closed and configured with a 300,000 ms CPU ceiling; previews have no D1, R2, or secret bindings. Larger representative EPIs still belong in the per-release performance matrix.

## Required collaborator-site evidence

Before a lab treats its scanner/export route as supported, record an institution-approved fresh functional export with scanner model/software, DICOM form (classic, mosaic, or enhanced), transfer syntax, file/frame organization, client/converter versions, accepted/held outcome, dimensions/volumes/TR/TE, retained metadata fields, local-versus-R2 hashes, interruption/resume result, and withdrawal result. Do not upload a participant scan merely to create a compatibility test; use an approved phantom or otherwise explicitly cleared acquisition.
