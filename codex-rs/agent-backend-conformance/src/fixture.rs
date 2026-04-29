//! TOML fixture format for conformance scenarios.
//!
//! Every optional field uses `serde(default)` so fixtures can be evolved
//! additively without breaking older ones.

use serde::{Deserialize, Serialize};

/// Top-level scenario document.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct ConformanceFixture {
    /// Bumped only on breaking format changes. Currently `1`.
    #[serde(default = "default_schema_version")]
    pub schema_version: u32,
    pub description: String,
    pub input: ScenarioInput,
    #[serde(default)]
    pub expected_events: Vec<ExpectedEvent>,
    /// Permissive mode: extra unknown notifications are tolerated rather
    /// than failing the scenario.
    #[serde(default)]
    pub strict: bool,
    /// Hard timeout in milliseconds.
    #[serde(default = "default_timeout")]
    pub timeout_ms: u64,
    #[serde(default)]
    pub tags: Vec<String>,
}

fn default_schema_version() -> u32 { 1 }
fn default_timeout() -> u64 { 30_000 }

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct ScenarioInput {
    pub prompt: String,
    #[serde(default)]
    pub network_disabled: bool,
    #[serde(default)]
    pub attachments: Vec<String>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct ExpectedEvent {
    pub method: String,
    #[serde(default, rename = "match")]
    pub match_: Option<Match>,
    #[serde(default)]
    pub respond_with: Option<RespondWith>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct Match {
    #[serde(default)]
    pub params: Option<serde_json::Value>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct RespondWith {
    pub decision: String,
}

impl ConformanceFixture {
    pub fn from_toml(input: &str) -> Result<Self, FixtureError> {
        toml::from_str(input).map_err(FixtureError::Parse)
    }
}

#[derive(Debug, thiserror::Error)]
pub enum FixtureError {
    #[error("toml parse error: {0}")]
    Parse(toml::de::Error),
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;

    #[test]
    fn parses_minimal_fixture() {
        let toml = r#"
description = "x"
[input]
prompt = "hello"
"#;
        let f = ConformanceFixture::from_toml(toml).expect("parse");
        assert_eq!(f.description, "x");
        assert_eq!(f.input.prompt, "hello");
        assert_eq!(f.schema_version, 1);
        assert_eq!(f.timeout_ms, 30_000);
        assert!(!f.strict);
    }

    #[test]
    fn parses_full_fixture() {
        let toml = r#"
schema_version = 1
description = "complex"
strict = true
timeout_ms = 5000
tags = ["approval", "exec"]

[input]
prompt = "delete foo"
network_disabled = true

[[expected_events]]
method = "thread/started"

[[expected_events]]
method = "exec/request_approval"
match = { params = { command = "rm" } }
respond_with = { decision = "approved" }

[[expected_events]]
method = "turn/completed"
"#;
        let f = ConformanceFixture::from_toml(toml).expect("parse");
        assert_eq!(f.expected_events.len(), 3);
        assert!(f.strict);
        assert_eq!(f.timeout_ms, 5000);
    }
}
