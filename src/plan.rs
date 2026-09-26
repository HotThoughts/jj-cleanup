//! Planning: deciding what may be cleaned.
//!
//! This module performs no I/O. Everything it reasons about arrives in [`InputData`], which is
//! also the `util dump` fixture format, so every safety rule can be pinned by a test.

use std::cmp::Reverse;
use std::collections::{BTreeMap, BTreeSet};
use std::fmt::Write;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::gh::{GhPr, PrNum, PrState};
use crate::jj::{JjLogEntry, WorkspaceInfo};
use crate::types::{Bookmark, CommitId, Revset};

/// Which PR states make a bookmark cleanable.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, clap::ValueEnum)]
pub enum StateFilter {
    /// Only merged PRs are cleanable. The default.
    #[value(name = "merged")]
    Merged,
    /// Merged and closed PRs are cleanable.
    #[value(name = "closed")]
    Closed,
}

impl StateFilter {
    /// Whether a PR in `state` justifies cleaning its bookmark.
    pub fn accepts(self, state: PrState) -> bool {
        match self {
            Self::Merged => state == PrState::Merged,
            Self::Closed => matches!(state, PrState::Merged | PrState::Closed),
        }
    }

    /// How this filter reads in prose.
    pub fn describe(self) -> &'static str {
        match self {
            Self::Merged => "merged",
            Self::Closed => "merged or closed",
        }
    }
}

/// Everything the planner needs, and nothing else.
///
/// This is the `util dump` format and the fixture format: a plan is a pure function of one of
/// these plus [`PlanOptions`].
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct InputData {
    /// The commits to reason about, from [`crate::jj::PLANNING_REVSET`].
    pub jj_entries: Vec<JjLogEntry>,
    /// Every workspace jj reports.
    pub workspaces: Vec<WorkspaceInfo>,
    /// Every pull request found by number or head branch.
    pub prs: Vec<GhPr>,
    /// The repository's default branch, when `gh` reports one.
    pub default_bookmark: Option<Bookmark>,
    /// The directory holding the repository's git store, when that identifies the primary
    /// working copy. A workspace rooted there is never touched.
    pub primary_workspace_root: Option<PathBuf>,
    /// Bookmarks explicitly excluded from cleanup.
    pub locks: BTreeSet<Bookmark>,
    /// Observations made while gathering the state, surfaced with the plan.
    #[serde(default)]
    pub load_warnings: Vec<String>,
}

/// How to plan.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PlanOptions {
    /// Which PR states are cleanable.
    pub state: StateFilter,
    /// Whether workspaces participate at all.
    pub workspaces: bool,
    /// Whether workspace directories are deleted rather than only forgotten.
    pub remove_workspaces: bool,
}

/// One change the plan will make.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Action {
    /// Stop tracking a workspace, leaving its directory in place.
    ForgetWorkspace {
        /// Workspace name.
        name: String,
    },
    /// Stop tracking a workspace and delete its directory.
    RemoveWorkspace {
        /// Workspace name.
        name: String,
        /// The directory to delete.
        root: PathBuf,
    },
    /// Abandon the revisions above trunk that only this bookmark holds.
    AbandonBookmark {
        /// The bookmark being cleaned up.
        bookmark: Bookmark,
        /// The revset that selects the revisions to abandon.
        revset: Revset,
        /// How many revisions that revset selects right now.
        revisions: usize,
        /// The pull requests that justified the cleanup.
        prs: Vec<PrNum>,
    },
    /// Delete a bookmark whose revisions are already in trunk, so only the pointer goes.
    DeleteBookmark {
        /// The bookmark being cleaned up.
        bookmark: Bookmark,
        /// The pull requests that justified the cleanup.
        prs: Vec<PrNum>,
    },
}

/// Something the plan deliberately refused to do.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Skipped {
    /// What was skipped, e.g. `bookmark "feat-x"`.
    pub subject: String,
    /// Why.
    pub reason: String,
}

/// The complete plan.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Plan {
    /// Changes to apply, in the order they must be applied.
    pub actions: Vec<Action>,
    /// Refusals, for the user to inspect.
    pub skipped: Vec<Skipped>,
    /// Non-fatal observations.
    pub warnings: Vec<String>,
}

impl Plan {
    /// Whether the plan would change anything.
    pub fn is_empty(&self) -> bool {
        self.actions.is_empty()
    }
}

/// The loaded commit graph, restricted to the commits that were loaded.
struct Ancestry {
    /// Commit id to parents, for every loaded commit.
    parents: BTreeMap<CommitId, Vec<CommitId>>,
    /// The commits strictly above trunk.
    above_trunk: BTreeSet<CommitId>,
}

impl Ancestry {
    fn new(entries: &[JjLogEntry]) -> Self {
        let mut parents = BTreeMap::new();
        let mut above_trunk = BTreeSet::new();
        for entry in entries {
            let commit_id = entry.commit.commit_id.clone();
            if entry.above_trunk {
                above_trunk.insert(commit_id.clone());
            }
            parents.insert(commit_id, entry.commit.parents.clone());
        }
        Self { parents, above_trunk }
    }

    /// Every commit in `::tip` that is above trunk.
    ///
    /// The walk stops at the first commit that is not above trunk, which is exactly the
    /// `~ ::trunk()` half of the trimming revset: a commit above trunk has all of its parents
    /// either above trunk or in `::trunk()`, so nothing above trunk is ever missed.
    fn above_trunk_ancestors(&self, tip: &CommitId) -> BTreeSet<CommitId> {
        let mut found = BTreeSet::new();
        if !self.above_trunk.contains(tip) {
            return found;
        }
        let mut stack = vec![tip.clone()];
        while let Some(commit) = stack.pop() {
            if !self.above_trunk.contains(&commit) || !found.insert(commit.clone()) {
                continue;
            }
            if let Some(parents) = self.parents.get(&commit) {
                stack.extend(parents.iter().cloned());
            }
        }
        found
    }
}

/// What the loaded entries say about one local bookmark.
struct LocalBookmark {
    /// Every commit the bookmark points at.
    targets: BTreeSet<CommitId>,
    /// Whether jj reports the bookmark as conflicted.
    conflicted: bool,
    /// Whether the bookmark is locked.
    locked: bool,
}

impl LocalBookmark {
    /// The single commit the bookmark points at, when it is not conflicted.
    fn tip(&self) -> Option<&CommitId> {
        match self.conflicted {
            true => None,
            false => self.targets.iter().next(),
        }
    }
}

/// Gathers the local bookmarks the loaded entries mention.
fn collect_local_bookmarks(
    entries: &[JjLogEntry],
    locks: &BTreeSet<Bookmark>,
) -> BTreeMap<Bookmark, LocalBookmark> {
    let mut bookmarks: BTreeMap<Bookmark, LocalBookmark> = BTreeMap::new();
    for entry in entries {
        for bookmark in &entry.local_bookmarks {
            let slot = bookmarks.entry(bookmark.name.clone()).or_insert_with(|| LocalBookmark {
                targets: BTreeSet::new(),
                conflicted: false,
                locked: locks.contains(&bookmark.name),
            });
            // A conflicted bookmark is one with more than one target, or with a `None` side for a
            // deleted target. jj prints it on every commit that holds an add side.
            if bookmark.target.len() != 1 {
                slot.conflicted = true;
            }
            for target in &bookmark.target {
                match target {
                    Some(commit_id) => {
                        slot.targets.insert(commit_id.clone());
                    }
                    None => slot.conflicted = true,
                }
            }
        }
    }
    bookmarks
}

/// The result of weighing one bookmark.
enum Verdict {
    /// Cleanable, justified by these pull requests.
    Candidate(Vec<PrNum>),
    /// Worth telling the user about, but not cleanable.
    Refused(String),
    /// Nothing points at a cleanable PR; not interesting.
    NotApplicable,
}

/// Builds the plan. Pure: same input, same plan.
pub fn build_plan(input: &InputData, options: &PlanOptions) -> Plan {
    let ancestry = Ancestry::new(&input.jj_entries);
    let entries_by_commit: BTreeMap<&CommitId, &JjLogEntry> =
        input.jj_entries.iter().map(|entry| (&entry.commit.commit_id, entry)).collect();
    let prs_by_branch = group_prs_by_branch(&input.prs);
    let prs_by_number: BTreeMap<PrNum, &GhPr> =
        input.prs.iter().map(|pr| (pr.number, pr)).collect();
    let bookmarks = collect_local_bookmarks(&input.jj_entries, &input.locks);

    let mut warnings = input.load_warnings.clone();
    let mut plan = Plan { actions: Vec::new(), skipped: Vec::new(), warnings: Vec::new() };

    // Pass 1: decide every candidate before computing any abandon set, because the abandon set of
    // one bookmark depends on which other bookmarks are being kept.
    let mut candidates: BTreeMap<Bookmark, Vec<PrNum>> = BTreeMap::new();
    for (name, bookmark) in &bookmarks {
        let tip_entry = bookmark.tip().and_then(|tip| entries_by_commit.get(tip).copied());
        let trailer_prs: Vec<PrNum> = tip_entry
            .map(|entry| crate::jj::parse_pr_trailer(&entry.commit.description))
            .into_iter()
            .flatten()
            .collect();
        let associated =
            associated_prs(name, bookmark.tip(), &prs_by_branch, &trailer_prs, &prs_by_number);
        if associated.is_empty() {
            if let Some(tip) = bookmark.tip() {
                if let Some(pr) = prs_by_branch.get(name).and_then(|prs| {
                    prs.iter().find(|pr| {
                        pr.state != PrState::Open
                            && pr.head_ref_oid.as_ref().is_some_and(|head| head != tip)
                    })
                }) {
                    plan.skipped.push(Skipped {
                        subject: format!("bookmark \"{name}\""),
                        reason: format!("local work differs from the head of {}", pr.number),
                    });
                    continue;
                }
            }
        }
        match weigh(name, bookmark, tip_entry, &associated, input, options, &mut warnings) {
            Verdict::Candidate(prs) => {
                candidates.insert(name.clone(), prs);
            }
            Verdict::Refused(reason) => {
                plan.skipped.push(Skipped { subject: format!("bookmark \"{name}\""), reason })
            }
            Verdict::NotApplicable => {}
        }
    }

    // Every local bookmark that is not a candidate is kept, so its ancestry is protected from
    // being abandoned. Conflicted bookmarks protect every target they hold.
    let mut kept_bookmark_commits = BTreeSet::new();
    for (name, bookmark) in &bookmarks {
        if candidates.contains_key(name) {
            continue;
        }
        kept_bookmark_commits.extend(bookmark.targets.iter().cloned());
    }
    let mut kept_bookmark_ancestors = BTreeSet::new();
    for commit_id in &kept_bookmark_commits {
        kept_bookmark_ancestors.extend(ancestry.above_trunk_ancestors(commit_id));
    }

    // Workspaces this run will not clean are refused here, before any abandon set is computed,
    // because each one still has a commit checked out: abandoning that commit would leave the
    // workspace stale, which is not "leaving it alone".
    let mut refused_workspaces = Vec::new();
    if options.workspaces {
        for (index, workspace) in input.workspaces.iter().enumerate() {
            if let Some(reason) = workspace_refusal(workspace, input, &bookmarks) {
                refused_workspaces.push((index, reason));
            }
        }
    }
    let mut kept_workspace_commits = BTreeSet::new();
    for (index, workspace) in input.workspaces.iter().enumerate() {
        // With `--no-workspaces` every working copy is left alone, so every one is protected.
        let still_holds_its_commit =
            !options.workspaces || refused_workspaces.iter().any(|(refused, _)| *refused == index);
        if still_holds_its_commit {
            kept_workspace_commits.insert(workspace.target.clone());
            if let Some(head) = &workspace.head {
                kept_workspace_commits.insert(head.commit_id.clone());
            }
        }
    }
    let mut kept_workspace_ancestors = BTreeSet::new();
    for commit_id in &kept_workspace_commits {
        kept_workspace_ancestors.extend(ancestry.above_trunk_ancestors(commit_id));
    }

    // Two views of the same thing: what the plan *would* touch, used to explain a refusal, and
    // what it *will* touch, which excludes everything a live working copy still holds.
    let mut abandon_sets: BTreeMap<Bookmark, BTreeSet<CommitId>> = BTreeMap::new();
    let mut would_touch: BTreeMap<Bookmark, BTreeSet<CommitId>> = BTreeMap::new();
    for (name, bookmark) in &bookmarks {
        let Some(prs) = candidates.get(name) else {
            continue;
        };
        let Some(tip) = bookmark.tip() else {
            continue;
        };
        let ancestors = ancestry.above_trunk_ancestors(tip);
        let untrimmed: BTreeSet<CommitId> =
            ancestors.difference(&kept_bookmark_ancestors).cloned().collect();
        let trimmed: BTreeSet<CommitId> =
            untrimmed.difference(&kept_workspace_ancestors).cloned().collect();
        would_touch.insert(name.clone(), untrimmed.clone());
        let revisions = trimmed.len();
        abandon_sets.insert(name.clone(), trimmed);
        let revset = trimming_revset(tip, &[&kept_bookmark_commits, &kept_workspace_commits]);
        if revisions == 0 {
            if ancestors.is_empty() {
                // Every revision is already in trunk; only the bookmark pointer is left.
                plan.actions
                    .push(Action::DeleteBookmark { bookmark: name.clone(), prs: prs.clone() });
            } else {
                let reason = if untrimmed.is_empty() {
                    "held by another bookmark"
                } else {
                    "checked out in a workspace"
                };
                plan.skipped.push(Skipped {
                    subject: format!("bookmark \"{name}\""),
                    reason: reason.to_owned(),
                });
            }
        } else {
            plan.actions.push(Action::AbandonBookmark {
                bookmark: name.clone(),
                revset,
                revisions,
                prs: prs.clone(),
            });
        }
    }

    // A refused workspace is only interesting when it is holding something a candidate wanted.
    for (index, reason) in &refused_workspaces {
        let workspace = &input.workspaces[*index];
        let affected = would_touch.values().any(|set| set.contains(&workspace.target))
            || workspace
                .head
                .as_ref()
                .is_some_and(|head| would_touch.values().any(|set| set.contains(&head.commit_id)));
        if affected {
            plan.skipped.push(Skipped {
                subject: format!("workspace \"{}\"", workspace.name),
                reason: reason.clone(),
            });
        }
    }

    // Deepest first. Abandoning a descendant's revset already includes everything its ancestors
    // hold, so the whole stack goes in one operation and jj deletes every bookmark pointing into
    // it. The reverse order is wrong: abandoning a parent first makes jj rebase its children onto
    // trunk, which rewrites their commit ids and leaves the child's own revset matching nothing.
    let bookmark_actions: Vec<Action> = {
        let mut sorted: Vec<Action> = std::mem::take(&mut plan.actions);
        sorted.sort_by_key(|action| match action {
            Action::AbandonBookmark { bookmark, revisions, .. } => {
                (Reverse(*revisions), bookmark.as_str().to_owned())
            }
            // A bare pointer removal needs no ancestry ordering; jj tolerates it running late,
            // because an abandon that already removed the bookmark makes it a no-op.
            Action::DeleteBookmark { bookmark, .. } => (Reverse(0), bookmark.as_str().to_owned()),
            Action::ForgetWorkspace { name } | Action::RemoveWorkspace { name, .. } => {
                (Reverse(0), name.clone())
            }
        });
        sorted
    };

    let mut workspace_actions = Vec::new();
    if options.workspaces {
        workspace_actions = plan_workspaces(input, options, &bookmarks, &abandon_sets);
    }

    // Workspaces first: forgetting a working copy that sits on doomed revisions must happen while
    // those revisions still exist.
    plan.actions = workspace_actions;
    plan.actions.extend(bookmark_actions);
    plan.warnings = warnings;

    if input.default_bookmark.is_none() {
        plan.warnings.push(
            "Could not determine the default branch. Cleanup candidates were refused.".to_owned(),
        );
    }
    plan
}

/// Every PR whose head branch is `branch`.
fn group_prs_by_branch(prs: &[GhPr]) -> BTreeMap<&Bookmark, Vec<&GhPr>> {
    let mut by_branch: BTreeMap<&Bookmark, Vec<&GhPr>> = BTreeMap::new();
    for pr in prs {
        by_branch.entry(&pr.head_ref_name).or_default().push(pr);
    }
    by_branch
}

/// The pull requests associated with a bookmark: those opened from it, plus those named by the
/// `PR: #N` trailer on its tip commit.
fn associated_prs<'a>(
    name: &Bookmark,
    tip: Option<&CommitId>,
    prs_by_branch: &BTreeMap<&'a Bookmark, Vec<&'a GhPr>>,
    trailer_prs: &[PrNum],
    prs_by_number: &BTreeMap<PrNum, &'a GhPr>,
) -> Vec<&'a GhPr> {
    let mut found: BTreeMap<PrNum, &'a GhPr> = BTreeMap::new();
    if let Some(from_branch) = prs_by_branch.get(name) {
        for pr in from_branch {
            // An old merged PR can share a name with new local work. Open PRs protect the name,
            // but a finished PR justifies cleanup by name only when it held this exact tip.
            if pr.state == PrState::Open || tip.is_none() || pr.head_ref_oid.as_ref() == tip {
                found.insert(pr.number, pr);
            }
        }
    }
    for number in trailer_prs {
        if let Some(pr) = prs_by_number.get(number) {
            found.insert(pr.number, pr);
        }
    }
    found.into_values().collect()
}

/// Weighs one bookmark against every rule.
fn weigh(
    name: &Bookmark,
    bookmark: &LocalBookmark,
    tip_entry: Option<&JjLogEntry>,
    associated: &[&GhPr],
    input: &InputData,
    options: &PlanOptions,
    warnings: &mut Vec<String>,
) -> Verdict {
    // A conflicted bookmark has no single target to reason about, and jj would refuse to delete
    // it by name anyway.
    if bookmark.conflicted {
        return if associated.is_empty() {
            Verdict::NotApplicable
        } else {
            Verdict::Refused("conflicted".to_owned())
        };
    }
    if associated.is_empty() {
        return Verdict::NotApplicable;
    }
    if bookmark.locked {
        return Verdict::Refused("locked".to_owned());
    }
    if let Some(open) = associated.iter().find(|pr| pr.state == PrState::Open) {
        return Verdict::Refused(format!("{} is still open", open.number));
    }
    if input.default_bookmark.is_none() {
        return Verdict::Refused("the default branch is unknown".to_owned());
    }
    if input.default_bookmark.as_ref() == Some(name) {
        return Verdict::Refused("is the default branch".to_owned());
    }
    if tip_entry.is_some_and(|entry| entry.is_trunk_tip) {
        return Verdict::Refused("points at trunk".to_owned());
    }

    let cleanable: Vec<PrNum> = associated
        .iter()
        .filter(|pr| options.state.accepts(pr.state))
        .map(|pr| pr.number)
        .collect();
    if cleanable.is_empty() {
        let sample = associated
            .iter()
            .map(|pr| format!("{} is {}", pr.number, pr.state_label()))
            .collect::<Vec<_>>()
            .join(", ");
        let mut reason = format!("no {} PR ({sample})", options.state.describe());
        if options.state == StateFilter::Merged
            && associated.iter().any(|pr| pr.state == PrState::Closed)
        {
            reason.push_str(". Use --state closed to include closed PRs");
        }
        return Verdict::Refused(reason);
    }

    // A trailer that names a different branch's PR is how a stale trailer looks. Cleaning is
    // still justified by the PR it names, but the user should see the mismatch.
    for pr in associated {
        if &pr.head_ref_name != name {
            warnings.push(format!(
                "bookmark \"{name}\" carries a trailer for {}, which was opened from \"{}\"",
                pr.number, pr.head_ref_name
            ));
        }
    }
    Verdict::Candidate(cleanable)
}

/// The revset that abandons a bookmark's own revisions without touching anything that is protected.
///
/// `trunk()..B` alone would abandon a shared stack base, so the ancestry of every kept bookmark —
/// and of every working copy this run leaves alone — is subtracted.
fn trimming_revset(tip: &CommitId, protected_groups: &[&BTreeSet<CommitId>]) -> Revset {
    let mut expression = format!("::commit_id({tip}) ~ ::trunk()");
    for group in protected_groups {
        if group.is_empty() {
            continue;
        }
        let terms: Vec<String> =
            group.iter().map(|commit_id| format!("::commit_id({commit_id})")).collect();
        let _ = write!(expression, " ~ ({})", terms.join(" | "));
    }
    Revset::new(expression)
}

/// Why a workspace must not be cleaned, if it must not be.
///
/// Deliberately independent of the abandon sets: whether a workspace holds a candidate's
/// revisions is a separate question from whether it may be touched at all.
fn workspace_refusal(
    workspace: &WorkspaceInfo,
    input: &InputData,
    bookmarks: &BTreeMap<Bookmark, LocalBookmark>,
) -> Option<String> {
    if workspace.name == "default" {
        return Some("is the primary workspace".to_owned());
    }
    if workspace.root.is_none() {
        return Some("jj did not report its directory".to_owned());
    }
    if workspace.root == input.primary_workspace_root {
        return Some("is the primary workspace".to_owned());
    }
    if workspace.current {
        return Some("is the workspace you are in".to_owned());
    }
    if let Some(locked) = locked_bookmark_at(workspace, bookmarks) {
        return Some(format!("bookmark \"{locked}\" is locked"));
    }
    match &workspace.head {
        None => Some("its working copy could not be read".to_owned()),
        Some(head) if !head.empty => Some("has uncommitted changes".to_owned()),
        Some(head) if head.commit_id != workspace.target => {
            Some("its working copy changed while gathering state".to_owned())
        }
        Some(_) => None,
    }
}

/// Plans workspace cleanup.
///
/// Only workspaces that hold a candidate's revisions and were not refused are cleaned; refusals
/// were already reported while the abandon sets were computed.
fn plan_workspaces(
    input: &InputData,
    options: &PlanOptions,
    bookmarks: &BTreeMap<Bookmark, LocalBookmark>,
    abandon_sets: &BTreeMap<Bookmark, BTreeSet<CommitId>>,
) -> Vec<Action> {
    let mut actions = Vec::new();
    for workspace in &input.workspaces {
        // A workspace no candidate touches is simply not part of this cleanup: no skip line, so
        // the list stays a list of refusals rather than of every workspace in the repository.
        if !abandon_sets.values().any(|set| set.contains(&workspace.target)) {
            continue;
        }
        if workspace_refusal(workspace, input, bookmarks).is_some() {
            continue;
        }
        let Some(root) = workspace.root.clone() else {
            continue;
        };
        actions.push(match options.remove_workspaces {
            true => Action::RemoveWorkspace { name: workspace.name.clone(), root },
            false => Action::ForgetWorkspace { name: workspace.name.clone() },
        });
    }
    actions.sort_by_key(|action| match action {
        Action::ForgetWorkspace { name } | Action::RemoveWorkspace { name, .. } => name.clone(),
        Action::AbandonBookmark { bookmark, .. } | Action::DeleteBookmark { bookmark, .. } => {
            bookmark.as_str().to_owned()
        }
    });
    actions
}

/// The locked bookmark sitting at the workspace's target, if any.
fn locked_bookmark_at<'a>(
    workspace: &WorkspaceInfo,
    bookmarks: &'a BTreeMap<Bookmark, LocalBookmark>,
) -> Option<&'a Bookmark> {
    bookmarks
        .iter()
        .find(|(_, bookmark)| bookmark.locked && bookmark.targets.contains(&workspace.target))
        .map(|(name, _)| name)
}

/// Renders the plan for a human.
pub fn render(plan: &Plan) -> String {
    let mut out = String::new();
    if plan.actions.is_empty() {
        let _ = writeln!(out, "Nothing to clean.");
    } else {
        for action in &plan.actions {
            let _ = writeln!(out, "  {}", render_action(action));
        }
    }
    if !plan.skipped.is_empty() {
        let _ = writeln!(out, "\nskipped:");
        for skip in &plan.skipped {
            let _ = writeln!(out, "  {}: {}", skip.subject, skip.reason);
        }
    }
    if !plan.warnings.is_empty() {
        let _ = writeln!(out, "\nwarnings:");
        for warning in &plan.warnings {
            let _ = writeln!(out, "  {warning}");
        }
    }
    out
}

/// One action as a sentence.
pub(crate) fn render_action(action: &Action) -> String {
    match action {
        Action::ForgetWorkspace { name } => format!("forget workspace \"{name}\""),
        Action::RemoveWorkspace { name, root } => {
            format!("remove workspace \"{name}\" and delete {}", root.display())
        }
        Action::AbandonBookmark { bookmark, revisions, .. } => {
            let noun = if *revisions == 1 { "revision" } else { "revisions" };
            format!("abandon {revisions} {noun} for bookmark \"{bookmark}\"")
        }
        Action::DeleteBookmark { bookmark, .. } => {
            format!("delete bookmark \"{bookmark}\" (already in trunk)")
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::jj::JjCommit;

    fn entry(id: &str, parents: &[&str], above_trunk: bool) -> JjLogEntry {
        JjLogEntry {
            commit: JjCommit {
                commit_id: CommitId::new(id),
                parents: parents.iter().map(|parent| CommitId::new(*parent)).collect(),
                description: String::new(),
            },
            local_bookmarks: Vec::new(),
            is_trunk_tip: false,
            empty: true,
            above_trunk,
        }
    }

    fn trunk_tip(id: &str, parents: &[&str]) -> JjLogEntry {
        let mut entry = entry(id, parents, false);
        entry.is_trunk_tip = true;
        entry
    }

    #[test]
    fn ancestors_stop_at_trunk_boundary() {
        // trunk <- base <- tip, where an extra commit below trunk is loaded because a stale
        // bookmark points at it.
        let entries = vec![
            trunk_tip("trunk", &["old"]),
            entry("old", &[], false),
            entry("base", &["trunk"], true),
            entry("tip", &["base"], true),
        ];
        let ancestry = Ancestry::new(&entries);
        let found = ancestry.above_trunk_ancestors(&CommitId::new("tip"));
        assert_eq!(
            found,
            BTreeSet::from([CommitId::new("base"), CommitId::new("tip")]),
            "must not collect trunk or anything below it"
        );
        assert!(ancestry.above_trunk_ancestors(&CommitId::new("trunk")).is_empty());
        assert!(
            ancestry.above_trunk_ancestors(&CommitId::new("old")).is_empty(),
            "a commit below trunk has no above-trunk ancestry"
        );
    }

    #[test]
    fn ancestors_cover_a_branching_history() {
        let entries = vec![
            entry("base", &["trunk"], true),
            entry("left", &["base"], true),
            entry("right", &["base"], true),
        ];
        let ancestry = Ancestry::new(&entries);
        assert_eq!(
            ancestry.above_trunk_ancestors(&CommitId::new("right")),
            BTreeSet::from([CommitId::new("base"), CommitId::new("right")])
        );
    }

    #[test]
    fn conflicted_bookmarks_are_detected_including_none_sides() {
        let mut one = entry("a", &[], true);
        one.local_bookmarks = vec![crate::jj::JjBookmark {
            name: Bookmark::new("x"),
            target: vec![Some(CommitId::new("a")), None],
        }];
        let bookmarks = collect_local_bookmarks(&[one], &BTreeSet::new());
        assert!(bookmarks[&Bookmark::new("x")].conflicted);
        assert_eq!(bookmarks[&Bookmark::new("x")].tip(), None);
    }

    #[test]
    fn state_filters_cover_github_states() {
        assert!(StateFilter::Merged.accepts(PrState::Merged));
        assert!(!StateFilter::Merged.accepts(PrState::Closed));
        assert!(!StateFilter::Merged.accepts(PrState::Open));
        assert!(StateFilter::Closed.accepts(PrState::Closed));
        assert!(StateFilter::Closed.accepts(PrState::Merged));
        assert!(!StateFilter::Closed.accepts(PrState::Open));
    }

    #[test]
    fn trimming_revset_subtracts_every_protected_group() {
        let kept = BTreeSet::from([CommitId::new("base")]);
        let held_by_a_workspace = BTreeSet::from([CommitId::new("ws")]);
        let revset = trimming_revset(&CommitId::new("tip"), &[&kept, &held_by_a_workspace]);
        assert_eq!(
            revset.as_str(),
            "::commit_id(tip) ~ ::trunk() ~ (::commit_id(base)) ~ (::commit_id(ws))"
        );
    }

    #[test]
    fn trimming_revset_skips_empty_groups() {
        let kept = BTreeSet::from([CommitId::new("base")]);
        let empty = BTreeSet::new();
        let revset = trimming_revset(&CommitId::new("tip"), &[&kept, &empty]);
        assert_eq!(revset.as_str(), "::commit_id(tip) ~ ::trunk() ~ (::commit_id(base))");
    }
}
