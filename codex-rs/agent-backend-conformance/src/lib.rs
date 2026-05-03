#![forbid(unsafe_code)]

//! Conformance harness for [`codex_agent_backend::AgentBackend`] implementations.
//!
//! See `docs/desktop-architecture.md` §10 ("The 10 conformance scenarios").
//! Each scenario is a TOML fixture under `scenarios/` describing input
//! prompts, expected `ServerNotification` sequences, and approval round-
//! trips. Both `CodexBackend` and `ClaudeBackend` must pass every scenario
//! before they are advertised in the model picker.

pub mod fixture;
pub mod runner;

pub use fixture::{ConformanceFixture, ExpectedEvent, Match, RespondWith, ScenarioInput};
pub use runner::{run_scenario, ScenarioOutcome, ScenarioReport};
