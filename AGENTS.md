# AGENTS.md: how to work in kwikvid

kwikvid is a Rust desktop video editor that recreates the GoPro Quik desktop
workflow: load clips, add a music file, get an automatic edit cut to the beat,
and steer it by forcing moments in or out. Primary target: macOS on Apple
Silicon. Everything except the UI shell and platform media backends must stay
portable (Linux, Windows).

You are probably an agent starting with no context. This file, the issue you
picked up, and `docs/` are meant to be enough. If they are not, say so on the
issue (see "When you are stuck").

## Ground rules

- All development happens in cloud sessions. Never assume access to the
  owner's Mac. Mac-specific behaviour is verified by the `mac-smoke` CI job.
- Work only on what your issue asks. Anything else you notice becomes a new
  issue (use the Task template), not a drive-by change.
- Keep the core crates free of I/O and platform code. Platform code lives
  behind traits in `kv-media` / `kv-render` (see `docs/architecture.md`).
- Every user-facing action is a `Command` in `kv-core`, so the desktop app, the
  CLI and the MCP server share one API.
- Dependencies must pass `cargo deny` (permissive licences only; FFmpeg only as
  an LGPL dynamic build). No GPL/AGPL crates or libraries, not even optional.
- Never commit proprietary GoPro material (the Quik installer, its music, its
  gauge templates) or large media. Tiny generated fixtures only; full footage
  lives in Git LFS / external storage for the regression suite.

## Build and test

```sh
cargo fmt --all
cargo clippy --workspace --all-targets -- -D warnings
cargo nextest run --workspace     # or: cargo test --workspace
cargo deny check                  # if cargo-deny is installed
```

The merge gate (`.github/workflows/ci.yml`) runs exactly these on Linux for
every PR. The macOS arm64 build and test (`mac-smoke`) runs only on pushes to
`main` and manual dispatch, because Mac minutes cost 10x on a private repo; if
it goes red on `main`, fixing it is the top priority. Keep it fast: target under 10 minutes total. Slow,
large-data or benchmark tests belong in the full regression
(`.github/workflows/regression.yml`, manually dispatched): mark them
`#[ignore = "regression"]`.

## Full regression suite

Slow tests, tests on real footage and benchmarks run only in
`.github/workflows/regression.yml`, never in the merge gate.

- Mark a slow or corpus test so the gate skips it:

  ```rust
  #[test]
  #[ignore = "regression"]
  fn hilights_on_real_hero12_clip() {
      let path = kv_testkit::corpus_file_or_skip!("hero12-hilights.mp4");
      // ...
  }
  ```

  Run them locally with `cargo test --workspace -- --include-ignored`.
- Real footage is listed in `fixtures/corpus.toml` and stored with Git LFS
  under `fixtures/corpus/` (ADR 0008). `corpus_file_or_skip!` returns early
  (the test passes with a `SKIPPED:` line) when the file is not checked out,
  so never fetch the corpus just to make a test run. Clone with
  `GIT_LFS_SKIP_SMUDGE=1` unless you need it. Only the owner adds footage.
- Benchmarks are criterion benches (`benches/*.rs`, `harness = false`) and
  run on macOS arm64 in the `benchmarks` job; results are uploaded as the
  `criterion-<run id>` artifact.
- Dispatch: Actions tab > regression > Run workflow, or
  `gh workflow run regression.yml --ref <branch> -f suite=all`
  (`suite`: `all`, `platforms`, `performance`, ...). When a job fails, the
  `report` job files a `status:triage` Task issue with the failing jobs, the
  run link and a log excerpt (untick `file_issue` to suppress it, e.g. while
  testing the workflow itself).

## Picking up an issue

1. Pick the oldest open issue labelled `status:ready` whose "Blocked by" issues
   are all closed and that has no assignee.
2. Claim it: assign yourself (or comment "claimed by agent session <link>"),
   swap `status:ready` for `status:in-progress`.
3. Branch from latest `main`: `issue-<number>-<short-slug>`.
4. Do the work. Tests that prove the acceptance criteria are part of the work.
5. Open a PR titled like the issue, body starting with `Closes #<number>`,
   following `.github/pull_request_template.md`.
6. When the `gate` check is green, squash-merge your own PR. `main` is
   protected: no direct pushes, PR and green `gate` required, branch up to date.
7. If CI fails, fix it. Never skip, delete or `#[ignore]` a test to get green.

## Filing issues

Use the Task template (`.github/ISSUE_TEMPLATE/task.md`). Write it for a reader
with zero context: why, goal, testable acceptance criteria, where to look, test
plan, out of scope, dependencies. Mark guesses as `(guess)`. New issues start
as `status:triage`; only make it `status:ready` when it meets that bar and is
size S or M (about one PR, under ~500 changed lines). Split anything larger.

## When you are stuck

Comment on the issue with what you tried and exactly what you need, set
`status:blocked` and `needs-human`, unassign yourself, and stop.

## Labels

- `status:triage` | `status:ready` | `status:in-progress` | `status:blocked`
- `needs-human`
- `type:feature` | `type:bug` | `type:chore` | `type:spike`
- `area:<crate or topic>` e.g. `area:core`, `area:autoedit`, `area:media`, `area:ci`
- `size:S` | `size:M` | `size:L`
- Milestones: `M0 Foundation`, `M1 Headless engine`, `M2 Desktop MVP`,
  `Roadmap I`, `Roadmap II`

## Conventions

- Rust stable, edition 2021, workspace lints. `unsafe` is denied by default;
  an FFI crate may allow it locally with a comment explaining why.
- Errors: `thiserror` in libraries, `anyhow` only in binaries.
- Logging: `tracing`.
- Time values: `kv_core::Micros` (microseconds, i64) everywhere.
- Decisions go in `docs/decisions/NNNN-title.md` (short ADR: context, decision,
  consequences).
