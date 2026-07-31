-- Archive readers must accept a versioned no-identification, no-reidentification,
-- and no-contact agreement before a request can be approved or a grant used.
-- Existing rows intentionally remain unaccepted until the researcher resubmits.
ALTER TABLE archive_access_requests
  ADD COLUMN data_use_agreement INTEGER NOT NULL DEFAULT 0
    CHECK (data_use_agreement IN (0, 1));

ALTER TABLE archive_access_requests
  ADD COLUMN accepted_data_use_policy_version TEXT;

ALTER TABLE archive_access_requests
  ADD COLUMN data_use_agreement_accepted_at INTEGER;

ALTER TABLE archive_access_registrations
  ADD COLUMN data_use_agreement INTEGER NOT NULL DEFAULT 0
    CHECK (data_use_agreement IN (0, 1));

ALTER TABLE archive_access_registrations
  ADD COLUMN accepted_data_use_policy_version TEXT;

ALTER TABLE archive_access_registrations
  ADD COLUMN data_use_agreement_accepted_at INTEGER;
