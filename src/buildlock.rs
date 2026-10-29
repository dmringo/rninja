//! Per-build-directory lock for concurrent rninja invocations.
//!
//! Ninja has no cross-process lock, so this only protects rninja against other
//! rninja processes. The lock file is advisory on Unix and enforced by a
//! no-sharing file handle on Windows; it is intentionally left in place so
//! removing it cannot race with another process opening it.

use std::fs::{File, OpenOptions};
use std::io::{self, Seek, SeekFrom, Write};
use std::path::Path;

const LOCK_FILE: &str = ".rninja_lock";

/// Held for the duration of a build. Dropping it releases the lock.
pub struct BuildLock {
    /// Keeping the handle alive holds the lock. The OS releases it on drop.
    _file: File,
}

impl BuildLock {
    /// Try to acquire the build lock without waiting.
    ///
    /// Returns [`io::ErrorKind::WouldBlock`] if another rninja process owns it.
    pub fn acquire(dir: &Path) -> io::Result<Self> {
        let path = dir.join(LOCK_FILE);
        let mut file = open_locked(&path)?;
        write_owner(&mut file)?;
        Ok(Self { _file: file })
    }
}

fn busy(path: &Path) -> io::Error {
    io::Error::new(
        io::ErrorKind::WouldBlock,
        format!(
            "another rninja build is already running ({})",
            path.display()
        ),
    )
}

fn write_owner(file: &mut File) -> io::Result<()> {
    file.set_len(0)?;
    file.seek(SeekFrom::Start(0))?;
    writeln!(file, "{}", std::process::id())?;
    file.flush()
}

#[cfg(windows)]
fn open_locked(path: &Path) -> io::Result<File> {
    use std::os::windows::fs::OpenOptionsExt;

    let mut options = OpenOptions::new();
    options
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .share_mode(0);

    match options.open(path) {
        Ok(file) => Ok(file),
        Err(error)
            if error.kind() == io::ErrorKind::PermissionDenied
                || error.raw_os_error() == Some(32) =>
        {
            Err(busy(path))
        }
        Err(error) => Err(error),
    }
}

#[cfg(unix)]
fn open_locked(path: &Path) -> io::Result<File> {
    use std::os::unix::io::AsRawFd;

    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(path)?;

    let result = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
    if result == 0 {
        Ok(file)
    } else {
        let error = io::Error::last_os_error();
        if error.kind() == io::ErrorKind::WouldBlock {
            Err(busy(path))
        } else {
            Err(error)
        }
    }
}

#[cfg(not(any(windows, unix)))]
fn open_locked(path: &Path) -> io::Result<File> {
    // Best effort on platforms without advisory locks.
    OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(path)
}

// The OS releases the lock when the file handle closes. Leaving the file in
// place avoids an unlink/open race between processes.
