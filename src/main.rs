mod autostart;
mod camera;
mod config;
mod i18n;
mod mcp;
mod preview;
mod tray;
mod ui;
mod uninstall;
mod vcam;

use adw::prelude::*;
use gtk::{gio, glib};

pub const APP_ID: &str = "io.github.linkstudio.LinkStudio";

fn main() -> glib::ExitCode {
    if std::env::args().nth(1).as_deref() == Some("mcp") {
        return mcp::run();
    }
    let config = config::Config::load();
    if let Some(lang) = &config.language {
        // Also covers strings from GTK and libadwaita themselves, which read
        // LANGUAGE through gettext. Set before any other thread exists.
        std::env::set_var("LANGUAGE", lang);
        i18n::set_language(Some(lang));
    }

    // `--minimized` (used at login) starts in the status icon only. GApplication
    // does not know the flag, so it is taken out before handing over the arguments.
    let args: Vec<String> = std::env::args().collect();
    let minimized = std::cell::Cell::new(args.iter().any(|a| a == autostart::MINIMIZED_FLAG));
    let args: Vec<String> = args.into_iter().filter(|a| a != autostart::MINIMIZED_FLAG).collect();

    let app = adw::Application::builder().application_id(APP_ID).build();

    app.connect_startup(move |app| {
        gtk::Window::set_default_icon_name(APP_ID);
        ui::load_css();
        ui::apply_appearance(config.appearance);
        ui::start_tray(app);
        ui::register_claude(app);
        ui::start_update_checks(app);

        let quit = gio::ActionEntry::builder("quit").activate(|app: &adw::Application, _, _| app.quit()).build();
        app.add_action_entries([quit]);
        app.set_accels_for_action("app.quit", &["<Control>q"]);
        app.set_accels_for_action("window.close", &["<Control>w"]);
        app.set_accels_for_action("win.preferences", &["<Control>comma"]);
    });

    app.connect_activate(move |app| ui::activate(app, minimized.replace(false)));

    app.run_with_args(&args)
}
