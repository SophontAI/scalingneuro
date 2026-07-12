use std::{
    ffi::OsString,
    fs::OpenOptions,
    path::{Path, PathBuf},
    time::Duration,
};

#[cfg(unix)]
use std::fs;

use anyhow::{Context, Result};
use chrono::Utc;
use rusqlite::{Connection, OptionalExtension, params};

use crate::model::SourceSummary;

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

    pub fn resumable_runs(&self, id: Option<&str>) -> Result<Vec<RunRecord>> {
        let connection = self.connection()?;
        let sql = if id.is_some() {
            "SELECT id,source_path,status,dry_run,summary_json,manifest_path,report_path,worker_upload_id,error_code,created_at,updated_at FROM runs WHERE id=?1 AND status IN ('prepared','uploading','upload_failed')"
        } else {
            "SELECT id,source_path,status,dry_run,summary_json,manifest_path,report_path,worker_upload_id,error_code,created_at,updated_at FROM runs WHERE status IN ('prepared','uploading','upload_failed') ORDER BY created_at"
        };
        let mut statement = connection.prepare(sql)?;
        let mapped = if let Some(id) = id {
            statement
                .query_map([id], row_to_run)?
                .collect::<rusqlite::Result<Vec<_>>>()?
        } else {
            statement
                .query_map([], row_to_run)?
                .collect::<rusqlite::Result<Vec<_>>>()?
        };
        Ok(mapped)
    }

    pub fn interrupted_preparation_runs(&self) -> Result<Vec<RunRecord>> {
        let connection = self.connection()?;
        let mut statement = connection.prepare(
            "SELECT id,source_path,status,dry_run,summary_json,manifest_path,report_path,worker_upload_id,error_code,created_at,updated_at FROM runs WHERE status IN ('discovering','converting') ORDER BY created_at",
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
    restrict_file(path)
}

fn sqlite_sidecar_path(path: &Path, suffix: &str) -> PathBuf {
    let mut value = OsString::from(path.as_os_str());
    value.push(suffix);
    PathBuf::from(value)
}

fn restrict_sqlite_files(path: &Path) -> Result<()> {
    restrict_file(path)?;
    for suffix in ["-wal", "-shm", "-journal"] {
        let sidecar = sqlite_sidecar_path(path, suffix);
        if sidecar.exists() {
            restrict_file(&sidecar)?;
        }
    }
    Ok(())
}

#[cfg(unix)]
fn restrict_file(path: &Path) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(path, fs::Permissions::from_mode(0o600))?;
    Ok(())
}

#[cfg(not(unix))]
fn restrict_file(_path: &Path) -> Result<()> {
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

    #[cfg(unix)]
    #[test]
    fn database_and_sqlite_sidecars_are_owner_only() {
        use std::os::unix::fs::PermissionsExt;

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
