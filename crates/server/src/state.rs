use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::rc::Rc;

use api::SlotStatusSummary;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::error::Result;

#[derive(Debug, Copy, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub struct SlotId(pub u32);

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Hash)]
pub struct ProjectId(pub Rc<str>);

#[derive(Debug, Copy, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub struct StepId(pub usize);

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum PendingAction {
    Clone(ProjectId, SlotId),
    CloneSubmodules(ProjectId, SlotId),
    Update(ProjectId, SlotId),
    UpdateSubmodules(ProjectId, SlotId),
    Build(ProjectId, SlotId, StepId),
}

#[derive(Debug, Clone)]
pub struct Paths {
    pub data_dir: PathBuf,
    pub config_file: PathBuf,
    pub state_file: PathBuf,
    pub lock_file: PathBuf,
    pub socket_file: PathBuf,
    pub repos_dir: PathBuf,
}

impl Paths {
    #[must_use]
    pub fn new(config_dir: &Path, data_dir: &Path) -> Self {
        Self {
            data_dir: data_dir.to_path_buf(),
            config_file: config_dir.join("config.toml"),
            state_file: data_dir.join("state.json"),
            lock_file: data_dir.join("server.lock"),
            socket_file: data_dir.join("server.sock"),
            repos_dir: data_dir.join("repos"),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum SlotStatus {
    Uninitialized,
    Cloned,
    SubmodulesCloned,
    PartiallyUpdated,
    WaitingToBuild,
    Built(StepId),
    Ready,
    CheckedOut(String),
    Error,
}

impl SlotStatus {
    #[must_use]
    pub fn to_api(&self) -> SlotStatusSummary {
        match self {
            SlotStatus::Uninitialized => SlotStatusSummary::Uninitialized,
            SlotStatus::Cloned | SlotStatus::SubmodulesCloned | SlotStatus::PartiallyUpdated => {
                SlotStatusSummary::Cloning
            }
            SlotStatus::WaitingToBuild | SlotStatus::Built(_) => SlotStatusSummary::Building,
            SlotStatus::Ready => SlotStatusSummary::Ready,
            SlotStatus::CheckedOut(_) => SlotStatusSummary::CheckedOut,
            SlotStatus::Error => SlotStatusSummary::Error,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Slot {
    pub id: SlotId,
    pub status: SlotStatus,
    pub last_refreshed: Option<DateTime<Utc>>,
    pub error_message: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct ProjectState {
    pub slots: Vec<Slot>,
}

impl ProjectState {
    #[must_use]
    pub fn next_free_slot_number(&self) -> u32 {
        self.slots.iter().map(|s| s.id).max().map_or(0, |n| n.0 + 1)
    }

    pub fn ready_slots(&self) -> impl Iterator<Item = &Slot> {
        self.slots.iter().filter(|s| s.status == SlotStatus::Ready)
    }

    pub fn available_slots(&self) -> impl Iterator<Item = &Slot> {
        self.slots
            .iter()
            .filter(|s| !matches!(s.status, SlotStatus::CheckedOut(_)))
    }

    pub fn checked_out_slots(&self) -> impl Iterator<Item = &Slot> {
        self.slots
            .iter()
            .filter(|s| matches!(s.status, SlotStatus::CheckedOut(_)))
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ServerState {
    pub projects: HashMap<ProjectId, ProjectState>,
    pub last_updated: DateTime<Utc>,
    #[serde(default)]
    pub pending_action: Option<PendingAction>,
}

impl Default for ServerState {
    fn default() -> Self {
        Self {
            projects: HashMap::new(),
            last_updated: Utc::now(),
            pending_action: None,
        }
    }
}

pub fn load_state(path: &Path) -> Result<ServerState> {
    if !path.exists() {
        return Ok(ServerState::default());
    }
    let contents = std::fs::read_to_string(path)?;
    let state: ServerState = serde_json::from_str(&contents)?;
    Ok(state)
}

pub fn save_state(path: &Path, state: &ServerState) -> Result<()> {
    let tmp_path = path.with_extension("json.tmp");
    let contents = serde_json::to_string_pretty(state)?;
    std::fs::write(&tmp_path, contents)?;
    std::fs::rename(&tmp_path, path)?;
    Ok(())
}
