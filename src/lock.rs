//! Bookmark locks, stored in repository-scoped jj config.
//!
//! Repository scope means a lock travels with the repository rather than with the machine, and it
//! is readable from every workspace of that repository.

use std::collections::BTreeSet;

use anyhow::{Context, Result};
use serde_json;

use crate::jj;
use crate::types::Bookmark;

/// The jj config key holding the lock list, as a JSON array of bookmark names.
pub const LOCK_KEY: &str = "jj-cleanup.locked-bookmarks";

/// Reads the locked bookmarks.
///
/// A malformed value is an error, never an empty set: silently dropping locks would turn locked
/// bookmarks back into cleanup candidates.
pub fn load() -> Result<BTreeSet<Bookmark>> {
    match jj::config_get(LOCK_KEY)? {
        None => Ok(BTreeSet::new()),
        Some(raw) => serde_json::from_str(&raw).with_context(|| {
            format!("`{LOCK_KEY}` in this repository's jj config is not a JSON list of bookmark names: {raw}")
        }),
    }
}

/// Adds bookmarks to the lock list and returns the new list.
pub fn lock(names: &[Bookmark]) -> Result<BTreeSet<Bookmark>> {
    let mut locks = load()?;
    locks.extend(names.iter().cloned());
    store(&locks)?;
    Ok(locks)
}

/// Removes bookmarks from the lock list and returns the new list.
pub fn unlock(names: &[Bookmark]) -> Result<BTreeSet<Bookmark>> {
    let mut locks = load()?;
    for name in names {
        locks.remove(name);
    }
    store(&locks)?;
    Ok(locks)
}

/// Writes the lock list back to jj config.
fn store(locks: &BTreeSet<Bookmark>) -> Result<()> {
    let names: Vec<&str> = locks.iter().map(Bookmark::as_str).collect();
    let value = serde_json::to_string(&names).context("Failed to serialize the lock list")?;
    jj::config_set_repo(LOCK_KEY, &value)
}
