# Bundled fonts: sources and licence evidence

Fonts are not crates, so `cargo deny` does not see them: each one here was
checked by hand against ADR 0005 (permissive licences only).

| File | Font | Licence | Source | SHA-256 |
| --- | --- | --- | --- | --- |
| `FiraSans-Bold.ttf` | Fira Sans Bold, version 4.203 (Mozilla / Telefonica, Carrois) | SIL Open Font License 1.1 (`OFL.txt`) | <https://github.com/google/fonts/blob/main/ofl/firasans/FiraSans-Bold.ttf>, fetched 2026-11-25 | `a4d8e149ecdd4874a0726eb0af894488b3b31c423d6b0017c8f415ed1b795b45` |

- Used by `kv_render::title` to draw the title card (MVP item 9). Embedded in
  the binary with `include_bytes!`, so it is a plain file, not Git LFS (a
  build with LFS smudging skipped must still compile).
- Shipped unmodified. The OFL allows bundling with software, including
  commercial software, as long as the licence travels with the font and the
  font is not sold on its own. The copyright line declares no Reserved Font
  Name; a modified version (subset, hinting stripped) would still be renamed
  and its changes noted here.
- Size limit for a bundled font: 500 KB (issue #56). This one is 478 KiB.
