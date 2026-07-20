# Artifact and API contracts

All JSON is UTF-8, rejects duplicate members and non-finite numbers, and is validated with exact/default-deny object shapes. Pseudonymous IDs are 24 lowercase hexadecimal characters; upload/job IDs are UUIDs. Hashes are lowercase SHA-256. Errors contain stable safe codes and never echo tokens, signed URLs, paths, request bodies, or arbitrary DICOM values.

## Versioned artifacts

| Artifact | Contract |
|---|---|
| Local preparation manifest | `schemas/local-manifest-v1.schema.json` |
| DICOM upload initialization | `schemas/dicom-upload-init-v1.schema.json` |
| DICOM upload allocation | `schemas/dicom-upload-session-v1.schema.json` |
| DICOM upload/status receipt | `schemas/dicom-upload-status-v1.schema.json` |
| Internal MR series manifest | `schemas/dicom-archive-manifest-v2.schema.json` |
| Device policy acceptance | `schemas/device-policy-v1.schema.json` |
| Multipart completion | `schemas/upload-complete-v1.schema.json` |
| Multipart part request/grant | `schemas/upload-part-request-v1.schema.json`, `upload-part-response-v1.schema.json` |
| Legacy NIfTI ingest | `schemas/upload-init-v1.schema.json`, `upload-session-v1.schema.json`, `upload-status-v1.schema.json` |
| Derived scan sidecar | `schemas/scan-sidecar-v1.schema.json` and `metadata-policy-v1.json` |
| Legacy immutable manifest | `schemas/archive-manifest-v1.schema.json` |
| API error | `schemas/api-error-v1.schema.json` |

`schemas/examples/` contains non-PHI examples validated by both Python `jsonschema` and strict Ajv. The DICOM source archive’s internal `manifest.json` follows the published v2 schema and is independently revalidated by the cluster processor because it is inside the content-addressed archive boundary.

## Authentication boundaries

- Public: `GET /health`, `GET /v1/contribution`, `POST /v1/register`, `POST /v1/enroll`.
- Device bearer: policy-acceptance and upload create/part/complete/status routes. A device token is returned once and stored only as a hash in D1.
- Processor bearer: `/v1/processor/jobs/*`. This token can claim/lease jobs and mint scoped object capabilities; it is not an R2 key.
- Admin bearer: registration/device/withdrawal/invite operations.
- Object capabilities: short-lived signed R2 GET, PUT, or UploadPart requests, deliberately sent without the device/processor Authorization header.

The client never receives a reusable R2 access key. Signed URLs must not appear in logs, reports, persistence, telemetry, converter arguments, or failure payloads.

## Public registration

`GET /v1/contribution` returns the open-registration state, project/policy names, policy URL, minimum client version, and `self_service_quota_bytes: null`. Null explicitly means that public contribution has no cumulative workstation allowance. During the `0.2.8` to `0.3.x` two-phase cutover, only an exact `neuro-sync/0.2.x` user agent receives the JSON-safe integer `9007199254740991` because that legacy client modeled the field as a required `u64`; browsers and `0.3+` clients continue to receive null, and the backend project quota remains SQL `NULL`/unlimited in both cases. The ordinary terminal flow generates a UUID registration operation, a 256-bit device token, and owner-only pending state before calling `POST /v1/register`.

Policy negotiation is release-bound. Recognized `neuro-sync` versions below `0.4.0` receive and may register only under `open-epi-1.0.0`; versions `0.4.0` and newer receive and may register only under `open-mri-1.0.0`. Browser and unrecognized callers see the current MRI policy. The Worker independently derives the required registration policy from `client_version`, so changing the request body cannot bypass this boundary.

An exact replay reuses the operation and token and returns the same enrollment result. A lab/contact may register multiple devices; uniqueness belongs to the operation/device, not email, institution, IP address, or lab name. There is no daily network registration allowance or cumulative public-workstation upload allowance. Per-request, per-object, multipart, and receipt-session safety bounds still apply.

Contact email is normalized and hashed for lookup, then separately encrypted with registration-bound authenticated encryption. Contact data never enters object keys, DICOM archives, sidecars, manifests, or operational logs. The site pseudonym key is returned only to the enrolled client and stored encrypted in the control plane.

When the public contribution scope changes from `open-epi-1.0.0` to `open-mri-1.0.0`, an existing self-service client must show and explicitly accept the new native-pixel policy. `POST /v1/device/policy` requires an exact `neuro-sync/0.4.0`-or-newer user agent, accepts the exact new version under the existing device bearer, and atomically updates that one-device public project. The response includes the canonical MRI project name so the client updates its local display label; older Worker responses without this optional field remain readable. Site, project, device, and pseudonym-key identity are preserved, and exact acceptance replays create only one audit event. Managed projects cannot self-mutate this route.

## DICOM receipt transaction

### Create

`POST /v1/dicom-uploads` accepts up to 8 series and 250 GiB for bounded protocol compatibility. `neuro-sync 0.4` intentionally sends exactly one series per durable receipt. This gives continuation, integrity repair, withdrawal, and terminal reporting a single unambiguous scientific unit while keeping every invocation inside Cloudflare's strictest subrequest limits:

```json
{
  "format": "dicom-series-v1",
  "client_version": "0.4.0",
  "deidentification": {
    "policy_id": "scaling-neuro.dicom-deidentification",
    "policy_version": "2.0.0"
  },
  "series": [{
    "series_archive_id": "24-lowercase-hex",
    "series_id": "24-lowercase-hex",
    "subject_id": "24-lowercase-hex",
    "session_id": "24-lowercase-hex",
    "protocol_group_id": "24-lowercase-hex",
    "dicom_count": 4153,
    "series_kind": "structural_t1w",
    "processing_route": "archive-verify-v1",
    "pixel_data_policy": "scanner-native-not-defaced",
    "archive": {
      "relative_key": "<series_archive_id>/dicom.tar.zst",
      "size": 123456789,
      "sha256": "64-lowercase-hex",
      "format": "dicom-tar-zstd"
    }
  }]
}
```

The Worker assigns `dicom/v1/{site_id}/{project_id}/{upload_id}/`, creates one R2 multipart object per pending series, and returns each full key, R2 multipart ID, and part size. The request hash makes lost create responses replay-safe. Purpose, processing route, and native-pixel policy participate in the exact identity. Policy `1.0.0` remains accepted only for field-absent legacy EPI requests during cutover; policy `2.0.0` requires all three declarations and client `0.4.0` or newer.

Already-received exact series are returned as `already_received_series` and are not allocated again. A mixed request allocates only new series. Exactness includes stable series identity and archive-derived bundle hash under the current site/project/privacy contract.

### Upload parts

`POST /v1/dicom-uploads/{upload_id}/parts` declares one initialized full key, part number, exact length, and SHA-256. The response contains a roughly 15-minute `UploadPart` URL and the exact `content-length` and `x-amz-content-sha256` headers covered by its signature.

Changing the key, multipart ID, part number, length, hash, or covered headers invalidates the signature. The capability cannot read, list, copy, delete, create, complete, or abort objects. Parts are replaceable at the same number, which makes the crash window between R2 acceptance and local ETag persistence safe.

`POST /v1/dicom-uploads/{upload_id}/credentials` refreshes allocation state and is idempotent. The client checkpoints canonical bare ETags locally.

### Checkpoint, then receive

`POST /v1/dicom-uploads/{upload_id}/checkpoint` lists every pending archive exactly once with declared size/hash and consecutive multipart receipts. A short D1 lease serializes duplicate attempts. For each object, the Worker:

1. validates the declaration against allocation;
2. completes the R2 multipart upload, resolving a lost completion response through authoritative `HEAD`;
3. validates R2 length and trusted custom metadata with `HEAD` only; and
4. records a provisional object checkpoint with a 90-day retention deadline.

Checkpointing creates no received-series reservation, scientific receipt, processing job, or catalog entry. This lets the client delete one local series archive at a time while a multi-terabyte folder continues beyond R2's seven-day multipart lifetime. After the client re-inventories and re-hashes the entire source folder, `POST /v1/dicom-uploads/{upload_id}/complete` revalidates the authoritative object metadata, atomically reserves the series identity and routing provenance, writes the durable receipt, and queues one server-verification job per series. Both endpoints are idempotent; a lost response is resolved from R2 and D1 without retransmitting bytes.

No object body crosses the Worker completion request. Receipt time is therefore bounded by multipart completion/metadata operations rather than archive size or conversion time.

When every pending series is received, upload status becomes `committed` (the durable source-receipt state). `GET /v1/dicom-uploads/{upload_id}` returns received counts/bytes and a separate processing summary with queued, processing, processed, failed, purged, repairable, and total series, plus functional-EPI, archive-only, and archive-verified counts. `repairable_series = 1` is emitted only after repeated independent full-object digest failures prove the stored singleton object corrupt, the object is purged, and the exact scientific identity is released for its one audited deterministic replacement.

### Concurrency and reconciliation

The reservation key is `(site_id, project_id, series_archive_id)`. If two devices race, the first exact receipt wins. The loser compares series identity and bundle hash, records `already_received`, purges its temporary R2 prefix/multipart state, and returns success. Later creates resolve directly to the winner. A withdrawn tombstone or mismatch returns a deterministic `DUPLICATE_BUNDLE` reason and never reconciles as success.

## Processing jobs

`POST /v1/processor/jobs/claim` accepts `processor_id`, a bounded lease duration, and optional `claim_input_format` with the exact value `dicom-series-v1` or `nifti-v1`. Omitting the filter preserves the original all-format behavior. With a filter, no eligible matching work returns HTTP `204` even when another format is queued. A processor identity retains at most one active lease: an exact retry replays that lease, while changing filters during an active different-format lease returns `204` rather than granting a second job. A new claim increments the attempt, assigns a random lease token, and returns one of:

- `dicom-series-v1`: `series_archive_id`, `series_id`, `series_kind`, `processing_route`, `pixel_data_policy`, declared DICOM count, and a short-lived scoped archive GET with size/hash; or
- `nifti-v1`: the legacy bundle/series identity and scoped NIfTI/sidecar GETs with compressed/uncompressed hashes.

Unfiltered consumers claim eligible `dicom-series-v1` jobs before the one-time `nifti-v1` migration backlog, with FIFO order retained within each format. The production launch consumer additionally requests only `dicom-series-v1`, so it cannot claim historical work before the release smoke archive exists. Separately configured unfiltered or `nifti-v1` consumers retain deterministic legacy processing.

The processor calls:

- `POST /v1/processor/jobs/{job_id}/heartbeat` to extend the exact lease;
- `POST /v1/processor/jobs/{job_id}/outputs` to declare functional-EPI NIfTI/sidecar/processing-manifest hashes and receive checksum-bound PUT capabilities; archive-verification jobs must not call this route;
- `POST /v1/processor/jobs/{job_id}/complete` to report pinned versions, the exact output descriptors, and validation booleans; or
- `POST /v1/processor/jobs/{job_id}/fail` with a stable safe code and retryability.

For every new-policy DICOM job, completion requires `archive_sha256_verified`, exact `dicom_count`, `dicom_parse_succeeded`, and `dicom_privacy_audit_succeeded`. `functional-epi-v1` additionally requires `functional_epi_confirmed = true` plus all three output `HEAD` receipts and catalog publication. `archive-verify-v1` requires `functional_epi_confirmed = false`, zero outputs, and no functional catalog row; its processed job state is the durable `archive verified` record. The same zero-output payload safely downgrades a client-proposed functional route when the independent server header audit disagrees: D1 records `other_mr` / `archive-verify-v1`, the job completes as archive-verified, and the raw source remains intact. A legacy job requires its six compressed/uncompressed/sidecar/NIfTI-consistency booleans and no new output objects.

Output publication is lease-conditional. The Worker checks the lease before capabilities, after object `HEAD`, and in the same conditional D1 publication step. A processor whose lease expired during object storage cannot update derived catalog rows or mark the job processed. The next claimant may safely verify/reuse deterministic prepared files or overwrite the same derived keys.

Retryable failures return to `queued` with bounded delay and attempts. Only an explicit terminal intrinsic privacy/archive violation deletes the exact unverified DICOM source object before recording `input_purged_at`, tombstones its reservation, and writes a `processing.input_purged` audit event. A full-object download hash mismatch is retried through five independent downloads; only the Worker—not the processor request—may then synthesize `STORED_OBJECT_SHA256_MISMATCH`. It purges the proven-corrupt R2 object and releases that singleton identity for one exact same-hash deterministic replacement. A second independently proven mismatch is permanently tombstoned. Withdrawal at any point closes the released lineage and prevents or withdraws its replacement. The upload receipt remains immutable and its processing summary records `purged_series` and whether one exact repair is available. Purpose disagreement uses the non-destructive archive downgrade above. Extraction timeout, converter, capacity, and scientific-compatibility failures retain the governed source archive for retry or review.

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

Administrative withdrawal retains a D1 tombstone/audit record and removes canonical source and derived R2 objects. It also closes any released integrity-replacement lineage and cascades to an already received exact replacement, so a withdrawn contribution cannot be reintroduced through repair. Object deletion is verified before `purged_at` is persisted. Absence of an R2 object must never erase catalog history silently.

Semantic changes to privacy policy, archive layout, identifier derivation, classifier acceptance, hashing, object layout, or derived sidecar behavior require a new explicit version and migration/compatibility path. Existing immutable source archives and processing manifests are never rewritten in place.
