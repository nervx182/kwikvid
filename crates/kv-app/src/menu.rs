//! The native menu bar (Tauri menu API).
//!
//! Menu items do nothing in Rust except tell the UI: a click (or its
//! shortcut) emits [`MENU_EVENT`] with a [`MenuAction`], and the UI runs the
//! flow (unsaved-changes prompt, file picker, project command). Keeping the
//! flows in the UI makes them testable in Playwright with a mock menu event
//! (ADR 0011).
//!
//! Other features add their items by extending [`build`] and [`action_for`].

use std::path::{Path, PathBuf};

use serde::Serialize;
use tauri::menu::{Menu, MenuBuilder, MenuItemBuilder, Submenu, SubmenuBuilder};
use tauri::{AppHandle, Emitter, Runtime};

/// The event the backend emits with a [`MenuAction`] when a menu item is
/// chosen (or the window is asked to close with unsaved changes).
pub const MENU_EVENT: &str = "kv://menu";

pub const NEW: &str = "new";
pub const OPEN: &str = "open";
pub const SAVE: &str = "save";
pub const SAVE_AS: &str = "save_as";
pub const QUIT: &str = "quit";
/// Prefix of the Open Recent items; the suffix is the index into the
/// recent projects list.
pub const RECENT_PREFIX: &str = "recent:";

/// Item id and accelerator of every File menu shortcut. `CmdOrCtrl` is Cmd
/// on macOS and Ctrl on Linux and Windows.
pub const SHORTCUTS: [(&str, &str); 5] = [
    (NEW, "CmdOrCtrl+N"),
    (OPEN, "CmdOrCtrl+O"),
    (SAVE, "CmdOrCtrl+S"),
    (SAVE_AS, "CmdOrCtrl+Shift+S"),
    (QUIT, "CmdOrCtrl+Q"),
];

fn accelerator(id: &str) -> &'static str {
    SHORTCUTS
        .iter()
        .find(|(i, _)| *i == id)
        .map(|(_, a)| *a)
        .unwrap_or_default()
}

/// What the UI should do, as `{ "action": "...", ... }` JSON.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "action", rename_all = "snake_case")]
pub enum MenuAction {
    New,
    Open,
    OpenRecent { path: PathBuf },
    Save,
    SaveAs,
    /// Quit (or close the window); the UI asks about unsaved changes first.
    Quit,
}

/// The action for a menu item id, given the recent projects list the menu
/// was built from. `None` for ids the app does not handle (predefined
/// items handle themselves).
pub fn action_for(id: &str, recent: &[PathBuf]) -> Option<MenuAction> {
    if let Some(i) = id.strip_prefix(RECENT_PREFIX) {
        let path = recent.get(i.parse::<usize>().ok()?)?;
        return Some(MenuAction::OpenRecent { path: path.clone() });
    }
    Some(match id {
        NEW => MenuAction::New,
        OPEN => MenuAction::Open,
        SAVE => MenuAction::Save,
        SAVE_AS => MenuAction::SaveAs,
        QUIT => MenuAction::Quit,
        _ => return None,
    })
}

/// Tell the UI to run `action`.
pub fn emit_action<R: Runtime>(app: &AppHandle<R>, action: &MenuAction) {
    if let Err(e) = app.emit(MENU_EVENT, action) {
        tracing::warn!(error = %e, "could not emit {MENU_EVENT}");
    }
}

/// The label of a recent project: its file name and folder.
pub fn recent_label(path: &Path) -> String {
    let name = path
        .file_stem()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_else(|| path.display().to_string());
    match path.parent() {
        Some(dir) if !dir.as_os_str().is_empty() => format!("{name} — {}", dir.display()),
        _ => name,
    }
}

fn item<R: Runtime>(
    app: &AppHandle<R>,
    id: &str,
    text: &str,
) -> tauri::Result<tauri::menu::MenuItem<R>> {
    let b = MenuItemBuilder::with_id(id, text);
    let acc = accelerator(id);
    let b = if acc.is_empty() { b } else { b.accelerator(acc) };
    b.build(app)
}

fn recent_submenu<R: Runtime>(app: &AppHandle<R>, recent: &[PathBuf]) -> tauri::Result<Submenu<R>> {
    let mut b = SubmenuBuilder::with_id(app, "open_recent", "Open Recent");
    if recent.is_empty() {
        b = b.item(
            &MenuItemBuilder::with_id("recent:none", "No Recent Projects")
                .enabled(false)
                .build(app)?,
        );
    }
    for (i, path) in recent.iter().enumerate() {
        b = b.text(format!("{RECENT_PREFIX}{i}"), recent_label(path));
    }
    b.build()
}

/// The whole menu bar. Rebuilt (see [`refresh`]) when the recent list
/// changes.
pub fn build<R: Runtime>(app: &AppHandle<R>, recent: &[PathBuf]) -> tauri::Result<Menu<R>> {
    let file = SubmenuBuilder::new(app, "File")
        .item(&item(app, NEW, "New")?)
        .item(&item(app, OPEN, "Open…")?)
        .item(&recent_submenu(app, recent)?)
        .separator()
        .item(&item(app, SAVE, "Save")?)
        .item(&item(app, SAVE_AS, "Save As…")?);
    #[cfg(not(target_os = "macos"))]
    let file = file.separator().item(&item(app, QUIT, "Quit")?);
    let file = file.build()?;

    // No Undo/Redo items: the UI handles Cmd/Ctrl+Z itself (toolbar.js),
    // and a predefined Undo item would swallow the shortcut.
    let edit = SubmenuBuilder::new(app, "Edit")
        .cut()
        .copy()
        .paste()
        .select_all()
        .build()?;

    let menu = MenuBuilder::new(app);
    #[cfg(target_os = "macos")]
    let menu = {
        // The first submenu is the app menu on macOS. Quit is our own item,
        // so it goes through the unsaved-changes prompt.
        let app_menu = SubmenuBuilder::new(app, "kwikvid")
            .about(None)
            .separator()
            .services()
            .separator()
            .hide()
            .hide_others()
            .show_all()
            .separator()
            .item(&item(app, QUIT, "Quit kwikvid")?)
            .build()?;
        let window = SubmenuBuilder::new(app, "Window")
            .minimize()
            .separator()
            .close_window()
            .build()?;
        menu.item(&app_menu).item(&file).item(&edit).item(&window)
    };
    #[cfg(not(target_os = "macos"))]
    let menu = menu.item(&file).item(&edit);
    menu.build()
}

/// Rebuild the menu bar with a new recent projects list.
pub fn refresh<R: Runtime>(app: &AppHandle<R>, recent: &[PathBuf]) {
    match build(app, recent) {
        Ok(menu) => {
            if let Err(e) = app.set_menu(menu) {
                tracing::warn!(error = %e, "could not set the menu");
            }
        }
        Err(e) => tracing::warn!(error = %e, "could not build the menu"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn item_ids_map_to_actions() {
        let recent = vec![PathBuf::from("/p/a.kwikvid"), PathBuf::from("/p/b.kwikvid")];
        assert_eq!(action_for("new", &recent), Some(MenuAction::New));
        assert_eq!(action_for("open", &recent), Some(MenuAction::Open));
        assert_eq!(action_for("save", &recent), Some(MenuAction::Save));
        assert_eq!(action_for("save_as", &recent), Some(MenuAction::SaveAs));
        assert_eq!(action_for("quit", &recent), Some(MenuAction::Quit));
        assert_eq!(
            action_for("recent:1", &recent),
            Some(MenuAction::OpenRecent {
                path: PathBuf::from("/p/b.kwikvid")
            })
        );
        assert_eq!(action_for("recent:2", &recent), None);
        assert_eq!(action_for("recent:none", &recent), None);
        assert_eq!(action_for("copy", &recent), None);
    }

    #[test]
    fn actions_serialise_as_tagged_json() {
        let j = |a: MenuAction| serde_json::to_value(a).unwrap();
        assert_eq!(j(MenuAction::New), serde_json::json!({"action": "new"}));
        assert_eq!(j(MenuAction::SaveAs), serde_json::json!({"action": "save_as"}));
        assert_eq!(
            j(MenuAction::OpenRecent {
                path: PathBuf::from("/p/a.kwikvid")
            }),
            serde_json::json!({"action": "open_recent", "path": "/p/a.kwikvid"})
        );
    }

    #[test]
    fn shortcuts_use_cmd_or_ctrl() {
        assert_eq!(accelerator(NEW), "CmdOrCtrl+N");
        assert_eq!(accelerator(OPEN), "CmdOrCtrl+O");
        assert_eq!(accelerator(SAVE), "CmdOrCtrl+S");
        assert_eq!(accelerator(SAVE_AS), "CmdOrCtrl+Shift+S");
        assert_eq!(accelerator(QUIT), "CmdOrCtrl+Q");
        assert_eq!(accelerator("nope"), "");
    }

    #[test]
    fn recent_labels_show_name_and_folder() {
        assert_eq!(
            recent_label(Path::new("/Users/ana/Movies/trip.kwikvid")),
            "trip — /Users/ana/Movies"
        );
        assert_eq!(recent_label(Path::new("trip.kwikvid")), "trip");
    }
}
