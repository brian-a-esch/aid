use std::path::PathBuf;

// TODO at some point we should purge errors if we are not using all of them
#[derive(Debug, thiserror::Error)]
pub enum ServerError {
    #[error("config error: {0}")]
    Config(String),

    #[error(transparent)]
    Io(#[from] std::io::Error),

    #[error(transparent)]
    Json(#[from] serde_json::Error),

    #[error("toml parse error: {0}")]
    Toml(#[from] toml::de::Error),

    #[error("another server is already running (pid {pid}, lockfile {path})")]
    LockfileHeld { pid: u32, path: PathBuf },

    #[error("pool error: {0}")]
    Pool(String),

    #[error("git error: {0}")]
    Git(String),

    #[error("build error: {0}")]
    Build(String),
}

pub type Result<T> = std::result::Result<T, ServerError>;
