//! `codex-lspd` role — LSP/lint supervisor.
//!
//! Runs as a child of `codex-desktop` (or any other JSON-RPC client),
//! speaks NDJSON over stdin/stdout, and per workspace spawns one or more
//! language servers as grandchild processes (rust-analyzer, pyright,
//! typescript-language-server, gopls). See
//! `docs/desktop-architecture.md` §3.1 for the architecture.
//!
//! ## Wire format (parent ↔ codex-lspd)
//!
//! Parent → codex-lspd: NDJSON JSON-RPC 2.0 (one message per line).
//! Methods: `initialize`, `lsp/start`, `lsp/textDocumentDidOpen`,
//! `lsp/textDocumentDidChange`, `lsp/textDocumentDidClose`, `shutdown`.
//!
//! ## Wire format (codex-lspd ↔ language server)
//!
//! Real LSP framing: `Content-Length: <N>\r\n\r\n<body>` per message.
//! Implemented by [`LspFrameReader`] / [`LspFrameWriter`] below.

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::collections::HashMap;
use std::process::Stdio;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;
use tokio::io::{
    AsyncBufReadExt as _, AsyncRead, AsyncReadExt as _, AsyncWrite, AsyncWriteExt as _, BufReader,
};
use tokio::process::Command;
use tokio::sync::Mutex;
use tracing::{debug, info, warn};

/// How long to wait for the language server's `initialize` reply before
/// giving up. Real servers (rust-analyzer warm) respond well under a second;
/// 30s is a generous upper bound that covers cold-cache CI runs.
const INIT_TIMEOUT: Duration = Duration::from_secs(30);

const SUPPORTED_NOTIFICATIONS: &[&str] = &["textDocument/publishDiagnostics"];
const SUPPORTED_METHODS: &[&str] = &[
    "initialize",
    "lsp/start",
    "lsp/textDocumentDidOpen",
    "lsp/textDocumentDidChange",
    "lsp/textDocumentDidClose",
    "shutdown",
];

/// Run the codex-lspd role on stdin/stdout.
pub async fn run() -> Result<()> {
    info!("codex-lspd: starting NDJSON JSON-RPC server on stdio");

    let stdin = tokio::io::stdin();
    let mut stdout = tokio::io::stdout();
    let mut reader = BufReader::new(stdin).lines();

    let supervisor = Arc::new(Supervisor::new());
    let mut line_buf = String::new();

    while let Some(line) = reader.next_line().await? {
        if line.trim().is_empty() {
            continue;
        }
        line_buf.clear();
        line_buf.push_str(&line);
        let response = match dispatch_line(&supervisor, &line_buf).await {
            Ok(resp) => resp,
            Err(err) => {
                warn!(error = %err, "lspd: dispatch error");
                json!({
                    "jsonrpc": "2.0",
                    "id": Value::Null,
                    "error": { "code": -32603, "message": err.to_string() },
                })
            }
        };
        let mut serialized = serde_json::to_string(&response)?;
        serialized.push('\n');
        stdout.write_all(serialized.as_bytes()).await?;
        stdout.flush().await?;

        if response
            .get("result")
            .and_then(|r| r.get("shutdown"))
            .and_then(serde_json::Value::as_bool)
            == Some(true)
        {
            info!("codex-lspd: shutdown requested, exiting");
            break;
        }
    }
    Ok(())
}

/// Parse one NDJSON line and dispatch to the right method handler.
pub(crate) async fn dispatch_line(supervisor: &Arc<Supervisor>, line: &str) -> Result<Value> {
    let request: JsonRpcRequest = serde_json::from_str(line)
        .with_context(|| format!("invalid JSON-RPC request: {line}"))?;
    Ok(handle_request(supervisor, request).await)
}

#[derive(Debug, Deserialize)]
struct JsonRpcRequest {
    #[allow(dead_code)]
    jsonrpc: Option<String>,
    id: Value,
    method: String,
    #[serde(default)]
    params: Value,
}

async fn handle_request(supervisor: &Arc<Supervisor>, req: JsonRpcRequest) -> Value {
    let id = req.id.clone();
    match req.method.as_str() {
        "initialize" => json!({
            "jsonrpc": "2.0",
            "id": id,
            "result": {
                "server_info": { "name": "codex-lspd", "version": env!("CARGO_PKG_VERSION") },
                "protocol_version": "0.0.0-pr-i",
                "supported_methods": SUPPORTED_METHODS,
                "supported_notifications": SUPPORTED_NOTIFICATIONS,
            },
        }),
        "lsp/start" => match handle_lsp_start(supervisor, &req.params).await {
            Ok(v) => json!({ "jsonrpc": "2.0", "id": id, "result": v }),
            Err(LspdError::UnknownLanguage(lang)) => json!({
                "jsonrpc": "2.0",
                "id": id,
                "error": { "code": -32602, "message": format!("unknown language: {lang}") },
            }),
            Err(LspdError::SpawnFailed(bin, why)) => json!({
                "jsonrpc": "2.0",
                "id": id,
                "error": { "code": -32001, "message": format!(
                    "failed to spawn `{bin}`: {why} (is it on PATH? hint: `which {bin}`)"
                )},
            }),
            Err(LspdError::Other(why)) => json!({
                "jsonrpc": "2.0",
                "id": id,
                "error": { "code": -32603, "message": why },
            }),
        },
        "lsp/textDocumentDidOpen"
        | "lsp/textDocumentDidChange"
        | "lsp/textDocumentDidClose" => {
            // Forwarding is a future PR; for now ack with a sentinel.
            json!({ "jsonrpc": "2.0", "id": id, "result": { "forwarded": false } })
        }
        "shutdown" => json!({
            "jsonrpc": "2.0",
            "id": id,
            "result": { "shutdown": true, "ok": true },
        }),
        other => json!({
            "jsonrpc": "2.0",
            "id": id,
            "error": { "code": -32601, "message": format!("method not found: {other}") },
        }),
    }
}

#[derive(Debug)]
#[allow(dead_code)]
// SpawnFailed and the LspMessage helper land alongside the actual
// rust-analyzer spawn flow in PR-I-2; the framing/dispatch layer here
// is intentionally a load-bearing first slice.
pub(crate) enum LspdError {
    UnknownLanguage(String),
    SpawnFailed(String, String),
    Other(String),
}

#[derive(Debug, Deserialize)]
struct LspStartParams {
    language: String,
    #[serde(default)]
    #[allow(dead_code)]
    root_uri: Option<String>,
}

async fn handle_lsp_start(supervisor: &Arc<Supervisor>, params: &Value) -> Result<Value, LspdError> {
    let p: LspStartParams = serde_json::from_value(params.clone())
        .map_err(|e| LspdError::Other(format!("invalid lsp/start params: {e}")))?;

    let (bin, args) = server_invocation(&p.language)
        .ok_or_else(|| LspdError::UnknownLanguage(p.language.clone()))?;

    // Opt-in: real spawn path is gated by `CODEX_LSPD_REAL_SPAWN=1` so the
    // unit-test path stays hermetic. Production sets the env var; PR-M2
    // wires fine-grained didChange/publishDiagnostics forwarding on top.
    if std::env::var_os("CODEX_LSPD_REAL_SPAWN").as_deref() == Some(std::ffi::OsStr::new("1")) {
        let (server_id, capabilities) =
            start_real_server(supervisor, &p.language, p.root_uri.as_deref()).await?;
        return Ok(json!({
            "server_id": server_id,
            "capabilities": capabilities,
        }));
    }

    // Default placeholder path (kept for hermetic tests and for callers that
    // just want to validate the language table without paying spawn cost).
    let server_id = supervisor
        .next_id()
        .map_err(|e| LspdError::Other(e.to_string()))?;
    Ok(json!({
        "server_id": server_id,
        "would_invoke": { "bin": bin, "args": args },
        "note": "spawn deferred; PR-I lands the framing & dispatch layer only",
    }))
}

/// Spawn the configured language server for `language`, drive the LSP
/// `initialize` handshake, return `(server_id, capabilities)`.
///
/// Sends `initialized`, then `shutdown` + `exit`, then kills the child —
/// this PR (M) only proves the spawn-and-init path works. Streaming
/// didChange / publishDiagnostics forwarding is PR-M2.
pub(crate) async fn start_real_server(
    supervisor: &Arc<Supervisor>,
    language: &str,
    root_uri: Option<&str>,
) -> Result<(String, Value), LspdError> {
    let (bin, args) =
        server_invocation(language).ok_or_else(|| LspdError::UnknownLanguage(language.into()))?;
    let capabilities = try_spawn_and_init_with(bin, args, root_uri).await?;
    let server_id = supervisor
        .next_id()
        .map_err(|e| LspdError::Other(e.to_string()))?;
    Ok((server_id, capabilities))
}

/// Pure spawn + initialize helper, parameterised on the binary so tests can
/// drive the SpawnFailed path with a known-missing executable without
/// monkey-patching the static `server_invocation` table.
pub(crate) async fn try_spawn_and_init_with(
    bin: &str,
    args: &[&str],
    root_uri: Option<&str>,
) -> Result<Value, LspdError> {
    let mut child = match Command::new(bin)
        .args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
    {
        Ok(c) => c,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return Err(LspdError::SpawnFailed(bin.into(), "not found on PATH".into()));
        }
        Err(e) => return Err(LspdError::SpawnFailed(bin.into(), e.to_string())),
    };

    let mut stdin = child
        .stdin
        .take()
        .ok_or_else(|| LspdError::Other("child stdin was not piped".into()))?;
    let mut stdout = child
        .stdout
        .take()
        .ok_or_else(|| LspdError::Other("child stdout was not piped".into()))?;

    let init = json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "initialize",
        "params": {
            "processId": std::process::id(),
            "rootUri": root_uri,
            "rootPath": Value::Null,
            "capabilities": {
                "workspace": { "applyEdit": false, "workspaceEdit": null },
                "textDocument": {
                    "synchronization": { "didSave": true },
                    "publishDiagnostics": { "relatedInformation": false },
                },
            },
            "clientInfo": {
                "name": "codex-lspd",
                "version": env!("CARGO_PKG_VERSION"),
            },
            "workspaceFolders": Value::Null,
            "trace": "off",
        },
    });

    // Run the whole init handshake under a single timeout so a wedged or
    // mis-built server can't hang the dispatcher loop.
    let capabilities = tokio::time::timeout(INIT_TIMEOUT, async {
        write_lsp_frame(&mut stdin, &init)
            .await
            .map_err(|e| LspdError::Other(format!("write initialize: {e}")))?;

        loop {
            let frame = read_lsp_frame(&mut stdout)
                .await
                .map_err(|e| LspdError::Other(format!("read frame: {e}")))?;
            let Some(text) = frame else {
                return Err(LspdError::Other(
                    "language server closed stdout before initialize reply".into(),
                ));
            };
            let value: Value = serde_json::from_str(&text)
                .map_err(|e| LspdError::Other(format!("parse frame as JSON: {e}")))?;

            // Notifications (no `id`) — diagnostics, log messages, progress —
            // can legitimately arrive before the initialize reply. Just log
            // and keep waiting.
            let Some(id) = value.get("id") else {
                debug!(method = ?value.get("method"), "lspd: ignoring pre-init notification");
                continue;
            };

            // Match our request id (1). LSP allows servers to send their own
            // requests during init (e.g. window/workDoneProgress/create),
            // which carry a different id and no `result` — skip them too.
            if id != &json!(1) {
                debug!(?id, "lspd: ignoring server-originated request during init");
                continue;
            }

            let caps = value
                .get("result")
                .and_then(|r| r.get("capabilities"))
                .cloned()
                .unwrap_or_else(|| json!({}));
            return Ok::<Value, LspdError>(caps);
        }
    })
    .await
    .map_err(|_| LspdError::Other(format!("initialize timed out after {INIT_TIMEOUT:?}")))??;

    // Best-effort `initialized` notification. If the server has already gone
    // away the spawn path still counts as "worked" for test purposes.
    let initialized = json!({"jsonrpc": "2.0", "method": "initialized", "params": {}});
    if let Err(e) = write_lsp_frame(&mut stdin, &initialized).await {
        debug!(error = %e, "lspd: best-effort initialized notification failed");
    }

    // Polite shutdown: real production lifecycle (long-lived servers,
    // restart-on-crash) lands in PR-M2.
    let shutdown = json!({"jsonrpc": "2.0", "id": 2, "method": "shutdown"});
    let exit = json!({"jsonrpc": "2.0", "method": "exit"});
    if let Err(e) = write_lsp_frame(&mut stdin, &shutdown).await {
        debug!(error = %e, "lspd: best-effort shutdown failed");
    }
    if let Err(e) = write_lsp_frame(&mut stdin, &exit).await {
        debug!(error = %e, "lspd: best-effort exit failed");
    }
    if let Err(e) = child.kill().await {
        debug!(error = %e, "lspd: child.kill() failed (likely already exited)");
    }

    Ok(capabilities)
}

/// Static table of language → server invocation. Public for tests.
pub(crate) fn server_invocation(language: &str) -> Option<(&'static str, &'static [&'static str])> {
    match language {
        "rust" => Some(("rust-analyzer", &[])),
        "python" => Some(("pyright-langserver", &["--stdio"])),
        "typescript" | "javascript" => Some(("typescript-language-server", &["--stdio"])),
        "go" => Some(("gopls", &[])),
        _ => None,
    }
}

#[derive(Default)]
pub(crate) struct Supervisor {
    next_server_id: AtomicU64,
    /// Reserved for the spawn-and-track follow-up PR; unused for now.
    #[allow(dead_code)]
    servers: Mutex<HashMap<String, ()>>,
}

impl Supervisor {
    pub(crate) fn new() -> Self {
        Self::default()
    }

    fn next_id(&self) -> Result<String> {
        let n = self.next_server_id.fetch_add(1, Ordering::Relaxed);
        Ok(format!("srv{n}"))
    }
}

// =====================================================================
// LSP frame I/O — `Content-Length: <N>\r\n\r\n<body>`
// =====================================================================

/// Read one LSP-framed JSON message from `reader` into a `String`.
/// Returns `Ok(None)` on clean EOF.
pub async fn read_lsp_frame<R>(reader: &mut R) -> Result<Option<String>>
where
    R: AsyncRead + Unpin,
{
    let mut header = Vec::with_capacity(64);
    let mut byte = [0u8; 1];
    let mut content_length: Option<usize> = None;
    loop {
        let n = reader.read(&mut byte).await?;
        if n == 0 {
            if header.is_empty() {
                return Ok(None);
            }
            anyhow::bail!("unexpected EOF inside LSP header");
        }
        header.push(byte[0]);
        if header.ends_with(b"\r\n\r\n") {
            break;
        }
    }
    let header_text = std::str::from_utf8(&header).context("non-UTF-8 LSP header")?;
    for line in header_text.split("\r\n") {
        if let Some(rest) = line.strip_prefix("Content-Length:").or_else(|| line.strip_prefix("content-length:")) {
            content_length = Some(rest.trim().parse().context("Content-Length not a number")?);
        }
    }
    let len = content_length.context("missing Content-Length header")?;
    let mut body = vec![0u8; len];
    reader.read_exact(&mut body).await?;
    Ok(Some(String::from_utf8(body).context("non-UTF-8 LSP body")?))
}

/// Write `value` as one LSP-framed JSON message to `writer`.
pub async fn write_lsp_frame<W>(writer: &mut W, value: &Value) -> Result<()>
where
    W: AsyncWrite + Unpin,
{
    let body = serde_json::to_vec(value)?;
    let header = format!("Content-Length: {}\r\n\r\n", body.len());
    writer.write_all(header.as_bytes()).await?;
    writer.write_all(&body).await?;
    writer.flush().await?;
    Ok(())
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
#[allow(dead_code)]
pub(crate) struct LspMessage {
    pub jsonrpc: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub method: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub params: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub result: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<Value>,
}

impl LspMessage {
    pub(crate) fn _request(id: u64, method: impl Into<String>, params: Value) -> Self {
        debug!(id, "constructing LSP request");
        Self {
            jsonrpc: "2.0".into(),
            id: Some(json!(id)),
            method: Some(method.into()),
            params: Some(params),
            result: None,
            error: None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;
    use serde_json::json;
    use std::sync::Arc;

    #[test]
    fn server_invocation_table_known_languages() {
        assert!(server_invocation("rust").is_some());
        assert!(server_invocation("python").is_some());
        assert!(server_invocation("typescript").is_some());
        assert!(server_invocation("javascript").is_some());
        assert!(server_invocation("go").is_some());
        assert!(server_invocation("klingon").is_none());
        assert!(server_invocation("").is_none());
    }

    #[tokio::test]
    async fn lsp_frame_round_trip() -> Result<()> {
        let value = json!({"id": 1, "method": "initialize", "x": [1, 2, 3]});
        let mut buf: Vec<u8> = Vec::new();
        write_lsp_frame(&mut buf, &value).await?;
        let head = std::str::from_utf8(&buf[..40])?;
        assert!(head.starts_with("Content-Length: "));
        let mut cursor = std::io::Cursor::new(buf);
        let body = read_lsp_frame(&mut cursor).await?;
        let body = body.expect("frame returned");
        let decoded: Value = serde_json::from_str(&body)?;
        assert_eq!(decoded, value);
        Ok(())
    }

    #[tokio::test]
    async fn lsp_frame_clean_eof_returns_none() -> Result<()> {
        let mut empty: &[u8] = &[];
        let body = read_lsp_frame(&mut empty).await?;
        assert!(body.is_none());
        Ok(())
    }

    #[tokio::test]
    async fn lsp_frame_rejects_missing_content_length() {
        let mut bytes: &[u8] = b"X-Header: 1\r\n\r\n";
        let res = read_lsp_frame(&mut bytes).await;
        assert!(res.is_err(), "should error on missing Content-Length");
    }

    #[tokio::test]
    async fn unknown_method_returns_method_not_found() -> Result<()> {
        let supervisor = Arc::new(Supervisor::new());
        let line = r#"{"jsonrpc":"2.0","id":"1","method":"lsp/teleport","params":{}}"#;
        let resp = dispatch_line(&supervisor, line).await?;
        assert_eq!(resp["error"]["code"], json!(-32601));
        assert!(resp["error"]["message"].as_str().unwrap_or_default().contains("lsp/teleport"));
        Ok(())
    }

    #[tokio::test]
    async fn lsp_start_unknown_language_returns_invalid_params() -> Result<()> {
        let supervisor = Arc::new(Supervisor::new());
        let line = r#"{"jsonrpc":"2.0","id":"1","method":"lsp/start","params":{"language":"klingon"}}"#;
        let resp = dispatch_line(&supervisor, line).await?;
        assert_eq!(resp["error"]["code"], json!(-32602));
        Ok(())
    }

    #[tokio::test]
    async fn lsp_start_known_language_returns_server_id() -> Result<()> {
        let supervisor = Arc::new(Supervisor::new());
        let line = r#"{"jsonrpc":"2.0","id":"1","method":"lsp/start","params":{"language":"rust","root_uri":"file:///x"}}"#;
        let resp = dispatch_line(&supervisor, line).await?;
        assert!(resp["result"]["server_id"].is_string());
        assert_eq!(resp["result"]["would_invoke"]["bin"], json!("rust-analyzer"));
        Ok(())
    }

    #[tokio::test]
    async fn initialize_returns_capabilities() -> Result<()> {
        let supervisor = Arc::new(Supervisor::new());
        let line = r#"{"jsonrpc":"2.0","id":"1","method":"initialize","params":{}}"#;
        let resp = dispatch_line(&supervisor, line).await?;
        assert_eq!(resp["result"]["server_info"]["name"], json!("codex-lspd"));
        assert!(resp["result"]["supported_methods"]
            .as_array()
            .unwrap()
            .iter()
            .any(|m| m == "lsp/start"));
        Ok(())
    }

    #[tokio::test]
    async fn shutdown_returns_ok() -> Result<()> {
        let supervisor = Arc::new(Supervisor::new());
        let line = r#"{"jsonrpc":"2.0","id":"99","method":"shutdown"}"#;
        let resp = dispatch_line(&supervisor, line).await?;
        assert_eq!(resp["result"]["shutdown"], json!(true));
        Ok(())
    }

    /// SpawnFailed path: parameterised helper means we don't need to mutate
    /// the static `server_invocation` table to exercise the missing-binary
    /// branch — call `try_spawn_and_init_with` directly with garbage.
    #[tokio::test(flavor = "multi_thread")]
    async fn lsp_start_with_unknown_binary_returns_spawn_failed() -> Result<()> {
        let err = try_spawn_and_init_with("definitely-not-a-real-binary-9z9z", &[], None)
            .await
            .err()
            .context("spawn must fail for missing binary")?;
        assert!(matches!(err, LspdError::SpawnFailed(_, _)));
        Ok(())
    }

    /// Real-server smoke test. Opt-in via `CODEX_LSPD_REAL_SPAWN_TEST=1` so
    /// CI without rust-analyzer (and the default `cargo test` invocation)
    /// skips it cleanly.
    #[tokio::test(flavor = "multi_thread")]
    async fn lsp_start_with_real_spawn_initializes_when_on_path() -> Result<()> {
        if std::env::var_os("CODEX_LSPD_REAL_SPAWN_TEST").is_none() {
            eprintln!("skip: set CODEX_LSPD_REAL_SPAWN_TEST=1 to run");
            return Ok(());
        }
        if which::which("rust-analyzer").is_err() {
            eprintln!("skip: rust-analyzer not on PATH");
            return Ok(());
        }
        let supervisor = Arc::new(Supervisor::new());
        let timed = tokio::time::timeout(
            std::time::Duration::from_secs(20),
            start_real_server(&supervisor, "rust", Some("file:///tmp")),
        )
        .await
        .context("real-spawn test wall-clock timeout")?;
        let (server_id, capabilities) = match timed {
            Ok(v) => v,
            Err(e) => anyhow::bail!("start_real_server returned an LspdError: {e:?}"),
        };
        assert!(server_id.starts_with("srv"));
        assert!(capabilities.is_object(), "capabilities must be a JSON object");
        Ok(())
    }
}
