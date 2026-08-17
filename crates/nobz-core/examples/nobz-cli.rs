//! CLI harness: download an NZB using the persistent queue.
//!
//! Usage:
//!   nobz-cli download --nzb <path> --out <dir> --host <host> --port <port> \
//!     [--tls] [--user <user> --pass <pass>] [--connections N]
//!   nobz-cli list
//!   nobz-cli resume --job <id> --host <host> --port <port> \
//!     [--tls] [--user <user> --pass <pass>] [--connections N]
//!
//! The queue DB is stored at `--db` (default: `nobz-queue.db` in the current
//! directory). Jobs survive restarts — run `nobz-cli resume` to continue a
//! killed download at the article level.

use std::path::PathBuf;
use std::sync::Arc;

use clap::{Parser, Subcommand};
use nobz_core::engine::{Engine, ProgressEvent};
use nobz_core::nntp::ServerConfig;
use nobz_core::nzb;
use nobz_core::postprocess::{PostProcessConfig, post_process};
use nobz_core::queue::QueueManager;
use tokio::sync::mpsc;

#[derive(Parser)]
#[command(name = "nobz-cli", about = "Download harness for nobz")]
struct Cli {
    /// Path to the queue database.
    #[arg(long, default_value = "nobz-queue.db")]
    db: PathBuf,

    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Add a download job to the queue and run it.
    Download {
        /// Path to the .nzb file.
        #[arg(long)]
        nzb: PathBuf,
        /// Output directory for decoded files.
        #[arg(long)]
        out: PathBuf,
        /// NNTP server hostname.
        #[arg(long)]
        host: String,
        /// NNTP server port (563 for TLS, 119 for plaintext).
        #[arg(long)]
        port: u16,
        /// Use implicit TLS (port 563).
        #[arg(long)]
        tls: bool,
        /// AUTHINFO username.
        #[arg(long)]
        user: Option<String>,
        /// AUTHINFO password.
        #[arg(long)]
        pass: Option<String>,
        /// Number of simultaneous connections.
        #[arg(long, default_value = "8")]
        connections: usize,
        /// Run post-processing (PAR2 verify + unpack) after download.
        #[arg(long)]
        post_process: bool,
        /// Category subfolder for completed files (e.g. "tv", "movies").
        #[arg(long)]
        category: Option<String>,
        /// Directory for completed/unpacked files (default: same as --out).
        #[arg(long)]
        completed_dir: Option<PathBuf>,
        /// Password for encrypted archives.
        #[arg(long)]
        archive_password: Option<String>,
        /// Skip PAR2 verification.
        #[arg(long)]
        skip_verify: bool,
        /// Delete archive files and temp dirs after successful unpack.
        #[arg(long, default_value = "true")]
        cleanup: bool,
    },
    /// List all jobs in the queue.
    List,
    /// Resume a paused/failed job.
    Resume {
        /// Job id to resume.
        #[arg(long)]
        job: i64,
        /// NNTP server hostname.
        #[arg(long)]
        host: String,
        /// NNTP server port.
        #[arg(long)]
        port: u16,
        /// Use implicit TLS.
        #[arg(long)]
        tls: bool,
        /// AUTHINFO username.
        #[arg(long)]
        user: Option<String>,
        /// AUTHINFO password.
        #[arg(long)]
        pass: Option<String>,
        /// Number of simultaneous connections.
        #[arg(long, default_value = "8")]
        connections: usize,
    },
    /// Pause a job.
    Pause {
        #[arg(long)]
        job: i64,
    },
    /// Delete a job from the queue.
    Delete {
        #[arg(long)]
        job: i64,
    },
    /// Run post-processing (PAR2 verify + unpack) on a directory.
    Postprocess {
        /// Directory containing downloaded files.
        #[arg(long)]
        dir: PathBuf,
        /// Directory for completed/unpacked files.
        #[arg(long)]
        completed_dir: PathBuf,
        /// Category subfolder (e.g. "tv", "movies").
        #[arg(long)]
        category: Option<String>,
        /// Password for encrypted archives.
        #[arg(long)]
        archive_password: Option<String>,
        /// Skip PAR2 verification.
        #[arg(long)]
        skip_verify: bool,
        /// Delete archive files and temp dirs after successful unpack.
        #[arg(long, default_value = "true")]
        cleanup: bool,
    },
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();

    let cli = Cli::parse();
    let queue = Arc::new(QueueManager::open(&cli.db).await?);

    match cli.command {
        Command::Download {
            nzb,
            out,
            host,
            port,
            tls,
            user,
            pass,
            connections,
            post_process: do_post_process,
            category,
            completed_dir,
            archive_password,
            skip_verify,
            cleanup,
        } => {
            let nzb_bytes = std::fs::read(&nzb)?;
            let nzb = nzb::parse(&nzb_bytes)?;
            println!(
                "NZB: {} ({} files)",
                nzb.title().unwrap_or("(untitled)"),
                nzb.files.len()
            );
            for f in &nzb.files {
                println!(
                    "  - {} ({} segments, {} missing)",
                    f.filename(),
                    f.segment_count,
                    f.missing_indices().len()
                );
            }

            let job_id = queue.add_job(&nzb, &out, 0).await?;
            println!("Job {job_id} added to queue.");

            let cfg = ServerConfig {
                host,
                port,
                tls,
                user,
                password: pass,
                max_connections: connections as u32,
                priority: 0,
            };
            let engine = Arc::new(Engine::new(vec![cfg], connections));

            let (tx, mut rx) = mpsc::unbounded_channel();
            let q = Arc::clone(&queue);
            let runner = tokio::spawn(async move { engine.run_job(q, job_id, tx).await });

            while let Some(ev) = rx.recv().await {
                match ev {
                    ProgressEvent::FileStarted { filename, segments } => {
                        println!("[file] {filename} ({segments} segments)");
                    }
                    ProgressEvent::SegmentDone {
                        filename,
                        segment,
                        status,
                        bytes,
                    } => {
                        println!("[seg]  {filename} #{segment} {status:?} ({bytes} bytes)");
                    }
                    ProgressEvent::FileCompleted {
                        filename,
                        path,
                        missing,
                        crc_mismatches,
                    } => {
                        println!(
                            "[done] {filename} -> {} (missing={missing}, crc={crc_mismatches})",
                            path.display()
                        );
                    }
                    ProgressEvent::ArticleError {
                        filename,
                        segment,
                        error,
                    } => {
                        eprintln!("[err]  {filename} #{segment}: {error}");
                    }
                    ProgressEvent::JobFinished { completed, failed } => {
                        println!("[job]  completed={completed} failed={failed}");
                    }
                }
            }

            runner.await??;

            // Post-process if requested.
            if do_post_process {
                let completed = completed_dir.clone().unwrap_or_else(|| out.clone());
                // Use NZB metadata password if no explicit password was given.
                let password =
                    archive_password.or_else(|| nzb.passwords().first().map(|s| s.to_string()));
                if password.is_some() {
                    println!("Using archive password from NZB metadata");
                }
                println!("\n=== Post-processing ===");
                let pp_config = PostProcessConfig {
                    download_dir: out.clone(),
                    completed_dir: completed,
                    category,
                    cleanup_archives: cleanup,
                    archive_password: password,
                    skip_verify,
                };
                match post_process(pp_config).await {
                    Ok(report) => {
                        if let Some(vr) = &report.verify {
                            println!(
                                "PAR2: {} healthy, {} damaged, {} missing, {} recovery slices",
                                vr.healthy, vr.damaged, vr.missing, vr.recovery_slices
                            );
                        } else {
                            println!("PAR2: skipped (no .par2 files)");
                        }
                        if let Some(ur) = &report.unpack {
                            println!(
                                "Unpacked: {} files (encrypted={})",
                                ur.extracted_files.len(),
                                ur.was_encrypted
                            );
                        }
                        println!("Status: {:?}", report.status);
                        println!("Final dir: {}", report.final_dir.display());
                    }
                    Err(e) => {
                        eprintln!("Post-processing failed: {e}");
                    }
                }
            }
        }
        Command::List => {
            let jobs = queue.list_jobs().await?;
            if jobs.is_empty() {
                println!("Queue is empty.");
                return Ok(());
            }
            println!(
                "{:<5} {:<30} {:<12} {:<10} {:<10}",
                "ID", "Name", "State", "Files", "Segments"
            );
            for job in &jobs {
                println!(
                    "{:<5} {:<30} {:<12} {}/{}       {}/{}",
                    job.id,
                    job.name,
                    job.state.as_str(),
                    job.files_done,
                    job.file_count,
                    job.segments_done,
                    job.total_segments,
                );
            }
        }
        Command::Resume {
            job,
            host,
            port,
            tls,
            user,
            pass,
            connections,
        } => {
            let cfg = ServerConfig {
                host,
                port,
                tls,
                user,
                password: pass,
                max_connections: connections as u32,
                priority: 0,
            };
            let engine = Arc::new(Engine::new(vec![cfg], connections));

            let (tx, mut rx) = mpsc::unbounded_channel();
            let q = Arc::clone(&queue);
            let runner = tokio::spawn(async move { engine.run_job(q, job, tx).await });

            println!("Resuming job {job}...");
            while let Some(ev) = rx.recv().await {
                match ev {
                    ProgressEvent::FileStarted { filename, segments } => {
                        println!("[file] {filename} ({segments} pending segments)");
                    }
                    ProgressEvent::SegmentDone {
                        filename,
                        segment,
                        status,
                        bytes,
                    } => {
                        println!("[seg]  {filename} #{segment} {status:?} ({bytes} bytes)");
                    }
                    ProgressEvent::FileCompleted {
                        filename,
                        path,
                        missing,
                        crc_mismatches,
                    } => {
                        println!(
                            "[done] {filename} -> {} (missing={missing}, crc={crc_mismatches})",
                            path.display()
                        );
                    }
                    ProgressEvent::ArticleError {
                        filename,
                        segment,
                        error,
                    } => {
                        eprintln!("[err]  {filename} #{segment}: {error}");
                    }
                    ProgressEvent::JobFinished { completed, failed } => {
                        println!("[job]  completed={completed} failed={failed}");
                    }
                }
            }

            runner.await??;
        }
        Command::Pause { job } => {
            queue
                .set_job_state(job, nobz_core::queue::JobState::Paused)
                .await?;
            println!("Job {job} paused.");
        }
        Command::Delete { job } => {
            queue.delete_job(job).await?;
            println!("Job {job} deleted.");
        }
        Command::Postprocess {
            dir,
            completed_dir,
            category,
            archive_password,
            skip_verify,
            cleanup,
        } => {
            println!("=== Post-processing {} ===", dir.display());
            let pp_config = PostProcessConfig {
                download_dir: dir,
                completed_dir,
                category,
                cleanup_archives: cleanup,
                archive_password,
                skip_verify,
            };
            match post_process(pp_config).await {
                Ok(report) => {
                    if let Some(vr) = &report.verify {
                        println!(
                            "PAR2: {} healthy, {} damaged, {} missing, {} recovery slices (repairable={})",
                            vr.healthy, vr.damaged, vr.missing, vr.recovery_slices, vr.repairable
                        );
                        for (filename, status) in &vr.files {
                            if status != &nobz_core::par2::VerifyStatus::Ok {
                                println!("  {status:?}: {filename}");
                            }
                        }
                    } else {
                        println!("PAR2: skipped (no .par2 files)");
                    }
                    if let Some(ur) = &report.unpack {
                        println!(
                            "Unpacked: {} files (encrypted={})",
                            ur.extracted_files.len(),
                            ur.was_encrypted
                        );
                    }
                    println!("Status: {:?}", report.status);
                    println!("Final dir: {}", report.final_dir.display());
                }
                Err(e) => {
                    eprintln!("Post-processing failed: {e}");
                }
            }
        }
    }

    Ok(())
}
