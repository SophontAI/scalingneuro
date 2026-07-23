-- Researchers receive read access after their lab opts into the shared EPI
-- archive. Tokens are stored only as hashes and work emails are encrypted.
CREATE TABLE archive_access_registrations (
  id TEXT PRIMARY KEY,
  token_hash TEXT NOT NULL UNIQUE,
  email_hash TEXT NOT NULL UNIQUE,
  email_ciphertext TEXT NOT NULL,
  contact_name TEXT NOT NULL,
  institution_name TEXT NOT NULL,
  lab_name TEXT NOT NULL,
  participation_commitment INTEGER NOT NULL
    CHECK (participation_commitment = 1),
  created_at INTEGER NOT NULL,
  updated_at INTEGER NOT NULL,
  last_seen_at INTEGER,
  revoked_at INTEGER
);

CREATE INDEX archive_access_active_idx
ON archive_access_registrations(revoked_at, created_at);
