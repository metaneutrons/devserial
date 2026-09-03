# Contributing

## The one command before you push

```bash
just check
```

That runs the same three gates CI runs: `cargo fmt --check`, clippy over all
targets and features with warnings denied, and the test suite with all features.
CI runs those same tests through `cargo nextest`, one process per test; the test
set is identical. If `just check` passes locally it passes in CI, apart from the
platform matrix and the per-feature-set clippy runs.

The toolchain is not your choice: [rust-toolchain.toml](rust-toolchain.toml)
pins it and rustup installs that version automatically, locally and in CI.

Two further gates are CI only because they need the network:

```bash
just audit          # cargo deny check: advisories, licences, duplicates, registries
```

## Commit messages

[Conventional Commits](https://www.conventionalcommits.org), without exception.
release-please derives the next version and the changelog from the commit types,
so a message outside the scheme produces a wrong version or a missing changelog
entry.

Allowed types: `feat`, `fix`, `docs`, `style`, `refactor`, `perf`, `test`,
`build`, `ci`, `chore`, `revert`. The scope is optional and lowercase. The
subject line is at most 100 characters.

A breaking change gets a `!` after the type and a `BREAKING CHANGE:` paragraph
in the body.

**The pull request title matters more than the individual commits.** Merges are
squashes, and the PR title becomes the subject line on `main`, which is what
release-please reads. The messages of individual commits on a feature branch are
discarded by the squash.

No AI attribution trailers. Specifically no `Co-authored-by:` naming Claude or
Anthropic, no `Generated with Claude Code` line and no `noreply@anthropic.com`
as author or committer. A real human co-author is welcome.

## Branches

`<type>/<short-description>`, with the same types as the commit messages, for
example `fix/daemon-stop-signal` or `refactor/audit-remediation`. Work goes
through a pull request; `main` is not a branch to commit to directly.

## Hooks

```bash
brew install lefthook gitleaks
just setup
```

`just setup` refuses to continue if either binary is missing, then runs
`lefthook install`. The configuration is [lefthook.yml](lefthook.yml) and the
checks themselves are three POSIX shell scripts under `scripts/hooks/`, so they
also work under `core.hooksPath` or husky if you prefer a different manager.

| Hook | Runs | Measured |
| --- | --- | --- |
| `commit-msg` | Conventional Commits, subject length, no AI attribution trailer | under 100 ms |
| `pre-commit` | branch and file-size guards, `gitleaks --staged`, `cargo fmt --check` on staged Rust files | 0.06 s |
| `pre-push` | `gitleaks` over the commits being pushed, `cargo clippy --all-targets -- -D warnings` | 5.9 s |

Two deliberate omissions. `cargo nextest` is configured in `pre-push` but
skipped: the run itself takes 3 seconds, the test-profile rebuild it needs takes
23, and together with clippy that would put the hook at the 30 second budget
with no margin. The tests run in CI on three operating systems instead.
`cargo deny check` needs the network and is CI only.

The `pre-push` secret scan is deliberately incremental. It scans `@{upstream}..HEAD`,
so a secret already in pushed history does not block you forever; the full
history scan runs in CI on every pull request.

Two documented ways out, so nobody reaches for `--no-verify`:

- `ALLOW_COMMIT_ON_DEFAULT=1 git commit …` to commit on the default branch
- `MAX_STAGED_BYTES=<bytes> git commit …` to stage something above 5 MiB.
  Consider whether the file belongs in git first.

`git commit --no-verify` bypasses the hooks and buys nothing: everything a hook
checks is checked again in CI, so the only result is a later and slower failure.

## What runs where

| Gate | Local hook | CI |
| --- | --- | --- |
| `cargo fmt --check` | pre-commit, staged files | yes |
| clippy with warnings denied | pre-push | yes, eight feature and platform cells |
| tests via `cargo nextest` | configured, skipped | yes, three operating systems |
| line coverage, floor 50% | no | yes, on the all-features cell |
| documentation with warnings denied | no | yes |
| `cargo deny check` | no | yes |
| secret scan | pre-commit staged, pre-push incremental | yes, full history |
| Conventional Commits | commit-msg, per commit | yes, on the pull request title |
| `lefthook validate` | no | yes |

Full test matrices, coverage thresholds and anything needing the network belong
in CI, not in a hook. A hook that needs the network fails on a train and teaches
people to bypass it.

## Code

Match the surrounding code. Two rules are worth stating because they are load
bearing rather than stylistic:

- `engine.rs` is the only place operations are carried out. The CLI, the daemon,
  the MCP server, the GUI and the TUI are transports over the vocabulary in
  `protocol.rs` and contain no operation logic. That is what keeps their
  behaviour identical.
- Facts live in exactly one module. Paths and file names in `paths.rs`, serial
  parameters in `serial_params.rs`, hex in `hex.rs`, export formats in
  `export.rs`, platform IPC in `transport.rs`. A second copy of one of these is
  a defect even when both copies agree today.

Clippy runs with `pedantic`, `nursery` and `cargo` at `deny`. A blanket
`#![allow(clippy::pedantic)]` in a source file is not an acceptable answer; if a
lint genuinely does not fit, the exception goes into `[lints.clippy]` in
`Cargo.toml` with a comment saying why.

## Licensing

devserial is GPL-3.0-or-later. By contributing you agree your contribution is
licensed the same way. If you add an embedded asset with its own licence, it goes
into [THIRD-PARTY-NOTICES.md](THIRD-PARTY-NOTICES.md), its licence text into
`licenses/`, and both into every packaging channel; `deny.toml` needs the licence
in its allow list as well.
