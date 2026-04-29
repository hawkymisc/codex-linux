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
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use tokio::io::{
    AsyncBufReadExt as _, AsyncRead, AsyncReadExt as _, AsyncWrite, AsyncWriteExt as _, BufReader,
};
use tokio::sync::Mutex;
use tracing::{debug, info, warn};

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
enum LspdError {
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

    // Spawn-skip: don't actually launch in the smoke path; report the
    // would-be invocation. The full spawn flow is a follow-up PR.
    let server_id = supervisor
        .next_id()
        .map_err(|e| LspdError::Other(e.to_string()))?;
    Ok(json!({
        "server_id": server_id,
        "would_invoke": { "bin": bin, "args": args },
        "note": "spawn deferred; PR-I lands the framing & dispatch layer only",
    }))
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
}
