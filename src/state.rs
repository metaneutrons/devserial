// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 Fabian Schmieder

//! Unified state persistence via `config.db`.
//!
//! Stores:
//! - Active port configurations (for auto-reopen on restart)

use std::path::Path;

use rusqlite::{Connection, params};

use crate::config::PortConfig;
use crate::paths;

/// State schema, defined once for file-backed and in-memory databases.
const SCHEMA: &str = "CREATE TABLE IF NOT EXISTS ports (
         name TEXT PRIMARY KEY,
         config_json TEXT NOT NULL,
         opened_at INTEGER NOT NULL
     );";

/// Pragmas for the file-backed state database.
const PRAGMAS: &str = "PRAGMA journal_mode=WAL;
     PRAGMA synchronous=NORMAL;
     PRAGMA busy_timeout=5000;";

/// Errors from the state store.
#[derive(Debug, thiserror::Error)]
pub enum StateError {
    #[error("sqlite error: {0}")]
    Sqlite(#[from] rusqlite::Error),
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
    #[error("json error: {0}")]
    Json(#[from] serde_json::Error),
}

/// A port entry stored in config.db.
#[derive(Debug, Clone)]
pub struct PortEntry {
    pub name: String,
    pub config: PortConfig,
}

/// Unified state database for tracking active port configurations.
pub struct StateDb {
    conn: Connection,
}

impl StateDb {
    /// Open or create the state database at the given path.
    ///
    /// # Errors
    /// Returns error if the database cannot be opened.
    pub fn open(data_dir: &Path) -> Result<Self, StateError> {
        std::fs::create_dir_all(data_dir)?;
        let conn = Connection::open(paths::state_db_path(data_dir))?;
        conn.execute_batch(PRAGMAS)?;
        conn.execute_batch(SCHEMA)?;
        Ok(Self { conn })
    }

    /// Open an in-memory state database (for testing).
    ///
    /// # Errors
    /// Returns error if the database cannot be created.
    pub fn open_memory() -> Result<Self, StateError> {
        let conn = Connection::open_in_memory()?;
        conn.execute_batch(SCHEMA)?;
        Ok(Self { conn })
    }

    /// Register a port as active.
    ///
    /// # Errors
    /// Returns error on database failure.
    pub fn port_opened(&self, name: &str, config: &PortConfig) -> Result<(), StateError> {
        let json = serde_json::to_string(config)?;
        let now = chrono::Utc::now().timestamp_nanos_opt().unwrap_or(0);
        self.conn.execute(
            "INSERT OR REPLACE INTO ports (name, config_json, opened_at) VALUES (?1, ?2, ?3)",
            params![name, json, now],
        )?;
        Ok(())
    }

    /// Remove a port from active state.
    ///
    /// # Errors
    /// Returns error on database failure.
    pub fn port_closed(&self, name: &str) -> Result<(), StateError> {
        self.conn
            .execute("DELETE FROM ports WHERE name = ?1", params![name])?;
        Ok(())
    }

    /// Get all ports that were active (for restore on startup).
    ///
    /// # Errors
    /// Returns error on database failure.
    pub fn active_ports(&self) -> Result<Vec<PortEntry>, StateError> {
        let mut stmt = self.conn.prepare("SELECT name, config_json FROM ports")?;
        let rows = stmt.query_map([], |row| {
            let name: String = row.get(0)?;
            let json: String = row.get(1)?;
            Ok((name, json))
        })?;
        let mut entries = Vec::new();
        for row in rows {
            let (name, json) = row?;
            if let Ok(config) = serde_json::from_str(&json) {
                entries.push(PortEntry { name, config });
            }
        }
        Ok(entries)
    }

    /// Remove every port entry, used when state should not be restored.
    ///
    /// # Errors
    /// Returns error on database failure.
    pub fn clear_ports(&self) -> Result<(), StateError> {
        self.conn.execute("DELETE FROM ports", [])?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_port_open_close() {
        let db = StateDb::open_memory().unwrap();
        let config = PortConfig::default();

        db.port_opened("/dev/ttyUSB0", &config).unwrap();
        let ports = db.active_ports().unwrap();
        assert_eq!(ports.len(), 1);
        assert_eq!(ports[0].name, "/dev/ttyUSB0");
        assert_eq!(ports[0].config.baudrate, 115_200);

        db.port_closed("/dev/ttyUSB0").unwrap();
        let ports = db.active_ports().unwrap();
        assert!(ports.is_empty());
    }

    #[test]
    fn test_port_reopen_preserves_config() {
        let db = StateDb::open_memory().unwrap();
        let config = PortConfig {
            baudrate: 9600,
            ..PortConfig::default()
        };

        db.port_opened("/dev/ttyUSB0", &config).unwrap();
        let ports = db.active_ports().unwrap();
        assert_eq!(ports[0].config.baudrate, 9600);
    }
}
