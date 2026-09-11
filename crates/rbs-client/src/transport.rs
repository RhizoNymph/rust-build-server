//! Client-side transports: a framed duplex of `ClientMessage`/`ServerMessage`
//! over a unix socket, an `ssh host rbs proxy` child, or an in-process fake.

use std::future::Future;
use std::path::PathBuf;
use std::pin::Pin;
use std::process::Stdio;
use std::time::{Duration, Instant};

use rbs_proto::{
    ClientMessage, Decoder, FrameError, PROTOCOL_VERSION, ServerMessage, ServerStatus,
};
use thiserror::Error;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

pub type BoxFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

#[derive(Debug, Error)]
pub enum TransportError {
    #[error("connect to {target} failed: {source}")]
    Connect {
        target: String,
        #[source]
        source: std::io::Error,
    },
    #[error("i/o error: {0}")]
    Io(#[from] std::io::Error),
    #[error("framing error: {0}")]
    Frame(#[from] FrameError),
    #[error("connection closed by peer")]
    Closed,
    #[error("server rejected protocol: {0}")]
    Protocol(String),
    #[error("unexpected message from server: {0}")]
    Unexpected(String),
    #[error("timed out after {0:?}")]
    Timeout(Duration),
}

/// A connected, framed duplex to one server.
pub trait Conn: Send {
    fn send(&mut self, msg: ClientMessage) -> BoxFuture<'_, Result<(), TransportError>>;
    /// `Ok(None)` when the peer closed cleanly.
    fn recv(&mut self) -> BoxFuture<'_, Result<Option<ServerMessage>, TransportError>>;
}

/// Something that can open a [`Conn`].
pub trait Transport: Send + Sync {
    fn connect(&self) -> BoxFuture<'_, Result<Box<dyn Conn>, TransportError>>;
    /// Human-readable target for logs (`node0`, `/path/to/sock`).
    fn describe(&self) -> String;
}

/// Newline-delimited JSON framing over any async read/write pair.
pub struct Framed<R, W> {
    reader: R,
    writer: W,
    decoder: Decoder,
    buf: Vec<u8>,
    /// Kept alive for the life of the connection (e.g. the ssh child).
    _guard: Option<Box<dyn std::any::Any + Send>>,
}

impl<R, W> Framed<R, W>
where
    R: AsyncRead + Unpin + Send,
    W: AsyncWrite + Unpin + Send,
{
    pub fn new(reader: R, writer: W) -> Self {
        Self {
            reader,
            writer,
            decoder: Decoder::new(),
            buf: vec![0; 64 * 1024],
            _guard: None,
        }
    }

    pub fn with_guard(mut self, guard: Box<dyn std::any::Any + Send>) -> Self {
        self._guard = Some(guard);
        self
    }

    async fn send_inner(&mut self, msg: ClientMessage) -> Result<(), TransportError> {
        let bytes = rbs_proto::encode(&msg)?;
        self.writer.write_all(&bytes).await?;
        self.writer.flush().await?;
        Ok(())
    }

    async fn recv_inner(&mut self) -> Result<Option<ServerMessage>, TransportError> {
        loop {
            if let Some(m) = self.decoder.next_frame::<ServerMessage>()? {
                return Ok(Some(m));
            }
            let n = self.reader.read(&mut self.buf).await?;
            if n == 0 {
                if self.decoder.pending() > 0 {
                    return Err(TransportError::Closed);
                }
                return Ok(None);
            }
            self.decoder.push(&self.buf[..n])?;
        }
    }
}

impl<R, W> Conn for Framed<R, W>
where
    R: AsyncRead + Unpin + Send,
    W: AsyncWrite + Unpin + Send,
{
    fn send(&mut self, msg: ClientMessage) -> BoxFuture<'_, Result<(), TransportError>> {
        Box::pin(self.send_inner(msg))
    }
    fn recv(&mut self) -> BoxFuture<'_, Result<Option<ServerMessage>, TransportError>> {
        Box::pin(self.recv_inner())
    }
}

/// Connect to a local `rbs server` socket.
#[derive(Debug, Clone)]
pub struct UnixTransport {
    pub path: PathBuf,
}

impl Transport for UnixTransport {
    fn connect(&self) -> BoxFuture<'_, Result<Box<dyn Conn>, TransportError>> {
        Box::pin(async move {
            let stream = tokio::net::UnixStream::connect(&self.path)
                .await
                .map_err(|source| TransportError::Connect {
                    target: self.path.display().to_string(),
                    source,
                })?;
            let (r, w) = stream.into_split();
            Ok(Box::new(Framed::new(r, w)) as Box<dyn Conn>)
        })
    }
    fn describe(&self) -> String {
        self.path.display().to_string()
    }
}

/// `ssh -o BatchMode=yes -o ConnectTimeout=<secs> <host> rbs proxy`, framed over
/// the child's stdin/stdout. ssh's stderr is inherited so auth/host errors show.
#[derive(Debug, Clone)]
pub struct SshTransport {
    pub host: String,
    pub connect_timeout_ms: u64,
    /// `rbs` binary path on the remote (see `rbs_config::Remote::remote_bin`).
    pub remote_bin: String,
}

impl SshTransport {
    pub fn argv(&self) -> Vec<String> {
        let secs = self.connect_timeout_ms.div_ceil(1000).max(1);
        vec![
            "ssh".into(),
            "-o".into(),
            "BatchMode=yes".into(),
            "-o".into(),
            format!("ConnectTimeout={secs}"),
            self.host.clone(),
            self.remote_bin.clone(),
            "proxy".into(),
        ]
    }
}

impl Transport for SshTransport {
    fn connect(&self) -> BoxFuture<'_, Result<Box<dyn Conn>, TransportError>> {
        Box::pin(async move {
            let argv = self.argv();
            let mut child = tokio::process::Command::new(&argv[0])
                .args(&argv[1..])
                .stdin(Stdio::piped())
                .stdout(Stdio::piped())
                .stderr(Stdio::inherit())
                .kill_on_drop(true)
                .spawn()
                .map_err(|source| TransportError::Connect {
                    target: self.host.clone(),
                    source,
                })?;
            let stdin = child.stdin.take().ok_or_else(|| TransportError::Connect {
                target: self.host.clone(),
                source: std::io::Error::other("ssh stdin not piped"),
            })?;
            let stdout = child.stdout.take().ok_or_else(|| TransportError::Connect {
                target: self.host.clone(),
                source: std::io::Error::other("ssh stdout not piped"),
            })?;
            Ok(Box::new(Framed::new(stdout, stdin).with_guard(Box::new(child))) as Box<dyn Conn>)
        })
    }
    fn describe(&self) -> String {
        self.host.clone()
    }
}

/// Scripted in-process transport for tests: every `connect` yields a
/// connection that replies with the next scripted `ServerMessage` on each
/// `recv` (regardless of what was sent) and records everything sent.
#[cfg(test)]
use std::collections::VecDeque;
#[cfg(test)]
use std::sync::{Arc, Mutex};

#[cfg(test)]
#[derive(Clone, Default)]
pub struct FakeTransport {
    script: Arc<Mutex<VecDeque<ServerMessage>>>,
    sent: Arc<Mutex<Vec<ClientMessage>>>,
    connect_error: Option<String>,
    connects: Arc<Mutex<usize>>,
}

#[cfg(test)]
impl FakeTransport {
    pub fn scripted(msgs: impl IntoIterator<Item = ServerMessage>) -> Self {
        Self {
            script: Arc::new(Mutex::new(msgs.into_iter().collect())),
            ..Default::default()
        }
    }

    pub fn failing(reason: &str) -> Self {
        Self {
            connect_error: Some(reason.to_string()),
            ..Default::default()
        }
    }

    pub fn sent(&self) -> Vec<ClientMessage> {
        self.sent.lock().unwrap_or_else(|e| e.into_inner()).clone()
    }

    pub fn connect_count(&self) -> usize {
        *self.connects.lock().unwrap_or_else(|e| e.into_inner())
    }
}

#[cfg(test)]
struct FakeConn {
    script: Arc<Mutex<VecDeque<ServerMessage>>>,
    sent: Arc<Mutex<Vec<ClientMessage>>>,
}

#[cfg(test)]
impl Conn for FakeConn {
    fn send(&mut self, msg: ClientMessage) -> BoxFuture<'_, Result<(), TransportError>> {
        self.sent
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .push(msg);
        Box::pin(async { Ok(()) })
    }
    fn recv(&mut self) -> BoxFuture<'_, Result<Option<ServerMessage>, TransportError>> {
        let next = self
            .script
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .pop_front();
        Box::pin(async move { Ok(next) })
    }
}

#[cfg(test)]
impl Transport for FakeTransport {
    fn connect(&self) -> BoxFuture<'_, Result<Box<dyn Conn>, TransportError>> {
        *self.connects.lock().unwrap_or_else(|e| e.into_inner()) += 1;
        let result = match &self.connect_error {
            Some(reason) => Err(TransportError::Connect {
                target: "fake".into(),
                source: std::io::Error::other(reason.clone()),
            }),
            None => Ok(Box::new(FakeConn {
                script: Arc::clone(&self.script),
                sent: Arc::clone(&self.sent),
            }) as Box<dyn Conn>),
        };
        Box::pin(async move { result })
    }
    fn describe(&self) -> String {
        "fake".into()
    }
}

/// Result of a successful [`probe`].
#[derive(Debug, Clone, PartialEq)]
pub struct ProbeResult {
    pub status: ServerStatus,
    pub rtt: Duration,
}

/// Send `Hello`, expect `Hello` back; send `Status`, expect `Status`. The RTT is
/// the full Hello round trip (what a job submission will pay), measured on
/// this already-open connection.
pub async fn probe(conn: &mut dyn Conn) -> Result<ProbeResult, TransportError> {
    hello(conn).await?;
    // Time the Status exchange, not Hello: the first round trip over ssh pays
    // for the whole session handshake (~200 ms even on a sub-ms LAN) and would
    // make every remote look slower than max_rtt_ms.
    let t0 = Instant::now();
    conn.send(ClientMessage::Status).await?;
    match conn.recv().await? {
        Some(ServerMessage::Status(status)) => Ok(ProbeResult {
            status,
            rtt: t0.elapsed(),
        }),
        Some(ServerMessage::Error(e)) => Err(TransportError::Protocol(e.message)),
        Some(other) => Err(TransportError::Unexpected(format!("{other:?}"))),
        None => Err(TransportError::Closed),
    }
}

/// Hello handshake only; returns the server's identity.
pub async fn hello(conn: &mut dyn Conn) -> Result<rbs_proto::ServerIdentity, TransportError> {
    conn.send(ClientMessage::Hello {
        version: PROTOCOL_VERSION,
    })
    .await?;
    match conn.recv().await? {
        Some(ServerMessage::Hello { version, server }) => {
            if version != PROTOCOL_VERSION {
                return Err(TransportError::Protocol(format!(
                    "server speaks protocol v{version}, client v{PROTOCOL_VERSION}"
                )));
            }
            Ok(server)
        }
        Some(ServerMessage::Error(e)) => Err(TransportError::Protocol(e.message)),
        Some(other) => Err(TransportError::Unexpected(format!("{other:?}"))),
        None => Err(TransportError::Closed),
    }
}

/// `probe` with a deadline.
pub async fn probe_with_timeout(
    conn: &mut dyn Conn,
    timeout: Duration,
) -> Result<ProbeResult, TransportError> {
    match tokio::time::timeout(timeout, probe(conn)).await {
        Ok(r) => r,
        Err(_) => Err(TransportError::Timeout(timeout)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rbs_proto::{
        Capacity, ClientIdentity, ExitStatus, JobEvent, JobId, JobRequest, Priority,
        ServerIdentity, ToolchainFingerprint,
    };
    use std::collections::BTreeMap;

    fn identity() -> ServerIdentity {
        ServerIdentity {
            hostname: "node0".into(),
            version: "0.1.0".into(),
        }
    }

    pub(crate) fn status(accepting: bool) -> ServerStatus {
        ServerStatus {
            identity: identity(),
            capacity: Capacity {
                tokens_total: 30,
                tokens_free: 12,
                mem_available_bytes: 1 << 36,
                load1: 1.0,
                accepting,
            },
            queued: 0,
            running: 2,
            uptime_secs: 10,
        }
    }

    fn request() -> JobRequest {
        JobRequest {
            cwd: "/w".into(),
            argv: vec!["cargo".into(), "build".into()],
            env: BTreeMap::new(),
            toolchain: ToolchainFingerprint {
                rustc_commit: "abc".into(),
                rustc_version: "1.95.0".into(),
                host: "x86_64-unknown-linux-gnu".into(),
                cargo_version: "cargo 1.95.0".into(),
                toolchain_name: String::new(),
            },
            priority: Priority::Agent,
            client: ClientIdentity {
                hostname: "laptop".into(),
                pid: 1,
                label: None,
            },
            tty: false,
        }
    }

    /// Drive a scripted "server" over an in-process duplex; returns what it received.
    async fn fake_server(
        stream: tokio::io::DuplexStream,
        replies: Vec<ServerMessage>,
    ) -> Vec<ClientMessage> {
        let (r, mut w) = tokio::io::split(stream);
        let mut r = tokio::io::BufReader::new(r);
        let mut got = Vec::new();
        let mut replies = replies.into_iter();
        loop {
            let mut line = String::new();
            use tokio::io::AsyncBufReadExt;
            let n = r.read_line(&mut line).await.expect("read");
            if n == 0 {
                break;
            }
            let msg: ClientMessage = serde_json::from_str(line.trim_end()).expect("client frame");
            got.push(msg.clone());
            let reply: Vec<ServerMessage> = match msg {
                ClientMessage::Hello { .. } | ClientMessage::Status => {
                    replies.next().into_iter().collect()
                }
                ClientMessage::Submit(_) => replies.by_ref().collect(),
                ClientMessage::Cancel { .. } => Vec::new(),
            };
            for m in reply {
                w.write_all(&rbs_proto::encode(&m).expect("encode"))
                    .await
                    .expect("write");
            }
            if replies.len() == 0 && matches!(got.last(), Some(ClientMessage::Submit(_))) {
                break;
            }
        }
        got
    }

    #[tokio::test]
    async fn framed_round_trips_hello_status_submit_and_events() {
        let (client_side, server_side) = tokio::io::duplex(1024);
        let replies = vec![
            ServerMessage::Hello {
                version: PROTOCOL_VERSION,
                server: identity(),
            },
            ServerMessage::Status(status(true)),
            ServerMessage::Event(JobEvent::Started { id: JobId(1) }),
            ServerMessage::Event(JobEvent::Stdout {
                id: JobId(1),
                bytes: b"hi\n\x00\xff".to_vec(),
            }),
            ServerMessage::Event(JobEvent::Exited {
                id: JobId(1),
                status: ExitStatus::Code(0),
            }),
        ];
        let server = tokio::spawn(fake_server(server_side, replies.clone()));

        let (r, w) = tokio::io::split(client_side);
        let mut conn = Framed::new(r, w);
        let pr = probe(&mut conn).await.expect("probe");
        assert_eq!(pr.status, status(true));
        conn.send(ClientMessage::Submit(request()))
            .await
            .expect("send");
        let mut events = Vec::new();
        while let Some(m) = conn.recv().await.expect("recv") {
            events.push(m);
        }
        assert_eq!(events, replies[2..].to_vec());
        drop(conn);
        let got = server.await.expect("join");
        assert_eq!(
            got,
            vec![
                ClientMessage::Hello {
                    version: PROTOCOL_VERSION
                },
                ClientMessage::Status,
                ClientMessage::Submit(request()),
            ]
        );
    }

    #[tokio::test]
    async fn hello_rejects_version_mismatch() {
        let mut conn = FakeTransport::scripted([ServerMessage::Hello {
            version: PROTOCOL_VERSION + 1,
            server: identity(),
        }])
        .connect()
        .await
        .expect("connect");
        assert!(matches!(
            hello(conn.as_mut()).await,
            Err(TransportError::Protocol(_))
        ));
    }

    #[tokio::test]
    async fn probe_reports_closed_and_unexpected() {
        let t = FakeTransport::scripted([ServerMessage::Hello {
            version: PROTOCOL_VERSION,
            server: identity(),
        }]);
        let mut conn = t.connect().await.expect("connect");
        assert!(matches!(
            probe(conn.as_mut()).await,
            Err(TransportError::Closed)
        ));
        assert_eq!(t.connect_count(), 1);
        assert_eq!(
            t.sent(),
            vec![
                ClientMessage::Hello {
                    version: PROTOCOL_VERSION
                },
                ClientMessage::Status
            ]
        );

        let t = FakeTransport::scripted([
            ServerMessage::Hello {
                version: PROTOCOL_VERSION,
                server: identity(),
            },
            ServerMessage::Event(JobEvent::Started { id: JobId(9) }),
        ]);
        let mut conn = t.connect().await.expect("connect");
        assert!(matches!(
            probe(conn.as_mut()).await,
            Err(TransportError::Unexpected(_))
        ));
    }

    #[tokio::test]
    async fn failing_transport_errors_on_connect() {
        let t = FakeTransport::failing("nope");
        let err = t.connect().await.err().expect("must fail");
        assert!(matches!(err, TransportError::Connect { .. }));
        assert!(err.to_string().contains("nope"));
    }

    #[tokio::test]
    async fn truncated_frame_at_eof_is_an_error() {
        let (client_side, mut server_side) = tokio::io::duplex(64);
        server_side
            .write_all(b"{\"type\":\"status\"")
            .await
            .expect("write");
        drop(server_side);
        let (r, w) = tokio::io::split(client_side);
        let mut conn = Framed::new(r, w);
        assert!(matches!(conn.recv().await, Err(TransportError::Closed)));
    }

    #[test]
    fn ssh_argv_rounds_timeout_up_to_seconds() {
        let t = SshTransport {
            host: "node0".into(),
            connect_timeout_ms: 2500,
            remote_bin: ".local/bin/rbs".into(),
        };
        assert_eq!(
            t.argv(),
            vec![
                "ssh",
                "-o",
                "BatchMode=yes",
                "-o",
                "ConnectTimeout=3",
                "node0",
                ".local/bin/rbs",
                "proxy"
            ]
        );
        let t = SshTransport {
            host: "h".into(),
            connect_timeout_ms: 0,
            remote_bin: "rbs".into(),
        };
        assert_eq!(t.argv()[4], "ConnectTimeout=1");
    }
}
