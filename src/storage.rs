// SPDX-License-Identifier: GPL-3.0-only
// Copyright (C) 2026 Fabian Schmieder

use std::path::{Path, PathBuf};

use rusqlite::{Connection, params};
use serde::{Deserialize, Serialize};

/// Maximum send history entries kept per port.
const SEND_HISTORY_CAP: u32 = 1000;

/// A single stored line from the serial buffer.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct StoredLine {
    /// Monotonically increasing line ID (1-based).
    pub id: i64,
    /// Nanosecond UTC timestamp at ingestion.
    pub timestamp_ns: i64,
    /// The line payload.
    pub payload: String,
}

/// Buffer statistics.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct BufferStats {
    pub total_lines: u64,
    pub total_bytes: u64,
    pub last_timestamp_ns: Option<i64>,
    pub db_size_bytes: u64,
}

/// Optional time range for queries.
#[derive(Debug, Clone, Copy)]
pub struct TimeRange {
    pub start_ns: i64,
    pub end_ns: i64,
}

/// Errors from the storage layer.
#[derive(Debug, thiserror::Error)]
pub enum StorageError {
    #[error("sqlite error: {0}")]
    Sqlite(#[from] rusqlite::Error),
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
    #[error("invalid regex: {0}")]
    InvalidRegex(#[from] regex::Error),
}

/// Static counter for generating unique in-memory database URIs.
static NEXT_MEM_ID: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);

/// SQLite-backed storage for a single port's serial data.
///
/// Maintains separate connections for writing and reading to maximize concurrency
/// under `SQLite` WAL mode, preventing long reads from blocking high-rate ingestion.
pub struct SqliteStorage {
    write_conn: std::sync::Mutex<Connection>,
    read_conn: std::sync::Mutex<Connection>,
    db_path: PathBuf,
}

impl SqliteStorage {
    /// Open or create a database for the given port.
    ///
    /// # Errors
    /// Returns error if the database cannot be opened or initialized.
    pub fn open(path: &Path) -> Result<Self, StorageError> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }

        let write_conn = Connection::open(path)?;
        write_conn.execute_batch(
            "PRAGMA journal_mode=WAL;
             PRAGMA synchronous=NORMAL;
             PRAGMA busy_timeout=5000;",
        )?;
        write_conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS lines (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                timestamp_ns INTEGER NOT NULL,
                payload TEXT NOT NULL
            );
            CREATE INDEX IF NOT EXISTS idx_timestamp ON lines(timestamp_ns);
            CREATE TABLE IF NOT EXISTS send_history (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                timestamp_ns INTEGER NOT NULL,
                command TEXT NOT NULL
            );",
        )?;

        let read_conn = Connection::open(path)?;
        read_conn.execute_batch("PRAGMA busy_timeout=5000;")?;

        Ok(Self {
            write_conn: std::sync::Mutex::new(write_conn),
            read_conn: std::sync::Mutex::new(read_conn),
            db_path: path.to_path_buf(),
        })
    }

    /// Open an in-memory database (for testing).
    ///
    /// Uses `SQLite` shared in-memory URI to allow distinct read and write connections
    /// to operate on the same in-memory database.
    ///
    /// # Errors
    /// Returns error if the database cannot be created.
    pub fn open_memory() -> Result<Self, StorageError> {
        use rusqlite::OpenFlags;

        let mem_id = NEXT_MEM_ID.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let uri = format!("file:devserial_mem_{mem_id}?mode=memory&cache=shared");

        let write_conn = Connection::open_with_flags(
            &uri,
            OpenFlags::SQLITE_OPEN_READ_WRITE
                | OpenFlags::SQLITE_OPEN_CREATE
                | OpenFlags::SQLITE_OPEN_URI,
        )?;
        write_conn.execute_batch(
            "PRAGMA busy_timeout=5000;
             CREATE TABLE lines (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                timestamp_ns INTEGER NOT NULL,
                payload TEXT NOT NULL
            );
            CREATE INDEX idx_timestamp ON lines(timestamp_ns);
            CREATE TABLE send_history (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                timestamp_ns INTEGER NOT NULL,
                command TEXT NOT NULL
            );",
        )?;

        let read_conn = Connection::open_with_flags(
            &uri,
            OpenFlags::SQLITE_OPEN_READ_WRITE | OpenFlags::SQLITE_OPEN_URI,
        )?;
        read_conn.execute_batch("PRAGMA busy_timeout=5000;")?;

        Ok(Self {
            write_conn: std::sync::Mutex::new(write_conn),
            read_conn: std::sync::Mutex::new(read_conn),
            db_path: PathBuf::from(":memory:"),
        })
    }

    /// Insert a batch of lines in a single transaction.
    ///
    /// # Errors
    /// Returns error on `SQLite` failure.
    pub fn insert_lines(&self, lines: &[(i64, &str)]) -> Result<(), StorageError> {
        let conn = self
            .write_conn
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let tx = conn.unchecked_transaction()?;
        {
            let mut stmt =
                tx.prepare_cached("INSERT INTO lines (timestamp_ns, payload) VALUES (?1, ?2)")?;
            for &(ts, payload) in lines {
                stmt.execute(params![ts, payload])?;
            }
        }
        tx.commit()?;
        drop(conn);
        Ok(())
    }

    /// Read lines starting from `start_id` (inclusive), up to `count` lines.
    ///
    /// # Errors
    /// Returns error on `SQLite` failure.
    pub fn read_lines(&self, start_id: i64, count: u32) -> Result<Vec<StoredLine>, StorageError> {
        let conn = self
            .read_conn
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let mut stmt = conn.prepare_cached(
            "SELECT id, timestamp_ns, payload FROM lines WHERE id >= ?1 ORDER BY id LIMIT ?2",
        )?;
        let rows = stmt.query_map(params![start_id, count], |row| {
            Ok(StoredLine {
                id: row.get(0)?,
                timestamp_ns: row.get(1)?,
                payload: row.get(2)?,
            })
        })?;
        let res = rows.collect::<Result<Vec<_>, _>>().map_err(Into::into);
        drop(stmt);
        drop(conn);
        res
    }

    /// Get buffer statistics.
    ///
    /// # Errors
    /// Returns error on `SQLite` failure.
    pub fn get_stats(&self) -> Result<BufferStats, StorageError> {
        let (total_lines, total_bytes, last_timestamp_ns) = {
            let conn = self
                .read_conn
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            let total_lines: u64 =
                conn.query_row("SELECT COUNT(*) FROM lines", [], |r| r.get(0))?;
            let total_bytes: u64 = conn.query_row(
                "SELECT COALESCE(SUM(LENGTH(payload)), 0) FROM lines",
                [],
                |r| r.get(0),
            )?;
            let last_timestamp_ns: Option<i64> =
                conn.query_row("SELECT MAX(timestamp_ns) FROM lines", [], |r| r.get(0))?;
            drop(conn);
            (total_lines, total_bytes, last_timestamp_ns)
        };

        let db_size_bytes = std::fs::metadata(&self.db_path).map_or(0, |m| m.len());

        Ok(BufferStats {
            total_lines,
            total_bytes,
            last_timestamp_ns,
            db_size_bytes,
        })
    }

    /// Search for lines containing a substring, optionally within a time range.
    ///
    /// # Errors
    /// Returns error on `SQLite` failure.
    pub fn search_substring(
        &self,
        query: &str,
        time_range: Option<TimeRange>,
        limit: u32,
    ) -> Result<Vec<StoredLine>, StorageError> {
        let pattern = format!("%{query}%");
        let conn = self
            .read_conn
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);

        let res = if let Some(tr) = time_range {
            let mut stmt = conn.prepare_cached(
                "SELECT id, timestamp_ns, payload FROM lines \
                 WHERE payload LIKE ?1 AND timestamp_ns >= ?2 AND timestamp_ns <= ?3 \
                 ORDER BY id LIMIT ?4",
            )?;
            let rows = stmt.query_map(params![pattern, tr.start_ns, tr.end_ns, limit], |row| {
                Ok(StoredLine {
                    id: row.get(0)?,
                    timestamp_ns: row.get(1)?,
                    payload: row.get(2)?,
                })
            })?;
            let r = rows.collect::<Result<Vec<_>, _>>().map_err(Into::into);
            drop(stmt);
            r
        } else {
            let mut stmt = conn.prepare_cached(
                "SELECT id, timestamp_ns, payload FROM lines \
                 WHERE payload LIKE ?1 ORDER BY id LIMIT ?2",
            )?;
            let rows = stmt.query_map(params![pattern, limit], |row| {
                Ok(StoredLine {
                    id: row.get(0)?,
                    timestamp_ns: row.get(1)?,
                    payload: row.get(2)?,
                })
            })?;
            let r = rows.collect::<Result<Vec<_>, _>>().map_err(Into::into);
            drop(stmt);
            r
        };
        drop(conn);
        res
    }

    /// Search for lines within a time range.
    ///
    /// # Errors
    /// Returns error on `SQLite` failure.
    pub fn search_time_range(
        &self,
        start_ns: i64,
        end_ns: i64,
        limit: u32,
    ) -> Result<Vec<StoredLine>, StorageError> {
        let conn = self
            .read_conn
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let mut stmt = conn.prepare_cached(
            "SELECT id, timestamp_ns, payload FROM lines \
             WHERE timestamp_ns >= ?1 AND timestamp_ns <= ?2 ORDER BY id LIMIT ?3",
        )?;
        let rows = stmt.query_map(params![start_ns, end_ns, limit], |row| {
            Ok(StoredLine {
                id: row.get(0)?,
                timestamp_ns: row.get(1)?,
                payload: row.get(2)?,
            })
        })?;
        let res = rows.collect::<Result<Vec<_>, _>>().map_err(Into::into);
        drop(stmt);
        drop(conn);
        res
    }

    /// Clear all lines. If `archive_path` is provided, copy the DB there first.
    ///
    /// # Errors
    /// Returns error on `SQLite` or IO failure.
    pub fn clear(&self, archive_path: Option<&Path>) -> Result<(), StorageError> {
        if let Some(archive) = archive_path {
            if self.db_path.to_str() != Some(":memory:") {
                if let Some(parent) = archive.parent() {
                    std::fs::create_dir_all(parent)?;
                }
                std::fs::copy(&self.db_path, archive)?;
            }
        }
        let conn = self
            .write_conn
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        conn.execute_batch("DELETE FROM lines; VACUUM;")?;
        drop(conn);
        Ok(())
    }

    /// Export a range of lines by ID (inclusive).
    ///
    /// # Errors
    /// Returns error on `SQLite` failure.
    pub fn export_range(
        &self,
        start_id: i64,
        end_id: i64,
    ) -> Result<Vec<StoredLine>, StorageError> {
        let conn = self
            .read_conn
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let mut stmt = conn.prepare_cached(
            "SELECT id, timestamp_ns, payload FROM lines \
             WHERE id >= ?1 AND id <= ?2 ORDER BY id",
        )?;
        let rows = stmt.query_map(params![start_id, end_id], |row| {
            Ok(StoredLine {
                id: row.get(0)?,
                timestamp_ns: row.get(1)?,
                payload: row.get(2)?,
            })
        })?;
        let res = rows.collect::<Result<Vec<_>, _>>().map_err(Into::into);
        drop(stmt);
        drop(conn);
        res
    }

    /// Append a command to send history (caps at 1000 entries).
    ///
    /// # Errors
    /// Returns error on `SQLite` failure.
    pub fn append_send_history(&self, command: &str) -> Result<(), StorageError> {
        let ts = chrono::Utc::now().timestamp_nanos_opt().unwrap_or(0);
        let conn = self
            .write_conn
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        conn.execute(
            "INSERT INTO send_history (timestamp_ns, command) VALUES (?1, ?2)",
            params![ts, command],
        )?;
        // Trim to cap
        conn.execute(
            "DELETE FROM send_history WHERE id NOT IN (SELECT id FROM send_history ORDER BY id DESC LIMIT ?1)",
            params![SEND_HISTORY_CAP],
        )?;
        drop(conn);
        Ok(())
    }

    /// Load the most recent send history entries.
    ///
    /// # Errors
    /// Returns error on `SQLite` failure.
    pub fn load_send_history(&self, limit: u32) -> Result<Vec<String>, StorageError> {
        let conn = self
            .read_conn
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let mut stmt =
            conn.prepare_cached("SELECT command FROM send_history ORDER BY id DESC LIMIT ?1")?;
        let rows = stmt.query_map(params![limit], |row| row.get::<_, String>(0))?;
        let mut history: Vec<String> = rows.collect::<Result<Vec<_>, _>>()?;
        drop(stmt);
        drop(conn);
        history.reverse(); // oldest first
        Ok(history)
    }

    /// Trim buffer to keep only the most recent `max_lines`. No-op if 0.
    ///
    /// # Errors
    /// Returns error on `SQLite` failure.
    pub fn trim_lines(&self, max_lines: u64) -> Result<(), StorageError> {
        if max_lines == 0 {
            return Ok(());
        }
        let conn = self
            .write_conn
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        conn.execute(
            "DELETE FROM lines WHERE id NOT IN (SELECT id FROM lines ORDER BY id DESC LIMIT ?1)",
            params![max_lines],
        )?;
        drop(conn);
        Ok(())
    }

    /// Get the path to the database file.
    #[must_use]
    pub fn db_path(&self) -> &Path {
        &self.db_path
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    fn make_storage() -> SqliteStorage {
        SqliteStorage::open_memory().unwrap()
    }

    #[test]
    fn test_insert_and_read_single() {
        let s = make_storage();
        s.insert_lines(&[(1_000_000, "hello")]).unwrap();

        let lines = s.read_lines(1, 10).unwrap();
        assert_eq!(lines.len(), 1);
        assert_eq!(lines[0].id, 1);
        assert_eq!(lines[0].timestamp_ns, 1_000_000);
        assert_eq!(lines[0].payload, "hello");
    }

    #[test]
    fn test_insert_batch_ordering() {
        let s = make_storage();
        let batch: Vec<(i64, &str)> = (0..100).map(|i| (i * 1000, "line")).collect();
        s.insert_lines(&batch).unwrap();

        let lines = s.read_lines(1, 100).unwrap();
        assert_eq!(lines.len(), 100);
        for (i, line) in lines.iter().enumerate() {
            assert_eq!(line.id, i64::try_from(i + 1).unwrap());
        }
    }

    #[test]
    fn test_insert_10k_no_gaps() {
        let s = make_storage();
        let batch: Vec<(i64, &str)> = (0..10_000).map(|i| (i * 1000, "data")).collect();
        s.insert_lines(&batch).unwrap();

        let lines = s.read_lines(1, 10_000).unwrap();
        assert_eq!(lines.len(), 10_000);
        for (i, line) in lines.iter().enumerate() {
            assert_eq!(line.id, i64::try_from(i + 1).unwrap());
        }
    }

    #[test]
    fn test_read_range() {
        let s = make_storage();
        let batch: Vec<(i64, &str)> = (0..1000).map(|i| (i * 1000, "x")).collect();
        s.insert_lines(&batch).unwrap();

        let lines = s.read_lines(500, 50).unwrap();
        assert_eq!(lines.len(), 50);
        assert_eq!(lines[0].id, 500);
        assert_eq!(lines[49].id, 549);
    }

    #[test]
    fn test_read_past_end() {
        let s = make_storage();
        s.insert_lines(&[(1000, "only")]).unwrap();

        let lines = s.read_lines(100, 50).unwrap();
        assert!(lines.is_empty());
    }

    #[test]
    fn test_get_stats() {
        let s = make_storage();
        s.insert_lines(&[(1000, "hello"), (2000, "world!")])
            .unwrap();

        let stats = s.get_stats().unwrap();
        assert_eq!(stats.total_lines, 2);
        assert_eq!(stats.total_bytes, 11); // "hello" + "world!"
        assert_eq!(stats.last_timestamp_ns, Some(2000));
    }

    #[test]
    fn test_search_substring() {
        let s = make_storage();
        s.insert_lines(&[
            (1000, "[INFO] all good"),
            (2000, "[ERROR] something broke"),
            (3000, "[INFO] recovered"),
            (4000, "[ERROR] again"),
        ])
        .unwrap();

        let results = s.search_substring("ERROR", None, 100).unwrap();
        assert_eq!(results.len(), 2);
        assert_eq!(results[0].id, 2);
        assert_eq!(results[1].id, 4);
    }

    #[test]
    fn test_search_substring_with_time_range() {
        let s = make_storage();
        s.insert_lines(&[
            (1000, "[ERROR] early"),
            (5000, "[ERROR] middle"),
            (9000, "[ERROR] late"),
        ])
        .unwrap();

        let results = s
            .search_substring(
                "ERROR",
                Some(TimeRange {
                    start_ns: 4000,
                    end_ns: 6000,
                }),
                100,
            )
            .unwrap();
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].payload, "[ERROR] middle");
    }

    #[test]
    fn test_search_time_range() {
        let s = make_storage();
        s.insert_lines(&[(1000, "a"), (2000, "b"), (3000, "c"), (4000, "d")])
            .unwrap();

        let results = s.search_time_range(2000, 3000, 100).unwrap();
        assert_eq!(results.len(), 2);
        assert_eq!(results[0].payload, "b");
        assert_eq!(results[1].payload, "c");
    }

    #[test]
    fn test_clear_no_archive() {
        let s = make_storage();
        s.insert_lines(&[(1000, "data")]).unwrap();
        s.clear(None).unwrap();

        let stats = s.get_stats().unwrap();
        assert_eq!(stats.total_lines, 0);
    }

    #[test]
    fn test_clear_with_archive() {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("test.db");
        let archive_path = dir.path().join("archive.db");

        let s = SqliteStorage::open(&db_path).unwrap();
        s.insert_lines(&[(1000, "preserved")]).unwrap();
        s.clear(Some(&archive_path)).unwrap();

        // Active DB is empty
        assert_eq!(s.get_stats().unwrap().total_lines, 0);
        // Archive exists
        assert!(archive_path.exists());
    }

    #[test]
    fn test_export_range() {
        let s = make_storage();
        let batch: Vec<(i64, &str)> = (0..100).map(|i| (i * 1000, "line")).collect();
        s.insert_lines(&batch).unwrap();

        let exported = s.export_range(10, 20).unwrap();
        assert_eq!(exported.len(), 11); // inclusive
        assert_eq!(exported[0].id, 10);
        assert_eq!(exported[10].id, 20);
    }

    #[test]
    fn test_stress_100k_lines() {
        let s = make_storage();
        // Insert in batches of 1000
        for batch_start in (0..100_000).step_by(1000) {
            let batch: Vec<(i64, &str)> = (batch_start..batch_start + 1000)
                .map(|i| (i * 1000, "stress"))
                .collect();
            s.insert_lines(&batch).unwrap();
        }

        let stats = s.get_stats().unwrap();
        assert_eq!(stats.total_lines, 100_000);

        // Verify no gaps: check first and last
        let first = s.read_lines(1, 1).unwrap();
        assert_eq!(first[0].id, 1);

        let last = s.read_lines(100_000, 1).unwrap();
        assert_eq!(last[0].id, 100_000);
    }

    #[test]
    fn test_file_based_storage() {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("port.db");

        let s = SqliteStorage::open(&db_path).unwrap();
        s.insert_lines(&[(1000, "persistent")]).unwrap();

        let stats = s.get_stats().unwrap();
        assert_eq!(stats.total_lines, 1);
        assert!(stats.db_size_bytes > 0);
    }

    #[tokio::test]
    async fn test_concurrent_read_and_write() {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("concurrent.db");
        let storage = Arc::new(SqliteStorage::open(&db_path).unwrap());

        let storage_writer = Arc::clone(&storage);
        let write_handle = tokio::spawn(async move {
            for chunk in 0..50 {
                let lines: Vec<(i64, &str)> = (0..10)
                    .map(|i| ((chunk * 10 + i) * 1000, "concurrent_line"))
                    .collect();
                storage_writer.insert_lines(&lines).unwrap();
                tokio::time::sleep(std::time::Duration::from_millis(2)).await;
            }
        });

        let storage_reader = Arc::clone(&storage);
        let read_handle = tokio::spawn(async move {
            let mut read_count = 0;
            for _ in 0..25 {
                let stats = storage_reader.get_stats().unwrap();
                let results = storage_reader
                    .search_substring("concurrent", None, 100)
                    .unwrap();
                if !results.is_empty() {
                    read_count += 1;
                }
                assert!(stats.total_lines <= 500);
                tokio::time::sleep(std::time::Duration::from_millis(3)).await;
            }
            read_count
        });

        write_handle.await.unwrap();
        let read_count = read_handle.await.unwrap();
        assert!(read_count > 0);

        let final_stats = storage.get_stats().unwrap();
        assert_eq!(final_stats.total_lines, 500);
    }
}
