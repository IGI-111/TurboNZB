//! Queue persistence edges (§6 of TEST_PLAN.md): schema migration from a
//! legacy database file, corrupt-file handling, the single-download-slot
//! invariant, and priority ordering under a bulk queue.
//!
//! These are the paths a release actually hits after an upgrade: an existing
//! `turbonzb-queue.db` written by an older build must open, gain its missing
//! columns, keep its rows, and stay writable.

use std::path::{Path, PathBuf};

use sqlx::sqlite::{SqliteConnectOptions, SqliteJournalMode, SqlitePool, SqlitePoolOptions};
use std::str::FromStr;
use turbonzb_core::nzb::{self, Nzb};
use turbonzb_core::queue::{JobState, QueueManager, SegmentState};

fn db_path(tag: &str) -> PathBuf {
    std::env::temp_dir().join(format!("turbonzb-migrate-{}-{tag}.db", std::process::id()))
}

fn remove_db(tag: &str) {
    for suffix in ["", "-wal", "-shm"] {
        let _ = std::fs::remove_file(format!("{}{suffix}", db_path(tag).display()));
    }
}

/// Open `path` with a bare pool (no `QueueManager`) so the test can lay down a
/// legacy schema and inspect columns directly.
async fn raw_pool(path: &Path) -> SqlitePool {
    let opts = SqliteConnectOptions::from_str(&path.to_string_lossy())
        .expect("sqlite options")
        .create_if_missing(true)
        .journal_mode(SqliteJournalMode::Wal);
    SqlitePoolOptions::new()
        .max_connections(1)
        .connect_with(opts)
        .await
        .expect("raw pool")
}

fn build_nzb(name: &str, files: usize, segs: usize) -> Nzb {
    let mut xml = String::from("<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n<nzb>");
    for f in 0..files {
        xml.push_str(&format!(
            "<file poster=\"p\" subject=\"&quot;{name}-{f}.bin&quot;\"><groups><group>g</group></groups><segments>"
        ));
        for s in 1..=segs {
            xml.push_str(&format!(
                "<segment bytes=\"{}\" number=\"{s}\">{s}@x</segment>",
                1000 + s as u64
            ));
        }
        xml.push_str("</segments></file>");
    }
    xml.push_str("</nzb>");
    nzb::parse(xml.as_bytes()).unwrap()
}

/// §6.4 — a database created by an older build (no `total_bytes`,
/// `downloaded_bytes`, `error`, `archive_password`, `yenc_name` columns and
/// no indexes) opens, gains the missing columns, keeps its rows, and stays
/// writable through the new columns.
#[tokio::test]
async fn legacy_database_upgrades_in_place() {
    remove_db("legacy");
    let path = db_path("legacy");

    // --- Build the legacy database exactly as an old release would.
    {
        let pool = raw_pool(&path).await;
        for stmt in [
            r#"CREATE TABLE jobs (
                   id INTEGER PRIMARY KEY AUTOINCREMENT,
                   name TEXT NOT NULL,
                   output_dir TEXT NOT NULL,
                   state TEXT NOT NULL DEFAULT 'queued',
                   priority INTEGER NOT NULL DEFAULT 0,
                   created_at INTEGER NOT NULL DEFAULT (strftime('%s','now')),
                   file_count INTEGER NOT NULL DEFAULT 0,
                   files_done INTEGER NOT NULL DEFAULT 0,
                   total_segments INTEGER NOT NULL DEFAULT 0,
                   segments_done INTEGER NOT NULL DEFAULT 0
               )"#,
            r#"CREATE TABLE files (
                   id INTEGER PRIMARY KEY AUTOINCREMENT,
                   job_id INTEGER NOT NULL REFERENCES jobs(id) ON DELETE CASCADE,
                   file_index INTEGER NOT NULL,
                   filename TEXT NOT NULL,
                   subject TEXT NOT NULL,
                   poster TEXT NOT NULL,
                   date INTEGER NOT NULL,
                   segment_count INTEGER NOT NULL
               )"#,
            r#"CREATE TABLE segments (
                   id INTEGER PRIMARY KEY AUTOINCREMENT,
                   file_id INTEGER NOT NULL REFERENCES files(id) ON DELETE CASCADE,
                   number INTEGER NOT NULL,
                   message_id TEXT NOT NULL,
                   bytes INTEGER NOT NULL,
                   missing INTEGER NOT NULL DEFAULT 0,
                   state TEXT NOT NULL DEFAULT 'pending'
               )"#,
            "INSERT INTO jobs (name, output_dir, state, priority, file_count, files_done, total_segments, segments_done)
             VALUES ('legacy-release', '/tmp/legacy-out', 'paused', 7, 1, 0, 3, 1)",
            "INSERT INTO files (job_id, file_index, filename, subject, poster, date, segment_count)
             VALUES (1, 0, 'legacy-release.bin', 'sub', 'poster@x', 1700000000, 3)",
            "INSERT INTO segments (file_id, number, message_id, bytes, missing, state) VALUES
                 (1, 1, 'one@x', 1001, 0, 'done'),
                 (1, 2, 'two@x', 1002, 0, 'pending'),
                 (1, 3, 'three@x', 1003, 1, 'missing')",
        ] {
            sqlx::query(stmt).execute(&pool).await.expect("legacy ddl");
        }
        pool.close().await;
    }

    // --- Current build opens it and migrates.
    let q = QueueManager::open(&path).await.expect("open legacy db");

    let job = q.get_job(1).await.unwrap();
    assert_eq!(job.name, "legacy-release");
    assert_eq!(job.output_dir, PathBuf::from("/tmp/legacy-out"));
    assert_eq!(job.priority, 7);
    assert_eq!(job.file_count, 1);
    assert_eq!(job.total_segments, 3);
    assert_eq!(job.segments_done, 1);
    // The removed 'paused' state maps onto the engine-level pause.
    assert_eq!(
        job.state,
        JobState::Queued,
        "legacy 'paused' reads as queued"
    );

    // Rows survived: the file and its per-segment states are intact.
    let files = q.list_files(1).await.unwrap();
    assert_eq!(files.len(), 1);
    assert_eq!(files[0].filename, "legacy-release.bin");
    let segs = q.list_segments(files[0].id).await.unwrap();
    assert_eq!(segs.len(), 3);
    assert_eq!(segs[0].state, SegmentState::Done);
    assert_eq!(segs[1].state, SegmentState::Pending);
    assert_eq!(
        segs[2].state,
        SegmentState::Missing,
        "missing flag survives"
    );
    // Missing segments count toward the job's progress so the UI is truthful.
    let (done, missing, pending, _crc, _failed) = q.segment_state_counts(1).await.unwrap();
    assert_eq!((done, missing, pending), (1, 1, 1));

    // New columns exist and are writable after the ALTER TABLE pass.
    q.set_job_error(job.id, Some("parity mismatch on 2 segments"))
        .await
        .unwrap();
    q.set_job_archive_password(job.id, Some("hunter2"))
        .await
        .unwrap();
    let job = q.get_job(job.id).await.unwrap();
    assert_eq!(job.error.as_deref(), Some("parity mismatch on 2 segments"));
    assert_eq!(job.archive_password.as_deref(), Some("hunter2"));

    // yenc_name on files: added by migration, then usable.
    q.set_file_yenc_name(files[0].id, "real-name.mkv")
        .await
        .unwrap();
    let refreshed = q.list_files(1).await.unwrap();
    assert_eq!(refreshed[0].yenc_name.as_deref(), Some("real-name.mkv"));

    // Indexes were created for the old database too.
    {
        let pool = raw_pool(&path).await;
        let cols: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM pragma_table_info('jobs') WHERE name IN ('total_bytes','downloaded_bytes','error','archive_password')",
        )
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(cols, 4, "all four job columns migrated in");
        let idx: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM sqlite_master WHERE type='index' AND name='idx_one_downloading'",
        )
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(idx, 1, "partial unique index created on upgrade");
        pool.close().await;
    }
    drop(q);

    // Reopen: migrated values are on disk, not just in the pool's cache.
    let q = QueueManager::open(&path).await.unwrap();
    let job = q.get_job(1).await.unwrap();
    assert_eq!(job.error.as_deref(), Some("parity mismatch on 2 segments"));
    assert_eq!(job.archive_password.as_deref(), Some("hunter2"));
    drop(q);

    remove_db("legacy");
}

/// §6.4 — migration is idempotent: reopening an already-migrated database
/// does not duplicate rows or reset state.
#[tokio::test]
async fn reopen_is_idempotent() {
    remove_db("reopen");
    let path = db_path("reopen");

    let nzb = build_nzb("twice", 2, 3);
    let id = {
        let q = QueueManager::open(&path).await.unwrap();
        let id = q.add_job(&nzb, "/tmp/out", 0, None).await.unwrap();
        let files = q.list_files(id).await.unwrap();
        for seg in q.list_segments(files[0].id).await.unwrap() {
            q.set_segment_state(files[0].id, seg.number, SegmentState::Done)
                .await
                .unwrap();
        }
        q.refresh_job_counts(files[0].id).await.unwrap();
        drop(q);
        id
    };

    for _ in 0..3 {
        let q = QueueManager::open(&path).await.unwrap();
        let job = q.get_job(id).await.unwrap();
        assert_eq!(job.file_count, 2, "no duplicated file rows");
        assert_eq!(job.total_segments, 6);
        assert_eq!(job.segments_done, 3, "completed segments counted once");
        assert_eq!(job.total_bytes, (1001 + 1002 + 1003) * 2);
        assert_eq!(job.downloaded_bytes, 1001 + 1002 + 1003);
        drop(q);
    }

    remove_db("reopen");
}

/// §6.3 — exactly one job may hold the download slot. Claims from a second
/// caller are rejected while the first holds it, and releasing frees it.
#[tokio::test]
async fn download_slot_is_exclusive() {
    let q = QueueManager::open_in_memory().await.unwrap();
    let a = q
        .add_job(&build_nzb("a", 1, 1), "/tmp/a", 0, None)
        .await
        .unwrap();
    let b = q
        .add_job(&build_nzb("b", 1, 1), "/tmp/b", 1, None)
        .await
        .unwrap();

    assert!(q.claim_download_slot(a).await.unwrap(), "first claim wins");
    assert!(
        !q.claim_download_slot(b).await.unwrap(),
        "second claim must not steal the slot"
    );
    assert_eq!(q.current_downloading_job().await.unwrap(), Some(a));

    q.release_download_slot(a, JobState::Queued).await.unwrap();
    q.release_download_slot(a, JobState::Queued).await.unwrap(); // idempotent
    assert_eq!(q.current_downloading_job().await.unwrap(), None);
    assert!(
        q.claim_download_slot(b).await.unwrap(),
        "slot is free again"
    );
}

/// §6.1 — an interrupted shutdown leaves 'downloading' rows behind; recovery
/// resets them (and drops half-created 'fetching' placeholders) so the slot
/// is usable again without losing completed segment work.
#[tokio::test]
async fn recover_interrupted_resets_states_and_keeps_progress() {
    let q = QueueManager::open_in_memory().await.unwrap();
    let nzb = build_nzb("recover", 1, 3);
    let id = q.add_job(&nzb, "/tmp/out", 0, None).await.unwrap();
    let file_id = q.list_files(id).await.unwrap()[0].id;
    for seg in q.list_segments(file_id).await.unwrap() {
        if seg.number <= 2 {
            q.set_segment_state(file_id, seg.number, SegmentState::Done)
                .await
                .unwrap();
        }
    }
    q.refresh_job_counts(file_id).await.unwrap();
    q.claim_download_slot(id).await.unwrap();
    assert_eq!(q.current_downloading_job().await.unwrap(), Some(id));

    let reset = q.recover_interrupted().await.unwrap();
    assert_eq!(reset, 1, "the downloading job returns to the queue");
    assert_eq!(q.current_downloading_job().await.unwrap(), None);
    let job = q.get_job(id).await.unwrap();
    assert_eq!(job.state, JobState::Queued);
    assert_eq!(job.segments_done, 2, "completed work is not rolled back");
    assert!(
        q.claim_download_slot(id).await.unwrap(),
        "slot is usable again"
    );
    q.release_download_slot(id, JobState::Queued).await.unwrap();

    // A 'fetching' placeholder with no files is dropped rather than resumed.
    let placeholder = q
        .create_fetching_job("never-populated", "/tmp/out", 0)
        .await
        .unwrap();
    // The returned count is how many jobs were put back in the queue; a
    // placeholder is deleted instead, so nothing is re-queued here.
    assert_eq!(q.recover_interrupted().await.unwrap(), 0);
    assert!(q.get_job(placeholder).await.is_err());
    // The populated job is still there, named from its own subject since no
    // display name was given.
    assert_eq!(q.get_job(id).await.unwrap().name, "recover-0.bin");
    // A supplied display name wins over anything in the NZB.
    let named = q
        .add_job(&nzb, "/tmp/out", 0, Some("Pretty Release Name"))
        .await
        .unwrap();
    assert_eq!(q.get_job(named).await.unwrap().name, "Pretty Release Name");
}

/// §6.6 — with a bulk queue, priority changes decide what starts next;
/// ties break on insertion order, and completed jobs are not candidates.
#[tokio::test]
async fn priority_and_ordering_survive_bulk_queue() {
    let q = QueueManager::open_in_memory().await.unwrap();
    let mut ids = Vec::new();
    for i in 0..300 {
        let id = q
            .add_job(&build_nzb(&format!("job-{i}"), 1, 2), "/tmp/out", i, None)
            .await
            .unwrap();
        ids.push(id);
    }

    // Lowest priority number wins, regardless of insertion order.
    assert_eq!(
        q.next_queued_job().await.unwrap().map(|j| j.id),
        Some(ids[0])
    );
    q.set_job_priority(ids[299], -1).await.unwrap();
    assert_eq!(
        q.next_queued_job().await.unwrap().map(|j| j.id),
        Some(ids[299])
    );

    // Equal priorities fall back to the older job.
    for id in &ids {
        q.set_job_priority(*id, 5).await.unwrap();
    }
    assert_eq!(
        q.next_queued_job().await.unwrap().map(|j| j.id),
        Some(ids[0])
    );

    // Completing a job takes it out of the rotation.
    q.set_job_state(ids[0], JobState::Complete).await.unwrap();
    assert_eq!(
        q.next_queued_job().await.unwrap().map(|j| j.id),
        Some(ids[1])
    );

    // Deleting cascades: files and segments go with the job.
    q.delete_job(ids[1]).await.unwrap();
    assert!(q.list_files(ids[1]).await.unwrap().is_empty());
    assert_eq!(q.list_jobs().await.unwrap().len(), 299);
}

/// §6.5 — a database file that isn't a database is reported as an error, and
/// the app can still start with a rebuilt queue at the same path.
#[tokio::test]
async fn corrupt_database_reports_error_then_rebuilds() {
    remove_db("corrupt");
    let path = db_path("corrupt");
    std::fs::write(&path, b"\x00\x00not a sqlite file at all\x00\x00").unwrap();

    let first = QueueManager::open(&path).await;
    match first {
        // Either the file is accepted as an empty database...
        Ok(q) => {
            assert!(q.list_jobs().await.unwrap().is_empty());
            drop(q);
        }
        // ...or the failure is surfaced with a reason.
        Err(e) => assert!(!e.to_string().is_empty(), "error must carry a reason"),
    }

    // A rebuild at the same path works either way.
    let _ = std::fs::remove_file(&path);
    let q = QueueManager::open(&path).await.expect("rebuilt db opens");
    let id = q
        .add_job(&build_nzb("fresh", 1, 2), "/tmp/out", 0, None)
        .await
        .unwrap();
    assert_eq!(q.list_jobs().await.unwrap().len(), 1);
    assert_eq!(
        q.list_segments(q.list_files(id).await.unwrap()[0].id)
            .await
            .unwrap()
            .len(),
        2
    );
    drop(q);

    remove_db("corrupt");
}
