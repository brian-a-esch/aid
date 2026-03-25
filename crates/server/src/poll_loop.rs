use std::collections::{HashMap, VecDeque};
use std::io::{Read, Write};
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::os::unix::net::{UnixListener, UnixStream};
use std::process::{Child, Stdio};

use tracing::{error, info, warn};

static mut SIGNAL_WRITE_FD: RawFd = -1;
static mut SIGCHLD_WRITE_FD: RawFd = -1;

extern "C" fn shutdown_handler(_sig: libc::c_int) {
    unsafe {
        let byte: u8 = 1;
        let _ = libc::write(SIGNAL_WRITE_FD, std::ptr::from_ref(&byte).cast(), 1);
    }
}

extern "C" fn sigchild_handler(_sig: libc::c_int) {
    unsafe {
        let byte: u8 = 1;
        let _ = libc::write(SIGCHLD_WRITE_FD, std::ptr::from_ref(&byte).cast(), 1);
    }
}

/// Create a non-blocking pipe suitable for use as a signal self-pipe.
/// Returns `(read_end, write_end)`.
pub fn create_signal_pipe() -> std::io::Result<(OwnedFd, OwnedFd)> {
    let mut fds = [0 as RawFd; 2];
    if unsafe { libc::pipe(fds.as_mut_ptr()) } != 0 {
        return Err(std::io::Error::last_os_error());
    }
    let read_end = unsafe { OwnedFd::from_raw_fd(fds[0]) };
    let write_end = unsafe { OwnedFd::from_raw_fd(fds[1]) };
    set_nonblocking(read_end.as_raw_fd())?;
    set_nonblocking(write_end.as_raw_fd())?;
    Ok((read_end, write_end))
}

/// Install `SIGINT`/`SIGTERM` handlers that write to `shutdown_fd`,
/// and a `SIGCHLD` handler that writes to `sigchild_fd`.
pub fn install_signal_handlers(shutdown_fd: RawFd, sigchild_fd: RawFd) {
    unsafe {
        SIGNAL_WRITE_FD = shutdown_fd;
        SIGCHLD_WRITE_FD = sigchild_fd;

        let mut sa: libc::sigaction = std::mem::zeroed();
        sa.sa_sigaction = shutdown_handler as *const () as usize;
        sa.sa_flags = libc::SA_RESTART;
        libc::sigemptyset(&raw mut sa.sa_mask);
        libc::sigaction(libc::SIGINT, &raw const sa, std::ptr::null_mut());
        libc::sigaction(libc::SIGTERM, &raw const sa, std::ptr::null_mut());

        let mut sa_child: libc::sigaction = std::mem::zeroed();
        sa_child.sa_sigaction = sigchild_handler as *const () as usize;
        // Want SA_NOCLDSTOP because we only care about when child exits, not when it gets other stop
        // signals
        sa_child.sa_flags = libc::SA_RESTART | libc::SA_NOCLDSTOP;
        libc::sigemptyset(&raw mut sa_child.sa_mask);
        libc::sigaction(libc::SIGCHLD, &raw const sa_child, std::ptr::null_mut());
    }
}

fn drain_pipe(fd: RawFd) {
    let mut buf = [0u8; 64];
    loop {
        let n = unsafe { libc::read(fd, buf.as_mut_ptr().cast(), buf.len()) };
        if n <= 0 {
            break;
        }
    }
}

fn set_nonblocking(fd: RawFd) -> std::io::Result<()> {
    let flags = unsafe { libc::fcntl(fd, libc::F_GETFL) };
    if flags < 0 {
        return Err(std::io::Error::last_os_error());
    }
    if unsafe { libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK) } < 0 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(())
}

/// Result of a completed child process.
pub struct ChildExit {
    pub success: bool,
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
}

/// Application-level handler that the event loop delegates to.
pub trait Handler {
    /// Called with a complete newline-delimited message from a client.
    /// Return the response bytes to send back (should include trailing newline).
    fn handle_message(&mut self, msg: &[u8]) -> Vec<u8>;

    /// Called when a spawned child process exits.
    fn handle_child_exit(&mut self, result: ChildExit);

    /// Called when no child is running. Return a command to spawn, or `None`
    /// if there is no work to do.
    fn on_idle(&mut self) -> Option<std::process::Command>;
}

struct Client {
    stream: UnixStream,
    read_buf: Vec<u8>,
    write_buf: VecDeque<u8>,
}

impl Client {
    fn new(stream: UnixStream) -> Self {
        Self {
            stream,
            read_buf: Vec::with_capacity(4096),
            write_buf: VecDeque::new(),
        }
    }

    fn has_pending_writes(&self) -> bool {
        !self.write_buf.is_empty()
    }

    fn enqueue_bytes(&mut self, data: &[u8]) {
        self.write_buf.extend(data);
    }

    fn process_client_lines<H: Handler>(&mut self, handler: &mut H) {
        let lines = {
            let mut lines = Vec::new();
            loop {
                let newline_pos = self.read_buf.iter().position(|&b| b == b'\n');
                let Some(pos) = newline_pos else { break };
                let line: Vec<u8> = self.read_buf.drain(..=pos).collect();
                lines.push(line);
            }
            lines
        };

        for line in lines {
            let response = handler.handle_message(&line);
            self.enqueue_bytes(&response);
        }
    }

    fn handle_client_writable(&mut self) -> std::io::Result<()> {
        while !self.write_buf.is_empty() {
            let (front, _) = self.write_buf.as_slices();
            if front.is_empty() {
                break;
            }
            match self.stream.write(front) {
                Ok(0) => return Err(std::io::ErrorKind::WriteZero.into()),
                Ok(n) => {
                    self.write_buf.drain(..n);
                }
                Err(ref e) if e.kind() == std::io::ErrorKind::WouldBlock => break,
                Err(e) => return Err(e),
            }
        }
        Ok(())
    }

    fn handle_client_readable<H: Handler>(&mut self, handler: &mut H) -> std::io::Result<()> {
        // TODO cleanup
        let mut tmp_buf = [0u8; 4960];
        loop {
            match self.stream.read(&mut tmp_buf) {
                Ok(0) => return Err(std::io::ErrorKind::ConnectionReset.into()),
                Ok(n) => {
                    self.read_buf.extend_from_slice(&tmp_buf[..n]);
                }
                Err(ref e) if e.kind() == std::io::ErrorKind::WouldBlock => break,
                Err(e) => return Err(e),
            }
        }

        self.process_client_lines(handler);
        Ok(())
    }
}

struct RunningChild {
    child: Child,
    stdout: OwnedFd,
    stderr: OwnedFd,
    stdout_buf: Vec<u8>,
    stderr_buf: Vec<u8>,
}

fn read_child_fd(fd: RawFd, buf: &mut Vec<u8>, tmp_buf: &mut [u8]) {
    loop {
        let n = unsafe { libc::read(fd, tmp_buf.as_mut_ptr().cast(), tmp_buf.len()) };
        // We in fact do not care if this fails, just stop reading from it
        if n <= 0 {
            return;
        }
        buf.extend_from_slice(&tmp_buf[..n.cast_unsigned()]);
    }
}

pub struct EventLoop<H: Handler> {
    listener: UnixListener,
    signal_fd: OwnedFd,
    sigchild_fd: OwnedFd,
    clients: HashMap<RawFd, Client>,
    running_child: Option<RunningChild>,
    poll_fds: Vec<libc::pollfd>,
    handler: H,
    shutting_down: bool,
    read_buf: [u8; 4096],
}

impl<H: Handler> EventLoop<H> {
    /// Create a new event loop.
    ///
    /// - `listener`: a bound `UnixListener` (will be set to non-blocking).
    /// - `signal_fd`: the read end of a self-pipe for shutdown signaling
    ///   (`SIGINT`/`SIGTERM`).
    /// - `sigchild_fd`: the read end of a self-pipe for `SIGCHLD` notification.
    /// - `handler`: application logic for message dispatch and child completion.
    ///
    /// The caller is responsible for creating both pipes and installing signal
    /// handlers (see [`create_signal_pipe`] and [`install_signal_handlers`]).
    pub fn new(
        listener: UnixListener,
        signal_fd: OwnedFd,
        sigchild_fd: OwnedFd,
        handler: H,
    ) -> std::io::Result<Self> {
        set_nonblocking(listener.as_raw_fd())?;
        set_nonblocking(signal_fd.as_raw_fd())?;
        set_nonblocking(sigchild_fd.as_raw_fd())?;

        let listener_raw = listener.as_raw_fd();
        let signal_raw = signal_fd.as_raw_fd();
        let sigchild_raw = sigchild_fd.as_raw_fd();
        Ok(Self {
            listener,
            signal_fd,
            sigchild_fd,
            clients: HashMap::new(),
            poll_fds: vec![
                libc::pollfd {
                    fd: signal_raw,
                    events: libc::POLLIN,
                    revents: 0,
                },
                libc::pollfd {
                    fd: sigchild_raw,
                    events: libc::POLLIN,
                    revents: 0,
                },
                libc::pollfd {
                    fd: listener_raw,
                    events: libc::POLLIN,
                    revents: 0,
                },
            ],
            running_child: None,
            handler,
            shutting_down: false,
            read_buf: [0u8; 4096],
        })
    }

    /// Mutable access to the handler.
    pub fn handler(&self) -> &H {
        &self.handler
    }

    pub fn run(&mut self) -> std::io::Result<()> {
        info!("event loop running");

        while !self.shutting_down {
            self.prep_pollfds();

            let ret = unsafe {
                libc::poll(
                    self.poll_fds.as_mut_ptr(),
                    libc::nfds_t::try_from(self.poll_fds.len()).expect("too many fds"),
                    Self::POLL_TIMEOUT_MS,
                )
            };

            if ret < 0 {
                let err = std::io::Error::last_os_error();
                if err.kind() == std::io::ErrorKind::Interrupted {
                    continue;
                }
                return Err(err);
            }

            self.dispatch_poll_results();

            // Ask the handler for work if idle
            if self.running_child.is_none()
                && let Some(cmd) = self.handler.on_idle()
            {
                self.start_child(cmd);
            }
        }

        self.shutdown();
        Ok(())
    }

    fn prep_pollfds(&mut self) {
        // Reserved fds for basic loop
        self.poll_fds.truncate(3);
        for e in &mut self.poll_fds {
            e.revents = 0;
        }

        // Clients
        for (&fd, client) in &self.clients {
            let mut events = libc::POLLIN;
            if client.has_pending_writes() {
                events |= libc::POLLOUT;
            }
            self.poll_fds.push(libc::pollfd {
                fd,
                events,
                revents: 0,
            });
        }

        // Child process stdout/stderr
        if let Some(ref child) = self.running_child {
            self.poll_fds.push(libc::pollfd {
                fd: child.stdout.as_raw_fd(),
                events: libc::POLLIN,
                revents: 0,
            });
            self.poll_fds.push(libc::pollfd {
                fd: child.stderr.as_raw_fd(),
                events: libc::POLLIN,
                revents: 0,
            });
        }
    }

    const POLL_TIMEOUT_MS: libc::c_int = 5000;

    fn dispatch_poll_results(&mut self) {
        let mut idx = 0;
        let mut child_done = false;

        // Shutdown signal pipe
        if self.poll_fds[idx].revents & libc::POLLIN != 0 {
            self.handle_signal();
        }
        idx += 1;

        // SIGCHLD pipe — drain it; reaping happens below with child fds
        if self.poll_fds[idx].revents & libc::POLLIN != 0 {
            self.drain_sigchild();
            //
            child_done = true;
        }
        idx += 1;

        // Listener — accept after we finish processing existing client pollfds
        let listener_ready = self.poll_fds[idx].revents & libc::POLLIN != 0;
        idx += 1;

        // Clients — count must match what build_pollfds produced, which is the
        // number of entries between the fixed prefix (signal + sigchild + listener)
        // and the optional child suffix (0 or 2 entries).
        let child_suffix = if self.running_child.is_some() { 2 } else { 0 };
        let client_count = self.poll_fds.len() - idx - child_suffix;
        let client_pollfds: Vec<libc::pollfd> = self.poll_fds[idx..idx + client_count].to_vec();
        idx += client_count;

        let mut to_remove = Vec::new();
        for pfd in &client_pollfds {
            if pfd.revents & (libc::POLLERR | libc::POLLHUP | libc::POLLNVAL) != 0 {
                to_remove.push(pfd.fd);
                continue;
            }
            if pfd.revents & libc::POLLIN != 0 {
                let client = self.clients.get_mut(&pfd.fd).expect("client fd in map");
                if client.handle_client_readable(&mut self.handler).is_err() {
                    to_remove.push(pfd.fd);
                }
            }

            if pfd.revents & libc::POLLOUT != 0 {
                let client = self.clients.get_mut(&pfd.fd).expect("client fd in map");
                if client.handle_client_writable().is_err() {
                    to_remove.push(pfd.fd);
                }
            }
        }
        for fd in to_remove {
            self.remove_client(fd);
        }

        // Child process fds
        if let Some(ref mut child) = self.running_child {
            let stdout_revents = self.poll_fds[idx].revents;
            let stderr_revents = self.poll_fds[idx + 1].revents;

            if stdout_revents & libc::POLLIN != 0 {
                read_child_fd(
                    child.stdout.as_raw_fd(),
                    &mut child.stdout_buf,
                    &mut self.read_buf,
                );
            }
            if stderr_revents & libc::POLLIN != 0 {
                read_child_fd(
                    child.stderr.as_raw_fd(),
                    &mut child.stderr_buf,
                    &mut self.read_buf,
                );
            }
            self.try_reap_child(child_done);
        }

        // Accept new clients last so they don't interfere with pollfd indexing
        if listener_ready {
            self.accept_clients();
        }
    }

    fn handle_signal(&mut self) {
        drain_pipe(self.signal_fd.as_raw_fd());
        info!("received shutdown signal");
        self.shutting_down = true;
    }

    fn drain_sigchild(&mut self) {
        drain_pipe(self.sigchild_fd.as_raw_fd());
    }

    fn accept_clients(&mut self) {
        loop {
            match self.listener.accept() {
                Ok((stream, _addr)) => {
                    let fd = stream.as_raw_fd();
                    if let Err(e) = set_nonblocking(stream.as_raw_fd()) {
                        error!("failed to set client non-blocking: {e}");
                        continue;
                    }
                    info!(fd, "client connected");
                    self.clients.insert(fd, Client::new(stream));
                }
                Err(ref e) if e.kind() == std::io::ErrorKind::WouldBlock => break,
                Err(e) => {
                    error!("accept error: {e}");
                    break;
                }
            }
        }
    }

    fn remove_client(&mut self, fd: RawFd) {
        if self.clients.remove(&fd).is_some() {
            info!(fd, "client disconnected");
        }
    }

    fn start_child(&mut self, mut cmd: std::process::Command) {
        match cmd.stdout(Stdio::piped()).stderr(Stdio::piped()).spawn() {
            Ok(mut child) => {
                let stdout: OwnedFd = child.stdout.take().expect("piped stdout").into();
                let stderr: OwnedFd = child.stderr.take().expect("piped stderr").into();

                let _ = set_nonblocking(stdout.as_raw_fd());
                let _ = set_nonblocking(stderr.as_raw_fd());

                self.running_child = Some(RunningChild {
                    child,
                    stdout,
                    stderr,
                    stdout_buf: Vec::new(),
                    stderr_buf: Vec::new(),
                });
            }
            Err(e) => {
                error!("failed to spawn child: {e}");
                self.handler.handle_child_exit(ChildExit {
                    success: false,
                    stdout: Vec::new(),
                    stderr: format!("spawn error: {e}").into_bytes(),
                });
            }
        }
    }

    fn try_reap_child(&mut self, child_sig: bool) {
        let Some(ref mut rc) = self.running_child else {
            return;
        };

        match rc.child.try_wait() {
            Ok(Some(status)) => {
                let exit = ChildExit {
                    success: status.success(),
                    stdout: std::mem::take(&mut rc.stdout_buf),
                    stderr: std::mem::take(&mut rc.stderr_buf),
                };
                self.running_child = None;
                self.handler.handle_child_exit(exit);
            }
            Ok(None) => {
                // still running
                if child_sig {
                    warn!("child signal received but child process did not exit");
                }
            }
            Err(e) => {
                warn!("error waiting for child: {e}");
            }
        }
    }

    fn shutdown(&mut self) {
        info!("shutting down event loop");

        if let Some(ref mut rc) = self.running_child {
            let _ = rc.child.kill();
            let _ = rc.child.wait();
        }
        self.running_child = None;

        self.clients.clear();

        info!("shutdown complete");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{BufRead, BufReader, Write};
    use std::os::unix::net::UnixStream;

    use std::sync::atomic::{AtomicU64, Ordering};

    static TEST_COUNTER: AtomicU64 = AtomicU64::new(0);

    /// Helper: create a UDS listener bound to a unique temp path.
    fn tmp_listener() -> (UnixListener, std::path::PathBuf) {
        let n = TEST_COUNTER.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir();
        let path = dir.join(format!("poll_loop_test_{}_{n}.sock", std::process::id()));
        let _ = std::fs::remove_file(&path);
        let listener = UnixListener::bind(&path).expect("bind");
        (listener, path)
    }

    /// Helper: create a pipe and return (read_fd, write_fd) as OwnedFds.
    fn pipe_pair() -> (OwnedFd, OwnedFd) {
        create_signal_pipe().expect("pipe")
    }

    /// Helper: write a byte to the signal pipe to trigger shutdown.
    fn signal_shutdown(write_end: &OwnedFd) {
        let byte: u8 = 1;
        unsafe {
            libc::write(write_end.as_raw_fd(), std::ptr::from_ref(&byte).cast(), 1);
        }
    }

    // ── Ping-pong handler ───────────────────────────────────────

    /// Echoes messages back, replacing "ping" with "pong".
    /// When `spawn_child` is true, spawns a subprocess once via `on_idle`.
    /// After the child exits, "ping" produces "pong\npong\n" instead of "pong\n".
    /// Clients can query "child_done?\n" to synchronize on child completion.
    struct PingPong {
        messages_received: usize,
        spawn_child: bool,
        child_spawned: bool,
        child_done: bool,
    }

    impl PingPong {
        fn new() -> Self {
            Self {
                messages_received: 0,
                spawn_child: false,
                child_spawned: false,
                child_done: false,
            }
        }
    }

    impl Handler for PingPong {
        fn handle_message(&mut self, msg: &[u8]) -> Vec<u8> {
            self.messages_received += 1;
            let text = String::from_utf8_lossy(msg);
            let trimmed = text.trim();
            if trimmed == "child_done?" {
                if self.child_done {
                    b"yes\n".to_vec()
                } else {
                    b"no\n".to_vec()
                }
            } else if trimmed == "ping" {
                if self.child_done {
                    b"pong\npong\n".to_vec()
                } else {
                    b"pong\n".to_vec()
                }
            } else {
                format!("echo: {trimmed}\n").into_bytes()
            }
        }

        fn handle_child_exit(&mut self, _result: ChildExit) {
            self.child_done = true;
        }

        fn on_idle(&mut self) -> Option<std::process::Command> {
            if self.spawn_child && !self.child_spawned {
                self.child_spawned = true;
                let mut cmd = std::process::Command::new("echo");
                cmd.arg("hello");
                Some(cmd)
            } else {
                None
            }
        }
    }

    #[test]
    fn ping_pong() {
        let (listener, sock_path) = tmp_listener();
        let (sig_read, sig_write) = pipe_pair();
        let (child_read, _child_write) = pipe_pair();

        let mut ev = EventLoop::new(listener, sig_read, child_read, PingPong::new())
            .expect("EventLoop::new");

        let path = sock_path.clone();
        let client_thread = std::thread::spawn(move || {
            let mut stream = UnixStream::connect(&path).expect("connect");
            let mut reader = BufReader::new(stream.try_clone().expect("clone"));

            // Send multiple messages
            stream.write_all(b"ping\n").expect("write");
            let mut line = String::new();
            reader.read_line(&mut line).expect("read");
            assert_eq!(line.trim(), "pong");

            line.clear();
            stream.write_all(b"hello\n").expect("write");
            reader.read_line(&mut line).expect("read");
            assert_eq!(line.trim(), "echo: hello");

            line.clear();
            stream.write_all(b"ping\n").expect("write");
            reader.read_line(&mut line).expect("read");
            assert_eq!(line.trim(), "pong");

            signal_shutdown(&sig_write);
        });

        ev.run().expect("run");
        client_thread.join().expect("client thread");

        assert_eq!(ev.handler().messages_received, 3);
        let _ = std::fs::remove_file(&sock_path);
    }

    #[test]
    fn signal_shutdown_no_clients() {
        let (listener, sock_path) = tmp_listener();
        let (sig_read, sig_write) = pipe_pair();
        let (child_read, _child_write) = pipe_pair();

        let mut ev = EventLoop::new(listener, sig_read, child_read, PingPong::new())
            .expect("EventLoop::new");

        // Signal immediately from another thread
        let shutdown_thread = std::thread::spawn(move || {
            std::thread::sleep(std::time::Duration::from_millis(50));
            signal_shutdown(&sig_write);
        });

        ev.run().expect("run");
        shutdown_thread.join().expect("shutdown thread");

        assert_eq!(ev.handler().messages_received, 0);
        let _ = std::fs::remove_file(&sock_path);
    }

    #[test]
    fn child_exit_changes_behavior() {
        let (listener, sock_path) = tmp_listener();
        let (sig_read, sig_write) = pipe_pair();
        let (child_read, _child_write) = pipe_pair();
        let mut ping_pong = PingPong::new();
        ping_pong.spawn_child = true;

        let mut ev =
            EventLoop::new(listener, sig_read, child_read, ping_pong).expect("EventLoop::new");

        let path = sock_path.clone();
        let client_thread = std::thread::spawn(move || {
            let mut stream = UnixStream::connect(&path).expect("connect");
            let mut reader = BufReader::new(stream.try_clone().expect("clone"));
            let mut line = String::new();

            // Before child completes: ping -> single pong
            stream.write_all(b"ping\n").expect("write");
            reader.read_line(&mut line).expect("read");
            assert_eq!(line.trim(), "pong");

            // Wait for the child to be reaped by querying through the event loop
            loop {
                line.clear();
                stream.write_all(b"child_done?\n").expect("write");
                reader.read_line(&mut line).expect("read");
                if line.trim() == "yes" {
                    break;
                }
            }

            // After child completes: ping -> pong repeated twice
            line.clear();
            stream.write_all(b"ping\n").expect("write");
            reader.read_line(&mut line).expect("read");
            assert_eq!(line.trim(), "pong");
            line.clear();
            reader.read_line(&mut line).expect("read");
            assert_eq!(line.trim(), "pong");

            signal_shutdown(&sig_write);
        });

        ev.run().expect("run");
        client_thread.join().expect("client thread");

        let _ = std::fs::remove_file(&sock_path);
    }
}
