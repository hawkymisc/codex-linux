//! Forward-compatible server-to-client notification envelope.
//!
//! # The forward-compatibility problem
//!
//! Server runtimes (Codex's app-server, the Anthropic Agent SDK transport)
//! evolve independently of the desktop client. New notification methods can
//! land at any point, and a desktop build that crashes on every unknown
//! method is unacceptable — users would be unable to interact with the agent
//! until a client release ships.
//!
//! # Why not `#[serde(other)]`?
//!
//! Serde's `other` attribute only works on plain (unit-variant) enums, not on
//! internally tagged enums where the discriminator is a string and the payload
//! is structured. The classic workaround — an `untagged` enum with a typed
//! variant followed by a fallback variant — is fragile because both variants
//! share the same JSON shape (`{ "method": "...", "params": ... }`); serde
//! deserialises the first that succeeds, which makes the unknown branch
//! essentially unreachable.
//!
//! # The chosen approach
//!
//! Rather than relying on serde's choice between two structurally identical
//! variants, this module exposes a small explicit classifier:
//!
//! ```ignore
//! let registry = KnownVariantRegistry::new().with_methods(["thread/started"]);
//! let n = IncomingServerNotification::classify("thread/started", params, &registry);
//! ```
//!
//! `classify` consults a [`KnownVariantRegistry`] to decide whether the
//! method falls in the recognised set. Backends construct the registry once
//! during [`AgentBackend::initialize`](crate::AgentBackend::initialize), then
//! drive every inbound notification through `classify`. The end result is a
//! deterministic split between [`IncomingServerNotification::Known`] and
//! [`IncomingServerNotification::Unknown`] without forcing serde to make the
//! decision.
//!
//! Round-tripping is preserved on both branches: serialising a `Known`
//! notification produces the original `{ "method": ..., "params": ... }`
//! shape, as does serialising an `Unknown` one. A future PR will replace
//! [`KnownNotification::recognised_as`] with a typed re-export of
//! `codex-app-server-protocol::ServerNotification` behind a `serde-strict`
//! feature flag.

use serde::{Deserialize, Serialize};
use std::collections::BTreeSet;

/// Server-to-client notification envelope that survives round-trips even when
/// the schema does not recognise the inner variant.
///
/// Deserialisation falls through the variants in declaration order: a
/// `Known` payload is tried first, and `Unknown` acts as the fallback so any
/// `{ "method": ..., "params": ... }` shape always parses. Production code
/// should prefer the explicit [`IncomingServerNotification::classify`]
/// helper, which uses an out-of-band [`KnownVariantRegistry`] to choose the
/// branch deterministically — the `untagged` derive is retained so the
/// [`Deserialize`]/[`Serialize`] round-trip still works on values produced
/// elsewhere (e.g. from logged JSON dumps).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(untagged)]
pub enum IncomingServerNotification {
    /// A notification whose `method` is known to the local registry.
    ///
    /// The `params` payload is intentionally a [`serde_json::Value`] for now
    /// so this crate stays dependency-light. A future PR will replace this
    /// with a typed re-export of
    /// `codex-app-server-protocol::ServerNotification` behind a feature flag.
    Known(KnownNotification),
    /// A notification whose `method` is not recognised; preserved verbatim
    /// so the desktop UI can surface it in the off-by-default Protocol Drift
    /// diagnostic pane rather than silently dropping it.
    Unknown(UnknownNotification),
}

/// Notification payload that the local registry recognises as a typed variant.
///
/// `recognised_as` is set by [`IncomingServerNotification::classify`] to the
/// canonical `method` string from the registry. It serves as a non-empty
/// marker that survives serialisation; downstream consumers can use it to
/// dispatch into typed variant handlers without re-running the lookup.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct KnownNotification {
    /// Wire `method` string as received from the server.
    pub method: String,
    /// Wire `params` payload, defaulting to [`serde_json::Value::Null`] when
    /// the server omits the field entirely.
    #[serde(default)]
    pub params: serde_json::Value,
    /// Non-empty marker filled in by the strict-mode classifier when it
    /// recognises a variant. The unknown fallback path leaves this `None`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub recognised_as: Option<String>,
}

/// Notification payload whose `method` is not in the local registry.
///
/// The desktop UI surfaces these in a diagnostic pane to make protocol drift
/// visible without breaking the user-facing flow.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UnknownNotification {
    /// Wire `method` string as received from the server.
    pub method: String,
    /// Wire `params` payload, defaulting to [`serde_json::Value::Null`] when
    /// the server omits the field entirely.
    #[serde(default)]
    pub params: serde_json::Value,
}

impl IncomingServerNotification {
    /// Returns the wire `method` string regardless of which branch this is.
    pub fn method(&self) -> &str {
        match self {
            Self::Known(k) => &k.method,
            Self::Unknown(u) => &u.method,
        }
    }

    /// Returns `true` if this is the unknown-fallback variant.
    pub fn is_unknown(&self) -> bool {
        matches!(self, Self::Unknown(_))
    }

    /// Borrows the `params` payload regardless of which branch this is.
    pub fn params(&self) -> &serde_json::Value {
        match self {
            Self::Known(k) => &k.params,
            Self::Unknown(u) => &u.params,
        }
    }

    /// Classifies a wire-level `(method, params)` pair against the local
    /// registry of recognised methods.
    ///
    /// This is the recommended entry point for adapters: it produces a
    /// deterministic split between [`IncomingServerNotification::Known`] and
    /// [`IncomingServerNotification::Unknown`] independent of serde's
    /// behaviour on structurally identical untagged variants.
    ///
    /// `params` may be [`serde_json::Value::Null`] if the wire payload had
    /// no params field; downstream consumers should treat that as an empty
    /// object.
    pub fn classify(
        method: &str,
        params: serde_json::Value,
        registry: &KnownVariantRegistry,
    ) -> Self {
        if let Some(canonical) = registry.canonical(method) {
            Self::Known(KnownNotification {
                method: method.to_owned(),
                params,
                recognised_as: Some(canonical.to_owned()),
            })
        } else {
            Self::Unknown(UnknownNotification {
                method: method.to_owned(),
                params,
            })
        }
    }
}

/// Set of notification method strings that the local strict-mode parser
/// recognises.
///
/// The registry is intentionally just a sorted set of `&'static str`
/// pointers; backends typically build it once during initialisation by
/// listing the methods they statically know how to decode. Adding a method
/// here is the gate to surfacing it as a typed
/// [`IncomingServerNotification::Known`] variant; everything else falls
/// through to the [`IncomingServerNotification::Unknown`] preservation path.
#[derive(Debug, Default, Clone)]
pub struct KnownVariantRegistry {
    methods: BTreeSet<&'static str>,
}

impl KnownVariantRegistry {
    /// Constructs an empty registry; no methods are recognised.
    pub fn new() -> Self {
        Self {
            methods: BTreeSet::new(),
        }
    }

    /// Adds `method` to the recognised set, returning `self` so calls can be
    /// chained.
    pub fn with_method(mut self, method: &'static str) -> Self {
        self.methods.insert(method);
        self
    }

    /// Adds every method in `methods` to the recognised set, returning `self`
    /// so calls can be chained.
    pub fn with_methods<I>(mut self, methods: I) -> Self
    where
        I: IntoIterator<Item = &'static str>,
    {
        self.methods.extend(methods);
        self
    }

    /// Returns `true` if `method` is in the recognised set.
    pub fn contains(&self, method: &str) -> bool {
        self.methods.contains(method)
    }

    /// Returns the canonical (registry-stored) representation of `method` if
    /// the registry contains it.
    ///
    /// The canonical form is always equal to `method` byte-for-byte; the
    /// helper exists so callers can stash a `&'static str` reference in the
    /// recognised-as field without re-allocating.
    pub fn canonical(&self, method: &str) -> Option<&'static str> {
        self.methods.get(method).copied()
    }

    /// Returns the number of methods registered.
    pub fn len(&self) -> usize {
        self.methods.len()
    }

    /// Returns `true` when no methods are registered.
    pub fn is_empty(&self) -> bool {
        self.methods.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;
    use serde_json::json;

    fn registry() -> KnownVariantRegistry {
        KnownVariantRegistry::new().with_methods(["thread/started", "agent/messageDelta"])
    }

    #[test]
    fn classify_known_method_marks_recognised_as() {
        let r = registry();
        let n = IncomingServerNotification::classify(
            "thread/started",
            json!({"thread_id": "abc"}),
            &r,
        );
        match n {
            IncomingServerNotification::Known(k) => {
                assert_eq!(k.method, "thread/started");
                assert_eq!(k.recognised_as.as_deref(), Some("thread/started"));
                assert_eq!(k.params, json!({"thread_id": "abc"}));
            }
            IncomingServerNotification::Unknown(_) => panic!("expected Known"),
        }
    }

    #[test]
    fn classify_unknown_method_falls_back() {
        let r = registry();
        let n = IncomingServerNotification::classify(
            "future/methodWeDoNotKnow",
            json!({"x": 1}),
            &r,
        );
        assert!(n.is_unknown());
        assert_eq!(n.method(), "future/methodWeDoNotKnow");
        assert_eq!(n.params(), &json!({"x": 1}));
    }

    #[test]
    fn unknown_round_trips_through_serde() {
        let original = IncomingServerNotification::Unknown(UnknownNotification {
            method: "future/method".into(),
            params: json!({"a": [1, 2, 3]}),
        });
        let s = serde_json::to_string(&original).expect("serialise");
        let back: IncomingServerNotification =
            serde_json::from_str(&s).expect("deserialise");
        assert_eq!(back.method(), "future/method");
        assert_eq!(back.params(), &json!({"a": [1, 2, 3]}));
    }

    #[test]
    fn known_round_trips_through_serde() {
        let original = IncomingServerNotification::Known(KnownNotification {
            method: "thread/started".into(),
            params: json!({"thread_id": "t-1"}),
            recognised_as: Some("thread/started".into()),
        });
        let s = serde_json::to_string(&original).expect("serialise");
        let back: IncomingServerNotification =
            serde_json::from_str(&s).expect("deserialise");
        assert_eq!(back.method(), "thread/started");
        assert_eq!(back.params(), &json!({"thread_id": "t-1"}));
    }

    #[test]
    fn empty_params_default_to_null() {
        let r = registry();
        let n = IncomingServerNotification::classify(
            "thread/started",
            serde_json::Value::Null,
            &r,
        );
        assert_eq!(n.params(), &serde_json::Value::Null);

        // Wire payload without a params field at all also deserialises with
        // params == Null thanks to #[serde(default)].
        let wire = json!({"method": "future/sansParams"});
        let parsed: IncomingServerNotification =
            serde_json::from_value(wire).expect("deserialise");
        assert_eq!(parsed.method(), "future/sansParams");
        assert_eq!(parsed.params(), &serde_json::Value::Null);
    }

    #[test]
    fn registry_helpers() {
        let r = KnownVariantRegistry::new();
        assert!(r.is_empty());
        assert_eq!(r.len(), 0);

        let r = r.with_method("a").with_methods(["b", "c"]);
        assert_eq!(r.len(), 3);
        assert!(r.contains("a"));
        assert!(r.contains("b"));
        assert!(r.contains("c"));
        assert!(!r.contains("d"));
        assert_eq!(r.canonical("a"), Some("a"));
        assert_eq!(r.canonical("z"), None);
    }
}
