//! In-app removal of the files install.sh puts under ~/.local, mirroring uninstall.sh.

use std::fs;
use std::io;
use std::path::PathBuf;

use gtk::glib;

use crate::APP_ID;

fn installed_files() -> Vec<PathBuf> {
    let data = glib::user_data_dir();
    vec![
        glib::home_dir().join(".local/bin/linkstudio"),
        data.join(format!("applications/{APP_ID}.desktop")),
        data.join(format!("icons/hicolor/scalable/apps/{APP_ID}.svg")),
        data.join(format!("icons/hicolor/symbolic/apps/{APP_ID}-symbolic.svg")),
    ]
}

pub fn config_dir() -> PathBuf {
    glib::user_config_dir().join("linkstudio")
}

/// Removes the installed program files, and the settings when `purge` is set.
/// Returns how many files were removed.
pub fn uninstall(purge: bool) -> io::Result<usize> {
    let mut removed = 0;
    for path in installed_files() {
        match fs::remove_file(&path) {
            Ok(()) => removed += 1,
            Err(err) if err.kind() == io::ErrorKind::NotFound => {}
            Err(err) => return Err(err),
        }
    }
    if purge {
        match fs::remove_dir_all(config_dir()) {
            Ok(()) => {}
            Err(err) if err.kind() == io::ErrorKind::NotFound => {}
            Err(err) => return Err(err),
        }
    }
    // Refresh menus so the launcher disappears right away; failures are harmless.
    let data = glib::user_data_dir();
    let _ = std::process::Command::new("update-desktop-database")
        .arg("-q")
        .arg(data.join("applications"))
        .status();
    let _ = std::process::Command::new("gtk4-update-icon-cache")
        .args(["-q", "-t"])
        .arg(data.join("icons/hicolor"))
        .status();
    Ok(removed)
}
