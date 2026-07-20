use std::{
    ffi::OsString,
    fs::OpenOptions,
    path::{Path, PathBuf},
    time::Duration,
};

use anyhow::{Context, Result};
use chrono::Utc;
use rusqlite::{Connection, OptionalExtension, params};

use crate::{
    dicom::SourceFingerprint,
    model::{ExistingArchiveBundle, SourceSummary},
    privacy,
};

#[derive(Debug, Clone)]
pub struct StateStore {
    path: PathBuf,
}

#[derive(Debug, Clone)]
pub struct RunRecord {
    pub id: String,
    pub source_path: String,
    pub status: String,
    pub dry_run: bool,
    pub summary: SourceSummary,
    pub manifest_path: Option<String>,
    pub report_path: Option<String>,
    pub worker_upload_id: Option<String>,
    pub error_code: Option<String>,
    pub created_at: String,
    pub updated_at: String,
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct PublicRunStatus {
    pub id: String,
    pub status: String,
    pub dry_run: bool,
    pub summary: SourceSummary,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub worker_upload_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error_code: Option<String>,
    pub created_at: String,
    pub updated_at: String,
}

impl From<&RunRecord> for PublicRunStatus {
    fn from(run: &RunRecord) -> Self {
        Self {
            id: run.id.clone(),
            status: run.status.clone(),
            dry_run: run.dry_run,
            summary: run.summary.clone(),
            worker_upload_id: run.worker_upload_id.clone(),
            error_code: run.error_code.clone(),
            created_at: run.created_at.clone(),
            updated_at: run.updated_at.clone(),
        }
    }
}

#[derive(Debug, Clone)]
pub struct UploadObjectRecord {
    pub run_id: String,
    pub worker_upload_id: String,
    pub key: String,
    pub local_path: String,
    pub size: u64,
    pub sha256: String,
    pub multipart_id: Option<String>,
    pub status: String,
    pub etag: Option<String>,
}

#[derive(Debug, Clone)]
pub struct UploadedPart {
    pub part_number: u32,
    pub etag: String,
    pub size: u64,
}

#[derive(Debug, Clone)]
pub struct RunUploadRecord {
    pub run_id: String,
    pub chunk_index: u32,
    pub bundle_start: usize,
    pub bundle_count: usize,
    pub worker_upload_id: Option<String>,
    pub status: String,
}

impl StateStore {
    pub fn open(path: &Path) -> Result<Self> {
        ensure_private_database_file(path)?;
        let store = Self {
            path: path.to_path_buf(),
        };
        store.migrate()?;
        restrict_sqlite_files(path)?;
        Ok(store)
    }

    fn connection(&self) -> Result<Connection> {
        let connection = Connection::open(&self.path)
            .with_context(|| format!("failed to open state database at {}", self.path.display()))?;
        connection.busy_timeout(Duration::from_secs(5))?;
        connection.pragma_update(None, "foreign_keys", "ON")?;
        restrict_sqlite_files(&self.path)?;
        Ok(connection)
    }

    fn migrate(&self) -> Result<()> {
        let connection = self.connection()?;
        connection.pragma_update(None, "journal_mode", "WAL")?;
        connection.execute_batch(
            r#"
            CREATE TABLE IF NOT EXISTS runs (
                id TEXT PRIMARY KEY,
                source_path TEXT NOT NULL,
                status TEXT NOT NULL,
                dry_run INTEGER NOT NULL,
                summary_json TEXT NOT NULL DEFAULT '{}',
                manifest_path TEXT,
                report_path TEXT,
                worker_upload_id TEXT,
                error_code TEXT,
                created_at TEXT NOT NULL,
                updated_at TEXT NOT NULL
            );
            CREATE INDEX IF NOT EXISTS runs_status_updated ON runs(status, updated_at DESC);
            CREATE TABLE IF NOT EXISTS source_fingerprints (
                run_id TEXT PRIMARY KEY REFERENCES runs(id) ON DELETE CASCADE,
                sha256 TEXT NOT NULL,
                file_count INTEGER NOT NULL,
                created_at TEXT NOT NULL
            );
            CREATE TABLE IF NOT EXISTS upload_objects (
                worker_upload_id TEXT NOT NULL,
                object_key TEXT NOT NULL,
                run_id TEXT NOT NULL REFERENCES runs(id) ON DELETE CASCADE,
                local_path TEXT NOT NULL,
                size INTEGER NOT NULL,
                sha256 TEXT NOT NULL,
                multipart_id TEXT,
                status TEXT NOT NULL DEFAULT 'pending',
                etag TEXT,
                PRIMARY KEY(worker_upload_id, object_key)
            );
            CREATE TABLE IF NOT EXISTS run_uploads (
                run_id TEXT NOT NULL REFERENCES runs(id) ON DELETE CASCADE,
                chunk_index INTEGER NOT NULL,
                bundle_start INTEGER NOT NULL,
                bundle_count INTEGER NOT NULL,
                worker_upload_id TEXT UNIQUE,
                status TEXT NOT NULL DEFAULT 'pending',
                updated_at TEXT NOT NULL,
                PRIMARY KEY(run_id, chunk_index)
            );
            CREATE TABLE IF NOT EXISTS existing_archive_bundles (
                run_id TEXT NOT NULL REFERENCES runs(id) ON DELETE CASCADE,
                bundle_id TEXT NOT NULL,
                series_id TEXT NOT NULL,
                subject_id TEXT NOT NULL,
                session_id TEXT NOT NULL,
                protocol_group_id TEXT NOT NULL,
                upload_id TEXT NOT NULL,
                nii_uncompressed_sha256 TEXT NOT NULL,
                PRIMARY KEY(run_id, bundle_id)
            );
            CREATE TABLE IF NOT EXISTS uploaded_parts (
                worker_upload_id TEXT NOT NULL,
                object_key TEXT NOT NULL,
                part_number INTEGER NOT NULL,
                etag TEXT NOT NULL,
                size INTEGER NOT NULL,
                PRIMARY KEY(worker_upload_id, object_key, part_number),
                FOREIGN KEY(worker_upload_id, object_key)
                    REFERENCES upload_objects(worker_upload_id, object_key) ON DELETE CASCADE
            );
            "#,
        )?;
        restrict_sqlite_files(&self.path)?;
        Ok(())
    }

    pub fn create_run(&self, id: &str, source: &Path, dry_run: bool) -> Result<()> {
        let now = Utc::now().to_rfc3339();
        self.connection()?.execute(
            "INSERT INTO runs (id, source_path, status, dry_run, summary_json, created_at, updated_at) VALUES (?1, ?2, 'discovering', ?3, ?4, ?5, ?5)",
            params![id, source.to_string_lossy(), dry_run, serde_json::to_string(&SourceSummary::default())?, now],
        )?;
        Ok(())
    }

    pub fn update_run(
        &self,
        id: &str,
        status: &str,
        summary: &SourceSummary,
        error_code: Option<&str>,
    ) -> Result<()> {
        self.connection()?.execute(
            "UPDATE runs SET status=?2, summary_json=?3, error_code=?4, updated_at=?5 WHERE id=?1",
            params![
                id,
                status,
                serde_json::to_string(summary)?,
                error_code,
                Utc::now().to_rfc3339()
            ],
        )?;
        Ok(())
    }

    pub fn update_run_summary(&self, id: &str, summary: &SourceSummary) -> Result<()> {
        self.connection()?.execute(
            "UPDATE runs SET summary_json=?2,updated_at=?3 WHERE id=?1",
            params![id, serde_json::to_string(summary)?, Utc::now().to_rfc3339()],
        )?;
        Ok(())
    }

    pub fn restart_interrupted_preparation(&self, id: &str) -> Result<()> {
        let mut connection = self.connection()?;
        let transaction = connection.transaction()?;
        transaction.execute("DELETE FROM source_fingerprints WHERE run_id=?1", [id])?;
        let updated = transaction.execute(
            "UPDATE runs SET status='discovering',summary_json=?2,manifest_path=NULL,report_path=NULL,worker_upload_id=NULL,error_code=NULL,updated_at=?3 WHERE id=?1 AND status='failed' AND error_code IN ('local_preparation_interrupted','local_preparation_failed')",
            params![
                id,
                serde_json::to_string(&SourceSummary::default())?,
                Utc::now().to_rfc3339()
            ],
        )?;
        if updated != 1 {
            anyhow::bail!("the selected interrupted preparation is no longer retryable");
        }
        transaction.commit()?;
        Ok(())
    }

    pub fn set_source_fingerprint(
        &self,
        run_id: &str,
        fingerprint: &SourceFingerprint,
    ) -> Result<()> {
        self.connection()?.execute(
            "INSERT INTO source_fingerprints (run_id,sha256,file_count,created_at) VALUES (?1,?2,?3,?4) ON CONFLICT(run_id) DO UPDATE SET sha256=excluded.sha256,file_count=excluded.file_count,created_at=excluded.created_at",
            params![
                run_id,
                fingerprint.sha256,
                fingerprint.file_count,
                Utc::now().to_rfc3339()
            ],
        )?;
        Ok(())
    }

    pub fn source_fingerprint(&self, run_id: &str) -> Result<Option<SourceFingerprint>> {
        self.connection()?
            .query_row(
                "SELECT sha256,file_count FROM source_fingerprints WHERE run_id=?1",
                [run_id],
                |row| {
                    Ok(SourceFingerprint {
                        sha256: row.get(0)?,
                        file_count: row.get::<_, i64>(1)? as u64,
                    })
                },
            )
            .optional()
            .map_err(Into::into)
    }

    pub fn supersede_run_for_repreparation(&self, old_id: &str, new_id: &str) -> Result<()> {
        self.supersede_run(old_id, new_id, "privacy_contract_superseded")
    }

    pub fn supersede_run(&self, old_id: &str, new_id: &str, reason: &str) -> Result<()> {
        if reason.is_empty()
            || !reason
                .bytes()
                .all(|byte| byte.is_ascii_lowercase() || byte == b'_')
        {
            anyhow::bail!("run supersession reason must be a non-empty lowercase code");
        }
        let now = Utc::now().to_rfc3339();
        let summary = serde_json::to_string(&SourceSummary::default())?;
        let mut connection = self.connection()?;
        let transaction = connection.transaction()?;
        let inserted = transaction.execute(
            "INSERT INTO runs (id,source_path,status,dry_run,summary_json,created_at,updated_at) SELECT ?2,source_path,'discovering',dry_run,?3,?4,?4 FROM runs WHERE id=?1 AND status IN ('prepared','uploading','upload_failed')",
            params![old_id, new_id, summary, now],
        )?;
        if inserted != 1 {
            anyhow::bail!("the requested run is not eligible for privacy repreparation");
        }
        let updated = transaction.execute(
            "UPDATE runs SET status='superseded',error_code=?4,updated_at=?3 WHERE id=?1 AND id<>?2 AND status IN ('prepared','uploading','upload_failed')",
            params![old_id, new_id, now, reason],
        )?;
        if updated != 1 {
            anyhow::bail!("could not supersede the outdated privacy checkpoint");
        }
        transaction.commit()?;
        Ok(())
    }

    pub fn set_artifacts(&self, id: &str, manifest: &Path, report: &Path) -> Result<()> {
        self.connection()?.execute(
            "UPDATE runs SET manifest_path=?2, report_path=?3, updated_at=?4 WHERE id=?1",
            params![
                id,
                manifest.to_string_lossy(),
                report.to_string_lossy(),
                Utc::now().to_rfc3339()
            ],
        )?;
        Ok(())
    }

    pub fn set_worker_upload(&self, id: &str, upload_id: &str) -> Result<()> {
        self.connection()?.execute(
            "UPDATE runs SET worker_upload_id=?2, status='uploading', updated_at=?3 WHERE id=?1",
            params![id, upload_id, Utc::now().to_rfc3339()],
        )?;
        Ok(())
    }

    pub fn ensure_run_uploads(
        &self,
        run_id: &str,
        bundle_subjects: &[String],
        bundle_sizes: &[u64],
        max_bundles: usize,
        max_bytes: u64,
    ) -> Result<()> {
        if bundle_subjects.len() != bundle_sizes.len() {
            anyhow::bail!("bundle subject and size layouts do not match");
        }
        let mut connection = self.connection()?;
        let transaction = connection.transaction()?;
        let mut start = 0_usize;
        let mut chunk_index = 0_usize;
        while start < bundle_sizes.len() {
            let mut count = 0_usize;
            let mut bytes = 0_u64;
            let subject = &bundle_subjects[start];
            while start + count < bundle_sizes.len() && count < max_bundles {
                if &bundle_subjects[start + count] != subject {
                    break;
                }
                let next = bundle_sizes[start + count];
                if count > 0 && bytes.saturating_add(next) > max_bytes {
                    break;
                }
                if next > max_bytes {
                    anyhow::bail!("one bundle exceeds the archive transaction byte limit");
                }
                bytes = bytes.saturating_add(next);
                count += 1;
            }
            transaction.execute(
                "INSERT OR IGNORE INTO run_uploads (run_id,chunk_index,bundle_start,bundle_count,status,updated_at) VALUES (?1,?2,?3,?4,'pending',?5)",
                params![run_id, chunk_index as i64, start as i64, count as i64, Utc::now().to_rfc3339()],
            )?;
            start += count;
            chunk_index += 1;
        }
        transaction.commit()?;
        Ok(())
    }

    /// Create the durable upload ledger entry for exactly one DICOM series.
    ///
    /// Streaming preparation appends one bundle to the manifest at a time, so
    /// its manifest index is also its stable chunk index.  Verifying an
    /// existing row is important: silently accepting an older multi-series
    /// layout could associate a receipt with the wrong local archive.
    pub fn ensure_single_series_upload(
        &self,
        run_id: &str,
        bundle_index: usize,
    ) -> Result<RunUploadRecord> {
        let chunk_index = u32::try_from(bundle_index)
            .context("DICOM series count exceeds the local upload ledger limit")?;
        let now = Utc::now().to_rfc3339();
        let connection = self.connection()?;
        connection.execute(
            "INSERT OR IGNORE INTO run_uploads (run_id,chunk_index,bundle_start,bundle_count,status,updated_at) VALUES (?1,?2,?3,1,'pending',?4)",
            params![run_id, chunk_index, bundle_index as i64, now],
        )?;
        let row = connection
            .query_row(
                "SELECT run_id,chunk_index,bundle_start,bundle_count,worker_upload_id,status FROM run_uploads WHERE run_id=?1 AND chunk_index=?2",
                params![run_id, chunk_index],
                |row| {
                    Ok(RunUploadRecord {
                        run_id: row.get(0)?,
                        chunk_index: row.get::<_, i64>(1)? as u32,
                        bundle_start: row.get::<_, i64>(2)? as usize,
                        bundle_count: row.get::<_, i64>(3)? as usize,
                        worker_upload_id: row.get(4)?,
                        status: row.get(5)?,
                    })
                },
            )
            .optional()?
            .context("could not create the local DICOM series upload checkpoint")?;
        if row.bundle_start != bundle_index || row.bundle_count != 1 {
            anyhow::bail!(
                "existing DICOM upload checkpoint uses an incompatible multi-series layout"
            );
        }
        Ok(row)
    }

    pub fn run_uploads(&self, run_id: &str) -> Result<Vec<RunUploadRecord>> {
        let connection = self.connection()?;
        let mut statement = connection.prepare(
            "SELECT run_id,chunk_index,bundle_start,bundle_count,worker_upload_id,status FROM run_uploads WHERE run_id=?1 ORDER BY chunk_index",
        )?;
        Ok(statement
            .query_map([run_id], |row| {
                Ok(RunUploadRecord {
                    run_id: row.get(0)?,
                    chunk_index: row.get::<_, i64>(1)? as u32,
                    bundle_start: row.get::<_, i64>(2)? as usize,
                    bundle_count: row.get::<_, i64>(3)? as usize,
                    worker_upload_id: row.get(4)?,
                    status: row.get(5)?,
                })
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?)
    }

    pub fn set_chunk_worker(&self, run_id: &str, chunk_index: u32, upload_id: &str) -> Result<()> {
        let mut connection = self.connection()?;
        let transaction = connection.transaction()?;
        transaction.execute(
            "UPDATE run_uploads SET worker_upload_id=?3,status='uploading',updated_at=?4 WHERE run_id=?1 AND chunk_index=?2",
            params![run_id, chunk_index, upload_id, Utc::now().to_rfc3339()],
        )?;
        transaction.execute(
            "UPDATE runs SET worker_upload_id=?2,status='uploading',updated_at=?3 WHERE id=?1",
            params![run_id, upload_id, Utc::now().to_rfc3339()],
        )?;
        transaction.commit()?;
        Ok(())
    }

    pub fn set_chunk_status(&self, run_id: &str, chunk_index: u32, status: &str) -> Result<()> {
        self.connection()?.execute(
            "UPDATE run_uploads SET status=?3,updated_at=?4 WHERE run_id=?1 AND chunk_index=?2",
            params![run_id, chunk_index, status, Utc::now().to_rfc3339()],
        )?;
        Ok(())
    }

    /// Record a receipt won by a different workstation without retaining its
    /// non-queryable upload ID as though it belonged to this device.
    pub fn set_chunk_reconciled(&self, run_id: &str, chunk_index: u32) -> Result<()> {
        let mut connection = self.connection()?;
        let transaction = connection.transaction()?;
        let now = Utc::now().to_rfc3339();
        transaction.execute(
            "UPDATE run_uploads SET worker_upload_id=NULL,status='reconciled',updated_at=?3 WHERE run_id=?1 AND chunk_index=?2",
            params![run_id, chunk_index, now],
        )?;
        transaction.execute(
            "UPDATE runs SET worker_upload_id=(SELECT worker_upload_id FROM run_uploads WHERE run_id=?1 AND worker_upload_id IS NOT NULL ORDER BY chunk_index LIMIT 1),updated_at=?2 WHERE id=?1",
            params![run_id, now],
        )?;
        transaction.commit()?;
        Ok(())
    }

    /// Re-open one previously committed DICOM series after the server has
    /// proven its stored archive corrupt and released that exact identity for
    /// one integrity replacement. Healthy receipt rows are left untouched.
    pub fn reset_single_series_chunk_for_repair(
        &self,
        run_id: &str,
        chunk_index: u32,
        expected_upload_id: &str,
    ) -> Result<()> {
        let mut connection = self.connection()?;
        let transaction = connection.transaction()?;
        let row = transaction
            .query_row(
                "SELECT bundle_count,worker_upload_id,status FROM run_uploads WHERE run_id=?1 AND chunk_index=?2",
                params![run_id, chunk_index],
                |row| {
                    Ok((
                        row.get::<_, i64>(0)? as usize,
                        row.get::<_, Option<String>>(1)?,
                        row.get::<_, String>(2)?,
                    ))
                },
            )
            .optional()?
            .context("integrity repair references a missing local upload receipt")?;
        if row.0 != 1 || row.1.as_deref() != Some(expected_upload_id) || row.2 != "committed" {
            anyhow::bail!(
                "integrity repair no longer matches the committed one-series receipt checkpoint"
            );
        }
        transaction.execute(
            "DELETE FROM uploaded_parts WHERE worker_upload_id=?1",
            [expected_upload_id],
        )?;
        transaction.execute(
            "DELETE FROM upload_objects WHERE worker_upload_id=?1",
            [expected_upload_id],
        )?;
        let updated = transaction.execute(
            "UPDATE run_uploads SET worker_upload_id=NULL,status='pending',updated_at=?3 WHERE run_id=?1 AND chunk_index=?2 AND worker_upload_id=?4 AND status='committed'",
            params![run_id, chunk_index, Utc::now().to_rfc3339(), expected_upload_id],
        )?;
        if updated != 1 {
            anyhow::bail!("committed DICOM receipt changed during integrity repair reset");
        }
        transaction.execute(
            "UPDATE runs SET worker_upload_id=(SELECT worker_upload_id FROM run_uploads WHERE run_id=?1 AND worker_upload_id IS NOT NULL ORDER BY chunk_index LIMIT 1),status='upload_failed',error_code='server_integrity_repair_required',updated_at=?2 WHERE id=?1",
            params![run_id, Utc::now().to_rfc3339()],
        )?;
        transaction.commit()?;
        Ok(())
    }

    /// Adopt a replacement session allocated after a receipt created by a
    /// different workstation was released for proven storage corruption.
    /// Reconciled rows intentionally had no queryable upload ID; this guarded
    /// transition binds only the exact one-series row to the new local device's
    /// replacement session.
    pub fn adopt_reconciled_repair_upload(
        &self,
        run_id: &str,
        chunk_index: u32,
        replacement_upload_id: &str,
    ) -> Result<()> {
        if replacement_upload_id.is_empty()
            || replacement_upload_id.len() > 128
            || replacement_upload_id
                .chars()
                .any(|character| character.is_control() || character.is_whitespace())
        {
            anyhow::bail!("integrity replacement returned an invalid server upload identity");
        }
        let mut connection = self.connection()?;
        let transaction = connection.transaction()?;
        let row = transaction
            .query_row(
                "SELECT bundle_count,worker_upload_id,status FROM run_uploads WHERE run_id=?1 AND chunk_index=?2",
                params![run_id, chunk_index],
                |row| {
                    Ok((
                        row.get::<_, i64>(0)? as usize,
                        row.get::<_, Option<String>>(1)?,
                        row.get::<_, String>(2)?,
                    ))
                },
            )
            .optional()?
            .context("integrity repair references a missing reconciled receipt")?;
        if row.0 != 1 || row.1.is_some() || row.2 != "reconciled" {
            anyhow::bail!(
                "integrity repair no longer matches the reconciled one-series receipt checkpoint"
            );
        }
        let now = Utc::now().to_rfc3339();
        let updated = transaction.execute(
            "UPDATE run_uploads SET worker_upload_id=?3,status='uploading',updated_at=?4 WHERE run_id=?1 AND chunk_index=?2 AND worker_upload_id IS NULL AND status='reconciled'",
            params![run_id, chunk_index, replacement_upload_id, now],
        )?;
        if updated != 1 {
            anyhow::bail!("reconciled DICOM receipt changed during integrity repair adoption");
        }
        transaction.execute(
            "UPDATE runs SET worker_upload_id=?2,status='upload_failed',error_code='server_integrity_repair_required',updated_at=?3 WHERE id=?1",
            params![run_id, replacement_upload_id, now],
        )?;
        transaction.commit()?;
        Ok(())
    }

    pub fn record_existing_bundles(
        &self,
        run_id: &str,
        bundles: &[ExistingArchiveBundle],
    ) -> Result<()> {
        let mut connection = self.connection()?;
        let transaction = connection.transaction()?;
        for bundle in bundles {
            let inserted = transaction.execute(
                "INSERT OR IGNORE INTO existing_archive_bundles (run_id,bundle_id,series_id,subject_id,session_id,protocol_group_id,upload_id,nii_uncompressed_sha256) VALUES (?1,?2,?3,?4,?5,?6,?7,?8)",
                params![
                    run_id,
                    bundle.bundle_id,
                    bundle.series_id,
                    bundle.subject_id,
                    bundle.session_id,
                    bundle.protocol_group_id,
                    bundle.upload_id,
                    bundle.nii_uncompressed_sha256,
                ],
            )?;
            if inserted == 0 {
                let stored = transaction.query_row(
                    "SELECT bundle_id,series_id,subject_id,session_id,protocol_group_id,upload_id,nii_uncompressed_sha256 FROM existing_archive_bundles WHERE run_id=?1 AND bundle_id=?2",
                    params![run_id, bundle.bundle_id],
                    existing_bundle_from_row,
                )?;
                if &stored != bundle {
                    anyhow::bail!("existing archive bundle identity changed during recovery");
                }
            }
        }
        transaction.commit()?;
        Ok(())
    }

    pub fn existing_bundles(&self, run_id: &str) -> Result<Vec<ExistingArchiveBundle>> {
        let connection = self.connection()?;
        let mut statement = connection.prepare(
            "SELECT bundle_id,series_id,subject_id,session_id,protocol_group_id,upload_id,nii_uncompressed_sha256 FROM existing_archive_bundles WHERE run_id=?1 ORDER BY bundle_id",
        )?;
        Ok(statement
            .query_map([run_id], existing_bundle_from_row)?
            .collect::<rusqlite::Result<Vec<_>>>()?)
    }

    pub fn run(&self, id: &str) -> Result<Option<RunRecord>> {
        let connection = self.connection()?;
        connection
            .query_row(
                "SELECT id,source_path,status,dry_run,summary_json,manifest_path,report_path,worker_upload_id,error_code,created_at,updated_at FROM runs WHERE id=?1",
                [id],
                row_to_run,
            )
            .optional()
            .map_err(Into::into)
    }

    pub fn latest_run(&self) -> Result<Option<RunRecord>> {
        let connection = self.connection()?;
        connection
            .query_row(
                "SELECT id,source_path,status,dry_run,summary_json,manifest_path,report_path,worker_upload_id,error_code,created_at,updated_at FROM runs ORDER BY created_at DESC LIMIT 1",
                [],
                row_to_run,
            )
            .optional()
            .map_err(Into::into)
    }

    pub fn continuable_run_for_source(
        &self,
        source: &Path,
        dry_run: bool,
    ) -> Result<Option<RunRecord>> {
        self.connection()?
            .query_row(
                "SELECT r.id,r.source_path,r.status,r.dry_run,r.summary_json,r.manifest_path,r.report_path,r.worker_upload_id,r.error_code,r.created_at,r.updated_at FROM runs r WHERE r.source_path=?1 AND r.dry_run=?2 AND r.status IN ('prepared','uploading','upload_failed') AND NOT EXISTS (SELECT 1 FROM runs newer WHERE newer.source_path=r.source_path AND newer.dry_run=r.dry_run AND newer.created_at>r.created_at AND newer.status IN ('complete','complete_no_eligible_series','dry_run_complete')) ORDER BY EXISTS (SELECT 1 FROM run_uploads u WHERE u.run_id=r.id AND u.worker_upload_id IS NOT NULL) DESC, r.created_at DESC LIMIT 1",
                params![source.to_string_lossy(), dry_run],
                row_to_run,
            )
            .optional()
            .map_err(Into::into)
    }

    pub fn interrupted_preparation_for_source(
        &self,
        source: &Path,
        dry_run: bool,
    ) -> Result<Option<RunRecord>> {
        self.connection()?
            .query_row(
                "SELECT r.id,r.source_path,r.status,r.dry_run,r.summary_json,r.manifest_path,r.report_path,r.worker_upload_id,r.error_code,r.created_at,r.updated_at FROM runs r WHERE r.source_path=?1 AND r.dry_run=?2 AND r.status='failed' AND r.error_code IN ('local_preparation_interrupted','local_preparation_failed') AND NOT EXISTS (SELECT 1 FROM runs newer WHERE newer.source_path=r.source_path AND newer.dry_run=r.dry_run AND newer.created_at>r.created_at AND newer.status IN ('complete','complete_no_eligible_series','dry_run_complete')) ORDER BY r.created_at DESC LIMIT 1",
                params![source.to_string_lossy(), dry_run],
                row_to_run,
            )
            .optional()
            .map_err(Into::into)
    }

    pub fn completed_run_for_source(
        &self,
        source: &Path,
        dry_run: bool,
    ) -> Result<Option<RunRecord>> {
        self.connection()?
            .query_row(
                "SELECT id,source_path,status,dry_run,summary_json,manifest_path,report_path,worker_upload_id,error_code,created_at,updated_at FROM runs WHERE source_path=?1 AND dry_run=?2 AND ((?2=1 AND status='dry_run_complete') OR (?2=0 AND status IN ('complete','complete_no_eligible_series'))) ORDER BY created_at DESC LIMIT 1",
                params![source.to_string_lossy(), dry_run],
                row_to_run,
            )
            .optional()
            .map_err(Into::into)
    }

    pub fn interrupted_preparation_runs(&self) -> Result<Vec<RunRecord>> {
        let connection = self.connection()?;
        let mut statement = connection.prepare(
            "SELECT id,source_path,status,dry_run,summary_json,manifest_path,report_path,worker_upload_id,error_code,created_at,updated_at FROM runs WHERE status IN ('discovering','preparing','converting') ORDER BY created_at",
        )?;
        Ok(statement
            .query_map([], row_to_run)?
            .collect::<rusqlite::Result<Vec<_>>>()?)
    }

    pub fn add_upload_object(&self, object: &UploadObjectRecord) -> Result<()> {
        self.connection()?.execute(
            "INSERT OR IGNORE INTO upload_objects (worker_upload_id,object_key,run_id,local_path,size,sha256,multipart_id,status,etag) VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9)",
            params![object.worker_upload_id, object.key, object.run_id, object.local_path, object.size, object.sha256, object.multipart_id, object.status, object.etag],
        )?;
        Ok(())
    }

    pub fn upload_objects(&self, worker_upload_id: &str) -> Result<Vec<UploadObjectRecord>> {
        let connection = self.connection()?;
        let mut statement = connection.prepare(
            "SELECT run_id,worker_upload_id,object_key,local_path,size,sha256,multipart_id,status,etag FROM upload_objects WHERE worker_upload_id=?1 ORDER BY object_key",
        )?;
        let objects = statement
            .query_map([worker_upload_id], |row| {
                Ok(UploadObjectRecord {
                    run_id: row.get(0)?,
                    worker_upload_id: row.get(1)?,
                    key: row.get(2)?,
                    local_path: row.get(3)?,
                    size: row.get::<_, i64>(4)? as u64,
                    sha256: row.get(5)?,
                    multipart_id: row.get(6)?,
                    status: row.get(7)?,
                    etag: row.get(8)?,
                })
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(objects)
    }

    pub fn set_multipart_id(
        &self,
        worker_upload_id: &str,
        key: &str,
        multipart_id: &str,
    ) -> Result<()> {
        self.connection()?.execute(
            "UPDATE upload_objects SET multipart_id=?3,status='uploading' WHERE worker_upload_id=?1 AND object_key=?2",
            params![worker_upload_id, key, multipart_id],
        )?;
        Ok(())
    }

    pub fn reset_multipart(
        &self,
        worker_upload_id: &str,
        key: &str,
        multipart_id: &str,
    ) -> Result<()> {
        let mut connection = self.connection()?;
        let transaction = connection.transaction()?;
        transaction.execute(
            "DELETE FROM uploaded_parts WHERE worker_upload_id=?1 AND object_key=?2",
            params![worker_upload_id, key],
        )?;
        transaction.execute(
            "UPDATE upload_objects SET multipart_id=?3,status='uploading',etag=NULL WHERE worker_upload_id=?1 AND object_key=?2",
            params![worker_upload_id, key, multipart_id],
        )?;
        transaction.commit()?;
        Ok(())
    }

    pub fn uploaded_parts(&self, worker_upload_id: &str, key: &str) -> Result<Vec<UploadedPart>> {
        let connection = self.connection()?;
        let mut statement = connection.prepare(
            "SELECT part_number,etag,size FROM uploaded_parts WHERE worker_upload_id=?1 AND object_key=?2 ORDER BY part_number",
        )?;
        Ok(statement
            .query_map(params![worker_upload_id, key], |row| {
                Ok(UploadedPart {
                    part_number: row.get::<_, i64>(0)? as u32,
                    etag: row.get(1)?,
                    size: row.get::<_, i64>(2)? as u64,
                })
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?)
    }

    pub fn save_part(&self, worker_upload_id: &str, key: &str, part: &UploadedPart) -> Result<()> {
        self.connection()?.execute(
            "INSERT OR REPLACE INTO uploaded_parts (worker_upload_id,object_key,part_number,etag,size) VALUES (?1,?2,?3,?4,?5)",
            params![worker_upload_id, key, part.part_number, part.etag, part.size],
        )?;
        Ok(())
    }

    pub fn complete_object(&self, worker_upload_id: &str, key: &str, etag: &str) -> Result<()> {
        self.connection()?.execute(
            "UPDATE upload_objects SET status='complete',etag=?3 WHERE worker_upload_id=?1 AND object_key=?2",
            params![worker_upload_id, key, etag],
        )?;
        Ok(())
    }
}

fn ensure_private_database_file(path: &Path) -> Result<()> {
    let mut options = OpenOptions::new();
    options.create(true).append(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    options.open(path).with_context(|| {
        format!(
            "failed to create private state database at {}",
            path.display()
        )
    })?;
    privacy::restrict_file(path)
}

fn sqlite_sidecar_path(path: &Path, suffix: &str) -> PathBuf {
    let mut value = OsString::from(path.as_os_str());
    value.push(suffix);
    PathBuf::from(value)
}

fn restrict_sqlite_files(path: &Path) -> Result<()> {
    privacy::restrict_file(path)?;
    for suffix in ["-wal", "-shm", "-journal"] {
        let sidecar = sqlite_sidecar_path(path, suffix);
        if sidecar.exists() {
            privacy::restrict_file(&sidecar)?;
        }
    }
    Ok(())
}

fn row_to_run(row: &rusqlite::Row<'_>) -> rusqlite::Result<RunRecord> {
    let summary_json: String = row.get(4)?;
    Ok(RunRecord {
        id: row.get(0)?,
        source_path: row.get(1)?,
        status: row.get(2)?,
        dry_run: row.get(3)?,
        summary: serde_json::from_str(&summary_json).unwrap_or_default(),
        manifest_path: row.get(5)?,
        report_path: row.get(6)?,
        worker_upload_id: row.get(7)?,
        error_code: row.get(8)?,
        created_at: row.get(9)?,
        updated_at: row.get(10)?,
    })
}

fn existing_bundle_from_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<ExistingArchiveBundle> {
    Ok(ExistingArchiveBundle {
        bundle_id: row.get(0)?,
        series_id: row.get(1)?,
        subject_id: row.get(2)?,
        session_id: row.get(3)?,
        protocol_group_id: row.get(4)?,
        upload_id: row.get(5)?,
        nii_uncompressed_sha256: row.get(6)?,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn run_and_multipart_state_survive_reopen() {
        let directory = tempdir().unwrap();
        let path = directory.path().join("state.sqlite3");
        let store = StateStore::open(&path).unwrap();
        store
            .create_run("run", Path::new("/private/source"), false)
            .unwrap();
        store
            .set_artifacts(
                "run",
                Path::new("/private/work/run.manifest.json"),
                Path::new("/private/work/run.report.json"),
            )
            .unwrap();
        let existing_bundle = ExistingArchiveBundle {
            bundle_id: "a".repeat(24),
            series_id: "b".repeat(24),
            subject_id: "c".repeat(24),
            session_id: "d".repeat(24),
            protocol_group_id: "e".repeat(24),
            upload_id: "11111111-1111-4111-8111-111111111111".into(),
            nii_uncompressed_sha256: "f".repeat(64),
        };
        store
            .record_existing_bundles("run", std::slice::from_ref(&existing_bundle))
            .unwrap();
        store
            .record_existing_bundles("run", std::slice::from_ref(&existing_bundle))
            .unwrap();
        assert_eq!(
            store.existing_bundles("run").unwrap(),
            vec![existing_bundle]
        );
        let public =
            serde_json::to_string(&PublicRunStatus::from(&store.run("run").unwrap().unwrap()))
                .unwrap();
        assert!(!public.contains("source_path"));
        assert!(!public.contains("manifest_path"));
        assert!(!public.contains("report_path"));
        assert!(!public.contains("/private/"));
        assert_eq!(store.interrupted_preparation_runs().unwrap().len(), 1);
        let sizes = vec![1_u64; 65];
        let subjects = vec!["subject-a".to_owned(); 65];
        store
            .ensure_run_uploads("run", &subjects, &sizes, 32, 32)
            .unwrap();
        let chunks = store.run_uploads("run").unwrap();
        assert_eq!(chunks.len(), 3);
        assert_eq!(chunks[2].bundle_start, 64);
        assert_eq!(chunks[2].bundle_count, 1);
        store
            .create_run("byte-run", Path::new("/private/source"), false)
            .unwrap();
        store
            .ensure_run_uploads(
                "byte-run",
                &["a".into(), "a".into(), "a".into()],
                &[20, 20, 10],
                32,
                32,
            )
            .unwrap();
        let byte_chunks = store.run_uploads("byte-run").unwrap();
        assert_eq!(byte_chunks.len(), 2);
        assert_eq!(byte_chunks[0].bundle_count, 1);
        assert_eq!(byte_chunks[1].bundle_count, 2);
        store
            .create_run("subject-run", Path::new("/private/source"), false)
            .unwrap();
        store
            .ensure_run_uploads(
                "subject-run",
                &["a".into(), "a".into(), "b".into(), "b".into()],
                &[1, 1, 1, 1],
                32,
                32,
            )
            .unwrap();
        let subject_chunks = store.run_uploads("subject-run").unwrap();
        assert_eq!(subject_chunks.len(), 2);
        assert_eq!(subject_chunks[0].bundle_count, 2);
        assert_eq!(subject_chunks[1].bundle_start, 2);
        let object = UploadObjectRecord {
            run_id: "run".into(),
            worker_upload_id: "up".into(),
            key: "prefix/file".into(),
            local_path: "/private/local".into(),
            size: 12,
            sha256: "aa".into(),
            multipart_id: None,
            status: "pending".into(),
            etag: None,
        };
        store.add_upload_object(&object).unwrap();
        store
            .set_multipart_id("up", "prefix/file", "multipart")
            .unwrap();
        store
            .save_part(
                "up",
                "prefix/file",
                &UploadedPart {
                    part_number: 1,
                    etag: "etag".into(),
                    size: 12,
                },
            )
            .unwrap();
        drop(store);
        let reopened = StateStore::open(&path).unwrap();
        assert_eq!(
            reopened.uploaded_parts("up", "prefix/file").unwrap().len(),
            1
        );
        assert_eq!(
            reopened.run("run").unwrap().unwrap().source_path,
            "/private/source"
        );
    }

    #[test]
    fn privacy_repreparation_supersedes_old_run_and_preserves_private_source() {
        let directory = tempfile::tempdir().unwrap();
        let store = StateStore::open(&directory.path().join("state.sqlite3")).unwrap();
        store
            .create_run("old-run", Path::new("/private/source"), false)
            .unwrap();
        store
            .update_run(
                "old-run",
                "prepared",
                &SourceSummary {
                    dicom_files: 12,
                    accepted: 1,
                    ..Default::default()
                },
                None,
            )
            .unwrap();
        store
            .supersede_run_for_repreparation("old-run", "new-run")
            .unwrap();
        let old = store.run("old-run").unwrap().unwrap();
        let new = store.run("new-run").unwrap().unwrap();
        assert_eq!(old.status, "superseded");
        assert_eq!(
            old.error_code.as_deref(),
            Some("privacy_contract_superseded")
        );
        assert_eq!(new.status, "discovering");
        assert_eq!(new.source_path, "/private/source");
        assert!(
            store
                .continuable_run_for_source(Path::new("/private/source"), false)
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn folder_history_selects_checkpointed_work_and_completed_snapshots() {
        let directory = tempdir().unwrap();
        let store = StateStore::open(&directory.path().join("state.sqlite3")).unwrap();
        let source = Path::new("/private/dicoms");
        store.create_run("partial", source, false).unwrap();
        store
            .update_run(
                "partial",
                "upload_failed",
                &SourceSummary {
                    files_seen: 40,
                    dicom_files: 40,
                    accepted: 1,
                    ..Default::default()
                },
                Some("upload_failed"),
            )
            .unwrap();
        assert_eq!(
            store
                .continuable_run_for_source(source, false)
                .unwrap()
                .unwrap()
                .id,
            "partial"
        );
        assert!(
            store
                .continuable_run_for_source(Path::new("/private/other"), false)
                .unwrap()
                .is_none()
        );

        store.create_run("complete", source, false).unwrap();
        store
            .update_run("complete", "complete", &SourceSummary::default(), None)
            .unwrap();
        let fingerprint = SourceFingerprint {
            sha256: "a".repeat(64),
            file_count: 40,
        };
        store
            .set_source_fingerprint("complete", &fingerprint)
            .unwrap();
        assert_eq!(
            store.source_fingerprint("complete").unwrap(),
            Some(fingerprint)
        );
        assert_eq!(
            store
                .completed_run_for_source(source, false)
                .unwrap()
                .unwrap()
                .id,
            "complete"
        );
        assert!(
            store
                .continuable_run_for_source(source, false)
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn server_bound_legacy_run_outranks_a_newer_empty_shadow_retry() {
        let directory = tempdir().unwrap();
        let store = StateStore::open(&directory.path().join("state.sqlite3")).unwrap();
        let source = Path::new("/private/dicoms");
        store.create_run("server-bound", source, false).unwrap();
        store
            .ensure_run_uploads("server-bound", &["subject".into()], &[1], 32, 32)
            .unwrap();
        store
            .set_chunk_worker("server-bound", 0, "11111111-1111-4111-8111-111111111111")
            .unwrap();
        store
            .update_run(
                "server-bound",
                "upload_failed",
                &SourceSummary::default(),
                Some("upload_failed"),
            )
            .unwrap();

        store.create_run("shadow-retry", source, false).unwrap();
        store
            .update_run(
                "shadow-retry",
                "upload_failed",
                &SourceSummary::default(),
                Some("upload_failed"),
            )
            .unwrap();

        assert_eq!(
            store
                .continuable_run_for_source(source, false)
                .unwrap()
                .unwrap()
                .id,
            "server-bound"
        );
    }

    #[test]
    fn interrupted_local_preparation_reuses_its_run_identity() {
        let directory = tempdir().unwrap();
        let store = StateStore::open(&directory.path().join("state.sqlite3")).unwrap();
        let source = Path::new("/private/dicoms");
        store.create_run("interrupted", source, false).unwrap();
        store
            .update_run(
                "interrupted",
                "failed",
                &SourceSummary::default(),
                Some("local_preparation_interrupted"),
            )
            .unwrap();
        let selected = store
            .interrupted_preparation_for_source(source, false)
            .unwrap()
            .unwrap();
        assert_eq!(selected.id, "interrupted");
        store
            .restart_interrupted_preparation("interrupted")
            .unwrap();
        let restarted = store.run("interrupted").unwrap().unwrap();
        assert_eq!(restarted.status, "discovering");
        assert!(restarted.error_code.is_none());
    }

    #[test]
    fn single_series_upload_checkpoint_is_idempotent_and_rejects_old_layout_collision() {
        let directory = tempdir().unwrap();
        let store = StateStore::open(&directory.path().join("state.sqlite3")).unwrap();
        let source = Path::new("/private/dicoms");
        store.create_run("stream", source, false).unwrap();
        let first = store.ensure_single_series_upload("stream", 0).unwrap();
        let replay = store.ensure_single_series_upload("stream", 0).unwrap();
        assert_eq!(first.bundle_start, 0);
        assert_eq!(first.bundle_count, 1);
        assert_eq!(replay.chunk_index, first.chunk_index);
        assert_eq!(store.run_uploads("stream").unwrap().len(), 1);

        store.create_run("legacy", source, false).unwrap();
        store
            .ensure_run_uploads(
                "legacy",
                &["subject".into(), "subject".into()],
                &[1, 1],
                8,
                1024,
            )
            .unwrap();
        let error = store.ensure_single_series_upload("legacy", 0).unwrap_err();
        assert!(error.to_string().contains("multi-series layout"));
    }

    #[test]
    fn integrity_repair_reopens_only_the_exact_committed_single_series_receipt() {
        let directory = tempdir().unwrap();
        let store = StateStore::open(&directory.path().join("state.sqlite3")).unwrap();
        store
            .create_run("repair", Path::new("/private/dicoms"), false)
            .unwrap();
        store.ensure_single_series_upload("repair", 0).unwrap();
        let upload_id = "11111111-1111-4111-8111-111111111111";
        store.set_chunk_worker("repair", 0, upload_id).unwrap();
        store.set_chunk_status("repair", 0, "committed").unwrap();
        store
            .add_upload_object(&UploadObjectRecord {
                run_id: "repair".into(),
                worker_upload_id: upload_id.into(),
                key: "prefix/dicom.tar.zst".into(),
                local_path: "/private/dicom.tar.zst".into(),
                size: 10,
                sha256: "a".repeat(64),
                multipart_id: Some("multipart".into()),
                status: "complete".into(),
                etag: Some("etag".into()),
            })
            .unwrap();
        store
            .save_part(
                upload_id,
                "prefix/dicom.tar.zst",
                &UploadedPart {
                    part_number: 1,
                    etag: "etag".into(),
                    size: 10,
                },
            )
            .unwrap();

        store
            .reset_single_series_chunk_for_repair("repair", 0, upload_id)
            .unwrap();
        let chunk = store.run_uploads("repair").unwrap().remove(0);
        assert_eq!(chunk.status, "pending");
        assert!(chunk.worker_upload_id.is_none());
        assert!(store.upload_objects(upload_id).unwrap().is_empty());
        assert!(
            store
                .uploaded_parts(upload_id, "prefix/dicom.tar.zst")
                .unwrap()
                .is_empty()
        );
        let run = store.run("repair").unwrap().unwrap();
        assert_eq!(run.status, "upload_failed");
        assert_eq!(
            run.error_code.as_deref(),
            Some("server_integrity_repair_required")
        );
        assert!(
            store
                .reset_single_series_chunk_for_repair("repair", 0, upload_id)
                .is_err()
        );
    }

    #[test]
    fn reconciled_integrity_repair_adopts_only_a_new_exact_server_session() {
        let directory = tempdir().unwrap();
        let store = StateStore::open(&directory.path().join("state.sqlite3")).unwrap();
        store
            .create_run("repair", Path::new("/private/dicoms"), false)
            .unwrap();
        store.ensure_single_series_upload("repair", 0).unwrap();
        store.set_chunk_reconciled("repair", 0).unwrap();
        let replacement = "33333333-3333-4333-8333-333333333333";

        store
            .adopt_reconciled_repair_upload("repair", 0, replacement)
            .unwrap();
        let chunk = store.run_uploads("repair").unwrap().remove(0);
        assert_eq!(chunk.status, "uploading");
        assert_eq!(chunk.worker_upload_id.as_deref(), Some(replacement));
        let run = store.run("repair").unwrap().unwrap();
        assert_eq!(run.status, "upload_failed");
        assert_eq!(
            run.error_code.as_deref(),
            Some("server_integrity_repair_required")
        );
        assert!(
            store
                .adopt_reconciled_repair_upload("repair", 0, replacement)
                .is_err()
        );
    }

    #[cfg(unix)]
    #[test]
    fn database_and_sqlite_sidecars_are_owner_only() {
        use std::{fs, os::unix::fs::PermissionsExt};

        let directory = tempdir().unwrap();
        let path = directory.path().join("private.sqlite3");
        let store = StateStore::open(&path).unwrap();
        let connection = store.connection().unwrap();
        connection
            .execute_batch(
                "BEGIN IMMEDIATE; INSERT INTO runs (id,source_path,status,dry_run,summary_json,created_at,updated_at) VALUES ('permissions','private','discovering',0,'{}','now','now'); COMMIT;",
            )
            .unwrap();

        let mode = |file: &Path| fs::metadata(file).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode(&path), 0o600);
        let wal = sqlite_sidecar_path(&path, "-wal");
        let shm = sqlite_sidecar_path(&path, "-shm");
        assert!(wal.is_file());
        assert!(shm.is_file());
        assert_eq!(mode(&wal), 0o600);
        assert_eq!(mode(&shm), 0o600);
        let journal = sqlite_sidecar_path(&path, "-journal");
        if journal.exists() {
            assert_eq!(mode(&journal), 0o600);
        }
    }
}
