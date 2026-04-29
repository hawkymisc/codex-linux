//! Common error type returned by [`AgentBackend`](crate::AgentBackend) methods.
//!
//! Errors are deliberately string-typed for transport/protocol/rejection
//! variants because each backend's underlying error hierarchy is different
//! (JSON-RPC error codes for Codex, structured Anthropic SDK errors for Claude
//! Code). Coercing them into a small set of variants gives the desktop UI a
//! stable surface to reason about without leaking adapter internals.

use thiserror::Error;

/// Error returned by every fallible [`AgentBackend`](crate::AgentBackend)
/// operation.
///
/// The variants are ordered roughly by abstraction level, from lifecycle
/// errors at the top to plumbing errors at the bottom. When mapping a
/// backend-specific error into [`BackendError`], prefer the highest-level
/// variant that still carries the failure's intent — for example, a JSON-RPC
/// `INVALID_PARAMS` reply belongs in [`BackendError::Rejected`], not
/// [`BackendError::Protocol`].
#[derive(Debug, Error)]
pub enum BackendError {
    /// A method was called before
    /// [`AgentBackend::initialize`](crate::AgentBackend::initialize) ran (or
    /// before the initialise response arrived).
    #[error("backend not initialised")]
    NotInitialised,

    /// The backend has already been shut down or its transport has closed
    /// out from under us.
    #[error("backend has shut down")]
    Closed,

    /// The underlying transport (stdio pipe, websocket, etc.) failed in a
    /// non-recoverable way.
    #[error("transport error: {0}")]
    Transport(String),

    /// The wire payload was syntactically valid but semantically inconsistent
    /// with the negotiated protocol — for example, a notification that lacks
    /// a required field.
    #[error("protocol error: {0}")]
    Protocol(String),

    /// The backend explicitly rejected a request (e.g. JSON-RPC error reply,
    /// SDK validation failure).
    #[error("backend rejected request: {0}")]
    Rejected(String),

    /// Bubble-through for [`std::io::Error`] from transports that surface raw
    /// I/O failures.
    #[error("io: {0}")]
    Io(#[from] std::io::Error),

    /// Bubble-through for [`serde_json::Error`] from (de)serialising wire
    /// payloads.
    #[error("serde: {0}")]
    Serde(#[from] serde_json::Error),
}
