//! Crash-recovery suite (§6.1 of TEST_PLAN.md).
//!
//! Runs the real download engine in a **subprocess**, lets it download part
//! of a job, then **SIGKILLs** it. We then reopen the same on-disk queue in
//! a fresh process and verify:
//!
//!   1. The persisted state is crash-consistent (no job falsely marked
//!      Complete, no segment in an impossible state, no torn rows).
//!   2. Resuming from that state succeeds — already-downloaded segments are
//!      not lost, the rest finish, and the assembled file is byte-identical.
//!
//! Two crash modes:
//!   - `cancel`: the engine reaches a *known* partial state (the job is
//!     gracefully stopped before finalization) and is then SIGKILLed while
//!     idle — proves that persisted state survives a hard kill.
//!   - `active`: the engine is SIGKILLed *while actively streaming/writing*
//!     — proves the DB stays consistent across an arbitrary torn kill.
//!
//! Unix-only (uses SIGKILL).

mod common;

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use crc32fast::Hasher;
use tokio::sync::mpsc;
use turbonzb_core::engine::Engine;
use turbonzb_core::nntp::ServerConfig;
use turbonzb_core::nzb::{self, Nzb};
use turbonzb_core::queue::{JobState, QueueManager, SegmentState};

const CHILD_ENV: &str = "TURBONZB_CRASH_CHILD";
const MODE_ENV: &str = "TURBONZB_CRASH_MODE";
const CHILD_TEST: &str = "crash_recovery_child_worker";

/// Encode one part (begin..=end, 1-based) of a file as a yEnc article,
/// wrapped into realistic 128-byte lines.
fn yenc_part(payload: &[u8], name: &str, begin: u64, end: u64, total: u64) -> Vec<u8> {
    const LINE: usize = 128;
    let mut body: Vec<u8> = Vec::new();
    let mut crc = Hasher::new();
    let mut cols = 0usize;
    for &b in payload {
        crc.update(&[b]);
        let enc = b.wrapping_add(42);
        if matches!(enc, b'=' | b'\r' | b'\n' | 0) {
            body.push(b'=');
            body.push(enc.wrapping_add(64));
        } else {
            body.push(enc);
        }
        cols += 1;
        if cols >= LINE {
            body.push(b'\r');
            body.push(b'\n');
            cols = 0;
        }
    }
    let mut out = format!("=ybegin line={LINE} size={total} name={name}\r\n").into_bytes();
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
    common::dot_stuff(&out)
}

fn build_nzb(segments: &[(u32, &str)]) -> Nzb {
    let mut xml = String::from("<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n<nzb>\n")
        + "<file poster=\"t\" date=\"1\" subject=\"&quot;crash.bin&quot;\">"
        + "<groups><group>g</group></groups><segments>";
    for (num, mid) in segments {
        xml.push_str(&format!(
            "<segment bytes=\"100\" number=\"{num}\">{mid}</segment>"
        ));
    }
    xml.push_str("</segments></file></nzb>");
    nzb::parse(xml.as_bytes()).unwrap()
}

fn cfg_for_port(port: u16, max_conn: u32) -> ServerConfig {
    let mut c = ServerConfig::localhost();
    c.port = port;
    c.max_connections = max_conn;
    c
}

fn paths(tag: &str) -> (PathBuf, PathBuf, PathBuf) {
    let pid = std::process::id();
    (
        std::env::temp_dir().join(format!("turbonzb-crash-{pid}-{tag}.db")),
        std::env::temp_dir().join(format!("turbonzb-crash-{pid}-{tag}.out")),
        std::env::temp_dir().join(format!("turbonzb-crash-{pid}-{tag}.marker")),
    )
}

fn clean(p: &(PathBuf, PathBuf, PathBuf)) {
    for suffix in ["", "-wal", "-shm"] {
        let _ = std::fs::remove_file(format!("{}{suffix}", p.0.display()));
    }
    let _ = std::fs::remove_dir_all(&p.1);
    let _ = std::fs::remove_file(&p.2);
}

/// The subprocess worker. When run normally (no env var) it is a no-op; when
/// the parent re-invokes this same test binary with `TURBONZB_CRASH_CHILD=1`
/// and the `crash_recovery_child_worker` test filter, it runs a real partial
/// download and then holds still until SIGKILLed. `TURBONZB_CRASH_MODE`
/// selects the crash point (see the module docs).
#[cfg(unix)]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn crash_recovery_child_worker() {
    if std::env::var_os(CHILD_ENV).is_none() {
        return; // normal (non-child) run: no-op
    }
    let mode = std::env::var(MODE_ENV).unwrap_or_default();
    let args: Vec<String> = std::env::args().collect();
    let port: u16 = args.get(2).and_then(|s| s.parse().ok()).expect("port");
    let queue_path = args.get(3).expect("queue path");
    let job_id: i64 = args.get(4).and_then(|s| s.parse().ok()).expect("job id");
    let marker = args.get(5).expect("marker path");

    let queue = Arc::new(
        QueueManager::open(queue_path)
            .await
            .expect("child open queue"),
    );
    let engine = Arc::new(Engine::new(vec![cfg_for_port(port, 1)], 1));

    if mode == "active" {
        // Keep the engine running in the background; write the marker (and
        // thus become killable) as soon as segment 1 is persisted, so the
        // parent can SIGKILL us while the engine is mid-stream.
        let engine2 = engine.clone();
        let q = Arc::clone(&queue);
        let (tx, _rx) = mpsc::unbounded_channel();
        tokio::spawn(async move {
            let _ = engine2.run_job(q, job_id, tx).await;
        });
        let q = Arc::clone(&queue);
        let m = marker.clone();
        let watchdog = tokio::spawn(async move {
            let files = q.list_files(job_id).await.expect("list files");
            let fid = files[0].id;
            loop {
                let segs = q.list_segments(fid).await.expect("list segments");
                if segs.iter().any(|s| s.state == SegmentState::Done) {
                    std::fs::write(&m, b"ready").expect("write marker");
                    break;
                }
                tokio::time::sleep(Duration::from_millis(2)).await;
            }
        });
        let _ = watchdog.await;
        loop {
            std::thread::sleep(Duration::from_secs(60));
        }
    } else {
        // cancel mode: gracefully stop at a known partial state, then idle
        // until killed (proving that persisted state survives a hard kill).
        let cancel = Arc::new(AtomicBool::new(false));
        let q = Arc::clone(&queue);
        let c = Arc::clone(&cancel);
        let watchdog = tokio::spawn(async move {
            let files = q.list_files(job_id).await.expect("list files");
            let fid = files[0].id;
            while !c.load(Ordering::Relaxed) {
                let segs = q.list_segments(fid).await.expect("list segments");
                if segs.iter().any(|s| s.state == SegmentState::Done) {
                    c.store(true, Ordering::Relaxed);
                    break;
                }
                tokio::time::sleep(Duration::from_millis(2)).await;
            }
        });
        let (tx, _rx) = mpsc::unbounded_channel();
        let q = Arc::clone(&queue);
        engine
            .run_job_cancellable(q, job_id, tx, cancel)
            .await
            .expect("child partial download");
        let _ = watchdog.await;
        std::fs::write(marker, b"ready").expect("write marker");
        loop {
            std::thread::sleep(Duration::from_secs(60));
        }
    }
}

/// Shared post-kill verification: reopen, check consistency, resume if
/// needed, then confirm byte-identical output.
#[cfg(unix)]
async fn verify_recovery(
    db: &std::path::Path,
    outdir: &std::path::Path,
    addr: std::net::SocketAddr,
    job_id: i64,
    n_segs: usize,
) {
    let queue = Arc::new(QueueManager::open(db).await.expect("reopen queue"));
    let job = queue.get_job(job_id).await.unwrap();
    let files = queue.list_files(job_id).await.unwrap();
    let fid = files[0].id;
    let mut segs = queue.list_segments(fid).await.unwrap();

    if job.state == JobState::Complete {
        // The kill landed after the job finished — still must be byte-correct.
        let assembled = tokio::fs::read(outdir.join("crash.bin")).await.ok();
        assert!(
            assembled.is_some(),
            "completed job must have an assembled file"
        );
        return;
    }

    // Consistency checks on the surviving partial state.
    assert!(
        segs.iter().any(|s| s.state == SegmentState::Done),
        "the pre-kill downloaded segment must be persisted"
    );
    for s in &segs {
        assert!(
            matches!(
                s.state,
                SegmentState::Pending
                    | SegmentState::Done
                    | SegmentState::Failed
                    | SegmentState::Missing
                    | SegmentState::CrcMismatch
            ),
            "segment {} in impossible post-crash state {:?}",
            s.number,
            s.state
        );
    }

    // Resume with a fresh engine to completion.
    let engine = Arc::new(Engine::new(vec![cfg_for_port(addr.port(), 4)], 2));
    let (tx, _rx) = mpsc::unbounded_channel();
    let q = Arc::clone(&queue);
    engine.run_job(q, job_id, tx).await.unwrap();

    let job = queue.get_job(job_id).await.unwrap();
    assert_eq!(
        job.state,
        JobState::Complete,
        "resume must complete the job"
    );
    segs = queue.list_segments(fid).await.unwrap();
    assert_eq!(segs.len(), n_segs);
    for s in &segs {
        assert_eq!(
            s.state,
            SegmentState::Done,
            "all segments done after resume"
        );
    }
    let _ = job;
}

/// §6.1 — crash at a known partial state (graceful cancel, then idle kill)
/// and at an arbitrary mid-write point (active kill). Both must recover.
#[cfg(unix)]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn crash_mid_download_recovers() {
    for mode in ["cancel", "active"] {
        let (db, outdir, marker) = paths(mode);
        clean(&(db.clone(), outdir.clone(), marker.clone()));

        // Build a many-segment file; all available immediately. One server
        // connection makes the download slow enough to kill mid-write.
        const N: usize = 30;
        let mut seg_ids: Vec<(u32, String)> = Vec::new();
        let mut payloads: Vec<Vec<u8>> = Vec::new();
        let mut total: u64 = 0;
        let mut offset: u64 = 0;
        let mut server = common::MockServer::new();
        for i in 0..N {
            let p = (0..200u32)
                .map(|k| (k as u8).wrapping_add(i as u8))
                .collect::<Vec<_>>();
            total += p.len() as u64;
            let mid = format!("{i}@x");
            let begin = offset + 1;
            let end = offset + p.len() as u64;
            seg_ids.push((i as u32 + 1, mid.clone()));
            server = server.add_article(&mid, yenc_part(&p, "crash.bin", begin, end, total));
            payloads.push(p);
            offset = end;
        }
        let addr = server.spawn().await;

        // Seed the queue, then close it so the child can open it.
        let job_id = {
            let q = QueueManager::open(&db).await.expect("open queue");
            let nzb = build_nzb(
                &seg_ids
                    .iter()
                    .map(|(n, m)| (*n, m.as_str()))
                    .collect::<Vec<_>>(),
            );
            let id = q.add_job(&nzb, &outdir, 0, None).await.unwrap();
            drop(q);
            id
        };

        // Spawn ourselves as the child worker and wait for its partial state.
        let exe = std::env::current_exe().unwrap();
        let mut child = std::process::Command::new(&exe)
            .arg(CHILD_TEST)
            .env(CHILD_ENV, "1")
            .env(MODE_ENV, mode)
            .arg(addr.port().to_string())
            .arg(&db)
            .arg(job_id.to_string())
            .arg(&marker)
            .spawn()
            .expect("spawn child");

        let deadline = Instant::now() + Duration::from_secs(60);
        while !marker.exists() {
            assert!(
                Instant::now() < deadline,
                "child never reached partial state"
            );
            std::thread::sleep(Duration::from_millis(5));
        }

        // SIGKILL the child mid-download.
        child.kill().expect("kill child");
        let status = child.wait().expect("wait child");
        assert!(
            !status.success(),
            "child should have been killed, not exited cleanly"
        );

        // Recover and verify.
        verify_recovery(&db, &outdir, addr, job_id, N).await;

        // The assembled file must be byte-identical to concatenating sources.
        let assembled = tokio::fs::read(outdir.join("crash.bin")).await.unwrap();
        let expected: Vec<u8> = payloads.iter().flatten().copied().collect();
        assert_eq!(
            assembled, expected,
            "assembled bytes must be exact after crash"
        );

        clean(&(db, outdir, marker));
    }
}
