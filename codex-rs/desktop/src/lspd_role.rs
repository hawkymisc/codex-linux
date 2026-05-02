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
//! Methods: `initialize`, `lsp/start`, `lsp/textDocumentDid{Open,Change,Close}`,
//! `shutdown`. The forwarding methods carry `server_id` returned by `lsp/start`.
//!
//! codex-lspd → parent: NDJSON JSON-RPC 2.0. Responses for the methods above
//! plus async server-originated notifications:
//!   - `textDocument/publishDiagnostics` (params include `server_id` so the
//!     parent can route by language server).
//!
//! ## Wire format (codex-lspd ↔ language server)
//!
//! Real LSP framing: `Content-Length: <N>\r\n\r\n<body>` per message.
//! Implemented by [`read_lsp_frame`] / [`write_lsp_frame`] below.

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::collections::HashMap;
use std::process::Stdio;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;
use tokio::io::{
    AsyncBufReadExt as _, AsyncRead, AsyncReadExt as _, AsyncWrite, AsyncWriteExt as _, BufReader,
};
use tokio::process::{Child, ChildStdin, ChildStdout, Command};
use tokio::sync::{Mutex, mpsc};
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
    let mut reader = BufReader::new(stdin).lines();

    // Single mpsc channel feeding stdout: dispatch responses and per-server
    // async notifications all flow through one writer task so frames cannot
    // interleave.
    let (out_tx, mut out_rx) = mpsc::unbounded_channel::<Value>();
    let writer = tokio::spawn(async move {
        let mut stdout = tokio::io::stdout();
        while let Some(value) = out_rx.recv().await {
            let mut serialized = match serde_json::to_string(&value) {
                Ok(s) => s,
                Err(e) => {
                    warn!(error = %e, "lspd: serialize outbound failed; dropping frame");
                    continue;
                }
            };
            serialized.push('\n');
            if stdout.write_all(serialized.as_bytes()).await.is_err() {
                break;
            }
            if stdout.flush().await.is_err() {
                break;
            }
        }
    });

    let supervisor = Arc::new(Supervisor::with_sink(out_tx.clone()));

    while let Some(line) = reader.next_line().await? {
        if line.trim().is_empty() {
            continue;
        }
        let response = match dispatch_line(&supervisor, &line).await {
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

        let is_shutdown = response
            .get("result")
            .and_then(|r| r.get("shutdown"))
            .and_then(Value::as_bool)
            == Some(true);

        if out_tx.send(response).is_err() {
            // Writer task gone — nothing left to do.
            break;
        }

        if is_shutdown {
            info!("codex-lspd: shutdown requested, exiting");
            break;
        }
    }

    // Dropping the supervisor kills children (kill_on_drop), which EOFs each
    // pump task; pumps drop their out_tx clones; the writer task drains and
    // exits. Drop the local clone first so the writer doesn't see us as a
    // straggler sender.
    drop(out_tx);
    drop(supervisor);
    let _ = writer.await;
    Ok(())
}

/// Parse one NDJSON line and dispatch to the right method handler.
pub(crate) async fn dispatch_line(supervisor: &Arc<Supervisor>, line: &str) -> Result<Value> {
    let request: JsonRpcRequest =
        serde_json::from_str(line).with_context(|| format!("invalid JSON-RPC request: {line}"))?;
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
                "protocol_version": "0.0.0-pr-u",
                "supported_methods": SUPPORTED_METHODS,
                "supported_notifications": SUPPORTED_NOTIFICATIONS,
            },
        }),
        "lsp/start" => match handle_lsp_start(supervisor, &req.params).await {
            Ok(v) => json!({ "jsonrpc": "2.0", "id": id, "result": v }),
            Err(e) => lspd_error_to_json(id, e),
        },
        "lsp/textDocumentDidOpen"
        | "lsp/textDocumentDidChange"
        | "lsp/textDocumentDidClose" => {
            match handle_lsp_forward(supervisor, &req.method, &req.params).await {
                Ok(v) => json!({ "jsonrpc": "2.0", "id": id, "result": v }),
                Err(e) => lspd_error_to_json(id, e),
            }
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
pub(crate) enum LspdError {
    UnknownLanguage(String),
    UnknownServerId(String),
    SpawnFailed(String, String),
    Other(String),
}

fn lspd_error_to_json(id: Value, err: LspdError) -> Value {
    match err {
        LspdError::UnknownLanguage(lang) => json!({
            "jsonrpc": "2.0",
            "id": id,
            "error": { "code": -32602, "message": format!("unknown language: {lang}") },
        }),
        LspdError::UnknownServerId(s) => json!({
            "jsonrpc": "2.0",
            "id": id,
            "error": { "code": -32602, "message": format!("unknown server_id: {s}") },
        }),
        LspdError::SpawnFailed(bin, why) => json!({
            "jsonrpc": "2.0",
            "id": id,
            "error": { "code": -32001, "message": format!(
                "failed to spawn `{bin}`: {why} (is it on PATH? hint: `which {bin}`)"
            )},
        }),
        LspdError::Other(why) => json!({
            "jsonrpc": "2.0",
            "id": id,
            "error": { "code": -32603, "message": why },
        }),
    }
}

#[derive(Debug, Deserialize)]
struct LspStartParams {
    language: String,
    #[serde(default)]
    root_uri: Option<String>,
}

async fn handle_lsp_start(
    supervisor: &Arc<Supervisor>,
    params: &Value,
) -> Result<Value, LspdError> {
    let p: LspStartParams = serde_json::from_value(params.clone())
        .map_err(|e| LspdError::Other(format!("invalid lsp/start params: {e}")))?;

    let (bin, args) = server_invocation(&p.language)
        .ok_or_else(|| LspdError::UnknownLanguage(p.language.clone()))?;

    // Opt-in: real spawn path is gated by `CODEX_LSPD_REAL_SPAWN=1` so the
    // unit-test path stays hermetic. PR-U keeps the spawned server alive and
    // wires didChange / publishDiagnostics forwarding on top of PR-M.
    if std::env::var_os("CODEX_LSPD_REAL_SPAWN").as_deref() == Some(std::ffi::OsStr::new("1")) {
        let (server_id, capabilities) =
            start_real_server(supervisor, &p.language, p.root_uri.as_deref()).await?;
        return Ok(json!({
            "server_id": server_id,
            "capabilities": capabilities,
        }));
    }

    // Default placeholder path — kept for hermetic tests and for callers
    // that just want to validate the language table without paying spawn
    // cost. No server_id is registered in the supervisor; subsequent
    // `lsp/textDocument*` requests against this id will return
    // -32602 (unknown server_id).
    let server_id = supervisor
        .next_id()
        .map_err(|e| LspdError::Other(e.to_string()))?;
    Ok(json!({
        "server_id": server_id,
        "would_invoke": { "bin": bin, "args": args },
        "note": "spawn deferred; set CODEX_LSPD_REAL_SPAWN=1 to spawn the real server",
    }))
}

/// Spawn the configured language server for `language`, drive the LSP
/// `initialize` handshake, register a long-lived [`ServerHandle`] in the
/// supervisor, and return `(server_id, capabilities)`.
///
/// Subsequent `lsp/textDocument*` requests against this `server_id` get
/// translated to LSP notifications and forwarded to this server. The
/// server's notifications (notably `textDocument/publishDiagnostics`) get
/// pumped back to the parent NDJSON channel.
pub(crate) async fn start_real_server(
    supervisor: &Arc<Supervisor>,
    language: &str,
    root_uri: Option<&str>,
) -> Result<(String, Value), LspdError> {
    let (bin, args) =
        server_invocation(language).ok_or_else(|| LspdError::UnknownLanguage(language.into()))?;
    let (child, stdin, stdout) = spawn_lsp_child(bin, args).await?;
    install_initialized_server(supervisor, stdin, stdout, root_uri, Some(child)).await
}

/// Spawn a language server child with piped stdio. Pure spawn step;
/// no handshake. Separated so tests can exercise the SpawnFailed path
/// against a known-missing binary without going through the rest of
/// the install machinery.
pub(crate) async fn spawn_lsp_child(
    bin: &str,
    args: &[&str],
) -> Result<(Child, ChildStdin, ChildStdout), LspdError> {
    let mut child = match Command::new(bin)
        .args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .kill_on_drop(true)
        .spawn()
    {
        Ok(c) => c,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return Err(LspdError::SpawnFailed(bin.into(), "not found on PATH".into()));
        }
        Err(e) => return Err(LspdError::SpawnFailed(bin.into(), e.to_string())),
    };
    let stdin = child
        .stdin
        .take()
        .ok_or_else(|| LspdError::Other("child stdin was not piped".into()))?;
    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| LspdError::Other("child stdout was not piped".into()))?;
    Ok((child, stdin, stdout))
}

/// Drive the LSP initialize handshake on already-opened streams, register a
/// long-lived [`ServerHandle`] in the supervisor, and spawn the notification
/// pump that forwards `textDocument/publishDiagnostics` (and any other
/// known async notification) back to the parent.
///
/// Generic over the stream types so tests can drive both sides via
/// `tokio::io::duplex`. The optional `child` parameter is moved into the
/// handle so its kill-on-drop semantics tear down the real grandchild
/// process when the supervisor is dropped.
pub(crate) async fn install_initialized_server<W, R>(
    supervisor: &Arc<Supervisor>,
    mut stdin: W,
    mut stdout: R,
    root_uri: Option<&str>,
    child: Option<Child>,
) -> Result<(String, Value), LspdError>
where
    W: AsyncWrite + Send + Unpin + 'static,
    R: AsyncRead + Send + Unpin + 'static,
{
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
    // away the install path still counts as "worked" and the pump will EOF
    // shortly after.
    let initialized = json!({"jsonrpc": "2.0", "method": "initialized", "params": {}});
    if let Err(e) = write_lsp_frame(&mut stdin, &initialized).await {
        debug!(error = %e, "lspd: best-effort initialized notification failed");
    }

    // Register and start the pump + writer. The pump owns `stdout`; the
    // writer owns `stdin` and drains an mpsc fed by `lsp/textDocument*`
    // forwarders. Splitting the two avoids holding a `tokio::sync::MutexGuard`
    // across `.await` (the workspace clippy lint forbids it) while still
    // serialising all writes to one server through one task.
    let server_id = supervisor
        .next_id()
        .map_err(|e| LspdError::Other(e.to_string()))?;
    let (writer_tx, writer_rx) = mpsc::unbounded_channel::<Value>();
    let writer = spawn_server_writer(server_id.clone(), stdin, writer_rx);
    let pump = spawn_server_pump(server_id.clone(), stdout, supervisor.out().clone());
    let handle = ServerHandle {
        writer_tx,
        _writer: writer,
        _pump: pump,
        _child: child,
    };
    supervisor
        .servers
        .lock()
        .await
        .insert(server_id.clone(), handle);

    Ok((server_id, capabilities))
}

/// Spawn the per-server writer task. It owns the LSP server's stdin and
/// drains an unbounded mpsc of LSP-shape `Value`s, writing each as one
/// LSP frame. Exits on receiver close (server dropped) or write error.
fn spawn_server_writer<W>(
    server_id: String,
    mut stdin: W,
    mut rx: mpsc::UnboundedReceiver<Value>,
) -> tokio::task::JoinHandle<()>
where
    W: AsyncWrite + Send + Unpin + 'static,
{
    tokio::spawn(async move {
        while let Some(value) = rx.recv().await {
            if let Err(e) = write_lsp_frame(&mut stdin, &value).await {
                warn!(server_id = %server_id, error = %e, "lspd: server writer error; exiting");
                break;
            }
        }
        debug!(server_id = %server_id, "lspd: server writer task exiting");
    })
}

/// Spawn the per-server pump task that reads LSP frames from the language
/// server's stdout, recognises `textDocument/publishDiagnostics`, tags each
/// occurrence with the originating `server_id`, and forwards it to the
/// parent NDJSON channel. Other server-originated frames are logged and
/// dropped (PR-U scope).
fn spawn_server_pump<R>(
    server_id: String,
    mut stdout: R,
    out: mpsc::UnboundedSender<Value>,
) -> tokio::task::JoinHandle<()>
where
    R: AsyncRead + Send + Unpin + 'static,
{
    tokio::spawn(async move {
        loop {
            match read_lsp_frame(&mut stdout).await {
                Ok(Some(text)) => {
                    let value: Value = match serde_json::from_str(&text) {
                        Ok(v) => v,
                        Err(e) => {
                            warn!(server_id = %server_id, error = %e, "lspd: pump dropped non-JSON frame");
                            continue;
                        }
                    };
                    let method = value.get("method").and_then(Value::as_str);
                    match method {
                        Some("textDocument/publishDiagnostics") => {
                            let mut params =
                                value.get("params").cloned().unwrap_or_else(|| json!({}));
                            if let Some(obj) = params.as_object_mut() {
                                obj.insert("server_id".into(), json!(server_id));
                            }
                            let outbound = json!({
                                "jsonrpc": "2.0",
                                "method": "textDocument/publishDiagnostics",
                                "params": params,
                            });
                            if out.send(outbound).is_err() {
                                debug!(server_id = %server_id, "lspd: parent sink closed; pump exiting");
                                break;
                            }
                        }
                        Some(m) => {
                            debug!(server_id = %server_id, method = m, "lspd: ignoring server notification");
                        }
                        None => {
                            // Reply or server-originated request. PR-U does
                            // not yet round-trip our own requests through the
                            // server, so any reply here is unsolicited.
                            debug!(server_id = %server_id, "lspd: ignoring server reply or request");
                        }
                    }
                }
                Ok(None) => {
                    info!(server_id = %server_id, "lspd: server stdout closed; pump exiting");
                    break;
                }
                Err(e) => {
                    warn!(server_id = %server_id, error = %e, "lspd: pump read error; exiting");
                    break;
                }
            }
        }
    })
}

/// Translate a parent `lsp/textDocumentDid{Open,Change,Close}` request into the
/// matching LSP notification and write it to the registered server's stdin.
async fn handle_lsp_forward(
    supervisor: &Arc<Supervisor>,
    parent_method: &str,
    params: &Value,
) -> Result<Value, LspdError> {
    let server_id = params
        .get("server_id")
        .and_then(Value::as_str)
        .ok_or_else(|| LspdError::Other("missing server_id".into()))?;

    let (lsp_method, lsp_params) = match parent_method {
        "lsp/textDocumentDidOpen" => (
            "textDocument/didOpen",
            json!({
                "textDocument": params.get("text_document").cloned().unwrap_or(Value::Null),
            }),
        ),
        "lsp/textDocumentDidChange" => (
            "textDocument/didChange",
            json!({
                "textDocument": params.get("text_document").cloned().unwrap_or(Value::Null),
                "contentChanges": params
                    .get("content_changes")
                    .cloned()
                    .unwrap_or_else(|| json!([])),
            }),
        ),
        "lsp/textDocumentDidClose" => (
            "textDocument/didClose",
            json!({
                "textDocument": params.get("text_document").cloned().unwrap_or(Value::Null),
            }),
        ),
        other => {
            return Err(LspdError::Other(format!(
                "unsupported forward method: {other}"
            )));
        }
    };

    let lsp_notif = json!({
        "jsonrpc": "2.0",
        "method": lsp_method,
        "params": lsp_params,
    });

    // Hold the supervisor's HashMap lock only long enough to clone the
    // per-handle writer sender; release before sending so we don't serialise
    // across servers. The actual frame write happens asynchronously inside
    // the per-server writer task.
    let writer_tx = {
        let servers = supervisor.servers.lock().await;
        let handle = servers
            .get(server_id)
            .ok_or_else(|| LspdError::UnknownServerId(server_id.into()))?;
        handle.writer_tx.clone()
    };

    writer_tx
        .send(lsp_notif)
        .map_err(|_| LspdError::Other(format!("server {server_id} writer task is gone")))?;

    Ok(json!({ "forwarded": true, "server_id": server_id }))
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

/// One running language server, addressable via `server_id`.
pub(crate) struct ServerHandle {
    /// Sender into the per-server writer task. Forwarders push LSP-shape
    /// `Value`s here and the writer serialises them to the server's stdin
    /// without collision.
    writer_tx: mpsc::UnboundedSender<Value>,
    /// Per-server writer task. Detached on drop; exits naturally when
    /// `writer_tx` is dropped (which happens when the supervisor drops
    /// the handle).
    _writer: tokio::task::JoinHandle<()>,
    /// Pump task that forwards async server notifications to the parent.
    /// Detached on drop; exits naturally when the server's stdout EOFs
    /// (which happens once `_child` is killed-on-drop).
    _pump: tokio::task::JoinHandle<()>,
    /// Owns the spawned process; killed-on-drop via `Command::kill_on_drop(true)`.
    /// `None` for in-test handles backed by `tokio::io::duplex`.
    _child: Option<Child>,
}

pub(crate) struct Supervisor {
    next_server_id: AtomicU64,
    pub(crate) servers: Mutex<HashMap<String, ServerHandle>>,
    /// Outbound NDJSON to the parent: dispatch responses + per-server
    /// async notifications. Pumped by the writer task in [`run`].
    out: mpsc::UnboundedSender<Value>,
}

impl Supervisor {
    /// Test-only constructor. The internal sink has no receiver, so any
    /// publishDiagnostics arriving from a registered server will silently
    /// drop and the pump will exit. Tests that want to observe outbound
    /// frames should use [`Supervisor::with_sink`].
    #[cfg(test)]
    pub(crate) fn new() -> Self {
        let (tx, _rx) = mpsc::unbounded_channel();
        Self::with_sink(tx)
    }

    pub(crate) fn with_sink(out: mpsc::UnboundedSender<Value>) -> Self {
        Self {
            next_server_id: AtomicU64::new(0),
            servers: Mutex::new(HashMap::new()),
            out,
        }
    }

    fn next_id(&self) -> Result<String> {
        let n = self.next_server_id.fetch_add(1, Ordering::Relaxed);
        Ok(format!("srv{n}"))
    }

    pub(crate) fn out(&self) -> &mpsc::UnboundedSender<Value> {
        &self.out
    }
}

// =====================================================================
// LSP frame I/O — `Content-Length: <N>\r\n\r\n<body>`
// =====================================================================

/// Read one LSP-framed JSON message from `reader` into a `String`.
/// Returns `Ok(None)` on clean EOF.
pub async fn read_lsp_frame<R>(reader: &mut R) -> Result<Option<String>>
where
    R: AsyncRead + Unpin + ?Sized,
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
        if let Some(rest) = line
            .strip_prefix("Content-Length:")
            .or_else(|| line.strip_prefix("content-length:"))
        {
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
    W: AsyncWrite + Unpin + ?Sized,
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
    use std::time::Duration;
    use tokio::io::duplex;

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
        assert!(
            resp["error"]["message"]
                .as_str()
                .unwrap_or_default()
                .contains("lsp/teleport")
        );
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
        assert!(
            resp["result"]["supported_methods"]
                .as_array()
                .unwrap()
                .iter()
                .any(|m| m == "lsp/start")
        );
        // PR-U adds the three textDocument forwarding methods.
        for m in [
            "lsp/textDocumentDidOpen",
            "lsp/textDocumentDidChange",
            "lsp/textDocumentDidClose",
        ] {
            assert!(
                resp["result"]["supported_methods"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .any(|s| s == m),
                "supported_methods missing {m}",
            );
        }
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
    /// branch — call `spawn_lsp_child` directly with garbage.
    #[tokio::test(flavor = "multi_thread")]
    async fn spawn_lsp_child_reports_spawn_failed_for_missing_binary() -> Result<()> {
        let err = spawn_lsp_child("definitely-not-a-real-binary-9z9z", &[])
            .await
            .err()
            .context("spawn must fail for missing binary")?;
        assert!(matches!(err, LspdError::SpawnFailed(_, _)));
        Ok(())
    }

    /// Forwarding for an unknown server_id returns -32602 (invalid params).
    #[tokio::test]
    async fn forward_to_unknown_server_id_returns_invalid_params() -> Result<()> {
        let supervisor = Arc::new(Supervisor::new());
        let line = r#"{"jsonrpc":"2.0","id":"1","method":"lsp/textDocumentDidChange",
            "params":{"server_id":"srv999",
                      "text_document":{"uri":"file:///x","version":1},
                      "content_changes":[{"text":"y"}]}}"#;
        let resp = dispatch_line(&supervisor, line).await?;
        assert_eq!(resp["error"]["code"], json!(-32602));
        assert!(
            resp["error"]["message"]
                .as_str()
                .unwrap_or_default()
                .contains("srv999")
        );
        Ok(())
    }

    /// Wire up a fake LSP server via duplex pipes, drive the install
    /// handshake, then forward a `lsp/textDocumentDidChange` and assert
    /// the server receives a real `textDocument/didChange` LSP frame.
    #[tokio::test(flavor = "multi_thread")]
    async fn forward_did_change_writes_lsp_notification_to_server() -> Result<()> {
        // Two unidirectional duplex pipes give us full-duplex.
        // - parent_to_server: lspd writes here, mock server reads
        // - server_to_parent: mock server writes here, lspd reads
        let (parent_to_server_w, mut server_in_r) = duplex(64 * 1024);
        let (mut server_out_w, parent_from_server_r) = duplex(64 * 1024);

        let (out_tx, _out_rx) = mpsc::unbounded_channel();
        let supervisor = Arc::new(Supervisor::with_sink(out_tx));

        // Mock LSP server: respond to initialize, then drain initialized,
        // then keep the pipes open so subsequent forwards land here.
        let mock = tokio::spawn(async move {
            let init_frame = read_lsp_frame(&mut server_in_r).await?;
            let init_text = init_frame.context("initialize frame")?;
            let init_value: Value = serde_json::from_str(&init_text)?;
            assert_eq!(init_value["method"], json!("initialize"));
            let id = init_value["id"].clone();
            let response = json!({
                "jsonrpc": "2.0",
                "id": id,
                "result": { "capabilities": { "textDocumentSync": 1 } },
            });
            write_lsp_frame(&mut server_out_w, &response).await?;

            // Drain `initialized` notification.
            let initialized_frame = read_lsp_frame(&mut server_in_r).await?;
            let initialized_text = initialized_frame.context("initialized frame")?;
            let initialized_value: Value = serde_json::from_str(&initialized_text)?;
            assert_eq!(initialized_value["method"], json!("initialized"));

            // Then the forwarded didChange.
            let did_change_frame = read_lsp_frame(&mut server_in_r).await?;
            let did_change_text = did_change_frame.context("didChange frame")?;
            let did_change_value: Value = serde_json::from_str(&did_change_text)?;
            Ok::<_, anyhow::Error>((did_change_value, server_in_r, server_out_w))
        });

        let (server_id, capabilities) = install_initialized_server(
            &supervisor,
            parent_to_server_w,
            parent_from_server_r,
            Some("file:///tmp"),
            None,
        )
        .await
        .map_err(|e| anyhow::anyhow!("install failed: {e:?}"))?;
        assert_eq!(capabilities["textDocumentSync"], json!(1));
        assert!(server_id.starts_with("srv"));

        let line = format!(
            r#"{{"jsonrpc":"2.0","id":"42","method":"lsp/textDocumentDidChange",
                "params":{{"server_id":"{server_id}",
                           "text_document":{{"uri":"file:///x.rs","version":2}},
                           "content_changes":[{{"text":"fn main() {{}}"}}]}}}}"#
        );
        let resp = dispatch_line(&supervisor, &line).await?;
        assert_eq!(resp["result"]["forwarded"], json!(true));
        assert_eq!(resp["result"]["server_id"], json!(server_id.clone()));

        let (did_change_value, _stay_alive_r, _stay_alive_w) =
            tokio::time::timeout(Duration::from_secs(5), mock).await???;
        assert_eq!(did_change_value["method"], json!("textDocument/didChange"));
        assert_eq!(
            did_change_value["params"]["textDocument"]["uri"],
            json!("file:///x.rs")
        );
        assert_eq!(
            did_change_value["params"]["contentChanges"][0]["text"],
            json!("fn main() {}")
        );
        Ok(())
    }

    /// `lsp/textDocumentDidOpen` and `lsp/textDocumentDidClose` translate
    /// to their LSP equivalents with the right param shape.
    #[tokio::test(flavor = "multi_thread")]
    async fn forward_did_open_and_close_translate_method_names() -> Result<()> {
        let (parent_to_server_w, mut server_in_r) = duplex(64 * 1024);
        let (mut server_out_w, parent_from_server_r) = duplex(64 * 1024);

        let (out_tx, _out_rx) = mpsc::unbounded_channel();
        let supervisor = Arc::new(Supervisor::with_sink(out_tx));

        let mock = tokio::spawn(async move {
            let init_text = read_lsp_frame(&mut server_in_r).await?.context("init")?;
            let init: Value = serde_json::from_str(&init_text)?;
            let response = json!({
                "jsonrpc": "2.0",
                "id": init["id"],
                "result": { "capabilities": {} },
            });
            write_lsp_frame(&mut server_out_w, &response).await?;
            let _ = read_lsp_frame(&mut server_in_r).await?; // initialized

            let open: Value =
                serde_json::from_str(&read_lsp_frame(&mut server_in_r).await?.context("open")?)?;
            let close: Value =
                serde_json::from_str(&read_lsp_frame(&mut server_in_r).await?.context("close")?)?;
            Ok::<_, anyhow::Error>((open, close, server_in_r, server_out_w))
        });

        let (server_id, _) = install_initialized_server(
            &supervisor,
            parent_to_server_w,
            parent_from_server_r,
            None,
            None,
        )
        .await
        .map_err(|e| anyhow::anyhow!("install failed: {e:?}"))?;

        let open_line = format!(
            r#"{{"jsonrpc":"2.0","id":"1","method":"lsp/textDocumentDidOpen",
                "params":{{"server_id":"{server_id}",
                           "text_document":{{"uri":"file:///a.rs","languageId":"rust","version":1,"text":"x"}}}}}}"#
        );
        let close_line = format!(
            r#"{{"jsonrpc":"2.0","id":"2","method":"lsp/textDocumentDidClose",
                "params":{{"server_id":"{server_id}",
                           "text_document":{{"uri":"file:///a.rs"}}}}}}"#
        );
        let r1 = dispatch_line(&supervisor, &open_line).await?;
        let r2 = dispatch_line(&supervisor, &close_line).await?;
        assert_eq!(r1["result"]["forwarded"], json!(true));
        assert_eq!(r2["result"]["forwarded"], json!(true));

        let (open, close, _r, _w) = tokio::time::timeout(Duration::from_secs(5), mock).await???;
        assert_eq!(open["method"], json!("textDocument/didOpen"));
        assert_eq!(open["params"]["textDocument"]["uri"], json!("file:///a.rs"));
        assert_eq!(open["params"]["textDocument"]["languageId"], json!("rust"));
        assert_eq!(close["method"], json!("textDocument/didClose"));
        assert_eq!(close["params"]["textDocument"]["uri"], json!("file:///a.rs"));
        Ok(())
    }

    /// `textDocument/publishDiagnostics` from the server is forwarded onto
    /// the parent NDJSON channel with `server_id` injected into params.
    #[tokio::test(flavor = "multi_thread")]
    async fn publish_diagnostics_pumps_to_parent_outbound() -> Result<()> {
        let (parent_to_server_w, mut server_in_r) = duplex(64 * 1024);
        let (mut server_out_w, parent_from_server_r) = duplex(64 * 1024);

        let (out_tx, mut out_rx) = mpsc::unbounded_channel();
        let supervisor = Arc::new(Supervisor::with_sink(out_tx));

        // Drive only the init handshake on the server side, then keep
        // pushing notifications out.
        let mock = tokio::spawn(async move {
            let init_text = read_lsp_frame(&mut server_in_r).await?.context("init")?;
            let init: Value = serde_json::from_str(&init_text)?;
            let response = json!({
                "jsonrpc": "2.0",
                "id": init["id"],
                "result": { "capabilities": {} },
            });
            write_lsp_frame(&mut server_out_w, &response).await?;
            let _ = read_lsp_frame(&mut server_in_r).await?; // initialized

            // First diagnostics frame: empty list.
            let diag1 = json!({
                "jsonrpc": "2.0",
                "method": "textDocument/publishDiagnostics",
                "params": { "uri": "file:///x.rs", "diagnostics": [] },
            });
            write_lsp_frame(&mut server_out_w, &diag1).await?;

            // Second diagnostics frame: one warning.
            let diag2 = json!({
                "jsonrpc": "2.0",
                "method": "textDocument/publishDiagnostics",
                "params": {
                    "uri": "file:///y.rs",
                    "diagnostics": [{
                        "range": {"start":{"line":0,"character":0},"end":{"line":0,"character":1}},
                        "severity": 2,
                        "message": "unused"
                    }]
                },
            });
            write_lsp_frame(&mut server_out_w, &diag2).await?;
            Ok::<_, anyhow::Error>((server_in_r, server_out_w))
        });

        let (server_id, _) = install_initialized_server(
            &supervisor,
            parent_to_server_w,
            parent_from_server_r,
            None,
            None,
        )
        .await
        .map_err(|e| anyhow::anyhow!("install failed: {e:?}"))?;

        let v1 = tokio::time::timeout(Duration::from_secs(5), out_rx.recv())
            .await?
            .context("first diagnostics frame")?;
        assert_eq!(v1["method"], json!("textDocument/publishDiagnostics"));
        assert_eq!(v1["params"]["uri"], json!("file:///x.rs"));
        assert_eq!(v1["params"]["server_id"], json!(server_id.clone()));
        assert_eq!(v1["params"]["diagnostics"], json!([]));

        let v2 = tokio::time::timeout(Duration::from_secs(5), out_rx.recv())
            .await?
            .context("second diagnostics frame")?;
        assert_eq!(v2["params"]["uri"], json!("file:///y.rs"));
        assert_eq!(v2["params"]["server_id"], json!(server_id));
        assert_eq!(v2["params"]["diagnostics"][0]["severity"], json!(2));

        // Keep mock alive until end so its writers don't drop early.
        let _ = tokio::time::timeout(Duration::from_secs(2), mock).await;
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
        // Server is now registered and alive in the supervisor; dropping the
        // Arc at end-of-test kills the child.
        Ok(())
    }
}
