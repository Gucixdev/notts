//! Advisory file lock (flock) so several notts processes can share one state directory.

use std::fs::{File, OpenOptions};
use std::io;
use std::os::unix::fs::OpenOptionsExt;
use std::os::unix::io::AsRawFd;
use std::path::Path;

/// Held until dropped.
pub struct FileLock {
    _f: File,
}

impl FileLock {
    /// Blocks until the exclusive lock on `path` (created mode 600) is ours.
    pub fn exclusive(path: &Path) -> io::Result<FileLock> {
        let f = OpenOptions::new().create(true).truncate(false).write(true).mode(0o600).open(path)?;
        loop {
            // SAFETY: plain flock(2) on a descriptor we own.
            let r = unsafe { libc::flock(f.as_raw_fd(), libc::LOCK_EX) };
            if r == 0 {
                return Ok(FileLock { _f: f });
            }
            let e = io::Error::last_os_error();
            if e.kind() != io::ErrorKind::Interrupted {
                return Err(e);
            }
        }
    }
}

/// Write `data` to `path` atomically: temp file in the same directory, fsync, rename, fsync dir.
pub fn atomic_write(path: &Path, data: &[u8]) -> io::Result<()> {
    atomic_write_with(path, data, false)
}

/// As `atomic_write`; with `crash_before_rename` it stops after the temp file is complete,
/// the way a killed process would. For tests.
#[doc(hidden)]
pub fn atomic_write_with(path: &Path, data: &[u8], crash_before_rename: bool) -> io::Result<()> {
    use std::io::Write;
    let dir = path.parent().unwrap_or_else(|| Path::new("."));
    let name = path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
    let tmp = dir.join(format!(".{name}.tmp{}", std::process::id()));
    {
        let mut f = OpenOptions::new().create(true).truncate(true).write(true).mode(0o600).open(&tmp)?;
        f.write_all(data)?;
        f.sync_all()?;
    }
    if crash_before_rename {
        return Ok(());
    }
    std::fs::rename(&tmp, path)?;
    if let Ok(d) = File::open(dir) {
        let _ = d.sync_all();
    }
    Ok(())
}
