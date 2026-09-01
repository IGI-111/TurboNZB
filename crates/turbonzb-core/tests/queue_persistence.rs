//! Queue persistence suite (§6 of TEST_PLAN.md).
//!
//! Writes jobs and segment state to a real on-disk database, drops the
//! manager, reopens the same file, and verifies nothing was lost or
//! corrupted — approximating the crash-recovery guarantee short of a
//! subprocess kill (the engine keeps per-segment state and never records a
//! file as complete before its bytes are on disk, so a reopen must see the
//! same picture).

use std::path::PathBuf;

use turbonzb_core::nzb::{self, Nzb};
use turbonzb_core::queue::{JobState, QueueManager, SegmentState};

fn build_nzb(name: &str, n_files: usize, n_segs: usize) -> Nzb {
    let mut xml = String::from("<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n<nzb>");
    for f in 0..n_files {
        let fname = if f == 0 {
            format!("{name}.bin")
        } else {
            "other.bin".to_string()
        };
        xml.push_str(&format!(
            "<file poster=\"p\" subject=\"&quot;{fname}&quot;\"><groups><group>g</group></groups><segments>"
        ));
        for s in 1..=n_segs {
            xml.push_str(&format!(
                "<segment bytes=\"10\" number=\"{s}\">{s}@x</segment>"
            ));
        }
        xml.push_str("</segments></file>");
    }
    xml.push_str("</nzb>");
    nzb::parse(xml.as_bytes()).unwrap()
}

fn db_path(tag: &str) -> PathBuf {
    std::env::temp_dir().join(format!("turbonzb-queue-{}-{tag}.db", std::process::id()))
}

fn remove_db(tag: &str) {
    for suffix in ["", "-wal", "-shm"] {
        let _ = std::fs::remove_file(format!("{}{suffix}", db_path(tag).display()));
    }
}

/// §6.1/6.2 — job indexes, per-file/segment rows and states survive a
/// close + reopen of the database file.
#[tokio::test]
async fn queue_state_persists_across_reopen() {
    remove_db("reopen");
    let path = db_path("reopen");

    let job_id = {
        let q = QueueManager::open(&path).await.expect("open for write");
        let nzb = build_nzb("persist", 2, 5);
        let id = q
            .add_job(&nzb, "/tmp/out", 0, Some("persist-release"))
            .await
            .unwrap();
        // Mark one file's segments done to make state visible.
        let files = q.list_files(id).await.unwrap();
        let file_id = files[0].id;
        for seg in q.list_segments(file_id).await.unwrap() {
            q.set_segment_state(file_id, seg.number, SegmentState::Done)
                .await
                .unwrap();
        }
        q.set_job_state(id, JobState::Complete).await.unwrap();
        drop(q); // close the pool; free the file for reopen
        id
    };

    // Reopen — fresh manager over the same file.
    {
        let q = QueueManager::open(&path).await.expect("reopen");
        let job = q.get_job(job_id).await.unwrap();
        assert_eq!(job.name, "persist-release");
        assert_eq!(job.state, JobState::Complete);
        let files = q.list_files(job_id).await.unwrap();
        assert_eq!(files.len(), 2);
        let segs = q.list_segments(files[0].id).await.unwrap();
        assert_eq!(segs.len(), 5);
        for s in &segs {
            assert_eq!(s.state, SegmentState::Done, "segment state must persist");
        }
    }

    remove_db("reopen");
}

/// §6.6 — many jobs can be added and enumerated without state corruption.
#[tokio::test]
async fn many_jobs_persist() {
    remove_db("many");
    let path = db_path("many");

    let q = QueueManager::open(&path).await.unwrap();
    let mut ids = Vec::new();
    for i in 0..50 {
        let nzb = build_nzb(&format!("job-{i}"), 1, 2);
        let id = q
            .add_job(&nzb, "/tmp/out", i, Some(&format!("job-{i}")))
            .await
            .unwrap();
        ids.push(id);
    }
    assert_eq!(ids.len(), 50);
    let uniq: std::collections::HashSet<_> = ids.iter().copied().collect();
    assert_eq!(uniq.len(), 50, "job ids must be distinct");
    drop(q);

    let q = QueueManager::open(&path).await.unwrap();
    for id in &ids {
        let job = q.get_job(*id).await.unwrap();
        assert_eq!(job.state, JobState::Queued);
        let files = q.list_files(*id).await.unwrap();
        assert_eq!(files.len(), 1);
    }

    remove_db("many");
}
