#![forbid(unsafe_code)]

//! Reusable JSON-RPC 2.0 framing adapters for stdio transports.
//!
//! This crate intentionally stays minimal: it deals only with *framing*
//! (how bytes are split into discrete JSON-RPC messages on a stream) and
//! leaves message *typing* to higher-level crates such as
//! `codex-app-server-protocol`. Two framings are provided:
//!
//! * **NDJSON** (`NdjsonReader` / `NdjsonWriter`) — newline-delimited JSON,
//!   used by `codex-app-server` and most stdio JSON-RPC tooling.
//! * **LSP-style** (`LspFramedReader` / `LspFramedWriter`) — `Content-Length`
//!   headers terminated by `\r\n\r\n` followed by a UTF-8 JSON body, used
//!   by Language Server Protocol implementations.
//!
//! Messages flow through both adapters as a [`JsonRpcMessage`], which is a
//! transparent newtype around [`serde_json::Value`]. This avoids
//! reimplementing what other crates already model strongly while still
//! offering convenient accessors (`method`, `id`, `params`, `is_notification`).

use std::io;

use serde::Deserialize;
use serde::Serialize;
use tokio::io::AsyncBufReadExt;
use tokio::io::AsyncRead;
use tokio::io::AsyncReadExt;
use tokio::io::AsyncWrite;
use tokio::io::AsyncWriteExt;
use tokio::io::BufReader;

/// Default maximum body size accepted by framed readers.
///
/// Sized to comfortably handle realistic JSON-RPC payloads (including large
/// tool-call results) while still rejecting pathological or malicious input.
pub const DEFAULT_MAX_MESSAGE_SIZE: usize = 16 * 1024 * 1024;

/// JSON-RPC 2.0 request identifier.
///
/// The spec allows numeric or string IDs (and `null`, but `null` is treated
/// as "no id" for routing purposes here — callers that care should inspect
/// the parsed message directly).
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(untagged)]
pub enum RequestId {
    /// Numeric identifier, as emitted by most clients.
    Number(i64),
    /// String identifier, common in language-server style RPC.
    String(String),
}

/// JSON-RPC 2.0 error object as sent inside a response.
///
/// Kept as a public type so callers can construct synthetic errors when
/// adapting non-RPC failures (e.g. transport drops) into RPC responses.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct JsonRpcError {
    /// Numeric error code following the JSON-RPC 2.0 conventions.
    pub code: i32,
    /// Human-readable error description.
    pub message: String,
    /// Optional structured payload providing additional detail.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub data: Option<serde_json::Value>,
}

/// Transparent newtype around a JSON-RPC envelope.
///
/// We deliberately avoid hard-coding the request/response/notification shape
/// inside this framing crate. The wire format is just "a JSON object", so
/// keeping a `serde_json::Value` lets higher-level crates pick whatever
/// strongly-typed model suits them while this crate focuses on framing.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct JsonRpcMessage(pub serde_json::Value);

impl JsonRpcMessage {
    /// Wrap a raw JSON value as a JSON-RPC message envelope.
    pub fn new(value: serde_json::Value) -> Self {
        Self(value)
    }

    /// Borrow the underlying JSON value.
    pub fn as_value(&self) -> &serde_json::Value {
        &self.0
    }

    /// Consume this envelope and return the underlying JSON value.
    pub fn into_value(self) -> serde_json::Value {
        self.0
    }

    /// Returns the `method` field for requests and notifications, `None` for
    /// responses (which carry `result`/`error` instead).
    pub fn method(&self) -> Option<&str> {
        self.0.get("method").and_then(|v| v.as_str())
    }

    /// Returns the `id` field if present and non-null.
    ///
    /// Notifications omit `id`; some servers send `"id": null` for parse
    /// errors. We treat both as "no id".
    pub fn id(&self) -> Option<&serde_json::Value> {
        match self.0.get("id") {
            Some(v) if !v.is_null() => Some(v),
            _ => None,
        }
    }

    /// Returns the `params` field if present.
    pub fn params(&self) -> Option<&serde_json::Value> {
        self.0.get("params")
    }

    /// True when the envelope is a JSON-RPC notification: a `method` is
    /// present but no `id` (per the spec, notifications are fire-and-forget).
    pub fn is_notification(&self) -> bool {
        self.method().is_some() && self.id().is_none()
    }
}

/// Errors that can arise while framing or parsing JSON-RPC messages.
#[derive(Debug, thiserror::Error)]
pub enum FramingError {
    /// Underlying transport I/O failed.
    #[error("transport I/O error: {0}")]
    Io(#[from] io::Error),

    /// The body bytes were received but did not parse as JSON.
    #[error("failed to parse JSON-RPC message: {0}")]
    Parse(#[from] serde_json::Error),

    /// LSP framing received malformed or missing headers (e.g. no
    /// `Content-Length`, non-numeric value, header line not `\r\n` terminated).
    #[error("invalid Content-Length framing: {0}")]
    InvalidContentLength(String),

    /// An LSP-framed message exceeded the configured size limit.
    ///
    /// We refuse to allocate a buffer larger than `limit` to bound peak
    /// memory in the face of hostile or buggy peers.
    #[error("message body of {size} bytes exceeds limit of {limit} bytes")]
    MessageTooLarge {
        /// Body size advertised by the sender.
        size: usize,
        /// Configured maximum.
        limit: usize,
    },
}

/// Result alias used throughout this crate.
pub type Result<T> = std::result::Result<T, FramingError>;

// ---------------------------------------------------------------------------
// NDJSON
// ---------------------------------------------------------------------------

/// Reader for newline-delimited JSON-RPC 2.0 streams.
///
/// Each line is independently parsed; blank lines are skipped to be lenient
/// about trailing newlines and pretty-printers that emit empty separators.
pub struct NdjsonReader<R> {
    inner: BufReader<R>,
    buf: String,
}

impl<R: AsyncRead + Unpin> NdjsonReader<R> {
    /// Wrap an asynchronous reader. The reader is buffered internally so the
    /// caller does not need to provide a `BufReader`.
    pub fn new(reader: R) -> Self {
        Self {
            inner: BufReader::new(reader),
            buf: String::new(),
        }
    }

    /// Read the next JSON-RPC message, returning `Ok(None)` on clean EOF.
    ///
    /// Blank lines are silently skipped; non-empty lines that fail to parse
    /// surface as `FramingError::Parse` so the caller can decide whether to
    /// drop the connection.
    pub async fn read_message(&mut self) -> Result<Option<JsonRpcMessage>> {
        loop {
            self.buf.clear();
            let n = self.inner.read_line(&mut self.buf).await?;
            if n == 0 {
                return Ok(None);
            }
            let trimmed = self.buf.trim();
            if trimmed.is_empty() {
                continue;
            }
            let value: serde_json::Value = serde_json::from_str(trimmed)?;
            return Ok(Some(JsonRpcMessage(value)));
        }
    }
}

/// Writer for newline-delimited JSON-RPC 2.0 streams.
///
/// Each call to `write_message` serializes the message, appends `\n`, and
/// flushes — matching the contract that `codex-app-server` expects from its
/// stdio transport.
pub struct NdjsonWriter<W> {
    inner: W,
}

impl<W: AsyncWrite + Unpin> NdjsonWriter<W> {
    /// Wrap an asynchronous writer.
    pub fn new(writer: W) -> Self {
        Self { inner: writer }
    }

    /// Serialize and write a single message followed by `\n`, then flush.
    pub async fn write_message(&mut self, msg: &JsonRpcMessage) -> Result<()> {
        let mut bytes = serde_json::to_vec(&msg.0)?;
        bytes.push(b'\n');
        self.inner.write_all(&bytes).await?;
        self.inner.flush().await?;
        Ok(())
    }

    /// Consume the writer and return the inner I/O handle.
    pub fn into_inner(self) -> W {
        self.inner
    }
}

// ---------------------------------------------------------------------------
// LSP framing
// ---------------------------------------------------------------------------

/// Reader for LSP-style `Content-Length` framed JSON-RPC.
///
/// Headers are CRLF-terminated and case-insensitive, matching the
/// Language Server Protocol base protocol specification.
pub struct LspFramedReader<R> {
    inner: BufReader<R>,
    max_size: usize,
}

impl<R: AsyncRead + Unpin> LspFramedReader<R> {
    /// Wrap a reader with the default body size limit.
    pub fn new(reader: R) -> Self {
        Self::with_max_size(reader, DEFAULT_MAX_MESSAGE_SIZE)
    }

    /// Wrap a reader with an explicit maximum body size, in bytes.
    pub fn with_max_size(reader: R, max_size: usize) -> Self {
        Self {
            inner: BufReader::new(reader),
            max_size,
        }
    }

    /// Read the next framed message, returning `Ok(None)` if EOF is hit
    /// cleanly *before* any header bytes have arrived. EOF mid-frame is
    /// treated as an error via the underlying I/O.
    pub async fn read_message(&mut self) -> Result<Option<JsonRpcMessage>> {
        let mut content_length: Option<usize> = None;
        let mut header_line = String::new();
        let mut saw_any_header = false;

        loop {
            header_line.clear();
            let n = self.inner.read_line(&mut header_line).await?;
            if n == 0 {
                // Clean EOF before any header bytes — signal end of stream.
                if !saw_any_header {
                    return Ok(None);
                }
                return Err(FramingError::InvalidContentLength(
                    "unexpected EOF inside header block".to_string(),
                ));
            }
            saw_any_header = true;

            // Headers must terminate with CRLF; tolerate bare LF for resilience
            // when interacting with clients that strip CR.
            let line = header_line
                .strip_suffix("\r\n")
                .or_else(|| header_line.strip_suffix('\n'))
                .unwrap_or(&header_line);

            if line.is_empty() {
                // End of header block.
                break;
            }

            let (name, value) = line.split_once(':').ok_or_else(|| {
                FramingError::InvalidContentLength(format!("malformed header line: {line:?}"))
            })?;

            if name.eq_ignore_ascii_case("content-length") {
                let value = value.trim();
                let parsed: usize = value.parse().map_err(|_| {
                    FramingError::InvalidContentLength(format!(
                        "non-numeric Content-Length: {value:?}"
                    ))
                })?;
                content_length = Some(parsed);
            }
            // Other headers (e.g. Content-Type) are accepted but ignored.
        }

        let len = content_length.ok_or_else(|| {
            FramingError::InvalidContentLength("missing Content-Length header".to_string())
        })?;

        if len > self.max_size {
            return Err(FramingError::MessageTooLarge {
                size: len,
                limit: self.max_size,
            });
        }

        let mut body = vec![0u8; len];
        self.inner.read_exact(&mut body).await?;
        let value: serde_json::Value = serde_json::from_slice(&body)?;
        Ok(Some(JsonRpcMessage(value)))
    }
}

/// Writer for LSP-style `Content-Length` framed JSON-RPC.
pub struct LspFramedWriter<W> {
    inner: W,
}

impl<W: AsyncWrite + Unpin> LspFramedWriter<W> {
    /// Wrap an asynchronous writer.
    pub fn new(writer: W) -> Self {
        Self { inner: writer }
    }

    /// Encode `msg` with a `Content-Length` header and flush.
    ///
    /// The header uses ASCII bytes only and is followed by a single
    /// `\r\n\r\n` separator before the UTF-8 JSON body, per the LSP spec.
    pub async fn write_message(&mut self, msg: &JsonRpcMessage) -> Result<()> {
        let body = serde_json::to_vec(&msg.0)?;
        let header = format!("Content-Length: {}\r\n\r\n", body.len());
        self.inner.write_all(header.as_bytes()).await?;
        self.inner.write_all(&body).await?;
        self.inner.flush().await?;
        Ok(())
    }

    /// Consume the writer and return the inner I/O handle.
    pub fn into_inner(self) -> W {
        self.inner
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;
    use serde_json::json;
    use tokio::io::AsyncWriteExt;
    use tokio::io::duplex;

    fn notification(method: &str) -> JsonRpcMessage {
        JsonRpcMessage(json!({
            "jsonrpc": "2.0",
            "method": method,
            "params": {"hello": "world"},
        }))
    }

    fn request(id: i64, method: &str) -> JsonRpcMessage {
        JsonRpcMessage(json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": method,
            "params": {"k": "v"},
        }))
    }

    fn response(id: i64) -> JsonRpcMessage {
        JsonRpcMessage(json!({
            "jsonrpc": "2.0",
            "id": id,
            "result": {"ok": true},
        }))
    }

    #[tokio::test]
    async fn ndjson_round_trip_notification() {
        let (client, server) = duplex(64 * 1024);
        let (server_read, _server_write) = tokio::io::split(server);
        let (_client_read, client_write) = tokio::io::split(client);

        let mut writer = NdjsonWriter::new(client_write);
        let mut reader = NdjsonReader::new(server_read);

        let msg = notification("ping");
        writer.write_message(&msg).await.expect("write");
        drop(writer);

        let got = reader
            .read_message()
            .await
            .expect("read")
            .expect("some message");
        assert_eq!(got, msg);

        // Next read should observe EOF.
        assert!(reader.read_message().await.expect("eof read").is_none());
    }

    #[tokio::test]
    async fn ndjson_skips_blank_lines() {
        let (client, server) = duplex(64 * 1024);
        let (server_read, _server_write) = tokio::io::split(server);
        let (_client_read, mut client_write) = tokio::io::split(client);

        client_write
            .write_all(b"\n\n{\"jsonrpc\":\"2.0\",\"method\":\"ping\"}\n\n")
            .await
            .expect("write raw");
        client_write.flush().await.expect("flush");
        drop(client_write);

        let mut reader = NdjsonReader::new(server_read);
        let got = reader
            .read_message()
            .await
            .expect("read")
            .expect("some message");
        assert_eq!(got.method(), Some("ping"));
        assert!(reader.read_message().await.expect("eof read").is_none());
    }

    #[tokio::test]
    async fn ndjson_returns_none_on_eof() {
        let (client, server) = duplex(64);
        let (server_read, _server_write) = tokio::io::split(server);
        drop(client); // immediate EOF on the server side
        let mut reader = NdjsonReader::new(server_read);
        assert!(reader.read_message().await.expect("eof").is_none());
    }

    #[tokio::test]
    async fn ndjson_rejects_invalid_json() {
        let (client, server) = duplex(64 * 1024);
        let (server_read, _server_write) = tokio::io::split(server);
        let (_client_read, mut client_write) = tokio::io::split(client);

        client_write
            .write_all(b"this-is-not-json\n")
            .await
            .expect("write raw");
        client_write.flush().await.expect("flush");
        drop(client_write);

        let mut reader = NdjsonReader::new(server_read);
        let err = reader.read_message().await.expect_err("should fail");
        match err {
            FramingError::Parse(_) => {}
            other => panic!("expected Parse error, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn lsp_round_trip() {
        let (client, server) = duplex(64 * 1024);
        let (server_read, _server_write) = tokio::io::split(server);
        let (_client_read, client_write) = tokio::io::split(client);

        let mut writer = LspFramedWriter::new(client_write);
        let mut reader = LspFramedReader::new(server_read);

        let req = request(7, "initialize");
        writer.write_message(&req).await.expect("write req");
        let resp = response(7);
        writer.write_message(&resp).await.expect("write resp");
        drop(writer);

        let got_req = reader
            .read_message()
            .await
            .expect("read req")
            .expect("some");
        assert_eq!(got_req, req);
        let got_resp = reader
            .read_message()
            .await
            .expect("read resp")
            .expect("some");
        assert_eq!(got_resp, resp);
        assert!(reader.read_message().await.expect("eof").is_none());
    }

    #[tokio::test]
    async fn lsp_case_insensitive_header() {
        let body = br#"{"jsonrpc":"2.0","method":"ping"}"#;
        let mut framed = Vec::new();
        framed.extend_from_slice(
            format!("CoNtEnT-LeNgTh: {}\r\n", body.len()).as_bytes(),
        );
        framed.extend_from_slice(b"Content-Type: application/vscode-jsonrpc; charset=utf-8\r\n");
        framed.extend_from_slice(b"\r\n");
        framed.extend_from_slice(body);

        let cursor = std::io::Cursor::new(framed);
        let mut reader = LspFramedReader::new(cursor);
        let got = reader.read_message().await.expect("read").expect("some");
        assert_eq!(got.method(), Some("ping"));
    }

    #[tokio::test]
    async fn lsp_missing_content_length() {
        // No Content-Length, just an empty header block followed by a body.
        let framed: &[u8] = b"X-Other: 1\r\n\r\n{}";
        let cursor = std::io::Cursor::new(framed.to_vec());
        let mut reader = LspFramedReader::new(cursor);
        let err = reader.read_message().await.expect_err("should fail");
        match err {
            FramingError::InvalidContentLength(msg) => {
                assert!(msg.contains("Content-Length"), "msg = {msg}");
            }
            other => panic!("expected InvalidContentLength, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn lsp_body_size_limit_enforced() {
        let framed = b"Content-Length: 9999\r\n\r\n";
        let cursor = std::io::Cursor::new(framed.to_vec());
        let mut reader = LspFramedReader::with_max_size(cursor, 1024);
        let err = reader.read_message().await.expect_err("should fail");
        match err {
            FramingError::MessageTooLarge { size, limit } => {
                assert_eq!(size, 9999);
                assert_eq!(limit, 1024);
            }
            other => panic!("expected MessageTooLarge, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn lsp_parses_content_length_with_extra_whitespace() {
        let body = br#"{"jsonrpc":"2.0","method":"ping"}"#;
        let mut framed = Vec::new();
        framed.extend_from_slice(format!("Content-Length:   {}\r\n\r\n", body.len()).as_bytes());
        framed.extend_from_slice(body);
        let cursor = std::io::Cursor::new(framed);
        let mut reader = LspFramedReader::new(cursor);
        let got = reader.read_message().await.expect("read").expect("some");
        assert_eq!(got.method(), Some("ping"));
    }

    #[test]
    fn request_id_serde() {
        let n: RequestId = serde_json::from_str("123").expect("number");
        assert_eq!(n, RequestId::Number(123));
        let s: RequestId = serde_json::from_str("\"abc\"").expect("string");
        assert_eq!(s, RequestId::String("abc".to_string()));

        // Round-trip both forms.
        assert_eq!(serde_json::to_string(&n).expect("ser"), "123");
        assert_eq!(serde_json::to_string(&s).expect("ser"), "\"abc\"");
    }

    #[test]
    fn helper_accessors() {
        let notif = notification("hello");
        assert_eq!(notif.method(), Some("hello"));
        assert!(notif.is_notification());
        assert!(notif.id().is_none());
        assert!(notif.params().is_some());

        let req = request(1, "do");
        assert_eq!(req.method(), Some("do"));
        assert!(!req.is_notification());
        assert!(req.id().is_some());

        let resp = response(1);
        assert_eq!(resp.method(), None);
        assert!(!resp.is_notification());
        assert!(resp.id().is_some());
    }

    #[test]
    fn json_rpc_error_serde_omits_none_data() {
        let err = JsonRpcError {
            code: -32601,
            message: "method not found".to_string(),
            data: None,
        };
        let json = serde_json::to_value(&err).expect("ser");
        assert_eq!(
            json,
            json!({"code": -32601, "message": "method not found"})
        );
    }
}
