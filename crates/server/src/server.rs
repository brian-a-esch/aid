use std::sync::Arc;

use tokio::sync::Mutex;
use tracing::{error, info, warn};
use tracing_subscriber::EnvFilter;

use crate::config::{self, Config};
use crate::error::{Result, ServerError};
use crate::pool;
use crate::socket;
use crate::state::{self, Paths, SlotStatus};

pub async fn run(paths: Paths) -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")),
        )
        .init();

    info!("starting aid server");

    // Ensure data directories exist
    std::fs::create_dir_all(&paths.data_dir)?;
    std::fs::create_dir_all(&paths.repos_dir)?;

    // Acquire lockfile
    acquire_lock(&paths)?;

    // Load config
    let config = config::load_config(&paths.config_file)?;
    info!("loaded {} project(s)", config.projects.len());

    // Load state
    let mut server_state = state::load_state(&paths.state_file)?;

    // Initial pool provisioning
    for project in &config.projects {
        let pool_size = config.effective_pool_size(project);
        info!(
            project = %project.name,
            pool_size,
            "ensuring pool"
        );
        if let Err(e) = pool::ensure_pool(&paths, project, pool_size, &mut server_state).await {
            error!(project = %project.name, "failed to provision pool: {e}");
        }
    }

    let shared_state = Arc::new(Mutex::new(server_state));
    let shared_config = Arc::new(config.clone());

    // Spawn refresh task
    let refresh_state = Arc::clone(&shared_state);
    let refresh_config = config.clone();
    let refresh_paths = paths.clone();
    tokio::spawn(async move {
        refresh_loop(refresh_state, &refresh_config, &refresh_paths).await;
    });

    // Spawn socket listener
    let socket_state = Arc::clone(&shared_state);
    let socket_config = Arc::clone(&shared_config);
    let socket_path = paths.socket_file.clone();
    let socket_handle = tokio::spawn(async move {
        if let Err(e) = socket::listen(socket_path, socket_state, socket_config).await {
            error!("socket listener failed: {e}");
        }
    });

    info!("aid server running, press Ctrl+C to stop");

    // Wait for shutdown signal
    tokio::signal::ctrl_c()
        .await
        .expect("failed to listen for ctrl-c");

    info!("shutting down");

    socket_handle.abort();

    // Save final state before cleaning up files
    let final_state = shared_state.lock().await;
    state::save_state(&paths.state_file, &final_state)?;
    drop(final_state);

    cleanup_on_shutdown(&paths);

    info!("shutdown complete");
    Ok(())
}

fn acquire_lock(paths: &Paths) -> Result<()> {
    let lock_path = &paths.lock_file;

    if lock_path.exists() {
        let contents = std::fs::read_to_string(lock_path)?;
        if let Ok(pid) = contents.trim().parse::<u32>() {
            if is_process_alive(pid) {
                return Err(ServerError::LockfileHeld {
                    pid,
                    path: lock_path.clone(),
                });
            }
            warn!("removing stale lockfile (pid {pid} is not running)");
        }
    }

    let pid = std::process::id();
    std::fs::write(lock_path, pid.to_string())?;
    info!("acquired lockfile at {} (pid {pid})", lock_path.display());
    Ok(())
}

fn is_process_alive(pid: u32) -> bool {
    std::process::Command::new("kill")
        .args(["-0", &pid.to_string()])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .is_ok_and(|s| s.success())
}

fn cleanup_on_shutdown(paths: &Paths) {
    let _ = std::fs::remove_file(&paths.lock_file);
    let _ = std::fs::remove_file(&paths.socket_file);
}

async fn refresh_loop(state: Arc<Mutex<state::ServerState>>, config: &Config, paths: &Paths) {
    let interval_secs = config.effective_refresh_interval();
    let mut interval = tokio::time::interval(std::time::Duration::from_secs(interval_secs));
    // Skip the first tick (we just provisioned)
    interval.tick().await;

    loop {
        interval.tick().await;
        info!("starting refresh cycle");

        for project in &config.projects {
            let pool_size = config.effective_pool_size(project);
            let mut server_state = state.lock().await;

            // First, ensure pool is full (in case new slots are needed)
            if let Err(e) = pool::ensure_pool(paths, project, pool_size, &mut server_state).await {
                error!(project = %project.name, "failed to ensure pool during refresh: {e}");
            }

            // Refresh each non-checked-out, ready slot
            let Some(project_state) = server_state.projects.get_mut(&project.name) else {
                continue;
            };

            let slots_to_refresh: Vec<u32> = project_state
                .slots
                .iter()
                .filter(|s| matches!(s.status, SlotStatus::Ready))
                .map(|s| s.slot)
                .collect();

            for slot_num in slots_to_refresh {
                if let Err(e) =
                    pool::refresh_slot(&paths.repos_dir, project, project_state, slot_num).await
                {
                    error!(
                        project = %project.name,
                        slot = slot_num,
                        "refresh failed: {e}"
                    );
                }
            }

            if let Err(e) = state::save_state(&paths.state_file, &server_state) {
                error!("failed to save state: {e}");
            }
        }

        info!("refresh cycle complete");
    }
}
