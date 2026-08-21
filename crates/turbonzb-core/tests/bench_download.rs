//! Local high-bandwidth download benchmark: a fake NNTP server on loopback
//! that serves a large pre-generated yEnc article at full socket speed.
//!
//! Run with:
//!
//! ```text
//! cargo test --release -p turbonzb-core --test bench_download -- --ignored --nocapture
//! ```
//!
//! The output shows achieved MB/s at 8 and 50 connections. If 50 conns ≈ 8
//! conns here, the client is the bottleneck (server is local and unlimited).

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Instant;

use crc32fast::Hasher;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::TcpListener;
use tokio::sync::mpsc;
use turbonzb_core::engine::{Engine, ProgressEvent};
use turbonzb_core::nntp::ServerConfig;
use turbonzb_core::nzb::{self, Nzb};
use turbonzb_core::queue::QueueManager;

const SEG_SIZE: usize = 700_000;
const SEGMENTS: usize = 2000;

fn make_yenc_body() -> Vec<u8> {
    let payload: Vec<u8> = (0..SEG_SIZE).map(|i| (i % 256) as u8).collect();
    let mut out: Vec<u8> = Vec::with_capacity(payload.len() * 2);
    out.extend_from_slice(
        format!("=ybegin line=128 size={} name=bench.bin\r\n", payload.len()).as_bytes(),
    );
    let mut crc = Hasher::new();
    let mut body: Vec<u8> = Vec::with_capacity(payload.len() + 16);
    for &b in &payload {
        crc.update(&[b]);
        let enc = b.wrapping_add(42);
        if enc == b'=' || enc == b'\r' || enc == b'\n' || enc == b'\0' {
            body.push(b'=');
            body.push(enc.wrapping_add(64));
        } else {
            body.push(enc);
        }
        if body.len() % 128 == 127 {
            body.push(b'\r');
            body.push(b'\n');
        }
    }
    out.extend_from_slice(&body);
    out.extend_from_slice(b"\r\n");
    let crc_val = crc.finalize();
    out.extend_from_slice(
        format!("=yend size={} crc32={:08x}\r\n", payload.len(), crc_val).as_bytes(),
    );
    out
}

async fn spawn_bench_server(body: Arc<Vec<u8>>) -> std::net::SocketAddr {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        loop {
            let (sock, _) = match listener.accept().await {
                Ok(s) => s,
                Err(_) => return,
            };
            let body = body.clone();
            tokio::spawn(async move {
                let (reader, mut writer) = tokio::io::split(sock);
                let mut reader = BufReader::new(reader);
                if writer
                    .write_all(b"200 turbonzb-bench ready\r\n")
                    .await
                    .is_err()
                {
                    return;
                }
                let mut line = String::new();
                loop {
                    line.clear();
                    if reader.read_line(&mut line).await.unwrap_or(0) == 0 {
                        return;
                    }
                    let cmd = line.trim().to_string();
                    if cmd.starts_with("BODY") {
                        if writer.write_all(b"222 body follows\r\n").await.is_err() {
                            return;
                        }
                        if writer.write_all(&body).await.is_err() {
                            return;
                        }
                        if writer.write_all(b".\r\n").await.is_err() {
                            return;
                        }
                    } else if cmd == "QUIT" {
                        let _ = writer.write_all(b"205 bye\r\n").await;
                        return;
                    } else {
                        let _ = writer.write_all(b"500 unknown\r\n").await;
                    }
                }
            });
        }
    });
    addr
}

fn build_nzb(segments: u32, name: &str) -> Nzb {
    let mut xml = String::from(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<nzb xmlns="http://www.newzbin.com/DTD/2003/nzb">
  <head><meta type="title">"#,
    );
    xml.push_str(name);
    xml.push_str("</meta></head>\n");
    xml.push_str(
        r#"  <file poster="p@t" date="1" subject="bench.bin (1/1)">
    <groups><group>alt.binaries.test</group></groups>
    <segments>"#,
    );
    for n in 1..=segments {
        xml.push_str(&format!(
            "<segment bytes=\"{SEG_SIZE}\" number=\"{n}\">bench.{n}@local</segment>"
        ));
    }
    xml.push_str("</segments></file></nzb>");
    nzb::parse(xml.as_bytes()).unwrap()
}

async fn run_bench(conns: usize, body: Arc<Vec<u8>>, segments: u32) -> f64 {
    let addr = spawn_bench_server(body.clone()).await;
    let nzb = build_nzb(segments, "bench");
    let tmp = tempfile_dir();

    let queue = Arc::new(QueueManager::open_in_memory().await.unwrap());
    let job_id = queue.add_job(&nzb, &tmp, 0, None).await.unwrap();

    let mut cfg = ServerConfig::localhost();
    cfg.port = addr.port();
    let engine = Arc::new(Engine::new(vec![cfg], conns));

    let (tx, mut rx) = mpsc::unbounded_channel();
    let q = Arc::clone(&queue);

    let start = Instant::now();
    let run = tokio::spawn(async move { engine.run_job(q, job_id, tx).await });
    // Wait for completion.
    let _ = run.await;
    let elapsed = start.elapsed().as_secs_f64();

    // Count decoded bytes via SegmentDone events.
    let mut raw_bytes = 0u64;
    while let Some(ev) = rx.recv().await {
        if let ProgressEvent::SegmentDone { bytes, .. } = ev {
            raw_bytes += bytes;
        }
    }
    let mbps = raw_bytes as f64 / 1024.0 / 1024.0 / elapsed;
    eprintln!(
        "** bench conns={} rawMBs={:.2} elapsed={:.2}s (avg_fetch={:.1}ms/seg)",
        conns,
        mbps,
        elapsed,
        elapsed * 1000.0 / segments as f64
    );
    mbps
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "benchmark; run with --ignored"]
async fn bench_download_scaling() {
    let body = Arc::new(make_yenc_body());
    let s8 = run_bench(8, body.clone(), SEGMENTS as u32).await;
    let s50 = run_bench(50, body.clone(), SEGMENTS as u32).await;
    eprintln!("** ratio 50/8 = {:.2}", s50 / s8);
    // Sanity: 50 conns should not be slower than a quarter of 8 conns
    // against a local unthrottled server. This guards against client-side
    // serialization regressions.
    assert!(s50 > s8 * 0.25, "50-conn throughput collapsed vs 8-conn");
}

fn tempfile_dir() -> PathBuf {
    use std::sync::atomic::{AtomicU64, Ordering};
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    let pid = std::process::id();
    let dir = std::env::temp_dir().join(format!("turbonzb-bench-{pid}-{n}"));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}
