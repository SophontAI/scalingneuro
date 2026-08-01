-- New beta receipts remain restricted for seven days. The scheduled time is
-- the future effective time, so no scheduler is required for archive listing
-- and download gates to change from staged to published.
ALTER TABLE uploads ADD COLUMN publication_scheduled_at INTEGER;

UPDATE uploads
SET publication_scheduled_at = data_license_granted_at
WHERE data_license_granted_at IS NOT NULL;

CREATE INDEX uploads_publication_schedule_idx
ON uploads(status, publication_scheduled_at, withdrawn_at)
WHERE publication_scheduled_at IS NOT NULL;

-- Production applies migrations before deploying the new Worker. If the old
-- Worker receives an archive during that interval, turn its immediate license
-- grant into a staged schedule and leave the grant NULL. The old archive query
-- therefore fails closed, while the new Worker uses the scheduled time.
CREATE TRIGGER stage_receipt_from_pre_buffer_worker
AFTER UPDATE OF status, data_license_granted_at ON uploads
WHEN NEW.status = 'committed'
  AND NEW.publication_scheduled_at IS NULL
  AND NEW.data_license_granted_at IS NOT NULL
BEGIN
  UPDATE uploads
  SET publication_scheduled_at = NEW.data_license_granted_at + 604800,
      data_license_granted_at = NULL
  WHERE id = NEW.id;
END;

CREATE TRIGGER ignore_premature_license_audit
BEFORE INSERT ON audit_events
WHEN NEW.event_type = 'upload.licensed'
  AND EXISTS (
    SELECT 1 FROM uploads
    WHERE id = NEW.upload_id
      AND publication_scheduled_at IS NOT NULL
      AND publication_scheduled_at > NEW.created_at
  )
BEGIN
  SELECT RAISE(IGNORE);
END;
