use std::path::PathBuf;
use std::process::Command;

use tracing::info;

use crate::config::Config;
use crate::poll_loop::{ChildExit, Handler};
use crate::state::{Paths, ProjectState, ServerState, SlotState, SlotStatus};

#[derive(Debug, Copy, Clone, PartialEq, Eq, PartialOrd, Ord)]
struct SlotId(u32);

#[derive(Debug, Copy, Clone, PartialEq, Eq, PartialOrd, Ord)]
struct ProjectId(usize);

#[derive(Debug, Copy, Clone, PartialEq, Eq, PartialOrd, Ord)]
struct StepId(usize);

#[derive(Debug, PartialEq)]
enum NextAction {
    Clone(ProjectId, SlotId),
    CloneSubmodules(ProjectId, SlotId),
    Update(ProjectId, SlotId),
    UpdateSubmodules(ProjectId, SlotId),
    Build(ProjectId, SlotId, StepId),
}

/// Inspect state and config and return the single most important action to take next.
fn step(state: &mut ServerState, config: &Config) -> Option<NextAction> {
    for (project_idx, project_config) in config.projects.iter().enumerate() {
        let project_id = ProjectId(project_idx);
        let nslots = config.nslots(project_config) as usize;

        let project_state = state
            .projects
            .entry(project_config.name.clone())
            .or_insert_with(ProjectState::default);

        let pool_count = project_state.available_slots().len();
        if pool_count < nslots {
            // Allocate the next slot number and immediately mark it as Cloning so
            // subsequent idle ticks don't schedule a duplicate.
            let slot_number = project_state.next_free_slot_number();
            let slot_id = SlotId(slot_number);

            project_state.slots.push(SlotState {
                slot: slot_number,
                status: SlotStatus::Cloning,
                last_refreshed: None,
                checked_out_as: None,
                error_message: None,
            });

            return Some(NextAction::Clone(project_id, slot_id));
        }
    }

    None
}

/// Build the destination path for a clone: `<repos_dir>/<project_name>/<slot_id>`.
fn clone_dest(repos_dir: &PathBuf, project_name: &str, slot_id: SlotId) -> PathBuf {
    repos_dir.join(project_name).join(slot_id.0.to_string())
}

/// Translate a `NextAction` into the `Command` that achieves it.
fn to_command(config: &Config, repos_dir: &PathBuf, action: NextAction) -> Option<Command> {
    match action {
        NextAction::Clone(project_id, slot_id) => {
            let project = config.projects.get(project_id.0)?;
            let dest = clone_dest(repos_dir, &project.name, slot_id);

            let mut cmd = Command::new("git");
            cmd.arg("clone")
                .arg("--branch")
                .arg(project.effective_branch())
                .arg(&project.repo_url)
                .arg(&dest);

            info!(
                "cloning '{}' (slot {}) -> {}",
                project.name,
                slot_id.0,
                dest.display()
            );

            Some(cmd)
        }

        // Not yet implemented; fall through with None so the event loop stays idle.
        NextAction::CloneSubmodules(_, _)
        | NextAction::Update(_, _)
        | NextAction::UpdateSubmodules(_, _)
        | NextAction::Build(_, _, _) => None,
    }
}

pub struct AidHandler<'a> {
    config: Config,
    state: ServerState,
    paths: &'a Paths,
}

impl<'a> AidHandler<'a> {
    pub fn new(config: Config, state: ServerState, paths: &'a Paths) -> Self {
        Self {
            config,
            state,
            paths,
        }
    }
}

impl<'a> Handler for AidHandler<'a> {
    fn handle_message(&mut self, msg: &[u8]) -> Vec<u8> {
        let text = String::from_utf8_lossy(msg);
        info!("received client message: {}", text.trim());
        vec![]
    }

    fn handle_child_exit(&mut self, result: ChildExit) {
        if result.success {
            info!("build step completed successfully");
        } else {
            let stderr = String::from_utf8_lossy(&result.stderr);
            info!("build step failed: {stderr}");
        }
    }

    fn on_idle(&mut self) -> Option<Command> {
        let action = step(&mut self.state, &self.config)?;
        to_command(&self.config, &self.paths.repos_dir, action)
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

    #[test]
    fn step_sequence_from_empty_state() {
        let config = load_test_config();
        let mut state = ServerState::default();

        let expected = [
            NextAction::Clone(ProjectId(0), SlotId(0)), // myproject slot 0
            NextAction::Clone(ProjectId(0), SlotId(1)), // myproject slot 1
            NextAction::Clone(ProjectId(0), SlotId(2)), // myproject slot 2
            NextAction::Clone(ProjectId(1), SlotId(0)), // other-project slot 0
            NextAction::Clone(ProjectId(1), SlotId(1)), // other-project slot 1
        ];

        for (step_num, want) in expected.into_iter().enumerate() {
            let action = step(&mut state, &config)
                .unwrap_or_else(|| panic!("step {step_num}: expected action, got None"));
            assert_eq!(action, want, "step {step_num}");
        }

        assert!(
            step(&mut state, &config).is_none(),
            "all pools full: expected None"
        );
    }
}
