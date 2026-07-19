-- The initial open-beta allowance was cumulative across a workstation's entire
-- project lifetime. Public contribution is intentionally open-ended; retain
-- nullable operator-managed quotas for non-public projects and only clear
-- projects created by the public registration path.
UPDATE projects
SET upload_quota_bytes = NULL
WHERE id IN (
  SELECT project_id
  FROM contributor_registrations
);
