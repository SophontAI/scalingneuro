DROP INDEX IF EXISTS dicom_upload_series_effective_route_idx;
DROP INDEX IF EXISTS devices_enrollment_id_unique_idx;
DROP INDEX IF EXISTS uploads_one_active_legacy_per_device_idx;
DROP INDEX IF EXISTS uploads_operation_lease_idx;

DROP TRIGGER IF EXISTS consume_invite_after_device_insert;
DROP TRIGGER IF EXISTS reject_unavailable_invite_before_device_insert;

ALTER TABLE devices DROP COLUMN invite_id;
ALTER TABLE devices DROP COLUMN enrollment_id;

DROP TABLE IF EXISTS catalog_series;
DROP TABLE IF EXISTS contributor_registration_limits;
DROP TABLE IF EXISTS dicom_upload_reconciled_series;
DROP TABLE IF EXISTS invites;
DROP TABLE IF EXISTS upload_bundles;
DROP TABLE IF EXISTS upload_objects;

ALTER TABLE uploads DROP COLUMN last_credential_at;
ALTER TABLE uploads DROP COLUMN committed_at;
ALTER TABLE uploads DROP COLUMN purged_at;
ALTER TABLE uploads DROP COLUMN manifest_object_key;
ALTER TABLE uploads DROP COLUMN manifest_sha256;
ALTER TABLE uploads DROP COLUMN operation_token;
ALTER TABLE uploads DROP COLUMN operation_kind;
ALTER TABLE uploads DROP COLUMN operation_expires_at;
ALTER TABLE uploads DROP COLUMN ingest_format;
ALTER TABLE uploads DROP COLUMN receipt_reconciled_at;

ALTER TABLE dicom_upload_series DROP COLUMN effective_series_kind;
ALTER TABLE dicom_upload_series DROP COLUMN effective_archive_route;

ALTER TABLE received_series_reservations DROP COLUMN input_format;
