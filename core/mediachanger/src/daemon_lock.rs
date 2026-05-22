// Copyright (c) 2026 Mete Balci
// SPDX-License-Identifier: Apache-2.0

// Daemon lock management to prevent concurrent CLI/daemon operations
//
// This module provides file-based locking to ensure that CLI operations
// cannot modify the library while the daemon is running, preventing data
// corruption and state inconsistencies.

use crate::errors::{Result, SmcError};
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};

/// Daemon lock file manager
///
/// Creates a lock file at <data_dir>/.daemon.lock with the daemon's PID.
/// The lock is automatically released when the DaemonLock is dropped.
pub struct DaemonLock {
    lock_path: PathBuf,
}

impl DaemonLock {
    /// Acquire the daemon lock
    ///
    /// Returns an error if the lock already exists (daemon is running).
    pub fn acquire<P: AsRef<Path>>(data_dir: P) -> Result<Self> {
        let lock_path = data_dir.as_ref().join(".daemon.lock");

        // Check if lock already exists
        if lock_path.exists() {
            // Try to read PID from lock file
            if let Ok(contents) = fs::read_to_string(&lock_path)
                && let Ok(pid) = contents.trim().parse::<i32>()
            {
                // Check if process is still running
                #[cfg(unix)]
                {
                    use std::process::Command;
                    if let Ok(output) = Command::new("kill").arg("-0").arg(pid.to_string()).output()
                        && output.status.success()
                    {
                        return Err(SmcError::DaemonRunning(pid));
                    }
                }

                #[cfg(windows)]
                {
                    use std::process::Command;
                    if let Ok(output) = Command::new("tasklist")
                        .arg("/FI")
                        .arg(&format!("PID eq {}", pid))
                        .output()
                    {
                        let output_str = String::from_utf8_lossy(&output.stdout);
                        if output_str.contains(&pid.to_string()) {
                            return Err(SmcError::DaemonRunning(pid));
                        }
                    }
                }
            }

            // Stale lock file - remove it
            tracing::warn!("Removing stale daemon lock file");
            let _ = fs::remove_file(&lock_path);
        }

        // Create lock file with current PID
        let pid = std::process::id();
        let mut file = fs::File::create(&lock_path)?;

        writeln!(file, "{}", pid)?;

        Ok(Self { lock_path })
    }

    /// Release the daemon lock explicitly (also happens automatically on drop)
    pub fn release(self) {
        drop(self);
    }
}

impl Drop for DaemonLock {
    fn drop(&mut self) {
        if self.lock_path.exists() {
            let _ = fs::remove_file(&self.lock_path);
        }
    }
}

/// Check if the daemon is currently running
///
/// This is used by CLI commands to determine if they need to refuse operations
/// that would conflict with the running daemon.
pub fn is_daemon_running<P: AsRef<Path>>(data_dir: P) -> bool {
    let lock_path = data_dir.as_ref().join(".daemon.lock");

    if !lock_path.exists() {
        return false;
    }

    // Try to read PID from lock file
    if let Ok(contents) = fs::read_to_string(&lock_path)
        && let Ok(pid) = contents.trim().parse::<i32>()
    {
        // Check if process is still running
        #[cfg(unix)]
        {
            use std::process::Command;
            if let Ok(output) = Command::new("kill").arg("-0").arg(pid.to_string()).output() {
                return output.status.success();
            }
        }

        #[cfg(windows)]
        {
            use std::process::Command;
            if let Ok(output) = Command::new("tasklist")
                .arg("/FI")
                .arg(&format!("PID eq {}", pid))
                .output()
            {
                let output_str = String::from_utf8_lossy(&output.stdout);
                return output_str.contains(&pid.to_string());
            }
        }
    }

    // Stale lock file - clean it up
    let _ = fs::remove_file(&lock_path);
    false
}

/// Ensure daemon is not running before proceeding with CLI operation
///
/// This should be called at the beginning of any CLI command that modifies
/// the library state (cartridge create/remove, library modify, etc.)
pub fn check_daemon_not_running<P: AsRef<Path>>(data_dir: P) -> Result<()> {
    if is_daemon_running(&data_dir) {
        let lock_path = data_dir.as_ref().join(".daemon.lock");
        if let Ok(contents) = fs::read_to_string(&lock_path)
            && let Ok(pid) = contents.trim().parse::<i32>()
        {
            return Err(SmcError::DaemonRunning(pid));
        }
        return Err(SmcError::InvalidOp(
            "Daemon is running. Stop the daemon before modifying the library.",
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn test_daemon_lock_acquire_and_release() {
        let temp_dir = TempDir::new().unwrap();
        let data_dir = temp_dir.path();

        // Acquire lock
        let lock = DaemonLock::acquire(data_dir).unwrap();

        // Should not be able to acquire again
        assert!(DaemonLock::acquire(data_dir).is_err());

        // Release lock
        drop(lock);

        // Should be able to acquire again
        let _lock2 = DaemonLock::acquire(data_dir).unwrap();
    }

    #[test]
    fn test_is_daemon_running() {
        let temp_dir = TempDir::new().unwrap();
        let data_dir = temp_dir.path();

        // Initially not running
        assert!(!is_daemon_running(data_dir));

        // Acquire lock
        let _lock = DaemonLock::acquire(data_dir).unwrap();

        // Now running
        assert!(is_daemon_running(data_dir));
    }

    #[test]
    fn test_check_daemon_not_running() {
        let temp_dir = TempDir::new().unwrap();
        let data_dir = temp_dir.path();

        // Should succeed when not running
        assert!(check_daemon_not_running(data_dir).is_ok());

        // Acquire lock
        let _lock = DaemonLock::acquire(data_dir).unwrap();

        // Should fail when running
        assert!(check_daemon_not_running(data_dir).is_err());
    }
}
