//! Unix-socket API: a pared-down JSON-RPC 2.0 lookalike, one JSON object per
//! line in each direction. Wire format and error codes match the Go version.

use std::fmt;
use std::os::unix::fs::{FileTypeExt, PermissionsExt};
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::{UnixListener, UnixStream};
use tokio_util::sync::CancellationToken;

use crate::logging::LogWriter;
use crate::supervisor::{Error, Manager};

const IDLE_TIMEOUT: Duration = Duration::from_secs(60);
const MAX_LINE: usize = 1 << 20;

// Standard JSON-RPC 2.0 codes.
pub const CODE_PARSE_ERROR: i64 = -32700;
pub const CODE_INVALID_REQUEST: i64 = -32600;
pub const CODE_METHOD_NOT_FOUND: i64 = -32601;
pub const CODE_INVALID_PARAMS: i64 = -32602;
pub const CODE_INTERNAL_ERROR: i64 = -32603;
// Domain codes, in the implementation-defined range.
pub const CODE_PROCESS_NOT_FOUND: i64 = -32001;
pub const CODE_READONLY: i64 = -32002;
pub const CODE_PROCESS_READONLY: i64 = -32003;
pub const CODE_ALREADY_IN_TARGET_STATE: i64 = -32004;

#[derive(Debug, Serialize, Deserialize)]
pub struct Request {
    #[serde(default)]
    pub id: Option<i64>,
    #[serde(default)]
    pub method: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub params: Option<Value>,
}

/// `id` is null only when the request could not be parsed at all.
#[derive(Debug, Serialize, Deserialize)]
pub struct Response {
    pub id: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub result: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<RpcError>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RpcError {
    pub code: i64,
    pub message: String,
}

impl RpcError {
    fn new(code: i64, message: impl Into<String>) -> Self {
        Self { code, message: message.into() }
    }
}

impl fmt::Display for RpcError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for RpcError {}

/// Binds the socket, replacing a stale socket file from a crashed previous
/// run - but never a non-socket file, and never a socket another instance is
/// still listening on.
pub fn bind(path: &Path, perm: u32) -> Result<UnixListener> {
    match std::fs::metadata(path) {
        Ok(meta) => {
            if !meta.file_type().is_socket() {
                bail!("refusing to remove {}: exists and is not a socket", path.display());
            }
            if std::os::unix::net::UnixStream::connect(path).is_ok() {
                bail!("socket {} is in use by another running procd instance", path.display());
            }
            std::fs::remove_file(path).with_context(|| format!("remove stale socket {}", path.display()))?;
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => return Err(e).with_context(|| format!("stat {}", path.display())),
    }
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).with_context(|| format!("create {}", dir.display()))?;
    }

    let listener = UnixListener::bind(path).with_context(|| format!("listen on {}", path.display()))?;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(perm))
        .with_context(|| format!("chmod {:o} {}", perm, path.display()))?;
    Ok(listener)
}

pub struct Server {
    pub manager: Arc<Manager>,
    /// Global `--readonly`: blocks all mutating methods.
    pub readonly: bool,
    pub console: Arc<LogWriter>,
}

impl Server {
    /// Accepts connections until `shutdown` is cancelled.
    pub async fn serve(self: Arc<Self>, listener: UnixListener, shutdown: CancellationToken) {
        loop {
            tokio::select! {
                _ = shutdown.cancelled() => return,
                accepted = listener.accept() => match accepted {
                    Ok((stream, _)) => {
                        tokio::spawn(Arc::clone(&self).handle_conn(stream));
                    }
                    Err(e) => {
                        // e.g. EMFILE - back off instead of spinning.
                        self.console.line("core", &format!("accept error: {e}"));
                        tokio::time::sleep(Duration::from_millis(100)).await;
                    }
                },
            }
        }
    }

    /// Several requests per connection; closed after IDLE_TIMEOUT without one.
    async fn handle_conn(self: Arc<Self>, stream: UnixStream) {
        let (read, mut write) = stream.into_split();
        let mut reader = BufReader::new(read);
        let mut buf = Vec::new();
        loop {
            buf.clear();
            let mut limited = (&mut reader).take(MAX_LINE as u64 + 1);
            let read = tokio::time::timeout(IDLE_TIMEOUT, limited.read_until(b'\n', &mut buf)).await;
            match read {
                Err(_) | Ok(Ok(0)) => return,
                Ok(Err(e)) => {
                    self.console.line("core", &format!("connection read error: {e}"));
                    return;
                }
                Ok(Ok(_)) => {}
            }
            if buf.len() > MAX_LINE {
                self.console.line("core", "connection closed: request line exceeds 1 MiB");
                return;
            }

            let line = buf.trim_ascii();
            if line.is_empty() {
                continue;
            }
            let mut out = serde_json::to_vec(&self.dispatch(line).await).expect("response serializes");
            out.push(b'\n');
            if write.write_all(&out).await.is_err() {
                return;
            }
        }
    }

    /// Never fails itself - every failure is encoded in the Response.
    pub async fn dispatch(&self, line: &[u8]) -> Response {
        let req: Request = match serde_json::from_slice(line) {
            Ok(req) => req,
            Err(e) => {
                return Response { id: None, result: None, error: Some(RpcError::new(CODE_PARSE_ERROR, format!("invalid JSON: {e}"))) };
            }
        };
        let id = Some(req.id.unwrap_or(0));
        let result = self.call(req.method.as_deref().unwrap_or(""), req.params.as_ref()).await;
        match result {
            Ok(result) => Response { id, result: Some(result), error: None },
            Err(error) => Response { id, result: None, error: Some(error) },
        }
    }

    async fn call(&self, method: &str, params: Option<&Value>) -> Result<Value, RpcError> {
        let mutating = match method {
            "" => return Err(RpcError::new(CODE_INVALID_REQUEST, "method is required")),
            "process.list" | "process.get" => false,
            "process.start" | "process.stop" => true,
            _ => return Err(RpcError::new(CODE_METHOD_NOT_FOUND, format!("unknown method {method:?}"))),
        };
        if mutating && self.readonly {
            return Err(RpcError::new(CODE_READONLY, "procd is running in --readonly mode"));
        }
        if method == "process.list" {
            return Ok(json!({ "processes": self.manager.list() }));
        }

        let key = params
            .and_then(|p| p.get("key"))
            .and_then(Value::as_str)
            .filter(|k| !k.is_empty())
            .ok_or_else(|| RpcError::new(CODE_INVALID_PARAMS, "params.key (string) is required"))?;
        let info = self
            .manager
            .get(key)
            .ok_or_else(|| RpcError::new(CODE_PROCESS_NOT_FOUND, format!("unknown process key {key:?}")))?;

        let result = match method {
            "process.get" => Ok(info),
            "process.start" => self.manager.start(key),
            _ => {
                if info.readonly {
                    return Err(RpcError::new(
                        CODE_PROCESS_READONLY,
                        format!("process {key:?} is protected and cannot be stopped via the API"),
                    ));
                }
                self.manager.stop(key).await
            }
        };
        match result {
            Ok(info) => Ok(json!({ "process": info })),
            Err(Error::AlreadyRunning(info) | Error::AlreadyStopped(info)) => Err(RpcError::new(
                CODE_ALREADY_IN_TARGET_STATE,
                format!("process {key:?} is already {}", info.state),
            )),
            Err(Error::NotFound) => Err(RpcError::new(CODE_PROCESS_NOT_FOUND, format!("unknown process key {key:?}"))),
            Err(e) => Err(RpcError::new(CODE_INTERNAL_ERROR, e.to_string())),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config;

    fn server(readonly: bool) -> (Server, tempfile::TempDir) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("process.toml");
        std::fs::write(
            &path,
            "[[process]]\nkey = \"fe\"\ncmd = \"sleep 30\"\nreadonly = true\nstdout = false\n\
             [[process]]\nkey = \"w\"\ncmd = \"sleep 30\"\nstdout = false\n",
        )
        .unwrap();
        let cfg = config::load(&path).unwrap();
        let console = Arc::new(LogWriter::stdout());
        let manager = Manager::new(&cfg, Arc::clone(&console)).unwrap();
        (Server { manager, readonly, console }, dir)
    }

    async fn call(s: &Server, line: &str) -> Value {
        serde_json::to_value(s.dispatch(line.as_bytes()).await).unwrap()
    }

    #[tokio::test]
    async fn error_codes() {
        let (s, _dir) = server(false);
        assert_eq!(call(&s, "nope").await["error"]["code"], CODE_PARSE_ERROR);
        assert_eq!(call(&s, "nope").await["id"], Value::Null);
        assert_eq!(call(&s, r#"{"id":1}"#).await["error"]["code"], CODE_INVALID_REQUEST);
        assert_eq!(call(&s, r#"{"id":1,"method":"x"}"#).await["error"]["code"], CODE_METHOD_NOT_FOUND);
        assert_eq!(call(&s, r#"{"id":1,"method":"process.get"}"#).await["error"]["code"], CODE_INVALID_PARAMS);
        assert_eq!(
            call(&s, r#"{"id":1,"method":"process.get","params":{"key":"x"}}"#).await["error"]["code"],
            CODE_PROCESS_NOT_FOUND
        );
        assert_eq!(
            call(&s, r#"{"id":1,"method":"process.stop","params":{"key":"fe"}}"#).await["error"]["code"],
            CODE_PROCESS_READONLY
        );
        assert_eq!(
            call(&s, r#"{"id":1,"method":"process.stop","params":{"key":"w"}}"#).await["error"]["code"],
            CODE_ALREADY_IN_TARGET_STATE
        );
    }

    #[tokio::test]
    async fn start_list_stop() {
        let (s, _dir) = server(false);
        let resp = call(&s, r#"{"id":7,"method":"process.start","params":{"key":"w"}}"#).await;
        assert_eq!(resp["id"], 7);
        assert_eq!(resp["result"]["process"]["state"], "running");
        assert!(resp.get("error").is_none());

        let resp = call(&s, r#"{"id":8,"method":"process.list"}"#).await;
        let procs = resp["result"]["processes"].as_array().unwrap();
        assert_eq!(procs.len(), 2);
        assert_eq!(procs[0]["state"], "stopped");
        assert!(procs[0].get("pid").is_none(), "pid is omitted while stopped");

        let resp = call(&s, r#"{"id":9,"method":"process.stop","params":{"key":"w"}}"#).await;
        assert_eq!(resp["result"]["process"]["state"], "stopped");
        assert_eq!(resp["result"]["process"]["exit_signal"], "terminated");
    }

    #[tokio::test]
    async fn readonly_blocks_mutations_only() {
        let (s, _dir) = server(true);
        assert_eq!(
            call(&s, r#"{"id":1,"method":"process.start","params":{"key":"w"}}"#).await["error"]["code"],
            CODE_READONLY
        );
        assert!(call(&s, r#"{"id":1,"method":"process.list"}"#).await.get("result").is_some());
    }
}
