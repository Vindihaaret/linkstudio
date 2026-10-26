//! Updates from GitHub, like Vifte: an automatic check once a month (can be
//! turned off) and a manual one. A new release is downloaded as source, built
//! with its own install.sh and installed into ~/.local – no root needed.
//!
//! The check uses GitHub's public API through `curl`; nothing about the user
//! is sent.

use std::cell::{Cell, RefCell};
use std::rc::Rc;
use std::time::Duration;

use adw::prelude::*;
use gtk::{gio, glib};
use serde_json::Value;

use super::{App, CURRENT};
use crate::i18n::{tr, trf};

const REPO: &str = "Vindihaaret/linkstudio";
const CHECK_INTERVAL_SECS: i64 = 30 * 24 * 3600;
/// The network may not be up right after login.
const FIRST_CHECK_DELAY: Duration = Duration::from_secs(60);
const RECHECK_EVERY: Duration = Duration::from_secs(6 * 3600);

/// Downloads a release and installs it with its install.sh. On x86_64 that is
/// the ready-made program the release workflow attaches, checked against its
/// checksum; elsewhere, or for a release without one, the source code is built
/// and the source and build files are deleted again afterwards.
/// Arguments: $1 = tag (e.g. v0.3.0), $2 = version (e.g. 0.3.0).
const INSTALL_SCRIPT: &str = r#"set -eu
dir="$(mktemp -d "${TMPDIR:-/tmp}/linkstudio-update-XXXXXX")"
# The build files (about 1 GB) are only needed while building, so they go too.
build="${XDG_CACHE_HOME:-$HOME/.cache}/linkstudio/build"
trap 'rm -rf "$dir" "$build"; rmdir "${build%/build}" 2>/dev/null || true' EXIT
package="linkstudio-$(uname -m)-linux.tar.gz"
base="https://github.com/Vindihaaret/linkstudio/releases/download/$1"
echo "Downloading $1 …"
if curl -fsL --max-time 300 "$base/$package" -o "$dir/package.tar.gz"; then
    curl -fsSL --max-time 60 "$base/$package.sha256" -o "$dir/package.sha256"
    expected="$(cut -d' ' -f1 "$dir/package.sha256")"
    actual="$(sha256sum "$dir/package.tar.gz" | cut -d' ' -f1)"
    if [ "$expected" != "$actual" ]; then
        echo "The download is damaged (wrong checksum)." >&2
        exit 1
    fi
    tar -xzf "$dir/package.tar.gz" -C "$dir" --no-same-owner
    if [ "$(cat "$dir/linkstudio/VERSION" 2>/dev/null)" != "$2" ]; then
        echo "The download is not Link Studio $2." >&2
        exit 1
    fi
    sh "$dir/linkstudio/install.sh"
    exit 0
fi
echo "No ready-made program for this computer; building from source …"
curl -fsSL --max-time 300 "https://github.com/Vindihaaret/linkstudio/archive/refs/tags/$1.tar.gz" -o "$dir/source.tar.gz"
tar -xzf "$dir/source.tar.gz" -C "$dir" --no-same-owner
src="$(find "$dir" -mindepth 1 -maxdepth 1 -type d | head -n 1)"
if [ ! -f "$src/install.sh" ] || ! grep -q "^version = \"$2\"$" "$src/Cargo.toml"; then
    echo "The download is not Link Studio $2." >&2
    exit 1
fi
export CARGO_TARGET_DIR="$build"
export PATH="$HOME/.cargo/bin:$PATH"
sh "$src/install.sh"
echo "Removing build files …"
"#;

#[derive(Clone)]
pub struct Release {
    tag: String,
    version: String,
    notes: String,
    url: String,
}

thread_local! {
    /// The newest release found, for the notification's "show" action.
    static LATEST: RefCell<Option<Release>> = const { RefCell::new(None) };
    static CHECKING: Cell<bool> = const { Cell::new(false) };
}

fn parse_version(text: &str) -> Option<(u32, u32, u32)> {
    let mut parts = text.trim().trim_start_matches('v').split('.');
    let mut next = || -> Option<u32> {
        match parts.next() {
            Some(p) if !p.is_empty() && p.bytes().all(|b| b.is_ascii_digit()) => p.parse().ok(),
            Some(_) => None,
            None => Some(0),
        }
    };
    let version = (next()?, next()?, next()?);
    parts.next().is_none().then_some(version)
}

fn is_newer(tag: &str) -> bool {
    match (parse_version(tag), parse_version(env!("CARGO_PKG_VERSION"))) {
        (Some(new), Some(current)) => new > current,
        _ => false,
    }
}

/// Updating from the app only applies to the copy install.sh put in ~/.local,
/// not to `cargo run` in a source tree.
fn is_installed() -> bool {
    let installed = glib::home_dir().join(".local/bin/linkstudio").canonicalize().ok();
    let running = std::env::current_exe().ok().and_then(|p| p.canonicalize().ok());
    installed.is_some() && installed == running
}

fn release_info(json: &str) -> Result<Release, String> {
    let data: Value = serde_json::from_str(json).map_err(|e| e.to_string())?;
    let tag = data["tag_name"].as_str().unwrap_or_default().to_owned();
    let Some((a, b, c)) = parse_version(&tag) else {
        return Err(format!("unexpected release “{tag}”"));
    };
    if data["draft"] == true || data["prerelease"] == true {
        return Err(format!("unexpected release “{tag}”"));
    }
    Ok(Release {
        version: format!("{a}.{b}.{c}"),
        notes: plain_notes(data["body"].as_str().unwrap_or_default()),
        url: data["html_url"].as_str().map(str::to_owned).unwrap_or_else(|| format!("https://github.com/{REPO}/releases")),
        tag,
    })
}

/// Release notes are Markdown; show them as readable plain text.
fn plain_notes(markdown: &str) -> String {
    let mut lines = Vec::new();
    for line in markdown.lines() {
        if line.trim_start().starts_with("```") {
            continue;
        }
        let line = line.trim_start_matches('#').trim_start();
        let line = line.replace("**", "").replace('`', "");
        let line = match line.trim_start().strip_prefix("- ").or_else(|| line.trim_start().strip_prefix("* ")) {
            Some(rest) => format!("• {rest}"),
            None => line,
        };
        lines.push(line);
    }
    let text = lines.join("\n").trim().to_owned();
    match text.char_indices().nth(900) {
        Some((cut, _)) => format!("{}\n…", text[..cut].rsplit_once('\n').map_or(&text[..cut], |(head, _)| head)),
        None => text,
    }
}

fn fetch_latest(done: impl FnOnce(Result<Release, String>) + 'static) {
    let url = format!("https://api.github.com/repos/{REPO}/releases/latest");
    let agent = format!("linkstudio/{}", env!("CARGO_PKG_VERSION"));
    let argv = ["curl", "-fsSL", "--max-time", "15", "-H", "Accept: application/vnd.github+json", "-A", &agent, &url];
    let argv: Vec<&std::ffi::OsStr> = argv.iter().map(|a| a.as_ref()).collect();
    let flags = gio::SubprocessFlags::STDOUT_PIPE | gio::SubprocessFlags::STDERR_PIPE;
    let process = match gio::Subprocess::newv(&argv, flags) {
        Ok(process) => process,
        Err(err) => return done(Err(err.to_string())),
    };
    let child = process.clone();
    process.communicate_utf8_async(None, None::<&gio::Cancellable>, move |result| {
        done(match result {
            Ok((Some(out), _)) if child.is_successful() => release_info(&out),
            Ok((_, err)) => Err(err.map(|e| e.trim().to_owned()).filter(|e| !e.is_empty()).unwrap_or_else(|| "no answer from GitHub".into())),
            Err(err) => Err(err.to_string()),
        });
    });
}

/// Starts the monthly background check and the notification action.
pub fn start_auto_checks(application: &adw::Application) {
    let show = gio::ActionEntry::builder("show-update")
        .activate(|_: &adw::Application, _, _| {
            let release = LATEST.with(|latest| latest.borrow().clone());
            if let (Some(app), Some(release)) = (CURRENT.with(|c| c.borrow().upgrade()), release) {
                app.present();
                app.show_update_dialog(release);
            }
        })
        .build();
    application.add_action_entries([show]);

    glib::timeout_add_local_once(FIRST_CHECK_DELAY, auto_tick);
}

fn auto_tick() {
    if let Some(app) = CURRENT.with(|c| c.borrow().upgrade()) {
        let config = app.config.borrow();
        let due = config.auto_update_check
            && glib::DateTime::now_utc().map(|now| now.to_unix()).unwrap_or(0) - config.last_update_check >= CHECK_INTERVAL_SECS;
        drop(config);
        if due {
            app.check_for_updates(false);
        }
    }
    glib::timeout_add_local_once(RECHECK_EVERY, auto_tick);
}

impl App {
    pub(super) fn check_for_updates(self: &Rc<Self>, manual: bool) {
        if CHECKING.with(|c| c.replace(true)) {
            return;
        }
        let weak = Rc::downgrade(self);
        fetch_latest(move |result| {
            CHECKING.with(|c| c.set(false));
            let Some(app) = weak.upgrade() else { return };
            let release = match result {
                Ok(release) => release,
                Err(err) => {
                    eprintln!("update check failed: {err}");
                    if manual {
                        app.update_message(tr("Could not check for updates"), &trf("Error: {}", &[&err]));
                    }
                    return;
                }
            };
            {
                let mut config = app.config.borrow_mut();
                config.last_update_check = glib::DateTime::now_utc().map(|now| now.to_unix()).unwrap_or(0);
                config.save();
            }
            if !is_newer(&release.tag) {
                if manual {
                    app.update_message(
                        tr("Link Studio is up to date"),
                        &trf("You have the newest version ({}).", &[&env!("CARGO_PKG_VERSION")]),
                    );
                }
                return;
            }
            LATEST.with(|latest| *latest.borrow_mut() = Some(release.clone()));
            let skipped = app.config.borrow().skipped_version.as_deref() == Some(release.tag.as_str());
            if !manual && skipped {
                return;
            }
            if manual || app.window.is_visible() {
                app.show_update_dialog(release);
            } else if let Some(application) = app.window.application() {
                let notification = gio::Notification::new(&trf("Link Studio {} is available", &[&release.version]));
                notification.set_body(Some(tr("Click to see what is new and update.")));
                notification.set_default_action("app.show-update");
                application.send_notification(Some("update"), &notification);
            }
        });
    }

    fn update_message(&self, heading: &str, body: &str) {
        let dialog = adw::AlertDialog::new(Some(heading), Some(body));
        dialog.add_response("ok", tr("OK"));
        dialog.present(Some(&self.window));
    }

    pub(super) fn show_update_dialog(self: &Rc<Self>, release: Release) {
        let installed = is_installed();
        let mut body = trf("You have version {}.", &[&env!("CARGO_PKG_VERSION")]);
        if !installed {
            body.push_str("\n\n");
            body.push_str(tr("Link Studio runs from a source folder. Update it with “git pull” and run install.sh."));
        }
        let dialog = adw::AlertDialog::new(Some(&trf("Link Studio {} is available", &[&release.version])), Some(&body));
        if !release.notes.is_empty() {
            let notes = gtk::Label::builder()
                .label(&release.notes)
                .wrap(true)
                .xalign(0.0)
                .valign(gtk::Align::Start)
                .margin_top(10)
                .margin_bottom(10)
                .margin_start(12)
                .margin_end(12)
                .build();
            let scroll = gtk::ScrolledWindow::builder()
                .child(&notes)
                .min_content_height(120)
                .max_content_height(260)
                .propagate_natural_height(true)
                .hscrollbar_policy(gtk::PolicyType::Never)
                .css_classes(["card"])
                .build();
            dialog.set_extra_child(Some(&scroll));
        }
        dialog.add_response("skip", tr("Skip This Version"));
        dialog.add_response("later", tr("Later"));
        if installed {
            dialog.add_response("update", tr("Update Now"));
            dialog.set_response_appearance("update", adw::ResponseAppearance::Suggested);
            dialog.set_default_response(Some("update"));
        } else {
            dialog.add_response("open", tr("Open GitHub"));
            dialog.set_response_appearance("open", adw::ResponseAppearance::Suggested);
        }
        dialog.set_close_response("later");
        let weak = Rc::downgrade(self);
        dialog.connect_response(None, move |_, response| {
            let Some(app) = weak.upgrade() else { return };
            match response {
                "skip" => {
                    app.config.borrow_mut().skipped_version = Some(release.tag.clone());
                    app.config.borrow().save();
                }
                "open" => gtk::UriLauncher::new(&release.url).launch(Some(&app.window), None::<&gio::Cancellable>, |_| {}),
                "update" => app.install_update(&release),
                _ => {}
            }
        });
        dialog.present(Some(&self.window));
    }

    fn install_update(self: &Rc<Self>, release: &Release) {
        let dialog = adw::AlertDialog::new(
            Some(&trf("Updating to Link Studio {}", &[&release.version])),
            Some(tr("Downloading and installing the new version. If it has to be built on this computer, this takes a few minutes.")),
        );
        let spinner = gtk::Spinner::builder().spinning(true).build();
        let status = gtk::Label::builder()
            .label(tr("Starting…"))
            .ellipsize(gtk::pango::EllipsizeMode::End)
            .xalign(0.0)
            .hexpand(true)
            .css_classes(["dim-label"])
            .build();
        let status_row = gtk::Box::builder().spacing(10).build();
        status_row.append(&spinner);
        status_row.append(&status);
        let log = gtk::TextView::builder()
            .editable(false)
            .cursor_visible(false)
            .monospace(true)
            .wrap_mode(gtk::WrapMode::WordChar)
            .build();
        let details = gtk::Expander::builder()
            .label(tr("Details"))
            .child(
                &gtk::ScrolledWindow::builder()
                    .min_content_height(160)
                    .hscrollbar_policy(gtk::PolicyType::Never)
                    .child(&log)
                    .build(),
            )
            .build();
        let content = gtk::Box::builder().orientation(gtk::Orientation::Vertical).spacing(12).build();
        content.append(&status_row);
        content.append(&details);
        dialog.set_extra_child(Some(&content));
        dialog.add_response("close", tr("Close"));
        dialog.set_response_enabled("close", false);
        dialog.set_can_close(false);
        dialog.present(Some(&self.window));

        let argv = ["sh", "-c", INSTALL_SCRIPT, "linkstudio-update", &release.tag, &release.version];
        let argv: Vec<&std::ffi::OsStr> = argv.iter().map(|a| a.as_ref()).collect();
        let flags = gio::SubprocessFlags::STDOUT_PIPE | gio::SubprocessFlags::STDERR_MERGE;
        let process = match gio::Subprocess::newv(&argv, flags) {
            Ok(process) => process,
            Err(err) => {
                dialog.set_can_close(true);
                dialog.force_close();
                return self.update_message(tr("The update failed"), &trf("Error: {}", &[&err]));
            }
        };
        let reader = gio::DataInputStream::new(&process.stdout_pipe().expect("stdout is piped"));
        let buffer = log.buffer();
        let weak = Rc::downgrade(self);
        let version = release.version.clone();
        glib::spawn_future_local(async move {
            while let Ok(Some(line)) = reader.read_line_utf8_future(glib::Priority::DEFAULT).await {
                let line = line.trim();
                if line.is_empty() {
                    continue;
                }
                status.set_label(line);
                buffer.insert(&mut buffer.end_iter(), &format!("{line}\n"));
                log.scroll_to_iter(&mut buffer.end_iter(), 0.0, false, 0.0, 1.0);
            }
            let ok = process.wait_future().await.is_ok() && process.is_successful();
            spinner.set_spinning(false);
            spinner.set_visible(false);
            dialog.set_response_enabled("close", true);
            dialog.set_can_close(true);
            if ok {
                dialog.force_close();
                if let Some(app) = weak.upgrade() {
                    app.offer_restart(&version);
                }
            } else {
                dialog.set_heading(Some(tr("The update failed")));
                dialog.set_body(tr("Link Studio was not changed. See the details below."));
                details.set_expanded(true);
            }
        });
    }

    fn offer_restart(self: &Rc<Self>, version: &str) {
        let dialog = adw::AlertDialog::new(
            Some(&trf("Link Studio {} is installed", &[&version])),
            Some(tr("Restart Link Studio to use the new version.")),
        );
        dialog.add_responses(&[("later", tr("Later")), ("restart", tr("Restart Now"))]);
        dialog.set_response_appearance("restart", adw::ResponseAppearance::Suggested);
        dialog.set_default_response(Some("restart"));
        dialog.set_close_response("later");
        let weak = Rc::downgrade(self);
        dialog.connect_response(None, move |_, response| {
            let Some(app) = weak.upgrade() else { return };
            if response != "restart" {
                return;
            }
            // Only one Link Studio can run, so the new one starts once this has quit.
            let exe = glib::home_dir().join(".local/bin/linkstudio");
            let started = std::process::Command::new("sh")
                .arg("-c")
                .arg("sleep 1; exec \"$0\"")
                .arg(&exe)
                .stdin(std::process::Stdio::null())
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .spawn();
            if started.is_ok() {
                if let Some(application) = app.window.application() {
                    application.quit();
                }
            }
        });
        dialog.present(Some(&self.window));
    }

    /// The "Updates" group in Preferences → General.
    pub(super) fn update_preferences_group(self: &Rc<Self>) -> adw::PreferencesGroup {
        let group = adw::PreferencesGroup::builder().title(tr("Updates")).build();
        let auto = adw::SwitchRow::builder()
            .title(tr("Check for updates automatically"))
            .subtitle(tr("Once a month, from GitHub"))
            .active(self.config.borrow().auto_update_check)
            .build();
        let last = self.config.borrow().last_update_check;
        let when = (last > 0)
            .then(|| glib::DateTime::from_unix_local(last).ok().and_then(|d| d.format("%x").ok()))
            .flatten()
            .map(|d| d.to_string())
            .unwrap_or_else(|| tr("never").to_owned());
        let now = adw::ActionRow::builder()
            .title(tr("Check for updates now"))
            .subtitle(trf("Version {} · last checked: {}", &[&env!("CARGO_PKG_VERSION"), &when]))
            .build();
        let check = gtk::Button::builder().label(tr("Check")).valign(gtk::Align::Center).build();
        now.add_suffix(&check);
        group.add(&auto);
        group.add(&now);

        let weak = Rc::downgrade(self);
        auto.connect_active_notify(move |row| {
            let Some(app) = weak.upgrade() else { return };
            app.config.borrow_mut().auto_update_check = row.is_active();
            app.config.borrow().save();
        });
        let weak = Rc::downgrade(self);
        check.connect_clicked(move |_| {
            if let Some(app) = weak.upgrade() {
                app.check_for_updates(true);
            }
        });
        group
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn versions() {
        assert_eq!(parse_version("v1.2.3"), Some((1, 2, 3)));
        assert_eq!(parse_version("0.2"), Some((0, 2, 0)));
        assert_eq!(parse_version("v1.2.3.4"), None);
        assert_eq!(parse_version("v1.2.3; rm -rf /"), None);
        assert_eq!(parse_version("latest"), None);
        assert!(is_newer("v99.0.0"));
        assert!(!is_newer(&format!("v{}", env!("CARGO_PKG_VERSION"))));
        assert!(!is_newer("v0.0.1"));
    }

    #[test]
    fn notes() {
        let text = plain_notes("## What's new\n- **Updates** from `GitHub`\n```\ncode\n```");
        assert_eq!(text, "What's new\n• Updates from GitHub\ncode");
    }
}
