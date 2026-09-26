//! Shell layer over the `jj` CLI.
//!
//! Everything this module reads comes from jj templates rather than human-readable output, so
//! user configuration (colors, wrapping, pager) cannot corrupt a parse.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::LazyLock;

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};

use crate::gh::PrNum;
use crate::types::{Bookmark, CommitId, Owner, Remote, Repo, Revset};

/// Oldest supported jj.
///
/// `WorkspaceRef.root()` — and therefore a workspace root in `jj workspace list -T` — landed in
/// jj 0.44.0 ([jj#9713](https://github.com/jj-vcs/jj/pull/9713)). The root is what identifies a
/// workspace's directory, which this tool needs for both its safety checks and `--remove`.
pub const MIN_JJ_VERSION: &str = "0.44.0";

/// The commit template. Byte-identical to the one `jj-pr` uses, plus `above_trunk`.
///
/// `above_trunk` answers "could this commit be inside an abandoned set?" without a second jj
/// invocation: `self.contained_in("trunk()..")` is true exactly when the commit is *above* trunk,
/// which is the half of `::tip ~ ::trunk()` that can hold a discarded revision.
const JJ_TEMPLATE: &str = concat!(
    r#""{\"commit\": " ++ json(self)"#,
    r#" ++ ", \"local_bookmarks\": " ++ json(local_bookmarks)"#,
    r#" ++ ", \"is_trunk_tip\": " ++ json(self.contained_in("trunk()"))"#,
    r#" ++ ", \"empty\": " ++ json(self.empty())"#,
    r#" ++ ", \"above_trunk\": " ++ json(self.contained_in("trunk().."))"#,
    r#" ++ "}\n""#,
);

/// Tab-separated workspace fields. `json(root)` distinguishes an absent root (`null`) from a path
/// that merely looks empty, and `target()` is the workspace's working-copy commit.
const WORKSPACE_LIST_TEMPLATE: &str =
    r#"name ++ "\t" ++ json(root) ++ "\t" ++ self.target().commit_id() ++ "\n""#;

/// Raw commit data from `json(self)`.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct JjCommit {
    /// The commit's SHA.
    pub commit_id: CommitId,
    /// Parent commit SHAs, in order.
    pub parents: Vec<CommitId>,
    /// The full commit description.
    pub description: String,
}

/// A bookmark as it appears in `json(local_bookmarks)` or `json(remote_bookmarks)`.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct JjBookmark {
    /// The bookmark name.
    pub name: Bookmark,
    /// Target commits. A non-conflicted bookmark has exactly one `Some` entry; a conflicted
    /// bookmark has several entries, or a `None` for a deleted side.
    pub target: Vec<Option<CommitId>>,
}

/// One line of the commit template: a commit plus the derived facts the planner needs.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct JjLogEntry {
    /// The commit itself.
    pub commit: JjCommit,
    /// Local bookmarks pointing at this commit.
    pub local_bookmarks: Vec<JjBookmark>,
    /// Whether the commit is contained in `trunk()`.
    pub is_trunk_tip: bool,
    /// Whether the commit has no uncommitted changes.
    pub empty: bool,
    /// Whether the commit is a descendant of trunk, i.e. outside `::trunk()`.
    pub above_trunk: bool,
}

impl JjBookmark {
    /// The single commit this bookmark points at, or `None` when it is conflicted.
    ///
    /// A conflicted bookmark is exactly the case this returns `None` for: jj represents the
    /// conflict as either more than one target or a `None` side.
    pub fn resolved_target(&self) -> Option<&CommitId> {
        match self.target.as_slice() {
            [Some(commit_id)] => Some(commit_id),
            _ => None,
        }
    }

    /// Whether this bookmark is conflicted.
    pub fn is_conflicted(&self) -> bool {
        self.resolved_target().is_none()
    }
}

/// A workspace as reported by `jj workspace list`, plus what could be read from inside it.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
pub struct WorkspaceInfo {
    /// Workspace name (`default` is the original working copy).
    pub name: String,
    /// The workspace directory, when jj reports one.
    pub root: Option<PathBuf>,
    /// The working-copy commit jj records for this workspace.
    pub target: CommitId,
    /// Whether this is the workspace the process is running in.
    pub current: bool,
    /// Live working-copy state, or `None` when the workspace could not be read. `None` is a
    /// skip: a workspace whose state is unknown is never cleaned.
    pub head: Option<WorkspaceHead>,
}

/// Live working-copy state read from inside a workspace.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
pub struct WorkspaceHead {
    /// The working-copy commit.
    pub commit_id: CommitId,
    /// Whether the working copy has no uncommitted changes.
    pub empty: bool,
    /// Whether the working-copy commit is a descendant of trunk.
    pub above_trunk: bool,
}

#[derive(Deserialize)]
struct WorkspaceHeadWire {
    commit: JjCommit,
    empty: bool,
    above_trunk: bool,
}

/// Creates a `jj log --no-graph` command with word-wrap disabled.
///
/// Word wrap would otherwise inject newlines into the JSONL output when a user config enables it.
fn jj_log_command() -> Command {
    let mut cmd = Command::new("jj");
    cmd.args(["log", "--no-graph", "--config", "ui.log-word-wrap=false"]);
    cmd
}

/// Runs a jj command that is expected to produce no output worth parsing.
fn run_jj(args: &[&str]) -> Result<()> {
    let display = args.join(" ");
    let output = Command::new("jj")
        .args(args)
        .output()
        .with_context(|| format!("Failed to run `jj {display}`"))?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        bail!("`jj {display}` failed: {}", stderr.trim());
    }
    Ok(())
}

/// Parses a `PR: #<n>` trailer out of a commit description.
///
/// Trailers are the `Key: Value` lines at the end of the description, so only the final block is
/// considered; the search stops at the blank line that separates it from the prose.
pub fn parse_pr_trailer(description: &str) -> Option<PrNum> {
    let mut in_trailer_block = false;
    for line in description.lines().rev() {
        let line = line.trim();
        if line.is_empty() {
            if in_trailer_block {
                break;
            }
            continue;
        }
        in_trailer_block = true;
        // Written without `let` chains, which would raise the minimum toolchain to Rust 1.88.
        let number = line
            .strip_prefix("PR: #")
            .and_then(|value| value.trim().parse::<u64>().ok())
            .and_then(PrNum::new);
        if let Some(number) = number {
            return Some(number);
        }
    }
    None
}

/// Loads every commit matching `revset` as structured entries.
pub fn load_entries(revset: &str) -> Result<Vec<JjLogEntry>> {
    let output = jj_log_command()
        .args(["-r", revset, "-T", JJ_TEMPLATE])
        .output()
        .context("Failed to run `jj log`")?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        bail!("jj log failed: {}", stderr.trim());
    }

    let stdout = String::from_utf8(output.stdout).context("jj log output not UTF-8")?;
    let mut entries = Vec::new();
    for line in stdout.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let entry = serde_json::from_str::<JjLogEntry>(line).with_context(|| {
            let truncated = line.char_indices().nth(80).map_or(line, |(idx, _)| &line[..idx]);
            format!(
                "Failed to parse jj log output as JSON.\n  \
                 Hint: check your jj config for settings that alter template output.\n  \
                 Content: {truncated}"
            )
        })?;
        entries.push(entry);
    }
    Ok(entries)
}

/// The revset every planning run is built on: trunk, everything above it, and every local
/// bookmark target.
///
/// The bookmark term is what makes stale bookmarks visible. A bookmark whose commit already
/// landed in trunk is an ancestor of trunk, so `trunk().. | trunk()` alone would never mention
/// it, and the planner would never be able to delete it.
pub const PLANNING_REVSET: &str = "trunk().. | trunk() | bookmarks()";

/// Reads the live working-copy state of the workspace rooted at `root`.
///
/// This snapshots that workspace, because that is the only way to see its uncommitted changes.
/// Snapshotting is additive — it records the working copy, it never discards it.
pub fn read_workspace_head(root: &Path) -> Result<WorkspaceHead> {
    let output = jj_log_command()
        .current_dir(root)
        .args(["-r", "@", "-T", JJ_TEMPLATE])
        .output()
        .with_context(|| format!("Failed to run `jj log` in {}", root.display()))?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        bail!("jj log failed in {}: {}", root.display(), stderr.trim());
    }

    let stdout = String::from_utf8(output.stdout).context("jj log output not UTF-8")?;
    let line = stdout
        .lines()
        .find(|line| !line.trim().is_empty())
        .context("jj log reported no working copy")?;
    let wire: WorkspaceHeadWire =
        serde_json::from_str(line).context("Failed to parse jj workspace head")?;
    Ok(WorkspaceHead {
        commit_id: wire.commit.commit_id,
        empty: wire.empty,
        above_trunk: wire.above_trunk,
    })
}

/// Lists workspaces.
///
/// `read_heads` controls whether each workspace's live working copy is read, which is what detects
/// uncommitted changes. Reading a workspace snapshots it: that is the only way to see uncommitted
/// changes, and snapshotting only ever adds information. A caller that does not plan workspace
/// cleanup passes `false` and leaves other workspaces untouched.
pub fn load_workspaces(read_heads: bool) -> Result<Vec<WorkspaceInfo>> {
    let output = Command::new("jj")
        .args(["workspace", "list", "-T", WORKSPACE_LIST_TEMPLATE])
        .output()
        .context("Failed to run `jj workspace list`")?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        bail!("jj workspace list failed: {}", stderr.trim());
    }

    let stdout = String::from_utf8(output.stdout).context("jj workspace list output not UTF-8")?;
    let current_root = current_workspace_root();
    let mut workspaces = Vec::new();
    for line in stdout.lines() {
        if line.is_empty() {
            continue;
        }
        let mut fields = line.split('\t');
        let (Some(name), Some(root_json), Some(target)) =
            (fields.next(), fields.next(), fields.next())
        else {
            bail!("Unexpected `jj workspace list` output: {line:?}");
        };
        let root: Option<PathBuf> = serde_json::from_str(root_json).with_context(|| {
            format!("Unexpected workspace root in `jj workspace list` output: {root_json:?}")
        })?;
        let head = match read_heads {
            true => root.as_deref().and_then(|root| read_workspace_head(root).ok()),
            false => None,
        };
        let current = match (&root, &current_root) {
            (Some(root), Some(current_root)) => root == current_root,
            _ => false,
        };
        workspaces.push(WorkspaceInfo {
            name: name.to_owned(),
            root,
            target: CommitId::new(target),
            current,
            head,
        });
    }
    Ok(workspaces)
}

/// The root of the workspace this process is running in.
///
/// `jj workspace root` is authoritative, unlike comparing against the process's working
/// directory, which a symlinked path can defeat.
fn current_workspace_root() -> Option<PathBuf> {
    let output = Command::new("jj").args(["workspace", "root"]).output().ok()?;
    if !output.status.success() {
        return None;
    }
    let stdout = String::from_utf8(output.stdout).ok()?;
    let trimmed = stdout.trim();
    (!trimmed.is_empty()).then(|| PathBuf::from(trimmed))
}

/// The directory holding the repository's git store, which is the primary working copy's
/// directory in a colocated repository.
///
/// Used as one more guard against touching the primary workspace: jj names that workspace
/// `default`, but a renamed primary workspace is still identified here.
pub fn primary_workspace_root() -> Option<PathBuf> {
    git_root().ok().and_then(|root| root.parent().map(Path::to_path_buf))
}

/// Parses a GitHub owner and repository out of a git remote URL.
///
/// Understands the HTTPS and SSH forms of a GitHub URL; anything else returns `None`.
pub fn parse_github_remote(url: &str) -> Option<(Owner, Repo)> {
    let path =
        url.strip_prefix("https://github.com/").or_else(|| url.strip_prefix("git@github.com:"))?;
    let mut parts = path.split('/');
    let owner = parts.next()?;
    let repo = parts.next()?;
    if owner.is_empty() || repo.is_empty() {
        return None;
    }
    let repo = repo.strip_suffix(".git").unwrap_or(repo);
    Some((Owner::new(owner), Repo::new(repo)))
}

/// Maps every GitHub remote to the owner and repository it points at.
pub fn load_remotes() -> Result<BTreeMap<Remote, (Owner, Repo)>> {
    let output = Command::new("jj")
        .args(["git", "remote", "list"])
        .output()
        .context("Failed to run `jj git remote list`")?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        bail!("jj git remote list failed: {}", stderr.trim());
    }
    let stdout = String::from_utf8(output.stdout).context("jj git remote list output not UTF-8")?;
    let mut map = BTreeMap::new();
    for line in stdout.lines() {
        let Some((name, url)) = line.split_once(' ') else {
            continue;
        };
        if let Some(identity) = parse_github_remote(url.trim()) {
            map.insert(Remote::from(name), identity);
        }
    }
    Ok(map)
}

/// Fetches from `remotes`, or from jj's configured default when the list is empty.
pub fn git_fetch(remotes: &[Remote]) -> Result<()> {
    let mut args = vec!["git".to_owned(), "fetch".to_owned()];
    for remote in remotes {
        args.push("--remote".to_owned());
        // `--remote` takes a string pattern; `exact:` keeps a remote name containing glob
        // metacharacters from matching some other remote.
        args.push(format!("exact:{remote}"));
    }
    let borrowed: Vec<&str> = args.iter().map(String::as_str).collect();
    run_jj(&borrowed)
}

/// The repository's git directory, via `jj git root`. Cached; `gh` needs it to resolve the
/// repository from any workspace, not just the one holding `.git`.
pub fn git_root() -> Result<&'static Path> {
    static GIT_ROOT: LazyLock<Result<PathBuf, String>> = LazyLock::new(|| {
        let output = Command::new("jj")
            .args(["git", "root"])
            .output()
            .map_err(|e| format!("failed to run `jj git root`: {e}"))?;
        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            return Err(format!("jj git root failed: {}", stderr.trim()));
        }
        let path = String::from_utf8(output.stdout)
            .map_err(|e| format!("jj git root output not UTF-8: {e}"))?;
        Ok(PathBuf::from(path.trim()))
    });

    GIT_ROOT.as_ref().map(PathBuf::as_path).map_err(|e| anyhow::anyhow!("{e}"))
}

/// Abandons every revision in `revset`.
///
/// jj deletes any bookmark pointing at an abandoned commit, so a bookmark listed in the revset is
/// removed together with its revisions.
pub fn abandon(revset: &Revset) -> Result<()> {
    run_jj(&["abandon", revset.as_str()])
}

/// Deletes a bookmark. Deleting a bookmark that no longer exists is not an error.
pub fn bookmark_delete(name: &Bookmark) -> Result<()> {
    run_jj(&["bookmark", "delete", name.as_str()])
}

/// Stops tracking a workspace. The workspace directory is left on disk.
pub fn workspace_forget(name: &str) -> Result<()> {
    run_jj(&["workspace", "forget", name])
}

/// Reads a jj config value. A missing key is `Ok(None)`; other failures are errors.
pub fn config_get(key: &str) -> Result<Option<String>> {
    let output = Command::new("jj")
        .args(["config", "get", key])
        .output()
        .with_context(|| format!("Failed to run `jj config get {key}`"))?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        if stderr.contains(&format!("Value not found for {key}")) {
            return Ok(None);
        }
        bail!("jj config get {key} failed: {}", stderr.trim());
    }
    let value = String::from_utf8(output.stdout)
        .with_context(|| format!("`jj config get {key}` output not UTF-8"))?;
    Ok(Some(value.trim().to_owned()))
}

/// Writes a repository-scoped jj config value.
///
/// Repository scope is what makes a lock follow the repository rather than the machine, and it is
/// readable from every workspace of that repository.
pub fn config_set_repo(key: &str, value: &str) -> Result<()> {
    run_jj(&["config", "set", "--repo", key, value])
}

/// Refuses to run on a jj older than [`MIN_JJ_VERSION`].
pub fn version_check() -> Result<()> {
    let output =
        Command::new("jj").arg("version").output().context("Failed to run `jj version`")?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        bail!("jj version failed: {}", stderr.trim());
    }
    let stdout = String::from_utf8(output.stdout).context("jj version output not UTF-8")?;
    let reported = stdout.split_whitespace().nth(1).unwrap_or_default();
    if !version_at_least(reported, MIN_JJ_VERSION) {
        bail!(
            "jj {reported} is too old. jj-cleanup needs jj >= {MIN_JJ_VERSION} \
             (`jj workspace list` only reports workspace roots from that version)"
        );
    }
    Ok(())
}

/// Compares dotted versions, padded with zeros. Unparseable input is treated as too old, so an
/// unknown version fails closed rather than proceeding.
fn version_at_least(reported: &str, required: &str) -> bool {
    fn parts(version: &str) -> Option<[u64; 3]> {
        let mut out = [0_u64; 3];
        for (i, part) in version.split('.').take(3).enumerate() {
            let digits: String = part.chars().take_while(char::is_ascii_digit).collect();
            if digits.is_empty() {
                return None;
            }
            out[i] = digits.parse().ok()?;
        }
        Some(out)
    }
    match (parts(reported), parts(required)) {
        (Some(reported), Some(required)) => reported >= required,
        (None, _) => false,
        (Some(_), None) => true,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_pr_trailer_basic() {
        assert_eq!(parse_pr_trailer("some commit\n\nPR: #42\n"), PrNum::new(42));
    }

    #[test]
    fn parse_pr_trailer_with_other_trailers() {
        assert_eq!(
            parse_pr_trailer("fix bug\n\nCo-authored-by: Alice\nPR: #123\n"),
            PrNum::new(123)
        );
    }

    #[test]
    fn parse_pr_trailer_ignores_prose_mention() {
        assert_eq!(parse_pr_trailer("PR: #7 was reverted\n\nbecause it broke the build\n"), None);
    }

    #[test]
    fn parse_pr_trailer_missing() {
        assert_eq!(parse_pr_trailer("just a commit message\n"), None);
    }

    #[test]
    fn parse_pr_trailer_no_trailing_newline() {
        assert_eq!(parse_pr_trailer("msg\n\nPR: #7"), PrNum::new(7));
    }

    #[test]
    fn parse_github_remote_https_and_ssh() {
        assert_eq!(
            parse_github_remote("https://github.com/HotThoughts/jj-cleanup.git"),
            Some((Owner::new("HotThoughts"), Repo::new("jj-cleanup")))
        );
        assert_eq!(
            parse_github_remote("git@github.com:hydro-project/jj-pr"),
            Some((Owner::new("hydro-project"), Repo::new("jj-pr")))
        );
    }

    #[test]
    fn parse_github_remote_rejects_other_forges() {
        assert_eq!(parse_github_remote("https://gitlab.com/foo/bar"), None);
        assert_eq!(parse_github_remote("https://github.com/foo"), None);
    }

    #[test]
    fn version_comparison() {
        assert!(version_at_least("0.44.0", "0.44.0"));
        assert!(version_at_least("0.45.1", "0.44.0"));
        assert!(version_at_least("1.0.0", "0.44.0"));
        assert!(version_at_least("0.45.1-dev", "0.44.0"));
        assert!(!version_at_least("0.43.0", "0.44.0"));
        assert!(!version_at_least("0.44", "0.44.1"));
        assert!(!version_at_least("", "0.44.0"));
        assert!(!version_at_least("nightly", "0.44.0"));
    }

    #[test]
    fn conflicted_bookmark_has_no_resolved_target() {
        let conflicted = JjBookmark {
            name: Bookmark::new("x"),
            target: vec![Some(CommitId::new("a")), Some(CommitId::new("b"))],
        };
        assert!(conflicted.is_conflicted());
        assert_eq!(conflicted.resolved_target(), None);

        let deleted_side =
            JjBookmark { name: Bookmark::new("x"), target: vec![Some(CommitId::new("a")), None] };
        assert!(deleted_side.is_conflicted());

        let resolved =
            JjBookmark { name: Bookmark::new("x"), target: vec![Some(CommitId::new("a"))] };
        assert!(!resolved.is_conflicted());
        assert_eq!(resolved.resolved_target(), Some(&CommitId::new("a")));
    }
}
