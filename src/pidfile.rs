use anyhow::{bail, Result};
use std::path::{Path, PathBuf};

/// Holds the pidfile for the lifetime of the process. Deletes on drop.
pub struct PidFile {
    path: PathBuf,
}

impl PidFile {
    /// Write our PID to `path`. Bails if a live process already holds the file.
    pub fn acquire(path: &Path) -> Result<Self> {
        if let Ok(contents) = std::fs::read_to_string(path) {
            if let Ok(pid) = contents.trim().parse::<i32>() {
                let alive = unsafe { libc::kill(pid, 0) } == 0;
                if alive {
                    bail!(
                        "ghcache watch is already running (pid {pid})\n  pidfile: {}\n  Stop the running instance first.",
                        path.display()
                    );
                }
                // Stale pidfile -- process is gone, safe to overwrite.
            }
        }
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(path, format!("{}\n", std::process::id()))?;
        Ok(PidFile { path: path.to_owned() })
    }
}

impl Drop for PidFile {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}

/// Fork to background (double-fork + setsid), redirect stdio to /dev/null.
/// Returns only in the daemonized grandchild; parent and first child exit.
#[cfg(unix)]
pub fn daemonize() -> Result<()> {
    unsafe {
        // First fork: parent exits so the child can setsid.
        match libc::fork() {
            -1 => bail!("fork failed: {}", std::io::Error::last_os_error()),
            0 => {} // child continues
            _ => std::process::exit(0), // parent exits
        }

        // New session: detach from controlling terminal.
        if libc::setsid() == -1 {
            bail!("setsid failed: {}", std::io::Error::last_os_error());
        }

        // Second fork: prevents the daemon from reacquiring a terminal.
        match libc::fork() {
            -1 => bail!("fork failed: {}", std::io::Error::last_os_error()),
            0 => {} // grandchild continues
            _ => std::process::exit(0), // first child exits
        }

        // Redirect stdin/stdout/stderr to /dev/null.
        let dev_null = libc::open(
            b"/dev/null\0".as_ptr() as *const libc::c_char,
            libc::O_RDWR,
        );
        if dev_null >= 0 {
            libc::dup2(dev_null, 0);
            libc::dup2(dev_null, 1);
            libc::dup2(dev_null, 2);
            libc::close(dev_null);
        }
    }
    Ok(())
}
