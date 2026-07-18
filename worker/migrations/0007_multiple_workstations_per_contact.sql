-- A contact email identifies the responsible lab contact, not a unique
-- workstation. Each machine still receives its own registration and device
-- identity, while the encrypted contact record remains available for audit.
DROP INDEX IF EXISTS contributor_registrations_email_idx;

CREATE INDEX contributor_registrations_email_idx
ON contributor_registrations(email_hash);
