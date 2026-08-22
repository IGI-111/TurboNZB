//! End-to-end tests: NZB → queue → engine → multi-connection download →
//! yEnc decode → assembled file on disk. Uses an in-process fake NNTP server
//! that serves real yEnc-encoded article bodies.

use std::path::PathBuf;
use std::sync::Arc;

use crc32fast::Hasher;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::TcpListener;
use tokio::sync::mpsc;
use turbonzb_core::engine::{Engine, ProgressEvent};
use turbonzb_core::nntp::ServerConfig;
use turbonzb_core::nzb::{self, Nzb};
use turbonzb_core::queue::{JobState, QueueManager, SegmentState};

/// Encode `payload` as a yEnc article body for the slice of a file from
/// `begin` (1-based) to `end`, with the file's `total` size and name. The
/// bytes coming back from a `BODY` command are dot-stuffed and terminated
/// by `.\r\n`. For a whole-file (single-part) post, pass `begin=1, end=total`.
fn yenc_article_part(payload: &[u8], name: &str, begin: u64, end: u64, total: u64) -> Vec<u8> {
    let mut out: Vec<u8> = Vec::new();
    out.extend_from_slice(format!("=ybegin line=128 size={total} name={name}\r\n").as_bytes());
    if begin != 1 || end != total {
        out.extend_from_slice(format!("=ypart begin={begin} end={end}\r\n").as_bytes());
    }
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
    // Multi-part posts carry pcrc32, single-part carries crc32.
    let crc_kind = if begin != 1 || end != total { "pcrc32" } else { "crc32" };
    out.extend_from_slice(
        format!("=yend size={} {crc_kind}={:08x}\r\n", payload.len(), crc_val).as_bytes(),
    );
    out
}

/// Convenience: encode `payload` as a whole-file (single-part) article.
fn yenc_article_body(payload: &[u8], name: &str) -> Vec<u8> {
    yenc_article_part(payload, name, 1, payload.len() as u64, payload.len() as u64)
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
                if writer
                    .write_all(b"200 turbonzb-fake ready\r\n")
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

/// Drain the progress channel and collect relevant events.
async fn collect_events(rx: &mut mpsc::UnboundedReceiver<ProgressEvent>) -> Vec<ProgressEvent> {
    let mut events = Vec::new();
    while let Some(ev) = rx.recv().await {
        events.push(ev);
    }
    events
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn end_to_end_download_and_assemble() {
    let payload_a = b"AAAA-first-segment-payload-here";
    let payload_b = b"BBBB-second-segment-payload-here!!";
    let total = (payload_a.len() + payload_b.len()) as u64;

    // Two parts of one file: seg1 covers [1..len1], seg2 covers
    // [len1+1..len1+len2]. Real NZB segments carry these =ypart ranges,
    // which the direct-write engine positions segments by.
    let seg1 = yenc_article_part(payload_a, "demo.bin", 1, payload_a.len() as u64, total);
    let seg2 = yenc_article_part(
        payload_b,
        "demo.bin",
        payload_a.len() as u64 + 1,
        total,
        total,
    );

    let addr = spawn_fake_nntp(vec![("a@x".into(), seg1), ("b@x".into(), seg2)]).await;

    let nzb = build_nzb(&[(1, "a@x"), (2, "b@x")], "demo");
    let tmp = tempfile_dir();

    let queue = Arc::new(QueueManager::open_in_memory().await.unwrap());
    let job_id = queue.add_job(&nzb, &tmp, 0, None).await.unwrap();

    let mut cfg = ServerConfig::localhost();
    cfg.port = addr.port();
    let engine = Arc::new(Engine::new(vec![cfg], 4));

    let (tx, mut rx) = mpsc::unbounded_channel();
    let q = Arc::clone(&queue);
    let runner = tokio::spawn(async move { engine.run_job(q, job_id, tx).await.unwrap() });

    runner.await.unwrap();

    let events = collect_events(&mut rx).await;
    let file_completed = events.iter().any(|e| {
        matches!(
            e,
            ProgressEvent::FileCompleted {
                missing: 0,
                crc_mismatches: 0,
                ..
            }
        )
    });
    let job_finished = events
        .iter()
        .any(|e| matches!(e, ProgressEvent::JobFinished { completed: 1, .. }));
    assert!(
        file_completed,
        "file should complete with no missing/crc errors"
    );
    assert!(job_finished, "job should report 1 completed");

    let assembled = tokio::fs::read(tmp.join("demo.bin")).await.unwrap();
    let expected = [payload_a.as_ref(), payload_b.as_ref()].concat();
    assert_eq!(assembled, expected);

    // Verify the job state in the DB.
    let job = queue.get_job(job_id).await.unwrap();
    assert_eq!(job.state, JobState::Complete);
    assert_eq!(job.segments_done, 2);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn missing_segment_is_reported_not_fatal() {
    // Only seg1 is served; seg2 is absent → 430.
    let payload_a = b"only-segment-one-payload-bytes!!";
    let seg1 = yenc_article_body(payload_a, "partial.bin");
    let addr = spawn_fake_nntp(vec![("a@x".into(), seg1)]).await;

    let nzb = build_nzb(&[(1, "a@x"), (2, "missing@x")], "partial");
    let tmp = tempfile_dir();

    let queue = Arc::new(QueueManager::open_in_memory().await.unwrap());
    let job_id = queue.add_job(&nzb, &tmp, 0, None).await.unwrap();

    let mut cfg = ServerConfig::localhost();
    cfg.port = addr.port();
    let engine = Arc::new(Engine::new(vec![cfg], 2));

    let (tx, mut rx) = mpsc::unbounded_channel();
    let q = Arc::clone(&queue);
    let runner = tokio::spawn(async move { engine.run_job(q, job_id, tx).await.unwrap() });

    runner.await.unwrap();

    let events = collect_events(&mut rx).await;
    let saw_missing = events.iter().any(|e| {
        matches!(
            e,
            ProgressEvent::SegmentDone {
                status: SegmentState::Missing,
                ..
            }
        )
    });
    assert!(saw_missing, "engine should report the missing segment");

    // The assembled file contains only segment 1's bytes.
    let assembled = tokio::fs::read(tmp.join("partial.bin")).await.unwrap();
    assert_eq!(assembled, payload_a);

    // The missing segment should be persisted as Missing in the DB.
    let files = queue.list_files(job_id).await.unwrap();
    let segments = queue.list_segments(files[0].id).await.unwrap();
    let seg2 = segments.iter().find(|s| s.number == 2).unwrap();
    assert_eq!(seg2.state, SegmentState::Missing);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn resume_after_partial_download() {
    // Two segments; only the first is served in the initial run.
    // The second is served in the resume run — simulating a restart.
    let payload_a = b"AAAA-first-segment-payload-here";
    let payload_b = b"BBBB-second-segment-payload-here!!";
    let total = (payload_a.len() + payload_b.len()) as u64;

    let seg1 = yenc_article_part(payload_a, "resume.bin", 1, payload_a.len() as u64, total);
    let seg2 = yenc_article_part(
        payload_b,
        "resume.bin",
        payload_a.len() as u64 + 1,
        total,
        total,
    );

    // First run: only seg1 is available.
    let addr1 = spawn_fake_nntp(vec![("a@x".into(), seg1.clone())]).await;

    let nzb = build_nzb(&[(1, "a@x"), (2, "b@x")], "resume");
    let tmp = tempfile_dir();

    let queue = Arc::new(QueueManager::open_in_memory().await.unwrap());
    let job_id = queue.add_job(&nzb, &tmp, 0, None).await.unwrap();

    let mut cfg1 = ServerConfig::localhost();
    cfg1.port = addr1.port();
    let engine1 = Arc::new(Engine::new(vec![cfg1], 2));

    let (tx1, _rx1) = mpsc::unbounded_channel();
    let q1 = Arc::clone(&queue);
    engine1.run_job(q1, job_id, tx1).await.unwrap();

    // After first run: seg1 done, seg2 missing (430).
    let job = queue.get_job(job_id).await.unwrap();
    assert_eq!(job.state, JobState::Failed); // has missing segments
    let files = queue.list_files(job_id).await.unwrap();
    let segments = queue.list_segments(files[0].id).await.unwrap();
    let seg1_state = segments.iter().find(|s| s.number == 1).unwrap().state;
    let seg2_state = segments.iter().find(|s| s.number == 2).unwrap().state;
    assert_eq!(seg1_state, SegmentState::Done);
    assert_eq!(seg2_state, SegmentState::Missing); // 430 → hopeless

    // Reset seg2 to pending for the resume (simulating a server that now has it).
    // In real life, this would happen if the article propagated to the server.
    queue
        .set_segment_state(files[0].id, 2, SegmentState::Pending)
        .await
        .unwrap();
    queue.set_job_state(job_id, JobState::Queued).await.unwrap();

    // Second run: both segments available now.
    let addr2 = spawn_fake_nntp(vec![("a@x".into(), seg1), ("b@x".into(), seg2)]).await;

    let mut cfg2 = ServerConfig::localhost();
    cfg2.port = addr2.port();
    let engine2 = Arc::new(Engine::new(vec![cfg2], 2));

    let (tx2, mut rx2) = mpsc::unbounded_channel();
    let q2 = Arc::clone(&queue);
    let runner = tokio::spawn(async move { engine2.run_job(q2, job_id, tx2).await.unwrap() });

    runner.await.unwrap();

    let events = collect_events(&mut rx2).await;
    // Only seg2 should have been fetched (seg1 was already done).
    let segment_done_count = events
        .iter()
        .filter(|e| matches!(e, ProgressEvent::SegmentDone { .. }))
        .count();
    assert_eq!(
        segment_done_count, 1,
        "only the pending segment should be fetched on resume"
    );

    // The assembled file should now contain both segments.
    let assembled = tokio::fs::read(tmp.join("resume.bin")).await.unwrap();
    let expected = [payload_a.as_ref(), payload_b.as_ref()].concat();
    assert_eq!(assembled, expected);

    let job = queue.get_job(job_id).await.unwrap();
    assert_eq!(job.state, JobState::Complete);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn obfuscated_file_assembled_under_yenc_name() {
    // Obfuscated posts put a hash in the NZB subject but the real filename
    // in the article's `=ybegin name=` header. The assembled file must be
    // named after the real (yEnc) name, not the hash.
    let payload = b"actual-binary-payload-bytes-for-the-real-movie";
    let hex_name = "da2e2d71d5376d20cacce12c936da33e.mkv";
    let real_name = "Some.Real.Release.2026.1080p.BluRay.x264.mkv";

    let body = yenc_article_body(payload, real_name);
    let addr = spawn_fake_nntp(vec![("x@y".into(), body)]).await;

    let mut xml = String::from(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<nzb xmlns="http://www.newzbin.com/DTD/2003/nzb">
  <head><meta type="title">test</meta></head>
  <file poster="t@t" date="1" subject="&quot;"#,
    );
    xml.push_str(hex_name);
    xml.push_str(
        r#"&quot; (1/1)">
    <groups><group>alt.binaries.test</group></groups>
    <segments>
      <segment bytes="100" number="1">x@y</segment>
    </segments></file></nzb>"#,
    );
    let nzb = nzb::parse(xml.as_bytes()).unwrap();
    let tmp = tempfile_dir();

    let queue = Arc::new(QueueManager::open_in_memory().await.unwrap());
    let job_id = queue.add_job(&nzb, &tmp, 0, None).await.unwrap();

    let mut cfg = ServerConfig::localhost();
    cfg.port = addr.port();
    let engine = Arc::new(Engine::new(vec![cfg], 2));

    let (tx, mut rx) = mpsc::unbounded_channel();
    let q = Arc::clone(&queue);
    let runner = tokio::spawn(async move { engine.run_job(q, job_id, tx).await.unwrap() });

    runner.await.unwrap();
    let events = collect_events(&mut rx).await;
    assert!(events.iter().any(|e| matches!(
        e,
        ProgressEvent::FileCompleted {
            missing: 0,
            crc_mismatches: 0,
            ..
        }
    )));

    // Assembled under the real name, not the hash.
    let assembled = tokio::fs::read(tmp.join(real_name))
        .await
        .expect("real name file");
    assert_eq!(assembled, payload);
    assert!(
        !tmp.join(hex_name).exists(),
        "hex (subject) filename should not be created"
    );

    // The real name should be persisted on the file.
    let files = queue.list_files(job_id).await.unwrap();
    assert_eq!(files[0].yenc_name.as_deref(), Some(real_name));
    assert_eq!(files[0].filename, hex_name);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn alphanumeric_obfuscated_file_named_after_release() {
    // Regression: many obfuscators use arbitrary mixed-case alphanumeric
    // tokens (`0kfagna8bx9e9x5ux2un9kh`) rather than pure hex. These must
    // be recognized as obfuscated and named after the release with a
    // sniffed extension — not left as the raw token.
    let payload = b"\x1a\x45\xdf\xa3-alphanumeric-obfuscated-video-content";
    let obf = "0kfagna8bx9e9x5ux2un9kh";
    let release = "Some.Alphanumeric.Release.2026.2160p";

    let body = yenc_article_body(payload, obf);
    let addr = spawn_fake_nntp(vec![(obf.to_string() + "@x", body)]).await;

    let mut xml = String::from(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<nzb xmlns="http://www.newzbin.com/DTD/2003/nzb">
  <head><meta type="title">"#,
    );
    xml.push_str(release);
    xml.push_str(
        r#"</meta></head>
  <file poster="t@t" date="1" subject="&quot;"#,
    );
    xml.push_str(obf);
    xml.push_str(
        r#"&quot; (1/1)">
    <groups><group>alt.binaries.test</group></groups>
    <segments>
      <segment bytes="100" number="1">"#,
    );
    xml.push_str(obf);
    xml.push_str(
        r#"@x</segment>
    </segments></file></nzb>"#,
    );
    let nzb = nzb::parse(xml.as_bytes()).unwrap();
    let tmp = tempfile_dir();

    let queue = Arc::new(QueueManager::open_in_memory().await.unwrap());
    let job_id = queue.add_job(&nzb, &tmp, 0, Some(release)).await.unwrap();

    let mut cfg = ServerConfig::localhost();
    cfg.port = addr.port();
    let engine = Arc::new(Engine::new(vec![cfg], 2));

    let (tx, mut rx) = mpsc::unbounded_channel();
    let q = Arc::clone(&queue);
    let runner = tokio::spawn(async move { engine.run_job(q, job_id, tx).await });
    runner.await.unwrap();
    let events = collect_events(&mut rx).await;
    assert!(events.iter().any(|e| matches!(
        e,
        ProgressEvent::FileCompleted { missing: 0, crc_mismatches: 0, .. }
    )));

    let expected = tmp.join(format!("{release}.mkv"));
    let assembled = std::fs::read(&expected).expect("release-named file with sniffed extension");
    assert_eq!(assembled, payload);
    assert!(
        !tmp.join(obf).exists(),
        "raw obfuscated token must not remain as the file name"
    );
}


#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn fully_obfuscated_file_named_after_release() {
    // Fully obfuscated post: the hash appears in BOTH the subject and the
    // yEnc header, so no readable name exists in the data. The assembled
    // file must be named after the job's release name, with the extension
    // sniffed from the content.
    let payload = b"\x1a\x45\xdf\xa3-obfuscated-mkv-like-binary-content";
    let hash = "da2e2d71d5376d20cacce12c936da33e";
    let release = "Mr.Robot.S02.1080p.10bit.BluRay.AAC5.1.HEVC-Vyndros";

    let body = yenc_article_body(payload, hash);
    let addr = spawn_fake_nntp(vec![("x@y".into(), body)]).await;

    let mut xml = String::from(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<nzb xmlns="http://www.newzbin.com/DTD/2003/nzb">
  <head><meta type="title">"#,
    );
    xml.push_str(release);
    xml.push_str(
        r#"</meta></head>
  <file poster="t@t" date="1" subject="&quot;"#,
    );
    xml.push_str(hash);
    xml.push_str(
        r#"&quot; (1/1)">
    <groups><group>alt.binaries.test</group></groups>
    <segments>
      <segment bytes="100" number="1">x@y</segment>
    </segments></file></nzb>"#,
    );
    let nzb = nzb::parse(xml.as_bytes()).unwrap();
    let tmp = tempfile_dir();

    let queue = Arc::new(QueueManager::open_in_memory().await.unwrap());
    let job_id = queue.add_job(&nzb, &tmp, 0, Some(release)).await.unwrap();

    let mut cfg = ServerConfig::localhost();
    cfg.port = addr.port();
    let engine = Arc::new(Engine::new(vec![cfg], 2));

    let (tx, mut rx) = mpsc::unbounded_channel();
    let q = Arc::clone(&queue);
    let runner = tokio::spawn(async move { engine.run_job(q, job_id, tx).await.unwrap() });

    runner.await.unwrap();
    let events = collect_events(&mut rx).await;
    assert!(events.iter().any(|e| matches!(
        e,
        ProgressEvent::FileCompleted {
            missing: 0,
            crc_mismatches: 0,
            ..
        }
    )));

    // Named after the release with the sniffed .mkv extension.
    let expected = tmp.join(format!("{release}.mkv"));
    let assembled = tokio::fs::read(&expected)
        .await
        .expect("release-named file with sniffed extension");
    assert_eq!(assembled, payload);
    assert!(
        !tmp.join(hash).exists(),
        "hash filename should not be created"
    );
}

/// Create a unique temp directory for a test.
fn tempfile_dir() -> PathBuf {
    use std::sync::atomic::{AtomicU64, Ordering};
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    let pid = std::process::id();
    let dir = std::env::temp_dir().join(format!("turbonzb-test-{pid}-{n}"));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn direct_write_survives_directory_name_collision() {
    // Regression: if the output dir already contains a *directory* named the
    // same as the file it's writing to, the engine must not die with
    // "Is a directory" — it falls back to a temp path, then finalizes to a
    // unique name, and the bytes are still written correctly.
    let payload = b"the-directory-collision-payload-bytes";
    let total = payload.len() as u64;
    let body = yenc_article_part(payload, "collide.bin", 1, total, total);
    let addr = spawn_fake_nntp(vec![("c@x".into(), body)]).await;

    let nzb = build_nzb(&[(1, "c@x")], "collide");
    let tmp = tempfile_dir();
    // Pre-create a directory occupying the intended file name.
    std::fs::create_dir_all(tmp.join("collide.bin")).unwrap();

    let queue = Arc::new(QueueManager::open_in_memory().await.unwrap());
    let job_id = queue.add_job(&nzb, &tmp, 0, None).await.unwrap();

    let mut cfg = ServerConfig::localhost();
    cfg.port = addr.port();
    let engine = Arc::new(Engine::new(vec![cfg], 2));

    let (tx, mut rx) = mpsc::unbounded_channel();
    let q = Arc::clone(&queue);
    let runner = tokio::spawn(async move { engine.run_job(q, job_id, tx).await });

    runner.await.unwrap();

    let events = collect_events(&mut rx).await;
    assert!(
        events.iter().any(|e| matches!(e, ProgressEvent::FileCompleted { .. })),
        "file must still complete even with a directory-name collision"
    );

    // The bytes must be written somewhere in the output dir (the directory
    // occupying `collide.bin` is left alone), under a unique sibling name.
    let mut found = false;
    for entry in std::fs::read_dir(&tmp).unwrap().flatten() {
        if entry.path().is_file() {
            let data = std::fs::read(entry.path()).unwrap();
            if data == payload {
                found = true;
                break;
            }
        }
    }
    assert!(
        found,
        "payload must be written to some regular file despite the directory collision"
    );
    // The pre-existing directory must still be a directory (not clobbered).
    assert!(tmp.join("collide.bin").is_dir());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn pause_persists_completed_and_resume_skips_them() {
    // A single file split into 6 parts. We pause (cancel) the first run as
    // soon as the first segment completes; that run must persist whatever it
    // finished (no throwaway), and the resume run must fetch ONLY the
    // remaining segments — not re-download what was already persisted.
    let payloads: Vec<Vec<u8>> = (0..6)
        .map(|i| format!("pause-segment-{i:0>2}-bytes-abcdefghij").into_bytes())
        .collect();
    let total: u64 = payloads.iter().map(|p| p.len() as u64).sum();

    const MIDS: [&str; 6] = ["q0@x", "q1@x", "q2@x", "q3@x", "q4@x", "q5@x"];
    let mut articles = Vec::new();
    let mut segdefs: Vec<(u32, &str)> = Vec::new();
    let mut offset: u64 = 1;
    for (i, p) in payloads.iter().enumerate() {
        let end = offset + p.len() as u64 - 1;
        let body = yenc_article_part(p, "pause.bin", offset, end, total);
        let mid = MIDS[i];
        articles.push((mid.to_string(), body));
        segdefs.push((i as u32 + 1, mid));
        offset = end + 1;
    }

    let addr = spawn_fake_nntp(articles).await;
    let nzb = build_nzb(&segdefs, "pause");
    let tmp = tempfile_dir();
    let queue = Arc::new(QueueManager::open_in_memory().await.unwrap());
    let job_id = queue.add_job(&nzb, &tmp, 0, None).await.unwrap();

    let mut cfg = ServerConfig::localhost();
    cfg.port = addr.port();
    let engine = Arc::new(Engine::new(vec![cfg], 2));

    let flag = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let (tx, engine_rx) = mpsc::unbounded_channel::<ProgressEvent>();
    {
        // Cancel as soon as the very first segment is reported done.
        let f = Arc::clone(&flag);
        tokio::spawn(async move {
            let mut rx = engine_rx;
            while let Some(ev) = rx.recv().await {
                if matches!(ev, ProgressEvent::SegmentDone { .. }) {
                    f.store(true, std::sync::atomic::Ordering::Relaxed);
                    break;
                }
            }
        });
    }
    let eng = Arc::clone(&engine);
    let q = Arc::clone(&queue);
    let f = Arc::clone(&flag);
    let run1 = tokio::spawn(async move { eng.run_job_cancellable(q, job_id, tx, f).await });
    run1.await.unwrap().unwrap();

    let files = queue.list_files(job_id).await.unwrap();
    let segs = queue.list_segments(files[0].id).await.unwrap();
    let done_after_pause = segs.iter().filter(|s| s.state == SegmentState::Done).count();
    assert!(done_after_pause >= 1, "pause run must persist at least 1 completed segment");

    // The job's aggregate counters must be refreshed on pause so the queued
    // job shows real progress (not 0) in the queue list.
    let job = queue.get_job(job_id).await.unwrap();
    assert_eq!(
        job.segments_done as usize, done_after_pause,
        "job aggregate must reflect persisted progress after pause"
    );

    // Resume: only non-Done segments must be fetched.
    queue.set_job_state(job_id, JobState::Queued).await.unwrap();
    let (tx2, mut rx2) = mpsc::unbounded_channel::<ProgressEvent>();
    let q2 = Arc::clone(&queue);
    let run2 = tokio::spawn(async move { engine.run_job(q2, job_id, tx2).await });
    run2.await.unwrap().unwrap();

    let events = collect_events(&mut rx2).await;
    let fetched_in_resume = events
        .iter()
        .filter(|e| matches!(e, ProgressEvent::SegmentDone { .. }))
        .count();
    assert_eq!(
        fetched_in_resume,
        payloads.len() - done_after_pause,
        "resume must fetch only the segments not persisted by the paused run (no re-download)"
    );

    // The final file must be complete and correct.
    let assembled = tokio::fs::read(tmp.join("pause.bin")).await.unwrap();
    let expected: Vec<u8> = payloads.concat();
    assert_eq!(assembled, expected);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
async fn heavy_pause_resume_does_not_full_redownload() {
    // 40 segments over 8 workers, paused early. Verifies that resume does
    // NOT re-fetch already-persisted segments (full restart would re-fetch
    // everything); only segments still genuinely in-flight at pause may be.
    let n: usize = 40;
    let payloads: Vec<Vec<u8>> = (0..n)
        .map(|i| format!("heavy-seg-{i:0>3}-payload-data-....").into_bytes())
        .collect();
    let total: u64 = payloads.iter().map(|p| p.len() as u64).sum();

    let mut articles = Vec::new();
    let mut segdefs: Vec<(u32, &str)> = Vec::new();
    let mut offset: u64 = 1;
    for (i, p) in payloads.iter().enumerate() {
        let end = offset + p.len() as u64 - 1;
        let body = yenc_article_part(p, "heavy.bin", offset, end, total);
        let mid = format!("h{i}@x");
        articles.push((mid.clone(), body));
        // build_nzb needs &str; use the boxed String only for the server.
        // We keep segdefs as (number, Box<str>) -> convert below.
        offset = end + 1;
        let _ = mid;
        let smid: String = format!("h{i}@x");
        segdefs.push((i as u32 + 1, Box::leak(smid.into_boxed_str())));
    }

    let addr = spawn_fake_nntp(articles).await;
    let nzb = build_nzb(&segdefs, "heavy");
    let tmp = tempfile_dir();
    let queue = Arc::new(QueueManager::open_in_memory().await.unwrap());
    let job_id = queue.add_job(&nzb, &tmp, 0, None).await.unwrap();

    let mut cfg = ServerConfig::localhost();
    cfg.port = addr.port();
    let engine = Arc::new(Engine::new(vec![cfg], 8));

    let flag = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let (tx, engine_rx) = mpsc::unbounded_channel::<ProgressEvent>();
    {
        let f = Arc::clone(&flag);
        tokio::spawn(async move {
            let mut rx = engine_rx;
            let mut done_seen = 0usize;
            while let Some(ev) = rx.recv().await {
                if let ProgressEvent::SegmentDone { .. } = ev {
                    done_seen += 1;
                    if done_seen >= 3 {
                        f.store(true, std::sync::atomic::Ordering::Relaxed);
                        break;
                    }
                }
            }
        });
    }

    let eng = Arc::clone(&engine);
    let q = Arc::clone(&queue);
    let f = Arc::clone(&flag);
    let run1 = tokio::spawn(async move { eng.run_job_cancellable(q, job_id, tx, f).await });
    run1.await.unwrap().unwrap();

    let files = queue.list_files(job_id).await.unwrap();
    let segs = queue.list_segments(files[0].id).await.unwrap();
    let done_after_pause = segs.iter().filter(|s| s.state == SegmentState::Done).count();
    assert!(done_after_pause >= 3);

    queue.set_job_state(job_id, JobState::Queued).await.unwrap();
    let (tx2, mut rx2) = mpsc::unbounded_channel::<ProgressEvent>();
    let q2 = Arc::clone(&queue);
    let run2 = tokio::spawn(async move { engine.run_job(q2, job_id, tx2).await });
    run2.await.unwrap().unwrap();
    let events = collect_events(&mut rx2).await;
    let fetched = events
        .iter()
        .filter(|e| matches!(e, ProgressEvent::SegmentDone { .. }))
        .count();

    // A full restart would fetch all 40; this must be substantially less.
    assert!(
        fetched < n,
        "resume must not full-redownload: fetched {fetched} of {n}"
    );
    assert_eq!(
        fetched + done_after_pause,
        n,
        "resume fetches exactly the gap left after pause"
    );
    // Final file must be byte-identical.
    let assembled = std::fs::read(tmp.join("heavy.bin")).unwrap();
    assert_eq!(assembled, payloads.concat());
}
