//! The PID file — the gateway's process number on disk.
//!
//! The file holds one decimal process number and a newline. It is
//! the anchor for the operator commands that address the gateway by
//! its process number ("is it alive?", "which process is that?") in
//! the deployments that do not go through systemd.
//!
//! ## The lifecycle
//!
//! * The **supervisor** writes the file (issue #234): it is the
//!   resident parent, the process systemd's `MAINPID` points at, and
//!   the one `pangolin-ngx --reload` signals. The number is written
//!   **after** the configuration has been read and **before** any
//!   listener is bound: the file says "this process intends to
//!   serve", not "the ports are up".
//! * A graceful exit removes it.
//! * A `SIGKILL` leaves it behind. The next start truncates it, so
//!   a stale number never survives a restart.
//!
//! ## Why the removal is conditional
//!
//! The removal only happens when the file still carries *this*
//! process's number. Only the supervisor writes the file, but a
//! **restart** replaces it: the outgoing supervisor's guard must not
//! delete the incoming one's file on its way out, or the operator's
//! "which process is that?" would answer with nothing.

use std::io::Write;
use std::path::{Path, PathBuf};

/// Write this process's number into `path`.
///
/// The file is truncated first: a PID file left behind by a process
/// that was `SIGKILL`ed carries a stale number, and the file has to
/// hold exactly one number.
///
/// The parent directory is **not** created — a PID file whose
/// directory does not exist is a configuration mistake, and the
/// caller reports it rather than silently inventing a directory.
pub fn write(path: &Path) -> anyhow::Result<()> {
    let mut file = std::fs::File::create(path)?;
    file.write_all(format!("{}\n", std::process::id()).as_bytes())?;
    // The number has to have left this process's buffer before the
    // process claims the ports: an operator that reads the file has
    // to see the number of the process that is already serving.
    // (`flush` pushes it into the page cache — the file survives a
    // crash of the process, which is the case that matters here.)
    file.flush()?;
    Ok(())
}

/// Read the process number out of a PID file.
///
/// The trailing newline is optional, so a file written by hand with
/// `echo 1234 > file` reads back the same way.
pub fn read(path: &Path) -> anyhow::Result<u32> {
    let raw = std::fs::read_to_string(path)?;
    let trimmed = raw.trim();
    trimmed.parse::<u32>().map_err(|e| {
        anyhow::anyhow!(
            "the pid file {} does not hold a process number: {e}",
            path.display()
        )
    })
}

/// Remove the PID file — but only when it still carries this
/// process's number.
///
/// A file that holds somebody else's number belongs to another
/// process (the incoming generation of a handover) and is left
/// alone. A file that is already gone is not an error: the exit path
/// runs the removal whether or not the write succeeded.
pub fn remove(path: &Path) -> anyhow::Result<()> {
    match std::fs::read_to_string(path) {
        Ok(raw) => {
            if raw.trim() != std::process::id().to_string() {
                return Ok(());
            }
            std::fs::remove_file(path)?;
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => return Err(e.into()),
    }
    Ok(())
}

/// The PID file of this process, removed when the guard is dropped.
///
/// The guard is what makes the removal unconditional: the process
/// exits through a dozen paths (a failed service, a panic, the
/// normal shutdown), and every one of them has to take the file with
/// it. A `SIGKILL` runs no destructor, so the file survives that —
/// which is the case the next start truncates.
pub struct PidFileGuard {
    path: PathBuf,
}

impl PidFileGuard {
    /// Take ownership of `path`: the file is removed when the guard
    /// is dropped.
    pub fn new(path: PathBuf) -> Self {
        Self { path }
    }
}

impl Drop for PidFileGuard {
    fn drop(&mut self) {
        // A failure here is not worth failing the exit over: the
        // process is going away anyway, and the file is truncated by
        // the next start.
        if let Err(e) = remove(&self.path) {
            log::warn!("could not remove the pid file {}: {e}", self.path.display());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The write → read → remove round trip.
    ///
    /// The file has to hold the number of the process that wrote it,
    /// and the removal has to take it away — those are the two ends
    /// the operator commands depend on.
    #[test]
    fn the_pid_file_round_trips() {
        let dir = tempfile::TempDir::new().expect("tempdir");
        let path = dir.path().join("ngx.pid");

        write(&path).expect("the pid file is written");
        assert_eq!(
            read(&path).expect("the pid file reads back"),
            std::process::id()
        );

        remove(&path).expect("the pid file is removed");
        assert!(!path.exists(), "the file is gone after the removal");
    }

    /// The file holds exactly one line: the number and a newline.
    ///
    /// A tool that reads the file with `read pid` expects the number
    /// on the first line and nothing else.
    #[test]
    fn the_pid_file_holds_one_number_and_a_newline() {
        let dir = tempfile::TempDir::new().expect("tempdir");
        let path = dir.path().join("ngx.pid");

        write(&path).expect("the pid file is written");
        let raw = std::fs::read_to_string(&path).expect("read the raw file");
        assert_eq!(raw, format!("{}\n", std::process::id()));
        assert_eq!(raw.lines().count(), 1);
    }

    /// A file left behind by a `SIGKILL`ed process is overwritten,
    /// not appended to.
    ///
    /// The stale number would otherwise sit in front of the new one
    /// and every reader would read the wrong process.
    #[test]
    fn a_stale_pid_file_is_truncated_on_write() {
        let dir = tempfile::TempDir::new().expect("tempdir");
        let path = dir.path().join("ngx.pid");

        std::fs::write(&path, "999999\n").expect("write a stale pid file");
        write(&path).expect("the pid file is written");
        assert_eq!(
            std::fs::read_to_string(&path).expect("read the file"),
            format!("{}\n", std::process::id())
        );
    }

    /// The removal leaves a file that carries somebody else's number
    /// alone.
    ///
    /// This is the handover case: the outgoing generation exits while
    /// the incoming one already owns the path. Deleting the file
    /// would leave the operator without a PID to address.
    #[test]
    fn the_removal_leaves_another_process_s_pid_file_alone() {
        let dir = tempfile::TempDir::new().expect("tempdir");
        let path = dir.path().join("ngx.pid");

        std::fs::write(&path, "999999\n").expect("write another process's pid file");
        remove(&path).expect("the removal succeeds");
        assert!(
            path.exists(),
            "the file of another process must survive the removal"
        );
    }

    /// Removing a file that is already gone is not an error.
    ///
    /// The exit path runs the removal whether or not the write
    /// succeeded — the write can fail on a read-only directory.
    #[test]
    fn removing_a_missing_pid_file_is_not_an_error() {
        let dir = tempfile::TempDir::new().expect("tempdir");
        let path = dir.path().join("ngx.pid");
        remove(&path).expect("removing a missing file succeeds");
    }

    /// The guard removes the file when it is dropped.
    #[test]
    fn the_guard_removes_the_file_on_drop() {
        let dir = tempfile::TempDir::new().expect("tempdir");
        let path = dir.path().join("ngx.pid");

        {
            let _guard = PidFileGuard::new(path.clone());
            write(&path).expect("the pid file is written");
            assert!(path.exists());
        }
        assert!(!path.exists(), "the guard removed the file");
    }
}
