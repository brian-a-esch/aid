use std::path::{Path, PathBuf};
use std::process::Command;

use chrono::{DateTime, Utc};
use tracing::info;

use crate::config::Config;
use crate::error::{Result, ServerError};
use crate::poll_loop::{ChildExit, Handler};
use crate::state::{Paths, PendingAction, ProjectId, ServerState, Slot, SlotId, SlotStatus};

/// Inspect state and config and return the single most important action to take next.
fn step(now: DateTime<Utc>, state: &mut ServerState, config: &Config) -> Option<PendingAction> {
    // Don't start a new action while one is already running.
    if state.pending_action.is_some() {
        return None;
    }

    for (project_idx, project_config) in config.projects.iter().enumerate() {
        let project_id = ProjectId(project_idx);
        let nslots = config.nslots(project_config) as usize;

        let project_state = state
            .projects
            .entry(project_config.name.clone())
            .or_default();

        let pool_count = project_state.available_slots().len();
        if pool_count < nslots {
            // Allocate the next slot number and immediately mark it as Cloning so
            // subsequent idle ticks don't schedule a duplicate.
            let slot_id = SlotId(project_state.next_free_slot_number());

            project_state.slots.push(Slot {
                id: slot_id,
                status: SlotStatus::Cloning,
                last_refreshed: None,
                checked_out_as: None,
                error_message: None,
            });

            let action = PendingAction::Clone(project_id, slot_id);
            state.pending_action = Some(action.clone());
            return Some(action);
        }

        if project_config.has_submodules {
            for slot in &mut project_state.slots {
                if slot.status == SlotStatus::Cloned {
                    slot.status = SlotStatus::CloningSubmodules;
                    let action = PendingAction::CloneSubmodules(project_id, slot.id);
                    state.pending_action = Some(action.clone());
                    return Some(action);
                }
            }
        }
    }

    // Update loop — only reached when all slots across all projects are fully cloned.
    for (project_idx, project_config) in config.projects.iter().enumerate() {
        let project_id = ProjectId(project_idx);

        let project_state = state
            .projects
            .entry(project_config.name.clone())
            .or_default();

        for slot in &mut project_state.slots {
            let eligible = if project_config.has_submodules {
                slot.status == SlotStatus::SubmodulesCloned
            } else {
                slot.status == SlotStatus::Cloned
            };

            let is_stale = slot.last_refreshed.is_none_or(|t| {
                (now - t).num_seconds() >= config.effective_refresh_interval().cast_signed()
            });

            if eligible && is_stale {
                slot.status = SlotStatus::Updating;
                let action = PendingAction::Update(project_id, slot.id);
                state.pending_action = Some(action.clone());
                return Some(action);
            }
        }

        for slot in &mut project_state.slots {
            if slot.status == SlotStatus::UpdatingSubmodules {
                let action = PendingAction::UpdateSubmodules(project_id, slot.id);
                state.pending_action = Some(action.clone());
                return Some(action);
            }
        }
    }

    None
}

fn illegal_transition(status: &SlotStatus, action: &PendingAction) -> ServerError {
    ServerError::Pool(format!(
        "illegal transition: state={status:?}, action={action:?}"
    ))
}

/// Apply the result of a completed child process to the state. Returns `Err` if the state is internally inconsistent
fn complete(
    now: DateTime<Utc>,
    state: &mut ServerState,
    config: &Config,
    result: &ChildExit,
) -> Result<()> {
    let action = state.pending_action.take().ok_or_else(|| {
        ServerError::Pool("child exited but no pending action was recorded".into())
    })?;

    let (project_id, slot_id) = match action {
        PendingAction::Clone(p, s)
        | PendingAction::CloneSubmodules(p, s)
        | PendingAction::Update(p, s)
        | PendingAction::UpdateSubmodules(p, s)
        | PendingAction::Build(p, s, _) => (p, s),
    };

    let project_config = config
        .projects
        .get(project_id.0)
        .ok_or_else(|| ServerError::Pool(format!("no project at index {}", project_id.0)))?;

    let project_state = state
        .projects
        .get_mut(&project_config.name)
        .ok_or_else(|| {
            ServerError::Pool(format!("no state for project '{}'", project_config.name))
        })?;

    let slot = project_state
        .slots
        .get_mut(slot_id.0 as usize)
        .ok_or_else(|| {
            ServerError::Pool(format!(
                "no slot {} in project '{}'",
                slot_id.0, project_config.name
            ))
        })?;

    if result.success {
        match (&slot.status, action) {
            (SlotStatus::Cloning, PendingAction::Clone(_, _)) => {
                slot.status = SlotStatus::Cloned;
            }
            (SlotStatus::CloningSubmodules, PendingAction::CloneSubmodules(_, _)) => {
                slot.status = SlotStatus::SubmodulesCloned;
            }
            (SlotStatus::Updating, PendingAction::Update(_, _)) => {
                if project_config.has_submodules {
                    slot.status = SlotStatus::UpdatingSubmodules;
                } else {
                    slot.status = SlotStatus::Ready;
                    slot.last_refreshed = Some(now);
                }
            }
            (SlotStatus::UpdatingSubmodules, PendingAction::UpdateSubmodules(_, _)) => {
                slot.status = SlotStatus::Ready;
                slot.last_refreshed = Some(now);
            }
            (_, PendingAction::Build(_, _, _)) => {
                todo!()
            }
            (status, other) => return Err(illegal_transition(status, &other)),
        }
    } else {
        let stderr = String::from_utf8_lossy(&result.stderr);
        slot.status = SlotStatus::Error;
        slot.error_message = Some(stderr.into_owned());
    }

    Ok(())
}

/// Build the destination path for a clone: `<repos_dir>/<project_name>/<slot_id>`.
fn clone_dest(repos_dir: &Path, project_name: &str, slot_id: SlotId) -> PathBuf {
    repos_dir.join(project_name).join(slot_id.0.to_string())
}

/// Translate a `PendingAction` into the `Command` that achieves it.
fn to_command(config: &Config, repos_dir: &Path, action: &PendingAction) -> Command {
    match action {
        PendingAction::Clone(project_id, slot_id) => {
            let project = config
                .projects
                .get(project_id.0)
                .expect("invalid project_id");
            let dest = clone_dest(repos_dir, &project.name, *slot_id);

            let mut cmd = Command::new("git");
            cmd.arg("clone")
                .arg("--branch")
                .arg(project.effective_branch())
                .arg(&project.repo_url)
                .arg(&dest);

            cmd
        }
        PendingAction::CloneSubmodules(project_id, slot_id) => {
            let project = config
                .projects
                .get(project_id.0)
                .expect("invalid project_id");
            let dest = clone_dest(repos_dir, &project.name, *slot_id);

            let mut cmd = Command::new("git");
            cmd.arg("submodule")
                .arg("update")
                .arg("--init")
                .arg("--recursive")
                .current_dir(&dest);

            cmd
        }
        PendingAction::Update(project_id, slot_id) => {
            let project = config
                .projects
                .get(project_id.0)
                .expect("invalid project_id");
            let dest = clone_dest(repos_dir, &project.name, *slot_id);

            let mut cmd = Command::new("git");
            cmd.arg("pull").arg("--ff-only").current_dir(&dest);
            cmd
        }
        PendingAction::UpdateSubmodules(project_id, slot_id) => {
            let project = config
                .projects
                .get(project_id.0)
                .expect("invalid project_id");
            let dest = clone_dest(repos_dir, &project.name, *slot_id);

            let mut cmd = Command::new("git");
            cmd.arg("submodule")
                .arg("update")
                .arg("--recursive")
                .current_dir(&dest);
            cmd
        }
        PendingAction::Build(_, _, _) => todo!(),
    }
}

pub struct AidHandler<'a> {
    config: Config,
    state: ServerState,
    paths: &'a Paths,
}

impl<'a> AidHandler<'a> {
    #[must_use]
    pub fn new(config: Config, state: ServerState, paths: &'a Paths) -> Self {
        Self {
            config,
            state,
            paths,
        }
    }
}

impl Handler for AidHandler<'_> {
    fn handle_message(&mut self, _now: DateTime<Utc>, msg: &[u8]) -> Vec<u8> {
        let text = String::from_utf8_lossy(msg);
        info!("received client message: {}", text.trim());
        vec![]
    }

    fn handle_child_exit(&mut self, now: DateTime<Utc>, result: ChildExit) {
        complete(now, &mut self.state, &self.config, &result)
            .expect("failed to apply completed action to state");
    }

    fn on_idle(&mut self, now: DateTime<Utc>) -> Option<Command> {
        let action = step(now, &mut self.state, &self.config)?;
        Some(to_command(&self.config, &self.paths.repos_dir, &action))
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::*;
    use crate::config::load_config;

    fn load_test_config() -> Config {
        let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("testdata")
            .join("config.toml");
        load_config(&path).expect("testdata/config.toml should parse cleanly")
    }

    fn simulate_success(state: &mut ServerState, config: &Config, now: DateTime<Utc>) {
        complete(
            now,
            state,
            config,
            &ChildExit {
                success: true,
                stdout: vec![],
                stderr: vec![],
            },
        )
        .expect("complete should not fail in step sequence test");
    }

    #[test]
    fn step_sequence_from_empty_state() {
        let config = load_test_config();
        let mut state = ServerState::default();
        let now = Utc::now();

        let expected = [
            // Initial clone wave
            PendingAction::Clone(ProjectId(0), SlotId(0)),
            PendingAction::Clone(ProjectId(0), SlotId(1)),
            PendingAction::Clone(ProjectId(0), SlotId(2)),
            PendingAction::CloneSubmodules(ProjectId(0), SlotId(0)),
            PendingAction::CloneSubmodules(ProjectId(0), SlotId(1)),
            PendingAction::CloneSubmodules(ProjectId(0), SlotId(2)),
            PendingAction::Clone(ProjectId(1), SlotId(0)),
            PendingAction::Clone(ProjectId(1), SlotId(1)),
            // Update wave (last_refreshed=None so all slots are immediately stale)
            // project 0 has_submodules=true: Update → UpdatingSubmodules → UpdateSubmodules → Ready
            PendingAction::Update(ProjectId(0), SlotId(0)),
            PendingAction::Update(ProjectId(0), SlotId(1)),
            PendingAction::Update(ProjectId(0), SlotId(2)),
            PendingAction::UpdateSubmodules(ProjectId(0), SlotId(0)),
            PendingAction::UpdateSubmodules(ProjectId(0), SlotId(1)),
            PendingAction::UpdateSubmodules(ProjectId(0), SlotId(2)),
            // project 1 has no submodules: Update → Ready directly
            PendingAction::Update(ProjectId(1), SlotId(0)),
            PendingAction::Update(ProjectId(1), SlotId(1)),
        ];

        for (step_num, want) in expected.into_iter().enumerate() {
            let action = step(now, &mut state, &config)
                .unwrap_or_else(|| panic!("step {step_num}: expected action, got None"));
            assert_eq!(action, want, "step {step_num}");
            simulate_success(&mut state, &config, now);
        }

        assert!(
            step(now, &mut state, &config).is_none(),
            "all pools full: expected None"
        );

        for (name, p) in &state.projects {
            for s in p.slots.iter() {
                assert_eq!(
                    s.status,
                    SlotStatus::Ready,
                    "project '{name}' slot {:?}",
                    s.id
                );
                assert!(
                    s.last_refreshed.is_some(),
                    "project '{name}' slot {:?} should have last_refreshed set",
                    s.id
                );
            }
        }
    }
}
