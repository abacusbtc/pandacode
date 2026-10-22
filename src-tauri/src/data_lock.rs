//! One writer per data directory.
//!
//! The desktop app and `pandacode-gateway` open the same `kingcode.sqlite3`, so
//! both take this lock before they open the store. On unix it is a kernel
//! advisory lock (`flock`) held by an open descriptor: the kernel releases it
//! when the process dies, including on SIGKILL, so holding the lock is the state
//! and there is no stale-lock recovery to get wrong. The file's contents are the
//! holder's PID for a human reading the directory — never read back to decide
//! anything, because a PID can be reused and a file can be stale.

use std::path::{Path, PathBuf};

const LOCK_FILE: &str = "kingcode.lock";

/// Held for as long as the process needs the database.
pub struct Lock {
    /// Keeps the descriptor — and with it the kernel lock — alive on unix.
    _file: std::fs::File,
}

/// What `claim` learned.
pub enum Claim {
    /// This process is now the writer.
    Held(Lock),
    /// A live process holds it; the message names the directory and its PID.
    Busy(String),
    /// This platform or filesystem cannot prove exclusivity, so it is unproven.
    Unavailable(String),
}

#[cfg(unix)]
pub fn claim(data_dir: &Path, force: bool) -> Claim {
    use std::os::unix::io::AsRawFd;

    let path = match prepare(data_dir) {
        Ok(path) => path,
        Err(claim) => return claim,
    };
    let file = match open(&path) {
        Ok(file) => file,
        Err(claim) => return claim,
    };
    // LOCK_NB: never wait. A held lock is a refusal, not a queue.
    if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } == 0 {
        return held(file, &path);
    }
    let error = std::io::Error::last_os_error();
    if error.raw_os_error() == Some(libc::EWOULDBLOCK) {
        // --force-owner deliberately cannot take a live lock here: a flag that
        // can silently steal a live writer is worse than no flag.
        return Claim::Busy(busy_message(data_dir, &path, force));
    }
    // The kernel could not answer "is anyone else writing?". A gateway that
    // cannot promise it is the only writer is not a gateway, so this is a refusal
    // rather than a warning: starting anyway produces the corruption the lock
    // exists to prevent.
    Claim::Unavailable(format!(
        "cannot lock {}: {error}\nstart the gateway with --data-dir on a filesystem that supports file locking",
        path.display()
    ))
}

/// No kernel lock here, and no portable way to ask whether a recorded PID is
/// still alive, so this build does not pretend to guard the database on this
/// platform: it records the PID for a human and reports Held. Two writers remain
/// possible here, which is a known limitation rather than an oversight — the
/// desktop check is a no-op on these platforms for the same reason, and the
/// gateway's served target is unix.
#[cfg(not(unix))]
pub fn claim(data_dir: &Path, _force: bool) -> Claim {
    let path = match prepare(data_dir) {
        Ok(path) => path,
        Err(claim) => return claim,
    };
    let file = match open(&path) {
        Ok(file) => file,
        Err(claim) => return claim,
    };
    if let Some(pid) = read_pid(&path) {
        eprintln!(
            "{} records pid {pid}; this platform has no kernel lock, so exclusivity is unproven",
            path.display()
        );
    }
    held(file, &path)
}

fn prepare(data_dir: &Path) -> Result<PathBuf, Claim> {
    std::fs::create_dir_all(data_dir).map_err(|error| {
        Claim::Unavailable(format!("cannot create {}: {error}", data_dir.display()))
    })?;
    Ok(data_dir.join(LOCK_FILE))
}

fn open(path: &Path) -> Result<std::fs::File, Claim> {
    std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        // Never truncate on open: the file exists to be locked, and its contents
        // belong to whoever holds it, not to whoever is asking.
        .truncate(false)
        .open(path)
        .map_err(|error| Claim::Unavailable(format!("cannot open {}: {error}", path.display())))
}

/// Records our PID for a human and returns the held lock. Failing to write the
/// number is not failing to hold the lock, so it stays best effort.
fn held(file: std::fs::File, path: &Path) -> Claim {
    let _ = std::fs::write(path, format!("{}\n", std::process::id()));
    Claim::Held(Lock { _file: file })
}

fn busy_message(data_dir: &Path, path: &Path, force: bool) -> String {
    let holder = read_pid(path)
        .map(|pid| format!(" (pid {pid})"))
        .unwrap_or_default();
    let mut message = format!(
        "{} is in use by another process{holder}: a second PandaCode or the pandacode-gateway holds the database lock.\nStop that process and try again.",
        data_dir.display()
    );
    if force {
        message.push_str("\n--force-owner does not take a live lock; stop the process instead.");
    }
    message
}

fn read_pid(path: &Path) -> Option<u32> {
    let text = std::fs::read_to_string(path).ok()?;
    text.trim().parse().ok()
}
