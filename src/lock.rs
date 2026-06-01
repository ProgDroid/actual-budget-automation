//! Cross-platform advisory file locking via std (Rust 1.89+ stabilised
//! `File::lock`/`try_lock`). Used to serialise `aba run` and `aba poll`, which
//! both read-modify-write the shared TOML state file.

use std::fs::{File, TryLockError};
use std::path::Path;

/// Acquire an exclusive advisory lock, blocking until available. The returned
/// `File` holds the lock until it is dropped.
pub fn lock_blocking(path: &Path) -> anyhow::Result<File> {
    let f = File::create(path)?; // nosemgrep: rust.actix.path-traversal.tainted-path.tainted-path
    f.lock()?;
    Ok(f)
}

/// Try to acquire the lock without blocking. `Ok(Some(file))` = acquired,
/// `Ok(None)` = currently held by another handle/process.
pub fn lock_try(path: &Path) -> anyhow::Result<Option<File>> {
    let f = File::create(path)?; // nosemgrep: rust.actix.path-traversal.tainted-path.tainted-path
    match f.try_lock() {
        Ok(()) => Ok(Some(f)),
        Err(TryLockError::WouldBlock) => Ok(None),
        Err(TryLockError::Error(e)) => Err(e.into()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn try_lock_blocks_while_held() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("aba.lock");

        let held = lock_blocking(&path).unwrap();
        // A second handle to the same file cannot acquire while `held` lives.
        assert!(lock_try(&path).unwrap().is_none());

        drop(held);
        // Once released, it can be acquired again.
        assert!(lock_try(&path).unwrap().is_some());
    }
}
