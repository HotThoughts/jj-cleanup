# jj-cleanup

Remove the local Jujutsu revisions, bookmarks, and workspaces that are left behind after a
GitHub pull request is merged or closed.

Inspired by [gh-poi](https://github.com/seachicken/gh-poi).

```console
$ jj cleanup
```

`jj cleanup` is a plan by default: it gathers the state of your repository and your pull requests,
prints exactly what it would remove, and asks before changing anything.

"Clean" here means bookmarks, revisions, and workspaces — the leftovers of finished pull requests.
It is not `git clean`: no file is ever removed from a working copy.

## Install

Prebuilt binaries and a shell installer are published for each release:

```console
$ curl -LsSf https://github.com/HotThoughts/jj-cleanup/releases/latest/download/jj-cleanup-installer.sh | sh
$ jj-cleanup util install-aliases      # adds `jj cleanup` and `jj cl`
```

Or with Cargo:

```console
$ cargo install --git https://github.com/HotThoughts/jj-cleanup
$ jj-cleanup util install-aliases
```

To install the current checkout locally while developing, run this from the repository root:

```console
$ cargo install --path .
$ jj-cleanup --help
$ jj-cleanup --dry-run --no-fetch
```

For quick iteration without installing, use `cargo run -- --help`. After changing the code, rerun
`cargo install --path . --force` to update the installed binary used by `jj cleanup` and `jj cl`.

Or build a release binary from a checkout:

```console
$ cargo build --release
$ install -m755 target/release/jj-cleanup ~/.local/bin/
$ jj-cleanup util install-aliases --repo   # or --user, the default
```

`jj-cleanup` needs:

- **jj 0.44 or newer.** Earlier versions do not report a workspace's directory, which is what
  makes workspace cleanup safe. It refuses to run against anything older.
- **`gh`**, authenticated. `gh` owns all GitHub access; `jj-cleanup` stores no credentials.
- **Rust 1.85 or newer**, only to build it.

### Aliases

`jj-cleanup util install-aliases` writes two jj aliases — `jj cleanup` and `jj cl` — that both run
`jj-cleanup` from your `PATH` through `jj util exec`. They go to your user config by default; pass
`--repo` to scope them to the current repository instead.

To add them by hand, run the same two commands:

```console
$ jj config set --user aliases.cleanup '["util", "exec", "--", "jj-cleanup"]'
$ jj config set --user aliases.cl '["util", "exec", "--", "jj-cleanup"]'
```

Or put them in your jj config file directly (`jj config path --user` shows where it is):

```toml
[aliases]
cleanup = ["util", "exec", "--", "jj-cleanup"]
cl = ["util", "exec", "--", "jj-cleanup"]
```

## Usage

```
jj cleanup [OPTIONS]                 # default = plan, prompt, apply
jj cleanup list                      # the plan, never applied
jj cleanup lock <bookmark>...        # never treat these bookmarks as candidates
jj cleanup unlock <bookmark>...
jj cleanup util dump                 # print the gathered jj + gh state as JSON
jj cleanup util install-aliases      # write the `clean` and `cl` jj aliases

OPTIONS
  --state <merged|closed>   merged = only MERGED PRs (default); closed = MERGED or CLOSED
  --scan <quick|deep>       quick = the repository `gh` resolves (default); deep = every remote
  --dry-run                 print the plan, apply nothing
  --no-fetch                skip the automatic `jj git fetch` before planning
  --no-workspaces           leave workspaces alone
  --remove                  workspaces: delete the directory instead of only forgetting it
  -y, --yes                 skip the confirmation prompt
```

Exit codes: `0` success (including a plan with nothing to do), `1` any error or an abort.

## Safety model

`jj-cleanup` never discards work it cannot account for.

A bookmark is a cleanup candidate only when all of the following hold:

- it is associated with at least one pull request — its tip carries a `PR: #N` trailer, or a
  pull request's head branch has that name and its head commit matches the bookmark's tip;
- no associated pull request is open;
- at least one associated pull request is in the requested state set;
- it is not locked, and is neither the default branch nor a bookmark sitting on trunk;
- it is not conflicted.

If GitHub does not report the PR repository's default branch, cleanup candidates are refused.
Branch lookups check open, merged, and closed PRs separately so a reused branch is protected when
any of its PRs is still open.

Even then, revisions are abandoned with a trimming revset rather than a plain
`trunk()..bookmark` range:

```
::commit_id(tip) ~ ::trunk() ~ (protected…)
```

Every commit that is still held somewhere else is subtracted first: the ancestry of each kept
bookmark, and the ancestry of each working copy this run leaves alone. A bookmark whose revisions
are still held elsewhere is kept for a later cleanup run. That protects a shared stack base and
lets cleanup retry after the other bookmark or workspace moves away.

Workspaces participate by default. A workspace is cleaned only when it is clean, is not the
primary workspace, is not the one you are in, its bookmark is not locked, and it holds a candidate's
revisions. Otherwise it is listed as skipped and **its checked-out commits are protected** —
`jj-cleanup` will not abandon a revision out from under a live working copy, because that would leave
the workspace stale.

The distinctions that matter:

| Situation | What happens |
|---|---|
| Revisions still held by a kept bookmark | bookmark and revisions kept for a later run |
| Revisions checked out in a workspace this run leaves alone | bookmark and revisions kept for a later run |
| Revisions already in trunk | bookmark deleted |
| Workspace with uncommitted changes | workspace untouched, reported as skipped |
| Bookmark locked, conflicted, open PR, or the default branch | refused, reported as skipped |

`jj log` may still show revisions after cleanup when they are part of trunk, held by another
bookmark, or ancestors of a workspace's current revision. Protected bookmarks are kept so a later
cleanup can retry. A closed but unmerged PR is skipped by default; use `--state closed` if you want
to include it. If a local bookmark's tip differs from the PR's recorded head, cleanup keeps it and
reports the mismatch so local work is not mistaken for the finished PR.

`--remove` deletes a cleaned workspace's directory after forgetting it, so **ignored files in that
directory are lost**; uncommitted tracked changes are snapshotted into the repository first, which
is why `--remove` is opt-in and never the default.

Two footnotes on `--dry-run` and `list`, both of which change nothing:

- Reading a workspace's state snapshots that workspace's working copy, because that is the only way
  to see whether it has uncommitted changes. Snapshotting only ever adds information — jj itself
  does it on any command — but it is a recorded operation, so `--dry-run` may add one for a
  workspace that has uncommitted changes. `--no-workspaces` reads no workspace at all and therefore
  records nothing.
- With `--scan deep`, a pull request number that appears in more than one scanned repository is
  ignored: a `PR: #N` trailer cannot say which repository it means. This can only ever remove
  candidates, never add one.

Locks live in repository-scoped jj config (`jj-cleanup.locked-bookmarks`), so they travel with the
repository and are visible from every workspace. A malformed lock list is an error, never silently
an empty list.

## Development

See [CONTRIBUTING.md](CONTRIBUTING.md).

## License

Licensed under either of [Apache License, Version 2.0](LICENSE-APACHE) or
[MIT license](LICENSE-MIT) at your option.
