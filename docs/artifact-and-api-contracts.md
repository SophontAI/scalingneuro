# Artifact and API contracts

All JSON is UTF-8 and uses default-deny request shapes. Pseudonymous scientific
IDs are 24 lowercase hexadecimal characters. Upload IDs are UUIDs. Hashes are
lowercase SHA-256. Errors never echo tokens, signed URLs, paths, DICOM values, or
request bodies.

## Public routes

- `GET /health`
- `GET /v1/contribution`
- `POST /v1/register`
- `POST /v1/archive-access`

Registration creates a device identity for contribution. Archive access is a
separate participation form:

```json
{
  "contact_name": "Researcher Name",
  "contact_email": "researcher@example.edu",
  "institution_name": "Example University",
  "lab_name": "Example Lab",
  "participation_commitment": true
}
```

The archive access response contains a bearer token once. D1 stores only its
SHA-256 digest. The normalized work email is separately hashed for lookup and
encrypted for administration.

## Device routes

- `POST /v1/device/policy`
- `POST /v1/dicom-uploads`
- `POST /v1/dicom-uploads/{id}/credentials`
- `POST /v1/dicom-uploads/{id}/parts`
- `POST /v1/dicom-uploads/{id}/checkpoint`
- `POST /v1/dicom-uploads/{id}/complete`
- `GET /v1/dicom-uploads/{id}`

The upload request may contain one `functional_epi` series under the current
DICOM deidentification contract. The Worker rejects every other series kind.
One series per durable receipt keeps continuation and withdrawal scoped to one
scientific unit.

Part grants are short-lived R2 `UploadPart` URLs bound to the exact object key,
multipart ID, part number, content length, and SHA-256 header. The client never
receives an R2 credential.

Checkpoint completes and `HEAD`-verifies the multipart object. Complete reserves
the exact series identity and commits the archive receipt. Neither call reads
the scientific object body or starts downstream work.

## Archive routes

- `GET /v1/archive`
- `GET /v1/archive/{upload_id}/{series_archive_id}/download`

Both require the bearer token returned by the participation form. Listing
returns committed, non-withdrawn functional EPI series only. Each download
route redirects to a short-lived signed R2 GET URL. Size and SHA-256 are
included in the listing so researchers can verify each downloaded archive.

## Published schemas

- `api-error-v1.schema.json`
- `archive-access-request-v1.schema.json`
- `archive-access-response-v1.schema.json`
- `archive-list-v1.schema.json`
- `common-v1.schema.json`
- `contribution-info-v1.schema.json`
- `device-policy-v1.schema.json`
- `dicom-archive-manifest-v2.schema.json`
- `dicom-upload-init-v1.schema.json`
- `dicom-upload-session-v1.schema.json`
- `dicom-upload-status-v1.schema.json`
- `local-manifest-v1.schema.json`
- `registration-request-v1.schema.json`
- `registration-response-v1.schema.json`
- `upload-complete-v1.schema.json`
- `upload-part-request-v1.schema.json`
- `upload-part-response-v1.schema.json`
