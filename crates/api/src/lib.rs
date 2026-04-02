pub const PROTOCOL_VERSION: u32 = 1;

use serde::{Deserialize, Serialize};

/// Filter applied to the `List` command.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ListFilter {
    /// All slots, both in-use and background
    All,
    /// Only slots that are currently checked out
    Active,
    /// Only slots that are ready and not checked out
    Free,
}

/// The payload of a client request.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Request {
    /// Allocate a ready slot for the given project
    Add {
        project_name: String,
        checkout_name: String,
    },
    /// List slots, optionally filtered
    List { filter: ListFilter },
    /// Return a checked-out slot to the pool
    Remove {
        checkout_name: String,
        /// Skip dirty-tree check.
        force: bool,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SlotStatusSummary {
    Uninitialized,
    Cloning,
    Building,
    Ready,
    CheckedOut,
    Error,
}

/// A flat, serialization-friendly view of a slot shown in list responses.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SlotInfo {
    pub project: String,
    /// Set when the slot is currently checked out.
    pub checkout_name: Option<String>,
    pub status: SlotStatusSummary,
    /// ISO-8601 timestamp of the last successful refresh, if any.
    pub last_refreshed: Option<String>,
    pub error_message: Option<String>,
}

/// The payload of a server response.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Response {
    /// Generic success with no additional data.
    Ok,
    /// Returned after a successful `Add`; carries the path that was handed out.
    Added { checkout_name: String, path: String },
    /// Returned for `List` requests.
    List(Vec<SlotInfo>),
    /// The server encountered an error while processing the request.
    Error { message: String },
    /// The client's protocol version does not match the server's.
    VersionMismatch { expected: u32, got: u32 },
}

/// Wrapper that adds a protocol `version` and a caller-supplied `request_id`
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Envelope<T> {
    pub version: u32,
    pub request_id: String,
    pub content: T,
}

pub type RequestEnvelope = Envelope<Request>;
pub type ResponseEnvelope = Envelope<Response>;

/// Serialize a [`RequestEnvelope`] to JSON bytes (no trailing newline).
///
/// # Errors
/// Returns [`serde_json::Error`] if serialization fails (should be infallible
/// for well-formed types, but the signature is kept explicit for callers).
pub fn serialize_request(r: &RequestEnvelope) -> Result<Vec<u8>, serde_json::Error> {
    serde_json::to_vec(r)
}

/// Deserialize a [`RequestEnvelope`] from JSON bytes (newline stripped by the
/// caller / event loop before this is called).
///
/// # Errors
/// Returns [`serde_json::Error`] on malformed JSON or unexpected schema.
pub fn deserialize_request(bytes: &[u8]) -> Result<RequestEnvelope, serde_json::Error> {
    serde_json::from_slice(bytes)
}

/// Serialize a [`ResponseEnvelope`] to JSON bytes (no trailing newline).
///
/// # Errors
/// Returns [`serde_json::Error`] if serialization fails.
pub fn serialize_response(r: &ResponseEnvelope) -> Result<Vec<u8>, serde_json::Error> {
    serde_json::to_vec(r)
}

/// Deserialize a [`ResponseEnvelope`] from JSON bytes.
///
/// # Errors
/// Returns [`serde_json::Error`] on malformed JSON or unexpected schema.
pub fn deserialize_response(bytes: &[u8]) -> Result<ResponseEnvelope, serde_json::Error> {
    serde_json::from_slice(bytes)
}
