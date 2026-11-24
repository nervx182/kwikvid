# 0004 Test media and Mac builds

Status: accepted (2026-11-24)

Decision: tiny generated clips (seconds long) are committed or generated for the fast gate. Full GoPro footage lives in Git LFS or a storage bucket and is used only by the manual regression suite. Mac builds are unsigned until the app is shared (first launch: right-click, Open).

Consequences: no Apple Developer account needed yet; the repo stays small.
