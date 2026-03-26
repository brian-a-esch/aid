use server::state::Paths;
use std::os::fd::AsRawFd;

fn main() -> anyhow::Result<()> {
    let args: Vec<String> = std::env::args().collect();
    match args.get(1).map(String::as_str) {
        Some("server") => {
            let paths = resolve_paths()?;
            let (shutdown_read, shutdown_write) = server::poll_loop::create_signal_pipe()?;
            let (sigchild_read, sigchild_write) = server::poll_loop::create_signal_pipe()?;
            server::poll_loop::install_signal_handlers(
                shutdown_write.as_raw_fd(),
                sigchild_write.as_raw_fd(),
            );
            server::server::run(&paths, shutdown_read, sigchild_read)?;
        }
        Some(cmd) => {
            eprintln!("unknown command: {cmd}");
            eprintln!("Usage: aid server");
            std::process::exit(1);
        }
        None => {
            eprintln!("Usage: aid server");
            std::process::exit(1);
        }
    }
    Ok(())
}

fn resolve_paths() -> anyhow::Result<Paths> {
    let home = std::env::var("HOME")
        .map_err(|_| anyhow::anyhow!("HOME environment variable is not set"))?;
    let home = std::path::Path::new(&home);

    let config_dir = std::env::var("XDG_CONFIG_HOME")
        .map_or_else(|_| home.join(".config"), std::path::PathBuf::from)
        .join("aid");

    let data_dir = std::env::var("XDG_STATE_HOME")
        .map_or_else(|_| home.join(".local/state"), std::path::PathBuf::from)
        .join("aid");

    Ok(Paths::new(&config_dir, &data_dir))
}
