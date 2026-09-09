//! `rbs status`: one line per backend with probe RTT and capacity.

use std::time::Duration;

use rbs_config::Config;

use crate::backend::Backend;
use crate::transport::{ProbeResult, Transport, probe_with_timeout};

#[derive(Debug, Clone, PartialEq)]
pub struct BackendLine {
    pub backend: Backend,
    pub target: String,
    pub result: Result<ProbeResult, String>,
}

pub fn format_line(line: &BackendLine) -> String {
    match &line.result {
        Ok(p) => {
            let c = &p.status.capacity;
            format!(
                "{:<7} {:<24} ok  rtt {:>4}ms  tokens {}/{}  mem {:.1}GiB  load {:.2}  queued {}  running {}  {}",
                line.backend,
                line.target,
                p.rtt.as_millis(),
                c.tokens_free,
                c.tokens_total,
                c.mem_available_bytes as f64 / (1u64 << 30) as f64,
                c.load1,
                p.status.queued,
                p.status.running,
                if c.accepting {
                    "accepting"
                } else {
                    "NOT accepting"
                },
            )
        }
        Err(e) => format!("{:<7} {:<24} unavailable: {e}", line.backend, line.target),
    }
}

pub async fn probe_backend(
    backend: Backend,
    transport: &dyn Transport,
    timeout: Duration,
) -> BackendLine {
    let target = transport.describe();
    let result = async {
        let mut conn = match tokio::time::timeout(timeout, transport.connect()).await {
            Ok(r) => r.map_err(|e| e.to_string())?,
            Err(_) => return Err(format!("connect timed out after {timeout:?}")),
        };
        probe_with_timeout(conn.as_mut(), timeout)
            .await
            .map_err(|e| e.to_string())
    }
    .await;
    BackendLine {
        backend,
        target,
        result,
    }
}

/// Probe every configured backend and print the table. Exit code 0 when at
/// least one backend answered.
pub async fn run(
    cfg: &Config,
    remote: Option<&dyn Transport>,
    local: Option<&dyn Transport>,
) -> i32 {
    let timeout = Duration::from_millis(cfg.remote.connect_timeout_ms);
    let mut lines = Vec::new();
    match remote {
        Some(t) if cfg.remote.enabled => {
            lines.push(probe_backend(Backend::Remote, t, timeout).await)
        }
        _ => lines.push(BackendLine {
            backend: Backend::Remote,
            target: cfg.remote.host.clone(),
            result: Err("disabled".into()),
        }),
    }
    match local {
        Some(t) if cfg.local_server.enabled => {
            lines.push(probe_backend(Backend::Local, t, timeout).await);
        }
        _ => lines.push(BackendLine {
            backend: Backend::Local,
            target: cfg.local_server.socket.display().to_string(),
            result: Err("disabled".into()),
        }),
    }
    lines.push(BackendLine {
        backend: Backend::Plain,
        target: "real cargo".into(),
        result: Err(match crate::real_cargo::find() {
            Ok(p) => format!("always available ({})", p.display()),
            Err(e) => e.to_string(),
        }),
    });
    let any_ok = lines.iter().any(|l| l.result.is_ok());
    for l in &lines {
        println!("{}", format_line(l));
    }
    if any_ok { 0 } else { 1 }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::transport::FakeTransport;
    use rbs_proto::{Capacity, PROTOCOL_VERSION, ServerIdentity, ServerMessage, ServerStatus};

    fn status() -> ServerStatus {
        ServerStatus {
            identity: ServerIdentity {
                hostname: "node0".into(),
                version: "0.1.0".into(),
            },
            capacity: Capacity {
                tokens_total: 30,
                tokens_free: 12,
                mem_available_bytes: 64 << 30,
                load1: 1.5,
                accepting: true,
            },
            queued: 1,
            running: 2,
            uptime_secs: 10,
        }
    }

    #[tokio::test]
    async fn probe_line_reports_capacity() {
        let t = FakeTransport::scripted([
            ServerMessage::Hello {
                version: PROTOCOL_VERSION,
                server: status().identity,
            },
            ServerMessage::Status(status()),
        ]);
        let line = probe_backend(Backend::Remote, &t, Duration::from_secs(1)).await;
        let s = format_line(&line);
        assert!(s.starts_with("remote  fake"), "{s}");
        assert!(s.contains("tokens 12/30"));
        assert!(s.contains("mem 64.0GiB"));
        assert!(s.contains("queued 1  running 2  accepting"));
    }

    #[tokio::test]
    async fn failure_line_carries_reason() {
        let t = FakeTransport::failing("connection refused");
        let line = probe_backend(Backend::Local, &t, Duration::from_secs(1)).await;
        let s = format_line(&line);
        assert!(s.starts_with("local   fake"), "{s}");
        assert!(s.contains("unavailable: connect to fake failed: connection refused"));
    }
}
