ALTER TABLE upload_bundles ADD COLUMN protocol_group_id TEXT;
ALTER TABLE catalog_series ADD COLUMN protocol_group_id TEXT;

CREATE UNIQUE INDEX catalog_series_bundle_tombstone_idx
ON catalog_series(site_id, project_id, bundle_id);
