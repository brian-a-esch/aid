use std::os::fd::OwnedFd;
use std::os::unix::net::UnixListener;

use tracing::{error, info, warn};
use tracing_subscriber::EnvFilter;

use crate::config::{self};
use crate::error::{Result, ServerError};
use crate::handler::AidHandler;
use crate::poll_loop;
use crate::state::{self, Paths};

pub fn run(paths: &Paths, shutdown_fd: OwnedFd, sigchild_fd: OwnedFd) -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")),
        )
        .init();

    info!("starting aid server");

    // Ensure data directories exist
    std::fs::create_dir_all(&paths.data_dir)?;
    std::fs::create_dir_all(&paths.repos_dir)?;

    acquire_lock(paths)?;
    run_with_lockfile(paths, shutdown_fd, sigchild_fd)?;
    cleanup_lock_and_sock(paths);

    info!("shutdown complete");
    Ok(())
}

fn run_with_lockfile(paths: &Paths, shutdown_fd: OwnedFd, sigchild_fd: OwnedFd) -> Result<()> {
    let config = config::load_config(&paths.config_file)?;
    info!("loaded {} project(s)", config.projects.len());

    let server_state = state::load_state(&paths.state_file)?;
    info!("loaded server state");

    let listener = UnixListener::bind(&paths.socket_file)?;
    info!("started listening on {}", paths.socket_file.display());

    let mut event_loop = poll_loop::EventLoop::new(
        listener,
        shutdown_fd,
        sigchild_fd,
        AidHandler::new(config, server_state, paths),
    )?;
    match event_loop.run() {
        Ok(()) => info!("aid server loop closed, exiting"),
        Err(e) => error!("aid server loop encountered error: {e}"),
    }

    Ok(())
}

fn acquire_lock(paths: &Paths) -> Result<()> {
    let lock_path = &paths.lock_file;
    info!("checking lockfile at path {}", paths.lock_file.display());

    if lock_path.exists() {
        let contents = std::fs::read_to_string(lock_path)?;
        if let Ok(pid) = contents.trim().parse::<u32>() {
            if is_process_alive(pid) {
                return Err(ServerError::LockfileHeld {
                    pid,
                    path: lock_path.clone(),
                });
            }
            warn!("removing stale lockfile & socket file (pid {pid} is not running)");
            cleanup_lock_and_sock(paths);
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

fn cleanup_lock_and_sock(paths: &Paths) {
    let _ = std::fs::remove_file(&paths.socket_file);
    let _ = std::fs::remove_file(&paths.lock_file);
}
