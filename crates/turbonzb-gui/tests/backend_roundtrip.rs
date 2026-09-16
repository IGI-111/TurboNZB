//! GUI ↔ backend round-trips (§11 of TEST_PLAN.md).
//!
//! Every UI action is a command into the backend and every visible change is
//! an event back out. These tests drive that channel directly — no windowing
//! system needed: spawn the backend with a temp config, send the commands the
//! widgets send, and assert on the events that come back. The pure
//! state-reduce side (`QueueState::handle_events`) is checked the same way.

use std::collections::HashSet;
use std::path::PathBuf;
use std::time::{Duration, Instant};

use tokio::sync::mpsc;
use turbonzb_core::nzb;
use turbonzb_core::queue::{JobState, QueueManager, SegmentState};
use turbonzb_gui::backend::{Backend, BackendCmd, BackendEvent, BackendHandle};
use turbonzb_gui::queue_tab::{DetailsTab, QueueState};
use turbonzb_gui::settings::{AppConfig, CategoryMapping, PostProcessDefaults, ServerEntry};

// ------------------------------------------------------------------ setup ---

fn temp_dir(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("turbonzb-gui-{}-{tag}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

/// A two-file, three-segment NZB with a declared password, as an indexer
/// would hand it over.
fn sample_nzb() -> Vec<u8> {
    let mut xml = String::from("<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n<nzb>");
    xml.push_str("<head>");
    xml.push_str("<meta type=\"title\">Sample Release</meta>");
    xml.push_str("<meta type=\"password\">hunter2</meta>");
    xml.push_str("</head>");
    for f in 0..2 {
        xml.push_str(&format!(
            "<file poster=\"someone\" subject=\"&quot;sample-{f}.bin&quot;\" groups=\"alt.binaries\"><segments>"
        ));
        for s in 1..=3 {
            xml.push_str(&format!(
                "<segment bytes=\"{}\" number=\"{s}\">{s}@sample.invalid</segment>",
                1000 + s
            ));
        }
        xml.push_str("</segments></file>");
    }
    xml.push_str("</nzb>");
    xml.into_bytes()
}

fn config_with(download_dir: PathBuf, db_path: PathBuf) -> AppConfig {
    AppConfig {
        servers: vec![ServerEntry {
            host: "news.example.invalid".into(),
            port: 119,
            tls: false,
            user: None,
            password: None,
            max_connections: 4,
            priority: 0,
        }],
        indexers: Vec::new(),
        db_path,
        download_dir: download_dir.clone(),
        completed_dir: download_dir.join("completed"),
        categories: vec![CategoryMapping {
            name: "tv".into(),
            subfolder: "tv".into(),
        }],
        max_connections: 0,
        post_process: PostProcessDefaults::default(),
    }
}

/// Spawn a backend and hand back the handle plus its runtime. The runtime is
/// kept alive by the caller for as long as the handle is used.
fn spawn_backend(tag: &str) -> (BackendHandle, tokio::runtime::Runtime, PathBuf) {
    let dir = temp_dir(tag);
    let config = config_with(dir.clone(), dir.join("turbonzb-queue.db"));
    let (handle, runtime) = Backend::spawn(config, egui::Context::default());
    (handle, runtime, dir)
}

/// Drain events into `seen` until `done` is satisfied, or give up after a few
/// seconds. `seen` belongs to the test and keeps every event received so far,
/// so a later wait can still read an earlier reply.
fn drain_until<F>(handle: &BackendHandle, seen: &mut Vec<BackendEvent>, what: &str, done: F)
where
    F: Fn(&[BackendEvent]) -> bool,
{
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        seen.extend(handle.drain());
        if done(seen) {
            return;
        }
        assert!(
            Instant::now() <= deadline,
            "timed out waiting for {what}; saw {} events",
            seen.len()
        );
        std::thread::sleep(Duration::from_millis(5));
    }
}

/// The most recent view of a job, which is what the queue tab shows.
fn job_named<'a>(events: &'a [BackendEvent], name: &str) -> &'a turbonzb_core::queue::QueueJob {
    for ev in events.iter().rev() {
        if let BackendEvent::JobsList(jobs) = ev {
            if let Some(job) = jobs.iter().find(|j| j.name == name) {
                return job;
            }
        }
    }
    panic!("no job named {name} in the job list");
}

/// Wait for a job list whose names, in order, are `names`. Only a job list
/// that arrived after the call counts, so an earlier one cannot satisfy it.
fn wait_for_names(handle: &BackendHandle, seen: &mut Vec<BackendEvent>, names: &[&str]) {
    let before = seen
        .iter()
        .filter(|e| matches!(e, BackendEvent::JobsList(_)))
        .count();
    let expected: Vec<String> = names.iter().map(|s| (*s).to_string()).collect();
    drain_until(handle, seen, &format!("job list {names:?}"), |evs| {
        let fresh = evs
            .iter()
            .filter(|e| matches!(e, BackendEvent::JobsList(_)))
            .count()
            > before;
        fresh && job_ids(evs) == expected
    });
}

/// Job names in the newest job list, in display order.
fn job_ids(events: &[BackendEvent]) -> Vec<String> {
    events
        .iter()
        .rev()
        .find_map(|ev| match ev {
            BackendEvent::JobsList(jobs) => Some(jobs.iter().map(|j| j.name.clone()).collect()),
            _ => None,
        })
        .unwrap_or_default()
}

// -------------------------------------------------------------- round trips ---

/// §11.1 — opening an NZB file adds exactly one job, named after the file,
/// writing into the configured download directory, with the NZB's password on
/// the job for later unpacking.
#[test]
fn open_nzb_file_round_trip() {
    let (handle, _runtime, dir) = spawn_backend("open-nzb");
    let mut seen: Vec<BackendEvent> = Vec::new();
    let nzb_path = dir.join("Sample.Release.nzb");
    std::fs::write(&nzb_path, sample_nzb()).unwrap();

    // Pause first so the job stays in the queue instead of racing through
    // the download against a server that does not exist.
    handle.send(BackendCmd::PauseEngine);
    drain_until(&handle, &mut seen, "EnginePaused(true)", |evs| {
        evs.iter()
            .any(|e| matches!(e, BackendEvent::EnginePaused(true)))
    });

    handle.send(BackendCmd::OpenNzbFile { path: nzb_path });

    drain_until(&handle, &mut seen, "JobAdded", |evs| {
        evs.iter()
            .any(|e| matches!(e, BackendEvent::JobAdded { .. }))
    });
    let job_id = seen
        .iter()
        .rev()
        .find_map(|e| match e {
            BackendEvent::JobAdded { job_id } => Some(*job_id),
            _ => None,
        })
        .expect("JobAdded carries the new id");

    drain_until(&handle, &mut seen, "Queued state", |evs| {
        evs.iter().any(|e| {
            matches!(
                e,
                BackendEvent::JobStateChanged { job_id: id, state: JobState::Queued } if *id == job_id
            )
        })
    });

    let job = job_named(&seen, "Sample.Release");
    assert_eq!(job.id, job_id);
    assert_eq!(job.file_count, 2, "both NZB files are recorded");
    assert_eq!(job.total_segments, 6);
    assert_eq!(job.total_bytes, (1001 + 1002 + 1003) * 2);
    assert_eq!(job.state, JobState::Queued);
    assert_eq!(job.archive_password.as_deref(), Some("hunter2"));
    assert!(
        job.output_dir.starts_with(&dir),
        "job writes under the configured download dir, got {}",
        job.output_dir.display()
    );

    // Details for that job come back with per-segment state.
    handle.send(BackendCmd::GetJobDetails { job_id });
    drain_until(&handle, &mut seen, "JobDetails", |evs| {
        evs.iter()
            .any(|e| matches!(e, BackendEvent::JobDetails { job_id: id, .. } if *id == job_id))
    });
    let files = seen
        .iter()
        .rev()
        .find_map(|e| match e {
            BackendEvent::JobDetails { job_id: id, files } if *id == job_id => Some(files.clone()),
            _ => None,
        })
        .expect("details for the job");
    assert_eq!(files.len(), 2);
    assert_eq!(files[0].segment_count, 3);
    assert_eq!(files[0].segments.len(), 3, "every segment is listed");
    assert_eq!(files[0].segments_done, 0, "nothing fetched yet");
    assert_eq!(files[0].total_bytes, 3006);
}

/// §11.1 — pause stops auto-start, resume re-enables it. The pause flag is
/// echoed so the button label can flip immediately.
#[test]
fn pause_and_resume_are_echoed_and_gate_autostart() {
    let (handle, _runtime, dir) = spawn_backend("pause");
    let mut seen: Vec<BackendEvent> = Vec::new();
    let nzb_path = dir.join("Pausable.nzb");
    std::fs::write(&nzb_path, sample_nzb()).unwrap();

    handle.send(BackendCmd::PauseEngine);
    drain_until(&handle, &mut seen, "EnginePaused(true)", |evs| {
        evs.iter()
            .any(|e| matches!(e, BackendEvent::EnginePaused(true)))
    });

    handle.send(BackendCmd::OpenNzbFile { path: nzb_path });
    drain_until(&handle, &mut seen, "queued job while paused", |evs| {
        evs.iter().any(|e| {
            matches!(
                e,
                BackendEvent::JobStateChanged {
                    state: JobState::Queued,
                    ..
                }
            )
        })
    });
    let job = job_named(&seen, "Pausable");
    assert_eq!(
        job.state,
        JobState::Queued,
        "paused engine must not start it"
    );

    handle.send(BackendCmd::ResumeEngine);
    drain_until(&handle, &mut seen, "EnginePaused(false)", |evs| {
        evs.iter()
            .any(|e| matches!(e, BackendEvent::EnginePaused(false)))
    });
}

/// §11.1 — reorder, delete, and clear-completed each round-trip through the
/// database and come back as a fresh job list.
#[test]
fn reorder_delete_and_clear_completed_round_trip() {
    let dir = temp_dir("reorder");
    let db = dir.join("turbonzb-queue.db");

    // Start with an empty queue and the engine paused, so a queued job is
    // still queued when the queue tab reads it back.
    let config = config_with(dir.clone(), db.clone());
    let (handle, _runtime) = Backend::spawn(config, egui::Context::default());
    let mut seen: Vec<BackendEvent> = Vec::new();
    handle.send(BackendCmd::PauseEngine);
    drain_until(&handle, &mut seen, "EnginePaused(true)", |evs| {
        evs.iter()
            .any(|e| matches!(e, BackendEvent::EnginePaused(true)))
    });

    // Seed the queue the way a previous session would have left it.
    let seed_dir = dir.clone();
    let seed_db = db.clone();
    let (first, second, done_one) = tokio_handle(|| async move {
        let q = QueueManager::open(&seed_db).await.unwrap();
        let nzb = nzb::parse(&sample_nzb()).unwrap();
        let first = q.add_job(&nzb, &seed_dir, 0, Some("first")).await.unwrap();
        let second = q.add_job(&nzb, &seed_dir, 1, Some("second")).await.unwrap();
        let done_one = q
            .add_job(&nzb, &seed_dir, 2, Some("done-one"))
            .await
            .unwrap();
        q.set_job_state(done_one, JobState::Complete).await.unwrap();
        drop(q);
        (first, second, done_one)
    });

    handle.send(BackendCmd::RefreshJobs);
    wait_for_names(&handle, &mut seen, &["first", "second", "done-one"]);

    // Up swaps with the neighbour above; priorities are renormalized so the
    // order survives a restart.
    handle.send(BackendCmd::MoveJobUp { job_id: second });
    wait_for_names(&handle, &mut seen, &["second", "first", "done-one"]);

    // Down moves it back.
    handle.send(BackendCmd::MoveJobDown { job_id: second });
    wait_for_names(&handle, &mut seen, &["first", "second", "done-one"]);

    // Deleting one job leaves the others untouched.
    handle.send(BackendCmd::DeleteJob { job_id: first });
    wait_for_names(&handle, &mut seen, &["second", "done-one"]);

    // Clear-completed drops the finished job and keeps the queued one,
    // whatever state the running job happens to be in.
    handle.send(BackendCmd::ClearCompleted);
    wait_for_names(&handle, &mut seen, &["second"]);
    assert!(done_one > 0);

    // The new order is what a reopening reads: priorities decide, not ids.
    let reopened = tokio_handle(|| {
        let db = db.clone();
        async move {
            let q = QueueManager::open(&db).await.unwrap();
            let names: Vec<String> = q
                .list_jobs()
                .await
                .unwrap()
                .into_iter()
                .map(|j| j.name)
                .collect();
            drop(q);
            names
        }
    });
    assert_eq!(reopened, vec!["second"]);
}

/// §11.1 — a saved settings change is visible to commands that run afterwards.
#[test]
fn set_config_affects_later_commands() {
    let dir = temp_dir("set-config");
    let mut config = config_with(dir.clone(), dir.join("turbonzb-queue.db"));

    let (handle, _runtime) = Backend::spawn(config.clone(), egui::Context::default());
    let mut seen: Vec<BackendEvent> = Vec::new();

    let nzb_path = dir.join("Early.nzb");
    std::fs::write(&nzb_path, sample_nzb()).unwrap();
    handle.send(BackendCmd::PauseEngine);
    handle.send(BackendCmd::OpenNzbFile { path: nzb_path });
    drain_until(&handle, &mut seen, "first job", |evs| {
        evs.iter()
            .any(|e| matches!(e, BackendEvent::JobsList(j) if j.iter().any(|j| j.name == "Early")))
    });
    let first_output = job_named(&seen, "Early").output_dir.clone();

    // Save a new download directory, then add another job.
    let second_dir = dir.join("second-dir");
    config.download_dir = second_dir.clone();
    config.completed_dir = second_dir.join("done");
    config.max_connections = 8;
    config.indexers = vec![turbonzb_index::types::IndexerConfig {
        name: "mock".into(),
        url: "http://127.0.0.1:1/api".into(),
        api_key: "k".into(),
        max_concurrent: 1,
        timeout_s: 2,
        priority: 0,
    }];
    handle.send(BackendCmd::SetConfig(Box::new(config.clone())));

    let nzb_path2 = dir.join("Later.nzb");
    std::fs::write(&nzb_path2, sample_nzb()).unwrap();
    handle.send(BackendCmd::OpenNzbFile { path: nzb_path2 });
    drain_until(&handle, &mut seen, "second job", |evs| {
        evs.iter()
            .any(|e| matches!(e, BackendEvent::JobsList(j) if j.iter().any(|j| j.name == "Later")))
    });

    let later = job_named(&seen, "Later");
    assert!(
        later.output_dir.starts_with(&second_dir),
        "the saved download dir is used for new jobs, got {}",
        later.output_dir.display()
    );
    assert_ne!(later.output_dir, first_output);
    assert!(later.is_fetching_or_queued());
}

/// §11.1 — an error raised on the UI side reaches the queue tab as a
/// readable message instead of being swallowed.
#[test]
fn error_command_is_surfaced() {
    let (handle, _runtime, _dir) = spawn_backend("error");
    let mut seen: Vec<BackendEvent> = Vec::new();
    handle.send(BackendCmd::Error("disk full while writing /tmp/x".into()));
    drain_until(&handle, &mut seen, "Error event", |evs| {
        evs.iter().any(|e| matches!(e, BackendEvent::Error(_)))
    });
    let msg = seen
        .iter()
        .rev()
        .find_map(|e| match e {
            BackendEvent::Error(m) => Some(m.clone()),
            _ => None,
        })
        .expect("error text survives the round trip");
    assert!(msg.contains("disk full"), "message kept intact: {msg}");
}

// ------------------------------------------------------- queue tab reduce ---

fn handle_pair() -> (BackendHandle, mpsc::UnboundedReceiver<BackendCmd>) {
    let (cmd_tx, cmd_rx) = mpsc::unbounded_channel();
    let (_event_tx, event_rx) = mpsc::unbounded_channel();
    (
        BackendHandle {
            cmd_tx,
            event_rx: std::sync::Arc::new(std::sync::Mutex::new(event_rx)),
        },
        cmd_rx,
    )
}

/// §11.4 — the queue tab mirrors engine state: the active job, speed history,
/// live byte counters, and progress bumps.
#[test]
fn queue_state_mirrors_engine_events() {
    let (backend, _cmd_rx) = handle_pair();
    let mut state = QueueState::default();

    let jobs = vec![queued_job(1, "alpha", 2, 4), queued_job(2, "beta", 0, 0)];
    state.handle_events(&[BackendEvent::JobsList(jobs)], &backend);
    assert_eq!(state.current_job_id, None, "nothing downloading yet");
    assert_eq!(state.jobs.len(), 2);

    // Job 1 starts.
    state.handle_events(
        &[BackendEvent::JobStateChanged {
            job_id: 1,
            state: JobState::Downloading,
        }],
        &backend,
    );
    assert_eq!(state.current_job_id, Some(1));

    // Segment progress moves the bar without waiting for the DB poll.
    state.handle_events(
        &[BackendEvent::Progress(
            turbonzb_core::engine::ProgressEvent::SegmentDone {
                filename: "alpha.bin".into(),
                segment: 1,
                status: SegmentState::Done,
                bytes: 700_000,
            },
        )],
        &backend,
    );
    assert_eq!(state.current_downloaded, 700_000);
    assert_eq!(state.jobs[0].segments_done, 1);

    // A missing segment transferred nothing but still counts as attempted.
    state.handle_events(
        &[BackendEvent::Progress(
            turbonzb_core::engine::ProgressEvent::SegmentDone {
                filename: "alpha.bin".into(),
                segment: 2,
                status: SegmentState::Missing,
                bytes: 0,
            },
        )],
        &backend,
    );
    assert_eq!(
        state.current_downloaded, 700_000,
        "missing bytes are not counted"
    );
    assert_eq!(state.jobs[0].segments_done, 1);

    // Speed samples carry the graph history and connection count.
    state.handle_events(
        &[BackendEvent::Speed {
            job_id: Some(1),
            bytes_per_sec: 12_500_000.0,
            downloaded_bytes: 1_400_000,
            total_bytes: 2_800_000,
            history: vec![10_000_000.0, 12_500_000.0],
            active_connections: 4,
        }],
        &backend,
    );
    assert_eq!(state.current_speed, 12_500_000.0);
    assert_eq!(state.speed_history.len(), 2);
    assert_eq!(state.active_connections, 4);
    assert_eq!(state.current_downloaded, 1_400_000);
    assert_eq!(state.current_total, 2_800_000);

    // Completion cannot overshoot the file count.
    for _ in 0..4 {
        state.handle_events(
            &[BackendEvent::Progress(
                turbonzb_core::engine::ProgressEvent::FileCompleted {
                    filename: "alpha.bin".into(),
                    path: PathBuf::from("/tmp/alpha.bin"),
                    missing: 0,
                    crc_mismatches: 0,
                },
            )],
            &backend,
        );
    }
    assert_eq!(state.jobs[0].files_done, 2, "capped at file_count");

    // The job finishing clears the active-job pointer.
    state.handle_events(
        &[BackendEvent::JobStateChanged {
            job_id: 1,
            state: JobState::Complete,
        }],
        &backend,
    );
    assert_eq!(state.current_job_id, None);
}

/// §11.4 — post-processing progress shows while a job is being verified and
/// disappears when it is done; reports are kept per job.
#[test]
fn queue_state_tracks_post_processing() {
    let (backend, _cmd_rx) = handle_pair();
    let mut state = QueueState::default();

    state.handle_events(&[BackendEvent::PostProcessStarted { job_id: 7 }], &backend);
    assert!(state.pp_in_progress.contains(&7));
    assert_eq!(state.pp_progress.get(&7), Some(&(0, 0)));

    state.handle_events(
        &[BackendEvent::PostProcessProgress {
            job_id: 7,
            done: 4096,
            total: 8192,
        }],
        &backend,
    );
    assert_eq!(state.pp_progress.get(&7), Some(&(4096, 8192)));

    state.handle_events(
        &[BackendEvent::PostProcessDone {
            job_id: Some(7),
            report: sample_report("/tv"),
        }],
        &backend,
    );
    assert!(!state.pp_in_progress.contains(&7));
    assert!(!state.pp_progress.contains_key(&7));
    assert_eq!(state.pp_reports.len(), 1);
    assert_eq!(state.pp_reports[0].1.final_dir, PathBuf::from("/tv"));
    assert_eq!(
        state.pp_reports[0].1.status,
        turbonzb_core::PostProcessStatus::Complete
    );

    // A standalone run (no job id) still leaves a report behind.
    state.handle_events(
        &[BackendEvent::PostProcessDone {
            job_id: None,
            report: sample_report("/misc"),
        }],
        &backend,
    );
    assert_eq!(state.pp_reports.len(), 2);
    let seen: HashSet<Option<i64>> = state.pp_reports.iter().map(|(id, _)| *id).collect();
    assert!(seen.contains(&Some(7)) && seen.contains(&None));
}

/// §11.4 — details only land for the selected job, so a background refresh
/// cannot blank the pane the user is looking at.
#[test]
fn details_only_apply_to_the_selected_job() {
    let (backend, mut cmd_rx) = handle_pair();
    let mut state = QueueState {
        jobs: vec![queued_job(1, "alpha", 1, 2)],
        details_tab: DetailsTab::Files,
        ..Default::default()
    };

    state.select_job(1, &backend);
    assert_eq!(state.selected_job, Some(1));
    assert!(
        state.job_details.is_empty(),
        "selection clears stale details"
    );

    // The widget's two commands reach the backend, in order.
    let mut cmds = Vec::new();
    while let Ok(cmd) = cmd_rx.try_recv() {
        cmds.push(match cmd {
            BackendCmd::SetSelectedJob { job_id } => format!("select:{job_id:?}"),
            BackendCmd::GetJobDetails { job_id } => format!("details:{job_id}"),
            other => format!("{other:?}"),
        });
    }
    assert_eq!(cmds, vec!["select:Some(1)", "details:1"]);

    state.handle_events(
        &[BackendEvent::JobDetails {
            job_id: 1,
            files: vec![file_detail(1, "alpha.bin", 2, 1)],
        }],
        &backend,
    );
    assert_eq!(state.job_details.len(), 1);

    // Clearing the selection drops its details; a late reply for another
    // job then stays out of the way.
    state.clear_selection(&backend);
    assert!(state.job_details.is_empty(), "clearing drops the details");
    state.handle_events(
        &[BackendEvent::JobDetails {
            job_id: 99,
            files: vec![file_detail(9, "other.bin", 1, 1)],
        }],
        &backend,
    );
    assert!(state.job_details.is_empty(), "unselected reply is ignored");

    let mut cleared = false;
    while let Ok(cmd) = cmd_rx.try_recv() {
        match cmd {
            BackendCmd::SetSelectedJob { job_id: None } => cleared = true,
            other => panic!("unexpected command after clear: {other:?}"),
        }
    }
    assert!(cleared, "clearing tells the backend to stop refreshing");
}

/// §11.6 — settings survive a save/load round trip, and an unreadable or
/// partial file falls back to the wizard instead of crashing.
#[test]
fn settings_round_trip_and_fallback() {
    let dir = temp_dir("settings");
    let path = dir.join("config.json");

    let mut config = config_with(dir.join("downloads"), dir.join("turbonzb-queue.db"));
    config.max_connections = 6;
    config.save(&path).unwrap();

    let loaded = AppConfig::load(&path).expect("written config reads back");
    assert_eq!(loaded.servers.len(), 1);
    assert_eq!(loaded.max_connections, 6);
    assert_eq!(loaded.categories.len(), 1);
    assert_eq!(loaded.download_dir, dir.join("downloads"));
    assert!(loaded.completed_dir.ends_with("completed"));

    // Server entries convert into engine configs, keeping credentials.
    let servers = loaded.server_configs();
    assert_eq!(servers.len(), 1);
    assert_eq!(servers[0].host, "news.example.invalid");

    // Connection budget: sum of per-server counts, capped by the global
    // limit when set, and never below one socket.
    let mut capped = config.clone();
    capped.servers = vec![server("a", 4, 0), server("b", 8, 1)];
    capped.max_connections = 0;
    assert_eq!(capped.effective_max_connections(), 12, "0 means uncapped");
    capped.max_connections = 6;
    assert_eq!(capped.effective_max_connections(), 6, "global cap wins");
    capped.max_connections = 20;
    assert_eq!(
        capped.effective_max_connections(),
        12,
        "a cap above the server total does not invent sockets"
    );
    capped.servers = Vec::new();
    capped.max_connections = 0;
    assert_eq!(capped.effective_max_connections(), 1, "never zero");

    // Server order follows priority, not insertion order.
    let mut ordered = config.clone();
    ordered.servers = vec![server("later", 2, 5), server("first", 2, 1)];
    let names: Vec<String> = ordered
        .server_configs()
        .into_iter()
        .map(|s| s.host)
        .collect();
    assert_eq!(names, vec!["first", "later"]);

    // The wizard is skipped only once both halves are configured.
    assert!(!config.is_configured(), "indexers are still empty");
    config.indexers = vec![turbonzb_index::types::IndexerConfig {
        name: "mock".into(),
        url: "http://example.invalid/api".into(),
        api_key: "k".into(),
        max_concurrent: 2,
        timeout_s: 10,
        priority: 0,
    }];
    assert!(config.is_configured());

    // A truncated hand-edited file is not fatal: the app falls back to defaults.
    std::fs::write(&path, "{ \"servers\": [").unwrap();
    assert!(AppConfig::load(&path).is_none());

    // A missing file also means "run the wizard".
    assert!(AppConfig::load(&dir.join("absent.json")).is_none());
}

// --------------------------------------------------------- tiny test harness ---

/// Run a future on a throwaway runtime (the GUI's own runtime is owned by
/// `Backend::spawn`, so seeding uses its own).
fn tokio_handle<F: std::future::Future>(make: impl FnOnce() -> F) -> F::Output {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("tokio runtime")
        .block_on(make())
}

fn server(host: &str, max_connections: u32, priority: u32) -> ServerEntry {
    ServerEntry {
        host: host.into(),
        port: 119,
        tls: false,
        user: None,
        password: None,
        max_connections,
        priority,
    }
}

fn sample_report(final_dir: &str) -> turbonzb_core::PostProcessReport {
    turbonzb_core::PostProcessReport {
        verify: Some(turbonzb_core::par2::VerifyReport::default()),
        unpack: None,
        status: turbonzb_core::PostProcessStatus::Complete,
        final_dir: PathBuf::from(final_dir),
    }
}

fn queued_job(
    id: i64,
    name: &str,
    file_count: u32,
    total_segments: u32,
) -> turbonzb_core::queue::QueueJob {
    turbonzb_core::queue::QueueJob {
        id,
        name: name.into(),
        output_dir: PathBuf::from("/tmp/does-not-matter"),
        state: JobState::Queued,
        priority: id,
        file_count,
        files_done: 0,
        total_segments,
        segments_done: 0,
        total_bytes: 0,
        downloaded_bytes: 0,
        error: None,
        archive_password: None,
    }
}

fn file_detail(
    id: i64,
    filename: &str,
    segments: u32,
    done: u32,
) -> turbonzb_gui::backend::JobFileDetail {
    turbonzb_gui::backend::JobFileDetail {
        id,
        filename: filename.into(),
        segment_count: segments,
        segments_done: done,
        segments_missing: 0,
        total_bytes: 2000,
        downloaded_bytes: 1000,
        segments: (1..=segments)
            .map(|n| turbonzb_gui::backend::JobSegmentDetail {
                number: n,
                bytes: 1000,
                state: if n <= done {
                    SegmentState::Done
                } else {
                    SegmentState::Pending
                },
            })
            .collect(),
    }
}

/// Jobs in either pre-download state are eligible for another attempt.
trait JobStateHelpers {
    fn is_fetching_or_queued(&self) -> bool;
}

impl JobStateHelpers for turbonzb_core::queue::QueueJob {
    fn is_fetching_or_queued(&self) -> bool {
        matches!(self.state, JobState::Queued | JobState::Fetching)
    }
}
