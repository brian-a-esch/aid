use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::path::Path;

use anyhow::{Context, anyhow};
use api::{RequestEnvelope, ResponseEnvelope};

/// Open a connection to the server's Unix domain socket.
pub fn connect(socket_path: &Path) -> anyhow::Result<UnixStream> {
    UnixStream::connect(socket_path)
        .with_context(|| format!("failed to connect to socket {}", socket_path.display()))
}

/// Serialize `req` and write it to `stream` as a single newline-terminated line.
pub fn send_request(stream: &mut UnixStream, req: &RequestEnvelope) -> anyhow::Result<()> {
    let mut bytes = api::serialize_request(req).context("failed to serialize request")?;
    bytes.push(b'\n');
    stream
        .write_all(&bytes)
        .context("failed to write request to socket")
}

/// Read one newline-terminated line from `stream` and deserialize it as a
/// [`ResponseEnvelope`].
pub fn recv_response(stream: &mut UnixStream) -> anyhow::Result<ResponseEnvelope> {
    let mut reader = BufReader::new(stream);
    let mut line = String::new();
    reader
        .read_line(&mut line)
        .context("failed to read response from socket")?;
    if line.is_empty() {
        return Err(anyhow!(
            "server closed the connection without sending a response"
        ));
    }
    api::deserialize_response(line.trim_end_matches('\n').as_bytes())
        .context("failed to deserialize response")
}
