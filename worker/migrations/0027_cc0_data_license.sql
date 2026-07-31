-- License state is recorded only after the current contribution policy is
-- accepted. The grant time remains NULL until the archive is committed.
ALTER TABLE uploads ADD COLUMN data_license_id TEXT
  CHECK (data_license_id IS NULL OR data_license_id = 'CC0-1.0');

ALTER TABLE uploads ADD COLUMN data_license_granted_at INTEGER;

CREATE INDEX uploads_data_license_idx
ON uploads(data_license_id, data_license_granted_at)
WHERE data_license_id IS NOT NULL;
