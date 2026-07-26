CREATE TABLE archive_access_requests (
  id TEXT PRIMARY KEY,
  email_hash TEXT NOT NULL UNIQUE,
  email_ciphertext TEXT NOT NULL,
  contact_name TEXT NOT NULL,
  institution_name TEXT NOT NULL,
  lab_name TEXT NOT NULL,
  participation_commitment INTEGER NOT NULL
    CHECK (participation_commitment = 1),
  status TEXT NOT NULL
    CHECK (status IN ('pending', 'approved', 'rejected')),
  created_at INTEGER NOT NULL,
  updated_at INTEGER NOT NULL,
  reviewed_at INTEGER,
  approved_registration_id TEXT
    REFERENCES archive_access_registrations(id)
);

CREATE INDEX archive_access_requests_status_created
  ON archive_access_requests(status, created_at);
