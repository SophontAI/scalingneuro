use std::{
    collections::BTreeMap,
    fs::{self, File, OpenOptions},
    io::{Read, Write},
    path::{Component, Path, PathBuf},
};

use anyhow::{Context, Result, bail};
use chrono::Utc;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};
use walkdir::WalkDir;

use crate::{
    archive::{
        DICOM_MANIFEST_SCHEMA_VERSION, DICOM_METADATA_POLICY_ID, DICOM_METADATA_POLICY_VERSION,
        FUNCTIONAL_EPI_ARCHIVE_ROUTE, SCANNER_NATIVE_NOT_DEFACED_PIXEL_POLICY,
    },
    model::{LocalManifest, ManifestBundle, ReportBundle, RunReport, SourceSummary},
    privacy,
};

pub const REVIEW_PACKAGE_SCHEMA_VERSION: &str = "1.0.0";
const INTERNAL_DIRECTORY: &str = ".neuro-sync";
const PACKAGE_FILENAME: &str = "review-package.json";
const PUBLIC_REPORT_FILENAME: &str = "preparation-report.json";
const README_FILENAME: &str = "README.txt";
const MAX_ARCHIVE_MANIFEST_BYTES: u64 = 16 * 1024 * 1024;

#[derive(Debug, Clone, Serialize, Deserialize)]
struct ReviewPackage {
    schema_version: String,
    created_at: String,
    preparation_client_version: String,
    site_id: String,
    project_id: String,
    source_summary: SourceSummary,
    series: Vec<ReviewSeries>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct ReviewSeries {
    series_id: String,
    dicom_files: u64,
    folder: String,
}

#[derive(Debug)]
pub struct ReviewSummary {
    pub dicom_files: u64,
    pub prepared_series: u64,
}

#[derive(Debug, Clone, Eq, PartialEq)]
struct ArchivedFile {
    size: u64,
    sha256: String,
}

#[derive(Debug)]
struct ArchiveContents {
    manifest_bytes: Vec<u8>,
    files: BTreeMap<String, ArchivedFile>,
}

pub fn package_path(root: &Path) -> PathBuf {
    root.join(INTERNAL_DIRECTORY).join(PACKAGE_FILENAME)
}

pub fn is_review_folder(root: &Path) -> bool {
    package_path(root).is_file() && root.join("series").is_dir()
}

pub fn write_review_package(
    root: &Path,
    source_manifest: &LocalManifest,
    report: &mut RunReport,
) -> Result<ReviewSummary> {
    if !root.is_dir() {
        bail!("local review package destination is not a folder");
    }
    let internal = root.join(INTERNAL_DIRECTORY);
    let series_root = root.join("series");
    fs::create_dir(&internal)?;
    fs::create_dir(&series_root)?;
    privacy::restrict_dir(&internal)?;
    privacy::restrict_dir(&series_root)?;

    let mut series = Vec::new();
    let mut dicom_files = 0_u64;
    for bundle in &source_manifest.bundles {
        validate_bundle_identity(bundle)?;
        let archive = bundle
            .archive
            .as_ref()
            .context("review package bundle has no DICOM archive")?;
        let review_series = series_root.join(&bundle.bundle_id);
        fs::create_dir(&review_series)?;
        privacy::restrict_dir(&review_series)?;
        let contents = read_archive(Path::new(&archive.object.local_path), &review_series)?;
        validate_archive_contents(bundle, &contents)?;
        series.push(ReviewSeries {
            series_id: bundle.bundle_id.clone(),
            dicom_files: archive.dicom_instance_count,
            folder: format!("series/{}", bundle.bundle_id),
        });
        dicom_files = dicom_files
            .checked_add(archive.dicom_instance_count)
            .context("review package DICOM count overflow")?;
    }

    let package = ReviewPackage {
        schema_version: REVIEW_PACKAGE_SCHEMA_VERSION.into(),
        created_at: Utc::now().to_rfc3339(),
        preparation_client_version: source_manifest.client_version.clone(),
        site_id: source_manifest.site_id.clone(),
        project_id: source_manifest.project_id.clone(),
        source_summary: source_manifest.source_summary.clone(),
        series,
    };
    write_json(&package_path(root), &package)?;

    report.status = "ready_for_review".into();
    report.completed_at = Some(Utc::now().to_rfc3339());
    report.bundles = source_manifest
        .bundles
        .iter()
        .map(ReportBundle::from)
        .collect();
    report.worker_upload_id = None;
    report.worker_upload_ids.clear();
    report.archive_commit_count = 0;
    write_json(&root.join(PUBLIC_REPORT_FILENAME), report)?;
    write_readme(root)?;
    restrict_review_tree(root)?;
    inspect_review_folder(root).and_then(|summary| {
        if summary.dicom_files != dicom_files {
            bail!("local review DICOM count changed while the folder was finalized");
        }
        Ok(summary)
    })
}

pub fn inspect_review_folder(root: &Path) -> Result<ReviewSummary> {
    let marker = package_path(root);
    let bytes = fs::read(&marker)
        .with_context(|| format!("could not read review package {}", marker.display()))?;
    let package: ReviewPackage =
        serde_json::from_slice(&bytes).context("local review package metadata is invalid")?;
    if package.schema_version != REVIEW_PACKAGE_SCHEMA_VERSION {
        bail!("local review package uses an unsupported schema version");
    }
    let mut dicom_files = 0_u64;
    for entry in WalkDir::new(root.join("series")).follow_links(false) {
        let entry = entry?;
        if entry.file_type().is_file()
            && entry
                .path()
                .extension()
                .and_then(|extension| extension.to_str())
                .is_some_and(|extension| extension.eq_ignore_ascii_case("dcm"))
        {
            dicom_files = dicom_files
                .checked_add(1)
                .context("review folder DICOM count overflow")?;
        }
    }
    Ok(ReviewSummary {
        dicom_files,
        prepared_series: package.series.len() as u64,
    })
}

fn validate_bundle_identity(bundle: &ManifestBundle) -> Result<()> {
    let archive = bundle
        .archive
        .as_ref()
        .context("local review package bundle has no DICOM archive")?;
    if archive.deidentification_profile != DICOM_METADATA_POLICY_ID
        || archive.deidentification_profile_version != DICOM_METADATA_POLICY_VERSION
        || bundle.series_kind != "functional_epi"
        || bundle.archive_route != FUNCTIONAL_EPI_ARCHIVE_ROUTE
        || bundle.pixel_data_policy != SCANNER_NATIVE_NOT_DEFACED_PIXEL_POLICY
        || archive.dicom_instance_count != bundle.source_dicom_count
    {
        bail!("local review package contains an invalid functional EPI bundle");
    }
    Ok(())
}

fn validate_archive_contents(bundle: &ManifestBundle, contents: &ArchiveContents) -> Result<()> {
    let manifest: Value = serde_json::from_slice(&contents.manifest_bytes)
        .context("DICOM archive manifest is invalid")?;
    let archive = bundle
        .archive
        .as_ref()
        .context("DICOM bundle has no archive metadata")?;
    for (field, expected) in [
        ("schema_version", DICOM_MANIFEST_SCHEMA_VERSION),
        ("series_archive_id", bundle.bundle_id.as_str()),
        ("series_id", bundle.series_id.as_str()),
        ("subject_id", bundle.subject_id.as_str()),
        ("session_id", bundle.session_id.as_str()),
        ("protocol_group_id", bundle.protocol_group_id.as_str()),
        ("series_kind", "functional_epi"),
        ("archive_route", FUNCTIONAL_EPI_ARCHIVE_ROUTE),
        ("pixel_data_policy", SCANNER_NATIVE_NOT_DEFACED_PIXEL_POLICY),
    ] {
        if manifest.get(field).and_then(Value::as_str) != Some(expected) {
            bail!("DICOM archive manifest field {field} does not match the review package");
        }
    }
    let instances = manifest
        .get("instances")
        .and_then(Value::as_array)
        .context("DICOM archive manifest omitted its instance inventory")?;
    if instances.len() as u64 != archive.dicom_instance_count
        || contents.files.len() != instances.len()
    {
        bail!("DICOM archive instance inventory is incomplete");
    }
    let mut expected = BTreeMap::new();
    for instance in instances {
        let path = instance
            .get("path")
            .and_then(Value::as_str)
            .context("DICOM archive instance path is invalid")?;
        validate_dicom_archive_path(path)?;
        let size = instance
            .get("size_bytes")
            .and_then(Value::as_u64)
            .context("DICOM archive instance size is invalid")?;
        let sha256 = instance
            .get("sha256")
            .and_then(Value::as_str)
            .context("DICOM archive instance hash is invalid")?
            .to_ascii_lowercase();
        if expected
            .insert(path.to_owned(), ArchivedFile { size, sha256 })
            .is_some()
        {
            bail!("DICOM archive repeats an instance path");
        }
    }
    if expected != contents.files {
        bail!("DICOM archive bytes do not match its instance inventory");
    }
    Ok(())
}

fn read_archive(path: &Path, extraction_root: &Path) -> Result<ArchiveContents> {
    let decoder = zstd::stream::read::Decoder::new(File::open(path)?)
        .context("could not open prepared DICOM archive")?;
    let mut archive = tar::Archive::new(decoder);
    let mut manifest_bytes = None;
    let mut files = BTreeMap::new();
    let mut buffer = vec![0_u8; 1024 * 1024];
    for entry in archive
        .entries()
        .context("could not read prepared DICOM archive")?
    {
        let mut entry = entry.context("prepared DICOM archive contains an invalid entry")?;
        if !entry.header().entry_type().is_file() {
            bail!("prepared DICOM archive contains a non-file entry");
        }
        let entry_path = entry
            .path()
            .context("prepared DICOM archive contains an invalid path")?;
        let entry_path = path_to_portable_string(&entry_path)?;
        if entry_path == "manifest.json" {
            if manifest_bytes.is_some() || entry.size() > MAX_ARCHIVE_MANIFEST_BYTES {
                bail!("prepared DICOM archive manifest is duplicated or too large");
            }
            let mut bytes = Vec::with_capacity(entry.size() as usize);
            entry.read_to_end(&mut bytes)?;
            manifest_bytes = Some(bytes);
            continue;
        }
        validate_dicom_archive_path(&entry_path)?;
        let destination = extraction_root.join(&entry_path);
        let parent = destination
            .parent()
            .context("review DICOM destination has no parent")?;
        fs::create_dir_all(parent)?;
        privacy::restrict_dir(parent)?;
        let mut output = open_new_file(&destination)?;
        let mut digest = Sha256::new();
        let mut size = 0_u64;
        loop {
            let read = entry.read(&mut buffer)?;
            if read == 0 {
                break;
            }
            output.write_all(&buffer[..read])?;
            digest.update(&buffer[..read]);
            size = size
                .checked_add(read as u64)
                .context("review DICOM size overflow")?;
        }
        output.flush()?;
        privacy::restrict_file(&destination)?;
        if files
            .insert(
                entry_path,
                ArchivedFile {
                    size,
                    sha256: hex::encode(digest.finalize()),
                },
            )
            .is_some()
        {
            bail!("prepared DICOM archive repeats an instance path");
        }
    }
    Ok(ArchiveContents {
        manifest_bytes: manifest_bytes.context("prepared DICOM archive has no manifest")?,
        files,
    })
}

fn open_new_file(path: &Path) -> Result<File> {
    let mut options = OpenOptions::new();
    options.create_new(true).write(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    options
        .open(path)
        .with_context(|| format!("could not create review file {}", path.display()))
}

fn write_new_file(path: &Path, bytes: &[u8]) -> Result<()> {
    let mut file = open_new_file(path)?;
    file.write_all(bytes)?;
    file.flush()?;
    privacy::restrict_file(path)
}

fn write_json(path: &Path, value: &impl Serialize) -> Result<()> {
    let mut bytes = serde_json::to_vec_pretty(value)?;
    bytes.push(b'\n');
    write_new_file(path, &bytes)
}

fn write_readme(root: &Path) -> Result<()> {
    let readme = b"Scaling Neuro local review folder\n\
\n\
Nothing in this folder has been uploaded.\n\
\n\
Inspect or edit the deidentified DICOM files under\n\
series/<series-id>/dicom/. The original source folder is unchanged. Pixel Data\n\
is preserved exactly as exported by the scanner and is not defaced, cropped,\n\
masked, or resampled, so recognizable visual features may remain.\n\
\n\
preparation-report.json describes the files as initially prepared. It is not\n\
updated when you edit the DICOMs and is not used to reject researcher changes.\n\
\n\
After inspection and institutional approval, run:\n\
    neuro-sync upload /path/to/this-review-folder\n\
\n\
The upload command uses the DICOMs as they exist at that time. It rechecks\n\
functional EPI eligibility and local privacy, then builds fresh archives from\n\
the current files before syncing them.\n";
    write_new_file(&root.join(README_FILENAME), readme)
}

fn restrict_review_tree(root: &Path) -> Result<()> {
    for entry in WalkDir::new(root).follow_links(false) {
        let entry = entry?;
        if entry.file_type().is_dir() {
            privacy::restrict_dir(entry.path())?;
        } else if entry.file_type().is_file() {
            privacy::restrict_file(entry.path())?;
        }
    }
    Ok(())
}

fn validate_dicom_archive_path(path: &str) -> Result<()> {
    let Some(name) = path.strip_prefix("dicom/") else {
        bail!("prepared DICOM archive contains an unexpected path");
    };
    let Some(number) = name.strip_suffix(".dcm") else {
        bail!("prepared DICOM archive contains an unexpected path");
    };
    if number.len() != 6 || !number.bytes().all(|byte| byte.is_ascii_digit()) {
        bail!("prepared DICOM archive contains an unexpected path");
    }
    validate_relative_path(Path::new(path))
}

fn path_to_portable_string(path: &Path) -> Result<String> {
    validate_relative_path(path)?;
    let parts = path
        .components()
        .map(|component| match component {
            Component::Normal(value) => value
                .to_str()
                .map(str::to_owned)
                .context("review package path is not valid UTF-8"),
            _ => bail!("review package path is not a plain relative path"),
        })
        .collect::<Result<Vec<_>>>()?;
    Ok(parts.join("/"))
}

fn validate_relative_path(path: &Path) -> Result<()> {
    if path.as_os_str().is_empty()
        || path
            .components()
            .any(|component| !matches!(component, Component::Normal(_)))
    {
        bail!("review package path is not a plain relative path");
    }
    Ok(())
}
