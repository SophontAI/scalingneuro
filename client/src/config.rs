use std::{
    fs,
    path::{Path, PathBuf},
};

use anyhow::{Context, Result, bail};
use directories::ProjectDirs;
use serde::{Deserialize, Serialize};

use crate::{DEFAULT_API_URL, privacy};

#[derive(Debug, Clone)]
pub struct AppPaths {
    pub root: PathBuf,
    pub config: PathBuf,
    pub database: PathBuf,
    pub lock: PathBuf,
    pub pending_enrollment: PathBuf,
    pub pending_registration: PathBuf,
    pub work: PathBuf,
    pub bundles: PathBuf,
    pub reports: PathBuf,
}

impl AppPaths {
    pub fn discover(override_root: Option<&Path>) -> Result<Self> {
        let root = if let Some(root) = override_root {
            root.to_path_buf()
        } else if let Ok(root) = std::env::var("NEURO_SYNC_STATE_DIR") {
            PathBuf::from(root)
        } else {
            ProjectDirs::from("med", "Sophont", "ScalingNeuro")
                .context("could not determine the operating-system data directory")?
                .data_local_dir()
                .join("neuro-sync")
        };
        Ok(Self {
            config: root.join("config.json"),
            database: root.join("state.sqlite3"),
            lock: root.join("instance.lock"),
            pending_enrollment: root.join("pending-enrollment.json"),
            pending_registration: root.join("pending-registration.json"),
            work: root.join("work"),
            bundles: root.join("bundles"),
            reports: root.join("reports"),
            root,
        })
    }

    pub fn initialize(&self) -> Result<()> {
        for path in [&self.root, &self.work, &self.bundles, &self.reports] {
            fs::create_dir_all(path)
                .with_context(|| format!("failed to create {}", path.display()))?;
            privacy::restrict_dir(path)?;
        }
        // Windows directory ACL changes do not necessarily rewrite existing
        // descendants. Sweep the private state tree so a custom directory
        // cannot retain older inherited access on reports or archive files.
        privacy::restrict_state_tree(&self.root)?;
        Ok(())
    }
}

#[derive(Clone, Serialize, Deserialize)]
pub struct ClientConfig {
    pub api_url: String,
    pub device_token: String,
    pub site_id: String,
    pub project_id: String,
    pub project_name: String,
    pub consent_policy_version: String,
    pub pseudonym_key_b64: String,
}

impl ClientConfig {
    pub fn load(paths: &AppPaths) -> Result<Self> {
        let bytes = fs::read(&paths.config).with_context(|| {
            format!(
                "this device is not registered (missing {}); run neuro-sync to register",
                paths.config.display()
            )
        })?;
        let config: Self = serde_json::from_slice(&bytes)
            .with_context(|| format!("invalid config at {}", paths.config.display()))?;
        config.validate()?;
        Ok(config)
    }

    pub fn save(&self, paths: &AppPaths) -> Result<()> {
        self.validate()?;
        paths.initialize()?;
        let temporary = paths.config.with_extension("json.tmp");
        let bytes = serde_json::to_vec_pretty(self)?;
        fs::write(&temporary, bytes)
            .with_context(|| format!("failed to write {}", temporary.display()))?;
        privacy::restrict_file(&temporary)?;
        fs::rename(&temporary, &paths.config)
            .with_context(|| format!("failed to replace {}", paths.config.display()))?;
        privacy::restrict_file(&paths.config)?;
        Ok(())
    }

    pub fn validate(&self) -> Result<()> {
        let url = url::Url::parse(&self.api_url).context("api_url is not a valid URL")?;
        if !matches!(url.scheme(), "https" | "http") {
            bail!("api_url must use https (or http for a loopback test server)");
        }
        if url.scheme() == "http"
            && !matches!(url.host_str(), Some("127.0.0.1" | "localhost" | "::1"))
        {
            bail!("unencrypted API URLs are allowed only for loopback test servers");
        }
        for (name, value) in [
            ("device_token", &self.device_token),
            ("site_id", &self.site_id),
            ("project_id", &self.project_id),
            ("pseudonym_key_b64", &self.pseudonym_key_b64),
        ] {
            if value.trim().is_empty() {
                bail!("{name} must not be empty");
            }
        }
        Ok(())
    }

    pub fn unenrolled_local(key_b64: String) -> Self {
        Self {
            api_url: DEFAULT_API_URL.to_owned(),
            device_token: "local-dry-run".into(),
            site_id: "local".into(),
            project_id: "dry-run".into(),
            project_name: "Local dry run".into(),
            consent_policy_version: "not-uploadable".into(),
            pseudonym_key_b64: key_b64,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_plaintext_remote_api() {
        let config = ClientConfig {
            api_url: "http://example.com".into(),
            device_token: "token".into(),
            site_id: "site".into(),
            project_id: "project".into(),
            project_name: "Project".into(),
            consent_policy_version: "v1".into(),
            pseudonym_key_b64: "a2V5".into(),
        };
        assert!(config.validate().is_err());
    }
}
