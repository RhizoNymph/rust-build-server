//! End-to-end tests: run the server on a temp socket (unscoped) and talk to it
//! over a raw `UnixStream` using the wire types from `rbs-proto`.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::Duration;

use rbs_proto::{
    ClientIdentity, ClientMessage, Decoder, ExitStatus, JobEvent, JobId, JobRequest,
    PROTOCOL_VERSION, Priority, ProtoErrorCode, RejectReason, ServerMessage, ToolchainFingerprint,
    encode,
};
use rbs_server::{RunOptions, ScopeMode, ServerOpts, run_server_until};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::UnixStream;
use tokio::sync::oneshot;

/// A running test server plus the handle that stops it.
struct TestServer {
    socket: PathBuf,
    _dir: tempfile::TempDir,
    stop: Option<oneshot::Sender<()>>,
    task: tokio::task::JoinHandle<anyhow::Result<()>>,
}

impl TestServer {
    async fn start(max_jobs: u32, tokens: u32) -> TestServer {
        let dir = tempfile::tempdir().expect("tempdir");
        let socket = dir.path().join("srv/server.sock");
        let mut cfg = rbs_config::Config::default();
        cfg.server.max_jobs = max_jobs;
        cfg.server.min_mem_available_gib = 0;
        cfg.server.queue_limit = 4;
        cfg.server.job_timeout_secs = 30;
        let (tx, rx) = oneshot::channel();
        let opts = ServerOpts {
            socket: Some(socket.clone()),
            tokens: Some(tokens),
        };
        let run = RunOptions {
            scope: ScopeMode::Disabled,
        };
        let task = tokio::spawn(run_server_until(cfg, opts, run, async move {
            let _ = rx.await;
        }));
        // Wait for the socket to come up.
        for _ in 0..200 {
            if UnixStream::connect(&socket).await.is_ok() {
                return TestServer {
                    socket,
                    _dir: dir,
                    stop: Some(tx),
                    task,
                };
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
        panic!("server did not start listening on {}", socket.display());
    }

    async fn stop(mut self) {
        if let Some(tx) = self.stop.take() {
            let _ = tx.send(());
        }
        let res = tokio::time::timeout(Duration::from_secs(5), &mut self.task)
            .await
            .expect("server stops promptly")
            .expect("server task joins");
        res.expect("server exits cleanly");
        assert!(!self.socket.exists(), "socket must be removed on exit");
    }
}

/// Thin framed client over a `UnixStream`.
struct Client {
    stream: UnixStream,
    dec: Decoder,
}

impl Client {
    async fn connect(socket: &Path) -> Client {
        let stream = UnixStream::connect(socket).await.expect("connect");
        Client {
            stream,
            dec: Decoder::new(),
        }
    }

    async fn send(&mut self, msg: &ClientMessage) {
        let bytes = encode(msg).expect("encode");
        self.stream.write_all(&bytes).await.expect("write");
    }

    /// Next frame, or `None` when the server closed the connection.
    async fn recv(&mut self) -> Option<ServerMessage> {
        loop {
            if let Some(m) = self.dec.next_frame::<ServerMessage>().expect("decode") {
                return Some(m);
            }
            let mut buf = [0u8; 4096];
            let n = self.stream.read(&mut buf).await.expect("read");
            if n == 0 {
                return None;
            }
            self.dec.push(&buf[..n]).expect("push");
        }
    }

    async fn recv_timeout(&mut self, d: Duration) -> Option<ServerMessage> {
        tokio::time::timeout(d, self.recv())
            .await
            .expect("timed out waiting for server message")
    }

    async fn hello(&mut self) {
        self.send(&ClientMessage::Hello {
            version: PROTOCOL_VERSION,
        })
        .await;
        match self.recv_timeout(Duration::from_secs(5)).await {
            Some(ServerMessage::Hello { version, server }) => {
                assert_eq!(version, PROTOCOL_VERSION);
                assert!(!server.hostname.is_empty());
            }
            other => panic!("expected Hello, got {other:?}"),
        }
    }

    async fn next_event(&mut self) -> JobEvent {
        match self.recv_timeout(Duration::from_secs(10)).await {
            Some(ServerMessage::Event(ev)) => ev,
            other => panic!("expected Event, got {other:?}"),
        }
    }
}

fn request(cwd: &Path, argv: &[&str], fp: ToolchainFingerprint) -> JobRequest {
    JobRequest {
        cwd: cwd.to_path_buf(),
        argv: argv.iter().map(|s| s.to_string()).collect(),
        env: BTreeMap::new(),
        toolchain: fp,
        priority: Priority::Agent,
        client: ClientIdentity {
            hostname: "test".into(),
            pid: std::process::id(),
            label: Some("e2e".into()),
        },
        tty: false,
    }
}

fn fingerprint(cwd: &Path) -> ToolchainFingerprint {
    rbs_toolchain::fingerprint(cwd).expect("fingerprint local toolchain")
}

/// Collect events until the terminal one; returns (id, events).
async fn run_to_completion(c: &mut Client) -> Vec<JobEvent> {
    let mut events = Vec::new();
    loop {
        let ev = c.next_event().await;
        let done = ev.is_terminal();
        events.push(ev);
        if done {
            return events;
        }
    }
}

#[tokio::test]
async fn hello_status_submit_and_stream_output() {
    let srv = TestServer::start(2, 3).await;
    let cwd = tempfile::tempdir().expect("cwd");
    let fp = fingerprint(cwd.path());

    let mut c = Client::connect(&srv.socket).await;
    c.hello().await;

    c.send(&ClientMessage::Status).await;
    match c.recv_timeout(Duration::from_secs(5)).await {
        Some(ServerMessage::Status(st)) => {
            assert!(st.capacity.accepting);
            assert_eq!(st.capacity.tokens_total, 3);
            assert_eq!(st.capacity.tokens_free, 3);
            assert_eq!(st.running, 0);
            assert_eq!(st.queued, 0);
        }
        other => panic!("expected Status, got {other:?}"),
    }

    c.send(&ClientMessage::Submit(request(
        cwd.path(),
        &["sh", "-c", "echo out; echo err 1>&2; exit 3"],
        fp,
    )))
    .await;
    let events = run_to_completion(&mut c).await;
    let id = events[0].id();
    assert!(
        matches!(events[0], JobEvent::Started { .. }),
        "first event must be Started: {events:?}"
    );
    let mut out = Vec::new();
    let mut err = Vec::new();
    for ev in &events[1..events.len() - 1] {
        assert_eq!(ev.id(), id);
        match ev {
            JobEvent::Stdout { bytes, .. } => out.extend_from_slice(bytes),
            JobEvent::Stderr { bytes, .. } => err.extend_from_slice(bytes),
            other => panic!("unexpected mid-stream event {other:?}"),
        }
    }
    assert_eq!(out, b"out\n");
    assert_eq!(err, b"err\n");
    assert_eq!(
        events.last(),
        Some(&JobEvent::Exited {
            id,
            status: ExitStatus::Code(3)
        })
    );

    // Status still works on a fresh connection after a job ran.
    let mut c2 = Client::connect(&srv.socket).await;
    c2.hello().await;
    c2.send(&ClientMessage::Status).await;
    match c2.recv_timeout(Duration::from_secs(5)).await {
        Some(ServerMessage::Status(st)) => {
            assert_eq!(st.running, 0);
            assert!(st.capacity.accepting);
        }
        other => panic!("expected Status, got {other:?}"),
    }
    srv.stop().await;
}

#[tokio::test]
async fn toolchain_mismatch_is_rejected() {
    let srv = TestServer::start(2, 2).await;
    let cwd = tempfile::tempdir().expect("cwd");
    let mut fp = fingerprint(cwd.path());
    fp.rustc_commit = "deadbeef0".into();

    let mut c = Client::connect(&srv.socket).await;
    c.hello().await;
    c.send(&ClientMessage::Submit(request(
        cwd.path(),
        &["true"],
        fp.clone(),
    )))
    .await;
    match c.next_event().await {
        JobEvent::Rejected {
            reason: RejectReason::ToolchainMismatch { server, client },
            ..
        } => {
            assert_eq!(client, fp);
            assert_ne!(server.rustc_commit, "deadbeef0");
        }
        other => panic!("expected ToolchainMismatch, got {other:?}"),
    }
    srv.stop().await;
}

#[tokio::test]
async fn bad_cwd_is_rejected_internal() {
    let srv = TestServer::start(2, 2).await;
    let cwd = tempfile::tempdir().expect("cwd");
    let fp = fingerprint(cwd.path());

    let mut c = Client::connect(&srv.socket).await;
    c.hello().await;
    let missing = cwd.path().join("does-not-exist");
    c.send(&ClientMessage::Submit(request(
        &missing,
        &["true"],
        fp.clone(),
    )))
    .await;
    assert!(matches!(
        c.next_event().await,
        JobEvent::Rejected {
            reason: RejectReason::Internal(_),
            ..
        }
    ));

    let mut c = Client::connect(&srv.socket).await;
    c.hello().await;
    c.send(&ClientMessage::Submit(request(
        Path::new("relative/dir"),
        &["true"],
        fp,
    )))
    .await;
    assert!(matches!(
        c.next_event().await,
        JobEvent::Rejected {
            reason: RejectReason::Internal(_),
            ..
        }
    ));
    srv.stop().await;
}

#[tokio::test]
async fn wrong_protocol_version_is_refused() {
    let srv = TestServer::start(1, 1).await;
    let mut c = Client::connect(&srv.socket).await;
    c.send(&ClientMessage::Hello {
        version: PROTOCOL_VERSION + 1,
    })
    .await;
    match c.recv_timeout(Duration::from_secs(5)).await {
        Some(ServerMessage::Error(e)) => assert_eq!(e.code, ProtoErrorCode::VersionMismatch),
        other => panic!("expected Error, got {other:?}"),
    }
    assert!(c.recv_timeout(Duration::from_secs(5)).await.is_none());

    // A non-Hello first message is also refused.
    let mut c = Client::connect(&srv.socket).await;
    c.send(&ClientMessage::Status).await;
    match c.recv_timeout(Duration::from_secs(5)).await {
        Some(ServerMessage::Error(e)) => assert_eq!(e.code, ProtoErrorCode::UnexpectedMessage),
        other => panic!("expected Error, got {other:?}"),
    }
    srv.stop().await;
}

#[tokio::test]
async fn second_job_queues_until_first_exits() {
    let srv = TestServer::start(1, 1).await;
    let cwd = tempfile::tempdir().expect("cwd");
    let fp = fingerprint(cwd.path());

    let mut a = Client::connect(&srv.socket).await;
    a.hello().await;
    a.send(&ClientMessage::Submit(request(
        cwd.path(),
        &["sleep", "2"],
        fp.clone(),
    )))
    .await;
    let a_started = a.next_event().await;
    assert!(matches!(a_started, JobEvent::Started { .. }));

    let mut b = Client::connect(&srv.socket).await;
    b.hello().await;
    b.send(&ClientMessage::Submit(request(
        cwd.path(),
        &["sleep", "2"],
        fp,
    )))
    .await;
    let queued = b.next_event().await;
    let b_id = match queued {
        JobEvent::Queued { id, position } => {
            assert_eq!(position, 1);
            id
        }
        other => panic!("expected Queued, got {other:?}"),
    };

    // Status from a third connection reports the queue.
    let mut s = Client::connect(&srv.socket).await;
    s.hello().await;
    s.send(&ClientMessage::Status).await;
    match s.recv_timeout(Duration::from_secs(5)).await {
        Some(ServerMessage::Status(st)) => {
            assert_eq!(st.running, 1);
            assert_eq!(st.queued, 1);
            assert!(!st.capacity.accepting);
            assert_eq!(st.capacity.tokens_free, 0);
        }
        other => panic!("expected Status, got {other:?}"),
    }

    let a_events = run_to_completion(&mut a).await;
    assert!(matches!(
        a_events.last(),
        Some(JobEvent::Exited {
            status: ExitStatus::Code(0),
            ..
        })
    ));
    let started_at = std::time::Instant::now();
    match b.next_event().await {
        JobEvent::Started { id } => assert_eq!(id, b_id),
        other => panic!("expected Started, got {other:?}"),
    }
    assert!(started_at.elapsed() < Duration::from_secs(4));
    let b_events = run_to_completion(&mut b).await;
    assert!(matches!(
        b_events.last(),
        Some(JobEvent::Exited {
            status: ExitStatus::Code(0),
            ..
        })
    ));
    srv.stop().await;
}

#[tokio::test]
async fn queue_limit_rejects_saturated() {
    let srv = TestServer::start(1, 1).await;
    let cwd = tempfile::tempdir().expect("cwd");
    let fp = fingerprint(cwd.path());

    let mut clients = Vec::new();
    // 1 running + 4 queued (queue_limit = 4 in TestServer::start).
    for i in 0..5 {
        let mut c = Client::connect(&srv.socket).await;
        c.hello().await;
        c.send(&ClientMessage::Submit(request(
            cwd.path(),
            &["sleep", "5"],
            fp.clone(),
        )))
        .await;
        let ev = c.next_event().await;
        if i == 0 {
            assert!(matches!(ev, JobEvent::Started { .. }));
        } else {
            assert!(matches!(ev, JobEvent::Queued { position, .. } if position == i));
        }
        clients.push(c);
    }
    let mut c = Client::connect(&srv.socket).await;
    c.hello().await;
    c.send(&ClientMessage::Submit(request(cwd.path(), &["true"], fp)))
        .await;
    assert!(matches!(
        c.next_event().await,
        JobEvent::Rejected {
            reason: RejectReason::Saturated,
            ..
        }
    ));
    // Dropping the clients cancels/dequeues their jobs; the server must still stop promptly.
    drop(clients);
    srv.stop().await;
}

#[tokio::test]
async fn cancel_terminates_running_job() {
    let srv = TestServer::start(1, 1).await;
    let cwd = tempfile::tempdir().expect("cwd");
    let fp = fingerprint(cwd.path());

    let mut c = Client::connect(&srv.socket).await;
    c.hello().await;
    c.send(&ClientMessage::Submit(request(
        cwd.path(),
        &["sleep", "10"],
        fp,
    )))
    .await;
    let id = match c.next_event().await {
        JobEvent::Started { id } => id,
        other => panic!("expected Started, got {other:?}"),
    };
    let t0 = std::time::Instant::now();
    c.send(&ClientMessage::Cancel { id }).await;
    let events = run_to_completion(&mut c).await;
    assert!(
        t0.elapsed() < Duration::from_secs(2),
        "cancel took too long"
    );
    match events.last() {
        Some(JobEvent::Exited { id: eid, status }) => {
            assert_eq!(*eid, id);
            assert!(
                matches!(status, ExitStatus::Signal(15) | ExitStatus::Code(_)),
                "unexpected status {status:?}"
            );
        }
        other => panic!("expected Exited, got {other:?}"),
    }
    srv.stop().await;
}

#[tokio::test]
async fn cancel_removes_queued_job() {
    let srv = TestServer::start(1, 1).await;
    let cwd = tempfile::tempdir().expect("cwd");
    let fp = fingerprint(cwd.path());

    let mut a = Client::connect(&srv.socket).await;
    a.hello().await;
    a.send(&ClientMessage::Submit(request(
        cwd.path(),
        &["sleep", "10"],
        fp.clone(),
    )))
    .await;
    assert!(matches!(a.next_event().await, JobEvent::Started { .. }));

    let mut b = Client::connect(&srv.socket).await;
    b.hello().await;
    b.send(&ClientMessage::Submit(request(cwd.path(), &["true"], fp)))
        .await;
    let id = match b.next_event().await {
        JobEvent::Queued { id, .. } => id,
        other => panic!("expected Queued, got {other:?}"),
    };
    b.send(&ClientMessage::Cancel { id }).await;
    assert!(matches!(
        b.next_event().await,
        JobEvent::Exited {
            status: ExitStatus::Signal(15),
            ..
        }
    ));
    drop(a);
    srv.stop().await;
}

#[tokio::test]
async fn cancel_unknown_job_is_a_protocol_error() {
    let srv = TestServer::start(1, 1).await;
    let mut c = Client::connect(&srv.socket).await;
    c.hello().await;
    c.send(&ClientMessage::Cancel { id: JobId(999) }).await;
    match c.recv_timeout(Duration::from_secs(5)).await {
        Some(ServerMessage::Error(e)) => assert_eq!(e.code, ProtoErrorCode::UnexpectedMessage),
        other => panic!("expected Error, got {other:?}"),
    }
    srv.stop().await;
}

#[tokio::test]
async fn disconnect_kills_running_job_and_frees_slot() {
    let srv = TestServer::start(1, 1).await;
    let cwd = tempfile::tempdir().expect("cwd");
    let fp = fingerprint(cwd.path());

    let mut a = Client::connect(&srv.socket).await;
    a.hello().await;
    a.send(&ClientMessage::Submit(request(
        cwd.path(),
        &["sleep", "10"],
        fp.clone(),
    )))
    .await;
    assert!(matches!(a.next_event().await, JobEvent::Started { .. }));
    drop(a);

    // The slot must free up quickly: a new job starts without queueing.
    let mut b = Client::connect(&srv.socket).await;
    b.hello().await;
    b.send(&ClientMessage::Submit(request(cwd.path(), &["true"], fp)))
        .await;
    let events = tokio::time::timeout(Duration::from_secs(3), run_to_completion(&mut b))
        .await
        .expect("job after disconnect completes");
    assert!(
        events.iter().all(|e| !matches!(e, JobEvent::Queued { .. })),
        "job must not queue after the disconnected job was killed: {events:?}"
    );
    srv.stop().await;
}

#[tokio::test]
async fn job_env_has_jobserver_and_shim_guard() {
    let srv = TestServer::start(1, 2).await;
    let cwd = tempfile::tempdir().expect("cwd");
    let fp = fingerprint(cwd.path());

    let mut c = Client::connect(&srv.socket).await;
    c.hello().await;
    let mut req = request(
        cwd.path(),
        &[
            "sh",
            "-c",
            "echo \"$MAKEFLAGS\"; echo \"$CARGO_MAKEFLAGS\"; echo \"$RUSTC_WRAPPER\"; echo \"$RBS_SHIM_ACTIVE\"; echo \"$MY_VAR\"; pwd",
        ],
        fp,
    );
    req.env.insert("MY_VAR".into(), "hello".into());
    c.send(&ClientMessage::Submit(req)).await;
    let events = run_to_completion(&mut c).await;
    let mut out = Vec::new();
    for ev in &events {
        if let JobEvent::Stdout { bytes, .. } = ev {
            out.extend_from_slice(bytes);
        }
    }
    let out = String::from_utf8(out).expect("utf8");
    let lines: Vec<&str> = out.lines().collect();
    assert_eq!(lines.len(), 6, "{out}");
    assert!(lines[0].contains("--jobserver-auth=fifo:"), "{out}");
    assert!(lines[0].ends_with("jobserver.fifo"), "{out}");
    assert_eq!(lines[1], lines[0]);
    assert_eq!(lines[2], "kache");
    assert_eq!(lines[3], "1");
    assert_eq!(lines[4], "hello");
    assert_eq!(
        Path::new(lines[5]).canonicalize().ok(),
        cwd.path().canonicalize().ok()
    );
    // The FIFO lives next to the socket.
    let fifo = srv.socket.parent().expect("parent").join("jobserver.fifo");
    assert!(fifo.exists());
    assert!(matches!(
        events.last(),
        Some(JobEvent::Exited {
            status: ExitStatus::Code(0),
            ..
        })
    ));
    srv.stop().await;
    assert!(!fifo.exists(), "fifo must be removed on exit");
}

#[tokio::test]
async fn job_timeout_kills_job() {
    let dir = tempfile::tempdir().expect("tempdir");
    let socket = dir.path().join("server.sock");
    let mut cfg = rbs_config::Config::default();
    cfg.server.min_mem_available_gib = 0;
    cfg.server.job_timeout_secs = 1;
    let (tx, rx) = oneshot::channel();
    let task = tokio::spawn(run_server_until(
        cfg,
        ServerOpts {
            socket: Some(socket.clone()),
            tokens: Some(1),
        },
        RunOptions {
            scope: ScopeMode::Disabled,
        },
        async move {
            let _ = rx.await;
        },
    ));
    for _ in 0..200 {
        if UnixStream::connect(&socket).await.is_ok() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    let cwd = tempfile::tempdir().expect("cwd");
    let fp = fingerprint(cwd.path());
    let mut c = Client::connect(&socket).await;
    c.hello().await;
    c.send(&ClientMessage::Submit(request(
        cwd.path(),
        &["sleep", "30"],
        fp,
    )))
    .await;
    let t0 = std::time::Instant::now();
    let events = run_to_completion(&mut c).await;
    assert!(t0.elapsed() < Duration::from_secs(8));
    assert!(matches!(
        events.last(),
        Some(JobEvent::Exited {
            status: ExitStatus::Signal(_),
            ..
        })
    ));
    let _ = tx.send(());
    task.await.expect("join").expect("clean exit");
}

#[tokio::test]
async fn proxy_bridges_stdio_to_socket() {
    // `run_proxy` uses the process's real stdin/stdout, so exercise the
    // underlying bridge helper directly with in-memory pipes.
    let srv = TestServer::start(1, 1).await;
    let (client_side, proxy_side) = tokio::io::duplex(4096);
    let (proxy_in, proxy_out) = tokio::io::split(proxy_side);
    let socket = srv.socket.clone();
    let bridge =
        tokio::spawn(
            async move { rbs_server::bridge_to_socket(&socket, proxy_in, proxy_out).await },
        );
    let (mut rd, mut wr) = tokio::io::split(client_side);
    wr.write_all(
        &encode(&ClientMessage::Hello {
            version: PROTOCOL_VERSION,
        })
        .expect("encode"),
    )
    .await
    .expect("write");
    let mut dec = Decoder::new();
    let msg = loop {
        if let Some(m) = dec.next_frame::<ServerMessage>().expect("decode") {
            break m;
        }
        let mut buf = [0u8; 1024];
        let n = tokio::time::timeout(Duration::from_secs(5), rd.read(&mut buf))
            .await
            .expect("timely")
            .expect("read");
        assert!(n > 0, "proxy closed early");
        dec.push(&buf[..n]).expect("push");
    };
    assert!(matches!(msg, ServerMessage::Hello { .. }));
    // Closing the client's write side (EOF on the proxy's input) ends the bridge.
    wr.shutdown().await.expect("shutdown");
    drop(wr);
    tokio::time::timeout(Duration::from_secs(5), bridge)
        .await
        .expect("bridge exits after EOF")
        .expect("join")
        .expect("bridge ok");
    srv.stop().await;
}
