//! Placeholder data types used across the [`AgentBackend`](crate::AgentBackend)
//! API.
//!
//! # Why placeholders?
//!
//! These types are intentionally minimal so this crate stays dependency-light
//! during the bootstrap phase of the codex-desktop project. Concrete backends
//! (`codex-backend`, `claude-backend`) will be added in later PRs and will
//! adapt to/from `codex-app-server-protocol` types — but the abstraction
//! layer must not pull in that dependency or it would couple every backend
//! (including the Claude Code adapter) to Codex's wire protocol.
//!
//! Each id type is a transparent newtype around [`String`] so that wire
//! formats remain trivially serialisable while still giving the type system
//! enough to prevent accidental cross-id substitution.

use serde::{Deserialize, Serialize};

/// Identifier for a logical conversation/thread.
///
/// Wire-equivalent to a `String`; the newtype prevents mixing thread ids with
/// turn ids or submission ids at the type level.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct ThreadId(pub String);

impl ThreadId {
    /// Returns the inner string slice.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl From<String> for ThreadId {
    fn from(value: String) -> Self {
        Self(value)
    }
}

impl From<&str> for ThreadId {
    fn from(value: &str) -> Self {
        Self(value.to_owned())
    }
}

/// Identifier for a single turn within a thread.
///
/// Cancelling a turn (via [`AgentBackend::interrupt`](crate::AgentBackend::interrupt))
/// requires the [`TurnId`] returned by the server when the turn started, not
/// the [`SubmissionId`] used to dispatch it.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct TurnId(pub String);

impl TurnId {
    /// Returns the inner string slice.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl From<String> for TurnId {
    fn from(value: String) -> Self {
        Self(value)
    }
}

impl From<&str> for TurnId {
    fn from(value: &str) -> Self {
        Self(value.to_owned())
    }
}

/// Client-side correlation id for a [`Submission`].
///
/// The backend echoes this back on any response/notification that pertains to
/// the submission so the UI can match outgoing requests with their results
/// without relying on JSON-RPC `id` fields (which some adapters consume
/// internally).
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct SubmissionId(pub String);

impl SubmissionId {
    /// Returns the inner string slice.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl From<String> for SubmissionId {
    fn from(value: String) -> Self {
        Self(value)
    }
}

impl From<&str> for SubmissionId {
    fn from(value: &str) -> Self {
        Self(value.to_owned())
    }
}

/// Static metadata advertised by the desktop client during initialisation.
///
/// Backends are free to log this, surface it to the server for telemetry, or
/// gate features on minimum client versions.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ClientInfo {
    /// Human-readable client name (e.g. `"codex-desktop"`).
    pub name: String,
    /// Semver-compatible client version string.
    pub version: String,
}

/// Static metadata returned by the server during initialisation.
///
/// Mirrors [`ClientInfo`] in the opposite direction.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ServerInfo {
    /// Human-readable server name.
    pub name: String,
    /// Semver-compatible server version string.
    pub version: String,
}

/// Parameters passed to [`AgentBackend::initialize`](crate::AgentBackend::initialize).
///
/// `protocol_version` and `supported_methods` are optional/empty by default
/// so adapters that do not negotiate either explicitly can leave them blank.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct InitializeParams {
    /// Identifies the desktop client to the backend.
    pub client_info: ClientInfo,
    /// Protocol version the client wants to speak, if any.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub protocol_version: Option<String>,
    /// Methods the client is prepared to handle as inbound requests.
    #[serde(default)]
    pub supported_methods: Vec<String>,
}

/// Response returned from [`AgentBackend::initialize`](crate::AgentBackend::initialize).
///
/// The `supported_methods` and `supported_notifications` lists are typically
/// copied verbatim into [`BackendCapabilities`](crate::BackendCapabilities)
/// for cheap runtime checks.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct InitializeResponse {
    /// Identifies the agent runtime to the desktop client.
    pub server_info: ServerInfo,
    /// Protocol version the server agreed to speak, if any.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub protocol_version: Option<String>,
    /// Request methods the server understands.
    #[serde(default)]
    pub supported_methods: Vec<String>,
    /// Notification methods the server may emit.
    #[serde(default)]
    pub supported_notifications: Vec<String>,
}

/// A unit of work submitted to the backend.
///
/// The [`payload`](Submission::payload) is opaque so backends can carry their
/// own typed submissions. A future Codex-specific layer can deserialise the
/// payload via [`AgentBackendExtras`](crate::AgentBackendExtras) downcasts or
/// internal type registries; this abstraction merely guarantees the plumbing.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Submission {
    /// Client-assigned correlation id (see [`SubmissionId`]).
    pub id: SubmissionId,
    /// The thread the submission belongs to.
    pub thread_id: ThreadId,
    /// Backend-specific payload. Usually a serialised request envelope.
    pub payload: serde_json::Value,
}
