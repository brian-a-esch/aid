use api::{ListFilter, PROTOCOL_VERSION, Request, RequestEnvelope, Response};
use cli::client;
use server::state::Paths;
use std::os::fd::AsRawFd;
use tracing_subscriber::EnvFilter;

fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")),
        )
        .init();

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
        Some("add") => {
            let project_name = args
                .get(2)
                .ok_or_else(|| anyhow::anyhow!("Usage: aid add <project_name> <checkout_name>"))?
                .clone();
            let checkout_name = args
                .get(3)
                .ok_or_else(|| anyhow::anyhow!("Usage: aid add <project_name> <checkout_name>"))?
                .clone();
            let paths = resolve_paths()?;
            run_add(&paths, project_name, checkout_name)?;
        }
        Some("list") => {
            let filter = parse_list_filter(&args[2..])?;
            let paths = resolve_paths()?;
            run_list(&paths, filter)?;
        }
        Some("cd") => {
            let checkout_name = args
                .get(2)
                .ok_or_else(|| anyhow::anyhow!("Usage: aid cd <checkout_name>"))?;
            let paths = resolve_paths()?;
            run_cd(&paths, checkout_name)?;
        }
        Some("init") => {
            let shell = args
                .get(2)
                .ok_or_else(|| anyhow::anyhow!("Usage: aid init <bash|zsh>"))?;
            run_init(shell)?;
        }
        Some("rm") => {
            let mut force = false;
            let mut rest = &args[2..];
            if rest.first().map(String::as_str) == Some("--force") {
                force = true;
                rest = &rest[1..];
            }
            let checkout_name = rest
                .first()
                .ok_or_else(|| anyhow::anyhow!("Usage: aid rm [--force] <checkout_name>"))?;
            let paths = resolve_paths()?;
            run_rm(&paths, checkout_name, force)?;
        }
        Some("completions") => {
            let subcmd = args
                .get(2)
                .ok_or_else(|| anyhow::anyhow!("Usage: aid completions <add|cd>"))?;
            let paths = resolve_paths()?;
            run_completions(&paths, subcmd)?;
        }
        Some(cmd) => {
            eprintln!("unknown command: {cmd}");
            eprintln!(
                "Usage: aid <server|add <project> <checkout>|list [--active|--free]|cd <checkout>|rm [--force] <checkout>|init <bash|zsh>>"
            );
            std::process::exit(1);
        }
        None => {
            eprintln!(
                "Usage: aid <server|add <project> <checkout>|list [--active|--free]|cd <checkout>|rm [--force] <checkout>|init <bash|zsh>>"
            );
            std::process::exit(1);
        }
    }
    Ok(())
}

fn run_add(paths: &Paths, project_name: String, checkout_name: String) -> anyhow::Result<()> {
    let req = RequestEnvelope {
        version: PROTOCOL_VERSION,
        request_id: "1".to_string(),
        content: Request::Add {
            project_name,
            checkout_name,
        },
    };

    let mut stream = client::connect(&paths.socket_file)?;
    client::send_request(&mut stream, &req)?;
    let resp = client::recv_response(&mut stream)?;

    match resp.content {
        Response::Added {
            checkout_name,
            path,
        } => {
            println!("checked out '{checkout_name}' at {path}");
        }
        Response::Error { message } => {
            eprintln!("error: {message}");
            std::process::exit(1);
        }
        Response::VersionMismatch { expected, got } => {
            eprintln!("protocol version mismatch: server expects {expected}, client sent {got}");
            std::process::exit(1);
        }
        other => {
            eprintln!("unexpected response: {other:?}");
            std::process::exit(1);
        }
    }

    Ok(())
}

fn parse_list_filter(args: &[String]) -> anyhow::Result<ListFilter> {
    let mut filter = ListFilter::All;
    for arg in args {
        match arg.as_str() {
            "--active" => filter = ListFilter::Active,
            "--free" => filter = ListFilter::Free,
            other => {
                anyhow::bail!("unknown flag for list: {other}");
            }
        }
    }
    Ok(filter)
}

fn run_list(paths: &Paths, filter: ListFilter) -> anyhow::Result<()> {
    let req = RequestEnvelope {
        version: PROTOCOL_VERSION,
        request_id: "1".to_string(),
        content: Request::List { filter },
    };

    let mut stream = client::connect(&paths.socket_file)?;
    client::send_request(&mut stream, &req)?;
    let resp = client::recv_response(&mut stream)?;

    match resp.content {
        Response::List(slot_infos) => {
            if slot_infos.slots.is_empty() {
                println!("No slots found.");
            } else {
                macro_rules! col_fmt {
                    ($a:expr, $b:expr, $c:expr, $d:expr $(,)?) => {
                        println!("{:<20} {:<20} {:<12} {}", $a, $b, $c, $d)
                    };
                }
                let home = std::env::var("HOME").unwrap_or_default();
                col_fmt!("CHECKOUT", "PROJECT", "STATUS", "PATH");
                for slot in &slot_infos.slots {
                    let name = slot.checkout_name.as_deref().unwrap_or("-");
                    let path_raw = slot.path.as_deref().unwrap_or("-");
                    let path = if home.is_empty() {
                        path_raw
                    } else {
                        path_raw.strip_prefix(&home).unwrap_or(path_raw)
                    };
                    col_fmt!(
                        name,
                        slot.project,
                        format!("{:?}", slot.status).to_lowercase(),
                        path,
                    );
                }
            }
        }
        Response::Error { message } => {
            eprintln!("error: {message}");
            std::process::exit(1);
        }
        Response::VersionMismatch { expected, got } => {
            eprintln!("protocol version mismatch: server expects {expected}, client sent {got}");
            std::process::exit(1);
        }
        other => {
            eprintln!("unexpected response: {other:?}");
            std::process::exit(1);
        }
    }

    Ok(())
}

/// Look up the path for `checkout_name` and print it to stdout.
/// The shell wrapper installed by `aid init` will `cd` to this path.
fn run_cd(paths: &Paths, checkout_name: &str) -> anyhow::Result<()> {
    let req = RequestEnvelope {
        version: PROTOCOL_VERSION,
        request_id: "1".to_string(),
        content: Request::List {
            filter: ListFilter::Active,
        },
    };

    let mut stream = client::connect(&paths.socket_file)?;
    client::send_request(&mut stream, &req)?;
    let resp = client::recv_response(&mut stream)?;

    match resp.content {
        Response::List(slot_infos) => {
            let slot = slot_infos
                .slots
                .iter()
                .find(|s| s.checkout_name.as_deref() == Some(checkout_name));

            if let Some(s) = slot {
                let path = s.path.as_deref().ok_or_else(|| {
                    anyhow::anyhow!("server returned no path for checkout '{checkout_name}'")
                })?;
                println!("{path}");
            } else {
                eprintln!("no active checkout named '{checkout_name}'");
                std::process::exit(1);
            }
        }
        Response::Error { message } => {
            eprintln!("error: {message}");
            std::process::exit(1);
        }
        Response::VersionMismatch { expected, got } => {
            eprintln!("protocol version mismatch: server expects {expected}, client sent {got}");
            std::process::exit(1);
        }
        other => {
            eprintln!("unexpected response: {other:?}");
            std::process::exit(1);
        }
    }

    Ok(())
}

/// Print one project name per line for `aid add` completions.
/// Print one active checkout name per line for `aid cd` completions.
fn run_completions(paths: &Paths, subcmd: &str) -> anyhow::Result<()> {
    let filter = match subcmd {
        "add" => ListFilter::All,
        "cd" => ListFilter::Active,
        other => anyhow::bail!("unknown completions target '{other}'; supported: add, cd"),
    };

    let req = RequestEnvelope {
        version: PROTOCOL_VERSION,
        request_id: "1".to_string(),
        content: Request::List { filter },
    };

    let mut stream = client::connect(&paths.socket_file)?;
    client::send_request(&mut stream, &req)?;
    let resp = client::recv_response(&mut stream)?;

    match resp.content {
        Response::List(slot_infos) => {
            match subcmd {
                "add" => {
                    // Emit each unique project name once.
                    let mut seen = std::collections::HashSet::new();
                    for slot in &slot_infos.slots {
                        if seen.insert(slot.project.as_ref()) {
                            println!("{}", slot.project);
                        }
                    }
                }
                "cd" => {
                    for slot in &slot_infos.slots {
                        if let Some(ref name) = slot.checkout_name {
                            println!("{name}");
                        }
                    }
                }
                _ => unreachable!(),
            }
        }
        Response::Error { message } => {
            eprintln!("error: {message}");
            std::process::exit(1);
        }
        other => {
            eprintln!("unexpected response: {other:?}");
            std::process::exit(1);
        }
    }

    Ok(())
}

/// Print a shell snippet that wraps `aid cd` so the shell actually changes directory,
/// and registers completions for `add` and `cd`.
fn run_init(shell: &str) -> anyhow::Result<()> {
    match shell {
        "bash" => {
            println!(
                r#"aid() {{
  if [ "$1" = "cd" ]; then
    local _aid_path
    _aid_path="$(command aid "$@")" || return 1
    builtin cd "$_aid_path"
  else
    command aid "$@"
  fi
}}

_aid_completions() {{
  local cur prev
  cur="${{COMP_WORDS[COMP_CWORD]}}"
  prev="${{COMP_WORDS[COMP_CWORD-1]}}"
  case "$prev" in
    add)
      COMPREPLY=( $(compgen -W "$(command aid completions add 2>/dev/null)" -- "$cur") )
      ;;
    cd|rm)
      COMPREPLY=( $(compgen -W "$(command aid completions cd 2>/dev/null)" -- "$cur") )
      ;;
    aid)
      COMPREPLY=( $(compgen -W "server add list cd rm init" -- "$cur") )
      ;;
  esac
}}

complete -F _aid_completions aid"#
            );
        }
        "zsh" => {
            println!(
                r#"aid() {{
  if [ "$1" = "cd" ]; then
    local _aid_path
    _aid_path="$(command aid "$@")" || return 1
    builtin cd "$_aid_path"
  else
    command aid "$@"
  fi
}}

_aid_completions() {{
  local state
  _arguments \
    '1: :->subcmd' \
    '2: :->arg' && return

  case $state in
    subcmd)
      _values 'subcommand' server add list cd rm init
      ;;
    arg)
      case $words[2] in
        add)
          local projects
          projects=($( command aid completions add 2>/dev/null ))
          _values 'project' $projects
          ;;
        cd|rm)
          local checkouts
          checkouts=($( command aid completions cd 2>/dev/null ))
          _values 'checkout' $checkouts
          ;;
      esac
      ;;
  esac
}}

compdef _aid_completions aid"#
            );
        }
        other => {
            anyhow::bail!("unsupported shell '{other}'; supported: bash, zsh");
        }
    }
    Ok(())
}

fn run_rm(paths: &Paths, checkout_name: &str, force: bool) -> anyhow::Result<()> {
    // First, resolve the project name by listing active slots.
    let list_req = RequestEnvelope {
        version: PROTOCOL_VERSION,
        request_id: "1".to_string(),
        content: Request::List {
            filter: ListFilter::Active,
        },
    };

    let mut stream = client::connect(&paths.socket_file)?;
    client::send_request(&mut stream, &list_req)?;
    let list_resp = client::recv_response(&mut stream)?;

    let project_name = match list_resp.content {
        Response::List(slot_infos) => {
            let slot = slot_infos
                .slots
                .iter()
                .find(|s| s.checkout_name.as_deref() == Some(checkout_name));
            if let Some(s) = slot {
                s.project.to_string()
            } else {
                eprintln!("no active checkout named '{checkout_name}'");
                std::process::exit(1);
            }
        }
        Response::Error { message } => {
            eprintln!("error: {message}");
            std::process::exit(1);
        }
        Response::VersionMismatch { expected, got } => {
            eprintln!("protocol version mismatch: server expects {expected}, client sent {got}");
            std::process::exit(1);
        }
        other => {
            eprintln!("unexpected response: {other:?}");
            std::process::exit(1);
        }
    };

    let rm_req = RequestEnvelope {
        version: PROTOCOL_VERSION,
        request_id: "2".to_string(),
        content: Request::Remove {
            project_name,
            checkout_name: checkout_name.to_string(),
            force,
        },
    };

    let mut stream = client::connect(&paths.socket_file)?;
    client::send_request(&mut stream, &rm_req)?;
    let resp = client::recv_response(&mut stream)?;

    match resp.content {
        Response::Ok => {
            println!("returned '{checkout_name}'");
        }
        Response::Error { message } => {
            eprintln!("error: {message}");
            std::process::exit(1);
        }
        Response::VersionMismatch { expected, got } => {
            eprintln!("protocol version mismatch: server expects {expected}, client sent {got}");
            std::process::exit(1);
        }
        other => {
            eprintln!("unexpected response: {other:?}");
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
