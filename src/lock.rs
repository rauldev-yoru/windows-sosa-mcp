//! Cross-process `busy` lock (`<state>\mcp.lock`).
//!
//! One tool call at a time per user, across every server process: a
//! non-blocking exclusive OS file lock (`LockFileEx` on Windows, `flock`
//! elsewhere) on `<state>\mcp.lock`, held for the whole call. A second
//! call — from the same or another process — gets `busy` immediately.

use std::fs::{File, OpenOptions, TryLockError};
use std::path::Path;

pub struct Guard(#[allow(dead_code)] File);

pub enum Acquire {
    Held(Guard),
    Busy,
    Error(std::io::Error),
}

pub fn try_acquire(path: &Path) -> Acquire {
    if let Some(dir) = path.parent()
        && let Err(e) = std::fs::create_dir_all(dir)
    {
        return Acquire::Error(e);
    }
    let f = match OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(path)
    {
        Ok(f) => f,
        Err(e) => return Acquire::Error(e),
    };
    match f.try_lock() {
        Ok(()) => Acquire::Held(Guard(f)),
        Err(TryLockError::WouldBlock) => Acquire::Busy,
        Err(TryLockError::Error(e)) => Acquire::Error(e),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn second_holder_is_busy_until_release() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("mcp.lock");
        let a = try_acquire(&p);
        assert!(matches!(a, Acquire::Held(_)));
        assert!(matches!(try_acquire(&p), Acquire::Busy));
        drop(a);
        assert!(matches!(try_acquire(&p), Acquire::Held(_)));
    }
}
