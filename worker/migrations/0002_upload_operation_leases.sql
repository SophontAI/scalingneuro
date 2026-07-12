ALTER TABLE uploads ADD COLUMN operation_token TEXT;
ALTER TABLE uploads ADD COLUMN operation_kind TEXT
  CHECK (operation_kind IS NULL OR operation_kind IN ('initialize', 'verify', 'purge'));
ALTER TABLE uploads ADD COLUMN operation_expires_at INTEGER;

CREATE INDEX uploads_operation_lease_idx
ON uploads(status, operation_expires_at);

CREATE UNIQUE INDEX uploads_one_active_per_device_idx
ON uploads(device_id)
WHERE status IN ('created', 'uploading');
