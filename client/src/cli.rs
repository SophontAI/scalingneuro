use std::path::PathBuf;

use anyhow::{Context, Result, bail};
use clap::{Parser, Subcommand};

use crate::{
    DEFAULT_API_URL,
    pipeline::{ContributorDetails, Runtime},
    state::PublicRunStatus,
};

#[derive(Parser)]
#[command(
    name = "neuro-sync",
    version,
    about = "Share approved functional EPI scans with Scaling Neuro",
    long_about = None,
    subcommand_precedence_over_arg = true
)]
pub struct Cli {
    /// Override the private local state directory (primarily for managed deployments and tests).
    #[arg(long, global = true, env = "NEURO_SYNC_STATE_DIR", hide = true)]
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
    /// Register this machine for the open public EPI contribution.
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
        /// Confirm acceptance of the contribution policy reported by the server.
        #[arg(long)]
        accept_policy: bool,
        #[arg(long, default_value = DEFAULT_API_URL)]
        server: String,
        #[arg(long)]
        device_name: Option<String>,
    },
    /// Enroll this machine using a one-time project invite.
    #[command(hide = true)]
    Enroll {
        invite: String,
        #[arg(long, default_value = DEFAULT_API_URL)]
        server: String,
        #[arg(long)]
        device_name: Option<String>,
    },
    /// Sync a DICOM folder, automatically continuing any checkpointed work.
    #[command(alias = "run")]
    Upload {
        folder: PathBuf,
        /// Perform every local privacy/QC step but do not contact the ingest service or R2.
        #[arg(long)]
        dry_run: bool,
        /// Confirm the selected scans are institutionally authorized for contribution.
        #[arg(long)]
        confirm_authorized: bool,
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
            accept_policy,
            server,
            device_name,
        }) => {
            let contribution = runtime.contribution_info(&server).await?;
            if !contribution.registration_open {
                bail!("public contribution registration is temporarily paused");
            }
            if !accept_policy {
                bail!(
                    "review the {} contribution policy at {} and rerun with --accept-policy to confirm acceptance",
                    contribution.consent_policy_version,
                    contribution.policy_url
                );
            }
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
        Some(Command::Enroll {
            invite,
            server,
            device_name,
        }) => {
            let device_name = device_name.unwrap_or_else(default_device_name);
            let config = runtime.enroll(invite, &server, device_name).await?;
            println!(
                "enrolled for {} ({})",
                config.project_name, config.project_id
            );
            println!("contribution policy: {}", config.consent_policy_version);
            Ok(())
        }
        Some(Command::Upload {
            folder,
            dry_run,
            confirm_authorized,
        }) => {
            let folder = folder
                .canonicalize()
                .with_context(|| format!("could not open selected folder: {}", folder.display()))?;
            if !folder.is_dir() {
                bail!("selected source is not a folder");
            }
            if !dry_run && !confirm_authorized {
                let config = crate::config::ClientConfig::load(&runtime.paths)?;
                if !crate::terminal::confirm_authorized_upload(
                    &folder,
                    &config.consent_policy_version,
                )? {
                    println!("cancelled; nothing was uploaded");
                    return Ok(());
                }
            }
            println!("\nSyncing {}…", folder.display());
            let run_id = runtime.sync_folder(folder, dry_run).await?;
            crate::terminal::print_run_summary(&runtime, &run_id, &mut std::io::stdout())
        }
        Some(Command::Status { run_id, json }) => {
            let run = runtime
                .run_record(run_id.as_deref())?
                .context("no matching run was found")?;
            if json {
                println!(
                    "{}",
                    serde_json::to_string_pretty(&PublicRunStatus::from(&run))?
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
}
