//! What can be done with a file or folder, wherever it is shown.

use eframe::egui;
use std::ffi::OsString;
use std::io;
use std::os::windows::ffi::{OsStrExt, OsStringExt};
use std::os::windows::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::Command;

pub(crate) enum Action {
    /// Show the items under this path in the Cleanup tab.
    ShowInCleanup(String),
    /// Select every Safe item under this path.
    SelectSafe(PathBuf),
    /// Clear the selection under this path.
    ClearHere(PathBuf),
    OpenInExplorer(PathBuf),
    CopyPath(PathBuf),
}

/// Buttons that concern the suggestions at and below `path`.
pub(crate) fn cleanup_buttons(ui: &mut egui::Ui, path: &Path) -> Option<Action> {
    let mut chosen = None;
    if ui.button("Show in Cleanup").on_hover_text("Lists the suggested items here.").clicked() {
        chosen = Some(Action::ShowInCleanup(path.display().to_string()));
    }
    let select = ui.button("Select Safe here");
    if select.on_hover_text("Selects every Safe item here. Nothing moves until you confirm.").clicked() {
        chosen = Some(Action::SelectSafe(path.to_path_buf()));
    }
    chosen
}

/// Buttons that take `path` elsewhere.
pub(crate) fn location_buttons(ui: &mut egui::Ui, path: &Path) -> Option<Action> {
    let mut chosen = None;
    if ui.button("Open in Explorer").clicked() {
        chosen = Some(Action::OpenInExplorer(path.to_path_buf()));
    }
    if ui.button("Copy path").clicked() {
        chosen = Some(Action::CopyPath(path.to_path_buf()));
    }
    chosen
}

/// The context menu of an entry in the explorer.
pub(crate) fn entry_menu(ui: &mut egui::Ui, path: &Path) -> Option<Action> {
    let mut chosen = cleanup_buttons(ui, path);
    if ui.button("Clear selection here").clicked() {
        chosen = Some(Action::ClearHere(path.to_path_buf()));
    }
    ui.separator();
    chosen = location_buttons(ui, path).or(chosen);
    if chosen.is_some() {
        ui.close();
    }
    chosen
}

/// Opens File Explorer with `path` highlighted in its folder. Nothing at the path is ever run.
pub(crate) fn open_in_explorer(path: &Path) -> io::Result<()> {
    // Explorer silently opens an unrelated folder for a path that does not exist.
    path.symlink_metadata()?;
    // The full path to Explorer, so that no other program of the same name is started.
    let windows = std::env::var_os("SystemRoot").unwrap_or_else(|| r"C:\Windows".into());
    Command::new(Path::new(&windows).join("explorer.exe")).raw_arg(explorer_argument(path)).spawn()?;
    Ok(())
}

/// Explorer reads its command line its own way: the path is quoted on its own and uses backslashes.
fn explorer_argument(path: &Path) -> OsString {
    let wide: Vec<u16> = path
        .as_os_str()
        .encode_wide()
        .map(|c| if c == u16::from(b'/') { u16::from(b'\\') } else { c })
        .collect();
    // A drive root has no folder to be highlighted in, so it is opened itself.
    let mut argument = OsString::from(if path.parent().is_some() { "/select,\"" } else { "\"" });
    argument.push(OsString::from_wide(&wide));
    // A name cannot contain a quote, so nothing in it can end the quoting early.
    argument.push("\"");
    argument
}

#[cfg(test)]
mod tests {
    use super::explorer_argument;
    use std::path::Path;

    #[test]
    fn explorer_is_asked_to_highlight_the_quoted_path() {
        let argument = |path: &str| explorer_argument(Path::new(path)).into_string().unwrap();

        assert_eq!(argument(r"C:\My Files\a, b & c.exe"), r#"/select,"C:\My Files\a, b & c.exe""#);
        assert_eq!(argument("C:/Users/you/notes.txt"), r#"/select,"C:\Users\you\notes.txt""#);
        assert_eq!(argument(r"D:\"), r#""D:\""#);
    }
}
