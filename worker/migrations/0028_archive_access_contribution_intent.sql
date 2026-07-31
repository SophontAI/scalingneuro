-- Every new archive access request records an explicit yes-or-no answer about
-- future data contribution. A yes answer also records the versioned uploader
-- attestation. Existing requests and grants remain NULL as legacy records.
ALTER TABLE archive_access_requests
  ADD COLUMN plans_to_contribute INTEGER
    CHECK (plans_to_contribute IN (0, 1));

ALTER TABLE archive_access_requests
  ADD COLUMN contributor_attestation INTEGER
    CHECK (contributor_attestation IN (0, 1));

ALTER TABLE archive_access_requests
  ADD COLUMN accepted_contribution_policy_version TEXT;

ALTER TABLE archive_access_requests
  ADD COLUMN contributor_attestation_accepted_at INTEGER;

ALTER TABLE archive_access_registrations
  ADD COLUMN plans_to_contribute INTEGER
    CHECK (plans_to_contribute IN (0, 1));

ALTER TABLE archive_access_registrations
  ADD COLUMN contributor_attestation INTEGER
    CHECK (contributor_attestation IN (0, 1));

ALTER TABLE archive_access_registrations
  ADD COLUMN accepted_contribution_policy_version TEXT;

ALTER TABLE archive_access_registrations
  ADD COLUMN contributor_attestation_accepted_at INTEGER;
