use std::path::{Path, PathBuf};
use std::process::Command;

use tracing::{error, info};

use crate::config::ProjectConfig;
use crate::error::{Result, ServerError};
use crate::state::{self, Paths, ProjectState, ServerState, SlotState, SlotStatus};

#[must_use]
pub fn project_dir(repos_dir: &Path, project_name: &str) -> PathBuf {
    repos_dir.join(project_name)
}

#[must_use]
pub fn slot_dir(repos_dir: &Path, project_name: &str, slot: u32) -> PathBuf {
    project_dir(repos_dir, project_name).join(format!("{slot:04}"))
}

pub fn ensure_pool(
    paths: &Paths,
    project: &ProjectConfig,
    pool_size: u32,
    state: &mut ServerState,
) -> Result<()> {
    let dir = project_dir(&paths.repos_dir, &project.name);
    std::fs::create_dir_all(&dir)?;

    // Compute what we need before entering the provisioning loop
    let ps = state.projects.entry(project.name.clone()).or_default();
    let available_count = ps
        .slots
        .iter()
        // We want to include Error, since any slot which errored should not cause us to
        // clone/build another
        .filter(|s| {
            matches!(
                s.status,
                SlotStatus::Ready | SlotStatus::Cloning | SlotStatus::Building | SlotStatus::Error
            )
        })
        .count();
    let needed = pool_size.saturating_sub(u32::try_from(available_count).unwrap_or(u32::MAX));
    let start_slot = ps.next_free_slot_number();

    for i in 0..needed {
        let slot_num = start_slot + i;
        let slot_path = slot_dir(&paths.repos_dir, &project.name, slot_num);

        info!(
            project = %project.name,
            slot = slot_num,
            "provisioning new slot at {}",
            slot_path.display()
        );

        // Add slot in Cloning status
        let ps = state
            .projects
            .get_mut(&project.name)
            .expect("just inserted");
        ps.slots.push(SlotState {
            slot: slot_num,
            status: SlotStatus::Cloning,
            last_refreshed: None,
            checked_out_as: None,
            error_message: None,
        });
        state::save_state(&paths.state_file, state)?;

        // Clone
        if let Err(e) = clone_repo(project, &slot_path) {
            error!(project = %project.name, slot = slot_num, "clone failed: {e}");
            let ps = state
                .projects
                .get_mut(&project.name)
                .expect("just inserted");
            update_slot_status(ps, slot_num, SlotStatus::Error, Some(e.to_string()));
            state::save_state(&paths.state_file, state)?;
            continue;
        }
        info!(project = %project.name, slot = slot_num, "clone complete");

        // Mark as Building
        let ps = state
            .projects
            .get_mut(&project.name)
            .expect("just inserted");
        update_slot_status(ps, slot_num, SlotStatus::Building, None);
        state::save_state(&paths.state_file, state)?;

        // Build
        match run_build(project, &slot_path) {
            Ok(()) => {
                info!(project = %project.name, slot = slot_num, "build complete");
                let ps = state
                    .projects
                    .get_mut(&project.name)
                    .expect("just inserted");
                update_slot_status(ps, slot_num, SlotStatus::Ready, None);
                if let Some(s) = ps.slots.iter_mut().find(|s| s.slot == slot_num) {
                    s.last_refreshed = Some(chrono::Utc::now());
                }
                state::save_state(&paths.state_file, state)?;
            }
            Err(e) => {
                error!(project = %project.name, slot = slot_num, "build failed: {e}");
                let ps = state
                    .projects
                    .get_mut(&project.name)
                    .expect("just inserted");
                update_slot_status(ps, slot_num, SlotStatus::Error, Some(e.to_string()));
                state::save_state(&paths.state_file, state)?;
            }
        }
    }

    Ok(())
}

pub fn refresh_slot(
    repos_dir: &Path,
    project: &ProjectConfig,
    project_state: &mut ProjectState,
    slot_num: u32,
) -> Result<()> {
    let slot_path = slot_dir(repos_dir, &project.name, slot_num);

    info!(project = %project.name, slot = slot_num, "refreshing");

    let branch = project.effective_branch();

    let output = Command::new("git")
        .args(["fetch", "origin"])
        .current_dir(&slot_path)
        .output()?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(ServerError::Git(format!("git fetch failed: {stderr}")));
    }

    let output = Command::new("git")
        .args(["reset", "--hard", &format!("origin/{branch}")])
        .current_dir(&slot_path)
        .output()?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(ServerError::Git(format!("git reset failed: {stderr}")));
    }

    match run_build(project, &slot_path) {
        Ok(()) => {
            update_slot_status(project_state, slot_num, SlotStatus::Ready, None);
            if let Some(s) = project_state.slots.iter_mut().find(|s| s.slot == slot_num) {
                s.last_refreshed = Some(chrono::Utc::now());
            }
        }
        Err(e) => {
            error!(project = %project.name, slot = slot_num, "build failed during refresh: {e}");
            update_slot_status(
                project_state,
                slot_num,
                SlotStatus::Error,
                Some(e.to_string()),
            );
        }
    }

    Ok(())
}

fn clone_repo(project: &ProjectConfig, dest: &Path) -> Result<()> {
    let mut args = vec!["clone"];

    let branch = project.effective_branch();
    args.extend(["--branch", branch]);

    let url = &project.repo_url;
    args.push(url);

    let dest_str = dest.to_str().expect("non-utf8 path");
    args.push(dest_str);

    let output = Command::new("git").args(&args).output()?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(ServerError::Git(format!(
            "git clone failed for {}: {stderr}",
            project.name
        )));
    }

    Ok(())
}

fn run_build(project: &ProjectConfig, dir: &Path) -> Result<()> {
    let Some(ref steps) = project.build_command else {
        return Ok(());
    };

    for step in &steps.0 {
        let mut tokens = step.split_ascii_whitespace();
        let program = tokens.next().ok_or_else(|| {
            ServerError::Build(format!("empty step in build_command for {}", project.name))
        })?;
        let args: Vec<&str> = tokens.collect();

        info!(project = %project.name, "running build step: {step}");

        let output = Command::new(program)
            .args(&args)
            .current_dir(dir)
            .output()?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            return Err(ServerError::Build(format!(
                "build step `{step}` failed for {}: {stderr}",
                project.name
            )));
        }
    }

    Ok(())
}

fn update_slot_status(
    project_state: &mut ProjectState,
    slot_num: u32,
    status: SlotStatus,
    error_message: Option<String>,
) {
    if let Some(slot) = project_state.slots.iter_mut().find(|s| s.slot == slot_num) {
        slot.status = status;
        slot.error_message = error_message;
    }
}
