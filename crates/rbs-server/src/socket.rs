//! Unix socket listener, per-connection protocol state machine and the
//! server-wide shared state that ties scheduler, runner and jobserver together.

use std::collections::HashMap;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Instant;

use rbs_proto::{
    Capacity, ClientMessage, Decoder, ExitStatus, JobEvent, JobId, JobRequest, PROTOCOL_VERSION,
    ProtoError, ProtoErrorCode, ServerIdentity, ServerMessage, ServerStatus, encode,
};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{UnixListener, UnixStream, unix::OwnedReadHalf, unix::OwnedWriteHalf};
use tokio::sync::{mpsc, oneshot};
use tokio_util::task::TaskTracker;
use tracing::{debug, info, warn};

use crate::error::ServerError;
use crate::jobserver::JobServer;
use crate::runner::{CancelToken, Runner};
use crate::scheduler::{Decision, RejectCause, Scheduler};
use crate::sysinfo::{self, Snapshot};

/// Event channel depth per job; the runner blocks on a full channel, which
/// back-pressures the child's output pipes.
const EVENT_CHANNEL: usize = 256;

/// Server-wide state shared by all connections.
pub struct Shared {
    pub identity: ServerIdentity,
    pub started: Instant,
    pub scheduler: Mutex<Scheduler>,
    pub runner: Runner,
    pub jobserver: JobServer,
    /// Queued jobs waiting to be released by the scheduler.
    waiters: Mutex<HashMap<JobId, oneshot::Sender<()>>>,
    next_id: AtomicU64,
    /// Tracks job completion tasks so shutdown can drain them.
    pub jobs: TaskTracker,
}

impl Shared {
    pub fn new(
        identity: ServerIdentity,
        scheduler: Scheduler,
        runner: Runner,
        jobserver: JobServer,
    ) -> Shared {
        Shared {
            identity,
            started: Instant::now(),
            scheduler: Mutex::new(scheduler),
            runner,
            jobserver,
            waiters: Mutex::new(HashMap::new()),
            next_id: AtomicU64::new(1),
            jobs: TaskTracker::new(),
        }
    }

    fn next_id(&self) -> JobId {
        JobId(self.next_id.fetch_add(1, Ordering::Relaxed))
    }

    fn lock_scheduler(&self) -> std::sync::MutexGuard<'_, Scheduler> {
        self.scheduler
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    fn lock_waiters(&self) -> std::sync::MutexGuard<'_, HashMap<JobId, oneshot::Sender<()>>> {
        self.waiters
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    pub async fn status(&self) -> ServerStatus {
        let snap = sysinfo::snapshot().await;
        let (running, queued, accepting) = {
            let s = self.lock_scheduler();
            (s.running(), s.queued(), s.accepting(&snap))
        };
        ServerStatus {
            identity: self.identity.clone(),
            capacity: Capacity {
                tokens_total: self.jobserver.tokens_total(),
                tokens_free: self.jobserver.tokens_free(running),
                mem_available_bytes: snap.mem_available_bytes,
                load1: snap.load1,
                accepting,
            },
            queued,
            running,
            uptime_secs: self.started.elapsed().as_secs(),
        }
    }

    /// Wake queued jobs the scheduler released. A released job whose
    /// connection vanished is immediately finished again.
    fn wake(&self, mut released: Vec<JobId>, snap: &Snapshot) {
        while !released.is_empty() {
            let mut orphaned = Vec::new();
            {
                let mut waiters = self.lock_waiters();
                for id in released.drain(..) {
                    let delivered = waiters.remove(&id).is_some_and(|tx| tx.send(()).is_ok());
                    if !delivered {
                        orphaned.push(id);
                    }
                }
            }
            for id in orphaned {
                debug!(job = id.0, "released job has no waiter; releasing slot");
                released.extend(self.lock_scheduler().finish(snap));
            }
        }
    }

    /// A running job ended (or failed to start): free its slot.
    pub async fn job_finished(&self, id: JobId) {
        let snap = sysinfo::snapshot().await;
        let released = self.lock_scheduler().finish(&snap);
        debug!(job = id.0, released = ?released, "slot released");
        self.wake(released, &snap);
    }

    /// Periodic re-evaluation (memory may have freed up without a job ending).
    pub async fn poll(&self) {
        let snap = sysinfo::snapshot().await;
        let released = self.lock_scheduler().poll(&snap);
        if !released.is_empty() {
            debug!(released = ?released, "poll released queued jobs");
        }
        self.wake(released, &snap);
    }

    /// Spawn the job and register its completion with the scheduler.
    /// Returns the event receiver and the cancel token, or a reject reason
    /// (in which case the slot has already been released).
    async fn launch(
        self: &Arc<Self>,
        id: JobId,
        req: &JobRequest,
    ) -> Result<(mpsc::Receiver<JobEvent>, CancelToken), Box<rbs_proto::RejectReason>> {
        let (tx, rx) = mpsc::channel(EVENT_CHANNEL);
        match self.runner.spawn(id, req, tx) {
            Ok(handle) => {
                let (cancel, task) = handle.into_parts();
                let shared = Arc::clone(self);
                self.jobs.spawn(async move {
                    if let Err(e) = task.await {
                        warn!(job = id.0, error = %e, "job supervisor task failed");
                    }
                    shared.job_finished(id).await;
                });
                Ok((rx, cancel))
            }
            Err(reason) => {
                warn!(job = id.0, reason = ?reason, "job failed to start");
                self.job_finished(id).await;
                Err(reason)
            }
        }
    }
}

/// Bind the listener: create the parent dir, remove a stale socket, chmod 0600.
pub fn bind(path: &Path) -> Result<UnixListener, ServerError> {
    let dir = path
        .parent()
        .ok_or_else(|| ServerError::NoParent(path.to_path_buf()))?;
    std::fs::create_dir_all(dir).map_err(|source| ServerError::CreateDir {
        path: dir.to_path_buf(),
        source,
    })?;
    match std::fs::remove_file(path) {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(source) => {
            return Err(ServerError::RemoveStale {
                path: path.to_path_buf(),
                source,
            });
        }
    }
    let listener = UnixListener::bind(path).map_err(|source| ServerError::Bind {
        path: path.to_path_buf(),
        source,
    })?;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600)).map_err(|source| {
        ServerError::Chmod {
            path: path.to_path_buf(),
            source,
        }
    })?;
    Ok(listener)
}

struct Reader {
    rd: OwnedReadHalf,
    dec: Decoder,
    buf: Vec<u8>,
}

impl Reader {
    /// Next frame, or `None` at EOF.
    async fn next(&mut self) -> Result<Option<ClientMessage>, ServerError> {
        loop {
            if let Some(m) = self.dec.next_frame::<ClientMessage>()? {
                return Ok(Some(m));
            }
            let n = self.rd.read(&mut self.buf).await?;
            if n == 0 {
                return Ok(None);
            }
            self.dec.push(&self.buf[..n])?;
        }
    }
}

async fn write_msg(wr: &mut OwnedWriteHalf, msg: &ServerMessage) -> Result<(), ServerError> {
    wr.write_all(&encode(msg)?).await?;
    Ok(())
}

async fn write_error(
    wr: &mut OwnedWriteHalf,
    code: ProtoErrorCode,
    message: impl Into<String>,
) -> Result<(), ServerError> {
    write_msg(
        wr,
        &ServerMessage::Error(ProtoError {
            code,
            message: message.into(),
        }),
    )
    .await
}

/// The connection's job, if any.
enum Active {
    None,
    Queued {
        id: JobId,
        req: Box<JobRequest>,
        released: oneshot::Receiver<()>,
    },
    Running {
        id: JobId,
        events: mpsc::Receiver<JobEvent>,
        cancel: CancelToken,
    },
}

impl Active {
    fn id(&self) -> Option<JobId> {
        match self {
            Active::None => None,
            Active::Queued { id, .. } | Active::Running { id, .. } => Some(*id),
        }
    }
}

struct Connection {
    shared: Arc<Shared>,
    rd: Reader,
    wr: OwnedWriteHalf,
    active: Active,
}

impl Connection {
    async fn handshake(&mut self) -> Result<bool, ServerError> {
        match self.rd.next().await? {
            Some(ClientMessage::Hello { version }) if version == PROTOCOL_VERSION => {
                write_msg(
                    &mut self.wr,
                    &ServerMessage::Hello {
                        version: PROTOCOL_VERSION,
                        server: self.shared.identity.clone(),
                    },
                )
                .await?;
                Ok(true)
            }
            Some(ClientMessage::Hello { version }) => {
                warn!(
                    client_version = version,
                    server_version = PROTOCOL_VERSION,
                    "protocol version mismatch"
                );
                write_error(
                    &mut self.wr,
                    ProtoErrorCode::VersionMismatch,
                    format!("server speaks protocol {PROTOCOL_VERSION}, client sent {version}"),
                )
                .await?;
                Ok(false)
            }
            Some(_) => {
                write_error(
                    &mut self.wr,
                    ProtoErrorCode::UnexpectedMessage,
                    "first message must be hello",
                )
                .await?;
                Ok(false)
            }
            None => Ok(false),
        }
    }

    async fn send_event(&mut self, ev: JobEvent) -> Result<(), ServerError> {
        debug!(job = ev.id().0, event = ?ev_kind(&ev), "event");
        write_msg(&mut self.wr, &ServerMessage::Event(ev)).await
    }

    async fn handle_submit(&mut self, req: JobRequest) -> Result<(), ServerError> {
        if self.active.id().is_some() {
            return write_error(
                &mut self.wr,
                ProtoErrorCode::UnexpectedMessage,
                "a job is already active on this connection",
            )
            .await;
        }
        let id = self.shared.next_id();
        if let Err(reason) = Runner::validate_cwd(&req.cwd) {
            warn!(job = id.0, reason = ?reason, "rejecting job");
            return self
                .send_event(JobEvent::Rejected {
                    id,
                    reason: *reason,
                })
                .await;
        }
        if let Err(reason) = self.shared.runner.check_toolchain(&req).await {
            return self
                .send_event(JobEvent::Rejected {
                    id,
                    reason: *reason,
                })
                .await;
        }
        let snap = sysinfo::snapshot().await;
        let decision = self.shared.lock_scheduler().admit(id, req.priority, &snap);
        match decision {
            Decision::Accept => {
                info!(job = id.0, priority = ?req.priority, client = %req.client.hostname, "job accepted");
                self.start(id, &req).await
            }
            Decision::Queue { position } => {
                info!(job = id.0, position, priority = ?req.priority, "job queued");
                let (tx, rx) = oneshot::channel();
                self.shared.lock_waiters().insert(id, tx);
                self.active = Active::Queued {
                    id,
                    req: Box::new(req),
                    released: rx,
                };
                self.send_event(JobEvent::Queued { id, position }).await
            }
            Decision::Reject(RejectCause::Saturated) => {
                warn!(job = id.0, "rejecting job: saturated");
                self.send_event(JobEvent::Rejected {
                    id,
                    reason: rbs_proto::RejectReason::Saturated,
                })
                .await
            }
        }
    }

    /// The scheduler has counted `id` as running; spawn it.
    async fn start(&mut self, id: JobId, req: &JobRequest) -> Result<(), ServerError> {
        match self.shared.launch(id, req).await {
            Ok((events, cancel)) => {
                self.active = Active::Running { id, events, cancel };
                Ok(())
            }
            Err(reason) => {
                self.active = Active::None;
                self.send_event(JobEvent::Rejected {
                    id,
                    reason: *reason,
                })
                .await
            }
        }
    }

    async fn handle_cancel(&mut self, id: JobId) -> Result<(), ServerError> {
        match std::mem::replace(&mut self.active, Active::None) {
            Active::Running {
                id: active,
                events,
                mut cancel,
            } if active == id => {
                cancel.cancel();
                self.active = Active::Running { id, events, cancel };
                Ok(())
            }
            Active::Queued {
                id: active,
                req,
                released,
            } if active == id => {
                self.dequeue(id, released).await;
                drop(req);
                self.send_event(JobEvent::Exited {
                    id,
                    status: ExitStatus::Signal(15),
                })
                .await
            }
            other => {
                self.active = other;
                write_error(
                    &mut self.wr,
                    ProtoErrorCode::UnexpectedMessage,
                    format!("job {} is not active on this connection", id.0),
                )
                .await
            }
        }
    }

    /// Remove a queued job; if it was released concurrently, give the slot back.
    async fn dequeue(&self, id: JobId, mut released: oneshot::Receiver<()>) {
        let was_queued = self.shared.lock_scheduler().remove_queued(id);
        self.shared.lock_waiters().remove(&id);
        if !was_queued && released.try_recv().is_ok() {
            self.shared.job_finished(id).await;
        }
        info!(job = id.0, "queued job withdrawn");
    }

    async fn run(&mut self) -> Result<(), ServerError> {
        if !self.handshake().await? {
            return Ok(());
        }
        loop {
            match &mut self.active {
                Active::None => match self.rd.next().await? {
                    None => return Ok(()),
                    Some(msg) => self.handle_msg(msg).await?,
                },
                Active::Queued { released, .. } => {
                    tokio::select! {
                        r = released => {
                            let Active::Queued { id, req, .. } =
                                std::mem::replace(&mut self.active, Active::None)
                            else {
                                unreachable!("active state changed under select");
                            };
                            if r.is_ok() {
                                info!(job = id.0, "queued job released");
                                self.start(id, &req).await?;
                            } else {
                                self.send_event(JobEvent::Rejected {
                                    id,
                                    reason: rbs_proto::RejectReason::Internal("scheduler dropped job".into()),
                                }).await?;
                            }
                        }
                        m = self.rd.next() => match m? {
                            None => return Ok(()),
                            Some(msg) => self.handle_msg(msg).await?,
                        },
                    }
                }
                Active::Running { events, .. } => {
                    tokio::select! {
                        ev = events.recv() => match ev {
                            Some(ev) => {
                                let terminal = ev.is_terminal();
                                self.send_event(ev).await?;
                                if terminal {
                                    self.active = Active::None;
                                }
                            }
                            None => {
                                warn!("job event channel closed without terminal event");
                                self.active = Active::None;
                            }
                        },
                        m = self.rd.next() => match m? {
                            None => return Ok(()),
                            Some(msg) => self.handle_msg(msg).await?,
                        },
                    }
                }
            }
        }
    }

    async fn handle_msg(&mut self, msg: ClientMessage) -> Result<(), ServerError> {
        match msg {
            ClientMessage::Hello { .. } => {
                write_error(
                    &mut self.wr,
                    ProtoErrorCode::UnexpectedMessage,
                    "duplicate hello",
                )
                .await
            }
            ClientMessage::Status => {
                let st = self.shared.status().await;
                write_msg(&mut self.wr, &ServerMessage::Status(st)).await
            }
            ClientMessage::Submit(req) => self.handle_submit(req).await,
            ClientMessage::Cancel { id } => self.handle_cancel(id).await,
        }
    }

    /// Connection is going away: withdraw a queued job or cancel a running one.
    async fn cleanup(&mut self) {
        match std::mem::replace(&mut self.active, Active::None) {
            Active::None => {}
            Active::Queued { id, released, .. } => self.dequeue(id, released).await,
            Active::Running { id, cancel, .. } => {
                info!(job = id.0, "client disconnected; cancelling job");
                drop(cancel);
            }
        }
    }
}

fn ev_kind(ev: &JobEvent) -> &'static str {
    match ev {
        JobEvent::Queued { .. } => "queued",
        JobEvent::Started { .. } => "started",
        JobEvent::Stdout { .. } => "stdout",
        JobEvent::Stderr { .. } => "stderr",
        JobEvent::Exited { .. } => "exited",
        JobEvent::Rejected { .. } => "rejected",
    }
}

/// Serve one client connection to completion.
pub async fn handle_connection(shared: Arc<Shared>, stream: UnixStream) {
    let (rd, wr) = stream.into_split();
    let mut conn = Connection {
        shared,
        rd: Reader {
            rd,
            dec: Decoder::new(),
            buf: vec![0u8; 64 * 1024],
        },
        wr,
        active: Active::None,
    };
    let result = conn.run().await;
    conn.cleanup().await;
    match result {
        Ok(()) => debug!("connection closed"),
        Err(e) => debug!(error = %e, "connection ended with error"),
    }
}
