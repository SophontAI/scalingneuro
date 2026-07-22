-- A committed raw-DICOM receipt wakes the Sophont processor exactly once.
-- The cluster-side listener independently deduplicates upload IDs, so a
-- response lost after dispatch remains safe to retry.
ALTER TABLE uploads ADD COLUMN cluster_launch_dispatched_at INTEGER;

CREATE INDEX uploads_cluster_launch_idx
ON uploads(status, ingest_format, cluster_launch_dispatched_at);
