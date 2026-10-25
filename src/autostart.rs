//! "Start at login": an XDG autostart entry that starts Link Studio in the
//! status icon (`--minimized`) without opening the window.

use std::fs;
use std::io;
use std::path::PathBuf;

use gtk::glib;

use crate::APP_ID;

pub const MINIMIZED_FLAG: &str = "--minimized";

fn entry_path() -> PathBuf {
    glib::user_config_dir().join("autostart").join(format!("{APP_ID}.desktop"))
}

pub fn is_enabled() -> bool {
    entry_path().exists()
}

pub fn set_enabled(on: bool) -> io::Result<()> {
    let path = entry_path();
    if !on {
        return match fs::remove_file(&path) {
            Err(err) if err.kind() != io::ErrorKind::NotFound => Err(err),
            _ => Ok(()),
        };
    }
    let exe = std::env::current_exe()?;
    if let Some(dir) = path.parent() {
        fs::create_dir_all(dir)?;
    }
    fs::write(
        path,
        format!(
            "[Desktop Entry]\nType=Application\nName=Link Studio\nExec={} {MINIMIZED_FLAG}\nIcon={APP_ID}\n\
             X-GNOME-Autostart-enabled=true\nNoDisplay=true\n",
            exe.display()
        ),
    )
}
