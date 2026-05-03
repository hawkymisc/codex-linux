# codex-agent-backend-conformance

Conformance harness for `codex_agent_backend::AgentBackend` implementations.
Both `CodexBackend` and `ClaudeBackend` must pass every scenario in
`scenarios/` before they can be advertised in the desktop model picker
(see `docs/desktop-architecture.md` §10).

## Scenario file format

Each scenario is a TOML document parsed into `ConformanceFixture`:

```toml
schema_version = 1                 # bumped only on breaking format changes
description = "what this exercises"
tags = ["approval", "exec"]        # optional, free-form
strict = false                     # extra unknown notifications fail iff true
timeout_ms = 30000                 # hard deadline

[input]
prompt = "user message"
network_disabled = false           # optional offline policy
attachments = []                   # optional list of relative paths

[[expected_events]]
method = "thread/started"          # observed in order
match = { params = { ... } }       # optional structural match (PR-C)
respond_with = { decision = "approved" }  # optional approval reply (PR-C)
```

## Adding a new scenario

1. Create `scenarios/NN_short_name.toml` using the next free number.
2. Document the user-visible behaviour the scenario gates in `description`.
3. List the expected `ServerNotification` methods in observation order.
4. Wire the fixture into your backend's integration test by calling
   `runner::run_scenario(&backend, &fixture).await`.
