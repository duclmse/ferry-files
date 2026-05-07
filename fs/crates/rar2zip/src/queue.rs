use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use r2d2::Pool;
use r2d2_sqlite::SqliteConnectionManager;
use uuid::Uuid;

use crate::error::{AppError, Result};

// ---------------------------------------------------------------------------

const SCHEMA: &str = "
PRAGMA journal_mode=WAL;
PRAGMA synchronous=NORMAL;

CREATE TABLE IF NOT EXISTS jobs (
    id           TEXT PRIMARY KEY,
    file_path    TEXT NOT NULL,
    file_hash    TEXT NOT NULL,
    status       TEXT NOT NULL CHECK(status IN
                 ('pending','converting','transferring','done','failed')),
    strategy     TEXT NOT NULL CHECK(strategy IN ('disk','stream')),
    destinations TEXT NOT NULL,
    retry_count  INTEGER NOT NULL DEFAULT 0,
    created_at   INTEGER NOT NULL,
    updated_at   INTEGER NOT NULL,
    error        TEXT
);

CREATE TABLE IF NOT EXISTS multipart_groups (
    group_id   TEXT NOT NULL,
    part_path  TEXT NOT NULL,
    arrived_at INTEGER NOT NULL,
    PRIMARY KEY (group_id, part_path)
);

CREATE INDEX IF NOT EXISTS idx_jobs_status ON jobs(status);
CREATE INDEX IF NOT EXISTS idx_jobs_hash   ON jobs(file_hash);
";

// ---------------------------------------------------------------------------
// Enums
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Status {
    Pending,
    Converting,
    Transferring,
    Done,
    Failed,
}

impl Status {
    pub fn as_str(&self) -> &'static str {
        match self {
            Status::Pending      => "pending",
            Status::Converting   => "converting",
            Status::Transferring => "transferring",
            Status::Done         => "done",
            Status::Failed       => "failed",
        }
    }

    fn from_str(s: &str) -> Result<Self> {
        match s {
            "pending"      => Ok(Status::Pending),
            "converting"   => Ok(Status::Converting),
            "transferring" => Ok(Status::Transferring),
            "done"         => Ok(Status::Done),
            "failed"       => Ok(Status::Failed),
            other => Err(AppError::Config(format!("unknown status: {other}"))),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Strategy {
    Disk,
    Stream,
}

impl Strategy {
    pub fn as_str(&self) -> &'static str {
        match self {
            Strategy::Disk   => "disk",
            Strategy::Stream => "stream",
        }
    }

    fn from_str(s: &str) -> Result<Self> {
        match s {
            "disk"   => Ok(Strategy::Disk),
            "stream" => Ok(Strategy::Stream),
            other => Err(AppError::Config(format!("unknown strategy: {other}"))),
        }
    }
}

// ---------------------------------------------------------------------------
// Job
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
pub struct Job {
    pub id:           String,
    pub file_path:    PathBuf,
    pub file_hash:    String,
    pub status:       Status,
    pub strategy:     Strategy,
    pub destinations: Vec<String>,
    pub retry_count:  u32,
    pub created_at:   i64,
    pub updated_at:   i64,
    pub error:        Option<String>,
}

// ---------------------------------------------------------------------------
// JobQueue
// ---------------------------------------------------------------------------

pub struct JobQueue {
    pool: Pool<SqliteConnectionManager>,
}

impl JobQueue {
    pub fn new(db_path: &Path) -> Result<Self> {
        let manager = SqliteConnectionManager::file(db_path).with_init(|conn| {
            conn.execute_batch(
                "PRAGMA journal_mode=WAL;
                 PRAGMA synchronous=NORMAL;",
            )?;
            Ok(())
        });
        let pool = r2d2::Pool::builder()
            .max_size(16)
            .build(manager)
            .map_err(|e| AppError::Config(e.to_string()))?;

        let conn = pool.get().map_err(|e| AppError::Config(e.to_string()))?;
        conn.execute_batch(SCHEMA)?;

        Ok(Self { pool })
    }

    fn get_conn(&self) -> Result<r2d2::PooledConnection<SqliteConnectionManager>> {
        self.pool.get().map_err(|e| AppError::Config(e.to_string()))
    }

    // -----------------------------------------------------------------------
    // Job operations
    // -----------------------------------------------------------------------

    pub fn enqueue(
        &self,
        file_path: &Path,
        file_hash: &str,
        strategy: Strategy,
        destinations: &[String],
    ) -> Result<String> {
        let id = Uuid::new_v4().to_string();
        let now = now_secs();
        let dests_json = serde_json::to_string(destinations)
            .map_err(|e| AppError::Config(e.to_string()))?;
        let path_str = file_path.to_string_lossy();

        let conn = self.get_conn()?;
        conn.execute(
            "INSERT INTO jobs
             (id, file_path, file_hash, status, strategy, destinations,
              retry_count, created_at, updated_at, error)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, 0, ?7, ?7, NULL)",
            rusqlite::params![
                &id,
                path_str.as_ref(),
                file_hash,
                Status::Pending.as_str(),
                strategy.as_str(),
                &dests_json,
                now,
            ],
        )?;
        Ok(id)
    }

    pub fn set_status(&self, id: &str, status: Status, error: Option<&str>) -> Result<()> {
        let now = now_secs();
        let conn = self.get_conn()?;
        conn.execute(
            "UPDATE jobs SET status = ?1, error = ?2, updated_at = ?3 WHERE id = ?4",
            rusqlite::params![status.as_str(), error, now, id],
        )?;
        Ok(())
    }

    pub fn increment_retry(&self, id: &str) -> Result<()> {
        let now = now_secs();
        let conn = self.get_conn()?;
        conn.execute(
            "UPDATE jobs SET retry_count = retry_count + 1, updated_at = ?1 WHERE id = ?2",
            rusqlite::params![now, id],
        )?;
        Ok(())
    }

    pub fn get_stale_jobs(&self) -> Result<Vec<Job>> {
        let conn = self.get_conn()?;
        let mut stmt = conn.prepare(
            "SELECT id, file_path, file_hash, status, strategy, destinations,
                    retry_count, created_at, updated_at, error
             FROM jobs WHERE status IN ('converting', 'transferring')",
        )?;
        rows_to_jobs(&mut stmt, [])
    }

    pub fn get_failed_jobs(&self) -> Result<Vec<Job>> {
        let conn = self.get_conn()?;
        let mut stmt = conn.prepare(
            "SELECT id, file_path, file_hash, status, strategy, destinations,
                    retry_count, created_at, updated_at, error
             FROM jobs WHERE status = 'failed'",
        )?;
        rows_to_jobs(&mut stmt, [])
    }

    pub fn reset_failed(&self) -> Result<usize> {
        let now = now_secs();
        let conn = self.get_conn()?;
        let n = conn.execute(
            "UPDATE jobs SET status = 'pending', retry_count = 0,
                             error = NULL, updated_at = ?1
             WHERE status = 'failed'",
            rusqlite::params![now],
        )?;
        Ok(n)
    }

    pub fn is_duplicate(&self, file_hash: &str) -> Result<bool> {
        let conn = self.get_conn()?;
        let count: i64 = conn.query_row(
            "SELECT COUNT(*) FROM jobs WHERE file_hash = ? AND status = 'done'",
            [file_hash],
            |row| row.get(0),
        )?;
        Ok(count > 0)
    }

    pub fn queue_depth(&self) -> Result<HashMap<String, i64>> {
        let conn = self.get_conn()?;
        let mut stmt =
            conn.prepare("SELECT status, COUNT(*) FROM jobs GROUP BY status")?;
        let rows = stmt.query_map([], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)?))
        })?;
        let mut map = HashMap::new();
        for r in rows {
            let (status, count) = r?;
            map.insert(status, count);
        }
        Ok(map)
    }

    pub fn get_pending_jobs(&self, limit: usize) -> Result<Vec<Job>> {
        let conn = self.get_conn()?;
        let mut stmt = conn.prepare(
            "SELECT id, file_path, file_hash, status, strategy, destinations,
                    retry_count, created_at, updated_at, error
             FROM jobs WHERE status = 'pending' ORDER BY created_at ASC LIMIT ?",
        )?;
        rows_to_jobs(&mut stmt, rusqlite::params![limit as i64])
    }

    pub fn recent_jobs(&self, limit: usize) -> Result<Vec<Job>> {
        let conn = self.get_conn()?;
        let mut stmt = conn.prepare(
            "SELECT id, file_path, file_hash, status, strategy, destinations,
                    retry_count, created_at, updated_at, error
             FROM jobs WHERE status IN ('done', 'failed')
             ORDER BY updated_at DESC LIMIT ?",
        )?;
        rows_to_jobs(&mut stmt, rusqlite::params![limit as i64])
    }

    // -----------------------------------------------------------------------
    // Multipart operations
    // -----------------------------------------------------------------------

    pub fn add_part(&self, group_id: &str, part_path: &Path) -> Result<()> {
        let now = now_secs();
        let path_str = part_path.to_string_lossy();
        let conn = self.get_conn()?;
        conn.execute(
            "INSERT OR IGNORE INTO multipart_groups (group_id, part_path, arrived_at)
             VALUES (?1, ?2, ?3)",
            rusqlite::params![group_id, path_str.as_ref(), now],
        )?;
        Ok(())
    }

    pub fn get_parts(&self, group_id: &str) -> Result<Vec<PathBuf>> {
        let conn = self.get_conn()?;
        let mut stmt =
            conn.prepare("SELECT part_path FROM multipart_groups WHERE group_id = ?")?;
        let rows = stmt.query_map([group_id], |row| row.get::<_, String>(0))?;
        let mut paths = Vec::new();
        for r in rows {
            paths.push(PathBuf::from(r?));
        }
        Ok(paths)
    }

    pub fn clear_parts(&self, group_id: &str) -> Result<()> {
        let conn = self.get_conn()?;
        conn.execute(
            "DELETE FROM multipart_groups WHERE group_id = ?",
            [group_id],
        )?;
        Ok(())
    }

    /// Return group IDs where the earliest `arrived_at` is older than
    /// `timeout_hours` ago. Returns empty if `timeout_hours == 0`.
    pub fn get_expired_groups(&self, timeout_hours: u64) -> Result<Vec<String>> {
        if timeout_hours == 0 {
            return Ok(Vec::new());
        }
        let cutoff = now_secs() - (timeout_hours as i64 * 3600);
        let conn = self.get_conn()?;
        let mut stmt = conn.prepare(
            "SELECT group_id FROM multipart_groups
             GROUP BY group_id
             HAVING MIN(arrived_at) < ?",
        )?;
        let rows = stmt.query_map(rusqlite::params![cutoff], |row| {
            row.get::<_, String>(0)
        })?;
        let mut groups = Vec::new();
        for r in rows {
            groups.push(r?);
        }
        Ok(groups)
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn now_secs() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs() as i64
}

fn row_to_job(row: &rusqlite::Row<'_>) -> rusqlite::Result<Job> {
    let status_str: String = row.get(3)?;
    let strategy_str: String = row.get(4)?;
    let dests_json: String = row.get(5)?;

    let status = Status::from_str(&status_str)
        .unwrap_or(Status::Failed);
    let strategy = Strategy::from_str(&strategy_str)
        .unwrap_or(Strategy::Disk);
    let destinations: Vec<String> =
        serde_json::from_str(&dests_json).unwrap_or_default();

    Ok(Job {
        id:           row.get(0)?,
        file_path:    PathBuf::from(row.get::<_, String>(1)?),
        file_hash:    row.get(2)?,
        status,
        strategy,
        destinations,
        retry_count:  row.get::<_, i64>(6)? as u32,
        created_at:   row.get(7)?,
        updated_at:   row.get(8)?,
        error:        row.get(9)?,
    })
}

fn rows_to_jobs(
    stmt: &mut rusqlite::Statement<'_>,
    params: impl rusqlite::Params,
) -> Result<Vec<Job>> {
    let rows = stmt.query_map(params, row_to_job)?;
    rows.map(|r| r.map_err(AppError::Database)).collect()
}

// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    fn make_queue() -> JobQueue {
        JobQueue::new(Path::new(":memory:")).unwrap()
    }

    fn enqueue_one(q: &JobQueue, hash: &str) -> String {
        q.enqueue(
            Path::new("/watch/file.rar"),
            hash,
            Strategy::Disk,
            &["host1".into()],
        )
        .unwrap()
    }

    #[test]
    fn enqueue_set_status_stale_jobs() {
        let q = make_queue();
        let id = enqueue_one(&q, "abc123");
        q.set_status(&id, Status::Converting, None).unwrap();
        let stale = q.get_stale_jobs().unwrap();
        assert_eq!(stale.len(), 1);
        assert_eq!(stale[0].id, id);
        assert_eq!(stale[0].status, Status::Converting);
    }

    #[test]
    fn is_duplicate_false_then_true() {
        let q = make_queue();
        assert!(!q.is_duplicate("hash1").unwrap());
        let id = enqueue_one(&q, "hash1");
        assert!(!q.is_duplicate("hash1").unwrap());
        q.set_status(&id, Status::Done, None).unwrap();
        assert!(q.is_duplicate("hash1").unwrap());
    }

    #[test]
    fn reset_failed_returns_count_and_marks_pending() {
        let q = make_queue();
        let ids: Vec<_> = (0..3).map(|i| enqueue_one(&q, &format!("h{i}"))).collect();
        for id in &ids {
            q.set_status(id, Status::Failed, Some("oops")).unwrap();
        }
        assert_eq!(q.reset_failed().unwrap(), 3);
        let depth = q.queue_depth().unwrap();
        assert_eq!(*depth.get("pending").unwrap_or(&0), 3);
        assert_eq!(*depth.get("failed").unwrap_or(&0), 0);
    }

    #[test]
    fn multipart_add_get_clear() {
        let q = make_queue();
        q.add_part("grp1", Path::new("/watch/arc.part1.rar")).unwrap();
        q.add_part("grp1", Path::new("/watch/arc.part2.rar")).unwrap();
        q.add_part("grp1", Path::new("/watch/arc.part3.rar")).unwrap();
        assert_eq!(q.get_parts("grp1").unwrap().len(), 3);
        q.clear_parts("grp1").unwrap();
        assert_eq!(q.get_parts("grp1").unwrap().len(), 0);
    }

    #[test]
    fn concurrent_enqueue_all_succeed() {
        // :memory: gives each pool connection its own private database.
        // Use a real temp file so all connections share the same schema.
        let db_file = tempfile::NamedTempFile::new().unwrap();
        let q = Arc::new(JobQueue::new(db_file.path()).unwrap());
        let mut handles = Vec::new();
        for i in 0..10_usize {
            let qc = q.clone();
            handles.push(std::thread::spawn(move || {
                qc.enqueue(
                    Path::new("/watch/file.rar"),
                    &format!("hash{i}"),
                    Strategy::Disk,
                    &["host1".into()],
                )
                .unwrap();
            }));
        }
        for h in handles {
            h.join().unwrap();
        }
        let depth = q.queue_depth().unwrap();
        assert_eq!(*depth.get("pending").unwrap_or(&0), 10);
    }

    #[test]
    fn queue_depth_correct_per_status() {
        let q = make_queue();
        let id1 = enqueue_one(&q, "h1");
        let id2 = enqueue_one(&q, "h2");
        let _id3 = enqueue_one(&q, "h3");
        q.set_status(&id1, Status::Done, None).unwrap();
        q.set_status(&id2, Status::Failed, Some("err")).unwrap();
        let depth = q.queue_depth().unwrap();
        assert_eq!(*depth.get("pending").unwrap_or(&0),  1);
        assert_eq!(*depth.get("done").unwrap_or(&0),     1);
        assert_eq!(*depth.get("failed").unwrap_or(&0),   1);
    }

    #[test]
    fn get_expired_groups_past_timeout() {
        let q = make_queue();
        // Insert a part with arrived_at = 25 hours ago
        let old_ts = now_secs() - 25 * 3600;
        let conn = q.pool.get().unwrap();
        conn.execute(
            "INSERT INTO multipart_groups (group_id, part_path, arrived_at) VALUES (?1,?2,?3)",
            rusqlite::params!["old_grp", "/watch/a.part1.rar", old_ts],
        )
        .unwrap();
        drop(conn);

        let expired = q.get_expired_groups(24).unwrap();
        assert!(expired.contains(&"old_grp".to_owned()));
    }

    #[test]
    fn get_expired_groups_within_timeout() {
        let q = make_queue();
        let recent_ts = now_secs() - 3600; // 1 hour ago
        let conn = q.pool.get().unwrap();
        conn.execute(
            "INSERT INTO multipart_groups (group_id, part_path, arrived_at) VALUES (?1,?2,?3)",
            rusqlite::params!["new_grp", "/watch/b.part1.rar", recent_ts],
        )
        .unwrap();
        drop(conn);

        let expired = q.get_expired_groups(24).unwrap();
        assert!(!expired.contains(&"new_grp".to_owned()));
    }

    #[test]
    fn get_expired_groups_timeout_zero_always_empty() {
        let q = make_queue();
        let old_ts = now_secs() - 100 * 3600;
        let conn = q.pool.get().unwrap();
        conn.execute(
            "INSERT INTO multipart_groups (group_id, part_path, arrived_at) VALUES (?1,?2,?3)",
            rusqlite::params!["grp", "/watch/c.part1.rar", old_ts],
        )
        .unwrap();
        drop(conn);

        assert!(q.get_expired_groups(0).unwrap().is_empty());
    }
}
