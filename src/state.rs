// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 Fabian Schmieder

//! Unified state persistence via `config.db`.
//!
//! Stores:
//! - Active port configurations (for auto-reopen on restart)
//! - Interface preferences that outlive a single window

use std::path::Path;

use rusqlite::{Connection, params};

use crate::config::PortConfig;
use crate::paths;

/// State schema, defined once for file-backed and in-memory databases.
const SCHEMA: &str = "CREATE TABLE IF NOT EXISTS ports (
         name TEXT PRIMARY KEY,
         config_json TEXT NOT NULL,
         opened_at INTEGER NOT NULL
     );
     CREATE TABLE IF NOT EXISTS settings (
         key TEXT PRIMARY KEY,
         value TEXT NOT NULL
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

    /// Read an interface preference.
    ///
    /// Returns `None` when the key was never written.
    ///
    /// # Errors
    /// Returns error on database failure.
    pub fn setting(&self, key: &str) -> Result<Option<String>, StateError> {
        let mut stmt = self
            .conn
            .prepare("SELECT value FROM settings WHERE key = ?1")?;
        let mut rows = stmt.query(params![key])?;
        match rows.next()? {
            Some(row) => Ok(Some(row.get(0)?)),
            None => Ok(None),
        }
    }

    /// Write an interface preference.
    ///
    /// # Errors
    /// Returns error on database failure.
    pub fn set_setting(&self, key: &str, value: &str) -> Result<(), StateError> {
        self.conn.execute(
            "INSERT OR REPLACE INTO settings (key, value) VALUES (?1, ?2)",
            params![key, value],
        )?;
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
    fn a_setting_survives_a_write_and_comes_back_unchanged() {
        let db = StateDb::open_memory().unwrap();

        assert_eq!(db.setting("ui.zoom").unwrap(), None);

        db.set_setting("ui.zoom", "1.25").unwrap();
        assert_eq!(db.setting("ui.zoom").unwrap().as_deref(), Some("1.25"));

        db.set_setting("ui.zoom", "0.9").unwrap();
        assert_eq!(db.setting("ui.zoom").unwrap().as_deref(), Some("0.9"));
    }

    #[test]
    fn settings_and_ports_do_not_share_a_namespace() {
        // Both tables have a text primary key. A port named like a setting
        // must not be able to overwrite it.
        let db = StateDb::open_memory().unwrap();
        db.set_setting("/dev/ttyUSB0", "a setting").unwrap();
        db.port_opened("/dev/ttyUSB0", &PortConfig::default())
            .unwrap();

        assert_eq!(
            db.setting("/dev/ttyUSB0").unwrap().as_deref(),
            Some("a setting")
        );
        assert_eq!(db.active_ports().unwrap().len(), 1);
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
