//! One process per library: an exclusive OS lock on `<library>/catalog.lock`, held for as long as
//! the library is open (issue #99).
//!
//! Two processes on the same library (the app and `lightcraft-cli --library`, two app instances,
//! two computers on a shared folder) would each keep their own catalog and `seq` counter, and
//! whichever wrote the last snapshot would silently drop the other's edits. So opening a library
//! takes the lock first, and a second opener gets [`LockError::InUse`] instead.
//!
//! - The lock is the OS's (`flock` on Unix, `LockFileEx` on Windows, via [`std::fs::File::try_lock`]):
//!   it is released explicitly when the [`LibraryLock`] is dropped and, after a crash or a kill, by
//!   the OS when the process ends. A `catalog.lock` file left behind is therefore never "stale":
//!   only a lock that's actually held refuses the open.
//! - Dropping the lock calls [`std::fs::File::unlock`] rather than relying on the handle closing.
//!   On Unix an `flock` lock belongs to the open file description and lasts while *any* descriptor
//!   on it is open; a process that another thread is spawning holds a copy of every descriptor
//!   (`O_CLOEXEC` only closes them at `execve`), so closing ours would leave the library "already
//!   open" for a moment and a re-open would be refused (issue #680). `LOCK_UN` through our
//!   descriptor frees the lock whatever copies a child still holds.
//! - Who holds it (process id, computer, program, version) is written to `catalog.lock.owner`, for
//!   the error message. It's a separate file because a Windows lock also blocks reading the
//!   locked file. It's only a hint: it may be missing or left over from a crash.
//! - File systems without lock support (some network shares): the library opens unlocked, with a
//!   warning — refusing it would lock the user out of their own library. Locks over SMB/NFS are
//!   advisory and depend on the server; the owner file is then the only hint.
//! - Browser storage (OPFS) has no file locks: see the web host for its own guard.

use std::fs::{File, OpenOptions, TryLockError};
use std::io;
use std::path::{Path, PathBuf};

/// The lock file in the library directory.
pub const LOCK: &str = "catalog.lock";
/// Who holds the lock (JSON, see [`LockOwner`]).
pub const OWNER: &str = "catalog.lock.owner";

/// Who has the library open (as written in [`OWNER`]).
#[derive(Clone, Debug, Default, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(default)]
pub struct LockOwner {
    pub pid: u32,
    /// Computer name, if known.
    pub host: String,
    /// The program (`LightCraft`, `lightcraft-cli`…) and its version.
    pub program: String,
    pub version: String,
    /// When it opened the library (seconds since 1970).
    pub since: u64,
}

impl LockOwner {
    /// This process, as `program`.
    pub fn this_process(program: &str) -> LockOwner {
        LockOwner {
            pid: std::process::id(),
            host: host_name(),
            program: program.to_string(),
            version: env!("CARGO_PKG_VERSION").to_string(),
            since: web_time::SystemTime::now().duration_since(web_time::UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0),
        }
    }

    /// "LightCraft 0.3.0 (process 123 on studio-mac)".
    pub fn describe(&self) -> String {
        let program = if self.program.is_empty() { "another LightCraft" } else { self.program.as_str() };
        let version = if self.version.is_empty() { String::new() } else { format!(" {}", self.version) };
        let host = if self.host.is_empty() {
            String::new()
        } else if self.host == host_name() {
            " on this computer".to_string()
        } else {
            format!(" on {}", self.host)
        };
        format!("{program}{version} (process {}{host})", self.pid)
    }
}

/// The computer's name, best effort (empty if unknown).
fn host_name() -> String {
    let env = ["COMPUTERNAME", "HOSTNAME"].iter().find_map(|k| std::env::var(k).ok().filter(|h| !h.trim().is_empty()));
    env.or_else(|| std::fs::read_to_string("/etc/hostname").ok()).map(|h| h.trim().to_string()).filter(|h| !h.is_empty()).unwrap_or_default()
}

#[derive(Debug, thiserror::Error)]
pub enum LockError {
    /// Another process has the library open.
    #[error("This library is already open in {}. Close it there first.", .0.as_ref().map(LockOwner::describe).unwrap_or_else(|| "another LightCraft program".into()))]
    InUse(Option<LockOwner>),
    /// Unsafe lock entries must not silently disable mutual exclusion.
    #[error("The library lock is not a safe regular file: {0}")]
    UnsafeFile(io::Error),
}

/// The exclusive lock on a library directory; released on drop (and by the OS if the process
/// dies).
#[derive(Debug)]
pub struct LibraryLock {
    dir: PathBuf,
    /// `None` when the file system can't lock (the library is open unlocked).
    file: Option<File>,
    held: bool,
}

fn unsafe_lock_entry(error: &io::Error, path: &Path) -> bool {
    if let Ok(metadata) = std::fs::symlink_metadata(path) {
        if metadata.file_type().is_symlink() || (!metadata.is_file() && !metadata.is_dir()) {
            return true;
        }
        #[cfg(windows)]
        {
            use std::os::windows::fs::MetadataExt;
            if metadata.file_attributes() & 0x400 != 0 {
                return true;
            }
        }
    }
    if error.kind() == io::ErrorKind::InvalidInput {
        return true;
    }
    #[cfg(unix)]
    if error.raw_os_error() == Some(libc::ELOOP) {
        return true;
    }
    false
}

impl LibraryLock {
    /// Lock `dir` (which must exist). Reject a held lock or an unsafe lock-file entry.
    pub fn acquire(dir: &Path, program: &str) -> Result<LibraryLock, LockError> {
        let unlocked = |why: io::Error| {
            log::warn!("library {}: can't lock {LOCK} ({why}); opening it without protection against a second program", dir.display());
            LibraryLock { dir: dir.to_path_buf(), file: None, held: false }
        };
        let file = match crate::safe_file::open_regular(&dir.join(LOCK), OpenOptions::new().read(true).write(true).create(true).truncate(false)) {
            Ok(f) => f,
            Err(e) if unsafe_lock_entry(&e, &dir.join(LOCK)) => return Err(LockError::UnsafeFile(e)),
            Err(e) => return Ok(unlocked(e)),
        };
        match file.try_lock() {
            Ok(()) => {}
            Err(TryLockError::WouldBlock) => {
                let owner =
                    crate::safe_file::read_regular(&dir.join(OWNER), 64 * 1024).ok().and_then(|b| serde_json::from_slice::<LockOwner>(&b).ok());
                return Err(LockError::InUse(owner));
            }
            Err(TryLockError::Error(e)) => return Ok(unlocked(e)),
        }
        let owner = LockOwner::this_process(program);
        if let Ok(json) = serde_json::to_vec_pretty(&owner)
            && let Err(e) = crate::safe_file::write_atomic(&dir.join(OWNER), &json)
        {
            log::warn!("library {}: can't write {OWNER}: {e}", dir.display());
        }
        Ok(LibraryLock { dir: dir.to_path_buf(), file: Some(file), held: true })
    }

    /// The OS lock is held (`false`: the file system can't lock; opened unprotected).
    pub fn held(&self) -> bool {
        self.held
    }

    pub fn dir(&self) -> &Path {
        &self.dir
    }

    /// Another descriptor on the locked file: what a process spawned by another thread holds
    /// between fork and exec (a `dup` refers to the same open file description). Tests only (the
    /// test that uses it is Unix-only: `flock` semantics).
    #[cfg(all(test, unix))]
    pub(crate) fn descriptor_copy(&self) -> Option<File> {
        self.file.as_ref().and_then(|f| f.try_clone().ok())
    }
}

impl Drop for LibraryLock {
    /// Remove the owner note while still holding the lock (so it can't delete the next holder's),
    /// then release the lock explicitly. Closing the handle isn't enough on Unix: `flock` lasts
    /// while any copy of the descriptor is open, and a process another thread is spawning holds a
    /// copy of every descriptor until it execs (issue #680). If the unlock fails the handle still
    /// closes right after, which releases the lock once every copy is gone, as before.
    fn drop(&mut self) {
        if self.held {
            let _ = std::fs::remove_file(self.dir.join(OWNER));
            if let Some(f) = &self.file
                && let Err(e) = f.unlock()
            {
                log::warn!("library {}: can't release {LOCK}: {e}", self.dir.display());
            }
        }
    }
}

#[cfg(all(test, unix))]
mod tests_owner_link {
    use super::*;

    #[test]
    fn linked_lock_is_an_error_not_unlocked_access() {
        let dir = std::env::temp_dir().join(format!("lc-lock-link-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let victim = dir.join("victim");
        std::fs::write(&victim, b"keep").unwrap();
        std::os::unix::fs::symlink(&victim, dir.join(LOCK)).unwrap();
        assert!(matches!(LibraryLock::acquire(&dir, "test"), Err(LockError::UnsafeFile(_))));
        assert_eq!(std::fs::read(victim).unwrap(), b"keep");
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn owner_note_replaces_link_without_touching_target() {
        let dir = std::env::temp_dir().join(format!("lc-owner-link-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let victim = dir.join("victim");
        std::fs::write(&victim, b"keep").unwrap();
        std::os::unix::fs::symlink(&victim, dir.join(OWNER)).unwrap();
        let lock = LibraryLock::acquire(&dir, "test").unwrap();
        assert_eq!(std::fs::read(&victim).unwrap(), b"keep");
        assert!(!std::fs::symlink_metadata(dir.join(OWNER)).unwrap().file_type().is_symlink());
        drop(lock);
        assert_eq!(std::fs::read(&victim).unwrap(), b"keep");
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
