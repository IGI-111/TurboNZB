//! Logging hygiene (§12.4 of TEST_PLAN.md).
//!
//! Two properties the queue tab depends on:
//!   - a failure ends up in the log with the job it belongs to and a reason,
//!     so a support request can be answered from the log file alone;
//!   - credentials do not leak into that log at any level.
//!
//! Logs are captured in-process with a `tracing_subscriber` layer writing
//! into a buffer, on a current-thread runtime so the thread-scoped subscriber
//! is visible to everything the engine spawns.

use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use crc32fast::Hasher;
use tokio::sync::mpsc;
use tracing_subscriber::prelude::*;
use turbonzb_core::engine::Engine;
use turbonzb_core::nntp::ServerConfig;
use turbonzb_core::nzb::{self, Nzb};
use turbonzb_core::queue::{JobState, QueueManager};

mod common;

use common::MockServer;

// ------------------------------------------------------------ log capture ---

#[derive(Clone)]
struct Buffer(Arc<Mutex<Vec<u8>>>);

impl Buffer {
    fn new() -> Self {
        Self(Arc::new(Mutex::new(Vec::new())))
    }

    fn text(&self) -> String {
        let guard = self.0.lock().unwrap_or_else(|e| e.into_inner());
        String::from_utf8_lossy(&guard).into_owned()
    }
}

impl std::io::Write for Buffer {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .extend_from_slice(buf);
        Ok(buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

/// Install a capturing subscriber for the duration of `body`.
fn with_capture<R>(body: impl FnOnce(Buffer) -> R) -> R {
    let buffer = Buffer::new();
    let sink = buffer.clone();
    let layer = tracing_subscriber::fmt::layer()
        .with_writer(move || sink.clone())
        .with_ansi(false)
        .without_time();
    let subscriber = tracing_subscriber::Registry::default().with(layer);
    let _guard = tracing::subscriber::set_default(subscriber);
    body(buffer)
}

fn runtime() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("tokio runtime")
}

// ----------------------------------------------------------------- fixtures ---

fn yenc_article(payload: &[u8], name: &str) -> Vec<u8> {
    let total = payload.len() as u64;
    let mut body: Vec<u8> = Vec::new();
    let mut crc = Hasher::new();
    for &b in payload {
        crc.update(&[b]);
        let enc = b.wrapping_add(42);
        if matches!(enc, b'=' | b'\r' | b'\n' | 0) {
            body.push(b'=');
            body.push(enc.wrapping_add(64));
        } else {
            body.push(enc);
        }
    }
    let mut out = format!("=ybegin line=128 size={total} name={name}\r\n").into_bytes();
    out.extend_from_slice(&body);
    out.extend_from_slice(b"\r\n");
    out.extend_from_slice(
        format!(
            "=yend size={} crc32={:08x}\r\n",
            payload.len(),
            crc.finalize()
        )
        .as_bytes(),
    );
    common::dot_stuff(&out)
}

fn build_nzb(segments: &[(u32, &str)], name: &str, password: Option<&str>) -> Nzb {
    let mut xml = String::from("<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n<nzb>");
    xml.push_str("<head>");
    xml.push_str(&format!("<meta type=\"title\">{name}</meta>"));
    if let Some(pw) = password {
        xml.push_str(&format!("<meta type=\"password\">{pw}</meta>"));
    }
    xml.push_str("</head>");
    xml.push_str(&format!(
        "<file poster=\"t@t\" date=\"1\" subject=\"&quot;{name}.bin&quot;\"><groups><group>alt.binaries.test</group></groups><segments>"
    ));
    for (num, mid) in segments {
        xml.push_str(&format!(
            "<segment bytes=\"100\" number=\"{num}\">{mid}</segment>"
        ));
    }
    xml.push_str("</segments></file></nzb>");
    nzb::parse(xml.as_bytes()).unwrap()
}

fn temp_dir(tag: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("turbonzb-logging-{}-{tag}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    d
}

fn server_cfg(addr: std::net::SocketAddr) -> ServerConfig {
    let mut c = ServerConfig::localhost();
    c.port = addr.port();
    c.max_connections = 2;
    c
}

// ------------------------------------------------------------------- tests ---

/// §12.4 — a job that runs leaves an identifiable trace: the job id, the
/// directory it wrote to, and the completion line. Someone reading only the
/// log must be able to tell which job succeeded.
#[test]
fn successful_job_is_attributable_in_the_log() {
    with_capture(|log| {
        runtime().block_on(async {
            let payload = b"logged-segment-payload";
            let server = MockServer::new().add_article("a@x", yenc_article(payload, "logged.bin"));
            let addr = server.spawn().await;

            let nzb = build_nzb(&[(1, "a@x")], "logged", None);
            let dir = temp_dir("ok");
            let queue = Arc::new(QueueManager::open_in_memory().await.unwrap());
            let job_id = queue.add_job(&nzb, &dir, 0, None).await.unwrap();

            let engine = Arc::new(Engine::new(vec![server_cfg(addr)], 2));
            let (tx, _rx) = mpsc::unbounded_channel();
            engine
                .run_job(Arc::clone(&queue), job_id, tx)
                .await
                .expect("engine run");

            let job = queue.get_job(job_id).await.unwrap();
            assert_eq!(job.state, JobState::Complete, "sanity: download worked");

            let text = log.text();
            assert!(
                text.contains("engine: starting job"),
                "the job start is logged, got: {text}"
            );
            assert!(
                text.contains(&format!("job_id={}", job.id)),
                "log lines carry the job id, got: {text}"
            );
            assert!(
                text.contains("turbonzb-logging-"),
                "the write target is identifiable from the log: {text}"
            );
            assert!(
                text.contains("engine: all workers finished"),
                "the run reaches an end state in the log: {text}"
            );
        })
    });
}

/// §12.4 — a connection that never comes up is reported with its reason, not
/// silently swallowed. This is the failure the user sees as "nothing happens".
#[test]
fn connect_failure_is_logged_with_reason() {
    with_capture(|log| {
        runtime().block_on(async {
            // Grab a port, then release it: nothing will be listening.
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
                .await
                .expect("bind");
            let addr = listener.local_addr().unwrap();
            drop(listener);

            let nzb = build_nzb(&[(1, "gone@x")], "offline", None);
            let dir = temp_dir("offline");
            let queue = Arc::new(QueueManager::open_in_memory().await.unwrap());
            let job_id = queue.add_job(&nzb, &dir, 0, None).await.unwrap();

            let engine = Arc::new(Engine::new(vec![server_cfg(addr)], 1));
            let (tx, _rx) = mpsc::unbounded_channel();
            let _ = engine.run_job(Arc::clone(&queue), job_id, tx).await;

            let text = log.text();
            assert!(
                text.contains("connect failed"),
                "the connect failure is logged, got: {text}"
            );
            assert!(
                text.contains(&format!("job_id={}", job_id)),
                "the failure is attributable to the job: {text}"
            );
        })
    });
}

/// §12.4 — credentials stay out of the log. The archive password is carried
/// on the job and used when unpacking; none of those paths may print it, and
/// the same goes for server credentials sitting in the engine config.
#[test]
fn credentials_are_not_echoed_into_the_log() {
    with_capture(|log| {
        runtime().block_on(async {
            let payload = b"pw-segment-payload";
            let server = MockServer::new().add_article("p@x", yenc_article(payload, "pw.bin"));
            let addr = server.spawn().await;

            let nzb = build_nzb(&[(1, "p@x")], "pw", Some("hunter2"));
            let dir = temp_dir("pw");
            let queue = Arc::new(QueueManager::open_in_memory().await.unwrap());
            let job_id = queue
                .add_job(&nzb, &dir, 0, Some("pw-holder"))
                .await
                .unwrap();
            queue
                .set_job_archive_password(job_id, Some("hunter2"))
                .await
                .expect("password sticks to the job");
            assert_eq!(
                queue
                    .get_job(job_id)
                    .await
                    .unwrap()
                    .archive_password
                    .as_deref(),
                Some("hunter2"),
                "the password is stored on the job for unpacking"
            );

            let engine = Arc::new(Engine::new(vec![server_cfg(addr)], 2));
            let (tx, _rx) = mpsc::unbounded_channel();
            engine
                .run_job(Arc::clone(&queue), job_id, tx)
                .await
                .expect("engine run");

            let text = log.text();
            assert!(
                !text.contains("hunter2"),
                "the archive password must not be echoed: {text}"
            );
            assert!(
                text.contains(&format!("job_id={job_id}")),
                "the job is still identifiable without its credentials: {text}"
            );
        })
    });
}
