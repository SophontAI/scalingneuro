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
    long_about = None
)]
pub struct Cli {
    /// Override the private local state directory (primarily for managed deployments and tests).
    #[arg(long, global = true, env = "NEURO_SYNC_STATE_DIR", hide = true)]
    pub state_dir: Option<PathBuf>,
    #[command(subcommand)]
    pub command: Option<Command>,
}

#[derive(Subcommand)]
pub enum Command {
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
    /// Select, validate, convert, and upload a DICOM folder.
    #[command(alias = "run")]
    Upload {
        folder: PathBuf,
        /// Perform every local privacy/QC step but do not contact the ingest service or R2.
        #[arg(long)]
        dry_run: bool,
    },
    /// Resume interrupted prepared or multipart uploads.
    Resume { run_id: Option<String> },
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
    match cli.command {
        None => crate::ui::serve(runtime).await,
        Some(Command::Register {
            email,
            name,
            institution,
            lab,
            ror,
            contact_opt_in,
            server,
            device_name,
        }) => {
            let contribution = runtime.contribution_info(&server).await?;
            if !contribution.registration_open {
                bail!("public contribution registration is temporarily paused");
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
        Some(Command::Upload { folder, dry_run }) => {
            let run_id = runtime.upload(folder, dry_run).await?;
            let run = runtime
                .run_record(Some(&run_id))?
                .context("run state is missing")?;
            println!("run: {run_id}");
            println!("status: {}", run.status);
            println!(
                "series: {} accepted, {} held, {} excluded",
                run.summary.accepted, run.summary.held, run.summary.excluded
            );
            if let Some(report) = run.report_path {
                println!("report: {report}");
            }
            Ok(())
        }
        Some(Command::Resume { run_id }) => {
            let completed = runtime.resume(run_id.as_deref()).await?;
            if completed.is_empty() {
                println!("no interrupted uploads need resuming");
            } else {
                for run_id in completed {
                    println!("completed: {run_id}");
                }
            }
            Ok(())
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

fn default_device_name() -> String {
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
