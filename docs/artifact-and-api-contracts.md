# Artifact and API contracts

All JSON is UTF-8, rejects duplicate members and non-finite numbers, and is validated with exact/default-deny object shapes. Pseudonymous IDs are 24 lowercase hexadecimal characters; upload/job IDs are UUIDs. Hashes are lowercase SHA-256. Errors contain stable safe codes and never echo tokens, signed URLs, paths, request bodies, or arbitrary DICOM values.

## Versioned artifacts

| Artifact | Contract |
|---|---|
| Local preparation manifest | `schemas/local-manifest-v1.schema.json` |
| DICOM upload initialization | `schemas/dicom-upload-init-v1.schema.json` |
| DICOM upload allocation | `schemas/dicom-upload-session-v1.schema.json` |
| DICOM upload/status receipt | `schemas/dicom-upload-status-v1.schema.json` |
| Multipart completion | `schemas/upload-complete-v1.schema.json` |
| Multipart part request/grant | `schemas/upload-part-request-v1.schema.json`, `upload-part-response-v1.schema.json` |
| Legacy NIfTI ingest | `schemas/upload-init-v1.schema.json`, `upload-session-v1.schema.json`, `upload-status-v1.schema.json` |
| Derived scan sidecar | `schemas/scan-sidecar-v1.schema.json` and `metadata-policy-v1.json` |
| Legacy immutable manifest | `schemas/archive-manifest-v1.schema.json` |
| API error | `schemas/api-error-v1.schema.json` |

`schemas/examples/` contains non-PHI examples validated by both Python `jsonschema` and strict Ajv. The current DICOM source archive’s internal `manifest.json` is independently validated by the cluster processor because it is inside the content-addressed archive boundary; its semantics are specified in the ingest and de-identification contracts.

## Authentication boundaries

- Public: `GET /health`, `GET /v1/contribution`, `POST /v1/register`, `POST /v1/enroll`.
- Device bearer: upload create/part/complete/status routes. A device token is returned once and stored only as a hash in D1.
- Processor bearer: `/v1/processor/jobs/*`. This token can claim/lease jobs and mint scoped object capabilities; it is not an R2 key.
- Admin bearer: registration/device/withdrawal/invite operations.
- Object capabilities: short-lived signed R2 GET, PUT, or UploadPart requests, deliberately sent without the device/processor Authorization header.

The client never receives a reusable R2 access key. Signed URLs must not appear in logs, reports, persistence, telemetry, converter arguments, or failure payloads.

## Public registration

`GET /v1/contribution` returns the open-registration state, project/policy names, policy URL, minimum client version, and `self_service_quota_bytes: null`. Null explicitly means that public contribution has no cumulative workstation allowance. During the `0.2.8` to `0.3.x` two-phase cutover, only an exact `neuro-sync/0.2.x` user agent receives the JSON-safe integer `9007199254740991` because that legacy client modeled the field as a required `u64`; browsers and `0.3+` clients continue to receive null, and the backend project quota remains SQL `NULL`/unlimited in both cases. The ordinary terminal flow generates a UUID registration operation, a 256-bit device token, and owner-only pending state before calling `POST /v1/register`.

An exact replay reuses the operation and token and returns the same enrollment result. A lab/contact may register multiple devices; uniqueness belongs to the operation/device, not email, institution, IP address, or lab name. There is no daily network registration allowance or cumulative public-workstation upload allowance. Per-request, per-object, multipart, and receipt-session safety bounds still apply.

Contact email is normalized and hashed for lookup, then separately encrypted with registration-bound authenticated encryption. Contact data never enters object keys, DICOM archives, sidecars, manifests, or operational logs. The site pseudonym key is returned only to the enrolled client and stored encrypted in the control plane.

## DICOM receipt transaction

### Create

`POST /v1/dicom-uploads` accepts up to 8 series and 250 GiB. The client transparently splits a larger folder into stable same-subject receipt sessions; this bound keeps multipart completion inside Cloudflare's strictest per-invocation limits:

```json
{
  "format": "dicom-series-v1",
  "client_version": "0.3.0",
  "deidentification": {
    "policy_id": "scaling-neuro.dicom-deidentification",
    "policy_version": "1.0.0"
  },
  "series": [{
    "series_archive_id": "24-lowercase-hex",
    "series_id": "24-lowercase-hex",
    "subject_id": "24-lowercase-hex",
    "session_id": "24-lowercase-hex",
    "protocol_group_id": "24-lowercase-hex",
    "dicom_count": 4153,
    "archive": {
      "relative_key": "<series_archive_id>/dicom.tar.zst",
      "size": 123456789,
      "sha256": "64-lowercase-hex",
      "format": "dicom-tar-zstd"
    }
  }]
}
```

The Worker assigns `dicom/v1/{site_id}/{project_id}/{upload_id}/`, creates one R2 multipart object per pending series, and returns each full key, R2 multipart ID, and part size. The request hash makes lost create responses replay-safe.

Already-received exact series are returned as `already_received_series` and are not allocated again. A mixed request allocates only new series. Exactness includes stable series identity and archive-derived bundle hash under the current site/project/privacy contract.

### Upload parts

`POST /v1/dicom-uploads/{upload_id}/parts` declares one initialized full key, part number, exact length, and SHA-256. The response contains a roughly 15-minute `UploadPart` URL and the exact `content-length` and `x-amz-content-sha256` headers covered by its signature.

Changing the key, multipart ID, part number, length, hash, or covered headers invalidates the signature. The capability cannot read, list, copy, delete, create, complete, or abort objects. Parts are replaceable at the same number, which makes the crash window between R2 acceptance and local ETag persistence safe.

`POST /v1/dicom-uploads/{upload_id}/credentials` refreshes allocation state and is idempotent. The client checkpoints canonical bare ETags locally.

### Complete and receive

`POST /v1/dicom-uploads/{upload_id}/complete` lists every pending archive exactly once with declared size/hash and consecutive multipart receipts. A short D1 receipt lease serializes duplicate completion attempts. For each object, the Worker:

1. validates the declaration against allocation;
2. completes the R2 multipart upload, resolving a lost completion response through authoritative `HEAD`;
3. validates R2 length and trusted custom metadata with `HEAD` only; and
4. atomically reserves the series identity, writes the durable receipt, and queues a processing job.

No object body crosses the Worker completion request. Receipt time is therefore bounded by multipart completion/metadata operations rather than archive size or conversion time.

When every pending series is received, upload status becomes `committed` (the durable source-receipt state). `GET /v1/dicom-uploads/{upload_id}` returns received counts/bytes and a separate processing summary with queued, processing, processed, failed, purged, and total series.

### Concurrency and reconciliation

The reservation key is `(site_id, project_id, series_archive_id)`. If two devices race, the first exact receipt wins. The loser compares series identity and bundle hash, records `already_received`, purges its temporary R2 prefix/multipart state, and returns success. Later creates resolve directly to the winner. A withdrawn tombstone or mismatch returns a deterministic `DUPLICATE_BUNDLE` reason and never reconciles as success.

## Processing jobs

`POST /v1/processor/jobs/claim` accepts `processor_id`, a bounded lease duration, and optional `claim_input_format` with the exact value `dicom-series-v1` or `nifti-v1`. Omitting the filter preserves the original all-format behavior. With a filter, no eligible matching work returns HTTP `204` even when another format is queued. A processor identity retains at most one active lease: an exact retry replays that lease, while changing filters during an active different-format lease returns `204` rather than granting a second job. A new claim increments the attempt, assigns a random lease token, and returns one of:

- `dicom-series-v1`: `series_archive_id`, `series_id`, declared DICOM count, and a short-lived scoped archive GET with size/hash; or
- `nifti-v1`: the legacy bundle/series identity and scoped NIfTI/sidecar GETs with compressed/uncompressed hashes.

Unfiltered consumers claim eligible `dicom-series-v1` jobs before the one-time `nifti-v1` migration backlog, with FIFO order retained within each format. The production launch consumer additionally requests only `dicom-series-v1`, so it cannot claim historical work before the release smoke archive exists. Separately configured unfiltered or `nifti-v1` consumers retain deterministic legacy processing.

The processor calls:

- `POST /v1/processor/jobs/{job_id}/heartbeat` to extend the exact lease;
- `POST /v1/processor/jobs/{job_id}/outputs` to declare DICOM-job NIfTI/sidecar/processing-manifest hashes and receive checksum-bound PUT capabilities;
- `POST /v1/processor/jobs/{job_id}/complete` to report pinned versions, the exact output descriptors, and validation booleans; or
- `POST /v1/processor/jobs/{job_id}/fail` with a stable safe code and retryability.

For a DICOM job, completion requires `archive_sha256_verified`, exact `dicom_count`, `dicom_parse_succeeded`, and `functional_epi_confirmed`, plus all three output `HEAD` receipts. A legacy job requires its six compressed/uncompressed/sidecar/NIfTI-consistency booleans and no new output objects.

Output publication is lease-conditional. The Worker checks the lease before capabilities, after object `HEAD`, and in the same conditional D1 publication step. A processor whose lease expired during object storage cannot update derived catalog rows or mark the job processed. The next claimant may safely verify/reuse deterministic prepared files or overwrite the same derived keys.

Retryable failures return to `queued` with bounded delay and attempts. A terminal `DICOM_PRIVACY_AUDIT_FAILED`, `INVALID_DICOM_ARCHIVE`, `ARCHIVE_*`, or `FUNCTIONAL_EPI_NOT_CONFIRMED` result deletes the exact rejected DICOM source object before recording `input_purged_at`, tombstones its reservation, and writes a `processing.input_purged` audit event. The upload receipt remains immutable and its processing summary increments `purged_series`. Terminal converter or scientific-compatibility failures retain the de-identified source archive for review.

## Legacy migration

Existing `0.2.x` uploads contain locally converted NIfTI/sidecar objects. Migration `0010` backfills one `nifti-v1` processing job per existing received bundle. The processor validates those objects in place and records processed state without reconversion or duplicate output uploads. This preserves prior uploads while making all future workstation ingestion DICOM-first.

The legacy `/v1/uploads` routes remain available only for compatible checkpoints during migration. Their completion boundary has also been reduced to authoritative R2 receipts plus queued processing; new clients use `/v1/dicom-uploads`.

## Error and retry behavior

```json
{
  "error": {
    "code": "OBJECT_MISMATCH",
    "message": "Stored object metadata does not match its declaration",
    "request_id": "req_...",
    "details": {"field": "sha256"}
  }
}
```

Clients retry network failures, `408`, `425`, `429`, `5xx`, `CREDENTIALS_UNAVAILABLE`, and `STORAGE_UNAVAILABLE` with bounded exponential backoff and server `Retry-After`. An expired part capability is refreshed for the same allocation. Ordinary `CONFLICT` is not used as a permanent duplicate-series outcome; exact duplicates use structured reconciliation.

Invalid requests, revoked devices, policy/client updates, quota failures, withdrawal tombstones, identity mismatches, and object mismatches require a state change or user-visible action. Sensitive material is never placed in error text.

## Withdrawal and evolution

Administrative withdrawal retains a D1 tombstone/audit record and removes canonical source and derived R2 objects. Object deletion is verified before `purged_at` is persisted. Absence of an R2 object must never erase catalog history silently.

Semantic changes to privacy policy, archive layout, identifier derivation, classifier acceptance, hashing, object layout, or derived sidecar behavior require a new explicit version and migration/compatibility path. Existing immutable source archives and processing manifests are never rewritten in place.
