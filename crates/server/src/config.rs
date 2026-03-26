use std::path::Path;

use serde::{Deserialize, Serialize};

use crate::error::{Result, ServerError};

/// An ordered sequence of shell-style commands to execute during the build step.
///
/// Each entry is split on whitespace into program + arguments and executed directly
/// (no shell involved). Steps are run in order; the first failure aborts the build.
///
/// # TOML example
/// ```toml
/// build_command = ["make build", "make test", "make clang-tidy"]
/// ```
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Steps(pub Vec<String>);

const DEFAULT_SLOT_SIZE: u32 = 2;
const DEFAULT_REFRESH_INTERVAL_SECS: u64 = 1800; // 30 minutes

#[derive(Debug, Clone, Deserialize, Default)]
pub struct Config {
    pub nslots: Option<u32>,
    pub refresh_interval_secs: Option<u64>,
    #[serde(default)]
    pub projects: Vec<ProjectConfig>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct ProjectConfig {
    pub name: String,
    pub repo_url: String,
    pub build_command: Option<Steps>,
    pub branch: Option<String>,
    pub nslots: Option<u32>,
}

impl ProjectConfig {
    #[must_use]
    pub fn effective_branch(&self) -> &str {
        self.branch.as_deref().unwrap_or("main")
    }
}

impl Config {
    #[must_use]
    pub fn nslots(&self, project: &ProjectConfig) -> u32 {
        project
            .nslots
            .or(self.nslots)
            .unwrap_or(DEFAULT_SLOT_SIZE)
    }

    #[must_use]
    pub fn effective_refresh_interval(&self) -> u64 {
        self.refresh_interval_secs
            .unwrap_or(DEFAULT_REFRESH_INTERVAL_SECS)
    }
}

pub fn load_config(path: &Path) -> Result<Config> {
    if !path.exists() {
        tracing::info!("no config at {}, using defaults", path.display());
        return Ok(Config::default());
    }
    let contents = std::fs::read_to_string(path)?;
    let config: Config = toml::from_str(&contents)?;

    for project in &config.projects {
        validate_project(project)?;
    }

    if config.projects.is_empty() {
        tracing::warn!("no projects configured in {}", path.display());
    }

    Ok(config)
}

fn validate_project(project: &ProjectConfig) -> Result<()> {
    if project.name.is_empty() {
        return Err(ServerError::Config("project name is required".into()));
    }
    if project.repo_url.is_empty() {
        return Err(ServerError::Config(format!(
            "repo_url is required for project '{}'",
            project.name
        )));
    }
    Ok(())
}
