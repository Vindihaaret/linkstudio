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

/// Quotes a program path the way the desktop entry specification asks, so
/// paths with spaces work.
fn quote_exec(path: &str) -> String {
    let mut quoted = String::from("\"");
    for c in path.chars() {
        if matches!(c, '"' | '`' | '$' | '\\') {
            quoted.push('\\');
        }
        quoted.push(c);
    }
    quoted.push('"');
    quoted
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
    let quoted = quote_exec(&exe.to_string_lossy());
    fs::write(
        path,
        format!(
            "[Desktop Entry]\nType=Application\nName=Link Studio\nExec={quoted} {MINIMIZED_FLAG}\nIcon={APP_ID}\n\
             X-GNOME-Autostart-enabled=true\nNoDisplay=true\n"
        ),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exec_is_quoted() {
        assert_eq!(quote_exec("/home/me/.local/bin/linkstudio"), "\"/home/me/.local/bin/linkstudio\"");
        assert_eq!(quote_exec("/home/my name/a$b"), "\"/home/my name/a\\$b\"");
    }
}
