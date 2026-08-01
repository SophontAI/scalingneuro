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

Registration creates a device identity for contribution. Archive access uses
the current `archive-access-request-v3` contribution-intent and data-use form:

```json
{
  "contact_name": "Researcher Name",
  "contact_email": "researcher@example.edu",
  "institution_name": "Example University",
  "lab_name": "Example Lab",
  "plans_to_contribute": true,
  "contributor_attestation": true,
  "accepted_contribution_policy_version": "open-epi-4.0.0",
  "data_use_agreement": true,
  "accepted_data_use_policy_version": "archive-access-2.0.0"
}
```

The public response is `202 Accepted` with a pseudonymous request ID and
`pending_review` status. It never contains a bearer token. The normalized work
email is hashed for lookup and encrypted for administration. After D1 accepts
the pending request, the Pages Worker asks a private service-only Worker to
email the request details to the archive administrator. The mail binding is
restricted to one verified destination and one Scaling Neuro sender address.
Notification failure does not roll back or duplicate the D1 request.

An operator reviews pending requests with `scripts/archive-access-admin.sh`.
The private admin routes require a separate production secret. Approval mints
and returns a personal bearer token once, stores only its SHA-256 digest in D1,
and marks the request approved. Rejection never creates archive credentials.
Every request records an explicit yes-or-no contribution plan. A requester who
plans to contribute must also explicitly accept the current data contribution
and CC0 policy, and the
request, administrator notification, and resulting grant retain that answer and
attestation. The request and resulting grant also record the exact archive
access and privacy agreement version accepted by the researcher and the
acceptance time. The
API rejects an access request that omits either required agreement or names a
stale version. Archive listing and download routes also
reject credentials that are not bound to the current policy.

## Device routes

- `POST /v1/device/policy`
- `POST /v1/dicom-uploads`
- `POST /v1/dicom-uploads/{id}/credentials`
- `POST /v1/dicom-uploads/{id}/parts`
- `POST /v1/dicom-uploads/{id}/checkpoint`
- `POST /v1/dicom-uploads/{id}/complete`
- `GET /v1/dicom-uploads/{id}`
- `POST /v1/admin/dicom-uploads/{id}/cancel` (admin only)

The upload request may contain one `functional_epi` series under the current
DICOM deidentification contract. The Worker rejects every other series kind.
One series per durable receipt keeps continuation and cancellation scoped to one
scientific unit. New receipts schedule `data_license_id = CC0-1.0` to take
effect seven days after receipt. Before that time, status reports the archive as
`staged`, archive routes exclude it, and an administrator can cancel it after a
contributor emails the upload ID. The same status reports `published` after the
effective time without a scheduled job. A policy-version gate prevents an older
client or stale device acceptance from creating a new upload.

Part grants are short-lived R2 `UploadPart` URLs bound to the exact object key,
multipart ID, part number, content length, and SHA-256 header. The client never
receives an R2 credential.

Checkpoint completes and `HEAD`-verifies the multipart object. Complete reserves
the exact series identity and commits the archive receipt. Neither call reads
the scientific object body or starts downstream work.

## Archive routes

- `GET /v1/archive`
- `GET /v1/archive/{upload_id}/{series_archive_id}/download`

Both require the bearer token emailed after an access request is approved.
Listing returns committed, non-withdrawn functional EPI series only after their
seven-day publication time. Each
download route redirects to a short-lived signed R2 GET URL. Size and SHA-256
are included in the listing so researchers can verify each downloaded archive.
CC0-licensed entries also include the license identifier, canonical URL, and
grant time.
The current archive access and privacy agreement is published at
`/docs/archive-access-policy`.

## Published schemas

- `api-error-v1.schema.json`
- `archive-access-request-v1.schema.json` (historical, before the data-use agreement)
- `archive-access-request-v2.schema.json` (historical, participation required)
- `archive-access-request-v3.schema.json` (current)
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
