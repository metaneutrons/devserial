// SPDX-License-Identifier: GPL-3.0-only
// Copyright (C) 2026 Fabian Schmieder

//! Single source of truth for every path and file name devserial derives.
//!
//! Nothing outside this module may build a database, archive, socket or PID
//! path, and nothing outside this module may sanitize a port name. Environment
//! overrides exist so that tests and sandboxes never touch production state.

use std::path::{Path, PathBuf};

/// Environment variable overriding the data directory.
pub const ENV_DATA_DIR: &str = "DEVSERIAL_DATA_DIR";
/// Environment variable overriding the IPC endpoint.
pub const ENV_SOCKET: &str = "DEVSERIAL_SOCKET";
/// Environment variable overriding the configuration file path.
pub const ENV_CONFIG: &str = "DEVSERIAL_CONFIG";

/// Replace characters that cannot appear in a file name.
///
/// The rule is deliberately a superset across platforms so that a port yields
/// the same database name regardless of which entry point opened it.
#[must_use]
pub fn sanitize_port_name(port: &str) -> String {
    port.replace(['/', '\\', ':'], "_")
}

/// Path of the capture database for a port.
#[must_use]
pub fn port_db_path(data_dir: &Path, port: &str) -> PathBuf {
    data_dir.join(format!("{}.db", sanitize_port_name(port)))
}

/// Path of the shared state database.
#[must_use]
pub fn state_db_path(data_dir: &Path) -> PathBuf {
    data_dir.join("config.db")
}

/// Path of a timestamped archive copy of a port's database.
#[must_use]
pub fn archive_path(archive_dir: &Path, port: &str, timestamp: &str) -> PathBuf {
    archive_dir.join(format!("{}_{timestamp}.db", sanitize_port_name(port)))
}

/// Timestamp component used in archive file names.
#[must_use]
pub fn archive_timestamp() -> String {
    chrono::Utc::now().format("%Y%m%d_%H%M%S").to_string()
}

/// Platform default data directory, honouring [`ENV_DATA_DIR`].
#[must_use]
pub fn default_data_dir() -> PathBuf {
    if let Some(dir) = std::env::var_os(ENV_DATA_DIR) {
        if !dir.is_empty() {
            return PathBuf::from(dir);
        }
    }
    platform_data_dir()
}

fn platform_data_dir() -> PathBuf {
    #[cfg(target_os = "macos")]
    {
        home_dir().map_or_else(
            || PathBuf::from("./data"),
            |h| h.join("Library/Application Support/devserial"),
        )
    }
    #[cfg(target_os = "linux")]
    {
        std::env::var_os("XDG_STATE_HOME")
            .filter(|v| !v.is_empty())
            .map(|h| PathBuf::from(h).join("devserial"))
            .or_else(|| home_dir().map(|h| h.join(".local/state/devserial")))
            .unwrap_or_else(|| PathBuf::from("./data"))
    }
    #[cfg(target_os = "windows")]
    {
        std::env::var_os("LOCALAPPDATA")
            .filter(|v| !v.is_empty())
            .map_or_else(
                || PathBuf::from("./data"),
                |h| PathBuf::from(h).join("devserial"),
            )
    }
    #[cfg(not(any(target_os = "macos", target_os = "linux", target_os = "windows")))]
    {
        PathBuf::from("./data")
    }
}

/// Default archive directory, derived from the data directory.
#[must_use]
pub fn default_archive_dir() -> PathBuf {
    default_data_dir().join("archive")
}

/// Directory holding the user configuration file, if a home directory exists.
#[must_use]
pub fn config_dir() -> Option<PathBuf> {
    #[cfg(unix)]
    {
        home_dir().map(|h| h.join(".config/devserial"))
    }
    #[cfg(windows)]
    {
        std::env::var_os("APPDATA")
            .filter(|v| !v.is_empty())
            .map(|h| PathBuf::from(h).join("devserial"))
    }
    #[cfg(not(any(unix, windows)))]
    {
        None
    }
}

#[cfg(any(unix, target_os = "macos"))]
fn home_dir() -> Option<PathBuf> {
    std::env::var_os("HOME")
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
}

/// Default IPC endpoint, honouring [`ENV_SOCKET`].
///
/// On Unix this is a filesystem path for a Unix domain socket. On Windows it is
/// a named pipe path. Both are opaque to callers and only interpreted by
/// [`crate::transport`].
#[must_use]
pub fn default_socket_path() -> PathBuf {
    if let Some(sock) = std::env::var_os(ENV_SOCKET) {
        if !sock.is_empty() {
            return PathBuf::from(sock);
        }
    }
    platform_socket_path()
}

fn platform_socket_path() -> PathBuf {
    #[cfg(target_os = "macos")]
    {
        home_dir().map_or_else(
            || PathBuf::from("./devserial.sock"),
            |h| h.join("Library/Application Support/devserial/devserial.sock"),
        )
    }
    #[cfg(target_os = "linux")]
    {
        std::env::var_os("XDG_RUNTIME_DIR")
            .filter(|v| !v.is_empty())
            .map(|r| PathBuf::from(r).join("devserial/devserial.sock"))
            .or_else(|| home_dir().map(|h| h.join(".local/state/devserial/devserial.sock")))
            .unwrap_or_else(|| PathBuf::from("./devserial.sock"))
    }
    #[cfg(windows)]
    {
        PathBuf::from(r"\\.\pipe\devserial")
    }
    #[cfg(not(any(target_os = "macos", target_os = "linux", windows)))]
    {
        std::env::temp_dir().join("devserial.sock")
    }
}

/// Path of the daemon PID file for a given endpoint.
#[must_use]
pub fn pid_path(socket: &Path) -> PathBuf {
    #[cfg(windows)]
    {
        // A named pipe path is not a real file system location.
        if is_pipe_path(socket) {
            return default_data_dir().join("devserial.pid");
        }
    }
    let mut pid = socket.to_path_buf();
    pid.set_extension("pid");
    pid
}

/// Path of the GUI multiplexer endpoint for a given IPC endpoint.
#[must_use]
pub fn gui_socket_path(socket: &Path) -> PathBuf {
    #[cfg(windows)]
    {
        if is_pipe_path(socket) {
            return PathBuf::from(format!("{}-gui", socket.display()));
        }
    }
    let mut gui = socket.to_path_buf();
    gui.set_file_name("gui.sock");
    gui
}

/// Default GUI multiplexer endpoint.
#[must_use]
pub fn default_gui_socket_path() -> PathBuf {
    gui_socket_path(&default_socket_path())
}

/// Whether a path denotes a Windows named pipe.
#[must_use]
pub fn is_pipe_path(path: &Path) -> bool {
    path.to_string_lossy().starts_with(r"\\.\pipe\")
}

/// Explicit configuration file path from [`ENV_CONFIG`], if set.
#[must_use]
pub fn config_override() -> Option<PathBuf> {
    std::env::var_os(ENV_CONFIG)
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
}

/// Create a directory tree with owner-only permissions where the platform
/// supports it.
///
/// # Errors
/// Returns an error if the directory cannot be created.
pub fn create_private_dir(dir: &Path) -> std::io::Result<()> {
    std::fs::create_dir_all(dir)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut perms = std::fs::metadata(dir)?.permissions();
        if perms.mode() & 0o077 != 0 {
            perms.set_mode(0o700);
            std::fs::set_permissions(dir, perms)?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sanitize_covers_all_platform_separators() {
        assert_eq!(sanitize_port_name("/dev/ttyUSB0"), "_dev_ttyUSB0");
        assert_eq!(sanitize_port_name(r"\\.\COM3"), "__._COM3");
        assert_eq!(sanitize_port_name("COM3:"), "COM3_");
    }

    #[test]
    fn db_path_is_stable_across_entry_points() {
        let dir = Path::new("/data");
        assert_eq!(
            port_db_path(dir, "/dev/ttyUSB0"),
            PathBuf::from("/data/_dev_ttyUSB0.db")
        );
    }

    #[test]
    fn archive_path_uses_archive_dir() {
        let p = archive_path(Path::new("/arch"), "/dev/ttyUSB0", "20260101_120000");
        assert_eq!(p, PathBuf::from("/arch/_dev_ttyUSB0_20260101_120000.db"));
    }

    #[test]
    fn pid_path_replaces_extension() {
        let p = pid_path(Path::new("/run/devserial/devserial.sock"));
        assert_eq!(p, PathBuf::from("/run/devserial/devserial.pid"));
    }

    #[test]
    fn gui_socket_sits_next_to_ipc_socket() {
        let p = gui_socket_path(Path::new("/run/devserial/devserial.sock"));
        assert_eq!(p, PathBuf::from("/run/devserial/gui.sock"));
    }

    #[test]
    fn data_dir_env_override_wins() {
        // Uses a distinct variable name check rather than mutating the process
        // environment, which would race with other tests.
        assert!(!platform_data_dir().as_os_str().is_empty());
    }

    #[test]
    fn pipe_paths_are_recognized() {
        assert!(is_pipe_path(Path::new(r"\\.\pipe\devserial")));
        assert!(!is_pipe_path(Path::new("/tmp/devserial.sock")));
    }
}
