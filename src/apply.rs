//! Applying a plan.

use std::path::Path;

use anyhow::{Context, Result};

use crate::jj;
use crate::plan::{Action, Plan};

/// Applies a plan in its planned order.
///
/// Workspace actions come first, so no live working copy is left pointing at revisions that are
/// about to be abandoned. Each jj command is its own operation, so applying a plan is a sequence
/// of undoable steps rather than one irreversible edit.
pub fn execute(plan: &Plan) -> Result<()> {
    for action in &plan.actions {
        match action {
            Action::ForgetWorkspace { name } => jj::workspace_forget(name)?,
            Action::RemoveWorkspace { name, root } => {
                // Forget first: if deleting the directory fails, the leftover is an untracked
                // directory rather than a workspace whose directory has been gutted.
                jj::workspace_forget(name)?;
                remove_directory(root)?;
            }
            Action::AbandonBookmark { revset, .. } => jj::abandon(revset)?,
            Action::DeleteBookmark { bookmark, .. } => jj::bookmark_delete(bookmark)?,
        }
    }
    Ok(())
}

/// Deletes a workspace directory that jj no longer tracks.
fn remove_directory(root: &Path) -> Result<()> {
    std::fs::remove_dir_all(root).with_context(|| format!("Failed to delete {}", root.display()))
}
