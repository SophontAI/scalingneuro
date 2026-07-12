-- Persist the exact privacy contract that was revalidated at archive commit.
-- Existing 0.1.0 pilot rows were produced under metadata policy 1.0.0; they
-- remain queryable but cannot be silently reconciled as current-policy data.
ALTER TABLE catalog_series ADD COLUMN metadata_policy_id TEXT;
ALTER TABLE catalog_series ADD COLUMN metadata_policy_version TEXT;

UPDATE catalog_series
SET metadata_policy_id = 'scaling-neuro-epi-default-deny',
    metadata_policy_version = '1.0.0'
WHERE metadata_policy_id IS NULL OR metadata_policy_version IS NULL;
