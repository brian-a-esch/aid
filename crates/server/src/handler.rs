use std::path::{Path, PathBuf};
use std::process::Command;

use chrono::{DateTime, Utc};
use tracing::info;

use crate::config::Config;
use crate::error::{Result, ServerError};
use crate::poll_loop::{ChildExit, Handler};
use crate::state::{
    Paths, PendingAction, ProjectState, ServerState, Slot, SlotId, SlotStatus, StepId,
};

/// Ensure every project in `config` has been allocated and has enough slots
fn initialize(state: &mut ServerState, config: &Config) {
    for project_config in &config.projects {
        let nslots = config.nslots(project_config) as usize;

        let project_state: &mut ProjectState = state
            .projects
            .entry(project_config.name.clone())
            .or_default();

        while project_state.available_slots().len() < nslots {
            let slot_id = SlotId(project_state.next_free_slot_number());
            project_state.slots.push(Slot {
                id: slot_id,
                status: SlotStatus::Uninitialized,
                last_refreshed: None,
                checked_out_as: None,
                error_message: None,
            });
        }
    }
}

/// Inspect state and config and return the single most important action to take next.
fn step(now: DateTime<Utc>, state: &ServerState, config: &Config) -> Option<PendingAction> {
    if state.pending_action.is_some() {
        return None;
    }

    for project_config in &config.projects {
        let project_state = state.projects.get(&project_config.name).unwrap();
        for slot in &project_state.slots {
            if slot.status == SlotStatus::Uninitialized {
                return Some(PendingAction::Clone(project_config.name.clone(), slot.id));
            }
        }

        if project_config.has_submodules {
            for slot in &project_state.slots {
                if slot.status == SlotStatus::Cloned {
                    return Some(PendingAction::CloneSubmodules(
                        project_config.name.clone(),
                        slot.id,
                    ));
                }
            }
        }
    }

    // Separate loop, to prefer cloning another repo before updating
    for project_config in &config.projects {
        let project_state = state.projects.get(&project_config.name).unwrap();
        for slot in &project_state.slots {
            let eligible = if project_config.has_submodules {
                slot.status == SlotStatus::SubmodulesCloned || slot.status == SlotStatus::Ready
            } else {
                slot.status == SlotStatus::Cloned || slot.status == SlotStatus::Ready
            };

            let is_stale = slot.last_refreshed.is_none_or(|t| {
                (now - t).num_seconds() >= config.effective_refresh_interval().cast_signed()
            });

            if eligible && is_stale {
                return Some(PendingAction::Update(project_config.name.clone(), slot.id));
            }
        }

        if project_config.has_submodules {
            for slot in &project_state.slots {
                if slot.status == SlotStatus::PartiallyUpdated {
                    return Some(PendingAction::UpdateSubmodules(
                        project_config.name.clone(),
                        slot.id,
                    ));
                }
            }
        }
    }

    // Prioritize all updates before any builds
    for project_config in &config.projects {
        let project_state = state.projects.get(&project_config.name).unwrap();
        if project_config.build_command.is_some() {
            for slot in &project_state.slots {
                let next_step_id = if slot.status == SlotStatus::WaitingToBuild {
                    Some(0)
                } else if let SlotStatus::Built(step_id) = slot.status {
                    Some(step_id.0 + 1)
                } else {
                    None
                };

                if let Some(s) = next_step_id {
                    return Some(PendingAction::Build(
                        project_config.name.clone(),
                        slot.id,
                        StepId(s),
                    ));
                }
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

    let (project_id, slot_id) = match &action {
        PendingAction::Clone(p, s)
        | PendingAction::CloneSubmodules(p, s)
        | PendingAction::Update(p, s)
        | PendingAction::UpdateSubmodules(p, s)
        | PendingAction::Build(p, s, _) => (p, s),
    };

    let project_config = config
        .projects
        .iter()
        .find(|p| p.name == *project_id)
        .ok_or_else(|| ServerError::Pool(format!("no project named '{}'", project_id.0)))?;

    let project_state = state
        .projects
        .get_mut(project_id)
        .ok_or_else(|| ServerError::Pool(format!("no state for project '{}'", project_id.0)))?;

    let slot = project_state
        .slots
        .get_mut(slot_id.0 as usize)
        .ok_or_else(|| {
            ServerError::Pool(format!(
                "no slot {} in project '{}'",
                slot_id.0, project_config.name.0
            ))
        })?;

    if result.success {
        match (&slot.status, action) {
            (SlotStatus::Uninitialized, PendingAction::Clone(_, _)) => {
                slot.status = SlotStatus::Cloned;
            }
            (SlotStatus::Cloned, PendingAction::CloneSubmodules(_, _)) => {
                slot.status = SlotStatus::SubmodulesCloned;
            }
            (
                SlotStatus::Cloned | SlotStatus::SubmodulesCloned | SlotStatus::Ready,
                PendingAction::Update(_, _),
            ) => {
                if project_config.has_submodules {
                    slot.status = SlotStatus::PartiallyUpdated;
                } else if project_config.build_command.is_some() {
                    slot.status = SlotStatus::Built(StepId(0));
                } else {
                    slot.status = SlotStatus::Ready;
                    slot.last_refreshed = Some(now);
                }
            }
            (SlotStatus::PartiallyUpdated, PendingAction::UpdateSubmodules(_, _)) => {
                if project_config.build_command.is_some() {
                    slot.status = SlotStatus::WaitingToBuild;
                } else {
                    slot.status = SlotStatus::Ready;
                    slot.last_refreshed = Some(now);
                }
            }
            (
                SlotStatus::WaitingToBuild | SlotStatus::Built(_),
                PendingAction::Build(_, _, step_id),
            ) => {
                let n_steps = project_config
                    .build_command
                    .as_ref()
                    .map_or(0, |s| s.0.len());
                if step_id.0 + 1 < n_steps {
                    slot.status = SlotStatus::Built(StepId(step_id.0));
                } else {
                    slot.status = SlotStatus::Ready;
                    slot.last_refreshed = Some(now);
                }
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
                .iter()
                .find(|p| p.name == *project_id)
                .expect("invalid project_id");
            let dest = clone_dest(repos_dir, &project.name.0, *slot_id);

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
                .iter()
                .find(|p| p.name == *project_id)
                .expect("invalid project_id");
            let dest = clone_dest(repos_dir, &project.name.0, *slot_id);

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
                .iter()
                .find(|p| p.name == *project_id)
                .expect("invalid project_id");
            let dest = clone_dest(repos_dir, &project.name.0, *slot_id);

            let mut cmd = Command::new("git");
            cmd.arg("pull").arg("--ff-only").current_dir(&dest);
            cmd
        }
        PendingAction::UpdateSubmodules(project_id, slot_id) => {
            let project = config
                .projects
                .iter()
                .find(|p| p.name == *project_id)
                .expect("invalid project_id");
            let dest = clone_dest(repos_dir, &project.name.0, *slot_id);

            let mut cmd = Command::new("git");
            cmd.arg("submodule")
                .arg("update")
                .arg("--recursive")
                .current_dir(&dest);
            cmd
        }
        PendingAction::Build(project_id, slot_id, step_id) => {
            let project = config
                .projects
                .iter()
                .find(|p| p.name == *project_id)
                .expect("invalid project_id");
            let dest = clone_dest(repos_dir, &project.name.0, *slot_id);
            let steps = project
                .build_command
                .as_ref()
                .expect("Build action requires build_command");
            let step_str = &steps.0[step_id.0];
            let mut parts = step_str.split_whitespace();
            let program = parts.next().expect("build step must not be empty");
            let mut cmd = Command::new(program);
            cmd.args(parts).current_dir(&dest);
            cmd
        }
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
        initialize(&mut self.state, &self.config);
        let action = step(now, &self.state, &self.config)?;
        self.state.pending_action = Some(action.clone());
        Some(to_command(&self.config, &self.paths.repos_dir, &action))
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;
    use std::rc::Rc;

    use super::*;
    use crate::config::load_config;
    use crate::state::{ProjectId, StepId};

    fn load_test_config() -> Config {
        let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("testdata")
            .join("config.toml");
        load_config(&path).expect("testdata/config.toml should parse cleanly")
    }

    /// Mirrors the logic of `on_idle`: initialize, step, set pending_action.
    fn simulate_step(
        state: &mut ServerState,
        config: &Config,
        now: DateTime<Utc>,
    ) -> Option<PendingAction> {
        initialize(state, config);
        let action = step(now, state, config)?;
        state.pending_action = Some(action.clone());
        Some(action)
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

        let myproject = ProjectId(Rc::from("myproject"));
        let other_project = ProjectId(Rc::from("other-project"));

        let expected = [
            // Initial clone wave
            PendingAction::Clone(myproject.clone(), SlotId(0)),
            PendingAction::Clone(myproject.clone(), SlotId(1)),
            PendingAction::Clone(myproject.clone(), SlotId(2)),
            PendingAction::CloneSubmodules(myproject.clone(), SlotId(0)),
            PendingAction::CloneSubmodules(myproject.clone(), SlotId(1)),
            PendingAction::CloneSubmodules(myproject.clone(), SlotId(2)),
            PendingAction::Clone(other_project.clone(), SlotId(0)),
            PendingAction::Clone(other_project.clone(), SlotId(1)),
            // Update wave (last_refreshed=None so all slots are immediately stale)
            // myproject has_submodules=true: Update → PartiallyUpdated → UpdateSubmodules → Building → Ready
            PendingAction::Update(myproject.clone(), SlotId(0)),
            PendingAction::Update(myproject.clone(), SlotId(1)),
            PendingAction::Update(myproject.clone(), SlotId(2)),
            PendingAction::UpdateSubmodules(myproject.clone(), SlotId(0)),
            PendingAction::UpdateSubmodules(myproject.clone(), SlotId(1)),
            PendingAction::UpdateSubmodules(myproject.clone(), SlotId(2)),
            // other-project has no submodules and no build_command: Update → Ready directly
            PendingAction::Update(other_project.clone(), SlotId(0)),
            PendingAction::Update(other_project.clone(), SlotId(1)),
            // myproject has build_command with 3 steps: each slot runs all steps before the next slot starts
            // (step() scans slots in order, so slot 0 progresses through all steps first)
            PendingAction::Build(myproject.clone(), SlotId(0), StepId(0)),
            PendingAction::Build(myproject.clone(), SlotId(0), StepId(1)),
            PendingAction::Build(myproject.clone(), SlotId(0), StepId(2)),
            PendingAction::Build(myproject.clone(), SlotId(1), StepId(0)),
            PendingAction::Build(myproject.clone(), SlotId(1), StepId(1)),
            PendingAction::Build(myproject.clone(), SlotId(1), StepId(2)),
            PendingAction::Build(myproject.clone(), SlotId(2), StepId(0)),
            PendingAction::Build(myproject.clone(), SlotId(2), StepId(1)),
            PendingAction::Build(myproject.clone(), SlotId(2), StepId(2)),
        ];

        for (step_num, want) in expected.into_iter().enumerate() {
            let action = simulate_step(&mut state, &config, now)
                .unwrap_or_else(|| panic!("step {step_num}: expected action, got None"));
            assert_eq!(action, want, "step {step_num}");
            simulate_success(&mut state, &config, now);
        }

        assert!(
            simulate_step(&mut state, &config, now).is_none(),
            "all pools full: expected None"
        );

        for (name, p) in &state.projects {
            for s in p.slots.iter() {
                assert_eq!(
                    s.status,
                    SlotStatus::Ready,
                    "project '{:?}' slot {:?}",
                    name,
                    s.id
                );
                assert!(
                    s.last_refreshed.is_some(),
                    "project '{:?}' slot {:?} should have last_refreshed set",
                    name,
                    s.id
                );
            }
        }
    }
}
