# 0002 UI framework: Tauri 2

Status: accepted (2026-11-24)

Context: Quik used a Qt shell with an HTML/JS UI. Agents write web UI well and web UI is testable in a Linux browser in CI.

Decision: Tauri 2 for the desktop shell and UI. An M0 spike must prove 60 fps video preview; iced is the fallback if it cannot.

Consequences: video preview frames must cross from the Rust core to the webview (native GPU layer or proxy files).
