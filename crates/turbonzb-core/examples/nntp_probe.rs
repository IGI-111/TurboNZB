//! Raw NNTP throughput probe — measures how fast the server itself serves
//! articles to N bare connections (no engine, no queue, no GUI).
//!
//! Reads the first server from the real config (`~/.config/turbonzb/config.json`
//! or a path given as first arg) and message ids from the queue DB, then
//! runs a `BODY` fetch loop for `--seconds` on `--conns` connections and
//! prints achieved MB/s. This isolates server-side delivery from the whole
//! application stack.
//!
//! Usage:
//! ```text
//! cargo run -p turbonzb-core --example nntp_probe -- --conns 8 --seconds 10
//! cargo run -p turbonzb-core --example nntp_probe -- --config /path/to/config.json
//! ```

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Instant;

use tokio::sync::Mutex;
use tokio::task::JoinSet;

use turbonzb_core::nntp::{NntpClient, ServerConfig};
use turbonzb_core::queue::QueueManager;

fn default_config_path() -> PathBuf {
    let home = std::env::var("HOME").unwrap_or_else(|_| "/".into());
    PathBuf::from(home)
        .join(".config")
        .join("turbonzb")
        .join("config.json")
}

#[derive(serde::Deserialize)]
struct ProbeConfig {
    #[serde(default)]
    servers: Vec<ProbeServer>,
}

#[derive(serde::Deserialize)]
struct ProbeServer {
    host: String,
    port: u16,
    #[serde(default)]
    tls: bool,
    user: Option<String>,
    password: Option<String>,
    #[serde(default = "default_max_connections")]
    max_connections: u32,
    #[serde(default)]
    priority: u32,
}

fn default_max_connections() -> u32 {
    50
}

impl From<ProbeServer> for ServerConfig {
    fn from(s: ProbeServer) -> Self {
        Self {
            host: s.host,
            port: s.port,
            tls: s.tls,
            user: s.user,
            password: s.password,
            max_connections: s.max_connections,
            priority: s.priority,
        }
    }
}

async fn grab_message_ids(db: &str, limit: usize) -> Vec<String> {
    let queue = QueueManager::open(db).await.unwrap();
    let jobs = queue.list_jobs().await.unwrap();
    let job = jobs
        .first()
        .expect("no jobs in queue — queue a download first");
    let files = queue.list_files(job.id).await.unwrap();
    let file = files.first().expect("no files in first job");
    queue
        .list_segments(file.id)
        .await
        .unwrap()
        .into_iter()
        .take(limit)
        .map(|s| s.message_id)
        .collect()
}

async fn run(server: ServerConfig, ids: Arc<std::vec::Vec<String>>, conns: usize, seconds: u64) {
    let shared: Arc<Mutex<std::collections::VecDeque<String>>> =
        Arc::new(Mutex::new(ids.iter().cloned().collect()));

    // Phase 1: establish all connections so Eweka's connection-setup
    // throttling doesn't pollute the transfer measurement. Each task
    // connects and waits at a barrier-ish gate; timing begins after all
    // are up.
    let (ready_tx, mut ready_rx) = tokio::sync::mpsc::channel::<()>(conns);
    let start_gate: Arc<tokio::sync::Notify> = Arc::new(tokio::sync::Notify::new());
    let mut set: JoinSet<(u64, u64)> = JoinSet::new();
    for _ in 0..conns {
        let ids = Arc::clone(&ids);
        let shared = Arc::clone(&shared);
        let server = server.clone();
        let ready_tx = ready_tx.clone();
        let gate = Arc::clone(&start_gate);
        set.spawn(async move {
            let mut client = NntpClient::connect(&server).await.expect("connect failed");
            let _ = ready_tx.send(()).await;
            gate.notified().await; // wait until all conns are up
            let mut bytes: u64 = 0;
            let mut fetch_us: u64 = 0;
            let start = Instant::now();
            loop {
                if start.elapsed().as_secs() >= seconds {
                    break;
                }
                let mid = {
                    let mut q = shared.lock().await;
                    match q.pop_front() {
                        Some(m) => m,
                        None => {
                            q.extend(ids.iter().cloned());
                            q.pop_front().unwrap()
                        }
                    }
                };
                let t = Instant::now();
                match client.body(&mid).await {
                    Ok(Ok(b)) => {
                        bytes += b.bytes.len() as u64;
                    }
                    Ok(Err(_)) => {}
                    Err(e) => {
                        eprintln!("E conn error: {e}; reconnecting");
                        client = NntpClient::connect(&server)
                            .await
                            .expect("reconnect failed");
                    }
                }
                fetch_us += t.elapsed().as_micros() as u64;
            }
            (bytes, fetch_us)
        });
    }

    // Wait for all connections to be established.
    let setup_host = std::time::Instant::now();
    for _ in 0..conns {
        ready_rx.recv().await;
    }
    let setup_s = setup_host.elapsed().as_secs_f32();
    let start = Instant::now();
    start_gate.notify_waiters();
    // Start the timed loop...
    drop(start_gate);
    let _ = start;

    let mut total_bytes = 0u64;
    let mut total_fetch_us = 0u64;
    while let Some(res) = set.join_next().await {
        let (b, f) = res.unwrap_or((0, 0));
        total_bytes += b;
        total_fetch_us += f;
    }
    let wall = start.elapsed().as_secs_f64();
    let mbs = total_bytes as f64 / 1024.0 / 1024.0 / wall;
    let per_conn = mbs / conns as f64;
    eprintln!(
        "PROBE conns={conns} setup={setup_s:.1}s transfer={wall:.1}s totalMB={:.1} MB/s={:.2} per_conn={:.3}MB/s avg_fetch={:.0}ms",
        total_bytes as f64 / 1024.0 / 1024.0,
        mbs,
        per_conn,
        total_fetch_us as f64 / (std::cmp::max(total_bytes, 1) as f64),
    );
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let mut conns = 8usize;
    let mut seconds = 10u64;
    let mut config_path: Option<PathBuf> = None;
    let mut i = 1;
    while i < args.len() {
        match args[i].as_str() {
            "--conns" => {
                conns = args[i + 1].parse().unwrap();
                i += 2;
            }
            "--seconds" => {
                seconds = args[i + 1].parse().unwrap();
                i += 2;
            }
            "--config" => {
                config_path = Some(args[i + 1].parse().unwrap());
                i += 2;
            }
            _ => {
                eprintln!("usage: nntp_probe [--conns N] [--seconds S] [--config PATH]");
                std::process::exit(2);
            }
        }
    }

    let path = config_path.unwrap_or_else(default_config_path);
    eprintln!("reading config: {}", path.display());
    let raw = std::fs::read_to_string(&path).expect("read config");
    let cfg: ProbeConfig = serde_json::from_str(&raw).expect("parse config");
    let server = ServerConfig::from(cfg.servers.into_iter().next().expect("no servers"));

    let db = {
        let raw = std::fs::read_to_string(&path).unwrap();
        let v: serde_json::Value = serde_json::from_str(&raw).unwrap();
        v["db_path"].as_str().unwrap().to_string()
    };
    eprintln!("grabbing message ids from: {db}");
    let rt = tokio::runtime::Runtime::new().unwrap();
    let ids = Arc::new(rt.block_on(async move { grab_message_ids(&db, conns * 4).await }));
    eprintln!("got {} message ids", ids.len());

    rt.block_on(run(server, ids, conns, seconds));
}
