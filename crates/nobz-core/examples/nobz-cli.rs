//! M1 CLI harness: download an NZB from a single NNTP server.
//!
//! Usage:
//!   nobz-cli --nzb <path> --out <dir> --host <host> --port <port> \
//!            [--tls] [--user <user> --pass <pass>] [--connections N]
//!
//! This is a developer/testing harness, not the v1 product. The real CLI
//! surface ships as part of the GUI binary (M5).

use std::path::PathBuf;

use clap::Parser;
use nobz_core::engine::{DownloadJob, Engine, ProgressEvent};
use nobz_core::nntp::ServerConfig;
use nobz_core::nzb;
use tokio::sync::mpsc;

#[derive(Debug, Parser)]
#[command(name = "nobz-cli", about = "M1 download harness for nobz")]
struct Args {
    /// Path to the .nzb file to download.
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
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();

    let args = Args::parse();

    let nzb_bytes = std::fs::read(&args.nzb)?;
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

    let job = DownloadJob::from_nzb(&nzb, &args.out);

    let cfg = ServerConfig {
        host: args.host,
        port: args.port,
        tls: args.tls,
        user: args.user,
        password: args.pass,
        max_connections: args.connections as u32,
        priority: 0,
    };
    let engine = Engine::new(vec![cfg], args.connections);

    let (tx, mut rx) = mpsc::unbounded_channel();
    let runner = tokio::spawn(async move { engine.run(job, tx).await });

    let mut started = 0u64;
    let mut done = 0u64;
    while let Some(ev) = rx.recv().await {
        match ev {
            ProgressEvent::FileStarted { filename, segments } => {
                started += 1;
                println!("[file] {filename} ({segments} segments)");
            }
            ProgressEvent::SegmentDone {
                filename,
                segment,
                status,
                bytes,
            } => {
                done += 1;
                println!("[seg]  {filename} #{segment} {status:?} ({bytes} bytes)",);
            }
            ProgressEvent::FileCompleted {
                filename,
                path,
                missing,
                crc_mismatches,
            } => {
                println!(
                    "[done] {filename} -> {} (missing={missing}, crc_mismatch={crc_mismatches})",
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
                println!(
                    "[job]  completed={completed} failed={failed} (segments: {done}/{started} files)"
                );
            }
        }
    }

    runner.await??;
    Ok(())
}
