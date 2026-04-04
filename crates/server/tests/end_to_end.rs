/// End to end test for the server
///
/// These tests spin up a real `server::run()` in a background thread, pointed at a
/// locally-created git repository, and verify that the server reaches the
/// expected state (slots cloned and at `Ready`) before being shut down cleanly.
///
/// Each test gets its own isolated tempdir so tests can run in parallel without
/// interfering with each other's socket files, lock files, or repo directories.
use std::io::{BufRead, BufReader, Write};
use std::os::fd::{AsRawFd, OwnedFd, RawFd};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Once;
use std::sync::atomic::{AtomicU64, Ordering};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use api::{
    Envelope, ListFilter, PROTOCOL_VERSION, Request, Response, ResponseEnvelope, SlotStatusSummary,
};

use tracing::info;

use server::poll_loop::create_signal_pipe;
use server::state::Paths;

static TEST_COUNTER: AtomicU64 = AtomicU64::new(0);
static LOGGING_INIT: Once = Once::new();

fn init_logging() {
    LOGGING_INIT.call_once(|| {
        tracing_subscriber::fmt().with_test_writer().init();
    });
}

fn test_paths() -> (Paths, PathBuf) {
    let n = TEST_COUNTER.fetch_add(1, Ordering::Relaxed);
    let dir = std::env::temp_dir().join(format!("aid_integration_{}_{n}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("create test tempdir");

    let paths = Paths::new(&dir, &dir);
    info!("Using data dir {dir:?}");
    (paths, dir)
}

/// Run a git command
fn git(args: &[&str]) -> bool {
    let out = Command::new("git")
        .args(args)
        .output()
        .unwrap_or_else(|e| panic!("failed to spawn git {args:?}: {e}"));
    // If we want to inspect the stderr/stdout, we can do
    //print!("{}", String::from_utf8_lossy(&out.stdout/&out.stderr));
    out.status.success()
}

/// Create a minimal local git repository that the server can clone from
fn make_repo(base_dir: &Path) -> PathBuf {
    let repo = base_dir.join("repo");
    std::fs::create_dir_all(&repo).expect("create repo dir");

    assert!(
        git(&["init", "-b", "main", repo.to_str().unwrap()]),
        "git init failed"
    );
    for (key, val) in [("user.email", "test@example.com"), ("user.name", "Test")] {
        git(&["-C", repo.to_str().unwrap(), "config", key, val]);
    }

    std::fs::write(repo.join("README.md"), "hello\n").expect("write README");

    assert!(
        git(&["-C", repo.to_str().unwrap(), "add", "."]),
        "git add failed"
    );

    assert!(
        git(&[
            "-C",
            repo.to_str().unwrap(),
            "commit",
            "-m",
            "initial commit"
        ]),
        "git commit failed"
    );

    repo
}

/// Write the integration test config fixture to `config_file`, substituting
/// `repo_url` for the `__REPO_URL__` placeholder in the template.
fn write_config(repo_url: &str, config_file: &Path) {
    let template = include_str!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/testdata/end_to_end.toml"
    ));
    let toml = template.replace("__REPO_URL__", repo_url);
    std::fs::write(config_file, toml).expect("write config.toml");
}

/// Poll until `pred` returns `true` or `timeout` elapses.
fn wait_until(timeout: Duration, pred: impl Fn() -> bool) -> bool {
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        if pred() {
            return true;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    false
}

fn list_request(id: &str) -> api::RequestEnvelope {
    Envelope {
        version: PROTOCOL_VERSION,
        request_id: id.to_string(),
        content: Request::List {
            filter: ListFilter::All,
        },
    }
}

fn add_request(id: &str, project_name: &str, checkout_name: &str) -> api::RequestEnvelope {
    Envelope {
        version: PROTOCOL_VERSION,
        request_id: id.to_string(),
        content: Request::Add {
            project_name: project_name.to_string(),
            checkout_name: checkout_name.to_string(),
        },
    }
}

fn remove_request(
    id: &str,
    project_name: &str,
    checkout_name: &str,
    force: bool,
) -> api::RequestEnvelope {
    Envelope {
        version: PROTOCOL_VERSION,
        request_id: id.to_string(),
        content: Request::Remove {
            project_name: project_name.to_string(),
            checkout_name: checkout_name.to_string(),
            force,
        },
    }
}

struct TestServer {
    pub paths: Paths,
    pub base_dir: PathBuf,
    shutdown_write: OwnedFd,
    server_thread: Option<JoinHandle<server::error::Result<()>>>,
}

impl TestServer {
    fn start(paths: &Paths, base_dir: PathBuf, repo_url: &str) -> Self {
        init_logging();
        write_config(repo_url, &paths.config_file);

        let (shutdown_read, shutdown_write) =
            create_signal_pipe().expect("create shutdown signal pipe");
        let (sigchild_read, _sigchild_write) =
            create_signal_pipe().expect("create sigchild signal pipe");

        let paths_clone = paths.clone();
        let server_thread = std::thread::spawn(move || {
            server::server::run(&paths_clone, shutdown_read, sigchild_read)
        });

        let server = TestServer {
            paths: paths.clone(),
            base_dir,
            shutdown_write,
            server_thread: Some(server_thread),
        };

        assert!(wait_until(Duration::from_secs(5), || {
            server.paths.socket_file.exists()
        }));

        server
    }

    fn wait_for_ready(&self, project: &str) {
        let req = list_request("poll");
        let project = project.to_string();
        assert!(wait_until(Duration::from_secs(15), || {
            if let Response::List(slots) = self.send(&req).content {
                slots.slots.iter().any(|s| {
                    s.status == SlotStatusSummary::Ready && s.project == project.as_str().into()
                })
            } else {
                false
            }
        }));
    }

    fn send(&self, req: &api::RequestEnvelope) -> ResponseEnvelope {
        let mut stream =
            UnixStream::connect(&self.paths.socket_file).expect("connect to server socket");

        let mut bytes = api::serialize_request(req).expect("serialize request");
        bytes.push(b'\n');
        stream.write_all(&bytes).expect("write request");

        let mut reader = BufReader::new(&stream);
        let mut line = String::new();
        reader.read_line(&mut line).expect("read response");
        api::deserialize_response(line.trim_end().as_bytes()).expect("deserialize response")
    }

    fn shutdown(&mut self) {
        Self::signal_shutdown(self.shutdown_write.as_raw_fd());
        info!("sent shutdown signal");
        let thread = self.server_thread.take().expect("server already stopped");
        assert!(wait_until(Duration::from_secs(5), || thread.is_finished()));
        thread
            .join()
            .expect("server thread panicked")
            .expect("server error");
    }

    fn signal_shutdown(fd: RawFd) {
        let byte: u8 = 1;
        let written = unsafe { libc::write(fd, std::ptr::from_ref(&byte).cast(), 1) };
        assert_eq!(written, 1);
    }

    fn checkout(&self, project_name: &str, checkout_name: &str) -> String {
        let resp = self.send(&add_request("add", project_name, checkout_name));
        match resp.content {
            Response::Added { path, .. } => path,
            other => panic!("expected Added, got {other:?}"),
        }
    }

    fn list_slots(&self) -> Vec<api::SlotInfo> {
        match self.send(&list_request("list")).content {
            Response::List(s) => s.slots,
            other => panic!("expected List, got {other:?}"),
        }
    }

    fn assert_remove_fails(&self, project_name: &str, checkout_name: &str, expected_substr: &str) {
        let resp = self.send(&remove_request("rm", project_name, checkout_name, false));
        match resp.content {
            Response::Error { message } => {
                assert!(message.contains(expected_substr), "{message:?}")
            }
            other => panic!("expected Error, got {other:?}"),
        }
    }
}

impl Drop for TestServer {
    fn drop(&mut self) {
        if let Some(_) = self.server_thread.take() {
            panic!("server thread not exited by the time we drop!!")
        }
        let _ = std::fs::remove_dir_all(&self.base_dir);
    }
}

#[test]
fn server_clones_repo_to_ready() {
    let (paths, base_dir) = test_paths();
    let repo = make_repo(&base_dir);
    let mut server = TestServer::start(&paths, base_dir, repo.to_str().unwrap());

    server.wait_for_ready("test-project");

    // Explicitly verify the response round-trip, even though the "wait_for_ready" does this
    let req = list_request("1");
    let resp = server.send(&req);
    assert_eq!(resp.request_id, "1");

    let repo_dir = server.paths.repos_dir.join("test-project").join("0");
    assert!(repo_dir.join(".git").exists());
    assert!(repo_dir.join("README.md").exists());
    let content = std::fs::read_to_string(repo_dir.join("README.md")).unwrap();
    assert_eq!(content, "hello\n");
    server.shutdown();
}

#[test]
fn checkout_triggers_background_provisioning_and_return_restores_ready() {
    let (paths, base_dir) = test_paths();
    let repo = make_repo(&base_dir);
    let mut server = TestServer::start(&paths, base_dir, repo.to_str().unwrap());

    server.wait_for_ready("test-project");
    let checkout_path = server.checkout("test-project", "my-checkout");

    let slots = server.list_slots();
    assert!(
        slots
            .iter()
            .any(|s| s.status == SlotStatusSummary::CheckedOut)
    );

    server.wait_for_ready("test-project");
    let slots = server.list_slots();
    assert!(slots.iter().any(|s| s.status == SlotStatusSummary::Ready));

    assert!(std::path::Path::new(&checkout_path).join(".git").exists());

    let remove_resp = server.send(&remove_request("rm1", "test-project", "my-checkout", false));
    assert_eq!(remove_resp.content, Response::Ok);

    assert!(wait_until(Duration::from_secs(5), || {
        server
            .list_slots()
            .iter()
            .filter(|s| s.status == SlotStatusSummary::Ready)
            .count()
            >= 2
    }));

    server.shutdown();
}

/// Checkout a slot, modify a tracked file, and verify that returning the slot
/// is rejected because the working tree is dirty.
#[test]
fn return_fails_when_readme_is_modified() {
    let (paths, base_dir) = test_paths();
    let repo = make_repo(&base_dir);
    let mut server = TestServer::start(&paths, base_dir, repo.to_str().unwrap());

    server.wait_for_ready("test-project");
    let checkout_path = server.checkout("test-project", "dirty-checkout");

    std::fs::write(
        std::path::Path::new(&checkout_path).join("README.md"),
        "modified content\n",
    )
    .expect("write README");

    server.assert_remove_fails("test-project", "dirty-checkout", "dirty");
    server.shutdown();
}

#[test]
fn return_fails_when_untracked_file_present() {
    let (paths, base_dir) = test_paths();
    let repo = make_repo(&base_dir);
    let mut server = TestServer::start(&paths, base_dir, repo.to_str().unwrap());

    server.wait_for_ready("test-project");
    let checkout_path = server.checkout("test-project", "untracked-checkout");

    std::fs::write(
        std::path::Path::new(&checkout_path).join("new_untracked.txt"),
        "untracked\n",
    )
    .expect("write untracked file");

    server.assert_remove_fails("test-project", "untracked-checkout", "dirty");
    server.shutdown();
}

#[test]
fn return_fails_when_unpushed_commit_exists() {
    let (paths, base_dir) = test_paths();
    let repo = make_repo(&base_dir);
    let mut server = TestServer::start(&paths, base_dir, repo.to_str().unwrap());

    server.wait_for_ready("test-project");
    let checkout_path = server.checkout("test-project", "unpushed-checkout");

    std::fs::write(
        std::path::Path::new(&checkout_path).join("README.md"),
        "committed but not pushed\n",
    )
    .expect("write README");
    assert!(git(&["-C", &checkout_path, "add", "README.md"]));
    assert!(git(&["-C", &checkout_path, "commit", "-m", "local change"]));

    server.assert_remove_fails("test-project", "unpushed-checkout", "unpushed");
    server.shutdown();
}
