-- A scanner console may prepare or transfer more than one independent DICOM
-- folder at a time. Legacy NIfTI sessions retain their original single-active
-- guard, while raw DICOM sessions are isolated by their request hash and
-- stable per-series identities.
DROP INDEX IF EXISTS uploads_one_active_per_device_idx;

CREATE UNIQUE INDEX uploads_one_active_legacy_per_device_idx
ON uploads(device_id)
WHERE status IN ('created', 'uploading') AND ingest_format = 'nifti-v1';

-- Marks the terminal loser of an exact receipt race. This is deliberately
-- separate from purged_at: the durable reconciliation must be replayable even
-- when R2 cleanup is delayed or the HTTP success response is lost.
ALTER TABLE uploads ADD COLUMN receipt_reconciled_at INTEGER;
