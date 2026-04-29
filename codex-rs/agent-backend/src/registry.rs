//! Static-only backend registration scaffolding.
//!
//! Concrete backend crates (`codex-backend`, `claude-backend`) register
//! themselves at link time via [`inventory::submit!`]. The desktop binary
//! then iterates [`registered_backends`] to populate its picker UI without
//! a hard dependency on every backend crate.
//!
//! # Why `inventory`?
//!
//! The desktop binary lives downstream of the backend crates: it already
//! pulls them in as dependencies, so dynamic loading would be overkill, but
//! we still want each backend's module to be self-registering so the desktop
//! does not need a hand-maintained `match` over backend ids. `inventory`'s
//! link-time collection gives us exactly that — every backend crate emits a
//! [`BackendDescriptor`] and the abstraction crate exposes the iterator.
//!
//! # Why a `fn` factory and not a closure?
//!
//! `inventory::submit!` wants a `'static` value. A bare `fn` pointer is the
//! simplest way to satisfy that without leaking allocation or a `Lazy`
//! initialiser. Backends that need configuration should read it inside the
//! factory body (e.g. from environment, the config file, or a global
//! state).

use crate::AgentBackend;
use crate::BackendError;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

/// Stable identifier for a registered backend.
///
/// The wrapped string slice is `'static` so it can be embedded in
/// [`inventory`] submissions without allocation. Equality and hashing are
/// borrowed from the inner pointer's bytes (via `&str`), so two
/// [`BackendId`]s referencing the same literal compare equal.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct BackendId(pub &'static str);

impl BackendId {
    /// Returns the inner identifier as a string slice.
    pub fn as_str(&self) -> &'static str {
        self.0
    }
}

impl std::fmt::Display for BackendId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.0)
    }
}

/// Boxed-future return type for [`BackendDescriptor::factory`].
///
/// Spelled out as a type alias so the `inventory::submit!` site stays
/// readable. The trailing `Send` bound matters: the desktop runtime spawns
/// the factory future on a multi-threaded executor.
pub type BackendFactoryFuture =
    Pin<Box<dyn Future<Output = Result<Arc<dyn AgentBackend>, BackendError>> + Send>>;

/// Static metadata about a registered backend.
///
/// Backend crates emit one of these via [`inventory::submit!`] in their
/// crate roots. The desktop binary reads them at startup to populate its
/// backend picker UI.
pub struct BackendDescriptor {
    /// Stable id surfaced to the UI and persisted in user settings.
    pub id: BackendId,
    /// Human-readable label shown in the backend picker.
    pub display_name: &'static str,
    /// Asynchronous factory that constructs an [`AgentBackend`] handle.
    ///
    /// The factory is responsible for any heavy initialisation (process
    /// spawning, transport handshakes, credential loading). Errors are
    /// surfaced to the UI so the user can pick a different backend.
    pub factory: fn() -> BackendFactoryFuture,
}

inventory::collect!(BackendDescriptor);

/// Iterates every backend registered at link time via [`inventory::submit!`].
///
/// The order of iteration is implementation-defined by `inventory` and may
/// change between builds; UIs that need a stable order should sort by
/// [`BackendDescriptor::display_name`] or [`BackendDescriptor::id`].
pub fn registered_backends() -> impl Iterator<Item = &'static BackendDescriptor> {
    inventory::iter::<BackendDescriptor>().into_iter()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::AgentBackend;
    use crate::AgentBackendExtras;
    use crate::BackendCapabilities;
    use crate::IncomingServerNotification;
    use crate::InitializeParams;
    use crate::InitializeResponse;
    use crate::Submission;
    use crate::TurnId;
    use crate::types::ServerInfo;
    use async_trait::async_trait;
    use futures::stream::BoxStream;
    use futures::stream::StreamExt;

    /// A no-op backend used solely to satisfy the factory signature in the
    /// inventory test below. It panics on every operation so any accidental
    /// use surfaces immediately.
    struct DummyBackend {
        caps: BackendCapabilities,
    }

    #[async_trait]
    impl AgentBackend for DummyBackend {
        async fn initialize(
            &mut self,
            _p: InitializeParams,
        ) -> Result<InitializeResponse, BackendError> {
            Ok(InitializeResponse {
                server_info: ServerInfo {
                    name: "dummy".into(),
                    version: "0.0.0".into(),
                },
                protocol_version: None,
                supported_methods: Vec::new(),
                supported_notifications: Vec::new(),
            })
        }

        async fn submit(&self, _sub: Submission) -> Result<(), BackendError> {
            Err(BackendError::Closed)
        }

        async fn interrupt(&self, _turn_id: TurnId) -> Result<(), BackendError> {
            Err(BackendError::Closed)
        }

        async fn shutdown(self: Box<Self>) -> Result<(), BackendError> {
            Ok(())
        }

        fn events(&self) -> BoxStream<'static, IncomingServerNotification> {
            futures::stream::empty().boxed()
        }

        fn capabilities(&self) -> &BackendCapabilities {
            &self.caps
        }

        fn extras(&self) -> Option<&dyn AgentBackendExtras> {
            None
        }
    }

    fn dummy_factory() -> BackendFactoryFuture {
        Box::pin(async {
            let b: Arc<dyn AgentBackend> = Arc::new(DummyBackend {
                caps: BackendCapabilities::default(),
            });
            Ok(b)
        })
    }

    inventory::submit! {
        BackendDescriptor {
            id: BackendId("test-dummy"),
            display_name: "Test Dummy Backend",
            factory: dummy_factory,
        }
    }

    #[test]
    fn registered_backends_includes_test_submission() {
        let count = registered_backends().count();
        assert!(count >= 1, "expected at least the test-dummy submission");
        let found = registered_backends().any(|d| d.id == BackendId("test-dummy"));
        assert!(found, "test-dummy backend should be registered");
    }

    #[tokio::test]
    async fn dummy_factory_constructs_backend() {
        let descriptor = registered_backends()
            .find(|d| d.id == BackendId("test-dummy"))
            .expect("test-dummy descriptor present");
        let backend = (descriptor.factory)().await.expect("factory ok");
        assert!(backend.capabilities().supported_methods.is_empty());
    }
}
