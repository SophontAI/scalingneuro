use std::path::PathBuf;

use anyhow::{Context, Result, bail};
use clap::{Parser, Subcommand};

use crate::{
    DEFAULT_API_URL,
    pipeline::{ContributorDetails, Runtime},
};

#[derive(Parser)]
#[command(
    name = "neuro-sync",
    version,
    about = "Sync approved functional EPI DICOMs with Scaling Neuro",
    long_about = None,
    subcommand_precedence_over_arg = true,
    after_help = "TWO WORKFLOWS:\n  One command:  neuro-sync /path/to/dicom-export\n  Review first: neuro-sync prepare /path/to/dicom-export\n                neuro-sync upload ./dicom-export-review"
)]
pub struct Cli {
    /// Store private checkpoints and the current one-series staging archive in this directory.
    #[arg(long, global = true, env = "NEURO_SYNC_STATE_DIR")]
    pub state_dir: Option<PathBuf>,
    /// DICOM export folder to sync.
    #[arg(value_name = "DICOM_FOLDER")]
    pub folder: Option<PathBuf>,
    #[command(subcommand)]
    pub command: Option<Command>,
}

#[derive(Subcommand)]
pub enum Command {
    /// Run the guided terminal setup and folder-sync flow.
    Setup,
    /// Register this machine for shared functional EPI contribution.
    Register {
        #[arg(long)]
        email: String,
        #[arg(long)]
        name: String,
        #[arg(long)]
        institution: String,
        #[arg(long)]
        lab: String,
        #[arg(long)]
        ror: Option<String>,
        #[arg(long)]
        contact_opt_in: bool,
        /// Exact contribution-policy version reviewed and accepted by this automation.
        #[arg(long, value_name = "VERSION")]
        accept_policy_version: Option<String>,
        #[arg(long, default_value = DEFAULT_API_URL)]
        server: String,
        #[arg(long)]
        device_name: Option<String>,
    },
    /// Sync a DICOM folder, automatically continuing any checkpointed work.
    #[command(alias = "run")]
    Upload {
        /// Raw DICOM export folder or a folder created by `neuro-sync prepare`.
        folder: PathBuf,
        /// Perform every local privacy/QC step but do not contact the ingest service or R2.
        #[arg(long)]
        dry_run: bool,
        /// Confirm the specific data permit irrevocable sharing, commercial reuse, and unconditional public-domain redistribution.
        #[arg(long)]
        confirm_authorized: bool,
        /// Exact new policy version accepted when this workstation's policy is out of date.
        #[arg(long, value_name = "VERSION")]
        accept_policy_version: Option<String>,
    },
    /// Create inspectable deidentified DICOMs locally without uploading anything.
    Prepare {
        /// Raw DICOM export folder to deidentify.
        folder: PathBuf,
        /// New folder for the inspectable DICOMs. Defaults to `<source-folder>-review` in the current directory.
        #[arg(long, short, value_name = "REVIEW_FOLDER")]
        output: Option<PathBuf>,
    },
    /// Show local progress for the latest run or a specific run ID.
    Status {
        run_id: Option<String>,
        #[arg(long)]
        json: bool,
    },
    /// Print the final local report for the latest run or a specific run ID.
    Report {
        run_id: Option<String>,
        #[arg(long)]
        json: bool,
    },
}

pub async fn execute(cli: Cli) -> Result<()> {
    let runtime = Runtime::initialize(cli.state_dir.as_deref())?;
    if let Some(folder) = cli.folder {
        if cli.command.is_some() {
            bail!("a DICOM folder cannot be combined with a subcommand");
        }
        return crate::terminal::run_for_folder(runtime, folder).await;
    }
    match cli.command {
        None | Some(Command::Setup) => crate::terminal::run(runtime).await,
        Some(Command::Register {
            email,
            name,
            institution,
            lab,
            ror,
            contact_opt_in,
            accept_policy_version,
            server,
            device_name,
        }) => {
            let contribution = runtime.contribution_info(&server).await?;
            if !contribution.registration_open {
                bail!("public contribution registration is temporarily paused");
            }
            validate_explicit_policy_version(
                accept_policy_version.as_deref(),
                &contribution.consent_policy_version,
                true,
            )
            .with_context(|| {
                format!(
                    "review the contribution policy at {} before registering",
                    contribution.policy_url
                )
            })?;
            let config = runtime
                .register(
                    ContributorDetails {
                        contact_email: email,
                        contact_name: name,
                        institution_name: institution,
                        institution_ror_id: ror,
                        lab_name: lab,
                        contact_opt_in,
                    },
                    contribution.consent_policy_version,
                    &server,
                    device_name.unwrap_or_else(default_device_name),
                )
                .await?;
            println!("registered for {}", config.project_name);
            println!("contribution policy: {}", config.consent_policy_version);
            Ok(())
        }
        Some(Command::Upload {
            folder,
            dry_run,
            confirm_authorized,
            accept_policy_version,
        }) => {
            let folder = folder
                .canonicalize()
                .with_context(|| format!("could not open selected folder: {}", folder.display()))?;
            if !folder.is_dir() {
                bail!("selected source is not a folder");
            }
            let reviewed = Runtime::is_review_folder(&folder);
            if reviewed {
                let inspection = runtime.verify_review_folder(&folder)?;
                println!(
                    "Local review folder: {} current DICOM files, originally prepared from {} functional EPI series",
                    inspection.dicom_files, inspection.series
                );
            }
            if !dry_run {
                let config = crate::config::ClientConfig::load(&runtime.paths)?;
                let contribution = runtime.contribution_info(&config.api_url).await?;
                let automated_authorization = confirm_authorized;
                validate_explicit_policy_version(
                    accept_policy_version.as_deref(),
                    &contribution.consent_policy_version,
                    automated_authorization
                        && config.consent_policy_version != contribution.consent_policy_version,
                )?;
                let authorized = if automated_authorization {
                    true
                } else {
                    crate::terminal::confirm_authorized_upload(
                        &folder,
                        &config.project_name,
                        &contribution.policy_url,
                    )?
                };
                if !authorized {
                    println!("cancelled; nothing was uploaded");
                    return Ok(());
                }
                if config.consent_policy_version != contribution.consent_policy_version {
                    runtime
                        .accept_contribution_policy(&config, &contribution.consent_policy_version)
                        .await?;
                    println!("Contribution policy accepted: {}", contribution.policy_url);
                }
            }
            if reviewed {
                if dry_run {
                    println!(
                        "\nRechecking the current reviewed DICOMs in {}\nNothing will be uploaded.\n",
                        folder.display()
                    );
                } else {
                    println!("\nUploading reviewed DICOMs from {}\n", folder.display());
                }
            } else {
                println!("\nSyncing {}\n", folder.display());
            }
            let run_id = if reviewed && !dry_run {
                runtime.upload_reviewed_folder(folder).await?
            } else {
                runtime.sync_folder(folder, dry_run).await?
            };
            crate::terminal::print_run_summary(&runtime, &run_id, &mut std::io::stdout())
        }
        Some(Command::Prepare { folder, output }) => {
            if !runtime.paths.config.is_file()
                && !crate::terminal::ensure_registered_for_review(&runtime).await?
            {
                println!("Registration cancelled. Nothing was prepared or uploaded.");
                return Ok(());
            }
            println!(
                "\nPreparing local review copies from {}\nNothing will be uploaded.\n",
                folder.display()
            );
            let prepared = runtime.prepare_review_folder(folder, output).await?;
            println!("Local review package ready: {}", prepared.folder.display());
            println!(
                "Review: {} deidentified DICOM files in {} functional EPI series",
                prepared.dicom_files, prepared.series
            );
            println!("Original source DICOMs were not changed. Nothing was uploaded.");
            println!(
                "Pixel Data is scanner-native and not defaced; inspect it under {}/series.",
                prepared.folder.display()
            );
            println!("\nAfter inspection and institutional approval, run:");
            println!("  neuro-sync upload \"{}\"", prepared.folder.display());
            Ok(())
        }
        Some(Command::Status { run_id, json }) => {
            let run = runtime
                .run_record(run_id.as_deref())?
                .context("no matching run was found")?;
            if json {
                println!(
                    "{}",
                    serde_json::to_string_pretty(&crate::state::PublicRunStatus::from(&run))?
                );
            } else {
                println!("run: {}", run.id);
                println!("status: {}", run.status);
                println!(
                    "DICOM: {} files in {} series",
                    run.summary.dicom_files, run.summary.series_found
                );
                println!(
                    "result: {} accepted, {} held, {} excluded",
                    run.summary.accepted, run.summary.held, run.summary.excluded
                );
                if let Some(error) = run.error_code {
                    println!("error: {error}");
                }
            }
            Ok(())
        }
        Some(Command::Report { run_id, json: _ }) => {
            let report = runtime.report(run_id.as_deref())?;
            println!("{}", serde_json::to_string_pretty(&report)?);
            Ok(())
        }
    }
}

fn validate_explicit_policy_version(
    accepted: Option<&str>,
    advertised: &str,
    required: bool,
) -> Result<()> {
    match accepted {
        Some(value) if value == advertised => Ok(()),
        Some(value) => bail!(
            "--accept-policy-version names {value}, but the server requires {advertised}; review the current policy and pass that exact version"
        ),
        None if required => bail!(
            "explicit policy acceptance is required; review the current policy and pass --accept-policy-version {advertised}"
        ),
        None => Ok(()),
    }
}

pub(crate) fn default_device_name() -> String {
    std::env::var("COMPUTERNAME")
        .or_else(|_| std::env::var("HOSTNAME"))
        .ok()
        .filter(|value| !value.trim().is_empty())
        .map(|value| {
            value
                .chars()
                .filter(|character| !character.is_control())
                .take(64)
                .collect()
        })
        .unwrap_or_else(|| format!("{} workstation", std::env::consts::OS))
}

pub fn parse() -> Cli {
    Cli::parse()
}

pub fn ensure_no_unexpected_args() -> Result<()> {
    if std::env::args_os().len() == 0 {
        bail!("process argument vector is empty");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accepts_a_dicom_folder_as_the_primary_command() {
        let cli = Cli::try_parse_from(["neuro-sync", "/data/new dicoms"]).unwrap();
        assert_eq!(cli.folder, Some(PathBuf::from("/data/new dicoms")));
        assert!(cli.command.is_none());
    }

    #[test]
    fn former_recovery_token_is_an_ordinary_folder_argument() {
        let cli = Cli::try_parse_from(["neuro-sync", "resume"]).unwrap();
        assert_eq!(cli.folder, Some(PathBuf::from("resume")));
        assert!(cli.command.is_none());
    }

    #[test]
    fn prepare_accepts_the_default_or_an_explicit_review_folder() {
        let default = Cli::try_parse_from(["neuro-sync", "prepare", "source"]).unwrap();
        assert!(matches!(
            default.command,
            Some(Command::Prepare { output: None, .. })
        ));

        let explicit = Cli::try_parse_from([
            "neuro-sync",
            "prepare",
            "source",
            "--output",
            "custom-review",
        ])
        .unwrap();
        assert!(matches!(
            explicit.command,
            Some(Command::Prepare {
                output: Some(path),
                ..
            }) if path == std::path::Path::new("custom-review")
        ));
    }

    #[test]
    fn policy_acceptance_must_name_the_exact_advertised_version() {
        assert!(validate_explicit_policy_version(None, "open-epi-4.0.0", true).is_err());
        assert!(
            validate_explicit_policy_version(Some("open-mri-1.0.0"), "open-epi-4.0.0", true)
                .is_err()
        );
        validate_explicit_policy_version(Some("open-epi-4.0.0"), "open-epi-4.0.0", true).unwrap();
        validate_explicit_policy_version(None, "open-epi-4.0.0", false).unwrap();
    }
}
