//! Planning tests.
//!
//! Planning is pure, so every safety rule is pinned here from fixtures rather than from a live
//! repository. The rendered plan is the observable: a regression in what would be cleaned shows
//! up as a snapshot diff.

use jj_cleanup::gh::{GhPr, PrNum, PrState};
use jj_cleanup::jj::{JjBookmark, JjCommit, JjLogEntry, WorkspaceHead, WorkspaceInfo};
use jj_cleanup::plan::{InputData, PlanOptions, StateFilter, build_plan, render};
use jj_cleanup::types::{Bookmark, CommitId, Owner, Repo};

const TRUNK: &str = "trunk";

/// A commit above trunk.
fn commit(id: &str, parents: &[&str]) -> JjLogEntry {
    JjLogEntry {
        commit: JjCommit {
            commit_id: CommitId::new(id),
            parents: parents.iter().map(|parent| CommitId::new(*parent)).collect(),
            description: format!("work on {id}\n"),
        },
        local_bookmarks: Vec::new(),
        is_trunk_tip: false,
        empty: true,
        above_trunk: true,
    }
}

/// The trunk commit itself, which is never above trunk.
fn trunk() -> JjLogEntry {
    let mut entry = commit(TRUNK, &[]);
    entry.above_trunk = false;
    entry.is_trunk_tip = true;
    entry.commit.description = "trunk\n".to_owned();
    entry
}

/// A commit that is an ancestor of trunk, i.e. already landed.
fn landed(id: &str, child_of: &str) -> JjLogEntry {
    let mut entry = commit(id, &[]);
    entry.above_trunk = false;
    entry.commit.description = format!("{child_of} landed work\n");
    entry
}

/// A bookmark pointing at one commit, or at several when it is conflicted.
fn bookmark(name: &str, targets: &[Option<&str>]) -> JjBookmark {
    JjBookmark {
        name: Bookmark::new(name),
        target: targets.iter().map(|target| target.map(CommitId::new)).collect(),
    }
}

/// A pull request whose head branch is `head`.
fn pr(number: u64, head: &str, state: PrState) -> GhPr {
    GhPr {
        number: PrNum::new(number).unwrap(),
        head_ref_name: Bookmark::new(head),
        head_ref_oid: None,
        base_ref_name: Bookmark::new("main"),
        state,
        is_draft: false,
        url: format!("https://github.com/acme/widgets/pull/{number}"),
        title: format!("work on {head}"),
        merge_commit_oid: None,
        head_repo_owner: None,
        repo_owner: Owner::new("acme"),
        repo: Repo::new("widgets"),
    }
}

/// A workspace whose working copy is `target`.
fn workspace(name: &str, root: &str, target: &str, empty: bool) -> WorkspaceInfo {
    WorkspaceInfo {
        name: name.to_owned(),
        root: Some(root.into()),
        target: CommitId::new(target),
        current: false,
        head: Some(WorkspaceHead { commit_id: CommitId::new(target), empty, above_trunk: true }),
    }
}

/// Builds a fixture repository.
struct Fixture {
    entries: Vec<JjLogEntry>,
    prs: Vec<GhPr>,
    workspaces: Vec<WorkspaceInfo>,
    default_bookmark: Option<Bookmark>,
    primary_workspace_root: Option<String>,
    locks: Vec<&'static str>,
}

impl Fixture {
    /// A repository with only trunk.
    fn new() -> Self {
        Self {
            entries: vec![trunk()],
            prs: Vec::new(),
            workspaces: Vec::new(),
            default_bookmark: Some(Bookmark::new("main")),
            primary_workspace_root: Some("/repo".to_owned()),
            locks: Vec::new(),
        }
    }

    /// Adds a commit and attaches bookmarks to it.
    fn with(mut self, mut entry: JjLogEntry, bookmarks: &[JjBookmark]) -> Self {
        entry.local_bookmarks = bookmarks.to_vec();
        self.entries.push(entry);
        self
    }

    /// Adds pull requests.
    fn prs(mut self, prs: &[GhPr]) -> Self {
        self.prs.extend(prs.iter().cloned().map(|mut pr| {
            if pr.head_ref_oid.is_none() {
                pr.head_ref_oid = self
                    .entries
                    .iter()
                    .find(|entry| {
                        entry
                            .local_bookmarks
                            .iter()
                            .any(|bookmark| bookmark.name == pr.head_ref_name)
                    })
                    .map(|entry| entry.commit.commit_id.clone());
            }
            pr
        }));
        self
    }

    /// Adds a workspace.
    fn workspace(mut self, workspace: WorkspaceInfo) -> Self {
        self.workspaces.push(workspace);
        self
    }

    /// Locks bookmarks.
    fn locked(mut self, names: &[&'static str]) -> Self {
        self.locks.extend_from_slice(names);
        self
    }

    /// The fixture.
    fn input(&self) -> InputData {
        InputData {
            jj_entries: self.entries.clone(),
            workspaces: self.workspaces.clone(),
            prs: self.prs.clone(),
            default_bookmark: self.default_bookmark.clone(),
            primary_workspace_root: self.primary_workspace_root.clone().map(Into::into),
            locks: self.locks.iter().map(|name| Bookmark::new(*name)).collect(),
            load_warnings: Vec::new(),
        }
    }
}

/// The default options.
fn options() -> PlanOptions {
    PlanOptions { state: StateFilter::Merged, workspaces: true, remove_workspaces: false }
}

#[test]
fn merged_bookmark_abandons_its_own_revisions() {
    let repo = Fixture::new()
        .with(commit("base", &[TRUNK]), &[bookmark("shared", &[Some("base")])])
        .with(commit("tip", &["base"]), &[bookmark("feat-x", &[Some("tip")])])
        .prs(&[pr(1, "feat-x", PrState::Merged)]);
    let plan = build_plan(&repo.input(), &options());
    insta::assert_snapshot!(render(&plan));
}

#[test]
fn a_moved_bookmark_is_reported_and_kept() {
    let mut merged = pr(65, "feat-x", PrState::Merged);
    merged.head_ref_oid = Some(CommitId::new("original-pr-head"));
    let repo = Fixture::new()
        .with(commit("local-tip", &[TRUNK]), &[bookmark("feat-x", &[Some("local-tip")])])
        .prs(&[merged]);

    let plan = build_plan(&repo.input(), &options());

    assert!(plan.actions.is_empty(), "local revisions must be preserved: {plan:?}");
    assert!(
        render(&plan).contains("bookmark \"feat-x\": local work differs from the head of #65"),
        "the mismatch should be visible: {}",
        render(&plan)
    );
}

#[test]
fn a_bookmark_shared_with_a_kept_bookmark_is_kept_for_retry() {
    // `stack-base` merged, but its revision is still held by the open child. Nothing may be
    // abandoned, and the child's base must survive.
    let repo = Fixture::new()
        .with(commit("base", &[TRUNK]), &[bookmark("stack-base", &[Some("base")])])
        .with(commit("child", &["base"]), &[bookmark("open-child", &[Some("child")])])
        .prs(&[pr(4, "stack-base", PrState::Merged), pr(2, "open-child", PrState::Open)]);
    let plan = build_plan(&repo.input(), &options());
    insta::assert_snapshot!(render(&plan));
}

#[test]
fn a_base_shared_with_a_kept_bookmark_is_preserved() {
    // `shared` has no PR, so it is kept; only `feat-x`'s own revision may be abandoned.
    let repo = Fixture::new()
        .with(commit("base", &[TRUNK]), &[bookmark("shared", &[Some("base")])])
        .with(commit("tip", &["base"]), &[bookmark("feat-x", &[Some("tip")])])
        .prs(&[pr(1, "feat-x", PrState::Merged)]);
    let plan = build_plan(&repo.input(), &options());
    let abandoned = plan.actions.iter().find_map(|action| match action {
        jj_cleanup::plan::Action::AbandonBookmark { bookmark, revset, revisions, .. } => {
            Some((bookmark.clone(), revset.clone(), *revisions))
        }
        _ => None,
    });
    let (bookmark, revset, revisions) = abandoned.expect("feat-x should be abandoned");
    assert_eq!(bookmark, Bookmark::new("feat-x"));
    assert_eq!(revisions, 1, "only the tip revision is unique to feat-x");
    assert_eq!(
        revset.as_str(),
        "::commit_id(tip) ~ ::trunk() ~ (::commit_id(base))",
        "the kept bookmark's revision must be subtracted from the abandon set"
    );
}

#[test]
fn revisions_already_in_trunk_are_only_deleted() {
    let repo = Fixture::new()
        .with(landed("old", TRUNK), &[bookmark("landed", &[Some("old")])])
        .prs(&[pr(7, "landed", PrState::Merged)]);
    let plan = build_plan(&repo.input(), &options());
    insta::assert_snapshot!(render(&plan));
}

#[test]
fn closed_pull_requests_need_the_closed_filter() {
    let repo = Fixture::new()
        .with(commit("tip", &[TRUNK]), &[bookmark("closed-one", &[Some("tip")])])
        .prs(&[pr(3, "closed-one", PrState::Closed)]);
    let plan = build_plan(&repo.input(), &options());
    insta::assert_snapshot!(render(&plan));
}

#[test]
fn closed_pull_requests_are_cleanable_with_the_closed_filter() {
    let repo = Fixture::new()
        .with(commit("tip", &[TRUNK]), &[bookmark("closed-one", &[Some("tip")])])
        .prs(&[pr(3, "closed-one", PrState::Closed)]);
    let plan = build_plan(&repo.input(), &PlanOptions { state: StateFilter::Closed, ..options() });
    insta::assert_snapshot!(render(&plan));
}

#[test]
fn open_and_locked_and_conflicted_bookmarks_are_refused() {
    let repo = Fixture::new()
        .with(commit("open", &[TRUNK]), &[bookmark("open-one", &[Some("open")])])
        .with(commit("locked", &[TRUNK]), &[bookmark("lock-me", &[Some("locked")])])
        .with(
            commit("conflicted", &[TRUNK]),
            &[bookmark("broken", &[Some("conflicted"), Some(TRUNK)])],
        )
        .prs(&[
            pr(2, "open-one", PrState::Open),
            pr(6, "lock-me", PrState::Merged),
            pr(9, "broken", PrState::Merged),
        ])
        .locked(&["lock-me"]);
    let plan = build_plan(&repo.input(), &options());
    insta::assert_snapshot!(render(&plan));
    assert!(plan.actions.is_empty(), "nothing here may be cleaned: {plan:?}");
}

#[test]
fn a_reused_branch_with_an_open_pr_is_refused() {
    let repo = Fixture::new()
        .with(commit("tip", &[TRUNK]), &[bookmark("feat-x", &[Some("tip")])])
        .prs(&[pr(1, "feat-x", PrState::Merged), pr(2, "feat-x", PrState::Open)]);
    let plan = build_plan(&repo.input(), &options());

    assert!(plan.actions.is_empty(), "an open PR must protect a reused branch: {plan:?}");
    assert!(plan.skipped.iter().any(|skip| skip.reason.contains("#2 is still open")));
}

#[test]
fn a_reused_local_branch_with_an_old_merged_pr_is_not_a_candidate() {
    let mut old_pr = pr(1, "feat-x", PrState::Merged);
    old_pr.head_ref_oid = Some(CommitId::new("old-tip"));
    let repo = Fixture::new()
        .with(commit("new-tip", &[TRUNK]), &[bookmark("feat-x", &[Some("new-tip")])])
        .prs(&[old_pr]);
    let plan = build_plan(&repo.input(), &options());

    assert!(plan.actions.is_empty(), "old PR must not justify abandoning new work: {plan:?}");
}

#[test]
fn the_default_branch_is_never_cleaned() {
    let repo = Fixture::new()
        .with(commit("tip", &[TRUNK]), &[bookmark("main", &[Some("tip")])])
        .prs(&[pr(5, "main", PrState::Merged)]);
    let plan = build_plan(&repo.input(), &options());
    insta::assert_snapshot!(render(&plan));
    assert!(plan.actions.is_empty());
}

#[test]
fn a_bookmark_with_no_pull_request_is_ignored_silently() {
    let repo = Fixture::new().with(commit("tip", &[TRUNK]), &[bookmark("work", &[Some("tip")])]);
    let plan = build_plan(&repo.input(), &options());
    assert!(plan.actions.is_empty());
    assert!(plan.skipped.is_empty(), "a bookmark with no PR is not a refusal: {plan:?}");
}

#[test]
fn a_stale_trailer_for_another_branch_is_warned_about() {
    // The trailer says `PR: #1`, but PR #1 was opened from a different branch.
    let mut tip = commit("tip", &[TRUNK]);
    tip.commit.description = "work\n\nPR: #1\n".to_owned();
    let repo = Fixture::new().with(tip, &[bookmark("local-name", &[Some("tip")])]).prs(&[pr(
        1,
        "renamed-branch",
        PrState::Merged,
    )]);
    let plan = build_plan(&repo.input(), &options());
    insta::assert_snapshot!(render(&plan));
}

#[test]
fn workspaces_on_a_merged_stack_are_forgotten() {
    let repo = Fixture::new()
        .with(commit("tip", &[TRUNK]), &[bookmark("feat-x", &[Some("tip")])])
        .workspace(workspace("ws2", "/repo-ws2", "tip", true))
        .prs(&[pr(1, "feat-x", PrState::Merged)]);
    let plan = build_plan(&repo.input(), &options());
    insta::assert_snapshot!(render(&plan));
}

#[test]
fn remove_deletes_the_directory_instead_of_forgetting_it() {
    let repo = Fixture::new()
        .with(commit("tip", &[TRUNK]), &[bookmark("feat-x", &[Some("tip")])])
        .workspace(workspace("ws2", "/repo-ws2", "tip", true))
        .prs(&[pr(1, "feat-x", PrState::Merged)]);
    let plan = build_plan(&repo.input(), &PlanOptions { remove_workspaces: true, ..options() });
    insta::assert_snapshot!(render(&plan));
}

#[test]
fn no_workspaces_leaves_every_workspace_alone() {
    let repo = Fixture::new()
        .with(commit("tip", &[TRUNK]), &[bookmark("feat-x", &[Some("tip")])])
        .workspace(workspace("ws2", "/repo-ws2", "tip", true))
        .prs(&[pr(1, "feat-x", PrState::Merged)]);
    let plan = build_plan(&repo.input(), &PlanOptions { workspaces: false, ..options() });
    assert!(plan.actions.is_empty(), "the workspace still protects the revision: {plan:?}");
    assert!(plan.skipped.iter().any(|skip| skip.subject == "bookmark \"feat-x\""));
}

#[test]
fn unsafe_workspaces_are_refused_and_reported() {
    let repo = Fixture::new()
        .with(commit("tip", &[TRUNK]), &[bookmark("feat-x", &[Some("tip")])])
        // The primary workspace is named `default` even though its root also matches.
        .workspace(workspace("default", "/repo", "tip", true))
        // The workspace the process runs in.
        .workspace(WorkspaceInfo { current: true, ..workspace("here", "/repo-here", "tip", true) })
        // Uncommitted changes.
        .workspace(workspace("dirty", "/repo-dirty", "tip", false))
        // A version of jj that reports no root at all.
        .workspace(WorkspaceInfo { root: None, ..workspace("rootless", "/ignored", "tip", true) })
        // An unreadable working copy.
        .workspace(WorkspaceInfo {
            head: None,
            ..workspace("unreadable", "/repo-unreadable", "tip", true)
        })
        .prs(&[pr(1, "feat-x", PrState::Merged)]);
    let plan = build_plan(&repo.input(), &options());
    insta::assert_snapshot!(render(&plan));
    assert!(plan.actions.is_empty(), "the protected bookmark must remain: {plan:?}");
}

#[test]
fn a_workspace_whose_head_changed_after_listing_is_protected() {
    let mut shifted = workspace("ws2", "/repo-ws2", "tip", true);
    shifted.head.as_mut().unwrap().commit_id = CommitId::new("new-head");
    let repo = Fixture::new()
        .with(commit("tip", &[TRUNK]), &[bookmark("feat-x", &[Some("tip")])])
        .workspace(shifted)
        .prs(&[pr(1, "feat-x", PrState::Merged)]);

    let plan = build_plan(&repo.input(), &options());
    assert!(
        plan.actions
            .iter()
            .all(|action| !matches!(action, jj_cleanup::plan::Action::ForgetWorkspace { .. })),
        "the changed working copy must not be forgotten: {plan:?}"
    );
    assert!(
        plan.skipped.iter().any(|skip| skip.subject.contains("ws2")),
        "the refusal should be visible: {plan:?}"
    );
}

#[test]
fn a_workspace_holding_a_locked_bookmark_is_refused() {
    let repo = Fixture::new()
        .with(commit("tip", &[TRUNK]), &[bookmark("feat-x", &[Some("tip")])])
        .workspace(workspace("ws2", "/repo-ws2", "tip", true))
        .prs(&[pr(1, "feat-x", PrState::Merged)])
        .locked(&["feat-x"]);
    let plan = build_plan(&repo.input(), &options());
    insta::assert_snapshot!(render(&plan));
    assert!(plan.actions.is_empty(), "a locked bookmark protects its workspace: {plan:?}");
}

#[test]
fn unrelated_workspaces_are_not_reported() {
    let repo = Fixture::new()
        .with(commit("tip", &[TRUNK]), &[bookmark("feat-x", &[Some("tip")])])
        // This workspace sits on a commit no candidate touches.
        .workspace(workspace("elsewhere", "/repo-elsewhere", TRUNK, true))
        .prs(&[pr(1, "feat-x", PrState::Merged)]);
    let plan = build_plan(&repo.input(), &options());
    assert!(plan.skipped.is_empty(), "an unaffected workspace is not a refusal: {plan:?}");
}

#[test]
fn a_stack_is_cleaned_deepest_first() {
    let repo = Fixture::new()
        .with(commit("base", &[TRUNK]), &[bookmark("parent", &[Some("base")])])
        .with(commit("child", &["base"]), &[bookmark("child-bm", &[Some("child")])])
        .prs(&[pr(1, "parent", PrState::Merged), pr(2, "child-bm", PrState::Merged)]);
    let plan = build_plan(&repo.input(), &options());
    let order: Vec<String> = plan
        .actions
        .iter()
        .map(|action| match action {
            jj_cleanup::plan::Action::AbandonBookmark { bookmark, .. } => bookmark.to_string(),
            other => panic!("expected only abandon actions, got {other:?}"),
        })
        .collect();
    assert_eq!(
        order,
        vec!["child-bm".to_owned(), "parent".to_owned()],
        "the descendant must go first: its revset already covers the parent, whereas abandoning \
         the parent first would rebase the child and leave it behind"
    );
}

#[test]
fn actions_put_workspaces_before_revision_cleanup() {
    let repo = Fixture::new()
        .with(commit("tip", &[TRUNK]), &[bookmark("feat-x", &[Some("tip")])])
        .workspace(workspace("ws2", "/repo-ws2", "tip", true))
        .prs(&[pr(1, "feat-x", PrState::Merged)]);
    let plan = build_plan(&repo.input(), &options());
    assert!(
        matches!(plan.actions.first(), Some(jj_cleanup::plan::Action::ForgetWorkspace { .. })),
        "a working copy must be released before its revisions are abandoned: {plan:?}"
    );
}

#[test]
fn an_absent_default_branch_is_warned_about() {
    let mut input = Fixture::new()
        .with(commit("tip", &[TRUNK]), &[bookmark("feat-x", &[Some("tip")])])
        .prs(&[pr(1, "feat-x", PrState::Merged)])
        .input();
    input.default_bookmark = None;
    let plan = build_plan(&input, &options());
    insta::assert_snapshot!(render(&plan));
    assert!(plan.actions.is_empty(), "unknown default branch must prevent cleanup: {plan:?}");
}
