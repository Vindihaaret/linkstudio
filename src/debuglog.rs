//! Debug log, turned on under Preferences → General → Troubleshooting (`Config::debug_log`).
//!
//! While it is on, every Link Studio process – the window, the status icon and `linkstudio mcp` –
//! writes what it does to `~/.local/state/linkstudio/logs/linkstudio-<date>.log`: camera controls
//! and vendor commands with their results, the stream starting and stopping, Claude's requests and
//! whether they were allowed, privileged commands with their output, update checks and unexpected
//! errors (panics). Pictures and the camera's serial number are never written. The folder is only
//! readable by the user, files older than `KEEP_DAYS` days are deleted, and a day's file stops
//! growing at `MAX_BYTES`.

use std::fs::{self, OpenOptions};
use std::io::Write;
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Mutex;
use std::time::{Duration, Instant, SystemTime};

use gtk::glib;

pub const KEEP_DAYS: u64 = 14;
const MAX_BYTES: u64 = 20_000_000;
/// How often a process looks at the setting again, so a change in the window also reaches
/// `linkstudio mcp`, which runs on its own.
const RECHECK: Duration = Duration::from_secs(5);

static ON: AtomicBool = AtomicBool::new(false);
static CHECKED: Mutex<Option<Instant>> = Mutex::new(None);
static WRITE: Mutex<()> = Mutex::new(());
static ROLE: Mutex<&'static str> = Mutex::new("app");

pub fn log_dir() -> PathBuf {
    // glib::user_state_dir() needs a newer GLib than the one we build against.
    let state = std::env::var_os("XDG_STATE_HOME")
        .map(PathBuf::from)
        .filter(|path| path.is_absolute())
        .unwrap_or_else(|| glib::home_dir().join(".local/state"));
    state.join("linkstudio").join("logs")
}

/// Whether the debug log is on. Reads the setting from the config file at most every few seconds.
pub fn enabled() -> bool {
    let mut checked = CHECKED.lock().unwrap_or_else(|e| e.into_inner());
    if checked.is_none_or(|at| at.elapsed() > RECHECK) {
        *checked = Some(Instant::now());
        ON.store(crate::config::Config::read_debug_log(), Ordering::Relaxed);
    }
    ON.load(Ordering::Relaxed)
}

/// Sets the switch at once in this process (the config file is saved by the caller).
pub fn set_enabled(on: bool) {
    ON.store(on, Ordering::Relaxed);
    *CHECKED.lock().unwrap_or_else(|e| e.into_inner()) = Some(Instant::now());
}

/// Called once when a process starts. `role` is written on every line ("app" or "mcp").
pub fn setup(role: &'static str) {
    *ROLE.lock().unwrap_or_else(|e| e.into_inner()) = role;
    let previous = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        // Release builds are stripped, so a backtrace would only say "<unknown>".
        write(&format!("PANIC {info}").replace('\n', "\n    "));
        previous(info);
    }));
    if enabled() {
        write(&format!(
            "Link Studio {} starts ({role}), args {:?}",
            env!("CARGO_PKG_VERSION"),
            std::env::args().skip(1).collect::<Vec<_>>(),
        ));
        cleanup();
    }
}

/// GTK and libadwaita versions, once GTK is running.
pub fn log_versions() {
    if !enabled() {
        return;
    }
    write(&format!(
        "GTK {}.{}.{}, libadwaita {}.{}.{}, desktop {:?}",
        gtk::major_version(),
        gtk::minor_version(),
        gtk::micro_version(),
        adw::major_version(),
        adw::minor_version(),
        adw::micro_version(),
        std::env::var("XDG_CURRENT_DESKTOP").unwrap_or_default(),
    ));
}

/// Writes one line (or several, indented) when the log is on. Use the `debug!` macro, which
/// skips building the text when the log is off.
pub fn write(text: &str) {
    if !enabled() {
        return;
    }
    let _guard = WRITE.lock().unwrap_or_else(|e| e.into_inner());
    let dir = log_dir();
    if fs::create_dir_all(&dir).is_err() {
        return;
    }
    let _ = fs::set_permissions(&dir, fs::Permissions::from_mode(0o700));
    let now = glib::DateTime::now_local().ok();
    let stamp = |format: &str| now.as_ref().and_then(|t| t.format(format).ok()).map(|s| s.to_string());
    let day = stamp("%Y-%m-%d").unwrap_or_else(|| "unknown".into());
    let path = dir.join(format!("linkstudio-{day}.log"));
    if fs::metadata(&path).is_ok_and(|m| m.len() > MAX_BYTES) {
        return;
    }
    let role = *ROLE.lock().unwrap_or_else(|e| e.into_inner());
    let time = stamp("%H:%M:%S.%f").unwrap_or_default();
    let thread = std::thread::current();
    let thread = thread.name().unwrap_or("thread");
    let line = format!("{time} [{} {role} {thread}] {text}\n", std::process::id());
    if let Ok(mut file) = OpenOptions::new().create(true).append(true).mode(0o600).open(&path) {
        let _ = file.write_all(line.as_bytes());
    }
}

/// Deletes logs older than `KEEP_DAYS` days.
pub fn cleanup() {
    let Ok(entries) = fs::read_dir(log_dir()) else { return };
    let limit = SystemTime::now() - Duration::from_secs(KEEP_DAYS * 86_400);
    for entry in entries.flatten() {
        let path = entry.path();
        let old = entry.metadata().and_then(|m| m.modified()).is_ok_and(|t| t < limit);
        if old && path.extension().is_some_and(|e| e == "log") {
            let _ = fs::remove_file(path);
        }
    }
}

/// The newest log file, for "Copy Last Log".
pub fn newest() -> Option<PathBuf> {
    fs::read_dir(log_dir())
        .ok()?
        .flatten()
        .filter(|e| e.path().extension().is_some_and(|x| x == "log"))
        .max_by_key(|e| e.metadata().and_then(|m| m.modified()).ok())
        .map(|e| e.path())
}

/// The output of a command, shortened to its last lines.
pub fn tail(text: &str) -> String {
    const LINES: usize = 40;
    let lines: Vec<&str> = text.trim_end().lines().collect();
    let skipped = lines.len().saturating_sub(LINES);
    let mut out = String::new();
    if skipped > 0 {
        out.push_str(&format!("\n    … ({skipped} lines left out)"));
    }
    for line in &lines[skipped..] {
        let line: String = line.chars().take(500).collect();
        out.push_str("\n    ");
        out.push_str(&line);
    }
    out
}

/// `debug!("format", args…)` writes a line to the debug log when it is on.
macro_rules! debug {
    ($($arg:tt)*) => {
        if $crate::debuglog::enabled() {
            $crate::debuglog::write(&format!($($arg)*));
        }
    };
}
pub(crate) use debug;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tail_keeps_the_last_lines() {
        let text: String = (1..=50).map(|n| format!("line {n}\n")).collect();
        let out = tail(&text);
        assert!(out.starts_with("\n    … (10 lines left out)"));
        assert!(out.contains("line 11") && out.ends_with("line 50"));
        assert!(!out.lines().any(|line| line.trim() == "line 10"));
    }

    #[test]
    fn tail_shortens_long_lines() {
        let out = tail(&"x".repeat(2000));
        assert_eq!(out.trim().len(), 500);
    }
}
