//! End-to-end M1 test: NZB → engine → multi-connection download → yEnc decode
//! → assembled file on disk. Uses an in-process fake NNTP server that serves
//! real yEnc-encoded article bodies.

use std::path::PathBuf;
use std::sync::Arc;

use crc32fast::Hasher;
use nobz_core::engine::{ArticleStatus, DownloadJob, Engine, ProgressEvent};
use nobz_core::nntp::ServerConfig;
use nobz_core::nzb::{self, Nzb};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::TcpListener;
use tokio::sync::mpsc;

/// Encode `payload` as a single-part yEnc article body (the bytes that come
/// back from a `BODY` command, dot-stuffed, terminated by `.\r\n`).
fn yenc_article_body(payload: &[u8], name: &str) -> Vec<u8> {
    let mut out: Vec<u8> = Vec::new();
    out.extend_from_slice(
        format!("=ybegin line=128 size={} name={name}\r\n", payload.len()).as_bytes(),
    );
    let mut crc = Hasher::new();
    let mut body: Vec<u8> = Vec::with_capacity(payload.len());
    for &b in payload {
        crc.update(&[b]);
        let enc = b.wrapping_add(42);
        if enc == b'=' || enc == b'\r' || enc == b'\n' || enc == b'\0' {
            body.push(b'=');
            body.push(enc.wrapping_add(64));
        } else {
            body.push(enc);
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

/// A fake NNTP server that serves one yEnc-encoded article per known
/// message-id. Unknown IDs return 430. Accepts multiple concurrent
/// connections (one per engine worker).
async fn spawn_fake_nntp(articles: Vec<(String, Vec<u8>)>) -> std::net::SocketAddr {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let articles: Arc<std::collections::HashMap<String, Vec<u8>>> =
        Arc::new(articles.into_iter().collect());
    tokio::spawn(async move {
        loop {
            let (sock, _) = match listener.accept().await {
                Ok(s) => s,
                Err(_) => return,
            };
            let articles = articles.clone();
            tokio::spawn(async move {
                let (reader, mut writer) = tokio::io::split(sock);
                let mut reader = BufReader::new(reader);
                if writer.write_all(b"200 nobz-fake ready\r\n").await.is_err() {
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
                        let mid = cmd
                            .split('<')
                            .nth(1)
                            .and_then(|s| s.split('>').next())
                            .unwrap_or("");
                        if let Some(body) = articles.get(mid) {
                            writer.write_all(b"222 body follows\r\n").await.unwrap();
                            writer.write_all(body).await.unwrap();
                            writer.write_all(b".\r\n").await.unwrap();
                        } else {
                            writer.write_all(b"430 no such article\r\n").await.unwrap();
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

fn build_nzb(segments: &[(u32, &str)], name: &str) -> Nzb {
    let mut xml = String::from(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<nzb xmlns="http://www.newzbin.com/DTD/2003/nzb">
  <head><meta type="title">"#,
    );
    xml.push_str(name);
    xml.push_str(
        r#"</meta></head>
  <file poster="t@t" date="1" subject="&quot;"#,
    );
    xml.push_str(name);
    xml.push_str(
        r#".bin&quot; (1/N)">
    <groups><group>alt.binaries.test</group></groups>
    <segments>"#,
    );
    for (num, mid) in segments {
        xml.push_str(&format!(
            "<segment bytes=\"100\" number=\"{num}\">{mid}</segment>"
        ));
    }
    xml.push_str("</segments></file></nzb>");
    nzb::parse(xml.as_bytes()).unwrap()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn end_to_end_download_and_assemble() {
    let payload_a = b"AAAA-first-segment-payload-here";
    let payload_b = b"BBBB-second-segment-payload-here!!";

    let seg1 = yenc_article_body(payload_a, "demo.bin");
    let seg2 = yenc_article_body(payload_b, "demo.bin");

    let addr = spawn_fake_nntp(vec![("a@x".into(), seg1), ("b@x".into(), seg2)]).await;

    let nzb = build_nzb(&[(1, "a@x"), (2, "b@x")], "demo");
    let tmp = tempfile_dir();
    let job = DownloadJob::from_nzb(&nzb, &tmp);

    let mut cfg = ServerConfig::localhost();
    cfg.port = addr.port();
    let engine = Engine::new(vec![cfg], 4);

    let (tx, mut rx) = mpsc::unbounded_channel();
    engine.run(job, tx).await.unwrap();

    let mut file_completed = false;
    let mut job_completed = false;
    while let Some(ev) = rx.recv().await {
        match ev {
            ProgressEvent::FileCompleted {
                missing,
                crc_mismatches,
                ..
            } => {
                file_completed = true;
                assert_eq!(missing, 0, "no missing segments expected");
                assert_eq!(crc_mismatches, 0, "no CRC mismatches expected");
            }
            ProgressEvent::JobFinished { completed, .. } => {
                job_completed = true;
                assert_eq!(completed, 1);
            }
            _ => {}
        }
    }
    assert!(file_completed && job_completed);

    let assembled = tokio::fs::read(tmp.join("demo.bin")).await.unwrap();
    let expected = [payload_a.as_ref(), payload_b.as_ref()].concat();
    assert_eq!(assembled, expected);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn missing_segment_is_reported_not_fatal() {
    // Only seg1 is served; seg2 is absent → 430.
    let payload_a = b"only-segment-one-payload-bytes!!";
    let seg1 = yenc_article_body(payload_a, "partial.bin");
    let addr = spawn_fake_nntp(vec![("a@x".into(), seg1)]).await;

    let nzb = build_nzb(&[(1, "a@x"), (2, "missing@x")], "partial");
    let tmp = tempfile_dir();
    let job = DownloadJob::from_nzb(&nzb, &tmp);

    let mut cfg = ServerConfig::localhost();
    cfg.port = addr.port();
    let engine = Engine::new(vec![cfg], 2);

    let (tx, mut rx) = mpsc::unbounded_channel();
    engine.run(job, tx).await.unwrap();

    let mut saw_missing = false;
    while let Some(ev) = rx.recv().await {
        if let ProgressEvent::SegmentDone {
            status: ArticleStatus::Missing,
            ..
        } = ev
        {
            saw_missing = true;
        }
    }
    assert!(saw_missing, "engine should report the missing segment");
    // The assembled file contains only segment 1's bytes.
    let assembled = tokio::fs::read(tmp.join("partial.bin")).await.unwrap();
    assert_eq!(assembled, payload_a);
}

/// Create a unique temp directory for a test. Uses `std::env::temp_dir` +
/// process id + a counter to avoid pulling in the `tempfile` crate just for
/// two tests.
fn tempfile_dir() -> PathBuf {
    use std::sync::atomic::{AtomicU64, Ordering};
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    let pid = std::process::id();
    let dir = std::env::temp_dir().join(format!("nobz-test-{pid}-{n}"));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}
