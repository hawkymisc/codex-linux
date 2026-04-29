//! Capability negotiation primitives.
//!
//! [`BackendCapabilities`] is the only contract the desktop UI is allowed to
//! consult at runtime to decide whether to expose a feature. Adapters fill it
//! in during [`AgentBackend::initialize`](crate::AgentBackend::initialize) by
//! copying the relevant fields out of the server's initialise response.
//!
//! The set-intersection helper exists so the UI can compute the intersection
//! of "what the client supports" and "what the server supports" without
//! duplicating logic at every call site.

use serde::{Deserialize, Serialize};

/// Capability bits negotiated between the client and the backend.
///
/// All fields default to empty / `None` so a default-constructed value
/// represents "no capabilities yet" rather than "all capabilities".
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct BackendCapabilities {
    /// Negotiated protocol version, if the wire format identifies one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub protocol_version: Option<String>,
    /// Request methods the backend will accept.
    #[serde(default)]
    pub supported_methods: Vec<String>,
    /// Notification methods the backend may emit.
    #[serde(default)]
    pub supported_notifications: Vec<String>,
}

impl BackendCapabilities {
    /// Returns `true` if the backend advertises support for `m` as a request
    /// method.
    pub fn supports_method(&self, m: &str) -> bool {
        self.supported_methods.iter().any(|x| x == m)
    }

    /// Returns `true` if the backend advertises that it may emit
    /// notifications named `n`.
    pub fn supports_notification(&self, n: &str) -> bool {
        self.supported_notifications.iter().any(|x| x == n)
    }

    /// Returns the set intersection of `self` and `other` over both method
    /// lists.
    ///
    /// `protocol_version` from `self` is preserved verbatim — the caller is
    /// expected to have already chosen a single negotiated value before
    /// reaching this helper. Order from `self` is preserved.
    pub fn intersect(&self, other: &Self) -> Self {
        let supported_methods = self
            .supported_methods
            .iter()
            .filter(|m| other.supports_method(m))
            .cloned()
            .collect();
        let supported_notifications = self
            .supported_notifications
            .iter()
            .filter(|n| other.supports_notification(n))
            .cloned()
            .collect();
        Self {
            protocol_version: self.protocol_version.clone(),
            supported_methods,
            supported_notifications,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;

    fn caps(methods: &[&str], notifs: &[&str]) -> BackendCapabilities {
        BackendCapabilities {
            protocol_version: Some("1".into()),
            supported_methods: methods.iter().map(|s| (*s).to_owned()).collect(),
            supported_notifications: notifs.iter().map(|s| (*s).to_owned()).collect(),
        }
    }

    #[test]
    fn intersect_returns_overlap() {
        let a = caps(&["a", "b", "c"], &["x", "y"]);
        let b = caps(&["b", "c", "d"], &["y", "z"]);
        let got = a.intersect(&b);
        assert_eq!(got.protocol_version.as_deref(), Some("1"));
        assert_eq!(got.supported_methods, vec!["b".to_owned(), "c".to_owned()]);
        assert_eq!(got.supported_notifications, vec!["y".to_owned()]);
    }

    #[test]
    fn intersect_empty_when_disjoint() {
        let a = caps(&["a"], &["x"]);
        let b = caps(&["b"], &["y"]);
        let got = a.intersect(&b);
        assert!(got.supported_methods.is_empty());
        assert!(got.supported_notifications.is_empty());
    }

    #[test]
    fn supports_method_honours_list() {
        let c = caps(&["alpha", "beta"], &[]);
        assert!(c.supports_method("alpha"));
        assert!(c.supports_method("beta"));
        assert!(!c.supports_method("gamma"));
    }

    #[test]
    fn supports_notification_honours_list() {
        let c = caps(&[], &["thread/started", "agent/messageDelta"]);
        assert!(c.supports_notification("thread/started"));
        assert!(!c.supports_notification("thread/finished"));
    }

    #[test]
    fn default_is_empty() {
        let c = BackendCapabilities::default();
        assert!(c.protocol_version.is_none());
        assert!(c.supported_methods.is_empty());
        assert!(c.supported_notifications.is_empty());
    }
}
