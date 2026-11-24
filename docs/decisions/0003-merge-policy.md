# 0003 Agents merge on a green gate; main is protected

Status: accepted (2026-11-24)

Decision: agents squash-merge their own PRs once the required gate check is green. main is protected: PR required, gate required, branch up to date, no direct pushes, no force pushes.

Consequences: the merge gate must stay fast and trustworthy; never weaken it to get a PR in.
