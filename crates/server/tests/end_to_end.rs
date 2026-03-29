/// End to end test for the server
///
/// These tests spin up a real `server::run()` in a background thread, pointed at a
/// locally-created git repository, and verify that the server reaches the
/// expected state (slots cloned and at `Ready`) before being shut down cleanly.
///
/// Each test gets its own isolated tempdir so tests can run in parallel without
/// interfering with each other's socket files, lock files, or repo directories.
use std::os::fd::{AsRawFd, RawFd};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Once;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use tracing::info;

use server::poll_loop::create_signal_pipe;
use server::state::Paths;

static TEST_COUNTER: AtomicU64 = AtomicU64::new(0);
static LOGGING_INIT: Once = Once::new();

fn init_logging() {
    LOGGING_INIT.call_once(|| {
        tracing_subscriber::fmt()
            //.with_timer(ChronoUtc::rfc_3339())
            .with_test_writer()
            .init();
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

fn send_shutdown(shutdown_write: RawFd) {
    let byte: u8 = 1;
    let s = unsafe { libc::write(shutdown_write, std::ptr::from_ref(&byte).cast(), 1) };
    assert!(s == 1);
}

#[test]
fn server_clones_repo_to_ready() {
    init_logging();
    let (paths, base_dir) = test_paths();

    // Create a local repo and write config pointing at it
    let repo = make_repo(&base_dir);
    write_config(
        repo.to_str().expect("repo path is valid UTF-8"),
        &paths.config_file,
    );
    info!("created repo {repo:?}");

    // Startup the server
    let (shutdown_read, shutdown_write) =
        create_signal_pipe().expect("create shutdown signal pipe");
    let (sigchild_read, _sigchild_write) =
        create_signal_pipe().expect("create sigchild signal pipe");

    let paths_clone = paths.clone();
    let server_thread =
        std::thread::spawn(move || server::server::run(&paths_clone, shutdown_read, sigchild_read));

    wait_until(Duration::from_secs(5), || paths.socket_file.exists());

    let repo_dir = paths.repos_dir.join("test-project").join("0");
    let cloned = wait_until(Duration::from_secs(5), || repo_dir.join(".git").exists());

    send_shutdown(shutdown_write.as_raw_fd());
    info!("sent shutdown signal");

    let server_result = server_thread.join().expect("server thread panicked");
    assert!(
        server_result.is_ok(),
        "server::run returned an error: {server_result:?}"
    );

    // Check things in the repo to make sure it was cloned
    assert!(
        cloned,
        "cloned repo directory never appeared at {repo_dir:?}"
    );
    assert!(
        repo_dir.join(".git").exists(),
        "expected .git directory at {repo_dir:?}"
    );
    assert!(
        repo_dir.join("README.md").exists(),
        "README.md should be present in the cloned repo"
    );
    let content = std::fs::read_to_string(repo_dir.join("README.md")).unwrap();
    assert_eq!(content, "hello\n");

    let _ = std::fs::remove_dir_all(&base_dir);
}
