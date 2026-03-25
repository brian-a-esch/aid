use std::sync::Arc;

use serde::{Deserialize, Serialize};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::Mutex;
use tracing::{error, info};

use crate::config::Config;
use crate::error::Result;
use crate::state::ServerState;

#[derive(Debug, Deserialize)]
pub struct Request {
    pub method: String,
    pub params: Option<serde_json::Value>,
}

#[derive(Debug, Serialize)]
pub struct Response {
    pub ok: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub data: Option<serde_json::Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

impl Response {
    #[must_use]
    pub fn success(data: serde_json::Value) -> Self {
        Self {
            ok: true,
            data: Some(data),
            error: None,
        }
    }

    pub fn error(msg: impl Into<String>) -> Self {
        Self {
            ok: false,
            data: None,
            error: Some(msg.into()),
        }
    }
}

pub type SharedState = Arc<Mutex<ServerState>>;
pub type SharedConfig = Arc<Config>;

pub async fn listen(
    socket_path: std::path::PathBuf,
    state: SharedState,
    config: SharedConfig,
) -> Result<()> {
    // Remove stale socket if it exists
    if socket_path.exists() {
        std::fs::remove_file(&socket_path)?;
    }

    let listener = UnixListener::bind(&socket_path)?;
    info!("listening on {}", socket_path.display());

    loop {
        match listener.accept().await {
            Ok((stream, _addr)) => {
                let state = Arc::clone(&state);
                let config = Arc::clone(&config);
                tokio::spawn(async move {
                    if let Err(e) = handle_connection(stream, state, config).await {
                        error!("connection error: {e}");
                    }
                });
            }
            Err(e) => {
                error!("accept error: {e}");
            }
        }
    }
}

async fn handle_connection(
    stream: UnixStream,
    state: SharedState,
    config: SharedConfig,
) -> Result<()> {
    let (reader, mut writer) = stream.into_split();
    let mut reader = BufReader::new(reader);
    let mut line = String::new();

    while reader.read_line(&mut line).await? > 0 {
        let response = match serde_json::from_str::<Request>(&line) {
            Ok(req) => dispatch(&req, &state, &config).await,
            Err(e) => Response::error(format!("invalid request: {e}")),
        };

        let mut resp_json = serde_json::to_string(&response)?;
        resp_json.push('\n');
        writer.write_all(resp_json.as_bytes()).await?;

        line.clear();
    }

    Ok(())
}

async fn dispatch(req: &Request, state: &SharedState, config: &SharedConfig) -> Response {
    match req.method.as_str() {
        "status" => handle_status(state).await,
        "list_projects" => handle_list_projects(state, config).await,
        _ => Response::error(format!("unknown method: {}", req.method)),
    }
}

async fn handle_status(state: &SharedState) -> Response {
    let state = state.lock().await;
    match serde_json::to_value(&*state) {
        Ok(v) => Response::success(v),
        Err(e) => Response::error(e.to_string()),
    }
}

async fn handle_list_projects(state: &SharedState, config: &SharedConfig) -> Response {
    let state = state.lock().await;

    let project_list: Vec<serde_json::Value> = config
        .projects
        .iter()
        .map(|p| {
            let project_state = state.projects.get(&p.name);
            serde_json::json!({
                "name": p.name,
                "repo_url": p.repo_url,
                "branch": p.effective_branch(),
                "slots": project_state.map(|ps| &ps.slots),
            })
        })
        .collect();

    Response::success(serde_json::json!({ "projects": project_list }))
}
