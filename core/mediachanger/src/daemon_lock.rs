// Copyright (c) 2026 Mete Balci
// SPDX-License-Identifier: Apache-2.0

// Daemon lock management to prevent concurrent CLI/daemon operations
//
// This module provides file-based locking to ensure that CLI operations
// cannot modify the library while the daemon is running, preventing data
// corruption and state inconsistencies.

use crate::errors::{Result, SmcError};
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};

/// Probe whether `pid` names a live process.
///
/// On Linux this checks `/proc/<pid>`, which exists iff the process
/// exists — independent of who owns it. That is what fixes issue #231:
/// the old `kill -0` subprocess could not distinguish "exists but owned
/// by another user" (`EPERM`, exits non-zero) from "no such process"
/// (`ESRCH`), so a daemon running as the `thurvtl`/`thurvsa` system user
/// was misclassified as dead by an unprivileged CLI and its live lock
/// deleted. The crate forbids `unsafe`, so a direct `kill(2)` syscall is
/// out — `/proc` is the owner-independent, allocation-free equivalent.
#[cfg(target_os = "linux")]
fn process_alive(pid: i32) -> bool {
    pid > 0 && Path::new("/proc").join(pid.to_string()).exists()
}

/// Best-effort fallback on non-Linux unix: `kill -0` cannot tell `EPERM`
/// from `ESRCH`, but Linux is the supported deployment target.
#[cfg(all(unix, not(target_os = "linux")))]
fn process_alive(pid: i32) -> bool {
    use std::process::Command;
    Command::new("kill")
        .arg("-0")
        .arg(pid.to_string())
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

#[cfg(windows)]
fn process_alive(pid: i32) -> bool {
    use std::process::Command;
    if let Ok(output) = Command::new("tasklist")
        .arg("/FI")
        .arg(format!("PID eq {}", pid))
        .output()
    {
        return String::from_utf8_lossy(&output.stdout).contains(&pid.to_string());
    }
    false
}

/// Read and parse the PID recorded in a lock file, or `None` if the
/// file is missing / unreadable / not a valid PID.
fn read_lock_pid(lock_path: &Path) -> Option<i32> {
    fs::read_to_string(lock_path).ok()?.trim().parse::<i32>().ok()
}

/// Daemon lock file manager
///
/// Creates a lock file at <data_dir>/.daemon.lock with the daemon's PID.
/// The lock is automatically released when the DaemonLock is dropped.
pub struct DaemonLock {
    lock_path: PathBuf,
}

/// Outcome of one atomic lock-create attempt.
enum CreateOutcome {
    /// The lock file already existed (O_EXCL refused).
    Exists,
    /// A real I/O error other than "already exists".
    Io(std::io::Error),
}

impl DaemonLock {
    /// Acquire the daemon lock
    ///
    /// Returns an error if the lock already exists (daemon is running).
    pub fn acquire<P: AsRef<Path>>(data_dir: P) -> Result<Self> {
        let lock_path = data_dir.as_ref().join(".daemon.lock");
        let pid = std::process::id();

        // Atomic create: `create_new` (O_EXCL) fails if the file exists,
        // so two racing starters can't both "acquire" by truncating each
        // other's lock (issue #231). On an existing lock, probe liveness
        // with a real `kill(2)` (EPERM-aware); only a genuinely dead
        // holder lets us reclaim it, and the reclaim is a single retry.
        match Self::try_create(&lock_path, pid) {
            Ok(this) => return Ok(this),
            Err(CreateOutcome::Io(e)) => return Err(e.into()),
            Err(CreateOutcome::Exists) => {
                if let Some(existing) = read_lock_pid(&lock_path)
                    && process_alive(existing)
                {
                    return Err(SmcError::DaemonRunning(existing));
                }
                tracing::warn!("Removing stale daemon lock file");
                let _ = fs::remove_file(&lock_path);
            }
        }

        // Second (and final) attempt after clearing a stale lock. A
        // fresh `Exists` here means a concurrent starter beat us to it —
        // treat that as the daemon already running rather than clobber.
        match Self::try_create(&lock_path, pid) {
            Ok(this) => Ok(this),
            Err(CreateOutcome::Io(e)) => Err(e.into()),
            Err(CreateOutcome::Exists) => {
                Err(SmcError::DaemonRunning(read_lock_pid(&lock_path).unwrap_or(0)))
            }
        }
    }

    /// Attempt one atomic `create_new` of the lock file, writing `pid`.
    fn try_create(lock_path: &Path, pid: u32) -> std::result::Result<Self, CreateOutcome> {
        match OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(lock_path)
        {
            Ok(mut file) => {
                writeln!(file, "{}", pid).map_err(CreateOutcome::Io)?;
                Ok(Self {
                    lock_path: lock_path.to_path_buf(),
                })
            }
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => Err(CreateOutcome::Exists),
            Err(e) => Err(CreateOutcome::Io(e)),
        }
    }

    /// Release the daemon lock explicitly (also happens automatically on drop)
    pub fn release(self) {
        drop(self);
    }
}

impl Drop for DaemonLock {
    fn drop(&mut self) {
        // Only remove the lock if it still records *our* PID. Guards
        // against deleting a lock another daemon legitimately took over
        // after ours was force-removed (issue #231).
        if read_lock_pid(&self.lock_path) == Some(std::process::id() as i32) {
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
    // A read-only query must not mutate shared state — it no longer
    // deletes a stale lock (the previous EPERM misread plus this delete
    // is exactly how a live daemon's lock got removed, issue #231).
    // Stale-lock reclamation is `acquire`'s job alone.
    match read_lock_pid(&lock_path) {
        Some(pid) => process_alive(pid),
        None => false,
    }
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

    /// Issue #231: a read-only liveness query must not delete the lock,
    /// and `acquire` must reclaim a genuinely-stale (dead-PID) lock.
    #[test]
    fn query_does_not_delete_lock_and_acquire_reclaims_stale() {
        let temp_dir = TempDir::new().unwrap();
        let data_dir = temp_dir.path();
        let lock_path = data_dir.join(".daemon.lock");

        // A reaped child's PID is dead.
        let mut child = std::process::Command::new("true").spawn().unwrap();
        let dead_pid = child.id();
        child.wait().unwrap();
        fs::write(&lock_path, format!("{}\n", dead_pid)).unwrap();

        // Query reports not-running but must NOT delete the lock.
        assert!(!is_daemon_running(data_dir));
        assert!(
            lock_path.exists(),
            "is_daemon_running must not delete a stale lock (read-only query)"
        );

        // acquire reclaims the stale lock and records our own PID.
        let lock = DaemonLock::acquire(data_dir).unwrap();
        let pid_now: i32 = fs::read_to_string(&lock_path).unwrap().trim().parse().unwrap();
        assert_eq!(pid_now, std::process::id() as i32);
        drop(lock);
        assert!(!lock_path.exists(), "drop removes our own lock");
    }

    /// Issue #231: Drop must not remove a lock that now records a
    /// *different* PID (another daemon took over after a force-remove).
    #[test]
    fn drop_does_not_remove_another_holders_lock() {
        let temp_dir = TempDir::new().unwrap();
        let data_dir = temp_dir.path();
        let lock = DaemonLock::acquire(data_dir).unwrap();
        let lock_path = data_dir.join(".daemon.lock");

        // Simulate another daemon overwriting the lock with its PID.
        let other_pid = std::process::id() + 1;
        fs::write(&lock_path, format!("{}\n", other_pid)).unwrap();
        drop(lock);

        assert!(
            lock_path.exists(),
            "drop must leave a lock that no longer records our PID"
        );
    }

    /// A live holder's lock can't be reclaimed: a second acquire while
    /// the first is alive (our own PID) must error.
    #[test]
    fn live_lock_is_not_reclaimed() {
        let temp_dir = TempDir::new().unwrap();
        let data_dir = temp_dir.path();
        let _lock = DaemonLock::acquire(data_dir).unwrap();
        match DaemonLock::acquire(data_dir) {
            Err(SmcError::DaemonRunning(pid)) => {
                assert_eq!(pid, std::process::id() as i32);
            }
            Err(e) => panic!("expected DaemonRunning, got error {e}"),
            Ok(_) => panic!("expected DaemonRunning, but acquire succeeded"),
        }
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
