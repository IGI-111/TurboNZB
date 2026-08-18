//! Persistent download queue backed by SQLite.
//!
//! Stores jobs, files, and per-segment state so that a download can be
//! killed and resumed at the article level. The schema is intentionally
//! simple — three tables: `jobs`, `files`, `segments`.

use std::path::{Path, PathBuf};
use std::str::FromStr;

use sqlx::sqlite::{SqliteConnectOptions, SqlitePoolOptions};
use sqlx::{Row, SqlitePool};
use tracing::debug;

use crate::error::{CoreError, Result};
use crate::nzb::Nzb;

/// The on-disk database file name.
pub const DB_FILENAME: &str = "nobz-queue.db";

/// Job lifecycle states persisted in the database.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum JobState {
    /// Waiting to be downloaded.
    Queued,
    /// Currently downloading.
    Downloading,
    /// User-paused; will not auto-resume.
    Paused,
    /// All files downloaded and assembled.
    Complete,
    /// Download failed irrecoverably.
    Failed,
}

impl JobState {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Queued => "queued",
            Self::Downloading => "downloading",
            Self::Paused => "paused",
            Self::Complete => "complete",
            Self::Failed => "failed",
        }
    }

    pub fn from_str_lossy(s: &str) -> Self {
        match s {
            "downloading" => Self::Downloading,
            "paused" => Self::Paused,
            "complete" => Self::Complete,
            "failed" => Self::Failed,
            _ => Self::Queued,
        }
    }
}

/// Per-segment state persisted in the database.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum SegmentState {
    /// Not yet attempted.
    Pending,
    /// Decoded successfully, CRC verified.
    Done,
    /// Missing on all servers (430 on every server).
    Missing,
    /// Decoded but CRC mismatch — corrupt on server.
    CrcMismatch,
    /// Failed for an unexpected reason (protocol error, decode error).
    Failed,
}

impl SegmentState {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::Done => "done",
            Self::Missing => "missing",
            Self::CrcMismatch => "crc_mismatch",
            Self::Failed => "failed",
        }
    }

    pub fn from_str_lossy(s: &str) -> Self {
        match s {
            "done" => Self::Done,
            "missing" => Self::Missing,
            "crc_mismatch" => Self::CrcMismatch,
            "failed" => Self::Failed,
            _ => Self::Pending,
        }
    }
}

/// A persisted job in the queue.
#[derive(Debug, Clone)]
pub struct QueueJob {
    pub id: i64,
    pub name: String,
    pub output_dir: PathBuf,
    pub state: JobState,
    pub priority: i64,
    /// Total files in the job.
    pub file_count: u32,
    /// Files completed (all segments done).
    pub files_done: u32,
    /// Total segments across all files.
    pub total_segments: u32,
    /// Segments completed (done or missing or crc_mismatch).
    pub segments_done: u32,
    /// Total bytes across all segments in the job.
    pub total_bytes: u64,
    /// Bytes downloaded so far (sum of done segment sizes).
    pub downloaded_bytes: u64,
}

/// A persisted file belonging to a job.
#[derive(Debug, Clone)]
pub struct QueueFile {
    pub id: i64,
    pub job_id: i64,
    pub filename: String,
    pub subject: String,
    pub poster: String,
    pub date: u64,
    pub segment_count: u32,
    /// Index of this file within the job (0-based).
    pub file_index: u32,
}

/// A persisted segment belonging to a file.
#[derive(Debug, Clone)]
pub struct QueueSegment {
    pub id: i64,
    pub file_id: i64,
    pub number: u32,
    pub message_id: String,
    pub bytes: u64,
    pub missing: bool,
    pub state: SegmentState,
}

/// The queue manager. Owns a SQLite connection pool.
pub struct QueueManager {
    pool: SqlitePool,
}

impl QueueManager {
    /// Open (or create) the queue database at `path`.
    pub async fn open(path: impl AsRef<Path>) -> Result<Self> {
        let path = path.as_ref().to_path_buf();
        if let Some(parent) = path.parent() {
            tokio::fs::create_dir_all(parent)
                .await
                .map_err(CoreError::from)?;
        }

        let opts = SqliteConnectOptions::from_str(&path.to_string_lossy())
            .map_err(|e| CoreError::Other(anyhow::anyhow!("sqlite options: {e}")))?
            .create_if_missing(true)
            .journal_mode(sqlx::sqlite::SqliteJournalMode::Wal)
            .busy_timeout(std::time::Duration::from_secs(5))
            .synchronous(sqlx::sqlite::SqliteSynchronous::Normal);

        let pool = SqlitePoolOptions::new()
            .max_connections(16)
            .connect_with(opts)
            .await
            .map_err(|e| CoreError::Other(anyhow::anyhow!("sqlite connect: {e}")))?;

        let mgr = Self { pool };
        mgr.init_schema().await?;
        Ok(mgr)
    }

    /// Open an in-memory database (for tests).
    pub async fn open_in_memory() -> Result<Self> {
        let pool = SqlitePool::connect("sqlite::memory:")
            .await
            .map_err(|e| CoreError::Other(anyhow::anyhow!("sqlite connect: {e}")))?;
        let mgr = Self { pool };
        mgr.init_schema().await?;
        Ok(mgr)
    }

    async fn init_schema(&self) -> Result<()> {
        sqlx::query(
            r#"
            CREATE TABLE IF NOT EXISTS jobs (
                id          INTEGER PRIMARY KEY AUTOINCREMENT,
                name        TEXT NOT NULL,
                output_dir  TEXT NOT NULL,
                state       TEXT NOT NULL DEFAULT 'queued',
                priority    INTEGER NOT NULL DEFAULT 0,
                created_at  INTEGER NOT NULL DEFAULT (strftime('%s','now')),
                file_count      INTEGER NOT NULL DEFAULT 0,
                files_done      INTEGER NOT NULL DEFAULT 0,
                total_segments  INTEGER NOT NULL DEFAULT 0,
                segments_done   INTEGER NOT NULL DEFAULT 0,
                total_bytes     INTEGER NOT NULL DEFAULT 0,
                downloaded_bytes INTEGER NOT NULL DEFAULT 0
            );

            CREATE TABLE IF NOT EXISTS files (
                id          INTEGER PRIMARY KEY AUTOINCREMENT,
                job_id      INTEGER NOT NULL REFERENCES jobs(id) ON DELETE CASCADE,
                file_index  INTEGER NOT NULL,
                filename    TEXT NOT NULL,
                subject     TEXT NOT NULL,
                poster      TEXT NOT NULL,
                date        INTEGER NOT NULL,
                segment_count INTEGER NOT NULL
            );

            CREATE TABLE IF NOT EXISTS segments (
                id          INTEGER PRIMARY KEY AUTOINCREMENT,
                file_id     INTEGER NOT NULL REFERENCES files(id) ON DELETE CASCADE,
                number      INTEGER NOT NULL,
                message_id  TEXT NOT NULL,
                bytes       INTEGER NOT NULL,
                missing     INTEGER NOT NULL DEFAULT 0,
                state       TEXT NOT NULL DEFAULT 'pending'
            );

        CREATE INDEX IF NOT EXISTS idx_files_job ON files(job_id);
        CREATE INDEX IF NOT EXISTS idx_segments_file ON segments(file_id);
        CREATE INDEX IF NOT EXISTS idx_jobs_state ON jobs(state);

        -- Only one job may be in the 'downloading' state at a time.
        -- This partial unique index makes that invariant impossible to
        -- violate at the database level, even across crashes/restarts.
        CREATE UNIQUE INDEX IF NOT EXISTS idx_one_downloading
            ON jobs(state) WHERE state = 'downloading';
        "#,
        )
        .execute(&self.pool)
        .await
        .map_err(|e| CoreError::Other(anyhow::anyhow!("schema init: {e}")))?;

        // Migration: add columns if they don't exist (for existing DBs
        // created before the total_bytes/downloaded_bytes columns).
        self.migrate_add_column("jobs", "total_bytes", "INTEGER NOT NULL DEFAULT 0")
            .await?;
        self.migrate_add_column("jobs", "downloaded_bytes", "INTEGER NOT NULL DEFAULT 0")
            .await?;

        debug!("queue schema initialized");
        Ok(())
    }

    /// Add a column to a table if it doesn't already exist. SQLite doesn't
    /// support IF NOT EXISTS for ALTER TABLE ADD COLUMN, so we check
    /// pragma_table_info first.
    async fn migrate_add_column(&self, table: &str, column: &str, decl: &str) -> Result<()> {
        let exists: i64 =
            sqlx::query_scalar("SELECT COUNT(*) FROM pragma_table_info(?) WHERE name = ?")
                .bind(table)
                .bind(column)
                .fetch_one(&self.pool)
                .await
                .map_err(|e| CoreError::Other(anyhow::anyhow!("pragma check: {e}")))?;

        if exists == 0 {
            let sql = format!("ALTER TABLE {table} ADD COLUMN {column} {decl}");
            sqlx::query(&sql)
                .execute(&self.pool)
                .await
                .map_err(|e| CoreError::Other(anyhow::anyhow!("migrate {column}: {e}")))?;
            debug!(table, column, "migration: added column");
        }
        Ok(())
    }

    /// Add a new job to the queue. Returns the job id.
    pub async fn add_job(
        &self,
        nzb: &Nzb,
        output_dir: impl Into<PathBuf>,
        priority: i64,
    ) -> Result<i64> {
        let output_dir = output_dir.into().to_string_lossy().to_string();
        let name = nzb
            .title()
            .map(str::to_string)
            .or_else(|| nzb.files.first().map(|f| f.filename()))
            .unwrap_or_else(|| "nobz-download".into());

        let conn = self
            .pool
            .acquire()
            .await
            .map_err(|e| CoreError::Other(anyhow::anyhow!("acquire conn: {e}")))?;

        // Begin a transaction — inserting 2000+ segments one-by-one
        // without a transaction is extremely slow (each INSERT is its
        // own implicit transaction + fsync).
        let mut tx = self
            .pool
            .begin()
            .await
            .map_err(|e| CoreError::Other(anyhow::anyhow!("begin tx: {e}")))?;

        let total_segments: u32 = nzb.files.iter().map(|f| f.segment_count).sum();
        let total_bytes: u64 = nzb
            .files
            .iter()
            .flat_map(|f| &f.segments)
            .map(|s| s.bytes)
            .sum();

        let job_id: i64 = sqlx::query(
            r#"INSERT INTO jobs (name, output_dir, priority, file_count, total_segments, total_bytes)
               VALUES (?1, ?2, ?3, ?4, ?5, ?6)"#,
        )
        .bind(&name)
        .bind(&output_dir)
        .bind(priority)
        .bind(nzb.files.len() as i64)
        .bind(total_segments as i64)
        .bind(total_bytes as i64)
        .execute(&mut *tx)
        .await
        .map_err(|e| CoreError::Other(anyhow::anyhow!("insert job: {e}")))?
        .last_insert_rowid();

        for (file_index, file) in nzb.files.iter().enumerate() {
            let file_id: i64 = sqlx::query(
                r#"INSERT INTO files (job_id, file_index, filename, subject, poster, date, segment_count)
                   VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)"#,
            )
            .bind(job_id)
            .bind(file_index as i64)
            .bind(file.filename())
            .bind(&file.subject)
            .bind(&file.poster)
            .bind(file.date as i64)
            .bind(file.segment_count as i64)
            .execute(&mut *tx)
            .await
            .map_err(|e| CoreError::Other(anyhow::anyhow!("insert file: {e}")))?
            .last_insert_rowid();

            for seg in &file.segments {
                sqlx::query(
                    r#"INSERT INTO segments (file_id, number, message_id, bytes, missing, state)
                       VALUES (?1, ?2, ?3, ?4, ?5, ?6)"#,
                )
                .bind(file_id)
                .bind(seg.number as i64)
                .bind(&seg.message_id)
                .bind(seg.bytes as i64)
                .bind(seg.missing as i64)
                .bind(if seg.missing {
                    SegmentState::Missing.as_str()
                } else {
                    SegmentState::Pending.as_str()
                })
                .execute(&mut *tx)
                .await
                .map_err(|e| CoreError::Other(anyhow::anyhow!("insert segment: {e}")))?;
            }
        }

        tx.commit()
            .await
            .map_err(|e| CoreError::Other(anyhow::anyhow!("commit tx: {e}")))?;
        drop(conn);

        debug!(job_id, "job added to queue");
        Ok(job_id)
    }

    /// List all jobs in priority order.
    pub async fn list_jobs(&self) -> Result<Vec<QueueJob>> {
        let rows = sqlx::query(
            r#"SELECT id, name, output_dir, state, priority, file_count, files_done,
                      total_segments, segments_done, total_bytes, downloaded_bytes
               FROM jobs ORDER BY priority ASC, id ASC"#,
        )
        .fetch_all(&self.pool)
        .await
        .map_err(|e| CoreError::Other(anyhow::anyhow!("list jobs: {e}")))?;

        let jobs = rows
            .into_iter()
            .map(|r| QueueJob {
                id: r.get("id"),
                name: r.get("name"),
                output_dir: PathBuf::from(r.get::<String, _>("output_dir")),
                state: JobState::from_str_lossy(r.get("state")),
                priority: r.get("priority"),
                file_count: r.get::<i64, _>("file_count") as u32,
                files_done: r.get::<i64, _>("files_done") as u32,
                total_segments: r.get::<i64, _>("total_segments") as u32,
                segments_done: r.get::<i64, _>("segments_done") as u32,
                total_bytes: r.get::<i64, _>("total_bytes") as u64,
                downloaded_bytes: r.get::<i64, _>("downloaded_bytes") as u64,
            })
            .collect();
        Ok(jobs)
    }

    /// Get a single job by id.
    pub async fn get_job(&self, job_id: i64) -> Result<QueueJob> {
        let row = sqlx::query(
            r#"SELECT id, name, output_dir, state, priority, file_count, files_done,
                      total_segments, segments_done, total_bytes, downloaded_bytes
               FROM jobs WHERE id = ?1"#,
        )
        .bind(job_id)
        .fetch_one(&self.pool)
        .await
        .map_err(|e| CoreError::Other(anyhow::anyhow!("get job: {e}")))?;

        Ok(QueueJob {
            id: row.get("id"),
            name: row.get::<String, _>("name"),
            output_dir: PathBuf::from(row.get::<String, _>("output_dir")),
            state: JobState::from_str_lossy(row.get("state")),
            priority: row.get("priority"),
            file_count: row.get::<i64, _>("file_count") as u32,
            files_done: row.get::<i64, _>("files_done") as u32,
            total_segments: row.get::<i64, _>("total_segments") as u32,
            segments_done: row.get::<i64, _>("segments_done") as u32,
            total_bytes: row.get::<i64, _>("total_bytes") as u64,
            downloaded_bytes: row.get::<i64, _>("downloaded_bytes") as u64,
        })
    }

    /// Update a job's state.
    pub async fn set_job_state(&self, job_id: i64, state: JobState) -> Result<()> {
        sqlx::query(r#"UPDATE jobs SET state = ?1 WHERE id = ?2"#)
            .bind(state.as_str())
            .bind(job_id)
            .execute(&self.pool)
            .await
            .map_err(|e| CoreError::Other(anyhow::anyhow!("set job state: {e}")))?;
        Ok(())
    }

    /// Atomically claim the single download slot for `job_id`. Returns
    /// `true` if this caller won the slot, `false` if the slot is already
    /// held by another job or the job is not in a claimable state (queued
    /// or paused). The partial unique index `idx_one_downloading`
    /// guarantees that at most one job can ever be in the 'downloading'
    /// state — a concurrent claim is rejected by SQLite.
    pub async fn claim_download_slot(&self, job_id: i64) -> Result<bool> {
        let result = sqlx::query(
            r#"UPDATE jobs SET state = 'downloading'
               WHERE id = ?1 AND state IN ('queued', 'paused')"#,
        )
        .bind(job_id)
        .execute(&self.pool)
        .await
        .map_err(|e| CoreError::Other(anyhow::anyhow!("claim download slot: {e}")))?;
        Ok(result.rows_affected() > 0)
    }

    /// Release the download slot, setting the job to `new_state`. Only
    /// has an effect if the job is currently 'downloading'. This is
    /// idempotent — safe to call multiple times.
    pub async fn release_download_slot(&self, job_id: i64, new_state: JobState) -> Result<()> {
        sqlx::query(r#"UPDATE jobs SET state = ?1 WHERE id = ?2 AND state = 'downloading'"#)
            .bind(new_state.as_str())
            .bind(job_id)
            .execute(&self.pool)
            .await
            .map_err(|e| CoreError::Other(anyhow::anyhow!("release download slot: {e}")))?;
        Ok(())
    }

    /// Get the id of the job currently in the 'downloading' state, if any.
    pub async fn current_downloading_job(&self) -> Result<Option<i64>> {
        let row = sqlx::query(r#"SELECT id FROM jobs WHERE state = 'downloading' LIMIT 1"#)
            .fetch_optional(&self.pool)
            .await
            .map_err(|e| CoreError::Other(anyhow::anyhow!("current downloading: {e}")))?;
        Ok(row.map(|r| r.get::<i64, _>("id")))
    }

    /// Reset all jobs in 'downloading' state back to 'queued'. Called at
    /// startup to recover from an unclean shutdown (crash, force-quit,
    /// power loss). After this call, the download slot is guaranteed
    /// empty and `claim_download_slot` can be used to start the next job.
    pub async fn recover_interrupted(&self) -> Result<u64> {
        let result = sqlx::query(r#"UPDATE jobs SET state = 'queued' WHERE state = 'downloading'"#)
            .execute(&self.pool)
            .await
            .map_err(|e| CoreError::Other(anyhow::anyhow!("recover interrupted: {e}")))?;
        Ok(result.rows_affected())
    }

    /// Reset segments in the 'failed' state back to 'pending' for a job,
    /// so they get retried on the next download attempt. Transient
    /// failures (connection timeouts, protocol errors) should get
    /// retried; 'crc_mismatch' segments are left alone because the
    /// article is corrupt on the server and retrying won't help.
    pub async fn reset_failed_segments(&self, job_id: i64) -> Result<u64> {
        let result = sqlx::query(
            r#"UPDATE segments SET state = 'pending'
               WHERE state = 'failed'
               AND file_id IN (SELECT id FROM files WHERE job_id = ?1)"#,
        )
        .bind(job_id)
        .execute(&self.pool)
        .await
        .map_err(|e| CoreError::Other(anyhow::anyhow!("reset failed segments: {e}")))?;
        Ok(result.rows_affected())
    }

    /// Update a job's priority (for reordering).
    pub async fn set_job_priority(&self, job_id: i64, priority: i64) -> Result<()> {
        sqlx::query(r#"UPDATE jobs SET priority = ?1 WHERE id = ?2"#)
            .bind(priority)
            .bind(job_id)
            .execute(&self.pool)
            .await
            .map_err(|e| CoreError::Other(anyhow::anyhow!("set priority: {e}")))?;
        Ok(())
    }

    /// Delete a job and all its files/segments (cascade).
    pub async fn delete_job(&self, job_id: i64) -> Result<()> {
        sqlx::query(r#"DELETE FROM jobs WHERE id = ?1"#)
            .bind(job_id)
            .execute(&self.pool)
            .await
            .map_err(|e| CoreError::Other(anyhow::anyhow!("delete job: {e}")))?;
        Ok(())
    }

    /// List all files for a job, in file_index order.
    pub async fn list_files(&self, job_id: i64) -> Result<Vec<QueueFile>> {
        let rows = sqlx::query(
            r#"SELECT id, job_id, file_index, filename, subject, poster, date, segment_count
               FROM files WHERE job_id = ?1 ORDER BY file_index ASC"#,
        )
        .bind(job_id)
        .fetch_all(&self.pool)
        .await
        .map_err(|e| CoreError::Other(anyhow::anyhow!("list files: {e}")))?;

        Ok(rows
            .into_iter()
            .map(|r| QueueFile {
                id: r.get("id"),
                job_id: r.get("job_id"),
                file_index: r.get::<i64, _>("file_index") as u32,
                filename: r.get("filename"),
                subject: r.get("subject"),
                poster: r.get("poster"),
                date: r.get::<i64, _>("date") as u64,
                segment_count: r.get::<i64, _>("segment_count") as u32,
            })
            .collect())
    }

    /// List all segments for a file, in number order.
    pub async fn list_segments(&self, file_id: i64) -> Result<Vec<QueueSegment>> {
        let rows = sqlx::query(
            r#"SELECT id, file_id, number, message_id, bytes, missing, state
               FROM segments WHERE file_id = ?1 ORDER BY number ASC"#,
        )
        .bind(file_id)
        .fetch_all(&self.pool)
        .await
        .map_err(|e| CoreError::Other(anyhow::anyhow!("list segments: {e}")))?;

        Ok(rows
            .into_iter()
            .map(|r| QueueSegment {
                id: r.get("id"),
                file_id: r.get("file_id"),
                number: r.get::<i64, _>("number") as u32,
                message_id: r.get("message_id"),
                bytes: r.get::<i64, _>("bytes") as u64,
                missing: r.get::<i64, _>("missing") != 0,
                state: SegmentState::from_str_lossy(r.get("state")),
            })
            .collect())
    }

    /// Update a segment's state. Called after each article fetch attempt.
    /// Does NOT refresh job-level aggregate counters — that's expensive
    /// (3 queries) and should only be done when a file completes or the
    /// job finishes. Use [`refresh_job_counts`] for that.
    pub async fn set_segment_state(
        &self,
        file_id: i64,
        segment_number: u32,
        state: SegmentState,
    ) -> Result<()> {
        sqlx::query(
            r#"UPDATE segments SET state = ?1
               WHERE file_id = ?2 AND number = ?3"#,
        )
        .bind(state.as_str())
        .bind(file_id)
        .bind(segment_number as i64)
        .execute(&self.pool)
        .await
        .map_err(|e| CoreError::Other(anyhow::anyhow!("set segment state: {e}")))?;

        Ok(())
    }

    /// Get all pending segments for a job (across all files), grouped by file.
    /// This is used by the engine to know what still needs downloading.
    pub async fn pending_segments(
        &self,
        job_id: i64,
    ) -> Result<Vec<(QueueFile, Vec<QueueSegment>)>> {
        let files = self.list_files(job_id).await?;
        let mut result = Vec::new();
        for file in &files {
            let segments = self.list_segments(file.id).await?;
            let pending: Vec<QueueSegment> = segments
                .into_iter()
                .filter(|s| s.state == SegmentState::Pending && !s.missing)
                .collect();
            if !pending.is_empty() {
                result.push((file.clone(), pending));
            }
        }
        Ok(result)
    }

    /// Mark a segment as hopeless (missing on all servers). This prevents
    /// retries on restart.
    pub async fn mark_segment_hopeless(&self, file_id: i64, segment_number: u32) -> Result<()> {
        self.set_segment_state(file_id, segment_number, SegmentState::Missing)
            .await
    }

    /// Mark a segment as CRC mismatch.
    pub async fn mark_segment_crc_mismatch(&self, file_id: i64, segment_number: u32) -> Result<()> {
        self.set_segment_state(file_id, segment_number, SegmentState::CrcMismatch)
            .await
    }

    /// Recompute the aggregate counters on the job from segment states.
    /// Call this after a file completes, not after every single segment.
    pub async fn refresh_job_counts(&self, file_id: i64) -> Result<()> {
        // Get the job_id for this file.
        let job_id: i64 = sqlx::query("SELECT job_id FROM files WHERE id = ?1")
            .bind(file_id)
            .fetch_one(&self.pool)
            .await
            .map_err(|e| CoreError::Other(anyhow::anyhow!("get job_id: {e}")))?
            .get("job_id");

        // Count done segments across the whole job.
        let segments_done: i64 = sqlx::query(
            r#"SELECT COUNT(*) as cnt FROM segments s
               JOIN files f ON s.file_id = f.id
               WHERE f.job_id = ?1
               AND s.state IN ('done', 'missing', 'crc_mismatch', 'failed')"#,
        )
        .bind(job_id)
        .fetch_one(&self.pool)
        .await
        .map_err(|e| CoreError::Other(anyhow::anyhow!("count segments: {e}")))?
        .get("cnt");

        // Count files where all segments are done/missing/etc.
        let files_done: i64 = sqlx::query(
            r#"SELECT COUNT(*) as cnt FROM files f
               WHERE f.job_id = ?1
               AND NOT EXISTS (
                   SELECT 1 FROM segments s
                   WHERE s.file_id = f.id
                   AND s.state = 'pending'
                   AND s.missing = 0
               )"#,
        )
        .bind(job_id)
        .fetch_one(&self.pool)
        .await
        .map_err(|e| CoreError::Other(anyhow::anyhow!("count files done: {e}")))?
        .get("cnt");

        // Sum bytes of done segments for download speed/progress.
        let downloaded_bytes: i64 = sqlx::query(
            r#"SELECT COALESCE(SUM(s.bytes), 0) as cnt FROM segments s
               JOIN files f ON s.file_id = f.id
               WHERE f.job_id = ?1
               AND s.state = 'done'"#,
        )
        .bind(job_id)
        .fetch_one(&self.pool)
        .await
        .map_err(|e| CoreError::Other(anyhow::anyhow!("sum downloaded bytes: {e}")))?
        .get("cnt");

        sqlx::query(
            r#"UPDATE jobs SET segments_done = ?1, files_done = ?2, downloaded_bytes = ?3 WHERE id = ?4"#,
        )
        .bind(segments_done)
        .bind(files_done)
        .bind(downloaded_bytes)
        .bind(job_id)
        .execute(&self.pool)
        .await
        .map_err(|e| CoreError::Other(anyhow::anyhow!("update counts: {e}")))?;

        Ok(())
    }

    /// Check if a file has any pending (non-missing) segments left.
    pub async fn file_has_pending(&self, file_id: i64) -> Result<bool> {
        let count: i64 = sqlx::query(
            r#"SELECT COUNT(*) as cnt FROM segments
               WHERE file_id = ?1 AND state = 'pending' AND missing = 0"#,
        )
        .bind(file_id)
        .fetch_one(&self.pool)
        .await
        .map_err(|e| CoreError::Other(anyhow::anyhow!("check pending: {e}")))?
        .get("cnt");
        Ok(count > 0)
    }

    /// Get the next queued job (highest priority, oldest).
    pub async fn next_queued_job(&self) -> Result<Option<QueueJob>> {
        let row = sqlx::query(
            r#"SELECT id, name, output_dir, state, priority, file_count, files_done,
                      total_segments, segments_done, total_bytes, downloaded_bytes
               FROM jobs WHERE state = 'queued'
               ORDER BY priority ASC, id ASC LIMIT 1"#,
        )
        .fetch_optional(&self.pool)
        .await
        .map_err(|e| CoreError::Other(anyhow::anyhow!("next queued: {e}")))?;

        Ok(row.map(|r| QueueJob {
            id: r.get("id"),
            name: r.get::<String, _>("name"),
            output_dir: PathBuf::from(r.get::<String, _>("output_dir")),
            state: JobState::from_str_lossy(r.get("state")),
            priority: r.get("priority"),
            file_count: r.get::<i64, _>("file_count") as u32,
            files_done: r.get::<i64, _>("files_done") as u32,
            total_segments: r.get::<i64, _>("total_segments") as u32,
            segments_done: r.get::<i64, _>("segments_done") as u32,
            total_bytes: r.get::<i64, _>("total_bytes") as u64,
            downloaded_bytes: r.get::<i64, _>("downloaded_bytes") as u64,
        }))
    }

    /// Close the database pool.
    pub async fn close(self) {
        self.pool.close().await;
    }
}
