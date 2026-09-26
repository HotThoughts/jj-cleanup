# Contributing

## Setup

- Rust 1.85 or newer (the MSRV; CI builds with exactly 1.85).
- `jj >= 0.44` to run the integration tests. They skip themselves when `jj` is missing, so a plain
  `cargo test` still works on a machine without it — but CI installs jj and therefore exercises
  them, so do not rely on the skip.
- An authenticated `gh` is only needed for the commands that talk to GitHub; the integration tests
  put a stub `gh` on `PATH` instead.

## Checks

Everything CI runs, locally:

```console
$ cargo fmt --check
$ cargo clippy --all-targets -- -D warnings
$ cargo test
$ cargo +1.85 build --locked     # MSRV
$ cargo deny check               # licenses, advisories, sources
```

## Commits

Conventional commits: `feat:`, `fix:`, `docs:`, `test:`, `refactor:`, `chore:`. Keep the subject
under 72 characters and explain the "why" in the body.

## Tests

Three layers, each covering what the one below cannot:

- **Unit tests** beside the code: revset construction, trailer parsing, GraphQL and jj output
  parsing, version comparison.
- **`tests/plan.rs`** drives `build_plan` from fixtures. Planning is pure and does no I/O, so every
  safety rule is pinned here as a rendered-plan snapshot. Run `cargo insta review` after an
  intentional change.
- **`tests/cli.rs`** runs the real binary against a real jj repository in a temporary directory,
  with a stub `gh` on `PATH`. This is the layer that proves the jj templates parse, the revsets
  resolve, and applying a plan leaves the repository in the intended state.

## Safety

`src/plan.rs` decides what gets deleted from someone's repository. Changes there need a test that
pins the unsafe direction, not just the happy path: an open PR, a dirty workspace, a locked
bookmark, a conflicted bookmark, a shared base between a candidate and a kept bookmark, and a
revision checked out in a workspace the run leaves alone.

Two invariants are worth stating explicitly, because reviews hinge on them:

1. A revision is abandoned only if it is above trunk, is not protected by a kept bookmark, and is
   not protected by a working copy this run leaves alone.
2. A working copy is released (forgotten or removed) before any revision it holds is abandoned.

## License

By contributing you agree that your contributions are dual-licensed under MIT OR Apache-2.0, as
described in [README.md](README.md).
