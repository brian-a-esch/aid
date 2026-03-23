use server::state::Paths;

fn main() -> anyhow::Result<()> {
    let args: Vec<String> = std::env::args().collect();
    match args.get(1).map(String::as_str) {
        Some("server") => {
            let paths = resolve_paths()?;
            tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()?
                .block_on(server::server::run(paths))?;
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
    let config_dir = dirs::config_dir()
        .ok_or_else(|| anyhow::anyhow!("could not determine config directory"))?
        .join("aid");
    let data_dir = dirs::data_local_dir()
        .ok_or_else(|| anyhow::anyhow!("could not determine data directory"))?
        .join("aid");
    Ok(Paths::new(&config_dir, &data_dir))
}
