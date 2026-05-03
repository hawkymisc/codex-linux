#![forbid(unsafe_code)]

//! Backend-agnostic agent abstraction for codex-desktop.
//!
//! Codex (via app-server JSON-RPC) and Claude Code (via Anthropic Agent SDK)
//! both implement [`AgentBackend`]; the desktop UI never sees backend-specific
//! types. Unknown server notifications survive round-trips via
//! [`IncomingServerNotification::Unknown`] rather than being silently dropped.
//!
//! # Design rationale
//!
//! * **Trait object friendliness.** [`AgentBackend`] is `Send + Sync + 'static`
//!   and the methods take `&self` / `&mut self` (only [`shutdown`](AgentBackend::shutdown)
//!   takes `Box<Self>`) so the desktop UI can hold an `Arc<dyn AgentBackend>`
//!   in shared state without leaking implementation details.
//! * **Forward compatibility.** Notification deserialisation is intentionally
//!   tolerant — see [`envelope`]. Backends evolve independently and the UI
//!   should not crash when a future server emits a method we have not yet
//!   coded variants for.
//! * **Capability negotiation.** [`BackendCapabilities`] is the only contract
//!   the UI may rely on at runtime to gate features. Adapters fill it in
//!   during [`initialize`](AgentBackend::initialize).
//! * **Dependency-light.** This crate deliberately does not depend on
//!   `codex-app-server-protocol`; concrete backends will adapt to/from
//!   protocol types in their own crates (`codex-backend`, `claude-backend`).
//!   That keeps this trait reusable by non-Codex implementations such as the
//!   Claude Code adapter.

pub mod capabilities;
pub mod codex;
pub mod envelope;
pub mod error;
pub mod process;
pub mod registry;
pub mod types;

pub use capabilities::BackendCapabilities;
pub use codex::CodexBackend;
pub use envelope::{IncomingServerNotification, KnownNotification, KnownVariantRegistry, UnknownNotification};
pub use error::BackendError;
pub use process::{ProcessBackend, ProcessBackendConfig};
pub use registry::{BackendDescriptor, BackendId, registered_backends};
pub use types::{
    ClientInfo, InitializeParams, InitializeResponse, ServerInfo, Submission, SubmissionId,
    ThreadId, TurnId,
};

use async_trait::async_trait;
use futures::stream::BoxStream;
use std::any::Any;

/// The backend-agnostic agent contract.
///
/// Implementations bridge the desktop UI to a concrete agent runtime
/// (Codex via `codex-app-server`'s JSON-RPC dialect, or Claude Code via the
/// Anthropic Agent SDK). Each method maps to a high-level operation the UI
/// performs; transport, framing, retries and credential management are all
/// adapter concerns and must not leak through this trait.
///
/// # Lifecycle
///
/// 1. The UI constructs a backend through a [`registry::BackendDescriptor`]
///    factory.
/// 2. It calls [`initialize`](AgentBackend::initialize) once with client
///    metadata; the response advertises capability bits which are mirrored on
///    [`capabilities`](AgentBackend::capabilities) for cheap subsequent reads.
/// 3. It subscribes to notifications via [`events`](AgentBackend::events) and
///    feeds new turns in via [`submit`](AgentBackend::submit).
/// 4. [`interrupt`](AgentBackend::interrupt) cancels an in-flight turn.
/// 5. [`shutdown`](AgentBackend::shutdown) consumes the backend, draining the
///    notification stream and releasing any transport resources.
///
/// # Threading
///
/// Implementations must be `Send + Sync + 'static`; the UI typically wraps
/// them in `Arc` and calls non-mutating methods from many tasks. The single
/// `&mut self` initialisation call happens before any clones are taken.
#[async_trait]
pub trait AgentBackend: Send + Sync + 'static {
    /// Performs a one-shot handshake with the underlying agent runtime.
    ///
    /// Adapters use this to exchange protocol versions and capability lists
    /// with the server. Calling [`initialize`](AgentBackend::initialize) more
    /// than once is implementation-defined; most adapters should treat the
    /// second call as an error.
    async fn initialize(
        &mut self,
        p: InitializeParams,
    ) -> Result<InitializeResponse, BackendError>;

    /// Submits a turn (or other backend-specific submission) for processing.
    ///
    /// The [`Submission::payload`] is intentionally opaque so each backend
    /// can transport its own typed envelope. Adapters validate the payload
    /// shape; this trait only guarantees plumbing.
    async fn submit(&self, sub: Submission) -> Result<(), BackendError>;

    /// Cancels the in-flight turn identified by `turn_id`.
    ///
    /// Cancellation is best-effort: the backend may still deliver some
    /// trailing notifications for the turn after this returns.
    async fn interrupt(&self, turn_id: TurnId) -> Result<(), BackendError>;

    /// Tears down the backend.
    ///
    /// Consumes the boxed receiver so the type system enforces single-call
    /// shutdown. After this returns the [`events`](AgentBackend::events)
    /// stream is expected to terminate.
    async fn shutdown(self: Box<Self>) -> Result<(), BackendError>;

    /// Returns a fresh subscription to server-initiated notifications.
    ///
    /// The stream yields [`IncomingServerNotification`] values which are
    /// either typed (`Known`) or preserved opaquely (`Unknown`). The UI
    /// should treat `Unknown` as a soft signal — surface it in a diagnostic
    /// pane, never crash on it.
    fn events(&self) -> BoxStream<'static, IncomingServerNotification>;

    /// Reports the negotiated capabilities.
    ///
    /// This is filled in by [`initialize`](AgentBackend::initialize); calling
    /// it before initialisation should return a conservative default value
    /// rather than panicking.
    fn capabilities(&self) -> &BackendCapabilities;

    /// Optional escape hatch for backend-specific extensions.
    ///
    /// The desktop UI must not rely on the existence of any particular
    /// extension; this is intended for power-user features that ride along
    /// with a specific adapter (e.g. Codex turn-context management). Returns
    /// [`None`] by default.
    fn extras(&self) -> Option<&dyn AgentBackendExtras> {
        None
    }
}

/// Marker trait for backend-specific extension objects exposed via
/// [`AgentBackend::extras`].
///
/// Consumers downcast through [`Any::downcast_ref`] to access concrete
/// implementations. Keeping the surface here as an [`Any`] supertrait avoids
/// pulling backend-specific types into the abstraction.
pub trait AgentBackendExtras: Any + Send + Sync {
    /// Returns `self` as an [`Any`] so callers can downcast to a concrete
    /// extras type.
    fn as_any(&self) -> &dyn Any;
}
