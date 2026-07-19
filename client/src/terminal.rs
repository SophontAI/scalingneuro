use std::{
    io::{self, BufRead, BufReader, IsTerminal, Write},
    path::{Path, PathBuf},
};

use anyhow::{Context, Result, bail};

use crate::{
    DEFAULT_API_URL,
    config::ClientConfig,
    pipeline::{ContributorDetails, Runtime},
};

const POLICY_SUMMARY: &str = "This beta accepts only confidently identified functional 4D EPI. Source DICOMs and structural, diffusion, ASL, field-map, reference, derived, or uncertain series stay local.\nUploaded bundles contain native-space NIfTI voxels, an allowlisted acquisition-metadata sidecar, site-scoped pseudonyms, QC/provenance, and hashes. Direct identifiers, dates, DICOM UIDs, institution/station/operator fields, source names and paths, free text, and private-tag dumps are prohibited.\nYou must be authorized by your institution to contribute the selected scans for research storage, validation, preservation, scientific analysis, and governed sharing. This confirmation does not replace participant consent, IRB review, a data-use agreement, or institutional review. Scaling Neuro may validate, deduplicate, quarantine, preserve, and analyze accepted bundles. A lab may request device revocation or upload withdrawal through admin@sophont.med using the pseudonymous upload ID in its local report.";

pub async fn run(runtime: Runtime) -> Result<()> {
    run_for_optional_folder(runtime, None).await
}

pub async fn run_for_folder(runtime: Runtime, folder: PathBuf) -> Result<()> {
    let folder = folder
        .canonicalize()
        .with_context(|| format!("could not open selected folder: {}", folder.display()))?;
    if !folder.is_dir() {
        bail!("selected source is not a folder");
    }
    run_for_optional_folder(runtime, Some(folder)).await
}

async fn run_for_optional_folder(runtime: Runtime, folder: Option<PathBuf>) -> Result<()> {
    if !io::stdin().is_terminal() || !io::stdout().is_terminal() {
        bail!(
            "interactive setup needs a terminal. Run neuro-sync in a terminal, or use `neuro-sync register --help` and `neuro-sync upload --help` for non-interactive operation"
        );
    }

    let mut input = BufReader::new(io::stdin());
    let mut output = io::stdout();
    run_with_io(runtime, &mut input, &mut output, DEFAULT_API_URL, folder).await
}

async fn run_with_io(
    runtime: Runtime,
    input: &mut impl BufRead,
    output: &mut impl Write,
    api_url: &str,
    selected_folder: Option<PathBuf>,
) -> Result<()> {
    writeln!(output, "Scaling Neuro · functional EPI contribution")?;
    writeln!(output, "No browser is needed or opened.\n")?;

    let config = if runtime.paths.config.is_file() {
        ClientConfig::load(&runtime.paths)?
    } else {
        let Some(config) = register_interactively(&runtime, input, output, api_url).await? else {
            writeln!(output, "Registration cancelled. Nothing was uploaded.")?;
            return Ok(());
        };
        config
    };

    writeln!(output, "Project: {}", config.project_name)?;
    writeln!(
        output,
        "Contribution policy: {}\n",
        config.consent_policy_version
    )?;

    let folder = match selected_folder {
        Some(folder) => folder,
        None => prompt_folder(input, output)?,
    };
    if !confirm_upload(input, output, &folder, &config.consent_policy_version)? {
        writeln!(output, "Cancelled. Nothing was uploaded.")?;
        return Ok(());
    }

    writeln!(output, "\nValidating {}…", folder.display())?;
    let run_id = runtime.upload(folder, false).await?;
    print_run_summary(&runtime, &run_id, output)
}

async fn register_interactively(
    runtime: &Runtime,
    input: &mut impl BufRead,
    output: &mut impl Write,
    api_url: &str,
) -> Result<Option<ClientConfig>> {
    writeln!(output, "One-time lab registration")?;
    writeln!(output, "Connecting to Scaling Neuro…")?;
    output.flush()?;
    let contribution = runtime.contribution_info(api_url).await?;
    if !contribution.registration_open {
        bail!("public contribution registration is temporarily paused");
    }

    writeln!(output, "Project: {}", contribution.project_name)?;
    writeln!(
        output,
        "Policy: {} ({})",
        contribution.consent_policy_version, contribution.policy_url
    )?;
    writeln!(output, "{POLICY_SUMMARY}\n")?;

    let contact_name = prompt_required(input, output, "Your name")?;
    let contact_email = prompt_required(input, output, "Work email")?;
    let institution_name = prompt_required(input, output, "Institution")?;
    let lab_name = prompt_required(input, output, "Lab or research group")?;
    let ror = prompt_optional(input, output, "ROR ID (optional)")?;
    let contact_opt_in = prompt_yes_no(
        input,
        output,
        "May Scaling Neuro contact you about this contribution or research collaboration?",
        false,
    )?;

    writeln!(output, "\n{POLICY_SUMMARY}")?;
    if !prompt_yes_no(
        input,
        output,
        "Do you accept this policy and confirm you are authorized to contribute eligible scans?",
        false,
    )? {
        return Ok(None);
    }

    writeln!(output, "Registering this workstation…")?;
    output.flush()?;
    let config = runtime
        .register(
            ContributorDetails {
                contact_email,
                contact_name,
                institution_name,
                institution_ror_id: ror,
                lab_name,
                contact_opt_in,
            },
            contribution.consent_policy_version,
            api_url,
            crate::cli::default_device_name(),
        )
        .await?;
    writeln!(
        output,
        "Registered. This workstation now has a private upload identity.\n"
    )?;
    Ok(Some(config))
}

pub fn confirm_authorized_upload(folder: &Path, policy_version: &str) -> Result<bool> {
    if !io::stdin().is_terminal() || !io::stdout().is_terminal() {
        bail!(
            "non-interactive upload requires --confirm-authorized; use it only after confirming the selected scans are approved under the contribution policy"
        );
    }
    let mut input = BufReader::new(io::stdin());
    let mut output = io::stdout();
    confirm_upload(&mut input, &mut output, folder, policy_version)
}

fn confirm_upload(
    input: &mut impl BufRead,
    output: &mut impl Write,
    folder: &Path,
    policy_version: &str,
) -> Result<bool> {
    writeln!(output, "Selected folder: {}", folder.display())?;
    writeln!(output, "Policy: {policy_version}")?;
    writeln!(output, "{POLICY_SUMMARY}")?;
    prompt_yes_no(
        input,
        output,
        "Confirm this folder is institutionally approved and begin upload?",
        false,
    )
}

pub fn print_run_summary(runtime: &Runtime, run_id: &str, output: &mut impl Write) -> Result<()> {
    let run = runtime
        .run_record(Some(run_id))?
        .context("run state is missing")?;
    writeln!(output, "\nRun: {run_id}")?;
    writeln!(output, "Status: {}", run.status)?;
    writeln!(
        output,
        "Series: {} accepted, {} held, {} excluded",
        run.summary.accepted, run.summary.held, run.summary.excluded
    )?;
    if let Some(report) = run.report_path {
        writeln!(output, "Report: {report}")?;
    }
    Ok(())
}

fn prompt_folder(input: &mut impl BufRead, output: &mut impl Write) -> Result<PathBuf> {
    loop {
        let raw = prompt_required(
            input,
            output,
            "DICOM folder (type or paste the completed export path)",
        )?;
        let path = normalize_folder_input(&raw);
        match path.canonicalize() {
            Ok(path) if path.is_dir() => return Ok(path),
            Ok(_) => writeln!(output, "That path is not a folder. Try again.")?,
            Err(_) => writeln!(output, "That folder could not be found. Try again.")?,
        }
    }
}

fn prompt_required(
    input: &mut impl BufRead,
    output: &mut impl Write,
    label: &str,
) -> Result<String> {
    loop {
        let value = prompt(input, output, label)?;
        if !value.is_empty() {
            return Ok(value);
        }
        writeln!(output, "A value is required.")?;
    }
}

fn prompt_optional(
    input: &mut impl BufRead,
    output: &mut impl Write,
    label: &str,
) -> Result<Option<String>> {
    Ok(match prompt(input, output, label)? {
        value if value.is_empty() => None,
        value => Some(value),
    })
}

fn prompt_yes_no(
    input: &mut impl BufRead,
    output: &mut impl Write,
    label: &str,
    default: bool,
) -> Result<bool> {
    let hint = if default { "Y/n" } else { "y/N" };
    loop {
        let value = prompt(input, output, &format!("{label} [{hint}]"))?;
        match value.to_ascii_lowercase().as_str() {
            "" => return Ok(default),
            "y" | "yes" => return Ok(true),
            "n" | "no" => return Ok(false),
            _ => writeln!(output, "Enter yes or no.")?,
        }
    }
}

fn prompt(input: &mut impl BufRead, output: &mut impl Write, label: &str) -> Result<String> {
    write!(output, "{label}: ")?;
    output.flush()?;
    let mut value = String::new();
    if input.read_line(&mut value)? == 0 {
        bail!("terminal input closed before setup completed");
    }
    Ok(value.trim().to_owned())
}

fn normalize_folder_input(value: &str) -> PathBuf {
    let exact = PathBuf::from(value);
    if exact.exists() {
        return exact;
    }

    let unquoted = value
        .strip_prefix('"')
        .and_then(|value| value.strip_suffix('"'))
        .or_else(|| {
            value
                .strip_prefix('\'')
                .and_then(|value| value.strip_suffix('\''))
        })
        .unwrap_or(value);
    let unquoted_path = PathBuf::from(unquoted);
    if unquoted_path.exists() {
        return unquoted_path;
    }

    #[cfg(unix)]
    {
        let mut unescaped = String::with_capacity(unquoted.len());
        let mut characters = unquoted.chars();
        while let Some(character) = characters.next() {
            if character == '\\' {
                if let Some(next) = characters.next() {
                    unescaped.push(next);
                } else {
                    unescaped.push(character);
                }
            } else {
                unescaped.push(character);
            }
        }
        let unescaped_path = PathBuf::from(unescaped);
        if unescaped_path.exists() {
            return unescaped_path;
        }
    }

    unquoted_path
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn required_prompt_retries_empty_input() {
        let mut input = "\nResearcher Name\n".as_bytes();
        let mut output = Vec::new();
        assert_eq!(
            prompt_required(&mut input, &mut output, "Name").unwrap(),
            "Researcher Name"
        );
        assert!(
            String::from_utf8(output)
                .unwrap()
                .contains("A value is required.")
        );
    }

    #[test]
    fn yes_no_prompt_is_explicit_and_retries_invalid_input() {
        let mut input = "maybe\nyes\n".as_bytes();
        let mut output = Vec::new();
        assert!(prompt_yes_no(&mut input, &mut output, "Continue?", false).unwrap());
        assert!(
            String::from_utf8(output)
                .unwrap()
                .contains("Enter yes or no.")
        );
    }

    #[test]
    fn folder_prompt_accepts_quoted_paths_with_spaces() {
        let directory = tempdir().unwrap();
        let folder = directory.path().join("DICOM export");
        std::fs::create_dir(&folder).unwrap();
        let raw = format!("\"{}\"\n", folder.display());
        let mut input = raw.as_bytes();
        let mut output = Vec::new();
        assert_eq!(
            prompt_folder(&mut input, &mut output).unwrap(),
            folder.canonicalize().unwrap()
        );
    }

    #[cfg(unix)]
    #[test]
    fn folder_prompt_accepts_terminal_drag_style_escaping() {
        let directory = tempdir().unwrap();
        let folder = directory.path().join("DICOM export");
        std::fs::create_dir(&folder).unwrap();
        let raw = format!("{}\n", folder.display().to_string().replace(' ', "\\ "));
        let mut input = raw.as_bytes();
        let mut output = Vec::new();
        assert_eq!(
            prompt_folder(&mut input, &mut output).unwrap(),
            folder.canonicalize().unwrap()
        );
    }

    #[tokio::test]
    async fn first_run_registration_and_folder_confirmation_stay_in_terminal() {
        use axum::{
            Json, Router,
            http::StatusCode,
            routing::{get, post},
        };

        let app = Router::new()
            .route(
                "/v1/contribution",
                get(|| async {
                    Json(serde_json::json!({
                        "registration_open": true,
                        "project_name": "Scaling Neuro public EPI contribution",
                        "consent_policy_version": "open-epi-1.0.0",
                        "policy_url": "https://scalingneuro.com/docs/contribution-policy",
                        "self_service_quota_bytes": 10737418240_u64,
                        "minimum_client_version": "0.2.2"
                    }))
                }),
            )
            .route(
                "/v1/register",
                post(|Json(body): Json<serde_json::Value>| async move {
                    assert_eq!(body["contact_email"], "researcher@example.edu");
                    assert_eq!(body["institution_name"], "Example University");
                    assert_eq!(body["lab_name"], "Example Lab");
                    assert_eq!(body["contact_opt_in"], false);
                    assert_eq!(body["accepted_consent_policy_version"], "open-epi-1.0.0");
                    (
                        StatusCode::CREATED,
                        Json(serde_json::json!({
                            "enrollment_id": body["registration_id"],
                            "device_token": body["device_token"],
                            "device_id": "11111111-1111-4111-8111-111111111111",
                            "site_id": "22222222-2222-4222-8222-222222222222",
                            "project_id": "33333333-3333-4333-8333-333333333333",
                            "project_name": "Scaling Neuro public EPI contribution",
                            "consent_policy_version": "open-epi-1.0.0",
                            "pseudonym_key_b64": "AAECAwQFBgcICQoLDA0ODxAREhMUFRYXGBkaGxwdHh8="
                        })),
                    )
                }),
            );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });

        let directory = tempdir().unwrap();
        let source = directory.path().join("DICOM export");
        std::fs::create_dir(&source).unwrap();
        let state = directory.path().join("state");
        let runtime = Runtime::initialize(Some(&state)).unwrap();
        let responses = "Researcher Name\nresearcher@example.edu\nExample University\nExample Lab\n\nno\nyes\nno\n";
        let mut input = responses.as_bytes();
        let mut output = Vec::new();

        run_with_io(
            runtime.clone(),
            &mut input,
            &mut output,
            &format!("http://{address}"),
            Some(source.canonicalize().unwrap()),
        )
        .await
        .unwrap();

        let output = String::from_utf8(output).unwrap();
        assert!(output.contains("No browser is needed or opened."));
        assert!(output.contains("Registered. This workstation now has a private upload identity."));
        assert!(output.contains("Selected folder:"));
        assert!(!output.contains("DICOM folder (type or paste"));
        assert!(output.contains("Cancelled. Nothing was uploaded."));
        assert!(ClientConfig::load(&runtime.paths).is_ok());
        server.abort();
    }
}
