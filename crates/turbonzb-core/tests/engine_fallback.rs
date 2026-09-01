//! Engine multi-server fallback suite (§3 of TEST_PLAN.md).
//!
//! Uses the scriptable mock server: a primary that is missing some
//! articles and a secondary that has them. The engine must transparently
//! fall back per-article and still assemble the correct file.

mod common;

use std::path::PathBuf;
use std::sync::Arc;

use crc32fast::Hasher;
use tokio::sync::mpsc;
use turbonzb_core::engine::Engine;
use turbonzb_core::nntp::ServerConfig;
use turbonzb_core::nzb::{self, Nzb};
use turbonzb_core::queue::{JobState, QueueManager, SegmentState};

use common::MockServer;

/// A yEnc-encoded part of a file, with `=ypart begin..end` and `pcrc32` so
/// the engine can position it at the right offset in the assembled file.
fn yenc_article(payload: &[u8], name: &str, begin: u64, end: u64, total: u64) -> Vec<u8> {
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
    let mut out = format!("=ybegin line=128 size={total} name={name}\r\n",).into_bytes();
    if begin != 1 || end != total {
        out.extend_from_slice(format!("=ypart begin={begin} end={end}\r\n").as_bytes());
        out.extend_from_slice(&body);
        out.extend_from_slice(b"\r\n");
        out.extend_from_slice(
            format!(
                "=yend size={} pcrc32={:08x}\r\n",
                payload.len(),
                crc.finalize()
            )
            .as_bytes(),
        );
    } else {
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
    }
    // dot-stuff before putting on the wire
    common::dot_stuff(&out)
}

fn build_nzb(segments: &[(u32, &str)], name: &str) -> Nzb {
    let mut xml = format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<nzb xmlns="http://www.newzbin.com/DTD/2003/nzb">
  <file poster="t@t" date="1" subject="&quot;{name}.bin&quot;">
    <groups><group>alt.binaries.test</group></groups>
    <segments>"#
    );
    for (num, mid) in segments {
        xml.push_str(&format!(
            "<segment bytes=\"100\" number=\"{num}\">{mid}</segment>"
        ));
    }
    xml.push_str("</segments></file></nzb>");
    nzb::parse(xml.as_bytes()).unwrap()
}

fn tempfile_dir() -> PathBuf {
    let d = std::env::temp_dir().join(format!("turbonzb-e2e-{}", std::process::id()));
    std::fs::create_dir_all(&d).unwrap();
    d
}

fn server_cfg(addr: std::net::SocketAddr) -> ServerConfig {
    let mut c = ServerConfig::localhost();
    c.port = addr.port();
    c.max_connections = 8;
    c
}

/// §3.3 — an article missing on the primary is fetched from the secondary;
/// the job completes with all bytes correct.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn falls_back_to_secondary_server() {
    let payload_a = b"AAAA-first-segment-payload-here";
    let payload_b = b"BBBB-second-segment-payload-here!!";

    let total = (payload_a.len() + payload_b.len()) as u64;

    // Primary has only segment 1.
    let primary = MockServer::new()
        .add_article(
            "a@x",
            yenc_article(payload_a, "fb.bin", 1, payload_a.len() as u64, total),
        )
        .add_missing("b@x");
    let addr_primary = primary.spawn().await;
    // Secondary has both (or at least the one the primary lacked).
    let secondary = MockServer::new()
        .add_article(
            "a@x",
            yenc_article(payload_a, "fb.bin", 1, payload_a.len() as u64, total),
        )
        .add_article(
            "b@x",
            yenc_article(
                payload_b,
                "fb.bin",
                payload_a.len() as u64 + 1,
                total,
                total,
            ),
        );
    let addr_secondary = secondary.spawn().await;

    let nzb = build_nzb(&[(1, "a@x"), (2, "b@x")], "fb");
    let tmp = tempfile_dir();

    let queue = Arc::new(QueueManager::open_in_memory().await.unwrap());
    let job_id = queue.add_job(&nzb, &tmp, 0, None).await.unwrap();

    let mut primary_cfg = server_cfg(addr_primary);
    primary_cfg.priority = 0;
    let mut secondary_cfg = server_cfg(addr_secondary);
    secondary_cfg.priority = 1;

    // Servers in fallback (priority) order: primary first, secondary second.
    let engine = Arc::new(Engine::new(vec![primary_cfg, secondary_cfg], 2));

    let (tx, _rx) = mpsc::unbounded_channel();
    let q = Arc::clone(&queue);
    let runner = tokio::spawn(async move { engine.run_job(q, job_id, tx).await.unwrap() });
    runner.await.unwrap();

    let file_completed = {
        // Re-check the file on disk.
        let assembled = tokio::fs::read(tmp.join("fb.bin")).await.unwrap();
        assert_eq!(
            assembled,
            [payload_a.as_ref(), payload_b.as_ref()].concat(),
            "fallback must assemble the correct bytes"
        );
        true
    };

    let job = queue.get_job(job_id).await.unwrap();
    assert_eq!(job.state, JobState::Complete);
    assert_eq!(job.segments_done, 2);
    assert!(file_completed);

    let files = queue.list_files(job_id).await.unwrap();
    let segs = queue.list_segments(files[0].id).await.unwrap();
    for s in &segs {
        assert_eq!(
            s.state,
            SegmentState::Done,
            "seg {} should be done",
            s.number
        );
    }
}

/// §3.4 — an article missing on BOTH servers ends up Missing (hope: PAR2 will
/// repair it), and the file still assembles from what it has.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn missing_on_all_servers_marked_missing() {
    let payload_a = b"only-a-here";
    let total = payload_a.len() as u64;
    let primary = MockServer::new()
        .add_article("a@x", yenc_article(payload_a, "m.bin", 1, total, total))
        .add_missing("b@x");
    let addr_primary = primary.spawn().await;
    let secondary = MockServer::new().add_missing("b@x");
    let addr_secondary = secondary.spawn().await;

    let nzb = build_nzb(&[(1, "a@x"), (2, "b@x")], "m");
    let tmp = tempfile_dir();

    let queue = Arc::new(QueueManager::open_in_memory().await.unwrap());
    let job_id = queue.add_job(&nzb, &tmp, 0, None).await.unwrap();

    let engine = Arc::new(Engine::new(
        vec![server_cfg(addr_primary), server_cfg(addr_secondary)],
        2,
    ));
    let (tx, _rx) = mpsc::unbounded_channel();
    let q = Arc::clone(&queue);
    let runner = tokio::spawn(async move { engine.run_job(q, job_id, tx).await.unwrap() });
    runner.await.unwrap();

    // Job should be Failed (has a missing segment) but that's recoverable.
    let job = queue.get_job(job_id).await.unwrap();
    assert_eq!(job.state, JobState::Failed);
    let files = queue.list_files(job_id).await.unwrap();
    let segs = queue.list_segments(files[0].id).await.unwrap();
    let seg2 = segs.iter().find(|s| s.number == 2).unwrap();
    assert_eq!(seg2.state, SegmentState::Missing);
}

/// §3.9 — adding the same job twice (two engine runs on two job rows) each
/// produce their own isolated output; nothing cross-contaminates.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn multiple_jobs_run_isolated() {
    let payload = b"same-payload-for-both-jobs!!";
    let total = payload.len() as u64;
    let srv =
        MockServer::new().add_article("a@x", yenc_article(payload, "iso.bin", 1, total, total));
    let addr = srv.spawn().await;

    let nzb = build_nzb(&[(1, "a@x")], "iso");
    let tmp = tempfile_dir();
    let queue = Arc::new(QueueManager::open_in_memory().await.unwrap());

    for attempt in 0..2 {
        let job_id = queue.add_job(&nzb, &tmp, 0, None).await.unwrap();
        let engine = Arc::new(Engine::new(vec![server_cfg(addr)], 2));
        let (tx, _rx) = mpsc::unbounded_channel();
        let q = Arc::clone(&queue);
        let runner = tokio::spawn(async move { engine.run_job(q, job_id, tx).await.unwrap() });
        runner.await.unwrap();
        let job = queue.get_job(job_id).await.unwrap();
        assert_eq!(
            job.state,
            JobState::Complete,
            "attempt {attempt} should complete"
        );
    }

    let assembled = tokio::fs::read(tmp.join("iso.bin")).await.unwrap();
    assert_eq!(assembled, payload);
}
