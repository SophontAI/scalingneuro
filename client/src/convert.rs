use std::{
    collections::BTreeSet,
    ffi::OsString,
    fs,
    io::Read,
    path::{Path, PathBuf},
    process::{Command, Stdio},
};

use anyhow::{Context, Result, bail};
use serde_json::Value;
use tempfile::TempDir;

use crate::{PINNED_DCM2NIIX_VERSION, dicom::SeriesGroup, model::ConversionProvenance};

pub const CONVERSION_ARGUMENTS: &[&str] = &[
    "-b", "y", // create vendor-normalized JSON metadata
    "-ba", "y", // ask dcm2niix to anonymize its JSON before we whitelist it
    "-g", "i", // ignore machine-local defaults
    "-i", "n", // classification is done explicitly by neuro-sync
    "-l", "o", // retain original datatype and scaling
    "-m", "2", // dcm2niix's modality-aware merge behavior
    "-p", "y", // Philips precise rather than display scaling
    "-t", "n", // never emit patient-detail text notes
    "-x", "i", // neither crop nor rotate to canonical space
    "-z", "n", // deterministic compression happens after header scrubbing
];

#[derive(Debug, Clone)]
pub struct Converter {
    pub executable: PathBuf,
    pub version: String,
}

#[derive(Debug)]
pub struct ConvertedSeries {
    _workspace: TempDir,
    pub images: Vec<ConvertedImage>,
    pub provenance: ConversionProvenance,
}

#[derive(Debug)]
pub struct ConvertedImage {
    pub nifti_path: PathBuf,
    pub metadata_path: Option<PathBuf>,
    pub metadata: Value,
}

impl Converter {
    pub fn discover(work_root: &Path) -> Result<Self> {
        let executable = resolve_executable().context(
            "dcm2niix was not found; install the release bundle intact or set NEURO_SYNC_DCM2NIIX",
        )?;
        let version = read_version(&executable)?;
        let allow_unpinned =
            std::env::var("NEURO_SYNC_ALLOW_UNPINNED_DCM2NIIX").is_ok_and(|value| value == "1");
        if version != PINNED_DCM2NIIX_VERSION && !allow_unpinned {
            bail!(
                "unsupported dcm2niix version ({version}); this client requires {PINNED_DCM2NIIX_VERSION}"
            );
        }
        fs::create_dir_all(work_root)?;
        Ok(Self {
            executable,
            version,
        })
    }

    pub fn convert(&self, group: &SeriesGroup, work_root: &Path) -> Result<ConvertedSeries> {
        let workspace = tempfile::Builder::new()
            .prefix("conversion-")
            .tempdir_in(work_root)
            .context("could not create a private conversion workspace")?;
        let input = workspace.path().join("input");
        let output = workspace.path().join("output");
        fs::create_dir(&input)?;
        fs::create_dir(&output)?;
        stage_series(&group.files, &input)?;

        let mut command = Command::new(&self.executable);
        command.args(CONVERSION_ARGUMENTS);
        command.args([OsString::from("-f"), OsString::from("series")]);
        command.args([OsString::from("-o"), output.as_os_str().to_owned()]);
        command.arg(&input);
        command.env_remove("DCM2NIIX_SEARCH_URL");
        let result = command.output().context("failed to launch dcm2niix")?;
        if !result.status.success() {
            // Converter output can contain source paths and unredacted DICOM values.
            // Deliberately report only the exit status.
            bail!("dcm2niix conversion failed with status {}", result.status);
        }

        let mut nifti_paths = Vec::new();
        for entry in fs::read_dir(&output)? {
            let path = entry?.path();
            if path.extension().and_then(|value| value.to_str()) == Some("nii") {
                nifti_paths.push(path);
            }
        }
        nifti_paths.sort();
        let mut images = Vec::with_capacity(nifti_paths.len());
        for nifti_path in nifti_paths {
            let metadata_path = nifti_path.with_extension("json");
            let (metadata_path, metadata) = if metadata_path.is_file() {
                let bytes = fs::read(&metadata_path)?;
                let metadata: Value = serde_json::from_slice(&bytes)
                    .context("dcm2niix emitted malformed JSON metadata")?;
                (Some(metadata_path), metadata)
            } else {
                (None, Value::Object(Default::default()))
            };
            images.push(ConvertedImage {
                nifti_path,
                metadata_path,
                metadata,
            });
        }
        let mut arguments: Vec<String> = CONVERSION_ARGUMENTS.iter().map(|s| (*s).into()).collect();
        arguments.extend(["-f".into(), "series".into()]);
        Ok(ConvertedSeries {
            _workspace: workspace,
            images,
            provenance: ConversionProvenance {
                client_version: crate::CLIENT_VERSION.into(),
                converter: "dcm2niix".into(),
                converter_version: PINNED_DCM2NIIX_VERSION.into(),
                arguments,
            },
        })
    }
}

fn resolve_executable() -> Option<PathBuf> {
    if let Some(path) = std::env::var_os("NEURO_SYNC_DCM2NIIX").map(PathBuf::from) {
        if path.is_file() {
            return Some(path);
        }
    }
    let executable_name = if cfg!(windows) {
        "dcm2niix.exe"
    } else {
        "dcm2niix"
    };
    if let Ok(current) = std::env::current_exe() {
        if let Some(directory) = current.parent() {
            for candidate in [
                directory.join("libexec").join(executable_name),
                directory.join(executable_name),
            ] {
                if candidate.is_file() {
                    return Some(candidate);
                }
            }
        }
    }
    std::env::var_os("PATH").and_then(|path| {
        std::env::split_paths(&path)
            .map(|directory| directory.join(executable_name))
            .find(|candidate| candidate.is_file())
    })
}

fn read_version(executable: &Path) -> Result<String> {
    const MAX_VERSION_OUTPUT_BYTES: u64 = 64 * 1024;
    let mut child = Command::new(executable)
        .arg("--version")
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .with_context(|| format!("failed to inspect converter at {}", executable.display()))?;
    let stdout = child
        .stdout
        .take()
        .context("converter stdout was unavailable")?;
    let stderr = child
        .stderr
        .take()
        .context("converter stderr was unavailable")?;
    let stdout_reader =
        std::thread::spawn(move || read_bounded_output(stdout, MAX_VERSION_OUTPUT_BYTES));
    let stderr_reader =
        std::thread::spawn(move || read_bounded_output(stderr, MAX_VERSION_OUTPUT_BYTES));
    let status = child
        .wait()
        .context("could not wait for dcm2niix --version")?;
    let stdout = stdout_reader
        .join()
        .map_err(|_| anyhow::anyhow!("converter stdout reader stopped unexpectedly"))??;
    let stderr = stderr_reader
        .join()
        .map_err(|_| anyhow::anyhow!("converter stderr reader stopped unexpectedly"))??;
    let text = format!(
        "{} {}",
        String::from_utf8_lossy(&stdout),
        String::from_utf8_lossy(&stderr)
    );
    extract_version_token(&text).with_context(|| {
        format!("dcm2niix --version ({status}) did not report one unambiguous version token")
    })
}

fn read_bounded_output(reader: impl Read, maximum: u64) -> Result<Vec<u8>> {
    let mut bytes = Vec::new();
    reader.take(maximum + 1).read_to_end(&mut bytes)?;
    if bytes.len() as u64 > maximum {
        bail!("dcm2niix version output exceeded the safety limit");
    }
    Ok(bytes)
}

fn extract_version_token(text: &str) -> Option<String> {
    let tokens: BTreeSet<String> = text
        .split(|character: char| {
            character.is_ascii_whitespace() || matches!(character, ',' | ';' | '(' | ')')
        })
        .filter(|token| {
            let Some(version) = token.strip_prefix('v') else {
                return false;
            };
            let mut pieces = version.split('.');
            matches!(
                (pieces.next(), pieces.next(), pieces.next(), pieces.next()),
                (Some(major), Some(minor), Some(date), None)
                    if !major.is_empty()
                        && major.bytes().all(|byte| byte.is_ascii_digit())
                        && !minor.is_empty()
                        && minor.bytes().all(|byte| byte.is_ascii_digit())
                        && date.len() == 8
                        && date.bytes().all(|byte| byte.is_ascii_digit())
            )
        })
        .map(str::to_owned)
        .collect();
    (tokens.len() == 1)
        .then(|| tokens.into_iter().next())
        .flatten()
}

fn stage_series(files: &[PathBuf], target: &Path) -> Result<()> {
    if files.is_empty() {
        bail!("series contains no DICOM files");
    }
    for (index, source) in files.iter().enumerate() {
        let destination = target.join(format!("{index:08}.dcm"));
        if fs::hard_link(source, &destination).is_err() {
            fs::copy(source, &destination).with_context(|| {
                format!("could not stage a DICOM file from {}", source.display())
            })?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn conversion_contract_preserves_native_space_and_scaling() {
        assert!(
            CONVERSION_ARGUMENTS
                .windows(2)
                .any(|pair| pair == ["-x", "i"])
        );
        assert!(
            CONVERSION_ARGUMENTS
                .windows(2)
                .any(|pair| pair == ["-l", "o"])
        );
        assert!(
            CONVERSION_ARGUMENTS
                .windows(2)
                .any(|pair| pair == ["-p", "y"])
        );
        assert!(
            CONVERSION_ARGUMENTS
                .windows(2)
                .any(|pair| pair == ["-ba", "y"])
        );
    }

    #[test]
    fn version_parser_ignores_platform_banner_and_warnings() {
        let banner = "Chris Rorden's dcm2niix version v1.0.20260416  Clang ARM64 (64-bit MacOS)\npigz not found";
        assert_eq!(
            extract_version_token(banner).as_deref(),
            Some("v1.0.20260416")
        );
        assert!(extract_version_token("v1.0.20260416 v1.0.20250101").is_none());
    }

    #[cfg(unix)]
    #[test]
    fn official_nonzero_version_exit_is_accepted_when_token_is_exact() {
        use std::os::unix::fs::PermissionsExt;

        let directory = tempfile::tempdir().unwrap();
        let executable = directory.path().join("dcm2niix");
        fs::write(
            &executable,
            "#!/bin/sh\nprintf '%s\\n' 'dcm2niix version v1.0.20260416' >&2\nexit 3\n",
        )
        .unwrap();
        fs::set_permissions(&executable, fs::Permissions::from_mode(0o700)).unwrap();
        assert_eq!(read_version(&executable).unwrap(), "v1.0.20260416");
    }
}
