# 0008 Regression corpus: Git LFS in this repo

Status: accepted (2026-11-24)

## Context

ADR 0004 keeps full GoPro footage out of normal git: it lives "in Git LFS or a
storage bucket" and is used only by the manually dispatched regression suite
(`.github/workflows/regression.yml`). We need to pick one. The footage is the
owner's personal recordings (faces, places, GPS tracks in GPMF), so privacy
matters as much as cost. Expected size: a handful of short clips per camera
generation, a few GiB in total (guess).

Options considered:

| | Git LFS in this repo | Separate private corpus repo (LFS) | External bucket (S3, R2, GCS) |
| --- | --- | --- | --- |
| Cost | 10 GiB storage + 10 GiB/month bandwidth included on the owner's plan, metered beyond that and only if a budget is set (guess: check Settings > Billing) | same quota, shared across the account | storage cheap, but needs a paid account and a card |
| Access from Actions | built in: the workflow's `GITHUB_TOKEN` can fetch LFS objects of its own repo | needs a PAT or deploy key secret (GITHUB_TOKEN cannot read another private repo) | needs cloud credentials as secrets |
| Access from cloud sessions | same as code; agents need no extra credentials | extra credentials | extra credentials |
| Privacy | as private as the repo | as private as that repo | depends on bucket policy; one more thing to misconfigure |
| Setup today | none | create repo + secret | create account, bucket, IAM, secret |

## Decision

Store the corpus with **Git LFS in this repository**, under `fixtures/corpus/`
(`.gitattributes` routes every file there through LFS). `fixtures/corpus.toml`
lists each file with its SHA-256, camera model, codec and notes; tests reach a
file through `kv_testkit::corpus_file` / `corpus_file_or_skip!`, which return
the path or skip the test with a clear message when the file is absent or is
only an LFS pointer, and fail loudly on a checksum mismatch.

This needs no purchase and no credentials now. The corpus starts **empty**:
real footage must come from the owner, who adds files later (steps at the top
of `fixtures/corpus.toml`). Until then every corpus test skips cleanly.

To protect the bandwidth quota:

- Only `regression.yml` fetches LFS objects (`git lfs pull --include
  "fixtures/corpus/**"`), and caches `.git/lfs/objects` keyed on the manifest
  hash, so a run with an unchanged corpus downloads nothing from LFS.
- The merge gate (`ci.yml`) checks out without LFS (the `actions/checkout`
  default) and never sees the footage.
- Agents in cloud sessions should clone with `GIT_LFS_SKIP_SMUDGE=1` unless the
  task needs the footage; the helper skips cleanly on pointer files.

## Consequences

- No new accounts or secrets. Access control is the repo's.
- If the corpus outgrows the included quota, the owner must set an LFS budget
  or move to a bucket; only `corpus_dir()` (overridable with `KV_CORPUS_DIR`)
  and the fetch step in `regression.yml` would change.
- LFS objects stay in the repo's history. **Before the repo is ever made
  public**, the footage must be removed from LFS (history rewrite and LFS
  object deletion via GitHub support) or moved elsewhere. Personal GPS
  telemetry is the main risk.
- Never add proprietary GoPro material (Quik music, templates, installers):
  only footage the owner shot.
- Once the corpus has files, set `KV_REQUIRE_CORPUS=1` in the regression
  workflow so a missing file fails the run instead of silently skipping.
- Regression runs on a private repo consume Actions minutes (macOS minutes
  count several times over); keep the workflow manually dispatched.
