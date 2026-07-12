# Artifact and API contracts

## Versioned artifacts

| Artifact | Canonical schema | Purpose |
|---|---|---|
| Enrollment request | `schemas/enrollment-request-v1.schema.json` | Client-bound, replay-safe invite/device enrollment operation |
| Enrollment response | `schemas/enrollment-response-v1.schema.json` | Stable result returned for a new or exactly replayed enrollment |
| Local preparation manifest | `schemas/local-manifest-v1.schema.json` | Owner-only conversion/resume checkpoint; never an upload artifact |
| Scan sidecar | `schemas/scan-sidecar-v1.schema.json` | Privacy-filtered metadata beside one acquisition-space NIfTI |
| Metadata policy | `schemas/metadata-policy-v1.schema.json` | Machine-readable default-deny allow/local-only/deny rules |
| Upload initialization | `schemas/upload-init-v1.schema.json` | Exact body accepted by `POST /v1/uploads` |
| Upload session | `schemas/upload-session-v1.schema.json` | Server-created multipart IDs and part sizes; no reusable cloud credential |
| Part URL request | `schemas/upload-part-request-v1.schema.json` | One exact key/part/size/SHA-256 capability request |
| Part URL response | `schemas/upload-part-response-v1.schema.json` | One 15-minute presigned URL and its required signed headers |
| Upload completion | `schemas/upload-complete-v1.schema.json` | Exact object receipts accepted at commit time |
| Archive manifest | `schemas/archive-manifest-v1.schema.json` | Immutable Worker-authored record of a committed upload |
| Upload status | `schemas/upload-status-v1.schema.json` | Pollable control-plane state |
| API error | `schemas/api-error-v1.schema.json` | Stable error envelope and codes |

Examples live in `schemas/examples/`. `python3 schemas/validate.py` validates every schema and example, resolves canonical public `$id` references locally, and proves that every metadata-policy output path exists in the scan-sidecar schema.

Schema files use JSON Schema draft 2020-12 and strict `additionalProperties: false` boundaries. The v1 API request bodies do not carry `$schema` or `schema_version`: the endpoint contract is versioned by `/v1`, and the Worker rejects unknown fields. Stored sidecars and archive manifests carry their own explicit version because they outlive the API process that created them.

`bundle_id`, `series_id`, `subject_id`, `session_id`, and `protocol_group_id` are bare 24-character lowercase hexadecimal site-HMAC pseudonyms. They never contain `sub_`, `ses_`, or another label; labels appear only in human-readable filenames. Worker-owned upload, site, and project IDs are UUIDs. One upload request may contain several series/sessions/protocol groups, but every bundle must have the same `subject_id`.

## HTTP surface

All JSON endpoints return `content-type: application/json`. Device endpoints use `Authorization: Bearer <opaque-device-token>`; admin endpoints use the separate admin bearer secret.

| Method and path | Request | Success response |
|---|---|---|
| `GET /health` | none | service health/version |
| `POST /v1/enroll` | enrollment-request schema | enrollment-response schema: stable operation/device IDs and token, site/project context, contribution-policy version, site pseudonym key |
| `POST /v1/uploads` | upload-init schema | upload-session schema; committed idempotent replay has an empty multipart plan |
| `POST /v1/uploads/{id}/credentials` | empty body | deprecated-name compatibility route that refreshes the multipart plan; it returns no credentials |
| `POST /v1/uploads/{id}/parts` | upload-part request schema | upload-part response schema for one exact part |
| `POST /v1/uploads/{id}/complete` | upload-complete schema | committed ID/time and manifest key/SHA-256 |
| `GET /v1/uploads/{id}` | none | upload-status schema |
| `POST /v1/admin/invites` | site/project names/slugs, consent-policy version, expiry, uses | one-time invite metadata and plaintext invite code |
| `POST /v1/admin/devices/{id}/revoke` | empty body | revoked device status |
| `POST /v1/admin/uploads/{id}/withdraw` | empty body | withdrawn upload status and tombstone audit state |

## Enrollment transaction

Before the first enrollment request, the client generates a UUIDv4 `enrollment_id` and a 256-bit `sn_device_…` token. It atomically checkpoints those values and the non-secret request metadata in owner-only local state keyed by the SHA-256 of the invite; the plaintext invite is not persisted. The exact pending operation is reused after a timeout, connection loss, process crash, or lost response. It is deleted only after the final enrolled configuration has been saved successfully.

The Worker validates the exact request, stores only the device-token SHA-256, and inserts the device under the invite-consumption trigger. A new device consumes one invite use. A replay succeeds only when the invite hash, `enrollment_id`, and device-token hash all match the existing non-revoked device; it returns the same enrollment response without another device, audit event, or invite use. The same operation UUID with a different token, or the same used invite with a different operation, receives the generic `INVALID_INVITE` response. Neither secret is included in logs or error details.

Enrollment returns `pseudonym_key_b64` only to the enrolled client over TLS. The client stores it in its protected local configuration and uses it to generate site-scoped HMAC identifiers. The control plane stores the site key encrypted; neither raw identifiers nor the HMAC input cross the API.

No R2 access key, secret access key, session token, or reusable prefix credential crosses the API. The Worker owns the parent signing key and all create/complete/abort operations. For each part, the client declares the initialized full key, part number, exact size, and SHA-256. The Worker validates those values against the allocated object and returns a presigned `UploadPart` URL that expires after 15 minutes by default, plus the exact `content-length` and `x-amz-content-sha256` headers covered by the signature.

That URL authorizes only one payload for one part of one Worker-created multipart upload. Changing the key, upload ID, part number, length, hash, or required headers invalidates it. It cannot read, list, copy, delete, create, complete, or abort. The URL is a bearer capability until expiry and must never be logged, persisted into reports, or sent to telemetry.

## Upload transaction

Before upload, the client writes a local preparation manifest conforming to `local-manifest-v1.schema.json`. This owner-only file deliberately contains staging paths and therefore is never uploaded, logged, or used as the shareable run report. It records the enrolled `site_id`, `project_id`, `consent_policy_version`, client version, and metadata-policy provenance. Resume fails closed unless the enrollment and privacy contract still match. An older privacy checkpoint is superseded and re-prepared locally from its private stored source path; the replacement must reproduce the exact prior bundle-identity set before upload can begin.

The client initializes up to 32 same-subject bundles at a time with pseudonymous protocol-group identity, relative keys, sizes, and compressed SHA-256 values for each NIfTI and metadata object, plus the NIfTI's uncompressed SHA-256. A compressed NIfTI may be at most 5 GiB and one Worker session at most 32 GiB. Only one upload may be active per device; larger or multi-subject folders are automatically split into sequential sessions. Each bundle is exactly one flat directory named for `bundle_id`; the `.nii.gz` and `.json` have the same basename. The service assigns:

```text
archive/v1/{site_id}/{project_id}/{upload_id}/
```

The Worker creates the 2–64 R2 multipart objects, persists their multipart IDs, and sets trusted `sha256`/`upload_id` metadata. The client requests and uses one presigned URL per allocated part and persists each returned opaque ETag. Completion lists every expected full key exactly once, its declared size/hash, and parts numbered consecutively from 1. Clients send canonical bare ETags; the Worker tolerates and strips one matching surrounding S3 quote pair. The client cannot create or complete objects itself.

The Worker completes each multipart object through its binding, performs HEAD verification, streams every stored object through server-side SHA-256, strictly validates each sidecar against the public schema/privacy contract, writes the immutable archive manifest, and atomically moves status to `committed`. A caller may safely retry create, multipart-plan refresh, part-URL minting, complete, and status operations. A hash/sidecar failure expires and purges that uncommitted session; no client response alone is a commit boundary—the immutable manifest is.

Catalog reconciliation is also fail-closed and resumable. If a create request contains an already committed active `bundle_id`, the Worker returns `DUPLICATE_BUNDLE` with `details.reason = active_exact_match` and an `existing_bundles` array containing only pseudonymous IDs, the existing upload UUID, and the uncompressed NIfTI SHA-256. It emits that reconciliation only after the stored series/subject/session/protocol identities, canonical `bundle_hash`, and server-validated metadata-policy ID/version all match the current request and privacy contract. The client independently compares those fields, persists the no-op result, removes the exact matches from the request, and retries the remaining new subset. This lets a mixed old/new folder proceed and makes a lost subset-allocation response replay-safe. `withdrawn_tombstone`, `identity_conflict`, and `privacy_contract_stale` are deterministic failures and are never reconciled as success.

Two workstations may allocate the same not-yet-cataloged bundle concurrently. If one commits first, the Worker validates the winner under the same identity/privacy rules, purges the losing R2 prefix and multipart state, and returns the same structured `active_exact_match` response from completion. The losing client records the winner as already archived and finishes without a manual Resume. An active upload produced by a client older than the minimum privacy contract is similarly held in its active slot until the Worker aborts and purges it; only then can a current replacement be allocated. Both transitions retain D1 audit state while preventing stale prepared bytes from being resumed or silently treated as current.

Upload states are `created`, `uploading`, `committed`, `expired`, and `withdrawn`. Only `created` and `uploading` accept writes. `committed`, `expired`, and `withdrawn` are terminal for that upload ID. Withdrawal records a tombstone/audit state and removes archive objects through the authorized admin path; object disappearance must never make the catalog silently look as if the upload never existed.

## Error behavior

Errors use one envelope:

```json
{
  "error": {
    "code": "OBJECT_MISMATCH",
    "message": "Uploaded object metadata does not match the initialized bundle",
    "request_id": "req_...",
    "details": { "field": "sha256" }
  }
}
```

Messages and details are safe operational text only. They never echo bearer tokens, presigned URLs, cloud signing material, source paths, raw identifiers, arbitrary DICOM values, or request bodies.

Clients may retry network failures, `CREDENTIALS_UNAVAILABLE` (the stable v1 code for a temporarily unavailable part signer), `STORAGE_UNAVAILABLE`, and ordinary 429/5xx responses with bounded exponential backoff. After a part URL expires, they request a new URL for the same allocated part. The one structured `DUPLICATE_BUNDLE/active_exact_match` response is a reconciliation signal, not a blind retry: the client validates and removes only the named exact matches, whether the signal arrives during allocation or after a concurrent completion. `INVALID_REQUEST`, consent-policy updates, revocation, object mismatch, withdrawn/identity-conflict/privacy-contract-stale duplicates, and other deterministic 4xx states require a local state transition or user-visible action.

## Compatibility and evolution

V1 fields are additive only inside a new versioned schema. A semantic change to identifiers, hashing, classifier decisions, metadata policy, object layout, or manifest canonicalization requires a new schema/policy version and explicit migration/read compatibility. Existing immutable manifests are never rewritten in place.

The archive manifest is canonical key-sorted UTF-8 JSON with a single trailing LF. Its published SHA-256 covers the exact stored bytes. Bundle-hash canonicalization is defined in `docs/epi-ingestion-contract.md` and excludes storage keys, ETags, compressed transport identity, and the sidecar byte hash so retries, metadata enrichment, and multipart layouts do not change scientific identity. Catalog deduplication and withdrawal tombstones use stable site/project-scoped `bundle_id`, not a mutable transport hash.
